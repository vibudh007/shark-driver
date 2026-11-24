use std::{
    fmt, io,
    thread::sleep,
    time::{Duration, Instant},
};

use crate::{actions, models, settings};

const QUERY_ID: u8 = 0xa0;
pub(crate) const RESET_PREAMBLE: [u8; 10] = [0x0c, 0x0a, 0x01, 0xfe, 0x01, 0xfe, 0, 0, 0, 0];
pub(crate) const RECEIVER_RESET_PREAMBLE: [u8; 6] = [0x0c, 6, 1, 0xfe, 1, 0xfe];
const STATUS_ID: u8 = 0x03;
const R1_MODEL: u8 = models::R1.receiver_tag;
const RECEIVER_ACK_TIMEOUT: Duration = Duration::from_millis(400);
const QUERY_DELAYS: [u64; 5] = [0, 40, 80, 160, 240];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Connection {
    Wired,
    Receiver,
    Bluetooth,
}

impl fmt::Display for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Wired => "wired",
            Self::Receiver => "receiver",
            Self::Bluetooth => "bluetooth",
        })
    }
}

pub trait Transport {
    fn connection(&self) -> Connection;
    fn battery_percentage(&self) -> io::Result<Option<u8>> {
        Ok(None)
    }
    fn firmware_identity(&self, reported: u16) -> u16 {
        reported
    }
    fn macro_slots(&self) -> u8 {
        18
    }
    fn query_attempts(&self) -> u8 {
        1
    }
    fn query_delay(&self) -> Duration {
        Duration::ZERO
    }
    fn set_feature(&mut self, report: &[u8]) -> io::Result<()>;
    fn get_feature(&mut self, id: u8, length: usize) -> io::Result<Vec<u8>>;
    fn read_input(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>>;
}

pub fn temporarily_unreadable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    ) || error.raw_os_error() == Some(libc::EPIPE)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Report {
    Dpi,
    Timing,
    Polling,
    Buttons,
    Macro,
    Legacy,
    Version,
    ProfileRange,
}

impl Report {
    pub const SETTINGS: [Self; 4] = [Self::Dpi, Self::Timing, Self::Polling, Self::Buttons];

    pub fn id(self) -> u8 {
        match self {
            Self::Dpi => 0x04,
            Self::Timing => 0x05,
            Self::Polling => 0x06,
            Self::Buttons => 0x08,
            Self::Macro => 0x09,
            Self::Legacy => 0x0a,
            Self::Version => 0x0b,
            Self::ProfileRange => 0x0c,
        }
    }

    pub fn length(self) -> usize {
        match self {
            Self::Dpi => settings::DPI_LEN,
            Self::Timing => settings::TIMING_LEN,
            Self::Polling => settings::POLLING_LEN,
            Self::Buttons => actions::BUTTONS_LEN,
            Self::Macro => actions::MACRO_LEN,
            Self::Legacy => settings::LEGACY_LEN,
            Self::Version => settings::VERSION_LEN,
            Self::ProfileRange => 6,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Dpi => "DPI",
            Self::Timing => "timing",
            Self::Polling => "polling",
            Self::Buttons => "buttons",
            Self::Macro => "macro",
            Self::Legacy => "legacy configuration",
            Self::Version => "firmware version",
            Self::ProfileRange => "device profile range",
        }
    }

    pub fn validate(self, bytes: &[u8]) -> io::Result<()> {
        let length_matches = if self == Self::Version {
            (4..=self.length()).contains(&bytes.len())
        } else {
            bytes.len() == self.length()
        };
        if length_matches
            && bytes[0] == self.id()
            && (!Self::SETTINGS.contains(&self) || usize::from(bytes[1]) == self.length())
        {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} report: expected ID 0x{:02x} and {} bytes, got {:?} and {} bytes",
                self.name(),
                self.id(),
                self.length(),
                bytes.get(..2).unwrap_or(bytes),
                bytes.len()
            ),
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    DpiStage(u8),
    PollingNotice(u16),
    Battery { level: u8, state: u8 },
    Acknowledged(u8),
    BatteryNotice(u16),
    LightMode(u8),
    ProfileNotice(u16),
    Other { class: u8, value: u16 },
}

impl Event {
    pub fn parse(report: &[u8]) -> Option<Self> {
        let &[STATUS_ID, R1_MODEL, class, lo, hi, ..] = report else {
            return None;
        };
        let value = u16::from_le_bytes([lo, hi]);
        Some(match class {
            0x10 if (1..=8).contains(&value) => Self::DpiStage(lo),
            0x20 => Self::PollingNotice(value),
            0x40 => Self::Battery {
                level: hi,
                state: lo,
            },
            0x50 => Self::Acknowledged(lo),
            0x60 => Self::BatteryNotice(value),
            0x70 => Self::LightMode((lo >> 4) & 0x0f),
            0x80 => Self::ProfileNotice(value),
            _ => Self::Other { class, value },
        })
    }
}

pub struct Driver {
    transport: Box<dyn Transport>,
    firmware: Option<u16>,
    model_seen: bool,
    receiver_mode_byte: bool,
}

impl Driver {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            firmware: None,
            model_seen: false,
            receiver_mode_byte: false,
        }
    }

    pub fn connection(&self) -> Connection {
        self.transport.connection()
    }

    pub fn firmware(&mut self) -> io::Result<u16> {
        if let Some(version) = self.firmware {
            return Ok(version);
        }
        let frame = self.query(Report::Version)?;
        let firmware = u16::from_be_bytes([frame[2], frame[3]]);
        if matches!(firmware, 0 | u16::MAX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid R1 firmware version bytes",
            ));
        }
        let firmware = self.firmware_identity(firmware);
        self.firmware = Some(firmware);
        Ok(firmware)
    }

    pub fn firmware_identity(&self, reported: u16) -> u16 {
        if self.receiver_mode_byte && reported.to_be_bytes()[0] == R1_MODEL {
            return u16::from(R1_MODEL) << 8;
        }
        self.transport.firmware_identity(reported)
    }

    pub fn query(&mut self, kind: Report) -> io::Result<Vec<u8>> {
        if kind == Report::Macro {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "macro readback requires a slot",
            ));
        }
        let selector = u8::from(!matches!(kind, Report::Version | Report::ProfileRange));
        self.query_at(kind, selector)
    }

    pub fn profile_range(&mut self) -> io::Result<(u8, u8)> {
        let frame = self.query(Report::ProfileRange)?;
        let [_, 6, active, active_check, count, count_check] = frame[..] else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed device profile range",
            ));
        };
        if active_check != !active
            || count_check != !count
            || !(1..=5).contains(&count)
            || !(1..=count).contains(&active)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid device profile range",
            ));
        }
        Ok((active, count))
    }

    pub fn macro_slots(&self) -> u8 {
        self.transport.macro_slots()
    }

    pub fn read_macro(&mut self, slot: u8) -> io::Result<Vec<u8>> {
        let maximum = self.macro_slots();
        if !(1..=maximum).contains(&slot) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("macro slot must be 1–{maximum}"),
            ));
        }
        self.firmware()?;
        self.query_at(Report::Macro, slot)
    }

    fn query_at(&mut self, kind: Report, selector: u8) -> io::Result<Vec<u8>> {
        let mut result = self.query_once(kind, selector);
        for _ in 1..self.transport.query_attempts() {
            if !result.as_ref().is_err_and(|error| {
                temporarily_unreadable(error) || error.kind() == io::ErrorKind::InvalidData
            }) {
                break;
            }
            result = self.query_once(kind, selector);
        }
        result
    }

    fn query_once(&mut self, kind: Report, selector: u8) -> io::Result<Vec<u8>> {
        let _timing = crate::timing::Span::start("driver.query_once");
        if self.connection() == Connection::Receiver && !self.model_seen {
            match self.next_event(Duration::ZERO) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
                Err(error) => return Err(error),
            }
        }
        let request = [
            QUERY_ID,
            kind.id(),
            kind.length() as u8,
            0,
            selector,
            0,
            0,
            0,
        ];
        self.transport.set_feature(&request)?;
        let initial = self.transport.query_delay();
        if !initial.is_zero() {
            sleep(initial);
        }
        self.wait_query_receipt()?;
        let mut result = self.transport.get_feature(kind.id(), kind.length());
        for delay in [60, 120] {
            if !result.as_ref().is_err_and(temporarily_unreadable) {
                break;
            }
            sleep(Duration::from_millis(delay));
            result = self.transport.get_feature(kind.id(), kind.length());
        }
        let mut report = result?;
        if self.connection() == Connection::Receiver
            && report.len() == kind.length()
            && report[0] == kind.id()
            && usize::from(report[1]) == kind.length() + 2
        {
            if kind == Report::Version && report[2] == R1_MODEL {
                // On this receiver variant the second identity byte tracks lighting mode.
                self.receiver_mode_byte = true;
            }
            report[1] = kind.length() as u8;
        }
        kind.validate(&report)?;
        if kind == Report::Macro && report.get(1..3) != Some(&[kind.length() as u8, selector]) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected macro frame for slot {selector}"),
            ));
        }
        Ok(report)
    }

    fn wait_query_receipt(&mut self) -> io::Result<()> {
        let _timing = crate::timing::Span::start("driver.wait_query_receipt");
        let mut last_error = None;
        for delay in QUERY_DELAYS {
            if delay != 0 {
                sleep(Duration::from_millis(delay));
            }
            match self.transport.get_feature(QUERY_ID, 8) {
                Ok(bytes) if (1..=8).contains(&bytes.len()) && bytes[0] == QUERY_ID => {
                    if bytes.get(1) == Some(&1) {
                        return Ok(());
                    }
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "malformed 0xa0 query receipt",
                    ));
                }
                Err(error) if temporarily_unreadable(&error) => last_error = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            last_error.map_or_else(
                || String::from("mouse did not acknowledge the feature query"),
                |error| format!("mouse did not acknowledge the feature query ({error})"),
            ),
        ))
    }

    pub fn next_event(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        let start = Instant::now();
        loop {
            let remaining = timeout.saturating_sub(start.elapsed());
            let Some(report) = self.transport.read_input(remaining)? else {
                return Ok(None);
            };
            if let Some(event) = self.interpret_event(&report)? {
                return Ok(Some(event));
            }
            if start.elapsed() >= timeout {
                return Ok(None);
            }
        }
    }

    fn interpret_event(&mut self, report: &[u8]) -> io::Result<Option<Event>> {
        if self.connection() == Connection::Receiver {
            if let Some(model) =
                models::Model::from_receiver_report(models::SHARED_RECEIVER, report)
            {
                if model != models::R1 {
                    return Err(io::Error::other(format!(
                        "receiver reports Attack Shark {}; an R1 operation was stopped",
                        model.name
                    )));
                }
            }
        }
        let event = Event::parse(report);
        self.model_seen |= event.is_some();
        Ok(event)
    }

    pub fn battery_percentage(&self) -> io::Result<Option<u8>> {
        self.transport.battery_percentage()
    }

    pub fn confirm_write_identity(&mut self) -> io::Result<()> {
        let _timing = crate::timing::Span::start("driver.confirm_write_identity");
        let firmware = self.firmware()?;
        let version_identifies_ble =
            self.connection() == Connection::Bluetooth && firmware.to_be_bytes()[0] == R1_MODEL;
        if self.connection() == Connection::Wired || version_identifies_ble || self.model_seen {
            return Ok(());
        }
        let timeout = if self.connection() == Connection::Receiver {
            Duration::from_secs(6)
        } else {
            Duration::from_millis(1500)
        };
        if self.next_event(timeout)?.is_some() {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "firmware 0x{firmware:04x} needs an R1 status event before writes; move the mouse and retry"
            ),
        ))
    }

    fn drain_pending_events(&mut self) -> io::Result<()> {
        // The receiver acknowledgment has no sequence number.
        for _ in 0..64 {
            let Some(bytes) = self.transport.read_input(Duration::ZERO)? else {
                return Ok(());
            };
            self.interpret_event(&bytes)?;
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "receiver event queue did not drain",
        ))
    }

    fn wait_receiver_ack(&mut self) -> io::Result<()> {
        let _timing = crate::timing::Span::start("driver.wait_receiver_ack");
        let start = Instant::now();
        while let Some(remaining) = RECEIVER_ACK_TIMEOUT.checked_sub(start.elapsed()) {
            match self.next_event(remaining)? {
                Some(Event::Acknowledged(0)) => return Ok(()),
                Some(Event::Acknowledged(code)) => {
                    return Err(io::Error::other(format!(
                        "receiver rejected write with status 0x{code:02x}"
                    )));
                }
                Some(_) => {}
                None => break,
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "receiver write acknowledgment timed out",
        ))
    }

    pub fn write_settings(&mut self, kind: Report, bytes: &[u8]) -> io::Result<()> {
        if !Report::SETTINGS.contains(&kind) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this report is read-only",
            ));
        }
        kind.validate(bytes)?;
        if bytes[2] != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid report write subcode",
            ));
        }
        if self.connection() == Connection::Bluetooth && kind == Report::Polling {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the R1 vendor Bluetooth path does not send polling writes",
            ));
        }
        self.send_control(bytes)
    }

    pub fn begin_reset(&mut self) -> io::Result<()> {
        if self.connection() != Connection::Receiver {
            return self.send_control(&RESET_PREAMBLE);
        }
        match self.send_control(&RECEIVER_RESET_PREAMBLE) {
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                if self.profile_range()? == (1, 1) {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            result => result,
        }
    }

    fn send_control(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.confirm_write_identity()?;
        if self.connection() != Connection::Receiver {
            return self.transport.set_feature(bytes);
        }
        self.drain_pending_events()?;
        self.transport.set_feature(bytes)?;
        self.wait_receiver_ack()
    }

    pub fn upload_macro(&mut self, report: &[u8]) -> io::Result<()> {
        actions::validate_macro_page(report)?;
        let receiver = self.connection() == Connection::Receiver;
        let chunks = actions::macro_chunks(report, receiver)?;
        self.confirm_write_identity()?;
        if self.connection() == Connection::Bluetooth {
            return self.transport.set_feature(report);
        }
        if receiver {
            self.drain_pending_events()?;
        }
        let spacing = Duration::from_millis(if receiver { 300 } else { 200 });
        for (index, chunk) in chunks.iter().enumerate() {
            self.transport.set_feature(chunk).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("macro chunk {} of 3: {error}", index + 1),
                )
            })?;
            if index + 1 != chunks.len() {
                sleep(spacing);
            }
        }
        if receiver {
            self.wait_receiver_ack()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn rejects_another_models_receiver_status_before_r1_feature_io() {
        struct OtherMouse;

        impl Transport for OtherMouse {
            fn connection(&self) -> Connection {
                Connection::Receiver
            }

            fn set_feature(&mut self, _: &[u8]) -> io::Result<()> {
                panic!("an R1 query was sent to a different model")
            }

            fn get_feature(&mut self, _: u8, _: usize) -> io::Result<Vec<u8>> {
                panic!("an R1 report was requested from a different model")
            }

            fn read_input(&mut self, _: Duration) -> io::Result<Option<Vec<u8>>> {
                Ok(Some(vec![0x03, 0x4d, 0x40, 0, 7]))
            }
        }

        let mut driver = Driver::new(Box::new(OtherMouse));
        let error = driver.query(Report::Version).unwrap_err();
        assert!(error.to_string().contains("Attack Shark X3"));
    }

    #[test]
    fn normalizes_receiver_lengths_without_rewriting_payloads() {
        struct Receiver(crate::simulator::Simulator);
        impl Transport for Receiver {
            fn connection(&self) -> Connection {
                Connection::Receiver
            }
            fn set_feature(&mut self, report: &[u8]) -> io::Result<()> {
                self.0.set_feature(report)
            }
            fn get_feature(&mut self, id: u8, length: usize) -> io::Result<Vec<u8>> {
                let mut reply = if id == 9 {
                    let mut erased = vec![0xff; length];
                    erased[..3].copy_from_slice(&[id, length as u8, 1]);
                    erased
                } else {
                    self.0.get_feature(id, length)?
                };
                if id != QUERY_ID {
                    reply[1] += 2;
                }
                Ok(reply)
            }
            fn read_input(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
                self.0.read_input(timeout)
            }
        }
        let simulator = crate::simulator::Simulator::new(Connection::Receiver);
        let mut driver = Driver::new(Box::new(Receiver(simulator)));
        for kind in Report::SETTINGS {
            let report = driver.query(kind).unwrap();
            assert_eq!(&report[..2], &[kind.id(), kind.length() as u8]);
        }
        assert_eq!(driver.profile_range().unwrap(), (1, 1));
        assert_eq!(driver.firmware().unwrap(), 0x1000);
        assert_eq!(driver.firmware_identity(0x1020), 0x1000);
        assert_eq!(driver.firmware_identity(0x1120), 0x1120);
        let page = driver.read_macro(1).unwrap();
        assert_eq!(&page[..3], &[9, 131, 1]);
        assert!(page[3..].iter().all(|&byte| byte == 0xff));
    }

    #[test]
    fn short_query_receipt_and_temporary_linux_stall_are_accepted() {
        struct ReplyFixture(VecDeque<io::Result<Vec<u8>>>);
        impl Transport for ReplyFixture {
            fn connection(&self) -> Connection {
                Connection::Wired
            }
            fn set_feature(&mut self, bytes: &[u8]) -> io::Result<()> {
                assert_eq!(bytes, [0xa0, 0x0b, 8, 0, 0, 0, 0, 0]);
                Ok(())
            }
            fn get_feature(&mut self, _id: u8, _length: usize) -> io::Result<Vec<u8>> {
                self.0.pop_front().expect("bounded fixture replies")
            }
            fn read_input(&mut self, _timeout: Duration) -> io::Result<Option<Vec<u8>>> {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "fixture has no status node",
                ))
            }
        }
        let mut driver = Driver::new(Box::new(ReplyFixture(VecDeque::from([
            Err(io::Error::from_raw_os_error(libc::EPIPE)),
            Ok(vec![0xa0, 1]),
            Ok(vec![0x0b, 0, 0x0f, 1]),
        ]))));
        assert_eq!(driver.firmware().unwrap(), 0x0f01);
        driver.confirm_write_identity().unwrap();
    }

    #[test]
    fn rejects_foreign_and_truncated_events() {
        assert_eq!(Event::parse(&[3, 0x11, 0x50, 0, 0]), None);
        assert_eq!(Event::parse(&[3, 0x10, 0x50, 0]), None);
        assert_eq!(
            Event::parse(&[3, 0x10, 0x50, 1, 0]),
            Some(Event::Acknowledged(1))
        );
        assert_eq!(
            Event::parse(&[3, 0x10, 0x40, 3, 7, 0]),
            Some(Event::Battery { level: 7, state: 3 })
        );
    }

    #[test]
    fn report_checks_id_and_declared_length() {
        assert!(
            Report::Polling
                .validate(&[6, 9, 1, 8, 247, 0, 0, 0, 0])
                .is_ok()
        );
        assert!(
            Report::Polling
                .validate(&[6, 8, 1, 8, 247, 0, 0, 0, 0])
                .is_err()
        );
        assert!(Report::Polling.validate(&[]).is_err());
    }
}
