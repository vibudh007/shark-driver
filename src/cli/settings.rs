use std::{
    collections::BTreeMap,
    io::{self, Write},
};

use shark_driver::{
    actions::{self, Action},
    driver::{Driver, Report},
    macro_wire,
    operations::{self, Change, Settings},
    profiles,
    settings::{self, Dpi, Polling, Timing},
};

use super::args::{Options, color, flag_pairs, input, number, toggle};

fn show_dpi(out: &mut dyn Write, dpi: &Dpi, only: Option<u8>) -> io::Result<()> {
    for number in only.map_or(1..=settings::STAGES, |n| n..=n) {
        let stage = dpi.stage(number)?;
        let value = stage
            .dpi
            .map_or_else(|| String::from("unknown"), |value| value.to_string());
        let enabled = if stage.enabled { "on" } else { "off" };
        let selected = if dpi.active_stage() == number {
            " (active)"
        } else {
            ""
        };
        writeln!(
            out,
            "Stage {number}: {value} DPI, {enabled}, #{}{selected}",
            profiles::hex(&stage.color)
        )?;
    }
    Ok(())
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn show_sensor(out: &mut dyn Write, dpi: &Dpi) -> io::Result<()> {
    writeln!(
        out,
        "Angle snap: {}\nRipple control: {}",
        on_off(dpi.angle_snap()),
        on_off(dpi.ripple_control())
    )
}

fn show_timing(out: &mut dyn Write, timing: &Timing) -> io::Result<()> {
    writeln!(
        out,
        "Sleep: {} s\nDeep sleep: {} min\nDebounce: {} ms",
        timing.sleep_seconds(),
        timing.deep_sleep_minutes(),
        timing.debounce_ms()
    )
}

const LIGHT_MODES: [&str; 7] = [
    "off",
    "static",
    "breathing",
    "neon",
    "color-breathing",
    "static-dpi",
    "breathing-dpi",
];

fn show_lighting(out: &mut dyn Write, timing: &Timing) -> io::Result<()> {
    let light = timing.lighting();
    let mode = LIGHT_MODES
        .get(usize::from(light.mode))
        .copied()
        .unwrap_or("unknown");
    let speed = light
        .speed
        .map_or_else(|| String::from("unknown"), |speed| speed.to_string());
    writeln!(
        out,
        "Lighting: {mode} ({})\nBrightness: {}\nBreath speed: {speed}\nColor: #{}",
        light.mode,
        light.brightness,
        profiles::hex(&light.color)
    )
}

fn show_buttons(
    out: &mut dyn Write,
    bytes: &[u8],
    only: Option<usize>,
    names: &[(usize, &str)],
) -> io::Result<()> {
    if let Some(button) = only {
        let action = actions::button_action(bytes, button)?;
        return if names == actions::BUTTON_NAMES {
            writeln!(out, "Button {button}: {action}")
        } else {
            match names.iter().find(|(index, _)| *index == button) {
                Some((_, name)) => writeln!(out, "{name} ({button}): {action}"),
                None => writeln!(out, "Button {button}: {action}"),
            }
        };
    }
    for &(button, name) in names {
        writeln!(
            out,
            "{name} ({button}): {}",
            actions::button_action(bytes, button)?
        )?;
    }
    Ok(())
}

pub(super) fn show_settings(
    out: &mut dyn Write,
    firmware: u16,
    config: &Settings,
) -> io::Result<()> {
    let dpi = Dpi::parse(&config.dpi)?;
    let timing = Timing::parse(&config.timing)?;
    writeln!(
        out,
        "Firmware: 0x{firmware:04x}\nPolling: {} Hz",
        Polling::parse(&config.polling)?.hz()
    )?;
    show_dpi(out, &dpi, None)?;
    show_sensor(out, &dpi)?;
    show_timing(out, &timing)?;
    show_lighting(out, &timing)?;
    show_buttons(out, &config.buttons, None, &actions::BUTTON_NAMES)
}

pub(super) fn show(options: &Options, out: &mut dyn Write) -> io::Result<()> {
    if let Some(device) = options.model_device()? {
        let archive =
            shark_driver::linux::archive_model_profile(&device, options.expected.as_deref())?;
        let profile = settings::Legacy::parse(&archive[7..])?;
        let model = device.model.unwrap();
        writeln!(out, "Firmware: 0x{:02x}{:02x}", archive[5], archive[6])?;
        if let Ok(rate) = profile.polling() {
            writeln!(out, "Polling: {} Hz", rate.hz())?;
        }
        show_dpi(
            out,
            &shark_driver::model_wire::model_dpi(model, &profile)?,
            None,
        )?;
        show_timing(out, &profile.timing())?;
        show_lighting(out, &profile.timing())?;
        return show_buttons(out, &profile.buttons(), None, &actions::button_names(model));
    }
    let (firmware, settings) = operations::capture(&mut options.open()?)?;
    show_settings(out, firmware, &settings)
}

pub(super) fn reset(options: &Options, out: &mut dyn Write) -> io::Result<()> {
    if let Some(device) = options.model_device()? {
        let groups = shark_driver::linux::reset_model_full(&device, options.expected.as_deref())?;
        return writeln!(
            out,
            "Verified factory defaults for {groups} settings groups. Macro storage preserved."
        );
    }
    changed(out, operations::reset(&mut options.open()?)?)
}

pub(super) fn reset_settings_only(options: &Options, out: &mut dyn Write) -> io::Result<()> {
    if let Some(address) = options.bluetooth.as_deref() {
        let mut peer = shark_driver::bluetooth::open_model(address)?;
        let groups = shark_driver::model_wire::reset_model_settings(
            peer.model(),
            &mut peer,
            shark_driver::model_wire::bluetooth_width,
        )?;
        return writeln!(
            out,
            "Verified {groups} settings groups. Existing macros kept."
        );
    }
    let device = options
        .model_device()?
        .ok_or_else(|| input("select a non-R1 wired mouse with settings readback"))?;
    let groups = shark_driver::linux::reset_model_settings(&device, options.expected.as_deref())?;
    writeln!(
        out,
        "Verified {groups} settings groups. Existing macros kept."
    )
}

pub(super) fn changed(out: &mut dyn Write, reports: Vec<Report>) -> io::Result<()> {
    if reports.is_empty() {
        return writeln!(out, "Settings already match.");
    }
    let names: Vec<_> = reports.iter().map(|report| report.name()).collect();
    writeln!(out, "Verified changes: {}.", names.join(", "))
}

fn set_setting(
    driver: &mut Driver,
    out: &mut dyn Write,
    kind: Report,
    before: &[u8],
    after: &[u8],
) -> io::Result<()> {
    changed(
        out,
        operations::apply(driver, &[Change::new(kind, before, after)?])?,
    )
}

pub(super) fn polling_command(
    options: &Options,
    args: &[&str],
    out: &mut dyn Write,
) -> io::Result<()> {
    let desired: Option<u16> = match args {
        [] | ["show" | "get"] => None,
        ["set", value] => Some(number(value, "polling rate")?),
        _ => return Err(input("usage: polling show | set HZ")),
    };
    if options.emulation.is_none() && options.bluetooth.is_none() {
        if let [device] = options.detected()?.as_slice() {
            if device.polling_read {
                return writeln!(
                    out,
                    "{} Hz",
                    shark_driver::linux::update_model_polling(
                        device,
                        options.expected.as_deref(),
                        desired
                    )?
                );
            }
        }
    }
    if let Some(value) = desired {
        Polling::default().set_hz(value)?;
    }
    let mut driver = options.open()?;
    let before = operations::read_setting(&mut driver, Report::Polling)?;
    let mut value = Polling::parse(&before)?;
    if let Some(rate) = desired {
        value.set_hz(rate)?;
        set_setting(
            &mut driver,
            out,
            Report::Polling,
            &before,
            &value.write_bytes(),
        )?;
    }
    writeln!(out, "{} Hz", value.hz())
}

pub(super) fn dpi_command(options: &Options, args: &[&str], out: &mut dyn Write) -> io::Result<()> {
    let mut selection = None;
    let mut stage = None;
    let mut dpi_value = None;
    let mut enabled = None;
    let mut rgb = None;
    match args {
        [] | ["show" | "get"] => {}
        ["show" | "get", n] => stage = Some(number::<u8>(n, "stage")?),
        ["select", n] => selection = Some(number::<u8>(n, "stage")?),
        ["set", n, flags @ ..] => {
            stage = Some(number::<u8>(n, "stage")?);
            let flags = flag_pairs(flags, &["--value", "--enabled", "--color"], false)?;
            dpi_value = flags
                .get("--value")
                .map(|value| number::<u16>(value, "DPI"))
                .transpose()?;
            enabled = flags
                .get("--enabled")
                .map(|value| toggle(value))
                .transpose()?;
            rgb = flags.get("--color").map(|value| color(value)).transpose()?;
        }
        _ => {
            return Err(input(
                "usage: dpi show [STAGE] | select STAGE | set STAGE OPTIONS",
            ));
        }
    }
    if let Some(n) = stage.or(selection) {
        Dpi::default().stage(n)?;
    }
    let edit = |dpi: &mut Dpi| -> io::Result<()> {
        if let Some(n) = selection {
            dpi.set_active_stage(n)?;
        }
        if let Some(n) = stage {
            if let Some(value) = dpi_value {
                dpi.set_stage_dpi(n, value)?;
            }
            if let Some(value) = enabled {
                dpi.set_stage_enabled(n, value)?;
            }
            if let Some(value) = rgb {
                dpi.set_stage_color(n, value)?;
            }
        }
        Ok(())
    };
    let writing = matches!(args.first(), Some(&("set" | "select")));
    if let Some(address) = options.bluetooth.as_deref() {
        let mut peer = shark_driver::bluetooth::open_model(address)?;
        let dpi = shark_driver::model_wire::update_bluetooth_dpi(
            peer.model(),
            &mut peer,
            writing.then_some(edit),
        )?;
        return show_dpi(out, &dpi, stage.or(selection));
    }
    if options.emulation.is_none() && options.bluetooth.is_none() {
        if let [device] = options.detected()?.as_slice() {
            if device
                .model
                .and_then(|model| shark_driver::model_wire::WiredDpi::for_slot(model.slot))
                .is_some()
            {
                let dpi = shark_driver::linux::update_model_dpi(
                    device,
                    options.expected.as_deref(),
                    writing.then_some(edit),
                )?;
                return show_dpi(out, &dpi, stage.or(selection));
            }
        }
    }
    if let (Some(n), Some(value)) = (stage, dpi_value) {
        Dpi::default().set_stage_dpi(n, value)?;
    }
    let mut driver = options.open()?;
    let before = operations::read_setting(&mut driver, Report::Dpi)?;
    let mut dpi = Dpi::parse(&before)?;
    edit(&mut dpi)?;
    if writing {
        set_setting(&mut driver, out, Report::Dpi, &before, &dpi.write_bytes())?;
    }
    show_dpi(out, &dpi, stage.or(selection))
}

fn show_or_set<'a>(args: &[&'a str], flags: &[&str]) -> io::Result<BTreeMap<&'a str, &'a str>> {
    match args {
        [] | ["show" | "get"] => Ok(BTreeMap::new()),
        ["set", values @ ..] => flag_pairs(values, flags, false),
        _ => Err(input("expected show or set with options")),
    }
}

pub(super) fn sensor_command(
    options: &Options,
    args: &[&str],
    out: &mut dyn Write,
) -> io::Result<()> {
    let flags = show_or_set(args, &["--angle-snap", "--ripple-control"])?;
    let angle = flags
        .get("--angle-snap")
        .map(|value| toggle(value))
        .transpose()?;
    let ripple = flags
        .get("--ripple-control")
        .map(|value| toggle(value))
        .transpose()?;
    let mut driver = options.open()?;
    let before = operations::read_setting(&mut driver, Report::Dpi)?;
    let mut dpi = Dpi::parse(&before)?;
    if let Some(value) = angle {
        dpi.set_angle_snap(value);
    }
    if let Some(value) = ripple {
        dpi.set_ripple_control(value);
    }
    if !flags.is_empty() {
        set_setting(&mut driver, out, Report::Dpi, &before, &dpi.write_bytes())?;
    }
    show_sensor(out, &dpi)
}

pub(super) fn timing_command(
    options: &Options,
    args: &[&str],
    out: &mut dyn Write,
    lighting: bool,
) -> io::Result<()> {
    let allowed: &[&str] = if lighting {
        &["--mode", "--brightness", "--speed", "--color"]
    } else {
        &["--sleep-seconds", "--deep-sleep-minutes", "--debounce-ms"]
    };
    let flags = show_or_set(args, allowed)?;
    let mode = flags
        .get("--mode")
        .map(|name| {
            LIGHT_MODES
                .iter()
                .position(|mode| mode == name)
                .map(|i| i as u8)
                .ok_or_else(|| input(format!("unknown lighting mode '{name}'")))
        })
        .transpose()?;
    let brightness = flags
        .get("--brightness")
        .map(|value| number::<u8>(value, "brightness"))
        .transpose()?;
    let speed = flags
        .get("--speed")
        .map(|value| number::<u8>(value, "speed"))
        .transpose()?;
    let rgb = flags.get("--color").map(|value| color(value)).transpose()?;
    let sleep = flags
        .get("--sleep-seconds")
        .map(|value| number::<u16>(value, "sleep seconds"))
        .transpose()?;
    let deep = flags
        .get("--deep-sleep-minutes")
        .map(|value| number::<u8>(value, "deep sleep minutes"))
        .transpose()?;
    let debounce = flags
        .get("--debounce-ms")
        .map(|value| number::<u16>(value, "debounce"))
        .transpose()?;
    let mut validator = Timing::default();
    if let Some(value) = sleep {
        validator.set_sleep_seconds(value)?;
    }
    if let Some(value) = deep {
        validator.set_deep_sleep_minutes(value)?;
    }
    if let Some(value) = debounce {
        validator.set_debounce_ms(value)?;
    }
    let edit = |timing: &mut Timing| -> io::Result<()> {
        if let Some(value) = mode {
            timing.set_light_mode(value)?;
        }
        if let Some(value) = brightness {
            timing.set_brightness(value)?;
        }
        if let Some(value) = speed {
            timing.set_breath_speed(value)?;
        }
        if let Some(value) = rgb {
            timing.set_light_color(value);
        }
        if let Some(value) = sleep {
            timing.set_sleep_seconds(value)?;
        }
        if let Some(value) = deep {
            timing.set_deep_sleep_minutes(value)?;
        }
        if let Some(value) = debounce {
            timing.set_debounce_ms(value)?;
        }
        Ok(())
    };
    if let Some(address) = options.bluetooth.as_deref() {
        let mut peer = shark_driver::bluetooth::open_model(address)?;
        let timing = shark_driver::model_wire::update_bluetooth_timing(
            peer.model(),
            &mut peer,
            (!flags.is_empty()).then_some(edit),
        )?;
        return if lighting {
            show_lighting(out, &timing)
        } else {
            show_timing(out, &timing)
        };
    }
    if let Some(device) = options.model_device()? {
        let timing = shark_driver::linux::update_model_timing(
            &device,
            options.expected.as_deref(),
            (!flags.is_empty()).then_some(edit),
        )?;
        return if lighting {
            show_lighting(out, &timing)
        } else {
            show_timing(out, &timing)
        };
    }
    let mut driver = options.open()?;
    let before = operations::read_setting(&mut driver, Report::Timing)?;
    let mut timing = Timing::parse(&before)?;
    edit(&mut timing)?;
    if !flags.is_empty() {
        set_setting(
            &mut driver,
            out,
            Report::Timing,
            &before,
            &timing.write_bytes(),
        )?;
    }
    if lighting {
        show_lighting(out, &timing)
    } else {
        show_timing(out, &timing)
    }
}

pub(super) fn button_command(
    options: &Options,
    args: &[&str],
    out: &mut dyn Write,
) -> io::Result<()> {
    if args == ["actions"] {
        for name in actions::preset_names() {
            writeln!(out, "{name}")?;
        }
        return writeln!(out, "key:MODIFIER+KEY\nmacro:BUTTON\nraw:XX,YY,ZZ");
    }
    let mut peer = options
        .bluetooth
        .as_deref()
        .map(shark_driver::bluetooth::open_model)
        .transpose()?;
    let model_device = options.model_device()?;
    let button_names = model_device
        .as_ref()
        .and_then(|device| device.model)
        .or_else(|| peer.as_ref().map(|peer| peer.model()))
        .map_or(actions::BUTTON_NAMES, actions::button_names);
    let (only, action) = match args {
        [] | ["show" | "get"] => (None, None),
        ["show" | "get", name] => (
            Some(actions::parse_named_button(name, &button_names)?),
            None,
        ),
        ["set", name, value] => (
            Some(actions::parse_named_button(name, &button_names)?),
            Some(actions::parse_action(value)?),
        ),
        _ => return Err(input("usage: button show [BUTTON] | set BUTTON ACTION")),
    };
    if let Some(peer) = peer.as_mut().filter(|peer| peer.model().slot != 3) {
        let buttons = shark_driver::model_wire::update_model_button(
            peer.model(),
            peer,
            shark_driver::model_wire::bluetooth_width,
            only.zip(action),
        )?;
        return show_buttons(out, &buttons, only, &button_names);
    }
    drop(peer);
    if let Some(device) = model_device {
        let buttons = shark_driver::linux::update_model_button(
            &device,
            options.expected.as_deref(),
            only.zip(action),
        )?;
        return show_buttons(out, &buttons, only, &button_names);
    }
    let mut driver = options.open()?;
    let before = operations::read_setting(&mut driver, Report::Buttons)?;
    let mut buttons = before.clone();
    if let (Some(button), Some(action)) = (only, action) {
        if let Action::Macro { slot } = action {
            macro_wire::read_decoded(&mut driver, slot)?;
        }
        actions::set_button_action(&mut buttons, button, action)?;
        set_setting(&mut driver, out, Report::Buttons, &before, &buttons)?;
    }
    show_buttons(out, &buttons, only, &button_names)
}
