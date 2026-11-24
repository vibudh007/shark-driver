use std::{
    io::{self, Write},
    path::Path,
};

use shark_driver::{
    actions::{self, Action, Macro},
    driver::{Connection, Driver, Report},
    macro_wire,
    operations::{self, Settings},
    profiles::{self, PROFILE_LIMIT, Profile, Store, WINDOWS_LIMIT, WindowsProfiles},
    settings::Polling,
};

use super::{
    args::{Options, flag_pairs, input, number, toggle},
    files::{print_names, windows_library_names, windows_model, write_file},
    settings::{changed, show_settings},
};

pub(super) fn capture_profile(driver: &mut Driver) -> io::Result<Profile> {
    let (firmware, settings) = operations::capture(driver)?;
    let mut macros = Vec::new();
    for button in 1..=18 {
        if let Action::Macro { slot } = actions::button_action(&settings.buttons, button)? {
            if !macros.iter().any(|(saved, _)| *saved == slot) {
                macros.push((slot, macro_wire::read_decoded(driver, slot)?));
            }
        }
    }
    Ok(Profile {
        firmware,
        settings,
        macros,
        windows_options: None,
        windows_record: None,
    })
}

pub(super) fn default_profile(firmware: u16) -> Profile {
    Profile {
        firmware,
        settings: Settings::defaults(),
        macros: Vec::new(),
        windows_options: None,
        windows_record: None,
    }
}

fn store_capture(
    store: &Store,
    name: &str,
    mut captured: Profile,
    replace: bool,
) -> io::Result<()> {
    if replace {
        let previous = Profile::parse(&store.read(name, PROFILE_LIMIT)?)?;
        captured.windows_options = previous.windows_options;
        captured.windows_record = previous.windows_record;
        store.replace(name, captured.encode()?.as_bytes())
    } else {
        store.save(name, captured.encode()?.as_bytes())
    }
}

fn apply_saved(driver: &mut Driver, profile: &Profile) -> io::Result<(Vec<Report>, usize)> {
    let firmware = driver.firmware()?;
    if firmware != driver.firmware_identity(profile.firmware) {
        return Err(input(format!(
            "profile targets firmware 0x{:04x}; this mouse reports 0x{firmware:04x}",
            profile.firmware
        )));
    }
    if driver.connection() == Connection::Bluetooth {
        let current = operations::read_setting(driver, Report::Polling)?;
        if Polling::parse(&current)?.hz() != Polling::parse(&profile.settings.polling)?.hz() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this profile changes polling; apply it over USB or the receiver",
            ));
        }
    }
    let mut occupied = [false; 19];
    for (head, value) in &profile.macros {
        for slot in macro_wire::planned_slots(*head, value)? {
            if std::mem::replace(&mut occupied[usize::from(slot)], true) {
                return Err(input(format!("profile macros overlap at slot {slot}")));
            }
        }
    }
    apply_profile_macros(driver, profile, &profile.macros)
}

pub(super) fn apply_and_report(
    options: &Options,
    profile: &Profile,
    out: &mut dyn Write,
) -> io::Result<()> {
    let (reports, macros) = apply_saved(&mut options.open()?, profile)?;
    changed(out, reports)?;
    if macros > 0 {
        writeln!(out, "Verified {macros} macro slots.")?;
    }
    Ok(())
}

fn apply_profile_macros(
    driver: &mut Driver,
    profile: &Profile,
    macros: &[(u8, Macro)],
) -> io::Result<(Vec<Report>, usize)> {
    let Some(((slot, value), rest)) = macros.split_first() else {
        return operations::apply_profile(driver, profile.firmware, &profile.settings)
            .map(|reports| (reports, 0));
    };
    let mut applied = (Vec::new(), 0);
    let written = macro_wire::author(driver, *slot, value, |driver| {
        applied = apply_profile_macros(driver, profile, rest)?;
        Ok(())
    })?;
    Ok((applied.0, applied.1 + written))
}

pub(super) fn profile_command(
    options: &Options,
    args: &[&str],
    out: &mut dyn Write,
) -> io::Result<()> {
    let store = Store::new(&options.data, "profiles", "r1profile");
    let stored =
        |name| -> io::Result<Profile> { Profile::parse(&store.read(name, PROFILE_LIMIT)?) };
    match args {
        ["read-bluetooth", file] => {
            let address = options
                .bluetooth
                .as_deref()
                .ok_or_else(|| input("select a paired mouse with --bluetooth ADDRESS"))?;
            let report = shark_driver::bluetooth::read_settings_notification(address)?;
            write_file(out, file, &report)
        }
        [
            "plan-windows" | "plan-fast-receiver",
            file,
            index,
            flags @ ..,
        ] => {
            let model = windows_model(flags)?.ok_or_else(|| input("select --model SLOT"))?;
            let index: usize = number(index, "Windows profile index")?;
            let data = profiles::read_file(Path::new(file), WINDOWS_LIMIT)?;
            let plan = shark_driver::vendor_plan::Plan::from_windows(model, &data, index)?;
            let connection = if options.bluetooth.is_some() {
                Connection::Bluetooth
            } else {
                options.connection.unwrap_or(Connection::Wired)
            };
            writeln!(out, "Windows full-apply packets for {}", model.name)?;
            if args[0] == "plan-fast-receiver" {
                for packet in plan.fast_receiver_reports(model)? {
                    writeln!(out, "{}", profiles::hex(&packet))?;
                }
            } else {
                for packet in plan.reports(connection, true) {
                    writeln!(out, "{}", profiles::hex(packet))?;
                }
            }
            Ok(())
        }
        ["apply-windows", file, index, "--model", slot] => {
            let index = number(index, "Windows profile index")?;
            let slot = number::<u8>(slot, "Windows model slot")?;
            let device = options
                .model_device()?
                .filter(|device| device.model.is_some_and(|model| model.slot == slot))
                .ok_or_else(|| input("select a wired mouse matching the Windows model slot"))?;
            let data = profiles::read_file(Path::new(file), WINDOWS_LIMIT)?;
            let plan =
                shark_driver::vendor_plan::Plan::from_windows(device.model.unwrap(), &data, index)?;
            let groups = shark_driver::linux::apply_model_windows(
                &device,
                options.expected.as_deref(),
                &plan,
            )?;
            writeln!(out, "Verified {groups} settings groups.")
        }
        ["backup-device" | "restore-device", file] => {
            if let Some(address) = options.bluetooth.as_deref() {
                let mut peer = shark_driver::bluetooth::open_model(address)?;
                if args[0] == "backup-device" {
                    let archive = shark_driver::model_wire::archive_model_profile(
                        peer.model(),
                        &mut peer,
                        shark_driver::model_wire::bluetooth_width,
                    )?;
                    return write_file(out, file, &archive);
                }
                let archive = profiles::read_file(Path::new(file), 135)?;
                let groups = shark_driver::model_wire::restore_model_profile(
                    peer.model(),
                    &mut peer,
                    shark_driver::model_wire::bluetooth_width,
                    &archive,
                )?;
                return writeln!(out, "Verified {groups} settings groups.");
            }
            let device = options
                .model_device()?
                .ok_or_else(|| input("select a wired model with settings readback"))?;
            if args[0] == "backup-device" {
                let archive = shark_driver::linux::archive_model_profile(
                    &device,
                    options.expected.as_deref(),
                )?;
                write_file(out, file, &archive)
            } else {
                let archive = profiles::read_file(Path::new(file), 135)?;
                let groups = shark_driver::linux::restore_model_profile(
                    &device,
                    options.expected.as_deref(),
                    &archive,
                )?;
                writeln!(out, "Verified {groups} settings groups.")
            }
        }
        ["read-device", file] => {
            if options.emulation.is_some() || options.bluetooth.is_some() {
                return Err(input("this command reads a wired combined profile"));
            }
            let devices = options.detected()?;
            let [device] = devices.as_slice() else {
                return Err(input(
                    "select one wired mouse with a combined profile report",
                ));
            };
            let profile =
                shark_driver::linux::read_model_profile(device, options.expected.as_deref())?;
            write_file(out, file, profile.bytes())
        }
        ["device-info"] => {
            let (active, count) = options.open()?.profile_range()?;
            writeln!(out, "Active profile: {active} of {count}")
        }

        ["list"] => print_names(out, &store.list()?),
        ["show", name] => {
            let profile = stored(name)?;
            show_settings(out, profile.firmware, &profile.settings)?;
            writeln!(out, "Saved macro slots: {}", profile.macros.len())
        }
        [mode @ ("save" | "update"), name] => {
            let captured = capture_profile(&mut options.open()?)?;
            store_capture(&store, name, captured, *mode == "update")
        }
        ["store-capture", mode @ ("save" | "update"), name] => {
            let data = super::desktop::payload(PROFILE_LIMIT)?;
            store_capture(
                &store,
                name,
                Profile::parse(data.as_bytes())?,
                *mode == "update",
            )
        }
        ["defaults", name] => {
            let profile = default_profile(options.open()?.firmware()?);
            store.save(name, profile.encode()?.as_bytes())
        }
        ["apply", name] => apply_and_report(options, &stored(name)?, out),
        ["delete", name] => store.delete(name),
        ["rename", old, new] => store.rename(old, new),
        ["import", name, file] => {
            let profile = Profile::parse(&profiles::read_file(Path::new(file), PROFILE_LIMIT)?)?;
            store.save(name, profile.encode()?.as_bytes())
        }
        ["export", name, file] => write_file(out, file, stored(name)?.encode()?.as_bytes()),
        ["export-windows", name, file] => write_file(out, file, &stored(name)?.to_windows(name)?),
        ["export-windows-library", file, active, names @ ..] => {
            windows_library_names(names)?;
            let active = number::<usize>(active, "active profile index")?
                .checked_sub(1)
                .ok_or_else(|| input("active profile index starts at 1"))?;
            let records = names.iter().map(|name| stored(name)?.to_windows(name));
            write_file(
                out,
                file,
                &WindowsProfiles::encode_library(active, records)?,
            )
        }
        ["windows-options", name, args @ ..] => {
            let mut profile = stored(name)?;
            let flags = flag_pairs(args, &["--lift-off", "--motion-sync", "--wake-mode"], true)?;
            let mut options = profile.windows_options.unwrap_or_default();
            if let Some(value) = flags.get("--lift-off") {
                options.lift_off = match *value {
                    "1mm" | "low" => 0,
                    "2mm" | "high" => 1,
                    _ => return Err(input("lift-off must be 1mm or 2mm")),
                };
            }
            if let Some(value) = flags.get("--motion-sync") {
                options.motion_sync = i32::from(toggle(value)?);
            }
            if let Some(value) = flags.get("--wake-mode") {
                options.wake_mode = i32::from(toggle(value)?);
            }
            if !flags.is_empty() {
                profile.windows_options = Some(options);
                store.replace(name, profile.encode()?.as_bytes())?;
            } else if profile.windows_options.is_none() {
                return writeln!(out, "No Windows-only settings stored in this profile.");
            }
            writeln!(
                out,
                "Windows profile fields (local only)\nLift-off distance: {}\nMotion sync: {}\nWake mode: {}",
                options.lift_off, options.motion_sync, options.wake_mode
            )
        }
        ["inspect-windows", file, args @ ..] => {
            let bytes = profiles::read_file(Path::new(file), WINDOWS_LIMIT)?;
            let profiles = windows_for_model(&bytes, args)?;
            writeln!(
                out,
                "Windows active index: {} (zero-based)",
                profiles.active
            )?;
            for (index, record) in profiles.records().enumerate() {
                writeln!(
                    out,
                    "{}: {}",
                    index + 1,
                    WindowsProfiles::name(record).escape_default()
                )?;
            }
            Ok(())
        }
        ["extract-windows", file, index, output, args @ ..] => {
            let bytes = profiles::read_file(Path::new(file), WINDOWS_LIMIT)?;
            let library = windows_for_model(&bytes, args)?;
            write_file(
                out,
                output,
                library.record(number(index, "Windows profile index")?)?,
            )
        }
        ["activate-windows", file, index, output, args @ ..] => {
            let bytes = profiles::read_file(Path::new(file), WINDOWS_LIMIT)?;
            let library = windows_for_model(&bytes, args)?;
            write_file(
                out,
                output,
                &library.with_active(number(index, "Windows profile index")?)?,
            )
        }
        ["rename-windows", file, index, name, output, args @ ..] => {
            let bytes = profiles::read_file(Path::new(file), WINDOWS_LIMIT)?;
            let library = windows_for_model(&bytes, args)?;
            write_file(
                out,
                output,
                &library.with_name(number(index, "Windows profile index")?, name)?,
            )
        }
        ["import-windows", name, file, index, flags @ ..] => {
            let index: usize = number(index, "Windows profile index")?;
            let flags = flag_pairs(flags, &["--firmware"], true)?;
            let bytes = profiles::read_file(Path::new(file), WINDOWS_LIMIT)?;
            let windows = WindowsProfiles::parse(&bytes)?;
            let record = windows
                .records()
                .nth(index.wrapping_sub(1))
                .ok_or_else(|| input("Windows profile index is out of range"))?;
            let firmware = match flags.get("--firmware") {
                Some(value) => {
                    u16::from_str_radix(value.strip_prefix("0x").unwrap_or(value), 16)
                        .map_err(|_| input("firmware must be hexadecimal, for example 1001"))?
                }
                None => options.open()?.firmware()?,
            };
            let profile = Profile::from_windows(record, firmware)?;
            let encoded = profile.encode()?;
            Profile::parse(encoded.as_bytes())?;
            store.save(name, encoded.as_bytes())
        }
        _ => Err(input("usage: profile <command>; see help profile")),
    }
}

fn windows_for_model<'a>(bytes: &'a [u8], args: &[&str]) -> io::Result<WindowsProfiles<'a>> {
    match windows_model(args)? {
        Some(model) => WindowsProfiles::for_model(bytes, model),
        None => WindowsProfiles::parse(bytes),
    }
}
