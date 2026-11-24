use crate::driver::{Connection, RESET_PREAMBLE, Report, Transport};
use crate::models::{FAST_RECEIVER, Model, R1, SHARED_RECEIVER, USB_VENDOR, VendorStatus};
use libc::{
    LOCK_EX, LOCK_NB, O_NONBLOCK, POLLERR, POLLHUP, POLLIN, POLLNVAL, flock, ioctl, poll, pollfd,
};
use std::collections::BTreeMap;
use std::ffi::{c_int, c_ulong};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(not(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "arm",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64",
)))]
compile_error!("hidraw ioctl layout is unsupported on this Linux architecture");

const HIDRAW_CLASS: &str = "/sys/class/hidraw";
const SYS_DEVICES: &str = "/sys/devices";
const BUS_USB: u32 = 0x03;
const VENDOR: u16 = USB_VENDOR;
const WIRED: u16 = R1.wired_pid;
const RECEIVER: u16 = SHARED_RECEIVER;
const DISCOVERY_WINDOW: Duration = Duration::from_millis(180);
const MAX_DESCRIPTOR: usize = 4096;
const MAX_INPUT: usize = 16_384;
const MAX_FEATURE: usize = (1 << 14) - 1;

// USB revision 1.04 needs the audited firmware's scratch-buffer preparation.
const PREPARE_QUERY: [u8; 5] = [0x2e, 5, 0, 0, 0];
const WIRED_V104_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x80, 0xa1, 0x01, 0x85, 0x01, 0x05, 0x01, 0x19, 0x81, 0x29, 0x83, 0x15, 0x00,
    0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x06, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0xc0, 0x05,
    0x0c, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x02, 0x15, 0x00, 0x26, 0x80, 0x03, 0x19, 0x00, 0x2a, 0x80,
    0x03, 0x75, 0x10, 0x95, 0x01, 0x81, 0x00, 0xc0, 0x05, 0x0a, 0x09, 0x00, 0xa1, 0x01, 0x85, 0x03,
    0x19, 0x00, 0x29, 0x00, 0x15, 0x00, 0x25, 0x00, 0x75, 0x08, 0x95, 0x04, 0x81, 0x01, 0xc0, 0x05,
    0x0b, 0x09, 0x00, 0xa1, 0x01, 0x85, 0x04, 0x95, 0x33, 0xb1, 0x01, 0x85, 0x05, 0x95, 0x0c, 0xb1,
    0x01, 0x85, 0x06, 0x95, 0x08, 0xb1, 0x01, 0x85, 0x07, 0x95, 0x07, 0xb1, 0x01, 0x85, 0x08, 0x95,
    0x3a, 0xb1, 0x01, 0x85, 0x09, 0x95, 0x3f, 0xb1, 0x01, 0x85, 0x0a, 0x95, 0x07, 0xb1, 0x01, 0x85,
    0x0b, 0x95, 0x07, 0xb1, 0x01, 0x85, 0x0c, 0x95, 0x05, 0xb1, 0x01, 0x85, 0x10, 0x95, 0x07, 0xb1,
    0x01, 0x85, 0xa0, 0x95, 0x07, 0xb1, 0x01, 0x85, 0x22, 0x95, 0x83, 0xb1, 0x01, 0x85, 0x23, 0x95,
    0x83, 0xb1, 0x01, 0x85, 0x24, 0x95, 0x07, 0xb1, 0x01, 0x85, 0x25, 0x95, 0x0c, 0xb1, 0x01, 0x85,
    0x26, 0x95, 0x08, 0xb1, 0x01, 0x85, 0x27, 0x95, 0x07, 0xb1, 0x01, 0x85, 0x28, 0x95, 0x83, 0xb1,
    0x01, 0x85, 0x29, 0x95, 0x80, 0xb1, 0x01, 0x85, 0x2a, 0x95, 0x82, 0xb1, 0x01, 0x85, 0x2b, 0x95,
    0x07, 0xb1, 0x01, 0x85, 0x2c, 0x95, 0x03, 0xb1, 0x01, 0x85, 0x2d, 0x95, 0x66, 0xb1, 0x01, 0x85,
    0x23, 0x95, 0x82, 0xb1, 0x01, 0x85, 0x2e, 0x95, 0x04, 0xb1, 0x01, 0xc0,
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Features {
    Standard,
    Prepared,
    Blocked,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub feature_path: PathBuf,
    pub monitor_path: Option<PathBuf>,
    pub connection: Connection,
    pub label: String,
    usb_parent: PathBuf,
}

pub struct Hidraw {
    feature: File,
    feature_ids: ReportIds,
    monitor: Option<File>,
    input: Vec<u8>,
    connection: Connection,
    features: Features,
}

#[derive(Clone, Copy, Default)]
struct ReportIds([u64; 4]);

impl ReportIds {
    fn insert(&mut self, id: u8) {
        self.0[usize::from(id / 64)] |= 1 << (id % 64);
    }

    fn contains(self, id: u8) -> bool {
        self.0[usize::from(id / 64)] & (1 << (id % 64)) != 0
    }
}

#[derive(Default)]
struct Capabilities {
    status_report: bool,
    features: ReportIds,
    feature_bits: BTreeMap<u8, u32>,
    fast_input_bits: u32,
    fast_output_bits: u32,
    wired_v104_descriptor: bool,
}

impl Capabilities {
    fn feature_width(&self, id: u8) -> Option<usize> {
        let bits = self.feature_bits.get(&id)?;
        (id != 0 && *bits != 0).then_some(bits.div_ceil(8) as usize + 1)
    }

    fn fast_output_width(&self) -> Option<usize> {
        let width = self.fast_output_bits.div_ceil(8) as usize + 1;
        (matches!(width, 64 | 65) && self.fast_input_bits == 63 * 8).then_some(width)
    }

    fn config_channel(&self) -> bool {
        [0x04, 0x05, 0x06, 0x0b, 0xa0]
            .into_iter()
            .all(|id| self.features.contains(id))
    }
}

struct Candidate {
    product: u16,
    node: PathBuf,
    usb_parent: PathBuf,
    connection: Connection,
    label: String,
    capabilities: Capabilities,
}

#[derive(Clone, Copy, Default)]
struct Globals {
    usage_page: u16,
    report_id: u8,
    report_size: u32,
    report_count: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct HidrawInfo {
    bus: u32,
    vendor: u16,
    product: u16,
}

#[repr(C)]
struct RawDescriptor {
    size: u32,
    bytes: [u8; MAX_DESCRIPTOR],
}

pub fn discover() -> io::Result<Vec<Device>> {
    Ok(pair_candidates(
        scan_candidates()?
            .into_iter()
            .filter(|candidate| matches!(candidate.product, WIRED | RECEIVER))
            .collect(),
    ))
}

fn scan_candidates() -> io::Result<Vec<Candidate>> {
    let entries = match fs::read_dir(HIDRAW_CLASS) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut candidates = Vec::new();
    for entry in entries {
        if let Some(candidate) = read_candidate(&entry?.path())? {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn read_candidate(path: &Path) -> io::Result<Option<Candidate>> {
    let Some(name) = path.file_name().and_then(|word| word.to_str()) else {
        return Ok(None);
    };
    if name
        .strip_prefix("hidraw")
        .and_then(|index| index.parse::<u32>().ok())
        .is_none()
    {
        return Ok(None);
    }

    // Removal can invalidate any sysfs attribute between enumeration and reading.
    match inspect_candidate(path, name) {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            Ok(None)
        }
        result => result,
    }
}

fn inspect_candidate(path: &Path, name: &str) -> io::Result<Option<Candidate>> {
    let hid = fs::canonicalize(path.join("device"))?;
    let event = fs::read_to_string(hid.join("uevent"))?;
    let Some((bus, vendor, product)) = event
        .lines()
        .find_map(|line| parse_hid_id(line.strip_prefix("HID_ID=")?))
    else {
        return Ok(None);
    };
    let Some(connection) = connection_for(bus, vendor, product) else {
        return Ok(None);
    };
    let usb_parent = usb_parent(&hid, product)?;
    let descriptor = fs::read(hid.join("report_descriptor"))?;
    let capabilities = parse_descriptor(&descriptor)?;

    let label = event
        .lines()
        .find_map(|line| line.strip_prefix("HID_NAME="))
        .filter(|name| !name.is_empty())
        .unwrap_or("Attack Shark compatible HID")
        .chars()
        .filter(|ch| !ch.is_control())
        .collect();
    Ok(Some(Candidate {
        product,
        node: Path::new("/dev").join(name),
        usb_parent,
        connection,
        label,
        capabilities,
    }))
}

fn parse_hid_id(id: &str) -> Option<(u32, u16, u16)> {
    let mut parts = id.split(':');
    let bus = u32::from_str_radix(parts.next()?, 16).ok()?;
    let vendor = u16::from_str_radix(parts.next()?, 16).ok()?;
    let product = u16::from_str_radix(parts.next()?, 16).ok()?;
    parts.next().is_none().then_some((bus, vendor, product))
}

fn connection_for(bus: u32, vendor: u16, product: u16) -> Option<Connection> {
    if (bus, vendor) != (BUS_USB, VENDOR) {
        return None;
    }
    match product {
        SHARED_RECEIVER | FAST_RECEIVER => Some(Connection::Receiver),
        pid => Model::wired(pid).map(|_| Connection::Wired),
    }
}

fn usb_parent(hid: &Path, product: u16) -> io::Result<PathBuf> {
    for path in hid
        .ancestors()
        .take_while(|parent| parent.starts_with(SYS_DEVICES))
    {
        let vendor_attr = match fs::read_to_string(path.join("idVendor")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let product_attr = fs::read_to_string(path.join("idProduct"))?;
        if u16::from_str_radix(vendor_attr.trim(), 16) == Ok(VENDOR)
            && u16::from_str_radix(product_attr.trim(), 16) == Ok(product)
        {
            return Ok(path.to_owned());
        }
        break;
    }
    Err(invalid_data("HID has no matching physical USB parent"))
}

fn pair_candidates(mut candidates: Vec<Candidate>) -> Vec<Device> {
    candidates.sort_unstable_by(|left, right| left.node.cmp(&right.node));
    candidates
        .iter()
        .filter(|feature| feature.capabilities.config_channel())
        .map(|feature| {
            let monitor_path = if feature.capabilities.status_report {
                Some(feature.node.clone())
            } else {
                let mut monitors = candidates.iter().filter(|candidate| {
                    candidate.usb_parent == feature.usb_parent
                        && candidate.capabilities.status_report
                });
                match (monitors.next(), monitors.next()) {
                    (Some(monitor), None) => Some(monitor.node.clone()),
                    _ => None,
                }
            };
            Device {
                feature_path: feature.node.clone(),
                monitor_path,
                connection: feature.connection,
                label: feature.label.clone(),
                usb_parent: feature.usb_parent.clone(),
            }
        })
        .collect()
}

/// Read-only inventory. Vendor status input can identify a paired receiver without
/// sending feature reports. A silent or unopenable receiver stays unidentified.
pub struct DetectedDevice {
    pub node: PathBuf,
    pub connection: Connection,
    pub model: Option<&'static Model>,
    pub configuration: bool,
    pub status_node: Option<PathBuf>,
    pub polling_read: bool,
    pub polling_write: bool,
    pub dpi_read: bool,
    pub dpi_write: bool,
    pub button_read: bool,
    pub button_write: bool,
    pub fast_settings: bool,
    product: u16,
    usb_parent: PathBuf,
}

pub fn survey() -> io::Result<Vec<DetectedDevice>> {
    let candidates = scan_candidates()?;
    let mut groups: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for candidate in &candidates {
        groups
            .entry((&candidate.usb_parent, candidate.product))
            .or_default()
            .push(candidate);
    }
    let deadline = Instant::now() + DISCOVERY_WINDOW;
    Ok(groups
        .into_iter()
        .filter_map(|((parent, product), group)| {
            let model = Model::wired(product);
            let polling = model.and_then(Model::wired_polling);
            let dpi = model.and_then(|model| crate::model_wire::WiredDpi::for_slot(model.slot));
            let dpi_match = |candidate: &Candidate| {
                dpi.is_some_and(|layout| {
                    layout.can_read(|id| candidate.capabilities.feature_width(id))
                })
            };
            let polling_match = |candidate: &Candidate| {
                polling.map_or_else(
                    || {
                        model.is_some_and(|model| model.slot != 3)
                            && candidate.capabilities.feature_width(6) == Some(9)
                    },
                    |layout| layout.can_read(|id| candidate.capabilities.feature_width(id)),
                )
            };
            let preferred = group.iter().min_by_key(|node| {
                (
                    !dpi_match(node),
                    if polling.is_some() {
                        !polling_match(node)
                    } else {
                        !node.capabilities.config_channel()
                    },
                    !node.capabilities.status_report,
                    &node.node,
                )
            })?;
            let polling_read = polling_match(preferred)
                && group.iter().filter(|node| polling_match(node)).count() == 1;
            let dpi_read =
                dpi_match(preferred) && group.iter().filter(|node| dpi_match(node)).count() == 1;
            let mut monitors = group.iter().filter(|node| node.capabilities.status_report);
            let status_node = if preferred.capabilities.status_report {
                Some(preferred.node.clone())
            } else if let (Some(monitor), None) = (monitors.next(), monitors.next()) {
                Some(monitor.node.clone())
            } else {
                None
            };
            let button_read = model.is_some_and(|model| model.slot != 3)
                && if model.is_some_and(|model| (16..=18).contains(&model.slot)) {
                    dpi_read
                } else {
                    preferred.capabilities.feature_width(8) == Some(59)
                        && group
                            .iter()
                            .filter(|node| node.capabilities.feature_width(8) == Some(59))
                            .count()
                            == 1
                };
            let mut device = DetectedDevice {
                node: preferred.node.clone(),
                connection: preferred.connection,
                model,
                configuration: matches!(product, WIRED | RECEIVER)
                    && group
                        .iter()
                        .filter(|node| node.capabilities.config_channel())
                        .count()
                        == 1,
                status_node,
                polling_read,
                polling_write: polling_read
                    && polling.map_or_else(
                        || preferred.capabilities.feature_width(6) == Some(9),
                        |layout| layout.can_write(|id| preferred.capabilities.feature_width(id)),
                    ),
                dpi_read,
                dpi_write: dpi_read
                    && dpi.is_some_and(|layout| {
                        layout.can_write(|id| preferred.capabilities.feature_width(id))
                    }),
                button_read,
                button_write: button_read && preferred.capabilities.feature_width(8) == Some(59),
                fast_settings: false,
                usb_parent: parent.clone(),
                product,
            };
            if device.connection == Connection::Receiver {
                device.model = observe_receiver(&device, deadline);
                if device.model.is_some_and(|model| !model.supports_settings()) {
                    device.configuration = false;
                }
            }
            if product == RECEIVER
                && device
                    .model
                    .is_some_and(|model| (16..=18).contains(&model.slot))
            {
                let matches = |entry: &&Candidate| {
                    crate::model_wire::WiredPolling::Combined
                        .can_read(|id| entry.capabilities.feature_width(id))
                };
                let mut features = group.iter().copied().filter(matches);
                if let (Some(feature), None) = (features.next(), features.next()) {
                    let width = |id| feature.capabilities.feature_width(id);
                    device.node = feature.node.clone();
                    device.dpi_read = true;
                    device.dpi_write = width(4) == Some(56);
                    device.polling_read = true;
                    device.polling_write = width(6) == Some(9);
                    device.button_read = true;
                    device.button_write = width(8) == Some(59);
                }
            }
            if product == FAST_RECEIVER && device.model.is_some_and(|model| model.fast_receiver) {
                let mut ports = group
                    .iter()
                    .filter(|node| node.capabilities.fast_output_width().is_some());
                if let (Some(port), None) = (ports.next(), ports.next()) {
                    device.node = port.node.clone();
                    device.fast_settings = true;
                    device.dpi_read = true;
                    device.dpi_write = true;
                    device.polling_read = true;
                    device.polling_write = true;
                }
            }
            Some(device)
        })
        .collect())
}

fn observe_receiver(device: &DetectedDevice, deadline: Instant) -> Option<&'static Model> {
    let mut file = open_status(device).ok()?;
    read_vendor_status(&mut file, device.product, deadline)
        .ok()?
        .map(|status| status.model)
}

fn open_status(device: &DetectedDevice) -> io::Result<File> {
    let input = device.status_node.as_deref().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no unique mouse status collection")
    })?;
    let file = open_node(input, false)?;
    if !verify_usb_node(&file, &device.usb_parent, device.product)?.status_report {
        return Err(invalid_data(
            "opened HID no longer has the expected status collection",
        ));
    }
    Ok(file)
}

pub fn detected_identity(device: &DetectedDevice) -> io::Result<String> {
    Ok(endpoint_token(&fs::metadata(&device.node)?))
}

/// A checked input collection with no feature-output access.
pub struct StatusReader {
    file: File,
    receiver_pid: u16,
    model: Option<&'static Model>,
}

impl StatusReader {
    pub fn open(device: &DetectedDevice, expected: Option<&str>) -> io::Result<Self> {
        let file = open_status(device)?;
        if let Some(expected) = expected {
            if detected_identity(device)? != expected {
                return Err(invalid_data(
                    "the selected USB endpoint changed; refresh the device list",
                ));
            }
        }
        // Wired PIDs use the common status envelope and are bound to one tag.
        let (receiver_pid, model) = if device.connection == Connection::Wired {
            (SHARED_RECEIVER, device.model)
        } else {
            (device.product, device.model)
        };
        Ok(Self {
            file,
            receiver_pid,
            model,
        })
    }

    pub fn read(&mut self, timeout: Duration) -> io::Result<Option<VendorStatus>> {
        let deadline = Instant::now() + timeout;
        for _ in 0..16 {
            let Some(status) = read_vendor_status(&mut self.file, self.receiver_pid, deadline)?
            else {
                return Ok(None);
            };
            if self.model.is_none_or(|model| status.model == model) {
                return Ok(Some(status));
            }
        }
        Ok(None)
    }
}

fn read_vendor_status(
    file: &mut File,
    product: u16,
    deadline: Instant,
) -> io::Result<Option<VendorStatus>> {
    let mut input = [0; MAX_INPUT];
    let mut fd = pollfd {
        fd: file.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    };
    for _ in 0..16 {
        let timeout = poll_millis(deadline.saturating_duration_since(Instant::now()));
        // SAFETY: fd describes exactly one open borrowed file.
        let ready = unsafe { poll(&mut fd, 1, timeout) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            return Ok(None);
        }
        if fd.revents & POLLIN == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "HID status device disconnected",
            ));
        }
        match file.read(&mut input) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "HID status device closed",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
            Ok(size) => {
                if let Some(status) = VendorStatus::parse(product, &input[..size]) {
                    return Ok(Some(status));
                }
            }
        }
    }
    Ok(None)
}

fn parse_descriptor(mut bytes: &[u8]) -> io::Result<Capabilities> {
    if bytes.is_empty() || bytes.len() >= MAX_DESCRIPTOR {
        return Err(invalid_data("invalid HID report descriptor size"));
    }
    let mut capability = Capabilities::default();
    let mut globals = Globals::default();
    let mut stack = Vec::new();
    let mut local_usage = None;
    let mut top_usage = None;
    let mut depth = 0usize;

    while let Some((&prefix, remaining)) = bytes.split_first() {
        if prefix == 0xfe {
            let size = remaining.first().ok_or_else(bad_descriptor)?;
            bytes = remaining
                .get(2 + usize::from(*size)..)
                .ok_or_else(bad_descriptor)?;
            continue;
        }
        let size = match prefix & 3 {
            3 => 4,
            size => usize::from(size),
        };
        let (body, after) = remaining
            .split_at_checked(size)
            .ok_or_else(bad_descriptor)?;
        bytes = after;
        let mut value = [0; 4];
        value[..size].copy_from_slice(body);
        let value = u32::from_le_bytes(value);
        let kind = (prefix >> 2) & 3;
        let tag = prefix >> 4;

        match (kind, tag) {
            (1, 0) => globals.usage_page = u16::try_from(value).map_err(|_| bad_descriptor())?,
            (1, 7) => globals.report_size = value,
            (1, 9) => globals.report_count = value,
            (1, 8) => {
                if size != 1 || value == 0 {
                    return Err(bad_descriptor());
                }
                globals.report_id = value as u8;
            }
            (1, 10) => stack.push(globals),
            (1, 11) => globals = stack.pop().ok_or_else(bad_descriptor)?,
            (2, 0) if local_usage.is_none() => {
                let page = if size == 4 {
                    (value >> 16) as u16
                } else {
                    globals.usage_page
                };
                local_usage = Some((page, value as u16));
            }
            (0, 10) => {
                if depth == 0 {
                    top_usage = Some(local_usage.unwrap_or((globals.usage_page, 0)));
                }
                depth += 1;
            }
            (0, 12) => {
                depth = depth.checked_sub(1).ok_or_else(bad_descriptor)?;
                if depth == 0 {
                    top_usage = None;
                }
            }
            (0, 8)
                if matches!(top_usage, Some((0x0a, 0) | (0xff00, 1)))
                    && globals.report_id == 0x03 =>
            {
                capability.status_report = true;
            }
            (0, tag @ (8 | 9)) if top_usage == Some((0xff01, 2)) && globals.report_id == 4 => {
                let bits = globals
                    .report_size
                    .checked_mul(globals.report_count)
                    .ok_or_else(bad_descriptor)?;
                let total = if tag == 8 {
                    &mut capability.fast_input_bits
                } else {
                    &mut capability.fast_output_bits
                };
                *total = total.checked_add(bits).ok_or_else(bad_descriptor)?;
            }
            (0, 11) if top_usage == Some((0x0b, 0)) => {
                capability.features.insert(globals.report_id);
                let bits = globals
                    .report_size
                    .checked_mul(globals.report_count)
                    .ok_or_else(bad_descriptor)?;
                let total = capability
                    .feature_bits
                    .entry(globals.report_id)
                    .or_default();
                *total = total.checked_add(bits).ok_or_else(bad_descriptor)?;
            }
            _ => {}
        }
        if kind == 0 {
            local_usage = None;
        }
    }

    if depth != 0 || !stack.is_empty() {
        return Err(bad_descriptor());
    }
    Ok(capability)
}

fn endpoint_token(metadata: &fs::Metadata) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.rdev(),
        metadata.ctime(),
        metadata.ctime_nsec()
    )
}

pub fn endpoint_identity(device: &Device) -> io::Result<String> {
    Ok(endpoint_token(&fs::metadata(&device.feature_path)?))
}

pub fn pointer_activity(
    device: &Device,
    expected: Option<&str>,
    out: &mut dyn Write,
) -> io::Result<()> {
    if device.connection != Connection::Wired
        || expected.is_none_or(|token| endpoint_identity(device).as_deref().ok() != Some(token))
    {
        return Err(invalid_data(
            "select the current wired R1 for pointer activity",
        ));
    }
    let parent = device.usb_parent.canonicalize()?;
    let mut matches = Vec::new();
    for entry in fs::read_dir("/sys/class/input")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(number) = name.to_str().and_then(|name| name.strip_prefix("event")) else {
            continue;
        };
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(path) = entry.path().canonicalize() else {
            continue;
        };
        if !path.starts_with(&parent) {
            continue;
        }
        let bits = fs::read_to_string(path.join("device/capabilities/rel"))?;
        if bits
            .split_ascii_whitespace()
            .last()
            .and_then(|word| u64::from_str_radix(word, 16).ok())
            .is_some_and(|mask| mask & 3 == 3)
        {
            matches.push((Path::new("/dev/input").join(name), path));
        }
    }
    let [(node, path)] = matches.as_slice() else {
        return Err(invalid_data("no unique wired R1 pointer collection"));
    };
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK | libc::O_NOFOLLOW)
        .open(node)?;
    let metadata = file.metadata()?;
    let (major, minor) = device_numbers(metadata.rdev());
    if !metadata.file_type().is_char_device()
        || fs::canonicalize(format!("/sys/dev/char/{major}:{minor}"))? != *path
    {
        return Err(invalid_data("the wired pointer endpoint changed"));
    }
    writeln!(out, "r1-status\t1")?;
    out.flush()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut shown = None::<Instant>;
    let mut activity = false;
    let mut fd = pollfd {
        fd: file.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    };
    const TIME: usize = size_of::<libc::timeval>();
    let mut event = [0; TIME + 8];
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        // The descriptor belongs to the verified File for the entire read loop.
        let ready = unsafe { poll(&mut fd, 1, poll_millis(remaining)) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            break;
        }
        if fd.revents & (POLLERR | POLLHUP | POLLNVAL) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "wired pointer disconnected",
            ));
        }
        match file.read_exact(&mut event) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
        let kind = u16::from_ne_bytes([event[TIME], event[TIME + 1]]);
        let code = u16::from_ne_bytes([event[TIME + 2], event[TIME + 3]]);
        if kind == 2
            && code < 2
            && shown.is_none_or(|time| time.elapsed() >= Duration::from_millis(250))
        {
            writeln!(out, "pointer-move")?;
            out.flush()?;
            shown = Some(Instant::now());
            activity = true;
        } else if kind == 1 && (0x110..=0x117).contains(&code) {
            writeln!(out, "pointer-button")?;
            out.flush()?;
            activity = true;
        }
    }
    if !activity {
        writeln!(out, "idle")?;
    }
    Ok(())
}

/// Revalidate a scoped wired collection before running an audited polling transaction.
pub struct FastReports {
    file: File,
    width: usize,
}

impl crate::fast_receiver::Reports for FastReports {
    fn send(&mut self, packet: &[u8]) -> io::Result<()> {
        let mut buffer = [0; 65];
        let destination = buffer
            .get_mut(..packet.len())
            .ok_or_else(|| invalid_data("oversized receiver report"))?;
        destination.copy_from_slice(packet);
        if packet.len() > self.width && packet[self.width..].iter().any(|&byte| byte != 0) {
            return Err(invalid_data(
                "receiver descriptor would truncate report data",
            ));
        }
        if self.file.write(&buffer[..self.width])? != self.width {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short receiver HID output write",
            ));
        }
        Ok(())
    }

    fn receive(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        let mut descriptor = pollfd {
            fd: self.file.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        // SAFETY: poll can modify one borrowed descriptor.
        let ready = unsafe { poll(&mut descriptor, 1, poll_millis(timeout)) };
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        if ready == 0 {
            return Ok(None);
        }
        if descriptor.revents & POLLIN == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "receiver disconnected",
            ));
        }
        let mut input = [0; 64];
        match self.file.read(&mut input) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "receiver closed",
            )),
            Ok(len) => Ok(Some(input[..len].to_vec())),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn open_fast(
    device: &DetectedDevice,
    expected: Option<&str>,
) -> io::Result<crate::fast_receiver::Session<FastReports>> {
    let model = device
        .model
        .filter(|model| model.fast_receiver)
        .filter(|_| device.fast_settings && device.product == FAST_RECEIVER)
        .ok_or_else(|| invalid_data("expected an identified high-rate receiver"))?;
    let file = open_node(&device.node, true)?;
    if let Some(expected) = expected {
        if endpoint_token(&file.metadata()?) != expected {
            return Err(invalid_data("the selected receiver endpoint changed"));
        }
    }
    let width = verify_usb_node(&file, &device.usb_parent, FAST_RECEIVER)?
        .fast_output_width()
        .ok_or_else(|| invalid_data("the receiver report collection changed"))?;
    // SAFETY: flock borrows the live descriptor until the session closes.
    if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut status = StatusReader::open(device, expected)?;
    if status.read(Duration::from_secs(2))?.is_none() {
        return Err(invalid_data("receiver did not confirm the selected mouse"));
    }
    crate::fast_receiver::Session::new(model, FastReports { file, width })
}

pub fn update_model_polling(
    device: &DetectedDevice,
    expected: Option<&str>,
    desired: Option<u16>,
) -> io::Result<u16> {
    if device.fast_settings {
        return open_fast(device, expected)?.polling(desired);
    }
    let model = device
        .model
        .filter(|model| !model.supports_settings())
        .filter(|_| device.polling_read && (desired.is_none() || device.polling_write))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "no documented polling transaction on this endpoint",
            )
        })?;
    let (mut hid, caps) = open_model_feature(device, expected)?;
    let width = |id| caps.feature_width(id);
    if let Some(layout) = model.wired_polling() {
        layout.update(&mut hid, width, desired)
    } else {
        crate::model_wire::update_direct_polling(model, &mut hid, width, desired)
    }
}

pub fn update_model_dpi(
    device: &DetectedDevice,
    expected: Option<&str>,
    edit: Option<impl FnOnce(&mut crate::settings::Dpi) -> io::Result<()>>,
) -> io::Result<crate::settings::Dpi> {
    if device.fast_settings {
        return open_fast(device, expected)?.dpi(edit);
    }
    let layout = device
        .model
        .and_then(|model| crate::model_wire::WiredDpi::for_slot(model.slot))
        .filter(|_| device.dpi_read && (edit.is_none() || device.dpi_write))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "DPI is unavailable for this model",
            )
        })?;
    let (mut hid, capabilities) = open_model_feature(device, expected)?;
    layout.update(&mut hid, |id| capabilities.feature_width(id), edit)
}

pub fn read_model_profile(
    device: &DetectedDevice,
    expected: Option<&str>,
) -> io::Result<crate::settings::Legacy> {
    let route = device
        .model
        .and_then(Model::wired_polling)
        .filter(|_| device.polling_read)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "no audited model profile reader",
            )
        })?;
    let (mut hid, capabilities) = open_model_feature(device, expected)?;
    crate::model_wire::read_combined_profile(route, &mut hid, |id| capabilities.feature_width(id))
}

fn open_v3_family(
    device: &DetectedDevice,
    expected: Option<&str>,
) -> io::Result<(Hidraw, Capabilities)> {
    if device
        .model
        .is_none_or(|model| !(16..=18).contains(&model.slot))
        || !device.polling_read
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "expected a wired V3 Pro, V6, or V3",
        ));
    }
    open_model_feature(device, expected)
}

fn open_model_settings(
    device: &DetectedDevice,
    expected: Option<&str>,
) -> io::Result<(&'static Model, Hidraw, Capabilities)> {
    let model = device
        .model
        .filter(|model| !model.supports_settings() && (device.dpi_read || device.button_read))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "expected a unique wired model settings collection",
            )
        })?;
    let (hid, caps) = open_model_feature(device, expected)?;
    Ok((model, hid, caps))
}

pub fn archive_model_profile(
    device: &DetectedDevice,
    expected: Option<&str>,
) -> io::Result<Vec<u8>> {
    let (model, mut hid, caps) = open_model_settings(device, expected)?;
    crate::model_wire::archive_model_profile(model, &mut hid, |id| caps.feature_width(id))
        .map(|archive| archive.to_vec())
}

pub fn restore_model_profile(
    device: &DetectedDevice,
    expected: Option<&str>,
    archive: &[u8],
) -> io::Result<usize> {
    let (model, mut hid, caps) = open_model_settings(device, expected)?;
    crate::model_wire::restore_model_profile(model, &mut hid, |id| caps.feature_width(id), archive)
}

pub fn reset_model_full(device: &DetectedDevice, expected: Option<&str>) -> io::Result<usize> {
    let (model, mut hid, caps) = open_model_settings(device, expected)?;
    crate::model_wire::reset_model_full(model, &mut hid, |id| caps.feature_width(id))
}

pub fn reset_model_settings(device: &DetectedDevice, expected: Option<&str>) -> io::Result<usize> {
    let (model, mut hid, caps) = open_model_settings(device, expected)?;
    crate::model_wire::reset_model_settings(model, &mut hid, |id| caps.feature_width(id))
}

pub fn apply_model_windows(
    device: &DetectedDevice,
    expected: Option<&str>,
    plan: &crate::vendor_plan::Plan,
) -> io::Result<usize> {
    let (model, mut hid, caps) = open_model_settings(device, expected)?;
    crate::model_wire::apply_model_windows(model, &mut hid, |id| caps.feature_width(id), plan)
}

pub fn update_model_button(
    device: &DetectedDevice,
    expected: Option<&str>,
    edit: Option<(usize, crate::actions::Action)>,
) -> io::Result<[u8; crate::actions::BUTTONS_LEN]> {
    if !device.button_read || edit.is_some() && !device.button_write {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no matched button mapping collection",
        ));
    }
    let (model, mut hid, caps) = open_model_settings(device, expected)?;
    crate::model_wire::update_model_button(model, &mut hid, |id| caps.feature_width(id), edit)
}

pub fn read_v3_family_macro(
    device: &DetectedDevice,
    expected: Option<&str>,
    slot: u8,
) -> io::Result<Vec<Vec<u8>>> {
    let (mut hid, capabilities) = open_v3_family(device, expected)?;
    crate::model_wire::read_v3_family_macro(&mut hid, |id| capabilities.feature_width(id), slot)
}

pub fn archive_model_macro(
    device: &DetectedDevice,
    expected: Option<&str>,
    slot: u8,
) -> io::Result<Vec<u8>> {
    let (model, mut hid, caps) = open_model_settings(device, expected)?;
    crate::model_wire::archive_model_macro(model, &mut hid, |id| caps.feature_width(id), slot)
}

pub fn restore_model_macro(
    device: &DetectedDevice,
    expected: Option<&str>,
    archive: &[u8],
) -> io::Result<usize> {
    let (model, mut hid, caps) = open_model_settings(device, expected)?;
    crate::model_wire::restore_model_macro(model, &mut hid, |id| caps.feature_width(id), archive)
}

pub fn update_model_timing(
    device: &DetectedDevice,
    expected: Option<&str>,
    edit: Option<impl FnOnce(&mut crate::settings::Timing) -> io::Result<()>>,
) -> io::Result<crate::settings::Timing> {
    if device.fast_settings {
        return open_fast(device, expected)?.timing(edit);
    }
    let (model, mut hid, caps) = open_model_settings(device, expected)?;
    crate::model_wire::update_model_timing(model, &mut hid, |id| caps.feature_width(id), edit)
}

pub fn update_x11_timing(
    device: &DetectedDevice,
    expected: Option<&str>,
    edit: impl FnOnce(&mut crate::settings::Timing) -> io::Result<()>,
) -> io::Result<crate::settings::Timing> {
    if device.model.is_none_or(|model| model.slot != 4) || !device.polling_read {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "expected the wired X11 feature collection",
        ));
    }
    let (mut hid, capabilities) = open_model_feature(device, expected)?;
    crate::model_wire::update_x11_timing(&mut hid, |id| capabilities.feature_width(id), edit)
}

fn open_model_feature(
    device: &DetectedDevice,
    expected: Option<&str>,
) -> io::Result<(Hidraw, Capabilities)> {
    let receiver = device.connection == Connection::Receiver;
    if receiver
        && (device.product != RECEIVER
            || !device.dpi_read
            || device
                .model
                .is_none_or(|model| !(16..=18).contains(&model.slot)))
    {
        return Err(invalid_data(
            "no matched V3-family receiver settings endpoint",
        ));
    }
    let feature = open_node(&device.node, true)?;
    if let Some(expected) = expected {
        if endpoint_token(&feature.metadata()?) != expected {
            return Err(invalid_data(
                "the selected USB endpoint changed; refresh the device list",
            ));
        }
    }
    let capabilities = verify_usb_node(&feature, &device.usb_parent, device.product)?;
    // SAFETY: flock operates on a live owned file descriptor.
    if unsafe { flock(feature.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if receiver {
        let mut status_file = open_status(device)?;
        let status = read_vendor_status(
            &mut status_file,
            device.product,
            Instant::now() + Duration::from_secs(2),
        )?;
        if status.map(|status| status.model) != device.model {
            return Err(invalid_data("receiver did not confirm the selected mouse"));
        }
    }
    let hid = Hidraw {
        feature,
        feature_ids: capabilities.features,
        monitor: None,
        input: Vec::new(),
        connection: device.connection,
        features: Features::Standard,
    };
    Ok((hid, capabilities))
}

pub fn open(device: &Device) -> io::Result<Hidraw> {
    open_identified(device, None)
}

pub fn open_identified(device: &Device, expected: Option<&str>) -> io::Result<Hidraw> {
    let _timing = crate::timing::Span::start("linux.open_identified");
    let feature = open_node(&device.feature_path, true)?;
    if let Some(expected) = expected {
        if endpoint_token(&feature.metadata()?) != expected {
            return Err(invalid_data(
                "the selected USB endpoint changed; refresh the device list",
            ));
        }
    }
    let capabilities = verify_node(&feature, device)?;
    if !capabilities.config_channel() {
        return Err(invalid_data(
            "opened HID no longer has a matching configuration collection",
        ));
    }

    // A lock on this open file description serializes our commands without a host lock file.
    // SAFETY: the borrowed file owns an open descriptor and the operations are Linux flock flags.
    let lock = unsafe { flock(feature.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if lock != 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("could not lock {} ({error})", device.feature_path.display()),
        ));
    }
    let features = if device.connection == Connection::Wired
        && fs::read_to_string(device.usb_parent.join("bcdDevice"))?.trim() == "0104"
    {
        if capabilities.wired_v104_descriptor {
            Features::Prepared
        } else {
            Features::Blocked
        }
    } else {
        Features::Standard
    };
    let monitor = device
        .monitor_path
        .as_ref()
        .map(|path| {
            let file = open_node(path, false)?;
            if !verify_node(&file, device)?.status_report {
                return Err(invalid_data(
                    "opened HID no longer has the expected status collection",
                ));
            }
            Ok(file)
        })
        .transpose()?;

    let input = if monitor.is_some() {
        vec![0; MAX_INPUT]
    } else {
        Vec::new()
    };
    Ok(Hidraw {
        feature,
        feature_ids: capabilities.features,
        monitor,
        input,
        connection: device.connection,
        features,
    })
}

fn open_node(path: &Path, writable: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(writable)
        .custom_flags(O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot open {} ({error})", path.display()),
            )
        })
}

fn verify_node(file: &File, device: &Device) -> io::Result<Capabilities> {
    let product = match device.connection {
        Connection::Wired => WIRED,
        Connection::Receiver => RECEIVER,
        Connection::Bluetooth => return Err(invalid_data("Bluetooth has no USB hidraw endpoint")),
    };
    verify_usb_node(file, &device.usb_parent, product)
}

fn verify_usb_node(file: &File, parent: &Path, product: u16) -> io::Result<Capabilities> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_char_device() {
        return Err(invalid_data("expected a hidraw character device"));
    }
    let mut info = HidrawInfo::default();
    // SAFETY: the output pointer is to a live kernel-ABI struct of the advertised size.
    let result = unsafe {
        ioctl(
            file.as_raw_fd(),
            read_request(0x03, size_of::<HidrawInfo>()),
            &mut info as *mut HidrawInfo,
        )
    };
    ioctl_result(result)?;
    if (info.bus, info.vendor, info.product) != (BUS_USB, VENDOR, product) {
        return Err(invalid_data(
            "opened HID bus or USB ID changed since discovery",
        ));
    }
    let (major, minor) = device_numbers(metadata.rdev());
    let sysfs = fs::canonicalize(format!("/sys/dev/char/{major}:{minor}/device"))?;
    if usb_parent(&sysfs, info.product)? != parent {
        return Err(invalid_data(
            "opened HID belongs to a different physical USB device",
        ));
    }

    let mut size: c_int = 0;
    // SAFETY: the output pointer is writable for sizeof(int) bytes.
    let result = unsafe {
        ioctl(
            file.as_raw_fd(),
            read_request(0x01, size_of::<c_int>()),
            &mut size as *mut c_int,
        )
    };
    ioctl_result(result)?;
    let size = usize::try_from(size).map_err(|_| bad_descriptor())?;
    if !(1..MAX_DESCRIPTOR).contains(&size) {
        return Err(bad_descriptor());
    }
    let mut descriptor = RawDescriptor {
        size: size as u32,
        bytes: [0; MAX_DESCRIPTOR],
    };
    // SAFETY: the live descriptor struct has the kernel layout and a validated request size.
    let result = unsafe {
        ioctl(
            file.as_raw_fd(),
            read_request(0x02, size_of::<RawDescriptor>()),
            &mut descriptor as *mut RawDescriptor,
        )
    };
    ioctl_result(result)?;
    let bytes = &descriptor.bytes[..size];
    let mut capabilities = parse_descriptor(bytes)?;
    capabilities.wired_v104_descriptor = bytes == WIRED_V104_DESCRIPTOR;
    Ok(capabilities)
}

fn device_numbers(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xffff_f000);
    let minor = (dev & 0xff) | ((dev >> 12) & 0xffff_ff00);
    (major as u32, minor as u32)
}

const fn read_request(number: u8, size: usize) -> c_ulong {
    io_request(2, number, size)
}

const fn feature_request(number: u8, length: usize) -> c_ulong {
    // HIDIOCGFEATURE and HIDIOCSFEATURE both use the read/write direction encoding.
    io_request(3, number, length)
}

const fn io_request(direction: u32, number: u8, size: usize) -> c_ulong {
    ((direction << 30) | ((size as u32) << 16) | ((b'H' as u32) << 8) | number as u32) as c_ulong
}

fn ioctl_result(result: c_int) -> io::Result<usize> {
    usize::try_from(result).map_err(|_| io::Error::last_os_error())
}

fn feature_error(error: io::Error, operation: &str, report_id: u8) -> io::Error {
    if error.raw_os_error() == Some(libc::ENODEV) {
        io::Error::new(
            io::ErrorKind::NotConnected,
            format!(
                "R1 disconnected during feature {operation} 0x{report_id:02x}; check USB mode and run list"
            ),
        )
    } else {
        error
    }
}

fn bad_descriptor() -> io::Error {
    invalid_data("malformed HID report descriptor")
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn prepared_query(report: &[u8]) -> bool {
    let &[0xa0, kind, length, 0, slot, 0, 0, 0] = report else {
        return false;
    };
    let matches = |report: Report| report.id() == kind && report.length() == usize::from(length);
    (slot == 0 && (matches(Report::Version) || matches(Report::ProfileRange)))
        || (slot == 1 && (matches(Report::Legacy) || Report::SETTINGS.into_iter().any(matches)))
        || ((1..=32).contains(&slot) && matches(Report::Macro))
}

fn audited_settings_write(report: &[u8]) -> bool {
    report.get(2) == Some(&1)
        && Report::SETTINGS
            .into_iter()
            .any(|kind| kind.validate(report).is_ok())
}

impl Hidraw {
    fn validate_feature(&self, id: u8, length: usize) -> io::Result<()> {
        if self.features == Features::Blocked {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "feature I/O is disabled for wired R1 USB revision 1.04 after a bootloader transition",
            ));
        }
        if !(2..=MAX_FEATURE).contains(&length) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid HID feature length",
            ));
        }
        if id == 0 || !self.feature_ids.contains(id) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("feature report 0x{id:02x} is absent from the configuration descriptor"),
            ));
        }
        Ok(())
    }
    fn send_feature(&self, report: &[u8]) -> io::Result<()> {
        let id = report.first().copied().unwrap_or(0);
        self.validate_feature(id, report.len())?;
        // SAFETY: HIDIOCSFEATURE copies the readable buffer in, using its exact length.
        let result = unsafe {
            ioctl(
                self.feature.as_raw_fd(),
                feature_request(0x06, report.len()),
                report.as_ptr(),
            )
        };
        if ioctl_result(result).map_err(|error| feature_error(error, "send", id))? != report.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short HID feature write",
            ));
        }
        Ok(())
    }
    fn receive_feature(&self, id: u8, length: usize) -> io::Result<Vec<u8>> {
        self.validate_feature(id, length)?;
        let mut buffer = vec![0; length];
        buffer[0] = id;
        // SAFETY: the writable buffer spans exactly the size encoded in HIDIOCGFEATURE.
        let result = unsafe {
            ioctl(
                self.feature.as_raw_fd(),
                feature_request(0x07, length),
                buffer.as_mut_ptr(),
            )
        };
        let received = ioctl_result(result).map_err(|error| feature_error(error, "read", id))?;
        if received == 0 || received > length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "feature 0x{id:02x} requested {length} bytes; got {received} bytes starting with 0x{:02x}",
                    buffer[0]
                ),
            ));
        }
        buffer.truncate(received);
        Ok(buffer)
    }
    fn prepare_control(&self) -> io::Result<()> {
        let _timing = crate::timing::Span::start("linux.prepare_control");
        self.send_feature(&PREPARE_QUERY)?;
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(25));
            let receipt = self.receive_feature(0xa0, 8)?;
            match receipt.as_slice() {
                [0xa0, 0, ..] => return Ok(()),
                [0xa0, 1, ..] => {
                    // A non-query GET consumes an older reply, whatever its report ID.
                    self.receive_feature(PREPARE_QUERY[0], PREPARE_QUERY.len())?;
                }
                _ => return Err(invalid_data("invalid wired query readiness response")),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "wired query readiness stayed set",
        ))
    }
}

impl Transport for Hidraw {
    fn connection(&self) -> Connection {
        self.connection
    }

    fn firmware_identity(&self, reported: u16) -> u16 {
        // This revision's 0x0b reply stores the timing mode byte at offset 3.
        if self.features == Features::Prepared && reported >> 8 == 0x10 {
            0x1000
        } else {
            reported
        }
    }

    fn macro_slots(&self) -> u8 {
        if self.features == Features::Prepared {
            32
        } else {
            18
        }
    }

    fn query_attempts(&self) -> u8 {
        if self.features == Features::Prepared || self.connection == Connection::Receiver {
            3
        } else {
            1
        }
    }

    fn query_delay(&self) -> Duration {
        Duration::from_millis(if self.connection == Connection::Receiver {
            500
        } else {
            100
        })
    }

    fn set_feature(&mut self, report: &[u8]) -> io::Result<()> {
        let _timing = crate::timing::Span::start("linux.set_feature");
        let id = report.first().copied().unwrap_or(0);
        self.validate_feature(id, report.len())?;
        if self.features != Features::Prepared {
            return self.send_feature(report);
        }
        if prepared_query(report) {
            self.prepare_control()?;
            return self.send_feature(report);
        }
        if audited_settings_write(report) || report == RESET_PREAMBLE {
            self.prepare_control()?;
            self.send_feature(report)?;
            std::thread::sleep(Duration::from_millis(200));
            return Ok(());
        }
        let message = if id == Report::Macro.id() {
            "wired R1 USB revision 1.04 has no verified erased-slot restoration command"
        } else {
            "this feature write is outside the audited wired R1 USB revision 1.04 protocol"
        };
        Err(io::Error::new(io::ErrorKind::Unsupported, message))
    }

    fn get_feature(&mut self, id: u8, length: usize) -> io::Result<Vec<u8>> {
        let _timing = crate::timing::Span::start("linux.get_feature");
        let result = self.receive_feature(id, length)?;
        if result[0] != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "feature 0x{id:02x} requested {length} bytes; got {} bytes starting with 0x{:02x}",
                    result.len(),
                    result[0]
                ),
            ));
        }
        Ok(result)
    }

    fn read_input(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        let monitor = self.monitor.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "no unambiguous HID status collection",
            )
        })?;
        let start = Instant::now();
        let mut first_poll = true;
        loop {
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() && !first_poll {
                return Ok(None);
            }
            first_poll = false;
            let mut descriptor = pollfd {
                fd: monitor.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            };
            // SAFETY: poll may modify one live descriptor, and the count matches that buffer.
            let ready = unsafe { poll(&mut descriptor, 1, poll_millis(remaining)) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                continue;
            }
            if descriptor.revents & POLLIN == 0 {
                if descriptor.revents & (POLLERR | POLLHUP | POLLNVAL) != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "HID status device disconnected",
                    ));
                }
                continue;
            }
            match monitor.read(&mut self.input) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "HID status device closed",
                    ));
                }
                Ok(size) => return Ok(Some(self.input[..size].to_vec())),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

fn poll_millis(duration: Duration) -> c_int {
    let millis = duration.as_nanos().div_ceil(1_000_000);
    millis.min(c_int::MAX as u128) as c_int
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    const FEATURE_DESCRIPTOR: &[u8] = &[
        0x05, 0x0b, 0x09, 0, 0xa1, 1, 0x85, 4, 0xb1, 2, 0x85, 5, 0xb1, 2, 0x85, 6, 0xb1, 2, 0x85,
        0xa0, 0xb1, 2, 0x85, 0x0b, 0xb1, 2, 0xc0,
    ];
    const MONITOR_DESCRIPTOR: &[u8] = &[0x0b, 0, 0, 0x0a, 0, 0xa1, 1, 0x85, 3, 0x81, 2, 0xc0];

    #[test]
    fn receives_all_model_status_tags_from_socket_fixtures() {
        use std::os::unix::net::UnixDatagram;

        for model in crate::models::MODELS {
            assert_eq!(
                connection_for(BUS_USB, VENDOR, model.wired_pid),
                Some(Connection::Wired)
            );
            assert_eq!(connection_for(BUS_USB, VENDOR + 1, model.wired_pid), None);
            let (sender, input) = UnixDatagram::pair().unwrap();
            let mut reader = StatusReader {
                file: File::from(OwnedFd::from(input)),
                receiver_pid: SHARED_RECEIVER,
                model: Some(&crate::models::MODELS[usize::from(model.slot) - 1]),
            };
            sender.send(&[0x03, 0xff, 0x10, 1, 0]).unwrap();
            sender
                .send(&[0x03, model.receiver_tag, 0x10, 4, 0])
                .unwrap();
            let status = reader.read(Duration::from_millis(20)).unwrap().unwrap();
            assert_eq!(*status.model, model);
            assert_eq!((status.class, status.value), (0x10, 4));
        }
    }

    #[test]
    fn split_polling_requires_the_exact_feature_report_widths() {
        let widths = [
            0x05, 0x0b, 0x09, 0, 0xa1, 1, 0x75, 8, 0x85, 0xa0, 0x95, 7, 0xb1, 2, 0x85, 0x0b, 0x95,
            7, 0xb1, 2, 0x85, 0x06, 0x95, 8, 0xb1, 2, 0xc0,
        ];
        let can_read = |capabilities: Capabilities| {
            crate::model_wire::WiredPolling::Versioned(0x55)
                .can_read(|id| capabilities.feature_width(id))
        };
        assert!(can_read(parse_descriptor(&widths).unwrap()));
        for index in [11, 17, 23] {
            let mut changed = widths;
            changed[index] += 1;
            assert!(!can_read(parse_descriptor(&changed).unwrap()));
        }
    }

    #[test]
    fn recognizes_split_and_combined_collections() {
        let feature = parse_descriptor(FEATURE_DESCRIPTOR).unwrap();
        assert!(feature.config_channel());
        assert!(feature.features.contains(0xa0));
        assert!(!feature.status_report);
        let monitor = parse_descriptor(MONITOR_DESCRIPTOR).unwrap();
        assert!(monitor.status_report);
        assert!(!monitor.config_channel());
        let descriptor = [FEATURE_DESCRIPTOR, MONITOR_DESCRIPTOR].concat();
        let combined = parse_descriptor(&descriptor).unwrap();
        assert!(combined.config_channel());
        assert!(combined.status_report);
    }

    #[test]
    fn rejects_incomplete_or_unbalanced_hid_items() {
        for malformed in [
            &[][..],
            &[0xc0],
            &[0xb4],
            &[0xfe, 4, 0, 1],
            &[0x05],
            &[0x85, 0],
            &[0xa1, 1],
        ] {
            assert!(parse_descriptor(malformed).is_err(), "{malformed:x?}");
        }
    }

    #[test]
    fn checks_discovery_pairing_with_matching_usb_parent() {
        let make = |node: &str, parent: &str, descriptor| Candidate {
            product: RECEIVER,
            node: PathBuf::from(node),
            usb_parent: PathBuf::from(parent),
            connection: Connection::Receiver,
            label: "Receiver".into(),
            capabilities: parse_descriptor(descriptor).unwrap(),
        };
        let devices = pair_candidates(vec![
            make("/dev/hidraw1", "/usb/1", FEATURE_DESCRIPTOR),
            make("/dev/hidraw2", "/usb/2", MONITOR_DESCRIPTOR),
            make("/dev/hidraw3", "/usb/1", MONITOR_DESCRIPTOR),
            make("/dev/hidraw4", "/usb/3", FEATURE_DESCRIPTOR),
        ]);
        assert_eq!(devices.len(), 2);
        assert_eq!(
            devices[0].monitor_path.as_deref(),
            Some(Path::new("/dev/hidraw3"))
        );
        assert_eq!(devices[1].monitor_path, None);
    }

    #[test]
    fn refuses_ambiguous_monitors_and_prefers_the_same_node() {
        let make = |name, descriptor| Candidate {
            product: RECEIVER,
            node: PathBuf::from(name),
            usb_parent: PathBuf::from("/usb/1"),
            connection: Connection::Receiver,
            label: String::new(),
            capabilities: parse_descriptor(descriptor).unwrap(),
        };
        let all = [FEATURE_DESCRIPTOR, MONITOR_DESCRIPTOR].concat();
        let devices = pair_candidates(vec![
            make("feature", FEATURE_DESCRIPTOR),
            make("monitor-1", MONITOR_DESCRIPTOR),
            make("monitor-2", MONITOR_DESCRIPTOR),
            make("combined", &all),
        ]);
        assert_eq!(devices[0].monitor_path, Some(PathBuf::from("combined")));
        assert_eq!(devices[1].monitor_path, None);
    }

    #[test]
    fn constructs_hidraw_abi_values_and_bounds_waits() {
        assert_eq!(read_request(0x01, 4), 0x8004_4801);
        assert_eq!(read_request(0x02, 4100), 0x9004_4802);
        assert_eq!(read_request(0x03, 8), 0x8008_4803);
        assert_eq!(feature_request(0x06, 9), 0xc009_4806);
        assert_eq!(feature_request(0x07, 56), 0xc038_4807);
        assert_eq!(device_numbers(0x10ef00), (0xef, 0x100));
        assert_eq!(poll_millis(Duration::ZERO), 0);
        assert_eq!(poll_millis(Duration::from_nanos(1)), 1);
        assert_eq!(poll_millis(Duration::MAX), c_int::MAX);
    }

    #[test]
    fn wired_preparation_only_accepts_audited_feature_requests() {
        let descriptor = parse_descriptor(WIRED_V104_DESCRIPTOR).unwrap();
        assert!(descriptor.features.contains(PREPARE_QUERY[0]));
        assert!(prepared_query(&[0xa0, 0x0b, 8, 0, 0, 0, 0, 0]));
        assert!(prepared_query(&[0xa0, 0x0c, 6, 0, 0, 0, 0, 0]));
        assert!(prepared_query(&[0xa0, 0x09, 131, 0, 32, 0, 0, 0]));
        for frame in [
            [0xa0, 0x09, 131, 0, 33, 0, 0, 0],
            [0xa0, 0x10, 8, 0, 1, 0, 0, 0],
            [0x10, 8, 0, 0, 0, 0, 0, 0],
        ] {
            assert!(!prepared_query(&frame));
            assert!(!audited_settings_write(&frame));
        }
    }

    #[test]
    fn audited_wired_transport_rejects_macro_writes_before_io() {
        let (feature, _unused) = UnixStream::pair().unwrap();
        let mut feature_ids = ReportIds::default();
        feature_ids.insert(Report::Macro.id());
        let mut device = Hidraw {
            feature: File::from(OwnedFd::from(feature)),
            feature_ids,
            monitor: None,
            input: Vec::new(),
            features: Features::Prepared,
            connection: Connection::Wired,
        };
        let mut chunk = [0; crate::actions::MACRO_CHUNK_LEN];
        chunk[..4].copy_from_slice(&[Report::Macro.id(), 0x40, 7, 0]);
        let error = device.set_feature(&chunk).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("erased-slot restoration"));
    }

    #[test]
    fn wired_incident_guard_blocks_both_feature_operations() {
        let (feature, _unused) = UnixStream::pair().unwrap();
        let mut feature_ids = ReportIds::default();
        feature_ids.insert(0xa0);
        let mut device = Hidraw {
            feature: File::from(OwnedFd::from(feature)),
            feature_ids,
            monitor: None,
            input: Vec::new(),
            connection: Connection::Wired,
            features: Features::Blocked,
        };
        let request = [0xa0, 0x0b, 8, 0, 0, 0, 0, 0];
        for result in [
            device.set_feature(&request),
            device.get_feature(0xa0, 8).map(drop),
        ] {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(error.to_string().contains("USB revision 1.04"));
        }
    }

    #[test]
    fn polls_input_and_detects_disconnect_without_hardware() {
        let (monitor, mut writer) = UnixStream::pair().unwrap();
        monitor.set_nonblocking(true).unwrap();
        let (feature, _unused) = UnixStream::pair().unwrap();
        let mut device = Hidraw {
            feature: File::from(OwnedFd::from(feature)),
            feature_ids: ReportIds::default(),
            monitor: Some(File::from(OwnedFd::from(monitor))),
            input: vec![0; MAX_INPUT],
            features: Features::Standard,
            connection: Connection::Receiver,
        };
        assert_eq!(device.read_input(Duration::ZERO).unwrap(), None);
        let status = [0x03, 0x10, 0x40, 0, 6];
        writer.write_all(&status).unwrap();
        assert_eq!(
            device.read_input(Duration::from_millis(20)).unwrap(),
            Some(status.to_vec())
        );
        drop(writer);
        let error = device.read_input(Duration::from_millis(20)).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
        ));
    }
}
#[test]
fn feature_disconnect_identifies_operation_and_preserves_other_errno() {
    let disconnected = feature_error(io::Error::from_raw_os_error(libc::ENODEV), "read", 0xa0);
    assert_eq!(disconnected.kind(), io::ErrorKind::NotConnected);
    assert!(disconnected.to_string().contains("feature read 0xa0"));
    let stalled = feature_error(io::Error::from_raw_os_error(libc::EPIPE), "read", 0xa0);
    assert_eq!(stalled.raw_os_error(), Some(libc::EPIPE));
}
