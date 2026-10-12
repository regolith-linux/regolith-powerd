//! systemd-logind inhibitor locks taken by this daemon.

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

pub async fn setup_logind_inhibits() -> Result<OwnedFd> {
    let connection = zbus::Connection::system().await?;
    let proxy = LogindProxy::new(&connection).await?;
    proxy
        .Inhibit(
            "handle-power-key:handle-lid-switch",
            "regolith-powerd",
            "Power key action handled by regolith-powerd",
            "block",
        )
        .await
}

/// Reconcile one named inhibitor lock slot with its desired state: releases
/// the lock (by dropping the fd) when `inhibited` is false, takes one when
/// true and the slot is still empty. Repeated reconciliations of the same
/// state are no-ops. Dropping the returned fd at any later point releases
/// the inhibitor.
pub async fn reconcile_lock(
    lock: &mut Option<OwnedFd>,
    proxy: &LogindProxy<'_>,
    inhibited: bool,
    what: &str,
) -> Result<()> {
    if !inhibited {
        *lock = None;
    } else if lock.is_none() {
        *lock = Some(
            proxy
                .Inhibit(what, "regolith-powerd", "GNOME session inhibition", "block")
                .await?,
        );
    }
    Ok(())
}
