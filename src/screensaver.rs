//! `org.gnome.ScreenSaver` for the Wayland session.
//!
//! GNOME components and applications use this interface to follow and drive
//! the lock screen: gnome-session and xdg-desktop-portal-gtk follow
//! `ActiveChanged`, password managers such as KeePassXC lock their databases
//! on it, and `xdg-screensaver lock` calls `Lock`. In the X11 session
//! gnome-flashback provides it. In the sway session nothing did, and the
//! activatable service that the gnome-shell package installs exits at once
//! outside GNOME Shell.
//!
//! The lock screen itself stays where it is: swayidle starts it on idle, on
//! suspend and on logind's `Lock` signal, and the lock key starts it
//! directly. This module follows it instead:
//!
//! - The screen counts as locked while a known lock screen process of this
//!   user runs. `ActiveChanged` is emitted and logind's `LockedHint` set on
//!   every change, however the lock screen was started.
//! - `Lock` and `SetActive(true)` ask logind to lock the session, which is
//!   what `loginctl lock-session` does; swayidle then starts the lock screen.
//! - logind's `Unlock` signal (`loginctl unlock-session`) unlocks: the lock
//!   screen gets the signal on which it unlocks and exits.

use glib::g_log;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use zbus::export::futures_util::StreamExt;
use zbus::fdo::{self, RequestNameFlags};
use zbus::zvariant::OwnedObjectPath;
use zbus::{dbus_interface, dbus_proxy, Connection, SignalContext};

const NAME: &str = "org.gnome.ScreenSaver";
const PATH: &str = "/org/gnome/ScreenSaver";

/// Lock screens, and the signal on which each unlocks and exits. gtklock is
/// the session's default (`wm.program.lock_sway`); it unlocks on SIGTERM.
/// swaylock, the usual replacement, unlocks on SIGUSR1. A lock screen that
/// dies without unlocking leaves the session locked behind sway's fallback
/// screen, so each must get its own signal.
const LOCKERS: &[(&str, &str)] = &[("gtklock", "TERM"), ("swaylock", "USR1")];

/// How often the lock screen is looked for.
const POLL: Duration = Duration::from_secs(1);

#[dbus_proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LoginManager {
    fn get_session(&self, session_id: &str) -> zbus::Result<OwnedObjectPath>;
}

#[dbus_proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1"
)]
trait LoginSession {
    fn lock(&self) -> zbus::Result<()>;
    fn set_locked_hint(&self, locked: bool) -> zbus::Result<()>;
    #[dbus_proxy(signal)]
    fn unlock(&self) -> zbus::Result<()>;
}

/// A lock screen, by process name, and the signal that makes it unlock.
#[derive(Clone)]
pub struct Locker {
    pub name: String,
    pub unlock_signal: String,
}

/// Where and how often to look for which lock screens.
pub struct Config {
    pub proc_root: PathBuf,
    pub lockers: Vec<Locker>,
    pub interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            lockers: LOCKERS
                .iter()
                .map(|(name, unlock_signal)| Locker {
                    name: name.to_string(),
                    unlock_signal: unlock_signal.to_string(),
                })
                .collect(),
            interval: POLL,
        }
    }
}

/// When the screen was locked, while it is.
type LockedSince = Arc<Mutex<Option<Instant>>>;

struct ScreenSaver {
    locked_since: LockedSince,
    session: LoginSessionProxy<'static>,
}

impl ScreenSaver {
    async fn request_lock(&self) -> fdo::Result<()> {
        self.session
            .lock()
            .await
            .map_err(|e| fdo::Error::Failed(format!("Cannot lock the session: {e}")))
    }
}

#[dbus_interface(name = "org.gnome.ScreenSaver")]
impl ScreenSaver {
    async fn lock(&self) -> fdo::Result<()> {
        self.request_lock().await
    }

    fn get_active(&self) -> bool {
        self.locked_since.lock().unwrap().is_some()
    }

    /// Seconds since the screen was locked, 0 when it is not.
    fn get_active_time(&self) -> u32 {
        self.locked_since
            .lock()
            .unwrap()
            .map_or(0, |since| since.elapsed().as_secs() as u32)
    }

    /// Turning the screen saver on locks the screen. Turning it off is
    /// ignored: only the lock screen itself can unlock.
    async fn set_active(&self, value: bool) -> fdo::Result<()> {
        if value {
            self.request_lock().await
        } else {
            Ok(())
        }
    }

    fn show_message(&self, _summary: &str, _body: &str, _icon: &str) {}

    fn simulate_user_activity(&self) {}

    #[dbus_interface(signal)]
    async fn active_changed(ctxt: &SignalContext<'_>, new_value: bool) -> zbus::Result<()>;
}

/// The uid this daemon runs as.
fn current_uid() -> u32 {
    fs::metadata("/proc/self").map_or(u32::MAX, |m| m.uid())
}

/// Running processes of `uid` that are one of `lockers`. Zombies (a lock
/// screen that exited but was not reaped yet) are not running.
fn find_lockers<'l>(proc_root: &Path, uid: u32, lockers: &'l [Locker]) -> Vec<(u32, &'l Locker)> {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if entry.metadata().ok()?.uid() != uid {
                return None;
            }
            let comm = fs::read_to_string(entry.path().join("comm")).ok()?;
            let locker = lockers
                .iter()
                .find(|l| l.name == comm.trim_end_matches('\n'))?;
            // The state follows the parenthesised name in /proc/<pid>/stat.
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let state = stat.rsplit_once(')')?.1.split_whitespace().next()?;
            (state != "Z").then_some((pid, locker))
        })
        .collect()
}

/// Ask a lock screen to unlock.
fn unlock(pid: u32, locker: &Locker) {
    let pid = pid.to_string();
    let signal = format!("-{}", locker.unlock_signal);
    if let Err(e) = Command::new("kill").args([&signal, &pid]).status() {
        g_log!(glib::LogLevel::Warning, "Cannot unlock {pid}: {e}");
    }
}

/// Provide `org.gnome.ScreenSaver` on `session`, for the login session that
/// logind's `auto` resolves to on `system`. Only returns if that cannot be
/// set up; later D-Bus errors are logged.
pub fn serve(session: &Connection, system: &Connection, config: Config) -> zbus::Result<()> {
    let locked_since = LockedSince::default();
    let login = zbus::block_on(async {
        let path = LoginManagerProxy::new(system)
            .await?
            .get_session("auto")
            .await?;
        let login = LoginSessionProxy::builder(system)
            .path(path)?
            .build()
            .await?;
        let iface = ScreenSaver {
            locked_since: locked_since.clone(),
            session: login.clone(),
        };
        session.object_server().at(PATH, iface).await?;
        // gnome-shell's activatable service may already hold the name if
        // something asked for it early; it gives the name up as soon as it
        // finds no GNOME Shell. Queue up behind it instead of failing.
        session
            .request_name_with_flags(NAME, RequestNameFlags::ReplaceExisting.into())
            .await?;
        Ok::<_, zbus::Error>(login)
    })?;

    let uid = current_uid();
    let Config {
        proc_root,
        lockers,
        interval,
    } = config;

    let mut unlocks = zbus::block_on(login.receive_unlock())?;
    let (unlock_root, unlock_lockers) = (proc_root.clone(), lockers.clone());
    thread::spawn(move || {
        zbus::block_on(async {
            while unlocks.next().await.is_some() {
                for (pid, locker) in find_lockers(&unlock_root, uid, &unlock_lockers) {
                    unlock(pid, locker);
                }
            }
        })
    });

    let ctxt = SignalContext::new(session, PATH)?;
    loop {
        let locked = !find_lockers(&proc_root, uid, &lockers).is_empty();
        let changed = {
            let mut since = locked_since.lock().unwrap();
            let changed = since.is_some() != locked;
            if changed {
                *since = locked.then(Instant::now);
            }
            changed
        };
        if changed {
            zbus::block_on(async {
                ScreenSaver::active_changed(&ctxt, locked).await?;
                login.set_locked_hint(locked).await
            })
            .unwrap_or_else(|e| {
                g_log!(
                    glib::LogLevel::Warning,
                    "Cannot announce the lock state: {e}"
                )
            });
        }
        thread::sleep(interval);
    }
}

/// Provide `org.gnome.ScreenSaver` on the user's buses for the lifetime of
/// the daemon, from a thread of its own.
pub fn start() {
    thread::spawn(|| {
        let result = zbus::block_on(async {
            Ok::<_, zbus::Error>((Connection::session().await?, Connection::system().await?))
        })
        .and_then(|(session, system)| serve(&session, &system, Config::default()));
        if let Err(e) = result {
            g_log!(
                glib::LogLevel::Warning,
                "org.gnome.ScreenSaver unavailable: {e}"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Child, Stdio};

    const SESSION_PATH: &str = "/org/freedesktop/login1/session/_31";

    #[test]
    fn finds_running_lockers_of_the_user_only() {
        let root = std::env::temp_dir().join(format!("powerd-proc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let process = |pid: &str, comm: &str, state: &str| {
            let dir = root.join(pid);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
            fs::write(dir.join("stat"), format!("{pid} ({comm}) {state} 1 2 3\n")).unwrap();
        };
        process("100", "gtklock", "S");
        process("101", "swaylock", "R");
        process("102", "gtklock", "Z");
        process("103", "foot", "S");
        process("104", "my (gtklock)", "S");
        fs::create_dir_all(root.join("self")).unwrap();
        let lockers = Config::default().lockers;

        let mut found: Vec<(u32, &str)> = find_lockers(&root, current_uid(), &lockers)
            .into_iter()
            .map(|(pid, locker)| (pid, locker.unlock_signal.as_str()))
            .collect();
        found.sort();
        assert_eq!(found, vec![(100, "TERM"), (101, "USR1")]);
        assert!(find_lockers(&root, current_uid().wrapping_add(1), &lockers).is_empty());
        assert!(find_lockers(&root.join("missing"), current_uid(), &lockers).is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    struct Bus(Child);

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A private bus that stands in for both the system and session bus.
    fn private_bus() -> (Bus, String) {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon is needed to run this test");
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        (Bus(child), address.trim().to_string())
    }

    /// What the stand-in logind was asked to do.
    #[derive(Default)]
    struct LoginLog {
        locks: usize,
        hints: Vec<bool>,
    }

    struct FakeManager;

    #[dbus_interface(name = "org.freedesktop.login1.Manager")]
    impl FakeManager {
        fn get_session(&self, session_id: &str) -> OwnedObjectPath {
            assert_eq!(session_id, "auto");
            OwnedObjectPath::try_from(SESSION_PATH).unwrap()
        }
    }

    /// A login session whose `Lock` starts the lock screen, as swayidle's
    /// `lock` hook does. The lock screens are not reaped until the test
    /// ends, so they linger as zombies once they exit.
    struct FakeSession {
        log: Arc<Mutex<LoginLog>>,
        locker: PathBuf,
        children: Arc<Mutex<Vec<Child>>>,
    }

    #[dbus_interface(name = "org.freedesktop.login1.Session")]
    impl FakeSession {
        fn lock(&self) {
            self.log.lock().unwrap().locks += 1;
            let child = Command::new(&self.locker).spawn().unwrap();
            self.children.lock().unwrap().push(child);
        }

        fn set_locked_hint(&self, locked: bool) {
            self.log.lock().unwrap().hints.push(locked);
        }

        #[dbus_interface(signal)]
        async fn unlock(ctxt: &SignalContext<'_>) -> zbus::Result<()>;
    }

    #[dbus_proxy(
        interface = "org.gnome.ScreenSaver",
        default_service = "org.gnome.ScreenSaver",
        default_path = "/org/gnome/ScreenSaver"
    )]
    trait Client {
        fn lock(&self) -> zbus::Result<()>;
        fn get_active(&self) -> zbus::Result<bool>;
        fn get_active_time(&self) -> zbus::Result<u32>;
        fn set_active(&self, value: bool) -> zbus::Result<()>;
        #[dbus_proxy(signal)]
        fn active_changed(&self, new_value: bool) -> zbus::Result<()>;
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn follows_the_lock_screen_and_locks_and_unlocks_through_logind() {
        let (_bus, address) = private_bus();
        let dir = std::env::temp_dir().join(format!("powerd-locker-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // A lock screen that unlocks on SIGTERM, as gtklock does, and ignores
        // SIGUSR1, which swaylock uses. The name keeps a real lock screen of
        // the same user out of the test.
        let locker = dir.join("rpd-test-lock");
        fs::write(
            &locker,
            "#!/bin/sh\ntrap 'exit 0' TERM\ntrap '' USR1\ni=0\nwhile [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done\n",
        )
        .unwrap();
        fs::set_permissions(&locker, fs::Permissions::from_mode(0o755)).unwrap();

        let log = Arc::new(Mutex::new(LoginLog::default()));
        let children = Arc::new(Mutex::new(Vec::new()));
        let login = zbus::block_on(async {
            zbus::ConnectionBuilder::address(address.as_str())?
                .name("org.freedesktop.login1")?
                .serve_at("/org/freedesktop/login1", FakeManager)?
                .serve_at(
                    SESSION_PATH,
                    FakeSession {
                        log: log.clone(),
                        locker: locker.clone(),
                        children: children.clone(),
                    },
                )?
                .build()
                .await
        })
        .unwrap();

        // Something holds the name first, as gnome-shell's activatable
        // service does when it is started early.
        let early = zbus::block_on(async {
            let conn = zbus::ConnectionBuilder::address(address.as_str())?
                .build()
                .await?;
            conn.request_name(NAME).await?;
            Ok::<_, zbus::Error>(conn)
        })
        .unwrap();

        let server_address = address.clone();
        thread::spawn(move || {
            zbus::block_on(async {
                let conn = zbus::ConnectionBuilder::address(server_address.as_str())?
                    .build()
                    .await?;
                Ok::<_, zbus::Error>(conn)
            })
            .and_then(|conn| {
                serve(
                    &conn,
                    &conn,
                    Config {
                        proc_root: PathBuf::from("/proc"),
                        lockers: vec![Locker {
                            name: "rpd-test-lock".to_string(),
                            unlock_signal: "TERM".to_string(),
                        }],
                        interval: Duration::from_millis(100),
                    },
                )
            })
            .unwrap();
        });

        let client_conn = zbus::block_on(async {
            zbus::ConnectionBuilder::address(address.as_str())?
                .build()
                .await
        })
        .unwrap();
        let client = zbus::block_on(ClientProxy::new(&client_conn)).unwrap();
        let mut changes = zbus::block_on(client.receive_active_changed()).unwrap();
        // Wait (bounded) for the lock state, then for the signal announcing it.
        let expect = |changes: &mut ActiveChangedStream<'_>, locked: bool| {
            wait_until(if locked { "the lock" } else { "the unlock" }, || {
                zbus::block_on(client.get_active()).unwrap() == locked
            });
            let announced = zbus::block_on(changes.next())
                .unwrap()
                .args()
                .unwrap()
                .new_value;
            assert_eq!(announced, locked, "ActiveChanged");
        };

        // The earlier owner keeps the name until it goes away.
        let dbus = zbus::block_on(fdo::DBusProxy::new(&client_conn)).unwrap();
        let owner = || {
            zbus::block_on(dbus.get_name_owner(NAME.try_into().unwrap()))
                .map(|o| o.to_string())
                .ok()
        };
        thread::sleep(Duration::from_millis(300));
        let early_name = early.unique_name().map(|n| n.to_string());
        assert_eq!(owner(), early_name);
        drop(early);
        wait_until("the name", || owner().is_some() && owner() != early_name);
        assert!(!zbus::block_on(client.get_active()).unwrap());
        assert_eq!(zbus::block_on(client.get_active_time()).unwrap(), 0);

        // Lock goes through logind, which starts the lock screen.
        zbus::block_on(client.lock()).unwrap();
        expect(&mut changes, true);
        assert_eq!(log.lock().unwrap().locks, 1);
        wait_until("LockedHint", || log.lock().unwrap().hints == [true]);

        // logind's Unlock (loginctl unlock-session) ends the lock screen.
        let ctxt = SignalContext::new(&login, SESSION_PATH).unwrap();
        zbus::block_on(FakeSession::unlock(&ctxt)).unwrap();
        expect(&mut changes, false);
        wait_until("LockedHint", || log.lock().unwrap().hints == [true, false]);

        // SetActive(true) locks; SetActive(false) does not unlock.
        zbus::block_on(client.set_active(true)).unwrap();
        expect(&mut changes, true);
        zbus::block_on(client.set_active(false)).unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(zbus::block_on(client.get_active()).unwrap());
        assert_eq!(log.lock().unwrap().locks, 2);

        // A lock screen started some other way (the lock key, swayidle's
        // timeout) is followed too, and so is its end.
        zbus::block_on(FakeSession::unlock(&ctxt)).unwrap();
        expect(&mut changes, false);
        let mut direct = Command::new(&locker).spawn().unwrap();
        expect(&mut changes, true);
        direct.kill().unwrap();
        direct.wait().unwrap();
        expect(&mut changes, false);
        wait_until("LockedHint", || {
            log.lock().unwrap().hints == [true, false, true, false, true, false]
        });

        for mut child in children.lock().unwrap().drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
