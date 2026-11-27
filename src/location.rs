//! The GeoClue location agent for the Wayland session.
//!
//! GeoClue, the system location service, serves a user only while a location
//! agent of that user is registered with it: the agent says how precise
//! locations may be (`MaxAccuracyLevel`) and decides which sandboxed
//! applications get one (`AuthorizeApp`). In GNOME, GNOME Shell is that
//! agent; in the X11 session gnome-flashback starts GeoClue's demo agent. In
//! the sway session nothing registered one, so every location request (the
//! automatic time zone, Night Light's sunset-to-sunrise schedule, Weather,
//! Maps, browsers) waited until its caller gave up.
//!
//! This module is that agent, following GNOME Shell:
//!
//! - It exports `org.freedesktop.GeoClue2.Agent` at
//!   `/org/freedesktop/GeoClue2/Agent` on the system bus and registers under
//!   GNOME Shell's agent id whenever `org.freedesktop.GeoClue2` is taken
//!   over by a GeoClue process, at start if GeoClue runs, and again after
//!   every restart or activation of it.
//! - `MaxAccuracyLevel` follows `org.gnome.system.location`: 0 while location
//!   services are off, otherwise the level for `max-accuracy-level`. Changes
//!   are announced at once.
//! - `AuthorizeApp`, which only GeoClue may call, answers from the grants in
//!   the permission store (table and id `location`, GNOME Shell's format, so
//!   Settings shows and edits the same records). Applications without one are
//!   asked through a notification with two actions. Requests for the same
//!   application share its notification; an unanswered one is closed after
//!   [`PROMPT_TIMEOUT`]. Without a permission store the answers are kept in
//!   memory for the daemon's lifetime.
//!
//! It runs in a thread of its own, with its own main context, so nothing it
//! waits for delays the daemon's other services.

use gio::prelude::*;
use glib::g_log;
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zbus::export::futures_util::future::{self, Either};
use zbus::export::futures_util::StreamExt;
use zbus::fdo;
use zbus::names::BusName;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{
    dbus_interface, dbus_proxy, CacheProperties, Connection, MatchRule, MessageHeader,
    MessageStream, MessageType,
};

const GEOCLUE: &str = "org.freedesktop.GeoClue2";
const MANAGER_PATH: &str = "/org/freedesktop/GeoClue2/Manager";
const MANAGER_INTERFACE: &str = "org.freedesktop.GeoClue2.Manager";
const AGENT_PATH: &str = "/org/freedesktop/GeoClue2/Agent";

/// The agent id GNOME Shell registers with. GeoClue only accepts the ids its
/// configuration lists, and this daemon stands in for GNOME Shell.
const AGENT_ID: &str = "gnome-shell";

const SCHEMA: &str = "org.gnome.system.location";
const ENABLED_KEY: &str = "enabled";
const ACCURACY_KEY: &str = "max-accuracy-level";

/// Where GNOME Shell keeps its location grants in the permission store.
const STORE_TABLE: &str = "location";
const STORE_ID: &str = "location";
const STORE_NOT_FOUND: &str = "org.freedesktop.portal.Error.NotFound";

/// How long an application's notification waits for an answer.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a permission store or notification call may take before the
/// service counts as unavailable.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

const NOTIFICATIONS_INTERFACE: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const APP_NAME: &str = "regolith-powerd";
const ICON: &str = "find-location-symbolic";
const ALLOW: &str = "allow";
const DENY: &str = "deny";

/// GeoClue's accuracy levels (`GClueAccuracyLevel`) and the names the
/// permission store records them under.
const LEVELS: &[(u32, &str)] = &[
    (0, "NONE"),
    (1, "COUNTRY"),
    (4, "CITY"),
    (5, "NEIGHBORHOOD"),
    (6, "STREET"),
    (8, "EXACT"),
];

/// The accuracy limit for the location settings: 0 when location services
/// are off or the setting is unknown.
pub fn max_level(enabled: bool, accuracy: &str) -> u32 {
    if !enabled {
        return 0;
    }
    match accuracy {
        "country" => 1,
        "city" => 4,
        "neighborhood" => 5,
        "street" => 6,
        "exact" => 8,
        _ => 0,
    }
}

/// `level` rounded down to one of GeoClue's levels.
pub fn floor_level(level: u32) -> u32 {
    LEVELS
        .iter()
        .rev()
        .map(|(l, _)| *l)
        .find(|l| *l <= level)
        .unwrap_or(0)
}

/// The permission store name of a level (rounded down first).
pub fn level_name(level: u32) -> &'static str {
    let level = floor_level(level);
    LEVELS
        .iter()
        .find(|(l, _)| *l == level)
        .map(|(_, n)| *n)
        .unwrap_or("NONE")
}

/// The level a permission store name stands for; unknown names grant none.
pub fn level_from_name(name: &str) -> u32 {
    LEVELS
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(l, _)| *l)
        .unwrap_or(0)
}

#[dbus_proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
    fn close_notification(&self, id: u32) -> zbus::Result<()>;
}

#[dbus_proxy(
    interface = "org.freedesktop.impl.portal.PermissionStore",
    default_service = "org.freedesktop.impl.portal.PermissionStore",
    default_path = "/org/freedesktop/impl/portal/PermissionStore"
)]
trait PermissionStore {
    fn lookup(
        &self,
        table: &str,
        id: &str,
    ) -> zbus::Result<(HashMap<String, Vec<String>>, OwnedValue)>;
    fn set_permission(
        &self,
        table: &str,
        create: bool,
        id: &str,
        app: &str,
        permissions: &[&str],
    ) -> zbus::Result<()>;
}

/// An authorization request, handed from the bus to the agent's thread.
struct Request {
    desktop_id: String,
    level: u32,
    reply: async_channel::Sender<u32>,
}

/// The agent object on the system bus.
struct Agent {
    max_level: Arc<AtomicU32>,
    requests: async_channel::Sender<Request>,
}

#[dbus_interface(name = "org.freedesktop.GeoClue2.Agent")]
impl Agent {
    /// Whether, and how precisely, `desktop_id` may get locations. Only
    /// GeoClue may ask.
    async fn authorize_app(
        &self,
        #[zbus(header)] header: MessageHeader<'_>,
        #[zbus(connection)] connection: &Connection,
        desktop_id: String,
        req_accuracy_level: u32,
    ) -> fdo::Result<(bool, u32)> {
        let sender = header.sender().ok().flatten().map(|s| s.to_string());
        let geoclue = name_owner(connection, GEOCLUE).await;
        if sender.is_none() || sender != geoclue {
            return Err(fdo::Error::AccessDenied(
                "Only the location service may ask for authorization".into(),
            ));
        }
        let (reply, answer) = async_channel::bounded(1);
        self.requests
            .send(Request {
                desktop_id,
                level: req_accuracy_level,
                reply,
            })
            .await
            .map_err(|_| fdo::Error::Failed("The location agent is not running".into()))?;
        let level = answer
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("The location agent is not running".into()))?;
        Ok((level > 0, level))
    }

    #[dbus_interface(property)]
    fn max_accuracy_level(&self) -> u32 {
        self.max_level.load(Ordering::SeqCst)
    }
}

/// The unique name that owns `name`, if any.
async fn name_owner(connection: &Connection, name: &str) -> Option<String> {
    let dbus = fdo::DBusProxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .ok()?;
    let name = BusName::try_from(name).ok()?;
    dbus.get_name_owner(name).await.ok().map(|o| o.to_string())
}

/// `fut`'s output, unless it takes longer than `limit`.
async fn within<T>(limit: Duration, fut: impl Future<Output = T>) -> Option<T> {
    let fut = std::pin::pin!(fut);
    match future::select(fut, glib::timeout_future(limit)).await {
        Either::Left((value, _)) => Some(value),
        Either::Right(_) => None,
    }
}

/// The location settings, when their schema is installed.
fn location_settings() -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(SCHEMA, true)?;
    Some(gio::Settings::new(SCHEMA))
}

fn settings_level(settings: Option<&gio::Settings>) -> u32 {
    settings.map_or(0, |s| {
        max_level(s.boolean(ENABLED_KEY), s.string(ACCURACY_KEY).as_str())
    })
}

/// Register with GeoClue every time its name gets an owner, for as long as
/// the system bus connection lasts.
async fn register(system: Connection) {
    let watch = async {
        let dbus = fdo::DBusProxy::builder(&system)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        // Subscribe before asking for the current owner, so an owner that
        // appears in between is not missed.
        let mut changes = dbus
            .receive_name_owner_changed_with_args(&[(0, GEOCLUE)])
            .await?;
        let mut registered = name_owner(&system, GEOCLUE).await;
        match &registered {
            Some(owner) => spawn_add_agent(&system, owner.clone()),
            None => g_log!(
                glib::LogLevel::Info,
                "The location service is not running yet"
            ),
        }
        while let Some(change) = changes.next().await {
            let Ok(args) = change.args() else { continue };
            let owner = args.new_owner().as_ref().map(|o| o.to_string());
            match owner {
                // The same owner the start-up check already found.
                Some(owner) if registered.as_ref() == Some(&owner) => {}
                Some(owner) => {
                    spawn_add_agent(&system, owner.clone());
                    registered = Some(owner);
                }
                None => registered = None,
            }
        }
        Ok::<_, zbus::Error>(())
    };
    if let Err(e) = watch.await {
        g_log!(
            glib::LogLevel::Warning,
            "Cannot follow the location service: {e}"
        );
    }
}

/// Register the agent with the GeoClue instance that is `owner`, without
/// waiting for it: GeoClue reads the agent's properties before it replies.
fn spawn_add_agent(system: &Connection, owner: String) {
    let system = system.clone();
    glib::MainContext::ref_thread_default().spawn_local(async move {
        let result = system
            .call_method(
                Some(owner.as_str()),
                MANAGER_PATH,
                Some(MANAGER_INTERFACE),
                "AddAgent",
                &(AGENT_ID,),
            )
            .await;
        match result {
            Ok(_) => g_log!(
                glib::LogLevel::Info,
                "Registered as the location agent with {owner}"
            ),
            Err(e) => g_log!(
                glib::LogLevel::Warning,
                "The location service refused the agent: {e}"
            ),
        }
    });
}

/// What the user answered to a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Allow,
    Deny,
    /// Closed without an action, not answered in time, or not shown.
    Dismissed,
}

/// What the permission store knows about an application.
enum Grant {
    Stored(u32),
    NotStored,
    StoreUnavailable,
}

/// The application a request is for, as its desktop entry describes it.
struct App {
    name: String,
    reason: Option<String>,
}

fn find_app(desktop_id: &str) -> Option<App> {
    let info = gio::DesktopAppInfo::new(&format!("{desktop_id}.desktop"))?;
    let reason = info
        .filename()
        .and_then(|path| {
            let file = glib::KeyFile::new();
            file.load_from_file(path, glib::KeyFileFlags::NONE).ok()?;
            file.locale_string("Desktop Entry", "X-Geoclue-Reason", None)
                .ok()
        })
        .map(|r| r.to_string())
        .filter(|r| !r.trim().is_empty());
    Some(App {
        name: info.name().to_string(),
        reason,
    })
}

/// The agent's state on its own thread.
struct State {
    max_level: Arc<AtomicU32>,
    session: Option<Connection>,
    /// How long a notification waits for an answer ([`PROMPT_TIMEOUT`]).
    prompt_timeout: Duration,
    /// Applications whose notification is open, with the requests waiting
    /// for its answer besides the one that opened it.
    prompts: RefCell<HashMap<String, Vec<async_channel::Sender<Answer>>>>,
    /// Answers kept while there is no permission store.
    remembered: RefCell<HashMap<String, u32>>,
}

impl State {
    fn max_level(&self) -> u32 {
        self.max_level.load(Ordering::SeqCst)
    }

    async fn store(&self) -> Option<PermissionStoreProxy<'static>> {
        let session = self.session.as_ref()?;
        PermissionStoreProxy::builder(session)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .ok()
    }

    async fn grant(&self, desktop_id: &str) -> Grant {
        let Some(store) = self.store().await else {
            return Grant::StoreUnavailable;
        };
        match within(CALL_TIMEOUT, store.lookup(STORE_TABLE, STORE_ID)).await {
            Some(Ok((apps, _))) => match apps.get(desktop_id) {
                Some(permissions) => {
                    Grant::Stored(permissions.first().map_or(0, |name| level_from_name(name)))
                }
                None => Grant::NotStored,
            },
            Some(Err(zbus::Error::MethodError(name, _, _))) if name.as_str() == STORE_NOT_FOUND => {
                Grant::NotStored
            }
            _ => Grant::StoreUnavailable,
        }
    }

    /// Record an answer: in the permission store, next to the other
    /// applications' records, or in memory when there is none.
    async fn remember(&self, desktop_id: &str, level: u32) {
        let name = level_name(level);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            .to_string();
        if let Some(store) = self.store().await {
            let permissions = [name, now.as_str()];
            let call = store.set_permission(STORE_TABLE, true, STORE_ID, desktop_id, &permissions);
            match within(CALL_TIMEOUT, call).await {
                Some(Ok(())) => return,
                Some(Err(e)) => g_log!(
                    glib::LogLevel::Warning,
                    "Cannot store the location permission of {desktop_id}: {e}"
                ),
                None => g_log!(
                    glib::LogLevel::Warning,
                    "Cannot store the location permission of {desktop_id}: no answer"
                ),
            }
        }
        self.remembered
            .borrow_mut()
            .insert(desktop_id.to_string(), floor_level(level));
    }

    /// The level `desktop_id` gets for a request at `requested` (already
    /// rounded down).
    async fn decide(&self, desktop_id: &str, requested: u32) -> u32 {
        if self.max_level() == 0 {
            return 0;
        }
        let Some(app) = find_app(desktop_id) else {
            return 0;
        };
        let known = match self.grant(desktop_id).await {
            Grant::Stored(level) => Some(level),
            Grant::NotStored => None,
            Grant::StoreUnavailable => self.remembered.borrow().get(desktop_id).copied(),
        };
        if let Some(level) = known {
            return level.min(requested).min(self.max_level());
        }
        match self.ask(desktop_id, &app, requested).await {
            Answer::Allow => requested.min(self.max_level()),
            Answer::Deny | Answer::Dismissed => 0,
        }
    }

    /// Ask the user about `desktop_id`, or wait for the answer to the
    /// notification already open for it.
    async fn ask(&self, desktop_id: &str, app: &App, requested: u32) -> Answer {
        let waiting = {
            let mut prompts = self.prompts.borrow_mut();
            match prompts.get_mut(desktop_id) {
                Some(waiters) => {
                    let (send, receive) = async_channel::bounded(1);
                    waiters.push(send);
                    Some(receive)
                }
                None => {
                    prompts.insert(desktop_id.to_string(), Vec::new());
                    None
                }
            }
        };
        if let Some(answer) = waiting {
            return answer.recv().await.unwrap_or(Answer::Dismissed);
        }
        let answer = self.prompt(app).await;
        match answer {
            Answer::Allow => {
                let level = requested.min(self.max_level());
                if level > 0 {
                    self.remember(desktop_id, level).await;
                }
            }
            Answer::Deny => self.remember(desktop_id, 0).await,
            Answer::Dismissed => {}
        }
        let waiters = self
            .prompts
            .borrow_mut()
            .remove(desktop_id)
            .unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.try_send(answer);
        }
        answer
    }

    /// Show one notification for `app` and wait for its answer, for at most
    /// the prompt timeout.
    async fn prompt(&self, app: &App) -> Answer {
        let deadline = glib::timeout_future(self.prompt_timeout);
        let shown = async {
            let session = self.session.as_ref()?;
            let notifications = NotificationsProxy::builder(session)
                .cache_properties(CacheProperties::No)
                .build()
                .await
                .ok()?;
            // Listen before showing it, so no answer is missed. Both signals
            // come through one stream, in the order they were sent: servers
            // report a notification closed right after its action.
            let rule = MatchRule::builder()
                .msg_type(MessageType::Signal)
                .interface(NOTIFICATIONS_INTERFACE)
                .ok()?
                .path(NOTIFICATIONS_PATH)
                .ok()?
                .build();
            let signals = MessageStream::for_match_rule(rule, session, None)
                .await
                .ok()?;
            let summary = format!("Allow {} to access your location?", app.name);
            let mut body = String::new();
            if let Some(reason) = &app.reason {
                body.push_str(reason);
                body.push_str("\n\n");
            }
            body.push_str("Location access can be changed at any time from the privacy settings.");
            let id = notifications
                .notify(
                    APP_NAME,
                    0,
                    ICON,
                    &summary,
                    &body,
                    // GNOME Shell's button labels.
                    &[ALLOW, "Grant Access", DENY, "Deny Access"],
                    HashMap::new(),
                    0,
                )
                .await;
            match id {
                Ok(id) => Some((notifications, signals, id)),
                Err(e) => {
                    g_log!(
                        glib::LogLevel::Warning,
                        "Cannot ask for location access: {e}"
                    );
                    None
                }
            }
        };
        let shown = std::pin::pin!(shown);
        let first = future::select(shown, deadline).await;
        let (notifications, signals, id, mut deadline) = match first {
            Either::Left((Some((n, signals, id)), deadline)) => (n, signals, id, deadline),
            // No notification: the request is answered when the time is up,
            // as an unanswered one would be.
            Either::Left((None, deadline)) => {
                deadline.await;
                return Answer::Dismissed;
            }
            Either::Right(_) => return Answer::Dismissed,
        };
        enum Event {
            Action(u32, String),
            Closed(u32),
        }
        let mut events = signals
            .filter_map(|message| async move {
                let message = message.ok()?;
                match message.member()?.as_str() {
                    "ActionInvoked" => {
                        let (id, key): (u32, String) = message.body().ok()?;
                        Some(Event::Action(id, key))
                    }
                    "NotificationClosed" => {
                        let (id, _reason): (u32, u32) = message.body().ok()?;
                        Some(Event::Closed(id))
                    }
                    _ => None,
                }
            })
            .boxed_local();
        loop {
            match future::select(events.next(), &mut deadline).await {
                Either::Left((Some(Event::Action(signal_id, key)), _)) if signal_id == id => {
                    match key.as_str() {
                        ALLOW => return Answer::Allow,
                        DENY => return Answer::Deny,
                        _ => {}
                    }
                }
                Either::Left((Some(Event::Closed(signal_id)), _)) if signal_id == id => {
                    return Answer::Dismissed
                }
                Either::Left((Some(_), _)) => {}
                Either::Left((None, _)) => {
                    deadline.await;
                    return Answer::Dismissed;
                }
                Either::Right(_) => {
                    if within(CALL_TIMEOUT, notifications.close_notification(id))
                        .await
                        .is_none()
                    {
                        g_log!(
                            glib::LogLevel::Warning,
                            "Cannot close the location access notification"
                        );
                    }
                    return Answer::Dismissed;
                }
            }
        }
    }
}

async fn run() -> zbus::Result<()> {
    let system = Connection::system().await?;
    let session = match Connection::session().await {
        Ok(session) => Some(session),
        Err(e) => {
            g_log!(
                glib::LogLevel::Warning,
                "Location access cannot be asked for or stored: {e}"
            );
            None
        }
    };
    let settings = location_settings();
    if settings.is_none() {
        g_log!(
            glib::LogLevel::Warning,
            "The {SCHEMA} settings are not installed; location access stays off"
        );
    }
    let max_level = Arc::new(AtomicU32::new(settings_level(settings.as_ref())));

    // Follow the settings and announce every change of the limit.
    if let Some(settings) = &settings {
        let system = system.clone();
        let max_level = max_level.clone();
        settings.connect_changed(None, move |settings, key| {
            if key != ENABLED_KEY && key != ACCURACY_KEY {
                return;
            }
            let level = settings_level(Some(settings));
            if max_level.swap(level, Ordering::SeqCst) == level {
                return;
            }
            let system = system.clone();
            glib::MainContext::ref_thread_default().spawn_local(async move {
                announce_max_level(&system).await;
            });
        });
    }

    let served = serve(system, session, max_level, PROMPT_TIMEOUT).await;
    // Keeps the settings, and with them their change notifications, alive.
    drop(settings);
    served
}

/// Announce the agent's current accuracy limit.
async fn announce_max_level(system: &Connection) {
    let server = system.object_server();
    let Ok(iface) = server.interface::<_, Agent>(AGENT_PATH).await else {
        return;
    };
    let agent = iface.get().await;
    if let Err(e) = agent
        .max_accuracy_level_changed(iface.signal_context())
        .await
    {
        g_log!(
            glib::LogLevel::Warning,
            "Cannot announce the location accuracy: {e}"
        );
    }
}

/// Export the agent on `system`, register it with every GeoClue that
/// appears there, and answer its requests (asking the user and storing the
/// answers through `session`) for as long as the agent object exists. It
/// runs on the thread-default main context.
async fn serve(
    system: Connection,
    session: Option<Connection>,
    max_level: Arc<AtomicU32>,
    prompt_timeout: Duration,
) -> zbus::Result<()> {
    let (requests, incoming) = async_channel::unbounded();
    system
        .object_server()
        .at(
            AGENT_PATH,
            Agent {
                max_level: max_level.clone(),
                requests,
            },
        )
        .await?;

    glib::MainContext::ref_thread_default().spawn_local(register(system.clone()));

    let state = Rc::new(State {
        max_level,
        session,
        prompt_timeout,
        prompts: RefCell::default(),
        remembered: RefCell::default(),
    });
    while let Ok(request) = incoming.recv().await {
        let state = state.clone();
        glib::MainContext::ref_thread_default().spawn_local(async move {
            let level = state
                .decide(&request.desktop_id, floor_level(request.level))
                .await;
            let _ = request.reply.send(level).await;
        });
    }
    Ok(())
}

/// Be the session's location agent for the lifetime of the daemon, from a
/// thread of its own.
pub fn start() {
    let spawned = thread::Builder::new().name("location".into()).spawn(|| {
        let context = glib::MainContext::new();
        let main_loop = glib::MainLoop::new(Some(&context), false);
        let result = context.with_thread_default(|| {
            let quit = main_loop.clone();
            glib::MainContext::ref_thread_default().spawn_local(async move {
                if let Err(e) = run().await {
                    g_log!(
                        glib::LogLevel::Warning,
                        "The location agent is unavailable: {e}"
                    );
                }
                quit.quit();
            });
            main_loop.run();
        });
        if let Err(e) = result {
            g_log!(
                glib::LogLevel::Warning,
                "The location agent is unavailable: {e}"
            );
        }
    });
    if let Err(e) = spawned {
        g_log!(
            glib::LogLevel::Warning,
            "The location agent is unavailable: {e}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_follows_the_settings() {
        assert_eq!(max_level(false, "exact"), 0);
        assert_eq!(max_level(true, "country"), 1);
        assert_eq!(max_level(true, "city"), 4);
        assert_eq!(max_level(true, "neighborhood"), 5);
        assert_eq!(max_level(true, "street"), 6);
        assert_eq!(max_level(true, "exact"), 8);
        assert_eq!(max_level(true, "everywhere"), 0);
    }

    #[test]
    fn levels_round_down() {
        let rounded: Vec<u32> = (0..=10).map(floor_level).collect();
        assert_eq!(rounded, [0, 1, 1, 1, 4, 5, 6, 6, 8, 8, 8]);
    }

    #[test]
    fn store_names_round_trip() {
        for (level, name) in LEVELS {
            assert_eq!(level_name(*level), *name);
            assert_eq!(level_from_name(name), *level);
        }
        assert_eq!(level_name(7), "STREET");
        assert_eq!(level_from_name("exact"), 0);
        assert_eq!(level_from_name("SOMEWHERE"), 0);
    }

    use crate::end_session::tests::{private_bus, wait_until};
    use std::sync::Mutex;
    use zbus::ConnectionBuilder;

    /// A stand-in for GeoClue's manager: records who registered which agent.
    struct FakeManager {
        agents: Arc<Mutex<Vec<(String, String)>>>,
    }

    #[dbus_interface(name = "org.freedesktop.GeoClue2.Manager")]
    impl FakeManager {
        fn add_agent(&self, #[zbus(header)] header: MessageHeader<'_>, id: String) {
            let sender = header.sender().ok().flatten().map(|s| s.to_string());
            self.agents
                .lock()
                .unwrap()
                .push((sender.unwrap_or_default(), id));
        }
    }

    fn connect(address: &str) -> Connection {
        zbus::block_on(ConnectionBuilder::address(address).unwrap().build()).unwrap()
    }

    /// A GeoClue stand-in owning its name on the bus at `address`.
    fn geoclue(address: &str, agents: &Arc<Mutex<Vec<(String, String)>>>) -> Connection {
        let manager = FakeManager {
            agents: agents.clone(),
        };
        zbus::block_on(
            ConnectionBuilder::address(address)
                .unwrap()
                .name(GEOCLUE)
                .unwrap()
                .serve_at(MANAGER_PATH, manager)
                .unwrap()
                .build(),
        )
        .unwrap()
    }

    /// Run the agent on `system`, as `start` does, in a thread of its own.
    fn agent(system: &Connection, max_level: &Arc<AtomicU32>) {
        let system = system.clone();
        let max_level = max_level.clone();
        thread::spawn(move || {
            let context = glib::MainContext::new();
            context
                .with_thread_default(|| {
                    let served = serve(system, None, max_level, Duration::from_millis(200));
                    context.block_on(served).unwrap();
                })
                .unwrap();
        });
    }

    fn authorize(
        caller: &Connection,
        agent: &str,
        desktop_id: &str,
        level: u32,
    ) -> zbus::Result<(bool, u32)> {
        let reply = zbus::block_on(caller.call_method(
            Some(agent),
            AGENT_PATH,
            Some("org.freedesktop.GeoClue2.Agent"),
            "AuthorizeApp",
            &(desktop_id, level),
        ))?;
        reply.body()
    }

    #[test]
    fn registers_with_each_geoclue_and_answers_only_it() {
        let (_bus, address) = private_bus();
        let agents = Arc::new(Mutex::new(Vec::new()));
        let registered = |expected: &[(&String, &str)]| {
            let expected: Vec<(String, String)> = expected
                .iter()
                .map(|(sender, id)| (sender.to_string(), id.to_string()))
                .collect();
            wait_until(&format!("{expected:?}"), || {
                *agents.lock().unwrap() == expected
            });
        };

        // The agent starts before GeoClue: it registers once GeoClue comes,
        // and again with the next GeoClue after a restart.
        let system = connect(&address);
        let name = system.unique_name().unwrap().to_string();
        let max_level = Arc::new(AtomicU32::new(8));
        agent(&system, &max_level);
        let other = connect(&address);
        let properties = zbus::blocking::fdo::PropertiesProxy::builder(&other.clone().into())
            .destination(name.as_str())
            .unwrap()
            .path(AGENT_PATH)
            .unwrap()
            .build()
            .unwrap();
        let get_limit = || {
            let interface = "org.freedesktop.GeoClue2.Agent".try_into().unwrap();
            properties.get(interface, "MaxAccuracyLevel")
        };
        let limit = || u32::try_from(get_limit().unwrap()).unwrap();
        wait_until("the agent object", || get_limit().is_ok());
        let first = geoclue(&address, &agents);
        registered(&[(&name, AGENT_ID)]);
        drop(first);
        let geoclue = geoclue(&address, &agents);
        registered(&[(&name, AGENT_ID), (&name, AGENT_ID)]);

        // An agent started while GeoClue runs registers at once.
        let late = connect(&address);
        let late_name = late.unique_name().unwrap().to_string();
        agent(&late, &Arc::new(AtomicU32::new(0)));
        registered(&[(&name, AGENT_ID), (&name, AGENT_ID), (&late_name, AGENT_ID)]);

        // Only GeoClue may ask.
        match authorize(&other, &name, "org.example.App", 8) {
            Err(zbus::Error::MethodError(error, _, _)) => {
                assert_eq!(error.as_str(), "org.freedesktop.DBus.Error.AccessDenied")
            }
            reply => panic!("a client was answered: {reply:?}"),
        }
        // No desktop entry: no access, and no prompt.
        assert_eq!(
            authorize(&geoclue, &name, "org.example.NoSuchApp", 8).unwrap(),
            (false, 0)
        );
        // Location services off: no access.
        assert_eq!(
            authorize(&geoclue, &late_name, "org.example.NoSuchApp", 8).unwrap(),
            (false, 0)
        );

        // The limit is announced when it changes.
        assert_eq!(limit(), 8);
        let mut changes = properties.receive_properties_changed().unwrap();
        max_level.store(4, Ordering::SeqCst);
        zbus::block_on(announce_max_level(&system));
        let change = changes.next().unwrap();
        let args = change.args().unwrap();
        assert_eq!(
            args.interface_name().as_str(),
            "org.freedesktop.GeoClue2.Agent"
        );
        assert_eq!(
            args.changed_properties().get("MaxAccuracyLevel"),
            Some(&Value::U32(4))
        );
        assert_eq!(limit(), 4);
    }
}
