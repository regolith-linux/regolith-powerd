//! `org.gnome.SessionManager.EndSessionDialog` for the Wayland session.
//!
//! Applications end the session through gnome-session: `gnome-session-quit`,
//! update-notifier and GNOME Software call its `Logout`, `Reboot` and
//! `Shutdown` methods. Unless told not to ask, gnome-session then asks the
//! desktop shell to confirm. It calls `Open` on this interface, exported at
//! `/org/gnome/SessionManager/EndSessionDialog` by whoever owns
//! `org.gnome.Shell`, and waits for one of the `Confirmed*` signals or for
//! `Canceled`. In the X11 session gnome-flashback owns that name and shows its
//! dialog. In the sway session nothing owned it, so gnome-session stopped
//! half way: `Logout(0)` did nothing, `Reboot` and `Shutdown` never returned,
//! and the next end-session request carried out the stalled one instead of
//! its own.
//!
//! This module owns `org.gnome.Shell` to receive those calls. The only other
//! thing served under that name is the Displays panel's monitor labels (see
//! `monitor_labels`). It asks with a swaynag bar, the same kind of prompt
//! that the session's logout, reboot and shutdown keybindings show:
//!
//! - the button confirms, and closing the bar cancels;
//! - when nothing inhibits logging out, the action is taken once the time
//!   gnome-session gives (60 seconds) is up, as gnome-flashback's dialog
//!   does;
//! - applications that inhibit logging out are listed in the bar's details,
//!   and stop that timer;
//! - a request that `gnome-session-quit --no-prompt` (or `--force`) is still
//!   waiting for is confirmed at once, without a second prompt. That is what
//!   the keybindings run once their own prompt is confirmed, but
//!   gnome-session ignores the option for restart and power off. If
//!   something inhibits logging out, the bar is shown anyway, as gnome-session
//!   would do for a logout.

use crate::monitor_labels;
use gio::prelude::*;
use glib::g_log;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use zbus::fdo::{self, RequestNameFlags};
use zbus::zvariant::OwnedObjectPath;
use zbus::{dbus_interface, dbus_proxy, CacheProperties, Connection, SignalContext};

const NAME: &str = "org.gnome.Shell";
const PATH: &str = "/org/gnome/SessionManager/EndSessionDialog";

/// gnome-session's inhibitor flag for logging out, which also covers restart
/// and power off.
const INHIBIT_LOGOUT: u32 = 1;

/// What the confirm button prints, through the shell swaynag runs it with.
const CONFIRMED: &str = "confirmed";

/// What gnome-session asks to confirm: the `type` argument of `Open`.
/// gnome-session only asks for these three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Logout,
    Shutdown,
    Restart,
}

impl Action {
    fn from_type(dialog_type: u32) -> Option<Self> {
        match dialog_type {
            0 => Some(Self::Logout),
            1 => Some(Self::Shutdown),
            2 => Some(Self::Restart),
            _ => None,
        }
    }

    /// The question and button of the session's keybinding prompts
    /// (`sway.prompt.*` in regolith-wm-config).
    fn question(self) -> &'static str {
        match self {
            Self::Logout => "Do you really want to logout?",
            Self::Shutdown => "Do you really want to shutdown?",
            Self::Restart => "Do you really want to reboot your system?",
        }
    }

    fn button(self) -> &'static str {
        match self {
            Self::Logout => "Logout",
            Self::Shutdown => "Shutdown",
            Self::Restart => "Reboot",
        }
    }

    fn countdown(self, seconds: u32) -> String {
        match self {
            Self::Logout => format!("You will be logged out automatically in {seconds} seconds."),
            Self::Shutdown => {
                format!("The system will power off automatically in {seconds} seconds.")
            }
            Self::Restart => format!("The system will restart automatically in {seconds} seconds."),
        }
    }
}

/// An application that inhibits logging out, as the prompt shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inhibitor {
    pub name: String,
    pub reason: String,
}

/// The prompt's command line arguments, and the details it reads from its
/// standard input, if any.
fn prompt_args(
    action: Action,
    inhibitors: &[Inhibitor],
    seconds: u32,
) -> (Vec<String>, Option<String>) {
    let mut message = action.question().to_string();
    if !inhibitors.is_empty() {
        message.push_str(" Some applications are busy or have unsaved work.");
    } else if seconds > 0 {
        message.push(' ');
        message.push_str(&action.countdown(seconds));
    }
    let mut args: Vec<String> = vec![
        "-t".into(),
        "warning".into(),
        "-m".into(),
        message,
        "-Z".into(),
        action.button().into(),
        format!("echo {CONFIRMED}"),
    ];
    if inhibitors.is_empty() {
        return (args, None);
    }
    args.extend(["-l".into(), "-L".into(), "Applications".into()]);
    let details = inhibitors
        .iter()
        .map(|i| match i.reason.as_str() {
            "" => format!("{}\n", i.name),
            reason => format!("{}: {reason}\n", i.name),
        })
        .collect();
    (args, Some(details))
}

/// Whether a `gnome-session-quit` of `uid` that asked for `action` without a
/// prompt is running, that is, still waiting for gnome-session.
fn no_prompt_requested(proc_root: &Path, uid: u32, action: Action) -> bool {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let is_pid = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.parse::<u32>().is_ok());
        if !is_pid || entry.metadata().map_or(true, |m| m.uid() != uid) {
            return false;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            return false;
        };
        let mut argv = cmdline
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(String::from_utf8_lossy);
        let program = argv.next().unwrap_or_default();
        if program.rsplit('/').next() != Some("gnome-session-quit") {
            return false;
        }
        let args: Vec<_> = argv.collect();
        let has = |opt: &str| args.iter().any(|a| a == opt);
        let requested = if has("--power-off") {
            Action::Shutdown
        } else if has("--reboot") {
            Action::Restart
        } else {
            Action::Logout
        };
        requested == action && (has("--no-prompt") || has("--force"))
    })
}

#[dbus_proxy(
    interface = "org.gnome.SessionManager.Inhibitor",
    default_service = "org.gnome.SessionManager"
)]
trait SessionInhibitor {
    fn get_app_id(&self) -> zbus::Result<String>;
    fn get_reason(&self) -> zbus::Result<String>;
    fn get_flags(&self) -> zbus::Result<u32>;
}

/// The name an application id is shown with: its desktop entry's name, as
/// gnome-flashback's dialog shows it, or else the id itself.
fn app_name(app_id: &str) -> String {
    if app_id.is_empty() {
        return "Unknown".into();
    }
    let id = app_id.strip_suffix(".desktop").unwrap_or(app_id);
    [format!("{id}.desktop"), format!("gnome-{id}.desktop")]
        .iter()
        .find_map(|file| gio::DesktopAppInfo::new(file))
        .map_or_else(|| id.to_string(), |info| info.name().to_string())
}

/// The inhibitors among `paths` that inhibit logging out. gnome-session
/// passes all of its inhibitors, including those that only keep the screen
/// from going idle; those don't stop the session from ending.
fn logout_inhibitors(session: &Connection, paths: &[OwnedObjectPath]) -> Vec<Inhibitor> {
    paths
        .iter()
        .filter_map(|path| {
            zbus::block_on(async {
                let proxy = SessionInhibitorProxy::builder(session)
                    .path(path.as_ref())?
                    .cache_properties(CacheProperties::No)
                    .build()
                    .await?;
                if proxy.get_flags().await? & INHIBIT_LOGOUT == 0 {
                    return Ok(None);
                }
                let app_id = proxy.get_app_id().await.unwrap_or_default();
                let reason = proxy.get_reason().await.unwrap_or_default();
                Ok::<_, zbus::Error>(Some((app_id, reason)))
            })
            // An inhibitor that went away meanwhile no longer inhibits.
            .ok()
            .flatten()
            .map(|(app_id, reason)| Inhibitor {
                name: app_name(&app_id),
                reason,
            })
        })
        .collect()
}

enum Event {
    Open {
        action: Action,
        seconds: u32,
        inhibitors: Vec<OwnedObjectPath>,
    },
    Close,
    PromptEnded {
        id: u64,
        confirmed: bool,
    },
}

struct EndSessionDialog {
    events: Mutex<Sender<Event>>,
}

#[dbus_interface(name = "org.gnome.SessionManager.EndSessionDialog")]
impl EndSessionDialog {
    /// Ask to confirm `dialog_type` (0 logout, 1 power off, 2 restart).
    /// Returns at once; the answer comes as a signal.
    fn open(
        &self,
        dialog_type: u32,
        _timestamp: u32,
        seconds_to_stay_open: u32,
        inhibitor_object_paths: Vec<OwnedObjectPath>,
    ) -> fdo::Result<()> {
        let action = Action::from_type(dialog_type).ok_or_else(|| {
            fdo::Error::NotSupported(format!("Unsupported dialog type {dialog_type}"))
        })?;
        self.events
            .lock()
            .unwrap()
            .send(Event::Open {
                action,
                seconds: seconds_to_stay_open,
                inhibitors: inhibitor_object_paths,
            })
            .map_err(|_| fdo::Error::Failed("The dialog is not available".into()))
    }

    /// Withdraw the prompt; gnome-session calls this when the session
    /// cannot end after all.
    fn close(&self) {
        let _ = self.events.lock().unwrap().send(Event::Close);
    }

    #[dbus_interface(signal)]
    async fn confirmed_logout(ctxt: &SignalContext<'_>) -> zbus::Result<()>;
    #[dbus_interface(signal)]
    async fn confirmed_reboot(ctxt: &SignalContext<'_>) -> zbus::Result<()>;
    #[dbus_interface(signal)]
    async fn confirmed_shutdown(ctxt: &SignalContext<'_>) -> zbus::Result<()>;
    #[dbus_interface(signal)]
    async fn confirmed_hibernate(ctxt: &SignalContext<'_>) -> zbus::Result<()>;
    #[dbus_interface(signal)]
    async fn confirmed_suspend(ctxt: &SignalContext<'_>) -> zbus::Result<()>;
    #[dbus_interface(signal)]
    async fn confirmed_hybrid_sleep(ctxt: &SignalContext<'_>) -> zbus::Result<()>;
    #[dbus_interface(signal)]
    async fn canceled(ctxt: &SignalContext<'_>) -> zbus::Result<()>;
    #[dbus_interface(signal)]
    async fn closed(ctxt: &SignalContext<'_>) -> zbus::Result<()>;
}

/// The prompt program, and where to look for `gnome-session-quit`.
pub struct Config {
    pub prompt: PathBuf,
    pub proc_root: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prompt: PathBuf::from("swaynag"),
            proc_root: PathBuf::from("/proc"),
        }
    }
}

/// The prompt on screen.
struct Shown {
    id: u64,
    action: Action,
    inhibitors: Vec<Inhibitor>,
    child: Child,
    /// When the action is taken unless answered; none while inhibited.
    deadline: Option<Instant>,
}

enum Answer {
    Confirmed(Action),
    Canceled,
}

struct Dialog {
    session: Connection,
    config: Config,
    uid: u32,
    events: Sender<Event>,
    shown: Option<Shown>,
    next_id: u64,
}

impl Dialog {
    fn handle(&mut self, event: Event) {
        match event {
            Event::Open {
                action,
                seconds,
                inhibitors,
            } => self.open(action, seconds, &inhibitors),
            Event::Close => {
                self.withdraw();
                self.emit(None);
            }
            Event::PromptEnded { id, confirmed } => {
                if self.shown.as_ref().map(|s| s.id) != Some(id) {
                    return; // A prompt that was replaced or withdrawn.
                }
                let shown = self.shown.take().unwrap();
                let mut child = shown.child;
                let _ = child.wait();
                self.emit(Some(if confirmed {
                    Answer::Confirmed(shown.action)
                } else {
                    Answer::Canceled
                }));
            }
        }
    }

    fn open(&mut self, action: Action, seconds: u32, paths: &[OwnedObjectPath]) {
        let inhibitors = logout_inhibitors(&self.session, paths);
        // gnome-session opens the dialog again when its inhibitors change.
        if let Some(shown) = &mut self.shown {
            if shown.action == action && shown.inhibitors == inhibitors {
                return;
            }
        }
        let deadline = match &self.shown {
            Some(shown) if shown.action == action => shown.deadline,
            _ => None,
        };
        self.withdraw();
        if inhibitors.is_empty() && no_prompt_requested(&self.config.proc_root, self.uid, action) {
            self.emit(Some(Answer::Confirmed(action)));
            return;
        }
        let deadline = match (inhibitors.is_empty(), seconds) {
            (false, _) | (true, 0) => None,
            (true, _) => {
                deadline.or_else(|| Some(Instant::now() + Duration::from_secs(seconds.into())))
            }
        };
        let left = deadline.map_or(seconds, |d| {
            d.saturating_duration_since(Instant::now())
                .as_secs_f64()
                .round() as u32
        });
        let (args, details) = prompt_args(action, &inhibitors, left);
        let spawned = Command::new(&self.config.prompt)
            .args(&args)
            .stdin(if details.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                g_log!(
                    glib::LogLevel::Warning,
                    "Cannot show the end-session prompt {}: {e}",
                    self.config.prompt.display()
                );
                // Let gnome-session go back to the running session rather
                // than wait for an answer that will never come.
                self.emit(Some(Answer::Canceled));
                return;
            }
        };
        if let (Some(details), Some(mut stdin)) = (details, child.stdin.take()) {
            let _ = stdin.write_all(details.as_bytes());
        }
        let id = self.next_id;
        self.next_id += 1;
        // The button prints CONFIRMED; the output ends when the prompt and
        // the button's command have exited.
        let mut stdout = child.stdout.take().unwrap();
        let events = self.events.clone();
        thread::spawn(move || {
            let mut out = String::new();
            let _ = stdout.read_to_string(&mut out);
            let confirmed = out.lines().any(|l| l.trim() == CONFIRMED);
            let _ = events.send(Event::PromptEnded { id, confirmed });
        });
        self.shown = Some(Shown {
            id,
            action,
            inhibitors,
            child,
            deadline,
        });
    }

    /// Take the prompt off the screen, without answering.
    fn withdraw(&mut self) {
        if let Some(mut shown) = self.shown.take() {
            let _ = shown.child.kill();
            let _ = shown.child.wait();
        }
    }

    fn time_out(&mut self) {
        if let Some(action) = self.shown.as_ref().map(|s| s.action) {
            self.withdraw();
            self.emit(Some(Answer::Confirmed(action)));
        }
    }

    /// Tell gnome-session the answer, if any, and that the dialog closed.
    fn emit(&self, answer: Option<Answer>) {
        let result = zbus::block_on(async {
            let ctxt = SignalContext::new(&self.session, PATH)?;
            match answer {
                Some(Answer::Confirmed(Action::Logout)) => {
                    EndSessionDialog::confirmed_logout(&ctxt).await?
                }
                Some(Answer::Confirmed(Action::Shutdown)) => {
                    EndSessionDialog::confirmed_shutdown(&ctxt).await?
                }
                Some(Answer::Confirmed(Action::Restart)) => {
                    EndSessionDialog::confirmed_reboot(&ctxt).await?
                }
                Some(Answer::Canceled) => EndSessionDialog::canceled(&ctxt).await?,
                None => {}
            }
            EndSessionDialog::closed(&ctxt).await
        });
        if let Err(e) = result {
            g_log!(glib::LogLevel::Warning, "Cannot answer gnome-session: {e}");
        }
    }

    fn run(mut self, events: Receiver<Event>) {
        loop {
            let deadline = self.shown.as_ref().and_then(|s| s.deadline);
            let event = match deadline {
                Some(deadline) => {
                    match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(event) => event,
                        Err(RecvTimeoutError::Timeout) => {
                            self.time_out();
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                None => match events.recv() {
                    Ok(event) => event,
                    Err(_) => return,
                },
            };
            self.handle(event);
        }
    }
}

/// The uid this daemon runs as.
fn current_uid() -> u32 {
    fs::metadata("/proc/self").map_or(u32::MAX, |m| m.uid())
}

/// Provide the end-session dialog on `session`. Only returns if that cannot
/// be set up.
pub fn serve(session: &Connection, config: Config) -> zbus::Result<()> {
    let (send, receive) = mpsc::channel();
    zbus::block_on(async {
        let iface = EndSessionDialog {
            events: Mutex::new(send.clone()),
        };
        session.object_server().at(PATH, iface).await?;
        // Nothing else owns this name in the sway session. Should something
        // that does turn up, it takes over.
        session
            .request_name_with_flags(NAME, RequestNameFlags::AllowReplacement.into())
            .await?;
        Ok::<_, zbus::Error>(())
    })?;
    Dialog {
        session: session.clone(),
        config,
        uid: current_uid(),
        events: send,
        shown: None,
        next_id: 0,
    }
    .run(receive);
    Ok(())
}

/// Provide the end-session dialog, and the monitor labels that are asked for
/// under the same name, on the user's session bus for the lifetime of the
/// daemon, from a thread of its own.
pub fn start() {
    thread::spawn(|| {
        let result = zbus::block_on(Connection::session()).and_then(|session| {
            if let Err(e) = monitor_labels::serve(&session) {
                g_log!(
                    glib::LogLevel::Warning,
                    "The monitor labels are unavailable: {e}"
                );
            }
            serve(&session, Config::default())
        });
        if let Err(e) = result {
            g_log!(
                glib::LogLevel::Warning,
                "The end-session dialog is unavailable: {e}"
            );
        }
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use zbus::export::futures_util::StreamExt;

    #[test]
    fn prompts_like_the_keybindings() {
        let (args, details) = prompt_args(Action::Restart, &[], 60);
        assert_eq!(
            args,
            [
                "-t",
                "warning",
                "-m",
                "Do you really want to reboot your system? The system will restart automatically in 60 seconds.",
                "-Z",
                "Reboot",
                "echo confirmed",
            ]
        );
        assert_eq!(details, None);
        assert_eq!(
            prompt_args(Action::Logout, &[], 0).0[3],
            "Do you really want to logout?"
        );
        assert_eq!(prompt_args(Action::Shutdown, &[], 0).0[5], "Shutdown");

        let busy = [
            Inhibitor {
                name: "Text Editor".into(),
                reason: "Unsaved document".into(),
            },
            Inhibitor {
                name: "backup".into(),
                reason: String::new(),
            },
        ];
        let (args, details) = prompt_args(Action::Logout, &busy, 60);
        assert_eq!(
            args[3],
            "Do you really want to logout? Some applications are busy or have unsaved work."
        );
        assert_eq!(args[7..], ["-l", "-L", "Applications"]);
        assert_eq!(
            details.as_deref(),
            Some("Text Editor: Unsaved document\nbackup\n")
        );
    }

    fn fake_process(root: &Path, pid: &str, argv: &[&str]) {
        let dir = root.join(pid);
        fs::create_dir_all(&dir).unwrap();
        let mut cmdline = argv.join("\0");
        cmdline.push('\0');
        fs::write(dir.join("cmdline"), cmdline).unwrap();
    }

    #[test]
    fn finds_requests_without_a_prompt() {
        let root = std::env::temp_dir().join(format!("powerd-quit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fake_process(
            &root,
            "10",
            &["/usr/bin/gnome-session-quit", "--reboot", "--no-prompt"],
        );
        fake_process(&root, "11", &["gnome-session-quit", "--power-off"]);
        fake_process(
            &root,
            "12",
            &["/bin/sh", "-c", "gnome-session-quit --logout --no-prompt"],
        );
        fs::create_dir_all(root.join("self")).unwrap();
        let uid = current_uid();

        assert!(no_prompt_requested(&root, uid, Action::Restart));
        assert!(!no_prompt_requested(&root, uid, Action::Shutdown));
        assert!(!no_prompt_requested(&root, uid, Action::Logout));
        assert!(!no_prompt_requested(
            &root,
            uid.wrapping_add(1),
            Action::Restart
        ));

        fake_process(&root, "13", &["gnome-session-quit", "--force"]);
        fake_process(
            &root,
            "14",
            &["gnome-session-quit", "--power-off", "--no-prompt"],
        );
        assert!(no_prompt_requested(&root, uid, Action::Logout));
        assert!(no_prompt_requested(&root, uid, Action::Shutdown));
        assert!(!no_prompt_requested(
            &root.join("missing"),
            uid,
            Action::Logout
        ));
        fs::remove_dir_all(&root).unwrap();
    }

    pub(crate) struct Bus(Child);

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    pub(crate) fn private_bus() -> (Bus, String) {
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

    /// A gnome-session inhibitor.
    struct FakeInhibitor {
        app_id: &'static str,
        reason: &'static str,
        flags: u32,
    }

    #[dbus_interface(name = "org.gnome.SessionManager.Inhibitor")]
    impl FakeInhibitor {
        fn get_app_id(&self) -> String {
            self.app_id.into()
        }
        fn get_reason(&self) -> String {
            self.reason.into()
        }
        fn get_flags(&self) -> u32 {
            self.flags
        }
    }

    pub(crate) fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A stand-in for swaynag. It records its arguments and details, then
    /// does what the file `mode` next to it says: `confirm` presses the
    /// button (runs its command, as swaynag does), `dismiss` closes the bar,
    /// anything else stays until killed.
    const FAKE_PROMPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
n=$(ls "$dir" | grep -c '^args\.')
printf '%s\n' "$@" > "$dir/args.$n"
for a in "$@"; do [ "$a" = -l ] && cat > "$dir/details.$n"; done
case "$(cat "$dir/mode")" in
confirm)
    while [ $# -gt 0 ]; do
        if [ "$1" = -Z ]; then sh -c "$3"; exit 0; fi
        shift
    done ;;
dismiss) exit 0 ;;
*) i=0; while [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done ;;
esac
"#;

    #[test]
    fn asks_and_answers_gnome_session() {
        let (_bus, address) = private_bus();
        let dir = std::env::temp_dir().join(format!("powerd-esd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let prompt_dir = dir.join("prompt");
        let proc_root = dir.join("proc");
        fs::create_dir_all(&prompt_dir).unwrap();
        fs::create_dir_all(&proc_root).unwrap();
        let prompt = prompt_dir.join("swaynag");
        fs::write(&prompt, FAKE_PROMPT).unwrap();
        fs::set_permissions(&prompt, fs::Permissions::from_mode(0o755)).unwrap();
        let set_mode = |mode: &str| fs::write(prompt_dir.join("mode"), mode).unwrap();
        let prompts = || {
            fs::read_dir(&prompt_dir)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("args."))
                .count()
        };
        let read = |name: String| fs::read_to_string(prompt_dir.join(name)).unwrap_or_default();

        // gnome-session, with one inhibitor that inhibits logging out and one
        // that only keeps the screen from going idle.
        let session_manager = zbus::block_on(async {
            zbus::ConnectionBuilder::address(address.as_str())?
                .name("org.gnome.SessionManager")?
                .serve_at(
                    "/org/gnome/SessionManager/Inhibitor1",
                    FakeInhibitor {
                        app_id: "rpd-test-editor",
                        reason: "Unsaved document",
                        flags: INHIBIT_LOGOUT | 8,
                    },
                )?
                .serve_at(
                    "/org/gnome/SessionManager/Inhibitor2",
                    FakeInhibitor {
                        app_id: "rpd-test-player",
                        reason: "Playing a video",
                        flags: 8,
                    },
                )?
                .build()
                .await
        })
        .unwrap();

        // Something holds the name first; the dialog queues behind it.
        let early = zbus::block_on(async {
            let conn = zbus::ConnectionBuilder::address(address.as_str())?
                .build()
                .await?;
            conn.request_name(NAME).await?;
            Ok::<_, zbus::Error>(conn)
        })
        .unwrap();

        let server_address = address.clone();
        let (server_prompt, server_proc) = (prompt.clone(), proc_root.clone());
        thread::spawn(move || {
            zbus::block_on(async {
                zbus::ConnectionBuilder::address(server_address.as_str())?
                    .build()
                    .await
            })
            .and_then(|conn| {
                serve(
                    &conn,
                    Config {
                        prompt: server_prompt,
                        proc_root: server_proc,
                    },
                )
            })
            .unwrap();
        });

        let dbus = zbus::block_on(fdo::DBusProxy::new(&session_manager)).unwrap();
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

        // gnome-session calls the dialog through the name and follows its
        // signals.
        let dialog = zbus::block_on(zbus::Proxy::new(
            &session_manager,
            NAME,
            PATH,
            "org.gnome.SessionManager.EndSessionDialog",
        ))
        .unwrap();
        let signals = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut stream = zbus::block_on(dialog.receive_all_signals()).unwrap();
        let seen = signals.clone();
        thread::spawn(move || {
            zbus::block_on(async {
                while let Some(msg) = stream.next().await {
                    let member = msg.member().map(|m| m.to_string()).unwrap_or_default();
                    seen.lock().unwrap().push(member);
                }
            })
        });
        let open = |dialog_type: u32, seconds: u32, inhibitors: &[&str]| {
            let paths: Vec<OwnedObjectPath> = inhibitors
                .iter()
                .map(|p| OwnedObjectPath::try_from(*p).unwrap())
                .collect();
            zbus::block_on(dialog.call_method("Open", &(dialog_type, 0u32, seconds, paths)))
                .map(|_| ())
        };
        let expect = |what: &[&str]| {
            wait_until(&format!("{what:?}"), || *signals.lock().unwrap() == what);
            signals.lock().unwrap().clear();
        };

        // The button confirms.
        set_mode("confirm");
        open(2, 60, &["/org/gnome/SessionManager/Inhibitor2"]).unwrap();
        expect(&["ConfirmedReboot", "Closed"]);
        let args = read("args.0".into());
        assert!(args.contains("-Z\nReboot\necho confirmed\n"), "{args}");
        assert!(
            args.contains("restart automatically in 60 seconds"),
            "{args}"
        );

        // Closing the bar cancels.
        set_mode("dismiss");
        open(0, 60, &[]).unwrap();
        expect(&["Canceled", "Closed"]);
        assert!(read("args.1".into()).contains("-Z\nLogout\n"));

        // Unanswered, the action is taken when the time is up, and the bar
        // goes away.
        set_mode("stay");
        open(1, 1, &[]).unwrap();
        expect(&["ConfirmedShutdown", "Closed"]);
        assert_eq!(prompts(), 3);

        // An application that inhibits logging out is listed and stops the
        // timer; gnome-session's Close withdraws the bar.
        open(
            2,
            1,
            &[
                "/org/gnome/SessionManager/Inhibitor1",
                "/org/gnome/SessionManager/Inhibitor2",
            ],
        )
        .unwrap();
        wait_until("the prompt", || prompts() == 4);
        thread::sleep(Duration::from_millis(1500));
        assert!(signals.lock().unwrap().is_empty());
        assert!(read("args.3".into()).contains("Some applications are busy"));
        assert_eq!(
            read("details.3".into()),
            "rpd-test-editor: Unsaved document\n"
        );
        // Opening it again with the same inhibitors keeps the bar.
        open(2, 1, &["/org/gnome/SessionManager/Inhibitor1"]).unwrap();
        thread::sleep(Duration::from_millis(300));
        assert_eq!(prompts(), 4);
        zbus::block_on(dialog.call_method("Close", &())).unwrap();
        expect(&["Closed"]);

        // A request made with `gnome-session-quit --no-prompt` is confirmed
        // without a prompt, unless something inhibits it.
        fake_process(
            &proc_root,
            "42",
            &["/usr/bin/gnome-session-quit", "--reboot", "--no-prompt"],
        );
        open(2, 60, &[]).unwrap();
        expect(&["ConfirmedReboot", "Closed"]);
        assert_eq!(prompts(), 4);
        set_mode("dismiss");
        open(2, 60, &["/org/gnome/SessionManager/Inhibitor1"]).unwrap();
        expect(&["Canceled", "Closed"]);
        assert_eq!(prompts(), 5);

        // gnome-session never asks for the other types.
        assert!(open(4, 60, &[]).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }
}
