use std::io;

use crate::{
    actions::{self, Action, Macro},
    driver::{Connection, Driver, Report},
    operations,
    profiles::decode_hex,
};

type Page = (u8, Vec<u8>);

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn next_slot(connection: Connection, slot: u8, marker: u8) -> u8 {
    if connection == Connection::Bluetooth {
        marker
    } else {
        slot + 1
    }
}

pub fn read(driver: &mut Driver, slot: u8) -> io::Result<Vec<Page>> {
    let mut pages = Vec::new();
    let mut next = slot;
    loop {
        if !(1..=18).contains(&next) || pages.iter().any(|(slot, _)| *slot == next) {
            return Err(invalid(
                "macro continuation is out of range or contains a cycle",
            ));
        }
        let page = driver.read_macro(next)?;
        let marker = actions::macro_continuation(&page)?;
        pages.push((next, page));
        match marker {
            Some(marker) => next = next_slot(driver.connection(), next, marker),
            None => return Ok(pages),
        }
    }
}

pub fn read_decoded(driver: &mut Driver, slot: u8) -> io::Result<Macro> {
    let pages = read(driver, slot)?;
    let frames: Vec<_> = pages.iter().map(|(_, page)| page.as_slice()).collect();
    actions::decode_macro_pages(&frames, slot)
}

fn writable(mut bytes: Vec<u8>, slot: u8) -> io::Result<Vec<u8>> {
    actions::macro_continuation(&bytes)?;
    if bytes[2] != slot {
        return Err(invalid("macro frame slot differs from the archive slot"));
    }
    // Some readers return the action body without a checksum trailer.
    if bytes[129..131] == [0, 0] {
        let sum: u16 = bytes[3..129].iter().copied().map(u16::from).sum();
        bytes[129..131].copy_from_slice(&sum.to_be_bytes());
    }
    actions::validate_macro_page(&bytes)?;
    Ok(bytes)
}

fn parse(source: &str, connection: Connection, head: u8) -> io::Result<(u16, Vec<Page>)> {
    let mut lines = source.lines();
    let firmware = lines
        .next()
        .and_then(|line| line.strip_prefix("firmware "))
        .filter(|value| value.len() == 4)
        .and_then(|value| u16::from_str_radix(value, 16).ok())
        .ok_or_else(|| invalid("macro archive must start with a four-digit firmware identity"))?;
    let mut pages = Vec::new();
    let mut expected = Some(head);
    for line in lines.filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_ascii_whitespace();
        let (Some(slot), Some(data), None) = (fields.next(), fields.next(), fields.next()) else {
            return Err(invalid("expected a slot and hexadecimal macro page"));
        };
        let slot = slot
            .parse::<u8>()
            .map_err(|_| invalid("invalid archive slot"))?;
        if expected != Some(slot)
            || !(1..=18).contains(&slot)
            || pages.iter().any(|(prior, _)| *prior == slot)
        {
            return Err(invalid(
                "archive pages do not form a valid chain for this transport",
            ));
        }
        let bytes = writable(decode_hex(data)?, slot)?;
        expected =
            actions::macro_continuation(&bytes)?.map(|marker| next_slot(connection, slot, marker));
        pages.push((slot, bytes));
    }
    if expected.is_some() {
        return Err(invalid(
            "macro archive is empty or ends before the chain is complete",
        ));
    }
    Ok((firmware, pages))
}

pub const BACKUP_HEADER: &str = "attack-shark-r1 macro backup 1";

/// Compare each listed raw page without attempting to program any slot.
pub fn verify_backup(driver: &mut Driver, source: &str) -> io::Result<usize> {
    let mut lines = source.lines();
    if lines.next() != Some(BACKUP_HEADER) {
        return Err(invalid("unrecognized raw macro backup format"));
    }
    let firmware = lines
        .next()
        .and_then(|line| line.strip_prefix("firmware "))
        .filter(|value| value.len() == 4)
        .and_then(|value| u16::from_str_radix(value, 16).ok())
        .ok_or_else(|| invalid("macro backup requires a four-digit firmware identity"))?;
    let mut slots = Vec::new();
    for line in lines.filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_ascii_whitespace();
        let (Some(slot), Some(data), None) = (fields.next(), fields.next(), fields.next()) else {
            return Err(invalid("expected a slot and raw macro page"));
        };
        let slot: u8 = slot.parse().map_err(|_| invalid("invalid backup slot"))?;
        if !(1..=driver.macro_slots()).contains(&slot)
            || slots.iter().any(|(prior, _)| *prior == slot)
        {
            return Err(invalid("backup contains a duplicate or unavailable slot"));
        }
        let bytes = if data == "empty" {
            None
        } else {
            let bytes = decode_hex(data)?;
            if bytes.len() != Report::Macro.length()
                || !bytes.starts_with(&[Report::Macro.id(), Report::Macro.length() as u8, slot])
            {
                return Err(invalid(format!("invalid raw macro frame for slot {slot}")));
            }
            Some(bytes)
        };
        slots.push((slot, bytes));
    }
    if slots.is_empty() {
        return Err(invalid("macro backup has no slots"));
    }
    if driver.firmware()? != driver.firmware_identity(firmware) {
        return Err(invalid(
            "the archived macro firmware does not match this mouse",
        ));
    }
    for (slot, expected) in &slots {
        let actual = match driver.read_macro(*slot) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if actual != *expected {
            return Err(invalid(format!(
                "macro slot {slot} differs from the raw backup"
            )));
        }
    }
    Ok(slots.len())
}

fn optional_page(driver: &mut Driver, slot: u8) -> io::Result<Option<Vec<u8>>> {
    match driver
        .read_macro(slot)
        .and_then(|bytes| writable(bytes, slot))
    {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn upload(driver: &mut Driver, slot: u8, bytes: &[u8]) -> io::Result<()> {
    driver.upload_macro(bytes)?;
    if optional_page(driver, slot)?.as_deref() != Some(bytes) {
        return Err(invalid(format!(
            "macro slot {slot} differs from the archived page"
        )));
    }
    Ok(())
}

pub fn planned_slots(head: u8, value: &Macro) -> io::Result<std::ops::RangeInclusive<u8>> {
    slots_for_pages(head, actions::macro_page_events(value)?.len())
}

fn slots_for_pages(head: u8, count: usize) -> io::Result<std::ops::RangeInclusive<u8>> {
    let last = usize::from(head) + count - 1;
    if head == 0 || last > 18 {
        return Err(invalid(
            "macro needs more consecutive slots than remain after its head",
        ));
    }
    Ok(head..=last as u8)
}

pub fn plan(connection: Connection, head: u8, value: &Macro) -> io::Result<Vec<Page>> {
    let groups = actions::macro_page_events(value)?;
    let slots = slots_for_pages(head, groups.len())?;
    let last = *slots.end();
    let mut pages = Vec::with_capacity(groups.len());
    for (slot, events) in slots.zip(groups) {
        let page = Macro {
            mode: value.mode,
            repeat: value.repeat,
            metadata: value.metadata,
            events: events.to_vec(),
        };
        let next = (slot < last).then_some(if connection == Connection::Bluetooth {
            slot + 1
        } else {
            1
        });
        pages.push((
            slot,
            actions::encode_macro_continuation(slot, &page, next)?.to_vec(),
        ));
    }
    Ok(pages)
}

pub fn author(
    driver: &mut Driver,
    head: u8,
    value: &Macro,
    finish: impl FnOnce(&mut Driver) -> io::Result<()>,
) -> io::Result<usize> {
    let pages = plan(driver.connection(), head, value)?;
    restore_pages(driver, head, pages, finish)
}

pub fn restore(driver: &mut Driver, head: u8, source: &str) -> io::Result<usize> {
    let (firmware, pages) = parse(source, driver.connection(), head)?;
    if driver.firmware()? != driver.firmware_identity(firmware) {
        return Err(invalid(
            "the archived macro firmware does not match this mouse",
        ));
    }
    restore_pages(driver, head, pages, |_| Ok(()))
}

fn restore_pages(
    driver: &mut Driver,
    head: u8,
    pages: Vec<Page>,
    finish: impl FnOnce(&mut Driver) -> io::Result<()>,
) -> io::Result<usize> {
    let mut owned = Vec::new();
    if optional_page(driver, head)?.is_some() {
        owned = read(driver, head)?;
    }
    let buttons = operations::read_setting(driver, Report::Buttons)?;
    let mut checked_heads = Vec::new();
    for button in 1..=18 {
        if let Action::Macro { slot } = actions::button_action(&buttons, button)? {
            if slot == head || checked_heads.contains(&slot) {
                continue;
            }
            checked_heads.push(slot);
            if read(driver, slot)?
                .iter()
                .any(|(other, _)| pages.iter().any(|(slot, _)| slot == other))
            {
                return Err(invalid(format!(
                    "archive would overwrite a page used by macro slot {slot}"
                )));
            }
        }
    }
    let mut pending = Vec::new();
    for (slot, bytes) in pages.iter().rev() {
        let previous = optional_page(driver, *slot)?;
        if *slot != head
            && previous.as_ref().is_some_and(|page| page[28] != 0)
            && !owned.iter().any(|(owned, _)| owned == slot)
        {
            return Err(invalid(format!(
                "continuation slot {slot} is occupied by another macro"
            )));
        }
        if previous.as_ref() != Some(bytes) {
            pending.push((*slot, bytes, previous));
        }
    }
    let mut attempted = 0;
    let result = (|| {
        for (slot, bytes, _) in &pending {
            attempted += 1;
            upload(driver, *slot, bytes)?;
        }
        let actual = read(driver, head)?
            .into_iter()
            .map(|(slot, bytes)| writable(bytes, slot).map(|bytes| (slot, bytes)))
            .collect::<io::Result<Vec<_>>>()?;
        if actual != pages {
            return Err(invalid("assembled macro chain differs from the archive"));
        }
        finish(driver)?;
        Ok(pending.len())
    })();
    result.map_err(|error| {
        let mut incomplete = Vec::new();
        for (slot, _, previous) in pending[..attempted].iter().rev() {
            let restored = match previous {
                Some(bytes) => upload(driver, *slot, bytes).is_ok(),
                None => matches!(optional_page(driver, *slot), Ok(None)),
            };
            if !restored {
                incomplete.push(slot.to_string());
            }
        }
        let status = if incomplete.is_empty() {
            String::from("original pages verified")
        } else {
            format!(
                "could not restore or verify slots {}",
                incomplete.join(", ")
            )
        };
        io::Error::new(error.kind(), format!("{error} ({status})"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{profiles::hex, simulator::Simulator};

    #[test]
    fn restores_original_wired_and_receiver_pages_after_extended_event_test() {
        let original = actions::parse_macro("down b 70").unwrap();
        let mut original_head = actions::encode_macro(7, &original).unwrap();
        original_head[28] = 2;
        original_head[31..33].copy_from_slice(&[1, 0xfe]);
        original_head[129..].fill(0);
        let original_tail =
            actions::encode_macro(8, &actions::parse_macro("up b 90").unwrap()).unwrap();
        let baseline = format!(
            "firmware 1001\n7 {}\n8 {}\n",
            hex(&original_head),
            hex(&original_tail)
        );

        let extended = actions::parse_macro("extended 200 12fe34fe").unwrap();
        let mut first = actions::encode_macro(7, &extended).unwrap();
        assert_eq!(actions::macro_continuation(&first).unwrap(), None);
        assert_eq!(actions::decode_macro(&first, 7).unwrap(), extended);
        first[28] = 4;
        first[35..37].copy_from_slice(&[1, 0xfe]);
        first[129..].fill(0);
        let last = actions::encode_macro(8, &actions::parse_macro("down a 50").unwrap()).unwrap();
        let archive = format!("firmware 1001\n7 {}\n8 {}\n", hex(&first), hex(&last));
        for connection in [Connection::Wired, Connection::Receiver] {
            let mut driver = Driver::new(Box::new(Simulator::new(connection)));
            assert_eq!(restore(&mut driver, 7, &baseline).unwrap(), 2);
            let original_pages = read(&mut driver, 7).unwrap();

            assert_eq!(restore(&mut driver, 7, &archive).unwrap(), 2);
            assert_eq!(restore(&mut driver, 7, &archive).unwrap(), 0);
            assert_eq!(
                read(&mut driver, 7).unwrap(),
                parse(&archive, connection, 7).unwrap().1
            );
            assert!(restore(&mut driver, 7, &archive.replace("1001", "1002")).is_err());

            assert_eq!(restore(&mut driver, 7, &baseline).unwrap(), 2);
            assert_eq!(read(&mut driver, 7).unwrap(), original_pages);
        }
        assert!(parse(&archive, Connection::Bluetooth, 7).is_err());
    }

    #[test]
    fn authors_a_chain_without_splitting_an_extended_event() {
        let mut source = "down a 10\n".repeat(48);
        source.push_str("extended 200 0001fffe\nup a 10");
        let value = actions::parse_macro(&source).unwrap();
        for connection in [
            Connection::Wired,
            Connection::Receiver,
            Connection::Bluetooth,
        ] {
            let mut driver = Driver::new(Box::new(Simulator::new(connection)));
            assert_eq!(author(&mut driver, 7, &value, |_| Ok(())).unwrap(), 2);
            let pages = read(&mut driver, 7).unwrap();
            assert_eq!(pages.iter().map(|page| page.0).collect::<Vec<_>>(), [7, 8]);
            let frames: Vec<_> = pages.iter().map(|page| page.1.as_slice()).collect();
            assert_eq!(actions::decode_macro_pages(&frames, 7).unwrap(), value);
            assert!(author(&mut driver, 18, &value, |_| Ok(())).is_err());
        }
    }

    #[test]
    fn windows_extended_projection_round_trips_as_local_data() {
        let value = actions::parse_macro("extended 240 unavailable").unwrap();
        let exported = actions::export_vendor_macro("projection", &value).unwrap();
        assert_eq!(&exported[56..60], &[0xf6, 0, 240, 0]);
        assert_eq!(
            actions::import_vendor_macros(&exported).unwrap()[0].1,
            value
        );
        assert!(actions::encode_macro(1, &value).is_err());
    }
}
