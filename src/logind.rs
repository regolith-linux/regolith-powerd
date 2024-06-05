use zbus::blocking::Connection;
use zbus::zvariant::OwnedFd;
use zbus::{dbus_proxy, Result};

#[dbus_proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Logind {
    #[allow(non_snake_case)]
    fn Inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;
}

pub fn setup_logind_inhibits() -> Result<OwnedFd> {
    let connection = Connection::system()?;
    let proxy = LogindProxyBlocking::new(&connection)?;
    let fd = proxy.Inhibit(
        "handle-power-key:handle-lid-switch",
        "regolith-powerd",
        "Power key action handled by regolith-powerd",
        "block",
    )?;
    Ok(fd)
}
