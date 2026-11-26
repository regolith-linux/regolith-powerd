//! Battery warnings for wireless devices: mice, keyboards, headsets and the
//! other peripherals UPower knows about.
//!
//! In the X11 session gnome-settings-daemon's power plugin warns about
//! them; this does the same for the Wayland session, with the plugin's texts.
//! Each device UPower enumerates (or adds later) is followed, apart from line
//! power, batteries and UPSes, which warn through the composite display
//! device in [`crate::battery`]. A device warns when its `WarningLevel`
//! rises to low or critical, once per level: the highest level announced is
//! remembered per device, by its serial number, until the device reports
//! the level none, even across disconnections.
//!
//! Every level a device reports counts, however briefly it lasts: property
//! changes are applied signal by signal, in the order UPower sends them. A
//! level is on record as announced only once the notification server took
//! the warning. The notification service may start late or restart; the
//! warnings up when it went away are shown again on its successor. An id is
//! only ever replaced or closed through the instance that issued it, and
//! never once it died: the service's news is taken in even while the daemon
//! waits for an answer.

use glib::g_log;
use std::collections::HashMap;
use std::future::poll_fn;
use std::pin::pin;
use std::task::Poll;
use zbus::export::futures_util::stream::{self, Stream};
use zbus::export::futures_util::StreamExt;
use zbus::export::ordered_stream::{self, OrderedStreamExt};
use zbus::fdo::{DBusProxy, PropertiesChanged, PropertiesProxy};
use zbus::names::InterfaceName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, MessageType};

use crate::battery::{Issued, Notifier, Outcome, ServiceEvent, UPowerProxy, Warning};

const UPOWER: &str = "org.freedesktop.UPower";
const DEVICE_INTERFACE: &str = "org.freedesktop.UPower.Device";
const RETRY_SECS: u32 = 5;
/// How many device property changes may wait while the daemon is busy
/// (zbus's default is 64). They are shared by all devices, and the system
/// bus stalls once they are queued up.
const MAX_QUEUED_CHANGES: usize = 1024;

/// The device properties that decide a warning.
const PROPERTIES: [&str; 5] = [
    "Type",
    "WarningLevel",
    "Percentage",
    "BatteryLevel",
    "Serial",
];

/// UPower's UpDeviceKind values that are not peripherals: line power,
/// battery and UPS.
const KIND_LINE_POWER: u32 = 1;
const KIND_UPS: u32 = 3;

/// How bad a peripheral's battery is. The action level is critical for a
/// peripheral: UPower takes no action for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Low,
    Critical,
}

/// What a `WarningLevel` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Level none: the battery is fine again.
    Recovered,
    /// Level low, critical or action.
    Warn(Severity),
    /// Unknown or discharging: nothing to say.
    Keep,
}

impl Change {
    pub fn from_upower(warning_level: u32) -> Change {
        match warning_level {
            1 => Change::Recovered,
            3 => Change::Warn(Severity::Low),
            4 | 5 => Change::Warn(Severity::Critical),
            _ => Change::Keep,
        }
    }
}

/// The kind's name in the title and the subject of the body, as
/// gnome-settings-daemon's power plugin has them, or `None` for a kind it
/// has no texts for.
fn kind_texts(kind: u32) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        5 => ("Mouse", "Wireless mouse"),
        6 => ("Keyboard", "Wireless keyboard"),
        7 => ("PDA", "PDA"),
        8 => ("Cell phone", "Cell phone"),
        9 => ("Media player", "Media player"),
        10 => ("Tablet", "Tablet"),
        11 => ("Attached computer", "Attached computer"),
        12 => ("Game controller", "Game controller"),
        13 => ("Pen", "Pen"),
        14 => ("Touchpad", "Touchpad"),
        17 => ("Headset", "Headset"),
        18 => ("Speaker", "Speaker"),
        19 => ("Headphones", "Headphones"),
        21 => ("Audio device", "Audio device"),
        22 => ("Remote", "Remote"),
        23 => ("Printer", "Printer"),
        24 => ("Scanner", "Scanner"),
        25 => ("Camera", "Camera"),
        28 => ("Bluetooth device", "Bluetooth device"),
        _ => return None,
    })
}

/// The notification for a peripheral. `battery_level` is UPower's
/// `BatteryLevel`: unknown (0) or none (1) for a device that reports a
/// percentage, a coarse level otherwise, which the body leaves out.
pub fn warning(severity: Severity, kind: u32, percentage: f64, battery_level: u32) -> Warning {
    let (summary, subject) = match kind_texts(kind) {
        Some((name, subject)) => (format!("{name} battery low"), subject),
        None => (
            "Connected device battery is low".to_string(),
            "A connected device",
        ),
    };
    // Headphones are the one plural.
    let verb = if kind == 19 { "are" } else { "is" };
    let percentage = match battery_level {
        0 | 1 => format!(" ({percentage:.0}%)"),
        _ => String::new(),
    };
    match severity {
        Severity::Low => Warning::low(
            summary,
            format!("{subject} {verb} low on power{percentage}"),
        ),
        Severity::Critical => {
            let closing = match kind {
                5..=10 => "This device will soon stop functioning if not charged.",
                _ => "The device will soon shutdown if not charged.",
            };
            Warning::critical(
                summary,
                format!("{subject} {verb} very low on power{percentage}. {closing}"),
            )
        }
    }
}

/// The properties of a device that decide its warning.
#[derive(Debug, Default, Clone, PartialEq)]
struct State {
    kind: u32,
    warning_level: u32,
    percentage: f64,
    battery_level: u32,
    serial: String,
}

impl State {
    /// Take a property's value, if it is one of [`PROPERTIES`] and of its
    /// type.
    fn set(&mut self, name: &str, value: &Value<'_>) {
        let uint = || value.downcast_ref::<u32>().copied();
        match name {
            "Type" => self.kind = uint().unwrap_or(self.kind),
            "WarningLevel" => self.warning_level = uint().unwrap_or(self.warning_level),
            "BatteryLevel" => self.battery_level = uint().unwrap_or(self.battery_level),
            "Percentage" => {
                if let Some(percentage) = value.downcast_ref::<f64>() {
                    self.percentage = *percentage;
                }
            }
            "Serial" => {
                if let Some(serial) = value.downcast_ref::<str>() {
                    self.serial = serial.to_string();
                }
            }
            _ => {}
        }
    }

    fn from_properties(properties: &HashMap<String, OwnedValue>) -> State {
        let mut state = State::default();
        for (name, value) in properties {
            state.set(name, value);
        }
        state
    }

    /// Line power, batteries and UPSes warn through the display device.
    fn is_peripheral(&self) -> bool {
        !(KIND_LINE_POWER..=KIND_UPS).contains(&self.kind)
    }
}

/// What tells devices apart across reconnections: the serial number, or the
/// object path for a device without one. A serial never stands for a path,
/// even one that reads the same.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Identity {
    Serial(String),
    Path(OwnedObjectPath),
}

impl Identity {
    fn of(serial: &str, path: &OwnedObjectPath) -> Identity {
        if serial.is_empty() {
            Identity::Path(path.clone())
        } else {
            Identity::Serial(serial.to_string())
        }
    }
}

/// One `PropertiesChanged` signal of a device: the values it carries and
/// the names of the properties it invalidates.
struct Changes {
    changed: HashMap<String, OwnedValue>,
    invalidated: Vec<String>,
}

enum Event {
    /// UPower (re)started: enumerate its devices.
    Started,
    Added(OwnedObjectPath),
    Removed(OwnedObjectPath),
    Changed(OwnedObjectPath, Changes),
}

/// A device's warning, from the notification service's side.
#[derive(Debug, Default, Clone, PartialEq)]
struct Shown {
    /// The severity of the warning the service accepted and still has up.
    severity: Option<Severity>,
    /// Its id, with the instance of the service that issued it; `None`
    /// while it still has to be shown again on a new instance.
    id: Option<Issued>,
    /// An escalation above what was announced that has not been accepted
    /// yet: there was no service, or `Notify` failed.
    pending: Option<Severity>,
}

/// A device being followed.
struct Followed {
    properties: PropertiesProxy<'static>,
    state: State,
    shown: Shown,
}

/// The decisions, apart from the D-Bus plumbing.
#[derive(Default)]
struct Warnings {
    /// The highest severity announced per identity. Outlives the device.
    announced: HashMap<Identity, Severity>,
}

/// What to do with a device's notification.
#[derive(Debug, PartialEq)]
enum Action {
    /// Nothing to say, not even an escalation still pending.
    Nothing,
    Close,
    /// Show this severity, unless it is already pending.
    Show(Severity),
    /// Nothing new to announce: drop an escalation still pending.
    Settle,
}

impl Warnings {
    fn update(&self, identity: &Identity, change: Change) -> Action {
        match change {
            Change::Keep => Action::Nothing,
            Change::Recovered => Action::Close,
            Change::Warn(severity) => {
                if self.announced.get(identity) >= Some(&severity) {
                    Action::Settle
                } else {
                    Action::Show(severity)
                }
            }
        }
    }

    /// The device is fine again: the next low level warns again.
    fn recovered(&mut self, identity: &Identity) {
        self.announced.remove(identity);
    }

    /// The notification server accepted a warning of this severity.
    fn accepted(&mut self, identity: Identity, severity: Severity) {
        let announced = self.announced.entry(identity).or_insert(severity);
        *announced = (*announced).max(severity);
    }
}

struct Watcher<'a> {
    system: &'a Connection,
    upower: UPowerProxy<'static>,
    notifier: Notifier<'a>,
    followed: HashMap<OwnedObjectPath, Followed>,
    warnings: Warnings,
    /// Whether a notification service appeared, to show again there what
    /// was up on its predecessor, or could not be shown.
    reshow: bool,
}

impl Watcher<'_> {
    /// Read UPower's devices, after a start or restart. Devices that are
    /// gone are dropped, the others followed afresh.
    async fn enumerate(&mut self) {
        let paths = match self.upower.enumerate_devices().await {
            Ok(paths) => paths,
            Err(e) => {
                g_log!(glib::LogLevel::Debug, "Cannot enumerate devices: {e}");
                return;
            }
        };
        let gone: Vec<_> = self
            .followed
            .keys()
            .filter(|path| !paths.contains(path))
            .cloned()
            .collect();
        for path in gone {
            self.remove(&path).await;
        }
        for path in paths {
            self.follow(path).await;
        }
    }

    /// Follow the device at `path`, or follow it afresh: read all of it. Its
    /// changes come in through the subscription [`watch`] holds for every
    /// device, so none is missed.
    async fn follow(&mut self, path: OwnedObjectPath) {
        let read = async {
            let properties = PropertiesProxy::builder(self.system)
                .destination(UPOWER)?
                .path(path.clone())?
                .cache_properties(zbus::CacheProperties::No)
                .build()
                .await?;
            let all = properties
                .get_all(InterfaceName::from_static_str_unchecked(DEVICE_INTERFACE))
                .await?;
            zbus::Result::Ok((properties, State::from_properties(&all)))
        }
        .await;
        let (properties, state) = match read {
            Ok(read) => read,
            Err(e) => {
                g_log!(glib::LogLevel::Debug, "Cannot read {}: {e}", path.as_str());
                self.remove(&path).await;
                return;
            }
        };
        let shown = self
            .followed
            .remove(&path)
            .map(|old| old.shown)
            .unwrap_or_default();
        self.followed.insert(
            path.clone(),
            Followed {
                properties,
                state,
                shown,
            },
        );
        self.evaluate(&path).await;
    }

    /// Apply one `PropertiesChanged` signal of a followed device: the values
    /// it carries as they are, and the invalidated properties read back.
    async fn changed(&mut self, path: &OwnedObjectPath, changes: Changes) {
        let Some(followed) = self.followed.get_mut(path) else {
            return;
        };
        for (name, value) in &changes.changed {
            followed.state.set(name, value);
        }
        for name in &changes.invalidated {
            if !PROPERTIES.contains(&name.as_str()) {
                continue;
            }
            match followed
                .properties
                .get(
                    InterfaceName::from_static_str_unchecked(DEVICE_INTERFACE),
                    name,
                )
                .await
            {
                Ok(value) => followed.state.set(name, &value),
                Err(e) => g_log!(
                    glib::LogLevel::Debug,
                    "Cannot read {name} of {}: {e}",
                    path.as_str()
                ),
            }
        }
        self.evaluate(path).await;
    }

    /// Act on a followed device's current state: warn, or withdraw its
    /// warning, as needed.
    async fn evaluate(&mut self, path: &OwnedObjectPath) {
        self.settle();
        let Some(followed) = self.followed.get_mut(path) else {
            return;
        };
        let state = &followed.state;
        let identity = Identity::of(&state.serial, path);
        let action = if state.is_peripheral() {
            self.warnings
                .update(&identity, Change::from_upower(state.warning_level))
        } else {
            // Not a peripheral (any longer): it has no warning of its own.
            Action::Close
        };
        match action {
            Action::Nothing => {
                // Unknown or discharging says nothing about the battery: an
                // escalation that was refused waits for the next rise.
                followed.shown.pending = None;
                return;
            }
            Action::Close => {
                if state.is_peripheral() {
                    self.warnings.recovered(&identity);
                }
                let shown = std::mem::take(&mut followed.shown);
                if let Some(id) = &shown.id {
                    self.notifier.close(id).await;
                }
            }
            Action::Settle => followed.shown.pending = None,
            Action::Show(severity) => followed.shown.pending = Some(severity),
        }
        self.show(path).await;
    }

    /// Show what a followed device has still to show: a pending
    /// escalation, or the warning that was up when the notification service
    /// was replaced.
    async fn show(&mut self, path: &OwnedObjectPath) {
        self.settle();
        let Some(followed) = self.followed.get(path) else {
            return;
        };
        let shown = followed.shown.clone();
        let severity = match (shown.pending, shown.severity, &shown.id) {
            (Some(severity), _, _) => severity,
            (None, Some(severity), None) => severity,
            _ => return,
        };
        let state = &followed.state;
        let warning = warning(severity, state.kind, state.percentage, state.battery_level);
        let identity = Identity::of(&state.serial, path);
        // A critical warning takes the place of the low one.
        let outcome = self.notifier.notify(&warning, shown.id.as_ref()).await;
        let Some(followed) = self.followed.get_mut(path) else {
            return;
        };
        match outcome {
            Ok(outcome) => {
                followed.shown = Shown {
                    severity: Some(severity),
                    id: None,
                    pending: None,
                };
                match outcome {
                    Outcome::Up(id) => followed.shown.id = Some(id),
                    // Dismissed before the answer came: dead, as if the
                    // server had closed it later.
                    Outcome::Closed => followed.shown.severity = None,
                    // Shown again on the next instance of the service.
                    Outcome::Lost => {}
                }
                if shown.pending.is_some() {
                    self.warnings.accepted(identity, severity);
                }
            }
            // Not announced: what was announced before still stands, and
            // the next change, or a new notification service, tries again.
            Err(e) => g_log!(
                glib::LogLevel::Warning,
                "Cannot show device battery warning: {e}"
            ),
        }
    }

    /// Stop following a device that went away and withdraw its warning. What
    /// was announced for it is kept.
    async fn remove(&mut self, path: &OwnedObjectPath) {
        self.settle();
        if let Some(followed) = self.followed.remove(path) {
            if let Some(id) = &followed.shown.id {
                self.notifier.close(id).await;
            }
        }
    }

    /// Apply the notification service's news that has arrived. Done before
    /// each step, so a step never acts on an id that died meanwhile, even
    /// while the daemon waited for an answer.
    fn settle(&mut self) {
        while let Some(event) = self.notifier.ready() {
            self.service(event);
        }
    }

    fn service(&mut self, event: ServiceEvent) {
        match event {
            ServiceEvent::Closed(closed) => {
                // Dead from now on: never replaced, never closed.
                for followed in self.followed.values_mut() {
                    if followed.shown.id.as_ref() == Some(&closed) {
                        followed.shown.id = None;
                        followed.shown.severity = None;
                    }
                }
            }
            ServiceEvent::Owner { gone, owner } => {
                if let Some(gone) = gone {
                    // The ids of the instance that lost the service mean
                    // nothing to the next one; what it had up is shown
                    // again there.
                    for followed in self.followed.values_mut() {
                        let shown = &mut followed.shown;
                        if shown.id.as_ref().is_some_and(|id| id.is_from(&gone)) {
                            shown.id = None;
                        }
                    }
                }
                if owner.is_some() {
                    self.reshow = true;
                }
            }
        }
    }

    async fn handle(&mut self, event: Event) {
        match event {
            Event::Started => self.enumerate().await,
            Event::Added(path) => self.follow(path).await,
            Event::Removed(path) => self.remove(&path).await,
            Event::Changed(path, changes) => self.changed(&path, changes).await,
        }
    }
}

/// What the device warnings wait for.
enum Next {
    Service(ServiceEvent),
    UPower(Event),
    /// Nothing from UPower is waiting: show again what is to be shown again.
    Reshow,
}

/// Follow UPower's devices and notify until a connection fails.
pub async fn watch(system: &Connection, session: &Connection) -> zbus::Result<()> {
    let upower = UPowerProxy::new(system).await?;
    let dbus = DBusProxy::new(system).await?;

    // Subscribe before enumerating, so that no device slips through.
    let notifier = Notifier::new(session).await?;
    // UPower's comings and goings and every device's property changes, as
    // one stream in the order UPower sent them: each level a device reports
    // is acted on before the device's removal, and a device removed and
    // added again right away stays followed. One subscription for all
    // devices: no change is dropped when a device goes away.
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(UPOWER)?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .add_arg(DEVICE_INTERFACE)?
        .build();
    let changed = OrderedStreamExt::filter_map(
        MessageStream::for_match_rule(rule, system, Some(MAX_QUEUED_CHANGES)).await?,
        |message| {
            let path = OwnedObjectPath::from(message.as_ref().ok()?.path()?);
            let signal = PropertiesChanged::from_message(message.ok()?)?;
            let args = signal.args().ok()?;
            if args.interface_name().as_str() != DEVICE_INTERFACE {
                return None;
            }
            let changed = args
                .changed_properties()
                .iter()
                .map(|(name, value)| (name.to_string(), OwnedValue::from(value)))
                .collect();
            let invalidated = args
                .invalidated_properties()
                .iter()
                .map(|name| name.to_string())
                .collect();
            Some(Event::Changed(
                path,
                Changes {
                    changed,
                    invalidated,
                },
            ))
        },
    );
    let started = OrderedStreamExt::filter_map(
        dbus.receive_name_owner_changed_with_args(&[(0, UPOWER)])
            .await?,
        |signal| {
            let started = signal.args().ok()?.new_owner().is_some();
            started.then_some(Event::Started)
        },
    );
    let added = OrderedStreamExt::filter_map(upower.receive_device_added().await?, |signal| {
        Some(Event::Added(signal.args().ok()?.device().to_owned().into()))
    });
    let removed = OrderedStreamExt::filter_map(upower.receive_device_removed().await?, |signal| {
        Some(Event::Removed(
            signal.args().ok()?.device().to_owned().into(),
        ))
    });
    let events = stream::once(async { Event::Started }).chain(
        ordered_stream::join(
            ordered_stream::join(started, changed),
            ordered_stream::join(added, removed),
        )
        .into_stream(),
    );
    let mut events = pin!(events);

    let mut watcher = Watcher {
        system,
        upower,
        notifier,
        followed: HashMap::new(),
        warnings: Warnings::default(),
        reshow: false,
    };
    loop {
        watcher.settle();
        let reshow = watcher.reshow;
        let next = poll_fn(|cx| {
            // The notification service's news first: which ids died.
            if let Poll::Ready(event) = watcher.notifier.poll_next(cx) {
                return Poll::Ready(Some(Next::Service(event)));
            }
            match events.as_mut().poll_next(cx) {
                Poll::Ready(event) => Poll::Ready(event.map(Next::UPower)),
                Poll::Pending if reshow => Poll::Ready(Some(Next::Reshow)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await;
        match next {
            None => return Ok(()),
            Some(Next::Service(event)) => watcher.service(event),
            Some(Next::UPower(event)) => watcher.handle(event).await,
            Some(Next::Reshow) => {
                watcher.reshow = false;
                let paths: Vec<_> = watcher.followed.keys().cloned().collect();
                for path in paths {
                    watcher.show(&path).await;
                }
            }
        }
    }
}

/// Run the device warnings for the lifetime of the daemon.
pub async fn run_warnings() {
    let mut level = glib::LogLevel::Warning;
    loop {
        let result = async {
            let system = Connection::system().await?;
            let session = Connection::session().await?;
            watch(&system, &session).await
        }
        .await;
        if let Err(e) = result {
            g_log!(level, "Device battery warnings unavailable: {e}");
            level = glib::LogLevel::Debug;
        }
        glib::timeout_future_seconds(RETRY_SECS).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOP: &str = "This device will soon stop functioning if not charged.";
    const SHUT: &str = "The device will soon shutdown if not charged.";

    fn texts(warning: Warning) -> (String, String) {
        (warning.summary, warning.body)
    }

    #[test]
    fn levels_map_to_severities() {
        assert_eq!(Change::from_upower(0), Change::Keep);
        assert_eq!(Change::from_upower(1), Change::Recovered);
        assert_eq!(Change::from_upower(2), Change::Keep);
        assert_eq!(Change::from_upower(3), Change::Warn(Severity::Low));
        assert_eq!(Change::from_upower(4), Change::Warn(Severity::Critical));
        assert_eq!(Change::from_upower(5), Change::Warn(Severity::Critical));
    }

    #[test]
    fn texts_follow_gnome() {
        assert_eq!(
            texts(warning(Severity::Low, 5, 9.4, 1)),
            (
                "Mouse battery low".into(),
                "Wireless mouse is low on power (9%)".into()
            )
        );
        assert_eq!(
            texts(warning(Severity::Critical, 5, 4.0, 0)),
            (
                "Mouse battery low".into(),
                format!("Wireless mouse is very low on power (4%). {STOP}")
            )
        );
        assert_eq!(
            texts(warning(Severity::Critical, 17, 8.0, 3)),
            (
                "Headset battery low".into(),
                format!("Headset is very low on power. {SHUT}")
            )
        );
        assert_eq!(
            texts(warning(Severity::Low, 19, 20.6, 1)),
            (
                "Headphones battery low".into(),
                "Headphones are low on power (21%)".into()
            )
        );
        assert_eq!(
            texts(warning(Severity::Low, 16, 18.0, 6)),
            (
                "Connected device battery is low".into(),
                "A connected device is low on power".into()
            )
        );
    }

    #[test]
    fn fields_follow_the_severity() {
        let low = warning(Severity::Low, 6, 10.0, 1);
        assert_eq!(
            (low.icon, low.urgency, low.transient, low.timeout_ms),
            ("battery-low-symbolic", 1, true, 30_000)
        );
        let critical = warning(Severity::Critical, 6, 3.0, 1);
        assert_eq!(
            (
                critical.icon,
                critical.urgency,
                critical.transient,
                critical.timeout_ms
            ),
            ("battery-caution-symbolic", 2, false, 0)
        );
    }

    fn serial(serial: &str) -> Identity {
        Identity::Serial(serial.into())
    }

    #[test]
    fn serial_or_path_identifies_a_device() {
        let path = OwnedObjectPath::try_from("/org/freedesktop/UPower/devices/mouse").unwrap();
        assert_eq!(Identity::of("M1", &path), serial("M1"));
        assert_eq!(Identity::of("", &path), Identity::Path(path.clone()));
        // A serial that reads like another device's path is still a serial.
        assert_ne!(Identity::of(path.as_str(), &path), Identity::of("", &path));
    }

    /// Feed `change` and, when it asks for a warning, let the server accept
    /// it (or not).
    fn step(warnings: &mut Warnings, identity: &str, change: Change, accept: bool) -> Action {
        let identity = serial(identity);
        let action = warnings.update(&identity, change);
        match action {
            Action::Close => warnings.recovered(&identity),
            Action::Show(severity) if accept => warnings.accepted(identity, severity),
            _ => {}
        }
        action
    }

    #[test]
    fn warns_once_per_rising_severity_until_none() {
        let mut warnings = Warnings::default();
        let low = Change::Warn(Severity::Low);
        let critical = Change::Warn(Severity::Critical);
        let steps = [
            (low, Action::Show(Severity::Low)),
            (low, Action::Settle),
            (Change::Keep, Action::Nothing),
            (critical, Action::Show(Severity::Critical)),
            (Change::from_upower(5), Action::Settle),
            (low, Action::Settle),
            (critical, Action::Settle),
            (Change::Recovered, Action::Close),
            (low, Action::Show(Severity::Low)),
            (Change::from_upower(5), Action::Show(Severity::Critical)),
        ];
        for (index, (change, action)) in steps.into_iter().enumerate() {
            assert_eq!(
                step(&mut warnings, "M1", change, true),
                action,
                "step {index}"
            );
        }
        // Another device has a memory of its own.
        assert_eq!(
            step(&mut warnings, "K1", low, true),
            Action::Show(Severity::Low)
        );
    }

    #[test]
    fn a_refused_warning_is_not_announced() {
        let mut warnings = Warnings::default();
        let low = Change::Warn(Severity::Low);
        let critical = Change::Warn(Severity::Critical);
        assert_eq!(
            step(&mut warnings, "M1", low, true),
            Action::Show(Severity::Low)
        );
        // The critical warning fails: the device is still on record as low.
        assert_eq!(
            step(&mut warnings, "M1", critical, false),
            Action::Show(Severity::Critical)
        );
        assert_eq!(step(&mut warnings, "M1", low, true), Action::Settle);
        assert_eq!(
            step(&mut warnings, "M1", critical, true),
            Action::Show(Severity::Critical)
        );
        assert_eq!(step(&mut warnings, "M1", critical, true), Action::Settle);
    }

    #[test]
    fn signals_apply_only_what_they_carry() {
        let mut state = State {
            kind: 5,
            warning_level: 3,
            percentage: 9.0,
            battery_level: 1,
            serial: "M1".into(),
        };
        state.set("WarningLevel", &Value::U32(1));
        state.set("Percentage", &Value::F64(12.5));
        // Wrong types and other properties leave the state alone.
        state.set("Type", &Value::Str("mouse".into()));
        state.set("IconName", &Value::Str("battery".into()));
        assert_eq!(
            state,
            State {
                kind: 5,
                warning_level: 1,
                percentage: 12.5,
                battery_level: 1,
                serial: "M1".into(),
            }
        );
    }

    #[test]
    fn excluded_kinds_are_not_peripherals() {
        for (kind, peripheral) in [(0, true), (1, false), (2, false), (3, false), (5, true)] {
            let state = State {
                kind,
                ..State::default()
            };
            assert_eq!(state.is_peripheral(), peripheral, "kind {kind}");
        }
    }

    use crate::battery::tests::{assert_own_ids, private_bus, wait_up, Bus, Call, Calls};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use zbus::export::futures_util::lock::Mutex as Gate;
    use zbus::zvariant::ObjectPath;
    use zbus::{dbus_interface, ConnectionBuilder, SignalContext};

    struct FakeUPower {
        devices: Vec<OwnedObjectPath>,
    }

    #[dbus_interface(name = "org.freedesktop.UPower")]
    impl FakeUPower {
        fn enumerate_devices(&self) -> Vec<OwnedObjectPath> {
            self.devices.clone()
        }
        #[dbus_interface(signal)]
        async fn device_added(ctxt: &SignalContext<'_>, device: ObjectPath<'_>)
            -> zbus::Result<()>;
        #[dbus_interface(signal)]
        async fn device_removed(
            ctxt: &SignalContext<'_>,
            device: ObjectPath<'_>,
        ) -> zbus::Result<()>;
    }

    struct FakeDevice(State);

    #[dbus_interface(name = "org.freedesktop.UPower.Device")]
    impl FakeDevice {
        #[dbus_interface(property, name = "Type")]
        fn kind(&self) -> u32 {
            self.0.kind
        }
        #[dbus_interface(property)]
        fn warning_level(&self) -> u32 {
            self.0.warning_level
        }
        #[dbus_interface(property)]
        fn percentage(&self) -> f64 {
            self.0.percentage
        }
        #[dbus_interface(property)]
        fn battery_level(&self) -> u32 {
            self.0.battery_level
        }
        #[dbus_interface(property)]
        fn serial(&self) -> String {
            self.0.serial.clone()
        }
    }

    /// A notification server that records what it is asked to do, refuses
    /// to show anything while `refuse` is set, takes `delay_ms` to show a
    /// notification and holds back its answer while `gate` is locked.
    struct FakeNotifications {
        log: Arc<Mutex<Vec<String>>>,
        calls: Calls,
        refuse: Arc<AtomicBool>,
        delay_ms: Arc<AtomicU64>,
        gate: Arc<Gate<()>>,
        next_id: u32,
    }

    #[dbus_interface(name = "org.freedesktop.Notifications")]
    impl FakeNotifications {
        #[allow(clippy::too_many_arguments)]
        async fn notify(
            &mut self,
            _app_name: &str,
            replaces_id: u32,
            _app_icon: &str,
            summary: &str,
            body: &str,
            _actions: Vec<&str>,
            _hints: HashMap<&str, Value<'_>>,
            _expire_timeout: i32,
        ) -> zbus::fdo::Result<u32> {
            std::thread::sleep(Duration::from_millis(self.delay_ms.load(Ordering::SeqCst)));
            if self.refuse.load(Ordering::SeqCst) {
                self.log.lock().unwrap().push(format!("refused {summary}"));
                return Err(zbus::fdo::Error::Failed("refused".into()));
            }
            let id = if replaces_id == 0 {
                self.next_id += 1;
                self.next_id
            } else {
                replaces_id
            };
            let text = format!("{summary}: {body}");
            self.log
                .lock()
                .unwrap()
                .push(format!("notify {id} replaces {replaces_id} {text}"));
            self.calls.lock().unwrap().push(Call::Notify {
                id,
                replaces: replaces_id,
                text,
            });
            let _open = self.gate.lock().await;
            Ok(id)
        }
        fn close_notification(&mut self, id: u32) {
            self.log.lock().unwrap().push(format!("close {id}"));
            self.calls.lock().unwrap().push(Call::Close(id));
        }
        #[dbus_interface(signal)]
        async fn notification_closed(
            ctxt: &SignalContext<'_>,
            id: u32,
            reason: u32,
        ) -> zbus::Result<()>;
    }

    const UPOWER_PATH: &str = "/org/freedesktop/UPower";
    const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
    const MOUSE: &str = "/org/freedesktop/UPower/devices/mouse_dev_1";
    const KEYBOARD: &str = "/org/freedesktop/UPower/devices/keyboard_dev_2";
    const BATTERY: &str = "/org/freedesktop/UPower/devices/battery_BAT0";
    const MOUSE_LOW: &str = "Mouse battery low: Wireless mouse is low on power";

    fn mouse(warning_level: u32, percentage: f64) -> State {
        State {
            kind: 5,
            warning_level,
            percentage,
            battery_level: 1,
            serial: "M1".into(),
        }
    }

    fn mouse_critical(percentage: u32) -> String {
        format!("Mouse battery low: Wireless mouse is very low on power ({percentage}%). {STOP}")
    }

    /// A private bus, standing in for both the system and the session bus,
    /// with a fake UPower and the devices it starts with, and the daemon's
    /// device warnings following them.
    struct World {
        _bus: Bus,
        address: String,
        upower: Connection,
        notifications: Option<Connection>,
        /// Instances that gave up the service but stay connected.
        former: Vec<Connection>,
        log: Arc<Mutex<Vec<String>>>,
        /// The calls of each instance of the service, in order.
        calls: Vec<Calls>,
        refuse: Arc<AtomicBool>,
        delay_ms: Arc<AtomicU64>,
        gate: Arc<Gate<()>>,
    }

    impl World {
        fn new(devices: &[(&str, State)], notifications: bool) -> World {
            let (bus, address) = private_bus();
            let upower = zbus::block_on(async {
                let paths = devices
                    .iter()
                    .map(|(path, _)| OwnedObjectPath::try_from(*path).unwrap())
                    .collect();
                let mut builder = ConnectionBuilder::address(address.as_str())?
                    .name(UPOWER)?
                    .serve_at(UPOWER_PATH, FakeUPower { devices: paths })?;
                for (path, state) in devices {
                    builder = builder.serve_at(*path, FakeDevice(state.clone()))?;
                }
                builder.build().await
            })
            .unwrap();
            let mut world = World {
                _bus: bus,
                address,
                upower,
                notifications: None,
                former: Vec::new(),
                log: Arc::default(),
                calls: Vec::new(),
                refuse: Arc::default(),
                delay_ms: Arc::default(),
                gate: Arc::default(),
            };
            if notifications {
                world.start_notifications();
            }
            let address = world.address.clone();
            std::thread::spawn(move || {
                zbus::block_on(async {
                    let conn = ConnectionBuilder::address(address.as_str())?
                        .build()
                        .await?;
                    watch(&conn, &conn).await
                })
            });
            // Let the daemon subscribe and read the devices.
            std::thread::sleep(Duration::from_millis(500));
            world
        }

        /// Start a new instance of the notification service, in place of
        /// the one there may be.
        fn start_notifications(&mut self) {
            if let Some(old) = self.notifications.take() {
                zbus::block_on(old.release_name("org.freedesktop.Notifications")).unwrap();
            }
            let calls = Calls::default();
            self.calls.push(calls.clone());
            let server = FakeNotifications {
                log: self.log.clone(),
                calls,
                refuse: self.refuse.clone(),
                delay_ms: self.delay_ms.clone(),
                gate: self.gate.clone(),
                next_id: 0,
            };
            let address = self.address.clone();
            self.notifications = Some(
                zbus::block_on(async {
                    ConnectionBuilder::address(address.as_str())?
                        .name("org.freedesktop.Notifications")?
                        .serve_at(NOTIFICATIONS_PATH, server)?
                        .build()
                        .await
                })
                .unwrap(),
            );
        }

        /// Hand the notification service over to a new instance. The old
        /// one stays connected, to answer what it was asked before.
        fn hand_over(&mut self) {
            let old = self.notifications.take().unwrap();
            zbus::block_on(old.release_name("org.freedesktop.Notifications")).unwrap();
            self.former.push(old);
            self.start_notifications();
        }

        /// The notification service closes a notification by itself.
        fn expire(&self, id: u32) {
            let conn = self.notifications.as_ref().unwrap();
            zbus::block_on(async {
                let ctxt = SignalContext::new(conn, NOTIFICATIONS_PATH).unwrap();
                FakeNotifications::notification_closed(&ctxt, id, 1)
                    .await
                    .unwrap();
            });
            // It comes on the session bus, device changes on the system bus:
            // let it arrive first.
            std::thread::sleep(Duration::from_millis(200));
        }

        /// Change properties of a device and announce them in one
        /// `PropertiesChanged` signal, as UPower does.
        fn set(&self, path: &str, changes: &[(&str, Value<'_>)]) {
            zbus::block_on(async {
                let iface = self
                    .upower
                    .object_server()
                    .interface::<_, FakeDevice>(path)
                    .await
                    .unwrap();
                let mut device = iface.get_mut().await;
                let mut changed = HashMap::new();
                for (name, value) in changes {
                    device.0.set(name, value);
                    changed.insert(*name, value);
                }
                zbus::fdo::Properties::properties_changed(
                    iface.signal_context(),
                    InterfaceName::from_static_str_unchecked(DEVICE_INTERFACE),
                    &changed,
                    &[],
                )
                .await
                .unwrap();
            });
        }

        fn set_level(&self, path: &str, warning_level: u32, percentage: f64) {
            self.set(
                path,
                &[
                    ("WarningLevel", Value::U32(warning_level)),
                    ("Percentage", Value::F64(percentage)),
                ],
            );
        }

        fn add(&self, path: &str, state: State) {
            zbus::block_on(async {
                let server = self.upower.object_server();
                server.at(path, FakeDevice(state)).await.unwrap();
                let iface = server
                    .interface::<_, FakeUPower>(UPOWER_PATH)
                    .await
                    .unwrap();
                iface
                    .get_mut()
                    .await
                    .devices
                    .push(OwnedObjectPath::try_from(path).unwrap());
                let path = ObjectPath::try_from(path).unwrap();
                FakeUPower::device_added(iface.signal_context(), path)
                    .await
                    .unwrap();
            });
        }

        fn remove(&self, path: &str) {
            zbus::block_on(async {
                let server = self.upower.object_server();
                server.remove::<FakeDevice, _>(path).await.unwrap();
                let iface = server
                    .interface::<_, FakeUPower>(UPOWER_PATH)
                    .await
                    .unwrap();
                iface.get_mut().await.devices.retain(|p| p.as_str() != path);
                let path = ObjectPath::try_from(path).unwrap();
                FakeUPower::device_removed(iface.signal_context(), path)
                    .await
                    .unwrap();
            });
        }

        /// What the notification service was asked to do, once there are
        /// `len` entries (or after 5 s), plus anything that follows soon.
        fn wait_for(&self, len: usize) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.log.lock().unwrap().len() < len && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            // Give unexpected extra calls a chance to show up.
            std::thread::sleep(Duration::from_millis(300));
            self.log.lock().unwrap().clone()
        }

        /// Check that the log grew by exactly `calls`.
        #[track_caller]
        fn expect(&self, expected: &mut Vec<String>, calls: &[&str]) {
            expected.extend(calls.iter().map(|call| call.to_string()));
            assert_eq!(&self.wait_for(expected.len()), expected);
        }
    }

    #[test]
    fn warns_about_each_device_and_withdraws_the_warning() {
        let world = World::new(
            &[
                (MOUSE, mouse(1, 50.0)),
                // Warns through the display device, never here.
                (
                    BATTERY,
                    State {
                        kind: 2,
                        warning_level: 3,
                        percentage: 9.0,
                        battery_level: 1,
                        serial: "BAT0".into(),
                    },
                ),
            ],
            true,
        );
        let log = &mut Vec::new();
        world.expect(log, &[]);

        world.set_level(MOUSE, 3, 9.4);
        world.expect(log, &[&format!("notify 1 replaces 0 {MOUSE_LOW} (9%)")]);
        world.set_level(MOUSE, 3, 8.0);
        world.expect(log, &[]);
        // The critical warning takes the low one's place.
        world.set_level(MOUSE, 4, 4.0);
        world.expect(
            log,
            &[&format!("notify 1 replaces 1 {}", mouse_critical(4))],
        );
        world.set_level(MOUSE, 5, 2.0);
        world.expect(log, &[]);
        world.set_level(MOUSE, 1, 2.0);
        world.expect(log, &["close 1"]);

        // A device that shows up later, with coarse levels only.
        let keyboard = State {
            kind: 6,
            warning_level: 3,
            percentage: 10.0,
            battery_level: 4,
            serial: "K1".into(),
        };
        world.add(KEYBOARD, keyboard.clone());
        world.expect(
            log,
            &["notify 2 replaces 0 Keyboard battery low: Wireless keyboard is low on power"],
        );
        world.remove(KEYBOARD);
        world.expect(log, &["close 2"]);
        // Back at the level it was announced at: nothing new to say.
        world.add(KEYBOARD, keyboard);
        world.expect(log, &[]);
    }

    #[test]
    fn follows_the_notification_service() {
        let mut world = World::new(&[(MOUSE, mouse(3, 9.0))], false);
        let log = &mut Vec::new();
        // Due while there is no notification service: shown once there is.
        world.start_notifications();
        world.expect(log, &[&format!("notify 1 replaces 0 {MOUSE_LOW} (9%)")]);
        // Expired: never replaced or closed afterwards.
        world.expire(1);
        world.set_level(MOUSE, 4, 4.0);
        world.expect(
            log,
            &[&format!("notify 2 replaces 0 {}", mouse_critical(4))],
        );
        // A new instance of the service shows what is up again, with ids of
        // its own.
        world.start_notifications();
        world.expect(
            log,
            &[&format!("notify 1 replaces 0 {}", mouse_critical(4))],
        );
        world.set_level(MOUSE, 1, 30.0);
        world.expect(log, &["close 1"]);
    }

    /// Devices that go away and come back while the daemon is busy are
    /// handled in the order UPower reported them.
    #[test]
    fn a_device_removed_and_added_again_stays_followed() {
        let world = World::new(
            &[
                (MOUSE, mouse(1, 50.0)),
                (
                    KEYBOARD,
                    State {
                        kind: 6,
                        warning_level: 1,
                        percentage: 50.0,
                        battery_level: 1,
                        serial: "K1".into(),
                    },
                ),
            ],
            true,
        );
        let log = &mut Vec::new();
        // While the daemon waits for its warning to be shown, both devices
        // go away, and the mouse comes back.
        world.delay_ms.store(500, Ordering::SeqCst);
        world.set_level(MOUSE, 3, 9.0);
        world.remove(KEYBOARD);
        world.remove(MOUSE);
        world.add(MOUSE, mouse(3, 9.0));
        world.expect(
            log,
            &[&format!("notify 1 replaces 0 {MOUSE_LOW} (9%)"), "close 1"],
        );
        world.delay_ms.store(0, Ordering::SeqCst);
        world.set_level(MOUSE, 4, 4.0);
        world.expect(
            log,
            &[&format!("notify 2 replaces 0 {}", mouse_critical(4))],
        );
    }

    /// A critical warning the server refused is not sent later at a level
    /// that never warns; the next rise above low tries again.
    #[test]
    fn a_refused_escalation_waits_for_the_next_rise() {
        let world = World::new(&[(MOUSE, mouse(3, 9.0))], true);
        let log = &mut Vec::new();
        world.expect(log, &[&format!("notify 1 replaces 0 {MOUSE_LOW} (9%)")]);
        world.refuse.store(true, Ordering::SeqCst);
        world.set_level(MOUSE, 4, 4.0);
        world.expect(log, &["refused Mouse battery low"]);
        world.refuse.store(false, Ordering::SeqCst);
        // Discharging and unknown say nothing about a device.
        world.set_level(MOUSE, 2, 4.0);
        world.set_level(MOUSE, 0, 4.0);
        world.expect(log, &[]);
        // Still on record as low, so this escalates.
        world.set_level(MOUSE, 5, 3.0);
        world.expect(
            log,
            &[&format!("notify 1 replaces 1 {}", mouse_critical(3))],
        );
    }

    /// A burst of levels, then the device's removal (and its return), all
    /// reported while the notification server is still busy with another
    /// device's warning: every level is acted on, before the removal.
    fn burst_before_removal(add_again: bool) {
        let world = World::new(
            &[
                (MOUSE, mouse(3, 9.0)),
                (
                    KEYBOARD,
                    State {
                        kind: 6,
                        warning_level: 1,
                        percentage: 50.0,
                        battery_level: 1,
                        serial: "K1".into(),
                    },
                ),
            ],
            true,
        );
        let log = &mut Vec::new();
        world.expect(log, &[&format!("notify 1 replaces 0 {MOUSE_LOW} (9%)")]);
        world.delay_ms.store(500, Ordering::SeqCst);
        world.set_level(KEYBOARD, 3, 8.0);
        world.set_level(MOUSE, 1, 9.0);
        world.set_level(MOUSE, 3, 9.0);
        world.remove(MOUSE);
        if add_again {
            world.add(MOUSE, mouse(3, 9.0));
        }
        world.expect(
            log,
            &[
                "notify 2 replaces 0 Keyboard battery low: Wireless keyboard is low on power (8%)",
                "close 1",
                &format!("notify 3 replaces 0 {MOUSE_LOW} (9%)"),
                "close 3",
            ],
        );
    }

    #[test]
    fn levels_reported_before_a_removal_count() {
        burst_before_removal(false);
    }

    /// Back at the level it was last announced at: nothing to say, and no
    /// warning left up.
    #[test]
    fn levels_reported_before_a_removal_count_when_the_device_returns() {
        burst_before_removal(true);
    }

    /// A serial that reads like another device's object path does not make
    /// the two one device.
    #[test]
    fn a_serial_never_stands_for_a_path() {
        let world = World::new(
            &[
                (
                    MOUSE,
                    State {
                        serial: KEYBOARD.into(),
                        ..mouse(3, 9.0)
                    },
                ),
                (
                    KEYBOARD,
                    State {
                        kind: 6,
                        warning_level: 3,
                        percentage: 8.0,
                        battery_level: 1,
                        serial: String::new(),
                    },
                ),
            ],
            true,
        );
        // Followed in the order UPower lists them.
        world.expect(
            &mut Vec::new(),
            &[
                &format!("notify 1 replaces 0 {MOUSE_LOW} (9%)"),
                "notify 2 replaces 0 Keyboard battery low: Wireless keyboard is low on power (8%)",
            ],
        );
    }

    /// The notification service changes hands while the old instance still
    /// owes the daemon an answer, and another device recovers: no id of the
    /// old instance reaches the new one, which shows what is still up.
    #[test]
    fn ids_stay_with_the_instance_that_issued_them() {
        let keyboard = State {
            kind: 6,
            warning_level: 1,
            percentage: 10.0,
            battery_level: 1,
            serial: "K1".into(),
        };
        let mut world = World::new(&[(MOUSE, mouse(3, 9.0)), (KEYBOARD, keyboard)], true);
        world.expect(
            &mut Vec::new(),
            &[&format!("notify 1 replaces 0 {MOUSE_LOW} (9%)")],
        );
        let gate = world.gate.clone();
        let held = zbus::block_on(gate.lock());
        world.set_level(KEYBOARD, 3, 10.0);
        std::thread::sleep(Duration::from_millis(200));
        world.set_level(MOUSE, 1, 50.0);
        world.hand_over();
        drop(held);
        let keyboard_low = "Keyboard battery low: Wireless keyboard is low on power (10%)";
        assert_eq!(wait_up(&world.calls[1], &[keyboard_low]), [keyboard_low]);
        // Both devices are followed on the new instance.
        world.set_level(KEYBOARD, 1, 50.0);
        world.set_level(MOUSE, 3, 9.0);
        let mouse_low = format!("{MOUSE_LOW} (9%)");
        assert_eq!(wait_up(&world.calls[1], &[&mouse_low]), [mouse_low]);
        for (instance, calls) in world.calls.iter().enumerate() {
            assert_own_ids(instance, calls);
        }
    }

    /// While the daemon waits for the answer to the keyboard's warning, the
    /// server closes the mouse's warning and the keyboard's own (before its
    /// answer came), and both devices get worse: neither closed id is
    /// replaced or closed again, the critical warnings are new ones.
    #[test]
    fn ids_closed_during_a_call_stay_dead() {
        let keyboard = State {
            kind: 6,
            warning_level: 1,
            percentage: 10.0,
            battery_level: 1,
            serial: "K1".into(),
        };
        let world = World::new(&[(MOUSE, mouse(3, 9.0)), (KEYBOARD, keyboard)], true);
        let keyboard_low = "Keyboard battery low: Wireless keyboard is low on power (10%)";
        let log = &mut Vec::new();
        world.expect(log, &[&format!("notify 1 replaces 0 {MOUSE_LOW} (9%)")]);
        let gate = world.gate.clone();
        let held = zbus::block_on(gate.lock());
        world.set_level(KEYBOARD, 3, 10.0);
        world.expect(log, &[&format!("notify 2 replaces 0 {keyboard_low}")]);
        world.expire(1);
        world.expire(2);
        world.set_level(MOUSE, 4, 4.0);
        world.set_level(KEYBOARD, 4, 4.0);
        std::thread::sleep(Duration::from_millis(200));
        drop(held);
        world.expect(
            log,
            &[
                &format!("notify 3 replaces 0 {}", mouse_critical(4)),
                &format!(
                    "notify 4 replaces 0 Keyboard battery low: Wireless keyboard is very low on power (4%). {STOP}"
                ),
            ],
        );
        // Fine again: only the new warnings are closed.
        world.set_level(MOUSE, 1, 50.0);
        world.set_level(KEYBOARD, 1, 50.0);
        world.expect(log, &["close 3", "close 4"]);
    }
}
