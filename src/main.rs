use gio::{prelude::*, Application, ApplicationFlags};
use glib::g_log;
use gsettings_macro::gen_settings;
use std::{
    cell::Cell,
    error::Error,
    mem::ManuallyDrop,
    process::{Child, Command},
    rc::Rc,
    time::Duration,
};
use swayipc;

mod battery;
mod end_session;
mod logind;
mod power_source;
mod screensaver;
mod session;

/// What decides the idle actions besides the settings: the power source
/// (UPower's `OnBattery`) and whether a GNOME application inhibits suspend.
/// gnome-settings-daemon re-arms its idle timers when either changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdleContext {
    pub on_battery: bool,
    pub suspend_inhibited: bool,
}

/// Shared idle context plus a way to ask the manager to re-arm the timers.
#[derive(Clone)]
pub struct IdleState {
    context: Rc<Cell<IdleContext>>,
    reload: async_channel::Sender<()>,
}

impl IdleState {
    fn new(reload: async_channel::Sender<()>) -> Self {
        Self {
            context: Rc::new(Cell::new(IdleContext::default())),
            reload,
        }
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
            while let Ok(_) = recv_reload_event.recv().await {
                // Restarting swayidle starts its idle count again, so keep
                // the running one when nothing it runs has changed (for
                // example a power-source change with the same action).
                let running = sway_idle_child
                    .as_mut()
                    .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
                if running && current_args.as_ref() == Some(&self.get_swayidle_args()) {
                    continue;
                }
                if let Some(mut prev_child) = sway_idle_child.take() {
                    stop_swayidle(&mut prev_child).await;
                }
                let swayidle_args = self.get_swayidle_args();
                current_args = Some(swayidle_args.clone());
                let child = Command::new("swayidle")
                    .arg("-w")
                    .args(swayidle_args)
                    .spawn();
                sway_idle_child = child.ok();
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

        // An idle delay of 0 means the session never becomes idle
        let idle_delay_sec = self.session_settings.idle_delay();

        if idle_delay_sec > 0 && self.power_settings.idle_dim() {
            let idle_brightness = self.power_settings.idle_brightness();
            let action = format!(
                r#"
                export curr_brightness=$(light)
                echo $curr_brightness | tee $XDG_RUNTIME_DIR/screen_brightness_old.var
                if [ 1 -eq "$(echo "${{curr_brightness}} > {idle_brightness}" | bc)" ]; then
                    light -S {idle_brightness}
                fi
                "#,
            );

            // Dim screen to $idle_brightness when idle dim timeout hit
            let mut swayidle_dim_timeout_args = Self::get_timeout_cmd(idle_delay_sec, &action);
            let mut swayidle_dim_resume_args = Self::get_resume_cmd(&format!(
                "light -S $(cat $XDG_RUNTIME_DIR/screen_brightness_old.var)"
            ));
            args.append(&mut swayidle_dim_timeout_args);
            args.append(&mut swayidle_dim_resume_args);
        }

        let locks = lock_triggers(
            self.screensaver_settings.lock_enabled(),
            self.lock_on_suspend(),
            self.lockdown_settings.disable_lock_screen(),
        );

        if idle_delay_sec > 0 {
            // Display off at 5 seconds after idle timeout, whether or not the screen dims
            let display_off_timeout_sec = idle_delay_sec.saturating_add(5);
            let mut swayidle_display_off_timeout_args =
                Self::get_timeout_cmd(display_off_timeout_sec, &display_off);
            let mut swayidle_display_off_resume_args = Self::get_resume_cmd(&display_on);
            args.append(&mut swayidle_display_off_timeout_args);
            args.append(&mut swayidle_display_off_resume_args);

            // Locked lock-delay seconds after display off
            if locks.idle {
                let lock_delay_sec = self.screensaver_settings.lock_delay();
                let lock_timeout_sec = display_off_timeout_sec.saturating_add(lock_delay_sec);
                let mut swayidle_lock_timeout_args =
                    Self::get_timeout_cmd(lock_timeout_sec, &lock_screen);
                args.append(&mut swayidle_lock_timeout_args);
            }
        }

        if let Some(sleep) = Self::sleep_timeout(
            self.idle.get(),
            self.power_settings.sleep_inactive_ac_type(),
            self.power_settings.sleep_inactive_ac_timeout(),
            self.power_settings.sleep_inactive_battery_type(),
            self.power_settings.sleep_inactive_battery_timeout(),
        ) {
            let mut swayidle_timeout_args = Self::get_timeout_cmd(sleep.timeout, sleep.action);
            args.append(&mut swayidle_timeout_args);

            if let Some(resume_action) = sleep.resume {
                let mut swayidle_resume_args = Self::get_resume_cmd(resume_action);
                args.append(&mut swayidle_resume_args);
            }
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
    /// means never.
    ///
    /// A suspend or hibernate action is left out while a GNOME application
    /// inhibits suspend: that inhibition holds back automatic suspend only,
    /// so logind's lock for it is weak and explicit requests (power button,
    /// keybinding) still go through. The manager re-arms the timers when the
    /// inhibitor goes away.
    fn sleep_timeout(
        ctx: IdleContext,
        ac_type: SleepInactiveAcType,
        ac_timeout: i32,
        battery_type: SleepInactiveBatteryType,
        battery_timeout: i32,
    ) -> Option<SleepTimeout> {
        let ((action, resume), timeout, sleeps) = if ctx.on_battery {
            use SleepInactiveBatteryType::*;
            (
                Self::battery_sleep_cmds(battery_type),
                battery_timeout,
                matches!(battery_type, Suspend | Hibernate),
            )
        } else {
            use SleepInactiveAcType::*;
            (
                Self::ac_sleep_cmds(ac_type),
                ac_timeout,
                matches!(ac_type, Suspend | Hibernate),
            )
        };
        if sleeps && ctx.suspend_inhibited {
            return None;
        }
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

/// Shell command that starts the lock screen: `wm.program.lock_sway` when it
/// is set, else `gtklock -d` showing the lock-screen wallpaper
/// (`regolith.lockscreen.wallpaper.file`).
///
/// The wallpaper path goes to gtklock as a single argument, so it may contain
/// spaces, and a leading `~` is expanded to the home directory. When the
/// resource is unset or the file doesn't exist, gtklock still starts, just
/// without the image. (A path with a space used to reach gtklock as several
/// arguments, and gtklock then crashed, leaving the session unlocked.) These
/// are the rules of `regolith-sway-gtklock` (from regolith-wm-config's
/// regolith-sway-gtklock package), which the lock key and the lid close
/// script use; powerd doesn't depend on that package, so it doesn't call it.
///
/// The group runs in the background in [`Manager::get_lock_cmd`], and each
/// branch `exec`s the locker, so the pid that is waited for is the locker's.
/// It is a single line, so it can be joined with other commands by `;`.
const LOCKER: &str = concat!(
    "{ locker=$(trawlcat wm.program.lock_sway ''); ",
    "if [ -n \"$locker\" ]; then exec $locker; fi; ",
    "wallpaper=$(trawlcat regolith.lockscreen.wallpaper.file ''); ",
    "case $wallpaper in '~/'*) wallpaper=$HOME/${wallpaper#'~/'};; esac; ",
    "if [ -n \"$wallpaper\" ] && [ -f \"$wallpaper\" ]; then ",
    "exec gtklock -d --background \"$wallpaper\"; fi; ",
    "exec gtklock -d; }"
);

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
        IdleContext, IdleState, Manager, SleepInactiveAcType as Ac,
        SleepInactiveBatteryType as Battery, SleepTimeout, DISPLAY_OFF, DISPLAY_ON,
    };

    const ON_AC: IdleContext = IdleContext {
        on_battery: false,
        suspend_inhibited: false,
    };
    const ON_BATTERY: IdleContext = IdleContext {
        on_battery: true,
        suspend_inhibited: false,
    };

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
        let inhibited = |on_battery| IdleContext {
            on_battery,
            suspend_inhibited: true,
        };
        for ctx in [inhibited(false), inhibited(true)] {
            for (ac, battery) in [
                (Ac::Suspend, Battery::Suspend),
                (Ac::Hibernate, Battery::Hibernate),
            ] {
                assert_eq!(Manager::sleep_timeout(ctx, ac, 60, battery, 60), None);
            }
        }
        // The other actions go on.
        let blank = SleepTimeout {
            timeout: 60,
            action: DISPLAY_OFF,
            resume: Some(DISPLAY_ON),
        };
        assert_eq!(
            Manager::sleep_timeout(inhibited(false), Ac::Blank, 60, Battery::Suspend, 30),
            Some(blank)
        );
        let logout = SleepTimeout {
            timeout: 30,
            action: "gnome-session-quit --no-prompt",
            resume: None,
        };
        assert_eq!(
            Manager::sleep_timeout(inhibited(true), Ac::Suspend, 60, Battery::Logout, 30),
            Some(logout)
        );
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

const POWER_OFF_KEY: &str = "XF86Poweroff";

/// Command the power button runs for the GNOME setting `power-button-action`,
/// or `None` when the button does nothing.
///
/// "Power Off" in Settings (`interactive`) asks gnome-session to power off,
/// which shows the end-session dialog from `end_session`: the same question,
/// 60-second countdown and list of busy apps as the X11 session's dialog.
fn power_button_command(action: PowerButtonAction) -> Option<&'static str> {
    use PowerButtonAction::*;
    match action {
        Nothing => None,
        Suspend => Some("systemctl suspend"),
        Hibernate => Some("systemctl hibernate"),
        Interactive => Some("/usr/bin/gnome-session-quit --power-off"),
    }
}

#[cfg(test)]
mod power_button_tests {
    use super::{power_button_command, PowerButtonAction};

    #[test]
    fn power_button_asks_through_the_end_session_dialog() {
        assert_eq!(
            power_button_command(PowerButtonAction::Interactive),
            Some("/usr/bin/gnome-session-quit --power-off")
        );
        assert_eq!(
            power_button_command(PowerButtonAction::Suspend),
            Some("systemctl suspend")
        );
        assert_eq!(
            power_button_command(PowerButtonAction::Hibernate),
            Some("systemctl hibernate")
        );
        assert_eq!(power_button_command(PowerButtonAction::Nothing), None);
    }
}

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
        let mut sway_conn = swayipc::Connection::new()?;
        let _ = sway_conn.run_command(format!("unbindsym {POWER_OFF_KEY}"));
        if let Some(action) = power_button_command(self.power_button_action()) {
            sway_conn.run_command(format!("bindsym {POWER_OFF_KEY} exec \"{action}\""))?;
        }
        Ok(())
    }
}
fn main() {
    // The lock screen's D-Bus interface, which gnome-flashback provides in
    // the X11 session. It runs in a thread of its own.
    screensaver::start();

    let app = Application::new(Some("org.regolith.powerd"), ApplicationFlags::IS_SERVICE);

    // Keep the application alive until the end of the program
    let hold_guard = ManuallyDrop::new(app.hold());

    // Take over power-key and lid-switch handling from logind for the
    // daemon's lifetime, once the session main context starts running.
    glib::spawn_future_local(async {
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
    });

    // Bridge GNOME session inhibition to logind, which swayidle already honors.
    let session_settings = Rc::new(SessionSettings::new());
    let manager = Manager::new(session_settings.clone());
    let idle = manager.idle_state();
    glib::spawn_future_local(session::run_inhibits(session_settings, idle.clone()));

    // Follow the power source, as gnome-settings-daemon does through UPower.
    glib::spawn_future_local(power_source::run(idle));

    // The confirmation that gnome-session asks the shell for before ending
    // the session, which gnome-flashback provides in the X11 session. It
    // runs in a thread of its own.
    end_session::start();

    manager.run().expect("Failed to run");

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
        os::unix::fs::PermissionsExt,
        path::PathBuf,
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
    /// `resources`, "key: value" lines) and gtklock, and return the arguments
    /// gtklock got, or None if it didn't start.
    fn locker_args(resources: &[&str]) -> Option<Vec<String>> {
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
        for locker in ["gtklock", "swaylock"] {
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
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
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
