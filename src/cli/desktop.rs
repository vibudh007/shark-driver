use std::{
    fs,
    io::{self, Read, Write},
    os::fd::AsFd,
    path::Path,
    time::Duration,
};

use shark_driver::{
    actions, bluetooth, linux, macro_wire, operations, profiles, recovery,
    settings::{Dpi, Polling, STAGES, Timing},
};

use super::args::{Options, input, number};

/// A versioned list with registry labels and no user-controlled titles.
pub(super) fn list(options: &Options, out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "r1-desktop\t5")?;
    for device in options.detected()? {
        let identity = match linux::detected_identity(&device) {
            Ok(identity) => identity,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let model = device.model.map_or("-", |model| model.name);
        let status = device.status_node.is_some();
        writeln!(
            out,
            "{}\t{}\t{identity}\t{model}\t{}\t{status}\t{}\t{}\t{}\t{}\t{}\t{}",
            device.connection,
            device.node.display(),
            device.configuration,
            device.polling_read,
            device.polling_write,
            device.dpi_read,
            device.dpi_write,
            device.button_read,
            device.button_write
        )?;
    }
    if options.device.is_none() && options.connection.is_none() {
        for peer in bluetooth::survey().unwrap_or_default() {
            let dpi = peer.settings_path;
            let buttons = dpi && (16..=18).contains(&peer.model.slot);
            writeln!(
                out,
                "bluetooth\t{}\t-\t{}\t{}\t{}\tfalse\tfalse\t{dpi}\t{dpi}\t{buttons}\t{buttons}",
                peer.address,
                peer.model.name,
                peer.configured,
                peer.configured || peer.battery.is_some()
            )?;
        }
    }
    Ok(())
}

pub(super) fn models(out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "r1-models\t1")?;
    for model in shark_driver::models::MODELS {
        writeln!(out, "{}\t{}", model.slot, model.name)?;
    }
    Ok(())
}

pub(super) fn settings(options: &Options, out: &mut dyn Write) -> io::Result<()> {
    let (firmware, state) = operations::capture(&mut options.open()?)?;
    write_settings(firmware, &state, out)
}

fn write_settings(
    firmware: u16,
    state: &operations::Settings,
    out: &mut dyn Write,
) -> io::Result<()> {
    let dpi = Dpi::parse(&state.dpi)?;
    let timing = Timing::parse(&state.timing)?;
    let light = timing.lighting();
    writeln!(out, "r1-settings\t1")?;
    writeln!(
        out,
        "device\t{firmware:04x}\t{}",
        Polling::parse(&state.polling)?.hz()
    )?;
    writeln!(
        out,
        "sensor\t{}\t{}",
        dpi.angle_snap(),
        dpi.ripple_control()
    )?;
    writeln!(
        out,
        "power\t{}\t{}\t{}",
        timing.sleep_seconds(),
        timing.deep_sleep_minutes(),
        timing.debounce_ms()
    )?;
    writeln!(
        out,
        "lighting\t{}\t{}\t{}\t{}",
        light.mode,
        light.brightness,
        light
            .speed
            .map_or_else(String::new, |value| value.to_string()),
        profiles::hex(&light.color)
    )?;
    for index in 1..=STAGES {
        let stage = dpi.stage(index)?;
        writeln!(
            out,
            "stage\t{index}\t{}\t{}\t{}\t{}",
            stage
                .dpi
                .map_or_else(String::new, |value| value.to_string()),
            stage.enabled,
            profiles::hex(&stage.color),
            index == dpi.active_stage()
        )?;
    }
    for (index, name) in actions::BUTTON_NAMES {
        writeln!(
            out,
            "button\t{name}\t{index}\t{}",
            actions::button_action(&state.buttons, index)?
        )?;
    }
    Ok(())
}

fn event_name(value: &str) -> bool {
    value.strip_prefix("event").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

pub(super) fn inputs(out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "r1-inputs\t1")?;
    for entry in fs::read_dir("/sys/class/input")? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if event_name(&name) {
            let label = fs::read_to_string(entry.path().join("device/name"))
                .unwrap_or_else(|_| name.clone());
            let label: String = label
                .trim()
                .chars()
                .filter(|c| !c.is_control())
                .take(96)
                .collect();
            writeln!(out, "/dev/input/{name}\t{label}")?;
        }
    }
    Ok(())
}

pub(super) fn pointer_activity(options: &Options, out: &mut dyn Write) -> io::Result<()> {
    let devices = options.devices()?;
    let [device] = devices.as_slice() else {
        return Err(input("select one wired R1 for pointer activity"));
    };
    linux::pointer_activity(device, options.expected.as_deref(), out)
}

pub(super) fn record(args: &[&str], out: &mut dyn Write) -> io::Result<()> {
    let [count, timeout, delay, nodes @ ..] = args else {
        return Err(input("invalid desktop recording request"));
    };
    if !(1..=8).contains(&nodes.len())
        || !nodes
            .iter()
            .all(|node| node.strip_prefix("/dev/input/").is_some_and(event_name))
    {
        return Err(input("select one to eight detected input nodes"));
    }
    let count = number(count, "event count")?;
    let timeout = Duration::from_millis(number(timeout, "recording timeout")?);
    let delay = if *delay == "recorded" {
        None
    } else {
        Some(number(delay, "fixed delay")?)
    };
    let stdin = io::stdin();
    writeln!(out, "r1-record\t1")?;
    out.flush()?;
    let mut start = [0; 6];
    stdin.lock().read_exact(&mut start)?;
    if &start != b"START\n" {
        return Err(input("recording was not started"));
    }
    let paths: Vec<_> = nodes.iter().map(Path::new).collect();
    let captured = actions::record_paths_until(&paths, count, timeout, delay, Some(stdin.as_fd()))?;
    write!(out, "{captured}")
}

pub(super) fn recovery_list(out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "r1-recovery\t1")?;
    for endpoint in recovery::discover()? {
        if let Some(port) = endpoint.port().file_name() {
            writeln!(
                out,
                "{}\t{}",
                endpoint.node.display(),
                port.to_string_lossy()
            )?;
        }
    }
    Ok(())
}

pub(super) fn recover(args: &[&str], out: &mut dyn Write) -> io::Result<()> {
    let [action @ ("check" | "restore"), node, port] = args else {
        return Err(input(
            "select a recovery action, node and physical USB port",
        ));
    };
    let endpoints = recovery::discover()?;
    let endpoint = endpoints
        .iter()
        .find(|endpoint| {
            endpoint.node == Path::new(node)
                && endpoint
                    .port()
                    .file_name()
                    .is_some_and(|name| name == *port)
        })
        .ok_or_else(|| input("the selected recovery endpoint changed"))?;
    recovery::run_with(endpoint, *action == "restore", out, |data, out| {
        writeln!(out, "r1-backup\t{}", profiles::hex(data))?;
        out.flush()?;
        let mut answer = [0; 6];
        io::stdin().lock().read_exact(&mut answer)?;
        if &answer != b"SAVED\n" {
            return Err(input("recovery backup was not confirmed"));
        }
        Ok(())
    })
}

fn setting_write(command: &[&str]) -> bool {
    matches!(
        command,
        [
            "polling" | "dpi" | "sensor" | "timing" | "lighting" | "button",
            "set",
            ..
        ] | ["dpi", "select", _]
    )
}

pub(super) fn payload(limit: usize) -> io::Result<String> {
    let mut value = String::new();
    io::stdin()
        .take((limit + 1) as u64)
        .read_to_string(&mut value)?;
    if value.len() > limit {
        return Err(input("desktop payload exceeds the maximum size"));
    }
    Ok(value)
}

/// Restrict the desktop polkit entry point to operations with no file arguments.
pub(super) fn device_command(args: &[&str], out: &mut dyn Write) -> io::Result<()> {
    let _timing = shark_driver::timing::Span::start("desktop.device_command");
    let [
        connection @ ("wired" | "receiver"),
        node,
        identity,
        command @ ..,
    ] = args
    else {
        return Err(input("invalid desktop device request"));
    };
    let valid_node = node
        .strip_prefix("/dev/hidraw")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()));
    if !valid_node || identity.len() > 128 {
        return Err(input("desktop operation is outside the device scope"));
    }
    let target = [
        "--connection",
        connection,
        "--device",
        node,
        "--expect-device",
        identity,
    ];
    let invoke = |command: &[&str], output: &mut dyn Write| {
        let mut args = target.to_vec();
        args.extend_from_slice(command);
        super::run(&args, output)
    };
    let (options, _) = Options::parse(&target)?;
    match command {
        ["desktop-quick", edit @ ..] if setting_write(edit) => {
            let _timing = shark_driver::timing::Span::start("desktop.quick_change");
            let group = match edit[0] {
                "dpi" | "sensor" => shark_driver::driver::Report::Dpi,
                "timing" | "lighting" => shark_driver::driver::Report::Timing,
                "polling" => shark_driver::driver::Report::Polling,
                _ => shark_driver::driver::Report::Buttons,
            };
            invoke(edit, &mut io::sink())?;
            let mut driver = options.open()?;
            let firmware = driver.firmware()?;
            let value = operations::read_setting(&mut driver, group)?;
            let mut state = operations::Settings::defaults();
            use shark_driver::driver::Report;
            let fields: &[&str] = match group {
                Report::Dpi => {
                    state.dpi = value;
                    &["sensor", "stage"]
                }
                Report::Timing => {
                    state.timing = value;
                    &["power", "lighting"]
                }
                Report::Polling => {
                    state.polling = value;
                    &["device"]
                }
                Report::Buttons => {
                    state.buttons = value;
                    &["button"]
                }
                _ => unreachable!(),
            };
            let mut output = Vec::new();
            write_settings(firmware, &state, &mut output)?;
            writeln!(out, "r1-change\t1")?;
            for line in String::from_utf8_lossy(&output).lines() {
                if line
                    .split_once('\t')
                    .is_some_and(|(field, _)| fields.contains(&field))
                {
                    writeln!(out, "{line}")?;
                }
            }
            return Ok(());
        }
        ["desktop-profile-apply"] => {
            let profile = profiles::Profile::parse(payload(profiles::PROFILE_LIMIT)?.as_bytes())?;
            return super::profiles::apply_and_report(&options, &profile, out);
        }
        ["desktop-profile-capture"] => {
            let profile = super::profiles::capture_profile(&mut options.open()?)?;
            return out.write_all(profile.encode()?.as_bytes());
        }
        ["desktop-profile-default"] => {
            let profile = super::profiles::default_profile(options.open()?.firmware()?);
            return out.write_all(profile.encode()?.as_bytes());
        }
        [
            "desktop-macro",
            operation @ ("upload" | "install" | "restore-raw"),
            slot,
        ] => {
            let slot = actions::parse_button(slot)?;
            let text = payload(profiles::TEXT_LIMIT)?;
            if *operation == "restore-raw" {
                let written = macro_wire::restore(&mut options.open()?, slot as u8, &text)?;
                return writeln!(
                    out,
                    "Verified macro chain in slot {slot} ({written} pages written)."
                );
            }
            let value = super::macros::macro_text(text.as_bytes())?;
            if *operation == "install" {
                return super::macros::install_macro(&options, slot, &value, out);
            }
            return super::macros::upload_macro(&options, slot as u8, &value, out);
        }
        _ => {}
    }
    if let ["desktop-apply", baseline, changes @ ..] = command {
        let commands: Vec<_> = changes.split(|argument| *argument == "--next").collect();
        if baseline.len() > 8192
            || commands.len() > 20
            || !commands.iter().all(|c| setting_write(c))
        {
            return Err(input("invalid desktop settings batch"));
        }
        let mut current = Vec::new();
        invoke(&["desktop-settings"], &mut current)?;
        if current != baseline.as_bytes() {
            return Err(io::Error::other(
                "The mouse settings changed. Discard and refresh before applying.",
            ));
        }
        for (index, command) in commands.iter().enumerate() {
            if let Err(error) = invoke(command, &mut io::sink()) {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "{index} settings operations completed before the failure. Refresh before retrying ({error})."
                    ),
                ));
            }
        }
        return invoke(&["desktop-settings"], out);
    }
    if !setting_write(command)
        && !matches!(
            command,
            ["desktop-settings" | "desktop-pointer-activity" | "info" | "reset"]
                | ["profile", "device-info"]
                | ["polling" | "dpi", "show"]
                | ["status" | "desktop-status", ..]
                | ["button", "show"]
                | ["button", "show", _]
                | ["macro", "read" | "read-raw", _, "-"]
                | ["macro", "backup", "-"]
                | ["macro", "backup", "-", _]
        )
    {
        return Err(input("desktop operation is outside the device scope"));
    }
    invoke(command, out)
}

#[cfg(test)]
mod tests {
    use super::device_command;

    #[test]
    fn desktop_entry_rejects_files_and_unknown_endpoints() {
        for request in [
            &["wired", "/dev/hidraw1", "1", "profile", "apply", "example"][..],
            &[
                "wired",
                "/dev/hidraw1",
                "1",
                "macro",
                "backup",
                "/tmp/example",
            ],
            &[
                "wired",
                "/dev/hidraw1",
                "1",
                "recovery",
                "restore",
                "example",
            ],
            &["wired", "/dev/hidraw/../example", "1", "settings"],
        ] {
            let error = device_command(request, &mut Vec::new()).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
    }
}
