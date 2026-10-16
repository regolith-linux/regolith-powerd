//! Watch `org.gnome.SessionManager.InhibitedActions` and mirror it onto
//! logind inhibitor locks, so that the Sway idle system (swayidle honors
//! logind's `BlockInhibited`) reacts to applications inhibiting idleness
//! through GNOME interfaces.

use glib::g_log;
use std::time::Duration;
use zbus::export::futures_util::{future, StreamExt};
use zbus::zvariant::OwnedFd;
use zbus::{dbus_proxy, Result};

use crate::logind::{reconcile_lock, LogindProxy};

fn warn_reconcile_failed(e: zbus::Error) {
    g_log!(
        glib::LogLevel::Warning,
        "Failed to reconcile session inhibition: {e}"
    );
}

const INHIBIT_IDLE: u32 = 1 << 3;
const INHIBIT_SUSPEND: u32 = 1 << 2;

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
}

#[derive(Default)]
struct SessionInhibitors {
    idle: Option<OwnedFd>,
    sleep: Option<OwnedFd>,
}

impl SessionInhibitors {
    async fn reconcile(&mut self, proxy: &LogindProxy<'_>, actions: u32) -> Result<()> {
        // Reconcile independently so a failure taking one lock does not leave
        // the other lock stale. Dropping an fd releases its inhibitor.
        let idle = reconcile_lock(&mut self.idle, proxy, actions & INHIBIT_IDLE != 0, "idle").await;
        let sleep = reconcile_lock(
            &mut self.sleep,
            proxy,
            actions & INHIBIT_SUSPEND != 0,
            "sleep",
        )
        .await;
        idle.and(sleep)
    }
}

/// Mirror GNOME's aggregated inhibition state to logind. Swayidle consumes
/// logind's idle lock through BlockInhibited without needing a restart.
/// Never returns: connection failures are logged and retried.
pub async fn run_inhibits() {
    let settings = crate::SessionSettings::new();
    loop {
        let result: Result<()> = async {
            let session_bus = zbus::Connection::session().await?;
            let system_bus = zbus::Connection::system().await?;
            watch_session_inhibits(&session_bus, &system_bus, || {
                Duration::from_secs(u64::from((settings.idle_delay() / 2).max(1)))
            })
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

async fn reconcile(
    session: &SessionManagerProxy<'_>,
    logind: &LogindProxy<'_>,
    inhibitors: &mut SessionInhibitors,
) -> Result<()> {
    let actions = session.inhibited_actions().await?;
    inhibitors.reconcile(logind, actions).await
}

async fn watch_session_inhibits(
    session_bus: &zbus::Connection,
    system_bus: &zbus::Connection,
    interval: impl Fn() -> Duration,
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
    // merely pokes us; `reconcile` below always fetches live state.
    let properties = zbus::fdo::PropertiesProxy::builder(session_bus)
        .destination("org.gnome.SessionManager")?
        .path("/org/gnome/SessionManager")?
        .build()
        .await?;
    // Subscribe before the initial read so a concurrent change is not lost.
    let mut changes = properties.receive_properties_changed().await?;
    let mut inhibitors = SessionInhibitors::default();
    // Never propagate a failed first reconciliation: it would abort the
    // watcher and drop locks already taken. The loop below keeps retrying
    // on every tick.
    if let Err(e) = reconcile(&session, &logind, &mut inhibitors).await {
        warn_reconcile_failed(e);
    }

    // Reconcile on property changes and, independently, on a periodic tick,
    // so a missed change signal cannot leave the locks stale. The tick is
    // polled before signals (so its timer is armed promptly) and reset only
    // when it fires, so draining a signal backlog cannot postpone the next
    // deadline — and every drained signal reconciles live state anyway.
    let mut tick = glib::timeout_future(interval());
    loop {
        let ticked = match future::select(&mut tick, changes.next()).await {
            future::Either::Left(((), _)) => true,
            // The signal stream only ends when the connection is lost; the
            // caller then reconnects and subscribes afresh.
            future::Either::Right((None, _)) => return Ok(()),
            future::Either::Right((Some(_), _)) => false,
        };
        if ticked {
            // Schedule the next tick before the slow D-Bus work below.
            tick = glib::timeout_future(interval());
        }
        if let Err(e) = reconcile(&session, &logind, &mut inhibitors).await {
            // A transient read failure keeps the last known lock state; the
            // next tick retries, including after a GNOME service restart.
            warn_reconcile_failed(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{ErrorKind, Read};
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;
    use std::sync::{
        atomic::{AtomicU32, AtomicUsize, Ordering},
        Arc,
    };
    use zbus::export::futures_util::{future::Either, pin_mut};
    use zbus::{dbus_interface, ConnectionBuilder};

    struct MockSession(Arc<AtomicU32>);

    #[dbus_interface(name = "org.gnome.SessionManager")]
    impl MockSession {
        #[dbus_interface(property)]
        fn inhibited_actions(&self) -> u32 {
            self.0.load(Ordering::SeqCst)
        }
    }

    struct MockLogind(async_channel::Sender<(String, UnixStream)>);

    #[dbus_interface(name = "org.freedesktop.login1.Manager")]
    impl MockLogind {
        async fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> OwnedFd {
            assert_eq!(who, crate::logind::WHO);
            assert_eq!(why, "GNOME session inhibition");
            assert_eq!(mode, "block");
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
        let watch = watch_session_inhibits(&client, &client, move || {
            tick_counter.fetch_add(1, Ordering::SeqCst);
            Duration::from_secs(1)
        });
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

    #[test]
    fn bridges_signals_and_reconciles_missed_changes() {
        // Mock well-known names must only be owned on a private test bus.
        // Run explicitly with: dbus-run-session -- cargo test
        if std::env::var("REGOLITH_POWERD_TEST_BUS").is_err() {
            eprintln!("skipping: run under dbus-run-session with REGOLITH_POWERD_TEST_BUS=1");
            return;
        }
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
