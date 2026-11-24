use crate::driver::{Connection, RESET_PREAMBLE, Transport};
use crate::models::{Model, R1};
use dbus::Path;
use dbus::arg::{PropMap, Variant, prop_cast};
use dbus::blocking::Connection as Bus;
use dbus::blocking::stdintf::org_freedesktop_dbus::{
    ObjectManager, Properties, PropertiesPropertiesChanged,
};
use dbus::message::MatchRule;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

const BLUEZ: &str = "org.bluez";
const DEVICE: &str = "org.bluez.Device1";
const BATTERY: &str = "org.bluez.Battery1";
const SERVICE: &str = "org.bluez.GattService1";
const CHARACTERISTIC: &str = "org.bluez.GattCharacteristic1";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const CONTROL_UUID: &str = "0000fee0-0000-1000-8000-00805f9b34fb";
const WRITE_UUID: &str = "0000fee3-0000-1000-8000-00805f9b34fb";
const NOTIFY_UUID: &str = "0000fee4-0000-1000-8000-00805f9b34fb";
const BUS_TIMEOUT: Duration = Duration::from_secs(2);
const WATCH_TIMEOUT: Duration = Duration::from_millis(350);
const REPLY_POLL: Duration = Duration::from_millis(25);
const QUEUE_LIMIT: usize = 64;
const MAX_NOTIFICATION: usize = 201;
const QUERY: u8 = 0xa0;
const MODEL: u8 = 0x10;

type Objects = HashMap<Path<'static>, HashMap<String, PropMap>>;

pub struct Bluetooth {
    model: &'static Model,
    bus: Bus,
    owner: String,
    peer_path: Path<'static>,
    write_path: Path<'static>,
    notify_path: Path<'static>,
    inbox: Arc<Mutex<Inbox>>,
    pending: Option<Pending>,
    _peer_lock: UnixListener,
}

struct Endpoints {
    write: Path<'static>,
    notify: Path<'static>,
}

#[derive(Default)]
struct Inbox {
    x8_pro: bool,
    disconnected: bool,
    identified: bool,
    events: VecDeque<[u8; 5]>,
    notifications: VecDeque<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Query {
    Version,
    Settings,
    Macro(u8),
}

struct Pending {
    query: Query,
    reply: Option<Vec<u8>>,
}

/// Enumerate supported peers without subscribing to notifications or opening a session.
pub fn discover() -> io::Result<Vec<String>> {
    Ok(discover_peers(&list_objects()?))
}

pub struct PeerStatus {
    pub model: &'static Model,
    pub address: String,
    pub configured: bool,
    pub settings_path: bool,
    pub battery: Option<u8>,
}

pub fn survey() -> io::Result<Vec<PeerStatus>> {
    Ok(survey_peers(&list_objects()?))
}

fn list_objects() -> io::Result<Objects> {
    let bus = Bus::new_system().map_err(|error| bus_error("connect to the system D-Bus", error))?;
    bus.with_proxy(BLUEZ, "/", BUS_TIMEOUT)
        .get_managed_objects()
        .map_err(|error| bus_error("list BlueZ objects", error))
}

fn discover_peers(objects: &Objects) -> Vec<String> {
    survey_peers(objects)
        .into_iter()
        .filter_map(|peer| peer.configured.then_some(peer.address))
        .collect()
}

fn survey_peers(objects: &Objects) -> Vec<PeerStatus> {
    let mut peers: Vec<_> = objects
        .iter()
        .filter_map(|(path, interfaces)| {
            let properties = interfaces.get(DEVICE)?;
            let model = Model::bluetooth_name(prop_cast::<String>(properties, "Name")?)?;
            let address = normalize_address(prop_cast::<String>(properties, "Address")?).ok()?;
            let (selected, resolved) = select_peer(objects, &address).ok()?;
            (&selected == path).then(|| PeerStatus {
                model,
                address,
                configured: model == R1 && resolved && select_endpoints(objects, path).is_ok(),
                settings_path: model != R1
                    && has_settings_callback(model)
                    && resolved
                    && select_endpoints(objects, path).is_ok(),
                battery: battery_property(interfaces).ok().flatten(),
            })
        })
        .collect();
    peers.sort_unstable_by(|left, right| left.address.cmp(&right.address));
    peers
}

fn battery_property(interfaces: &HashMap<String, PropMap>) -> io::Result<Option<u8>> {
    let value = interfaces
        .get(BATTERY)
        .and_then(|props| prop_cast::<u8>(props, "Percentage"));
    match value.copied() {
        Some(101..=255) => Err(error(
            io::ErrorKind::InvalidData,
            "BlueZ returned a battery percentage above 100",
        )),
        value => Ok(value),
    }
}

/// Read BlueZ's standard battery property without a vendor GATT session.
pub fn passive_status(address: &str) -> io::Result<PeerStatus> {
    let address = normalize_address(address)?;
    let objects = list_objects()?;
    let (path, resolved) = select_peer(&objects, &address)?;
    let interfaces = &objects[&path];
    let model = interfaces
        .get(DEVICE)
        .and_then(|props| prop_cast::<String>(props, "Name"))
        .and_then(|name| Model::bluetooth_name(name))
        .ok_or_else(|| {
            error(
                io::ErrorKind::Unsupported,
                "the Bluetooth peer is not a recognized Attack Shark model",
            )
        })?;
    Ok(PeerStatus {
        model,
        address,
        configured: model == R1 && resolved && select_endpoints(&objects, &path).is_ok(),
        settings_path: model != R1
            && has_settings_callback(model)
            && resolved
            && select_endpoints(&objects, &path).is_ok(),
        battery: battery_property(interfaces)?,
    })
}

pub fn open(address: &str) -> io::Result<Bluetooth> {
    open_peer(address, |model| model == R1)
}

pub fn open_model(address: &str) -> io::Result<Bluetooth> {
    let mut peer = open_peer(address, has_settings_callback)?;
    if peer.model.slot == 13 {
        peer.confirm_x8_pro()?;
    }
    Ok(peer)
}

fn open_peer(address: &str, allowed: impl FnOnce(&Model) -> bool) -> io::Result<Bluetooth> {
    let address = normalize_address(address)?;
    let peer_lock = lock_peer(&address)?;
    let bus = Bus::new_system().map_err(|error| bus_error("connect to the system D-Bus", error))?;
    let owner: String = bus
        .with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", BUS_TIMEOUT)
        .method_call("org.freedesktop.DBus", "GetNameOwner", (BLUEZ,))
        .map(|(owner,): (String,)| owner)
        .map_err(|error| bus_error("find the BlueZ service", error))?;

    let proxy = bus.with_proxy(owner.clone(), "/", BUS_TIMEOUT);
    let mut objects = proxy
        .get_managed_objects()
        .map_err(|error| bus_error("list BlueZ objects", error))?;
    let (peer, resolved) = select_peer(&objects, &address)?;
    if !resolved {
        wait_for_services(&bus, &owner, &peer)?;
        objects = proxy
            .get_managed_objects()
            .map_err(|error| bus_error("refresh Bluetooth services", error))?;
        if select_peer(&objects, &address)? != (peer.clone(), true) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Bluetooth peer changed during setup",
            ));
        }
    }
    let model = objects
        .get(&peer)
        .and_then(|interfaces| interfaces.get(DEVICE))
        .and_then(|properties| prop_cast::<String>(properties, "Name"))
        .and_then(|name| Model::bluetooth_name(name))
        .filter(|model| allowed(model))
        .ok_or_else(|| {
            error(
                io::ErrorKind::Unsupported,
                "the selected peer has no audited Bluetooth configuration route",
            )
        })?;
    let endpoints = select_endpoints(&objects, &peer)?;
    let inbox = Arc::new(Mutex::new(Inbox {
        x8_pro: model.slot == 13,
        ..Inbox::default()
    }));
    install_watchers(&bus, &owner, &peer, &endpoints.notify, &inbox)?;
    bus.with_proxy(owner.clone(), endpoints.notify.clone(), BUS_TIMEOUT)
        .method_call::<(), _, _, _>(CHARACTERISTIC, "StartNotify", ())
        .map_err(|error| bus_error("subscribe to the R1 Bluetooth characteristic", error))?;

    Ok(Bluetooth {
        model,
        bus,
        owner,
        peer_path: peer,
        write_path: endpoints.write,
        notify_path: endpoints.notify,
        inbox,
        pending: None,
        _peer_lock: peer_lock,
    })
}

pub(crate) fn has_settings_callback(model: &Model) -> bool {
    matches!(model.slot, 3 | 4 | 10 | 11 | 13..=18)
}

/// Capture the exact vendor settings notification from a traced paired peer.
pub fn read_settings_notification(address: &str) -> io::Result<Vec<u8>> {
    let mut peer = open_model(address)?;
    let model = peer.model;
    capture_settings(model, |request| peer.request_notification(request))
}

fn capture_settings(
    model: &Model,
    mut transfer: impl FnMut([u8; 8]) -> io::Result<Option<Vec<u8>>>,
) -> io::Result<Vec<u8>> {
    if !has_settings_callback(model) {
        return Err(error(
            io::ErrorKind::Unsupported,
            "no audited Bluetooth settings callback for this model",
        ));
    }
    if model.slot != 13 {
        let version = transfer(Query::Version.command())?
            .ok_or_else(|| error(io::ErrorKind::TimedOut, "no Bluetooth version notification"))?;
        if !matches!(version.as_slice(), [0xa1, tag, _, ..] if *tag == model.receiver_tag) {
            return Err(error(
                io::ErrorKind::InvalidData,
                "Bluetooth version does not identify the selected model",
            ));
        }
        transfer([0xa2, 8, 0, 0, 0, 0, 0, 0])?;
    }
    let settings = transfer(Query::Settings.command())?.ok_or_else(|| {
        error(
            io::ErrorKind::TimedOut,
            "no Bluetooth settings notification",
        )
    })?;
    if !valid_snapshot(model, &settings) {
        return Err(error(
            io::ErrorKind::InvalidData,
            "unexpected Bluetooth settings packet",
        ));
    }
    Ok(settings)
}

fn valid_snapshot(model: &Model, settings: &[u8]) -> bool {
    let minimum = if model.slot == 13 { 63 } else { 119 };
    matches!(settings, [0xa0, rate, ..] if crate::model_wire::valid_rate_code(model.slot, *rate))
        && (minimum..=MAX_NOTIFICATION).contains(&settings.len())
}

fn x8_settings_reply(model: &Model, value: &[u8]) -> io::Result<Vec<u8>> {
    if !valid_snapshot(model, value) {
        return Err(error(
            io::ErrorKind::InvalidData,
            "invalid X8 Pro settings notification",
        ));
    }
    let mut reply = vec![0; 128];
    reply[..4].copy_from_slice(&[10, 128, 1, value[1]]);
    reply[5..54].copy_from_slice(&value[3..52]);
    reply[55..65].copy_from_slice(&value[53..63]);
    Ok(reply)
}

fn normalize_address(address: &str) -> io::Result<String> {
    if address.len() != 17
        || !address.bytes().enumerate().all(|(index, byte)| {
            if index % 3 == 2 {
                byte == b':'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(error(
            io::ErrorKind::InvalidInput,
            "Bluetooth address must have the form AA:BB:CC:DD:EE:FF",
        ));
    }
    Ok(address.to_ascii_uppercase())
}

fn lock_peer(address: &str) -> io::Result<UnixListener> {
    let name = format!("shark-driver-bluetooth/{address}");
    let socket = SocketAddr::from_abstract_name(name.as_bytes())?;
    UnixListener::bind_addr(&socket).map_err(|cause| {
        let kind = if cause.kind() == io::ErrorKind::AddrInUse {
            io::ErrorKind::WouldBlock
        } else {
            cause.kind()
        };
        error(
            kind,
            format!("could not reserve Bluetooth peer {address}: {cause}"),
        )
    })
}

fn select_peer(objects: &Objects, address: &str) -> io::Result<(Path<'static>, bool)> {
    let mut matched = objects.iter().filter_map(|(path, interfaces)| {
        let properties = interfaces.get(DEVICE)?;
        let current = prop_cast::<String>(properties, "Address")?;
        current
            .eq_ignore_ascii_case(address)
            .then_some((path, properties))
    });
    let Some((path, properties)) = matched.next() else {
        return Err(error(
            io::ErrorKind::NotFound,
            format!("BlueZ did not find Bluetooth peer {address}"),
        ));
    };
    if matched.next().is_some() {
        return Err(error(
            io::ErrorKind::InvalidData,
            format!("Bluetooth peer {address} is visible on multiple adapters"),
        ));
    }
    if prop_cast::<bool>(properties, "Paired") != Some(&true)
        || prop_cast::<bool>(properties, "Connected") != Some(&true)
    {
        return Err(error(
            io::ErrorKind::NotConnected,
            format!("Bluetooth peer {address} must already be paired and connected"),
        ));
    }
    Ok((
        path.clone(),
        prop_cast::<bool>(properties, "ServicesResolved") == Some(&true),
    ))
}

fn wait_for_services(bus: &Bus, owner: &str, device: &Path<'static>) -> io::Result<()> {
    let start = Instant::now();
    while let Some(remaining) = BUS_TIMEOUT.checked_sub(start.elapsed()) {
        let proxy = bus.with_proxy(owner.to_owned(), device.clone(), remaining);
        if proxy
            .get::<bool>(DEVICE, "ServicesResolved")
            .map_err(|error| bus_error("check Bluetooth service discovery", error))?
        {
            return Ok(());
        }
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
    Err(error(
        io::ErrorKind::TimedOut,
        "BlueZ did not resolve the selected peer's services",
    ))
}

fn select_endpoints(objects: &Objects, peer: &Path<'static>) -> io::Result<Endpoints> {
    let mut write = None;
    let mut notify = None;
    for (path, interfaces) in objects {
        let Some(characteristic) = interfaces.get(CHARACTERISTIC) else {
            continue;
        };
        let Some(service_path) = prop_cast::<Path<'static>>(characteristic, "Service") else {
            continue;
        };
        let Some(service) = objects
            .get(service_path)
            .and_then(|entry| entry.get(SERVICE))
        else {
            continue;
        };
        if prop_cast::<Path<'static>>(service, "Device") != Some(peer)
            || !prop_cast::<String>(service, "UUID")
                .is_some_and(|uuid| uuid.eq_ignore_ascii_case(CONTROL_UUID))
        {
            continue;
        }
        let Some(uuid) = prop_cast::<String>(characteristic, "UUID") else {
            continue;
        };
        let target = if uuid.eq_ignore_ascii_case(WRITE_UUID) {
            (&mut write, "write")
        } else if uuid.eq_ignore_ascii_case(NOTIFY_UUID) {
            (&mut notify, "notify")
        } else {
            continue;
        };
        let flags = prop_cast::<Vec<String>>(characteristic, "Flags");
        if !flags.is_some_and(|flags| flags.iter().any(|flag| flag == target.1)) {
            continue;
        }
        if target.0.replace((path, service_path)).is_some() {
            return Err(error(
                io::ErrorKind::InvalidData,
                "multiple matching R1 Bluetooth characteristics",
            ));
        }
    }
    match (write, notify) {
        (Some((write, service_a)), Some((notify, service_b))) if service_a == service_b => {
            Ok(Endpoints {
                write: write.clone(),
                notify: notify.clone(),
            })
        }
        _ => Err(error(
            io::ErrorKind::Unsupported,
            "peer lacks a unique FEE3 writer and FEE4 notifier on one service",
        )),
    }
}

fn install_watchers(
    bus: &Bus,
    owner: &str,
    peer: &Path<'static>,
    notify: &Path<'static>,
    inbox: &Arc<Mutex<Inbox>>,
) -> io::Result<()> {
    let notice_inbox = Arc::clone(inbox);
    bus.add_match(
        MatchRule::new_signal(PROPERTIES, "PropertiesChanged")
            .with_strict_sender(owner.to_owned())
            .with_path(notify.clone()),
        move |signal: PropertiesPropertiesChanged, _, _| {
            if signal.interface_name == CHARACTERISTIC {
                if let Ok(mut inbox) = notice_inbox.lock() {
                    if prop_cast::<bool>(&signal.changed_properties, "Notifying") == Some(&false) {
                        inbox.disconnected = true;
                    }
                    if let Some(value) = prop_cast::<Vec<u8>>(&signal.changed_properties, "Value") {
                        inbox.record(value);
                    }
                }
            }
            true
        },
    )
    .map_err(|error| bus_error("watch R1 Bluetooth notifications", error))?;

    let peer_inbox = Arc::clone(inbox);
    bus.add_match(
        MatchRule::new_signal(PROPERTIES, "PropertiesChanged")
            .with_strict_sender(owner.to_owned())
            .with_path(peer.clone()),
        move |signal: PropertiesPropertiesChanged, _, _| {
            if signal.interface_name == DEVICE
                && ["Connected", "ServicesResolved", "Paired"]
                    .iter()
                    .any(|name| {
                        prop_cast::<bool>(&signal.changed_properties, name) == Some(&false)
                            || signal
                                .invalidated_properties
                                .iter()
                                .any(|property| property == name)
                    })
            {
                if let Ok(mut inbox) = peer_inbox.lock() {
                    inbox.disconnected = true;
                }
            }
            true
        },
    )
    .map_err(|error| bus_error("watch R1 Bluetooth connection state", error))?;
    Ok(())
}

impl Inbox {
    fn record(&mut self, value: &[u8]) {
        if self.disconnected || value.len() > MAX_NOTIFICATION {
            return;
        }
        if self.x8_pro && matches!(value, [8, 0x10 | 0x20 | 0x40 | 0x70, _, _, ..]) {
            self.identified = true;
            if self.events.len() == QUEUE_LIMIT {
                self.events.pop_front();
            }
            self.events.push_back([3, 8, value[1], value[2], value[3]]);
            return;
        }
        match value {
            &[
                MODEL,
                class @ (0x10 | 0x20 | 0x50 | 0x60 | 0x70 | 0x80),
                lo,
                hi,
                ..,
            ] if !self.x8_pro => {
                self.identified = true;
                if self.events.len() == QUEUE_LIMIT {
                    self.events.pop_front();
                }
                self.events.push_back([3, MODEL, class, lo, hi]);
            }
            [0xa0 | 0xa1 | 0xa9, ..] => {
                if self.notifications.len() == QUEUE_LIMIT {
                    self.notifications.pop_front();
                }
                self.notifications.push_back(value.to_vec());
            }
            _ => {}
        }
    }

    fn check_connected(&self) -> io::Result<()> {
        if self.disconnected {
            return Err(error(
                io::ErrorKind::NotConnected,
                "R1 Bluetooth peer disconnected or changed services",
            ));
        }
        Ok(())
    }

    fn require_identity(&self) -> io::Result<()> {
        self.check_connected()?;
        if !self.identified {
            return Err(error(
                io::ErrorKind::PermissionDenied,
                "Bluetooth peer has not returned the expected model identity",
            ));
        }
        Ok(())
    }
}

impl Query {
    fn parse(bytes: &[u8]) -> io::Result<Self> {
        match bytes {
            [QUERY, 0x0b, 8, 0, 0, 0, 0, 0] => Ok(Self::Version),
            [QUERY, 0x0a, 128, 0, 1, 0, 0, 0] => Ok(Self::Settings),
            &[QUERY, 0x09, 131, 0, slot @ 1..=24, 0, 0, 0] => Ok(Self::Macro(slot)),
            _ => Err(error(
                io::ErrorKind::Unsupported,
                "Bluetooth uses version, combined settings, or macro reads",
            )),
        }
    }

    fn shape(self) -> (u8, usize) {
        match self {
            Self::Version => (0x0b, 8),
            Self::Settings => (0x0a, 128),
            Self::Macro(_) => (0x09, 131),
        }
    }

    fn command(self) -> [u8; 8] {
        let (id, selector) = match self {
            Self::Version => (0xa1, 0),
            Self::Settings => (0xa0, 0),
            Self::Macro(slot) => (0xa9, slot - 1),
        };
        [id, 8, selector, 0, 0, 0, 0, 0]
    }

    fn normalize_reply(self, value: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let (id, length) = self.shape();
        match (self, value) {
            (Self::Version, &[0xa1, hi, lo, ..]) => Ok(Some(vec![id, 8, hi, lo, 0, 0, 0, 0])),
            (Self::Version, [0xa1, ..]) => Err(error(
                io::ErrorKind::InvalidData,
                "truncated Bluetooth version notification",
            )),
            (Self::Settings, [0xa0, ..]) => {
                if value.len() < 119 {
                    return Err(error(
                        io::ErrorKind::InvalidData,
                        "truncated Bluetooth settings notification",
                    ));
                }
                let mut result = vec![0; length];
                result[..3].copy_from_slice(&[id, length as u8, 1]);
                result[3] = value[1];
                result[5..54].copy_from_slice(&value[3..52]);
                result[55..65].copy_from_slice(&value[53..63]);
                result[65..121].copy_from_slice(&value[63..119]);
                Ok(Some(result))
            }
            (Self::Macro(slot), &[0xa9, index, ref payload @ ..]) if index == slot - 1 => {
                let erased = payload
                    .get(..length - 3)
                    .is_some_and(|bytes| bytes.iter().all(|&byte| byte == 0xff));
                if !erased {
                    let Some(&count) = payload.get(25) else {
                        return Err(error(
                            io::ErrorKind::InvalidData,
                            "truncated Bluetooth macro header",
                        ));
                    };
                    if count > 50 || payload.len() < 26 + usize::from(count) * 2 {
                        return Err(error(
                            io::ErrorKind::InvalidData,
                            "invalid Bluetooth macro action count",
                        ));
                    }
                }
                let mut result = vec![0; length];
                result[..3].copy_from_slice(&[id, length as u8, slot]);
                let count = payload.len().min(length - 3);
                result[3..3 + count].copy_from_slice(&payload[..count]);
                Ok(Some(result))
            }
            _ => Ok(None),
        }
    }
}

impl Bluetooth {
    fn confirm_x8_pro(&mut self) -> io::Result<()> {
        if self.mailbox()?.identified {
            return Ok(());
        }
        self.read_input(BUS_TIMEOUT)?;
        self.mailbox()?.require_identity()
    }

    pub fn model(&self) -> &'static Model {
        self.model
    }

    fn mailbox(&self) -> io::Result<MutexGuard<'_, Inbox>> {
        self.inbox
            .lock()
            .map_err(|_| io::Error::other("Bluetooth notification state was interrupted"))
    }

    fn process(&self, timeout: Duration) -> io::Result<bool> {
        let processed = self
            .bus
            .process(timeout)
            .map_err(|error| bus_error("process Bluetooth notifications", error))?;
        self.mailbox()?.check_connected()?;
        Ok(processed)
    }

    fn write_value(&self, bytes: &[u8]) -> io::Result<()> {
        let options: PropMap = HashMap::from([(
            "type".into(),
            Variant(Box::new("request".to_owned()) as Box<dyn dbus::arg::RefArg>),
        )]);
        self.bus
            .with_proxy(self.owner.clone(), self.write_path.clone(), BUS_TIMEOUT)
            .method_call(CHARACTERISTIC, "WriteValue", (bytes, options))
            .map_err(|error| bus_error("write the FEE3 Bluetooth characteristic", error))
    }

    fn drain_callbacks(&self) -> io::Result<()> {
        for _ in 0..QUEUE_LIMIT {
            if !self.process(Duration::ZERO)? {
                return Ok(());
            }
        }
        Err(error(
            io::ErrorKind::WouldBlock,
            "Bluetooth notification queue is busy",
        ))
    }

    fn request_notification(&mut self, command: [u8; 8]) -> io::Result<Option<Vec<u8>>> {
        self.drain_callbacks()?;
        {
            let mut inbox = self.mailbox()?;
            inbox.check_connected()?;
            inbox.notifications.clear();
        }
        self.write_value(&command)?;
        let start = Instant::now();
        if command[0] == 0xa2 {
            let gap = Duration::from_millis(100);
            self.process(gap)?;
            thread::sleep(gap.saturating_sub(start.elapsed()));
            return Ok(None);
        }
        loop {
            {
                let mut inbox = self.mailbox()?;
                inbox.check_connected()?;
                if let Some(index) = inbox
                    .notifications
                    .iter()
                    .position(|packet| packet[0] == command[0])
                {
                    return Ok(inbox.notifications.remove(index));
                }
            }
            let Some(remaining) = BUS_TIMEOUT.checked_sub(start.elapsed()) else {
                return Ok(None);
            };
            self.process(remaining.min(REPLY_POLL))?;
        }
    }

    fn start_query(&mut self, query: Query) -> io::Result<()> {
        self.drain_callbacks()?;
        let mut inbox = self.mailbox()?;
        if query != Query::Version {
            inbox.require_identity()?;
        }
        inbox.notifications.clear();
        drop(inbox);
        self.pending = None;
        if query == Query::Settings && self.model.slot != 13 {
            self.write_value(&[0xa2, 8, 0, 0, 0, 0, 0, 0])?;
            // The Windows client waits 100 ms between the preparation and snapshot requests.
            let start = Instant::now();
            let gap = Duration::from_millis(100);
            self.process(gap)?;
            thread::sleep(gap.saturating_sub(start.elapsed()));
            self.drain_callbacks()?;
        }
        self.write_value(&query.command())?;
        self.pending = Some(Pending { query, reply: None });
        Ok(())
    }

    fn poll_query(&mut self) -> io::Result<()> {
        let start = Instant::now();
        loop {
            let Some(pending) = self.pending.as_ref() else {
                return Err(error(
                    io::ErrorKind::InvalidInput,
                    "no Bluetooth query is pending",
                ));
            };
            if pending.reply.is_some() {
                return Ok(());
            }
            let query = pending.query;
            let mut decoded = None;
            {
                let mut inbox = self.mailbox()?;
                inbox.check_connected()?;
                while let Some(value) = inbox.notifications.pop_front() {
                    if self.model.slot == 13
                        && query == Query::Settings
                        && value.first() == Some(&0xa0)
                    {
                        decoded = Some(x8_settings_reply(self.model, &value)?);
                        break;
                    }
                    if let Some(reply) = query.normalize_reply(&value)? {
                        if query == Query::Version && reply[2] == self.model.receiver_tag {
                            inbox.identified = true;
                        }
                        decoded = Some(reply);
                        break;
                    }
                }
            }
            if let Some(reply) = decoded {
                self.pending = Some(Pending {
                    query,
                    reply: Some(reply),
                });
                return Ok(());
            }
            let Some(remaining) = REPLY_POLL.checked_sub(start.elapsed()) else {
                break;
            };
            if !self.process(remaining)? {
                break;
            }
        }
        Err(error(
            io::ErrorKind::WouldBlock,
            "the mouse has not replied on FEE4. If Bluetooth configuration is unavailable on this firmware, use USB or the 2.4 GHz receiver",
        ))
    }
}

impl Drop for Bluetooth {
    fn drop(&mut self) {
        let _ = self
            .bus
            .with_proxy(self.owner.clone(), self.notify_path.clone(), WATCH_TIMEOUT)
            .method_call::<(), _, _, _>(CHARACTERISTIC, "StopNotify", ());
    }
}

impl Transport for Bluetooth {
    fn connection(&self) -> Connection {
        Connection::Bluetooth
    }

    fn battery_percentage(&self) -> io::Result<Option<u8>> {
        self.mailbox()?.check_connected()?;
        let percentage = self
            .bus
            .with_proxy(self.owner.clone(), self.peer_path.clone(), BUS_TIMEOUT)
            .get::<u8>(BATTERY, "Percentage");
        match percentage {
            Ok(percentage @ 0..=100) => Ok(Some(percentage)),
            Ok(_) => Err(error(
                io::ErrorKind::InvalidData,
                "BlueZ returned a battery percentage above 100",
            )),
            Err(cause)
                if matches!(
                    cause.name().and_then(|name| name.rsplit('.').next()),
                    Some("UnknownInterface" | "UnknownProperty")
                ) =>
            {
                Ok(None)
            }
            Err(cause) => Err(bus_error("read the R1 Bluetooth battery percentage", cause)),
        }
    }

    fn set_feature(&mut self, bytes: &[u8]) -> io::Result<()> {
        match bytes.first() {
            Some(&QUERY) => {
                let query = Query::parse(bytes)?;
                if matches!(query, Query::Macro(slot) if (self.model == R1 && slot > 18)
                    || (self.model != R1 && !(16..=18).contains(&self.model.slot)))
                {
                    return Err(error(
                        io::ErrorKind::Unsupported,
                        "no audited macro read for this Bluetooth model or slot",
                    ));
                }
                return self.start_query(query);
            }
            Some(0x06) => {
                return Err(error(
                    io::ErrorKind::Unsupported,
                    "the R1 Bluetooth path does not write polling rate",
                ));
            }
            _ => {}
        }
        match bytes {
            bytes if bytes == RESET_PREAMBLE && self.model == R1 => {}
            [0x04, 56, 1, ..] if bytes.len() == 56 => {}
            [0x05, 15, 1, ..] if bytes.len() == 15 => {}
            [0x08, 59, 1, ..]
                if bytes.len() == 59
                    && (self.model == R1 || (16..=18).contains(&self.model.slot)) => {}
            [0x09, 131, slot @ 1..=24, ..]
                if bytes.len() == 131
                    && (self.model == R1 && *slot <= 18
                        || (16..=18).contains(&self.model.slot)) => {}
            _ => {
                return Err(error(
                    io::ErrorKind::InvalidInput,
                    "invalid or unsupported R1 Bluetooth write frame",
                ));
            }
        }
        self.drain_callbacks()?;
        self.mailbox()?.require_identity()?;
        self.write_value(bytes)
    }

    fn get_feature(&mut self, id: u8, length: usize) -> io::Result<Vec<u8>> {
        if (id, length) == (QUERY, 8) {
            self.poll_query()?;
            return Ok(vec![QUERY, 1, 0, 0, 0, 0, 0, 0]);
        }
        let Some(pending) = self.pending.as_mut() else {
            return Err(error(
                io::ErrorKind::InvalidInput,
                "no Bluetooth query is pending",
            ));
        };
        if pending.query.shape() != (id, length) {
            return Err(error(
                io::ErrorKind::InvalidInput,
                "feature read does not match the Bluetooth query",
            ));
        }
        let reply = pending.reply.take().ok_or_else(|| {
            error(
                io::ErrorKind::WouldBlock,
                "Bluetooth query reply has not arrived",
            )
        })?;
        self.pending = None;
        Ok(reply)
    }

    fn read_input(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        let start = Instant::now();
        let mut first_poll = true;
        loop {
            {
                let mut inbox = self.mailbox()?;
                inbox.check_connected()?;
                if let Some(event) = inbox.events.pop_front() {
                    return Ok(Some(event.to_vec()));
                }
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() && !first_poll {
                return Ok(None);
            }
            first_poll = false;
            if !self.process(remaining)? {
                return Ok(None);
            }
        }
    }
}

fn bus_error(operation: &str, cause: dbus::Error) -> io::Error {
    let kind = match cause.name().and_then(|name| name.rsplit('.').next()) {
        Some("AccessDenied" | "AuthFailed" | "NotAuthorized" | "NotPermitted") => {
            io::ErrorKind::PermissionDenied
        }
        Some("Disconnected" | "NotConnected" | "NotReady") => io::ErrorKind::NotConnected,
        Some("NoReply" | "Timeout" | "TimedOut") => io::ErrorKind::TimedOut,
        Some("NameHasNoOwner" | "ServiceUnknown" | "UnknownObject" | "FileNotFound") => {
            io::ErrorKind::NotFound
        }
        Some("InProgress" | "Busy") => io::ErrorKind::WouldBlock,
        Some("NotSupported") => io::ErrorKind::Unsupported,
        _ => io::ErrorKind::Other,
    };
    error(kind, format!("{operation}: {cause}"))
}

fn error(kind: io::ErrorKind, message: impl Into<String>) -> io::Error {
    io::Error::new(kind, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbus::arg::RefArg;

    fn property(value: impl RefArg + 'static) -> Variant<Box<dyn RefArg>> {
        Variant(Box::new(value))
    }

    fn service(peer: Path<'static>) -> HashMap<String, PropMap> {
        HashMap::from([(
            SERVICE.into(),
            HashMap::from([
                ("Device".into(), property(peer)),
                ("UUID".into(), property(CONTROL_UUID.to_owned())),
            ]),
        )])
    }

    fn characteristic(service: Path<'static>, uuid: &str, flag: &str) -> HashMap<String, PropMap> {
        HashMap::from([(
            CHARACTERISTIC.into(),
            HashMap::from([
                ("Service".into(), property(service)),
                ("UUID".into(), property(uuid.to_owned())),
                ("Flags".into(), property(vec![flag.to_owned()])),
            ]),
        )])
    }

    #[test]
    fn discovers_only_connected_paired_r1_with_resolved_control_service() {
        let address = "B6:6B:18:E4:AA:48";
        let peer = Path::from("/peer");
        let control = Path::from("/peer/control");
        let mut objects = Objects::from([
            (
                peer.clone(),
                HashMap::from([(
                    DEVICE.into(),
                    HashMap::from([
                        ("Address".into(), property(address.to_owned())),
                        ("Name".into(), property("R1 mouse 2".to_owned())),
                        ("Paired".into(), property(true)),
                        ("Connected".into(), property(true)),
                        ("ServicesResolved".into(), property(true)),
                    ]),
                )]),
            ),
            (control.clone(), service(peer.clone())),
            (
                Path::from("/peer/write"),
                characteristic(control.clone(), WRITE_UUID, "write"),
            ),
            (
                Path::from("/peer/notify"),
                characteristic(control.clone(), NOTIFY_UUID, "notify"),
            ),
        ]);
        assert_eq!(discover_peers(&objects), [address]);
        for field in ["Paired", "Connected", "ServicesResolved"] {
            let properties = objects.get_mut(&peer).unwrap().get_mut(DEVICE).unwrap();
            properties.insert(field.into(), property(false));
            assert!(discover_peers(&objects).is_empty());
            objects
                .get_mut(&peer)
                .unwrap()
                .get_mut(DEVICE)
                .unwrap()
                .insert(field.into(), property(true));
        }
        objects
            .get_mut(&control)
            .unwrap()
            .get_mut(SERVICE)
            .unwrap()
            .insert(
                "UUID".into(),
                property("0000fff0-0000-1000-8000-00805f9b34fb".to_owned()),
            );
        assert!(discover_peers(&objects).is_empty());
    }

    #[test]
    fn vendor_snapshot_fixtures_check_model_and_notification_order() {
        for model in crate::models::MODELS {
            let mut issued = Vec::new();
            let mut settings = vec![0; 119];
            let x8_pro = model.slot == 13;
            if x8_pro {
                settings.truncate(63);
            }
            settings[..2].copy_from_slice(&[0xa0, if x8_pro { 0x44 } else { 2 }]);
            let response = capture_settings(&model, |request| {
                issued.push(request);
                Ok(match request[0] {
                    0xa1 => Some(vec![0xa1, model.receiver_tag, 0x17]),
                    0xa0 => Some(settings.clone()),
                    _ => None,
                })
            });
            if matches!(model.slot, 3 | 4 | 10 | 11 | 13..=18) {
                assert_eq!(response.unwrap(), settings);
                let selectors: &[u8] = if x8_pro { &[0xa0] } else { &[0xa1, 0xa2, 0xa0] };
                assert_eq!(
                    issued,
                    selectors
                        .iter()
                        .map(|&id| [id, 8, 0, 0, 0, 0, 0, 0])
                        .collect::<Vec<_>>()
                );
                let error =
                    capture_settings(&model, |_| Ok(Some(vec![0xa1, 0xff, 0x17]))).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                let error = capture_settings(&model, |_| Ok(None)).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            } else {
                assert_eq!(response.unwrap_err().kind(), io::ErrorKind::Unsupported);
                assert!(issued.is_empty());
            }
        }
    }

    #[test]
    fn x8_pro_settings_and_identity_follow_the_distinct_callback() {
        let model = &crate::models::MODELS[12];
        let mut inbox = Inbox {
            x8_pro: true,
            ..Inbox::default()
        };
        inbox.record(&[MODEL, 0x10, 70, 0]);
        assert!(!inbox.identified);
        inbox.record(&[8, 0x10, 70, 0]);
        assert!(inbox.identified);
        assert_eq!(inbox.events.pop_front(), Some([3, 8, 0x10, 70, 0]));

        let mut value: Vec<u8> = (0..63).collect();
        value[..2].copy_from_slice(&[0xa0, 0x44]);
        let reply = x8_settings_reply(model, &value).unwrap();
        assert_eq!(reply[..5], [10, 128, 1, 0x44, 0]);
        assert_eq!(reply[5..54], value[3..52]);
        assert_eq!(reply[55..65], value[53..63]);
        assert_eq!(reply[65..], [0; 63]);
        assert!(x8_settings_reply(model, &value[..62]).is_err());
        value[1] = 0x40;
        assert!(x8_settings_reply(model, &value).is_err());
    }

    #[test]
    fn all_model_status_records_expose_only_same_peer_battery() {
        for model in crate::models::MODELS {
            let address = "C0:AA:00:13:B4:27";
            let path = Path::from("/mouse");
            let foreign = Path::from("/other_mouse");
            let battery = |value| HashMap::from([("Percentage".into(), property(value))]);
            let mut objects = Objects::from([
                (
                    path.clone(),
                    HashMap::from([
                        (
                            DEVICE.into(),
                            HashMap::from([
                                (
                                    "Name".into(),
                                    property(format!("Attack Shark {}", model.name)),
                                ),
                                ("Address".into(), property(address.to_string())),
                                ("Paired".into(), property(true)),
                                ("Connected".into(), property(true)),
                            ]),
                        ),
                        (BATTERY.into(), battery(72u8)),
                    ]),
                ),
                (foreign, HashMap::from([(BATTERY.into(), battery(25u8))])),
            ]);
            let peers = survey_peers(&objects);
            assert_eq!(peers.len(), 1);
            assert_eq!(peers[0].model, &model);
            assert_eq!(peers[0].battery, Some(72));
            assert!(!peers[0].configured);
            objects
                .get_mut(&path)
                .unwrap()
                .insert(BATTERY.into(), battery(101u8));
            assert_eq!(
                battery_property(&objects[&path]).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert!(survey_peers(&objects)[0].battery.is_none());
        }
    }

    #[test]
    fn requires_complete_explicit_address_and_paired_connected_peer() {
        let address = normalize_address("c0:aa:00:13:b4:27").unwrap();
        assert_eq!(address, "C0:AA:00:13:B4:27");
        assert!(normalize_address("C0:AA:00:13:B4: 7").is_err());

        let path = Path::from("/selected");
        let mut devices = Objects::from([(
            path.clone(),
            HashMap::from([(
                DEVICE.into(),
                HashMap::from([
                    ("Address".into(), property(address.clone())),
                    ("Paired".into(), property(true)),
                    ("Connected".into(), property(true)),
                    ("ServicesResolved".into(), property(false)),
                ]),
            )]),
        )]);
        assert_eq!(
            select_peer(&devices, &address).unwrap(),
            (path.clone(), false)
        );
        devices
            .get_mut(&path)
            .unwrap()
            .get_mut(DEVICE)
            .unwrap()
            .insert("Paired".into(), property(false));
        assert_eq!(
            select_peer(&devices, &address).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        assert_eq!(
            select_peer(&devices, "00:00:00:00:00:00")
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn only_accepts_one_service_pair_belonging_to_the_selected_peer() {
        let peer = Path::from("/selected");
        let other = Path::from("/other");
        let selected_service = Path::from("/selected/service1");
        let selected_writer = Path::from("/selected/service1/write");
        let selected_notifier = Path::from("/selected/service1/notify");
        let other_service = Path::from("/other/service2");
        let mut objects = Objects::from([
            (selected_service.clone(), service(peer.clone())),
            (other_service.clone(), service(other.clone())),
            (
                selected_writer.clone(),
                characteristic(selected_service.clone(), WRITE_UUID, "write"),
            ),
            (
                selected_notifier.clone(),
                characteristic(selected_service.clone(), NOTIFY_UUID, "notify"),
            ),
            (
                Path::from("/other/service2/write"),
                characteristic(other_service.clone(), WRITE_UUID, "write"),
            ),
        ]);
        let selected = select_endpoints(&objects, &peer).unwrap();
        assert_eq!(
            (selected.write, selected.notify),
            (selected_writer, selected_notifier.clone())
        );
        assert!(select_endpoints(&objects, &other).is_err());

        let wrong_service = Path::from("/selected/service3");
        objects.insert(wrong_service.clone(), service(peer.clone()));
        objects.insert(
            selected_notifier,
            characteristic(wrong_service.clone(), NOTIFY_UUID, "notify"),
        );
        assert!(select_endpoints(&objects, &peer).is_err());
        objects.insert(
            Path::from("/selected/service3/write"),
            characteristic(wrong_service, WRITE_UUID, "write"),
        );
        assert!(select_endpoints(&objects, &peer).is_err());
    }

    #[test]
    fn maps_queries_and_normalizes_distinct_gatt_reply_shapes() {
        let version = Query::parse(&[0xa0, 0x0b, 8, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(version.command(), [0xa1, 8, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            version.normalize_reply(&[0xa1, 0x10, 2]).unwrap().unwrap(),
            [0x0b, 8, 0x10, 2, 0, 0, 0, 0],
        );
        assert!(version.normalize_reply(&[0xa1, 0x10]).is_err());
        assert!(Query::parse(&[0xa0, 6, 9, 0, 1, 0, 0, 0]).is_err());

        let mut settings: Vec<_> = (0..119).collect();
        settings[0] = 0xa0;
        let frame = Query::Settings.normalize_reply(&settings).unwrap().unwrap();
        assert_eq!(frame[..4], [0x0a, 128, 1, settings[1]]);
        assert_eq!(frame[5..54], settings[3..52]);
        assert_eq!(frame[55..65], settings[53..63]);
        assert_eq!(frame[65..121], settings[63..119]);
        assert!(Query::Settings.normalize_reply(&settings[..118]).is_err());

        let macro_request = Query::parse(&[0xa0, 9, 131, 0, 8, 0, 0, 0]).unwrap();
        assert_eq!(macro_request.command(), [0xa9, 8, 7, 0, 0, 0, 0, 0]);
        let mut page = [0; 32];
        page[..2].copy_from_slice(&[0xa9, 7]);
        page[27..32].copy_from_slice(&[2, 1, 4, 9, 0xfe]);
        let frame = macro_request.normalize_reply(&page).unwrap().unwrap();
        assert_eq!(frame[..3], [9, 131, 8]);
        assert_eq!(frame[28..33], [2, 1, 4, 9, 0xfe]);
        assert!(Query::Macro(9).normalize_reply(&page).unwrap().is_none());
        assert!(macro_request.normalize_reply(&page[..31]).is_err());
        let mut erased = [0xff; 130];
        erased[..2].copy_from_slice(&[0xa9, 7]);
        let frame = macro_request.normalize_reply(&erased).unwrap().unwrap();
        assert_eq!(&frame[..3], &[9, 131, 8]);
        assert!(frame[3..].iter().all(|&byte| byte == 0xff));
        assert!(macro_request.normalize_reply(&erased[..129]).is_err());
    }

    #[test]
    fn limits_inbox_and_requires_current_peer_identity() {
        let mut inbox = Inbox::default();
        inbox.record(&[0x10, 0x40, 1, 8]);
        assert!(inbox.events.is_empty());
        assert_eq!(
            inbox.require_identity().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        inbox.record(&[0x10, 0x60, 7, 0]);
        inbox.require_identity().unwrap();
        assert_eq!(inbox.events.pop_front(), Some([3, 0x10, 0x60, 7, 0]));
        for n in 0..=100 {
            inbox.record(&[0xa1, 0x10, n]);
        }
        assert_eq!(inbox.notifications.len(), QUEUE_LIMIT);
        assert_eq!(inbox.notifications.back().unwrap(), &[0xa1, 0x10, 100]);
        inbox.disconnected = true;
        assert_eq!(
            inbox.require_identity().unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
    }

    #[test]
    fn real_dbus_value_type_survives_in_memory_signal_round_trip() {
        let msg = dbus::Message::new_signal("/test", PROPERTIES, "PropertiesChanged")
            .unwrap()
            .append3(
                CHARACTERISTIC,
                HashMap::from([("Value".to_owned(), property(vec![0xa1u8, 0x10, 2]))]),
                Vec::<String>::new(),
            );
        let (_, properties, _): (String, PropMap, Vec<String>) = msg.read3().unwrap();
        assert_eq!(
            prop_cast::<Vec<u8>>(&properties, "Value").unwrap(),
            &[0xa1, 0x10, 2]
        );
    }
}
