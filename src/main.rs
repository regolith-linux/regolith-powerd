use gio::{prelude::*, Application, ApplicationFlags};
use glib::g_log;
use gsettings_macro::gen_settings;
use std::{
    error::Error,
    mem::ManuallyDrop,
    process::{Child, Command},
};
use swayipc;

mod logind;

struct Manager {
    power_settings: PowerSettings,
    session_settings: SessionSettings,
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
    pub fn new() -> Self {
        Self {
            session_settings: SessionSettings::new(),
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
                if let Some(prev_child) = sway_idle_child.as_mut() {
                    prev_child
                        .kill()
                        .unwrap_or_else(|e| println!("Cannot kill swayidle: {e}"));
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
            let timeout = self.session_settings.idle_delay();
            let mut swayidle_timeout_args = Self::get_timeout_cmd(timeout, &action);
            let mut swayidle_resume_args =
                Self::get_resume_cmd("light -S $(cat $XDG_RUNTIME_DIR/screen_brightness_old.var)");
            args.append(&mut swayidle_timeout_args);
            args.append(&mut swayidle_resume_args);
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
            let on_timout = format!(
                r#"
            if on_ac_power; then
                {action}
            fi
            "#
            );
            let mut swayidle_timeout_args = Self::get_timeout_cmd(timeout as u32, &on_timout);
            args.append(&mut swayidle_timeout_args);

            if let Some(resume_action) = resume_action_ac_opt {
                let mut swayidle_resume_args = Self::get_resume_cmd(resume_action);
                args.append(&mut swayidle_resume_args);
            }
        }

        if let Some(action) = idle_action_bat_opt {
            let timeout = self.power_settings.sleep_inactive_battery_timeout();
            let on_timout = format!(
                r#"
            if ! on_ac_power; then
                {action}
            fi
            "#
            );
            let mut swayidle_timout_args = Self::get_timeout_cmd(timeout as u32, &on_timout);
            args.append(&mut swayidle_timout_args);

            if let Some(resume_action) = resume_action_bat_opt {
                let mut swayidle_resume_args = Self::get_resume_cmd(&resume_action);
                args.append(&mut swayidle_resume_args);
            }
        }

        let default_lock = format!("gtklock -d --background $(trawlcat regolith.lockscreen.wallpaper.file /dev/null)");
        let lock_screen = format!("$(trawlcat wm.program.lock \"{default_lock}\")");
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
}

#[derive(Debug)]
enum KeySymAction {
    Unbind { key: String },
    ReBind { key: String, action: String },
}

const POWER_OFF_KEY: &str = "XF86Poweroff";

impl PowerSettings {
    /// Requires settings HandlePowerKey=ignore in logind.conf
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

    // Setup holds for event loop and logind inhibit
    let hold = (app.hold(), logind::setup_logind_inhibits());

    // Keep the hold guard alive until the end of the program
    let hold_guard = ManuallyDrop::new(hold);

    let manager = Manager::new();
    manager.run().expect("Failed to run");
    app.run();

    // Drop the hold guard
    let _ = ManuallyDrop::into_inner(hold_guard);
}
