//! `org.gnome.Mutter.IdleMonitor` for the sway session.
//!
//! GNOME components ask this interface how long the user has been idle and
//! to be told when the user goes idle or comes back: gnome-session's presence
//! (through gnome-desktop's `GnomeIdleMonitor`), chat clients and similar
//! status tools. Mutter serves it in GNOME Shell and gnome-flashback in the
//! X11 session; in the sway session nothing did, so idle time read as zero
//! and presence never turned idle.
//!
//! The service follows mutter's contract:
//!
//! - An object manager at `/org/gnome/Mutter/IdleMonitor` exports the monitor
//!   at `/org/gnome/Mutter/IdleMonitor/Core`; gnome-desktop finds it through
//!   the object manager.
//! - Idle time is measured from the last keyboard or pointer input on the
//!   seat, as sway reports it through `ext-idle-notify-v1`.
//! - An idle watch fires once each time the idle time reaches its interval,
//!   and again after the next input once the interval passes again. A
//!   user-active watch fires on the next input and is gone.
//! - `WatchFired` goes to the watch's owner only, only the owner can remove
//!   a watch, and a client's watches end when it leaves the bus.
//! - Idle inhibition pauses idle watches without stopping the idle time.
//!   GNOME's comes from gnome-session's `InhibitedActions`; sway's own (idle
//!   inhibitors of visible windows, `inhibit_idle` rules) from a second idle
//!   notification that respects inhibitors. The two count as one inhibition:
//!   when the last of them ends, the idle period starts over at that moment,
//!   as in mutter, where Wayland inhibitors reach the idle monitor through
//!   gnome-session.
//! - `ResetIdletime` acts like input arriving.

use glib::g_log;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use wayland_client::protocol::{wl_registry::WlRegistry, wl_seat::WlSeat};
use wayland_client::{
    delegate_noop,
    globals::{registry_queue_init, GlobalListContents},
    Connection as WaylandConnection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};
use zbus::export::futures_util::{future, stream, StreamExt};
use zbus::fdo::{self, RequestNameFlags};
use zbus::names::{BusName, InterfaceName, UniqueName};
use zbus::zvariant::OwnedValue;
use zbus::{dbus_interface, Connection, MatchRule, MessageHeader, MessageStream, MessageType};

const NAME: &str = "org.gnome.Mutter.IdleMonitor";
const MANAGER_PATH: &str = "/org/gnome/Mutter/IdleMonitor";
const CORE_PATH: &str = "/org/gnome/Mutter/IdleMonitor/Core";
const INTERFACE: &str = "org.gnome.Mutter.IdleMonitor";

const SESSION_MANAGER: &str = "org.gnome.SessionManager";
const SESSION_MANAGER_PATH: &str = "/org/gnome/SessionManager";
/// `GSM_INHIBITOR_FLAG_IDLE`.
const INHIBIT_IDLE: u32 = 1 << 3;

/// Timeout of the notification that follows input only. With no timeout the
/// compositor sends `resumed` and `idled` for every single input, so inputs
/// in quick succession are not merged: each one fires user-active watches.
const INPUT_TIMEOUT: Duration = Duration::ZERO;
/// Timeout of the notification that respects sway's idle inhibitors. It is
/// idle while sway does not inhibit and this long has passed since input.
const SWAY_TIMEOUT: Duration = Duration::from_millis(100);
/// How much later than input-plus-timeout the inhibitor-respecting
/// notification must go idle before that counts as the end of an inhibition
/// rather than late event delivery.
const SWAY_SLACK: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Fires when the idle time reaches the interval, once per idle period.
    Idle { interval: Duration, fired: bool },
    /// Fires on the next input, then ends.
    UserActive,
}

#[derive(Debug, Clone)]
struct Watch {
    owner: String,
    kind: Kind,
}

/// A watch to signal: its id and the bus name that owns it.
type Fire = (u32, String);

/// The idle bookkeeping, driven by explicit instants so it can be tested
/// without a compositor or a clock.
#[derive(Debug)]
struct Monitor {
    /// Start of the current idle period: the last input, or the end of the
    /// last inhibition if that came later.
    period_start: Instant,
    /// The most recent input.
    last_input: Instant,
    /// The input-only notification is idle.
    input_idle: bool,
    /// The inhibitor-respecting notification is idle: sway does not inhibit.
    sway_idle: bool,
    /// gnome-session inhibits idleness.
    gnome_inhibited: bool,
    /// When gnome-session stopped inhibiting while the inhibitor-respecting
    /// notification was not idle. Whether sway still inhibits, or that
    /// notification only follows recent input, shows when it goes idle.
    gnome_end: Option<Instant>,
    /// Last watch id handed out; ids start at 1 and are never reused.
    last_id: u32,
    watches: BTreeMap<u32, Watch>,
    /// Watches to signal, in the order they fired.
    pending: Vec<Fire>,
}

impl Monitor {
    fn new(now: Instant) -> Self {
        Self {
            period_start: now,
            last_input: now,
            input_idle: false,
            sway_idle: false,
            gnome_inhibited: false,
            gnome_end: None,
            last_id: 0,
            watches: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    fn idle_time(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.period_start)
    }

    fn paused(&self) -> bool {
        self.gnome_inhibited || !self.sway_idle
    }

    fn next_id(&mut self) -> u32 {
        loop {
            self.last_id = self.last_id.wrapping_add(1);
            if self.last_id != 0 && !self.watches.contains_key(&self.last_id) {
                return self.last_id;
            }
        }
    }

    fn add(&mut self, owner: &str, kind: Kind) -> u32 {
        let id = self.next_id();
        self.watches.insert(
            id,
            Watch {
                owner: owner.to_string(),
                kind,
            },
        );
        id
    }

    fn add_idle(&mut self, owner: &str, interval: Duration) -> u32 {
        self.add(
            owner,
            Kind::Idle {
                interval,
                fired: false,
            },
        )
    }

    fn add_user_active(&mut self, owner: &str) -> u32 {
        self.add(owner, Kind::UserActive)
    }

    fn remove(&mut self, id: u32) {
        self.watches.remove(&id);
        self.pending.retain(|(pending, _)| *pending != id);
    }

    /// Remove a watch on behalf of `caller`. Another client's watch is left
    /// alone, as if the id were unknown.
    fn remove_owned(&mut self, id: u32, caller: &str) {
        if self
            .watches
            .get(&id)
            .is_some_and(|watch| watch.owner == caller)
        {
            self.remove(id);
        }
    }

    fn remove_owner(&mut self, owner: &str) {
        self.watches.retain(|_, watch| watch.owner != owner);
        self.pending.retain(|(_, pending)| pending != owner);
    }

    fn rearm(&mut self) {
        for watch in self.watches.values_mut() {
            if let Kind::Idle { fired, .. } = &mut watch.kind {
                *fired = false;
            }
        }
    }

    /// Start a new idle period at `at`.
    fn restart_period(&mut self, at: Instant) {
        if at > self.period_start {
            self.period_start = at;
        }
        self.rearm();
    }

    /// Input on the seat: user-active watches fire and end, and idle watches
    /// count from now.
    fn input(&mut self, now: Instant) {
        self.input_idle = false;
        self.last_input = now;
        self.activity(now);
    }

    /// `ResetIdletime`: the same as input, without any from the seat.
    fn reset(&mut self, now: Instant) {
        self.activity(now);
    }

    fn activity(&mut self, now: Instant) {
        self.period_start = now;
        self.rearm();
        let active: Vec<u32> = self
            .watches
            .iter()
            .filter(|(_, watch)| watch.kind == Kind::UserActive)
            .map(|(id, _)| *id)
            .collect();
        for id in active {
            if let Some(watch) = self.watches.remove(&id) {
                self.pending.push((id, watch.owner));
            }
        }
    }

    /// The input-only notification went idle: the last input was its timeout
    /// ago.
    fn input_idled(&mut self, now: Instant) {
        self.input_idle = true;
        let last = now.checked_sub(INPUT_TIMEOUT).unwrap_or(now);
        if last > self.last_input {
            self.last_input = last;
        }
        if self.last_input > self.period_start {
            self.period_start = self.last_input;
        }
    }

    /// The inhibitor-respecting notification resumed: input, or sway started
    /// inhibiting. Either way idle watches wait.
    fn sway_resumed(&mut self) {
        self.sway_idle = false;
    }

    /// The inhibitor-respecting notification went idle. If that is well
    /// after the last input plus its timeout, sway's inhibition just ended.
    /// Unless gnome-session still inhibits, the idle period starts over where
    /// the later of the two inhibitions ended.
    fn sway_idled(&mut self, now: Instant) {
        self.sway_idle = true;
        let quiet_since = now.checked_sub(SWAY_TIMEOUT).unwrap_or(now);
        let sway_end = (quiet_since > self.last_input + SWAY_SLACK).then_some(quiet_since);
        let last_input = self.last_input;
        let gnome_end = self.gnome_end.take().filter(|end| *end > last_input);
        if self.gnome_inhibited {
            return;
        }
        if let Some(end) = sway_end.into_iter().chain(gnome_end).max() {
            self.restart_period(end);
        }
    }

    fn set_gnome_inhibited(&mut self, inhibited: bool, now: Instant) {
        if self.gnome_inhibited == inhibited {
            return;
        }
        self.gnome_inhibited = inhibited;
        if inhibited {
            self.gnome_end = None;
        } else if self.sway_idle {
            self.restart_period(now);
        } else {
            // Sway may still inhibit; the period starts over only once it
            // does not.
            self.gnome_end = Some(now);
        }
    }

    /// Fire the idle watches that are due. Returns when the next one is due,
    /// if any can be while nothing else changes.
    fn fire_due(&mut self, now: Instant) -> Option<Instant> {
        if self.paused() {
            return None;
        }
        let start = self.period_start;
        let mut next: Option<Instant> = None;
        for (id, watch) in self.watches.iter_mut() {
            if let Kind::Idle { interval, fired } = &mut watch.kind {
                if *fired {
                    continue;
                }
                let due = start + *interval;
                if due <= now {
                    *fired = true;
                    self.pending.push((*id, watch.owner.clone()));
                } else {
                    next = Some(next.map_or(due, |n| n.min(due)));
                }
            }
        }
        next
    }

    fn take_pending(&mut self) -> Vec<Fire> {
        std::mem::take(&mut self.pending)
    }
}

/// The monitor shared between the Wayland thread, the D-Bus handlers and
/// the thread that fires watches.
struct Shared {
    monitor: Mutex<Monitor>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Monitor> {
        self.monitor.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Change the monitor and wake the thread that fires watches.
    fn update<R>(&self, f: impl FnOnce(&mut Monitor) -> R) -> R {
        let result = f(&mut self.lock());
        self.changed.notify_all();
        result
    }
}

struct IdleMonitor {
    shared: Arc<Shared>,
}

/// The unique name that sent a method call.
fn caller(header: &MessageHeader<'_>) -> fdo::Result<String> {
    header
        .sender()
        .ok()
        .flatten()
        .map(|sender| sender.to_string())
        .ok_or_else(|| fdo::Error::Failed("The caller has no bus name".into()))
}

impl IdleMonitor {
    /// Keep a new watch only if its owner is still on the bus. An owner that
    /// left before the watch existed would never be cleaned up otherwise.
    async fn confirm_owner(&self, connection: &Connection, owner: &str, id: u32) {
        let Ok(name) = UniqueName::try_from(owner) else {
            return;
        };
        let present = match fdo::DBusProxy::new(connection).await {
            Ok(dbus) => !matches!(
                dbus.get_name_owner(BusName::from(name)).await,
                Err(fdo::Error::NameHasNoOwner(_))
            ),
            Err(_) => true,
        };
        if !present {
            self.shared.update(|monitor| monitor.remove(id));
        }
    }
}

#[dbus_interface(name = "org.gnome.Mutter.IdleMonitor")]
impl IdleMonitor {
    /// Milliseconds since the last input, or since the last inhibition ended.
    fn get_idletime(&self) -> u64 {
        let idle = self.shared.lock().idle_time(Instant::now());
        u64::try_from(idle.as_millis()).unwrap_or(u64::MAX)
    }

    async fn add_idle_watch(
        &self,
        #[zbus(header)] header: MessageHeader<'_>,
        #[zbus(connection)] connection: &Connection,
        interval: u64,
    ) -> fdo::Result<u32> {
        if interval == 0 {
            return Err(fdo::Error::InvalidArgs(
                "The interval of an idle watch must be greater than zero".into(),
            ));
        }
        let owner = caller(&header)?;
        let interval = Duration::from_millis(interval);
        let id = self
            .shared
            .update(|monitor| monitor.add_idle(&owner, interval));
        self.confirm_owner(connection, &owner, id).await;
        Ok(id)
    }

    async fn add_user_active_watch(
        &self,
        #[zbus(header)] header: MessageHeader<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<u32> {
        let owner = caller(&header)?;
        let id = self
            .shared
            .update(|monitor| monitor.add_user_active(&owner));
        self.confirm_owner(connection, &owner, id).await;
        Ok(id)
    }

    /// Unknown and already ended ids, and other clients' watches, are
    /// ignored.
    fn remove_watch(&self, #[zbus(header)] header: MessageHeader<'_>, id: u32) -> fdo::Result<()> {
        let owner = caller(&header)?;
        self.shared
            .update(|monitor| monitor.remove_owned(id, &owner));
        Ok(())
    }

    /// Idle time starts over as if input had arrived.
    fn reset_idletime(&self) {
        self.shared.update(|monitor| monitor.reset(Instant::now()));
    }

    #[dbus_interface(signal)]
    async fn watch_fired(ctxt: &zbus::SignalContext<'_>, id: u32) -> zbus::Result<()>;
}

/// Which of the two idle notifications an event belongs to.
#[derive(Debug, Clone, Copy)]
enum Notification {
    /// Ignores inhibitors: idle exactly when there was no input.
    Input,
    /// Respects sway's idle inhibitors.
    Sway,
}

struct WaylandState {
    shared: Arc<Shared>,
}

impl Dispatch<WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &WaylandConnection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(WaylandState: ignore WlSeat);
delegate_noop!(WaylandState: ExtIdleNotifierV1);

impl Dispatch<ExtIdleNotificationV1, Notification> for WaylandState {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        notification: &Notification,
        _: &WaylandConnection,
        _: &QueueHandle<Self>,
    ) {
        let now = Instant::now();
        state.shared.update(|monitor| match (notification, event) {
            (Notification::Input, ext_idle_notification_v1::Event::Resumed) => monitor.input(now),
            (Notification::Input, ext_idle_notification_v1::Event::Idled) => {
                monitor.input_idled(now)
            }
            (Notification::Sway, ext_idle_notification_v1::Event::Resumed) => {
                monitor.sway_resumed()
            }
            (Notification::Sway, ext_idle_notification_v1::Event::Idled) => monitor.sway_idled(now),
            _ => {}
        });
    }
}

/// Connect to the compositor and set up both notifications on its seat.
/// The returned closure runs the event loop until the connection ends.
fn connect_wayland(shared: Arc<Shared>) -> Result<impl FnOnce(), String> {
    let connection = WaylandConnection::connect_to_env()
        .map_err(|e| format!("cannot connect to the Wayland compositor: {e}"))?;
    let (globals, mut queue) = registry_queue_init::<WaylandState>(&connection)
        .map_err(|e| format!("cannot list the compositor's globals: {e}"))?;
    let qh = queue.handle();
    let seat: WlSeat = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("the compositor has no seat: {e}"))?;
    let notifier: ExtIdleNotifierV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| format!("the compositor has no idle notifications: {e}"))?;
    let timeout = |d: Duration| u32::try_from(d.as_millis()).unwrap_or(u32::MAX);
    let input = if notifier.version() >= 2 {
        notifier.get_input_idle_notification(
            timeout(INPUT_TIMEOUT),
            &seat,
            &qh,
            Notification::Input,
        )
    } else {
        // Version 1 (sway before 1.11) has no input-only notifications. While
        // sway inhibits idle, the notification reports no input, so idle time
        // keeps growing and user-active watches fire only at the first input
        // after the inhibition; idle watches still pause and start over.
        g_log!(
            glib::LogLevel::Message,
            "The compositor's idle notifications follow its inhibitors; input during idle inhibition is not seen"
        );
        notifier.get_idle_notification(timeout(INPUT_TIMEOUT), &seat, &qh, Notification::Input)
    };
    let sway =
        notifier.get_idle_notification(timeout(SWAY_TIMEOUT), &seat, &qh, Notification::Sway);
    let mut state = WaylandState { shared };
    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("the Wayland compositor stopped answering: {e}"))?;
    Ok(move || {
        let _objects = (seat, notifier, input, sway);
        loop {
            if let Err(e) = queue.blocking_dispatch(&mut state) {
                g_log!(
                    glib::LogLevel::Warning,
                    "Lost the Wayland compositor, idle monitoring stops: {e}"
                );
                return;
            }
        }
    })
}

/// Fire watches as they come due and signal them to their owners.
fn run_watches(shared: Arc<Shared>, connection: Connection) {
    let mut monitor = shared.lock();
    loop {
        let now = Instant::now();
        let next = monitor.fire_due(now);
        let fired = monitor.take_pending();
        if !fired.is_empty() {
            drop(monitor);
            for (id, owner) in fired {
                let sent = zbus::block_on(connection.emit_signal(
                    Some(owner.as_str()),
                    CORE_PATH,
                    INTERFACE,
                    "WatchFired",
                    &(id,),
                ));
                if let Err(e) = sent {
                    g_log!(
                        glib::LogLevel::Warning,
                        "Cannot signal idle watch {id} to {owner}: {e}"
                    );
                }
            }
            monitor = shared.lock();
            continue;
        }
        monitor = match next {
            Some(due) => {
                let wait = due.saturating_duration_since(now);
                shared
                    .changed
                    .wait_timeout(monitor, wait)
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            }
            None => shared
                .changed
                .wait(monitor)
                .unwrap_or_else(|e| e.into_inner()),
        };
    }
}

fn signal_rule(
    sender: Option<&'static str>,
    interface: &'static str,
    member: &'static str,
) -> zbus::Result<zbus::MatchRuleBuilder<'static>> {
    let mut rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(interface)?
        .member(member)?;
    if let Some(sender) = sender {
        rule = rule.sender(sender)?;
    }
    Ok(rule)
}

fn name_owner_rule() -> zbus::Result<zbus::MatchRuleBuilder<'static>> {
    signal_rule(
        Some("org.freedesktop.DBus"),
        "org.freedesktop.DBus",
        "NameOwnerChanged",
    )?
    .path("/org/freedesktop/DBus")
}

/// End the watches of clients that leave the bus.
async fn follow_departures(shared: Arc<Shared>, mut changes: MessageStream) {
    while let Some(message) = changes.next().await {
        let Ok(message) = message else { continue };
        let Ok((name, _old, new)) = message.body::<(String, String, String)>() else {
            continue;
        };
        if new.is_empty() && name.starts_with(':') {
            shared.update(|monitor| monitor.remove_owner(&name));
        }
    }
}

/// Whether gnome-session (at its unique name `owner`) inhibits idleness.
async fn read_inhibited(connection: &Connection, owner: &str) -> bool {
    let read = async {
        let properties = fdo::PropertiesProxy::builder(connection)
            .destination(owner.to_string())?
            .path(SESSION_MANAGER_PATH)?
            .cache_properties(zbus::CacheProperties::No)
            .build()
            .await?;
        let value: OwnedValue = properties
            .get(
                InterfaceName::from_static_str_unchecked(SESSION_MANAGER),
                "InhibitedActions",
            )
            .await?;
        Ok::<_, zbus::Error>(u32::try_from(value).unwrap_or(0))
    };
    match read.await {
        Ok(actions) => actions & INHIBIT_IDLE != 0,
        Err(e) => {
            g_log!(
                glib::LogLevel::Warning,
                "Cannot read the session's inhibited actions: {e}"
            );
            false
        }
    }
}

async fn session_manager_owner(connection: &Connection) -> Option<String> {
    let dbus = fdo::DBusProxy::new(connection).await.ok()?;
    let name = BusName::try_from(SESSION_MANAGER).ok()?;
    dbus.get_name_owner(name).await.ok().map(|o| o.to_string())
}

/// The subscriptions the idle monitor follows the bus with.
struct Subscriptions {
    departures: MessageStream,
    session_owner: MessageStream,
    session_properties: MessageStream,
}

impl Subscriptions {
    async fn new(connection: &Connection) -> zbus::Result<Self> {
        let departures = name_owner_rule()?.build();
        let session_owner = name_owner_rule()?.arg(0, SESSION_MANAGER)?.build();
        let session_properties =
            signal_rule(None, "org.freedesktop.DBus.Properties", "PropertiesChanged")?
                .path(SESSION_MANAGER_PATH)?
                .arg(0, SESSION_MANAGER)?
                .build();
        Ok(Self {
            departures: MessageStream::for_match_rule(departures, connection, None).await?,
            session_owner: MessageStream::for_match_rule(session_owner, connection, None).await?,
            session_properties: MessageStream::for_match_rule(session_properties, connection, None)
                .await?,
        })
    }
}

/// Follow gnome-session's idle inhibition: its `InhibitedActions` changes,
/// its arrival (possibly already inhibiting) and its departure.
async fn follow_session_inhibition(
    shared: Arc<Shared>,
    connection: Connection,
    mut owner: Option<String>,
    owners: MessageStream,
    properties: MessageStream,
) {
    let mut messages = Box::pin(stream::select(owners, properties));
    while let Some(message) = messages.next().await {
        let Ok(message) = message else { continue };
        let Ok(header) = message.header() else {
            continue;
        };
        let member = header.member().ok().flatten().map(|m| m.to_string());
        match member.as_deref() {
            Some("NameOwnerChanged") => {
                let Ok((_, _, new)) = message.body::<(String, String, String)>() else {
                    continue;
                };
                if new.is_empty() {
                    owner = None;
                    shared.update(|monitor| monitor.set_gnome_inhibited(false, Instant::now()));
                } else {
                    let inhibited = read_inhibited(&connection, &new).await;
                    owner = Some(new);
                    shared.update(|monitor| monitor.set_gnome_inhibited(inhibited, Instant::now()));
                }
            }
            Some("PropertiesChanged") => {
                let sender = header.sender().ok().flatten().map(|s| s.to_string());
                let Some(current) = owner.clone() else {
                    continue;
                };
                if sender.as_deref() != Some(current.as_str()) {
                    continue;
                }
                let Ok((_, changed, invalidated)) =
                    message.body::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                else {
                    continue;
                };
                let inhibited = if let Some(value) = changed.get("InhibitedActions") {
                    u32::try_from(value.clone()).is_ok_and(|a| a & INHIBIT_IDLE != 0)
                } else if invalidated.iter().any(|p| p == "InhibitedActions") {
                    read_inhibited(&connection, &current).await
                } else {
                    continue;
                };
                shared.update(|monitor| monitor.set_gnome_inhibited(inhibited, Instant::now()));
            }
            _ => {}
        }
    }
}

fn serve(shared: Arc<Shared>, run_wayland: impl FnOnce()) -> zbus::Result<()> {
    // Departures and inhibition are subscribed to before the name is taken,
    // so nothing a client does after seeing the name is missed.
    let (connection, subscriptions, session_owner) = zbus::block_on(async {
        let connection = Connection::session().await?;
        let subscriptions = Subscriptions::new(&connection).await?;
        // A session manager already inhibiting pauses idle watches from the
        // start.
        let session_owner = session_manager_owner(&connection).await;
        if let Some(owner) = &session_owner {
            let inhibited = read_inhibited(&connection, owner).await;
            shared.update(|monitor| monitor.set_gnome_inhibited(inhibited, Instant::now()));
        }
        {
            let server = connection.object_server();
            server.at(MANAGER_PATH, fdo::ObjectManager).await?;
            server
                .at(
                    CORE_PATH,
                    IdleMonitor {
                        shared: shared.clone(),
                    },
                )
                .await?;
        }
        Ok::<_, zbus::Error>((connection, subscriptions, session_owner))
    })?;

    {
        let (shared, connection) = (shared.clone(), connection.clone());
        thread::spawn(move || {
            zbus::block_on(future::join(
                follow_departures(shared.clone(), subscriptions.departures),
                follow_session_inhibition(
                    shared,
                    connection,
                    session_owner,
                    subscriptions.session_owner,
                    subscriptions.session_properties,
                ),
            ));
            g_log!(
                glib::LogLevel::Warning,
                "The idle monitor stopped following the session bus"
            );
        });
    }

    {
        let (shared, connection) = (shared.clone(), connection.clone());
        thread::spawn(move || run_watches(shared, connection));
    }

    // mutter or gnome-flashback, where one runs, is the real idle monitor:
    // leave the name to one that already has it, and let one that turns up
    // take it over.
    zbus::block_on(connection.request_name_with_flags(
        NAME,
        RequestNameFlags::AllowReplacement | RequestNameFlags::DoNotQueue,
    ))?;

    run_wayland();
    // Without the compositor there is no idle time to report.
    let _ = zbus::block_on(connection.release_name(NAME));
    Ok(())
}

pub fn start() {
    thread::spawn(|| {
        let shared = Arc::new(Shared {
            monitor: Mutex::new(Monitor::new(Instant::now())),
            changed: Condvar::new(),
        });
        let run_wayland = match connect_wayland(shared.clone()) {
            Ok(run) => run,
            Err(e) => {
                g_log!(glib::LogLevel::Warning, "{NAME} unavailable: {e}");
                return;
            }
        };
        if let Err(e) = serve(shared, run_wayland) {
            g_log!(glib::LogLevel::Warning, "{NAME} unavailable: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = ":1.10";
    const B: &str = ":1.11";

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A monitor that has seen both notifications go idle after start-up.
    fn settled(t0: Instant) -> Monitor {
        let mut monitor = Monitor::new(t0);
        monitor.input_idled(t0 + INPUT_TIMEOUT);
        monitor.sway_idled(t0 + SWAY_TIMEOUT);
        monitor
    }

    #[test]
    fn idle_watch_counts_from_the_last_input() {
        let t0 = Instant::now();
        let mut m = settled(t0);
        let id = m.add_idle(A, ms(1000));
        assert_eq!(m.fire_due(t0 + ms(500)), Some(t0 + ms(1000)));
        assert!(m.take_pending().is_empty());
        assert_eq!(m.fire_due(t0 + ms(1000)), None);
        assert_eq!(m.take_pending(), vec![(id, A.to_string())]);
        // Once per idle period.
        assert_eq!(m.fire_due(t0 + ms(5000)), None);
        assert!(m.take_pending().is_empty());
    }

    #[test]
    fn watch_added_late_fires_at_once() {
        let t0 = Instant::now();
        let mut m = settled(t0);
        let id = m.add_idle(A, ms(1000));
        m.fire_due(t0 + ms(2500));
        assert_eq!(m.take_pending(), vec![(id, A.to_string())]);
    }

    #[test]
    fn input_rearms_and_postpones() {
        let t0 = Instant::now();
        let mut m = settled(t0);
        let id = m.add_idle(A, ms(1000));
        m.input(t0 + ms(600));
        m.input_idled(t0 + ms(600) + INPUT_TIMEOUT);
        m.sway_resumed();
        m.sway_idled(t0 + ms(700));
        assert_eq!(m.fire_due(t0 + ms(1000)), Some(t0 + ms(1600)));
        m.fire_due(t0 + ms(1600));
        assert_eq!(m.take_pending(), vec![(id, A.to_string())]);
        m.input(t0 + ms(3000));
        m.input_idled(t0 + ms(3000) + INPUT_TIMEOUT);
        m.sway_resumed();
        m.sway_idled(t0 + ms(3100));
        m.fire_due(t0 + ms(4000));
        assert_eq!(m.take_pending(), vec![(id, A.to_string())]);
    }

    #[test]
    fn user_active_watch_fires_once_on_input() {
        let t0 = Instant::now();
        let mut m = settled(t0);
        let id = m.add_user_active(B);
        m.fire_due(t0 + ms(1000));
        assert!(m.take_pending().is_empty());
        m.input(t0 + ms(1500));
        assert_eq!(m.take_pending(), vec![(id, B.to_string())]);
        m.input(t0 + ms(2000));
        assert!(m.take_pending().is_empty());
        assert!(m.watches.is_empty());
    }

    #[test]
    fn ids_are_unique_and_positive() {
        let t0 = Instant::now();
        let mut m = settled(t0);
        let mut seen = std::collections::HashSet::new();
        for i in 0..50 {
            let id = m.add_idle(if i % 2 == 0 { A } else { B }, ms(100));
            assert!(id > 0);
            assert!(seen.insert(id));
            m.remove(id);
        }
    }

    #[test]
    fn removal_and_departure_end_watches() {
        let t0 = Instant::now();
        let mut m = settled(t0);
        let a = m.add_idle(A, ms(500));
        let b = m.add_idle(B, ms(500));
        let c = m.add_user_active(A);
        m.remove(a);
        m.remove(4242);
        m.remove_owner(A);
        m.fire_due(t0 + ms(1000));
        m.input(t0 + ms(1100));
        assert_eq!(m.take_pending(), vec![(b, B.to_string())]);
        assert!(!m.watches.contains_key(&c));
    }

    #[test]
    fn gnome_inhibition_pauses_and_restarts_the_period() {
        let t0 = Instant::now();
        let mut m = settled(t0);
        m.set_gnome_inhibited(true, t0 + ms(200));
        let id = m.add_idle(A, ms(1000));
        let active = m.add_user_active(A);
        assert_eq!(m.fire_due(t0 + ms(3000)), None);
        assert!(m.take_pending().is_empty());
        assert_eq!(m.idle_time(t0 + ms(3000)), ms(3000));
        m.input(t0 + ms(3100));
        assert_eq!(m.take_pending(), vec![(active, A.to_string())]);
        m.input_idled(t0 + ms(3100) + INPUT_TIMEOUT);
        m.sway_resumed();
        m.sway_idled(t0 + ms(3200));
        m.set_gnome_inhibited(false, t0 + ms(4000));
        assert_eq!(m.idle_time(t0 + ms(4000)), Duration::ZERO);
        assert_eq!(m.fire_due(t0 + ms(4500)), Some(t0 + ms(5000)));
        m.fire_due(t0 + ms(5000));
        assert_eq!(m.take_pending(), vec![(id, A.to_string())]);
    }

    #[test]
    fn sway_inhibition_pauses_and_restarts_the_period() {
        let t0 = Instant::now();
        let mut m = settled(t0);
        let id = m.add_idle(A, ms(1000));
        // An inhibitor appears while idle: the notification resumes without
        // input. Idle time keeps counting.
        m.sway_resumed();
        assert_eq!(m.fire_due(t0 + ms(3000)), None);
        assert!(m.take_pending().is_empty());
        assert_eq!(m.idle_time(t0 + ms(3000)), ms(3000));
        // Released at 3 s; the notification idles its timeout later.
        m.sway_idled(t0 + ms(3000) + SWAY_TIMEOUT);
        assert_eq!(m.idle_time(t0 + ms(3000) + SWAY_TIMEOUT), SWAY_TIMEOUT);
        assert_eq!(m.fire_due(t0 + ms(3200)), Some(t0 + ms(4000)));
        m.fire_due(t0 + ms(4000));
        assert_eq!(m.take_pending(), vec![(id, A.to_string())]);
    }

    #[test]
    fn overlapping_inhibitions_restart_only_when_both_end() {
        for sway_first in [true, false] {
            let t0 = Instant::now();
            let mut m = settled(t0);
            let id = m.add_idle(A, ms(1000));
            m.set_gnome_inhibited(true, t0 + ms(200));
            m.sway_resumed();
            // The first source ends: still paused, the clock keeps counting.
            if sway_first {
                m.sway_idled(t0 + ms(2000) + SWAY_TIMEOUT);
            } else {
                m.set_gnome_inhibited(false, t0 + ms(2000));
            }
            assert_eq!(m.fire_due(t0 + ms(3500)), None);
            assert!(m.take_pending().is_empty());
            assert_eq!(m.idle_time(t0 + ms(3500)), ms(3500));
            // The second one ends at 4 s: the period starts over there.
            if sway_first {
                m.set_gnome_inhibited(false, t0 + ms(4000));
            } else {
                m.sway_idled(t0 + ms(4000) + SWAY_TIMEOUT);
            }
            assert_eq!(m.idle_time(t0 + ms(4000) + SWAY_TIMEOUT), SWAY_TIMEOUT);
            assert_eq!(m.fire_due(t0 + ms(4500)), Some(t0 + ms(5000)));
            m.fire_due(t0 + ms(5000));
            assert_eq!(m.take_pending(), vec![(id, A.to_string())]);
        }
    }

    #[test]
    fn gnome_end_just_after_input_restarts_the_period() {
        let t0 = Instant::now();
        let mut m = settled(t0);
        m.set_gnome_inhibited(true, t0 + ms(200));
        m.input(t0 + ms(1000));
        m.sway_resumed();
        m.input_idled(t0 + ms(1000) + INPUT_TIMEOUT);
        // Cleared before the inhibitor-respecting notification idles again.
        m.set_gnome_inhibited(false, t0 + ms(1050));
        m.sway_idled(t0 + ms(1000) + SWAY_TIMEOUT);
        assert_eq!(m.idle_time(t0 + ms(1100)), ms(50));
    }

    #[test]
    fn reset_acts_like_input() {
        let t0 = Instant::now();
        let mut m = settled(t0);
        let id = m.add_idle(A, ms(1000));
        let active_a = m.add_user_active(A);
        let active_b = m.add_user_active(B);
        assert_eq!(m.fire_due(t0 + ms(600)), Some(t0 + ms(1000)));
        m.reset(t0 + ms(600));
        assert_eq!(
            m.take_pending(),
            vec![(active_a, A.to_string()), (active_b, B.to_string())]
        );
        assert_eq!(m.idle_time(t0 + ms(600)), Duration::ZERO);
        assert_eq!(m.fire_due(t0 + ms(1000)), Some(t0 + ms(1600)));
        m.fire_due(t0 + ms(1600));
        assert_eq!(m.take_pending(), vec![(id, A.to_string())]);
        // A fired watch fires again after a reset.
        m.reset(t0 + ms(2000));
        m.fire_due(t0 + ms(3000));
        assert_eq!(m.take_pending(), vec![(id, A.to_string())]);
    }

    #[test]
    fn only_the_owner_removes_a_watch() {
        let t0 = Instant::now();
        let mut m = settled(t0);
        let a = m.add_idle(A, ms(500));
        let b = m.add_idle(B, ms(500));
        m.remove_owned(b, A);
        m.remove_owned(a, A);
        m.fire_due(t0 + ms(1000));
        assert_eq!(m.take_pending(), vec![(b, B.to_string())]);
    }

    #[test]
    fn ordinary_input_is_not_an_inhibition() {
        let t0 = Instant::now();
        let mut m = settled(t0);
        let id = m.add_idle(A, ms(1000));
        m.input(t0 + ms(2000));
        m.sway_resumed();
        m.input_idled(t0 + ms(2000) + INPUT_TIMEOUT);
        // Delivered a little late, but within the slack.
        m.sway_idled(t0 + ms(2000) + SWAY_TIMEOUT + ms(50));
        m.fire_due(t0 + ms(3000));
        assert_eq!(m.take_pending(), vec![(id, A.to_string())]);
    }
}
