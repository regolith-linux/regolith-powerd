//! Watch `org.gnome.SessionManager.InhibitedActions` and mirror it onto
//! logind inhibitor locks, so that the Sway idle system (swayidle honors
//! logind's `BlockInhibited`) reacts to applications inhibiting idleness
//! through GNOME interfaces.
//!
//! The idle lock is held just for an unlocked screen of a session that
//! is the active one (`SessionIsActive`), which is how the X11 session
//! (gnome-settings-daemon's power plugin) treats GNOME idle inhibitors.
//! Once the screen locks it is released, so swayidle's locked-screen
//! timeouts blank the outputs; and an inactive session never holds it, as
//! it would hold off idleness for the active session too. swayidle's own
//! commands skip what an idle inhibitor holds off while locked (the usual
//! dim, blank and lock, and the sleep-inactive action), and everything
//! while inactive (see `Manager::get_swayidle_args`). In the X11 session
//! the idle monitor stops counting idle time while an idle inhibitor is
//! held, so its sleep-inactive action never runs then, locked or not; when
//! the inhibitor goes away on a locked screen, that action counts from
//! then.
//!
//! The gate follows every change at once, sleep cycles included: logind's
//! `PrepareForSleep` only triggers another reconciliation.
//!
//! On a locked screen, inhibitor changes never restart swayidle: that would
//! start the lock screen's blank delays over, and stopping the old swayidle
//! would turn blanked outputs back on. A suspend inhibitor is checked when
//! the sleep-inactive action is due instead. With an idle inhibitor, the
//! lock screen's short blank delay counts from the lock, however long the
//! user had been idle before it.

use glib::g_log;
use std::rc::Rc;
use std::time::{Duration, Instant};
use zbus::export::futures_util::{future, FutureExt, StreamExt};
use zbus::{dbus_proxy, Result};

use crate::logind::{Lock, LogindProxy};
use crate::screensaver::LockWatch;

fn warn_reconcile_failed(e: zbus::Error) {
    g_log!(
        glib::LogLevel::Warning,
        "Failed to reconcile session inhibition: {e}"
    );
}

const INHIBIT_IDLE: u32 = 1 << 3;
const INHIBIT_SUSPEND: u32 = 1 << 2;
const MAX_POLL_SECS: u32 = 30;
const SESSION_MANAGER: &str = "org.gnome.SessionManager";
const LOGIND: &str = "org.freedesktop.login1";

/// Delay between periodic reads that repair missed change signals: half the
/// idle delay, bounded so a stale lock never outlives `MAX_POLL_SECS`.
/// An idle delay of 0 (Screen Blank "Never") arms no idle timeout to stay
/// ahead of, so it gets the longest delay.
fn poll_interval(idle_delay_secs: u32) -> Duration {
    let secs = match idle_delay_secs {
        0 => MAX_POLL_SECS,
        delay => (delay / 2).clamp(1, MAX_POLL_SECS),
    };
    Duration::from_secs(u64::from(secs))
}

#[dbus_proxy(
    interface = "org.gnome.SessionManager",
    default_service = "org.gnome.SessionManager",
    default_path = "/org/gnome/SessionManager"
)]
pub trait SessionManager {
    /// Aggregated inhibitor flags (`GsmManagerInhibitFlag`). This daemon acts
    /// on the `SUSPEND` (`1 << 2`) and `IDLE` (`1 << 3`) bits.
    #[dbus_proxy(property)]
    fn inhibited_actions(&self) -> zbus::Result<u32>;

    /// Whether the session is the one in front (false after a switch to
    /// another user's session).
    #[dbus_proxy(property)]
    fn session_is_active(&self) -> zbus::Result<bool>;
}

/// A change that swayidle has to be told about, beyond logind's idle lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleChange {
    /// The session became the active one again: swayidle's timeouts are
    /// restarted.
    BackInFront,
    /// The idle inhibitor holds idleness back again (say after an unlock):
    /// whatever the timeouts did meanwhile is undone, as gnome-settings-daemon
    /// goes back to its normal idle mode.
    HeldBackAgain,
    /// logind's locks were brought in line for the first time on this
    /// connection (successfully or not): swayidle may start.
    Ready,
    /// A suspend inhibitor came or went on an unlocked screen: swayidle's
    /// timeouts are restarted, as gnome-settings-daemon re-arms its timers.
    SuspendInhibitorChanged,
    /// The suspend inhibitor went away at this instant on a locked screen:
    /// an overdue sleep-inactive action runs straight away, and one not yet
    /// due keeps its deadline; swayidle is not restarted.
    SuspendAllowedWhileLocked(Instant),
    /// The idle inhibitor went away at this instant on a locked screen of a
    /// session in the foreground: the sleep-inactive action, held back
    /// until then, counts from this instant, as idle time does in the X11
    /// session; swayidle is not restarted.
    IdleAllowedWhileLocked(Instant),
    /// The screen locked at this instant has been locked for
    /// [`FRESH_LOCK_BLANK`] while an idle inhibitor is held, in a session in
    /// the foreground: its outputs go off unless the user came back since.
    /// Told once per lock, and not after the session came back to the
    /// foreground during it.
    FreshLockIdle(Instant),
}

/// How long after a lock the outputs go off while idleness is inhibited.
const FRESH_LOCK_BLANK: Duration = Duration::from_secs(crate::JUST_LOCKED_BLANK_SECS as u64);

/// Whether GNOME's idle inhibitor holds idleness back: only for an unlocked
/// session in the foreground, as gnome-settings-daemon's power plugin does.
fn idle_held_back(actions: u32, session_active: bool, locked: bool) -> bool {
    actions & INHIBIT_IDLE != 0 && session_active && !locked
}

struct SessionInhibitors {
    idle: Lock,
    sleep: Lock,
    /// Whether the session was in the foreground at the last reconciliation.
    session_active: bool,
    /// The inhibited actions at the last reconciliation, if any.
    actions: Option<u32>,
    /// The lock seen at the last reconciliation, and whether
    /// [`IdleChange::FreshLockIdle`] was told for it.
    lock: Option<(Instant, bool)>,
}

impl SessionInhibitors {
    fn new() -> Self {
        Self {
            // swayidle follows logind's (strong) idle locks.
            idle: Lock::new("idle", "block"),
            // GNOME's suspend inhibition holds back automatic suspend only;
            // powerd leaves its idle suspend out itself (see `IdleState`).
            // A strong lock would also refuse the user's own suspend
            // requests, so mirror it the way gnome-session does: weak.
            sleep: Lock::new("sleep", "block-weak"),
            session_active: true,
            actions: None,
            lock: None,
        }
    }

    /// When the current lock's [`IdleChange::FreshLockIdle`] may be due, if
    /// that is still to come.
    fn fresh_lock_due(&self) -> Option<Instant> {
        match self.lock {
            Some((since, false)) => {
                Some(since + FRESH_LOCK_BLANK).filter(|due| *due > Instant::now())
            }
            _ => None,
        }
    }

    async fn apply(
        &mut self,
        proxy: &LogindProxy<'_>,
        actions: u32,
        session_active: bool,
        locked: bool,
    ) -> Result<()> {
        self.session_active = session_active;
        // Sync independently so a failure taking one lock does not leave
        // the other lock stale.
        let idle = self
            .idle
            .sync(proxy, idle_held_back(actions, session_active, locked))
            .await;
        let sleep = self.sleep.sync(proxy, actions & INHIBIT_SUSPEND != 0).await;
        idle.and(sleep)
    }
}

/// Mirror GNOME's aggregated inhibition state to logind. Swayidle consumes
/// logind's idle lock through BlockInhibited without needing a restart.
/// `lock` and its wake-ups `lock_changes` follow the lock screen.
/// Never returns: connection failures are logged and retried.
pub async fn run_inhibits(
    settings: Rc<crate::SessionSettings>,
    idle: crate::IdleState,
    lock: LockWatch,
    lock_changes: async_channel::Receiver<()>,
) {
    loop {
        let result: Result<()> = async {
            let session_bus = zbus::Connection::session().await?;
            let system_bus = zbus::Connection::system().await?;
            watch_session_inhibits(
                &session_bus,
                &system_bus,
                || lock.locked_since(),
                &lock_changes,
                || poll_interval(settings.idle_delay()),
                |change| match change {
                    IdleChange::BackInFront | IdleChange::SuspendInhibitorChanged => {
                        idle.request_restart()
                    }
                    IdleChange::HeldBackAgain => idle.request_wake(),
                    IdleChange::Ready => idle.startup_step_done(),
                    IdleChange::SuspendAllowedWhileLocked(at) => idle.request_overdue_sleep(at),
                    IdleChange::IdleAllowedWhileLocked(at) => idle.request_sleep_from(at),
                    IdleChange::FreshLockIdle(since) => {
                        glib::spawn_future_local(crate::blank_fresh_lock(since));
                    }
                },
            )
            .await
        }
        .await;
        if let Err(e) = result {
            g_log!(
                glib::LogLevel::Warning,
                "Session inhibition bridge disconnected: {e}"
            );
        }
        glib::timeout_future(Duration::from_secs(5)).await;
    }
}

async fn session_manager_gone(bus: &zbus::fdo::DBusProxy<'_>) -> bool {
    match zbus::names::BusName::try_from(SESSION_MANAGER) {
        Ok(name) => matches!(bus.name_has_owner(name).await, Ok(false)),
        Err(_) => false,
    }
}

async fn sync_from_session(
    session: &SessionManagerProxy<'_>,
    session_bus: &zbus::fdo::DBusProxy<'_>,
    logind: &LogindProxy<'_>,
    inhibitors: &mut SessionInhibitors,
    locked_since: &impl Fn() -> Option<Instant>,
    on_change: &impl Fn(IdleChange),
) -> Result<()> {
    let actions = match session.inhibited_actions().await {
        Ok(actions) => actions,
        // Only a confirmed owner loss releases the locks; other read
        // failures are transient and keep the last known state.
        Err(_) if session_manager_gone(session_bus).await => 0,
        Err(e) => return Err(e),
    };
    // A session manager without the property (or a failed read) leaves the
    // session where it was: in the foreground, unless known otherwise.
    let session_active = session
        .session_is_active()
        .await
        .unwrap_or(inhibitors.session_active);
    let came_back = session_active && !inhibitors.session_active;
    let was_held_back = inhibitors.idle.is_held();
    let actions_before = inhibitors.actions.replace(actions);
    let suspend_before = actions_before.map(|a| a & INHIBIT_SUSPEND != 0);
    let idle_before = actions_before.is_some_and(|a| a & INHIBIT_IDLE != 0);
    // The lock state is read last, after the D-Bus round trips above.
    let locked = locked_since();
    if inhibitors.lock.map(|(since, _)| since) != locked {
        inhibitors.lock = locked.map(|since| (since, false));
    }
    let result = inhibitors
        .apply(logind, actions, session_active, locked.is_some())
        .await;
    if came_back {
        on_change(IdleChange::BackInFront);
    }
    if !was_held_back && inhibitors.idle.is_held() {
        on_change(IdleChange::HeldBackAgain);
    }
    let suspend_inhibited = actions & INHIBIT_SUSPEND != 0;
    match (suspend_before, locked) {
        (Some(before), None) if before != suspend_inhibited => {
            on_change(IdleChange::SuspendInhibitorChanged)
        }
        (Some(true), Some(_)) if !suspend_inhibited => {
            on_change(IdleChange::SuspendAllowedWhileLocked(Instant::now()))
        }
        _ => {}
    }
    if idle_before && actions & INHIBIT_IDLE == 0 && locked.is_some() && session_active {
        on_change(IdleChange::IdleAllowedWhileLocked(Instant::now()));
    }
    if let Some((since, told)) = &mut inhibitors.lock {
        // Back in front, the countdowns begin afresh (swayidle restarts)
        // rather than from a lock taken before.
        if came_back {
            *told = true;
        }
        if !*told
            && actions & INHIBIT_IDLE != 0
            && session_active
            && since.elapsed() >= FRESH_LOCK_BLANK
        {
            *told = true;
            on_change(IdleChange::FreshLockIdle(*since));
        }
    }
    result
}

/// Keep logind's locks in line with the session's inhibitors, its activity
/// and `locked_since` (when the screen was locked, while it is), reconciling
/// on every change signal, on every wake-up through `lock_changes`, on
/// logind's `PrepareForSleep` (both ways), when a fresh lock's blank delay
/// is up, and periodically.
///
/// `on_change` gets each [`IdleChange`], once logind's locks are in line.
async fn watch_session_inhibits(
    session_bus: &zbus::Connection,
    system_bus: &zbus::Connection,
    locked_since: impl Fn() -> Option<Instant>,
    lock_changes: &async_channel::Receiver<()>,
    next_poll_delay: impl Fn() -> Duration,
    on_change: impl Fn(IdleChange),
) -> Result<()> {
    let logind = LogindProxy::new(system_bus).await?;
    // Caching must stay off: a cached value is kept fresh by change signals,
    // and the whole point of the periodic read is to repair missed signals.
    let session = SessionManagerProxy::builder(session_bus)
        .cache_properties(zbus::CacheProperties::No)
        .build()
        .await?;
    // Not the typed proxy's `receive_inhibited_actions_changed`: that stream
    // is fed by the property cache and ends immediately on a cache-free
    // proxy. A raw PropertiesChanged subscription works without a cache and
    // merely pokes us; `sync_from_session` below always fetches live state.
    let properties = zbus::fdo::PropertiesProxy::builder(session_bus)
        .destination("org.gnome.SessionManager")?
        .path("/org/gnome/SessionManager")?
        .build()
        .await?;
    // Subscribe before the initial read so a concurrent change is not lost.
    let mut changes = properties.receive_properties_changed().await?;
    let session_dbus = zbus::fdo::DBusProxy::new(session_bus).await?;
    // This stream only ends when the system-bus connection closes. Logind
    // acquisitions would then fail forever, so the watcher must return and
    // let the caller reconnect.
    let system_dbus = zbus::fdo::DBusProxy::new(system_bus).await?;
    let mut system_alive = system_dbus
        .receive_name_owner_changed_with_args(&[(0, LOGIND)])
        .await?;
    let mut sleeps = logind.receive_prepare_for_sleep().await?;
    let mut inhibitors = SessionInhibitors::new();
    // Never propagate a failed first reconciliation: it would abort the
    // watcher and drop locks already taken. The loop below keeps retrying
    // on every tick.
    if let Err(e) = sync_from_session(
        &session,
        &session_dbus,
        &logind,
        &mut inhibitors,
        &locked_since,
        &on_change,
    )
    .await
    {
        warn_reconcile_failed(e);
    }
    on_change(IdleChange::Ready);

    // Reconcile on property changes and, independently, on a periodic tick,
    // so a missed change signal cannot leave the locks stale. The tick is
    // polled before signals (so its timer is armed promptly) and reset only
    // when it fires, so draining a signal backlog cannot postpone the next
    // deadline — and every drained signal reconciles live state anyway.
    let mut tick = glib::timeout_future(next_poll_delay());
    loop {
        // A fresh lock's blank delay running out is a change of its own.
        let lock_due = inhibitors
            .fresh_lock_due()
            .map(|due| glib::timeout_future(due.saturating_duration_since(Instant::now())));
        // Every wake-up but the tick carries no state of its own: the state
        // is read afresh below.
        let (woken, ..) = future::select_all([
            (&mut tick).map(|()| Wake::Tick).boxed_local(),
            changes.next().map(Wake::on_signal).boxed_local(),
            // The sender lives as long as `locked_since`: the channel stays
            // open.
            lock_changes.recv().map(|_| Wake::Reconcile).boxed_local(),
            // Logind came or went.
            system_alive.next().map(Wake::on_signal).boxed_local(),
            // Going to sleep or resumed: the gate is applied as it stands,
            // with no pause around the cycle. A session sent to the
            // background meanwhile must not keep its idle lock.
            sleeps.next().map(Wake::on_signal).boxed_local(),
            match lock_due {
                Some(due) => due.map(|()| Wake::Reconcile).boxed_local(),
                None => future::pending().boxed_local(),
            },
        ])
        .await;
        match woken {
            Wake::Closed => return Ok(()),
            // Schedule the next tick before the slow D-Bus work below.
            Wake::Tick => tick = glib::timeout_future(next_poll_delay()),
            Wake::Reconcile => {}
        }
        if let Err(e) = sync_from_session(
            &session,
            &session_dbus,
            &logind,
            &mut inhibitors,
            &locked_since,
            &on_change,
        )
        .await
        {
            // A transient read failure keeps the last known lock state; the
            // next tick retries, including after a GNOME service restart.
            warn_reconcile_failed(e);
        }
    }
}

/// What woke [`watch_session_inhibits`].
enum Wake {
    /// The periodic reconciliation is due.
    Tick,
    /// Something changed; the state is read afresh.
    Reconcile,
    /// A connection was lost.
    Closed,
}

impl Wake {
    /// A signal stream's next item: a signal stream only ends when its
    /// connection is lost, and the caller then reconnects afresh.
    fn on_signal<T>(item: Option<T>) -> Self {
        item.map_or(Self::Closed, |_| Self::Reconcile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::{ErrorKind, Read};
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;
    use std::sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        Arc,
    };
    use zbus::export::futures_util::{future::Either, pin_mut};
    use zbus::zvariant::OwnedFd;
    use zbus::{dbus_interface, ConnectionBuilder};

    /// Held by each test that owns the mock well-known names on the bus.
    static BUS_NAMES: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct MockSession(Arc<AtomicU32>);

    #[dbus_interface(name = "org.gnome.SessionManager")]
    impl MockSession {
        #[dbus_interface(property)]
        fn inhibited_actions(&self) -> u32 {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// A session manager that also tells whether the session is in front.
    struct MockActiveSession {
        actions: Arc<AtomicU32>,
        active: Arc<AtomicBool>,
    }

    #[dbus_interface(name = "org.gnome.SessionManager")]
    impl MockActiveSession {
        #[dbus_interface(property)]
        fn inhibited_actions(&self) -> u32 {
            self.actions.load(Ordering::SeqCst)
        }

        #[dbus_interface(property)]
        fn session_is_active(&self) -> bool {
            self.active.load(Ordering::SeqCst)
        }
    }

    struct MockLogind(async_channel::Sender<(String, UnixStream)>);

    #[dbus_interface(name = "org.freedesktop.login1.Manager")]
    impl MockLogind {
        async fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> OwnedFd {
            assert_eq!(who, crate::logind::WHO);
            assert_eq!(why, "GNOME session inhibition");
            assert_eq!(
                mode,
                if what == "idle" {
                    "block"
                } else {
                    "block-weak"
                }
            );
            let (lock, observer) = UnixStream::pair().unwrap();
            observer.set_nonblocking(true).unwrap();
            self.0.send((what.to_owned(), observer)).await.unwrap();
            unsafe { OwnedFd::from_raw_fd(lock.into_raw_fd()) }
        }
    }

    async fn await_released(mut observer: UnixStream) {
        loop {
            match observer.read(&mut [0]) {
                Ok(0) => return,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    glib::timeout_future(Duration::from_millis(10)).await;
                }
                result => panic!("unexpected lock socket result: {result:?}"),
            }
        }
    }

    /// Emits a `PropertiesChanged` for `InhibitedActions` from the mock
    /// session manager. Keep `actions.store(...)` at the call sites so that
    /// deliberately omitted signals stay recognizable.
    async fn emit_inhibited_actions_changed(server: &zbus::Connection, actions: u32) {
        let changed = std::collections::HashMap::from([(
            "InhibitedActions",
            zbus::zvariant::Value::from(actions),
        )]);
        server
            .emit_signal(
                None::<&str>,
                "/org/gnome/SessionManager",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &("org.gnome.SessionManager", changed, Vec::<&str>::new()),
            )
            .await
            .unwrap();
    }

    async fn exercise_bridge() {
        let actions = Arc::new(AtomicU32::new(INHIBIT_IDLE));
        let (send_calls, calls) = async_channel::unbounded();
        let server = ConnectionBuilder::session()
            .unwrap()
            .name("org.gnome.SessionManager")
            .unwrap()
            .name("org.freedesktop.login1")
            .unwrap()
            .serve_at("/org/gnome/SessionManager", MockSession(actions.clone()))
            .unwrap()
            .serve_at("/org/freedesktop/login1", MockLogind(send_calls))
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::Connection::session().await.unwrap();
        // Counts how often the watcher scheduled a periodic tick.
        let ticks = Arc::new(AtomicUsize::new(0));
        let tick_counter = ticks.clone();
        // How often the idle timers were told to re-arm for a suspend
        // inhibitor change.
        let rearmed = Rc::new(Cell::new(0));
        let rearmed_by_watcher = rearmed.clone();
        // The screen stays unlocked; keep the sender so the channel stays open.
        let (_lock_changed, lock_changes) = async_channel::bounded(1);
        let watch = watch_session_inhibits(
            &client,
            &client,
            || None,
            &lock_changes,
            move || {
                tick_counter.fetch_add(1, Ordering::SeqCst);
                Duration::from_secs(1)
            },
            move |change| {
                if change == IdleChange::SuspendInhibitorChanged {
                    rearmed_by_watcher.set(rearmed_by_watcher.get() + 1);
                }
            },
        );
        let sequence = async {
            // Initial state is read even before any signal has arrived.
            let (what, idle) = calls.recv().await.unwrap();
            assert_eq!(what, "idle");
            // Multiple reads of the same state must not acquire extra locks.
            glib::timeout_future(Duration::from_millis(250)).await;
            assert!(calls.try_recv().is_err());

            // Property signals reconcile both bits independently.
            actions.store(INHIBIT_SUSPEND, Ordering::SeqCst);
            emit_inhibited_actions_changed(&server, INHIBIT_SUSPEND).await;
            // The next poll is still well away: this must be signal-driven.
            let received = calls.recv();
            let deadline = glib::timeout_future(Duration::from_millis(250));
            pin_mut!(received, deadline);
            let (what, sleep) = match future::select(received, deadline).await {
                Either::Left((call, _)) => call.unwrap(),
                Either::Right(_) => {
                    panic!("property signal was not consumed")
                }
            };
            assert_eq!(what, "sleep");
            // The idle timers learn of it too, once logind has answered.
            while rearmed.get() == 0 {
                glib::timeout_future(Duration::from_millis(10)).await;
            }
            assert_eq!(rearmed.get(), 1);
            await_released(idle).await;

            // Intentionally omit PropertiesChanged: an uncached periodic
            // read must release the lock. Unrelated flags must not hold it.
            actions.store(1 | 2 | 16, Ordering::SeqCst);
            await_released(sleep).await;
            assert!(calls.try_recv().is_err());

            // A missed acquisition event is repaired as well.
            actions.store(INHIBIT_IDLE | INHIBIT_SUSPEND, Ordering::SeqCst);
            let (what, idle) = calls.recv().await.unwrap();
            assert_eq!(what, "idle");
            let (what, sleep) = calls.recv().await.unwrap();
            assert_eq!(what, "sleep");
            actions.store(0, Ordering::SeqCst);
            await_released(idle).await;
            await_released(sleep).await;

            // Burst phase: flip back to idle, then keep signals coming at a
            // steady clip for 1.5 s. The 1 s tick must still fire under
            // pressure, and the stable state must not acquire extra locks.
            actions.store(INHIBIT_IDLE, Ordering::SeqCst);
            emit_inhibited_actions_changed(&server, INHIBIT_IDLE).await;
            let (what, _idle) = calls.recv().await.unwrap();
            assert_eq!(what, "idle");
            let ticks_before = ticks.load(Ordering::SeqCst);
            for _ in 0..75 {
                emit_inhibited_actions_changed(&server, INHIBIT_IDLE).await;
                glib::timeout_future(Duration::from_millis(20)).await;
            }
            assert!(
                ticks.load(Ordering::SeqCst) > ticks_before,
                "periodic tick starved by property signals"
            );
            assert!(
                calls.try_recv().is_err(),
                "stable state acquired extra locks under signal pressure"
            );
        };
        pin_mut!(watch, sequence);
        match future::select(watch, sequence).await {
            Either::Left((result, _)) => {
                panic!("watcher ended early: {result:?}");
            }
            Either::Right(_) => {}
        }
    }

    async fn emit_session_is_active_changed(server: &zbus::Connection, active: bool) {
        let changed = std::collections::HashMap::from([(
            "SessionIsActive",
            zbus::zvariant::Value::from(active),
        )]);
        server
            .emit_signal(
                None::<&str>,
                "/org/gnome/SessionManager",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &("org.gnome.SessionManager", changed, Vec::<&str>::new()),
            )
            .await
            .unwrap();
    }

    async fn exercise_lock_and_activity() {
        let actions = Arc::new(AtomicU32::new(INHIBIT_IDLE));
        let active = Arc::new(AtomicBool::new(true));
        let (send_calls, calls) = async_channel::unbounded();
        let server = ConnectionBuilder::session()
            .unwrap()
            .name("org.gnome.SessionManager")
            .unwrap()
            .name("org.freedesktop.login1")
            .unwrap()
            .serve_at(
                "/org/gnome/SessionManager",
                MockActiveSession {
                    actions: actions.clone(),
                    active: active.clone(),
                },
            )
            .unwrap()
            .serve_at("/org/freedesktop/login1", MockLogind(send_calls))
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::Connection::session().await.unwrap();
        let locked = Rc::new(Cell::new(None));
        let locked_for_watcher = locked.clone();
        let (lock_changed, lock_changes) = async_channel::bounded(1);
        // The changes swayidle was told about.
        let told = Rc::new(std::cell::RefCell::new(Vec::new()));
        let told_by_watcher = told.clone();
        let watch = watch_session_inhibits(
            &client,
            &client,
            move || locked_for_watcher.get(),
            &lock_changes,
            // Far away: every change below must be acted on by itself.
            || Duration::from_secs(60),
            move |change| told_by_watcher.borrow_mut().push(change),
        );
        // Changes are passed on once logind has answered, after the mock
        // reported the call.
        let told_so_far = |changes: &[IdleChange]| {
            let told = told.clone();
            let changes = changes.to_vec();
            async move {
                while told.borrow().len() < changes.len() {
                    glib::timeout_future(Duration::from_millis(10)).await;
                }
                assert_eq!(*told.borrow(), changes);
            }
        };
        use IdleChange::*;
        let no_calls_for_a_while = || async {
            glib::timeout_future(Duration::from_millis(250)).await;
            assert!(calls.try_recv().is_err(), "unexpected logind lock");
        };
        let sequence = async {
            // Unlocked in the foreground the idle lock is taken.
            let (what, idle) = calls.recv().await.unwrap();
            assert_eq!(what, "idle");
            told_so_far(&[HeldBackAgain, Ready]).await;

            // Locking lets it go, and nothing else: swayidle's countdowns
            // run on from the lock.
            locked.set(Some(Instant::now()));
            lock_changed.try_send(()).unwrap();
            await_released(idle).await;
            // A stay-awake request on the locked screen takes nothing.
            actions.store(INHIBIT_IDLE | INHIBIT_SUSPEND, Ordering::SeqCst);
            emit_inhibited_actions_changed(&server, INHIBIT_IDLE | INHIBIT_SUSPEND).await;
            let (what, _sleep) = calls.recv().await.unwrap();
            assert_eq!(what, "sleep");
            no_calls_for_a_while().await;
            told_so_far(&[HeldBackAgain, Ready]).await;

            // Unlocking takes it again and undoes what the timeouts did.
            locked.set(None);
            lock_changed.try_send(()).unwrap();
            let (what, idle) = calls.recv().await.unwrap();
            assert_eq!(what, "idle");
            told_so_far(&[HeldBackAgain, Ready, HeldBackAgain]).await;

            // In the background it goes, locked or not.
            active.store(false, Ordering::SeqCst);
            emit_session_is_active_changed(&server, false).await;
            await_released(idle).await;
            locked.set(Some(Instant::now()));
            lock_changed.try_send(()).unwrap();
            no_calls_for_a_while().await;
            locked.set(None);
            lock_changed.try_send(()).unwrap();
            no_calls_for_a_while().await;
            told_so_far(&[HeldBackAgain, Ready, HeldBackAgain]).await;

            // Back in front, the countdowns start over and the lock returns.
            active.store(true, Ordering::SeqCst);
            emit_session_is_active_changed(&server, true).await;
            let (what, _idle) = calls.recv().await.unwrap();
            assert_eq!(what, "idle");
            told_so_far(&[
                HeldBackAgain,
                Ready,
                HeldBackAgain,
                BackInFront,
                HeldBackAgain,
            ])
            .await;

            // Back in front without an idle inhibitor: only the restart.
            active.store(false, Ordering::SeqCst);
            emit_session_is_active_changed(&server, false).await;
            actions.store(0, Ordering::SeqCst);
            emit_inhibited_actions_changed(&server, 0).await;
            no_calls_for_a_while().await;
            active.store(true, Ordering::SeqCst);
            emit_session_is_active_changed(&server, true).await;
            no_calls_for_a_while().await;
            // Dropping the suspend inhibitor unlocked re-arms the timeouts.
            told_so_far(&[
                HeldBackAgain,
                Ready,
                HeldBackAgain,
                BackInFront,
                HeldBackAgain,
                SuspendInhibitorChanged,
                BackInFront,
            ])
            .await;
        };
        pin_mut!(watch, sequence);
        match future::select(watch, sequence).await {
            Either::Left((result, _)) => {
                panic!("watcher ended early: {result:?}");
            }
            Either::Right(_) => {}
        }
    }

    async fn exercise_locked_changes() {
        let actions = Arc::new(AtomicU32::new(INHIBIT_IDLE | INHIBIT_SUSPEND));
        let active = Arc::new(AtomicBool::new(true));
        let (send_calls, calls) = async_channel::unbounded();
        let server = ConnectionBuilder::session()
            .unwrap()
            .name("org.gnome.SessionManager")
            .unwrap()
            .name("org.freedesktop.login1")
            .unwrap()
            .serve_at(
                "/org/gnome/SessionManager",
                MockActiveSession {
                    actions: actions.clone(),
                    active: active.clone(),
                },
            )
            .unwrap()
            .serve_at("/org/freedesktop/login1", MockLogind(send_calls))
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::Connection::session().await.unwrap();
        let locked = Rc::new(Cell::new(None));
        let locked_for_watcher = locked.clone();
        let (lock_changed, lock_changes) = async_channel::bounded(1);
        let told = Rc::new(std::cell::RefCell::new(Vec::new()));
        let told_by_watcher = told.clone();
        let watch = watch_session_inhibits(
            &client,
            &client,
            move || locked_for_watcher.get(),
            &lock_changes,
            || Duration::from_secs(60),
            move |change| told_by_watcher.borrow_mut().push((Instant::now(), change)),
        );
        let changes = |told: &Rc<std::cell::RefCell<Vec<(Instant, IdleChange)>>>| {
            told.borrow().iter().map(|(_, c)| *c).collect::<Vec<_>>()
        };
        let wait_for = |n: usize| {
            let told = told.clone();
            async move {
                while told.borrow().len() < n {
                    glib::timeout_future(Duration::from_millis(10)).await;
                }
            }
        };
        use IdleChange::*;
        let sequence = async {
            let (what, idle) = calls.recv().await.unwrap();
            assert_eq!(what, "idle");
            let (what, _sleep) = calls.recv().await.unwrap();
            assert_eq!(what, "sleep");
            wait_for(2).await;
            assert_eq!(changes(&told), [HeldBackAgain, Ready]);

            let since = Instant::now();
            locked.set(Some(since));
            lock_changed.try_send(()).unwrap();
            await_released(idle).await;
            // Adding nothing new, or dropping the suspend bit, on the locked
            // screen restarts nothing; the drop is told with its time.
            glib::timeout_future(Duration::from_millis(500)).await;
            actions.store(INHIBIT_IDLE, Ordering::SeqCst);
            emit_inhibited_actions_changed(&server, INHIBIT_IDLE).await;
            wait_for(3).await;
            let (told_at, change) = told.borrow()[2];
            assert!(
                matches!(change, SuspendAllowedWhileLocked(at) if at > since && at <= told_at),
                "{change:?}"
            );
            actions.store(INHIBIT_IDLE | INHIBIT_SUSPEND, Ordering::SeqCst);
            emit_inhibited_actions_changed(&server, INHIBIT_IDLE | INHIBIT_SUSPEND).await;
            let (what, _sleep) = calls.recv().await.unwrap();
            assert_eq!(what, "sleep");
            // The blank delay from the lock is kept, once, with no change
            // signal to wake the watcher.
            wait_for(4).await;
            let (at, change) = told.borrow()[3];
            assert_eq!(change, FreshLockIdle(since));
            assert!(at >= since + FRESH_LOCK_BLANK, "told early");
            assert!(
                at < since + FRESH_LOCK_BLANK + Duration::from_secs(2),
                "told late"
            );
            emit_inhibited_actions_changed(&server, INHIBIT_IDLE | INHIBIT_SUSPEND).await;
            glib::timeout_future(Duration::from_millis(300)).await;
            assert_eq!(told.borrow().len(), 4);

            // A new lock without an idle inhibitor: told once one comes, as
            // its delay is up.
            locked.set(None);
            lock_changed.try_send(()).unwrap();
            let (what, idle) = calls.recv().await.unwrap();
            assert_eq!(what, "idle");
            wait_for(5).await;
            actions.store(0, Ordering::SeqCst);
            emit_inhibited_actions_changed(&server, 0).await;
            await_released(idle).await;
            wait_for(6).await;
            let since = Instant::now() - FRESH_LOCK_BLANK;
            locked.set(Some(since));
            lock_changed.try_send(()).unwrap();
            glib::timeout_future(Duration::from_millis(300)).await;
            assert_eq!(told.borrow().len(), 6);
            actions.store(INHIBIT_IDLE, Ordering::SeqCst);
            emit_inhibited_actions_changed(&server, INHIBIT_IDLE).await;
            wait_for(7).await;
            assert_eq!(
                changes(&told)[4..],
                [HeldBackAgain, SuspendInhibitorChanged, FreshLockIdle(since)]
            );

            // The idle inhibitor going away on the locked screen is told with
            // its time (the sleep-inactive action counts from then); coming
            // back on it tells nothing.
            let dropped = Instant::now();
            actions.store(0, Ordering::SeqCst);
            emit_inhibited_actions_changed(&server, 0).await;
            wait_for(8).await;
            let (told_at, change) = told.borrow()[7];
            assert!(
                matches!(change, IdleAllowedWhileLocked(at) if at >= dropped && at <= told_at),
                "{change:?}"
            );
            actions.store(INHIBIT_IDLE, Ordering::SeqCst);
            emit_inhibited_actions_changed(&server, INHIBIT_IDLE).await;
            glib::timeout_future(Duration::from_millis(300)).await;
            assert!(calls.try_recv().is_err(), "unexpected logind lock");
            assert_eq!(told.borrow().len(), 8);

            // Back in front on a lock taken in the background: the
            // countdowns begin afresh, nothing is due from the lock.
            locked.set(None);
            lock_changed.try_send(()).unwrap();
            let (what, idle) = calls.recv().await.unwrap();
            assert_eq!(what, "idle");
            wait_for(9).await;
            active.store(false, Ordering::SeqCst);
            emit_session_is_active_changed(&server, false).await;
            await_released(idle).await;
            locked.set(Some(Instant::now() - FRESH_LOCK_BLANK));
            lock_changed.try_send(()).unwrap();
            active.store(true, Ordering::SeqCst);
            emit_session_is_active_changed(&server, true).await;
            wait_for(10).await;
            glib::timeout_future(Duration::from_millis(300)).await;
            assert_eq!(changes(&told)[8..], [HeldBackAgain, BackInFront]);
        };
        pin_mut!(watch, sequence);
        match future::select(watch, sequence).await {
            Either::Left((result, _)) => {
                panic!("watcher ended early: {result:?}");
            }
            Either::Right(_) => {}
        }
    }

    #[test]
    fn locked_changes_keep_the_lock_screen_timing() {
        if std::env::var("REGOLITH_POWERD_TEST_BUS").is_err() {
            eprintln!("skipping: run under dbus-run-session with REGOLITH_POWERD_TEST_BUS=1");
            return;
        }
        let _names = BUS_NAMES.lock().unwrap_or_else(|e| e.into_inner());
        glib::MainContext::new().block_on(async {
            let exercise = exercise_locked_changes();
            let watchdog = glib::timeout_future(Duration::from_secs(20));
            pin_mut!(exercise, watchdog);
            if let Either::Right(_) = future::select(exercise, watchdog).await {
                panic!("locked changes test timed out");
            }
        });
    }

    #[test]
    fn idle_lock_follows_the_lock_screen_and_the_foreground() {
        // Mock well-known names must only be owned on a private test bus.
        // Run explicitly with: dbus-run-session -- cargo test
        if std::env::var("REGOLITH_POWERD_TEST_BUS").is_err() {
            eprintln!("skipping: run under dbus-run-session with REGOLITH_POWERD_TEST_BUS=1");
            return;
        }
        let _names = BUS_NAMES.lock().unwrap_or_else(|e| e.into_inner());
        glib::MainContext::new().block_on(async {
            let exercise = exercise_lock_and_activity();
            let watchdog = glib::timeout_future(Duration::from_secs(20));
            pin_mut!(exercise, watchdog);
            if let Either::Right(_) = future::select(exercise, watchdog).await {
                panic!("lock and activity test timed out");
            }
        });
    }

    async fn exercise_lock_not_inherited() {
        let actions = Arc::new(AtomicU32::new(INHIBIT_IDLE));
        let (send_calls, calls) = async_channel::unbounded();
        let server = ConnectionBuilder::session()
            .unwrap()
            .name("org.gnome.SessionManager")
            .unwrap()
            .name("org.freedesktop.login1")
            .unwrap()
            .serve_at("/org/gnome/SessionManager", MockSession(actions.clone()))
            .unwrap()
            .serve_at("/org/freedesktop/login1", MockLogind(send_calls))
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::Connection::session().await.unwrap();
        // The screen stays unlocked; keep the sender so the channel stays open.
        let (_lock_changed, lock_changes) = async_channel::bounded(1);
        let watch = watch_session_inhibits(
            &client,
            &client,
            || None,
            &lock_changes,
            || Duration::from_secs(60),
            |_| {},
        );
        let sequence = async {
            let (what, idle) = calls.recv().await.unwrap();
            assert_eq!(what, "idle");
            // The mock reports the call before its reply reaches the watcher:
            // give the watcher time to take the fd.
            glib::timeout_future(Duration::from_millis(250)).await;
            // A program started while the lock is held, as swayidle is when
            // a setting changes.
            let mut child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap();
            actions.store(0, Ordering::SeqCst);
            emit_inhibited_actions_changed(&server, 0).await;
            let released = await_released(idle);
            let deadline = glib::timeout_future(Duration::from_secs(3));
            pin_mut!(released, deadline);
            let held = matches!(future::select(released, deadline).await, Either::Right(_));
            let _ = child.kill();
            let _ = child.wait();
            assert!(
                !held,
                "the lock was let go but a child process still held it"
            );
        };
        pin_mut!(watch, sequence);
        if let Either::Left((result, _)) = future::select(watch, sequence).await {
            panic!("watcher ended early: {result:?}");
        }
    }

    #[test]
    fn a_lock_let_go_is_not_held_by_child_processes() {
        if std::env::var("REGOLITH_POWERD_TEST_BUS").is_err() {
            eprintln!("skipping: run under dbus-run-session with REGOLITH_POWERD_TEST_BUS=1");
            return;
        }
        let _names = BUS_NAMES.lock().unwrap_or_else(|e| e.into_inner());
        glib::MainContext::new().block_on(async {
            let exercise = exercise_lock_not_inherited();
            let watchdog = glib::timeout_future(Duration::from_secs(20));
            pin_mut!(exercise, watchdog);
            if let Either::Right(_) = future::select(exercise, watchdog).await {
                panic!("inheritance test timed out");
            }
        });
    }

    #[test]
    fn poll_interval_is_half_idle_delay_within_bounds() {
        assert_eq!(poll_interval(1), Duration::from_secs(1));
        assert_eq!(poll_interval(60), Duration::from_secs(30));
        assert_eq!(poll_interval(600), Duration::from_secs(30));
        assert_eq!(poll_interval(20), Duration::from_secs(10));
    }

    #[test]
    fn idle_inhibitor_counts_only_unlocked_in_the_foreground() {
        // (actions, session active, locked) -> idle held back
        let cases = [
            ((INHIBIT_IDLE, true, false), true),
            ((INHIBIT_IDLE | INHIBIT_SUSPEND, true, false), true),
            // Locked: the screen still turns off and the machine sleeps
            ((INHIBIT_IDLE, true, true), false),
            ((INHIBIT_IDLE | INHIBIT_SUSPEND, true, true), false),
            // In the background, whatever else
            ((INHIBIT_IDLE, false, false), false),
            ((INHIBIT_IDLE, false, true), false),
            // No idle inhibitor
            ((0, true, false), false),
            ((INHIBIT_SUSPEND, true, false), false),
            ((1 | 2 | 16, true, false), false),
        ];
        for ((actions, active, locked), held) in cases {
            assert_eq!(
                idle_held_back(actions, active, locked),
                held,
                "actions {actions}, active {active}, locked {locked}"
            );
        }
    }

    #[test]
    fn poll_interval_is_longest_when_the_screen_never_blanks() {
        // idle-delay 0 is Screen Blank "Never"
        assert_eq!(poll_interval(0), Duration::from_secs(30));
    }

    #[test]
    fn bridges_signals_and_reconciles_missed_changes() {
        // Mock well-known names must only be owned on a private test bus.
        // Run explicitly with: dbus-run-session -- cargo test
        if std::env::var("REGOLITH_POWERD_TEST_BUS").is_err() {
            eprintln!("skipping: run under dbus-run-session with REGOLITH_POWERD_TEST_BUS=1");
            return;
        }
        let _names = BUS_NAMES.lock().unwrap_or_else(|e| e.into_inner());
        glib::MainContext::new().block_on(async {
            let exercise = exercise_bridge();
            let watchdog = glib::timeout_future(Duration::from_secs(20));
            pin_mut!(exercise, watchdog);
            if let Either::Right(_) = future::select(exercise, watchdog).await {
                panic!("bridge test timed out");
            }
        });
    }
}
