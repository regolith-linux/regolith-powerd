use gio::{prelude::*, Application, ApplicationFlags};
use glib::g_log;
use gsettings_macro::gen_settings;
use std::{
    error::Error,
    mem::ManuallyDrop,
    process::{Child, Command},
    rc::Rc,
};
use swayipc;

mod logind;
mod session;

struct Manager {
    power_settings: PowerSettings,
    session_settings: Rc<SessionSettings>,
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

impl Manager {
    pub fn new(session_settings: Rc<SessionSettings>) -> Self {
        Self {
            session_settings,
            power_settings: PowerSettings::new(),
        }
    }
    pub fn run(self) -> Result<(), Box<dyn Error>> {
        let (send_reload_event, recv_reload_event) = async_channel::bounded(1);

        // Send initial Reload
        send_reload_event
            .send_blocking(())
            .expect("Failed to send reload");
        self.power_settings.handle_power_btn_action_change()?;

        let send_reload_cb = || {
            let tx_cpy = send_reload_event.clone();
            return move |_: &PowerSettings| {
                tx_cpy.send_blocking(()).expect("Cannot reload config");
            };
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
        self.session_settings.connect_idle_delay_changed(move |_| {
            send_reload_event
                .clone()
                .send_blocking(())
                .expect("Failed to reload");
        });

        let mut sway_idle_child: Option<Child> = None;
        glib::spawn_future_local(async move {
            while let Ok(_) = recv_reload_event.recv().await {
                if let Some(mut prev_child) = sway_idle_child.take() {
                    stop_swayidle(&mut prev_child).await;
                }
                let swayidle_args = self.get_swayidle_args();
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
        let display_off = format!("swaymsg output '*' dpms off");
        let display_on = format!("swaymsg output '*' dpms on");

        let default_lock = format!(
            "gtklock -d --background $(trawlcat regolith.lockscreen.wallpaper.file /dev/null)"
        );
        let lock_screen = Self::get_lock_cmd(
            &format!("$(trawlcat wm.program.lock_sway \"{default_lock}\")"),
            LOCK_WAIT_TENTHS,
        );

        if self.power_settings.idle_dim() {
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
            let dim_timeout_sec = self.session_settings.idle_delay();
            let mut swayidle_dim_timeout_args = Self::get_timeout_cmd(dim_timeout_sec, &action);
            let mut swayidle_dim_resume_args = Self::get_resume_cmd(&format!(
                "light -S $(cat $XDG_RUNTIME_DIR/screen_brightness_old.var)"
            ));
            args.append(&mut swayidle_dim_timeout_args);
            args.append(&mut swayidle_dim_resume_args);

            // Display off at 5 seconds after dim timeout
            let display_off_timeout_sec = dim_timeout_sec + 5;
            let mut swayidle_display_off_timeout_args =
                Self::get_timeout_cmd(display_off_timeout_sec, &display_off);
            let mut swayidle_display_off_resume_args = Self::get_resume_cmd(&display_on);
            args.append(&mut swayidle_display_off_timeout_args);
            args.append(&mut swayidle_display_off_resume_args);

            // Locked 10 seconds after dim timeout / 5 seconds after display off
            let lock_timeout_sec = display_off_timeout_sec + 5;
            let mut swayidle_lock_timeout_args =
                Self::get_timeout_cmd(lock_timeout_sec, &lock_screen);
            args.append(&mut swayidle_lock_timeout_args);
        }

        let (idle_action_ac_opt, resume_action_ac_opt) = {
            use SleepInactiveAcType::*;
            match self.power_settings.sleep_inactive_ac_type() {
                Suspend => (Some("systemctl suspend"), None),
                Hibernate => (Some("systemctl hibernate"), None),
                Blank => (
                    Some("swaymsg output * dpms off"),
                    Some("swaymsg output * dpms on"),
                ),
                Shutdown => (Some("poweroff"), None),
                Logout => (Some("gnome-session-quit --no-prompt"), None),
                Interactive => todo!("Show some gui prompt to the user"),
                _ => (None, None),
            }
        };
        let (idle_action_bat_opt, resume_action_bat_opt) = {
            use SleepInactiveBatteryType::*;
            match self.power_settings.sleep_inactive_battery_type() {
                Suspend => (Some("systemctl suspend"), None),
                Hibernate => (Some("systemctl hibernate"), None),
                Blank => (
                    Some("swaymsg output * dpms off"),
                    Some("swaymsg output * dpms on"),
                ),
                Shutdown => (Some("poweroff"), None),
                Logout => (Some("gnome-session-quit --no-prompt"), None),
                Interactive => todo!("Show some gui prompt to the user"),
                _ => (None, None),
            }
        };

        if let Some(action) = idle_action_ac_opt {
            let timeout = self.power_settings.sleep_inactive_ac_timeout();
            let on_timout = Self::get_power_source_cmd(PowerSource::Ac, action);
            let mut swayidle_timeout_args = Self::get_timeout_cmd(timeout as u32, &on_timout);
            args.append(&mut swayidle_timeout_args);

            if let Some(resume_action) = resume_action_ac_opt {
                let mut swayidle_resume_args = Self::get_resume_cmd(resume_action);
                args.append(&mut swayidle_resume_args);
            }
        }

        if let Some(action) = idle_action_bat_opt {
            let timeout = self.power_settings.sleep_inactive_battery_timeout();
            let on_timout = Self::get_power_source_cmd(PowerSource::Battery, action);
            let mut swayidle_timout_args = Self::get_timeout_cmd(timeout as u32, &on_timout);
            args.append(&mut swayidle_timout_args);

            if let Some(resume_action) = resume_action_bat_opt {
                let mut swayidle_resume_args = Self::get_resume_cmd(&resume_action);
                args.append(&mut swayidle_resume_args);
            }
        }

        let pause_audio = format!("playerctl -a pause");
        let before_sleep = format!("{display_off};{lock_screen};{pause_audio};sleep 1");
        let after_resume = display_on.clone();
        let mut before_sleep_args = vec!["before-sleep".to_owned(), before_sleep];
        let mut after_resum_args = vec!["after-resume".to_owned(), after_resume];
        let mut lock_screen_args = vec!["lock".to_owned(), lock_screen];
        args.append(&mut before_sleep_args);
        args.append(&mut after_resum_args);
        args.append(&mut lock_screen_args);
        args
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

    /// Shell command that runs `action` only while on the given power source.
    ///
    /// `on_ac_power` exits 0 on mains power, 1 on battery and 255 when the
    /// power source cannot be determined, which is what desktops without
    /// power sensors report. Only exit status 1 means battery, so an unknown
    /// power source gets the AC settings rather than the battery ones.
    fn get_power_source_cmd(source: PowerSource, action: &str) -> String {
        let test = match source {
            PowerSource::Ac => "-ne",
            PowerSource::Battery => "-eq",
        };
        format!(
            r#"
            on_ac_power
            if [ $? {test} 1 ]; then
                {action}
            fi
            "#
        )
    }

    /// Shell command that starts `locker` and waits until it exits, but for
    /// at most `wait_tenths` tenths of a second.
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
    fn get_lock_cmd(locker: &str, wait_tenths: u32) -> String {
        format!(
            "{locker} & lock_pid=$!; i=0; \
             while [ $i -lt {wait_tenths} ] && kill -0 $lock_pid 2>/dev/null; do \
             sleep 0.1; i=$((i + 1)); done"
        )
    }
}

/// How long a lock command may take, in tenths of a second, before swayidle
/// carries on (the same limit as regolith-sway-clamshell's lock before sleep).
const LOCK_WAIT_TENTHS: u32 = 50;

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

#[derive(Debug, Clone, Copy)]
enum PowerSource {
    Ac,
    Battery,
}

#[derive(Debug)]
enum KeySymAction {
    Unbind { key: String },
    ReBind { key: String, action: String },
}

const POWER_OFF_KEY: &str = "XF86Poweroff";

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
    glib::spawn_future_local(session::run_inhibits(session_settings.clone()));

    let manager = Manager::new(session_settings);
    manager.run().expect("Failed to run");
    app.run();

    // Drop the hold guard
    let _ = ManuallyDrop::into_inner(hold_guard);
}

#[cfg(test)]
mod tests {
    use super::{stop_swayidle, Manager, PowerSource};
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
    fn run_lock_cmd(locker: &str, wait_tenths: u32) -> (Duration, bool) {
        let dir = test_dir(&format!("lock-{wait_tenths}"));
        let pid_file = dir.join("pid");
        let cmd = Manager::get_lock_cmd(
            &format!("sh -c 'echo $$ > {}; exec {locker}'", pid_file.display()),
            wait_tenths,
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
        let lock = Manager::get_lock_cmd("true", 50);
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
        let (elapsed, running) = run_lock_cmd("sleep 0.3", 50);
        assert!(!running);
        assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }

    #[test]
    fn lock_cmd_does_not_wait_for_a_locker_that_keeps_running() {
        let (elapsed, running) = run_lock_cmd("sleep 30", 10);
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

    /// Run the generated command with a stub `on_ac_power` that exits with
    /// `exit_code`, and report whether the guarded action ran.
    fn action_runs(source: PowerSource, exit_code: u8) -> bool {
        let dir: PathBuf = std::env::temp_dir().join(format!(
            "regolith-powerd-test-{}-{source:?}-{exit_code}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("on_ac_power");
        fs::write(&stub, format!("#!/bin/sh\nexit {exit_code}\n")).unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();

        let cmd = Manager::get_power_source_cmd(source, "echo ran");
        let path = format!("{}:/usr/bin:/bin", dir.display());
        let output = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .env("PATH", path)
            .output()
            .unwrap();
        fs::remove_dir_all(&dir).unwrap();
        String::from_utf8_lossy(&output.stdout).trim() == "ran"
    }

    #[test]
    fn ac_action_runs_on_ac_and_unknown_power_source() {
        assert!(action_runs(PowerSource::Ac, 0));
        assert!(!action_runs(PowerSource::Ac, 1));
        assert!(action_runs(PowerSource::Ac, 255));
    }

    #[test]
    fn battery_action_runs_only_on_battery() {
        assert!(!action_runs(PowerSource::Battery, 0));
        assert!(action_runs(PowerSource::Battery, 1));
        assert!(!action_runs(PowerSource::Battery, 255));
    }
}
