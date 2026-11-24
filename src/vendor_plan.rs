//! Convert a model-specific Windows profile into the logical packets built by its page.

use std::io::{self, ErrorKind};

use crate::{
    actions::{self, BUTTONS_LEN, MACRO_LEN},
    driver::{Connection, RESET_PREAMBLE},
    models::Model,
    profiles::WindowsProfiles,
    settings::{DPI_CODES, DPI_LEN, POLLING_LEN, TIMING_LEN, stamp_checksum},
};

#[derive(Debug, Eq, PartialEq)]
pub struct Plan {
    pub dpi: [u8; DPI_LEN],
    pub timing: [u8; TIMING_LEN],
    pub polling: [u8; POLLING_LEN],
    pub buttons: [u8; BUTTONS_LEN],
    pub macros: Vec<Vec<u8>>,
}

impl Plan {
    /// Build from the selected record, using the model's Windows layout and column offsets.
    pub fn from_windows(model: &Model, data: &[u8], index: usize) -> io::Result<Self> {
        let record = WindowsProfiles::for_model(data, model)?.record(index)?;
        let (buttons, macros) = assignments(model, record)?;
        Ok(Self {
            dpi: dpi(model, record)?,
            timing: timing(model, record)?,
            polling: polling(model, record)?,
            buttons,
            macros,
        })
    }

    /// Collect logical frames in Windows apply order. BLE skips the polling report.
    pub fn reports(&self, connection: Connection, full_apply: bool) -> impl Iterator<Item = &[u8]> {
        full_apply
            .then_some(RESET_PREAMBLE.as_slice())
            .into_iter()
            .chain([self.dpi.as_slice(), self.timing.as_slice()])
            .chain((connection != Connection::Bluetooth).then_some(self.polling.as_slice()))
            .chain([self.buttons.as_slice()])
            .chain(self.macros.iter().map(Vec::as_slice))
    }

    /// Frame the logical reports for the FA65 receiver's 65-byte HID output writes.
    pub fn fast_receiver_reports(&self, model: &Model) -> io::Result<Vec<[u8; 65]>> {
        if !model.fast_receiver {
            return Err(invalid(
                "the selected model does not use the high-rate receiver",
            ));
        }
        let mut packets = Vec::new();
        for logical in self.reports(Connection::Receiver, true) {
            if logical[0] == 9 {
                if logical.len() != 111 {
                    return Err(invalid(
                        "the high-rate macro requires a 111-byte logical frame",
                    ));
                }
                for (index, payload) in logical.chunks(59).enumerate() {
                    packets.push(fast_packet(index as u8, payload));
                }
            } else {
                packets.push(fast_packet(0, logical));
            }
        }
        Ok(packets)
    }
}

pub(crate) fn fast_packet(index: u8, payload: &[u8]) -> [u8; 65] {
    let mut packet = [0; 65];
    let checksum = payload.len() + 3;
    packet[..3].copy_from_slice(&[4, (checksum + 2) as u8, index]);
    packet[3..checksum].copy_from_slice(payload);
    let sum = packet[..checksum]
        .iter()
        .copied()
        .map(u16::from)
        .sum::<u16>();
    packet[checksum..checksum + 2].copy_from_slice(&sum.to_be_bytes());
    packet
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

fn word(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().expect("checked Windows record"))
}

fn field(data: &[u8], at: usize, max: u8) -> io::Result<u8> {
    let raw = word(data, at);
    if raw > u32::from(max) {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("invalid Windows field at 0x{at:x}: {raw}"),
        ));
    }
    Ok(raw as u8)
}

fn dpi(model: &Model, record: &[u8]) -> io::Result<[u8; DPI_LEN]> {
    let mut frame = [0; DPI_LEN];
    frame[..3].copy_from_slice(&[4, DPI_LEN as u8, 1]);
    let packed =
        |a, b| -> io::Result<u8> { Ok(field(record, a, 15)? << 4 | field(record, b, 15)?) };
    let (first, second) = match model.slot {
        5 | 6 => (packed(0x8b4, 0x940)?, packed(0x944, 0x948)?),
        12 | 13 => (packed(0x8b4, 0x944)?, packed(0x948, 0x94c)?),
        8 => (field(record, 0x8b4, 255)?, field(record, 0x944, 255)?),
        1..=4 => (field(record, 0x944, 255)?, field(record, 0x940, 255)?),
        _ => (field(record, 0x948, 255)?, field(record, 0x944, 255)?),
    };
    frame[3] = first;
    frame[4] = second;
    if matches!(model.slot, 1 | 2 | 8) {
        let masks = if model.slot == 8 { 0x948 } else { 0x944 };
        frame[6] = field(record, masks, 255)?;
        frame[7] = field(record, masks + 4, 255)?;
    }
    let rgb_base = if model.extended_windows_files() {
        0x964
    } else {
        0x960
    };
    for stage in 0..8 {
        let index = word(record, 0x8c4 + stage * 4);
        let enabled = field(record, 0x904 + stage * 4, 1)?;
        if index == 0 && enabled != 0 {
            return Err(invalid("an enabled Windows DPI stage has no index"));
        }
        frame[5] |= enabled << stage;
        if index != 0 {
            let (low, high, bank, other) = dpi_index(model.slot, index)?;
            frame[8 + stage] = low;
            frame[16 + stage] = high;
            frame[6] |= u8::from(bank) << stage;
            frame[7] |= u8::from(other) << stage;
        }
        let offset = rgb_base + stage * 4;
        frame[25 + stage * 3..28 + stage * 3].copy_from_slice(&record[offset..offset + 3]);
    }
    let active = field(record, 0x934, 7)?;
    if frame[5] & (1 << active) == 0 {
        return Err(invalid("the selected Windows DPI stage is disabled"));
    }
    frame[24] = active + 1;
    frame[49] = 2;
    stamp_checksum(&mut frame, 50);
    Ok(frame)
}

/// Windows stores a UI index, which need not be a DPI value or a sensor register.
fn dpi_index(slot: u8, index: u32) -> io::Result<(u8, u8, bool, bool)> {
    match slot {
        1 | 2 | 5 | 6 | 8 | 12 | 13 => {
            let (ceiling, switch) = match slot {
                1 | 2 | 8 => (520, 520),
                5 => (720, 600),
                _ => (660, 520),
            };
            if !(1..=ceiling).contains(&index) {
                return Err(invalid(
                    "Windows linear DPI index is outside the model limits",
                ));
            }
            let bank = index > switch;
            let code = if bank { index - switch / 2 } else { index };
            let [low, high] = ((code - 1) as u16).to_le_bytes();
            Ok((low, high, bank, false))
        }
        _ => {
            let extended = matches!(slot, 11 | 14..=18);
            let ceiling = match slot {
                3 => 280,
                4 => 320,
                _ if extended => 350,
                _ => 340,
            };
            if !(1..=ceiling).contains(&index) {
                return Err(invalid(
                    "Windows lookup DPI index is outside the model limits",
                ));
            }
            Ok(match index as usize {
                i @ 1..=200 => (DPI_CODES[i - 1], 0, false, false),
                i @ 201..=220 => (DPI_CODES[i - 101], 1, false, false),
                i @ 221..=300 => (DPI_CODES[i - 101], 0, true, !extended),
                301 => (DPI_CODES[199], 1, true, !extended),
                i @ 302..=340 => (DPI_CODES[99 + (i - 300) / 2], 1, true, !extended),
                350 => (199, 0, true, true),
                i => (DPI_CODES[(i - 100) * 2 / 3], 0, true, true),
            })
        }
    }
}

fn timing(model: &Model, record: &[u8]) -> io::Result<[u8; TIMING_LEN]> {
    let extended = usize::from(model.extended_windows_files()) * 4;
    let mode = field(record, 0x94c + extended, 15)?;
    let speed = field(record, 0x928, 8)?;
    let sleep = field(record, 0x92c, 255)?;
    let brightness = if matches!(mode, 1 | 5) {
        field(record, 0x924, 15)?
    } else {
        8
    };
    let rgb = 0x95c + extended;
    let mut frame = [0; TIMING_LEN];
    frame[..11].copy_from_slice(&[
        5,
        15,
        1,
        mode << 4,
        sleep & 0xf0 | (9 - speed),
        sleep << 4 | brightness,
        record[rgb],
        record[rgb + 1],
        record[rgb + 2],
        field(record, 0x930, 255)?,
        field(record, 0x958 + extended, 255)?,
    ]);
    stamp_checksum(&mut frame, 11);
    Ok(frame)
}

fn polling(model: &Model, record: &[u8]) -> io::Result<[u8; POLLING_LEN]> {
    let code = match model.slot {
        7 | 13 => (field(record, 0x93c, 14)? + 1) << 4 | (field(record, 0x938, 14)? + 1),
        8 => [0x20, 0x10, 8, 4, 2, 1, 0x40][usize::from(field(record, 0x938, 6)?)],
        _ => 8 >> field(record, 0x938, 3)?,
    };
    Ok([6, 9, 1, code, !code, 0, 0, 0, 0])
}

fn macro_slot(model: &Model, index: usize) -> Option<u8> {
    if !model.extended_windows_files() {
        return Some(index as u8 + 1);
    }
    let order: &[u8] = match model.slot {
        7 | 9 => &[1, 5, 9, 0, 13, 17, 0, 21],
        8 | 10 => &[1, 5, 9, 0, 0, 0, 13, 17],
        16..=18 => &[1, 5, 9, 13, 17, 21],
        _ => &[1, 5, 9, 0, 13, 17],
    };
    order.get(index).copied().filter(|slot| *slot != 0)
}

type Assignments = ([u8; BUTTONS_LEN], Vec<Vec<u8>>);

fn assignments(model: &Model, record: &[u8]) -> io::Result<Assignments> {
    let extended = model.extended_windows_files();
    let (offset, stride) = if extended {
        (0x9a4, 0x1020)
    } else {
        (0x9a0, 0x4f8)
    };
    let mut data = [0; 54];
    let mut macros = Vec::new();
    for (index, bytes) in data.chunks_exact_mut(3).enumerate() {
        let start = offset + index * stride;
        let button = &record[start..start + stride];
        let slot = macro_slot(model, index).unwrap_or(0);
        bytes.copy_from_slice(&actions::windows_action(
            button[1], button[3], button[7], slot,
        )?);
        if button[1] == 0x11 && extended {
            macros.extend(extended_pages(model, button, slot)?);
        }
    }
    if !extended {
        for (slot, mut value) in actions::vendor_profile_macros(record)? {
            value.metadata = [value.metadata[0]; 3];
            macros.push(actions::encode_macro(slot, &value)?.to_vec());
        }
    }
    Ok((actions::make_button_report(&data), macros))
}

/// Packetize 16-byte inline events without splitting key/delay pairs or an extended event.
fn extended_pages(model: &Model, button: &[u8], head: u8) -> io::Result<Vec<Vec<u8>>> {
    let short_report = model.fast_receiver;
    let length = if short_report { 111 } else { MACRO_LEN };
    let pairs_start = if short_report { 9 } else { 29 };
    let count = usize::from(u16::from_le_bytes([button[0x44], button[0x45]]));
    let events = button[0x440..]
        .get(..count * 16)
        .ok_or_else(|| invalid("Windows inline macro exceeds its button record"))?;
    if model.slot == 10 {
        if events
            .chunks_exact(16)
            .any(|event| !matches!(event[2], 1 | 2))
        {
            return Err(invalid(
                "X11 SE's Windows sender supports single-page key macros",
            ));
        }
        let prefix = button[0x43c];
        let mut value = actions::Macro {
            mode: match button[0x0c] {
                0 => actions::MacroMode::Times,
                1 => actions::MacroMode::PressToStop,
                2 => actions::MacroMode::Hold,
                _ => return Err(invalid("unknown Windows macro playback mode")),
            },
            repeat: button[0x40].max(1),
            metadata: [prefix; 3],
            events: Vec::new(),
        };
        for event in events.chunks_exact(16) {
            value.events.push(actions::MacroEvent {
                code: event[1],
                pressed: event[2] == 1,
                delay_ms: u16::from_le_bytes([event[4], event[5]]),
                extended: None,
            });
        }
        return Ok(vec![actions::encode_macro(head, &value)?.to_vec()]);
    }
    let mut pages = Vec::new();
    let mut payload = Vec::with_capacity(100);
    let mut slot = head;
    let frame = |slot, payload: &[u8]| {
        let mut report = vec![0; length];
        report[..3].copy_from_slice(&[9, length as u8, slot]);
        if slot == head {
            report[3] = button[0x0c];
            report[4..7].copy_from_slice(&button[0x43c..0x43f]);
            report[7] = button[0x40].max(1);
        }
        report[pairs_start - 1] = (payload.len() / 2) as u8;
        report[pairs_start..pairs_start + payload.len()].copy_from_slice(payload);
        stamp_checksum(&mut report, length - 2);
        report
    };
    for event in events.chunks_exact(16) {
        let delay = u16::from_le_bytes([event[4], event[5]]);
        let long = delay >= 128;
        if delay > 25_599 {
            return Err(invalid(
                "Windows inline macro delay overflows its wire field",
            ));
        }
        let short =
            if long { delay % 100 } else { delay }.max(if short_report { 2 } else { 1 }) as u8;
        let mut encoded = [0; 8];
        let mut size = if event[2] == 0 {
            encoded[..6].copy_from_slice(&[short, 0xf6, event[9], event[8], event[13], event[12]]);
            6
        } else {
            if !matches!(event[2], 1 | 2) || matches!(event[1], 0..=3 | 0xf6 | 0xfe) {
                return Err(invalid("invalid Windows inline key event"));
            }
            encoded[..2].copy_from_slice(&[short | if event[2] == 1 { 0 } else { 0x80 }, event[1]]);
            2
        };
        if long {
            encoded[size..size + 2].copy_from_slice(&[(delay / 100) as u8, 3]);
            size += 2;
        }
        // Windows leaves one pair for the next-slot marker, including on the last page.
        if payload.len() + size > 98 {
            if slot - head >= 3 {
                return Err(invalid(
                    "macro crosses the next Windows button's four-slot allocation",
                ));
            }
            payload.extend_from_slice(&[slot + 1, 0xfe]);
            pages.push(frame(slot, &payload));
            slot += 1;
            payload.clear();
        }
        payload.extend_from_slice(&encoded[..size]);
    }
    pages.push(frame(slot, &payload));
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MODELS;

    fn put(record: &mut [u8], offset: usize, value: u32) {
        record[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn record(model: &Model) -> Vec<u8> {
        let (length, button) = if model.extended_windows_files() {
            (0x18ca4, 0x9a4)
        } else {
            (0x80e0, 0x9a0)
        };
        let mut record = vec![0; length];
        put(&mut record, 0x8c4, 16);
        put(&mut record, 0x904, 1);
        put(&mut record, 0x928, 6);
        put(&mut record, 0x938, 2);
        record[button + 1] = 1;
        record
    }

    #[test]
    fn every_model_projects_model_specific_dpi_buttons_polling_and_full_apply() {
        for model in MODELS {
            let mut source = record(&model);
            let (index, code, high, mask) = match model.slot {
                1 | 2 | 8 => (520, 7, 2, 0),
                3 => (280, 211, 0, 1),
                4 => (320, 129, 1, 1),
                5 => (720, 163, 1, 1),
                6 | 12 | 13 => (660, 143, 1, 1),
                7 | 9 | 10 => (340, 141, 1, 1),
                _ => (350, 199, 0, 1),
            };
            put(&mut source, 0x8c4, index);
            let projected = Plan::from_windows(&model, &source, 1).unwrap();
            assert_eq!(
                [projected.dpi[8], projected.dpi[16], projected.dpi[6]],
                [code, high, mask],
                "{}",
                model.name
            );
            assert_eq!(&projected.buttons[3..6], &[2, 0, 0]);
            assert_eq!(projected.timing[4], 3);
            assert_eq!(
                projected.polling[3],
                match model.slot {
                    7 | 13 => 0x13,
                    8 => 8,
                    _ => 2,
                }
            );
            for transport in [
                Connection::Wired,
                Connection::Receiver,
                Connection::Bluetooth,
            ] {
                let frames: Vec<_> = projected.reports(transport, true).collect();
                let ids: Vec<_> = frames.iter().map(|bytes| bytes[0]).collect();
                let expected = if transport == Connection::Bluetooth {
                    vec![12, 4, 5, 8]
                } else {
                    vec![12, 4, 5, 6, 8]
                };
                assert_eq!(ids, expected);
                assert_eq!(frames[0], [12, 10, 1, 254, 1, 254, 0, 0, 0, 0]);
            }
            assert!(Plan::from_windows(&model, &source, 0).is_err());
        }
    }

    #[test]
    fn extended_model_plans_use_constructor_slots_and_keep_f6_operands_on_one_page() {
        for model in &MODELS[6..] {
            if model.slot == 10 {
                continue;
            }
            let mut source = record(model);
            let index = if matches!(model.slot, 8 | 10) { 6 } else { 4 };
            let offset = 0x9a4 + 0x1020 * index;
            source[offset + 1] = 0x11;
            source[offset + 0x44] = 49;
            for i in 0..48 {
                let event = offset + 0x440 + i * 16;
                source[event + 1] = 4;
                source[event + 2] = 1;
                source[event + 4] = 10;
            }
            let extended = offset + 0x440 + 48 * 16;
            source[extended + 4] = 150;
            source[extended + 8..extended + 10].copy_from_slice(&[0x22, 0x11]);
            source[extended + 12..extended + 14].copy_from_slice(&[0x44, 0x33]);
            let plan = Plan::from_windows(model, &source, 1).unwrap();
            let head = if model.slot >= 16 { 17 } else { 13 };
            assert_eq!(
                &plan.buttons[3 + index * 3..6 + index * 3],
                &[0x12, 0, head]
            );
            assert_eq!(plan.macros.len(), 2);
            let count_at = if model.fast_receiver { 8 } else { 28 };
            let first = &plan.macros[0];
            let marker = count_at + usize::from(first[count_at]) * 2;
            assert_eq!(&first[marker - 1..marker + 1], &[head + 1, 0xfe]);
            assert_eq!(
                &plan.macros[1][count_at..count_at + 9],
                &[4, 50, 0xf6, 0x11, 0x22, 0x33, 0x44, 1, 3]
            );
        }
    }
    #[test]
    fn x11_se_uses_a_single_page_with_compact_delay_and_header() {
        let model = &MODELS[9];
        let mut source = record(model);
        let offset = 0x9a4;
        source[offset + 1] = 0x11;
        source[offset + 0x43c] = 0xda;
        source[offset + 0x44] = 1;
        source[offset + 0x441] = 4;
        source[offset + 0x442] = 1;
        source[offset + 0x444] = 10;
        let plan = Plan::from_windows(model, &source, 1).unwrap();
        assert_eq!(plan.macros.len(), 1);
        assert_eq!(&plan.macros[0][..8], &[9, 131, 1, 0, 0xda, 0xda, 0xda, 1]);
        assert_eq!(&plan.macros[0][28..31], &[1, 1, 4]);
    }
    #[test]
    fn all_compact_models_use_the_button_ordinal_and_repeated_metadata() {
        for model in &MODELS[..6] {
            let mut source = record(model);
            let offset = 0x9a0 + 0x4f8 * 2;
            source[offset + 1] = 0x11;
            source[offset + 0x43c..offset + 0x43f].copy_from_slice(&[42, 10, 8]);
            source[offset + 0x44] = 1;
            source[offset + 0x440..offset + 0x444].copy_from_slice(&[4, 1, 10, 0]);
            let plan = Plan::from_windows(model, &source, 1).unwrap();
            assert_eq!(&plan.buttons[9..12], &[0x12, 0, 3]);
            assert_eq!(plan.macros.len(), 1);
            assert_eq!(&plan.macros[0][..8], &[9, 131, 3, 0, 42, 42, 42, 1]);
            assert_eq!(&plan.macros[0][28..31], &[1, 1, 4]);
            actions::validate_macro_page(&plan.macros[0]).unwrap();
        }
    }
    #[test]
    fn fast_receiver_fixture_wraps_settings_and_both_macro_fragments() {
        for model in MODELS.iter().filter(|model| model.fast_receiver) {
            let mut source = record(model);
            let offset = 0x9a4 + 0x1020;
            source[offset + 1] = 0x11;
            source[offset + 0x44] = 1;
            source[offset + 0x441] = 4;
            source[offset + 0x442] = 1;
            let plan = Plan::from_windows(model, &source, 1).unwrap();
            let packets = plan.fast_receiver_reports(model).unwrap();
            assert_eq!(packets.len(), 7);
            assert_eq!(
                packets[0][..15],
                [4, 15, 0, 12, 10, 1, 254, 1, 254, 0, 0, 0, 0, 2, 39]
            );
            assert_eq!(packets[5][..3], [4, 64, 0]);
            assert_eq!(packets[6][..3], [4, 57, 1]);
            assert_eq!(&packets[5][3..62], &plan.macros[0][..59]);
            assert_eq!(&packets[6][3..55], &plan.macros[0][59..]);
            assert!(packets.iter().all(|packet| packet[64] == 0));
        }
    }
}
