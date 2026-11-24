//! FA65 HID output and settings reply transactions.

use std::{
    io::{self, ErrorKind},
    thread::sleep,
    time::{Duration, Instant},
};

use crate::{
    model_wire::{decode_model_dpi, valid_rate_code},
    models::Model,
    settings::{Dpi, Timing},
    vendor_plan::fast_packet,
};

const QUERY_TIMEOUT: Duration = Duration::from_millis(900);
const SETTLE: Duration = Duration::from_millis(200);

pub trait Reports {
    fn send(&mut self, packet: &[u8]) -> io::Result<()>;
    fn receive(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>>;
}

pub struct Session<T> {
    model: &'static Model,
    wire: T,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

fn query_packet(id: u8, size: u8) -> [u8; 64] {
    let mut bytes = [0; 64];
    bytes[..6].copy_from_slice(&[4, 12, 0xa0, id, size, 1]);
    let sum = bytes[..10].iter().copied().map(u16::from).sum::<u16>();
    bytes[10..12].copy_from_slice(&sum.to_be_bytes());
    bytes
}

fn unwrap_reply(packet: &[u8], id: u8, size: usize) -> Option<&[u8]> {
    let end = size.checked_add(3)?;
    let expected = [4, (end + 2) as u8, 0, id, size as u8];
    if !packet.starts_with(&expected) || packet.len() < end + 2 {
        return None;
    }
    let sum = packet[..end].iter().copied().map(u16::from).sum::<u16>();
    (packet[end..end + 2] == sum.to_be_bytes()).then_some(&packet[3..end])
}

impl<T: Reports> Session<T> {
    pub fn new(model: &'static Model, wire: T) -> io::Result<Self> {
        if !model.fast_receiver {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "not a high-rate receiver model",
            ));
        }
        Ok(Self { model, wire })
    }

    pub fn read(&mut self, id: u8, size: usize) -> io::Result<Vec<u8>> {
        if !matches!((id, size), (4, 56) | (5, 15) | (6, 9)) {
            return Err(invalid("unsupported high-rate receiver read selector"));
        }
        // Discard stale input on the configuration collection before requesting a baseline.
        for _ in 0..32 {
            if self.wire.receive(Duration::ZERO)?.is_none() {
                self.wire.send(&query_packet(id, size as u8))?;
                let start = Instant::now();
                for _ in 0..32 {
                    let Some(remaining) = QUERY_TIMEOUT.checked_sub(start.elapsed()) else {
                        break;
                    };
                    let Some(packet) = self.wire.receive(remaining)? else {
                        break;
                    };
                    if let Some(reply) = unwrap_reply(&packet, id, size) {
                        return Ok(reply.to_vec());
                    }
                }
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    "high-rate receiver did not return a validated settings report",
                ));
            }
        }
        Err(io::Error::new(
            ErrorKind::WouldBlock,
            "high-rate receiver queue did not drain",
        ))
    }

    fn update<const N: usize>(
        &mut self,
        before: &[u8; N],
        after: &[u8; N],
        fields: std::ops::Range<usize>,
        delay: impl Fn(Duration),
    ) -> io::Result<()> {
        if before[fields.clone()] == after[fields.clone()] {
            return Ok(());
        }
        let write = |session: &mut Self, packet: &[u8; N]| {
            session.wire.send(&fast_packet(0, packet))?;
            delay(SETTLE);
            let current = session.read(packet[0], N)?;
            if current[fields.clone()] != packet[fields.clone()] {
                return Err(invalid("high-rate receiver write did not verify"));
            }
            Ok(())
        };
        if let Err(error) = write(self, after) {
            let unchanged = self
                .read(before[0], N)
                .is_ok_and(|reply| reply[fields.clone()] == before[fields.clone()]);
            let restored = unchanged || write(self, before).is_ok();
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "{error}. Original setting {}",
                    if restored {
                        "verified"
                    } else {
                        "could not be verified"
                    }
                ),
            ));
        }
        Ok(())
    }

    pub fn dpi(
        &mut self,
        edit: Option<impl FnOnce(&mut Dpi) -> io::Result<()>>,
    ) -> io::Result<Dpi> {
        let original = decode_model_dpi(self.model, &self.read(4, 56)?)?;
        let Some(edit) = edit else {
            return Ok(original);
        };
        let mut desired = original.clone();
        edit(&mut desired)?;
        desired.set_active_stage(desired.active_stage())?;
        self.update(
            &original.write_bytes(),
            &desired.write_bytes(),
            3..49,
            sleep,
        )?;
        Ok(desired)
    }

    pub fn timing(
        &mut self,
        edit: Option<impl FnOnce(&mut Timing) -> io::Result<()>>,
    ) -> io::Result<Timing> {
        let original = Timing::parse(&self.read(5, 15)?)?;
        let Some(edit) = edit else {
            return Ok(original);
        };
        let mut desired = original.clone();
        edit(&mut desired)?;
        self.update(
            &original.write_bytes(),
            &desired.write_bytes(),
            3..11,
            sleep,
        )?;
        Ok(desired)
    }

    pub fn polling(&mut self, desired: Option<u16>) -> io::Result<u16> {
        self.polling_with_delay(desired, sleep)
    }

    fn polling_with_delay(
        &mut self,
        desired: Option<u16>,
        delay: impl Fn(Duration),
    ) -> io::Result<u16> {
        let before: [u8; 9] = self
            .read(6, 9)?
            .try_into()
            .map_err(|_| invalid("short polling reply"))?;
        let code = before[3];
        if before[4] != !code || !valid_rate_code(self.model.slot, code) {
            return Err(invalid("invalid high-rate receiver polling code"));
        }
        let rates = [125, 250, 500, 1000, 2000, 4000, 8000];
        let x3_codes = [32, 16, 8, 4, 2, 1, 64];
        let selected = if self.model.slot == 8 {
            x3_codes
                .iter()
                .position(|&value| value == code)
                .ok_or_else(|| invalid("invalid X3 Pro rate"))?
        } else {
            usize::from((code >> 4) - 1)
        };
        let current = rates[selected];
        let Some(rate) = desired.filter(|rate| *rate != current) else {
            return Ok(current);
        };
        let index = rates
            .iter()
            .position(|&value| value == rate)
            .ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidInput, "unknown high-rate polling rate")
            })?;
        let next = if self.model.slot == 8 {
            x3_codes[index]
        } else {
            ((index + 1) as u8) << 4 | code & 0xf
        };
        let original = [6, 9, 1, code, !code, 0, 0, 0, 0];
        let replacement = [6, 9, 1, next, !next, 0, 0, 0, 0];
        self.update(&original, &replacement, 3..5, delay)?;
        Ok(rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Receiver {
        code: u8,
        replies: VecDeque<Vec<u8>>,
        writes: Vec<u8>,
        drop_replies: bool,
        reject_next_write: bool,
    }

    impl Reports for Receiver {
        fn send(&mut self, packet: &[u8]) -> io::Result<()> {
            if packet[2] == 0xa0 {
                assert_eq!(packet.len(), 64);
                assert_eq!(packet[..12], [4, 12, 0xa0, 6, 9, 1, 0, 0, 0, 0, 0, 0xc0]);
                assert_eq!(packet[12..], [0; 52]);
                if !self.drop_replies {
                    self.replies.push_back(vec![4, 4, 0xc0, 1]);
                    let mut report = vec![4, 14, 0, 6, 9, 1, self.code, !self.code, 0, 0, 0, 0];
                    report.extend_from_slice(&289u16.to_be_bytes());
                    self.replies.push_back(report);
                }
            } else {
                assert_eq!(packet[..6], [4, 14, 0, 6, 9, 1]);
                assert_eq!(packet[6], !packet[7]);
                self.writes.push(packet[6]);
                if !std::mem::take(&mut self.reject_next_write) {
                    self.code = packet[6];
                }
            }
            Ok(())
        }

        fn receive(&mut self, _: Duration) -> io::Result<Option<Vec<u8>>> {
            Ok(self.replies.pop_front())
        }
    }

    #[test]
    fn receiver_settings_transaction_fixtures() {
        for (slot, original, requested) in [(7, 0x43, 0x73), (8, 4, 64), (13, 0x45, 0x75)] {
            let model = &crate::models::MODELS[slot - 1];
            let wire = Receiver {
                code: original,
                ..Receiver::default()
            };
            let mut session = Session::new(model, wire).unwrap();
            assert_eq!(session.polling_with_delay(None, |_| {}).unwrap(), 1000);
            assert_eq!(
                session.polling_with_delay(Some(8000), |_| {}).unwrap(),
                8000
            );
            assert_eq!(session.wire.writes, [requested]);
            assert_eq!(
                session.polling_with_delay(Some(8000), |_| {}).unwrap(),
                8000
            );
            assert_eq!(session.wire.writes, [requested]);

            session.wire.reject_next_write = true;
            let err = session.polling_with_delay(Some(1000), |_| {}).unwrap_err();
            assert!(err.to_string().contains("Original setting verified"));
            assert_eq!(session.wire.code, requested);

            session.wire.drop_replies = true;
            let err = session.polling_with_delay(Some(125), |_| {}).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::TimedOut);
            assert_eq!(session.wire.writes, [requested, original]);
        }
    }
}
