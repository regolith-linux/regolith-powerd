//! Low-battery notifications for the Wayland session.
//!
//! In the X11 session gnome-settings-daemon's power plugin sends these; the
//! Wayland session does not run that plugin, regolith-powerd replaces it.
//! Like the plugin, this follows the `WarningLevel` that UPower computes for
//! its composite display device from the thresholds in UPower.conf: entering
//! the low, critical or action level shows one notification, which is closed
//! when the level drops back to none (on AC power or charged).

use glib::g_log;
use std::collections::HashMap;
use zbus::export::futures_util::{stream, StreamExt};
use zbus::zvariant::Value;
use zbus::{dbus_proxy, Connection};

const APP_NAME: &str = "Power";
const LOW_TIMEOUT_MS: i32 = 30_000;
const RETRY_SECS: u32 = 60;

// UPower's UpDeviceKind values for the display device.
const KIND_UPS: u32 = 3;

#[dbus_proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
trait UPower {
    fn get_critical_action(&self) -> zbus::Result<String>;
}

#[dbus_proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/devices/DisplayDevice"
)]
trait Device {
    #[dbus_proxy(property)]
    fn warning_level(&self) -> zbus::Result<u32>;
    #[dbus_proxy(property)]
    fn percentage(&self) -> zbus::Result<f64>;
    #[dbus_proxy(property, name = "Type")]
    fn kind(&self) -> zbus::Result<u32>;
}

#[dbus_proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
    fn close_notification(&self, id: u32) -> zbus::Result<()>;
}

/// UPower's `WarningLevel`, reduced to the levels that warn on battery.
/// ("discharging" only applies to UPSes and does not warn here.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    None,
    Low,
    Critical,
    Action,
}

impl Level {
    pub fn from_upower(warning_level: u32) -> Level {
        match warning_level {
            3 => Level::Low,
            4 => Level::Critical,
            5 => Level::Action,
            _ => Level::None,
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Warning {
    pub summary: String,
    pub body: String,
    pub icon: &'static str,
    /// Notification urgency: 1 normal, 2 critical.
    pub urgency: u8,
    /// Expiry in milliseconds, 0 for never.
    pub timeout_ms: i32,
}

/// The notification for a level, with the texts gnome-settings-daemon uses,
/// so both sessions warn alike. `critical_action` is what UPower does at the
/// action level (`GetCriticalAction`).
pub fn warning(level: Level, kind: u32, percentage: f64, critical_action: &str) -> Option<Warning> {
    let ups = kind == KIND_UPS;
    let (summary, body, icon, urgency, timeout_ms) = match level {
        Level::None => return None,
        Level::Low if ups => (
            "UPS Low",
            format!("{percentage:.0}% UPS power remaining"),
            "battery-low-symbolic",
            1,
            LOW_TIMEOUT_MS,
        ),
        Level::Low => (
            "Low Battery",
            format!("{percentage:.0}% battery remaining"),
            "battery-low-symbolic",
            1,
            LOW_TIMEOUT_MS,
        ),
        Level::Critical if ups => (
            "UPS Almost Empty",
            format!("{percentage:.0}% UPS power remaining"),
            "battery-caution-symbolic",
            2,
            0,
        ),
        Level::Critical => (
            "Battery Almost Empty",
            "Connect power now".to_string(),
            "battery-caution-symbolic",
            2,
            0,
        ),
        Level::Action => (
            if ups {
                "UPS is Empty"
            } else {
                "Battery is Empty"
            },
            match critical_action {
                "PowerOff" => "This device is about to shutdown",
                "Suspend" => "This device is about to suspend",
                _ => "This device is about to hibernate",
            }
            .to_string(),
            "battery-action-symbolic",
            2,
            0,
        ),
    };
    Some(Warning {
        summary: summary.to_string(),
        body,
        icon,
        urgency,
        timeout_ms,
    })
}

/// Which level is on screen, so each level is announced once.
struct Tracker {
    level: Level,
    shown: Option<u32>,
}

impl Tracker {
    async fn update(
        &mut self,
        level: Level,
        device: &DeviceProxy<'_>,
        upower: &UPowerProxy<'_>,
        notifications: &NotificationsProxy<'_>,
    ) {
        if level == self.level {
            return;
        }
        // Like gnome-settings-daemon: the previous warning goes away when
        // the level changes, and a new one replaces it.
        if let Some(id) = self.shown.take() {
            if let Err(e) = notifications.close_notification(id).await {
                g_log!(glib::LogLevel::Debug, "Cannot close notification: {e}");
            }
        }
        self.level = Level::None;
        let kind = device.kind().await.unwrap_or_default();
        let percentage = device.percentage().await.unwrap_or_default();
        let action = upower.get_critical_action().await.unwrap_or_default();
        let Some(warning) = warning(level, kind, percentage, &action) else {
            return;
        };
        let mut hints = HashMap::from([("urgency", Value::U8(warning.urgency))]);
        if level == Level::Low {
            hints.insert("transient", Value::Bool(true));
        }
        match notifications
            .notify(
                APP_NAME,
                0,
                warning.icon,
                &warning.summary,
                &warning.body,
                &[],
                hints,
                warning.timeout_ms,
            )
            .await
        {
            Ok(id) => {
                self.level = level;
                self.shown = Some(id);
            }
            // Retried on the next battery update, e.g. when the
            // notification server was not up yet at login.
            Err(e) => g_log!(glib::LogLevel::Warning, "Cannot show battery warning: {e}"),
        }
    }
}

/// Watch the battery and notify until a connection fails.
pub async fn watch(system: &Connection, session: &Connection) -> zbus::Result<()> {
    let upower = UPowerProxy::new(system).await?;
    let device = DeviceProxy::new(system).await?;
    let notifications = NotificationsProxy::new(session).await?;
    let mut tracker = Tracker {
        level: Level::None,
        shown: None,
    };

    // Re-read the level on every battery update: a warning that could not
    // be shown is retried, and an unchanged level is a no-op.
    let levels = device.receive_warning_level_changed().await.map(|_| ());
    let percentages = device.receive_percentage_changed().await.map(|_| ());
    let mut updates = stream::select(levels, percentages);
    loop {
        let level = Level::from_upower(device.warning_level().await?);
        tracker
            .update(level, &device, &upower, &notifications)
            .await;
        if updates.next().await.is_none() {
            return Ok(());
        }
    }
}

/// Run the battery warnings for the lifetime of the daemon. Without UPower
/// (it is not installed, or a desktop has no battery service) this retries
/// quietly after the first warning.
pub async fn run_warnings() {
    let mut level = glib::LogLevel::Warning;
    loop {
        let result = async {
            let system = Connection::system().await?;
            let session = Connection::session().await?;
            watch(&system, &session).await
        }
        .await;
        if let Err(e) = result {
            g_log!(level, "Battery warnings unavailable: {e}");
            level = glib::LogLevel::Debug;
        }
        glib::timeout_future_seconds(RETRY_SECS).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use zbus::{dbus_interface, ConnectionBuilder, SignalContext};

    #[test]
    fn levels_map_like_upower() {
        assert_eq!(Level::from_upower(0), Level::None); // unknown
        assert_eq!(Level::from_upower(1), Level::None);
        assert_eq!(Level::from_upower(2), Level::None); // UPS discharging
        assert_eq!(Level::from_upower(3), Level::Low);
        assert_eq!(Level::from_upower(4), Level::Critical);
        assert_eq!(Level::from_upower(5), Level::Action);
    }

    #[test]
    fn warning_texts_follow_gnome() {
        let low = warning(Level::Low, 2, 9.6, "HybridSleep").unwrap();
        assert_eq!(
            (low.summary.as_str(), low.body.as_str(), low.urgency),
            ("Low Battery", "10% battery remaining", 1)
        );
        assert_eq!(low.timeout_ms, 30_000);
        let critical = warning(Level::Critical, 2, 4.0, "HybridSleep").unwrap();
        assert_eq!(
            (critical.summary.as_str(), critical.body.as_str()),
            ("Battery Almost Empty", "Connect power now")
        );
        assert_eq!((critical.urgency, critical.timeout_ms), (2, 0));
        let action = warning(Level::Action, 2, 1.0, "PowerOff").unwrap();
        assert_eq!(action.body, "This device is about to shutdown");
        assert_eq!(
            warning(Level::Low, KIND_UPS, 20.0, "").unwrap().summary,
            "UPS Low"
        );
        assert_eq!(warning(Level::None, 2, 50.0, ""), None);
    }

    /// The display device of a fake UPower.
    struct FakeDevice {
        warning_level: u32,
        percentage: f64,
    }

    #[dbus_interface(name = "org.freedesktop.UPower.Device")]
    impl FakeDevice {
        #[dbus_interface(property)]
        fn warning_level(&self) -> u32 {
            self.warning_level
        }
        #[dbus_interface(property)]
        fn percentage(&self) -> f64 {
            self.percentage
        }
        #[dbus_interface(property, name = "Type")]
        fn kind(&self) -> u32 {
            2
        }
    }

    struct FakeUPower;

    #[dbus_interface(name = "org.freedesktop.UPower")]
    impl FakeUPower {
        fn get_critical_action(&self) -> String {
            "HybridSleep".to_string()
        }
    }

    /// A notification server that records what it is asked to do.
    struct FakeNotifications {
        log: Arc<Mutex<Vec<String>>>,
        next_id: u32,
    }

    #[dbus_interface(name = "org.freedesktop.Notifications")]
    impl FakeNotifications {
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &mut self,
            _app_name: &str,
            _replaces_id: u32,
            _app_icon: &str,
            summary: &str,
            body: &str,
            _actions: Vec<&str>,
            hints: HashMap<&str, Value<'_>>,
            _expire_timeout: i32,
        ) -> u32 {
            self.next_id += 1;
            let urgency = match hints.get("urgency") {
                Some(Value::U8(u)) => *u,
                _ => 255,
            };
            self.log.lock().unwrap().push(format!(
                "notify {} {summary}: {body} (urgency {urgency})",
                self.next_id
            ));
            self.next_id
        }
        fn close_notification(&mut self, id: u32) {
            self.log.lock().unwrap().push(format!("close {id}"));
        }
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

    const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

    /// Set the battery level and announce what changed, as UPower does.
    async fn set_battery(service: &Connection, warning_level: u32, percentage: f64) {
        let iface = service
            .object_server()
            .interface::<_, FakeDevice>(DISPLAY_DEVICE)
            .await
            .unwrap();
        let mut device = iface.get_mut().await;
        let level_changed = device.warning_level != warning_level;
        device.percentage = percentage;
        device.warning_level = warning_level;
        let ctxt: &SignalContext = iface.signal_context();
        device.percentage_changed(ctxt).await.unwrap();
        if level_changed {
            device.warning_level_changed(ctxt).await.unwrap();
        }
    }

    fn wait_for(log: &Arc<Mutex<Vec<String>>>, len: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while log.lock().unwrap().len() < len && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        // Give unexpected extra calls a chance to show up.
        std::thread::sleep(Duration::from_millis(200));
        log.lock().unwrap().clone()
    }

    #[test]
    fn warns_once_per_level_and_clears_on_ac() {
        let (_bus, address) = private_bus();
        let log = Arc::new(Mutex::new(Vec::new()));

        let service = zbus::block_on(async {
            ConnectionBuilder::address(address.as_str())?
                .name("org.freedesktop.UPower")?
                .name("org.freedesktop.Notifications")?
                .serve_at("/org/freedesktop/UPower", FakeUPower)?
                .serve_at(
                    DISPLAY_DEVICE,
                    FakeDevice {
                        warning_level: 1,
                        percentage: 50.0,
                    },
                )?
                .serve_at(
                    "/org/freedesktop/Notifications",
                    FakeNotifications {
                        log: log.clone(),
                        next_id: 0,
                    },
                )?
                .build()
                .await
        })
        .unwrap();

        let watcher_address = address.clone();
        std::thread::spawn(move || {
            zbus::block_on(async {
                let conn = ConnectionBuilder::address(watcher_address.as_str())?
                    .build()
                    .await?;
                watch(&conn, &conn).await
            })
        });
        std::thread::sleep(Duration::from_millis(500));
        assert!(log.lock().unwrap().is_empty(), "warned at 50%");

        // Discharging through the thresholds; percentage updates within a
        // level must not repeat the warning.
        let steps: &[(u32, f64, &[&str])] = &[
            (
                3,
                10.0,
                &["notify 1 Low Battery: 10% battery remaining (urgency 1)"],
            ),
            (3, 9.0, &[]),
            (3, 8.0, &[]),
            (
                4,
                4.0,
                &[
                    "close 1",
                    "notify 2 Battery Almost Empty: Connect power now (urgency 2)",
                ],
            ),
            (4, 3.0, &[]),
            (
                5,
                1.0,
                &[
                    "close 2",
                    "notify 3 Battery is Empty: This device is about to hibernate (urgency 2)",
                ],
            ),
            // AC plugged in: UPower drops the level to none.
            (1, 1.0, &["close 3"]),
            (1, 5.0, &[]),
        ];
        let mut expected: Vec<String> = Vec::new();
        for (level, percentage, calls) in steps {
            zbus::block_on(set_battery(&service, *level, *percentage));
            expected.extend(calls.iter().map(|c| c.to_string()));
            assert_eq!(
                wait_for(&log, expected.len()),
                expected,
                "after WarningLevel {level} at {percentage}%"
            );
        }
    }

    #[test]
    fn warns_at_startup_when_already_low() {
        let (_bus, address) = private_bus();
        let log = Arc::new(Mutex::new(Vec::new()));
        let _service = zbus::block_on(async {
            ConnectionBuilder::address(address.as_str())?
                .name("org.freedesktop.UPower")?
                .name("org.freedesktop.Notifications")?
                .serve_at("/org/freedesktop/UPower", FakeUPower)?
                .serve_at(
                    DISPLAY_DEVICE,
                    FakeDevice {
                        warning_level: 4,
                        percentage: 4.0,
                    },
                )?
                .serve_at(
                    "/org/freedesktop/Notifications",
                    FakeNotifications {
                        log: log.clone(),
                        next_id: 0,
                    },
                )?
                .build()
                .await
        })
        .unwrap();
        std::thread::spawn(move || {
            zbus::block_on(async {
                let conn = ConnectionBuilder::address(address.as_str())?
                    .build()
                    .await?;
                watch(&conn, &conn).await
            })
        });
        assert_eq!(
            wait_for(&log, 1),
            ["notify 1 Battery Almost Empty: Connect power now (urgency 2)"]
        );
    }

    /// At login the notification server may start after regolith-powerd.
    /// A warning that could not be shown is retried on the next battery
    /// update, even if the level has not changed.
    #[test]
    fn retries_when_the_notification_server_starts_late() {
        let (_bus, address) = private_bus();
        let log = Arc::new(Mutex::new(Vec::new()));
        let upower = zbus::block_on(async {
            ConnectionBuilder::address(address.as_str())?
                .name("org.freedesktop.UPower")?
                .serve_at("/org/freedesktop/UPower", FakeUPower)?
                .serve_at(
                    DISPLAY_DEVICE,
                    FakeDevice {
                        warning_level: 3,
                        percentage: 10.0,
                    },
                )?
                .build()
                .await
        })
        .unwrap();
        let watcher_address = address.clone();
        std::thread::spawn(move || {
            zbus::block_on(async {
                let conn = ConnectionBuilder::address(watcher_address.as_str())?
                    .build()
                    .await?;
                watch(&conn, &conn).await
            })
        });
        std::thread::sleep(Duration::from_millis(500));

        let _notifications = zbus::block_on(async {
            ConnectionBuilder::address(address.as_str())?
                .name("org.freedesktop.Notifications")?
                .serve_at(
                    "/org/freedesktop/Notifications",
                    FakeNotifications {
                        log: log.clone(),
                        next_id: 0,
                    },
                )?
                .build()
                .await
        })
        .unwrap();
        zbus::block_on(set_battery(&upower, 3, 9.0));
        assert_eq!(
            wait_for(&log, 1),
            ["notify 1 Low Battery: 9% battery remaining (urgency 1)"]
        );
    }
}
