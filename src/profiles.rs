//! The power-saver profile on low battery.
//!
//! gnome-settings-daemon's power plugin holds power-profiles-daemon's
//! power-saver profile while UPower's display device is at a warning level
//! and `power-saver-profile-on-low-battery` is on, and releases the hold
//! when the level goes back to none (on AC power or charged). If the user
//! picks another profile meanwhile, power-profiles-daemon drops the hold and
//! it is not taken again until the battery is low anew.

use glib::g_log;
use std::time::Duration;
use zbus::export::futures_util::{stream, StreamExt};
use zbus::{dbus_proxy, Connection, Result};

use crate::battery::Level;
use crate::PowerSettings;

const PPD: &str = "org.freedesktop.UPower.PowerProfiles";
const UPOWER: &str = "org.freedesktop.UPower";
const REASON: &str = "Power saver profile when low on battery";
const APP_ID: &str = "org.regolith.powerd";

#[dbus_proxy(
    interface = "org.freedesktop.UPower.PowerProfiles",
    default_service = "org.freedesktop.UPower.PowerProfiles",
    default_path = "/org/freedesktop/UPower/PowerProfiles"
)]
trait PowerProfiles {
    fn hold_profile(&self, profile: &str, reason: &str, application_id: &str) -> zbus::Result<u32>;
    fn release_profile(&self, cookie: u32) -> zbus::Result<()>;
    #[dbus_proxy(signal)]
    fn profile_released(&self, cookie: u32) -> zbus::Result<()>;
}

#[dbus_proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/devices/DisplayDevice"
)]
trait DisplayDevice {
    #[dbus_proxy(property)]
    fn warning_level(&self) -> zbus::Result<u32>;
}

enum Event {
    Battery,
    Setting,
    Released(u32),
    /// power-profiles-daemon appeared, restarted or went away: its holds
    /// went with the old instance.
    Daemon,
}

struct Saver {
    cookie: Option<u32>,
    /// The user replaced the hold with a profile of their own while low.
    declined: bool,
    low: bool,
    enabled: bool,
}

impl Saver {
    async fn reconcile(&mut self, ppd: &PowerProfilesProxy<'_>) {
        let want = self.low && self.enabled && !self.declined;
        match (want, self.cookie) {
            (true, None) => match ppd.hold_profile("power-saver", REASON, APP_ID).await {
                Ok(cookie) => self.cookie = Some(cookie),
                // Retried when the daemon (re)appears.
                Err(e) => g_log!(glib::LogLevel::Debug, "Cannot hold power-saver: {e}"),
            },
            (false, Some(cookie)) => {
                self.cookie = None;
                if let Err(e) = ppd.release_profile(cookie).await {
                    g_log!(glib::LogLevel::Debug, "Cannot release power-saver: {e}");
                }
            }
            _ => {}
        }
    }
}

/// Hold and release the profile until the connection to the system bus
/// ends. `enabled` reads `power-saver-profile-on-low-battery`, and
/// `setting_changes` gets a message whenever it changes.
async fn watch(
    system: &Connection,
    enabled: &impl Fn() -> bool,
    setting_changes: &async_channel::Receiver<()>,
) -> Result<()> {
    let ppd = PowerProfilesProxy::builder(system)
        .cache_properties(zbus::CacheProperties::No)
        .build()
        .await?;
    let device = DisplayDeviceProxy::builder(system)
        .cache_properties(zbus::CacheProperties::No)
        .build()
        .await?;
    let device_props = zbus::fdo::PropertiesProxy::builder(system)
        .destination(UPOWER)?
        .path("/org/freedesktop/UPower/devices/DisplayDevice")?
        .build()
        .await?;
    let dbus = zbus::fdo::DBusProxy::new(system).await?;

    let battery = device_props
        .receive_properties_changed()
        .await?
        .map(|_| Event::Battery);
    let upower_restarts = dbus
        .receive_name_owner_changed_with_args(&[(0, UPOWER)])
        .await?
        .map(|_| Event::Battery);
    let daemon = dbus
        .receive_name_owner_changed_with_args(&[(0, PPD)])
        .await?
        .map(|_| Event::Daemon);
    let released = ppd
        .receive_profile_released()
        .await?
        .map(|signal| Event::Released(signal.args().map(|a| a.cookie).unwrap_or_default()));
    let settings_stream = setting_changes.clone().map(|_| Event::Setting);
    let mut events = Box::pin(stream::select(
        stream::select(battery, upower_restarts),
        stream::select(stream::select(daemon, released), settings_stream),
    ));

    let mut saver = Saver {
        cookie: None,
        declined: false,
        low: false,
        enabled: enabled(),
    };
    let mut event = Event::Battery;
    loop {
        match event {
            Event::Battery => match device.warning_level().await {
                Ok(level) => {
                    saver.low = Level::from_upower(level) != Level::None;
                    if !saver.low {
                        saver.declined = false;
                    }
                }
                // UPower is gone (or restarting): keep the last known
                // level, so that a restart with the battery still low keeps
                // the one hold. The level is read again when UPower is back.
                // Without UPower from the start there is no battery to be
                // low, which is the initial state.
                Err(e) => g_log!(glib::LogLevel::Debug, "Cannot read the battery level: {e}"),
            },
            Event::Setting => {
                let enabled = enabled();
                if enabled && !saver.enabled {
                    saver.declined = false;
                }
                saver.enabled = enabled;
            }
            Event::Released(cookie) => {
                if saver.cookie == Some(cookie) {
                    saver.cookie = None;
                    saver.declined = saver.low;
                }
            }
            Event::Daemon => saver.cookie = None,
        }
        saver.reconcile(&ppd).await;
        event = match events.next().await {
            Some(e) => e,
            None => return Ok(()),
        };
    }
}

/// Run the power-saver hold for the lifetime of the daemon.
pub async fn run_power_saver() {
    let settings = PowerSettings::new();
    let (send, setting_changes) = async_channel::unbounded();
    settings.connect_power_saver_profile_on_low_battery_changed(move |_| {
        let _ = send.try_send(());
    });
    loop {
        let result = async {
            // The hold lives as long as this connection.
            let system = Connection::system().await?;
            let enabled = || settings.power_saver_profile_on_low_battery();
            watch(&system, &enabled, &setting_changes).await
        }
        .await;
        if let Err(e) = result {
            g_log!(
                glib::LogLevel::Warning,
                "Power-saver profile unavailable: {e}"
            );
        }
        glib::timeout_future(Duration::from_secs(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use zbus::export::futures_util::{future, pin_mut};
    use zbus::{dbus_interface, ConnectionBuilder, SignalContext};

    const PPD_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";
    const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

    /// The display device of a fake UPower.
    struct FakeDevice {
        warning_level: u32,
    }

    #[dbus_interface(name = "org.freedesktop.UPower.Device")]
    impl FakeDevice {
        #[dbus_interface(property)]
        fn warning_level(&self) -> u32 {
            self.warning_level
        }
    }

    /// The live holds of a fake power-profiles-daemon, and its next cookie.
    #[derive(Default)]
    struct Holds {
        live: Vec<(u32, String, String)>,
        next: u32,
    }

    struct FakePpd(Arc<Mutex<Holds>>);

    #[dbus_interface(name = "org.freedesktop.UPower.PowerProfiles")]
    impl FakePpd {
        fn hold_profile(&self, profile: &str, reason: &str, _application_id: &str) -> u32 {
            let mut holds = self.0.lock().unwrap();
            holds.next += 1;
            let cookie = holds.next;
            holds
                .live
                .push((cookie, profile.to_owned(), reason.to_owned()));
            cookie
        }
        fn release_profile(&self, cookie: u32) -> zbus::fdo::Result<()> {
            let mut holds = self.0.lock().unwrap();
            let before = holds.live.len();
            holds.live.retain(|(c, _, _)| *c != cookie);
            if holds.live.len() == before {
                return Err(zbus::fdo::Error::InvalidArgs("no such hold".into()));
            }
            Ok(())
        }
        #[dbus_interface(signal)]
        async fn profile_released(ctxt: &SignalContext<'_>, cookie: u32) -> zbus::Result<()>;
    }

    struct Bus(Child);

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A private bus that stands in for the system bus.
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

    async fn start_upower(address: &str, warning_level: u32) -> Connection {
        ConnectionBuilder::address(address)
            .unwrap()
            .name(UPOWER)
            .unwrap()
            .serve_at(DISPLAY_DEVICE, FakeDevice { warning_level })
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    async fn start_ppd(address: &str, holds: &Arc<Mutex<Holds>>) -> Connection {
        ConnectionBuilder::address(address)
            .unwrap()
            .name(PPD)
            .unwrap()
            .serve_at(PPD_PATH, FakePpd(holds.clone()))
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    /// Set the warning level and announce it, as UPower does.
    async fn set_level(upower: &Connection, warning_level: u32) {
        let iface = upower
            .object_server()
            .interface::<_, FakeDevice>(DISPLAY_DEVICE)
            .await
            .unwrap();
        iface.get_mut().await.warning_level = warning_level;
        iface
            .get()
            .await
            .warning_level_changed(iface.signal_context())
            .await
            .unwrap();
    }

    /// The user picks a profile: power-profiles-daemon drops every hold and
    /// tells the holders.
    async fn user_picks_a_profile(ppd: &Connection, holds: &Arc<Mutex<Holds>>) {
        let dropped: Vec<u32> = holds.lock().unwrap().live.drain(..).map(|h| h.0).collect();
        let ctxt = SignalContext::new(ppd, PPD_PATH).unwrap();
        for cookie in dropped {
            FakePpd::profile_released(&ctxt, cookie).await.unwrap();
        }
    }

    /// Wait until the live holds are `expected` power-saver holds, then
    /// check that they stay so for a moment. Returns their cookies.
    async fn expect_holds(holds: &Arc<Mutex<Holds>>, expected: usize, when: &str) -> Vec<u32> {
        let count = || holds.lock().unwrap().live.len();
        for _ in 0..150 {
            if count() == expected {
                break;
            }
            glib::timeout_future(Duration::from_millis(20)).await;
        }
        glib::timeout_future(Duration::from_millis(300)).await;
        let live = holds.lock().unwrap().live.clone();
        assert_eq!(live.len(), expected, "{when}: holds {live:?}");
        for (_, profile, reason) in &live {
            assert_eq!((profile.as_str(), reason.as_str()), ("power-saver", REASON));
        }
        live.into_iter().map(|h| h.0).collect()
    }

    #[test]
    fn holds_power_saver_while_the_battery_is_low() {
        let (_bus, address) = private_bus();
        glib::MainContext::new().block_on(async {
            let holds = Arc::new(Mutex::new(Holds::default()));
            let upower = start_upower(&address, 3).await;
            let ppd = start_ppd(&address, &holds).await;
            let client = ConnectionBuilder::address(address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let setting = Arc::new(AtomicBool::new(true));
            let enabled = {
                let setting = setting.clone();
                move || setting.load(Ordering::SeqCst)
            };
            let (setting_changed, setting_changes) = async_channel::unbounded();
            let watcher = watch(&client, &enabled, &setting_changes);
            let sequence = async {
                // Low at start; the level getting worse keeps the one hold.
                expect_holds(&holds, 1, "low at start").await;
                set_level(&upower, 4).await;
                expect_holds(&holds, 1, "critical").await;
                // Plugged in.
                set_level(&upower, 1).await;
                expect_holds(&holds, 0, "on AC").await;

                // Low again; the user picks another profile, which drops
                // the hold: it is not taken again until the next time the
                // battery gets low.
                set_level(&upower, 3).await;
                expect_holds(&holds, 1, "low again").await;
                user_picks_a_profile(&ppd, &holds).await;
                expect_holds(&holds, 0, "after the user's choice").await;
                set_level(&upower, 4).await;
                expect_holds(&holds, 0, "critical after the user's choice").await;
                set_level(&upower, 1).await;
                expect_holds(&holds, 0, "charged after the user's choice").await;
                set_level(&upower, 3).await;
                expect_holds(&holds, 1, "low after charging").await;

                // The setting turned off and on while low.
                setting.store(false, Ordering::SeqCst);
                setting_changed.send(()).await.unwrap();
                expect_holds(&holds, 0, "setting off").await;
                setting.store(true, Ordering::SeqCst);
                setting_changed.send(()).await.unwrap();
                expect_holds(&holds, 1, "setting on").await;

                // power-profiles-daemon restarts: the hold is taken again.
                drop(ppd);
                holds.lock().unwrap().live.clear();
                let _ppd = start_ppd(&address, &holds).await;
                expect_holds(&holds, 1, "after a power-profiles-daemon restart").await;

                // UPower restarts with the battery still low: still one hold.
                drop(upower);
                glib::timeout_future(Duration::from_millis(300)).await;
                let _upower = start_upower(&address, 3).await;
                expect_holds(&holds, 1, "after a UPower restart").await;
            };
            let watchdog = glib::timeout_future(Duration::from_secs(30));
            pin_mut!(watcher, sequence, watchdog);
            match future::select(watcher, future::select(sequence, watchdog)).await {
                future::Either::Left((result, _)) => panic!("watcher ended: {result:?}"),
                future::Either::Right((future::Either::Right(_), _)) => panic!("timed out"),
                future::Either::Right((future::Either::Left(_), _)) => {}
            }
        });
    }
}
