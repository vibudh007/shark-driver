//! Wired model transactions with traced readback.

use crate::{
    driver::Transport,
    settings::{Dpi, DpiLayout, Legacy, Polling, Timing},
};
use std::{io, thread::sleep, time::Duration};

use crate::driver::Connection;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WiredPolling {
    Versioned(u8),
    Direct,
    Combined,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Request {
    id: u8,
    len: u8,
    selector: u8,
}

const RECEIPT: Request = Request {
    id: 0xa0,
    len: 8,
    selector: 0,
};
const VERSION: Request = Request {
    id: 0x0b,
    len: 8,
    selector: 0,
};
const PROFILE_RANGE: Request = Request {
    id: 0x0c,
    len: 10,
    selector: 0,
};
const POLLING: Request = Request {
    id: 0x06,
    len: 9,
    selector: 1,
};
const DPI: Request = Request {
    id: 0x04,
    len: 56,
    selector: 1,
};
const BUTTONS: Request = Request {
    id: 0x08,
    len: 59,
    selector: 1,
};
const TIMING: Request = Request {
    id: 0x05,
    len: 15,
    selector: 1,
};
const COMBINED: Request = Request {
    id: 0x0a,
    len: 128,
    selector: 1,
};

impl Request {
    fn declared(self, width: &impl Fn(u8) -> Option<usize>) -> bool {
        width(self.id) == Some(usize::from(self.len))
    }

    fn read(
        self,
        transport: &mut dyn Transport,
        width: &impl Fn(u8) -> Option<usize>,
        delay: &impl Fn(Duration),
    ) -> io::Result<Vec<u8>> {
        if !self.declared(width) {
            return Err(unsupported(
                "HID descriptor does not declare the requested report width",
            ));
        }
        let settle = Duration::from_millis(100);
        delay(settle);
        transport.set_feature(&[RECEIPT.id, self.id, self.len, 0, self.selector, 0, 0, 0])?;
        delay(if transport.connection() == Connection::Receiver {
            Duration::from_millis(500)
        } else {
            settle
        });
        let receipt = if transport.connection() == Connection::Bluetooth {
            let mut reply = None;
            for _ in 0..20 {
                match transport.get_feature(RECEIPT.id, usize::from(RECEIPT.len)) {
                    Ok(bytes) => {
                        reply = Some(bytes);
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        delay(Duration::from_millis(75))
                    }
                    Err(error) => return Err(error),
                }
            }
            reply.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Bluetooth settings reply did not arrive",
                )
            })?
        } else {
            transport.get_feature(RECEIPT.id, usize::from(RECEIPT.len))?
        };
        if receipt.len() != usize::from(RECEIPT.len) || receipt[..2] != [RECEIPT.id, 1] {
            return Err(invalid("mouse did not acknowledge the settings request"));
        }
        delay(settle);
        let reply = transport.get_feature(self.id, usize::from(self.len))?;
        if reply.len() != usize::from(self.len) || reply[0] != self.id {
            return Err(invalid(
                "unexpected report ID or width in settings readback",
            ));
        }
        Ok(reply)
    }

    fn polling_rate(self, reply: &[u8]) -> io::Result<u16> {
        if reply[1] != self.len {
            return Err(invalid("polling readback has an invalid length byte"));
        }
        match reply[3] {
            code @ (1 | 2 | 4 | 8) => Ok(1000 / u16::from(code)),
            _ => Err(invalid("unknown polling rate in readback")),
        }
    }
}

/// These widths express the logical frames translated by the BlueZ adapter.
pub fn bluetooth_width(id: u8) -> Option<usize> {
    [
        RECEIPT,
        VERSION,
        COMBINED,
        DPI,
        TIMING,
        BUTTONS,
        POLLING,
        macro_request(0),
    ]
    .iter()
    .find(|report| report.id == id)
    .map(|report| usize::from(report.len))
}

fn bluetooth_version(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: &impl Fn(u8) -> Option<usize>,
    delay: &impl Fn(Duration),
) -> io::Result<[u8; 2]> {
    if !crate::bluetooth::has_settings_callback(model) {
        return Err(unsupported("no traced Bluetooth settings callback"));
    }
    let reply = VERSION.read(transport, width, delay)?;
    if reply[2] != model.receiver_tag {
        return Err(invalid("Bluetooth response belongs to a different model"));
    }
    Ok([reply[2], reply[3]])
}

fn direct_feature(
    transport: &mut dyn Transport,
    report: Request,
    checksum_at: Option<usize>,
) -> io::Result<Vec<u8>> {
    let bytes = transport.get_feature(report.id, usize::from(report.len))?;
    if bytes.len() != usize::from(report.len) || bytes[..2] != [report.id, report.len] {
        return Err(invalid(
            "direct feature read did not return the expected report",
        ));
    }
    if let Some(at) = checksum_at {
        let sum = bytes[3..at].iter().copied().map(u16::from).sum::<u16>();
        if bytes[at..at + 2] != sum.to_be_bytes() {
            return Err(invalid("direct feature checksum did not verify"));
        }
    }
    Ok(bytes)
}

impl WiredPolling {
    pub fn can_read(self, width: impl Fn(u8) -> Option<usize>) -> bool {
        RECEIPT.declared(&width)
            && match self {
                Self::Versioned(_) => {
                    VERSION.declared(&width)
                        && (POLLING.declared(&width) || COMBINED.declared(&width))
                }
                Self::Direct => POLLING.declared(&width),
                Self::Combined => {
                    PROFILE_RANGE.declared(&width)
                        && VERSION.declared(&width)
                        && COMBINED.declared(&width)
                }
            }
    }

    pub fn can_write(self, width: impl Fn(u8) -> Option<usize>) -> bool {
        POLLING.declared(&width) && self.can_read(width)
    }

    pub fn update(
        self,
        transport: &mut dyn Transport,
        width: impl Fn(u8) -> Option<usize>,
        desired: Option<u16>,
    ) -> io::Result<u16> {
        self.transact(transport, width, desired, sleep)
    }

    fn transact(
        self,
        transport: &mut dyn Transport,
        width: impl Fn(u8) -> Option<usize>,
        desired: Option<u16>,
        delay: impl Fn(Duration),
    ) -> io::Result<u16> {
        if !(transport.connection() == Connection::Wired
            || transport.connection() == Connection::Receiver && self == Self::Combined)
            || !self.can_read(&width)
        {
            return Err(unsupported(
                "no verified wired polling layout on this endpoint",
            ));
        }
        let outgoing = if let Some(rate) = desired {
            if !self.can_write(&width) {
                return Err(unsupported(
                    "HID descriptor does not declare the polling write report",
                ));
            }
            let mut frame = Polling::default();
            frame.set_hz(rate)?;
            Some(frame.write_bytes())
        } else {
            None
        };
        let report = self.select_readback(transport, &width, &delay)?;
        let read = |transport: &mut dyn Transport| {
            let reply = report.read(transport, &width, &delay)?;
            report.polling_rate(&reply)
        };
        let original = read(transport)?;
        if let Some((desired, outgoing)) =
            desired.zip(outgoing).filter(|(rate, _)| *rate != original)
        {
            transport.set_feature(&outgoing)?;
            delay(Duration::from_millis(200));
            let verified = read(transport).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("polling write sent, but readback failed: {error}"),
                )
            })?;
            if verified != desired {
                return Err(io::Error::other(format!(
                    "polling write did not verify: requested {desired} Hz, read {verified} Hz"
                )));
            }
            return Ok(verified);
        }
        Ok(original)
    }

    fn select_readback(
        self,
        transport: &mut dyn Transport,
        width: &impl Fn(u8) -> Option<usize>,
        delay: &impl Fn(Duration),
    ) -> io::Result<Request> {
        if self == Self::Direct {
            return Ok(POLLING);
        }
        if self == Self::Combined {
            PROFILE_RANGE.read(transport, width, delay)?;
        }
        let version = VERSION.read(transport, width, delay)?;
        if matches!(&version[2..4], [0, 0] | [0xff, 0xff]) {
            return Err(invalid("mouse returned an invalid firmware identity"));
        }
        Ok(
            if matches!(self, Self::Versioned(tag) if tag == version[2]) {
                POLLING
            } else {
                COMBINED
            },
        )
    }
}

/// DPI layout and firmware branch are selected only for models with a traced sender and reader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WiredDpi {
    route: Option<WiredPolling>,
    layout: DpiLayout,
}

impl WiredDpi {
    pub fn for_slot(slot: u8) -> Option<Self> {
        let layout = match slot {
            1 | 2 | 8 => DpiLayout::RawLinear,
            5 => DpiLayout::MaxLinear,
            7 | 9 => DpiLayout::Lookup { max: 24_000 },
            4 => DpiLayout::Lookup { max: 22_000 },
            10 => DpiLayout::Lookup { max: 24_000 },
            6 | 12 | 13 => DpiLayout::Linear,
            11 | 14..=18 => DpiLayout::ExtendedLookup,
            _ => return None,
        };
        let route = if slot == 13 {
            Some(WiredPolling::Direct)
        } else {
            crate::models::Model::vendor_slot(slot)?.wired_polling()
        };
        Some(Self { route, layout })
    }

    pub fn can_read(self, width: impl Fn(u8) -> Option<usize>) -> bool {
        let Some(route) = self.route else {
            return DPI.declared(&width);
        };
        RECEIPT.declared(&width)
            && match route {
                WiredPolling::Direct => DPI.declared(&width),
                WiredPolling::Versioned(_) => {
                    VERSION.declared(&width) && (DPI.declared(&width) || COMBINED.declared(&width))
                }
                WiredPolling::Combined => route.can_read(width),
            }
    }

    pub fn can_write(self, width: impl Fn(u8) -> Option<usize>) -> bool {
        DPI.declared(&width) && self.can_read(width)
    }

    pub fn update(
        self,
        transport: &mut dyn Transport,
        width: impl Fn(u8) -> Option<usize>,
        edit: Option<impl FnOnce(&mut Dpi) -> io::Result<()>>,
    ) -> io::Result<Dpi> {
        self.transact(transport, width, edit, sleep)
    }

    fn transact(
        self,
        transport: &mut dyn Transport,
        width: impl Fn(u8) -> Option<usize>,
        edit: Option<impl FnOnce(&mut Dpi) -> io::Result<()>>,
        delay: impl Fn(Duration),
    ) -> io::Result<Dpi> {
        if !(transport.connection() == Connection::Wired
            || transport.connection() == Connection::Receiver
                && self.route == Some(WiredPolling::Combined))
            || !self.can_read(&width)
        {
            return Err(unsupported("no audited wired DPI collection"));
        }
        if edit.is_some() && !DPI.declared(&width) {
            return Err(unsupported(
                "the mouse does not declare the DPI write report",
            ));
        }
        let combined = match self.route {
            Some(route) => route.select_readback(transport, &width, &delay)? == COMBINED,
            None => false,
        };
        let read = |transport: &mut dyn Transport| {
            let bytes = if combined {
                read_combined(transport, &width, &delay)?
                    .dpi()
                    .bytes()
                    .to_vec()
            } else if self.route.is_some() {
                DPI.read(transport, &width, &delay)?
            } else {
                direct_feature(transport, DPI, Some(50))?
            };
            let dpi = Dpi::parse_model(&bytes, self.layout)?;
            if !dpi.stage(dpi.active_stage())?.enabled {
                return Err(invalid("DPI readback identifies a disabled active stage"));
            }
            Ok(dpi)
        };
        let original = read(transport)?;
        let Some(edit) = edit else {
            return Ok(original);
        };
        let mut desired = original.clone();
        edit(&mut desired)?;
        desired.set_active_stage(desired.active_stage())?;
        if desired.bytes()[3..49] == original.bytes()[3..49] {
            return Ok(original);
        }
        write_verified(
            transport,
            &original.write_bytes(),
            &desired.write_bytes(),
            |transport, packet| Ok(read(transport)?.bytes()[3..49] == packet[3..49]),
            &delay,
        )?;
        Ok(desired)
    }
}

pub fn read_combined_profile(
    route: WiredPolling,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
) -> io::Result<Legacy> {
    combined_profile(route, transport, &width, &sleep)
}

fn combined_profile(
    route: WiredPolling,
    transport: &mut dyn Transport,
    width: &impl Fn(u8) -> Option<usize>,
    delay: &impl Fn(Duration),
) -> io::Result<Legacy> {
    if !(transport.connection() == Connection::Wired
        || transport.connection() == Connection::Receiver && route == WiredPolling::Combined)
        || route == WiredPolling::Direct
        || !route.can_read(width)
        || !COMBINED.declared(width)
    {
        return Err(unsupported("no audited combined profile collection"));
    }
    if route.select_readback(transport, width, delay)? != COMBINED {
        return Err(unsupported("this firmware uses separate settings reports"));
    }
    read_combined(transport, width, delay)
}

fn read_combined(
    transport: &mut dyn Transport,
    width: &impl Fn(u8) -> Option<usize>,
    delay: &impl Fn(Duration),
) -> io::Result<Legacy> {
    let bytes = COMBINED.read(transport, width, delay)?;
    COMBINED.polling_rate(&bytes)?;
    Legacy::parse(&bytes)
}

const ARCHIVE_HEADER: &[u8; 4] = b"SDP1";
const ARCHIVE_LEN: usize = 7 + crate::settings::LEGACY_LEN;

fn v3_session(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: &impl Fn(u8) -> Option<usize>,
    delay: &impl Fn(Duration),
) -> io::Result<[u8; 2]> {
    if (16..=18).contains(&model.slot) && transport.connection() == Connection::Bluetooth {
        return bluetooth_version(model, transport, width, delay);
    }
    if !(16..=18).contains(&model.slot)
        || !matches!(
            transport.connection(),
            Connection::Wired | Connection::Receiver
        )
        || !WiredPolling::Combined.can_read(width)
    {
        return Err(unsupported("no audited V3-family wired profile collection"));
    }
    PROFILE_RANGE.read(transport, width, delay)?;
    let version = VERSION.read(transport, width, delay)?;
    if matches!(&version[2..4], [0, 0] | [0xff, 0xff])
        || transport.connection() == Connection::Receiver && version[2] != model.receiver_tag
    {
        return Err(invalid("mouse returned an invalid firmware identity"));
    }
    Ok([version[2], version[3]])
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ProfileRead {
    Direct,
    Split,
    Combined { buttons: bool },
}

impl ProfileRead {
    fn open(
        model: &crate::models::Model,
        transport: &mut dyn Transport,
        width: &impl Fn(u8) -> Option<usize>,
        delay: &impl Fn(Duration),
    ) -> io::Result<(Self, [u8; 2])> {
        if transport.connection() == Connection::Bluetooth && (16..=18).contains(&model.slot) {
            return bluetooth_version(model, transport, width, delay)
                .map(|firmware| (Self::Combined { buttons: true }, firmware));
        }
        if !(transport.connection() == Connection::Wired
            || transport.connection() == Connection::Receiver && (16..=18).contains(&model.slot))
            || model.slot == 3
        {
            return Err(unsupported("expected an identified wired model"));
        }
        let (route, version) = match model.wired_polling() {
            Some(WiredPolling::Combined) => (
                Self::Combined { buttons: true },
                v3_session(model, transport, width, delay)?,
            ),
            Some(WiredPolling::Versioned(tag)) => {
                let version = VERSION.read(transport, width, delay)?;
                let route = if version[2] == tag {
                    Self::Split
                } else {
                    Self::Combined { buttons: false }
                };
                (route, [version[2], version[3]])
            }
            Some(WiredPolling::Direct) => {
                if !VERSION.declared(width) {
                    return Err(unsupported("the device offers no firmware report"));
                }
                let version = direct_feature(transport, VERSION, None)?;
                (Self::Split, [version[2], version[3]])
            }
            None => {
                if !VERSION.declared(width) {
                    return Err(unsupported("the device offers no firmware report"));
                }
                let version = direct_feature(transport, VERSION, None)?;
                (Self::Direct, [version[2], version[3]])
            }
        };
        if matches!(version, [0, 0] | [255, 255]) {
            return Err(invalid("mouse returned an invalid firmware identity"));
        }
        Ok((route, version))
    }

    fn capture(
        self,
        model: &crate::models::Model,
        transport: &mut dyn Transport,
        width: &impl Fn(u8) -> Option<usize>,
        delay: &impl Fn(Duration),
    ) -> io::Result<Legacy> {
        if !matches!(self, Self::Combined { buttons: true }) && !BUTTONS.declared(width) {
            return Err(unsupported(
                "the device offers no independent button readback",
            ));
        }
        let mut profile = [0; crate::settings::LEGACY_LEN];
        if matches!(self, Self::Combined { .. }) {
            profile.copy_from_slice(read_combined(transport, width, delay)?.bytes());
        } else {
            let mut get = |report: Request, checksum_at| {
                if !report.declared(width) {
                    return Err(unsupported(
                        "a required settings report is absent from the descriptor",
                    ));
                }
                if self == Self::Direct {
                    direct_feature(transport, report, checksum_at)
                } else {
                    report.read(transport, width, delay)
                }
            };
            let dpi = get(DPI, Some(50))?;
            let timing = get(TIMING, Some(11))?;
            let polling = get(POLLING, None)?;
            if dpi[1] != DPI.len
                || timing[1] != TIMING.len
                || polling[1] != POLLING.len
                || polling[4] != !polling[3]
                || !valid_rate_code(model.slot, polling[3])
            {
                return Err(invalid(
                    "split settings reply has invalid framing or polling code",
                ));
            }
            profile[..4].copy_from_slice(&[10, 128, 1, polling[3]]);
            profile[5..54].copy_from_slice(&dpi[3..52]);
            profile[55..65].copy_from_slice(&timing[3..13]);
        }
        if !matches!(self, Self::Combined { buttons: true }) {
            let buttons = direct_feature(transport, BUTTONS, Some(57))?;
            profile[65..119].copy_from_slice(&buttons[3..57]);
        }
        Legacy::parse(&profile)
    }
}

pub(crate) fn valid_rate_code(slot: u8, rate: u8) -> bool {
    match slot {
        7 | 13 => (1..=7).contains(&(rate & 15)) && (1..=7).contains(&(rate >> 4)),
        8 => matches!(rate, 1 | 2 | 4 | 8 | 16 | 32 | 64),
        _ => matches!(rate, 1 | 2 | 4 | 8),
    }
}

fn polling_frame(code: u8) -> Vec<u8> {
    vec![6, 9, 1, code, !code, 0, 0, 0, 0]
}

pub fn archive_model_profile(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
) -> io::Result<[u8; ARCHIVE_LEN]> {
    archive_profile(model, transport, width, sleep)
}

fn archive_profile(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    delay: impl Fn(Duration),
) -> io::Result<[u8; ARCHIVE_LEN]> {
    let (route, firmware) = ProfileRead::open(model, transport, &width, &delay)?;
    let profile = route.capture(model, transport, &width, &delay)?;
    let mut archive = [0; ARCHIVE_LEN];
    archive[..4].copy_from_slice(ARCHIVE_HEADER);
    archive[4] = model.slot;
    archive[5..7].copy_from_slice(&firmware);
    archive[7..].copy_from_slice(profile.bytes());
    Ok(archive)
}

pub fn restore_model_profile(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    archive: &[u8],
) -> io::Result<usize> {
    restore_profile(model, transport, width, archive, sleep)
}

fn restore_profile(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    archive: &[u8],
    delay: impl Fn(Duration),
) -> io::Result<usize> {
    restore_profile_inner(model, transport, width, archive, false, delay)
}

fn restore_profile_inner(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    archive: &[u8],
    full_reset: bool,
    delay: impl Fn(Duration),
) -> io::Result<usize> {
    if archive.len() != ARCHIVE_LEN || &archive[..4] != ARCHIVE_HEADER || archive[4] != model.slot {
        return Err(invalid("profile backup does not match this model"));
    }
    let requests = [DPI, TIMING, POLLING, BUTTONS];
    if requests.iter().any(|report| !report.declared(&width)) {
        return Err(unsupported(
            "the wired collection lacks a profile write report",
        ));
    }
    let desired = Legacy::parse(&archive[7..])?;
    if desired.bytes()[1] != COMBINED.len || !valid_rate_code(model.slot, desired.bytes()[3]) {
        return Err(invalid("invalid polling code in profile backup"));
    }
    let layout = WiredDpi::for_slot(model.slot)
        .ok_or_else(|| unsupported("unknown model DPI layout"))?
        .layout;
    let dpi = Dpi::parse_model(desired.dpi().bytes(), layout)?;
    let active = dpi.stage(dpi.active_stage())?;
    if !active.enabled || active.dpi.is_none() {
        return Err(invalid("invalid active DPI stage in profile backup"));
    }
    let names = crate::actions::button_names(model);
    crate::actions::validate_named_assignments(&desired.buttons(), &names)?;
    let (route, firmware) = ProfileRead::open(model, transport, &width, &delay)?;
    if firmware != archive[5..7] {
        return Err(invalid("profile backup does not match this firmware"));
    }
    let original = route.capture(model, transport, &width, &delay)?;
    for (before, after) in original
        .button_payload()
        .chunks_exact(3)
        .zip(desired.button_payload().chunks_exact(3))
    {
        if before != after && (before[0] == 0x12 || after[0] == 0x12) {
            return Err(unsupported("restore requires unchanged macro bindings"));
        }
    }
    crate::actions::validate_named_assignments(&original.buttons(), &names)?;
    let frames = |profile: &Legacy| {
        [
            profile.dpi().write_bytes().to_vec(),
            profile.timing().write_bytes().to_vec(),
            polling_frame(profile.bytes()[3]),
            profile.buttons().to_vec(),
        ]
    };
    let before = frames(&original);
    let after = frames(&desired);
    if transport.connection() == Connection::Bluetooth && before[2] != after[2] {
        return Err(unsupported(
            "Bluetooth profile restore requires the same polling setting",
        ));
    }
    if full_reset {
        if transport.connection() != Connection::Wired {
            return Err(unsupported("full model reset requires wired USB"));
        }
        let check_range = |transport: &mut dyn Transport| {
            if !PROFILE_RANGE.declared(&width) || !RECEIPT.declared(&width) {
                return Err(unsupported("no ten-byte profile metadata readback"));
            }
            let info = PROFILE_RANGE.read(transport, &width, &delay)?;
            if info[1..6] != [10, 1, 0xfe, 1, 0xfe] {
                return Err(unsupported(
                    "full reset requires a verified single-profile device",
                ));
            }
            Ok(())
        };
        check_range(transport)?;
        transport.set_feature(&crate::driver::RESET_PREAMBLE)?;
        delay(Duration::from_millis(200));
        check_range(transport)?;
    }
    let changed: Vec<usize> = (0..requests.len())
        .filter(|&i| full_reset || before[i] != after[i])
        .collect();
    let write = |transport: &mut dyn Transport, i: usize, bytes: &[u8]| {
        transport.set_feature(bytes)?;
        delay(Duration::from_millis(200));
        let readback = frames(&route.capture(model, transport, &width, &delay)?);
        if readback[i] != bytes {
            return Err(invalid("profile write readback differs"));
        }
        Ok(readback)
    };
    for (index, &report) in changed.iter().enumerate() {
        let verified = write(transport, report, &after[report]).and_then(|current| {
            if index + 1 == changed.len() && current != after {
                Err(invalid("final profile readback differs"))
            } else {
                Ok(())
            }
        });
        if let Err(error) = verified {
            for &prior in changed[..=index].iter().rev() {
                let _ = write(transport, prior, &before[prior]);
            }
            let recovery = route
                .capture(model, transport, &width, &delay)
                .map(|profile| frames(&profile));
            return Err(io::Error::new(
                error.kind(),
                if recovery.is_ok_and(|profile| profile == before) {
                    format!("{error}; original settings verified")
                } else {
                    format!("{error}; original profile restoration could not be verified")
                },
            ));
        }
    }
    Ok(changed.len())
}

pub(crate) fn decode_model_dpi(model: &crate::models::Model, bytes: &[u8]) -> io::Result<Dpi> {
    let layout = WiredDpi::for_slot(model.slot)
        .ok_or_else(|| unsupported("unknown model DPI layout"))?
        .layout;
    Dpi::parse_model(bytes, layout)
}

pub fn model_dpi(model: &crate::models::Model, profile: &Legacy) -> io::Result<Dpi> {
    if model.slot == 3 {
        return Ok(profile.dpi());
    }
    decode_model_dpi(model, profile.dpi().bytes())
}

pub fn update_bluetooth_dpi(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    edit: Option<impl FnOnce(&mut Dpi) -> io::Result<()>>,
) -> io::Result<Dpi> {
    bluetooth_dpi(model, transport, edit, sleep)
}

fn bluetooth_dpi(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    edit: Option<impl FnOnce(&mut Dpi) -> io::Result<()>>,
    delay: impl Fn(Duration),
) -> io::Result<Dpi> {
    bluetooth_edit(
        model,
        transport,
        edit,
        |profile| model_dpi(model, profile),
        Dpi::write_bytes,
        |dpi| dpi.set_active_stage(dpi.active_stage()),
        delay,
    )
}

pub fn update_bluetooth_timing(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    edit: Option<impl FnOnce(&mut Timing) -> io::Result<()>>,
) -> io::Result<Timing> {
    bluetooth_edit(
        model,
        transport,
        edit,
        |profile| Ok(profile.timing()),
        Timing::write_bytes,
        |_| Ok(()),
        sleep,
    )
}

fn bluetooth_edit<Value: Clone, const N: usize>(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    edit: Option<impl FnOnce(&mut Value) -> io::Result<()>>,
    extract: impl Fn(&Legacy) -> io::Result<Value>,
    encode: impl Fn(&Value) -> [u8; N],
    validate: impl FnOnce(&mut Value) -> io::Result<()>,
    delay: impl Fn(Duration),
) -> io::Result<Value> {
    if transport.connection() != Connection::Bluetooth {
        return Err(unsupported("expected a Bluetooth settings session"));
    }
    if model.slot != 13 {
        bluetooth_version(model, transport, &bluetooth_width, &delay)?;
    }
    let read = |transport: &mut dyn Transport| {
        let profile = if model.slot == 13 {
            let bytes = COMBINED.read(transport, &bluetooth_width, &delay)?;
            if bytes[1] != COMBINED.len || !valid_rate_code(model.slot, bytes[3]) {
                return Err(invalid("invalid X8 Pro combined settings reply"));
            }
            Legacy::parse(&bytes)?
        } else {
            read_combined(transport, &bluetooth_width, &delay)?
        };
        extract(&profile)
    };
    let original = read(transport)?;
    let Some(edit) = edit else {
        return Ok(original);
    };
    let mut desired = original.clone();
    edit(&mut desired)?;
    validate(&mut desired)?;
    let before = encode(&original);
    let after = encode(&desired);
    if before == after {
        return Ok(original);
    }
    let write = |transport: &mut dyn Transport, bytes: &[u8; N]| {
        transport.set_feature(bytes)?;
        delay(Duration::from_millis(500));
        if encode(&read(transport)?) != *bytes {
            return Err(invalid("Bluetooth settings write did not verify"));
        }
        Ok(())
    };
    if let Err(error) = write(transport, &after) {
        let recovery = if read(transport).is_ok_and(|value| encode(&value) == before)
            || write(transport, &before).is_ok()
        {
            "original setting verified"
        } else {
            "original setting could not be verified"
        };
        return Err(io::Error::new(error.kind(), format!("{error}; {recovery}")));
    }
    Ok(desired)
}

pub fn apply_model_windows(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    plan: &crate::vendor_plan::Plan,
) -> io::Result<usize> {
    if !plan.macros.is_empty() {
        return Err(unsupported(
            "Windows profiles with inline macros need a separate slot restore",
        ));
    }
    apply_model_frames(
        model,
        transport,
        width,
        [
            plan.dpi.as_slice(),
            plan.timing.as_slice(),
            plan.polling.as_slice(),
            plan.buttons.as_slice(),
        ],
        false,
        sleep,
    )
}

pub fn reset_model_settings(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
) -> io::Result<usize> {
    let frames = factory_frames(model.slot)?;
    apply_model_frames(model, transport, width, frames, true, sleep)
}

pub fn reset_model_full(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
) -> io::Result<usize> {
    reset_model_full_with_delay(model, transport, width, sleep)
}

fn reset_model_full_with_delay(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    delay: impl Fn(Duration),
) -> io::Result<usize> {
    if transport.connection() != Connection::Wired || model.slot == 3 {
        return Err(unsupported(
            "full model reset requires a non-R1 wired mouse",
        ));
    }
    let [dpi, timing, polling, buttons] = factory_frames(model.slot)?;
    let mut archive = archive_profile(model, transport, &width, &delay)?;
    archive[12..61].copy_from_slice(&dpi[3..52]);
    archive[62..72].copy_from_slice(&timing[3..13]);
    archive[10] = polling[3];
    archive[72..126].copy_from_slice(&buttons[3..57]);
    restore_profile_inner(model, transport, width, &archive, true, delay)
}

fn factory_frames(slot: u8) -> io::Result<[Vec<u8>; 4]> {
    let hex = include_str!("model_defaults.hex")
        .lines()
        .nth(usize::from(slot.saturating_sub(1)))
        .filter(|_| slot != 0)
        .ok_or_else(|| unsupported("no traced Windows defaults for this model"))?;
    let bytes = crate::profiles::decode_hex(hex)?;
    if bytes.len() != 139 {
        return Err(invalid("default report set has an invalid size"));
    }
    Ok([
        bytes[..56].to_vec(),
        bytes[56..71].to_vec(),
        bytes[71..80].to_vec(),
        bytes[80..].to_vec(),
    ])
}

fn apply_model_frames<B: AsRef<[u8]>>(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    [dpi, timing, polling, buttons]: [B; 4],
    keep_macros: bool,
    delay: impl Fn(Duration),
) -> io::Result<usize> {
    let mut archive = archive_profile(model, transport, &width, &delay)?;
    archive[7 + 5..7 + 54].copy_from_slice(&dpi.as_ref()[3..52]);
    archive[7 + 55..7 + 65].copy_from_slice(&timing.as_ref()[3..13]);
    if transport.connection() != Connection::Bluetooth {
        archive[7 + 3] = polling.as_ref()[3];
    }
    for (dest, value) in archive[7 + 65..7 + 119]
        .chunks_exact_mut(3)
        .zip(buttons.as_ref()[3..57].chunks_exact(3))
    {
        if !keep_macros || dest[0] != 0x12 {
            dest.copy_from_slice(value);
        }
    }
    restore_profile(model, transport, &width, &archive, delay)
}

pub fn update_model_button(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    edit: Option<(usize, crate::actions::Action)>,
) -> io::Result<[u8; crate::actions::BUTTONS_LEN]> {
    model_button(model, transport, width, edit, sleep)
}

fn model_button(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    edit: Option<(usize, crate::actions::Action)>,
    delay: impl Fn(Duration),
) -> io::Result<[u8; crate::actions::BUTTONS_LEN]> {
    use crate::actions::{
        Action, button_names, set_named_button_action, validate_named_assignments,
    };
    if (16..=18).contains(&model.slot) {
        if transport.connection() == Connection::Bluetooth {
            bluetooth_version(model, transport, &width, &delay)?;
        } else if transport.connection() == Connection::Receiver {
            v3_session(model, transport, &width, &delay)?;
        }
        return v3_family_button(transport, width, edit, delay);
    }
    if model.slot == 3 || transport.connection() != Connection::Wired || !BUTTONS.declared(&width) {
        return Err(unsupported(
            "no independent button feature on this endpoint",
        ));
    }
    let original: [u8; crate::actions::BUTTONS_LEN] = direct_feature(transport, BUTTONS, Some(57))?
        .try_into()
        .map_err(|_| invalid("invalid independent button reply"))?;
    let Some((button, action)) = edit else {
        return Ok(original);
    };
    if matches!(action, Action::Macro { .. } | Action::Raw([0x12, ..]))
        || crate::actions::button_action(&original, button)
            .is_ok_and(|a| matches!(a, Action::Macro { .. } | Action::Raw([0x12, ..])))
    {
        return Err(unsupported(
            "editing this macro assignment requires a slot backup",
        ));
    }
    let names = button_names(model);
    validate_named_assignments(&original, &names)?;
    let mut desired = original;
    set_named_button_action(&mut desired, button, action, &names)?;
    if desired == original {
        return Ok(original);
    }
    let write = |transport: &mut dyn Transport, frame: &[u8]| {
        transport.set_feature(frame)?;
        delay(Duration::from_millis(200));
        let after = direct_feature(transport, BUTTONS, Some(57))?;
        if after[3..57] != frame[3..57] {
            return Err(invalid("button write did not verify"));
        }
        Ok(())
    };
    if let Err(error) = write(transport, &desired) {
        let (kind, detail) = (error.kind(), error.to_string());
        let unchanged = direct_feature(transport, BUTTONS, Some(57))
            .is_ok_and(|current| current[3..57] == original[3..57]);
        let recovery = if unchanged || write(transport, &original).is_ok() {
            "original button map verified"
        } else {
            "original button map restoration could not be verified"
        };
        return Err(io::Error::new(kind, format!("{detail}; {recovery}")));
    }
    Ok(desired)
}

pub fn update_v3_family_button(
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    edit: Option<(usize, crate::actions::Action)>,
) -> io::Result<[u8; crate::actions::BUTTONS_LEN]> {
    v3_family_button(transport, width, edit, sleep)
}

fn v3_family_button(
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    edit: Option<(usize, crate::actions::Action)>,
    delay: impl Fn(Duration),
) -> io::Result<[u8; crate::actions::BUTTONS_LEN]> {
    use crate::actions::{
        Action, V3_BUTTON_NAMES, set_named_button_action, validate_named_assignments,
    };
    if let Some((_, action)) = edit {
        if !BUTTONS.declared(&width) {
            return Err(unsupported(
                "the mouse does not declare the button write report",
            ));
        }
        if matches!(action, Action::Macro { .. } | Action::Raw([0x12, ..])) {
            return Err(unsupported(
                "macro assignments require model-specific slot validation",
            ));
        }
    }
    let original = if transport.connection() == Connection::Bluetooth {
        read_combined(transport, &width, &delay)?
    } else {
        combined_profile(WiredPolling::Combined, transport, &width, &delay)?
    }
    .buttons();
    let Some((button, action)) = edit else {
        return Ok(original);
    };
    if crate::actions::button_action(&original, button)
        .is_ok_and(|a| matches!(a, Action::Macro { .. } | Action::Raw([0x12, ..])))
    {
        return Err(unsupported(
            "editing this macro assignment requires a slot backup",
        ));
    }
    validate_named_assignments(&original, &V3_BUTTON_NAMES)?;
    let mut desired = original;
    set_named_button_action(&mut desired, button, action, &V3_BUTTON_NAMES)?;
    write_verified(
        transport,
        &original,
        &desired,
        |transport, packet| Ok(read_combined(transport, &width, &delay)?.buttons() == packet),
        &delay,
    )?;
    Ok(desired)
}

pub fn read_v3_family_macro(
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    slot: u8,
) -> io::Result<Vec<Vec<u8>>> {
    v3_family_macro(transport, width, slot, sleep)
}

fn v3_family_macro(
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    slot: u8,
    delay: impl Fn(Duration),
) -> io::Result<Vec<Vec<u8>>> {
    let profile = combined_profile(WiredPolling::Combined, transport, &width, &delay)?;
    read_macro_chain(transport, &width, slot, &profile, &delay)
}

const MACRO_READ_BUDGET: usize = 32;
pub const MACRO_ARCHIVE_LIMIT: usize = 8 + MACRO_READ_BUDGET * crate::actions::MACRO_LEN;

fn macro_request(slot: u8) -> Request {
    Request {
        id: 9,
        len: crate::actions::MACRO_LEN as u8,
        selector: slot,
    }
}

fn read_macro_chain(
    transport: &mut dyn Transport,
    width: &impl Fn(u8) -> Option<usize>,
    slot: u8,
    profile: &Legacy,
    delay: &impl Fn(Duration),
) -> io::Result<Vec<Vec<u8>>> {
    let mut request = macro_request(slot);
    if slot == 0 || !request.declared(width) {
        return Err(unsupported("no audited macro report on this endpoint"));
    }
    let bound = |slot| {
        profile
            .button_payload()
            .chunks_exact(3)
            .any(|entry| entry == [0x12, 0, slot])
    };
    if !bound(slot) {
        return Err(unsupported(
            "this mouse does not report that start slot in its button map",
        ));
    }
    let mut pages = Vec::new();
    for _ in 0..MACRO_READ_BUDGET {
        let bytes = request.read(transport, width, delay)?;
        if bytes[2] != request.selector {
            return Err(invalid("macro reply did not identify the requested slot"));
        }
        let marker = crate::actions::macro_continuation(&bytes)?;
        pages.push(bytes);
        let Some(marker) = marker else {
            return Ok(pages);
        };
        let next = if transport.connection() == Connection::Bluetooth {
            (marker > request.selector && marker <= 24).then_some(marker)
        } else {
            request.selector.checked_add(1)
        };
        request.selector = next.filter(|next| !bound(*next)).ok_or_else(|| {
            invalid("macro continuation crosses another assignment or the selector range")
        })?;
    }
    Err(invalid(
        "macro continuation exceeds the supported read budget",
    ))
}

pub fn archive_model_macro(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    slot: u8,
) -> io::Result<Vec<u8>> {
    archive_macro(model, transport, width, slot, sleep)
}

fn macro_session(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: &impl Fn(u8) -> Option<usize>,
    delay: &impl Fn(Duration),
) -> io::Result<([u8; 2], Legacy, bool)> {
    if (16..=18).contains(&model.slot) {
        let firmware = v3_session(model, transport, width, delay)?;
        return Ok((firmware, read_combined(transport, width, delay)?, true));
    }
    if model.fast_receiver
        || transport.connection() != Connection::Wired
        || !macro_request(0).declared(width)
    {
        return Err(unsupported("no audited macro readback on this endpoint"));
    }
    let (route, firmware) = ProfileRead::open(model, transport, width, delay)?;
    let active = route.capture(model, transport, width, delay)?;
    Ok((firmware, active, false))
}

fn occupied_macro_pages(
    transport: &mut dyn Transport,
    width: &impl Fn(u8) -> Option<usize>,
    slot: u8,
    profile: &Legacy,
    delay: &impl Fn(Duration),
    selected: bool,
) -> io::Result<Vec<Vec<u8>>> {
    if selected {
        return read_macro_chain(transport, width, slot, profile, delay);
    }
    if slot == 0
        || !profile
            .button_payload()
            .chunks_exact(3)
            .any(|action| action == [0x12, 0, slot])
    {
        return Err(unsupported(
            "the active button map does not reference this macro slot",
        ));
    }
    let page = direct_feature(transport, macro_request(slot), Some(129))?;
    if page[2] != slot || crate::actions::macro_continues(&page)? {
        return Err(unsupported(
            "the device did not return the requested occupied single-page macro",
        ));
    }
    Ok(vec![page])
}

fn archive_macro(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    slot: u8,
    delay: impl Fn(Duration),
) -> io::Result<Vec<u8>> {
    let (firmware, profile, selected) = macro_session(model, transport, &width, &delay)?;
    let pages = occupied_macro_pages(transport, &width, slot, &profile, &delay, selected)?;
    let mut result = b"SDM1".to_vec();
    result.extend([model.slot, firmware[0], firmware[1], slot]);
    result.extend(pages.into_iter().flatten());
    Ok(result)
}

pub fn restore_model_macro(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    archive: &[u8],
) -> io::Result<usize> {
    restore_macro(model, transport, width, archive, sleep)
}

fn check_macro_archive(bytes: &[u8], slot: u8, receiver: bool) -> io::Result<[[u8; 64]; 3]> {
    let mut canonical = bytes.to_vec();
    crate::actions::macro_continuation(&canonical)?;
    if canonical[2] != slot {
        return Err(invalid("archive page identifies a different slot"));
    }
    let sum = canonical[3..129]
        .iter()
        .copied()
        .map(u16::from)
        .sum::<u16>()
        .to_be_bytes();
    if canonical[129..] != [0, 0] && canonical[129..] != sum {
        return Err(invalid("macro archive checksum differs"));
    }
    canonical[129..].copy_from_slice(&sum);
    crate::actions::model_macro_chunks(&canonical, receiver)
}

fn restore_macro(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    archive: &[u8],
    delay: impl Fn(Duration),
) -> io::Result<usize> {
    use crate::actions::{MACRO_LEN, macro_continuation};
    if !(8 + MACRO_LEN..=MACRO_ARCHIVE_LIMIT).contains(&archive.len())
        || (archive.len() - 8) % MACRO_LEN != 0
        || &archive[..4] != b"SDM1"
        || archive[4] != model.slot
        || archive[7] == 0
    {
        return Err(invalid(
            "macro backup is malformed or targets another model",
        ));
    }
    let slot = archive[7];
    let desired: Vec<_> = archive[8..].chunks_exact(MACRO_LEN).collect();
    let mut packets = Vec::new();
    let receiver = transport.connection() == Connection::Receiver;
    for (index, &page) in desired.iter().enumerate() {
        let target = slot
            .checked_add(index as u8)
            .ok_or_else(|| invalid("macro slot overflows"))?;
        let next = if index + 1 < desired.len() {
            Some(
                target
                    .checked_add(1)
                    .ok_or_else(|| invalid("macro slot overflows"))?,
            )
        } else {
            None
        };
        if macro_continuation(page)? != next {
            return Err(invalid(
                "archive continuation does not match the supplied pages",
            ));
        }
        packets.push(check_macro_archive(page, target, receiver)?);
    }
    let (firmware, profile, selected) = macro_session(model, transport, &width, &delay)?;
    if firmware != archive[5..7] {
        return Err(invalid("macro backup does not match this firmware"));
    }
    // Read every current page first. A shortened or expanded chain would touch storage whose
    // erased state cannot currently be restored.
    let original = occupied_macro_pages(transport, &width, slot, &profile, &delay, selected)?;
    if original.len() != desired.len() || original.iter().zip(&desired).any(|(a, b)| a[2] != b[2]) {
        return Err(unsupported(
            "restore requires the same occupied macro slots",
        ));
    }
    let rollback: Vec<_> = original
        .iter()
        .map(|page| check_macro_archive(page, page[2], receiver))
        .collect::<io::Result<_>>()?;
    let write = |transport: &mut dyn Transport, chunks: &[[u8; 64]; 3], expected: &[u8]| {
        if transport.connection() == Connection::Bluetooth {
            let mut report = expected.to_vec();
            crate::settings::stamp_checksum(&mut report, 129);
            transport.set_feature(&report)?;
        } else {
            for chunk in chunks {
                transport.set_feature(chunk)?;
                delay(Duration::from_millis(200));
            }
        }
        delay(Duration::from_millis(500));
        let request = macro_request(expected[2]);
        let actual = if selected {
            request.read(transport, &width, &delay)?
        } else {
            direct_feature(transport, request, Some(129))?
        };
        check_macro_archive(&actual, expected[2], receiver)?;
        if actual[..129] != expected[..129] {
            return Err(invalid("macro page did not verify"));
        }
        Ok(())
    };
    let changed: Vec<_> = (0..desired.len())
        .filter(|&i| desired[i][..129] != original[i][..129])
        .collect();
    for (at, &i) in changed.iter().enumerate() {
        if let Err(error) = write(transport, &packets[i], desired[i]) {
            let mut failed = Vec::new();
            for &prior in changed[..=at].iter().rev() {
                if write(transport, &rollback[prior], &original[prior]).is_err() {
                    failed.push(original[prior][2]);
                }
            }
            return Err(io::Error::new(
                error.kind(),
                if failed.is_empty() {
                    format!("{error}; original macro pages verified")
                } else {
                    format!("{error}; restoration could not be verified for slots {failed:?}")
                },
            ));
        }
    }
    Ok(changed.len())
}

pub fn update_direct_polling(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    desired: Option<u16>,
) -> io::Result<u16> {
    if transport.connection() != Connection::Wired || !POLLING.declared(&width) || model.slot == 3 {
        return Err(unsupported("no direct polling report on this endpoint"));
    }
    let read = |transport: &mut dyn Transport| {
        let bytes = direct_feature(transport, POLLING, None)?;
        if bytes[4] != !bytes[3] || !valid_rate_code(model.slot, bytes[3]) {
            return Err(invalid("direct polling report has an invalid rate code"));
        }
        Ok(bytes[3])
    };
    let before = read(transport)?;
    let rates = [125, 250, 500, 1000, 2000, 4000, 8000];
    let decode = |code| -> io::Result<u16> {
        match model.slot {
            7 | 13 => rates
                .get(usize::from((code & 0xfu8) - 1))
                .copied()
                .ok_or_else(|| invalid("unknown polling rate index")),
            8 => [0x20, 0x10, 8, 4, 2, 1, 0x40]
                .iter()
                .position(|&v| v == code)
                .map(|i| rates[i])
                .ok_or_else(|| invalid("unknown X3 Pro polling rate")),
            _ => Ok(1000 / u16::from(code)),
        }
    };
    let original = decode(before)?;
    let Some(rate) = desired.filter(|rate| *rate != original) else {
        return Ok(original);
    };
    let index = rates
        .iter()
        .position(|&value| value == rate)
        .filter(|&index| model.slot == 8 || index < 4)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "polling rate unavailable on this model",
            )
        })?;
    let next = match model.slot {
        7 | 13 => before & 0xf0 | (index as u8 + 1),
        8 => [0x20, 0x10, 8, 4, 2, 1, 0x40][index],
        _ => 8 >> index,
    };
    write_verified(
        transport,
        &polling_frame(before),
        &polling_frame(next),
        |transport, packet| Ok(read(transport)? == packet[3]),
        sleep,
    )?;
    Ok(rate)
}

pub fn update_model_timing(
    model: &crate::models::Model,
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    edit: Option<impl FnOnce(&mut Timing) -> io::Result<()>>,
) -> io::Result<Timing> {
    let route = model.wired_polling();
    if model.slot == 3
        || !(transport.connection() == Connection::Wired
            || transport.connection() == Connection::Receiver
                && route == Some(WiredPolling::Combined))
    {
        return Err(unsupported("no matched model timing endpoint"));
    }
    let combined = match route {
        Some(WiredPolling::Combined) => {
            v3_session(model, transport, &width, &sleep)?;
            true
        }
        Some(route) => route.select_readback(transport, &width, &sleep)? == COMBINED,
        None => false,
    };
    if (!combined || edit.is_some()) && !TIMING.declared(&width) {
        return Err(unsupported(
            "the timing report is absent from this descriptor",
        ));
    }
    let read = |transport: &mut dyn Transport| {
        if combined {
            return Ok(read_combined(transport, &width, &sleep)?.timing());
        }
        let bytes = if route.is_some() || model.slot == 13 {
            TIMING.read(transport, &width, &sleep)?
        } else {
            direct_feature(transport, TIMING, Some(11))?
        };
        Timing::parse(&bytes)
    };
    let original = read(transport)?;
    let mut desired = original.clone();
    if let Some(edit) = edit {
        edit(&mut desired)?;
        write_verified(
            transport,
            &original.write_bytes(),
            &desired.write_bytes(),
            |transport, packet| Ok(read(transport)?.write_bytes() == packet),
            sleep,
        )?;
    }
    Ok(desired)
}

fn write_verified(
    transport: &mut dyn Transport,
    before: &[u8],
    after: &[u8],
    verify: impl Fn(&mut dyn Transport, &[u8]) -> io::Result<bool>,
    delay: impl Fn(Duration),
) -> io::Result<()> {
    if before == after {
        return Ok(());
    }
    let write = |transport: &mut dyn Transport, frame: &[u8]| {
        transport.set_feature(frame)?;
        delay(Duration::from_millis(200));
        if !verify(transport, frame)? {
            return Err(invalid("settings write did not verify"));
        }
        Ok(())
    };
    if let Err(error) = write(transport, after) {
        let recovered =
            verify(transport, before).unwrap_or(false) || write(transport, before).is_ok();
        return Err(io::Error::new(
            error.kind(),
            format!(
                "{error}; original setting {}",
                if recovered {
                    "verified"
                } else {
                    "could not be verified"
                }
            ),
        ));
    }
    Ok(())
}

pub fn x11_timing_available(width: impl Fn(u8) -> Option<usize>) -> bool {
    [RECEIPT, VERSION, TIMING]
        .into_iter()
        .all(|report| report.declared(&width))
}

pub fn update_x11_timing(
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    edit: impl FnOnce(&mut Timing) -> io::Result<()>,
) -> io::Result<Timing> {
    x11_timing(transport, width, edit, sleep)
}

fn x11_timing(
    transport: &mut dyn Transport,
    width: impl Fn(u8) -> Option<usize>,
    edit: impl FnOnce(&mut Timing) -> io::Result<()>,
    delay: impl Fn(Duration),
) -> io::Result<Timing> {
    if transport.connection() != Connection::Wired || !x11_timing_available(&width) {
        return Err(unsupported("no audited wired X11 timing collection"));
    }
    let version = VERSION.read(transport, &width, &delay)?;
    if version[2] != 0x55 {
        return Err(unsupported(
            "the X11 timing port requires split firmware 0x55",
        ));
    }
    let original = Timing::parse(&TIMING.read(transport, &width, &delay)?)?;
    let mut desired = original.clone();
    edit(&mut desired)?;
    if desired.bytes()[3..11] == original.bytes()[3..11] {
        return Ok(original);
    }
    transport.set_feature(&desired.write_bytes())?;
    delay(Duration::from_millis(200));
    let after = Timing::parse(&TIMING.read(transport, &width, &delay)?)?;
    if after.bytes()[3..11] != desired.bytes()[3..11] {
        return Err(io::Error::other("X11 timing write did not verify"));
    }
    Ok(after)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn unsupported(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MODELS;

    struct Mouse {
        profile_count: u8,
        sent: Vec<Vec<u8>>,
        rate_code: u8,
        firmware: u8,
        accept_changes: bool,
        reject_requests: bool,
        connection: Connection,
        timing: Timing,
        dpi: [u8; 56],
        buttons: [u8; 59],
        macro_slot: u8,
        macro_continues: bool,
        pages: std::collections::BTreeMap<u8, Vec<u8>>,
        macro_transfer: Option<Vec<u8>>,
        fail_once: Option<u8>,
    }

    impl Mouse {
        fn new(firmware: u8) -> Self {
            Self {
                sent: Vec::new(),
                profile_count: 1,
                rate_code: 2,
                firmware,
                accept_changes: true,
                reject_requests: false,
                connection: Connection::Wired,
                timing: Timing::default(),
                dpi: Dpi::default().write_bytes(),
                buttons: crate::actions::default_buttons(),
                macro_slot: 12,
                macro_continues: false,
                pages: Default::default(),
                macro_transfer: None,
                fail_once: None,
            }
        }
    }

    impl Transport for Mouse {
        fn connection(&self) -> Connection {
            self.connection
        }

        fn set_feature(&mut self, report: &[u8]) -> io::Result<()> {
            self.sent.push(report.to_vec());
            if report == crate::driver::RESET_PREAMBLE {
                return Ok(());
            }
            if self.fail_once == Some(report[0]) && report[0] != 9 {
                self.fail_once = None;
                return Err(io::Error::other("fixture write failure"));
            }
            if report[0] == 9 && report.len() == 131 {
                self.pages.insert(report[2], report.to_vec());
                if self.fail_once == Some(9) && report[2] == 22 {
                    self.fail_once = None;
                    return Err(io::Error::other(
                        "fixture failure after applying Bluetooth page",
                    ));
                }
            } else if let [9, 64 | 12, slot, fragment, payload @ ..] = report {
                match fragment {
                    0 => {
                        let mut page = vec![9, 131, *slot];
                        page.extend_from_slice(payload);
                        self.macro_transfer = Some(page);
                    }
                    1 => self
                        .macro_transfer
                        .as_mut()
                        .unwrap()
                        .extend_from_slice(payload),
                    2 => {
                        let mut page = self.macro_transfer.take().unwrap();
                        page.extend_from_slice(&payload[..8]);
                        self.pages.insert(*slot, page);
                        if self.fail_once == Some(9) && *slot == 22 {
                            self.fail_once = None;
                            return Err(io::Error::other("fixture failure after applying page"));
                        }
                    }
                    _ => panic!("unexpected macro fragment"),
                }
            } else if let [6, 9, 1, code, inverse, 0, 0, 0, 0] = *report {
                assert_eq!(inverse, !code);
                if self.accept_changes {
                    self.rate_code = code;
                }
            } else if report[0] == 5 {
                let timing = Timing::parse(report).unwrap();
                assert_eq!(report, timing.write_bytes());
                if self.accept_changes {
                    self.timing = timing;
                }
            } else if matches!(report[0], 4 | 8) {
                let sum: u16 = report[3..if report[0] == 4 { 50 } else { 57 }]
                    .iter()
                    .copied()
                    .map(u16::from)
                    .sum();
                let at = if report[0] == 4 { 50 } else { 57 };
                assert_eq!(report[at..at + 2], sum.to_be_bytes());
                if self.accept_changes {
                    if report[0] == 4 {
                        self.dpi.copy_from_slice(report);
                    } else {
                        self.buttons.copy_from_slice(report);
                    }
                }
            } else {
                assert_eq!(
                    report,
                    match report[1] {
                        0x0a => [0xa0, 0x0a, 128, 0, 1, 0, 0, 0],
                        0x0b => [0xa0, 0x0b, 8, 0, 0, 0, 0, 0],
                        0x0c => [0xa0, 0x0c, 10, 0, 0, 0, 0, 0],
                        0x06 => [0xa0, 0x06, 9, 0, 1, 0, 0, 0],
                        0x05 => [0xa0, 0x05, 15, 0, 1, 0, 0, 0],
                        0x04 => [0xa0, 0x04, 56, 0, 1, 0, 0, 0],
                        0x09 => [0xa0, 0x09, 131, 0, report[4], 0, 0, 0],
                        _ => panic!("unexpected report: {report:?}"),
                    }
                );
            }
            Ok(())
        }

        fn get_feature(&mut self, id: u8, length: usize) -> io::Result<Vec<u8>> {
            let mut frame = vec![0; length];
            frame[..2].copy_from_slice(&[id, length as u8]);
            match id {
                0xa0 => frame[1] = u8::from(!self.reject_requests),
                0x0b => frame[2] = self.firmware,
                0x06 => {
                    frame[3] = self.rate_code;
                    frame[4] = !self.rate_code;
                }
                0x08 => return Ok(self.buttons.to_vec()),
                0x0a => {
                    frame[3] = self.rate_code;
                    frame[5..54].copy_from_slice(&self.dpi[3..52]);
                    frame[55..65].copy_from_slice(&self.timing.bytes()[3..13]);
                    frame[65..119].copy_from_slice(&self.buttons[3..57]);
                }
                0x04 => return Ok(self.dpi.to_vec()),
                0x09 => {
                    let next = self
                        .sent
                        .last()
                        .filter(|frame| frame[0..2] == [0xa0, 9])
                        .map_or(self.macro_slot, |request| request[4]);
                    if let Some(page) = self.pages.get(&next) {
                        return Ok(page.clone());
                    }
                    frame[2] = if next == 12 { self.macro_slot } else { next };
                    if self.macro_continues && next == 12 {
                        frame[28..37].copy_from_slice(&[4, 3, 0xf6, 1, 2, 3, 4, 1, 0xfe]);
                    }
                }
                0x0c => {
                    frame[2..6].copy_from_slice(&[1, 0xfe, self.profile_count, !self.profile_count])
                }
                0x05 => return Ok(self.timing.bytes().to_vec()),
                _ => panic!("unexpected read: {id:#04x}"),
            }
            Ok(frame)
        }

        fn read_input(&mut self, _: Duration) -> io::Result<Option<Vec<u8>>> {
            panic!("wired polling shouldn't start reading input")
        }
    }

    fn width(id: u8) -> Option<usize> {
        [
            RECEIPT,
            VERSION,
            PROFILE_RANGE,
            POLLING,
            TIMING,
            DPI,
            BUTTONS,
            COMBINED,
        ]
        .iter()
        .find(|report| report.id == id)
        .map(|report| usize::from(report.len))
    }

    #[test]
    fn every_traced_dpi_route_preserves_other_stages_and_reads_back() {
        for (slot, firmware, value, expected, query) in [
            (4, 0x55, 20_200, [118, 1, 0x21, 0x21], 4),
            (6, 0, 26_000, [7, 2, 0x20, 0x20], 4),
            (10, 0x07, 24_000, [141, 1, 0x21, 0x21], 4),
            (11, 0x55, 24_100, [189, 0, 0x21, 0x21], 4),
            (12, 0, 40_000, [143, 1, 0x21, 0x20], 4),
            (13, 0, 800, [15, 0, 0x20, 0x20], 4),
            (14, 0x55, 24_000, [141, 1, 0x21, 0x20], 4),
            (15, 0x41, 18_100, [212, 0, 0x21, 0x20], 0x0a),
            (16, 0xe5, 25_000, [199, 0, 0x21, 0x21], 0x0a),
            (17, 0x24, 400, [9, 0, 0x20, 0x20], 0x0a),
            (18, 0xea, 100, [2, 0, 0x20, 0x20], 0x0a),
        ] {
            let route = WiredDpi::for_slot(slot).unwrap();
            let mut mouse = Mouse::new(firmware);
            let before = mouse.dpi;
            let edit = |dpi: &mut Dpi| dpi.set_stage_dpi(1, value);
            let result = route
                .transact(&mut mouse, width, Some(edit), |_| {})
                .unwrap();
            assert_eq!(result.stage(1).unwrap().dpi, Some(value), "slot {slot}");
            assert_eq!(
                [mouse.dpi[8], mouse.dpi[16], mouse.dpi[6], mouse.dpi[7]],
                expected,
                "slot {slot}"
            );
            assert_eq!(&mouse.dpi[25..49], &before[25..49]);
            assert_eq!(&mouse.dpi[9..16], &before[9..16]);
            assert_eq!(mouse.sent.last().unwrap()[1], query);
            if slot >= 16 {
                assert_eq!(mouse.sent[0], [0xa0, 0x0c, 10, 0, 0, 0, 0, 0]);
            }
            mouse.accept_changes = false;
            let other = |dpi: &mut Dpi| dpi.set_stage_color(1, [1, 2, 3]);
            assert!(
                route
                    .transact(&mut mouse, width, Some(other), |_| {})
                    .is_err()
            );
            mouse.connection = Connection::Bluetooth;
            mouse.sent.clear();
            assert!(
                route
                    .transact(&mut mouse, width, Some(edit), |_| {})
                    .is_err()
            );
            assert!(mouse.sent.is_empty());
        }
    }

    #[test]
    fn v3_family_reads_bound_page_and_verifies_button_changes() {
        use crate::actions::{Action, button_action};
        for firmware in [0xe5, 0x24, 0xea] {
            let mut mouse = Mouse::new(firmware);
            mouse.buttons[6..9].copy_from_slice(&[0x12, 0, 12]);
            let width = |id| if id == 9 { Some(131) } else { width(id) };
            let before = mouse.buttons;
            assert_eq!(
                v3_family_macro(&mut mouse, width, 12, |_| {}).unwrap()[0][..3],
                [9, 131, 12]
            );
            assert_eq!(mouse.sent.last().unwrap()[4], 12);
            mouse.macro_continues = true;
            let pages = v3_family_macro(&mut mouse, width, 12, |_| {}).unwrap();
            assert_eq!(pages.len(), 2);
            assert_eq!(&pages[0][29..35], &[3, 0xf6, 1, 2, 3, 4]);
            assert_eq!(pages[1][2], 13);
            mouse.buttons[9..12].copy_from_slice(&[0x12, 0, 13]);
            assert!(v3_family_macro(&mut mouse, width, 12, |_| {}).is_err());
            assert_eq!(mouse.sent.last().unwrap()[4], 12);
            assert!(v3_family_macro(&mut mouse, width, 3, |_| {}).is_err());
            assert_eq!(mouse.sent.last().unwrap()[1], COMBINED.id);

            mouse.sent.clear();
            assert!(
                v3_family_button(
                    &mut mouse,
                    width,
                    Some((3, Action::Preset("right-click"))),
                    |_| {}
                )
                .is_err()
            );
            assert!(mouse.sent.iter().all(|packet| packet[0] != 8));
            let change = Some((5, Action::Preset("right-click")));
            let result = v3_family_button(&mut mouse, width, change, |_| {}).unwrap();
            assert_eq!(button_action(&result, 5).unwrap(), change.unwrap().1);
            assert_eq!(result[6..9], before[6..9]);
            let original =
                combined_profile(WiredPolling::Combined, &mut mouse, &width, &|_| {}).unwrap();
            assert_eq!(original.buttons(), result);

            mouse.sent.clear();
            let forbidden = Some((2, Action::Macro { slot: 2 }));
            assert!(v3_family_button(&mut mouse, width, forbidden, |_| {}).is_err());
            assert!(mouse.sent.is_empty());
            mouse.accept_changes = false;
            assert!(
                v3_family_button(
                    &mut mouse,
                    width,
                    Some((5, Action::Preset("middle-click"))),
                    |_| {}
                )
                .is_err()
            );
            mouse.macro_slot = 13;
            assert!(v3_family_macro(&mut mouse, width, 12, |_| {}).is_err());
        }
    }

    fn altered_archive(original: [u8; ARCHIVE_LEN], xor: u8) -> [u8; ARCHIVE_LEN] {
        let mut archive = original;
        archive[7 + 5 + 22] ^= xor;
        let sum: u16 = archive[7 + 5..7 + 52].iter().copied().map(u16::from).sum();
        archive[7 + 52..7 + 54].copy_from_slice(&sum.to_be_bytes());
        archive
    }

    fn factory_mouse(model: &crate::models::Model) -> Mouse {
        let [dpi, timing, polling, buttons] = factory_frames(model.slot).unwrap();
        let tag = match model.wired_polling() {
            Some(WiredPolling::Versioned(tag)) => tag,
            _ => model.receiver_tag,
        };
        let mut mouse = Mouse::new(tag);
        mouse.dpi.copy_from_slice(&dpi);
        mouse.timing = Timing::parse(&timing).unwrap();
        mouse.rate_code = polling[3];
        mouse.buttons.copy_from_slice(&buttons);
        mouse
    }

    #[test]
    fn full_wired_reset_tests_all_non_r1_models_and_guards_profile_range() {
        for model in MODELS.iter().filter(|model| model.slot != 3) {
            let mut mouse = factory_mouse(model);
            mouse.profile_count = 2;
            assert!(
                reset_model_full_with_delay(model, &mut mouse, width, |_| {}).is_err(),
                "{}",
                model.name
            );
            assert!(!mouse.sent.iter().any(|frame| frame[0] == 12));
            mouse.sent.clear();
            mouse.profile_count = 1;
            assert_eq!(
                reset_model_full_with_delay(model, &mut mouse, width, |_| {}).unwrap(),
                4
            );
            let writes: Vec<_> = mouse
                .sent
                .iter()
                .filter(|frame| frame[0] != 0xa0)
                .map(|frame| frame[0])
                .collect();
            assert_eq!(writes, [12, 4, 5, 6, 8]);
        }
    }

    #[test]
    fn every_non_r1_model_writes_through_its_readback_route_and_restores_defaults() {
        for model in MODELS.iter().filter(|model| model.slot != 3) {
            let mut mouse = factory_mouse(model);
            let first = archive_profile(model, &mut mouse, width, |_| {}).unwrap();
            let modified = altered_archive(first, 0x42);
            assert_eq!(
                restore_profile(model, &mut mouse, width, &modified, |_| {}).unwrap(),
                1,
                "{}",
                model.name
            );
            assert_eq!(
                archive_profile(model, &mut mouse, width, |_| {}).unwrap(),
                modified
            );
            let back =
                crate::actions::parse_named_button("back", &crate::actions::button_names(model))
                    .unwrap();
            let changed = model_button(
                model,
                &mut mouse,
                width,
                Some((back, crate::actions::Action::Preset("copy"))),
                |_| {},
            )
            .unwrap();
            assert_eq!(
                crate::actions::button_action(&changed, back).unwrap(),
                crate::actions::Action::Preset("copy")
            );
            assert_eq!(
                apply_model_frames(
                    model,
                    &mut mouse,
                    width,
                    factory_frames(model.slot).unwrap(),
                    true,
                    |_| {}
                )
                .unwrap(),
                2
            );
            assert_eq!(
                archive_profile(model, &mut mouse, width, |_| {}).unwrap(),
                first
            );
            if model.wired_polling().is_none() {
                assert!(
                    mouse
                        .sent
                        .iter()
                        .all(|frame| [4, 5, 6, 8].contains(&frame[0]))
                );
            }
            mouse.sent.clear();
            let missing = |id| if id == 8 { None } else { width(id) };
            assert!(restore_profile(model, &mut mouse, missing, &first, |_| {}).is_err());
            assert!(mouse.sent.is_empty());
        }
    }

    #[test]
    fn bluetooth_dpi_writes_require_model_matched_baseline_and_verify() {
        for model in MODELS
            .iter()
            .filter(|model| model.slot != 13 && crate::bluetooth::has_settings_callback(model))
        {
            let mut mouse = factory_mouse(model);
            mouse.connection = Connection::Bluetooth;
            mouse.firmware = model.receiver_tag;
            let edit = |dpi: &mut Dpi| dpi.set_stage_color(1, [0x12, 0x34, 0x56]);
            assert_eq!(
                bluetooth_dpi(model, &mut mouse, Some(edit), |_| {})
                    .unwrap()
                    .stage(1)
                    .unwrap()
                    .color,
                [0x12, 0x34, 0x56]
            );
            mouse.sent.clear();
            mouse.firmware = 0xff;
            assert!(bluetooth_dpi(model, &mut mouse, Some(edit), |_| {}).is_err());
            assert_eq!(mouse.sent, vec![vec![0xa0, 0x0b, 8, 0, 0, 0, 0, 0]]);
        }
    }

    #[test]
    fn x8_pro_bluetooth_dpi_uses_only_snapshot_when_transport_confirms_identity() {
        let model = &MODELS[12];
        let mut mouse = factory_mouse(model);
        mouse.connection = Connection::Bluetooth;
        mouse.rate_code = 0x44;
        let edit = |dpi: &mut Dpi| dpi.set_stage_color(1, [0x12, 0x34, 0x56]);
        let new = bluetooth_dpi(model, &mut mouse, Some(edit), |_| {}).unwrap();
        assert_eq!(new.stage(1).unwrap().color, [0x12, 0x34, 0x56]);
        assert_eq!(
            mouse
                .sent
                .iter()
                .map(|report| report[..2].to_vec())
                .collect::<Vec<_>>(),
            [[0xa0, 0x0a], [4, 56], [0xa0, 0x0a]]
        );

        mouse.sent.clear();
        mouse.rate_code = 4;
        assert!(bluetooth_dpi(model, &mut mouse, Some(edit), |_| {}).is_err());
        assert_eq!(mouse.sent, [[0xa0, 0x0a, 128, 0, 1, 0, 0, 0]]);
    }

    #[test]
    fn v3_profile_backup_checks_identity_and_rolls_back_all_attempted_groups() {
        for model in &MODELS[15..] {
            let mut mouse = Mouse::new(model.receiver_tag);
            let before = archive_profile(model, &mut mouse, width, |_| {}).unwrap();
            let mut desired = before;
            desired[7 + 3] = 1;
            desired[7 + 5 + 22] = 0x55;
            let mut wrong = desired;
            wrong[4] = 1;
            mouse.sent.clear();
            assert!(restore_profile(model, &mut mouse, width, &wrong, |_| {}).is_err());
            assert!(mouse.sent.is_empty());
            mouse.fail_once = Some(6);
            let error = restore_profile(model, &mut mouse, width, &desired, |_| {}).unwrap_err();
            assert!(error.to_string().contains("original settings verified"));
            assert_eq!(
                archive_profile(model, &mut mouse, width, |_| {}).unwrap(),
                before
            );
            assert_eq!(
                restore_profile(model, &mut mouse, width, &desired, |_| {}).unwrap(),
                2
            );
            assert_eq!(
                restore_profile(model, &mut mouse, width, &desired, |_| {}).unwrap(),
                0
            );
            let after = archive_profile(model, &mut mouse, width, |_| {}).unwrap();
            assert_eq!(after[10], 1);
            assert_eq!(after[7 + 5 + 22], 0x55);
            assert_eq!(&before[7 + 65..7 + 119], &after[7 + 65..7 + 119]);
        }
    }

    #[test]
    fn v3_profile_restore_uses_paired_receiver_and_bluetooth_readback() {
        for model in &MODELS[15..] {
            for connection in [Connection::Receiver, Connection::Bluetooth] {
                let mut mouse = factory_mouse(model);
                mouse.connection = connection;
                let before = archive_profile(model, &mut mouse, width, |_| {}).unwrap();
                let mut changed = altered_archive(before, 8);
                assert_eq!(
                    restore_profile(model, &mut mouse, width, &changed, |_| {}).unwrap(),
                    1
                );
                assert_eq!(
                    archive_profile(model, &mut mouse, width, |_| {}).unwrap(),
                    changed
                );
                assert!(!mouse.sent.iter().any(|packet| packet[0] == 0x0c));
                if connection == Connection::Bluetooth {
                    changed[7 + 3] = 2;
                    mouse.sent.clear();
                    assert!(restore_profile(model, &mut mouse, width, &changed, |_| {}).is_err());
                    assert!(mouse.sent.iter().all(|packet| packet[0] != 6));
                }
            }
        }
    }

    #[test]
    fn v3_macro_backup_reassembles_feature_fragments_and_recovers_from_partial_writes() {
        const WIDTH: fn(u8) -> Option<usize> = |id| if id == 9 { Some(131) } else { width(id) };
        for model in &MODELS[15..] {
            let mut mouse = Mouse::new(model.receiver_tag);
            mouse.buttons[6..9].copy_from_slice(&[0x12, 0, 21]);
            let mut first = vec![0; 131];
            let mut last = first.clone();
            first[..4].copy_from_slice(&[9, 131, 21, 0]);
            last[..3].copy_from_slice(&[9, 131, 22]);
            first[28..37].copy_from_slice(&[4, 1, 0xf6, 2, 3, 4, 5, 22, 0xfe]);
            last[28..31].copy_from_slice(&[1, 5, 4]);
            crate::settings::stamp_checksum(&mut first, 129);
            crate::settings::stamp_checksum(&mut last, 129);
            mouse.pages.insert(21, first);
            mouse.pages.insert(22, last);
            let original = archive_macro(model, &mut mouse, WIDTH, 21, |_| {}).unwrap();
            let mut desired = original.clone();
            desired[8 + 31] = 0x99;
            desired[8 + 131 + 29] = 10;
            for page in desired[8..].chunks_exact_mut(131) {
                crate::settings::stamp_checksum(page, 129);
            }
            mouse.fail_once = Some(9);
            let error = restore_macro(model, &mut mouse, WIDTH, &desired, |_| {}).unwrap_err();
            assert!(error.to_string().contains("original macro pages verified"));
            assert_eq!(
                archive_macro(model, &mut mouse, WIDTH, 21, |_| {}).unwrap(),
                original
            );
            assert_eq!(
                restore_macro(model, &mut mouse, WIDTH, &desired, |_| {}).unwrap(),
                2
            );
            assert_eq!(
                archive_macro(model, &mut mouse, WIDTH, 21, |_| {}).unwrap(),
                desired
            );
            let mut shortened = desired[..8 + 131].to_vec();
            shortened[8 + 28] = 3;
            crate::settings::stamp_checksum(&mut shortened[8..], 129);
            mouse.sent.clear();
            assert!(restore_macro(model, &mut mouse, WIDTH, &shortened, |_| {}).is_err());
            assert!(mouse.sent.iter().all(|report| report[0] == 0xa0));
        }
    }

    #[test]
    fn standard_wired_macro_restore_requires_the_current_matching_page() {
        let caps = |id| if id == 9 { Some(131) } else { width(id) };
        for model in MODELS
            .iter()
            .filter(|model| model.slot != 3 && model.slot < 16 && !model.fast_receiver)
        {
            let mut mouse = factory_mouse(model);
            mouse.buttons[6..9].copy_from_slice(&[0x12, 0, 12]);
            crate::settings::stamp_checksum(&mut mouse.buttons, 57);
            let mut page = vec![0; 131];
            page[..4].copy_from_slice(&[9, 131, 12, 0]);
            page[28..31].copy_from_slice(&[1, 1, 4]);
            crate::settings::stamp_checksum(&mut page, 129);
            mouse.pages.insert(12, page);
            let base = archive_macro(model, &mut mouse, caps, 12, |_| {}).unwrap();
            let mut after = base.clone();
            after[8 + 30] = 5;
            crate::settings::stamp_checksum(&mut after[8..], 129);
            assert_eq!(
                restore_macro(model, &mut mouse, caps, &after, |_| {}).unwrap(),
                1
            );
            assert_eq!(
                archive_macro(model, &mut mouse, caps, 12, |_| {}).unwrap(),
                after
            );
            mouse.macro_slot = 13;
            mouse.sent.clear();
            assert!(restore_macro(model, &mut mouse, caps, &base, |_| {}).is_err());
            assert!(mouse.sent.iter().all(|packet| packet[0] != 9));
        }
    }

    #[test]
    fn v3_receiver_and_bluetooth_macro_restore_verify_the_occupied_slots() {
        for model in &MODELS[15..] {
            for connection in [Connection::Receiver, Connection::Bluetooth] {
                let mut mouse = factory_mouse(model);
                mouse.connection = connection;
                mouse.buttons[6..9].copy_from_slice(&[0x12, 0, 21]);
                for (slot, marker) in [(21, Some(22)), (22, None)] {
                    let mut page = vec![0; 131];
                    page[..4].copy_from_slice(&[9, 131, slot, 0]);
                    page[28..35].copy_from_slice(&[3, 1, 0xf6, 1, 2, 3, 4]);
                    if let Some(marker) = marker {
                        page[28] = 4;
                        page[35..37].copy_from_slice(&[marker, 0xfe]);
                    }
                    crate::settings::stamp_checksum(&mut page, 129);
                    mouse.pages.insert(slot, page);
                }
                let base = archive_macro(
                    model,
                    &mut mouse,
                    bluetooth_width_for_macro_fixture,
                    21,
                    |_| {},
                )
                .unwrap();
                let mut desired = base.clone();
                desired[8 + 131 + 34] = 0x99;
                crate::settings::stamp_checksum(&mut desired[8 + 131..], 129);
                assert_eq!(
                    restore_macro(
                        model,
                        &mut mouse,
                        bluetooth_width_for_macro_fixture,
                        &desired,
                        |_| {}
                    )
                    .unwrap(),
                    1
                );
                let restored = archive_macro(
                    model,
                    &mut mouse,
                    bluetooth_width_for_macro_fixture,
                    21,
                    |_| {},
                )
                .unwrap();
                assert_eq!(restored, desired);
                if connection == Connection::Bluetooth {
                    // Same page count with different occupied slot. The archive must not write slot 22.
                    let other = mouse.pages.get_mut(&21).unwrap();
                    other[35] = 23;
                    crate::settings::stamp_checksum(other, 129);
                    let mut tail = mouse.pages.remove(&22).unwrap();
                    tail[2] = 23;
                    mouse.pages.insert(23, tail);
                    mouse.sent.clear();
                    assert!(
                        restore_macro(
                            model,
                            &mut mouse,
                            bluetooth_width_for_macro_fixture,
                            &desired,
                            |_| {}
                        )
                        .is_err()
                    );
                    assert!(mouse.sent.iter().all(|packet| packet[0] != 9));
                }
            }
        }
    }

    fn bluetooth_width_for_macro_fixture(id: u8) -> Option<usize> {
        if id == 9 { Some(131) } else { width(id) }
    }

    #[test]
    fn x11_timing_preserves_fields_and_verifies_the_split_write() {
        let mut mouse = Mouse::new(0x55);
        let start = mouse.timing.clone();
        let state = x11_timing(
            &mut mouse,
            width,
            |timing| {
                timing.set_sleep_seconds(120)?;
                timing.set_debounce_ms(12)
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(state.sleep_seconds(), 120);
        assert_eq!(state.debounce_ms(), 12);
        assert_eq!(&state.bytes()[3..9], &start.bytes()[3..9]);
        assert_eq!(
            mouse.sent,
            vec![
                vec![0xa0, 0x0b, 8, 0, 0, 0, 0, 0],
                vec![0xa0, 0x05, 15, 0, 1, 0, 0, 0],
                vec![0x05, 15, 1, 0, 3, 0xa8, 0, 255, 0, 4, 6, 0x01, 0xb4, 0, 0],
                vec![0xa0, 0x05, 15, 0, 1, 0, 0, 0],
            ]
        );
        mouse.sent.clear();
        mouse.firmware = 0x41;
        assert!(x11_timing(&mut mouse, width, |_| Ok(()), |_| {}).is_err());
        assert_eq!(mouse.sent, vec![vec![0xa0, 0x0b, 8, 0, 0, 0, 0, 0]]);
    }

    #[test]
    fn every_audited_models_polling_write_uses_its_own_readback_branch() {
        let mut count = 0;
        for model in MODELS {
            let Some(protocol) = model.wired_polling() else {
                continue;
            };
            count += 1;
            let branches = match protocol {
                WiredPolling::Versioned(tag) => vec![(tag, 6), (0xf0, 0x0a)],
                WiredPolling::Direct => vec![(0xf0, 6)],
                WiredPolling::Combined => vec![(0xf0, 0x0a)],
            };
            for (version, rate_report) in branches {
                let mut mouse = Mouse::new(version);
                assert_eq!(
                    protocol
                        .transact(&mut mouse, width, Some(250), |_| {})
                        .unwrap(),
                    250,
                    "{}",
                    model.name
                );
                let sequence: Vec<_> = mouse
                    .sent
                    .iter()
                    .map(|frame| (frame[0], frame[1]))
                    .collect();
                let mut expected = match protocol {
                    WiredPolling::Versioned(_) => vec![(0xa0, 0x0b)],
                    WiredPolling::Combined => vec![(0xa0, 0x0c), (0xa0, 0x0b)],
                    WiredPolling::Direct => vec![],
                };
                expected.extend([(0xa0, rate_report), (6, 9), (0xa0, rate_report)]);
                assert_eq!(sequence, expected, "{}", model.name);

                mouse.sent.clear();
                assert_eq!(
                    protocol
                        .transact(&mut mouse, width, Some(250), |_| {})
                        .unwrap(),
                    250
                );
                assert!(mouse.sent.iter().all(|frame| frame[0] == 0xa0));
            }
        }
        assert_eq!(count, 10);
    }

    #[test]
    fn rejects_an_unverified_change_and_preserves_read_only_descriptor_support() {
        let mut mouse = Mouse::new(0xf0);
        mouse.accept_changes = false;
        let versioned = WiredPolling::Versioned(0x55);
        let read_only = |id| if id == 6 { None } else { width(id) };
        assert!(
            versioned
                .transact(&mut mouse, read_only, Some(250), |_| {})
                .is_err()
        );
        assert!(mouse.sent.is_empty());
        assert_eq!(
            versioned
                .transact(&mut mouse, read_only, None, |_| {})
                .unwrap(),
            500
        );

        mouse.sent.clear();
        assert!(
            versioned
                .transact(&mut mouse, width, Some(250), |_| {})
                .unwrap_err()
                .to_string()
                .contains("did not verify")
        );
        assert_eq!(mouse.sent.iter().filter(|frame| frame[0] == 6).count(), 1);
        mouse.sent.clear();
        mouse.reject_requests = true;
        assert!(
            versioned
                .transact(&mut mouse, width, Some(250), |_| {})
                .is_err()
        );
        assert_eq!(mouse.sent.len(), 1);
        assert_eq!(mouse.sent[0][0], 0xa0);

        mouse.sent.clear();
        mouse.connection = Connection::Receiver;
        assert!(
            versioned
                .transact(&mut mouse, width, Some(250), |_| {})
                .is_err()
        );
        assert!(mouse.sent.is_empty());
    }
}
