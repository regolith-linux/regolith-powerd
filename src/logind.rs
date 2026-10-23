//! systemd-logind inhibitor locks taken by this daemon.

use zbus::zvariant::OwnedFd;
use zbus::{dbus_proxy, Result};

pub const WHO: &str = "regolith-powerd";

#[dbus_proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Logind {
    #[allow(non_snake_case)]
    fn Inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;
}

pub async fn setup_logind_inhibits() -> Result<OwnedFd> {
    let connection = zbus::Connection::system().await?;
    let proxy = LogindProxy::new(&connection).await?;
    proxy
        .Inhibit(
            "handle-power-key:handle-lid-switch",
            WHO,
            "Power key action handled by regolith-powerd",
            "block",
        )
        .await
}

/// One named logind inhibitor lock slot. The lock is held while the slot
/// owns an fd; dropping the fd releases the inhibitor.
pub struct Lock {
    what: &'static str,
    fd: Option<OwnedFd>,
}

impl Lock {
    pub fn new(what: &'static str) -> Self {
        Self { what, fd: None }
    }

    /// Release the lock when `wanted` is false, take it when true and not yet
    /// held. Repeating the same state is a no-op.
    pub async fn sync(&mut self, proxy: &LogindProxy<'_>, wanted: bool) -> Result<()> {
        if !wanted {
            self.fd = None;
        } else if self.fd.is_none() {
            self.fd = Some(
                proxy
                    .Inhibit(self.what, WHO, "GNOME session inhibition", "block")
                    .await?,
            );
        }
        Ok(())
    }
}
