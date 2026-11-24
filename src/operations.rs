use std::{io, thread::sleep, time::Duration};

use crate::{
    actions,
    driver::{Connection, Driver, Report, temporarily_unreadable},
    settings::{Dpi, Legacy, Polling, Timing},
};

pub struct Settings {
    pub dpi: Vec<u8>,
    pub timing: Vec<u8>,
    pub polling: Vec<u8>,
    pub buttons: Vec<u8>,
}

impl Settings {
    pub fn defaults() -> Self {
        Self {
            dpi: Dpi::default().write_bytes().to_vec(),
            timing: Timing::default().write_bytes().to_vec(),
            polling: Polling::default().write_bytes().to_vec(),
            buttons: actions::default_buttons().to_vec(),
        }
    }

    pub fn report(&self, kind: Report) -> &[u8] {
        match kind {
            Report::Dpi => &self.dpi,
            Report::Timing => &self.timing,
            Report::Polling => &self.polling,
            Report::Buttons => &self.buttons,
            _ => &[],
        }
    }
}

pub fn modern(firmware: u16) -> bool {
    firmware.to_be_bytes()[0] == 0x10
}

fn legacy_settings(driver: &mut Driver) -> io::Result<Settings> {
    let frame = driver.query(Report::Legacy)?;
    let legacy = Legacy::parse(&frame)?;
    Ok(Settings {
        dpi: legacy.dpi().write_bytes().to_vec(),
        timing: legacy.timing().write_bytes().to_vec(),
        polling: legacy.polling()?.write_bytes().to_vec(),
        buttons: legacy.buttons().to_vec(),
    })
}

pub fn capture(driver: &mut Driver) -> io::Result<(u16, Settings)> {
    let _timing = crate::timing::Span::start("operations.capture");
    let firmware = driver.firmware()?;
    if !modern(firmware) || driver.connection() == Connection::Bluetooth {
        return Ok((firmware, legacy_settings(driver)?));
    }
    Ok((
        firmware,
        Settings {
            dpi: driver.query(Report::Dpi)?,
            timing: driver.query(Report::Timing)?,
            polling: driver.query(Report::Polling)?,
            buttons: driver.query(Report::Buttons)?,
        },
    ))
}

pub fn read_setting(driver: &mut Driver, kind: Report) -> io::Result<Vec<u8>> {
    let _timing = crate::timing::Span::start("operations.read_setting");
    if !Report::SETTINGS.contains(&kind) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a settings report",
        ));
    }
    if modern(driver.firmware()?) && driver.connection() != Connection::Bluetooth {
        return driver.query(kind);
    }
    let frame = driver.query(Report::Legacy)?;
    let legacy = Legacy::parse(&frame)?;
    Ok(match kind {
        Report::Dpi => legacy.dpi().write_bytes().to_vec(),
        Report::Timing => legacy.timing().write_bytes().to_vec(),
        Report::Polling => legacy.polling()?.write_bytes().to_vec(),
        Report::Buttons => legacy.buttons().to_vec(),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a settings report",
            ));
        }
    })
}

pub fn validate_dpi_write(frame: &[u8]) -> io::Result<()> {
    let dpi = Dpi::parse(frame)?;
    let active = dpi.stage(dpi.active_stage())?;
    if !active.enabled || active.dpi.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "active DPI stage must be enabled with a recognized value",
        ));
    }
    Ok(())
}

pub fn outgoing(kind: Report, frame: &[u8]) -> io::Result<Vec<u8>> {
    kind.validate(frame)?;
    Ok(match kind {
        Report::Dpi => Dpi::parse(frame)?.write_bytes().to_vec(),
        Report::Timing => Timing::parse(frame)?.write_bytes().to_vec(),
        Report::Polling => Polling::parse(frame)?.write_bytes().to_vec(),
        Report::Buttons => {
            let mut result = frame.to_vec();
            result[2] = 1;
            let sum: u16 = result[3..57].iter().map(|&byte| u16::from(byte)).sum();
            result[57..59].copy_from_slice(&sum.to_be_bytes());
            result
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a settings report",
            ));
        }
    })
}

pub struct Change {
    kind: Report,
    before: Vec<u8>,
    after: Vec<u8>,
}

impl Change {
    pub fn new(kind: Report, before: &[u8], after: &[u8]) -> io::Result<Self> {
        Ok(Self {
            kind,
            before: outgoing(kind, before)?,
            after: outgoing(kind, after)?,
        })
    }
}

fn same_fields(kind: Report, left: &[u8], right: &[u8]) -> bool {
    let range = match kind {
        Report::Dpi => 3..49,
        Report::Timing => 3..11,
        Report::Polling => 3..4,
        Report::Buttons => 3..57,
        _ => return false,
    };
    left.get(range.clone()) == right.get(range)
}

fn write_and_verify(driver: &mut Driver, kind: Report, frame: &[u8]) -> io::Result<()> {
    let _timing = crate::timing::Span::start("operations.write_and_verify");
    match driver.write_settings(kind, frame) {
        Ok(()) => verify(driver, kind, frame),
        Err(error)
            if driver.connection() == Connection::Receiver
                && error.kind() == io::ErrorKind::TimedOut =>
        {
            verify(driver, kind, frame)
                .map_err(|readback| io::Error::new(readback.kind(), format!("{error}; {readback}")))
        }
        Err(error) => Err(error),
    }
}

fn verify(driver: &mut Driver, kind: Report, frame: &[u8]) -> io::Result<()> {
    let _timing = crate::timing::Span::start("operations.verify");
    let actual = read_setting(driver, kind)?;
    if same_fields(kind, frame, &actual) {
        return Ok(());
    }
    sleep(Duration::from_millis(100));
    if same_fields(kind, frame, &read_setting(driver, kind)?) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "readback differs from the requested fields",
    ))
}

fn restore(driver: &mut Driver, change: &Change) -> io::Result<()> {
    match read_setting(driver, change.kind) {
        Ok(current) if same_fields(change.kind, &current, &change.before) => return Ok(()),
        Ok(_) => {}
        Err(error)
            if error.kind() == io::ErrorKind::InvalidData || temporarily_unreadable(&error) => {}
        Err(error) => return Err(error),
    }
    write_and_verify(driver, change.kind, &change.before)
}

pub fn apply(driver: &mut Driver, changes: &[Change]) -> io::Result<Vec<Report>> {
    let mut pending = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        if changes[..index]
            .iter()
            .any(|prior| prior.kind == change.kind)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "duplicate settings group",
            ));
        }
        if !same_fields(change.kind, &change.before, &change.after) {
            if change.kind == Report::Dpi {
                validate_dpi_write(&change.after)?;
            }
            if change.kind == Report::Buttons {
                actions::validate_button_assignments(&change.after)?;
            }
            if driver.connection() == Connection::Bluetooth && change.kind == Report::Polling {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "polling changes require USB or the receiver",
                ));
            }
            pending.push(change);
        }
    }
    let mut completed = Vec::new();
    for (at, change) in pending.iter().enumerate() {
        if let Err(error) = write_and_verify(driver, change.kind, &change.after) {
            let issues: Vec<_> = pending[..=at]
                .iter()
                .rev()
                .filter_map(|prior| {
                    restore(driver, prior)
                        .err()
                        .map(|error| format!("{}: {error}", prior.kind.name()))
                })
                .collect();
            let recovery = if issues.is_empty() {
                String::from("verified original settings")
            } else {
                format!("could not verify restoration for {}", issues.join(", "))
            };
            return Err(io::Error::new(
                error.kind(),
                format!("{}: {error} ({recovery})", change.kind.name()),
            ));
        }
        completed.push(change.kind);
    }
    Ok(completed)
}

pub fn apply_profile(
    driver: &mut Driver,
    firmware: u16,
    target: &Settings,
) -> io::Result<Vec<Report>> {
    let actual = driver.firmware()?;
    if actual != driver.firmware_identity(firmware) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("profile firmware 0x{firmware:04x} differs from this mouse (0x{actual:04x})"),
        ));
    }
    let (_, previous) = capture(driver)?;
    apply_snapshot(driver, &previous, target)
}

fn apply_snapshot(
    driver: &mut Driver,
    before: &Settings,
    target: &Settings,
) -> io::Result<Vec<Report>> {
    let changes =
        Report::SETTINGS.map(|kind| Change::new(kind, before.report(kind), target.report(kind)));
    let [dpi, timing, polling, buttons] = changes;
    apply(driver, &[dpi?, timing?, polling?, buttons?])
}

pub fn reset(driver: &mut Driver) -> io::Result<Vec<Report>> {
    let (_, previous) = capture(driver)?;
    let defaults = Settings::defaults();
    let changes: Vec<_> = Report::SETTINGS
        .into_iter()
        .filter(|kind| *kind != Report::Polling || driver.connection() != Connection::Bluetooth)
        .map(|kind| Change::new(kind, previous.report(kind), defaults.report(kind)))
        .collect::<io::Result<_>>()?;
    validate_dpi_write(&defaults.dpi)?;
    actions::validate_button_assignments(&defaults.buttons)?;
    driver.confirm_write_identity()?;
    let result = (|| {
        driver.begin_reset()?;
        for change in &changes {
            match driver.write_settings(change.kind, &change.after) {
                Err(error)
                    if driver.connection() == Connection::Receiver
                        && error.kind() == io::ErrorKind::TimedOut => {}
                result => result?,
            }
        }
        for change in &changes {
            verify(driver, change.kind, &change.after)?;
        }
        Ok(changes.iter().map(|change| change.kind).collect())
    })();
    result.map_err(|error: io::Error| {
        let issues: Vec<_> = changes
            .iter()
            .rev()
            .filter_map(|change| {
                restore(driver, change)
                    .err()
                    .map(|error| format!("{}: {error}", change.kind.name()))
            })
            .collect();
        let recovery = if issues.is_empty() {
            String::from("original settings fields verified")
        } else {
            format!("settings restoration incomplete: {}", issues.join(", "))
        };
        io::Error::new(error.kind(), format!("vendor reset: {error} ({recovery})"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::Connection;
    use crate::simulator::Simulator;

    #[test]
    fn vendor_reset_sends_preamble_then_full_groups_before_readback() {
        use crate::driver::{RECEIVER_RESET_PREAMBLE, RESET_PREAMBLE, Transport};
        use std::{cell::RefCell, rc::Rc};
        struct Trace(Simulator, Rc<RefCell<Vec<Vec<u8>>>>);
        impl Transport for Trace {
            fn connection(&self) -> Connection {
                self.0.connection()
            }
            fn set_feature(&mut self, bytes: &[u8]) -> io::Result<()> {
                self.1.borrow_mut().push(bytes.to_vec());
                self.0.set_feature(bytes)
            }
            fn get_feature(&mut self, id: u8, len: usize) -> io::Result<Vec<u8>> {
                self.0.get_feature(id, len)
            }
            fn read_input(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
                self.0.read_input(timeout)
            }
        }
        for transport in [
            Connection::Wired,
            Connection::Receiver,
            Connection::Bluetooth,
        ] {
            let trace = Rc::new(RefCell::new(Vec::new()));
            let mut driver = Driver::new(Box::new(Trace(Simulator::new(transport), trace.clone())));
            reset(&mut driver).unwrap();
            let trace = trace.borrow();
            let preamble: &[u8] = if transport == Connection::Receiver {
                &RECEIVER_RESET_PREAMBLE
            } else {
                &RESET_PREAMBLE
            };
            let start = trace.iter().position(|frame| frame == preamble).unwrap();
            let settings: &[u8] = if transport == Connection::Bluetooth {
                &[4, 5, 8]
            } else {
                &[4, 5, 6, 8]
            };
            let sent: Vec<_> = trace[start + 1..]
                .iter()
                .take_while(|frame| frame[0] != 0xa0)
                .map(|frame| frame[0])
                .collect();
            assert_eq!(sent, settings);
        }
    }

    #[test]
    fn verifies_receiver_writes_and_reset_when_receipts_are_lost() {
        use crate::driver::{Event, Transport};

        struct MissedReceipts(Simulator);
        impl Transport for MissedReceipts {
            fn connection(&self) -> Connection {
                self.0.connection()
            }
            fn set_feature(&mut self, bytes: &[u8]) -> io::Result<()> {
                self.0.set_feature(bytes)
            }
            fn get_feature(&mut self, id: u8, length: usize) -> io::Result<Vec<u8>> {
                self.0.get_feature(id, length)
            }
            fn read_input(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
                while let Some(report) = self.0.read_input(timeout)? {
                    if !matches!(Event::parse(&report), Some(Event::Acknowledged(_))) {
                        return Ok(Some(report));
                    }
                }
                Ok(None)
            }
        }

        let simulator = Simulator::new(Connection::Receiver);
        let mut driver = Driver::new(Box::new(MissedReceipts(simulator)));
        let (firmware, mut desired) = capture(&mut driver).unwrap();
        let mut polling = Polling::parse(&desired.polling).unwrap();
        polling.set_hz(500).unwrap();
        desired.polling = polling.write_bytes().to_vec();
        assert_eq!(
            apply_profile(&mut driver, firmware, &desired).unwrap(),
            [Report::Polling]
        );
        assert_eq!(reset(&mut driver).unwrap(), Report::SETTINGS);
        let current = read_setting(&mut driver, Report::Polling).unwrap();
        assert_eq!(Polling::parse(&current).unwrap().hz(), 1000);
    }

    #[test]
    fn rejects_a_dpi_write_with_no_enabled_active_stage() {
        let mut frame = Dpi::default().write_bytes();
        frame[5] = 0;
        assert!(validate_dpi_write(&frame).is_err());
    }

    #[test]
    fn skips_unchanged_groups_and_checks_readback() {
        let mut driver = Driver::new(Box::new(Simulator::new(Connection::Wired)));
        let (_, before) = capture(&mut driver).unwrap();
        let mut polling = Polling::parse(&before.polling).unwrap();
        polling.set_hz(250).unwrap();
        let change = Change::new(Report::Polling, &before.polling, &polling.write_bytes()).unwrap();
        assert_eq!(
            apply(&mut driver, &[change]).unwrap(),
            vec![Report::Polling]
        );
        let after = read_setting(&mut driver, Report::Polling).unwrap();
        assert_eq!(Polling::parse(&after).unwrap().hz(), 250);
        let unchanged = Change::new(Report::Polling, &after, &after).unwrap();
        assert!(apply(&mut driver, &[unchanged]).unwrap().is_empty());
    }

    #[test]
    fn restores_a_nondefault_wired_snapshot_after_writes_and_reset() {
        let mut driver = Driver::new(Box::new(Simulator::new(Connection::Wired)));
        let (firmware, mut initial) = capture(&mut driver).unwrap();
        let mut polling = Polling::parse(&initial.polling).unwrap();
        polling.set_hz(500).unwrap();
        initial.polling = polling.write_bytes().to_vec();
        apply_profile(&mut driver, firmware, &initial).unwrap();
        let (_, baseline) = capture(&mut driver).unwrap();

        let mut edited = Settings::defaults();
        let mut dpi = Dpi::parse(&edited.dpi).unwrap();
        dpi.set_stage_dpi(1, 950).unwrap();
        dpi.set_stage_color(1, [0x44, 0xaa, 0xee]).unwrap();
        dpi.set_active_stage(2).unwrap();
        dpi.set_angle_snap(true);
        dpi.set_ripple_control(true);
        edited.dpi = dpi.write_bytes().to_vec();
        let mut timing = Timing::parse(&edited.timing).unwrap();
        timing.set_sleep_seconds(450).unwrap();
        timing.set_deep_sleep_minutes(12).unwrap();
        timing.set_debounce_ms(10).unwrap();
        timing.set_light_mode(1).unwrap();
        timing.set_brightness(2).unwrap();
        timing.set_light_color([0x44, 0xaa, 0xee]);
        edited.timing = timing.write_bytes().to_vec();
        polling.set_hz(250).unwrap();
        edited.polling = polling.write_bytes().to_vec();
        actions::set_button_action(
            &mut edited.buttons,
            actions::parse_button("forward").unwrap(),
            actions::parse_action("key:ctrl+c").unwrap(),
        )
        .unwrap();
        assert_eq!(
            apply_profile(&mut driver, firmware, &edited).unwrap(),
            Report::SETTINGS
        );
        timing.set_light_mode(2).unwrap();
        timing.set_breath_speed(5).unwrap();
        edited.timing = timing.write_bytes().to_vec();
        assert_eq!(
            apply_profile(&mut driver, firmware, &edited).unwrap(),
            [Report::Timing]
        );
        assert_eq!(reset(&mut driver).unwrap(), Report::SETTINGS);
        apply_profile(&mut driver, firmware, &baseline).unwrap();
        let (_, restored) = capture(&mut driver).unwrap();
        for kind in Report::SETTINGS {
            assert_eq!(restored.report(kind), baseline.report(kind), "{kind:?}");
        }
    }

    #[test]
    fn verifies_rollback_after_a_transport_error_with_a_partial_write() {
        let mut device = Simulator::new(Connection::Receiver);
        device.fail_once_after_writing(Report::Timing);
        let mut driver = Driver::new(Box::new(device));
        let (firmware, before) = capture(&mut driver).unwrap();
        let mut requested = Settings::defaults();
        let mut dpi = Dpi::parse(&requested.dpi).unwrap();
        dpi.set_stage_dpi(1, 950).unwrap();
        requested.dpi = dpi.write_bytes().to_vec();
        let mut timing = Timing::parse(&requested.timing).unwrap();
        timing.set_sleep_seconds(450).unwrap();
        requested.timing = timing.write_bytes().to_vec();
        let error = apply_profile(&mut driver, firmware, &requested).unwrap_err();
        assert!(error.to_string().contains("verified original settings"));
        let (_, after) = capture(&mut driver).unwrap();
        for kind in Report::SETTINGS {
            assert!(same_fields(kind, before.report(kind), after.report(kind)));
        }
    }

    #[test]
    fn bluetooth_rejects_polling_before_any_settings_change() {
        let mut driver = Driver::new(Box::new(Simulator::new(Connection::Bluetooth)));
        let (firmware, before) = capture(&mut driver).unwrap();
        let mut desired = Settings::defaults();
        let mut polling = Polling::parse(&desired.polling).unwrap();
        polling.set_hz(250).unwrap();
        desired.polling = polling.write_bytes().to_vec();
        let mut dpi = Dpi::parse(&desired.dpi).unwrap();
        dpi.set_stage_dpi(1, 950).unwrap();
        desired.dpi = dpi.write_bytes().to_vec();
        assert_eq!(
            apply_profile(&mut driver, firmware, &desired)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        let unchanged = read_setting(&mut driver, Report::Dpi).unwrap();
        assert!(same_fields(Report::Dpi, &before.dpi, &unchanged));
    }
}
