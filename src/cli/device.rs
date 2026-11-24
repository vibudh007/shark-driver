use std::{
    collections::BTreeSet,
    io::{self, Write},
    path::Path,
    thread,
    time::{Duration, Instant},
};

use shark_driver::{
    driver::{Connection, Event},
    linux, recovery,
};

use super::args::{Options, flag_pairs, input, number};

pub(super) fn status(
    options: &Options,
    args: &[&str],
    out: &mut dyn Write,
    machine: bool,
) -> io::Result<()> {
    let (milliseconds, count) = status_arguments(args)?;
    if let Some(address) = options.bluetooth.as_deref() {
        let peer = shark_driver::bluetooth::passive_status(address)?;
        if peer.battery.is_some() || !peer.configured {
            return bluetooth_battery(out, machine, peer.battery);
        }
    }
    let mut driver = options.open()?;
    if machine {
        writeln!(out, "r1-status\t1")?;
        out.flush()?;
    }
    for index in 0..count {
        let Some(event) = driver.next_event(Duration::from_millis(milliseconds))? else {
            if index == 0 {
                if let Some(percentage) = driver.battery_percentage()? {
                    if machine {
                        writeln!(out, "battery-percent\t{percentage}")?;
                        break;
                    }
                    writeln!(out, "Battery: {percentage}% (Bluetooth battery service)")?;
                } else {
                    if machine {
                        writeln!(out, "idle")?;
                        break;
                    }
                    writeln!(out, "No R1 status event received in {milliseconds} ms.")?;
                }
            }
            break;
        };
        if machine {
            match event {
                Event::Battery { level, state } => writeln!(out, "battery\t{level}\t{state}")?,
                Event::DpiStage(stage) => writeln!(out, "dpi\t{stage}")?,
                Event::LightMode(mode) => writeln!(out, "light\t{mode}")?,
                Event::Acknowledged(code) => writeln!(out, "receipt\t{code}")?,
                Event::PollingNotice(value) => writeln!(out, "polling\t{value}")?,
                Event::BatteryNotice(value) => writeln!(out, "battery-notice\t{value}")?,
                Event::ProfileNotice(value) => writeln!(out, "profile\t{value}")?,
                Event::Other { class, value } => writeln!(out, "other\t{class}\t{value}")?,
            }
            out.flush()?;
            continue;
        }
        match event {
            Event::Battery { level, state } => {
                let level = if driver.connection() == Connection::Wired && state == 2 {
                    10
                } else {
                    level
                };
                let description = match (driver.connection(), state) {
                    (Connection::Wired, 2) => "full",
                    (Connection::Wired, _) | (Connection::Receiver, 3) => "charging",
                    (Connection::Receiver, _) => "reported",
                    (Connection::Bluetooth, _) => "reported",
                };
                if level <= 10 {
                    writeln!(
                        out,
                        "Battery: {level}/10 ({description}, status 0x{state:02x})"
                    )?;
                } else {
                    writeln!(
                        out,
                        "Battery: raw level {level} ({description}, status 0x{state:02x})"
                    )?;
                }
            }
            Event::DpiStage(stage) => writeln!(out, "Active DPI stage: {stage}")?,
            Event::LightMode(mode) => writeln!(out, "Lighting mode: {mode}")?,
            Event::Acknowledged(code) => writeln!(out, "Write receipt: 0x{code:02x}")?,
            Event::PollingNotice(value) => writeln!(out, "Polling notice: {value}")?,
            Event::BatteryNotice(value)
                if driver.connection() == Connection::Bluetooth && (1..=10).contains(&value) =>
            {
                writeln!(out, "Battery: {value}/10 (bluetooth)")?;
            }
            Event::BatteryNotice(value) => writeln!(out, "Battery notice: {value}")?,
            Event::ProfileNotice(value) => writeln!(out, "Profile notice: {value}")?,
            Event::Other { class, value } => writeln!(out, "Status 0x{class:02x}: 0x{value:04x}")?,
        }
        out.flush()?;
    }
    Ok(())
}

fn bluetooth_battery(out: &mut dyn Write, machine: bool, battery: Option<u8>) -> io::Result<()> {
    if machine {
        writeln!(out, "r1-status\t1")?;
        return match battery {
            Some(percentage) => writeln!(out, "battery-percent\t{percentage}"),
            None => writeln!(out, "idle"),
        };
    }
    match battery {
        Some(percentage) => writeln!(out, "Battery: {percentage}% (Bluetooth battery service)"),
        None => writeln!(out, "Bluetooth battery level unavailable."),
    }
}

pub(super) fn recovery_command(
    options: &Options,
    args: &[&str],
    out: &mut dyn Write,
) -> io::Result<()> {
    if options.emulation.is_some()
        || options.bluetooth.is_some()
        || options.connection == Some(Connection::Receiver)
    {
        return Err(input("recovery requires a physical wired USB mouse"));
    }
    let [mode @ ("check" | "restore"), backup] = args else {
        return Err(input(
            "usage: --device /dev/hidrawN recovery check|restore BACKUP",
        ));
    };
    let node = options.device.as_ref().ok_or_else(|| {
        input("select the bootloader endpoint from list with --device /dev/hidrawN")
    })?;
    let endpoints = recovery::discover()?;
    let endpoint = endpoints
        .iter()
        .find(|endpoint| endpoint.node == *node)
        .ok_or_else(|| {
            input("the selected endpoint does not expose the Beken bootloader descriptor")
        })?;
    recovery::run(endpoint, Path::new(backup), *mode == "restore", out)
}

fn status_arguments(args: &[&str]) -> io::Result<(u64, u16)> {
    let flags = flag_pairs(args, &["--timeout-ms", "--count"], true)?;
    let milliseconds: u64 = flags
        .get("--timeout-ms")
        .map(|v| number(v, "timeout"))
        .transpose()?
        .unwrap_or(1500);
    let count: u16 = flags
        .get("--count")
        .map(|v| number(v, "count"))
        .transpose()?
        .unwrap_or(1);
    if !(1..=60_000).contains(&milliseconds) || !(1..=1000).contains(&count) {
        return Err(input("timeout must be 1–60000 ms and count 1–1000"));
    }
    Ok((milliseconds, count))
}

pub(super) fn catalog(out: &mut dyn Write) -> io::Result<()> {
    writeln!(
        out,
        "Slot  Model      Wired USB  Receiver tag  Configuration"
    )?;
    for model in shark_driver::models::MODELS {
        let configuration = if model.supports_settings() {
            "available"
        } else if (16..=18).contains(&model.slot) {
            "wired polling, DPI, buttons and readback"
        } else if model.wired_polling().is_some() {
            "wired polling and DPI"
        } else if shark_driver::model_wire::WiredDpi::for_slot(model.slot).is_some() {
            "wired DPI"
        } else {
            "not ported"
        };
        writeln!(
            out,
            "{:>4}  {:<9}  1d57:{:04x}  {:02x}            {configuration}",
            model.slot, model.name, model.wired_pid, model.receiver_tag
        )?;
    }
    Ok(())
}

pub(super) fn observe(
    options: &Options,
    args: &[&str],
    out: &mut dyn Write,
    machine: bool,
) -> io::Result<()> {
    let (milliseconds, count) = status_arguments(args)?;
    if let Some(address) = options.bluetooth.as_deref() {
        let peer = shark_driver::bluetooth::passive_status(address)?;
        return bluetooth_battery(out, machine, peer.battery);
    }
    if options.emulation.is_some() {
        return Err(input(
            "USB status observation requires a detected USB endpoint",
        ));
    }
    let mut devices = options.detected()?;
    devices.retain(|device| device.status_node.is_some());
    let [device] = devices.as_slice() else {
        return Err(input(
            "select one detected mouse with a status collection using list and --device",
        ));
    };
    let mut reader = linux::StatusReader::open(device, options.expected.as_deref())?;
    if machine {
        writeln!(out, "r1-status\t1")?;
        out.flush()?;
    }
    for index in 0..count {
        let event = reader.read(Duration::from_millis(milliseconds))?;
        if let Some(event) = event {
            if machine {
                if let Some((level, state)) = event.battery() {
                    writeln!(out, "battery\t{level}\t{state}")?;
                }
            } else if let Some((level, state)) = event.battery() {
                writeln!(
                    out,
                    "Attack Shark {} battery: {level}/10 (state 0x{state:02x})",
                    event.model.name
                )?;
            } else {
                writeln!(
                    out,
                    "Attack Shark {} status: class 0x{:02x}, value 0x{:04x}",
                    event.model.name, event.class, event.value
                )?;
            }
        } else {
            if index == 0 {
                if machine {
                    writeln!(out, "idle")?;
                } else {
                    writeln!(out, "No mouse status event received in {milliseconds} ms.")?;
                }
            }
            break;
        }
        out.flush()?;
    }
    Ok(())
}

fn usb_endpoint_lines(options: &Options) -> io::Result<BTreeSet<String>> {
    let mut endpoints = BTreeSet::new();
    if options.connection != Some(Connection::Receiver) {
        for endpoint in recovery::discover()? {
            if options
                .device
                .as_ref()
                .is_none_or(|node| *node == endpoint.node)
            {
                endpoints.insert(format!(
                    "{}  Beken bootloader a745:0033  ({})",
                    endpoint.node.display(),
                    endpoint
                        .port()
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                ));
            }
        }
    }
    for device in options.detected()? {
        let status = device.status_node.as_deref().map_or_else(
            || String::from("unavailable"),
            |path| path.display().to_string(),
        );
        let model = device
            .model
            .map_or("mouse unidentified", |model| model.name);
        let use_case = if device.configuration {
            "configuration candidate"
        } else {
            "detection only"
        };
        endpoints.insert(format!(
            "{}  {}  Attack Shark {}  ({use_case}; status: {status})",
            device.node.display(),
            device.connection,
            model
        ));
    }
    Ok(endpoints)
}

fn write_endpoint_changes(
    out: &mut dyn Write,
    previous: &BTreeSet<String>,
    current: &BTreeSet<String>,
    machine: bool,
) -> io::Result<()> {
    let (removed, found) = if machine {
        ("removed\t", "found\t")
    } else {
        ("Removed ", "Found ")
    };
    for endpoint in previous.difference(current) {
        writeln!(out, "{removed}{endpoint}")?;
    }
    for endpoint in current.difference(previous) {
        writeln!(out, "{found}{endpoint}")?;
    }
    out.flush()
}

pub(super) fn watch_usb_endpoints(
    options: &Options,
    args: &[&str],
    out: &mut dyn Write,
    machine: bool,
) -> io::Result<()> {
    if options.bluetooth.is_some() {
        return Err(input(
            "list watch follows USB endpoints; use list for a selected Bluetooth peer",
        ));
    }
    let flags = flag_pairs(args, &["--timeout-ms"], true)?;
    let duration: u64 = flags
        .get("--timeout-ms")
        .map(|value| number(value, "timeout"))
        .transpose()?
        .unwrap_or(15_000);
    if !(1..=60_000).contains(&duration) {
        return Err(input("watch timeout must be 1–60000 ms"));
    }
    let simulated = options
        .emulation
        .map(|_| {
            options
                .open()
                .map(|driver| format!("simulated {} R1", driver.connection()))
        })
        .transpose()?
        .map(|endpoint| BTreeSet::from([endpoint]));
    let mut previous = BTreeSet::new();
    let deadline = Instant::now() + Duration::from_millis(duration);
    if machine {
        writeln!(out, "r1-watch\t1")?;
    }
    loop {
        let current = match &simulated {
            Some(endpoints) => endpoints.clone(),
            None => usb_endpoint_lines(options)?,
        };
        write_endpoint_changes(out, &previous, &current, machine)?;
        previous = current;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        thread::sleep(remaining.min(Duration::from_millis(250)));
    }
}

pub(super) fn list(options: &Options, out: &mut dyn Write) -> io::Result<()> {
    if options.emulation.is_some() {
        return writeln!(out, "simulated {} R1", options.open()?.connection());
    }
    if let Some(address) = &options.bluetooth {
        let peer = shark_driver::bluetooth::passive_status(address)?;
        writeln!(
            out,
            "Attack Shark {} Bluetooth {address} (paired and connected)",
            peer.model.name
        )?;
        if let Some(percentage) = peer.battery {
            writeln!(out, "Battery: {percentage}%")?;
        }
        return Ok(());
    }
    let mut endpoints = usb_endpoint_lines(options)?;
    if options.device.is_none() && options.connection.is_none() {
        for peer in shark_driver::bluetooth::survey().unwrap_or_default() {
            let use_case = if peer.configured {
                "vendor service present"
            } else if peer.battery.is_some() {
                "battery available"
            } else {
                "advertised name"
            };
            endpoints.insert(format!(
                "{}  bluetooth  Attack Shark {}  ({use_case})",
                peer.address, peer.model.name
            ));
        }
    }
    if endpoints.is_empty() {
        writeln!(out, "No Attack Shark endpoints found.")?;
    }
    for endpoint in endpoints {
        writeln!(out, "{endpoint}")?;
    }
    Ok(())
}

pub(super) fn info(options: &Options, out: &mut dyn Write) -> io::Result<()> {
    if options.device.is_some() {
        if let [device] = options.detected()?.as_slice() {
            if let Some(model) = device.model {
                if !device.configuration {
                    return writeln!(
                        out,
                        "Model: Attack Shark {}\nConnection: {}\nConfiguration: unavailable",
                        model.name, device.connection
                    );
                }
            }
        }
    }
    let mut driver = options.open()?;
    writeln!(
        out,
        "Model: Attack Shark R1\nConnection: {}\nFirmware: 0x{:04x}",
        driver.connection(),
        driver.firmware()?
    )
}
