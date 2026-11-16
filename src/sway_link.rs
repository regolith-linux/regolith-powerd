//! The daemon's link to sway's IPC socket, kept up for the whole session.
//!
//! The power-button binding is set over sway IPC, sway's reloads drop it
//! again, and swayidle needs a running sway. Sway is not always there,
//! though: the daemon can start before sway has published its socket, or
//! with no socket address at all, and sway can go away and a new one take
//! its place, at the same socket or at a new one.
//!
//! So the link lives in a thread of its own, as the sway IPC client
//! blocks, and keeps looking for sway: at most once a second while none
//! can be reached, looking the socket up afresh every time. Once one
//! answers it installs the power-button binding, follows that sway's
//! reloads, binding the button again after each one, and tells the main
//! context, which starts swayidle for it (see [`Link::compositor`]). A
//! sway that closes the link or stops answering is given up, and the
//! search starts again.

use glib::g_log;
use std::{
    ffi::OsString,
    io,
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

/// How often sway is looked for while none can be reached.
const ATTEMPT_INTERVAL: Duration = Duration::from_secs(1);

/// How long sway may take to answer a request. One that takes longer is
/// taken for gone: a sway that stopped answering does not close its
/// socket.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How often an established link is checked, so that a sway that stopped
/// answering is noticed even when nothing is sent to it.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// How long the user manager may take to report its environment.
const MANAGER_TIMEOUT_MS: i32 = 1000;

/// The sway IPC socket, and the Wayland display of the same sway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub socket: PathBuf,
    pub display: Option<OsString>,
}

/// The sway the link is established with. `generation` tells a sway found
/// again at the same socket from the one before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compositor {
    pub target: Target,
    pub generation: u64,
}

enum Request {
    /// The sway commands that bind the power button, for the current
    /// power-button setting.
    PowerButton(Vec<String>),
    /// The sway of this generation reloaded its config.
    Reload(u64),
    /// The reload events of the sway of this generation ended.
    WatchEnded(u64, String),
}

/// Handle to the link thread.
#[derive(Clone)]
pub struct Link {
    requests: mpsc::Sender<Request>,
    current: Arc<Mutex<Option<Compositor>>>,
}

impl Link {
    /// Start looking for sway. `power_button` is the binding to install,
    /// see [`Link::set_power_button`]. `on_change` is called, from the link
    /// thread, whenever a sway is found or lost.
    pub fn start(power_button: Vec<String>, on_change: impl Fn() + Send + 'static) -> Self {
        Self::spawn(Target::own(), true, power_button, on_change)
    }

    /// [`Link::start`] with the daemon's own sway given, and the user
    /// manager asked or not.
    fn spawn(
        own: Option<Target>,
        ask_manager: bool,
        power_button: Vec<String>,
        on_change: impl Fn() + Send + 'static,
    ) -> Self {
        let (requests, receiver) = mpsc::channel();
        let current = Arc::new(Mutex::new(None));
        let worker = Worker {
            own,
            ask_manager,
            requests: requests.clone(),
            receiver,
            current: current.clone(),
            on_change: Box::new(on_change),
            power_button,
            generation: 0,
        };
        std::thread::spawn(move || worker.run());
        Self { requests, current }
    }

    /// Bind the power button with these commands now, if sway is there,
    /// and on every sway found from now on and after every reload. Never
    /// blocks.
    pub fn set_power_button(&self, commands: Vec<String>) {
        let _ = self.requests.send(Request::PowerButton(commands));
    }

    /// The sway the link is established with, if any.
    pub fn compositor(&self) -> Option<Compositor> {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Target {
    /// The daemon's own `SWAYSOCK` and `WAYLAND_DISPLAY`.
    fn own() -> Option<Self> {
        Self::from_vars(
            std::env::var_os("SWAYSOCK"),
            std::env::var_os("WAYLAND_DISPLAY"),
        )
    }

    fn from_vars(socket: Option<OsString>, display: Option<OsString>) -> Option<Self> {
        let socket = socket.filter(|s| !s.is_empty())?;
        Some(Self {
            socket: PathBuf::from(socket),
            display: display.filter(|d| !d.is_empty()),
        })
    }

    /// The sway named in the user manager's environment (`KEY=VALUE`
    /// entries). Its display comes from the same list, or else from the
    /// daemon's own environment.
    fn from_environment(entries: &[String], own: Option<&Target>) -> Option<Self> {
        let var = |name: &str| {
            entries
                .iter()
                .rev()
                .find_map(|e| e.strip_prefix(name)?.strip_prefix('='))
                .map(OsString::from)
        };
        let display = var("WAYLAND_DISPLAY").or_else(|| own.and_then(|o| o.display.clone()));
        Self::from_vars(var("SWAYSOCK"), display)
    }
}

/// The sways to try, in order: the one in the user manager's environment,
/// then the daemon's own. `I3SOCK` is never looked at: it is the socket
/// i3 tools prefer, and it can name another sway, or none.
fn candidates(manager: Option<&[String]>, own: Option<&Target>) -> Vec<Target> {
    let mut targets: Vec<Target> = Vec::new();
    let from_manager = manager.and_then(|entries| Target::from_environment(entries, own));
    for target in from_manager.into_iter().chain(own.cloned()) {
        if !targets.iter().any(|t| t.socket == target.socket) {
            targets.push(target);
        }
    }
    targets
}

/// The environment of the systemd user manager, which the session sets
/// `SWAYSOCK` and `WAYLAND_DISPLAY` in when sway starts. None without a
/// manager on the session bus. The property announces no changes, so it
/// is read again every time.
fn manager_environment(bus: &gio::DBusConnection) -> Option<Vec<String>> {
    use glib::ToVariant;
    let reply = bus
        .call_sync(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&("org.freedesktop.systemd1.Manager", "Environment").to_variant()),
            glib::VariantTy::new("(v)").ok(),
            gio::DBusCallFlags::NO_AUTO_START,
            MANAGER_TIMEOUT_MS,
            gio::Cancellable::NONE,
        )
        .ok()?;
    reply.child_value(0).as_variant()?.get::<Vec<String>>()
}

/// Open a connection to the sway IPC socket at `socket`. Nothing waits
/// longer than [`REPLY_TIMEOUT`] for sway.
fn connect(socket: &Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
    Ok(stream)
}

/// A connection to the sway at `socket`, if that sway answers.
fn open(socket: &Path) -> swayipc::Fallible<swayipc::Connection> {
    let mut conn = swayipc::Connection::from(connect(socket)?);
    conn.get_version()?;
    Ok(conn)
}

/// Subscribe to the workspace events of the sway at `socket`. Returns the
/// events, and a handle to the socket to end them with.
fn subscribe(socket: &Path) -> swayipc::Fallible<(swayipc::EventStream, UnixStream)> {
    let stream = connect(socket)?;
    let handle = stream.try_clone()?;
    let events = swayipc::Connection::from(stream).subscribe([swayipc::EventType::Workspace])?;
    // Events come when they come: the link is checked on the other
    // connection, and this one is shut down when it is lost.
    handle.set_read_timeout(None)?;
    Ok((events, handle))
}

/// Run the power-button commands.
fn bind_power_button(conn: &mut swayipc::Connection, commands: &[String]) -> swayipc::Fallible<()> {
    for command in commands {
        // A command sway refuses (unbinding a key that is not bound) is
        // fine; only losing sway is not.
        conn.run_command(command)?;
    }
    Ok(())
}

/// An established link.
struct Session {
    target: Target,
    conn: swayipc::Connection,
    events: UnixStream,
}

struct Worker {
    own: Option<Target>,
    /// Whether to look in the user manager's environment
    ask_manager: bool,
    requests: mpsc::Sender<Request>,
    receiver: mpsc::Receiver<Request>,
    current: Arc<Mutex<Option<Compositor>>>,
    on_change: Box<dyn Fn() + Send>,
    power_button: Vec<String>,
    /// Counts the links made
    generation: u64,
}

impl Worker {
    fn run(mut self) {
        // Looked up once: the session bus is not retried.
        let bus = self
            .ask_manager
            .then(|| gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok())
            .flatten();
        let mut warned = false;
        // The worker holds a sender of its own, for the reload threads, so
        // the requests never end and neither does the link: it lasts as long
        // as the daemon.
        loop {
            let Some(session) = self.find(bus.as_ref(), &mut warned) else {
                return;
            };
            let Some(reason) = self.follow(session) else {
                return;
            };
            g_log!(
                glib::LogLevel::Warning,
                "Lost the link to sway: {reason}; waiting for sway to come back"
            );
            warned = true;
        }
    }

    /// Look for sway until one answers. None if the requests ended.
    fn find(&mut self, bus: Option<&gio::DBusConnection>, warned: &mut bool) -> Option<Session> {
        loop {
            let started = Instant::now();
            match self.attempt(bus) {
                Ok(session) => {
                    *warned = false;
                    return Some(session);
                }
                // Once per outage, not once per attempt
                Err(e) if !*warned => {
                    g_log!(
                        glib::LogLevel::Warning,
                        "No sway IPC socket can be reached ({e}); waiting for sway"
                    );
                    *warned = true;
                }
                Err(_) => {}
            }
            let next = started + ATTEMPT_INTERVAL;
            while let Some(left) = next.checked_duration_since(Instant::now()) {
                match self.receiver.recv_timeout(left) {
                    Ok(request) => self.handle_idle(request),
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                }
            }
        }
    }

    /// A request while no sway is linked: only the binding is kept.
    fn handle_idle(&mut self, request: Request) {
        if let Request::PowerButton(commands) = request {
            self.power_button = commands;
        }
    }

    /// One attempt to link with sway, looking it up afresh.
    fn attempt(&mut self, bus: Option<&gio::DBusConnection>) -> Result<Session, String> {
        let manager = bus.and_then(manager_environment);
        let targets = candidates(manager.as_deref(), self.own.as_ref());
        if targets.is_empty() {
            return Err("SWAYSOCK is not set".to_string());
        }
        let mut errors = Vec::new();
        for target in targets {
            match open(&target.socket) {
                Ok(conn) => match subscribe(&target.socket) {
                    Ok((events, handle)) => {
                        return Ok(self.establish(target, conn, events, handle));
                    }
                    Err(e) => errors.push(format!("{}: {e}", target.socket.display())),
                },
                Err(e) => errors.push(format!("{}: {e}", target.socket.display())),
            }
        }
        Err(errors.join("; "))
    }

    fn establish(
        &mut self,
        target: Target,
        conn: swayipc::Connection,
        events: swayipc::EventStream,
        handle: UnixStream,
    ) -> Session {
        self.generation += 1;
        let generation = self.generation;
        let requests = self.requests.clone();
        std::thread::spawn(move || {
            let result = follow_sway_reloads(events, || {
                let _ = requests.send(Request::Reload(generation));
            });
            let reason = match result {
                Ok(()) => "sway closed the connection".to_string(),
                Err(e) => e.to_string(),
            };
            let _ = requests.send(Request::WatchEnded(generation, reason));
        });
        Session {
            target,
            conn,
            events: handle,
        }
    }

    fn publish(&self, compositor: Option<Compositor>) {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = compositor;
        (self.on_change)();
    }

    /// Keep the link with sway: bind the power button now and after every
    /// reload, and check that sway still answers. Returns why the link was
    /// lost, or None if the requests ended.
    fn follow(&mut self, mut session: Session) -> Option<String> {
        let generation = self.generation;
        // A binding chosen meanwhile is the one to install.
        while let Ok(request) = self.receiver.try_recv() {
            self.handle_idle(request);
        }
        let reason = 'link: {
            if let Err(e) = bind_power_button(&mut session.conn, &self.power_button) {
                break 'link e.to_string();
            }
            g_log!(
                glib::LogLevel::Info,
                "Linked with sway at {}",
                session.target.socket.display()
            );
            self.publish(Some(Compositor {
                target: session.target.clone(),
                generation,
            }));
            let mut beat = Instant::now() + HEARTBEAT_INTERVAL;
            loop {
                let left = beat.saturating_duration_since(Instant::now());
                let result = match self.receiver.recv_timeout(left) {
                    Ok(Request::PowerButton(commands)) => {
                        self.power_button = commands;
                        bind_power_button(&mut session.conn, &self.power_button)
                    }
                    Ok(Request::Reload(g)) if g == generation => {
                        bind_power_button(&mut session.conn, &self.power_button)
                    }
                    Ok(Request::WatchEnded(g, reason)) if g == generation => break 'link reason,
                    // From an earlier link
                    Ok(_) => Ok(()),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        beat = Instant::now() + HEARTBEAT_INTERVAL;
                        session.conn.get_version().map(|_| ())
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                };
                if let Err(e) = result {
                    break 'link e.to_string();
                }
            }
        };
        // Ends the reload events, also of a sway that stopped answering.
        let _ = session.events.shutdown(Shutdown::Both);
        if self.compositor().is_some() {
            self.publish(None);
        }
        Some(format!("{}: {reason}", session.target.socket.display()))
    }

    fn compositor(&self) -> Option<Compositor> {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Call `on_reload` after every sway config reload (the workspace event
/// with change `reload`, sent once the new config is in place), until the
/// connection to sway ends. Events that cannot be read are skipped.
///
/// The power-button binding is added over sway IPC, and `swaymsg reload`
/// (Super+Shift+C, and `regolith-look` after every look change) rebuilds
/// the bindings from the config files, which drops it. powerd keeps
/// logind's power key inhibitor, so the button would then do nothing at
/// all; the link binds it again after every reload.
pub fn follow_sway_reloads(
    events: swayipc::EventStream,
    mut on_reload: impl FnMut(),
) -> swayipc::Fallible<()> {
    for event in events {
        match event {
            Ok(swayipc::Event::Workspace(workspace)) => {
                if workspace.change == swayipc::WorkspaceChange::Reload {
                    on_reload();
                }
            }
            Ok(_) => {}
            // An event of a newer sway than this client knows; the stream
            // itself is fine.
            Err(swayipc::Error::SerdeJson(_)) | Err(swayipc::Error::UnimplementedEvent(..)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    fn target(socket: &str, display: Option<&str>) -> Target {
        Target {
            socket: PathBuf::from(socket),
            display: display.map(OsString::from),
        }
    }

    fn env(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|e| e.to_string()).collect()
    }

    #[test]
    fn the_user_managers_sway_comes_first_then_the_daemons_own() {
        let own = target("/run/own.sock", Some("wayland-0"));
        let manager = env(&[
            "PATH=/usr/bin",
            "SWAYSOCK=/run/mgr.sock",
            "WAYLAND_DISPLAY=wayland-1",
        ]);
        assert_eq!(
            candidates(Some(&manager), Some(&own)),
            [target("/run/mgr.sock", Some("wayland-1")), own.clone()]
        );
        // The same socket is tried once.
        let same = env(&["SWAYSOCK=/run/own.sock", "WAYLAND_DISPLAY=wayland-0"]);
        assert_eq!(
            candidates(Some(&same), Some(&own)),
            std::slice::from_ref(&own)
        );
    }

    #[test]
    fn the_daemons_own_sway_without_a_manager_or_its_swaysock() {
        let own = target("/run/own.sock", Some("wayland-0"));
        assert_eq!(candidates(None, Some(&own)), std::slice::from_ref(&own));
        let without = env(&["WAYLAND_DISPLAY=wayland-1"]);
        assert_eq!(
            candidates(Some(&without), Some(&own)),
            std::slice::from_ref(&own)
        );
        let empty = env(&["SWAYSOCK="]);
        assert_eq!(candidates(Some(&empty), Some(&own)), [own]);
        assert_eq!(candidates(Some(&without), None), []);
    }

    #[test]
    fn i3sock_is_never_used() {
        let manager = env(&["I3SOCK=/run/i3.sock", "WAYLAND_DISPLAY=wayland-1"]);
        assert_eq!(candidates(Some(&manager), None), []);
        assert_eq!(Target::from_vars(None, Some("wayland-0".into())), None);
    }

    #[test]
    fn the_managers_sway_without_a_display_takes_the_daemons_own() {
        let own = target("/run/own.sock", Some("wayland-0"));
        let manager = env(&["SWAYSOCK=/run/mgr.sock", "SWAYSOCK_OLD=/x"]);
        assert_eq!(
            candidates(Some(&manager), Some(&own))[0],
            target("/run/mgr.sock", Some("wayland-0"))
        );
        // The last entry of a name counts.
        let twice = env(&["SWAYSOCK=/run/a.sock", "SWAYSOCK=/run/b.sock"]);
        assert_eq!(
            candidates(Some(&twice), None),
            [target("/run/b.sock", None)]
        );
    }

    /// A socket path in a fresh directory. Kept short: socket paths have
    /// to fit in 107 bytes, and the temporary directory can be deep.
    fn socket_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rpl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("s")
    }

    /// Answer one GET_VERSION as sway does, or (with `answer` false) read
    /// it and say nothing until the client gives up.
    fn fake_sway(listener: UnixListener, answer: bool) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut header = [0u8; 14];
            stream.read_exact(&mut header).unwrap();
            assert_eq!(&header[..6], b"i3-ipc");
            assert_eq!(u32::from_ne_bytes(header[10..14].try_into().unwrap()), 7);
            if answer {
                let payload = r#"{"major":1,"minor":11,"patch":0,"human_readable":"1.11","loaded_config_file_name":"/etc/sway/config"}"#;
                let mut msg = b"i3-ipc".to_vec();
                msg.extend((payload.len() as u32).to_ne_bytes());
                msg.extend(7u32.to_ne_bytes());
                msg.extend(payload.as_bytes());
                stream.write_all(&msg).unwrap();
            } else {
                let mut rest = Vec::new();
                let _ = stream.read_to_end(&mut rest);
            }
        })
    }

    #[test]
    fn a_sway_is_reachable_only_when_it_answers() {
        let path = socket_path("answers");
        let sway = fake_sway(UnixListener::bind(&path).unwrap(), true);
        assert!(open(&path).is_ok());
        sway.join().unwrap();

        let path = socket_path("silent");
        let sway = fake_sway(UnixListener::bind(&path).unwrap(), false);
        let start = Instant::now();
        assert!(open(&path).is_err());
        assert!(start.elapsed() < REPLY_TIMEOUT * 3, "{:?}", start.elapsed());
        sway.join().unwrap();

        // Closed at once, or not there at all
        let path = socket_path("closes");
        let listener = UnixListener::bind(&path).unwrap();
        let closer = std::thread::spawn(move || drop(listener.accept().unwrap()));
        assert!(open(&path).is_err());
        closer.join().unwrap();
        let missing = socket_path("missing");
        assert!(open(&missing).is_err());
        for path in [
            missing,
            socket_path("answers"),
            socket_path("silent"),
            socket_path("closes"),
        ] {
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
    }

    /// A sway IPC message: magic, payload length, type, payload.
    fn ipc_message(kind: u32, payload: &str) -> Vec<u8> {
        let mut msg = b"i3-ipc".to_vec();
        msg.extend((payload.len() as u32).to_ne_bytes());
        msg.extend(kind.to_ne_bytes());
        msg.extend(payload.as_bytes());
        msg
    }

    /// Read one sway IPC message: its type and payload.
    fn read_message(stream: &mut UnixStream) -> io::Result<(u32, String)> {
        let mut header = [0u8; 14];
        stream.read_exact(&mut header)?;
        assert_eq!(&header[..6], b"i3-ipc");
        let len = u32::from_ne_bytes(header[6..10].try_into().unwrap());
        let kind = u32::from_ne_bytes(header[10..14].try_into().unwrap());
        let mut payload = vec![0u8; len as usize];
        stream.read_exact(&mut payload)?;
        Ok((kind, String::from_utf8(payload).unwrap()))
    }

    #[test]
    fn follows_sway_reloads_until_sway_goes_away() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let fake_sway = std::thread::spawn(move || {
            let (kind, payload) = read_message(&mut server).unwrap();
            assert_eq!(kind, 2, "SUBSCRIBE");
            assert_eq!(payload, r#"["workspace"]"#);
            server
                .write_all(&ipc_message(2, r#"{"success":true}"#))
                .unwrap();
            // Workspace events; only "reload" counts.
            for change in ["reload", "init", "empty", "reload"] {
                let event = format!(r#"{{"change":"{change}","current":null,"old":null}}"#);
                server.write_all(&ipc_message(0x8000_0000, &event)).unwrap();
            }
            // Dropping the socket stands for sway exiting.
        });
        let events = swayipc::Connection::from(client)
            .subscribe([swayipc::EventType::Workspace])
            .unwrap();
        let mut reloads = 0;
        let result = follow_sway_reloads(events, || reloads += 1);
        fake_sway.join().unwrap();
        assert_eq!(reloads, 2);
        assert!(result.is_err());
    }

    /// A sway at a socket that answers GET_VERSION, RUN_COMMAND and
    /// SUBSCRIBE, records the commands it is sent, and sends reload events
    /// on request. It goes away when stopped or dropped.
    struct FakeSway {
        path: PathBuf,
        commands: Arc<Mutex<Vec<String>>>,
        subscribers: Arc<Mutex<Vec<UnixStream>>>,
        clients: Arc<Mutex<Vec<UnixStream>>>,
        /// Set: requests are read and not answered.
        frozen: Arc<std::sync::atomic::AtomicBool>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        acceptor: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeSway {
        fn start(path: &Path, commands: Arc<Mutex<Vec<String>>>) -> Self {
            use std::sync::atomic::{AtomicBool, Ordering};
            let listener = UnixListener::bind(path).unwrap();
            listener.set_nonblocking(true).unwrap();
            let subscribers = Arc::new(Mutex::new(Vec::new()));
            let clients = Arc::new(Mutex::new(Vec::new()));
            let frozen = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let acceptor = {
                let (commands, subscribers, clients, frozen, stop) = (
                    commands.clone(),
                    subscribers.clone(),
                    clients.clone(),
                    frozen.clone(),
                    stop.clone(),
                );
                std::thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let mut stream = match listener.accept() {
                            Ok((stream, _)) => stream,
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(10));
                                continue;
                            }
                            Err(e) => panic!("{e}"),
                        };
                        stream.set_nonblocking(false).unwrap();
                        clients.lock().unwrap().push(stream.try_clone().unwrap());
                        let (commands, subscribers, frozen) =
                            (commands.clone(), subscribers.clone(), frozen.clone());
                        std::thread::spawn(move || {
                            while let Ok((kind, payload)) = read_message(&mut stream) {
                                if frozen.load(Ordering::SeqCst) {
                                    continue;
                                }
                                let reply = match kind {
                                    0 => {
                                        commands.lock().unwrap().push(payload);
                                        r#"[{"success":true}]"#.to_string()
                                    }
                                    2 => {
                                        subscribers
                                            .lock()
                                            .unwrap()
                                            .push(stream.try_clone().unwrap());
                                        r#"{"success":true}"#.to_string()
                                    }
                                    7 => r#"{"major":1,"minor":11,"patch":0,"human_readable":"1.11","loaded_config_file_name":"/etc/sway/config"}"#.to_string(),
                                    _ => panic!("unexpected message type {kind}"),
                                };
                                if stream.write_all(&ipc_message(kind, &reply)).is_err() {
                                    break;
                                }
                            }
                        });
                    }
                })
            };
            Self {
                path: path.to_owned(),
                commands,
                subscribers,
                clients,
                frozen,
                stop,
                acceptor: Some(acceptor),
            }
        }

        fn reload(&self) {
            let event = ipc_message(
                0x8000_0000,
                r#"{"change":"reload","current":null,"old":null}"#,
            );
            for subscriber in self.subscribers.lock().unwrap().iter_mut() {
                let _ = subscriber.write_all(&event);
            }
        }

        /// Keep the socket open and stop answering, as a stopped sway does.
        fn freeze(&self) {
            self.frozen.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    impl Drop for FakeSway {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(acceptor) = self.acceptor.take() {
                let _ = acceptor.join();
            }
            for client in self.clients.lock().unwrap().drain(..) {
                let _ = client.shutdown(Shutdown::Both);
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Wait up to `limit` for `done`.
    fn wait_for(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < limit, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn binding(action: &str) -> Vec<String> {
        vec![
            "unbindsym XF86Poweroff".to_string(),
            format!("bindsym XF86Poweroff exec \"{action}\""),
        ]
    }

    #[test]
    fn the_link_finds_sway_late_and_again_after_every_loss() {
        let path = socket_path("link");
        let (changed, changes) = mpsc::channel();
        let link = Link::spawn(
            Some(target(path.to_str().unwrap(), Some("wayland-7"))),
            false,
            binding("systemctl suspend"),
            move || {
                let _ = changed.send(());
            },
        );
        let reach = Duration::from_secs(3);

        // No sway yet: nothing is linked, and a binding chosen meanwhile is
        // kept for the sway to come.
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(link.compositor(), None);
        assert!(changes.try_recv().is_err());
        link.set_power_button(binding("systemctl hibernate"));

        // Sway appears: the current binding is installed.
        let commands = Arc::new(Mutex::new(Vec::new()));
        let sway = FakeSway::start(&path, commands.clone());
        wait_for(reach, "the first sway", || link.compositor().is_some());
        let first = link.compositor().unwrap();
        assert_eq!(
            first.target,
            target(path.to_str().unwrap(), Some("wayland-7"))
        );
        changes.recv_timeout(reach).unwrap();
        wait_for(reach, "the binding", || sway.commands().len() == 2);
        assert_eq!(sway.commands(), binding("systemctl hibernate"));

        // A reload drops the binding; it is installed again.
        sway.reload();
        wait_for(Duration::from_secs(2), "the binding after a reload", || {
            sway.commands().len() == 4
        });
        assert_eq!(sway.commands()[2..], binding("systemctl hibernate"));

        // A change of the setting is installed at once.
        link.set_power_button(binding("systemctl poweroff"));
        wait_for(reach, "the new binding", || sway.commands().len() == 6);
        assert_eq!(sway.commands()[4..], binding("systemctl poweroff"));

        // Sway goes away and a new one comes at the same socket.
        drop(sway);
        wait_for(reach, "the loss of sway", || link.compositor().is_none());
        changes.recv_timeout(reach).unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let sway = FakeSway::start(&path, commands.clone());
        wait_for(reach, "the second sway", || link.compositor().is_some());
        let second = link.compositor().unwrap();
        assert_ne!(second, first, "a new sway at the same socket is a new link");
        wait_for(reach, "the binding on the second sway", || {
            sway.commands().len() == 2
        });
        assert_eq!(sway.commands(), binding("systemctl poweroff"));
        sway.reload();
        wait_for(Duration::from_secs(2), "the binding after a reload", || {
            sway.commands().len() == 4
        });

        // A sway that stops answering is given up, though its socket stays.
        sway.freeze();
        wait_for(
            HEARTBEAT_INTERVAL + REPLY_TIMEOUT + reach,
            "a frozen sway to be given up",
            || link.compositor().is_none(),
        );
        drop(sway);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
