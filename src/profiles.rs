use std::{
    ffi::CString,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
};

use crate::{
    actions::{self, Action, Macro},
    driver::Report,
    models::Model,
    operations::{Settings, outgoing, validate_dpi_write},
    settings::{Dpi, Polling, Timing, put_vendor_word},
};

const PROFILE_MAGIC: &str = "attack-shark-r1 profile 1";
pub const PROFILE_LIMIT: usize = 131_072;
pub const TEXT_LIMIT: usize = 65_536;
pub const WINDOWS_LIMIT: usize = 16 * 1024 * 1024;
const WINDOWS_HEADER: usize = 0x28464;
const WINDOWS_RECORD: usize = 0x80e0;
const EXTENDED_WINDOWS_HEADER: usize = 0x7bf38;
const EXTENDED_WINDOWS_RECORD: usize = 0x18ca4;
const WINDOWS_NAME_BYTES: usize = 200;
const OPEN_FLAGS: i32 = libc::O_NOFOLLOW | libc::O_NONBLOCK;
static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowsOptions {
    pub lift_off: i32,
    pub motion_sync: i32,
    pub wake_mode: i32,
}

impl WindowsOptions {
    pub fn from_record(record: &[u8]) -> io::Result<Self> {
        let read = |offset| {
            let bytes: [u8; 4] = record
                .get(offset..offset + 4)
                .ok_or_else(|| invalid("truncated Windows options"))?
                .try_into()
                .unwrap();
            Ok::<_, io::Error>(i32::from_le_bytes(bytes))
        };
        Ok(Self {
            lift_off: read(0x8b4)?,
            motion_sync: read(0x948)?,
            wake_mode: read(0x93c)?,
        })
    }
}

pub struct Profile {
    pub firmware: u16,
    pub settings: Settings,
    pub macros: Vec<(u8, Macro)>,
    pub windows_options: Option<WindowsOptions>,
    pub windows_record: Option<Vec<u8>>,
}

impl Profile {
    pub fn from_windows(record: &[u8], firmware: u16) -> io::Result<Self> {
        let (buttons, macros) = actions::from_vendor_profile(record)?;
        Ok(Self {
            firmware,
            settings: Settings {
                dpi: Dpi::from_vendor_profile(record)?.write_bytes().to_vec(),
                timing: Timing::from_vendor_profile(record)?.write_bytes().to_vec(),
                polling: Polling::from_vendor_profile(record)?.write_bytes().to_vec(),
                buttons: buttons.to_vec(),
            },
            macros,
            windows_options: Some(WindowsOptions::from_record(record)?),
            windows_record: Some(record.to_vec()),
        })
    }

    pub fn to_windows(&self, name: &str) -> io::Result<Vec<u8>> {
        let units: Vec<_> = name.encode_utf16().collect();
        if units.is_empty() || units.len() >= 100 || units.contains(&0) {
            return Err(invalid(
                "Windows profile name must contain 1–99 UTF-16 code units",
            ));
        }
        let original = self
            .windows_record
            .as_deref()
            .map(|bytes| Self::from_windows(bytes, self.firmware))
            .transpose()?;
        let mut record = self
            .windows_record
            .clone()
            .unwrap_or_else(default_windows_record);
        if WindowsProfiles::name(&record) != name {
            record[..200].fill(0);
            for (pair, unit) in record[..200].chunks_exact_mut(2).zip(units) {
                pair.copy_from_slice(&unit.to_le_bytes());
            }
        }
        let options = self
            .windows_options
            .or_else(|| {
                original
                    .as_ref()
                    .and_then(|profile| profile.windows_options)
            })
            .unwrap_or_default();
        for (offset, value) in [
            (0x8b4, options.lift_off),
            (0x948, options.motion_sync),
            (0x93c, options.wake_mode),
        ] {
            record[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        if original
            .as_ref()
            .is_none_or(|p| p.settings.dpi != self.settings.dpi)
        {
            Dpi::parse(&self.settings.dpi)?.write_vendor_profile(&mut record)?;
        }
        if original
            .as_ref()
            .is_none_or(|p| p.settings.timing != self.settings.timing)
        {
            Timing::parse(&self.settings.timing)?.write_vendor_profile(&mut record)?;
        }
        if original
            .as_ref()
            .is_none_or(|p| p.settings.polling != self.settings.polling)
        {
            Polling::parse(&self.settings.polling)?.write_vendor_profile(&mut record)?;
        }
        let previous = original
            .as_ref()
            .map(|p| (p.settings.buttons.as_slice(), p.macros.as_slice()));
        actions::write_vendor_profile(&mut record, &self.settings.buttons, &self.macros, previous)?;
        Ok(record)
    }

    pub fn encode(&self) -> io::Result<String> {
        let mut text = format!("{PROFILE_MAGIC}\nfirmware {:04x}\n", self.firmware);
        for kind in Report::SETTINGS {
            let _ = writeln!(
                &mut text,
                "{:02x} {}",
                kind.id(),
                hex(self.settings.report(kind))
            );
        }
        if let Some(options) = self.windows_options {
            let _ = writeln!(
                &mut text,
                "windows-options {} {} {}",
                options.lift_off, options.motion_sync, options.wake_mode
            );
        }
        if let Some(record) = &self.windows_record {
            if record.len() != WINDOWS_RECORD {
                return Err(invalid("invalid retained Windows profile length"));
            }
            let _ = writeln!(&mut text, "windows-record {}", hex(record));
        }
        for (slot, value) in &self.macros {
            let text_value = value.to_string();
            actions::parse_macro(&text_value)?;
            let _ = writeln!(&mut text, "macro {slot} {}", hex(text_value.as_bytes()));
        }
        if text.len() > PROFILE_LIMIT {
            return Err(invalid("profile exceeds the native file size limit"));
        }
        Ok(text)
    }

    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        let text = std::str::from_utf8(bytes).map_err(invalid)?;
        let mut lines = text.lines();
        if lines.next() != Some(PROFILE_MAGIC) {
            return Err(invalid("unrecognized R1 profile header"));
        }
        let version = lines
            .next()
            .and_then(|line| line.strip_prefix("firmware "))
            .filter(|value| value.len() == 4)
            .ok_or_else(|| invalid("missing firmware identity"))?;
        let firmware = u16::from_str_radix(version, 16).map_err(invalid)?;
        if matches!(firmware, 0 | u16::MAX) {
            return Err(invalid("invalid firmware identity"));
        }
        let mut field = |kind: Report| -> io::Result<Vec<u8>> {
            let (id, value) = lines
                .next()
                .and_then(|line| line.split_once(' '))
                .ok_or_else(|| invalid(format!("missing {} settings", kind.name())))?;
            if u8::from_str_radix(id, 16).ok() != Some(kind.id())
                || value.len() != 2 * kind.length()
            {
                return Err(invalid(format!("invalid {} settings", kind.name())));
            }
            outgoing(kind, &decode_hex(value)?)
        };
        let settings = Settings {
            dpi: field(Report::Dpi)?,
            timing: field(Report::Timing)?,
            polling: field(Report::Polling)?,
            buttons: field(Report::Buttons)?,
        };
        actions::validate_button_assignments(&settings.buttons)?;
        validate_dpi_write(&settings.dpi)?;
        let mut macros = Vec::new();
        let mut windows_options = None;
        let mut windows_record = None;
        for line in lines.filter(|line| !line.trim().is_empty()) {
            if let Some(options) = line.strip_prefix("windows-options ") {
                let fields: Vec<_> = options.split_ascii_whitespace().collect();
                let [lift, motion, wake] = fields.as_slice() else {
                    return Err(invalid("malformed Windows options"));
                };
                if windows_options.is_some() {
                    return Err(invalid("duplicate Windows options"));
                }
                windows_options = Some(WindowsOptions {
                    lift_off: lift.parse().map_err(invalid)?,
                    motion_sync: motion.parse().map_err(invalid)?,
                    wake_mode: wake.parse().map_err(invalid)?,
                });
                continue;
            }
            if let Some(record) = line.strip_prefix("windows-record ") {
                if windows_record.is_some() || record.len() != WINDOWS_RECORD * 2 {
                    return Err(invalid(
                        "invalid or duplicate retained Windows profile record",
                    ));
                }
                windows_record = Some(decode_hex(record)?);
                continue;
            }
            let (slot, encoded) = line
                .strip_prefix("macro ")
                .and_then(|rest| rest.split_once(' '))
                .ok_or_else(|| invalid("unexpected trailing profile field"))?;
            let slot: u8 = slot.parse().map_err(invalid)?;
            if !(1..=18).contains(&slot)
                || macros.iter().any(|(previous, _)| *previous == slot)
                || encoded.len() > PROFILE_LIMIT
            {
                return Err(invalid("invalid or duplicate profile macro slot"));
            }
            let decoded = decode_hex(encoded)?;
            let text = std::str::from_utf8(&decoded).map_err(invalid)?;
            macros.push((slot, actions::parse_macro(text)?));
        }
        for button in 1..=18 {
            if let Action::Macro { slot } = actions::button_action(&settings.buttons, button)? {
                if !macros.iter().any(|(saved, _)| *saved == slot) {
                    return Err(invalid(format!(
                        "button {button} refers to missing macro slot {slot}"
                    )));
                }
            }
        }
        Ok(Self {
            firmware,
            settings,
            macros,
            windows_options,
            windows_record,
        })
    }
}

fn default_windows_record() -> Vec<u8> {
    let mut record = vec![0; WINDOWS_RECORD];
    for (offset, value) in [
        (0x898, 420),
        (0x89c, 3),
        (0x8a4, 6),
        (0x950, 3),
        (0x954, 1),
        (0x980, 0xff),
        (0x984, 0xff00),
        (0x988, 0xff0000),
        (0x98c, 0xff00ff),
        (0x990, 0xffff),
        (0x994, 0xffffff),
        (0x998, 0xffffff),
        (0x99c, 0x80ff),
    ] {
        put_vendor_word(&mut record, offset, value);
    }
    record
}

pub fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut text, "{byte:02x}");
    }
    text
}

pub(crate) fn decode_hex(value: &str) -> io::Result<Vec<u8>> {
    if !value.is_ascii() || value.len() % 2 != 0 {
        return Err(invalid("expected complete hexadecimal bytes"));
    }
    (0..value.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&value[at..at + 2], 16).map_err(invalid))
        .collect()
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

pub struct Store {
    directory: PathBuf,
    extension: &'static str,
}

impl Store {
    pub fn new(root: &Path, directory: &'static str, extension: &'static str) -> Self {
        Self {
            directory: root.join(directory),
            extension,
        }
    }

    fn path(&self, name: &str) -> io::Result<PathBuf> {
        if !valid_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "names must start with a letter or digit and use up to 64 ASCII letters, digits, dots, dashes, or underscores",
            ));
        }
        Ok(self.directory.join(format!("{name}.{}", self.extension)))
    }

    pub fn list(&self) -> io::Result<Vec<String>> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some(self.extension) {
                if let Some(name) = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .filter(|name| valid_name(name))
                {
                    names.push(name.to_owned());
                }
            }
        }
        names.sort_unstable();
        Ok(names)
    }

    pub fn read(&self, name: &str, limit: usize) -> io::Result<Vec<u8>> {
        read_file(&self.path(name)?, limit)
    }

    pub fn save(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path(name)?;
        fs::create_dir_all(&self.directory)?;
        write_new(&path, bytes)
    }

    pub fn replace(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path(name)?;
        if !fs::symlink_metadata(&path)?.is_file() {
            return Err(invalid("expected a regular file to replace"));
        }
        publish(&path, bytes, true)
    }

    pub fn delete(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.path(name)?)
    }

    pub fn rename(&self, old: &str, new: &str) -> io::Result<()> {
        let former = self.path(old)?;
        let target = self.path(new)?;
        if !fs::symlink_metadata(&former)?.is_file() {
            return Err(invalid("expected a regular file to rename"));
        }
        let old = CString::new(former.as_os_str().as_bytes()).map_err(invalid)?;
        let new = CString::new(target.as_os_str().as_bytes()).map_err(invalid)?;
        // The two C strings remain valid until the syscall returns.
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                old.as_ptr(),
                libc::AT_FDCWD,
                new.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

fn valid_name(name: &str) -> bool {
    name.len() <= 64
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

pub fn read_file(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_FLAGS)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("expected a regular file"));
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid(format!("input exceeds {limit} bytes")));
    }
    Ok(bytes)
}

fn open_stage(directory: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..64 {
        let id = TEMP_ID.fetch_add(1, Relaxed);
        let path = directory.join(format!(".r1-tmp-{}-{id}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a staging file",
    ))
}

pub fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    publish(path, bytes, false)
}

fn publish(path: &Path, bytes: &[u8], replace: bool) -> io::Result<()> {
    let directory = path
        .parent()
        .filter(|part| !part.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let (stage, mut file) = open_stage(directory)?;
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            if replace {
                fs::rename(&stage, path)
            } else {
                fs::hard_link(&stage, path)
            }
        });
    drop(file);
    if replace && result.is_ok() {
        return result;
    }
    let cleanup = fs::remove_file(stage);
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(io::Error::other(format!(
            "saved {}, but could not remove the staging link: {error}",
            path.display()
        ))),
    }
}

pub struct WindowsProfiles<'a> {
    pub active: u32,
    header: &'a [u8],
    records: &'a [u8],
    record_size: usize,
}

impl<'a> WindowsProfiles<'a> {
    pub fn encode_library(
        active: usize,
        records: impl ExactSizeIterator<Item = io::Result<Vec<u8>>>,
    ) -> io::Result<Vec<u8>> {
        let count = records.len();
        if active >= count {
            return Err(invalid("Windows active profile index is out of range"));
        }
        let size = Self::library_size(count)?;
        let mut bytes = Vec::with_capacity(size);
        bytes.resize(WINDOWS_HEADER, 0);
        let active = u32::try_from(active).map_err(invalid)?;
        bytes[..4].copy_from_slice(&active.to_le_bytes());
        for record in records {
            let record = record?;
            if record.len() != WINDOWS_RECORD {
                return Err(invalid("invalid Windows profile record length"));
            }
            bytes.extend(record);
        }
        if bytes.len() != size {
            return Err(invalid("Windows profile library record count changed"));
        }
        Ok(bytes)
    }

    fn library_size(count: usize) -> io::Result<usize> {
        count
            .checked_mul(WINDOWS_RECORD)
            .and_then(|size| size.checked_add(WINDOWS_HEADER))
            .filter(|&size| size <= WINDOWS_LIMIT)
            .ok_or_else(|| invalid("Windows profile library exceeds the size limit"))
    }

    pub fn parse(bytes: &'a [u8]) -> io::Result<Self> {
        Self::parse_layout(bytes, WINDOWS_HEADER, WINDOWS_RECORD)
    }

    pub fn for_model(bytes: &'a [u8], model: &Model) -> io::Result<Self> {
        let (header, record) = if model.extended_windows_files() {
            (EXTENDED_WINDOWS_HEADER, EXTENDED_WINDOWS_RECORD)
        } else {
            (WINDOWS_HEADER, WINDOWS_RECORD)
        };
        Self::parse_layout(bytes, header, record)
    }

    fn parse_layout(bytes: &'a [u8], header_size: usize, record_size: usize) -> io::Result<Self> {
        if bytes.len() > WINDOWS_LIMIT {
            return Err(invalid("Windows profile library exceeds the size limit"));
        }
        if bytes.len() == record_size {
            return Ok(Self {
                active: 0,
                header: &[],
                records: bytes,
                record_size,
            });
        }
        let (header, records) = bytes
            .split_at_checked(header_size)
            .ok_or_else(|| invalid("Windows profile header is truncated"))?;
        if records.is_empty() || records.len() % record_size != 0 {
            return Err(invalid("Windows profile record is missing or truncated"));
        }
        let active = u32::from_le_bytes(header[..4].try_into().expect("validated Windows header"));
        Ok(Self {
            active,
            header,
            records,
            record_size,
        })
    }

    pub fn records(&self) -> impl ExactSizeIterator<Item = &'a [u8]> {
        self.records.chunks_exact(self.record_size)
    }

    pub fn record(&self, index: usize) -> io::Result<&'a [u8]> {
        self.records()
            .nth(index.wrapping_sub(1))
            .ok_or_else(|| invalid("Windows profile index is out of range"))
    }

    pub fn with_active(&self, index: usize) -> io::Result<Vec<u8>> {
        self.record(index)?;
        if self.header.is_empty() {
            return Err(invalid(
                "active profile selection requires a complete pro.data library",
            ));
        }
        let zero_based = u32::try_from(index - 1).map_err(invalid)?;
        let mut output = self.copy();
        output[..4].copy_from_slice(&zero_based.to_le_bytes());
        Ok(output)
    }

    pub fn with_name(&self, index: usize, name: &str) -> io::Result<Vec<u8>> {
        self.record(index)?;
        let units = name.encode_utf16().collect::<Vec<_>>();
        if units.is_empty() || units.len() >= WINDOWS_NAME_BYTES / 2 || units.contains(&0) {
            return Err(invalid(
                "Windows profile name must contain 1–99 UTF-16 code units",
            ));
        }
        let folded = name.to_lowercase();
        if self
            .records()
            .enumerate()
            .any(|(i, record)| i + 1 != index && Self::name(record).to_lowercase() == folded)
        {
            return Err(invalid(
                "Windows library names must be unique without regard to case",
            ));
        }
        let mut output = self.copy();
        let offset = self.header.len() + (index - 1) * self.record_size;
        let field = &mut output[offset..offset + WINDOWS_NAME_BYTES];
        field.fill(0);
        for (pair, unit) in field.chunks_exact_mut(2).zip(units) {
            pair.copy_from_slice(&unit.to_le_bytes());
        }
        Ok(output)
    }

    fn copy(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(self.header.len() + self.records.len());
        output.extend_from_slice(self.header);
        output.extend_from_slice(self.records);
        output
    }

    pub fn name(record: &[u8]) -> String {
        let units: Vec<_> = record
            .chunks_exact(2)
            .take(WINDOWS_NAME_BYTES / 2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|&unit| unit != 0)
            .collect();
        String::from_utf16_lossy(&units)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_all_model_profile_layouts_when_renaming_and_selecting() {
        for model in crate::models::MODELS {
            let (head, size) = if model.slot <= 6 {
                (WINDOWS_HEADER, WINDOWS_RECORD)
            } else {
                (EXTENDED_WINDOWS_HEADER, EXTENDED_WINDOWS_RECORD)
            };
            let mut data = vec![0xff; head + 2 * size];
            data[..4].fill(0);
            data[head..head + WINDOWS_NAME_BYTES].fill(0);
            data[head + size..head + size + WINDOWS_NAME_BYTES].fill(0);
            data[head..head + 2].copy_from_slice(&u16::from(b'A').to_le_bytes());
            data[head + size..head + size + 2].copy_from_slice(&u16::from(b'B').to_le_bytes());

            let library = WindowsProfiles::for_model(&data, &model).unwrap();
            assert_eq!(library.active, 0);
            assert_eq!(library.records().len(), 2);
            assert_eq!(WindowsProfiles::name(library.record(1).unwrap()), "A");
            assert_eq!(library.record(2).unwrap(), &data[head + size..]);

            let renamed = library.with_name(1, "Café 🦈").unwrap();
            assert_eq!(&renamed[..head], &data[..head]);
            assert_eq!(
                &renamed[head + WINDOWS_NAME_BYTES..],
                &data[head + WINDOWS_NAME_BYTES..]
            );
            let renamed = WindowsProfiles::for_model(&renamed, &model).unwrap();
            assert_eq!(WindowsProfiles::name(renamed.record(1).unwrap()), "Café 🦈");
            assert!(library.with_name(1, "b").is_err());

            let selected = library.with_active(2).unwrap();
            assert_eq!(&selected[4..], &data[4..]);
            assert_eq!(
                WindowsProfiles::for_model(&selected, &model)
                    .unwrap()
                    .active,
                1
            );
            assert!(library.with_active(0).is_err());
            assert!(library.with_active(3).is_err());

            let single = WindowsProfiles::for_model(library.record(1).unwrap(), &model).unwrap();
            assert!(single.with_active(1).is_err());
            assert!(WindowsProfiles::for_model(&data[..data.len() - 1], &model).is_err());
        }
    }

    #[test]
    fn rejects_paths_and_truncated_profile() {
        assert!(!valid_name("../foo"));
        assert!(!valid_name(".hidden"));
        assert!(valid_name("Gaming-1"));
        assert!(Profile::parse(b"attack-shark-r1 profile 1\nfirmware 1001\n").is_err());
        assert!(WindowsProfiles::parse(&[0; 128]).is_err());
    }

    #[test]
    fn imports_hidden_options_at_the_windows_record_offsets() {
        let mut record = vec![0; WINDOWS_RECORD];
        record[0x8b4..0x8b8].copy_from_slice(&1i32.to_le_bytes());
        record[0x948..0x94c].copy_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(
            WindowsOptions::from_record(&record).unwrap(),
            WindowsOptions {
                lift_off: 1,
                motion_sync: -1,
                wake_mode: 0,
            }
        );
        assert!(WindowsOptions::from_record(&record[..0x948]).is_err());
    }

    #[test]
    fn profile_round_trips() {
        let profile = Profile {
            firmware: 0x1001,
            settings: Settings::defaults(),
            macros: Vec::new(),
            windows_options: None,
            windows_record: None,
        };
        let text = profile.encode().unwrap();
        assert_eq!(
            Profile::parse(text.as_bytes()).unwrap().encode().unwrap(),
            text
        );
    }

    #[test]
    fn windows_profile_round_trips_settings_and_an_embedded_macro() {
        let mut settings = Settings::defaults();
        let mut dpi = Dpi::parse(&settings.dpi).unwrap();
        dpi.set_stage_dpi(1, 10_100).unwrap();
        dpi.set_stage_dpi(6, 18_000).unwrap();
        settings.dpi = dpi.write_bytes().to_vec();
        let mut timing = Timing::parse(&settings.timing).unwrap();
        timing.set_light_mode(1).unwrap();
        timing.set_sleep_seconds(450).unwrap();
        settings.timing = timing.write_bytes().to_vec();
        let mut polling = Polling::parse(&settings.polling).unwrap();
        polling.set_hz(500).unwrap();
        settings.polling = polling.write_bytes().to_vec();
        actions::set_button_action(&mut settings.buttons, 7, Action::Macro { slot: 7 }).unwrap();
        let value =
            actions::parse_macro("mode hold\nrepeat 3\nmetadata 01,02,03\ntap a 25").unwrap();
        let mut profile = Profile {
            firmware: 0x1001,
            settings,
            macros: vec![(7, value)],
            windows_options: Some(WindowsOptions {
                lift_off: 1,
                motion_sync: 1,
                wake_mode: 1,
            }),
            windows_record: None,
        };
        let name = "R1 profile with a name longer than thirty units";
        let mut bytes = profile.to_windows(name).unwrap();
        bytes[0x234] = 0x5a;
        assert_eq!(WindowsProfiles::name(&bytes), name);
        let converted = Profile::from_windows(&bytes, profile.firmware).unwrap();
        let mut native = Profile::parse(converted.encode().unwrap().as_bytes()).unwrap();
        assert_eq!(native.to_windows(name).unwrap(), bytes);
        native.windows_options.as_mut().unwrap().lift_off = 0;
        bytes[0x8b4] = 0;
        assert_eq!(native.to_windows(name).unwrap(), bytes);

        profile.macros[0].1 = actions::parse_macro("extended 200 0001fffe").unwrap();
        assert!(profile.to_windows(name).is_err());
    }
}
