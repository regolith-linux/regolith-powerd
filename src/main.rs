use gio::{prelude::*, Application, ApplicationFlags};
use glib::g_log;
use gsettings_macro::gen_settings;
use std::{
    cell::Cell,
    error::Error,
    fs,
    mem::ManuallyDrop,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command},
    rc::Rc,
    time::{Duration, Instant, SystemTime},
};
use swayipc;
use zbus::export::futures_util::future;

mod battery;
mod end_session;
mod logind;
mod monitor_labels;
mod power_source;
mod profiles;
mod screensaver;
mod session;

// Removable-media automount and its passphrase prompts.
mod automount;
mod prompter;
mod secret_exchange;

/// What decides the idle actions besides the settings: the power source
/// (UPower's `OnBattery`). gnome-settings-daemon re-arms its idle timers
/// when it changes. Whether a GNOME application inhibits suspend is checked
/// when the sleep-inactive action is due (see [`SUSPEND_NOT_INHIBITED`]);
/// the session inhibition bridge has the timers re-armed when that changes
/// on an unlocked screen (see [`crate::session`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdleContext {
    pub on_battery: bool,
}

/// Shared idle context plus a way to ask the manager to re-arm the timers.
#[derive(Clone)]
pub struct IdleState {
    context: Rc<Cell<IdleContext>>,
    reload: async_channel::Sender<()>,
    /// Restart swayidle at the next reload even if its arguments stay the same.
    restart: Rc<Cell<bool>>,
    /// Undo what swayidle's timeouts did, at the next reload.
    wake: Rc<Cell<bool>>,
    /// When a suspend inhibitor went away on a locked screen, for the next
    /// reload to run an overdue sleep-inactive action (see
    /// [`run_overdue_sleep`]).
    overdue_sleep: Rc<Cell<Option<Instant>>>,
    /// When an idle inhibitor went away on a locked screen, for the next
    /// reload to count the sleep-inactive action from then (see
    /// [`sleep_from`]).
    idle_dropped: Rc<Cell<Option<Instant>>>,
    /// The latest such moment the sleep-inactive action counts from: an
    /// earlier count gives way to it.
    sleep_counts_from: Rc<Cell<Option<Instant>>>,
    /// One message per startup step done (see [`STARTUP_STEPS`]).
    startup_steps: async_channel::Receiver<()>,
    startup_steps_tx: async_channel::Sender<()>,
}

impl IdleState {
    fn new(reload: async_channel::Sender<()>) -> Self {
        let (startup_steps_tx, startup_steps) = async_channel::bounded(STARTUP_STEPS);
        Self {
            context: Rc::new(Cell::new(IdleContext::default())),
            reload,
            restart: Rc::new(Cell::new(false)),
            wake: Rc::new(Cell::new(false)),
            overdue_sleep: Rc::new(Cell::new(None)),
            idle_dropped: Rc::new(Cell::new(None)),
            sleep_counts_from: Rc::new(Cell::new(None)),
            startup_steps,
            startup_steps_tx,
        }
    }

    /// Report one of the [`STARTUP_STEPS`] done (or given up). Never blocks.
    pub fn startup_step_done(&self) {
        let _ = self.startup_steps_tx.try_send(());
    }

    /// Ask for swayidle to be restarted, even if its arguments stay the same:
    /// its idle countdowns start over, and stopping it runs the resume command
    /// of every timeout that has fired (see [`stop_swayidle`]), which
    /// brightens a dimmed screen and turns the outputs back on. Never blocks.
    pub fn request_restart(&self) {
        self.restart.set(true);
        self.request_reload();
    }

    /// Ask for what swayidle's timeouts did to be undone, without restarting
    /// it (see [`wake_cmd`]). Never blocks.
    pub fn request_wake(&self) {
        self.wake.set(true);
        self.request_reload();
    }

    /// Ask for the sleep-inactive action to run if it was already overdue
    /// when a suspend inhibitor went away on a locked screen at `since`,
    /// without restarting swayidle (see [`run_overdue_sleep`]). Never blocks.
    pub fn request_overdue_sleep(&self, since: Instant) {
        self.overdue_sleep.set(Some(since));
        self.request_reload();
    }

    /// Ask for the sleep-inactive action to count from `since`, when an idle
    /// inhibitor went away on a locked screen then, without restarting
    /// swayidle (see [`sleep_from`]). Never blocks.
    pub fn request_sleep_from(&self, since: Instant) {
        self.idle_dropped.set(Some(since));
        self.request_reload();
    }

    pub fn get(&self) -> IdleContext {
        self.context.get()
    }

    /// Ask for a reload; never blocks, see [`request_reload`].
    pub fn request_reload(&self) {
        request_reload(&self.reload);
    }

    /// Change the context, and ask for a reload if it changed.
    pub fn update(&self, f: impl FnOnce(&mut IdleContext)) {
        let mut ctx = self.context.get();
        f(&mut ctx);
        if ctx != self.context.get() {
            self.context.set(ctx);
            self.request_reload();
        }
    }
}

struct Manager {
    power_settings: PowerSettings,
    session_settings: Rc<SessionSettings>,
    screensaver_settings: ScreensaverSettings,
    lockdown_settings: LockdownSettings,
    idle: IdleState,
    recv_reload_event: async_channel::Receiver<()>,
}

#[gen_settings(
    file = "org.gnome.settings-daemon.plugins.power.gschema.xml",
    id = "org.gnome.settings-daemon.plugins.power"
)]
pub struct PowerSettings;

#[gen_settings(
    file = "org.gnome.desktop.session.gschema.xml",
    id = "org.gnome.desktop.session"
)]
pub struct SessionSettings;

#[gen_settings(
    file = "org.gnome.desktop.screensaver.gschema.xml",
    id = "org.gnome.desktop.screensaver"
)]
pub struct ScreensaverSettings;

#[gen_settings(
    file = "org.gnome.desktop.lockdown.gschema.xml",
    id = "org.gnome.desktop.lockdown"
)]
pub struct LockdownSettings;

// Added by Ubuntu's gsettings-desktop-schemas; upstream GNOME has no such key.
// It is left out of the schema file above on purpose: gen_settings generates
// accessors from that file at build time, but they read the schema installed
// at run time, and GSettings aborts on a key that schema lacks. So this key is
// read through the underlying gio::Settings after checking it exists.
const LOCK_ON_SUSPEND_KEY: &str = "ubuntu-lock-on-suspend";

impl Manager {
    pub fn new(session_settings: Rc<SessionSettings>) -> Self {
        let (send_reload_event, recv_reload_event) = async_channel::bounded(1);
        Self {
            session_settings,
            power_settings: PowerSettings::new(),
            screensaver_settings: ScreensaverSettings::new(),
            lockdown_settings: LockdownSettings::new(),
            idle: IdleState::new(send_reload_event),
            recv_reload_event,
        }
    }

    pub fn idle_state(&self) -> IdleState {
        self.idle.clone()
    }

    pub fn run(self) -> Result<(), Box<dyn Error>> {
        let send_reload_event = self.idle.reload.clone();
        let recv_reload_event = self.recv_reload_event.clone();

        // Send initial Reload
        request_reload(&send_reload_event);
        self.power_settings.handle_power_btn_action_change()?;
        rebind_power_button_after_reloads();

        let send_reload_cb = || {
            let tx_cpy = send_reload_event.clone();
            return move |_: &PowerSettings| request_reload(&tx_cpy);
        };

        self.power_settings
            .connect_idle_brightness_changed(send_reload_cb());
        self.power_settings
            .connect_idle_dim_changed(send_reload_cb());
        self.power_settings
            .connect_sleep_inactive_ac_timeout_changed(send_reload_cb());
        self.power_settings
            .connect_sleep_inactive_ac_type_changed(send_reload_cb());
        self.power_settings
            .connect_sleep_inactive_battery_timeout_changed(send_reload_cb());
        self.power_settings
            .connect_sleep_inactive_battery_type_changed(send_reload_cb());
        self.power_settings
            .connect_power_button_action_changed(|s| {
                s.handle_power_btn_action_change()
                    .expect("Failed to set Power button action")
            });
        let send_screensaver_reload_cb = || {
            let tx_cpy = send_reload_event.clone();
            return move |_: &ScreensaverSettings| request_reload(&tx_cpy);
        };
        self.screensaver_settings
            .connect_lock_enabled_changed(send_screensaver_reload_cb());
        self.screensaver_settings
            .connect_lock_delay_changed(send_screensaver_reload_cb());
        let tx_cpy = send_reload_event.clone();
        self.screensaver_settings
            .connect_changed(Some(LOCK_ON_SUSPEND_KEY), move |_, _| {
                request_reload(&tx_cpy)
            });
        let tx_cpy = send_reload_event.clone();
        self.lockdown_settings
            .connect_disable_lock_screen_changed(move |_| request_reload(&tx_cpy));
        self.session_settings
            .connect_idle_delay_changed(move |_| request_reload(&send_reload_event));

        let mut sway_idle_child: Option<Child> = None;
        let mut current_args: Option<Vec<String>> = None;
        glib::spawn_future_local(async move {
            // Marks left by an earlier daemon's swayidle tell nothing about
            // this one's idle time.
            for mark in [IDLE_MARK, SLEEP_DUE_MARK, SLEEP_HELD_UNTIL] {
                if let Some(path) = runtime_path(mark) {
                    let _ = fs::remove_file(path);
                }
            }
            self.idle.startup().await;
            while let Ok(_) = recv_reload_event.recv().await {
                // Restarting swayidle starts its idle count again, so keep
                // the running one when nothing it runs has changed (for
                // example a power-source change with the same action),
                // unless a restart was asked for.
                if self.idle.wake.take() {
                    run_shell(&wake_cmd(), "turn the screen on").await;
                }
                // First, so that an overdue action (below) finds the hold.
                if let Some(since) = self.idle.idle_dropped.take() {
                    self.idle.sleep_counts_from.set(Some(since));
                    if let Some((timeout, action)) = self.sleep_from_cmd() {
                        let (due, hold) = hold_sleep(since, timeout);
                        let counts_from = self.idle.sleep_counts_from.clone();
                        glib::spawn_future_local(async move {
                            sleep_from(since, due, hold, &action, &counts_from).await
                        });
                    }
                }
                if let Some(since) = self.idle.overdue_sleep.take() {
                    if let Some((_, action, _)) = self.sleep_cmds() {
                        glib::spawn_future_local(
                            async move { run_overdue_sleep(since, &action).await },
                        );
                    }
                }
                let restart = self.idle.restart.take();
                let running = sway_idle_child
                    .as_mut()
                    .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
                if running && !restart && current_args.as_ref() == Some(&self.get_swayidle_args()) {
                    continue;
                }
                if let Some(mut prev_child) = sway_idle_child.take() {
                    stop_swayidle(&mut prev_child).await;
                }
                let swayidle_args = self.get_swayidle_args();
                current_args = Some(swayidle_args.clone());
                let mut command = Command::new("swayidle");
                command.arg("-w").args(swayidle_args);
                // SAFETY: the closure only makes system calls, as is
                // allowed between fork and exec.
                unsafe {
                    command.pre_exec(|| {
                        close_inherited_fds_on_exec();
                        Ok(())
                    });
                }
                sway_idle_child = command.spawn().ok();
                g_log!(glib::LogLevel::Info, "Swayidle Reloaded");
            }
        });

        Ok(())
    }

    fn get_swayidle_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        let display_off = DISPLAY_OFF.to_owned();
        let display_on = DISPLAY_ON.to_owned();

        let lock_screen = Self::get_lock_cmd(LOCKER, LOCK_WAIT);

        let times = idle_times(
            self.session_settings.idle_delay(),
            self.power_settings.idle_dim(),
        );

        // While a GNOME application inhibits idleness, the screen doesn't
        // dim, turn off or lock on the usual idle times. swayidle holds them
        // back itself through logind's idle lock while the screen is unlocked;
        // on a locked screen, where only the locked-screen blanking goes on,
        // they leave themselves out (as does the sleep-inactive action, see
        // [`Self::sleep_cmds`]). In the background nothing runs at all.
        let usual_idle = [SESSION_IN_FOREGROUND, IDLE_NOT_INHIBITED];

        if let Some(dim_timeout_sec) = times.dim {
            let idle_brightness = self.power_settings.idle_brightness();
            let action = format!(
                r#"
                export curr_brightness=$(light)
                echo $curr_brightness | tee {DIM_SAVED_BRIGHTNESS}
                if [ 1 -eq "$(echo "${{curr_brightness}} > {idle_brightness}" | bc)" ]; then
                    light -S {idle_brightness}
                fi"#,
            );

            // Dim screen to $idle_brightness when idle dim timeout hit, and
            // restore the brightness saved then (once: a dim left out above
            // saves none)
            let mut swayidle_dim_timeout_args =
                Self::get_timeout_cmd(dim_timeout_sec, &only_if(&usual_idle, action.trim()));
            let mut swayidle_dim_resume_args = Self::get_resume_cmd(&restore_dim_cmd());
            args.append(&mut swayidle_dim_timeout_args);
            args.append(&mut swayidle_dim_resume_args);
        }

        let locks = lock_triggers(
            self.screensaver_settings.lock_enabled(),
            self.lock_on_suspend(),
            self.lockdown_settings.disable_lock_screen(),
        );

        if let Some(display_off_timeout_sec) = times.blank {
            let mut swayidle_display_off_timeout_args =
                Self::get_timeout_cmd(display_off_timeout_sec, &only_if(&usual_idle, &display_off));
            let mut swayidle_display_off_resume_args = Self::get_resume_cmd(&display_on);
            args.append(&mut swayidle_display_off_timeout_args);
            args.append(&mut swayidle_display_off_resume_args);

            // Locked lock-delay seconds after display off
            if locks.idle {
                let lock_delay_sec = self.screensaver_settings.lock_delay();
                let lock_timeout_sec = display_off_timeout_sec.saturating_add(lock_delay_sec);
                let mut swayidle_lock_timeout_args =
                    Self::get_timeout_cmd(lock_timeout_sec, &only_if(&usual_idle, &lock_screen));
                args.append(&mut swayidle_lock_timeout_args);
            }
        }

        // While the screen is locked, turn it off soon, whatever idle-delay
        // is set to
        args.append(&mut blank_while_locked_args());

        // Whether the user has been idle since a lock, for the fresh-lock
        // blank (see [`blank_fresh_lock`])
        args.append(&mut idle_mark_args());

        if let Some((timeout, action, resume)) = self.sleep_cmds() {
            // The mark tells [`run_overdue_sleep`] that the timeout has
            // passed with no input since.
            let mut swayidle_timeout_args =
                Self::get_timeout_cmd(timeout, &format!("touch {SLEEP_DUE_MARK}; {action}"));
            args.append(&mut swayidle_timeout_args);

            let resume = match resume {
                Some(resume_action) => format!("rm -f {SLEEP_DUE_MARK}; {resume_action}"),
                None => format!("rm -f {SLEEP_DUE_MARK}"),
            };
            let mut swayidle_resume_args = Self::get_resume_cmd(&resume);
            args.append(&mut swayidle_resume_args);
        }

        let pause_audio = format!("playerctl -a pause");
        let before_sleep = if locks.before_sleep {
            format!("{display_off};{lock_screen};{pause_audio};sleep 1")
        } else {
            format!("{display_off};{pause_audio};sleep 1")
        };
        let after_resume = display_on.clone();
        let mut before_sleep_args = vec!["before-sleep".to_owned(), before_sleep];
        let mut after_resum_args = vec!["after-resume".to_owned(), after_resume];
        args.append(&mut before_sleep_args);
        args.append(&mut after_resum_args);
        if locks.on_request {
            let mut lock_screen_args = vec!["lock".to_owned(), lock_screen];
            args.append(&mut lock_screen_args);
        }
        args
    }

    /// The sleep-inactive action swayidle arms, as (timeout, guarded command,
    /// resume command), see [`Self::sleep_timeout`].
    ///
    /// It is held back while a GNOME application inhibits idleness, locked or
    /// not: in the X11 session the idle monitor stops counting idle time
    /// then, so gnome-settings-daemon's sleep timer never fires. Unlocked,
    /// logind's idle lock holds it back too; on a locked screen only the test
    /// here does. When the inhibitor goes away on a locked screen, the action
    /// counts from then (see [`sleep_from`]).
    ///
    /// A suspend or hibernate action is held back while a GNOME application
    /// inhibits suspend: that inhibition holds back automatic suspend only,
    /// so logind's lock for it is weak and explicit requests (power button,
    /// keybinding) still go through. The tests are made when the action is
    /// due, so that a change of these inhibitors needs no new swayidle: on a
    /// locked screen a new one would start the lock screen's blank delays
    /// over, and stopping the old one would turn the outputs back on.
    fn sleep_cmds(&self) -> Option<(u32, String, Option<&'static str>)> {
        let sleep = Self::sleep_timeout(
            self.idle.get(),
            self.power_settings.sleep_inactive_ac_type(),
            self.power_settings.sleep_inactive_ac_timeout(),
            self.power_settings.sleep_inactive_battery_type(),
            self.power_settings.sleep_inactive_battery_timeout(),
        )?;
        Some((sleep.timeout, guarded_sleep_cmd(sleep.action), sleep.resume))
    }

    /// The sleep-inactive action as [`sleep_from`] runs it, as (timeout,
    /// guarded command): the hold it set itself (see [`SLEEP_NOT_HELD`]) is
    /// left out.
    fn sleep_from_cmd(&self) -> Option<(u32, String)> {
        let sleep = Self::sleep_timeout(
            self.idle.get(),
            self.power_settings.sleep_inactive_ac_type(),
            self.power_settings.sleep_inactive_ac_timeout(),
            self.power_settings.sleep_inactive_battery_type(),
            self.power_settings.sleep_inactive_battery_timeout(),
        )?;
        Some((
            sleep.timeout,
            only_if(&sleep_guards(sleep.action, false), sleep.action),
        ))
    }

    /// Whether to lock the screen before the system sleeps. Keeps locking
    /// when the installed screensaver schema lacks the Ubuntu key.
    fn lock_on_suspend(&self) -> bool {
        let has_key = self
            .screensaver_settings
            .settings_schema()
            .is_some_and(|schema| schema.has_key(LOCK_ON_SUSPEND_KEY));
        !has_key || self.screensaver_settings.boolean(LOCK_ON_SUSPEND_KEY)
    }

    /// The sleep-inactive timeout swayidle arms. Like gnome-settings-daemon,
    /// only the one of the power source in use is armed; the manager re-arms
    /// the timers when the source changes. None when there is nothing to
    /// arm: the type is nothing (or interactive), or the timeout is 0, which
    /// means never. What holds the action back is checked when it is due
    /// (see [`Self::sleep_cmds`]).
    fn sleep_timeout(
        ctx: IdleContext,
        ac_type: SleepInactiveAcType,
        ac_timeout: i32,
        battery_type: SleepInactiveBatteryType,
        battery_timeout: i32,
    ) -> Option<SleepTimeout> {
        let ((action, resume), timeout) = if ctx.on_battery {
            (Self::battery_sleep_cmds(battery_type), battery_timeout)
        } else {
            (Self::ac_sleep_cmds(ac_type), ac_timeout)
        };
        Some(SleepTimeout {
            timeout: u32::try_from(timeout).ok().filter(|t| *t > 0)?,
            action: action?,
            resume,
        })
    }

    /// Commands for the sleep-inactive type on AC: the one to run when the
    /// timeout is hit and, if any, the one to run when the user comes back.
    fn ac_sleep_cmds(action: SleepInactiveAcType) -> (Option<&'static str>, Option<&'static str>) {
        use SleepInactiveAcType::*;
        match action {
            Suspend => (Some("systemctl suspend"), None),
            Hibernate => (Some("systemctl hibernate"), None),
            Blank => (Some(DISPLAY_OFF), Some(DISPLAY_ON)),
            Shutdown => (Some("poweroff"), None),
            Logout => (Some("gnome-session-quit --no-prompt"), None),
            // TODO: Show some gui prompt to the user; until then, do nothing
            Interactive => (None, None),
            _ => (None, None),
        }
    }

    /// Commands for the sleep-inactive type on battery, as [`Self::ac_sleep_cmds`].
    fn battery_sleep_cmds(
        action: SleepInactiveBatteryType,
    ) -> (Option<&'static str>, Option<&'static str>) {
        use SleepInactiveBatteryType::*;
        match action {
            Suspend => (Some("systemctl suspend"), None),
            Hibernate => (Some("systemctl hibernate"), None),
            Blank => (Some(DISPLAY_OFF), Some(DISPLAY_ON)),
            Shutdown => (Some("poweroff"), None),
            Logout => (Some("gnome-session-quit --no-prompt"), None),
            // TODO: Show some gui prompt to the user; until then, do nothing
            Interactive => (None, None),
            _ => (None, None),
        }
    }

    fn get_timeout_cmd(timeout: u32, action: &str) -> Vec<String> {
        vec![
            "timeout".to_owned(),
            timeout.to_string(),
            action.to_string(),
        ]
    }

    fn get_resume_cmd(action: &str) -> Vec<String> {
        vec!["resume".to_owned(), action.to_owned()]
    }

    /// Shell command that starts `locker` and waits until it exits, but for
    /// at most `wait` (checked every 100 milliseconds).
    ///
    /// swayidle runs with `-w`, so it waits for every command before it
    /// handles anything else. `gtklock -d` exits as soon as the session is
    /// locked, but a locker that does not daemonize (plain `gtklock`, or
    /// `swaylock` without `-f`, set through `wm.program.lock_sway`) keeps
    /// running until the user unlocks. Waiting for it would stall swayidle
    /// for the whole time the session is locked: the resume and after-resume
    /// commands would not run, so the screen would stay off and dimmed behind
    /// the lock screen.
    ///
    /// The command is a single line, so it can be joined with other commands
    /// by `;`, as in the before-sleep command.
    fn get_lock_cmd(locker: &str, wait: Duration) -> String {
        let checks = (wait.as_millis() + 99) / 100;
        format!(
            "{locker} & lock_pid=$!; i=0; \
             while [ $i -lt {checks} ] && kill -0 $lock_pid 2>/dev/null; do \
             sleep 0.1; i=$((i + 1)); done"
        )
    }
}

/// What is done before the first swayidle starts: taking logind's power-key
/// and lid lock, and bringing the session's inhibitor locks in line.
///
/// swayidle (1.9.0) sets itself up with blocking D-Bus calls, and answers
/// some signals with them; a signal that arrives during one waits until the
/// next one comes in, and holds that one back in turn. A change of logind's
/// `BlockInhibited` while swayidle sets itself up could so make it miss the
/// next `PrepareForSleep` or `Lock`: with stay awake on at login, the screen
/// was then not locked and turned off before the sleep. So at startup the
/// locks are taken before swayidle starts, and swayidle finds them in place.
const STARTUP_STEPS: usize = 2;

/// How long the first swayidle waits for the [`STARTUP_STEPS`] at most.
const STARTUP_WAIT: Duration = Duration::from_secs(2);

impl IdleState {
    /// Wait until the [`STARTUP_STEPS`] are done, or for [`STARTUP_WAIT`].
    async fn startup(&self) {
        let steps = async {
            for _ in 0..STARTUP_STEPS {
                if self.startup_steps.recv().await.is_err() {
                    break;
                }
            }
        };
        let steps = std::pin::pin!(steps);
        let _ = future::select(steps, glib::timeout_future(STARTUP_WAIT)).await;
    }
}

/// Ask [`Manager::run`] to restart swayidle with the current settings.
///
/// Never blocks. The receiver runs on this same main context, so waiting
/// for room in the channel would wait forever: GSettings emits one `changed`
/// signal per key from a single dispatch, and a change of two keys at once
/// (`gsettings reset-recursively`, `dconf load`) used to hang the daemon on
/// the second one. A reload already queued covers this one, as swayidle's
/// arguments are read when the reload runs.
fn request_reload(reload: &async_channel::Sender<()>) {
    let _ = reload.try_send(());
}

#[cfg(test)]
mod reload_tests {
    use super::request_reload;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn reload_requests_never_block_and_coalesce() {
        let (send, recv) = async_channel::bounded(1);
        // Nothing reads the channel meanwhile, as when the requests come
        // from signal handlers on the receiver's own main context.
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            for _ in 0..3 {
                request_reload(&send);
            }
            done.send(send).unwrap();
        });
        let send = finished
            .recv_timeout(Duration::from_secs(5))
            .expect("a reload request blocked");
        // One reload is queued; it covers all three requests.
        assert_eq!(recv.try_recv(), Ok(()));
        assert!(recv.try_recv().is_err());
        // Once it has been taken, the next request queues a new one.
        request_reload(&send);
        assert_eq!(recv.try_recv(), Ok(()));
    }
}

/// Turn every output off and on again. swayidle runs its commands through
/// `sh -c` in powerd's working directory (the home directory), so the `*`
/// has to be quoted, or the shell replaces it with the file names there.
const DISPLAY_OFF: &str = "swaymsg output '*' dpms off";
const DISPLAY_ON: &str = "swaymsg output '*' dpms on";

/// Where the idle dim keeps the brightness to restore, until it is restored.
const DIM_SAVED_BRIGHTNESS: &str = "$XDG_RUNTIME_DIR/screen_brightness_old.var";

/// Shell command that restores the brightness saved by the idle dim, once:
/// with no dim since the last restore (or a dim left out), there is none.
fn restore_dim_cmd() -> String {
    format!(
        "if [ -e {DIM_SAVED_BRIGHTNESS} ]; then \
         light -S $(cat {DIM_SAVED_BRIGHTNESS}); rm -f {DIM_SAVED_BRIGHTNESS}; fi"
    )
}

/// Shell command that undoes what swayidle's timeouts did: it restores the
/// brightness from before a dim and turns the outputs on, as
/// gnome-settings-daemon does when it goes back to its normal idle mode.
///
/// Run when an idle inhibitor holds idleness back again, say after an
/// unlock. swayidle then lets go of its timeouts without running their
/// resume commands, and won't see the input that would have run them; so
/// the idle marks go too. Restarting it instead would run them, but would
/// leave a moment in which no swayidle follows logind, and a sleep then
/// would go without the before-sleep commands.
fn wake_cmd() -> String {
    format!(
        "{}; rm -f \"{LOCKED_BLANK_MARK}\" {IDLE_MARK} {SLEEP_DUE_MARK} \"{SLEEP_HELD_UNTIL}\"; \
         {DISPLAY_ON}",
        restore_dim_cmd()
    )
}

/// Run the shell command `cmd` and reap it, waiting for at most 5 seconds.
/// `what` it does goes into the warning if it cannot start.
async fn run_shell(cmd: &str, what: &str) {
    let mut child = match Command::new("sh").arg("-c").arg(cmd).spawn() {
        Ok(child) => child,
        Err(e) => {
            g_log!(glib::LogLevel::Warning, "Cannot {what}: {e}");
            return;
        }
    };
    for _ in 0..50 {
        match child.try_wait() {
            Ok(None) => glib::timeout_future(Duration::from_millis(100)).await,
            _ => return,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Shell command that prints the session manager's property `$name` as
/// busctl does (`b true`, `u 8`), waiting for an answer for 2 seconds at
/// most; nothing without one.
macro_rules! session_manager_property {
    ($name:literal) => {
        concat!(
            "timeout 2 busctl --user get-property org.gnome.SessionManager ",
            "/org/gnome/SessionManager org.gnome.SessionManager ",
            $name,
            " 2>/dev/null"
        )
    };
}

/// Shell test: no GNOME application inhibits the actions of bit `$bit` of
/// the session manager's `InhibitedActions`. Without a session manager to ask,
/// or an answer in time, none does.
macro_rules! not_inhibited {
    ($bit:literal) => {
        concat!(
            "{ a=$(",
            session_manager_property!("InhibitedActions"),
            ") && a=${a#u } && [ \"$a\" -eq \"$a\" ] 2>/dev/null || a=0; ",
            "[ $((a & ",
            $bit,
            ")) -eq 0 ]; }"
        )
    };
}

/// Shell test: `SessionIsActive` does not read false. No session manager to
/// ask, or no answer within 2 seconds, counts as active.
const SESSION_IN_FOREGROUND: &str = concat!(
    "[ \"$(",
    session_manager_property!("SessionIsActive"),
    ")\" != \"b false\" ]"
);

/// Shell test: no GNOME application inhibits idleness (the idle bit, 8).
const IDLE_NOT_INHIBITED: &str = not_inhibited!(8);

/// Shell test: no GNOME application inhibits suspend (the suspend bit, 4).
const SUSPEND_NOT_INHIBITED: &str = not_inhibited!(4);

/// What holds the sleep-inactive `action` back, as shell tests: a session
/// in the background, an idle inhibitor (locked or not), with `held` the
/// hold [`sleep_from`] sets, and for suspend and hibernate a suspend
/// inhibitor.
fn sleep_guards(action: &str, held: bool) -> Vec<&'static str> {
    let mut tests = vec![SESSION_IN_FOREGROUND, IDLE_NOT_INHIBITED];
    if held {
        tests.push(SLEEP_NOT_HELD);
    }
    if matches!(action, "systemctl suspend" | "systemctl hibernate") {
        tests.push(SUSPEND_NOT_INHIBITED);
    }
    tests
}

/// The sleep-inactive `action` as swayidle runs it, with what holds it back
/// (see [`sleep_guards`]) checked first. A single line.
fn guarded_sleep_cmd(action: &str) -> String {
    only_if(&sleep_guards(action, true), action)
}

/// [`SLEEP_HELD_UNTIL`], for `concat!`.
macro_rules! sleep_held_until {
    () => {
        "$XDG_RUNTIME_DIR/regolith-powerd-sleep-held-until"
    };
}

/// Holds the sleep-inactive action back until the time in it (whole
/// seconds since the epoch), see [`sleep_from`].
const SLEEP_HELD_UNTIL: &str = sleep_held_until!();

/// Shell test: [`SLEEP_HELD_UNTIL`] holds no time still to come.
const SLEEP_NOT_HELD: &str = concat!(
    "! { h=$(cat \"",
    sleep_held_until!(),
    "\" 2>/dev/null) && [ \"$h\" -eq \"$h\" ] 2>/dev/null ",
    "&& [ \"$(date +%s)\" -lt \"$h\" ]; }"
);

/// Shell command that runs `cmd` only if every one of the shell `tests` holds.
/// It is a single line if `cmd` is.
fn only_if(tests: &[&str], cmd: &str) -> String {
    format!("if {}; then {cmd}; fi", tests.join(" && "))
}

#[cfg(test)]
mod session_guard_tests {
    use super::{only_if, sleep_guards, IDLE_NOT_INHIBITED, SESSION_IN_FOREGROUND, SLEEP_NOT_HELD};
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};

    /// Run `only_if(tests, "echo ran")` through `sh -c`, as swayidle does,
    /// with a stand-in busctl that answers for `org.gnome.SessionManager`
    /// with `active` and `actions` (None: the session manager is not on the
    /// bus), and `XDG_RUNTIME_DIR` holding the sleep hold `held_until` (if
    /// any). Returns whether the command ran.
    fn runs(
        name: &str,
        tests: &[&str],
        active: bool,
        actions: Option<&str>,
        held_until: Option<&str>,
    ) -> bool {
        let dir = std::env::temp_dir().join(format!(
            "regolith-powerd-test-{}-guard-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let busctl = match actions {
            None => "#!/bin/sh\necho 'Failed to get property' >&2\nexit 1\n".to_string(),
            Some(actions) => format!(
                "#!/bin/sh\n\
                 [ \"$1 $2 $3 $4 $5\" = \"--user get-property org.gnome.SessionManager /org/gnome/SessionManager org.gnome.SessionManager\" ] || exit 2\n\
                 case $6 in SessionIsActive) echo 'b {active}';; InhibitedActions) echo '{actions}';; *) exit 2;; esac\n"
            ),
        };
        fs::write(dir.join("busctl"), busctl).unwrap();
        fs::set_permissions(dir.join("busctl"), fs::Permissions::from_mode(0o755)).unwrap();
        if let Some(held_until) = held_until {
            fs::write(dir.join("regolith-powerd-sleep-held-until"), held_until).unwrap();
        }
        let output = Command::new("sh")
            .arg("-c")
            .arg(only_if(tests, "echo ran"))
            .env("PATH", format!("{}:/usr/bin:/bin", dir.display()))
            .env("XDG_RUNTIME_DIR", &dir)
            .output()
            .unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stdout).trim() == "ran"
    }

    #[test]
    fn session_tests_ask_the_session_manager() {
        let usual = [SESSION_IN_FOREGROUND, IDLE_NOT_INHIBITED];
        let foreground = [SESSION_IN_FOREGROUND];
        let sleep = sleep_guards("poweroff", false);
        let suspend = sleep_guards("systemctl suspend", false);
        // (tests, session active, InhibitedActions, runs)
        type Case<'a> = (&'a [&'a str], bool, Option<&'a str>, bool);
        let cases: [Case; 20] = [
            (&usual, true, Some("u 0"), true),
            (&usual, true, Some("u 4"), true),
            // logging out is not idleness
            (&usual, true, Some("u 1"), true),
            (&usual, true, Some("u 8"), false),
            (&usual, true, Some("u 12"), false),
            (&usual, false, Some("u 0"), false),
            // Nothing to ask: as without inhibitors, in the foreground
            (&usual, true, None, true),
            (&usual, true, Some("garbage"), true),
            (&foreground, true, Some("u 8"), true),
            (&foreground, false, Some("u 0"), false),
            (&foreground, false, None, true),
            // The sleep-inactive action: held back by an idle inhibitor
            (&sleep, true, Some("u 0"), true),
            (&sleep, true, Some("u 4"), true),
            (&sleep, true, Some("u 8"), false),
            (&sleep, false, Some("u 0"), false),
            // Suspend and hibernate: also held back by a suspend inhibitor
            (&suspend, true, Some("u 0"), true),
            (&suspend, true, Some("u 4"), false),
            (&suspend, true, Some("u 8"), false),
            (&suspend, true, Some("u 12"), false),
            (&suspend, true, None, true),
        ];
        for (i, (tests, active, actions, ran)) in cases.into_iter().enumerate() {
            assert_eq!(
                runs(&i.to_string(), tests, active, actions, None),
                ran,
                "case {i}: active {active}, InhibitedActions {actions:?}"
            );
        }
    }

    #[test]
    fn the_sleep_hold_holds_until_its_time() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let held = |name: &str, until: Option<String>| {
            !runs(name, &[SLEEP_NOT_HELD], true, Some("u 0"), until.as_deref())
        };
        assert!(held("future", Some(format!("{}\n", now + 60))));
        assert!(!held("past", Some(format!("{}\n", now - 1))));
        assert!(!held("none", None));
        assert!(!held("garbage", Some("soon\n".into())));
        // swayidle runs the action guarded with the hold; the daemon's own
        // count from the inhibitor's drop leaves its hold out.
        assert!(sleep_guards("poweroff", true).contains(&SLEEP_NOT_HELD));
        assert!(!sleep_guards("poweroff", false).contains(&SLEEP_NOT_HELD));
    }

    #[test]
    fn wake_undoes_the_dim_and_the_blanks() {
        use super::wake_cmd;
        let dir =
            std::env::temp_dir().join(format!("regolith-powerd-test-{}-wake", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        for tool in ["light", "swaymsg"] {
            fs::write(
                bin.join(tool),
                format!("#!/bin/sh\necho {tool} \"$*\" >> \"$XDG_RUNTIME_DIR/calls\"\n"),
            )
            .unwrap();
            fs::set_permissions(bin.join(tool), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let run = || {
            let _ = fs::remove_file(dir.join("calls"));
            let status = Command::new("sh")
                .arg("-c")
                .arg(wake_cmd())
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .env("XDG_RUNTIME_DIR", &dir)
                .status()
                .unwrap();
            assert!(status.success());
            fs::read_to_string(dir.join("calls")).unwrap()
        };
        // Dimmed and blanked on the locked screen: both undone, once.
        fs::write(dir.join("screen_brightness_old.var"), "70\n").unwrap();
        fs::write(dir.join("regolith-powerd-locked-blank"), "").unwrap();
        fs::write(dir.join("regolith-powerd-idle"), "").unwrap();
        fs::write(dir.join("regolith-powerd-sleep-due"), "").unwrap();
        fs::write(dir.join("regolith-powerd-sleep-held-until"), "1\n").unwrap();
        assert_eq!(run(), "light -S 70\nswaymsg output * dpms on\n");
        assert!(!dir.join("screen_brightness_old.var").exists());
        assert!(!dir.join("regolith-powerd-locked-blank").exists());
        assert!(!dir.join("regolith-powerd-idle").exists());
        assert!(!dir.join("regolith-powerd-sleep-due").exists());
        assert!(!dir.join("regolith-powerd-sleep-held-until").exists());
        // Nothing to restore: the outputs just go on.
        assert_eq!(run(), "swaymsg output * dpms on\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn guarded_commands_stay_single_lines() {
        let cmd = only_if(&[SESSION_IN_FOREGROUND, IDLE_NOT_INHIBITED], "true");
        assert!(!cmd.contains('\n'));
        let status = Command::new("sh")
            .args(["-n", "-c", &format!("true;{cmd};true")])
            .status()
            .unwrap();
        assert!(status.success(), "{cmd}");
    }
}

#[cfg(test)]
mod display_cmd_tests {
    use super::{Manager, SleepInactiveAcType, SleepInactiveBatteryType, DISPLAY_OFF, DISPLAY_ON};
    use std::{fs, process::Command};

    /// Run `cmd` the way swayidle does (`sh -c`), from a directory that
    /// holds files, as the home directory does, with `swaymsg` replaced by a
    /// shell function that prints its arguments. Returns what it printed.
    fn swaymsg_args(name: &str, cmd: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "regolith-powerd-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Desktop")).unwrap();
        fs::create_dir_all(dir.join("Documents")).unwrap();
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("swaymsg() {{ echo \"$@\"; }}\n{cmd}"))
            .current_dir(&dir)
            .output()
            .unwrap();
        fs::remove_dir_all(&dir).unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn blank_turns_every_output_off_and_on() {
        let cmds = [
            Manager::ac_sleep_cmds(SleepInactiveAcType::Blank),
            Manager::battery_sleep_cmds(SleepInactiveBatteryType::Blank),
        ];
        for (off, on) in cmds {
            assert_eq!(swaymsg_args("blank", off.unwrap()), "output * dpms off");
            assert_eq!(swaymsg_args("blank", on.unwrap()), "output * dpms on");
        }
    }

    #[test]
    fn display_off_and_on_turn_every_output_off_and_on() {
        assert_eq!(swaymsg_args("display", DISPLAY_OFF), "output * dpms off");
        assert_eq!(swaymsg_args("display", DISPLAY_ON), "output * dpms on");
    }
}

/// Idle seconds after which the outputs turn off while the screen is locked
/// (gnome-settings-daemon's `SCREENSAVER_TIMEOUT_BLANK`).
const LOCKED_BLANK_SECS: u32 = 30;

/// Idle seconds after which the outputs turn off when the screen was locked
/// after the user's last input (the lock key, or the lid).
pub(crate) const JUST_LOCKED_BLANK_SECS: u32 = 5;

/// Present while the user has been idle for [`JUST_LOCKED_BLANK_SECS`] or
/// more (see [`idle_mark_args`]).
const IDLE_MARK: &str = "$XDG_RUNTIME_DIR/regolith-powerd-idle";

/// Present while the sleep-inactive timeout has passed with no input since.
const SLEEP_DUE_MARK: &str = "$XDG_RUNTIME_DIR/regolith-powerd-sleep-due";

/// The file a `$XDG_RUNTIME_DIR/...` path of the shell commands names here.
fn runtime_path(shell_path: &str) -> Option<PathBuf> {
    let name = shell_path.strip_prefix("$XDG_RUNTIME_DIR/")?;
    Some(PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join(name))
}

/// Whether the mark `shell_path` is present and was made no later than
/// `at`: the user has been idle since then at least.
fn marked_since(shell_path: &str, at: Instant) -> bool {
    let made = runtime_path(shell_path)
        .and_then(|path| fs::metadata(path).ok())
        .and_then(|meta| meta.modified().ok());
    let at = SystemTime::now().checked_sub(at.elapsed());
    matches!((made, at), (Some(made), Some(at)) if made <= at)
}

/// The swayidle timeout that keeps [`IDLE_MARK`] while the user is idle.
fn idle_mark_args() -> Vec<String> {
    let mut args = Manager::get_timeout_cmd(JUST_LOCKED_BLANK_SECS, &format!("touch {IDLE_MARK}"));
    args.append(&mut Manager::get_resume_cmd(&format!("rm -f {IDLE_MARK}")));
    args
}

/// Turn the outputs off on a screen locked at `locked_since` while an idle
/// inhibitor is held, [`JUST_LOCKED_BLANK_SECS`] after the lock, when the
/// user has not come back since: the session gathers no idle time while
/// idleness is held back, so the lock screen starts from the lock.
///
/// swayidle does this itself when its idle count starts at the lock (see
/// [`blank_while_locked_args`]). It cannot when the user had been idle for a
/// while before the lock and the stay-awake request only came afterwards:
/// its short timeout has passed by then, and it only has one idle count,
/// which the sleep-inactive action has to keep.
pub(crate) async fn blank_fresh_lock(locked_since: Instant) {
    let since_lock = locked_since + Duration::from_secs(JUST_LOCKED_BLANK_SECS.into());
    let blanked = runtime_path(LOCKED_BLANK_MARK).is_some_and(|path| path.exists());
    if !blanked && marked_since(IDLE_MARK, since_lock) {
        run_shell(&blank_while_locked_cmd(None), "turn the screen off").await;
    }
}

/// Run the sleep-inactive `action` now, when a suspend inhibitor went away
/// on a locked screen at `since` and the timeout had passed already with no
/// input since: the action was held back when it was due, and swayidle
/// won't run it again before the next input. When the timeout has not
/// passed yet, swayidle runs it when it does, with nothing holding it back
/// any more, so the action keeps the deadline it had.
///
/// Restarting swayidle for the change instead, as on an unlocked screen,
/// would turn the outputs on and start the lock screen's delays over.
async fn run_overdue_sleep(since: Instant, action: &str) {
    if marked_since(SLEEP_DUE_MARK, since) {
        run_shell(action, "run the sleep-inactive action").await;
    }
}

/// Hold the sleep-inactive action back until `timeout` seconds after
/// `since`, when an idle inhibitor went away on a locked screen then (see
/// [`sleep_from`]). Returns that deadline and the hold written to
/// [`SLEEP_HELD_UNTIL`]: the deadline's whole second.
fn hold_sleep(since: Instant, timeout: u32) -> (SystemTime, SystemTime) {
    let at = SystemTime::now()
        .checked_sub(since.elapsed())
        .unwrap_or_else(SystemTime::now);
    let due = at + Duration::from_secs(timeout.into());
    let secs = due
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Some(path) = runtime_path(SLEEP_HELD_UNTIL) {
        if let Err(e) = fs::write(&path, format!("{secs}\n")) {
            g_log!(
                glib::LogLevel::Warning,
                "Cannot hold the sleep-inactive action back: {e}"
            );
        }
    }
    (due, SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Run the sleep-inactive `action` (guarded, see [`Manager::sleep_from_cmd`])
/// at `due`, `timeout` seconds after an idle inhibitor went away on a locked
/// screen at `since`, as the X11 session counts idle time from that moment:
/// unless the user came back meanwhile, or a later such moment took over in
/// `counts_from`.
///
/// swayidle keeps its own count from the last input. Its timeout may have
/// passed already (the action was held back then) or may come before `due`;
/// [`SLEEP_HELD_UNTIL`] holds the action back until `hold`, `due`'s whole
/// second. A mark from a timeout that swayidle held back, made before
/// `hold` and not removed by input since, means no input since `since`: the
/// action runs here. A timeout after `hold` (after input since `since`, or
/// within the last second before `due`) runs it in swayidle.
async fn sleep_from(
    since: Instant,
    due: SystemTime,
    hold: SystemTime,
    action: &str,
    counts_from: &Cell<Option<Instant>>,
) {
    glib::timeout_future(due.duration_since(SystemTime::now()).unwrap_or_default()).await;
    let held_back = runtime_path(SLEEP_DUE_MARK)
        .and_then(|path| fs::metadata(path).ok())
        .and_then(|meta| meta.modified().ok())
        .is_some_and(|made| made < hold);
    if counts_from.get() == Some(since) && held_back {
        run_shell(action, "run the sleep-inactive action").await;
    }
}

/// Marks that the outputs were turned off by [`blank_while_locked_args`], so
/// that only then does coming back turn them on.
const LOCKED_BLANK_MARK: &str = "$XDG_RUNTIME_DIR/regolith-powerd-locked-blank";

/// swayidle timeouts that turn the outputs off while the screen is locked,
/// as gnome-settings-daemon does in the X11 session: soon after the screen
/// is locked ([`JUST_LOCKED_BLANK_SECS`] of idle time, if it was locked
/// after the last input), and after [`LOCKED_BLANK_SECS`] of idle time when
/// the user woke the lock screen and left it again.
///
/// The lock key and the lid close start the lock screen directly, not
/// through swayidle, and the idle timeouts count from the last key press.
/// So after Super+Escape the outputs stayed on until idle-delay + 5 seconds
/// (more than 5 minutes by default), and never with Screen Blank set to
/// Never.
///
/// Whether and since when the screen is locked comes from
/// `org.gnome.ScreenSaver`, which this daemon provides (see
/// [`screensaver`]). When the outputs were not turned off here, coming back
/// leaves them alone, so it doesn't turn on an output that something else
/// turned off.
fn blank_while_locked_args() -> Vec<String> {
    let mut args = Vec::new();
    for (timeout, locked_for_at_most) in [
        // Locked for no longer than the user has been idle
        (JUST_LOCKED_BLANK_SECS, Some(JUST_LOCKED_BLANK_SECS)),
        (LOCKED_BLANK_SECS, None),
    ] {
        args.append(&mut Manager::get_timeout_cmd(
            timeout,
            &blank_while_locked_cmd(locked_for_at_most),
        ));
        args.append(&mut Manager::get_resume_cmd(&format!(
            "if [ -e \"{LOCKED_BLANK_MARK}\" ]; then rm -f \"{LOCKED_BLANK_MARK}\"; {DISPLAY_ON}; fi"
        )));
    }
    args
}

/// Shell command that turns the outputs off if the screen is locked and,
/// with `locked_for_at_most`, has been locked for no more seconds than that.
/// A session in the background leaves them alone.
fn blank_while_locked_cmd(locked_for_at_most: Option<u32>) -> String {
    const CALL: &str =
        "busctl --user call org.gnome.ScreenSaver /org/gnome/ScreenSaver org.gnome.ScreenSaver";
    let recently = match locked_for_at_most {
        Some(secs) => {
            format!(" && t=$({CALL} GetActiveTime 2>/dev/null) && [ \"${{t#u }}\" -le {secs} ]")
        }
        None => String::new(),
    };
    format!(
        "if {SESSION_IN_FOREGROUND} && [ \"$({CALL} GetActive 2>/dev/null)\" = \"b true\" ]{recently}; then \
         {DISPLAY_OFF} && touch \"{LOCKED_BLANK_MARK}\"; fi"
    )
}

#[cfg(test)]
mod blank_while_locked_tests {
    use super::{blank_while_locked_args, JUST_LOCKED_BLANK_SECS, LOCKED_BLANK_SECS};
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};

    /// The swayidle arguments as (timeout, command, resume command).
    fn timeouts() -> Vec<(u32, String, String)> {
        let args = blank_while_locked_args();
        args.chunks(5)
            .map(|c| {
                assert_eq!((c[0].as_str(), c[3].as_str()), ("timeout", "resume"));
                (c[1].parse().unwrap(), c[2].clone(), c[4].clone())
            })
            .collect()
    }

    /// Run `cmd` through `sh -c`, as swayidle does, with a stand-in busctl
    /// that answers as `org.gnome.ScreenSaver` does when `locked_for` is the
    /// number of seconds the screen has been locked (None: not locked, or
    /// with `no_service`, not on the bus), and a stand-in swaymsg. Returns
    /// the swaymsg calls.
    fn run(
        dir: &std::path::Path,
        cmd: &str,
        locked_for: Option<u32>,
        no_service: bool,
    ) -> Vec<String> {
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let busctl = if no_service {
            "#!/bin/sh\necho 'Call failed: The name is not activatable' >&2\nexit 1\n".to_string()
        } else {
            let (active, time) = match locked_for {
                Some(t) => ("true", t),
                None => ("false", 0),
            };
            format!(
                "#!/bin/sh\n\
                 [ \"$1 $2 $3 $4 $5\" = \"--user call org.gnome.ScreenSaver /org/gnome/ScreenSaver org.gnome.ScreenSaver\" ] || exit 2\n\
                 case $6 in GetActive) echo 'b {active}';; GetActiveTime) echo 'u {time}';; *) exit 2;; esac\n"
            )
        };
        fs::write(bin.join("busctl"), busctl).unwrap();
        fs::write(
            bin.join("swaymsg"),
            "#!/bin/sh\necho \"$*\" >> \"$XDG_RUNTIME_DIR/swaymsg.log\"\n",
        )
        .unwrap();
        for f in ["busctl", "swaymsg"] {
            fs::set_permissions(bin.join(f), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let log = dir.join("swaymsg.log");
        let _ = fs::remove_file(&log);
        let status = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("XDG_RUNTIME_DIR", dir)
            .status()
            .unwrap();
        assert!(status.success(), "{cmd}");
        fs::read_to_string(&log)
            .map(|l| l.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    #[test]
    fn outputs_turn_off_while_locked_and_on_when_the_user_comes_back() {
        let dir = std::env::temp_dir().join(format!(
            "regolith-powerd-test-{}-locked-blank",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let timeouts = timeouts();
        let secs: Vec<u32> = timeouts.iter().map(|t| t.0).collect();
        assert_eq!(secs, [JUST_LOCKED_BLANK_SECS, LOCKED_BLANK_SECS]);
        let (just_locked, locked) = (&timeouts[0], &timeouts[1]);
        let off = vec!["output * dpms off".to_string()];
        let on = vec!["output * dpms on".to_string()];
        let none: Vec<String> = Vec::new();

        // (timeout, locked for, outputs turned off)
        let cases = [
            (just_locked, None, false),
            // Locked by the last input: the lock key, or the lid
            (just_locked, Some(0), true),
            (just_locked, Some(JUST_LOCKED_BLANK_SECS), true),
            // The user woke the lock screen and left it again
            (just_locked, Some(JUST_LOCKED_BLANK_SECS + 1), false),
            (just_locked, Some(600), false),
            (locked, None, false),
            (locked, Some(0), true),
            (locked, Some(600), true),
        ];
        for ((secs, cmd, resume), locked_for, blanks) in cases {
            let what = format!("{secs} s idle, locked for {locked_for:?}");
            assert_eq!(
                run(&dir, cmd, locked_for, false),
                if blanks { off.clone() } else { none.clone() },
                "{what}"
            );
            // Coming back turns the outputs on only if they were turned off
            // here, and only once.
            assert_eq!(
                run(&dir, resume, locked_for, false),
                if blanks { on.clone() } else { none.clone() },
                "{what}, back"
            );
            assert_eq!(
                run(&dir, resume, locked_for, false),
                none,
                "{what}, back again"
            );
        }

        // Without org.gnome.ScreenSaver on the bus nothing happens.
        for (_, cmd, resume) in [just_locked, locked] {
            assert_eq!(run(&dir, cmd, None, true), none);
            assert_eq!(run(&dir, resume, None, true), none);
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}

/// How long a lock command may take before swayidle carries on (the same
/// limit as regolith-sway-clamshell's lock before sleep).
const LOCK_WAIT: Duration = Duration::from_millis(5000);

/// When swayidle locks the screen.
#[derive(Debug, PartialEq)]
struct LockTriggers {
    /// lock-delay seconds after the screen turns off when idle
    idle: bool,
    /// before the system sleeps
    before_sleep: bool,
    /// on logind's `Lock` signal (`loginctl lock-session`, or `Lock` on
    /// `org.gnome.ScreenSaver`)
    on_request: bool,
}

/// When to lock, from the screensaver settings `lock-enabled` and
/// `ubuntu-lock-on-suspend` and the lockdown setting `disable-lock-screen`.
///
/// `disable-lock-screen` keeps the screen from being locked at all, as
/// gnome-flashback and GNOME Shell do: no lock when idle, before sleep or on
/// request. The screen still turns off. `lock-enabled` only turns off the
/// idle lock.
fn lock_triggers(
    lock_enabled: bool,
    lock_on_suspend: bool,
    lock_screen_disabled: bool,
) -> LockTriggers {
    LockTriggers {
        idle: lock_enabled && !lock_screen_disabled,
        before_sleep: lock_on_suspend && !lock_screen_disabled,
        on_request: !lock_screen_disabled,
    }
}

#[cfg(test)]
mod lock_trigger_tests {
    use super::{lock_triggers, LockTriggers};

    #[test]
    fn lock_triggers_follow_the_settings() {
        // (lock-enabled, ubuntu-lock-on-suspend, disable-lock-screen)
        //     -> (idle, before sleep, on request)
        let cases = [
            ((true, true, false), (true, true, true)),
            ((false, true, false), (false, true, true)),
            ((true, false, false), (true, false, true)),
            ((false, false, false), (false, false, true)),
            // disable-lock-screen wins over the other two
            ((true, true, true), (false, false, false)),
            ((false, false, true), (false, false, false)),
        ];
        for ((enabled, on_suspend, disabled), (idle, before_sleep, on_request)) in cases {
            assert_eq!(
                lock_triggers(enabled, on_suspend, disabled),
                LockTriggers {
                    idle,
                    before_sleep,
                    on_request
                },
                "lock-enabled {enabled}, lock-on-suspend {on_suspend}, disabled {disabled}"
            );
        }
    }
}

/// When the screen dims and when it turns off, in seconds of idle time.
#[derive(Debug, PartialEq)]
struct IdleTimes {
    dim: Option<u32>,
    blank: Option<u32>,
}

/// The idle times for `idle-delay` (Screen Blank) and `idle-dim` (Dim
/// Screen), as gnome-settings-daemon's power plugin sets them in the X11
/// session (`idle_configure` and the constants in gsd-power-constants.h):
///
/// - The screen turns off when the session becomes idle, after idle-delay
///   seconds. 0 (Never) turns blanking off.
/// - With Dim Screen on, it dims at half the idle delay, or not at all when
///   that is under 10 seconds. With Screen Blank set to Never it dims after
///   60 seconds.
///
/// The idle lock follows the blank, lock-delay seconds later.
fn idle_times(idle_delay: u32, idle_dim: bool) -> IdleTimes {
    let blank = (idle_delay > 0).then_some(idle_delay);
    let dim = match (idle_dim, idle_delay) {
        (false, _) => None,
        // IDLE_DIM_BLANK_DISABLED_MIN
        (true, 0) => Some(60),
        // IDLE_DELAY_TO_IDLE_DIM_MULTIPLIER and MINIMUM_IDLE_DIM_DELAY
        (true, delay) => Some(delay / 2).filter(|&dim| dim >= 10),
    };
    IdleTimes { dim, blank }
}

#[cfg(test)]
mod idle_times_tests {
    use super::{idle_times, IdleTimes};

    #[test]
    fn dim_and_blank_follow_gnome() {
        // (idle-delay, idle-dim) -> (dim, blank)
        let cases = [
            ((300, true), (Some(150), Some(300))),
            ((300, false), (None, Some(300))),
            ((60, true), (Some(30), Some(60))),
            ((21, true), (Some(10), Some(21))),
            // Half the delay is under 10 s: no dim
            ((19, true), (None, Some(19))),
            ((1, true), (None, Some(1))),
            // Screen Blank Never
            ((0, true), (Some(60), None)),
            ((0, false), (None, None)),
        ];
        for ((delay, dim_on), (dim, blank)) in cases {
            assert_eq!(
                idle_times(delay, dim_on),
                IdleTimes { dim, blank },
                "idle-delay {delay}, idle-dim {dim_on}"
            );
        }
    }
}

/// Shell command that starts the lock screen: `wm.program.lock_sway` when it
/// is set, else `regolith-sway-gtklock -d` when it is installed, else
/// `gtklock -d` showing the lock-screen wallpaper
/// (`regolith.lockscreen.wallpaper.file`).
///
/// `regolith-sway-gtklock` (from regolith-wm-config's regolith-sway-gtklock
/// package) is what the lock key and the lid close script run, so idle,
/// suspend and logind locks show the same lock screen: the look's wallpaper
/// or colours, and its gtklock stylesheet. powerd doesn't depend on that
/// package, so without it gtklock is started directly. The wallpaper path
/// then goes to gtklock as a single argument, so it may contain spaces, and
/// a leading `~` is expanded to the home directory. When the resource is
/// unset or the file doesn't exist, gtklock still starts, just without the
/// image. (A path with a space used to reach gtklock as several arguments,
/// and gtklock then crashed, leaving the session unlocked.)
///
/// The group runs in the background in [`Manager::get_lock_cmd`], and each
/// branch `exec`s the locker (`regolith-sway-gtklock` `exec`s gtklock in
/// turn), so the pid that is waited for is the locker's. It is a single
/// line, so it can be joined with other commands by `;`.
const LOCKER: &str = concat!(
    "{ locker=$(trawlcat wm.program.lock_sway ''); ",
    "if [ -n \"$locker\" ]; then exec $locker; fi; ",
    "if command -v regolith-sway-gtklock >/dev/null; then ",
    "exec regolith-sway-gtklock -d; fi; ",
    "wallpaper=$(trawlcat regolith.lockscreen.wallpaper.file ''); ",
    "case $wallpaper in '~/'*) wallpaper=$HOME/${wallpaper#'~/'};; esac; ",
    "if [ -n \"$wallpaper\" ] && [ -f \"$wallpaper\" ]; then ",
    "exec gtklock -d --background \"$wallpaper\"; fi; ",
    "exec gtklock -d; }"
);

/// Let a child started by [`Manager::run`] keep only its standard streams.
///
/// swayidle is started again whenever the idle settings change, at any
/// time, while other threads receive file descriptors that are not
/// close-on-exec, such as the logind inhibitor locks zbus hands over. A
/// swayidle that inherits one holds the lock after the daemon released it
/// or exited, which keeps the power key and the lid away from logind. Runs
/// in the child, between fork and exec, so it only makes system calls.
fn close_inherited_fds_on_exec() {
    const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
    // SAFETY: close_range(2) and fcntl(2) only change fd flags.
    unsafe {
        if libc::syscall(
            libc::SYS_close_range,
            3,
            libc::c_uint::MAX,
            CLOSE_RANGE_CLOEXEC,
        ) != 0
        {
            // Kernels before 5.11
            for fd in 3..1024 {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
        }
    }
}

/// Stop a swayidle started by [`Manager::run`] and reap it.
///
/// On SIGTERM swayidle runs the resume command of every timeout that has
/// fired, which restores the brightness and turns the outputs back on, and
/// then exits. SIGKILL skips that: a reload while the screen was dimmed or
/// off left it that way, and the next dim then saved the dimmed brightness
/// as the one to restore. If swayidle has not exited after 5 seconds it is
/// killed.
async fn stop_swayidle(child: &mut Child) {
    // SAFETY: kill(2) has no memory-safety requirements. The child has not
    // been reaped yet, so its pid cannot have been reused.
    if unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) } != 0 {
        println!("Cannot stop swayidle: {}", std::io::Error::last_os_error());
    }
    for _ in 0..50 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => glib::timeout_future(std::time::Duration::from_millis(100)).await,
            Err(e) => {
                println!("Cannot wait for swayidle: {e}");
                break;
            }
        }
    }
    child
        .kill()
        .unwrap_or_else(|e| println!("Cannot kill swayidle: {e}"));
    let _ = child.wait();
}

/// A sleep-inactive timeout for swayidle: after `timeout` seconds idle run
/// `action`, and `resume` when the user comes back.
#[derive(Debug, PartialEq)]
struct SleepTimeout {
    timeout: u32,
    action: &'static str,
    resume: Option<&'static str>,
}

#[cfg(test)]
mod idle_tests {
    use super::{
        guarded_sleep_cmd, IdleContext, IdleState, Manager, SleepInactiveAcType as Ac,
        SleepInactiveBatteryType as Battery, SleepTimeout, DISPLAY_OFF, DISPLAY_ON,
        SUSPEND_NOT_INHIBITED,
    };

    const ON_AC: IdleContext = IdleContext { on_battery: false };
    const ON_BATTERY: IdleContext = IdleContext { on_battery: true };

    #[test]
    fn only_the_sleep_action_of_the_power_source_in_use_is_armed() {
        let suspend = |timeout| SleepTimeout {
            timeout,
            action: "systemctl suspend",
            resume: None,
        };
        let blank = |timeout| SleepTimeout {
            timeout,
            action: DISPLAY_OFF,
            resume: Some(DISPLAY_ON),
        };
        assert_eq!(
            Manager::sleep_timeout(ON_AC, Ac::Suspend, 1200, Battery::Blank, 300),
            Some(suspend(1200))
        );
        assert_eq!(
            Manager::sleep_timeout(ON_BATTERY, Ac::Suspend, 1200, Battery::Blank, 300),
            Some(blank(300))
        );
        // Nothing on the source in use: the other source's action is not
        // armed either.
        assert_eq!(
            Manager::sleep_timeout(ON_AC, Ac::Nothing, 1200, Battery::Suspend, 30),
            None
        );
        assert_eq!(
            Manager::sleep_timeout(ON_BATTERY, Ac::Suspend, 1200, Battery::Nothing, 30),
            None
        );
        // A timeout of 0 means never.
        assert_eq!(
            Manager::sleep_timeout(ON_AC, Ac::Suspend, 0, Battery::Suspend, 30),
            None
        );
        assert_eq!(
            Manager::sleep_timeout(ON_BATTERY, Ac::Suspend, 1200, Battery::Suspend, 0),
            None
        );
    }

    #[test]
    fn idle_suspend_and_hibernate_are_held_back_while_suspend_is_inhibited() {
        // Checked when the action is due: the timeout stays armed.
        for ctx in [ON_AC, ON_BATTERY] {
            for (ac, battery) in [
                (Ac::Suspend, Battery::Suspend),
                (Ac::Hibernate, Battery::Hibernate),
            ] {
                let sleep = Manager::sleep_timeout(ctx, ac, 60, battery, 60).unwrap();
                assert!(guarded_sleep_cmd(sleep.action).contains(SUSPEND_NOT_INHIBITED));
            }
        }
        // The other actions go on.
        for action in [DISPLAY_OFF, "poweroff", "gnome-session-quit --no-prompt"] {
            let cmd = guarded_sleep_cmd(action);
            assert!(!cmd.contains(SUSPEND_NOT_INHIBITED), "{cmd}");
            assert!(cmd.ends_with(&format!("then {action}; fi")), "{cmd}");
        }
    }

    #[test]
    fn idle_state_asks_for_a_reload_only_when_the_context_changes() {
        let (send, recv) = async_channel::bounded(1);
        let idle = IdleState::new(send);
        assert_eq!(idle.get(), ON_AC);
        // Battery updates that leave the source as it is don't re-arm.
        idle.update(|c| c.on_battery = false);
        assert!(recv.try_recv().is_err());
        idle.update(|c| c.on_battery = true);
        assert_eq!(idle.get(), ON_BATTERY);
        assert_eq!(recv.try_recv(), Ok(()));
        // A second change while a reload is queued neither blocks nor
        // queues another: the queued one reads the latest context.
        idle.update(|c| c.on_battery = false);
        idle.update(|c| c.on_battery = true);
        assert_eq!(recv.try_recv(), Ok(()));
        assert!(recv.try_recv().is_err());
    }
}

#[derive(Debug)]
enum KeySymAction {
    Unbind { key: String },
    ReBind { key: String, action: String },
}

const POWER_OFF_KEY: &str = "XF86Poweroff";

/// Bind the power button again after every sway reload.
///
/// The binding is added over sway IPC, and `swaymsg reload` (Super+Shift+C,
/// and `regolith-look` after every look change) rebuilds the bindings from
/// the config files, which drops it. powerd keeps logind's power key
/// inhibitor, so the button then did nothing at all.
///
/// The reload events are read in a thread of their own, as the sway IPC
/// client blocks; the binding is set on the main context, where the
/// settings live.
fn rebind_power_button_after_reloads() {
    let (send_reload, recv_reload) = async_channel::unbounded();
    std::thread::spawn(move || {
        let result = swayipc::Connection::new().and_then(|conn| {
            watch_sway_reloads(conn, || {
                let _ = send_reload.try_send(());
            })
        });
        if let Err(e) = result {
            g_log!(
                glib::LogLevel::Warning,
                "Stopped following sway reloads: {e}"
            );
        }
    });
    glib::spawn_future_local(async move {
        let power_settings = PowerSettings::new();
        while recv_reload.recv().await.is_ok() {
            if let Err(e) = power_settings.handle_power_btn_action_change() {
                g_log!(
                    glib::LogLevel::Warning,
                    "Cannot bind the power button after a sway reload: {e}"
                );
            }
        }
    });
}

/// Call `on_reload` after every sway config reload (the workspace event
/// with change `reload`, sent once the new config is in place), until the
/// connection to sway ends.
fn watch_sway_reloads(
    conn: swayipc::Connection,
    mut on_reload: impl FnMut(),
) -> swayipc::Fallible<()> {
    for event in conn.subscribe([swayipc::EventType::Workspace])? {
        if let swayipc::Event::Workspace(workspace) = event? {
            if workspace.change == swayipc::WorkspaceChange::Reload {
                on_reload();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod sway_reload_tests {
    use super::watch_sway_reloads;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    /// A sway IPC message: magic, payload length, type, payload.
    fn ipc_message(kind: u32, payload: &str) -> Vec<u8> {
        let mut msg = b"i3-ipc".to_vec();
        msg.extend((payload.len() as u32).to_ne_bytes());
        msg.extend(kind.to_ne_bytes());
        msg.extend(payload.as_bytes());
        msg
    }

    #[test]
    fn follows_sway_reloads_until_sway_goes_away() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let fake_sway = std::thread::spawn(move || {
            let mut header = [0u8; 14];
            server.read_exact(&mut header).unwrap();
            assert_eq!(&header[..6], b"i3-ipc");
            let len = u32::from_ne_bytes(header[6..10].try_into().unwrap());
            let kind = u32::from_ne_bytes(header[10..14].try_into().unwrap());
            let mut payload = vec![0u8; len as usize];
            server.read_exact(&mut payload).unwrap();
            assert_eq!(kind, 2, "SUBSCRIBE");
            assert_eq!(String::from_utf8(payload).unwrap(), r#"["workspace"]"#);
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
        let mut reloads = 0;
        let result = watch_sway_reloads(swayipc::Connection::from(client), || reloads += 1);
        fake_sway.join().unwrap();
        assert_eq!(reloads, 2);
        assert!(result.is_err());
    }
}

impl PowerSettings {
    // Requires settings HandlePowerKey=ignore in logind.conf
    fn handle_power_btn_action_change(&self) -> Result<(), Box<dyn Error>> {
        use PowerButtonAction::*;
        let mut sway_conn = swayipc::Connection::new()?;
        use KeySymAction::*;
        let btn_change_action = match self.power_button_action() {
            Nothing => Unbind {
                key: POWER_OFF_KEY.to_string(),
            },
            Suspend => ReBind {
                key: POWER_OFF_KEY.to_string(),
                action: "systemctl suspend".to_string(),
            },
            Hibernate => ReBind {
                key: POWER_OFF_KEY.to_string(),
                action: "systemctl hibernate".to_string(),
            },
            Interactive => ReBind {
                key: POWER_OFF_KEY.to_string(),
                // TODO: Replace with a more sensible action (Prefferably user defined)
                action: "swaynag -t warning -m 'Do you really want to shutdown' -b 'Shutdown' '/usr/bin/gnome-session-quit --power-off --no-prompt'".to_string()
            }
        };

        match btn_change_action {
            Unbind { ref key } => sway_conn.run_command(format!("unbindsym {key}"))?,
            ReBind {
                ref key,
                ref action,
            } => {
                let _ = sway_conn.run_command(format!("unbindsym {key}"));
                sway_conn.run_command(format!("bindsym {key} exec \"{action}\""))?
            }
        };

        Ok(())
    }
}
fn main() {
    // The lock screen's D-Bus interface, which gnome-flashback provides in
    // the X11 session. It runs in a thread of its own, and tells the session
    // inhibition bridge when the screen locks and unlocks.
    let (lock_watch, lock_changes) = screensaver::LockWatch::new();
    screensaver::start(lock_watch.clone());

    let app = Application::new(Some("org.regolith.powerd"), ApplicationFlags::IS_SERVICE);

    // Keep the application alive until the end of the program
    let hold_guard = ManuallyDrop::new(app.hold());

    // Removable-media automount, which gnome-flashback provides in the X11
    // session.
    let _automount = automount::start();

    let session_settings = Rc::new(SessionSettings::new());
    let manager = Manager::new(session_settings.clone());
    let idle = manager.idle_state();

    // Take over power-key and lid-switch handling from logind for the
    // daemon's lifetime, once the session main context starts running.
    let startup = idle.clone();
    glib::spawn_future_local(async move {
        match logind::setup_logind_inhibits().await {
            Ok(fd) => {
                // Intentionally leak the fd: dropping it would release the
                // inhibitor, and the kernel closes it when the daemon exits.
                let _ = ManuallyDrop::new(fd);
            }
            Err(e) => g_log!(
                glib::LogLevel::Warning,
                "Failed to acquire logind inhibitors: {e}"
            ),
        }
        startup.startup_step_done();
    });

    // Bridge GNOME session inhibition to logind, which swayidle already honors.
    glib::spawn_future_local(session::run_inhibits(
        session_settings,
        idle.clone(),
        lock_watch,
        lock_changes,
    ));

    // Follow the power source, as gnome-settings-daemon does through UPower.
    glib::spawn_future_local(power_source::run(idle));

    // The confirmation that gnome-session asks the shell for before ending
    // the session, which gnome-flashback provides in the X11 session, and the
    // Displays panel's monitor labels, which are asked for under the same
    // name. They run in a thread of their own.
    end_session::start();

    manager.run().expect("Failed to run");

    // Hold the power-saver profile while the battery is low, as
    // gnome-settings-daemon's power plugin does.
    glib::spawn_future_local(profiles::run_power_saver());

    // Low-battery notifications, which gnome-settings-daemon's power
    // plugin gives the X11 session.
    glib::spawn_future_local(battery::run_warnings());

    app.run();

    // Drop the hold guard
    let _ = ManuallyDrop::into_inner(hold_guard);
}

#[cfg(test)]
mod tests {
    use super::{stop_swayidle, Manager, LOCKER};
    use std::{
        fs,
        os::unix::fs::{symlink, PermissionsExt},
        path::{Path, PathBuf},
        process::Command,
        time::{Duration, Instant},
    };

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "regolith-powerd-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Run the lock command for `locker` and report how long it took and
    /// whether the locker was still running when it returned.
    fn run_lock_cmd(locker: &str, wait: Duration) -> (Duration, bool) {
        let dir = test_dir(&format!("lock-{}", wait.as_millis()));
        let pid_file = dir.join("pid");
        let cmd = Manager::get_lock_cmd(
            &format!("sh -c 'echo $$ > {}; exec {locker}'", pid_file.display()),
            wait,
        );
        let start = Instant::now();
        let status = Command::new("sh").arg("-c").arg(&cmd).status().unwrap();
        let elapsed = start.elapsed();
        assert!(status.success());
        let pid = fs::read_to_string(&pid_file).unwrap().trim().to_string();
        let running = Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        let _ = Command::new("kill")
            .arg(&pid)
            .stderr(std::process::Stdio::null())
            .status();
        fs::remove_dir_all(&dir).unwrap();
        (elapsed, running)
    }

    #[test]
    fn lock_cmd_can_be_joined_with_other_commands() {
        let lock = Manager::get_lock_cmd("true", Duration::from_millis(5000));
        assert!(!lock.contains('\n'));
        let joined = format!("true;{lock};true");
        let status = Command::new("sh")
            .args(["-n", "-c", &joined])
            .status()
            .unwrap();
        assert!(status.success(), "{joined}");
    }

    #[test]
    fn lock_cmd_returns_when_the_locker_daemonizes() {
        let (elapsed, running) = run_lock_cmd("sleep 0.3", Duration::from_millis(5000));
        assert!(!running);
        assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }

    #[test]
    fn lock_cmd_does_not_wait_for_a_locker_that_keeps_running() {
        let (elapsed, running) = run_lock_cmd("sleep 30", Duration::from_millis(1000));
        assert!(running);
        assert!(elapsed >= Duration::from_secs(1), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    }

    /// A stand-in for swayidle that, like swayidle, runs its pending resume
    /// command when it gets SIGTERM.
    fn fake_swayidle(dir: &PathBuf, on_term: &str) -> std::process::Child {
        let script = dir.join("swayidle");
        fs::write(
            &script,
            format!("#!/bin/sh\ntrap '{on_term}' TERM\necho started > started\nwhile :; do sleep 0.05; done\n"),
        )
        .unwrap();
        // Run it through sh rather than executing the file just written: a
        // fork in another test thread can still hold it open for writing,
        // and exec would then fail with ETXTBSY.
        let child = Command::new("sh")
            .arg(&script)
            .current_dir(dir)
            .spawn()
            .unwrap();
        let start = Instant::now();
        while !dir.join("started").exists() {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(20));
        }
        child
    }

    #[test]
    fn stop_swayidle_lets_it_run_its_resume_commands() {
        let dir = test_dir("stop-term");
        let mut child = fake_swayidle(&dir, "echo resumed > resumed; exit 0");
        glib::MainContext::new().block_on(stop_swayidle(&mut child));
        assert_eq!(
            fs::read_to_string(dir.join("resumed")).unwrap().trim(),
            "resumed"
        );
        // Reaped: no zombie is left behind.
        assert!(child.try_wait().unwrap().is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stop_swayidle_kills_it_when_it_does_not_exit() {
        let dir = test_dir("stop-kill");
        let mut child = fake_swayidle(&dir, "echo ignored > ignored");
        let start = Instant::now();
        glib::MainContext::new().block_on(stop_swayidle(&mut child));
        assert!(start.elapsed() < Duration::from_secs(8));
        assert!(dir.join("ignored").exists());
        assert!(child.try_wait().unwrap().is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Run the lock command with stand-ins for trawlcat (answering from
    /// `resources`, "key: value" lines) and the lock screens, and return the
    /// lock screen that started and its arguments, or None if none did.
    /// regolith-sway-gtklock is installed if `script` is true.
    fn run_locker(resources: &[&str], script: bool) -> Option<Vec<String>> {
        static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = test_dir(&format!("locker-{run}"));
        let bin = dir.join("bin");
        let home = dir.join("home");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("lock.png"), "").unwrap();
        fs::create_dir_all(dir.join("my pictures")).unwrap();
        fs::write(dir.join("my pictures/lock screen.png"), "").unwrap();
        let resources: Vec<String> = resources
            .iter()
            .map(|r| r.replace("@DIR@", &dir.display().to_string()))
            .collect();
        fs::write(dir.join("resources"), resources.join("\n") + "\n").unwrap();
        fs::write(
            bin.join("trawlcat"),
            "#!/bin/sh\n\
             value=$(sed -n \"s/^$1:[[:space:]]*//p\" \"$STUB_RESOURCES\" | tail -n 1)\n\
             if [ -n \"$value\" ]; then printf '%s\\n' \"$value\"; else printf '%s\\n' \"$2\"; fi\n",
        )
        .unwrap();
        // The few programs the command needs, so that PATH can be the stand-ins
        // alone: a regolith-sway-gtklock installed on this system mustn't be
        // found.
        for tool in ["sh", "sed", "tail", "sleep"] {
            let path = ["/usr/bin", "/bin"]
                .iter()
                .map(|dir| Path::new(dir).join(tool))
                .find(|path| path.exists())
                .unwrap();
            symlink(path, bin.join(tool)).unwrap();
        }
        let lockers: &[&str] = if script {
            &["gtklock", "swaylock", "regolith-sway-gtklock"]
        } else {
            &["gtklock", "swaylock"]
        };
        for locker in lockers {
            fs::write(
                bin.join(locker),
                format!(
                    "#!/bin/sh\necho {locker} > \"$STUB_LOG\"\n\
                     for arg in \"$@\"; do printf '%s\\n' \"$arg\"; done >> \"$STUB_LOG\"\n"
                ),
            )
            .unwrap();
            fs::set_permissions(bin.join(locker), fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::set_permissions(bin.join("trawlcat"), fs::Permissions::from_mode(0o755)).unwrap();

        let cmd = Manager::get_lock_cmd(LOCKER, Duration::from_millis(5000));
        let status = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .env("PATH", &bin)
            .env("HOME", &home)
            .env("STUB_RESOURCES", dir.join("resources"))
            .env("STUB_LOG", dir.join("log"))
            .status()
            .unwrap();
        assert!(status.success());
        let args = fs::read_to_string(dir.join("log")).ok().map(|log| {
            log.lines()
                .map(|l| l.replace(&dir.display().to_string(), "@DIR@"))
                .collect()
        });
        fs::remove_dir_all(&dir).unwrap();
        args
    }

    /// [`run_locker`] without regolith-sway-gtklock
    fn locker_args(resources: &[&str]) -> Option<Vec<String>> {
        run_locker(resources, false)
    }

    #[test]
    fn locker_runs_regolith_sway_gtklock_when_it_is_installed() {
        let args = run_locker(&["regolith.lockscreen.wallpaper.file: ~/lock.png"], true);
        assert_eq!(args.unwrap(), ["regolith-sway-gtklock", "-d"]);
        let unset = run_locker(&[], true);
        assert_eq!(unset.unwrap(), ["regolith-sway-gtklock", "-d"]);
    }

    #[test]
    fn locker_prefers_wm_program_lock_sway_to_regolith_sway_gtklock() {
        let args = run_locker(&["wm.program.lock_sway: swaylock -f -c 000000"], true);
        assert_eq!(args.unwrap(), ["swaylock", "-f", "-c", "000000"]);
    }

    #[test]
    fn locker_passes_a_wallpaper_path_with_spaces_as_one_argument() {
        let args =
            locker_args(&["regolith.lockscreen.wallpaper.file: @DIR@/my pictures/lock screen.png"]);
        assert_eq!(
            args.unwrap(),
            [
                "gtklock",
                "-d",
                "--background",
                "@DIR@/my pictures/lock screen.png"
            ]
        );
    }

    #[test]
    fn locker_expands_a_wallpaper_path_under_home() {
        let args = locker_args(&["regolith.lockscreen.wallpaper.file: ~/lock.png"]);
        assert_eq!(
            args.unwrap(),
            ["gtklock", "-d", "--background", "@DIR@/home/lock.png"]
        );
    }

    #[test]
    fn locker_locks_without_an_image_when_the_wallpaper_is_missing_or_unset() {
        let missing = locker_args(&["regolith.lockscreen.wallpaper.file: /nonexistent/lock.png"]);
        assert_eq!(missing.unwrap(), ["gtklock", "-d"]);
        let unset = locker_args(&[]);
        assert_eq!(unset.unwrap(), ["gtklock", "-d"]);
    }

    #[test]
    fn locker_runs_wm_program_lock_sway_when_it_is_set() {
        let args = locker_args(&[
            "wm.program.lock_sway: swaylock -f -c 000000",
            "regolith.lockscreen.wallpaper.file: ~/lock.png",
        ]);
        assert_eq!(args.unwrap(), ["swaylock", "-f", "-c", "000000"]);
    }

    #[test]
    fn locker_can_be_joined_with_other_commands() {
        let lock = Manager::get_lock_cmd(LOCKER, Duration::from_millis(5000));
        assert!(!lock.contains('\n'));
        let joined = format!("true;{lock};true");
        let status = Command::new("sh")
            .args(["-n", "-c", &joined])
            .status()
            .unwrap();
        assert!(status.success(), "{joined}");
    }
}
