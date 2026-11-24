use std::{fmt, io, io::Read};

pub const BUTTONS_LEN: usize = 59;
pub const MACRO_LEN: usize = 131;
pub const MACRO_CHUNK_LEN: usize = 64;
pub const VENDOR_MACRO_LEN: usize = 240;
pub const BUTTON_NAMES: [(usize, &str); 6] = [
    (1, "left"),
    (2, "right"),
    (3, "middle"),
    (7, "forward"),
    (8, "back"),
    (4, "dpi"),
];

const SLOTS: usize = 18;
const VENDOR_MAX_EVENTS: usize = 46;
const MAX_EVENTS: usize = SLOTS * 50;
const MAX_DELAY_MS: u16 = 50_000;
const DEFAULT_DELAY_MS: u16 = 10;
const PAIR_COUNT: usize = 28;
const PAIRS_START: usize = 29;
const MACRO_CHECKSUM: usize = 129;
const VENDOR_PROFILE_LEN: usize = 0x80e0;
const VENDOR_BUTTONS: usize = 0x9a0;
const VENDOR_BUTTON_STRIDE: usize = 0x4f8;

// (CLI name, report bytes, Windows profile action ID)
const PRESETS: &[(&str, [u8; 3], u8)] = &[
    ("disabled", [0x01, 0, 0], 0x00),
    ("left-click", [0x02, 0, 0], 0x01),
    ("right-click", [0x03, 0, 0], 0x02),
    ("middle-click", [0x04, 0, 0], 0x03),
    ("back", [0x05, 0, 0], 0x05),
    ("forward", [0x06, 0, 0], 0x04),
    ("double-click", [0x07, 0, 0], 0x09),
    ("fire", [0x08, 0, 0], 0x0a),
    ("scroll-up", [0x09, 0, 0], 0x0b),
    ("scroll-down", [0x0a, 0, 0], 0x0c),
    ("tilt-left", [0x0b, 0, 0], 0x0d),
    ("tilt-right", [0x0c, 0, 0], 0x0e),
    ("dpi-cycle", [0x0d, 0, 0], 0x06),
    ("dpi-up", [0x0e, 0, 0], 0x07),
    ("dpi-down", [0x0f, 0, 0], 0x08),
    ("easy-aim", [0x10, 0, 3], 0x16),
    ("media-player", [0x15, 0, 0], 0x40),
    ("previous-track", [0x16, 0, 0], 0x43),
    ("next-track", [0x17, 0, 0], 0x44),
    ("play-pause", [0x18, 0, 0], 0x41),
    ("media-stop", [0x19, 0, 0], 0x42),
    ("mute", [0x1a, 0, 0], 0x47),
    ("volume-up", [0x1b, 0, 0], 0x45),
    ("volume-down", [0x1c, 0, 0], 0x46),
    ("calculator", [0x1d, 0, 0], 0x58),
    ("email", [0x1e, 0, 0], 0x57),
    ("browser-forward", [0x20, 0, 0], 0x52),
    ("browser-back", [0x21, 0, 0], 0x53),
    ("browser-stop", [0x22, 0, 0], 0x54),
    ("files", [0x23, 0, 0], 0x59),
    ("browser-refresh", [0x24, 0, 0], 0x55),
    ("browser-home", [0x25, 0, 0], 0x50),
    ("browser-search", [0x26, 0, 0], 0x56),
    ("led-loop", [0x29, 0, 3], 0x17),
    ("profile-cycle", [0x34, 0, 0], 0x36),
    ("profile-up", [0x35, 0, 0], 0x37),
    ("profile-down", [0x36, 0, 0], 0x38),
    ("browser-favorites", [0x11, 3, 0x12], 0x51),
    ("cut", [0x11, 1, 0x1b], 0x20),
    ("copy", [0x11, 1, 0x06], 0x21),
    ("paste", [0x11, 1, 0x19], 0x22),
    ("open", [0x11, 1, 0x12], 0x23),
    ("save", [0x11, 1, 0x16], 0x24),
    ("find", [0x11, 1, 0x09], 0x25),
    ("redo", [0x11, 1, 0x1c], 0x26),
    ("undo", [0x11, 1, 0x1d], 0x27),
    ("select-all", [0x11, 1, 0x04], 0x28),
    ("print", [0x11, 1, 0x13], 0x29),
    ("close-window", [0x11, 4, 0x3d], 0x2a),
    ("switch-window", [0x11, 4, 0x2b], 0x2b),
    ("show-desktop", [0x11, 8, 0x07], 0x2c),
    ("run-command", [0x11, 8, 0x15], 0x2d),
    ("lock", [0x11, 8, 0x0f], 0x2e),
    ("new", [0x11, 1, 0x11], 0x2f),
    ("ctrl-equals", [0x11, 1, 0x2e], 0x30),
    ("ctrl-minus", [0x11, 1, 0x2d], 0x31),
    ("screenshot", [0x11, 0x0a, 0x16], 0x32),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Preset(&'static str),
    Shortcut { modifiers: u8, key: u8 },
    Macro { slot: u8 },
    Raw([u8; 3]),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum MacroMode {
    #[default]
    Times = 0,
    PressToStop = 1,
    Hold = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacroEvent {
    pub code: u8,
    pub pressed: bool,
    pub delay_ms: u16,
    pub extended: Option<[u8; 4]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Macro {
    pub mode: MacroMode,
    pub repeat: u8,
    pub metadata: [u8; 3],
    pub events: Vec<MacroEvent>,
}

impl Default for Macro {
    fn default() -> Self {
        Self {
            mode: MacroMode::Times,
            repeat: 1,
            metadata: [0; 3],
            events: Vec::new(),
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preset(name) => f.write_str(name),
            Self::Macro { slot } => write!(f, "macro:{slot}"),
            Self::Raw([a, b, c]) => write!(f, "raw:{a:02x},{b:02x},{c:02x}"),
            Self::Shortcut { modifiers, key } => {
                f.write_str("key:")?;
                let mut separator = "";
                for (mask, name) in [(1, "ctrl"), (2, "shift"), (4, "alt"), (8, "super")] {
                    if modifiers & mask != 0 {
                        write!(f, "{separator}{name}")?;
                        separator = "+";
                    }
                }
                if *key != 0 {
                    write!(f, "{separator}0x{key:02x}")?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for Macro {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self.mode {
            MacroMode::Times => "times",
            MacroMode::PressToStop => "press-to-stop",
            MacroMode::Hold => "hold",
        };
        writeln!(f, "mode {mode}\nrepeat {}", self.repeat)?;
        if self.metadata != [0; 3] {
            let [a, b, c] = self.metadata;
            writeln!(f, "metadata {a:02x},{b:02x},{c:02x}")?;
        }
        for event in &self.events {
            if let Some([a, b, c, d]) = event.extended {
                writeln!(
                    f,
                    "extended {} {a:02x}{b:02x}{c:02x}{d:02x}",
                    event.delay_ms
                )?;
                continue;
            }
            if event.code == 0xf6 {
                writeln!(f, "extended {} unavailable", event.delay_ms)?;
                continue;
            }
            let direction = if event.pressed { "down" } else { "up" };
            writeln!(f, "{direction} 0x{:02x} {}", event.code, event.delay_ms)?;
        }
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn bad_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn slot(value: usize) -> io::Result<usize> {
    if (1..=SLOTS).contains(&value) {
        Ok(value)
    } else {
        Err(invalid("button slot must be between 1 and 18"))
    }
}

pub(crate) const V3_BUTTON_NAMES: [(usize, &str); 6] = [
    (1, "left"),
    (2, "right"),
    (3, "middle"),
    (4, "forward"),
    (5, "back"),
    (6, "dpi"),
];

pub fn button_names(model: &crate::models::Model) -> [(usize, &'static str); 6] {
    let [forward, back, dpi] = match model.slot {
        6 | 9 | 11..=15 => [6, 5, 4],
        7 => [5, 6, 8],
        16 | 18 => [4, 5, 6],
        17 => [5, 4, 6],
        4 | 10 => [7, 8, 6],
        _ => [7, 8, 4],
    };
    [
        (1, "left"),
        (2, "right"),
        (3, "middle"),
        (forward, "forward"),
        (back, "back"),
        (dpi, "dpi"),
    ]
}

pub fn parse_button(value: &str) -> io::Result<usize> {
    parse_named_button(value, &BUTTON_NAMES)
}

pub fn parse_named_button(value: &str, names: &[(usize, &str)]) -> io::Result<usize> {
    let value = value.trim().to_ascii_lowercase();
    let label = match value.as_str() {
        "backward" => "back",
        "wheel" => "middle",
        _ => &value,
    };
    if let Some((slot, _)) = names.iter().find(|(_, name)| *name == label) {
        return Ok(*slot);
    }
    let number = label
        .strip_prefix("slot-")
        .or_else(|| label.strip_prefix("slot"))
        .unwrap_or(label);
    slot(number.parse().map_err(|_| {
        invalid("expected left, right, middle, forward, back, dpi, or a slot number")
    })?)
}

fn triplet(value: &str) -> io::Result<[u8; 3]> {
    let mut bytes = [0; 3];
    let mut fields = value.split(',');
    for byte in &mut bytes {
        let field = fields
            .next()
            .ok_or_else(|| invalid("expected three hex bytes"))?;
        *byte = u8::from_str_radix(field.strip_prefix("0x").unwrap_or(field), 16)
            .map_err(|_| invalid("expected three hex bytes"))?;
    }
    if fields.next().is_some() {
        return Err(invalid("expected three hex bytes"));
    }
    Ok(bytes)
}

pub fn parse_action(value: &str) -> io::Result<Action> {
    let value = value.trim().to_ascii_lowercase();
    if let Some(value) = value.strip_prefix("key:") {
        return parse_shortcut(value);
    }
    if let Some(value) = value.strip_prefix("macro:") {
        return Ok(Action::Macro {
            slot: parse_button(value)? as u8,
        });
    }
    if let Some(value) = value.strip_prefix("raw:") {
        let bytes = triplet(value)?;
        if let [0x12, 0, number] = bytes {
            return Ok(Action::Macro {
                slot: slot(usize::from(number))? as u8,
            });
        }
        return Ok(Action::Raw(bytes));
    }
    let name = match value.as_str() {
        "disable" | "off" | "none" => "disabled",
        "left" => "left-click",
        "right" => "right-click",
        "middle" | "wheel-click" => "middle-click",
        "backward" => "back",
        _ => &value,
    };
    PRESETS
        .iter()
        .find(|(preset, _, _)| *preset == name)
        .map(|(preset, _, _)| Action::Preset(preset))
        .ok_or_else(|| invalid(format!("unknown button action '{value}'")))
}

pub fn preset_names() -> impl Iterator<Item = &'static str> {
    PRESETS.iter().map(|(name, _, _)| *name)
}

fn parse_shortcut(value: &str) -> io::Result<Action> {
    let mut modifiers = 0;
    let mut key = 0;
    for word in value.split('+') {
        let bit = match word {
            "ctrl" | "control" => 1,
            "shift" => 2,
            "alt" => 4,
            "super" | "meta" | "win" => 8,
            _ if key == 0 => {
                key = key_code(word, false)?;
                continue;
            }
            _ => return Err(invalid("a shortcut can contain only one non-modifier key")),
        };
        if modifiers & bit != 0 {
            return Err(invalid("shortcut repeats a modifier"));
        }
        modifiers |= bit;
    }
    Ok(Action::Shortcut { modifiers, key })
}

fn button_header(report: &[u8]) -> io::Result<()> {
    if report.len() != BUTTONS_LEN || report[..2] != [0x08, BUTTONS_LEN as u8] {
        return Err(bad_data("invalid R1 button report header or length"));
    }
    Ok(())
}

fn button_bytes(action: Action, button: usize) -> io::Result<[u8; 3]> {
    match action {
        Action::Preset(name) => PRESETS
            .iter()
            .find(|(preset, _, _)| *preset == name)
            .map(|(_, bytes, _)| *bytes)
            .ok_or_else(|| invalid("unknown button preset")),
        Action::Shortcut { modifiers, key } if modifiers <= 15 && (modifiers != 0 || key != 0) => {
            Ok([0x11, modifiers, key])
        }
        Action::Macro { slot } if usize::from(slot) == button => Ok([0x12, 0, slot]),
        Action::Raw(bytes) => Ok(bytes),
        Action::Macro { .. } => Err(invalid("the macro slot must match the button slot")),
        Action::Shortcut { .. } => Err(invalid("a shortcut requires a key or modifier")),
    }
}

pub fn button_action(report: &[u8], button: usize) -> io::Result<Action> {
    button_header(report)?;
    let end = slot(button)? * 3 + 3;
    let bytes: [u8; 3] = report[end - 3..end].try_into().unwrap();
    if let Some((name, _, _)) = PRESETS.iter().find(|(_, preset, _)| *preset == bytes) {
        return Ok(Action::Preset(name));
    }
    Ok(match bytes {
        [0x11, modifiers, key] if modifiers <= 15 && (modifiers != 0 || key != 0) => {
            Action::Shortcut { modifiers, key }
        }
        [0x12, 0, slot] if (1..=SLOTS as u8).contains(&slot) => Action::Macro { slot },
        _ => Action::Raw(bytes),
    })
}

pub fn set_button_action(report: &mut [u8], button: usize, action: Action) -> io::Result<()> {
    set_named_button_action(report, button, action, &BUTTON_NAMES)
}

pub(crate) fn set_named_button_action(
    report: &mut [u8],
    button: usize,
    action: Action,
    names: &[(usize, &str)],
) -> io::Result<()> {
    button_header(report)?;
    let end = slot(button)? * 3 + 3;
    let new_action = button_bytes(action, button)?;
    let mut updated: [u8; BUTTONS_LEN] = report[..].try_into().unwrap();
    updated[end - 3..end].copy_from_slice(&new_action);
    validate_named_assignments(&updated, names)?;
    updated[2] = 1;
    checksum(&mut updated, 57);
    report.copy_from_slice(&updated);
    Ok(())
}

pub fn validate_button_assignments(report: &[u8]) -> io::Result<()> {
    validate_named_assignments(report, &BUTTON_NAMES)
}

pub(crate) fn validate_named_assignments(report: &[u8], names: &[(usize, &str)]) -> io::Result<()> {
    button_header(report)?;
    if !names
        .iter()
        .any(|(button, _)| report[button * 3..button * 3 + 3] == [0x02, 0, 0])
    {
        return Err(invalid(
            "keep at least one physical button assigned to left-click",
        ));
    }
    Ok(())
}

pub fn default_buttons() -> [u8; BUTTONS_LEN] {
    let mut data = [0; 54];
    let defaults = [2, 3, 4, 13, 0x3c, 15, 6, 5, 1, 1, 1, 1, 1, 1, 1, 1, 9, 10];
    for (entry, code) in data.chunks_exact_mut(3).zip(defaults) {
        entry[0] = code;
    }
    make_button_report(&data)
}

pub(crate) fn make_button_report(data: &[u8]) -> [u8; BUTTONS_LEN] {
    let mut report = [0; BUTTONS_LEN];
    report[..3].copy_from_slice(&[8, BUTTONS_LEN as u8, 1]);
    report[3..57].copy_from_slice(data);
    checksum(&mut report, 57);
    report
}

pub fn buttons_from_legacy(legacy: &[u8]) -> io::Result<[u8; BUTTONS_LEN]> {
    if legacy.len() != 128 || legacy[..2] != [0x0a, 0x80] {
        return Err(bad_data("invalid combined R1 settings report"));
    }
    Ok(make_button_report(&legacy[65..119]))
}

fn vendor_button_records(profile: &[u8]) -> io::Result<impl Iterator<Item = &[u8]>> {
    if profile.len() != VENDOR_PROFILE_LEN {
        return Err(bad_data("expected a 0x80e0-byte R1 Windows profile record"));
    }
    let span = VENDOR_BUTTONS..VENDOR_BUTTONS + VENDOR_BUTTON_STRIDE * SLOTS;
    Ok(profile[span].chunks_exact(VENDOR_BUTTON_STRIDE))
}

pub(crate) fn windows_action(
    id: u8,
    key: u8,
    modifiers: u8,
    macro_slot: u8,
) -> io::Result<[u8; 3]> {
    Ok(match id {
        0x10 => [0x11, modifiers, key],
        0x11 if macro_slot != 0 => [0x12, 0, macro_slot],
        0x13 => [0x13, 0, 0],
        0x73 => [0x14, 0, 0],
        0x15 => [0x2b, 0, 0],
        0x34 => [0x3c, 0, 0],
        id => PRESETS
            .iter()
            .find(|(_, _, candidate)| *candidate == id)
            .map(|(_, code, _)| *code)
            .ok_or_else(|| bad_data(format!("unknown or unassigned Windows action 0x{id:02x}")))?,
    })
}

pub fn buttons_from_vendor_profile(profile: &[u8]) -> io::Result<[u8; BUTTONS_LEN]> {
    let mut data = [0; 54];
    for (index, button) in vendor_button_records(profile)?.enumerate() {
        let wire = windows_action(button[1], button[3], button[7], (index + 1) as u8)?;
        data[index * 3..index * 3 + 3].copy_from_slice(&wire);
    }
    Ok(make_button_report(&data))
}

pub fn vendor_profile_macros(profile: &[u8]) -> io::Result<Vec<(u8, Macro)>> {
    let mut macros = Vec::new();
    for (index, button) in vendor_button_records(profile)?.enumerate() {
        if button[1] != 0x11 {
            continue;
        }
        let mut value = Macro {
            mode: decode_mode(button[0x0c])?,
            repeat: button[0x40].max(1),
            metadata: button[0x43c..0x43f].try_into().unwrap(),
            events: Vec::new(),
        };
        let count = u16::from_le_bytes([button[0x44], button[0x45]]);
        import_events(&mut value, &button[0x440..], count, VENDOR_MAX_EVENTS)?;
        macros.push(((index + 1) as u8, value));
    }
    Ok(macros)
}

type VendorAssignments = ([u8; BUTTONS_LEN], Vec<(u8, Macro)>);

pub fn from_vendor_profile(profile: &[u8]) -> io::Result<VendorAssignments> {
    Ok((
        buttons_from_vendor_profile(profile)?,
        vendor_profile_macros(profile)?,
    ))
}

pub(crate) fn write_vendor_profile(
    profile: &mut [u8],
    report: &[u8],
    macros: &[(u8, Macro)],
    previous: Option<(&[u8], &[(u8, Macro)])>,
) -> io::Result<()> {
    validate_button_assignments(report)?;
    if profile.len() != VENDOR_PROFILE_LEN {
        return Err(invalid("expected a 0x80e0-byte R1 Windows profile record"));
    }
    let mut used = 0;
    let span = VENDOR_BUTTONS..VENDOR_BUTTONS + VENDOR_BUTTON_STRIDE * SLOTS;
    for (index, entry) in profile[span]
        .chunks_exact_mut(VENDOR_BUTTON_STRIDE)
        .enumerate()
    {
        let button = index + 1;
        let offset = index * 3 + 3;
        let wire: [u8; 3] = report[offset..offset + 3].try_into().unwrap();
        let embedded = macros.iter().find(|(slot, _)| *slot == wire[2]);
        if wire[0] == 0x12 {
            used += usize::from(embedded.is_some());
        }
        if previous.is_some_and(|(old_report, old_macros)| {
            old_report[offset..offset + 3] == wire
                && (wire[0] != 0x12
                    || old_macros.iter().find(|(slot, _)| *slot == wire[2]) == embedded)
        }) {
            continue;
        }
        entry.fill(0);
        entry[0] = index as u8;
        entry[8..10].copy_from_slice(&0x0232u16.to_le_bytes());
        entry[0x430..0x438].fill(0xff);
        if let Some((_, _, id)) = PRESETS.iter().find(|(_, bytes, _)| *bytes == wire) {
            entry[1] = *id;
            continue;
        }
        entry[1] = match wire {
            [0x11, modifiers, key] => {
                entry[3] = key;
                entry[7] = modifiers;
                0x10
            }
            [0x12, 0, slot] if usize::from(slot) == button => {
                let (_, value) = embedded
                    .ok_or_else(|| invalid(format!("button {button} has no macro data")))?;
                let data = export_vendor_macro(&format!("Macro {slot}"), value)?;
                entry[0x0c] = value.mode as u8;
                entry[0x10..0x40].copy_from_slice(&data[..48]);
                entry[0x40] = value.repeat;
                entry[0x44..0x46].copy_from_slice(&data[52..54]);
                entry[0x43c..0x43f].copy_from_slice(&value.metadata);
                entry[0x440..0x4f8].copy_from_slice(&data[56..]);
                0x11
            }
            [0x12, _, slot] => {
                return Err(invalid(format!(
                    "the Windows profile requires button {button} to use its own macro slot, found {slot}"
                )));
            }
            [0x13, 0, 0] => 0x13,
            [0x14, 0, 0] => 0x73,
            [0x2b, 0, 0] => 0x15,
            [0x3c, 0, 0] => 0x34,
            _ => {
                return Err(invalid(format!(
                    "button {button} has no Windows profile encoding"
                )));
            }
        };
    }
    if used != macros.len() {
        return Err(invalid(
            "Windows profiles store macros embedded in their assigned buttons",
        ));
    }
    Ok(())
}

fn checksum(report: &mut [u8], end: usize) {
    let sum: u16 = report[3..end].iter().map(|byte| u16::from(*byte)).sum();
    report[end..end + 2].copy_from_slice(&sum.to_be_bytes());
}

fn key_code(name: &str, mouse: bool) -> io::Result<u8> {
    let name = name.to_ascii_lowercase();
    if let Some(hex) = name.strip_prefix("0x") {
        return u8::from_str_radix(hex, 16)
            .ok()
            .filter(|code| *code > 3)
            .ok_or_else(|| invalid("firmware key code must be between 0x04 and 0xff"));
    }
    if let [byte] = name.as_bytes() {
        match byte {
            b'a'..=b'z' => return Ok(4 + byte - b'a'),
            b'1'..=b'9' => return Ok(0x1e + byte - b'1'),
            b'0' => return Ok(0x27),
            _ => {}
        }
    }
    if let Some(number) = name
        .strip_prefix('f')
        .and_then(|text| text.parse::<u8>().ok())
    {
        match number {
            1..=12 => return Ok(0x39 + number),
            13..=24 => return Ok(0x68 + number - 13),
            _ => {}
        }
    }
    if let Some(number) = name
        .strip_prefix("kp")
        .and_then(|text| text.parse::<u8>().ok())
    {
        match number {
            0 => return Ok(0x62),
            1..=9 => return Ok(0x58 + number),
            _ => {}
        }
    }
    Ok(match name.as_str() {
        "enter" | "return" => 0x28,
        "esc" | "escape" => 0x29,
        "backspace" => 0x2a,
        "tab" => 0x2b,
        "space" => 0x2c,
        "minus" | "-" => 0x2d,
        "equal" | "=" => 0x2e,
        "left-bracket" | "[" => 0x2f,
        "right-bracket" | "]" => 0x30,
        "backslash" | "\\" => 0x31,
        "semicolon" | ";" => 0x33,
        "apostrophe" | "'" => 0x34,
        "grave" => 0x35,
        "comma" | "," => 0x36,
        "period" | "." => 0x37,
        "slash" | "/" => 0x38,
        "caps-lock" => 0x39,
        "print-screen" => 0x46,
        "scroll-lock" => 0x47,
        "pause" => 0x48,
        "insert" => 0x49,
        "home" => 0x4a,
        "page-up" => 0x4b,
        "delete" => 0x4c,
        "end" => 0x4d,
        "page-down" => 0x4e,
        "right" => 0x4f,
        "left" => 0x50,
        "down" => 0x51,
        "up" => 0x52,
        "num-lock" => 0x53,
        "kp-divide" => 0x54,
        "kp-multiply" => 0x55,
        "kp-minus" => 0x56,
        "kp-plus" => 0x57,
        "kp-enter" => 0x58,
        "kp-period" => 0x63,
        "menu" => 0x65,
        "left-ctrl" | "ctrl" => 0xe0,
        "left-shift" | "shift" => 0xe1,
        "left-alt" | "alt" => 0xe2,
        "left-super" | "super" | "meta" => 0xe3,
        "right-ctrl" => 0xe4,
        "right-shift" => 0xe5,
        "right-alt" => 0xe6,
        "right-super" => 0xe7,
        "mouse-left" if mouse => 0xf1,
        "mouse-right" if mouse => 0xf2,
        "mouse-middle" if mouse => 0xf3,
        "mouse-forward" if mouse => 0xf4,
        "mouse-back" if mouse => 0xf5,
        _ => return Err(invalid(format!("unknown key '{name}'"))),
    })
}

fn validate_macro(value: &Macro) -> io::Result<()> {
    if value.repeat == 0 {
        return Err(invalid("macro repeat must be between 1 and 255"));
    }
    if value.events.len() > MAX_EVENTS {
        return Err(invalid(format!(
            "a macro can contain at most {MAX_EVENTS} input events"
        )));
    }
    if value.events.iter().any(|event| match event.extended {
        Some(_) => {
            event.code != 0xf6 || event.pressed || event.delay_ms > 2550 || event.delay_ms % 10 != 0
        }
        None if event.code == 0xf6 => event.pressed || event.delay_ms > MAX_DELAY_MS,
        None => event.code <= 3 || event.code == 0xfe || event.delay_ms > MAX_DELAY_MS,
    }) {
        return Err(invalid(
            "macro contains a reserved key code or delay over 50000 ms",
        ));
    }
    Ok(())
}

pub fn parse_macro(source: &str) -> io::Result<Macro> {
    let mut result = Macro::default();
    for (number, line) in source.lines().enumerate() {
        let fields: Vec<_> = line
            .split('#')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .collect();
        let parsed = match fields.as_slice() {
            [] => Ok(()),
            ["mode", value] => parse_mode(value).map(|mode| result.mode = mode),
            ["repeat", value] => value
                .parse()
                .map(|repeat| result.repeat = repeat)
                .map_err(|_| invalid("expected repeat count")),
            ["metadata", value] => triplet(value).map(|bytes| result.metadata = bytes),
            [direction @ ("down" | "up" | "tap"), name] => {
                push_event(&mut result, direction, name, "10")
            }
            [direction @ ("down" | "up" | "tap"), name, delay] => {
                push_event(&mut result, direction, name, delay)
            }
            ["wait", delay] => add_wait(&mut result, delay),
            ["extended", delay, operands] => push_extended(&mut result, delay, operands),
            _ => Err(invalid(
                "expected mode, repeat, metadata, down, up, tap, wait, or extended",
            )),
        };
        parsed.map_err(|error| invalid(format!("macro line {}: {error}", number + 1)))?;
    }
    validate_macro(&result)?;
    Ok(result)
}

fn parse_mode(value: &str) -> io::Result<MacroMode> {
    match value {
        "times" | "once" => Ok(MacroMode::Times),
        "press-to-stop" | "toggle" => Ok(MacroMode::PressToStop),
        "hold" => Ok(MacroMode::Hold),
        _ => Err(invalid("expected mode times, press-to-stop, or hold")),
    }
}

pub fn edit_macro(value: &mut Macro, args: &[&str]) -> io::Result<()> {
    let position = |index: &str, maximum| {
        index
            .parse::<usize>()
            .ok()
            .filter(|index| (1..=maximum).contains(index))
            .map(|index| index - 1)
            .ok_or_else(|| invalid("macro event index is out of range"))
    };
    let mut edited = value.clone();
    match args {
        [operation @ ("insert" | "replace"), index, fields @ ..] => {
            let replace = usize::from(*operation == "replace");
            let index = position(index, edited.events.len() + 1 - replace)?;
            let mut input = Macro::default();
            match fields {
                [direction @ ("down" | "up" | "tap"), key] => {
                    push_event(&mut input, direction, key, "10")?
                }
                [direction @ ("down" | "up" | "tap"), key, delay] => {
                    push_event(&mut input, direction, key, delay)?
                }
                ["extended", delay, operands] => push_extended(&mut input, delay, operands)?,
                _ => return Err(invalid("expected a down, up, tap, or extended event")),
            }
            edited.events.splice(index..index + replace, input.events);
        }
        ["remove", index] => {
            let index = position(index, edited.events.len())?;
            edited.events.remove(index);
        }
        ["delay", index, milliseconds] => {
            let index = position(index, edited.events.len())?;
            edited.events[index].delay_ms = duration(milliseconds)?;
        }
        ["move", from, to] => {
            let count = edited.events.len();
            let from = position(from, count)?;
            let to = position(to, count)?;
            let event = edited.events.remove(from);
            edited.events.insert(to, event);
        }
        ["playback", mode, repeat] => {
            edited.mode = parse_mode(mode)?;
            edited.repeat = repeat
                .parse()
                .map_err(|_| invalid("invalid repeat count"))?;
        }
        _ => {
            return Err(invalid(
                "expected insert, replace, remove, move, delay, or playback",
            ));
        }
    }
    validate_macro(&edited)?;
    *value = edited;
    Ok(())
}

fn push_extended(value: &mut Macro, delay: &str, operands: &str) -> io::Result<()> {
    if operands == "unavailable" {
        value.events.push(MacroEvent {
            code: 0xf6,
            pressed: false,
            delay_ms: duration(delay)?,
            extended: None,
        });
        return Ok(());
    }
    if operands.len() != 8 || !operands.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(
            "extended requires exactly four hexadecimal operand bytes",
        ));
    }
    let raw =
        u32::from_str_radix(operands, 16).map_err(|_| invalid("invalid extended operands"))?;
    value.events.push(MacroEvent {
        code: 0xf6,
        pressed: false,
        delay_ms: duration(delay)?,
        extended: Some(raw.to_be_bytes()),
    });
    Ok(())
}

fn duration(value: &str) -> io::Result<u16> {
    value
        .parse()
        .ok()
        .filter(|delay| *delay <= MAX_DELAY_MS)
        .ok_or_else(|| invalid("delay must be between 0 and 50000 ms"))
}

fn push_event(result: &mut Macro, direction: &str, name: &str, delay: &str) -> io::Result<()> {
    let code = key_code(name, true)?;
    let delay_ms = duration(delay)?;
    let added = if direction == "tap" { 2 } else { 1 };
    if result.events.len() + added > MAX_EVENTS {
        return Err(invalid(format!(
            "a macro can contain at most {MAX_EVENTS} input events"
        )));
    }
    result.events.push(MacroEvent {
        extended: None,
        code,
        pressed: direction != "up",
        delay_ms,
    });
    if direction == "tap" {
        result.events.push(MacroEvent {
            extended: None,
            code,
            pressed: false,
            delay_ms,
        });
    }
    Ok(())
}

fn add_wait(result: &mut Macro, delay: &str) -> io::Result<()> {
    let delay = duration(delay)?;
    let previous = result
        .events
        .last_mut()
        .ok_or_else(|| invalid("wait needs a prior event"))?;
    previous.delay_ms = previous
        .delay_ms
        .checked_add(delay)
        .filter(|value| *value <= MAX_DELAY_MS)
        .ok_or_else(|| invalid("combined delay exceeds 50000 ms"))?;
    Ok(())
}

pub fn encode_macro(slot_number: u8, value: &Macro) -> io::Result<[u8; MACRO_LEN]> {
    slot(usize::from(slot_number))?;
    validate_macro(value)?;
    let mut report = [0; MACRO_LEN];
    report[..4].copy_from_slice(&[9, MACRO_LEN as u8, slot_number, value.mode as u8]);
    report[4..7].copy_from_slice(&value.metadata);
    report[7] = value.repeat;
    let mut entries = report[PAIRS_START..MACRO_CHECKSUM].chunks_exact_mut(2);
    let mut count = 0;
    for event in &value.events {
        if let Some(operands) = event.extended {
            let bytes = [
                (event.delay_ms / 10) as u8,
                0xf6,
                operands[0],
                operands[1],
                operands[2],
                operands[3],
            ];
            for pair in bytes.chunks_exact(2) {
                entries
                    .next()
                    .ok_or_else(|| invalid("macro exceeds 50 encoded steps"))?
                    .copy_from_slice(pair);
                count += 1;
            }
            continue;
        }
        if event.code == 0xf6 {
            return Err(invalid(
                "the Windows record discarded the four extended-event operand bytes",
            ));
        }
        let delay = event.delay_ms;
        let (short, long) = if delay >= 1271 {
            (delay % 200, delay / 200)
        } else {
            (delay, 0)
        };
        let rounded = ((short.max(1) + 5) / 10) as u8;
        let flags = if event.pressed { 0x01 } else { 0x81 };
        entries
            .next()
            .ok_or_else(|| invalid("macro exceeds 50 encoded steps"))?
            .copy_from_slice(&[flags | rounded, event.code]);
        count += 1;
        if long != 0 {
            entries
                .next()
                .ok_or_else(|| invalid("macro exceeds 50 encoded steps"))?
                .copy_from_slice(&[long as u8, 3]);
            count += 1;
        }
    }
    report[PAIR_COUNT] = count;
    checksum(&mut report, MACRO_CHECKSUM);
    Ok(report)
}

/// Split only at event boundaries. A continuation consumes one pair on each nonfinal page.
pub(crate) fn macro_page_events(value: &Macro) -> io::Result<Vec<&[MacroEvent]>> {
    validate_macro(value)?;
    let pairs = |event: &MacroEvent| {
        if event.extended.is_some() {
            3
        } else {
            1 + usize::from(event.delay_ms >= 1271)
        }
    };
    let mut remaining: usize = value.events.iter().map(pairs).sum();
    let mut pages = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (index, event) in value.events.iter().enumerate() {
        let size = pairs(event);
        if used + size > if remaining > 50 { 49 } else { 50 } {
            pages.push(&value.events[start..index]);
            remaining -= used;
            start = index;
            used = 0;
        }
        used += size;
    }
    pages.push(&value.events[start..]);
    Ok(pages)
}

pub(crate) fn encode_macro_continuation(
    slot: u8,
    value: &Macro,
    next: Option<u8>,
) -> io::Result<[u8; MACRO_LEN]> {
    let mut report = encode_macro(slot, value)?;
    if let Some(next) = next {
        let count = usize::from(report[PAIR_COUNT]);
        let start = PAIRS_START + count * 2;
        if start >= MACRO_CHECKSUM {
            return Err(invalid("macro page has no room for its continuation"));
        }
        report[start..start + 2].copy_from_slice(&[next, 0xfe]);
        report[PAIR_COUNT] += 1;
        checksum(&mut report, MACRO_CHECKSUM);
    }
    Ok(report)
}

pub fn decode_macro(report: &[u8], slot_number: u8) -> io::Result<Macro> {
    decode_macro_pages(&[report], slot_number)
}

fn macro_pairs(report: &[u8]) -> io::Result<&[u8]> {
    if report.len() != MACRO_LEN || report[..2] != [9, MACRO_LEN as u8] {
        return Err(bad_data("invalid R1 macro report header or length"));
    }
    if report[3..].iter().all(|&byte| byte == 0xff) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "empty device macro slot",
        ));
    }
    let count = usize::from(report[PAIR_COUNT]);
    if count > (MACRO_CHECKSUM - PAIRS_START) / 2 {
        return Err(bad_data("device returned too many macro steps"));
    }
    Ok(&report[PAIRS_START..PAIRS_START + count * 2])
}

pub fn macro_continues(report: &[u8]) -> io::Result<bool> {
    Ok(macro_continuation(report)?.is_some())
}

pub fn macro_continuation(report: &[u8]) -> io::Result<Option<u8>> {
    let mut pairs = macro_pairs(report)?.chunks_exact(2);
    while let Some(pair) = pairs.next() {
        match pair[1] {
            0xf6 if pairs.len() >= 2 => {
                pairs.next();
                pairs.next();
            }
            0xf6 => return Err(bad_data("truncated extended macro event")),
            0xfe if pairs.len() == 0 => return Ok(Some(pair[0])),
            0xfe => {
                return Err(bad_data(
                    "macro continuation marker appears before the last event",
                ));
            }
            _ => {}
        }
    }
    Ok(None)
}

pub fn validate_macro_page(report: &[u8]) -> io::Result<()> {
    macro_continuation(report)?;
    slot(usize::from(report[2]))?;
    let sum: u16 = report[3..MACRO_CHECKSUM]
        .iter()
        .copied()
        .map(u16::from)
        .sum();
    if report[MACRO_CHECKSUM..] != sum.to_be_bytes() {
        return Err(bad_data("macro page checksum mismatch"));
    }
    Ok(())
}

pub fn decode_macro_pages(reports: &[&[u8]], slot_number: u8) -> io::Result<Macro> {
    slot(usize::from(slot_number))?;
    let first = reports
        .first()
        .ok_or_else(|| bad_data("missing first macro report"))?;
    macro_pairs(first)?;
    if reports.len() > SLOTS {
        return Err(bad_data("macro continuation exceeds the available slots"));
    }
    let mut value = Macro {
        mode: decode_mode(first[3])?,
        repeat: first[7].max(1),
        metadata: first[4..7].try_into().unwrap(),
        events: Vec::new(),
    };
    for (index, report) in reports.iter().enumerate() {
        let pairs = macro_pairs(report)?;
        let continues = macro_continuation(report)?.is_some();
        if continues != (index + 1 < reports.len()) {
            return Err(bad_data(
                "R1 macro continuation does not match the supplied pages",
            ));
        }
        let end = pairs.len() - if continues { 2 } else { 0 };
        decode_macro_pairs(&mut value, &pairs[..end])?;
    }
    for event in &mut value.events {
        if (MAX_DELAY_MS + 1..=MAX_DELAY_MS + 10).contains(&event.delay_ms) {
            event.delay_ms = MAX_DELAY_MS;
        } else if event.delay_ms > MAX_DELAY_MS {
            return Err(bad_data("device macro delay exceeds 50000 ms"));
        }
    }
    Ok(value)
}

fn decode_macro_pairs(value: &mut Macro, pairs: &[u8]) -> io::Result<()> {
    let mut pairs = pairs.chunks_exact(2);
    while let Some(pair) = pairs.next() {
        let (flags, code) = (pair[0], pair[1]);
        match code {
            0xfe => {
                return Err(bad_data(
                    "macro continuation marker appears before the last pair",
                ));
            }
            0xf6 => {
                let low = pairs
                    .next()
                    .ok_or_else(|| bad_data("truncated extended macro event"))?;
                let high = pairs
                    .next()
                    .ok_or_else(|| bad_data("truncated extended macro event"))?;
                value.events.push(MacroEvent {
                    code,
                    pressed: false,
                    delay_ms: u16::from(flags) * 10,
                    extended: Some([low[0], low[1], high[0], high[1]]),
                });
            }
            0..=3 => {
                let previous: &mut MacroEvent = value
                    .events
                    .last_mut()
                    .ok_or_else(|| bad_data("device macro starts with a delay extension"))?;
                let unit = [200, 50, 100, 200][usize::from(code)];
                previous.delay_ms = previous
                    .delay_ms
                    .checked_add(u16::from(flags) * unit)
                    .ok_or_else(|| bad_data("device macro delay overflows"))?;
            }
            4..=255 => value.events.push(MacroEvent {
                extended: None,
                code,
                pressed: flags & 0x80 == 0,
                delay_ms: u16::from(flags & 0x7f) * 10,
            }),
        }
    }
    Ok(())
}

fn decode_mode(mode: u8) -> io::Result<MacroMode> {
    match mode {
        0 => Ok(MacroMode::Times),
        1 => Ok(MacroMode::PressToStop),
        2 => Ok(MacroMode::Hold),
        _ => Err(bad_data("unknown R1 macro playback mode")),
    }
}

pub fn macro_chunks(report: &[u8], receiver: bool) -> io::Result<[[u8; MACRO_CHUNK_LEN]; 3]> {
    if report.len() == MACRO_LEN {
        slot(usize::from(report[2]))?;
    }
    model_macro_chunks(report, receiver)
}

pub(crate) fn model_macro_chunks(
    report: &[u8],
    receiver: bool,
) -> io::Result<[[u8; MACRO_CHUNK_LEN]; 3]> {
    if report.len() != MACRO_LEN || report[..2] != [9, MACRO_LEN as u8] {
        return Err(invalid("expected a logical 131-byte R1 macro report"));
    }
    if report[2] == 0 {
        return Err(invalid("macro slot starts at 1"));
    }
    let mut output = [[0; MACRO_CHUNK_LEN]; 3];
    for (index, (chunk, payload)) in output.iter_mut().zip(report[3..].chunks(60)).enumerate() {
        let size = if receiver && index == 2 { 0x0c } else { 0x40 };
        chunk[..4].copy_from_slice(&[9, size, report[2], index as u8]);
        chunk[4..4 + payload.len()].copy_from_slice(payload);
    }
    Ok(output)
}

#[derive(Clone, Copy)]
pub enum WindowsMacroLayout {
    Compact,
    Extended,
}

impl WindowsMacroLayout {
    pub fn record_len(self) -> usize {
        match self {
            Self::Compact => VENDOR_MACRO_LEN,
            Self::Extended => 0xc18,
        }
    }

    fn max_events(self) -> usize {
        (self.record_len() - 56) / 4
    }
}

pub fn import_vendor_macros(source: &[u8]) -> io::Result<Vec<(String, Macro)>> {
    import_windows_macros(source, WindowsMacroLayout::Compact)
}

pub fn import_windows_macros(
    source: &[u8],
    layout: WindowsMacroLayout,
) -> io::Result<Vec<(String, Macro)>> {
    let records = source.chunks_exact(layout.record_len());
    if !records.remainder().is_empty() {
        return Err(bad_data(
            "Windows macro library has a partial record for this model",
        ));
    }
    records
        .map(|record| {
            let name = record[..48]
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .take_while(|unit| *unit != 0)
                .collect::<Vec<_>>();
            let name =
                String::from_utf16(&name).map_err(|_| bad_data("invalid vendor macro name"))?;
            let count = u16::try_from(u32::from_le_bytes(record[52..56].try_into().unwrap()))
                .map_err(|_| bad_data("Windows macro event count is out of range"))?;
            let mut value = Macro::default();
            import_events(&mut value, &record[56..], count, layout.max_events())?;
            Ok((name, value))
        })
        .collect()
}

fn import_events(value: &mut Macro, records: &[u8], count: u16, limit: usize) -> io::Result<()> {
    let count = usize::from(count);
    let bytes = records
        .get(..count * 4)
        .filter(|_| count <= limit)
        .ok_or_else(|| bad_data("vendor macro record contains an invalid event count"))?;
    for event in bytes.chunks_exact(4) {
        let pressed = match event[1] {
            1 => true,
            2 => false,
            0 if event[0] == 0xf6 => false,
            _ => return Err(bad_data("vendor macro uses an unsupported event kind")),
        };
        value.events.push(MacroEvent {
            extended: None,
            code: event[0],
            pressed,
            delay_ms: u16::from_le_bytes([event[2], event[3]]),
        });
    }
    validate_macro(value).map_err(|error| bad_data(error.to_string()))
}

pub fn export_vendor_macro(name: &str, value: &Macro) -> io::Result<[u8; VENDOR_MACRO_LEN]> {
    let mut record = [0; VENDOR_MACRO_LEN];
    encode_windows_macro(name, value, &mut record)?;
    Ok(record)
}

pub fn export_windows_macro(
    name: &str,
    value: &Macro,
    layout: WindowsMacroLayout,
) -> io::Result<Vec<u8>> {
    let mut record = vec![0; layout.record_len()];
    encode_windows_macro(name, value, &mut record)?;
    Ok(record)
}

fn encode_windows_macro(name: &str, value: &Macro, record: &mut [u8]) -> io::Result<()> {
    validate_macro(value)?;
    let limit = (record.len() - 56) / 4;
    if value.events.len() > limit {
        return Err(invalid(format!(
            "this Windows macro record holds at most {limit} events"
        )));
    }
    if value.events.iter().any(|event| event.extended.is_some()) {
        return Err(invalid(
            "the Windows macro record cannot preserve extended operands",
        ));
    }
    let units: Vec<_> = name.encode_utf16().collect();
    if units.is_empty() || units.len() > 23 || units.contains(&0) {
        return Err(invalid(
            "vendor macro name must contain 1–23 UTF-16 code units",
        ));
    }
    for (entry, unit) in record[..48].chunks_exact_mut(2).zip(units) {
        entry.copy_from_slice(&unit.to_le_bytes());
    }
    record[52..56].copy_from_slice(&(value.events.len() as u32).to_le_bytes());
    for (entry, event) in record[56..].chunks_exact_mut(4).zip(&value.events) {
        let [low, high] = event.delay_ms.to_le_bytes();
        let kind = if event.code == 0xf6 {
            0
        } else if event.pressed {
            1
        } else {
            2
        };
        entry.copy_from_slice(&[event.code, kind, low, high]);
    }
    Ok(())
}

const EVDEV_WORD: usize = size_of::<isize>();
const EVDEV_TIME: usize = 2 * EVDEV_WORD;
type EvdevEvent = [u8; EVDEV_TIME + 8];

fn evdev_timestamp(bytes: &EvdevEvent) -> i128 {
    let seconds = isize::from_ne_bytes(bytes[..EVDEV_WORD].try_into().unwrap());
    let micros = isize::from_ne_bytes(bytes[EVDEV_WORD..EVDEV_TIME].try_into().unwrap());
    seconds as i128 * 1_000_000 + micros as i128
}

fn evdev_key(code: u16) -> Option<u8> {
    let code = match code {
        2..=10 => 0x1e + code - 2,
        11 => 0x27,
        16..=25 => u16::from(b"qwertyuiop"[usize::from(code - 16)] - b'a') + 4,
        30..=38 => u16::from(b"asdfghjkl"[usize::from(code - 30)] - b'a') + 4,
        44..=50 => u16::from(b"zxcvbnm"[usize::from(code - 44)] - b'a') + 4,
        59..=68 => 0x3a + code - 59,
        183..=194 => 0x68 + code - 183,
        1 => 0x29,
        12 => 0x2d,
        13 => 0x2e,
        14 => 0x2a,
        15 => 0x2b,
        26 => 0x2f,
        27 => 0x30,
        28 => 0x28,
        29 => 0xe0,
        39 => 0x33,
        40 => 0x34,
        41 => 0x35,
        42 => 0xe1,
        43 => 0x31,
        51 => 0x36,
        52 => 0x37,
        53 => 0x38,
        54 => 0xe5,
        55 => 0x55,
        56 => 0xe2,
        57 => 0x2c,
        58 => 0x39,
        69 => 0x53,
        70 => 0x47,
        71..=73 => 0x5f + code - 71,
        74 => 0x56,
        75..=77 => 0x5c + code - 75,
        78 => 0x57,
        79..=81 => 0x59 + code - 79,
        82 => 0x62,
        83 => 0x63,
        87 => 0x44,
        88 => 0x45,
        96 => 0x58,
        97 => 0xe4,
        98 => 0x54,
        99 => 0x46,
        100 => 0xe6,
        102 => 0x4a,
        103 => 0x52,
        104 => 0x4b,
        105 => 0x50,
        106 => 0x4f,
        107 => 0x4d,
        108 => 0x51,
        109 => 0x4e,
        110 => 0x49,
        111 => 0x4c,
        119 => 0x48,
        125 => 0xe3,
        126 => 0xe7,
        127 => 0x65,
        0x110 => 0xf1,
        0x111 => 0xf2,
        0x112 => 0xf3,
        0x113 | 0x116 => 0xf5,
        0x114 | 0x115 => 0xf4,
        _ => return None,
    };
    u8::try_from(code).ok()
}

fn record_with(
    count: usize,
    mut read_event: impl FnMut(&mut EvdevEvent) -> io::Result<()>,
) -> io::Result<Macro> {
    if count == 0 || count > MAX_EVENTS {
        return Err(invalid(format!(
            "record between 1 and {MAX_EVENTS} input events"
        )));
    }
    let mut value = Macro::default();
    let mut last_timestamp = 0i128;
    let mut bytes = [0; EVDEV_TIME + 8];
    while value.events.len() < count {
        if let Err(error) = read_event(&mut bytes) {
            if error.kind() == io::ErrorKind::TimedOut && !value.events.is_empty() {
                break;
            }
            return Err(error);
        }
        let kind = u16::from_ne_bytes(bytes[EVDEV_TIME..EVDEV_TIME + 2].try_into().unwrap());
        let key = u16::from_ne_bytes(bytes[EVDEV_TIME + 2..EVDEV_TIME + 4].try_into().unwrap());
        let state = i32::from_ne_bytes(bytes[EVDEV_TIME + 4..].try_into().unwrap());
        if (kind, key) == (0, 3) {
            return Err(bad_data(
                "the evdev event queue overflowed during recording",
            ));
        }
        if kind != 1 || !matches!(state, 0 | 1) {
            continue;
        }
        let Some(code) = evdev_key(key) else { continue };
        let timestamp = evdev_timestamp(&bytes);
        if let Some(event) = value.events.last_mut() {
            event.delay_ms =
                ((timestamp - last_timestamp) / 1000).clamp(0, i128::from(MAX_DELAY_MS)) as u16;
        }
        value.events.push(MacroEvent {
            extended: None,
            code,
            pressed: state == 1,
            delay_ms: DEFAULT_DELAY_MS,
        });
        last_timestamp = timestamp;
    }
    Ok(value)
}

#[cfg(test)]
fn record_events<R: Read>(mut reader: R, count: usize) -> io::Result<Macro> {
    record_with(count, |bytes| reader.read_exact(bytes))
}

#[cfg(target_os = "linux")]
pub fn record_path(
    path: &std::path::Path,
    count: usize,
    timeout: std::time::Duration,
) -> io::Result<Macro> {
    record_path_with_delay(path, count, timeout, None)
}

#[cfg(target_os = "linux")]
pub fn record_path_with_delay(
    path: &std::path::Path,
    count: usize,
    timeout: std::time::Duration,
    fixed_delay: Option<u16>,
) -> io::Result<Macro> {
    record_paths(&[path], count, timeout, fixed_delay)
}

#[cfg(target_os = "linux")]
pub fn record_paths(
    paths: &[&std::path::Path],
    count: usize,
    timeout: std::time::Duration,
    fixed_delay: Option<u16>,
) -> io::Result<Macro> {
    record_paths_until(paths, count, timeout, fixed_delay, None)
}

#[cfg(target_os = "linux")]
pub fn record_paths_until(
    paths: &[&std::path::Path],
    count: usize,
    timeout: std::time::Duration,
    fixed_delay: Option<u16>,
    stop: Option<std::os::fd::BorrowedFd<'_>>,
) -> io::Result<Macro> {
    use std::{collections::HashSet, os::fd::AsRawFd, time::Instant};
    if paths.is_empty() {
        return Err(invalid("select at least one evdev input device"));
    }
    if fixed_delay.is_some_and(|delay| delay > MAX_DELAY_MS) {
        return Err(invalid("recording delay must be between 0 and 50000 ms"));
    }
    if count == 0
        || count > MAX_EVENTS
        || timeout.is_zero()
        || timeout > std::time::Duration::from_secs(60)
    {
        return Err(invalid(format!(
            "recording needs 1–{MAX_EVENTS} input events and a timeout up to 60 seconds"
        )));
    }
    let mut device_ids = HashSet::with_capacity(paths.len());
    let mut inputs = Vec::with_capacity(paths.len());
    for path in paths {
        let (file, device_id) = open_record_device(path)?;
        if !device_ids.insert(device_id) {
            return Err(invalid("the same evdev input device was selected twice"));
        }
        inputs.push(file);
    }
    let deadline = Instant::now() + timeout;
    let mut pending = vec![None; inputs.len()];
    let mut poll_fds: Vec<_> = inputs
        .iter()
        .map(|file| libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    if let Some(stop) = stop {
        poll_fds.push(libc::pollfd {
            fd: stop.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }
    let mut captured = record_with(count, |bytes| {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "macro recording timed out",
                ));
            }
            let millis = if pending.iter().any(Option::is_some) {
                0
            } else {
                remaining.as_millis().saturating_add(1) as i32
            };
            for ((fd, file), buffered) in poll_fds.iter_mut().zip(&inputs).zip(&pending) {
                fd.fd = if buffered.is_some() {
                    -1
                } else {
                    file.as_raw_fd()
                };
            }
            // Each active descriptor is owned by a File in inputs for the whole poll.
            let result = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, millis) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
                continue;
            }
            if stop.is_some() && poll_fds[inputs.len()].revents != 0 {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "recording stopped"));
            }
            for ((fd, file), buffered) in poll_fds.iter().zip(&mut inputs).zip(&mut pending) {
                if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "an evdev input disconnected",
                    ));
                }
                if fd.revents & libc::POLLIN == 0 {
                    continue;
                }
                let mut event = [0; EVDEV_TIME + 8];
                match file.read_exact(&mut event) {
                    Ok(()) => *buffered = Some(event),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(error) => return Err(error),
                }
            }
            if let Some(event) = take_earliest(&mut pending) {
                *bytes = event;
                return Ok(());
            }
        }
    })?;
    balance_recording(&mut captured)?;
    if let (Some(delay), Some((_, preceding))) = (fixed_delay, captured.events.split_last_mut()) {
        for event in preceding {
            event.delay_ms = delay;
        }
    }
    Ok(captured)
}

fn take_earliest(pending: &mut [Option<EvdevEvent>]) -> Option<EvdevEvent> {
    let index = pending
        .iter()
        .enumerate()
        .filter_map(|(index, event)| event.as_ref().map(|event| (evdev_timestamp(event), index)))
        .min()?
        .1;
    pending[index].take()
}

#[cfg(target_os = "linux")]
fn open_record_device(path: &std::path::Path) -> io::Result<(std::fs::File, u64)> {
    use std::{
        fs::OpenOptions,
        os::fd::AsRawFd,
        os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    };
    const EVIOCGVERSION: libc::c_ulong = 0x8004_4501;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_char_device() {
        return Err(invalid(
            "macro recording requires an evdev character device",
        ));
    }
    let mut version: libc::c_int = 0;
    // The evdev version ioctl writes one C int to the supplied live buffer.
    if unsafe { libc::ioctl(file.as_raw_fd(), EVIOCGVERSION, &mut version) } < 0 {
        return Err(invalid(
            "this device does not expose the Linux evdev interface",
        ));
    }
    Ok((file, metadata.rdev()))
}

fn balance_recording(value: &mut Macro) -> io::Result<()> {
    let mut pressed = Vec::new();
    for event in &value.events {
        if let Some(index) = pressed.iter().position(|code| *code == event.code) {
            if !event.pressed {
                pressed.remove(index);
            }
        } else if event.pressed {
            pressed.push(event.code);
        }
    }
    if value.events.len() + pressed.len() > MAX_EVENTS {
        return Err(invalid(
            "the recording ended with held keys; record fewer events or release them sooner",
        ));
    }
    for code in pressed.into_iter().rev() {
        value.events.push(MacroEvent {
            extended: None,
            code,
            pressed: false,
            delay_ms: DEFAULT_DELAY_MS,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_map_preserves_other_slots() {
        let mut report = default_buttons();
        let original = report;
        assert_eq!(parse_button("forward").unwrap(), 7);
        assert_eq!(
            button_action(&report, 4).unwrap(),
            Action::Preset("dpi-cycle")
        );
        assert!(set_button_action(&mut report, 1, Action::Preset("disabled")).is_err());
        assert_eq!(report, original);
        set_button_action(&mut report, 7, parse_action("key:Ctrl+Shift+A").unwrap()).unwrap();
        assert_eq!(&report[21..24], &[0x11, 3, 4]);
        assert_eq!(&report[..21], &original[..21]);
        assert_eq!(&report[24..57], &original[24..57]);
        let mut check = report;
        checksum(&mut check, 57);
        assert_eq!(check, report);
    }

    #[test]
    fn recognizes_wire_wrapped_erased_macro_storage() {
        let mut page = [0xff; MACRO_LEN];
        page[..3].copy_from_slice(&[9, MACRO_LEN as u8, 1]);
        assert_eq!(
            decode_macro(&page, 1).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        page[3] = 0;
        assert_eq!(
            decode_macro(&page, 1).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn macro_matches_vendor_offsets_and_chunks() {
        let value = parse_macro("mode hold\nrepeat 2\ndown a 10\nup a 1300").unwrap();
        let report = encode_macro(7, &value).unwrap();
        assert_eq!(&report[..8], &[9, 131, 7, 2, 0, 0, 0, 2]);
        assert_eq!(&report[28..35], &[3, 1, 4, 0x8b, 4, 6, 3]);
        assert_eq!(&report[129..], &[0, 164]);
        let blocks = macro_chunks(&report, true).unwrap();
        assert_eq!(&blocks[0][..4], &[9, 64, 7, 0]);
        assert_eq!(&blocks[2][..4], &[9, 12, 7, 2]);
        assert_eq!(&blocks[2][4..12], &report[123..]);
        assert!(blocks[2][12..].iter().all(|byte| *byte == 0));
        assert_eq!(decode_macro(&report, 7).unwrap().events[1].delay_ms, 1310);
    }

    #[test]
    fn overlong_macro_fails_before_framing() {
        let source = "tap a 1300\n".repeat(13);
        let value = parse_macro(&source).unwrap();
        assert!(encode_macro(4, &value).is_err());
        assert!(parse_macro(&"tap a\n".repeat(MAX_EVENTS / 2 + 1)).is_err());
    }

    #[test]
    fn local_and_vendor_macro_text_round_trip() {
        let value = parse_macro("tap a 10\nwait 25\n").unwrap();
        assert_eq!(parse_macro(&value.to_string()).unwrap(), value);
        let record = export_vendor_macro("Test Δ", &value).unwrap();
        assert_eq!(
            import_vendor_macros(&record).unwrap(),
            vec![("Test Δ".into(), value)]
        );
        assert!(import_vendor_macros(&record[..239]).is_err());
    }

    #[test]
    fn maps_first_run_windows_slot_layout_and_combined_report() {
        let ids = [
            1, 2, 3, 6, 0x34, 8, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0x0b, 0x0c,
        ];
        let mut record = vec![0; VENDOR_PROFILE_LEN];
        for (index, id) in ids.into_iter().enumerate() {
            record[VENDOR_BUTTONS + index * VENDOR_BUTTON_STRIDE + 1] = id;
        }
        let expected = default_buttons();
        assert_eq!(buttons_from_vendor_profile(&record).unwrap(), expected);

        let mut combined = [0; 128];
        combined[..2].copy_from_slice(&[0x0a, 0x80]);
        combined[65..119].copy_from_slice(&expected[3..57]);
        combined[119..121].fill(255);
        assert_eq!(buttons_from_legacy(&combined).unwrap(), expected);
        assert!(buttons_from_legacy(&combined[..127]).is_err());

        let macro_slot = VENDOR_BUTTONS + 6 * VENDOR_BUTTON_STRIDE;
        record[macro_slot + 1] = 0x11;
        record[macro_slot + 0x0c] = 2;
        record[macro_slot + 0x44] = 1;
        record[macro_slot + 0x440..macro_slot + 0x444].copy_from_slice(&[4, 1, 10, 0]);
        assert_eq!(
            button_action(&buttons_from_vendor_profile(&record).unwrap(), 7).unwrap(),
            Action::Macro { slot: 7 }
        );
        let macros = vendor_profile_macros(&record).unwrap();
        assert_eq!(macros, vec![(7, parse_macro("mode hold\ndown a").unwrap())]);
    }

    #[test]
    fn follows_only_final_continuation_markers() {
        let first_value = parse_macro("down a").unwrap();
        let last_value = parse_macro("up a 50000").unwrap();
        let mut first = encode_macro(7, &first_value).unwrap();
        first[28] = 2;
        first[31..33].copy_from_slice(&[1, 0xfe]);
        checksum(&mut first, MACRO_CHECKSUM);
        let last = encode_macro(8, &last_value).unwrap();

        assert!(macro_continues(&first).unwrap());
        assert!(!macro_continues(&last).unwrap());
        assert!(decode_macro(&first, 7).is_err());
        let combined = decode_macro_pages(&[&first, &last], 7).unwrap();
        assert_eq!(
            combined.events,
            [first_value.events[0], last_value.events[0]]
        );
        assert!(decode_macro_pages(&[&first, &last], 19).is_err());
        assert_eq!(macro_continuation(&first).unwrap(), Some(1));
    }

    #[test]
    fn evdev_records_monotonic_delays_and_ignores_repeat() {
        let mut bytes = Vec::new();
        for (micros, state) in [(1000isize, 1i32), (21_000, 2), (51_000, 0)] {
            bytes.extend(1isize.to_ne_bytes());
            bytes.extend(micros.to_ne_bytes());
            bytes.extend(1u16.to_ne_bytes());
            bytes.extend(30u16.to_ne_bytes());
            bytes.extend(state.to_ne_bytes());
        }
        let recorded = record_events(bytes.as_slice(), 2).unwrap();
        assert_eq!(
            recorded.events[0],
            MacroEvent {
                extended: None,
                code: 4,
                pressed: true,
                delay_ms: 50
            }
        );
        assert_eq!(
            recorded.events[1],
            MacroEvent {
                extended: None,
                code: 4,
                pressed: false,
                delay_ms: 10
            }
        );
    }

    #[test]
    fn evdev_merges_sources_by_timestamp_and_rejects_queue_overflow() {
        let event = |micros: isize| {
            let mut result = [0; EVDEV_TIME + 8];
            result[EVDEV_WORD..EVDEV_TIME].copy_from_slice(&micros.to_ne_bytes());
            result
        };
        let mut pending = [Some(event(51_000)), Some(event(1000)), Some(event(19_000))];
        for micros in [1000, 19_000, 51_000] {
            assert_eq!(take_earliest(&mut pending), Some(event(micros)));
        }
        assert!(take_earliest(&mut pending).is_none());
        let mut dropped = event(0);
        dropped[EVDEV_TIME + 2..EVDEV_TIME + 4].copy_from_slice(&3u16.to_ne_bytes());
        assert_eq!(
            record_events(dropped.as_slice(), 1).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
