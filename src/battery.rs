//! Low-battery notifications for the Wayland session.
//!
//! In the X11 session gnome-settings-daemon's power plugin sends these; the
//! Wayland session does not run that plugin, regolith-powerd replaces it.
//! Like the plugin, this follows the `WarningLevel` that UPower computes for
//! its composite display device from the thresholds in UPower.conf: entering
//! the low, critical or action level shows one notification, which is closed
//! when the level drops back to none (on AC power or charged). A UPS also
//! warns when it starts discharging, that is when mains power fails. Every
//! change UPower signals is acted on, in order, so a level that lasts only
//! a moment still shows and withdraws its warning.
//!
//! The batteries of wireless devices are followed in [`crate::devices`].

use glib::g_log;
use std::collections::HashMap;
use std::pin::Pin;
use zbus::export::futures_util::stream::{self, Stream};
use zbus::export::futures_util::StreamExt;
use zbus::export::ordered_stream::{self, OrderedStreamExt};
use zbus::fdo::{DBusProxy, PropertiesProxy};
use zbus::names::{InterfaceName, OwnedUniqueName, WellKnownName};
use zbus::zvariant::{OwnedValue, Value};
use zbus::{dbus_proxy, Connection};

const APP_NAME: &str = "Power";
const LOW_TIMEOUT_MS: i32 = 30_000;
const RETRY_SECS: u32 = 60;
const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const UPOWER: &str = "org.freedesktop.UPower";
const DEVICE_INTERFACE: &str = "org.freedesktop.UPower.Device";
const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

// UPower's UpDeviceKind values for the display device.
const KIND_UPS: u32 = 3;

#[dbus_proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
pub(crate) trait UPower {
    fn get_critical_action(&self) -> zbus::Result<String>;
    fn enumerate_devices(&self) -> zbus::Result<Vec<zbus::zvariant::OwnedObjectPath>>;
    #[dbus_proxy(signal)]
    fn device_added(&self, device: zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;
    #[dbus_proxy(signal)]
    fn device_removed(&self, device: zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;
}

#[dbus_proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[dbus_proxy(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// A notification's id, with the instance of the notification service that
/// issued it: its unique bus name. The id means nothing to any other
/// instance, so the notification is replaced and closed through its issuer
/// only, even while the service changes hands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Issued {
    owner: OwnedUniqueName,
    id: u32,
}

impl Issued {
    pub(crate) fn is_from(&self, owner: &OwnedUniqueName) -> bool {
        &self.owner == owner
    }
}

/// Show `warning`, in place of `replaces` if given. A new notification goes
/// to whoever owns the service (which may start it on demand), a
/// replacement to the instance that issued the id.
pub(crate) async fn notify(
    session: &Connection,
    warning: &Warning,
    replaces: Option<&Issued>,
) -> zbus::Result<Issued> {
    let destination = replaces.map_or(NOTIFICATIONS, |issued| issued.owner.as_str());
    let actions: &[&str] = &[];
    let reply = session
        .call_method(
            Some(destination),
            NOTIFICATIONS_PATH,
            Some(NOTIFICATIONS),
            "Notify",
            &(
                APP_NAME,
                replaces.map_or(0, |issued| issued.id),
                warning.icon,
                &warning.summary,
                &warning.body,
                actions,
                warning.hints(),
                warning.timeout_ms,
            ),
        )
        .await?;
    let id = reply.body::<u32>()?;
    // The instance that answered, whichever owned the name by then.
    let owner = reply
        .header()?
        .sender()?
        .ok_or_else(|| zbus::Error::Failure("Notify reply without a sender".into()))?
        .to_owned()
        .into();
    Ok(Issued { owner, id })
}

/// Close a notification, through the instance that issued it.
pub(crate) async fn close(session: &Connection, issued: &Issued) {
    if let Err(e) = session
        .call_method(
            Some(issued.owner.as_str()),
            NOTIFICATIONS_PATH,
            Some(NOTIFICATIONS),
            "CloseNotification",
            &issued.id,
        )
        .await
    {
        g_log!(glib::LogLevel::Debug, "Cannot close notification: {e}");
    }
}

/// What the notification service tells a source of warnings.
#[derive(Debug)]
pub(crate) enum ServiceEvent {
    /// `org.freedesktop.Notifications` changed hands: `gone` lost it, if it
    /// had an owner, and `owner` has it now, if anyone. The ids `gone`
    /// issued are dead from now on.
    Owner {
        gone: Option<OwnedUniqueName>,
        owner: Option<OwnedUniqueName>,
    },
    /// An instance closed one of its notifications by itself: it expired or
    /// the user dismissed it. The id is dead from now on.
    Closed(Issued),
}

/// Who owns the notification service right now, and the changes to come,
/// in the order the bus delivered them. Subscribed before asking, so no
/// change slips through.
pub(crate) async fn service_events(
    session: &Connection,
) -> zbus::Result<(
    Option<OwnedUniqueName>,
    Pin<Box<dyn Stream<Item = ServiceEvent>>>,
)> {
    let dbus = DBusProxy::new(session).await?;
    let owners = OrderedStreamExt::filter_map(
        dbus.receive_name_owner_changed_with_args(&[(0, NOTIFICATIONS)])
            .await?,
        |signal| {
            let args = signal.args().ok()?;
            Some(ServiceEvent::Owner {
                gone: args.old_owner().as_ref().map(|gone| gone.to_owned().into()),
                owner: args
                    .new_owner()
                    .as_ref()
                    .map(|owner| owner.to_owned().into()),
            })
        },
    );
    let closed = OrderedStreamExt::filter_map(
        NotificationsProxy::new(session)
            .await?
            .receive_notification_closed()
            .await?,
        |signal| {
            let id = signal.args().ok()?.id;
            let owner = signal.header().ok()?.sender().ok()??.to_owned().into();
            Some(ServiceEvent::Closed(Issued { owner, id }))
        },
    );
    let owner = dbus
        .get_name_owner(WellKnownName::from_static_str_unchecked(NOTIFICATIONS).into())
        .await
        .ok();
    Ok((
        owner,
        Box::pin(ordered_stream::join(owners, closed).into_stream()),
    ))
}

/// UPower's `WarningLevel`, reduced to the levels that warn on battery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    None,
    /// A UPS running on its battery; see [`Level::of_display`].
    Discharging,
    Low,
    Critical,
    Action,
}

impl Level {
    /// The level of a battery. "discharging" only warns for a UPS, see
    /// [`Level::of_display`].
    pub fn from_upower(warning_level: u32) -> Level {
        match warning_level {
            3 => Level::Low,
            4 => Level::Critical,
            5 => Level::Action,
            _ => Level::None,
        }
    }

    /// The level of the display device of type `kind`: like
    /// [`Level::from_upower`], except that a UPS that is discharging warns,
    /// as in gnome-settings-daemon.
    pub fn of_display(warning_level: u32, kind: u32) -> Level {
        match warning_level {
            2 if kind == KIND_UPS => Level::Discharging,
            _ => Level::from_upower(warning_level),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Warning {
    pub summary: String,
    pub body: String,
    pub icon: &'static str,
    /// Notification urgency: 1 normal, 2 critical.
    pub urgency: u8,
    /// Whether the notification is transient (not kept by the server).
    pub transient: bool,
    /// Expiry in milliseconds, 0 for never.
    pub timeout_ms: i32,
}

impl Warning {
    /// A low warning: it expires and is not kept.
    pub(crate) fn low(summary: String, body: String) -> Warning {
        Warning {
            summary,
            body,
            icon: "battery-low-symbolic",
            urgency: 1,
            transient: true,
            timeout_ms: LOW_TIMEOUT_MS,
        }
    }

    /// A critical warning: it stays until it is closed.
    pub(crate) fn critical(summary: String, body: String) -> Warning {
        Warning {
            summary,
            body,
            icon: "battery-caution-symbolic",
            urgency: 2,
            transient: false,
            timeout_ms: 0,
        }
    }

    /// The notification hints for this warning.
    pub(crate) fn hints(&self) -> HashMap<&'static str, Value<'static>> {
        let mut hints = HashMap::from([("urgency", Value::U8(self.urgency))]);
        if self.transient {
            hints.insert("transient", Value::Bool(true));
        }
        hints
    }
}

/// How long a battery lasts, as gnome-settings-daemon's
/// `gpm_get_timestring` puts it: whole minutes, rounded to the nearest.
pub fn time_text(seconds: i64) -> String {
    let minutes = (seconds as f64 / 60.0 + 0.5).floor() as i64;
    let plural = |n: i64, one: &str, many: &str| {
        if n == 1 {
            format!("{n} {one}")
        } else {
            format!("{n} {many}")
        }
    };
    if minutes <= 0 {
        return "Unknown time".to_string();
    }
    if minutes < 60 {
        return plural(minutes, "minute", "minutes");
    }
    let (hours, minutes) = (minutes / 60, minutes % 60);
    let hours = plural(hours, "hour", "hours");
    if minutes == 0 {
        hours
    } else {
        format!("{hours} {}", plural(minutes, "minute", "minutes"))
    }
}

/// The notification for a level, with the texts gnome-settings-daemon uses,
/// so both sessions warn alike. `time_to_empty` (seconds, 0 or less for
/// unknown) is reported while a UPS discharges; `critical_action` is what UPower does at
/// the action level (`GetCriticalAction`).
pub fn warning(
    level: Level,
    kind: u32,
    percentage: f64,
    time_to_empty: i64,
    critical_action: &str,
) -> Option<Warning> {
    let ups = kind == KIND_UPS;
    let (summary, body, icon, urgency, timeout_ms) = match level {
        Level::None => return None,
        Level::Discharging => {
            let remaining = if time_to_empty <= 0 {
                "Unknown amount".to_string()
            } else {
                time_text(time_to_empty)
            };
            return Some(Warning::low(
                "UPS Discharging".to_string(),
                format!("{remaining} of UPS backup power remaining ({percentage:.0}%)"),
            ));
        }
        Level::Low if ups => (
            "UPS Low",
            format!("{percentage:.0}% UPS power remaining"),
            "battery-low-symbolic",
            1,
            LOW_TIMEOUT_MS,
        ),
        Level::Low => (
            "Low Battery",
            format!("{percentage:.0}% battery remaining"),
            "battery-low-symbolic",
            1,
            LOW_TIMEOUT_MS,
        ),
        Level::Critical if ups => (
            "UPS Almost Empty",
            format!("{percentage:.0}% UPS power remaining"),
            "battery-caution-symbolic",
            2,
            0,
        ),
        Level::Critical => (
            "Battery Almost Empty",
            "Connect power now".to_string(),
            "battery-caution-symbolic",
            2,
            0,
        ),
        Level::Action => (
            if ups {
                "UPS is Empty"
            } else {
                "Battery is Empty"
            },
            match critical_action {
                "PowerOff" => "This device is about to shutdown",
                "Suspend" => "This device is about to suspend",
                _ => "This device is about to hibernate",
            }
            .to_string(),
            "battery-action-symbolic",
            2,
            0,
        ),
    };
    Some(Warning {
        summary: summary.to_string(),
        body,
        icon,
        urgency,
        transient: level == Level::Low,
        timeout_ms,
    })
}

/// The display device's properties that decide its warning.
#[derive(Debug, Default)]
struct Display {
    kind: u32,
    warning_level: u32,
    percentage: f64,
    /// Seconds, 0 for unknown.
    time_to_empty: i64,
}

impl Display {
    /// Read all of the display device.
    async fn read(proxy: &PropertiesProxy<'_>) -> zbus::Result<Display> {
        let mut display = Display::default();
        let interface = InterfaceName::from_static_str_unchecked(DEVICE_INTERFACE);
        for (name, value) in proxy.get_all(interface).await? {
            display.set(&name, &value);
        }
        Ok(display)
    }

    /// Take a property's value, if it is one that decides the warning and
    /// of its type.
    fn set(&mut self, name: &str, value: &Value<'_>) {
        match name {
            "Type" => self.kind = value.downcast_ref().copied().unwrap_or(self.kind),
            "WarningLevel" => {
                self.warning_level = value.downcast_ref().copied().unwrap_or(self.warning_level)
            }
            "Percentage" => {
                self.percentage = value.downcast_ref().copied().unwrap_or(self.percentage)
            }
            "TimeToEmpty" => {
                self.time_to_empty = value.downcast_ref().copied().unwrap_or(self.time_to_empty)
            }
            _ => {}
        }
    }

    /// Apply one `PropertiesChanged` signal: the values it carries as they
    /// are, and the invalidated properties read back.
    async fn apply(
        &mut self,
        changed: &HashMap<String, OwnedValue>,
        invalidated: &[String],
        proxy: &PropertiesProxy<'_>,
    ) {
        for (name, value) in changed {
            self.set(name, value);
        }
        for name in invalidated {
            let interface = InterfaceName::from_static_str_unchecked(DEVICE_INTERFACE);
            match proxy.get(interface, name).await {
                Ok(value) => self.set(name, &value),
                Err(e) => g_log!(
                    glib::LogLevel::Debug,
                    "Cannot read {name} of the display device: {e}"
                ),
            }
        }
    }

    fn level(&self) -> Level {
        Level::of_display(self.warning_level, self.kind)
    }
}

/// What the battery warnings follow.
enum Update {
    /// One `PropertiesChanged` signal of the display device: the values it
    /// carries and the names of the properties it invalidates.
    Changed(HashMap<String, OwnedValue>, Vec<String>),
    /// UPower (re)started.
    Started,
    Service(ServiceEvent),
}

/// Which level is on screen, so each level is announced once.
struct Tracker {
    level: Level,
    /// The notification for `level`, while the notification service has it
    /// up.
    shown: Option<Issued>,
    /// The body of the discharging warning on screen, which follows the UPS.
    body: String,
}

impl Tracker {
    async fn update(&mut self, display: &Display, upower: &UPowerProxy<'_>, session: &Connection) {
        let level = display.level();
        if level == self.level {
            if let (Level::Discharging, Some(shown)) = (level, self.shown.clone()) {
                self.follow_discharge(&shown, display, session).await;
            }
            return;
        }
        // Like gnome-settings-daemon: the previous warning goes away when
        // the level changes, and a new one replaces it.
        if let Some(shown) = self.shown.take() {
            close(session, &shown).await;
        }
        self.level = Level::None;
        let action = upower.get_critical_action().await.unwrap_or_default();
        let Some(warning) = warning(
            level,
            display.kind,
            display.percentage,
            display.time_to_empty,
            &action,
        ) else {
            return;
        };
        match notify(session, &warning, None).await {
            Ok(shown) => {
                self.level = level;
                self.shown = Some(shown);
                self.body = warning.body;
            }
            // Retried on the next battery update, and when a notification
            // service appears: at login it may start after this daemon.
            Err(e) => g_log!(glib::LogLevel::Warning, "Cannot show battery warning: {e}"),
        }
    }

    /// While a UPS discharges, its warning counts the remaining time down in
    /// place: a new text replaces the notification on screen.
    async fn follow_discharge(&mut self, shown: &Issued, display: &Display, session: &Connection) {
        let Some(warning) = warning(
            Level::Discharging,
            display.kind,
            display.percentage,
            display.time_to_empty,
            "",
        ) else {
            return;
        };
        if warning.body == self.body {
            return;
        }
        match notify(session, &warning, Some(shown)).await {
            Ok(shown) => {
                self.shown = Some(shown);
                self.body = warning.body;
            }
            // Retried on the next update.
            Err(e) => g_log!(
                glib::LogLevel::Warning,
                "Cannot update battery warning: {e}"
            ),
        }
    }

    /// `gone` lost the notification service: its id means nothing to the
    /// next instance. A warning it had up is shown again once a service is
    /// there. (A critical warning never expires, so the user would
    /// otherwise lose it.)
    fn service_gone(&mut self, gone: &OwnedUniqueName) {
        if self.shown.as_ref().is_some_and(|shown| shown.is_from(gone)) {
            self.shown = None;
            self.level = Level::None;
        }
    }

    /// The server closed a notification by itself (it expired, or the user
    /// dismissed it): never close or replace that id.
    fn closed(&mut self, closed: &Issued) {
        if self.shown.as_ref() == Some(closed) {
            self.shown = None;
        }
    }
}

/// Watch the battery and notify until a connection fails.
pub async fn watch(system: &Connection, session: &Connection) -> zbus::Result<()> {
    let upower = UPowerProxy::new(system).await?;
    let device = PropertiesProxy::builder(system)
        .destination(UPOWER)?
        .path(DISPLAY_DEVICE)?
        .cache_properties(zbus::CacheProperties::No)
        .build()
        .await?;
    let mut tracker = Tracker {
        level: Level::None,
        shown: None,
        body: String::new(),
    };

    // Subscribe before reading, so that no change slips through. Each
    // signal is applied as it comes, in order: a level reported for a
    // moment counts too. Every battery update also retries a warning that
    // could not be shown, and an unchanged level is a no-op.
    let changes =
        OrderedStreamExt::filter_map(device.receive_properties_changed().await?, |signal| {
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
            Some(Update::Changed(changed, invalidated))
        });
    let started = OrderedStreamExt::filter_map(
        DBusProxy::new(system)
            .await?
            .receive_name_owner_changed_with_args(&[(0, UPOWER)])
            .await?,
        |signal| {
            let started = signal.args().ok()?.new_owner().is_some();
            started.then_some(Update::Started)
        },
    );
    let (mut owner, service) = service_events(session).await?;
    let mut updates = stream::select(
        ordered_stream::join(changes, started).into_stream(),
        service.map(Update::Service),
    );
    let mut display = Display::read(&device).await?;
    loop {
        tracker.update(&display, &upower, session).await;
        loop {
            match updates.next().await {
                None => return Ok(()),
                Some(Update::Changed(changed, invalidated)) => {
                    display.apply(&changed, &invalidated, &device).await;
                    break;
                }
                // A new UPower may report other values than its predecessor.
                Some(Update::Started) => match Display::read(&device).await {
                    Ok(read) => {
                        display = read;
                        break;
                    }
                    Err(e) => g_log!(glib::LogLevel::Debug, "Cannot read the display device: {e}"),
                },
                Some(Update::Service(ServiceEvent::Closed(closed))) => tracker.closed(&closed),
                Some(Update::Service(ServiceEvent::Owner { gone, owner: new })) => {
                    if let Some(gone) = gone {
                        tracker.service_gone(&gone);
                    }
                    if new == owner {
                        continue;
                    }
                    owner = new;
                    // A warning that could not be shown, or was lost with
                    // the old service, goes out now.
                    if owner.is_some() {
                        break;
                    }
                }
            }
        }
    }
}

/// Run the battery warnings for the lifetime of the daemon. Without UPower
/// (it is not installed, or a desktop has no battery service) this retries
/// quietly after the first warning.
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
            g_log!(level, "Battery warnings unavailable: {e}");
            level = glib::LogLevel::Debug;
        }
        glib::timeout_future_seconds(RETRY_SECS).await;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use zbus::export::futures_util::lock::Mutex as Gate;
    use zbus::{dbus_interface, ConnectionBuilder, SignalContext};

    #[test]
    fn levels_map_like_upower() {
        assert_eq!(Level::from_upower(0), Level::None); // unknown
        assert_eq!(Level::from_upower(1), Level::None);
        assert_eq!(Level::from_upower(2), Level::None); // UPS discharging
        assert_eq!(Level::from_upower(3), Level::Low);
        assert_eq!(Level::from_upower(4), Level::Critical);
        assert_eq!(Level::from_upower(5), Level::Action);
    }

    #[test]
    fn warning_texts_follow_gnome() {
        let low = warning(Level::Low, 2, 9.6, 0, "HybridSleep").unwrap();
        assert_eq!(
            (low.summary.as_str(), low.body.as_str(), low.urgency),
            ("Low Battery", "10% battery remaining", 1)
        );
        assert_eq!(low.timeout_ms, 30_000);
        let critical = warning(Level::Critical, 2, 4.0, 0, "HybridSleep").unwrap();
        assert_eq!(
            (critical.summary.as_str(), critical.body.as_str()),
            ("Battery Almost Empty", "Connect power now")
        );
        assert_eq!((critical.urgency, critical.timeout_ms), (2, 0));
        let action = warning(Level::Action, 2, 1.0, 0, "PowerOff").unwrap();
        assert_eq!(action.body, "This device is about to shutdown");
        assert_eq!(
            warning(Level::Low, KIND_UPS, 20.0, 0, "").unwrap().summary,
            "UPS Low"
        );
        assert_eq!(warning(Level::None, 2, 50.0, 0, ""), None);
    }

    #[test]
    fn only_a_ups_warns_when_discharging() {
        assert_eq!(Level::of_display(2, KIND_UPS), Level::Discharging);
        assert_eq!(Level::of_display(2, 2), Level::None);
        assert_eq!(Level::of_display(3, KIND_UPS), Level::Low);
        assert_eq!(Level::of_display(1, KIND_UPS), Level::None);
        let discharging = warning(Level::Discharging, KIND_UPS, 54.6, 1500, "").unwrap();
        assert_eq!(
            (discharging.summary.as_str(), discharging.body.as_str()),
            (
                "UPS Discharging",
                "25 minutes of UPS backup power remaining (55%)"
            )
        );
        assert_eq!(
            (
                discharging.urgency,
                discharging.transient,
                discharging.timeout_ms
            ),
            (1, true, 30_000)
        );
        assert_eq!(
            warning(Level::Discharging, KIND_UPS, 20.0, 0, "")
                .unwrap()
                .body,
            "Unknown amount of UPS backup power remaining (20%)"
        );
        // A UPS that cannot tell may report a negative time, too.
        assert_eq!(
            warning(Level::Discharging, KIND_UPS, 55.0, -1, "")
                .unwrap()
                .body,
            "Unknown amount of UPS backup power remaining (55%)"
        );
    }

    #[test]
    fn time_texts_follow_gnome() {
        for (seconds, text) in [
            (0, "Unknown time"),
            (29, "Unknown time"),
            (30, "1 minute"),
            (90, "2 minutes"),
            (1500, "25 minutes"),
            (3569, "59 minutes"),
            (3570, "1 hour"),
            (3630, "1 hour 1 minute"),
            (7200, "2 hours"),
            (7530, "2 hours 6 minutes"),
        ] {
            assert_eq!(time_text(seconds), text, "{seconds} s");
        }
    }

    /// The display device of a fake UPower.
    struct FakeDevice {
        kind: u32,
        warning_level: u32,
        percentage: f64,
        time_to_empty: i64,
    }

    impl FakeDevice {
        /// A laptop battery.
        fn battery(warning_level: u32, percentage: f64) -> FakeDevice {
            FakeDevice {
                kind: 2,
                warning_level,
                percentage,
                time_to_empty: 0,
            }
        }
    }

    #[dbus_interface(name = "org.freedesktop.UPower.Device")]
    impl FakeDevice {
        #[dbus_interface(property)]
        fn warning_level(&self) -> u32 {
            self.warning_level
        }
        #[dbus_interface(property)]
        fn percentage(&self) -> f64 {
            self.percentage
        }
        #[dbus_interface(property, name = "Type")]
        fn kind(&self) -> u32 {
            self.kind
        }
        #[dbus_interface(property)]
        fn time_to_empty(&self) -> i64 {
            self.time_to_empty
        }
    }

    struct FakeUPower;

    #[dbus_interface(name = "org.freedesktop.UPower")]
    impl FakeUPower {
        fn get_critical_action(&self) -> String {
            "HybridSleep".to_string()
        }
    }

    /// What one instance of a fake notification service was asked to do.
    #[derive(Debug, Clone)]
    pub(crate) enum Call {
        Notify {
            id: u32,
            replaces: u32,
            text: String,
        },
        Close(u32),
    }

    /// The calls one instance of a fake notification service took.
    pub(crate) type Calls = Arc<Mutex<Vec<Call>>>;

    /// Check that an instance of the notification service was only asked to
    /// replace or close notifications it issued itself.
    #[track_caller]
    pub(crate) fn assert_own_ids(instance: usize, calls: &Calls) {
        let mut issued = Vec::new();
        for call in calls.lock().unwrap().iter() {
            match call {
                Call::Notify { id, replaces, .. } => {
                    assert!(
                        *replaces == 0 || issued.contains(replaces),
                        "instance {instance} asked to replace an id of another: {call:?}"
                    );
                    issued.push(*id);
                }
                Call::Close(id) => assert!(
                    issued.contains(id),
                    "instance {instance} asked to close an id of another: {call:?}"
                ),
            }
        }
    }

    /// The notifications an instance of the notification service has up,
    /// once that settles (or after 5 s) at `expected`.
    pub(crate) fn wait_up(calls: &Calls, expected: &[&str]) -> Vec<String> {
        let up = || {
            let mut up: Vec<(u32, String)> = Vec::new();
            for call in calls.lock().unwrap().iter() {
                match call {
                    Call::Notify { id, replaces, text } => {
                        up.retain(|(shown, _)| shown != id && shown != replaces);
                        up.push((*id, text.clone()));
                    }
                    Call::Close(id) => up.retain(|(shown, _)| shown != id),
                }
            }
            up.into_iter().map(|(_, text)| text).collect::<Vec<_>>()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while up() != expected && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        // Give unexpected extra calls a chance to show up.
        std::thread::sleep(Duration::from_millis(300));
        up()
    }

    /// A notification server that records what it is asked to do, and
    /// holds back its answers to `Notify` while `gate` is locked.
    struct FakeNotifications {
        log: Arc<Mutex<Vec<String>>>,
        calls: Calls,
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
            hints: HashMap<&str, Value<'_>>,
            _expire_timeout: i32,
        ) -> u32 {
            let urgency = match hints.get("urgency") {
                Some(Value::U8(u)) => *u,
                _ => 255,
            };
            let id = if replaces_id == 0 {
                self.next_id += 1;
                self.next_id
            } else {
                replaces_id
            };
            let replaces = if replaces_id == 0 {
                String::new()
            } else {
                format!(" (replaces {replaces_id})")
            };
            let text = format!("{summary}: {body} (urgency {urgency})");
            self.log
                .lock()
                .unwrap()
                .push(format!("notify {id}{replaces} {text}"));
            self.calls.lock().unwrap().push(Call::Notify {
                id,
                replaces: replaces_id,
                text,
            });
            let _open = self.gate.lock().await;
            id
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

    pub(crate) struct Bus(Child);

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A private bus that stands in for both the system and session bus.
    /// It has no service directories: a notification daemon installed on
    /// the machine must not be started in place of a missing fake one.
    pub(crate) fn private_bus() -> (Bus, String) {
        static BUSES: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir();
        let config = dir.join(format!(
            "regolith-powerd-test-bus-{}-{}.conf",
            std::process::id(),
            BUSES.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir={}</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.display()
            ),
        )
        .unwrap();
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon is needed to run this test");
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        // The bus has read its configuration once it has an address.
        let _ = std::fs::remove_file(&config);
        (Bus(child), address.trim().to_string())
    }

    /// Change properties of the display device and announce them in one
    /// `PropertiesChanged` signal, as UPower does.
    async fn set_display(service: &Connection, changes: &[(&str, Value<'_>)]) {
        let iface = service
            .object_server()
            .interface::<_, FakeDevice>(DISPLAY_DEVICE)
            .await
            .unwrap();
        let mut device = iface.get_mut().await;
        let mut changed = HashMap::new();
        for (name, value) in changes {
            match (*name, value) {
                ("WarningLevel", Value::U32(level)) => device.warning_level = *level,
                ("Percentage", Value::F64(percentage)) => device.percentage = *percentage,
                ("TimeToEmpty", Value::I64(seconds)) => device.time_to_empty = *seconds,
                _ => panic!("cannot set {name} to {value:?}"),
            }
            changed.insert(*name, value);
        }
        let ctxt: &SignalContext = iface.signal_context();
        zbus::fdo::Properties::properties_changed(
            ctxt,
            InterfaceName::from_static_str_unchecked(DEVICE_INTERFACE),
            &changed,
            &[],
        )
        .await
        .unwrap();
    }

    /// Set the battery level and announce what changed.
    async fn set_battery(service: &Connection, warning_level: u32, percentage: f64) {
        let level_changed = service
            .object_server()
            .interface::<_, FakeDevice>(DISPLAY_DEVICE)
            .await
            .unwrap()
            .get()
            .await
            .warning_level
            != warning_level;
        let mut changes = vec![("Percentage", Value::F64(percentage))];
        if level_changed {
            changes.push(("WarningLevel", Value::U32(warning_level)));
        }
        set_display(service, &changes).await;
    }

    fn wait_for(log: &Arc<Mutex<Vec<String>>>, len: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while log.lock().unwrap().len() < len && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        // Give unexpected extra calls a chance to show up.
        std::thread::sleep(Duration::from_millis(200));
        log.lock().unwrap().clone()
    }

    #[test]
    fn warns_once_per_level_and_clears_on_ac() {
        let (_bus, address) = private_bus();
        let log = Arc::new(Mutex::new(Vec::new()));

        let service = zbus::block_on(async {
            ConnectionBuilder::address(address.as_str())?
                .name("org.freedesktop.UPower")?
                .name("org.freedesktop.Notifications")?
                .serve_at("/org/freedesktop/UPower", FakeUPower)?
                .serve_at(DISPLAY_DEVICE, FakeDevice::battery(1, 50.0))?
                .serve_at(
                    "/org/freedesktop/Notifications",
                    FakeNotifications {
                        log: log.clone(),
                        calls: Calls::default(),
                        gate: Arc::default(),
                        next_id: 0,
                    },
                )?
                .build()
                .await
        })
        .unwrap();

        let watcher_address = address.clone();
        std::thread::spawn(move || {
            zbus::block_on(async {
                let conn = ConnectionBuilder::address(watcher_address.as_str())?
                    .build()
                    .await?;
                watch(&conn, &conn).await
            })
        });
        std::thread::sleep(Duration::from_millis(500));
        assert!(log.lock().unwrap().is_empty(), "warned at 50%");

        // Discharging through the thresholds; percentage updates within a
        // level must not repeat the warning.
        let steps: &[(u32, f64, &[&str])] = &[
            (
                3,
                10.0,
                &["notify 1 Low Battery: 10% battery remaining (urgency 1)"],
            ),
            (3, 9.0, &[]),
            (3, 8.0, &[]),
            (
                4,
                4.0,
                &[
                    "close 1",
                    "notify 2 Battery Almost Empty: Connect power now (urgency 2)",
                ],
            ),
            (4, 3.0, &[]),
            (
                5,
                1.0,
                &[
                    "close 2",
                    "notify 3 Battery is Empty: This device is about to hibernate (urgency 2)",
                ],
            ),
            // AC plugged in: UPower drops the level to none.
            (1, 1.0, &["close 3"]),
            (1, 5.0, &[]),
        ];
        let mut expected: Vec<String> = Vec::new();
        for (level, percentage, calls) in steps {
            zbus::block_on(set_battery(&service, *level, *percentage));
            expected.extend(calls.iter().map(|c| c.to_string()));
            assert_eq!(
                wait_for(&log, expected.len()),
                expected,
                "after WarningLevel {level} at {percentage}%"
            );
        }
    }

    #[test]
    fn warns_at_startup_when_already_low() {
        let (_bus, address) = private_bus();
        let log = Arc::new(Mutex::new(Vec::new()));
        let _service = zbus::block_on(async {
            ConnectionBuilder::address(address.as_str())?
                .name("org.freedesktop.UPower")?
                .name("org.freedesktop.Notifications")?
                .serve_at("/org/freedesktop/UPower", FakeUPower)?
                .serve_at(DISPLAY_DEVICE, FakeDevice::battery(4, 4.0))?
                .serve_at(
                    "/org/freedesktop/Notifications",
                    FakeNotifications {
                        log: log.clone(),
                        calls: Calls::default(),
                        gate: Arc::default(),
                        next_id: 0,
                    },
                )?
                .build()
                .await
        })
        .unwrap();
        std::thread::spawn(move || {
            zbus::block_on(async {
                let conn = ConnectionBuilder::address(address.as_str())?
                    .build()
                    .await?;
                watch(&conn, &conn).await
            })
        });
        assert_eq!(
            wait_for(&log, 1),
            ["notify 1 Battery Almost Empty: Connect power now (urgency 2)"]
        );
    }

    /// At login the notification server may start after regolith-powerd.
    /// A warning that could not be shown goes out as soon as the server is
    /// there.
    #[test]
    fn warns_when_the_notification_server_starts_late() {
        let (_bus, address) = private_bus();
        let log = Arc::new(Mutex::new(Vec::new()));
        let upower = zbus::block_on(async {
            ConnectionBuilder::address(address.as_str())?
                .name("org.freedesktop.UPower")?
                .serve_at("/org/freedesktop/UPower", FakeUPower)?
                .serve_at(DISPLAY_DEVICE, FakeDevice::battery(3, 10.0))?
                .build()
                .await
        })
        .unwrap();
        let watcher_address = address.clone();
        std::thread::spawn(move || {
            zbus::block_on(async {
                let conn = ConnectionBuilder::address(watcher_address.as_str())?
                    .build()
                    .await?;
                watch(&conn, &conn).await
            })
        });
        std::thread::sleep(Duration::from_millis(500));

        let _notifications = zbus::block_on(async {
            ConnectionBuilder::address(address.as_str())?
                .name("org.freedesktop.Notifications")?
                .serve_at(
                    "/org/freedesktop/Notifications",
                    FakeNotifications {
                        log: log.clone(),
                        calls: Calls::default(),
                        gate: Arc::default(),
                        next_id: 0,
                    },
                )?
                .build()
                .await
        })
        .unwrap();
        let shown = ["notify 1 Low Battery: 10% battery remaining (urgency 1)"];
        assert_eq!(wait_for(&log, 1), shown);
        // Once.
        zbus::block_on(set_battery(&upower, 3, 9.0));
        assert_eq!(wait_for(&log, 1), shown);
    }

    /// While a UPS runs on its battery the warning counts the remaining time
    /// down in place, and it gives way to the next level.
    #[test]
    fn ups_discharging_counts_down_in_place() {
        let (_bus, address) = private_bus();
        let log = Arc::new(Mutex::new(Vec::new()));
        let service = zbus::block_on(async {
            ConnectionBuilder::address(address.as_str())?
                .name("org.freedesktop.UPower")?
                .name("org.freedesktop.Notifications")?
                .serve_at("/org/freedesktop/UPower", FakeUPower)?
                .serve_at(
                    DISPLAY_DEVICE,
                    FakeDevice {
                        kind: KIND_UPS,
                        warning_level: 1,
                        percentage: 100.0,
                        time_to_empty: 0,
                    },
                )?
                .serve_at(
                    "/org/freedesktop/Notifications",
                    FakeNotifications {
                        log: log.clone(),
                        calls: Calls::default(),
                        gate: Arc::default(),
                        next_id: 0,
                    },
                )?
                .build()
                .await
        })
        .unwrap();
        let watcher_address = address.clone();
        std::thread::spawn(move || {
            zbus::block_on(async {
                let conn = ConnectionBuilder::address(watcher_address.as_str())?
                    .build()
                    .await?;
                watch(&conn, &conn).await
            })
        });
        std::thread::sleep(Duration::from_millis(500));

        // The properties that change, and the calls that follow.
        type Step<'a> = (&'a [(&'a str, Value<'a>)], &'a [&'a str]);
        let steps: &[Step] = &[
            // Mains power fails.
            (
                &[
                    ("WarningLevel", Value::U32(2)),
                    ("Percentage", Value::F64(95.0)),
                    ("TimeToEmpty", Value::I64(1500)),
                ],
                &["notify 1 UPS Discharging: 25 minutes of UPS backup power remaining (95%) (urgency 1)"],
            ),
            // The same text: nothing to update.
            (&[("TimeToEmpty", Value::I64(1490))], &[]),
            (
                &[
                    ("Percentage", Value::F64(90.2)),
                    ("TimeToEmpty", Value::I64(1200)),
                ],
                &["notify 1 (replaces 1) UPS Discharging: 20 minutes of UPS backup power remaining (90%) (urgency 1)"],
            ),
            (
                &[
                    ("WarningLevel", Value::U32(3)),
                    ("Percentage", Value::F64(20.0)),
                ],
                &["close 1", "notify 2 UPS Low: 20% UPS power remaining (urgency 1)"],
            ),
            (&[("WarningLevel", Value::U32(1))], &["close 2"]),
        ];
        let mut expected: Vec<String> = Vec::new();
        for (index, (changes, calls)) in steps.iter().enumerate() {
            zbus::block_on(set_display(&service, changes));
            expected.extend(calls.iter().map(|c| c.to_string()));
            assert_eq!(wait_for(&log, expected.len()), expected, "step {index}");
        }
    }

    /// A private bus with a fake UPower, a notification server and the
    /// battery warnings following them.
    struct World {
        _bus: Bus,
        address: String,
        upower: Option<Connection>,
        notifications: Option<Connection>,
        /// Instances that gave up the service but stay connected.
        former: Vec<Connection>,
        log: Arc<Mutex<Vec<String>>>,
        /// The calls of each instance of the service, in order.
        calls: Vec<Calls>,
        gate: Arc<Gate<()>>,
    }

    impl World {
        fn new(display: FakeDevice) -> World {
            let (bus, address) = private_bus();
            let mut world = World {
                _bus: bus,
                address: address.clone(),
                upower: None,
                notifications: None,
                former: Vec::new(),
                log: Arc::default(),
                calls: Vec::new(),
                gate: Arc::default(),
            };
            world.start_upower(display);
            world.start_notifications();
            std::thread::spawn(move || {
                zbus::block_on(async {
                    let conn = ConnectionBuilder::address(address.as_str())?
                        .build()
                        .await?;
                    watch(&conn, &conn).await
                })
            });
            std::thread::sleep(Duration::from_millis(500));
            world
        }

        /// Start a new instance of UPower, in place of the one there may be.
        fn start_upower(&mut self, display: FakeDevice) {
            if let Some(old) = self.upower.take() {
                zbus::block_on(old.release_name("org.freedesktop.UPower")).unwrap();
            }
            let address = self.address.clone();
            self.upower = Some(
                zbus::block_on(async {
                    ConnectionBuilder::address(address.as_str())?
                        .name("org.freedesktop.UPower")?
                        .serve_at("/org/freedesktop/UPower", FakeUPower)?
                        .serve_at(DISPLAY_DEVICE, display)?
                        .build()
                        .await
                })
                .unwrap(),
            );
        }

        /// Start a new instance of the notification service, in place of
        /// the one there may be.
        fn start_notifications(&mut self) {
            if let Some(old) = self.notifications.take() {
                zbus::block_on(old.release_name("org.freedesktop.Notifications")).unwrap();
            }
            let address = self.address.clone();
            let calls = Calls::default();
            self.calls.push(calls.clone());
            let server = FakeNotifications {
                log: self.log.clone(),
                calls,
                gate: self.gate.clone(),
                next_id: 0,
            };
            self.notifications = Some(
                zbus::block_on(async {
                    ConnectionBuilder::address(address.as_str())?
                        .name("org.freedesktop.Notifications")?
                        .serve_at("/org/freedesktop/Notifications", server)?
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
                let ctxt = SignalContext::new(conn, "/org/freedesktop/Notifications").unwrap();
                FakeNotifications::notification_closed(&ctxt, id, 1)
                    .await
                    .unwrap();
            });
        }

        /// Announce each of `signals` as soon as the one before, without
        /// waiting for the daemon in between.
        fn burst(&self, signals: &[&[(&str, Value<'_>)]]) {
            let upower = self.upower.as_ref().unwrap();
            zbus::block_on(async {
                for changes in signals {
                    set_display(upower, changes).await;
                }
            });
        }

        /// Check that the log grew by exactly `calls`.
        #[track_caller]
        fn expect(&self, expected: &mut Vec<String>, calls: &[&str]) {
            expected.extend(calls.iter().map(|call| call.to_string()));
            assert_eq!(&wait_for(&self.log, expected.len()), expected);
        }
    }

    /// Every level UPower reports is acted on, however briefly it lasts.
    #[test]
    fn acts_on_every_level_in_order() {
        let ups = World::new(FakeDevice {
            kind: KIND_UPS,
            warning_level: 1,
            percentage: 80.0,
            time_to_empty: 0,
        });
        ups.burst(&[
            &[
                ("WarningLevel", Value::U32(2)),
                ("TimeToEmpty", Value::I64(600)),
            ],
            &[("WarningLevel", Value::U32(1))],
            &[("WarningLevel", Value::U32(2))],
        ]);
        let discharging =
            "UPS Discharging: 10 minutes of UPS backup power remaining (80%) (urgency 1)";
        ups.expect(
            &mut Vec::new(),
            &[
                &format!("notify 1 {discharging}"),
                "close 1",
                &format!("notify 2 {discharging}"),
            ],
        );

        let laptop = World::new(FakeDevice::battery(3, 10.0));
        let log = &mut Vec::new();
        laptop.expect(
            log,
            &["notify 1 Low Battery: 10% battery remaining (urgency 1)"],
        );
        laptop.burst(&[
            &[("WarningLevel", Value::U32(1))],
            &[("WarningLevel", Value::U32(3))],
        ]);
        laptop.expect(
            log,
            &[
                "close 1",
                "notify 2 Low Battery: 10% battery remaining (urgency 1)",
            ],
        );
    }

    /// A UPower that restarts may report another level than before.
    #[test]
    fn reads_the_display_device_again_when_upower_restarts() {
        let mut world = World::new(FakeDevice::battery(1, 50.0));
        world.start_upower(FakeDevice::battery(3, 10.0));
        let log = &mut Vec::new();
        world.expect(
            log,
            &["notify 1 Low Battery: 10% battery remaining (urgency 1)"],
        );
        // And its signals are followed.
        world.burst(&[&[("WarningLevel", Value::U32(4))]]);
        world.expect(
            log,
            &[
                "close 1",
                "notify 2 Battery Almost Empty: Connect power now (urgency 2)",
            ],
        );
    }

    /// A warning that is up when the notification service restarts is
    /// shown again on the new one; one the server closed is not.
    #[test]
    fn shows_the_warning_again_on_a_restarted_notification_service() {
        let mut world = World::new(FakeDevice::battery(4, 4.0));
        let critical = "Battery Almost Empty: Connect power now (urgency 2)";
        let log = &mut Vec::new();
        world.expect(log, &[&format!("notify 1 {critical}")]);
        world.start_notifications();
        world.expect(log, &[&format!("notify 1 {critical}")]);
        // Dismissed by the user: it stays down.
        world.expire(1);
        world.start_notifications();
        world.expect(log, &[]);
        world.burst(&[&[("WarningLevel", Value::U32(1))]]);
        world.expect(log, &[]);
    }

    /// The notification service changes hands while the old instance still
    /// owes the daemon an answer, and the UPS is back on mains power: the
    /// old instance's id never reaches the new one.
    #[test]
    fn ids_stay_with_the_instance_that_issued_them() {
        let mut world = World::new(FakeDevice {
            kind: KIND_UPS,
            warning_level: 2,
            percentage: 80.0,
            time_to_empty: 600,
        });
        let discharging = |percentage: u32| {
            format!(
                "UPS Discharging: 10 minutes of UPS backup power remaining ({percentage}%) (urgency 1)"
            )
        };
        world.expect(&mut Vec::new(), &[&format!("notify 1 {}", discharging(80))]);
        let gate = world.gate.clone();
        let held = zbus::block_on(gate.lock());
        world.burst(&[&[("Percentage", Value::F64(70.0))]]);
        std::thread::sleep(Duration::from_millis(200));
        world.burst(&[&[("WarningLevel", Value::U32(1))]]);
        world.hand_over();
        drop(held);
        assert_eq!(wait_up(&world.calls[1], &[]), Vec::<String>::new());
        // The next warning is the new instance's, and closed there.
        world.burst(&[&[("WarningLevel", Value::U32(2))]]);
        assert_eq!(
            wait_up(&world.calls[1], &[&discharging(70)]),
            [discharging(70)]
        );
        world.burst(&[&[("WarningLevel", Value::U32(1))]]);
        assert_eq!(wait_up(&world.calls[1], &[]), Vec::<String>::new());
        for (instance, calls) in world.calls.iter().enumerate() {
            assert_own_ids(instance, calls);
        }
    }
}
