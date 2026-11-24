//! Recovery for the Beken USB bootloader observed on an R1.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const FLAG: u32 = 0x7d000;
const FLAG_BYTES: [u8; 16] = [0x12, 0x34, 0xaa, 0, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
const START: u32 = 0x20000;
const END: u32 = FLAG + 0x1000;
const R1_USB_DESCRIPTOR: [u8; 12] = [18, 1, 0x10, 1, 0, 0, 0, 0x40, 0x57, 0x1d, 0x61, 0xfa];
const BACKUP_HEADERS: [u32; 3] = [0x40000, 0x41000, 0x52000];
const APP_HEADERS: [u32; 2] = [0x2b00a, 0x2c060];
const LINK_ACK: [u8; 8] = [4, 0x0e, 5, 1, 0xe0, 0xfc, 1, 0];
const READ_ACK: [u8; 10] = [4, 0x0e, 0xff, 1, 0xe0, 0xfc, 0xf4, 6, 0x10, 9];
const CRC_ACK: [u8; 7] = [4, 0x0e, 8, 1, 0xe0, 0xfc, 0x10];
const BOOT_DESCRIPTOR: [u8; 33] = [
    0x05, 0x0c, 0x09, 1, 0xa1, 1, 0x09, 0, 0x75, 8, 0x95, 0x40, 0x15, 0, 0x26, 0xff, 0, 0x81, 2,
    0x09, 0, 0x75, 8, 0x95, 0x40, 0x15, 0, 0x26, 0xff, 0, 0x91, 2, 0xc0,
];

#[derive(Debug)]
pub struct Endpoint {
    pub node: PathBuf,
    hid: PathBuf,
    usb: PathBuf,
}

impl Endpoint {
    pub fn port(&self) -> &Path {
        &self.usb
    }
}

pub fn discover() -> io::Result<Vec<Endpoint>> {
    let entries = match fs::read_dir("/sys/class/hidraw") {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut endpoints = Vec::new();
    for entry in entries {
        let entry = entry?;
        match inspect(&entry.path()) {
            Ok(Some(endpoint)) => endpoints.push(endpoint),
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    endpoints.sort_unstable_by(|a, b| a.node.cmp(&b.node));
    Ok(endpoints)
}

fn inspect(entry: &Path) -> io::Result<Option<Endpoint>> {
    let hid = fs::canonicalize(entry.join("device"))?;
    let event = fs::read_to_string(hid.join("uevent"))?;
    if !event
        .lines()
        .any(|line| line == "HID_ID=0003:0000A745:00000033")
    {
        return Ok(None);
    }
    if fs::read(hid.join("report_descriptor"))? != BOOT_DESCRIPTOR {
        return Ok(None);
    }
    let usb = hid
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| invalid("missing USB port"))?;
    if usb_id(usb)? != (0xa745, 0x0033) {
        return Ok(None);
    }
    let usb = usb.to_owned();
    let node = Path::new("/dev").join(
        entry
            .file_name()
            .ok_or_else(|| invalid("missing hidraw name"))?,
    );
    Ok(Some(Endpoint { node, hid, usb }))
}

fn usb_id(path: &Path) -> io::Result<(u16, u16)> {
    let read = |attribute| {
        u16::from_str_radix(fs::read_to_string(path.join(attribute))?.trim(), 16)
            .map_err(|_| invalid("malformed USB ID"))
    };
    Ok((read("idVendor")?, read("idProduct")?))
}

struct Bootloader(File);

impl Bootloader {
    fn open(endpoint: &Endpoint) -> io::Result<Self> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&endpoint.node)?;
        let meta = file.metadata()?;
        if !meta.file_type().is_char_device() {
            return Err(invalid("expected a hidraw character device"));
        }
        let (major, minor) = (libc::major(meta.rdev()), libc::minor(meta.rdev()));
        let hid = fs::canonicalize(format!("/sys/dev/char/{major}:{minor}/device"))?;
        if hid != endpoint.hid
            || fs::read(hid.join("report_descriptor"))? != BOOT_DESCRIPTOR
            || usb_id(&endpoint.usb)? != (0xa745, 0x0033)
            || fs::symlink_metadata(&endpoint.node)?.ino() != meta.ino()
        {
            return Err(invalid("bootloader changed since discovery"));
        }
        // SAFETY: The file owns the descriptor for the duration of the flock call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(file))
    }

    fn send(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut report = [0; 65];
        report[1..1 + payload.len()].copy_from_slice(payload);
        if self.0.write(&report)? != report.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short bootloader output report",
            ));
        }
        Ok(())
    }

    fn receive(&mut self, deadline: Instant) -> io::Result<Option<[u8; 64]>> {
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            let mut fd = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = remaining.as_nanos().div_ceil(1_000_000) as i32;
            // SAFETY: poll may mutate one initialized pollfd and the file remains open.
            match unsafe { libc::poll(&mut fd, 1, ms) } {
                0 => return Ok(None),
                -1 => {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                _ if fd.revents & libc::POLLIN == 0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "bootloader disconnected",
                    ));
                }
                _ => {}
            }
            let mut report = [0; 64];
            match self.0.read(&mut report) {
                Ok(64) => return Ok(Some(report)),
                Ok(_) => return Err(invalid("short bootloader input report")),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    fn link(&mut self) -> io::Result<()> {
        self.send(&[1, 0xe0, 0xfc, 1, 0])?;
        let end = Instant::now() + Duration::from_secs(1);
        while let Some(reply) = self.receive(end)? {
            if reply.starts_with(&LINK_ACK) {
                return Ok(());
            }
        }
        Err(invalid("Beken link response missing"))
    }

    fn read16(&mut self, address: u32) -> io::Result<[u8; 16]> {
        let mut command = [1, 0xe0, 0xfc, 0xff, 0xf4, 5, 0, 9, 0, 0, 0, 0];
        command[8..].copy_from_slice(&address.to_le_bytes());
        for _ in 0..3 {
            self.send(&command)?;
            let end = Instant::now() + Duration::from_secs(1);
            while let Some(reply) = self.receive(end)? {
                if reply.starts_with(&READ_ACK) && reply[11..15] == command[8..] {
                    if reply[10] != 0 {
                        return Err(invalid(format!("bootloader refused read at {address:#x}")));
                    }
                    return Ok(reply[15..31].try_into().unwrap());
                }
            }
        }
        Err(invalid(format!("no bootloader read reply at {address:#x}")))
    }

    fn clear_flag(&mut self) -> io::Result<()> {
        // BK3633 CRC command: erase the stay flag sector, check one block, then restart.
        self.send(&[1, 0xe0, 0xfc, 9, 0x10, 0, 0xb0, 2, 0, 0xff, 0xb0, 2, 0])
    }
}

#[derive(Debug)]
struct Image {
    address: u32,
    version: u16,
    crc: u32,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn crc32_raw(bytes: &[u8]) -> u32 {
    bytes.iter().fold(u32::MAX, |mut crc, byte| {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
        crc
    })
}

fn verified_image(dump: &[u8]) -> io::Result<Image> {
    if dump.len() != (END - START) as usize {
        return Err(invalid("short firmware backup"));
    }
    for address in BACKUP_HEADERS {
        let index = (address - START) as usize;
        if matches!(&dump[index + 8..index + 12], b"BBBB" | b"SSSS") {
            return Err(invalid(format!(
                "possible staged firmware image at {address:#x}"
            )));
        }
    }
    let mut image = None;
    for address in APP_HEADERS {
        let index = (address - START) as usize;
        let header = &dump[index..index + 16];
        if &header[8..12] != b"BBBB" {
            continue;
        }
        let crc = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let version = u16::from_le_bytes(header[4..6].try_into().unwrap());
        let words = u16::from_le_bytes(header[6..8].try_into().unwrap());
        if !matches!((header[12], header[13]), (0xff, 0xff) | (0xaa, 0xaa))
            || !matches!(words, 0x80..=0xa800)
            || matches!(crc, 0 | u32::MAX)
            || version == u16::MAX
            || header[14..16] == [0xff; 2]
        {
            return Err(invalid(format!("invalid firmware header at {address:#x}")));
        }
        let payload = dump
            .get(index + 16..index + usize::from(words) * 4)
            .filter(|_| address + u32::from(words) * 4 <= FLAG)
            .ok_or_else(|| invalid("application image extends past backup"))?;
        if crc32_raw(payload) != crc {
            return Err(invalid(format!("firmware CRC mismatch at {address:#x}")));
        }
        if payload[payload.len() - 256..]
            .iter()
            .all(|&byte| byte == 0xff)
        {
            return Err(invalid("firmware ends in a blank flash block"));
        }
        if payload
            .chunks_exact(16)
            .filter(|row| row.iter().all(|&byte| byte == 0xff))
            .count()
            > payload.len() / 64
        {
            return Err(invalid("over a quarter of the firmware rows are blank"));
        }
        let code: Vec<_> = payload
            .chunks(34)
            .flat_map(|block| block.iter().take(32).copied())
            .collect();
        if !code
            .windows(R1_USB_DESCRIPTOR.len())
            .any(|bytes| bytes == R1_USB_DESCRIPTOR)
        {
            return Err(invalid(
                "verified firmware lacks the R1 wired USB device descriptor",
            ));
        }
        if image
            .replace(Image {
                address,
                version,
                crc,
            })
            .is_some()
        {
            return Err(invalid("multiple verified app headers"));
        }
    }
    image.ok_or_else(|| invalid("no supported application header with verifiable CRC"))
}

pub fn run(endpoint: &Endpoint, backup: &Path, apply: bool, out: &mut dyn Write) -> io::Result<()> {
    run_with(endpoint, apply, out, |dump, out| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(backup)?;
        file.write_all(dump)?;
        file.sync_all()?;
        let parent = backup
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        File::open(parent)?.sync_all()?;
        writeln!(out, "Saved {} bytes to {}", dump.len(), backup.display())
    })
}

pub fn run_with(
    endpoint: &Endpoint,
    apply: bool,
    out: &mut dyn Write,
    backup: impl FnOnce(&[u8], &mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    let mut boot = Bootloader::open(endpoint)?;
    writeln!(out, "Beken recovery on {}", endpoint.port().display())?;
    out.flush()?;
    boot.link()?;
    let flag = boot.read16(FLAG)?;
    writeln!(out, "Bootloader flag at {FLAG:#x} = {flag:02x?}")?;
    if flag != FLAG_BYTES {
        return Err(invalid("unexpected stay-in-bootloader flag contents"));
    }
    let mut dump = Vec::with_capacity((END - START) as usize);
    for address in (START..END).step_by(16) {
        dump.extend_from_slice(&boot.read16(address)?);
        if (address - START) % 0x4000 == 0 {
            writeln!(out, "Read flash through {:#x}", address + 15)?;
            out.flush()?;
        }
    }
    if dump[(FLAG - START) as usize..][..16] != flag {
        return Err(invalid("bootloader flag changed during backup"));
    }
    backup(&dump, out)?;
    let image = verified_image(&dump)?;
    writeln!(
        out,
        "Verified firmware at {:#x}, version {:#x}, CRC {:08x}",
        image.address, image.version, image.crc
    )?;
    let stored_macro_data = dump[(FLAG - START + 16) as usize..]
        .iter()
        .any(|&byte| byte != 0xff);
    writeln!(
        out,
        "Possible macro data in shared flag sector: {stored_macro_data}"
    )?;
    if !apply {
        return Ok(());
    }
    if stored_macro_data {
        return Err(invalid(
            "the flag sector also contains possible macro data; automatic erase stopped after backup",
        ));
    }
    writeln!(
        out,
        "Type RECOVER to erase the shared flag and macro sector and restart this mouse."
    )?;
    out.flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim() != "RECOVER" {
        return Ok(());
    }
    if boot.read16(FLAG)? != flag {
        return Err(invalid("bootloader flag changed after verification"));
    }
    boot.clear_flag()?;
    let ack = boot.receive(Instant::now() + Duration::from_secs(3));
    writeln!(
        out,
        "Bootloader CRC acknowledgement: {}",
        matches!(ack, Ok(Some(reply)) if reply.starts_with(&CRC_ACK))
    )?;
    drop(boot);

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if matches!(usb_id(endpoint.port()), Ok((0x1d57, 0xfa61))) {
            writeln!(
                out,
                "R1 wired USB identity 1d57:fa61 restored on the same port."
            )?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(io::Error::other(
        "R1 wired USB identity did not return on the original port",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        let mut dump = vec![0xff; (END - START) as usize];
        let index = (APP_HEADERS[0] - START) as usize;
        dump[index + 16..index + 512].fill(0x41);
        dump[index + 16 + 26..index + 16 + 32].copy_from_slice(&R1_USB_DESCRIPTOR[..6]);
        dump[index + 16 + 34..index + 16 + 40].copy_from_slice(&R1_USB_DESCRIPTOR[6..]);
        let crc = crc32_raw(&dump[index + 16..index + 512]);
        dump[index..index + 16].copy_from_slice(&[
            crc as u8,
            (crc >> 8) as u8,
            (crc >> 16) as u8,
            (crc >> 24) as u8,
            1,
            0,
            0x80,
            0,
            b'B',
            b'B',
            b'B',
            b'B',
            0xff,
            0xff,
            1,
            0,
        ]);
        dump
    }

    #[test]
    fn validates_the_full_payload_and_rejects_staged_updates() {
        assert_eq!(crc32_raw(b"123456789"), 0x340bc6d9);
        let mut dump = fixture();
        assert_eq!(verified_image(&dump).unwrap().address, APP_HEADERS[0]);
        let index = (APP_HEADERS[0] - START) as usize;
        dump[index + 511] ^= 1;
        assert!(
            verified_image(&dump)
                .unwrap_err()
                .to_string()
                .contains("CRC mismatch")
        );
        let mut dump = fixture();
        let backup = (BACKUP_HEADERS[0] - START) as usize;
        dump[backup + 8..backup + 12].copy_from_slice(b"SSSS");
        assert!(
            verified_image(&dump)
                .unwrap_err()
                .to_string()
                .contains("staged")
        );
    }
}
