use std::{
    io::{self, Write},
    path::Path,
    time::Duration,
};

use shark_driver::{
    actions::{self, Action, Macro},
    driver::{Connection, Report},
    macro_wire,
    operations::{self, Change},
    profiles::{self, Store, TEXT_LIMIT, WINDOWS_LIMIT},
};

use super::{
    args::{Emulation, Options, flag_pairs, input, number},
    files::{print_names, windows_library_names, windows_model, write_file},
    settings::changed,
};

pub(super) fn install_macro(
    options: &Options,
    button: usize,
    value: &Macro,
    out: &mut dyn Write,
) -> io::Result<()> {
    let slot = button as u8;
    let mut driver = options.open()?;
    let before = operations::read_setting(&mut driver, Report::Buttons)?;
    let mut after = before.clone();
    actions::set_button_action(&mut after, button, Action::Macro { slot })?;
    let assignment = Change::new(Report::Buttons, &before, &after)?;
    let mut reports = Vec::new();
    macro_wire::author(&mut driver, slot, value, |driver| {
        reports = operations::apply(driver, &[assignment])?;
        Ok(())
    })?;
    changed(out, reports)?;
    writeln!(out, "Verified macro in slot {slot}.")
}

pub(super) fn macro_text(bytes: &[u8]) -> io::Result<Macro> {
    let text = std::str::from_utf8(bytes).map_err(|error| input(error.to_string()))?;
    actions::parse_macro(text)
}

pub(super) fn upload_macro(
    options: &Options,
    slot: u8,
    value: &Macro,
    out: &mut dyn Write,
) -> io::Result<()> {
    let pages = macro_wire::planned_slots(slot, value)?.count();
    macro_wire::author(&mut options.open()?, slot, value, |_| Ok(()))?;
    if pages == 1 {
        writeln!(out, "Verified macro in slot {slot}.")
    } else {
        writeln!(out, "Verified macro chain at slot {slot} ({pages} pages).")
    }
}

fn macro_file(file: &str) -> io::Result<Macro> {
    macro_text(&profiles::read_file(Path::new(file), TEXT_LIMIT)?)
}

pub(super) fn macro_command(
    options: &Options,
    args: &[&str],
    out: &mut dyn Write,
) -> io::Result<()> {
    let store = Store::new(&options.data, "macros", "r1macro");
    let stored = |name: &str| -> io::Result<Macro> { macro_text(&store.read(name, TEXT_LIMIT)?) };
    match args {
        ["inspect-windows", file, flags @ ..] => {
            let data = profiles::read_file(Path::new(file), WINDOWS_LIMIT)?;
            for (index, (name, _)) in
                actions::import_windows_macros(&data, windows_macro_layout(flags)?)?
                    .iter()
                    .enumerate()
            {
                writeln!(out, "{}: {}", index + 1, name.escape_default())?;
            }
            Ok(())
        }
        ["list"] => print_names(out, &store.list()?),
        ["new", name] => store.save(name, Macro::default().to_string().as_bytes()),
        ["show", name] => write!(out, "{}", stored(name)?),
        ["delete", name] => store.delete(name),
        ["rename", old, new] => store.rename(old, new),
        ["edit", name, args @ ..] => {
            let mut value = stored(name)?;
            actions::edit_macro(&mut value, args)?;
            store.replace(name, value.to_string().as_bytes())
        }
        [mode @ ("import" | "update"), name, file] => {
            let text = macro_file(file)?.to_string();
            if *mode == "update" {
                store.replace(name, text.as_bytes())
            } else {
                store.save(name, text.as_bytes())
            }
        }
        ["import-windows", name, file, args @ ..] => {
            let (index, flags) = match args {
                [index, rest @ ..] if !index.starts_with("--") => {
                    (number::<usize>(index, "macro index")?, rest)
                }
                _ => (1, args),
            };
            let data = profiles::read_file(Path::new(file), WINDOWS_LIMIT)?;
            let macros = actions::import_windows_macros(&data, windows_macro_layout(flags)?)?;
            let (_, value) = macros
                .get(index.wrapping_sub(1))
                .ok_or_else(|| input("Windows macro index is out of range"))?;
            store.save(name, value.to_string().as_bytes())
        }
        ["export-windows", name, file, flags @ ..] => write_file(
            out,
            file,
            &actions::export_windows_macro(name, &stored(name)?, windows_macro_layout(flags)?)?,
        ),
        ["export-windows-library", file, args @ ..] => {
            let (flags, names) = match args {
                ["--model", _, names @ ..] => (&args[..2], names),
                _ => (&args[..0], args),
            };
            if names.is_empty() {
                return Err(input("select at least one macro to export"));
            }
            windows_library_names(names)?;
            let layout = windows_macro_layout(flags)?;
            if names.len() > WINDOWS_LIMIT / layout.record_len() {
                return Err(input("Windows macro library exceeds the size limit"));
            }
            let mut bytes = Vec::with_capacity(names.len() * layout.record_len());
            for name in names {
                bytes.extend(actions::export_windows_macro(name, &stored(name)?, layout)?);
            }
            write_file(out, file, &bytes)
        }
        ["export", name, file] => write_file(out, file, stored(name)?.to_string().as_bytes()),
        ["record", name, args @ ..] => {
            let end = args
                .iter()
                .position(|arg| arg.starts_with("--"))
                .unwrap_or(args.len());
            let (devices, flags) = args.split_at(end);
            let flags = flag_pairs(
                flags,
                &["--events", "--timeout-ms", "--fixed-delay-ms"],
                true,
            )?;
            let count = flags
                .get("--events")
                .map(|v| number(v, "event count"))
                .transpose()?
                .unwrap_or(20);
            let millis = flags
                .get("--timeout-ms")
                .map(|v| number(v, "timeout"))
                .transpose()?
                .unwrap_or(5000);
            let fixed_delay = flags
                .get("--fixed-delay-ms")
                .map(|v| number(v, "fixed delay"))
                .transpose()?;
            let devices: Vec<_> = devices.iter().map(Path::new).collect();
            let capture =
                actions::record_paths(&devices, count, Duration::from_millis(millis), fixed_delay)?;
            store.save(name, capture.to_string().as_bytes())?;
            writeln!(out, "Saved {} events in {name}.", capture.events.len())
        }
        ["preview", slot, file] => {
            let slot = actions::parse_button(slot)? as u8;
            let connection = if options.bluetooth.is_some()
                || matches!(options.emulation, Some(Emulation::Bluetooth))
            {
                Connection::Bluetooth
            } else {
                options.connection.unwrap_or(Connection::Wired)
            };
            let pages = macro_wire::plan(connection, slot, &macro_file(file)?)?;
            if let [(_, page)] = pages.as_slice() {
                return writeln!(out, "{}", profiles::hex(page));
            }
            for (slot, page) in pages {
                writeln!(out, "{slot} {}", profiles::hex(&page))?;
            }
            Ok(())
        }
        ["read", slot] | ["read", slot, _] => {
            let slot = actions::parse_button(slot)? as u8;
            let value = macro_wire::read_decoded(&mut options.open()?, slot)?;
            write_file(
                out,
                args.get(2).copied().unwrap_or("-"),
                value.to_string().as_bytes(),
            )
        }
        ["backup", file] | ["backup", file, _] => {
            let mut driver = options.open()?;
            let slots = if let Some(slot) = args.get(2) {
                let slot = number::<u8>(slot, "macro slot")?;
                slot..=slot
            } else {
                1..=driver.macro_slots()
            };
            let mut contents = Vec::new();
            writeln!(&mut contents, "{}", macro_wire::BACKUP_HEADER)?;
            writeln!(&mut contents, "firmware {:04x}", driver.firmware()?)?;
            for slot in slots {
                match driver.read_macro(slot) {
                    Ok(page) => writeln!(&mut contents, "{slot} {}", profiles::hex(&page))?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        writeln!(&mut contents, "{slot} empty")?;
                    }
                    Err(error) => {
                        return Err(io::Error::new(
                            error.kind(),
                            format!("macro slot {slot}: {error}"),
                        ));
                    }
                }
            }
            write_file(out, file, &contents)
        }
        ["verify-backup", file] => {
            let source = profiles::read_file(Path::new(file), TEXT_LIMIT)?;
            let source = std::str::from_utf8(&source).map_err(|_| input("backup must be UTF-8"))?;
            let count = macro_wire::verify_backup(&mut options.open()?, source)?;
            writeln!(out, "Verified {count} macro slots.")
        }
        ["backup-device-pages", slot, file] => {
            let slot = number::<u8>(slot, "macro slot")?;
            if let Some(address) = options.bluetooth.as_deref() {
                let mut peer = shark_driver::bluetooth::open_model(address)?;
                let archive = shark_driver::model_wire::archive_model_macro(
                    peer.model(),
                    &mut peer,
                    shark_driver::model_wire::bluetooth_width,
                    slot,
                )?;
                return write_file(out, file, &archive);
            }
            let device = options
                .model_device()?
                .ok_or_else(|| input("select a detected mouse with macro readback"))?;
            let archive = shark_driver::linux::archive_model_macro(
                &device,
                options.expected.as_deref(),
                slot,
            )?;
            write_file(out, file, &archive)
        }
        ["restore-device-pages", file] => {
            let archive = profiles::read_file(
                Path::new(file),
                shark_driver::model_wire::MACRO_ARCHIVE_LIMIT,
            )?;
            if let Some(address) = options.bluetooth.as_deref() {
                let mut peer = shark_driver::bluetooth::open_model(address)?;
                let count = shark_driver::model_wire::restore_model_macro(
                    peer.model(),
                    &mut peer,
                    shark_driver::model_wire::bluetooth_width,
                    &archive,
                )?;
                return writeln!(out, "Verified {count} macro pages.");
            }
            let device = options
                .model_device()?
                .ok_or_else(|| input("select a detected mouse with macro readback"))?;
            let count = shark_driver::linux::restore_model_macro(
                &device,
                options.expected.as_deref(),
                &archive,
            )?;
            writeln!(out, "Verified {count} macro pages.")
        }
        ["read-device-pages", slot, file] => {
            let slot = number::<u8>(slot, "macro slot")?;
            let device = options
                .v3_family()?
                .ok_or_else(|| input("select a wired V3 Pro, V6, or V3"))?;
            let pages = shark_driver::linux::read_v3_family_macro(
                &device,
                options.expected.as_deref(),
                slot,
            )?;
            let mut archive = Vec::new();
            for page in pages {
                writeln!(&mut archive, "{} {}", page[2], profiles::hex(&page))?;
            }
            write_file(out, file, &archive)
        }
        ["read-raw", slot] | ["read-raw", slot, _] => {
            let slot = actions::parse_button(slot)? as u8;
            let mut bytes = Vec::new();
            let mut driver = options.open()?;
            writeln!(&mut bytes, "firmware {:04x}", driver.firmware()?)?;
            for (index, page) in macro_wire::read(&mut driver, slot)? {
                writeln!(&mut bytes, "{index} {}", profiles::hex(&page))?;
            }
            write_file(out, args.get(2).copied().unwrap_or("-"), &bytes)
        }
        ["restore-raw", slot, file] => {
            let slot = actions::parse_button(slot)? as u8;
            let bytes = profiles::read_file(Path::new(file), TEXT_LIMIT)?;
            let source = std::str::from_utf8(&bytes).map_err(|_| input("archive must be UTF-8"))?;
            let written = macro_wire::restore(&mut options.open()?, slot, source)?;
            writeln!(
                out,
                "Verified macro chain in slot {slot} ({written} pages written)."
            )
        }
        ["upload", slot, file] => {
            let slot = actions::parse_button(slot)? as u8;
            let value = macro_file(file)?;
            upload_macro(options, slot, &value, out)
        }
        ["install", button, file] => install_macro(
            options,
            actions::parse_button(button)?,
            &macro_file(file)?,
            out,
        ),
        ["assign", button, name] => {
            install_macro(options, actions::parse_button(button)?, &stored(name)?, out)
        }
        _ => Err(input("usage: macro <command>; see help macro")),
    }
}

fn windows_macro_layout(args: &[&str]) -> io::Result<actions::WindowsMacroLayout> {
    if windows_model(args)?.is_some_and(|model| model.extended_windows_files()) {
        Ok(actions::WindowsMacroLayout::Extended)
    } else {
        Ok(actions::WindowsMacroLayout::Compact)
    }
}
