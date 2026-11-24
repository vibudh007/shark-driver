//! Model identities and protocol selection traced from the Windows package.

use crate::model_wire::WiredPolling;

pub const USB_VENDOR: u16 = 0x1d57;
pub const SHARED_RECEIVER: u16 = 0xfa60;
pub const FAST_RECEIVER: u16 = 0xfa65;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Model {
    pub slot: u8,
    pub name: &'static str,
    pub wired_pid: u16,
    pub receiver_tag: u8,
    pub fast_receiver: bool,
}

impl Model {
    pub fn vendor_slot(slot: u8) -> Option<&'static Self> {
        MODELS.get(usize::from(slot.checked_sub(1)?))
    }

    pub fn extended_windows_files(&self) -> bool {
        self.slot > 6
    }

    pub fn wired(pid: u16) -> Option<&'static Self> {
        MODELS.iter().find(|model| model.wired_pid == pid)
    }

    pub fn from_receiver_report(pid: u16, report: &[u8]) -> Option<&'static Self> {
        let &[0x03, tag, _, _, _, ..] = report else {
            return None;
        };
        MODELS.iter().find(|model| {
            model.receiver_tag == tag
                && (pid == SHARED_RECEIVER || (pid == FAST_RECEIVER && model.fast_receiver))
        })
    }

    /// Only the Bluetooth device's advertised name, never the user alias.
    pub fn bluetooth_name(name: &str) -> Option<&'static Self> {
        let normalized = |text: &str| {
            text.bytes()
                .filter(u8::is_ascii_alphanumeric)
                .map(|byte| char::from(byte.to_ascii_lowercase()))
                .collect::<String>()
        };
        let name = normalized(name);
        if matches!(name.as_str(), "r1mouse" | "r1mouse1" | "r1mouse2") {
            return Some(R1);
        }
        let name = name.strip_prefix("attackshark")?;
        MODELS.iter().find(|model| normalized(model.name) == name)
    }

    pub fn wired_polling(&self) -> Option<WiredPolling> {
        Some(match self.slot {
            4 | 11 | 14 | 15 => WiredPolling::Versioned(0x55),
            10 => WiredPolling::Versioned(0x07),
            6 | 12 => WiredPolling::Direct,
            16..=18 => WiredPolling::Combined,
            _ => return None,
        })
    }

    pub fn supports_settings(&self) -> bool {
        self.slot == R1.slot
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VendorStatus {
    pub model: &'static Model,
    pub class: u8,
    pub value: u16,
}

impl VendorStatus {
    pub fn parse(receiver_pid: u16, frame: &[u8]) -> Option<Self> {
        let model = Model::from_receiver_report(receiver_pid, frame)?;
        Some(Self {
            model,
            class: frame[2],
            value: u16::from_le_bytes([frame[3], frame[4]]),
        })
    }

    pub fn battery(self) -> Option<(u8, u8)> {
        let [state, level] = self.value.to_le_bytes();
        // V6 uses class 0x41. The other common receiver handlers use 0x40.
        let expected = if self.model.slot == 17 { 0x41 } else { 0x40 };
        (self.class == expected && level <= 10).then_some((level, state))
    }
}

pub const R1: &Model = &MODELS[2];

const fn model(slot: u8, name: &'static str, wired_pid: u16, receiver_tag: u8) -> Model {
    Model {
        slot,
        name,
        wired_pid,
        receiver_tag,
        fast_receiver: matches!(slot, 7 | 8 | 13),
    }
}

pub const MODELS: [Model; 18] = [
    model(1, "X6", 0x2055, 0x85),
    model(2, "X3", 0x201b, 0x4d),
    model(3, "R1", 0xfa61, 0x10),
    model(4, "X11", 0xfa55, 0x55),
    model(5, "X3 Max", 0x201c, 0x4e),
    model(6, "X1", 0x2111, 0xb1),
    model(7, "X11 Pro", 0x211d, 0xbe),
    model(8, "X3 Pro", 0x211f, 0x01),
    model(9, "V5 SE", 0x2121, 0x03),
    model(10, "X11 SE", 0x2125, 0x07),
    model(11, "X8 SE", 0x2120, 0x02),
    model(12, "X8 Plus", 0x2124, 0x06),
    model(13, "X8 Pro", 0x2126, 0x08),
    model(14, "G3", 0x2122, 0x04),
    model(15, "G3 Pro", 0x212c, 0x0e),
    model(16, "V3 Pro", 0x2155, 0xe5),
    model(17, "V6", 0x2224, 0x24),
    model(18, "V3", 0x215a, 0xea),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_each_vendor_slot_without_conflating_model_and_report_id() {
        for (index, model) in MODELS.iter().enumerate() {
            assert_eq!(usize::from(model.slot), index + 1);
            assert_eq!(Model::wired(model.wired_pid), Some(model));
            assert_eq!(
                Model::bluetooth_name(&format!("Attack Shark {}", model.name)),
                Some(model)
            );
            assert_eq!(
                Model::bluetooth_name(&format!("Shark {}", model.name)),
                None
            );
            let report = [0x03, model.receiver_tag, 0x40, 0, 7];
            assert_eq!(
                Model::from_receiver_report(SHARED_RECEIVER, &report),
                Some(model)
            );
            assert_eq!(
                Model::from_receiver_report(FAST_RECEIVER, &report),
                model.fast_receiver.then_some(model)
            );
            assert_eq!(model.supports_settings(), model == R1);
            let battery_class = if model.slot == 17 { 0x41 } else { 0x40 };
            let event = VendorStatus::parse(
                SHARED_RECEIVER,
                &[3, model.receiver_tag, battery_class, 3, 7],
            )
            .unwrap();
            assert_eq!(event.battery(), Some((7, 3)));
            assert_eq!(event.value, 0x0703);
        }
        assert_eq!(Model::wired(SHARED_RECEIVER), None);
        assert_eq!(
            Model::from_receiver_report(SHARED_RECEIVER, &[3, 0x10]),
            None
        );
        assert_eq!(
            Model::from_receiver_report(SHARED_RECEIVER, &[2, 0x10, 0, 0, 0]),
            None
        );
        assert_eq!(
            Model::from_receiver_report(SHARED_RECEIVER, &[3, 0xff, 0, 0, 0]),
            None
        );
    }
}
