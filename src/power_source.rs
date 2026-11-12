//! Follow the power source through UPower's `OnBattery`, as
//! gnome-settings-daemon's power plugin does, so the idle timers of the
//! source in use are armed and re-armed when the machine is plugged in or
//! unplugged.

use glib::g_log;
use std::time::Duration;
use zbus::export::futures_util::{stream, StreamExt};
use zbus::{dbus_proxy, Connection, Result};

use crate::IdleState;

const UPOWER: &str = "org.freedesktop.UPower";

#[dbus_proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
pub trait UPowerSource {
    #[dbus_proxy(property)]
    fn on_battery(&self) -> zbus::Result<bool>;
}

async fn watch(system: &Connection, idle: &IdleState) -> Result<()> {
    let upower = UPowerSourceProxy::builder(system)
        .cache_properties(zbus::CacheProperties::No)
        .build()
        .await?;
    let properties = zbus::fdo::PropertiesProxy::builder(system)
        .destination(UPOWER)?
        .path("/org/freedesktop/UPower")?
        .build()
        .await?;
    let dbus = zbus::fdo::DBusProxy::new(system).await?;
    // Subscribe before reading, so that a change in between is not lost. A
    // restarted UPower is re-read as well.
    let changes = properties.receive_properties_changed().await?.map(|_| ());
    let owners = dbus
        .receive_name_owner_changed_with_args(&[(0, UPOWER)])
        .await?
        .map(|_| ());
    let mut updates = stream::select(changes, owners);
    loop {
        // Without UPower (a desktop without one) the source is unknown,
        // which gets the AC settings, as `on_ac_power` reports.
        let on_battery = upower.on_battery().await.unwrap_or(false);
        idle.update(|c| c.on_battery = on_battery);
        if updates.next().await.is_none() {
            return Ok(());
        }
    }
}

/// Track the power source for the lifetime of the daemon.
pub async fn run(idle: IdleState) {
    loop {
        let result = async {
            let system = Connection::system().await?;
            watch(&system, &idle).await
        }
        .await;
        if let Err(e) = result {
            g_log!(
                glib::LogLevel::Warning,
                "Cannot follow the power source: {e}"
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
    use zbus::export::futures_util::{future, pin_mut};
    use zbus::{dbus_interface, ConnectionBuilder};

    struct FakeUPower {
        on_battery: bool,
    }

    #[dbus_interface(name = "org.freedesktop.UPower")]
    impl FakeUPower {
        #[dbus_interface(property)]
        fn on_battery(&self) -> bool {
            self.on_battery
        }
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

    async fn start_upower(address: &str, on_battery: bool) -> Connection {
        ConnectionBuilder::address(address)
            .unwrap()
            .name(UPOWER)
            .unwrap()
            .serve_at("/org/freedesktop/UPower", FakeUPower { on_battery })
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    async fn set_on_battery(upower: &Connection, on_battery: bool) {
        let iface = upower
            .object_server()
            .interface::<_, FakeUPower>("/org/freedesktop/UPower")
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

    /// Wait until the idle context says `on_battery`, for at most 5 s.
    async fn expect_on_battery(idle: &IdleState, on_battery: bool) {
        for _ in 0..250 {
            if idle.get().on_battery == on_battery {
                return;
            }
            glib::timeout_future(Duration::from_millis(20)).await;
        }
        panic!("power source never became on_battery={on_battery}");
    }

    #[test]
    fn follows_on_battery_and_upower_restarts() {
        let (_bus, address) = private_bus();
        glib::MainContext::new().block_on(async {
            let upower = start_upower(&address, true).await;
            let client = ConnectionBuilder::address(address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let (send, reloads) = async_channel::bounded(1);
            let idle = IdleState::new(send);
            let watcher = watch(&client, &idle);
            let sequence = async {
                // Read at start.
                expect_on_battery(&idle, true).await;
                assert_eq!(reloads.try_recv(), Ok(()));
                // Plugged in, unplugged.
                set_on_battery(&upower, false).await;
                expect_on_battery(&idle, false).await;
                assert_eq!(reloads.try_recv(), Ok(()));
                set_on_battery(&upower, true).await;
                expect_on_battery(&idle, true).await;
                assert_eq!(reloads.try_recv(), Ok(()));
                // An update that leaves the source as it is doesn't re-arm.
                set_on_battery(&upower, true).await;
                glib::timeout_future(Duration::from_millis(300)).await;
                assert!(reloads.try_recv().is_err());
                // Without UPower the machine counts as on AC.
                drop(upower);
                expect_on_battery(&idle, false).await;
                // A restarted UPower is read again.
                let _upower = start_upower(&address, true).await;
                expect_on_battery(&idle, true).await;
            };
            let watchdog = glib::timeout_future(Duration::from_secs(20));
            pin_mut!(watcher, sequence, watchdog);
            match future::select(watcher, future::select(sequence, watchdog)).await {
                future::Either::Left((result, _)) => panic!("watcher ended: {result:?}"),
                future::Either::Right((future::Either::Right(_), _)) => panic!("timed out"),
                future::Either::Right((future::Either::Left(_), _)) => {}
            }
        });
    }
}
