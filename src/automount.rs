//! Removable-media automount for the sway session, which gnome-flashback
//! provides in the X11 session.
//!
//! Watches the system's volumes and mounts removable ones when they are
//! plugged in, following `org.gnome.desktop.media-handling`: `automount`
//! turns mounting on, and `automount-open` opens each new mount in the
//! default file manager. Encrypted volumes ask for their passphrase through
//! the GNOME system prompter, one drive at a time; a passphrase the user
//! asks to remember is stored in the keyring by gvfs, which uses it on the
//! next plug.
//!
//! Volumes present when the daemon starts, and volumes the user unmounts,
//! are left alone until they are plugged in again.
//!
//! Volumes are tracked by their `GVolume` object. The volume monitor keeps
//! one per plugged-in volume and updates it in place, so it is the one
//! identity that survives unlocking an encrypted volume (its name, uuid and
//! device all change to the unlocked device's). A replug makes a new one.

use gio::prelude::*;
use gio::{
    AppInfo, AskPasswordFlags, MountMountFlags, MountOperation, MountOperationResult, PasswordSave,
    Settings, SettingsSchemaSource, Volume, VolumeMonitor,
};
use glib::g_log;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::prompter::{Bus, PromptAnswer, PromptError, PromptQueue, PromptRequest};

const MEDIA_SCHEMA: &str = "org.gnome.desktop.media-handling";
const AUTOMOUNT_KEY: &str = "automount";
const AUTOMOUNT_OPEN_KEY: &str = "automount-open";
const REMEMBER_CHOICE: &str = "Remember passphrase";
const WRONG_PASSPHRASE_WARNING: &str = "Wrong passphrase, try again";
/// How long a prompt waits for its turn at the prompter, and then for the
/// user's answer.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);
/// The volume monitor reports the volumes present at startup a little at a
/// time. They count as present at startup until the list has been stable
/// for this long...
const STARTUP_SETTLE: Duration = Duration::from_millis(300);
/// ...or for at most this long.
const STARTUP_SETTLE_MAX: Duration = Duration::from_secs(5);
const STARTUP_POLL: Duration = Duration::from_millis(50);

#[derive(Default)]
struct State {
    /// Whether the volumes present at startup are all known.
    settled: bool,
    /// Volumes present at startup.
    startup: HashSet<Volume>,
    /// Volumes the user unmounted.
    user_unmounted: HashSet<Volume>,
    /// Volumes with a mount in flight.
    mounting: HashSet<Volume>,
    /// Volumes we are mounting or have mounted but not opened yet: only
    /// these are opened with `automount-open`, once.
    ours: HashSet<Volume>,
}

#[derive(Clone)]
pub struct Automount {
    state: Rc<RefCell<State>>,
    settings: Settings,
    monitor: VolumeMonitor,
    prompts: PromptQueue<Volume>,
}

/// Start automounting, unless GNOME's media-handling settings are missing.
/// Returns at once; volumes found within the first moments count as present
/// at startup.
pub fn start() -> Option<Automount> {
    let schema = SettingsSchemaSource::default().and_then(|s| s.lookup(MEDIA_SCHEMA, true));
    if schema.is_none() {
        g_log!(
            glib::LogLevel::Warning,
            "No {MEDIA_SCHEMA} settings: removable media won't be mounted"
        );
        return None;
    }
    let this = Automount {
        state: Rc::default(),
        settings: Settings::new(MEDIA_SCHEMA),
        monitor: VolumeMonitor::get(),
        prompts: PromptQueue::new(Bus::Session, PROMPT_TIMEOUT),
    };
    this.connect();
    this.settle();
    Some(this)
}

impl Automount {
    fn connect(&self) {
        let this = self.clone();
        self.monitor
            .connect_volume_added(move |_, volume| this.on_volume_added(volume));
        let this = self.clone();
        self.monitor
            .connect_volume_removed(move |_, volume| this.on_volume_removed(volume));
        let this = self.clone();
        self.monitor
            .connect_mount_added(move |_, mount| this.on_mount_added(mount));
        let this = self.clone();
        self.monitor
            .connect_mount_removed(move |_, mount| this.on_mount_removed(mount));
    }

    /// Collect the volumes present at startup from the main loop, until the
    /// list stops changing. A volume added meanwhile counts as one of them.
    fn settle(&self) {
        let started = Instant::now();
        let mut last = self.volumes();
        let mut stable_since = started;
        self.state.borrow_mut().startup.extend(last.iter().cloned());
        let this = self.clone();
        glib::timeout_add_local(STARTUP_POLL, move || {
            let current = this.volumes();
            if current != last {
                last = current.clone();
                stable_since = Instant::now();
            }
            let mut state = this.state.borrow_mut();
            state.startup.extend(current);
            if stable_since.elapsed() >= STARTUP_SETTLE || started.elapsed() >= STARTUP_SETTLE_MAX {
                state.settled = true;
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    fn volumes(&self) -> HashSet<Volume> {
        self.monitor.volumes().into_iter().collect()
    }

    fn automount_on(&self) -> bool {
        self.settings.boolean(AUTOMOUNT_KEY)
    }

    fn present(&self, volume: &Volume) -> bool {
        self.monitor.volumes().contains(volume)
    }

    fn on_volume_added(&self, volume: &Volume) {
        {
            let mut state = self.state.borrow_mut();
            if !state.settled {
                state.startup.insert(volume.clone());
                return;
            }
            if state.startup.contains(volume)
                || state.user_unmounted.contains(volume)
                || state.mounting.contains(volume)
            {
                return;
            }
        }
        let removable = volume
            .drive()
            .map(|drive| drive.is_removable())
            .unwrap_or(false);
        if self.automount_on()
            && removable
            && volume.should_automount()
            && volume.can_mount()
            && volume.get_mount().is_none()
        {
            self.mount(volume, None);
        }
    }

    fn on_volume_removed(&self, volume: &Volume) {
        {
            let mut state = self.state.borrow_mut();
            state.startup.remove(volume);
            state.user_unmounted.remove(volume);
            state.mounting.remove(volume);
            state.ours.remove(volume);
        }
        // Unplugging closes the drive's passphrase prompt.
        self.prompts.cancel(volume);
    }

    fn on_mount_added(&self, mount: &gio::Mount) {
        let Some(volume) = mount.volume() else {
            return;
        };
        // Taken, not just checked: a later mount of the same volume, by
        // hand, does not open it again.
        if !self.state.borrow_mut().ours.remove(&volume)
            || !self.settings.boolean(AUTOMOUNT_OPEN_KEY)
        {
            return;
        }
        let uri = mount.root().uri();
        AppInfo::launch_default_for_uri_async(
            &uri.clone(),
            None::<&gio::AppLaunchContext>,
            None::<&gio::Cancellable>,
            move |result| {
                if let Err(e) = result {
                    g_log!(glib::LogLevel::Warning, "Failed to open {uri}: {e}");
                }
            },
        );
    }

    fn on_mount_removed(&self, mount: &gio::Mount) {
        let Some(volume) = mount.volume() else {
            return;
        };
        // Unplugged volumes are gone already; one still here was unmounted.
        if self.present(&volume) {
            let mut state = self.state.borrow_mut();
            state.ours.remove(&volume);
            state.user_unmounted.insert(volume);
        }
    }

    /// Mount `volume`. `warning` goes on the passphrase prompt, after a
    /// wrong passphrase.
    fn mount(&self, volume: &Volume, warning: Option<&'static str>) {
        {
            let mut state = self.state.borrow_mut();
            state.mounting.insert(volume.clone());
            // Before the call: mount-added can arrive before its reply.
            state.ours.insert(volume.clone());
        }
        // Whether a passphrase was asked for, whether to ask again, and
        // whether gvfs is done with the operation (so it takes no reply).
        let asked = Rc::new(Cell::new(false));
        let retry = Rc::new(Cell::new(true));
        let done = Rc::new(Cell::new(false));

        let operation = MountOperation::new();
        {
            let this = self.clone();
            let volume = volume.clone();
            let (asked, retry, done) = (asked.clone(), retry.clone(), done.clone());
            operation.connect_ask_password(move |operation, message, _, _, flags| {
                // The default handler answers "unhandled" from an idle
                // callback, before any prompt is answered, which fails the
                // mount. Every ask is answered below.
                glib::signal_stop_emission_by_name(operation, "ask-password");
                asked.set(true);
                if !this.automount_on() {
                    retry.set(false);
                    operation.reply(MountOperationResult::Aborted);
                    return;
                }
                let operation = operation.clone();
                let (retry, done) = (retry.clone(), done.clone());
                this.prompts.ask(
                    volume.clone(),
                    prompt_request(&volume, message, flags, warning),
                    move |answer| {
                        if !done.get() {
                            answer_operation(&operation, &retry, answer);
                        }
                    },
                );
            });
        }
        {
            // gvfs gives up on the operation, as when the drive goes away.
            let this = self.clone();
            let volume = volume.clone();
            let done = done.clone();
            operation.connect_aborted(move |_| {
                done.set(true);
                this.prompts.cancel(&volume);
            });
        }

        let this = self.clone();
        let target = volume.clone();
        volume.mount(
            MountMountFlags::NONE,
            Some(&operation),
            None::<&gio::Cancellable>,
            move |result| {
                done.set(true);
                this.state.borrow_mut().mounting.remove(&target);
                let Err(e) = result else {
                    this.prompts.release(&target);
                    return;
                };
                this.state.borrow_mut().ours.remove(&target);
                // Not worth a warning: a cancelled prompt or an unplugged drive.
                if !e.matches(gio::IOErrorEnum::FailedHandled)
                    && !e.matches(gio::IOErrorEnum::Cancelled)
                {
                    g_log!(glib::LogLevel::Warning, "Mount failed: {}", e.message());
                }
                // A wrong passphrase asks again, unless the user cancelled,
                // the drive is gone or automount was turned off. The drive
                // keeps its turn at the prompt.
                if asked.get() && retry.get() && this.automount_on() && this.present(&target) {
                    this.mount(&target, Some(WRONG_PASSPHRASE_WARNING));
                } else {
                    // Also closes a prompt the failed mount left behind.
                    this.prompts.cancel(&target);
                }
            },
        );
    }
}

/// The prompt for gvfs's passphrase request. gvfs's (translated) message
/// is a line to show in bold and a line naming the drive.
fn prompt_request(
    volume: &Volume,
    message: &str,
    flags: AskPasswordFlags,
    warning: Option<&str>,
) -> PromptRequest {
    let name = volume.name();
    let (message, description) = match message.split_once('\n') {
        Some((first, rest)) => (first.to_string(), rest.to_string()),
        None if !message.is_empty() => (message.to_string(), String::new()),
        None => (
            format!("Enter passphrase to unlock \"{name}\""),
            String::new(),
        ),
    };
    PromptRequest {
        title: format!("Unlock {name}"),
        message,
        description,
        choice_label: flags
            .contains(AskPasswordFlags::SAVING_SUPPORTED)
            .then(|| REMEMBER_CHOICE.to_string()),
        warning: warning.map(str::to_string),
    }
}

/// Answer gvfs's passphrase request.
fn answer_operation(
    operation: &MountOperation,
    retry: &Cell<bool>,
    answer: Result<PromptAnswer, PromptError>,
) {
    match answer {
        Ok(PromptAnswer {
            password: Some(password),
            choice_chosen,
        }) => {
            operation.set_password(Some(&password));
            operation.set_password_save(if choice_chosen {
                PasswordSave::Permanently
            } else {
                PasswordSave::Never
            });
            operation.reply(MountOperationResult::Handled);
        }
        Ok(PromptAnswer { password: None, .. }) | Err(PromptError::Closed) => {
            retry.set(false);
            operation.reply(MountOperationResult::Aborted);
        }
        Err(e) => {
            g_log!(glib::LogLevel::Warning, "Passphrase prompt failed: {e}");
            retry.set(false);
            operation.reply(MountOperationResult::Aborted);
        }
    }
}
