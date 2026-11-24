use std::{collections::VecDeque, io, time::Duration};

use crate::{
    actions::{MACRO_LEN, validate_macro_page},
    driver::{Connection, RECEIVER_RESET_PREAMBLE, RESET_PREAMBLE, Report, Transport},
    operations::Settings,
};

pub struct Simulator {
    connection: Connection,
    firmware: u16,
    settings: Settings,
    request: Option<(Report, u8)>,
    events: VecDeque<Vec<u8>>,
    macros: [Option<[u8; MACRO_LEN]>; 18],
    partial_macro: Option<([u8; MACRO_LEN], u8)>,
    #[cfg(test)]
    fail_after: Option<Report>,
}

impl Simulator {
    pub fn new(connection: Connection) -> Self {
        let status = match connection {
            Connection::Wired => vec![3, 0x10, 0x40, 2, 10],
            Connection::Receiver => vec![3, 0x10, 0x40, 2, 8],
            Connection::Bluetooth => vec![3, 0x10, 0x60, 8, 0],
        };
        Self {
            connection,
            firmware: 0x1001,
            settings: Settings::defaults(),
            request: None,
            events: VecDeque::from([status]),
            macros: [None; 18],
            partial_macro: None,
            #[cfg(test)]
            fail_after: None,
        }
    }

    pub fn legacy() -> Self {
        Self {
            firmware: 0x0f01,
            ..Self::new(Connection::Wired)
        }
    }

    #[cfg(test)]
    pub fn fail_once_after_writing(&mut self, report: Report) {
        self.fail_after = Some(report);
    }

    fn begin_query(&mut self, frame: &[u8]) -> io::Result<()> {
        let &[0xa0, id, length, 0, selector, 0, 0, 0] = frame else {
            return Err(invalid("malformed simulated query"));
        };
        let report = report_type(id)?;
        let selector_ok = match report {
            Report::Version | Report::ProfileRange => selector == 0,
            Report::Macro => (1..=18).contains(&selector),
            _ => selector == 1,
        };
        if usize::from(length) != report.length() || !selector_ok {
            return Err(invalid("wrong length or selector in simulated query"));
        }
        self.request = Some((report, selector));
        Ok(())
    }

    fn legacy_frame(&self) -> Vec<u8> {
        let mut frame = vec![0; Report::Legacy.length()];
        frame[..3].copy_from_slice(&[0x0a, 128, 1]);
        frame[3] = self.settings.polling[3];
        frame[5..54].copy_from_slice(&self.settings.dpi[3..52]);
        frame[55..65].copy_from_slice(&self.settings.timing[3..13]);
        frame[65..121].copy_from_slice(&self.settings.buttons[3..59]);
        frame
    }

    fn receive_macro_chunk(&mut self, frame: &[u8]) -> io::Result<()> {
        if frame.len() != 64 || !(1..=18).contains(&frame[2]) || frame[3] > 2 {
            return Err(invalid("malformed simulated macro chunk"));
        }
        let slot = frame[2];
        let chunk = frame[3];
        let expected_tag = if self.connection == Connection::Receiver && chunk == 2 {
            0x0c
        } else {
            0x40
        };
        if frame[1] != expected_tag {
            return Err(invalid("wrong transport tag on simulated macro chunk"));
        }
        if chunk == 0 {
            let mut report = [0; MACRO_LEN];
            report[..3].copy_from_slice(&[0x09, 0x83, slot]);
            self.partial_macro = Some((report, 0));
        }
        let Some((mut report, next)) = self.partial_macro.take() else {
            return Err(invalid("macro chunk without a start"));
        };
        if next != chunk || report[2] != slot {
            return Err(invalid("out-of-order simulated macro chunk"));
        }
        let start = 3 + usize::from(chunk) * 60;
        let end = (start + 60).min(MACRO_LEN);
        report[start..end].copy_from_slice(&frame[4..4 + end - start]);
        if chunk < 2 {
            self.partial_macro = Some((report, chunk + 1));
        } else {
            validate_macro_page(&report)?;
            self.macros[usize::from(slot - 1)] = Some(report);
            self.acknowledge();
        }
        Ok(())
    }

    fn acknowledge(&mut self) {
        if self.connection == Connection::Receiver {
            self.events.push_back(vec![3, 0x10, 0x50, 0, 0]);
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn report_type(id: u8) -> io::Result<Report> {
    Report::SETTINGS
        .into_iter()
        .chain([
            Report::Macro,
            Report::Legacy,
            Report::Version,
            Report::ProfileRange,
        ])
        .find(|report| report.id() == id)
        .ok_or_else(|| invalid("unknown simulated report"))
}

impl Transport for Simulator {
    fn connection(&self) -> Connection {
        self.connection
    }

    fn set_feature(&mut self, frame: &[u8]) -> io::Result<()> {
        let &id = frame
            .first()
            .ok_or_else(|| invalid("empty simulated write"))?;
        if id == 0xa0 {
            return self.begin_query(frame);
        }
        if frame == RESET_PREAMBLE
            || (self.connection == Connection::Receiver && frame == RECEIVER_RESET_PREAMBLE)
        {
            self.acknowledge();
            return Ok(());
        }
        let kind = report_type(id)?;
        if kind == Report::Macro {
            if self.connection == Connection::Bluetooth {
                kind.validate(frame)?;
                let slot = frame[2];
                validate_macro_page(frame)?;
                self.macros[usize::from(slot - 1)] = Some(
                    frame
                        .try_into()
                        .map_err(|_| invalid("malformed simulated Bluetooth macro"))?,
                );
                return Ok(());
            }
            return self.receive_macro_chunk(frame);
        }
        kind.validate(frame)?;
        if kind == Report::Polling && self.connection == Connection::Bluetooth {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "simulated Bluetooth polling write",
            ));
        }
        let target = match kind {
            Report::Dpi => &mut self.settings.dpi,
            Report::Timing => &mut self.settings.timing,
            Report::Polling => &mut self.settings.polling,
            Report::Buttons => &mut self.settings.buttons,
            _ => return Err(invalid("unexpected simulated setting write")),
        };
        target.copy_from_slice(frame);
        #[cfg(test)]
        if self.fail_after == Some(kind) {
            self.fail_after = None;
            return Err(io::Error::other(
                "simulated failure after the device accepted a write",
            ));
        }
        self.acknowledge();
        Ok(())
    }

    fn get_feature(&mut self, id: u8, length: usize) -> io::Result<Vec<u8>> {
        if id == 0xa0 && length == 8 && self.request.is_some() {
            return Ok(vec![0xa0, 1, 0, 0, 0, 0, 0, 0]);
        }
        let (report, selector) = self
            .request
            .take()
            .ok_or_else(|| invalid("missing query before simulated readback"))?;
        if report.id() != id || report.length() != length {
            return Err(invalid("simulated readback does not match pending request"));
        }
        Ok(match report {
            Report::Version => {
                let [high, low] = self.firmware.to_be_bytes();
                vec![0x0b, 8, high, low, 0, 0, 0, 0]
            }
            Report::Legacy => self.legacy_frame(),
            Report::ProfileRange => vec![0x0c, 6, 1, 0xfe, 1, 0xfe],
            Report::Macro => self.macros[usize::from(selector - 1)]
                .as_ref()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "empty simulated macro slot")
                })?
                .to_vec(),
            _ if self.firmware >> 8 != 0x10 || self.connection == Connection::Bluetooth => {
                return Err(invalid("expected combined configuration readback"));
            }
            _ => self.settings.report(report).to_vec(),
        })
    }

    fn read_input(&mut self, _timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        Ok(self.events.pop_front())
    }
}
