use gio;
use gio::{prelude::ApplicationExtManual, traits::ApplicationExt, Application, ApplicationFlags};
use gsettings_macro::gen_settings;
use glib::g_log;
use log::info;
use std::{
    error::Error,
    process::{Child, Command},
    sync::mpsc,
};

struct Manager {
    power_settings: PowerSettings,
    session_settings: SessionSettings,
}

#[gen_settings(file = "org.gnome.settings-daemon.plugins.power.gschema.xml")]
pub struct PowerSettings;

#[gen_settings(file = "org.gnome.desktop.session.gschema.xml")]
pub struct SessionSettings;

impl Manager {
    pub fn new() -> Self {
        Self {
            session_settings: SessionSettings::new("org.gnome.desktop.session"),
            power_settings: PowerSettings::new("org.gnome.settings-daemon.plugins.power"),
        }
    }
    pub fn run(mut self) -> Result<(), Box<dyn Error>> {
        let (send_reload_event, recv_reload_event) = mpsc::channel::<()>();
        let (session_settings_tx, session_settings_rx) = mpsc::channel::<SessionSettings>();

        // Send initial Reload
        send_reload_event.send(()).expect("Failed to send reload");

        let send_reload_cb = || {
            let tx_cpy = send_reload_event.clone();
            g_log!(glib::LogLevel::Warning,"Callback created");
            
            return move |_: &PowerSettings| {
                g_log!(glib::LogLevel::Warning,"Callback executed");
                tx_cpy.send(()).expect("Cannot reload config");
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
        self.session_settings.connect_idle_delay_changed(move |s| {
            println!("idle delay changed");
            session_settings_tx
                .send(s.clone())
                .expect("Cannot reload idle settings");
            send_reload_event
                .clone()
                .send(())
                .expect("Failed to reload");
        });
        let mut sway_idle_child: Option<Child> = None;

        for _ in recv_reload_event {
            self.session_settings = match session_settings_rx.try_recv() {
                Ok(s) => s,
                Err(_) => self.session_settings,
            };
            if let Some(mut prev_child) = sway_idle_child {
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
            g_log!(glib::LogLevel::Warning,"Command Executed");
        }
        Ok(())
    }
    fn get_swayidle_args(&self) -> Vec<String> {
        let mut args = Vec::new();
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
                Nothing => (None, None),
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
                Nothing => (None, None),
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

        let lock_screen = format!("$(trawlcat i3-wm.program.lock gtklock)");
        let before_sleep = lock_screen.clone();
        let mut before_sleep_args = vec!["before-sleep".to_owned(), before_sleep];
        let mut lock_screen_args = vec!["lock".to_owned(), lock_screen];
        args.append(&mut before_sleep_args);
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

impl PowerSettings {
    fn handle_power_btn_action_change(&self) -> Result<(), Box<dyn Error>> {
        use PowerButtonAction::*;
        match self.power_button_action() {
            Nothing => todo!(),
            Suspend => todo!(),
            Hibernate => todo!(),
            Interactive => todo!(),
        }
    }
}
fn main() {
    std::env::set_var("RUST_LOG", "info");
    pretty_env_logger::init();
    let app = Application::new(Some("org.regolith.inputd"), ApplicationFlags::IS_SERVICE);
    let manager = Manager::new();
    manager.run().expect("Failed to run");
    app.hold();
    app.run();
}
