//! Power profiles: the power-saver hold on low battery, and one remembered
//! profile per power source.
//!
//! gnome-settings-daemon's power plugin holds power-profiles-daemon's
//! power-saver profile while UPower's display device is at a warning level
//! and `power-saver-profile-on-low-battery` is on, and releases the hold
//! when the level goes back to none (on AC power or charged). If the user
//! picks another profile meanwhile, power-profiles-daemon drops the hold and
//! it is not taken again until the battery is low anew.
//!
//! Besides, the profile the user picked last on AC and on battery is put
//! back into effect when the power source changes (see `remembered.rs`).
//! One loop does both, so that a remembered profile is never set over the
//! hold, which setting a profile would drop.

use glib::g_log;
use std::collections::HashMap;
use std::time::Duration;
use zbus::export::futures_util::{stream, StreamExt};
use zbus::fdo::{DBusProxy, PropertiesProxy};
use zbus::names::{BusName, InterfaceName};
use zbus::zvariant::OwnedValue;
use zbus::{dbus_proxy, Connection, Result};

use crate::battery::Level;
use crate::power_source::UPowerSourceProxy;
use crate::remembered::{Remembered, Service, Source, Store};
use crate::PowerSettings;

const PPD: &str = "org.freedesktop.UPower.PowerProfiles";
const PPD_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";
const UPOWER: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
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
    #[dbus_proxy(property)]
    fn set_active_profile(&self, profile: &str) -> zbus::Result<()>;
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
    /// The power source or the battery level may have changed.
    Battery,
    /// power-profiles-daemon's profile or holds may have changed.
    Profiles,
    Setting,
    Released(u32),
    /// An instance of power-profiles-daemon went away, and its holds with it.
    DaemonLeft,
}

struct Saver {
    cookie: Option<u32>,
    /// The user replaced the hold with a profile of their own while low.
    declined: bool,
    low: bool,
    enabled: bool,
}

impl Saver {
    /// The battery is at a warning level, or not.
    fn set_low(&mut self, low: bool) {
        self.low = low;
        if !low {
            self.declined = false;
        }
    }

    /// `power-saver-profile-on-low-battery` changed.
    fn set_enabled(&mut self, enabled: bool) {
        if enabled && !self.enabled {
            self.declined = false;
        }
        self.enabled = enabled;
    }

    /// power-profiles-daemon dropped the hold with `cookie`.
    fn released(&mut self, cookie: u32) {
        if self.cookie == Some(cookie) {
            self.cookie = None;
            self.declined = self.low;
        }
    }

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

/// Read power-profiles-daemon: `None` when it does not run. One `GetAll`
/// reads the profile and the holds of the same moment.
async fn read_service(
    dbus: &DBusProxy<'_>,
    properties: &PropertiesProxy<'_>,
) -> Result<Option<Service>> {
    type Entries = Vec<HashMap<String, OwnedValue>>;

    let owner = match dbus.get_name_owner(BusName::from_static_str(PPD)?).await {
        Ok(owner) => owner.to_string(),
        Err(zbus::fdo::Error::NameHasNoOwner(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut all = properties
        .get_all(InterfaceName::from_static_str(PPD)?)
        .await?;
    let mut take = |name: &str| {
        all.remove(name)
            .ok_or_else(|| zbus::Error::Failure(format!("No {name} property")))
    };
    let active = String::try_from(take("ActiveProfile")?)?;
    let holds = Entries::try_from(take("ActiveProfileHolds")?)?.len();
    let offered = Entries::try_from(take("Profiles")?)?
        .into_iter()
        .filter_map(|mut entry| String::try_from(entry.remove("Profile")?).ok())
        .collect();
    Ok(Some(Service {
        owner,
        active,
        holds,
        offered,
    }))
}

/// Hold and release the profile, and put the remembered profiles into
/// effect, until the connection to the system bus ends. `enabled` reads
/// `power-saver-profile-on-low-battery`, and `setting_changes` gets a
/// message whenever it changes.
async fn watch(
    system: &Connection,
    enabled: &impl Fn() -> bool,
    setting_changes: &async_channel::Receiver<()>,
    remembered: &mut Remembered,
) -> Result<()> {
    let ppd = PowerProfilesProxy::builder(system)
        .cache_properties(zbus::CacheProperties::No)
        .build()
        .await?;
    let ppd_props = PropertiesProxy::builder(system)
        .destination(PPD)?
        .path(PPD_PATH)?
        .build()
        .await?;
    let upower = UPowerSourceProxy::builder(system)
        .cache_properties(zbus::CacheProperties::No)
        .build()
        .await?;
    let upower_props = PropertiesProxy::builder(system)
        .destination(UPOWER)?
        .path(UPOWER_PATH)?
        .build()
        .await?;
    let device = DisplayDeviceProxy::builder(system)
        .cache_properties(zbus::CacheProperties::No)
        .build()
        .await?;
    let device_props = PropertiesProxy::builder(system)
        .destination(UPOWER)?
        .path("/org/freedesktop/UPower/devices/DisplayDevice")?
        .build()
        .await?;
    let dbus = DBusProxy::new(system).await?;

    // Subscribe before reading, so that a change in between is not lost.
    let mut events = stream::select_all([
        upower_props
            .receive_properties_changed()
            .await?
            .map(|_| Event::Battery)
            .boxed_local(),
        device_props
            .receive_properties_changed()
            .await?
            .map(|_| Event::Battery)
            .boxed_local(),
        dbus.receive_name_owner_changed_with_args(&[(0, UPOWER)])
            .await?
            .map(|_| Event::Battery)
            .boxed_local(),
        ppd_props
            .receive_properties_changed()
            .await?
            .map(|_| Event::Profiles)
            .boxed_local(),
        dbus.receive_name_owner_changed_with_args(&[(0, PPD)])
            .await?
            .map(|signal| match signal.args() {
                Ok(args) if args.old_owner().is_some() => Event::DaemonLeft,
                _ => Event::Profiles,
            })
            .boxed_local(),
        ppd.receive_profile_released()
            .await?
            .map(|signal| Event::Released(signal.args().map(|a| a.cookie).unwrap_or_default()))
            .boxed_local(),
        setting_changes
            .clone()
            .map(|_| Event::Setting)
            .boxed_local(),
    ]);

    let mut saver = Saver {
        cookie: None,
        declined: false,
        low: false,
        enabled: enabled(),
    };
    // The source is read only when UPower says it changed, so that a pick
    // announced before a change of source counts for the source it was
    // made on.
    let mut source = Source::Ac;
    let mut event = Event::Battery;
    loop {
        match event {
            Event::Battery => {
                match device.warning_level().await {
                    Ok(level) => saver.set_low(Level::from_upower(level) != Level::None),
                    // UPower is gone (or restarting): keep the last known
                    // level, so that a restart with the battery still low
                    // keeps the one hold. The level is read again when UPower
                    // is back. Without UPower from the start there is no
                    // battery to be low, which is the initial state.
                    Err(e) => g_log!(glib::LogLevel::Debug, "Cannot read the battery level: {e}"),
                }
                // Without UPower the machine counts as on AC, as in
                // `power_source`.
                source = Source::from_on_battery(upower.on_battery().await.unwrap_or(false));
            }
            Event::Profiles => {}
            Event::Setting => saver.set_enabled(enabled()),
            Event::Released(cookie) => saver.released(cookie),
            Event::DaemonLeft => saver.cookie = None,
        }
        match read_service(&dbus, &ppd_props).await {
            Ok(service) => remembered.observe(source, service),
            Err(e) => g_log!(glib::LogLevel::Debug, "Cannot read the power profiles: {e}"),
        }

        let cookie = saver.cookie;
        saver.reconcile(&ppd).await;
        // A hold just taken or released changes the profile and the holds:
        // decide on the next pass, with them read, so as not to set a
        // profile over a fresh hold.
        if saver.cookie == cookie {
            if let Some(profile) = remembered.wanted(source).map(str::to_owned) {
                match ppd.set_active_profile(&profile).await {
                    Ok(()) => remembered.applied(&profile),
                    Err(e) => g_log!(
                        glib::LogLevel::Debug,
                        "Cannot set the {profile} profile: {e}"
                    ),
                }
            }
        }

        event = match events.next().await {
            Some(e) => e,
            None => return Ok(()),
        };
    }
}

/// Run the power profiles for the lifetime of the daemon: the remembered
/// profile of each power source and, with `hold_on_low_battery`, the
/// power-saver hold while the battery is low.
pub async fn run(hold_on_low_battery: bool) {
    let (send, setting_changes) = async_channel::unbounded();
    let settings = hold_on_low_battery.then(|| {
        let settings = PowerSettings::new();
        settings.connect_power_saver_profile_on_low_battery_changed(move |_| {
            let _ = send.try_send(());
        });
        settings
    });
    let enabled = || {
        settings
            .as_ref()
            .is_some_and(PowerSettings::power_saver_profile_on_low_battery)
    };
    // What is remembered outlives a lost connection to the system bus.
    let mut remembered = Remembered::new(Store::load());
    loop {
        let result = async {
            // The hold lives as long as this connection.
            let system = Connection::system().await?;
            watch(&system, &enabled, &setting_changes, &mut remembered).await
        }
        .await;
        if let Err(e) = result {
            g_log!(glib::LogLevel::Warning, "Power profiles unavailable: {e}");
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
    use zbus::zvariant::Value;
    use zbus::{dbus_interface, ConnectionBuilder, SignalContext};

    const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

    /// The power source of a fake UPower.
    struct FakeSource {
        on_battery: bool,
    }

    #[dbus_interface(name = "org.freedesktop.UPower")]
    impl FakeSource {
        #[dbus_interface(property)]
        fn on_battery(&self) -> bool {
            self.on_battery
        }
    }

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

    /// A fake power-profiles-daemon with a profile and holds, for the
    /// remembered profiles.
    struct FakeProfiles {
        chosen: String,
        holds: Vec<(u32, String)>,
        next: u32,
    }

    impl FakeProfiles {
        fn new(chosen: &str) -> Self {
            Self {
                chosen: chosen.to_owned(),
                holds: Vec::new(),
                next: 0,
            }
        }

        /// The held profile, power-saver first, or else the chosen one.
        fn active(&self) -> &str {
            ["power-saver", "performance"]
                .into_iter()
                .find(|p| self.holds.iter().any(|(_, held)| held == p))
                .unwrap_or(&self.chosen)
        }

        async fn announce(&self, ctxt: &SignalContext<'_>) {
            self.active_profile_changed(ctxt).await.unwrap();
            self.active_profile_holds_changed(ctxt).await.unwrap();
        }
    }

    /// Entries of `Profiles` or `ActiveProfileHolds` for `profiles`.
    fn entries<'a>(profiles: impl Iterator<Item = &'a str>) -> Vec<HashMap<String, OwnedValue>> {
        profiles
            .map(|p| HashMap::from([("Profile".to_owned(), Value::from(p).into())]))
            .collect()
    }

    #[dbus_interface(name = "org.freedesktop.UPower.PowerProfiles")]
    impl FakeProfiles {
        async fn hold_profile(
            &mut self,
            profile: String,
            _reason: &str,
            _application_id: &str,
            #[zbus(signal_context)] ctxt: SignalContext<'_>,
        ) -> u32 {
            self.next += 1;
            self.holds.push((self.next, profile));
            self.announce(&ctxt).await;
            self.next
        }
        async fn release_profile(
            &mut self,
            cookie: u32,
            #[zbus(signal_context)] ctxt: SignalContext<'_>,
        ) {
            self.holds.retain(|(c, _)| *c != cookie);
            self.announce(&ctxt).await;
        }
        #[dbus_interface(property)]
        fn active_profile(&self) -> String {
            self.active().to_owned()
        }
        /// Setting a profile drops every hold.
        #[dbus_interface(property)]
        fn set_active_profile(&mut self, profile: String) {
            self.holds.clear();
            self.chosen = profile;
        }
        #[dbus_interface(property)]
        fn profiles(&self) -> Vec<HashMap<String, OwnedValue>> {
            entries(["power-saver", "balanced", "performance"].into_iter())
        }
        #[dbus_interface(property)]
        fn active_profile_holds(&self) -> Vec<HashMap<String, OwnedValue>> {
            entries(self.holds.iter().map(|(_, p)| p.as_str()))
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
            .serve_at(UPOWER_PATH, FakeSource { on_battery: false })
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

    /// Plug in or unplug, as UPower announces it.
    async fn set_on_battery(upower: &Connection, on_battery: bool) {
        let iface = upower
            .object_server()
            .interface::<_, FakeSource>(UPOWER_PATH)
            .await
            .unwrap();
        iface.get_mut().await.on_battery = on_battery;
        iface
            .get()
            .await
            .on_battery_changed(iface.signal_context())
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
            let mut remembered = Remembered::new(Store::default());
            let watcher = watch(&client, &enabled, &setting_changes, &mut remembered);
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

    /// Wait until the fake's profile is `expected`, then check that it
    /// stays so for a moment, and that `holds` holds are in place.
    async fn expect_profile(ppd: &Connection, expected: &str, holds: usize, when: &str) {
        let iface = ppd
            .object_server()
            .interface::<_, FakeProfiles>(PPD_PATH)
            .await
            .unwrap();
        for _ in 0..150 {
            if iface.get().await.active() == expected {
                break;
            }
            glib::timeout_future(Duration::from_millis(20)).await;
        }
        glib::timeout_future(Duration::from_millis(300)).await;
        let fake = iface.get().await;
        assert_eq!(fake.active(), expected, "{when}");
        assert_eq!(fake.holds.len(), holds, "{when}: holds");
    }

    #[test]
    fn switches_to_the_profile_remembered_for_the_power_source() {
        let (_bus, address) = private_bus();
        glib::MainContext::new().block_on(async {
            let upower = start_upower(&address, 1).await;
            let ppd = ConnectionBuilder::address(address.as_str())
                .unwrap()
                .name(PPD)
                .unwrap()
                .serve_at(PPD_PATH, FakeProfiles::new("balanced"))
                .unwrap()
                .build()
                .await
                .unwrap();
            let client = ConnectionBuilder::address(address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let user = PowerProfilesProxy::new(&client).await.unwrap();
            let (_setting_changed, setting_changes) = async_channel::unbounded();
            let mut remembered = Remembered::new(Store::default());
            let watcher = watch(&client, &|| true, &setting_changes, &mut remembered);
            let sequence = async {
                // The first time on each source, nothing is set.
                expect_profile(&ppd, "balanced", 0, "at start").await;
                user.set_active_profile("performance").await.unwrap();
                expect_profile(&ppd, "performance", 0, "picked on AC").await;
                set_on_battery(&upower, true).await;
                expect_profile(&ppd, "performance", 0, "first time on battery").await;
                user.set_active_profile("power-saver").await.unwrap();
                expect_profile(&ppd, "power-saver", 0, "picked on battery").await;

                // Each source gets its pick back.
                set_on_battery(&upower, false).await;
                expect_profile(&ppd, "performance", 0, "plugged in").await;
                set_on_battery(&upower, true).await;
                expect_profile(&ppd, "power-saver", 0, "unplugged").await;

                // Nothing is set over another program's hold, until it ends.
                let cookie = user
                    .hold_profile("power-saver", "test", "test")
                    .await
                    .unwrap();
                set_on_battery(&upower, false).await;
                expect_profile(&ppd, "power-saver", 1, "plugged in while held").await;
                user.release_profile(cookie).await.unwrap();
                expect_profile(&ppd, "performance", 0, "hold released").await;
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
