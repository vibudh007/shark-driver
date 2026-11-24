//! R1 feature report values and byte-preserving read/modify/write codecs.

use std::io::{self, ErrorKind};
use std::ops::RangeInclusive;

pub const DPI_LEN: usize = 56;
pub const TIMING_LEN: usize = 15;
pub const POLLING_LEN: usize = 9;
pub const VERSION_LEN: usize = 8;
pub const LEGACY_LEN: usize = 128;
pub const STAGES: u8 = 8;
pub const VENDOR_PROFILE_LEN: usize = 0x80e0;

// Measured register values in the vendor's ascending lookup table at 0x004e50a8.
// The two higher UI ranges reuse subranges of this table with a bank or selector bit.
pub(crate) const DPI_CODES: [u8; 200] = [
    1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 14, 15, 16, 17, 18, 19, 21, 22, 23, 24, 25, 27, 28, 29, 30,
    31, 32, 34, 35, 36, 37, 38, 39, 41, 42, 43, 44, 45, 47, 48, 49, 50, 51, 52, 54, 55, 56, 57, 58,
    59, 61, 62, 63, 64, 65, 67, 68, 69, 70, 71, 72, 74, 75, 76, 77, 78, 79, 81, 82, 83, 84, 85, 87,
    88, 89, 90, 91, 92, 94, 95, 96, 97, 98, 99, 101, 102, 103, 104, 105, 107, 108, 109, 110, 111,
    112, 114, 115, 116, 117, 118, 119, 121, 122, 123, 124, 125, 127, 128, 129, 130, 131, 132, 134,
    135, 136, 137, 138, 139, 141, 142, 143, 144, 145, 147, 148, 149, 150, 151, 152, 154, 155, 156,
    157, 158, 159, 161, 162, 163, 164, 165, 167, 168, 169, 170, 171, 172, 174, 175, 176, 177, 178,
    179, 181, 182, 183, 184, 185, 187, 188, 189, 190, 191, 192, 194, 195, 196, 197, 198, 199, 201,
    202, 203, 204, 205, 207, 208, 209, 210, 211, 212, 214, 215, 216, 217, 218, 219, 221, 222, 223,
    224, 225, 227, 228, 229, 230, 231, 232, 234, 235,
];

fn report<const N: usize>(data: &[u8], id: u8) -> io::Result<[u8; N]> {
    let bytes: [u8; N] = data.try_into().map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("report 0x{id:02x} needs {N} bytes, received {}", data.len()),
        )
    })?;
    if bytes.first() != Some(&id) {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("unexpected report ID, expected 0x{id:02x}"),
        ));
    }
    Ok(bytes)
}

fn split_report<const N: usize>(data: &[u8], id: u8) -> io::Result<[u8; N]> {
    let bytes = report::<N>(data, id)?;
    if bytes.get(1).copied().map(usize::from) != Some(N) {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("report 0x{id:02x} has an invalid embedded length"),
        ));
    }
    Ok(bytes)
}

pub(crate) fn stamp_checksum(bytes: &mut [u8], checksum_at: usize) {
    let sum: u16 = bytes[3..checksum_at].iter().copied().map(u16::from).sum();
    bytes[checksum_at..checksum_at + 2].copy_from_slice(&sum.to_be_bytes());
}

fn stage_index(stage: u8) -> io::Result<usize> {
    if (1..=STAGES).contains(&stage) {
        Ok(usize::from(stage - 1))
    } else {
        Err(io::Error::new(
            ErrorKind::InvalidInput,
            "DPI stage must be 1 through 8",
        ))
    }
}

fn dpi_encoding(dpi: u16) -> io::Result<(u8, bool, bool)> {
    match dpi {
        50..=10_000 if dpi % 50 == 0 => Ok((DPI_CODES[usize::from(dpi / 50 - 1)], false, false)),
        10_100..=18_000 if dpi % 100 == 0 => {
            let banked = dpi > 12_000;
            Ok((DPI_CODES[usize::from(dpi / 100 - 1)], banked, !banked))
        }
        _ => Err(io::Error::new(
            ErrorKind::InvalidInput,
            "DPI must be 50–10000 in steps of 50 or 10100–18000 in steps of 100",
        )),
    }
}

fn decode_dpi(code: u8, banked: bool, selector: u8) -> Option<u16> {
    let index = u16::try_from(DPI_CODES.binary_search(&code).ok()?).ok()? + 1;
    match (banked, selector, index) {
        (false, 0, _) => Some(index * 50),
        (false, 1, 101..=120) | (true, 0, 121..=180) => Some(index * 100),
        _ => None,
    }
}

fn vendor_record(data: &[u8]) -> io::Result<&[u8; VENDOR_PROFILE_LEN]> {
    data.try_into().map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidData,
            "Windows R1 profile record must be exactly 0x80e0 bytes",
        )
    })
}

fn vendor_field(
    record: &[u8; VENDOR_PROFILE_LEN],
    offset: usize,
    range: RangeInclusive<u16>,
    field: &str,
) -> io::Result<u16> {
    let word = u32::from_le_bytes(std::array::from_fn(|i| record[offset + i]));
    u16::try_from(word)
        .ok()
        .filter(|value| range.contains(value))
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("Windows R1 profile has invalid {field}: {word}"),
            )
        })
}

pub(crate) fn put_vendor_word(record: &mut [u8], offset: usize, value: u32) {
    record[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DpiStage {
    pub enabled: bool,
    pub dpi: Option<u16>,
    pub color: [u8; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DpiLayout {
    Lookup { max: u16 },
    ExtendedLookup,
    Linear,
    MaxLinear,
    RawLinear,
}

impl DpiLayout {
    fn decode(self, low: u8, high: u8, banked: bool, extended: bool) -> Option<u16> {
        if matches!(self, Self::Linear | Self::MaxLinear | Self::RawLinear) {
            let banked = banked && self != Self::RawLinear;
            let (boundary, max) = self.linear_bounds();
            let index = u16::from_le_bytes([low, high]).checked_add(1)?;
            let dpi = index.checked_mul(if banked { 100 } else { 50 })?;
            let allowed = if banked {
                boundary + 100..=max
            } else {
                50..=boundary
            };
            return allowed.contains(&dpi).then_some(dpi);
        }
        if self == Self::ExtendedLookup && extended {
            let index = DPI_CODES.binary_search(&low).ok()? as u16 + 1;
            return (banked && high == 0 && index >= 161)
                .then_some(((index * 150).min(25_000) / 100) * 100);
        }
        if banked && high == 1 {
            // The selector table repeats codes from the low range, with 200 DPI steps.
            let index = DPI_CODES[..120].binary_search(&low).ok()? as u16 + 1;
            let ceiling = if let Self::Lookup { max } = self {
                max
            } else {
                24_000
            };
            let dpi = index * 200;
            return (dpi > 20_000 && dpi <= ceiling).then_some(dpi);
        }
        decode_dpi(low, banked, high).or_else(|| {
            let index = DPI_CODES.binary_search(&low).ok()? as u16 + 1;
            (banked && high == 0 && (181..=200).contains(&index)).then_some(index * 100)
        })
    }

    fn linear_bounds(self) -> (u16, u16) {
        match self {
            Self::MaxLinear => (30_000, 42_000),
            Self::RawLinear => (26_000, 26_000),
            _ => (26_000, 40_000),
        }
    }

    fn encode(self, dpi: u16) -> io::Result<(u8, u8, bool, bool)> {
        let invalid = || {
            io::Error::new(
                ErrorKind::InvalidInput,
                "DPI is outside this mouse's supported steps",
            )
        };
        if matches!(self, Self::Linear | Self::MaxLinear | Self::RawLinear) {
            let (boundary, max) = self.linear_bounds();
            let banked = dpi > boundary;
            let step = if banked { 100 } else { 50 };
            if !(50..=max).contains(&dpi) || dpi % step != 0 {
                return Err(invalid());
            }
            let [low, high] = (dpi / step - 1).to_le_bytes();
            return Ok((low, high, banked, false));
        }
        if self == Self::ExtendedLookup && (24_100..=25_000).contains(&dpi) && dpi % 100 == 0 {
            let index = usize::from(dpi / 150);
            let low = if dpi == 25_000 { 199 } else { DPI_CODES[index] };
            if self.decode(low, 0, true, true) == Some(dpi) {
                return Ok((low, 0, true, true));
            }
            return Err(invalid());
        }
        let ceiling = if let Self::Lookup { max } = self {
            max
        } else {
            24_000
        };
        if (20_200..=ceiling).contains(&dpi) && dpi % 200 == 0 {
            return Ok((DPI_CODES[usize::from(dpi / 200 - 1)], 1, true, false));
        }
        if (18_100..=20_000).contains(&dpi) && dpi % 100 == 0 {
            return Ok((DPI_CODES[usize::from(dpi / 100 - 1)], 0, true, false));
        }
        let (low, banked, selector) = dpi_encoding(dpi)?;
        Ok((low, u8::from(selector), banked, false))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dpi([u8; DPI_LEN], Option<DpiLayout>);

impl Default for Dpi {
    fn default() -> Self {
        let mut bytes = [0; DPI_LEN];
        bytes[..8].copy_from_slice(&[0x04, 56, 1, 0, 1, 0x3f, 0x20, 0x20]);
        bytes[8..14].copy_from_slice(&[18, 37, 56, 75, 117, 211]);
        bytes[24] = 2;
        bytes[25..49].copy_from_slice(&[
            255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0, 0, 255, 255, 255, 0, 255, 255, 64, 0,
            255, 255, 255,
        ]);
        let mut settings = Self(bytes, None);
        settings.finish_edit();
        settings
    }
}

impl Dpi {
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        split_report(data, 0x04).map(|bytes| Self(bytes, None))
    }

    pub(crate) fn parse_model(data: &[u8], layout: DpiLayout) -> io::Result<Self> {
        split_report(data, 0x04).map(|bytes| Self(bytes, Some(layout)))
    }

    pub fn from_vendor_profile(data: &[u8]) -> io::Result<Self> {
        let record = vendor_record(data)?;
        let mut dpi = Self([0; DPI_LEN], None);
        dpi.0[..2].copy_from_slice(&[0x04, 56]);
        dpi.0[3] = vendor_field(record, 0x944, 0..=1, "angle snap")? as u8;
        dpi.0[4] = vendor_field(record, 0x940, 0..=1, "ripple control")? as u8;
        for position in 0..usize::from(STAGES) {
            let index = vendor_field(record, 0x8c4 + position * 4, 0..=280, "DPI index")?;
            let enabled = vendor_field(record, 0x904 + position * 4, 0..=1, "enabled stage")?;
            if index == 0 && enabled != 0 {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "Windows R1 profile enables a stage without a DPI value",
                ));
            }
            if index > 0 {
                let value = if index <= 200 {
                    index * 50
                } else {
                    (index - 100) * 100
                };
                dpi.write_stage_dpi(position, value)?;
            }
            dpi.0[5] |= (enabled as u8) << position;
            let field = 0x960 + position * 4;
            let rgb = 25 + position * 3;
            dpi.0[rgb..rgb + 3].copy_from_slice(&record[field..field + 3]);
        }
        let active = vendor_field(record, 0x934, 0..=7, "active stage")? as u8 + 1;
        dpi.set_active_stage(active)
            .map_err(|err| io::Error::new(ErrorKind::InvalidData, err))?;
        Ok(dpi)
    }

    pub fn bytes(&self) -> &[u8; DPI_LEN] {
        &self.0
    }

    pub(crate) fn write_vendor_profile(&self, record: &mut [u8]) -> io::Result<()> {
        vendor_record(record)?;
        self.stage(self.active_stage())?;
        put_vendor_word(record, 0x934, u32::from(self.active_stage() - 1));
        put_vendor_word(record, 0x944, u32::from(self.0[3]));
        put_vendor_word(record, 0x940, u32::from(self.0[4]));
        for position in 0..usize::from(STAGES) {
            let stage = self.stage(position as u8 + 1)?;
            let index = match stage.dpi {
                Some(dpi) if dpi <= 10_000 => dpi / 50,
                Some(dpi) => dpi / 100 + 100,
                None => 0,
            };
            for base in [0x8c4, 0x8e4] {
                put_vendor_word(record, base + position * 4, u32::from(index));
            }
            put_vendor_word(record, 0x904 + position * 4, u32::from(stage.enabled));
            let rgb = 0x960 + position * 4;
            record[rgb..rgb + 3].copy_from_slice(&stage.color);
        }
        Self::from_vendor_profile(record)?;
        Ok(())
    }

    pub fn write_bytes(&self) -> [u8; DPI_LEN] {
        let mut write = self.clone();
        write.finish_edit();
        write.0
    }

    pub fn active_stage(&self) -> u8 {
        self.0[24]
    }

    pub fn stage(&self, number: u8) -> io::Result<DpiStage> {
        let position = stage_index(number)?;
        let mask = 1 << position;
        let rgb = 25 + position * 3;
        Ok(DpiStage {
            enabled: self.0[5] & mask != 0,
            dpi: match self.1 {
                Some(layout) => layout.decode(
                    self.0[8 + position],
                    self.0[16 + position],
                    self.0[6] & mask != 0,
                    self.0[7] & mask != 0,
                ),
                None => decode_dpi(
                    self.0[8 + position],
                    self.0[6] & mask != 0,
                    self.0[16 + position],
                ),
            },
            color: [self.0[rgb], self.0[rgb + 1], self.0[rgb + 2]],
        })
    }

    pub fn set_active_stage(&mut self, number: u8) -> io::Result<()> {
        let stage = self.stage(number)?;
        if !stage.enabled || stage.dpi.is_none() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "an active stage must be enabled and have a recognized DPI value",
            ));
        }
        self.0[24] = number;
        self.finish_edit();
        Ok(())
    }

    pub fn set_stage_dpi(&mut self, number: u8, dpi: u16) -> io::Result<()> {
        let position = stage_index(number)?;
        self.write_stage_dpi(position, dpi)?;
        self.finish_edit();
        Ok(())
    }

    fn write_stage_dpi(&mut self, position: usize, dpi: u16) -> io::Result<()> {
        let (code, selector, banked, second_bank) = if let Some(layout) = self.1 {
            let (code, selector, banked, extended) = layout.encode(dpi)?;
            (
                code,
                selector,
                banked,
                if matches!(layout, DpiLayout::Lookup { .. }) {
                    banked
                } else {
                    extended
                },
            )
        } else {
            let (code, banked, selector) = dpi_encoding(dpi)?;
            (code, u8::from(selector), banked, banked)
        };
        let mask = 1 << position;
        if self.1 != Some(DpiLayout::RawLinear) {
            self.0[6] = (self.0[6] & !mask) | (u8::from(banked) << position);
            self.0[7] = (self.0[7] & !mask) | (u8::from(second_bank) << position);
        }
        self.0[8 + position] = code;
        self.0[16 + position] = selector;
        Ok(())
    }

    pub fn set_stage_enabled(&mut self, number: u8, enabled: bool) -> io::Result<()> {
        let position = stage_index(number)?;
        let mask = 1 << position;
        if enabled {
            if self.stage(number)?.dpi.is_none() {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "set a recognized DPI value before enabling this stage",
                ));
            }
            self.0[5] |= mask;
        } else if self.0[5] & mask != 0 {
            if self.active_stage() == number || self.0[5] == mask {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "select another enabled DPI stage before disabling this one",
                ));
            }
            self.0[5] &= !mask;
        }
        self.finish_edit();
        Ok(())
    }

    pub fn set_stage_color(&mut self, number: u8, color: [u8; 3]) -> io::Result<()> {
        let start = 25 + stage_index(number)? * 3;
        self.0[start..start + 3].copy_from_slice(&color);
        self.finish_edit();
        Ok(())
    }

    pub fn angle_snap(&self) -> bool {
        self.0[3] == 1
    }

    pub fn set_angle_snap(&mut self, enabled: bool) {
        self.0[3] = u8::from(enabled);
        self.finish_edit();
    }

    pub fn ripple_control(&self) -> bool {
        self.0[4] == 1
    }

    pub fn set_ripple_control(&mut self, enabled: bool) {
        self.0[4] = u8::from(enabled);
        self.finish_edit();
    }

    fn finish_edit(&mut self) {
        self.0[2] = 1;
        self.0[49] = 2;
        stamp_checksum(&mut self.0, 50);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Lighting {
    pub mode: u8,
    pub brightness: u8,
    pub speed: Option<u8>,
    pub color: [u8; 3],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Timing([u8; TIMING_LEN]);

impl Default for Timing {
    fn default() -> Self {
        Self([0x05, 15, 1, 0, 3, 0xa8, 0, 255, 0, 1, 4, 0x01, 0xaf, 0, 0])
    }
}

impl Timing {
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        split_report(data, 0x05).map(Self)
    }

    pub fn from_vendor_profile(data: &[u8]) -> io::Result<Self> {
        let record = vendor_record(data)?;
        let mode = vendor_field(record, 0x94c, 0..=6, "lighting mode")? as u8;
        let brightness = if matches!(mode, 1 | 5) {
            vendor_field(record, 0x924, 1..=8, "brightness")? as u8
        } else {
            8
        };
        let speed = vendor_field(record, 0x928, 4..=8, "lighting speed")? as u8;
        let deep_sleep = vendor_field(record, 0x92c, 1..=60, "deep sleep")? as u8;
        let sleep = vendor_field(record, 0x930, 1..=60, "sleep")? as u8;
        let debounce = vendor_field(record, 0x958, 2..=25, "debounce")? as u8;
        let mut bytes = [0; TIMING_LEN];
        bytes[..6].copy_from_slice(&[
            0x05,
            15,
            1,
            mode << 4,
            (deep_sleep & 0xf0) | (9 - speed),
            ((deep_sleep & 0x0f) << 4) | brightness,
        ]);
        bytes[6..9].copy_from_slice(&record[0x95c..0x95f]);
        bytes[9] = sleep;
        bytes[10] = debounce;
        let mut timing = Self(bytes);
        timing.finish_edit();
        Ok(timing)
    }

    pub fn bytes(&self) -> &[u8; TIMING_LEN] {
        &self.0
    }

    pub(crate) fn write_vendor_profile(&self, record: &mut [u8]) -> io::Result<()> {
        vendor_record(record)?;
        let lighting = self.lighting();
        for (offset, value) in [
            (0x924, lighting.brightness),
            (0x928, lighting.speed.unwrap_or(0)),
            (0x92c, self.deep_sleep_minutes()),
            (0x930, self.0[9]),
            (0x94c, lighting.mode),
            (0x958, self.0[10]),
        ] {
            put_vendor_word(record, offset, u32::from(value));
        }
        record[0x95c..0x95f].copy_from_slice(&lighting.color);
        Self::from_vendor_profile(record)?;
        Ok(())
    }

    pub fn write_bytes(&self) -> [u8; TIMING_LEN] {
        let mut write = self.clone();
        write.finish_edit();
        write.0
    }

    pub fn sleep_seconds(&self) -> u16 {
        u16::from(self.0[9]) * 30
    }

    pub fn set_sleep_seconds(&mut self, seconds: u16) -> io::Result<()> {
        if !(30..=1800).contains(&seconds) || seconds % 30 != 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "sleep must be 30–1800 seconds in steps of 30",
            ));
        }
        self.0[9] = (seconds / 30) as u8;
        self.finish_edit();
        Ok(())
    }

    pub fn deep_sleep_minutes(&self) -> u8 {
        (self.0[4] & 0xf0) | (self.0[5] >> 4)
    }

    pub fn set_deep_sleep_minutes(&mut self, minutes: u8) -> io::Result<()> {
        if !(1..=60).contains(&minutes) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "deep sleep must be 1–60 minutes",
            ));
        }
        self.0[4] = (self.0[4] & 0x0f) | (minutes & 0xf0);
        self.0[5] = (self.0[5] & 0x0f) | ((minutes & 0x0f) << 4);
        self.finish_edit();
        Ok(())
    }

    pub fn debounce_ms(&self) -> u16 {
        u16::from(self.0[10]) * 2
    }

    pub fn set_debounce_ms(&mut self, milliseconds: u16) -> io::Result<()> {
        if !(4..=50).contains(&milliseconds) || milliseconds % 2 != 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "debounce must be 4–50 milliseconds in steps of 2",
            ));
        }
        self.0[10] = (milliseconds / 2) as u8;
        self.finish_edit();
        Ok(())
    }

    pub fn lighting(&self) -> Lighting {
        Lighting {
            mode: self.0[3] >> 4,
            brightness: self.0[5] & 0x0f,
            speed: 9_u8.checked_sub(self.0[4] & 0x0f),
            color: [self.0[6], self.0[7], self.0[8]],
        }
    }

    pub fn set_light_mode(&mut self, mode: u8) -> io::Result<()> {
        if mode > 6 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "light mode must be 0–6",
            ));
        }
        self.0[3] = (self.0[3] & 0x0f) | (mode << 4);
        if !matches!(mode, 1 | 5) {
            self.0[5] = (self.0[5] & 0xf0) | 8;
        }
        self.finish_edit();
        Ok(())
    }

    pub fn set_brightness(&mut self, brightness: u8) -> io::Result<()> {
        if !(1..=8).contains(&brightness) || !matches!(self.lighting().mode, 1 | 5) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "brightness must be 1–8 in static or static DPI mode",
            ));
        }
        self.0[5] = (self.0[5] & 0xf0) | brightness;
        self.finish_edit();
        Ok(())
    }

    pub fn set_breath_speed(&mut self, speed: u8) -> io::Result<()> {
        if !(4..=8).contains(&speed) || !matches!(self.lighting().mode, 2 | 3 | 4 | 6) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "speed must be 4–8 in an animated lighting mode",
            ));
        }
        self.0[4] = (self.0[4] & 0xf0) | (9 - speed);
        self.finish_edit();
        Ok(())
    }

    pub fn set_light_color(&mut self, color: [u8; 3]) {
        self.0[6..9].copy_from_slice(&color);
        self.finish_edit();
    }

    fn finish_edit(&mut self) {
        self.0[2] = 1;
        stamp_checksum(&mut self.0, 11);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Polling([u8; POLLING_LEN]);

impl Default for Polling {
    fn default() -> Self {
        Self([0x06, 9, 1, 1, 0xfe, 0, 0, 0, 0])
    }
}

impl Polling {
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        let bytes = split_report::<POLLING_LEN>(data, 0x06)?;
        if !matches!(bytes[3], 1 | 2 | 4 | 8) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "unknown polling rate code",
            ));
        }
        Ok(Self(bytes))
    }

    pub fn from_vendor_profile(data: &[u8]) -> io::Result<Self> {
        let index = vendor_field(vendor_record(data)?, 0x938, 0..=3, "polling index")?;
        let mut report = Self::default();
        report.set_hz(125 << index)?;
        Ok(report)
    }

    pub fn bytes(&self) -> &[u8; POLLING_LEN] {
        &self.0
    }

    pub(crate) fn write_vendor_profile(&self, record: &mut [u8]) -> io::Result<()> {
        vendor_record(record)?;
        put_vendor_word(record, 0x938, 3 - self.0[3].trailing_zeros());
        Ok(())
    }

    pub fn write_bytes(&self) -> [u8; POLLING_LEN] {
        let mut write = self.clone();
        write.finish_edit();
        write.0
    }

    pub fn hz(&self) -> u16 {
        1000 / u16::from(self.0[3])
    }

    pub fn set_hz(&mut self, hz: u16) -> io::Result<()> {
        self.0[3] = match hz {
            125 => 8,
            250 => 4,
            500 => 2,
            1000 => 1,
            _ => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "polling rate must be 125, 250, 500 or 1000 Hz",
                ));
            }
        };
        self.finish_edit();
        Ok(())
    }

    fn finish_edit(&mut self) {
        self.0[2] = 1;
        self.0[4] = !self.0[3];
    }
}

pub fn version(data: &[u8]) -> io::Result<u16> {
    let bytes = report::<VERSION_LEN>(data, 0x0b)?;
    Ok(u16::from_be_bytes([bytes[2], bytes[3]]))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Legacy([u8; LEGACY_LEN]);

impl Legacy {
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        report(data, 0x0a).map(Self)
    }

    pub fn bytes(&self) -> &[u8; LEGACY_LEN] {
        &self.0
    }

    pub fn dpi(&self) -> Dpi {
        let mut bytes = [0; DPI_LEN];
        bytes[..2].copy_from_slice(&[0x04, 56]);
        bytes[3..52].copy_from_slice(&self.0[5..54]);
        let mut dpi = Dpi(bytes, None);
        dpi.finish_edit();
        dpi
    }

    pub fn timing(&self) -> Timing {
        let mut bytes = [0; TIMING_LEN];
        bytes[..2].copy_from_slice(&[0x05, 15]);
        bytes[3..13].copy_from_slice(&self.0[55..65]);
        let mut timing = Timing(bytes);
        timing.finish_edit();
        timing
    }

    pub fn polling(&self) -> io::Result<Polling> {
        let code = self.0[3];
        Polling::parse(&[0x06, 9, 1, code, !code, 0, 0, 0, 0])
    }

    pub fn button_payload(&self) -> &[u8] {
        &self.0[65..119]
    }

    pub fn buttons(&self) -> [u8; 59] {
        let mut bytes = [0; 59];
        bytes[..3].copy_from_slice(&[0x08, 59, 1]);
        bytes[3..57].copy_from_slice(self.button_payload());
        stamp_checksum(&mut bytes, 57);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_vendor_dpi_value_round_trips() {
        let mut report = Dpi::default();
        for dpi in (50..=10_000)
            .step_by(50)
            .chain((10_100..=18_000).step_by(100))
        {
            report.set_stage_dpi(8, dpi).unwrap();
            assert_eq!(report.stage(8).unwrap().dpi, Some(dpi));
        }
        assert_eq!((report.bytes()[15], report.bytes()[23]), (211, 0));
        assert_eq!((report.bytes()[6], report.bytes()[7]), (0xa0, 0xa0));
        assert!(report.set_stage_dpi(8, 10_050).is_err());
    }

    #[test]
    fn dpi_updates_preserve_other_fields_and_checksum() {
        let mut raw = *Dpi::default().bytes();
        raw[52] = 0xfe;
        raw[7] = 0x21;
        let mut report = Dpi::parse(&raw).unwrap();
        assert_eq!(*report.bytes(), raw);
        report.set_stage_dpi(2, 10_100).unwrap();
        report.set_stage_color(2, [5, 7, 9]).unwrap();
        assert_eq!(report.bytes()[9], 118);
        assert_eq!(report.bytes()[17], 1);
        assert_eq!(report.bytes()[7], 0x21);
        assert_eq!(report.bytes()[52], 0xfe);
        assert_eq!(report.stage(2).unwrap().color, [5, 7, 9]);
        let sum: u16 = report.bytes()[3..50].iter().copied().map(u16::from).sum();
        assert_eq!(&report.bytes()[50..52], &sum.to_be_bytes());
        assert!(report.set_stage_enabled(2, false).is_err());
        assert!(report.set_stage_enabled(8, true).is_err());
        report.set_active_stage(1).unwrap();
        report.set_stage_enabled(2, false).unwrap();
        assert!(!report.stage(2).unwrap().enabled);
    }

    #[test]
    fn timing_matches_vendor_default_and_preserves_shared_nibbles() {
        let mut report = Timing::default();
        assert_eq!(report.write_bytes(), *report.bytes());
        assert_eq!(report.deep_sleep_minutes(), 10);
        assert_eq!(report.sleep_seconds(), 30);
        assert_eq!(report.debounce_ms(), 8);
        report.set_light_mode(1).unwrap();
        report.set_brightness(5).unwrap();
        report.set_deep_sleep_minutes(60).unwrap();
        report.set_sleep_seconds(300).unwrap();
        report.set_debounce_ms(20).unwrap();
        assert_eq!(report.bytes()[3..6], [0x10, 0x33, 0xc5]);
        assert_eq!(report.bytes()[9..13], [10, 10, 2, 0x1b]);
        assert_eq!(report.lighting().brightness, 5);
        assert_eq!(report.deep_sleep_minutes(), 60);
        assert!(report.set_debounce_ms(7).is_err());
    }

    #[test]
    fn polling_and_version_validate_wire_shapes() {
        let mut polling = Polling::default();
        for (hz, code) in [(125, 8), (250, 4), (500, 2), (1000, 1)] {
            polling.set_hz(hz).unwrap();
            assert_eq!(polling.hz(), hz);
            assert_eq!(&polling.bytes()[3..5], &[code, !code]);
        }
        assert!(polling.set_hz(800).is_err());
        assert!(Polling::parse(&[6, 8, 1, 1, 0xfe, 0, 0, 0, 0]).is_err());
        assert!(Polling::parse(&[6, 9, 1, 0, 0xff, 0, 0, 0, 0]).is_err());
        assert_eq!(version(&[0x0b, 0, 0x10, 2, 0, 0, 0, 0]).unwrap(), 0x1002);
        assert!(version(&[0x0b; 7]).is_err());
    }

    #[test]
    fn combined_layout_recovers_each_settings_group() {
        let mut bytes = [0; LEGACY_LEN];
        bytes[0] = 0x0a;
        bytes[3] = 2;
        bytes[5..54].copy_from_slice(&Dpi::default().bytes()[3..52]);
        bytes[55..65].copy_from_slice(&Timing::default().bytes()[3..13]);
        bytes[65] = 17;
        bytes[118] = 42;
        bytes[119] = 0xff;
        let report = Legacy::parse(&bytes).unwrap();
        assert_eq!(report.dpi(), Dpi::default());
        assert_eq!(report.timing(), Timing::default());
        assert_eq!(report.polling().unwrap().hz(), 500);
        assert_eq!(report.button_payload().len(), 54);
        assert_eq!(report.button_payload()[0], 17);
        assert_eq!(report.button_payload()[53], 42);
        assert_eq!(report.bytes()[119], 0xff);
        assert_eq!(&report.buttons()[..4], &[0x08, 59, 1, 17]);
        assert_eq!(&report.buttons()[56..], &[42, 0, 59]);
    }

    #[test]
    fn windows_profile_fields_build_split_reports() {
        fn put(record: &mut [u8], offset: usize, value: u32) {
            record[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        let mut record = [0; VENDOR_PROFILE_LEN];
        for (position, dpi_index) in [16, 32, 48, 64, 100, 280, 0, 0].into_iter().enumerate() {
            put(&mut record, 0x8c4 + position * 4, dpi_index);
            put(&mut record, 0x904 + position * 4, u32::from(position < 6));
        }
        for (position, color) in [
            0xff, 0xff00, 0xff0000, 0xffff, 0xffff00, 0xff00ff, 0x40ff, 0xffffff,
        ]
        .into_iter()
        .enumerate()
        {
            put(&mut record, 0x960 + position * 4, color);
        }
        for (offset, value) in [
            (0x924, 8),
            (0x928, 6),
            (0x92c, 10),
            (0x930, 1),
            (0x934, 1),
            (0x938, 3),
            (0x940, 1),
            (0x958, 4),
            (0x95c, 0xff00),
        ] {
            put(&mut record, offset, value);
        }
        assert_eq!(Dpi::from_vendor_profile(&record).unwrap(), Dpi::default());
        assert_eq!(
            Timing::from_vendor_profile(&record).unwrap(),
            Timing::default()
        );
        assert_eq!(
            Polling::from_vendor_profile(&record).unwrap(),
            Polling::default()
        );
        assert!(Dpi::from_vendor_profile(&record[..100]).is_err());
        put(&mut record, 0x934, 6);
        assert!(Dpi::from_vendor_profile(&record).is_err());
    }
}
