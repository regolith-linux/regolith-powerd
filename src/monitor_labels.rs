//! The monitor number labels of Settings › Displays, for the Wayland session.
//!
//! While its window is active, the Displays panel asks the shell to show each
//! monitor's number on that monitor, so the monitors on the screen can be
//! matched with the numbered monitors in the panel. It calls
//! `ShowMonitorLabels` with the connector names mapped to the numbers, and
//! `HideMonitorLabels` when the window is no longer active, on
//! `org.gnome.Shell` at `/org/gnome/Shell`. In the sway session this daemon
//! owns that name for the end-session dialog (see `end_session`), so it
//! answers these two methods too, and passes them on to regolith-displayd,
//! which draws the labels. displayd serves the same two methods, with the
//! same arguments, on its own bus name at [`DISPLAYD_PATH`]; that interface
//! is internal to Regolith.
//!
//! As in GNOME Shell, the labels belong to the client that showed them: only
//! that client can hide them or show others, and they are hidden when it
//! leaves the bus, so a panel that exits or crashes does not leave them
//! behind. If displayd is not running, there are no labels and the panel
//! notices nothing.
//!
//! The calls to displayd are made from a thread of their own, in order, so a
//! slow displayd cannot hold up the end-session dialog. If several are
//! waiting, only the last is made: each one replaces what the previous one
//! showed.

use glib::g_log;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use zbus::export::futures_util::StreamExt;
use zbus::fdo;
use zbus::names::BusName;
use zbus::zvariant::OwnedValue;
use zbus::{dbus_interface, CacheProperties, Connection, MessageHeader, MethodFlags, ProxyBuilder};

const PATH: &str = "/org/gnome/Shell";

/// regolith-displayd, which draws the labels: its bus name, and where it
/// serves them.
pub const DISPLAYD: &str = "org.gnome.Mutter.DisplayConfig";
pub const DISPLAYD_PATH: &str = "/org/regolith/displayd/MonitorLabels";
pub const DISPLAYD_INTERFACE: &str = "org.regolith.displayd.MonitorLabels";

/// The labels as the panel asks for them: connector names mapped to
/// numbers.
type Labels = HashMap<String, OwnedValue>;

/// What to ask displayd for.
enum Request {
    Show(Labels),
    Hide,
}

/// Who the labels belong to.
#[derive(Default)]
struct Clients {
    /// The unique bus name of the client whose labels are shown, if any.
    holder: Option<String>,
    /// The clients that are being watched for leaving the bus.
    watched: HashSet<String>,
}

struct ShellLabels {
    clients: Arc<Mutex<Clients>>,
    requests: Arc<Mutex<Sender<Request>>>,
}

#[dbus_interface(name = "org.gnome.Shell")]
impl ShellLabels {
    /// Show the labels `params` asks for. Ignored while another client's
    /// labels are shown.
    async fn show_monitor_labels(
        &self,
        #[zbus(header)] header: MessageHeader<'_>,
        #[zbus(connection)] connection: &Connection,
        params: Labels,
    ) {
        let Some(sender) = sender(&header) else {
            return;
        };
        let watch = {
            let mut clients = self.clients.lock().unwrap();
            match &clients.holder {
                Some(holder) if *holder != sender => return,
                _ => clients.holder = Some(sender.clone()),
            }
            clients.watched.insert(sender.clone())
        };
        self.request(Request::Show(params));
        if watch {
            let (clients, requests) = (self.clients.clone(), self.requests.clone());
            connection
                .executor()
                .spawn(
                    hide_when_gone(connection.clone(), sender, clients, requests),
                    "monitor labels client",
                )
                .detach();
        }
    }

    /// Hide the labels, if they are the caller's.
    async fn hide_monitor_labels(&self, #[zbus(header)] header: MessageHeader<'_>) {
        let Some(sender) = sender(&header) else {
            return;
        };
        {
            let mut clients = self.clients.lock().unwrap();
            if clients.holder.as_ref() != Some(&sender) {
                return;
            }
            clients.holder = None;
        }
        self.request(Request::Hide);
    }
}

impl ShellLabels {
    fn request(&self, request: Request) {
        let _ = self.requests.lock().unwrap().send(request);
    }
}

fn sender(header: &MessageHeader<'_>) -> Option<String> {
    header.sender().ok().flatten().map(|s| s.to_string())
}

/// Hide the labels when `client` leaves the bus, if they are still its.
async fn hide_when_gone(
    connection: Connection,
    client: String,
    clients: Arc<Mutex<Clients>>,
    requests: Arc<Mutex<Sender<Request>>>,
) {
    let watched = async {
        let dbus = fdo::DBusProxy::new(&connection).await?;
        let mut changes = dbus
            .receive_name_owner_changed_with_args(&[(0, client.as_str())])
            .await?;
        // It may have left before the match rule was added.
        if dbus
            .name_has_owner(BusName::try_from(client.as_str())?)
            .await?
        {
            while let Some(change) = changes.next().await {
                if change.args()?.new_owner().is_none() {
                    break;
                }
            }
        }
        Ok::<_, zbus::Error>(())
    };
    let result = watched.await;
    let hide = {
        let mut clients = clients.lock().unwrap();
        clients.watched.remove(&client);
        let hide = result.is_ok() && clients.holder.as_ref() == Some(&client);
        if hide {
            clients.holder = None;
        }
        hide
    };
    match result {
        Ok(()) if hide => {
            let _ = requests.lock().unwrap().send(Request::Hide);
        }
        Ok(()) => {}
        // The labels stay until the client hides them.
        Err(e) => g_log!(
            glib::LogLevel::Warning,
            "Cannot follow the monitor labels' client {client}: {e}"
        ),
    }
}

/// Make the requests to displayd, in order, skipping those that a later one
/// replaces.
fn forward(connection: Connection, requests: Receiver<Request>) {
    let mut unavailable = false;
    while let Ok(mut request) = requests.recv() {
        while let Ok(later) = requests.try_recv() {
            request = later;
        }
        let shown = matches!(request, Request::Show(_));
        let result = zbus::block_on(async {
            let displayd: zbus::Proxy = ProxyBuilder::new_bare(&connection)
                .destination(DISPLAYD)?
                .path(DISPLAYD_PATH)?
                .interface(DISPLAYD_INTERFACE)?
                .cache_properties(CacheProperties::No)
                .build()
                .await?;
            // displayd is started with the session, not on demand.
            let flags = MethodFlags::NoAutoStart.into();
            match request {
                Request::Show(labels) => {
                    displayd
                        .call_with_flags::<_, _, ()>("ShowMonitorLabels", flags, &(labels,))
                        .await
                }
                Request::Hide => {
                    displayd
                        .call_with_flags::<_, _, ()>("HideMonitorLabels", flags, &())
                        .await
                }
            }
        });
        match result {
            Ok(_) => unavailable = false,
            // Say so once, when showing them fails.
            Err(e) if shown && !unavailable => {
                unavailable = true;
                g_log!(
                    glib::LogLevel::Warning,
                    "No monitor labels: regolith-displayd cannot show them: {e}"
                );
            }
            Err(_) => {}
        }
    }
}

/// Answer the Displays panel's monitor label requests at `/org/gnome/Shell`
/// of `connection`, which owns, or is about to own, `org.gnome.Shell`.
pub fn serve(connection: &Connection) -> zbus::Result<()> {
    let (send, receive) = mpsc::channel();
    let labels = ShellLabels {
        clients: Arc::default(),
        requests: Arc::new(Mutex::new(send)),
    };
    zbus::block_on(connection.object_server().at(PATH, labels))?;
    let connection = connection.clone();
    thread::Builder::new()
        .name("monitor-labels".into())
        .spawn(move || forward(connection, receive))
        .map_err(|e| zbus::Error::Failure(format!("Cannot start the monitor labels: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::end_session::{self, tests::private_bus, tests::wait_until};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use zbus::zvariant::Value;
    use zbus::ConnectionBuilder;

    /// A stand-in for regolith-displayd: records the calls it gets, with
    /// their sender.
    struct FakeDisplayd {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[dbus_interface(name = "org.regolith.displayd.MonitorLabels")]
    impl FakeDisplayd {
        fn show_monitor_labels(&self, #[zbus(header)] header: MessageHeader<'_>, params: Labels) {
            let mut labels: Vec<String> = params
                .iter()
                .map(|(connector, number)| match &**number {
                    Value::I32(n) => format!("{connector}={n}"),
                    other => format!("{connector}={other:?}"),
                })
                .collect();
            labels.sort();
            let sender = sender(&header).unwrap_or_default();
            self.calls
                .lock()
                .unwrap()
                .push(format!("{sender} show {}", labels.join(" ")));
        }

        fn hide_monitor_labels(&self, #[zbus(header)] header: MessageHeader<'_>) {
            let sender = sender(&header).unwrap_or_default();
            self.calls.lock().unwrap().push(format!("{sender} hide"));
        }
    }

    fn connect(address: &str) -> Connection {
        zbus::block_on(ConnectionBuilder::address(address).unwrap().build()).unwrap()
    }

    /// A stand-in for the Displays panel.
    struct Panel(Connection);

    impl Panel {
        fn call(
            &self,
            method: &str,
            body: &(impl zbus::export::serde::Serialize + zbus::zvariant::DynamicType),
        ) {
            let start = Instant::now();
            zbus::block_on(self.0.call_method(
                Some("org.gnome.Shell"),
                PATH,
                Some("org.gnome.Shell"),
                method,
                body,
            ))
            .unwrap();
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "{method} was slow"
            );
        }

        fn show(&self, labels: &[(&str, i32)]) {
            let labels: HashMap<&str, Value> =
                labels.iter().map(|(c, n)| (*c, Value::from(*n))).collect();
            self.call("ShowMonitorLabels", &(labels,));
        }

        fn hide(&self) {
            self.call("HideMonitorLabels", &());
        }
    }

    #[test]
    fn passes_the_labels_of_one_panel_on_to_displayd() {
        let (_bus, address) = private_bus();

        // powerd: the labels and the end-session dialog on one connection,
        // which owns org.gnome.Shell.
        let powerd = connect(&address);
        serve(&powerd).unwrap();
        let dialog_connection = powerd.clone();
        thread::spawn(move || {
            let config = end_session::Config {
                prompt: PathBuf::from("/nonexistent/swaynag"),
                proc_root: PathBuf::from("/nonexistent"),
            };
            end_session::serve(&dialog_connection, config).unwrap();
        });
        let powerd_name = powerd.unique_name().unwrap().to_string();
        let dbus = zbus::blocking::fdo::DBusProxy::new(&connect(&address).into()).unwrap();
        wait_until("org.gnome.Shell", || {
            dbus.get_name_owner("org.gnome.Shell".try_into().unwrap())
                .is_ok_and(|owner| owner.as_str() == powerd_name)
        });

        // Without displayd, the panel gets an answer and there are no labels.
        let panel = Panel(connect(&address));
        panel.show(&[("DP-1", 1)]);
        panel.hide();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let displayd = zbus::block_on(
            ConnectionBuilder::address(address.as_str())
                .unwrap()
                .name(DISPLAYD)
                .unwrap()
                .serve_at(
                    DISPLAYD_PATH,
                    FakeDisplayd {
                        calls: calls.clone(),
                    },
                )
                .unwrap()
                .build(),
        )
        .unwrap();
        let expect = |what: &[&str]| {
            let what: Vec<String> = what.iter().map(|w| format!("{powerd_name} {w}")).collect();
            wait_until(&format!("{what:?}"), || *calls.lock().unwrap() == what);
            calls.lock().unwrap().clear();
        };
        let nothing_more = || {
            thread::sleep(Duration::from_millis(300));
            assert_eq!(*calls.lock().unwrap(), Vec::<String>::new());
        };

        panel.show(&[("DP-1", 1), ("HDMI-A-1", 2)]);
        expect(&["show DP-1=1 HDMI-A-1=2"]);

        // Another client can neither hide the labels nor show others.
        let other = Panel(connect(&address));
        other.hide();
        other.show(&[("DP-1", 9)]);
        nothing_more();

        panel.hide();
        expect(&["hide"]);

        // Once they are hidden, another client can show them, and they go
        // when it leaves the bus.
        other.show(&[("HDMI-A-1", 1)]);
        expect(&["show HDMI-A-1=1"]);
        panel.hide();
        nothing_more();
        drop(other);
        expect(&["hide"]);

        // The same client can show them again, as the panel does each time
        // its window becomes active.
        for _ in 0..3 {
            panel.show(&[("DP-1", 1)]);
            expect(&["show DP-1=1"]);
            panel.hide();
            expect(&["hide"]);
        }
        panel.show(&[("DP-1", 2)]);
        expect(&["show DP-1=2"]);
        drop(panel);
        expect(&["hide"]);

        // displayd goes away: the panel still gets answers.
        drop(displayd);
        let panel = Panel(connect(&address));
        panel.show(&[("DP-1", 1)]);
        panel.hide();

        // The end-session dialog still answers on the same name: it cannot
        // show its prompt here, so it cancels.
        let session = connect(&address);
        let proxy = zbus::blocking::Proxy::new(
            &session.clone().into(),
            "org.gnome.Shell",
            "/org/gnome/SessionManager/EndSessionDialog",
            "org.gnome.SessionManager.EndSessionDialog",
        )
        .unwrap();
        let mut canceled = proxy.receive_signal("Canceled").unwrap();
        proxy
            .call_method(
                "Open",
                &(
                    0u32,
                    0u32,
                    60u32,
                    Vec::<zbus::zvariant::OwnedObjectPath>::new(),
                ),
            )
            .unwrap();
        assert!(canceled.next().is_some());
    }
}
