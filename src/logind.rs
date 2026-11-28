//! systemd-logind inhibitor locks taken by this daemon.

use std::os::fd::AsRawFd;
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

    /// Sent before the system sleeps (`start` true) and once it has resumed.
    #[dbus_proxy(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// Keep an inhibitor fd out of the programs this daemon starts (swayidle,
/// which is started again whenever the idle settings change, and the
/// commands it runs). A copy there would hold the lock after the daemon released it, or
/// after the daemon exited.
fn close_on_exec(fd: OwnedFd) -> OwnedFd {
    let raw = fd.as_raw_fd();
    // SAFETY: fcntl(2) on an fd this function owns; F_GETFD/F_SETFD only
    // read and set its flags.
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    fd
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
        .map(close_on_exec)
}

/// One named logind inhibitor lock slot. The lock is held while the slot
/// owns an fd; dropping the fd releases the inhibitor.
pub struct Lock {
    what: &'static str,
    mode: &'static str,
    fd: Option<OwnedFd>,
}

impl Lock {
    /// `mode` is logind's lock mode: `block` refuses every request, while
    /// `block-weak` only holds back requests nobody explicitly asked for.
    pub fn new(what: &'static str, mode: &'static str) -> Self {
        Self {
            what,
            mode,
            fd: None,
        }
    }

    /// Whether the lock is held.
    pub fn is_held(&self) -> bool {
        self.fd.is_some()
    }

    /// Release the lock when `wanted` is false, take it when true and not yet
    /// held. Repeating the same state is a no-op.
    pub async fn sync(&mut self, proxy: &LogindProxy<'_>, wanted: bool) -> Result<()> {
        if !wanted {
            self.fd = None;
        } else if self.fd.is_none() {
            self.fd = Some(close_on_exec(
                proxy
                    .Inhibit(self.what, WHO, "GNOME session inhibition", self.mode)
                    .await?,
            ));
        }
        Ok(())
    }
}
