use std::{collections::BTreeMap, io, path::PathBuf, str::FromStr};

use shark_driver::{
    bluetooth,
    driver::{Connection, Driver},
    linux, simulator,
};

pub(super) const HELP: &str = "Usage: shark-driver-cli [OPTIONS] <COMMAND>

Options
  --connection wired|receiver     Choose a connection when more than one is present
  --device /dev/hidrawN           Choose a discovered configuration endpoint
  --bluetooth ADDRESS             Use this connected, paired BlueZ peer
  --simulate wired|receiver|legacy|bluetooth
                                  Run against an in-memory mouse for this command
  --data-dir PATH                 Local profiles and macros (default .r1)
  --version                       Print the CLI version
  --help                          Show usage

Commands
  list                            List detected mice and recovery endpoints
  list models                     Show the model registry from the Windows package
  observe-status [--timeout-ms N] [--count N]
                                  Listen to USB status or read BlueZ battery telemetry
  list watch [--timeout-ms N]      Report USB endpoint arrivals and removals
  recovery check|restore BACKUP  Check or recover a wired R1 in Beken bootloader mode
  info                            Read connection and firmware
  status [--timeout-ms N] [--count N]
                                  Read battery, DPI, and other status events
  settings                        Show the current configuration
  polling show | set HZ           125, 250, 500, or 1000 Hz
  dpi show [STAGE] | select STAGE
  dpi set STAGE [--value DPI] [--enabled on|off] [--color RRGGBB]
  sensor show | set [--angle-snap on|off] [--ripple-control on|off]
  timing show | set [--sleep-seconds N] [--deep-sleep-minutes N] [--debounce-ms N]
  lighting show | set [--mode NAME] [--brightness N] [--speed N] [--color RRGGBB]
  button actions | show [BUTTON] | set BUTTON ACTION
  profile ...                     Save, import, apply, or organize profiles
  macro ...                       Edit, record, import, read, or install macros
  reset                           Apply vendor defaults on a supported USB endpoint
  reset --keep-macros             Restore model settings defaults and keep occupied macros

Use 'shark-driver-cli help <command>' for examples and accepted values.
";

const PROFILE_HELP: &str = "Usage: shark-driver-cli profile <COMMAND>

  list
  show NAME
  device-info               Read the active device profile and profile count
  plan-windows FILE INDEX --model SLOT
                           Preview model-specific full-apply packets without device access
  plan-fast-receiver FILE INDEX --model SLOT
                           Preview high-rate receiver HID output packets
  read-bluetooth FILE       Capture the vendor Bluetooth settings notification
  apply-windows FILE INDEX --model SLOT
                           Apply a macro-free Windows record to a matching wired mouse
  backup-device FILE | restore-device FILE
                           Back up or restore active settings on an audited wired model
  read-device FILE          Save the 128-byte combined report on supported wired models
  save NAME                 Capture the attached mouse settings and assigned macros
  update NAME               Replace an existing profile with the current settings
  defaults NAME             Create a profile with vendor defaults for this firmware
  apply NAME                Apply a saved profile and verify device readback
  rename OLD NEW
  delete NAME
  import NAME FILE         Import a native profile
  export NAME FILE         Export a native profile, or use - for stdout
  windows-options NAME [--lift-off 1mm|2mm] [--motion-sync on|off] [--wake-mode on|off]
                           Show or edit Windows-only profile fields
  inspect-windows FILE [--model SLOT]
                           List Windows profile names and active index
  extract-windows FILE INDEX OUTPUT [--model SLOT]
  activate-windows FILE INDEX OUTPUT [--model SLOT]
  rename-windows FILE INDEX NAME OUTPUT [--model SLOT]
                           Copy or edit Windows libraries without changing other bytes
  export-windows NAME FILE Export a Windows R1 .pro record, or use - for stdout
  export-windows-library FILE ACTIVE NAME [NAME ...]
                           Write pro.data with a 1-based active index
  import-windows NAME FILE INDEX [--firmware HEX]
                           Convert one Windows record by its 1-based index
";

const MACRO_HELP: &str = "Usage: shark-driver-cli macro <COMMAND>

  list | new NAME | show NAME | delete NAME | rename OLD NEW
  import NAME FILE | update NAME FILE | export NAME FILE
  edit NAME insert|replace INDEX down|up|tap KEY [DELAY_MS]
  edit NAME insert|replace INDEX extended DELAY_MS HEX8
  edit NAME remove INDEX | move FROM TO | delay INDEX MS | playback MODE REPEAT
  inspect-windows FILE [--model SLOT]
  import-windows NAME FILE [INDEX] [--model SLOT]
  export-windows NAME FILE [--model SLOT]
  export-windows-library FILE [--model SLOT] NAME [NAME ...]
  record NAME /dev/input/eventN [...] [--events N] [--timeout-ms N] [--fixed-delay-ms N]
  preview SLOT FILE        Show the exact logical report without opening a device
  read SLOT [FILE]          Read back a device macro
  read-raw SLOT [FILE]      Export the device's logical macro pages as hex
  backup-device-pages SLOT FILE | restore-device-pages FILE
                           Back up or restore an occupied wired V3-family macro chain
  read-device-pages SLOT FILE Save raw pages of a bound macro on wired V3 Pro, V6, or V3
  backup FILE [SLOT]       Save raw readback of all device macro slots or one slot
  verify-backup FILE        Compare raw macro slots with a backup
  restore-raw SLOT FILE     Restore captured pages to their original slots
  upload SLOT FILE          Upload and verify a macro, including consecutive pages
  install BUTTON FILE       Upload, verify, then assign a macro to the button
  assign BUTTON NAME        Install a macro from the local library

Macro text uses 'mode times', 'repeat 1', and event lines such as
'down a 50' and 'up a 100'. The delay is in milliseconds.
";

#[derive(Clone, Copy)]
pub(super) enum Emulation {
    Wired,
    Receiver,
    Legacy,
    Bluetooth,
}

pub(super) struct Options {
    pub(super) emulation: Option<Emulation>,
    pub(super) connection: Option<Connection>,
    pub(super) device: Option<PathBuf>,
    pub(super) bluetooth: Option<String>,
    pub(super) expected: Option<String>,
    pub(super) data: PathBuf,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            emulation: None,
            connection: None,
            device: None,
            bluetooth: None,
            expected: None,
            data: PathBuf::from(".r1"),
        }
    }
}

impl Options {
    pub(super) fn parse<'a>(mut args: &'a [&str]) -> io::Result<(Self, &'a [&'a str])> {
        let mut options = Self::default();
        while let Some(flag) = args.first().copied().filter(|flag| {
            matches!(
                *flag,
                "--simulate"
                    | "--connection"
                    | "--device"
                    | "--bluetooth"
                    | "--data-dir"
                    | "--expect-device"
            )
        }) {
            let value = args
                .get(1)
                .ok_or_else(|| input(format!("{flag} requires a value")))?;
            match flag {
                "--simulate" if options.emulation.is_none() => {
                    options.emulation = Some(match *value {
                        "wired" => Emulation::Wired,
                        "receiver" => Emulation::Receiver,
                        "legacy" => Emulation::Legacy,
                        "bluetooth" => Emulation::Bluetooth,
                        _ => {
                            return Err(input(
                                "simulation must be wired, receiver, legacy, or bluetooth",
                            ));
                        }
                    })
                }
                "--connection" if options.connection.is_none() => {
                    options.connection = Some(match *value {
                        "wired" => Connection::Wired,
                        "receiver" => Connection::Receiver,
                        _ => return Err(input("connection must be wired or receiver")),
                    })
                }
                "--device" if options.device.is_none() => {
                    options.device = Some(PathBuf::from(value))
                }
                "--bluetooth" if options.bluetooth.is_none() => {
                    options.bluetooth = Some(value.to_string())
                }
                "--data-dir" => options.data = PathBuf::from(value),
                "--expect-device" if options.expected.is_none() => {
                    options.expected = Some(value.to_string())
                }
                _ => return Err(input(format!("{flag} was specified more than once"))),
            }
            args = &args[2..];
        }
        let usb = options.connection.is_some() || options.device.is_some();
        if options.expected.is_some() && options.device.is_none() {
            return Err(input("endpoint identity requires an explicit USB device"));
        }
        if (options.emulation.is_some() && (usb || options.bluetooth.is_some()))
            || (usb && options.bluetooth.is_some())
        {
            return Err(input(
                "choose one of USB selection, --bluetooth, or --simulate",
            ));
        }
        Ok((options, args))
    }

    pub(super) fn detected(&self) -> io::Result<Vec<linux::DetectedDevice>> {
        let mut devices = linux::survey()?;
        devices.retain(|device| {
            self.connection
                .is_none_or(|connection| connection == device.connection)
                && self.device.as_ref().is_none_or(|path| *path == device.node)
        });
        Ok(devices)
    }

    pub(super) fn model_device(&self) -> io::Result<Option<linux::DetectedDevice>> {
        if self.emulation.is_some() || self.bluetooth.is_some() {
            return Ok(None);
        }
        let mut devices = self.detected()?;
        Ok((devices.len() == 1
            && devices[0]
                .model
                .is_some_and(|model| !model.supports_settings())
            && (devices[0].connection == Connection::Wired || devices[0].dpi_read))
            .then(|| devices.remove(0)))
    }

    pub(super) fn v3_family(&self) -> io::Result<Option<linux::DetectedDevice>> {
        Ok(self.model_device()?.filter(|device| {
            device
                .model
                .is_some_and(|model| (16..=18).contains(&model.slot))
        }))
    }

    pub(super) fn devices(&self) -> io::Result<Vec<linux::Device>> {
        let mut devices = linux::discover()?;
        devices.retain(|device| {
            self.connection
                .is_none_or(|connection| connection == device.connection)
                && self
                    .device
                    .as_ref()
                    .is_none_or(|path| *path == device.feature_path)
        });
        Ok(devices)
    }

    pub(super) fn open(&self) -> io::Result<Driver> {
        if let Some(mode) = self.emulation {
            let device = match mode {
                Emulation::Wired => simulator::Simulator::new(Connection::Wired),
                Emulation::Receiver => simulator::Simulator::new(Connection::Receiver),
                Emulation::Legacy => simulator::Simulator::legacy(),
                Emulation::Bluetooth => simulator::Simulator::new(Connection::Bluetooth),
            };
            return Ok(Driver::new(Box::new(device)));
        }
        if let Some(address) = &self.bluetooth {
            return Ok(Driver::new(Box::new(bluetooth::open(address)?)));
        }
        if self.device.is_none() && self.detected()?.len() > 1 {
            return Err(input(
                "multiple mice match; use list and select one with --device /dev/hidrawN",
            ));
        }
        let devices = self.devices()?;
        match devices.as_slice() {
            [device] => {
                let handle =
                    linux::open_identified(device, self.expected.as_deref()).map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!("{}: {error}", device.feature_path.display()),
                        )
                    })?;
                Ok(Driver::new(Box::new(handle)))
            }
            [] => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no matching R1 configuration endpoint; run list and check the cable or receiver",
            )),
            _ => Err(input(
                "multiple R1 endpoints match; use list and select one with --device /dev/hidrawN",
            )),
        }
    }
}

pub(super) fn input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub(super) fn number<T: FromStr>(value: &str, name: &str) -> io::Result<T> {
    value
        .parse()
        .map_err(|_| input(format!("invalid {name} '{value}'")))
}

pub(super) fn color(value: &str) -> io::Result<[u8; 3]> {
    let code = value.strip_prefix('#').unwrap_or(value);
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(input("color must be six hexadecimal digits such as 22aaee"));
    }
    let value = u32::from_str_radix(code, 16).map_err(|_| input("invalid RGB color"))?;
    let [_, r, g, b] = value.to_be_bytes();
    Ok([r, g, b])
}

pub(super) fn toggle(value: &str) -> io::Result<bool> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(input("expected on or off")),
    }
}

pub(super) fn flag_pairs<'a>(
    args: &[&'a str],
    allowed: &[&str],
    empty_ok: bool,
) -> io::Result<BTreeMap<&'a str, &'a str>> {
    if args.is_empty() && !empty_ok {
        return Err(input("provide at least one setting to change"));
    }
    if args.len() % 2 != 0 {
        return Err(input(format!("{} requires a value", args[args.len() - 1])));
    }
    let mut flags = BTreeMap::new();
    for pair in args.chunks_exact(2) {
        let flag = pair[0];
        if !allowed.contains(&flag) {
            return Err(input(format!("unknown option {flag}")));
        }
        if flags.insert(flag, pair[1]).is_some() {
            return Err(input(format!("duplicate option {flag}")));
        }
    }
    Ok(flags)
}

pub(super) fn help(topic: &str) -> &'static str {
    match topic {
        "profile" => PROFILE_HELP,
        "macro" => MACRO_HELP,
        "recovery" => {
            "Usage: shark-driver-cli --device /dev/hidrawN recovery check|restore BACKUP\nUse list to identify the wired Beken bootloader. BACKUP must be a new file.\n"
        }
        "dpi" => {
            "Usage: dpi show [STAGE] | select STAGE | set STAGE [--value DPI] [--enabled on|off] [--color RRGGBB]\nStages 1–8. Available DPI values depend on the mouse model.\n"
        }
        "button" => {
            "Usage: button actions | show [BUTTON] | set BUTTON ACTION\nButtons: left, right, middle, forward, back, dpi, or a numbered slot.\nAction examples: disabled, dpi-up, volume-up, key:ctrl+c, macro:forward.\n"
        }
        "timing" => {
            "Usage: timing show | set [--sleep-seconds N] [--deep-sleep-minutes N] [--debounce-ms N]\nSleep 30–1800 in steps of 30. Deep sleep 1–60 min. Debounce 4–50 in steps of 2 ms.\n"
        }
        "lighting" => {
            "Usage: lighting show | set [--mode NAME] [--brightness N] [--speed N] [--color RRGGBB]\nModes: off, static, breathing, neon, color-breathing, static-dpi, breathing-dpi.\nBrightness 1–8 for static modes. Speed 4–8 for animated modes.\n"
        }
        "sensor" => "Usage: sensor show | set [--angle-snap on|off] [--ripple-control on|off]\n",
        "polling" => "Usage: polling show | set 125|250|500|1000\n",
        _ => HELP,
    }
}
