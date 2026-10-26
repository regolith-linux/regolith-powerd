//! Passphrase prompts through the GNOME system prompter
//! (`org.gnome.keyring.SystemPrompter`), the dialog gnome-keyring and
//! gnome-shell use. gcr's prompter provides it in the sway session.
//!
//! The protocol is gcr's `GcrSystemPrompt`, client side:
//!
//! 1. `BeginPrompting(callback)`. The prompter shows one prompt at a time;
//!    when it is our turn it calls `PromptReady("", {}, exchange)` on our
//!    callback object, which starts a [secret exchange](crate::secret_exchange).
//! 2. `PerformPrompt(callback, "password", properties, exchange)`. Once the
//!    user answers, the prompter calls `PromptReady("yes" | "no",
//!    properties, exchange)`, with the passphrase encrypted in the exchange.
//! 3. `StopPrompting(callback)` closes the prompt, or takes it out of the
//!    prompter's queue. The prompter calls `PromptDone()` when it closes a
//!    prompt by itself.
//!
//! gtk-rs has no gcr bindings, so the daemon speaks the protocol itself.
//!
//! Each prompt runs on a thread of its own, so nothing here blocks the
//! daemon's main loop, and [`PromptQueue`] shows the daemon's prompts one
//! at a time.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use glib::g_log;
use zbus::dbus_interface;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::secret_exchange::{ExchangeError, SecretExchange};

const PROMPTER_NAME: &str = "org.gnome.keyring.SystemPrompter";
const PROMPTER_PATH: &str = "/org/gnome/keyring/Prompter";
const PROMPTER_INTERFACE: &str = "org.gnome.keyring.internal.Prompter";

/// Bound for connecting and for each method call to the prompter, which
/// answers them at once.
const CALL_TIMEOUT: Duration = Duration::from_secs(25);

/// What to ask.
#[derive(Debug, Clone, Default)]
pub struct PromptRequest {
    pub title: String,
    pub message: String,
    pub description: String,
    /// Label of the check box, such as "Remember passphrase".
    pub choice_label: Option<String>,
    /// Shown above the entry, such as after a wrong passphrase.
    pub warning: Option<String>,
}

/// The user's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptAnswer {
    /// The passphrase, or `None` if the user cancelled.
    pub password: Option<String>,
    /// Whether the check box was ticked.
    pub choice_chosen: bool,
}

#[derive(Debug, Clone)]
pub enum PromptError {
    Dbus(zbus::Error),
    Exchange(ExchangeError),
    /// The prompter broke the protocol.
    Protocol(&'static str),
    /// Closed by the daemon, or by the prompter without an answer.
    Closed,
    /// No answer in time.
    Timeout,
}

impl fmt::Display for PromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PromptError::Dbus(e) => write!(f, "D-Bus error: {e}"),
            PromptError::Exchange(e) => write!(f, "secret exchange failed: {e}"),
            PromptError::Protocol(what) => write!(f, "unexpected reply from the prompter: {what}"),
            PromptError::Closed => f.write_str("the prompt was closed"),
            PromptError::Timeout => f.write_str("the prompt timed out"),
        }
    }
}

impl std::error::Error for PromptError {}

impl From<zbus::Error> for PromptError {
    fn from(e: zbus::Error) -> Self {
        PromptError::Dbus(e)
    }
}

impl From<zbus::zvariant::Error> for PromptError {
    fn from(e: zbus::zvariant::Error) -> Self {
        PromptError::Dbus(e.into())
    }
}

impl From<ExchangeError> for PromptError {
    fn from(e: ExchangeError) -> Self {
        PromptError::Exchange(e)
    }
}

/// The bus the prompter is on.
#[derive(Debug, Clone)]
pub enum Bus {
    Session,
    /// A bus address, for tests.
    #[cfg(test)]
    Address(String),
}

impl Bus {
    async fn connect(&self) -> zbus::Result<zbus::Connection> {
        match self {
            Bus::Session => zbus::Connection::session().await,
            #[cfg(test)]
            Bus::Address(address) => {
                zbus::ConnectionBuilder::address(address.as_str())?
                    .build()
                    .await
            }
        }
    }
}

/// What reaches a running prompt: the prompter's callbacks, or a close.
#[derive(Debug)]
enum Event {
    Ready {
        sender: Option<String>,
        reply: String,
        choice_chosen: bool,
        exchange: String,
    },
    Done {
        sender: Option<String>,
    },
    Close,
}

/// The callback object the prompter calls. It passes the calls on; the
/// prompt checks who sent them.
struct Callback {
    events: async_channel::Sender<Event>,
}

fn sender(header: &zbus::MessageHeader<'_>) -> Option<String> {
    header.sender().ok().flatten().map(|name| name.to_string())
}

#[dbus_interface(name = "org.gnome.keyring.internal.Prompter.Callback")]
impl Callback {
    fn prompt_ready(
        &self,
        #[zbus(header)] header: zbus::MessageHeader<'_>,
        reply: String,
        properties: HashMap<String, OwnedValue>,
        exchange: String,
    ) {
        let _ = self.events.try_send(Event::Ready {
            sender: sender(&header),
            reply,
            choice_chosen: choice_chosen(&properties),
            exchange,
        });
    }

    fn prompt_done(&self, #[zbus(header)] header: zbus::MessageHeader<'_>) {
        let _ = self.events.try_send(Event::Done {
            sender: sender(&header),
        });
    }
}

/// The `choice-chosen` property. The prompter wraps property values in an
/// extra variant.
fn choice_chosen(properties: &HashMap<String, OwnedValue>) -> bool {
    fn as_bool(value: &Value<'_>) -> Option<bool> {
        match value {
            Value::Bool(b) => Some(*b),
            Value::Value(inner) => as_bool(inner),
            _ => None,
        }
    }
    properties
        .get("choice-chosen")
        .and_then(|value| as_bool(value))
        .unwrap_or(false)
}

/// Closes a running prompt, from any thread.
#[derive(Debug, Clone)]
pub struct PromptCloser(async_channel::Sender<Event>);

impl PromptCloser {
    pub fn close(&self) {
        let _ = self.0.try_send(Event::Close);
    }
}

/// One password prompt.
pub struct PasswordPrompt {
    events: async_channel::Receiver<Event>,
    sender: async_channel::Sender<Event>,
}

impl PasswordPrompt {
    pub fn new() -> (Self, PromptCloser) {
        let (sender, events) = async_channel::unbounded();
        let closer = PromptCloser(sender.clone());
        (Self { events, sender }, closer)
    }

    /// Show the prompt and wait for the answer, on a thread of its own.
    /// `timeout` bounds the wait for our turn and, again, the wait for the
    /// answer. A cancel is an answer without a passphrase.
    pub fn spawn(
        self,
        bus: Bus,
        request: PromptRequest,
        timeout: Duration,
    ) -> async_channel::Receiver<Result<PromptAnswer, PromptError>> {
        let (tx, rx) = async_channel::bounded(1);
        thread::Builder::new()
            .name("powerd-prompt".to_string())
            .spawn(move || {
                let context = glib::MainContext::new();
                let result = context
                    .with_thread_default(|| context.block_on(self.run(&bus, &request, timeout)))
                    .unwrap_or(Err(PromptError::Protocol("no main context")));
                let _ = tx.send_blocking(result);
            })
            .expect("cannot start a thread");
        rx
    }

    async fn run(
        self,
        bus: &Bus,
        request: &PromptRequest,
        timeout: Duration,
    ) -> Result<PromptAnswer, PromptError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = OwnedObjectPath::try_from(format!(
            "/org/regolith/powerd/Prompt/p{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))?;
        let connection = call(bus.connect()).await?;
        let callback = Callback {
            events: self.sender.clone(),
        };
        connection
            .object_server()
            .at(path.as_str(), callback)
            .await?;
        let prompter = zbus::Proxy::new(
            &connection,
            PROMPTER_NAME,
            PROMPTER_PATH,
            PROMPTER_INTERFACE,
        )
        .await?;
        let reply = call(prompter.call_method("BeginPrompting", &(&path,))).await?;
        // The prompter's own connection, which its callbacks come from.
        let owner = reply
            .header()?
            .sender()?
            .map(|name| name.to_string())
            .ok_or(PromptError::Protocol("anonymous reply"))?;
        let result = self
            .converse(&prompter, &path, &owner, request, timeout)
            .await;
        let _ = call(prompter.call_method("StopPrompting", &(&path,))).await;
        result
    }

    async fn converse(
        &self,
        prompter: &zbus::Proxy<'_>,
        path: &OwnedObjectPath,
        owner: &str,
        request: &PromptRequest,
        timeout: Duration,
    ) -> Result<PromptAnswer, PromptError> {
        let mut exchange = SecretExchange::new();
        match self.next_event(owner, timeout).await? {
            Event::Ready {
                reply,
                exchange: theirs,
                ..
            } if reply.is_empty() => {
                exchange.receive(&theirs)?;
            }
            Event::Ready { .. } => return Err(PromptError::Protocol("answer before the prompt")),
            _ => return Err(PromptError::Closed),
        }

        let mut properties: HashMap<&str, Value<'_>> = HashMap::from([
            ("title", Value::from(request.title.as_str())),
            ("message", Value::from(request.message.as_str())),
            ("description", Value::from(request.description.as_str())),
        ]);
        if let Some(label) = &request.choice_label {
            properties.insert("choice-label", Value::from(label.as_str()));
        }
        if let Some(warning) = &request.warning {
            properties.insert("warning", Value::from(warning.as_str()));
        }
        let ours = exchange.send(None)?;
        call(prompter.call_method("PerformPrompt", &(path, "password", properties, ours))).await?;

        match self.next_event(owner, timeout).await? {
            Event::Ready {
                reply,
                exchange: theirs,
                choice_chosen,
                ..
            } => {
                let password = if reply == "yes" {
                    let secret = exchange
                        .receive(&theirs)?
                        .ok_or(PromptError::Protocol("no passphrase"))?;
                    let password = String::from_utf8(secret)
                        .map_err(|_| PromptError::Protocol("passphrase not UTF-8"))?;
                    Some(password)
                } else {
                    None
                };
                Ok(PromptAnswer {
                    password,
                    choice_chosen,
                })
            }
            _ => Err(PromptError::Closed),
        }
    }

    /// The next callback from the prompter, or a close. Calls from anyone
    /// but the prompter are dropped.
    async fn next_event(&self, owner: &str, timeout: Duration) -> Result<Event, PromptError> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let event = glib::future_with_timeout(left, self.events.recv())
                .await
                .map_err(|_| PromptError::Timeout)?
                .map_err(|_| PromptError::Closed)?;
            let from_prompter = match &event {
                Event::Ready { sender, .. } | Event::Done { sender } => {
                    sender.as_deref() == Some(owner)
                }
                Event::Close => return Err(PromptError::Closed),
            };
            if from_prompter {
                return Ok(event);
            }
            g_log!(
                glib::LogLevel::Warning,
                "Ignoring a prompt callback that does not come from the prompter"
            );
        }
    }
}

/// A connection or method call, bounded by [`CALL_TIMEOUT`].
async fn call<T>(
    future: impl std::future::Future<Output = zbus::Result<T>>,
) -> Result<T, PromptError> {
    glib::future_with_timeout(CALL_TIMEOUT, future)
        .await
        .map_err(|_| PromptError::Timeout)?
        .map_err(PromptError::Dbus)
}

type Reply = Box<dyn FnOnce(Result<PromptAnswer, PromptError>)>;

struct Waiting<K> {
    key: K,
    request: PromptRequest,
    reply: Reply,
}

struct Showing {
    closer: PromptCloser,
    reply: Reply,
    serial: u64,
}

struct Turn<K> {
    key: K,
    showing: Option<Showing>,
}

struct QueueState<K> {
    turn: Option<Turn<K>>,
    waiting: VecDeque<Waiting<K>>,
    serial: u64,
}

/// The daemon's prompts, shown one at a time, from the main loop.
///
/// A key (a drive) gets the turn when its first prompt is shown and keeps
/// it, for any further prompts, until [`release`](Self::release) or
/// [`cancel`](Self::cancel). Prompts for other keys wait. Replies are
/// called on the main loop.
pub struct PromptQueue<K> {
    state: Rc<RefCell<QueueState<K>>>,
    bus: Bus,
    timeout: Duration,
}

impl<K> Clone for PromptQueue<K> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            bus: self.bus.clone(),
            timeout: self.timeout,
        }
    }
}

impl<K: PartialEq + Clone + 'static> PromptQueue<K> {
    /// `timeout` is as in [`PasswordPrompt::spawn`].
    pub fn new(bus: Bus, timeout: Duration) -> Self {
        Self {
            state: Rc::new(RefCell::new(QueueState {
                turn: None,
                waiting: VecDeque::new(),
                serial: 0,
            })),
            bus,
            timeout,
        }
    }

    /// Ask for a passphrase for `key`: now if it is `key`'s turn or
    /// nobody's, otherwise when its turn comes.
    pub fn ask(
        &self,
        key: K,
        request: PromptRequest,
        reply: impl FnOnce(Result<PromptAnswer, PromptError>) + 'static,
    ) {
        self.state.borrow_mut().waiting.push_back(Waiting {
            key,
            request,
            reply: Box::new(reply),
        });
        self.advance();
    }

    /// End `key`'s turn, once it needs no more prompts.
    pub fn release(&self, key: &K) {
        {
            let mut state = self.state.borrow_mut();
            match &state.turn {
                Some(turn) if turn.key == *key && turn.showing.is_none() => state.turn = None,
                _ => return,
            }
        }
        self.advance();
    }

    /// Withdraw `key`'s prompts and end its turn, closing the prompt on
    /// screen. Their replies get [`PromptError::Closed`].
    pub fn cancel(&self, key: &K) {
        let mut replies = Vec::new();
        {
            let mut state = self.state.borrow_mut();
            let (withdrawn, kept) = state.waiting.drain(..).partition(|w| w.key == *key);
            state.waiting = kept;
            replies.extend(withdrawn.into_iter().map(|w: Waiting<K>| w.reply));
            if state.turn.as_ref().is_some_and(|turn| turn.key == *key) {
                if let Some(showing) = state.turn.take().and_then(|turn| turn.showing) {
                    showing.closer.close();
                    replies.push(showing.reply);
                }
            }
        }
        for reply in replies {
            reply(Err(PromptError::Closed));
        }
        self.advance();
    }

    /// Show the next prompt, if one may be shown.
    fn advance(&self) {
        let (waiting, serial) = {
            let mut state = self.state.borrow_mut();
            let next = match &state.turn {
                None => 0,
                Some(Turn { key, showing: None }) => {
                    match state.waiting.iter().position(|w| w.key == *key) {
                        Some(index) => index,
                        None => return,
                    }
                }
                Some(_) => return,
            };
            let Some(waiting) = state.waiting.remove(next) else {
                return;
            };
            state.serial += 1;
            (waiting, state.serial)
        };
        let (prompt, closer) = PasswordPrompt::new();
        let answer = prompt.spawn(self.bus.clone(), waiting.request, self.timeout);
        self.state.borrow_mut().turn = Some(Turn {
            key: waiting.key,
            showing: Some(Showing {
                closer,
                reply: waiting.reply,
                serial,
            }),
        });
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = answer
                .recv()
                .await
                .unwrap_or(Err(PromptError::Protocol("the prompt thread ended")));
            this.answered(serial, result);
        });
    }

    fn answered(&self, serial: u64, result: Result<PromptAnswer, PromptError>) {
        let reply = {
            let mut state = self.state.borrow_mut();
            match state.turn.as_mut() {
                Some(turn) if turn.showing.as_ref().is_some_and(|s| s.serial == serial) => {
                    turn.showing.take().map(|showing| showing.reply)
                }
                // Cancelled meanwhile, and already replied to.
                _ => None,
            }
        };
        if let Some(reply) = reply {
            reply(result);
        }
        self.advance();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::{mpsc, Arc, Mutex};

    const CALLBACK_INTERFACE: &str = "org.gnome.keyring.internal.Prompter.Callback";

    struct DbusDaemon(Child);

    impl Drop for DbusDaemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A private bus that stands in for the session bus.
    fn private_bus() -> (DbusDaemon, String) {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon is needed to run this test");
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        (DbusDaemon(child), address.trim().to_string())
    }

    /// How the fake prompter answers a prompt.
    #[derive(Clone, Debug)]
    enum Answer {
        Password(&'static str, bool),
        Cancel,
        /// Closes the prompt with `PromptDone`.
        Close,
        /// Waits for [`FakePrompter::answer`].
        Later,
    }

    /// What the fake prompter was asked.
    #[derive(Default)]
    struct Log {
        begun: Vec<String>,
        shown: Vec<HashMap<String, String>>,
        stopped: Vec<String>,
    }

    /// A client's unique name and callback path.
    type Client = (String, String);

    struct State {
        answers: VecDeque<Answer>,
        waiting: VecDeque<Client>,
        active: Option<(Client, SecretExchange)>,
        log: Log,
    }

    /// Calls to a client, made by a thread of the fake, after the method
    /// call that caused them has returned (as gcr does).
    enum Call {
        Ready(Client, &'static str, String, Option<bool>),
        Done(Client),
    }

    /// A stand-in for gcr's prompter: one prompt at a time and the others
    /// queued, callbacks sent from the connection that owns the name.
    #[derive(Clone)]
    struct FakePrompter {
        state: Arc<Mutex<State>>,
        calls: mpsc::Sender<Call>,
    }

    impl FakePrompter {
        fn start(address: &str, answers: Vec<Answer>) -> (Self, zbus::blocking::Connection) {
            let (calls, rx) = mpsc::channel();
            let fake = FakePrompter {
                state: Arc::new(Mutex::new(State {
                    answers: answers.into(),
                    waiting: VecDeque::new(),
                    active: None,
                    log: Log::default(),
                })),
                calls,
            };
            let connection = zbus::blocking::ConnectionBuilder::address(address)
                .unwrap()
                .name(PROMPTER_NAME)
                .unwrap()
                .serve_at(PROMPTER_PATH, fake.clone())
                .unwrap()
                .build()
                .unwrap();
            let caller = connection.clone();
            thread::spawn(move || {
                for call in rx {
                    let _ = match call {
                        Call::Ready((name, path), reply, exchange, choice) => {
                            let mut properties: HashMap<&str, Value<'_>> = HashMap::new();
                            if let Some(choice) = choice {
                                // Wrapped twice, as gcr does.
                                properties.insert("choice-chosen", Value::new(Value::Bool(choice)));
                            }
                            caller.call_method(
                                Some(name.as_str()),
                                path.as_str(),
                                Some(CALLBACK_INTERFACE),
                                "PromptReady",
                                &(reply, properties, exchange),
                            )
                        }
                        Call::Done((name, path)) => caller.call_method(
                            Some(name.as_str()),
                            path.as_str(),
                            Some(CALLBACK_INTERFACE),
                            "PromptDone",
                            &(),
                        ),
                    };
                }
            });
            (fake, connection)
        }

        /// Make the next waiting client active.
        fn next(&self, state: &mut State) {
            if state.active.is_some() {
                return;
            }
            if let Some(client) = state.waiting.pop_front() {
                let mut exchange = SecretExchange::new();
                let begin = exchange.begin().unwrap();
                state.active = Some((client.clone(), exchange));
                let _ = self.calls.send(Call::Ready(client, "", begin, None));
            }
        }

        /// Answer the active prompt.
        fn reply(&self, state: &mut State, answer: Answer) {
            let Some((client, exchange)) = state.active.as_ref() else {
                return;
            };
            let client = client.clone();
            let call = match answer {
                Answer::Password(password, choice) => Call::Ready(
                    client,
                    "yes",
                    exchange.send(Some(password.as_bytes())).unwrap(),
                    Some(choice),
                ),
                Answer::Cancel => Call::Ready(client, "no", exchange.send(None).unwrap(), None),
                Answer::Close => Call::Done(client),
                Answer::Later => return,
            };
            let _ = self.calls.send(call);
        }

        fn answer(&self, answer: Answer) {
            let mut state = self.state.lock().unwrap();
            self.reply(&mut state, answer);
        }

        fn log<T>(&self, f: impl FnOnce(&Log) -> T) -> T {
            f(&self.state.lock().unwrap().log)
        }
    }

    #[dbus_interface(name = "org.gnome.keyring.internal.Prompter")]
    impl FakePrompter {
        fn begin_prompting(
            &self,
            #[zbus(header)] header: zbus::MessageHeader<'_>,
            callback: OwnedObjectPath,
        ) {
            let mut state = self.state.lock().unwrap();
            let client = (sender(&header).unwrap(), callback.to_string());
            state.log.begun.push(client.1.clone());
            state.waiting.push_back(client);
            self.next(&mut state);
        }

        fn perform_prompt(
            &self,
            #[zbus(header)] header: zbus::MessageHeader<'_>,
            callback: OwnedObjectPath,
            kind: String,
            properties: HashMap<String, OwnedValue>,
            exchange: String,
        ) -> zbus::fdo::Result<()> {
            let mut state = self.state.lock().unwrap();
            let client = (sender(&header).unwrap(), callback.to_string());
            match state.active.as_mut() {
                Some((active, ours)) if *active == client => {
                    ours.receive(&exchange)
                        .map_err(|e| zbus::fdo::Error::InvalidArgs(e.to_string()))?;
                }
                _ => return Err(zbus::fdo::Error::Failed("not prompting".into())),
            }
            assert_eq!(kind, "password");
            let shown = properties
                .iter()
                .map(|(k, v)| (k.clone(), String::try_from(v.clone()).unwrap()))
                .collect();
            state.log.shown.push(shown);
            let answer = state.answers.pop_front().unwrap_or(Answer::Later);
            self.reply(&mut state, answer);
            Ok(())
        }

        fn stop_prompting(
            &self,
            #[zbus(header)] header: zbus::MessageHeader<'_>,
            callback: OwnedObjectPath,
        ) {
            let mut state = self.state.lock().unwrap();
            let client = (sender(&header).unwrap(), callback.to_string());
            state.log.stopped.push(client.1.clone());
            state.waiting.retain(|c| *c != client);
            if state.active.as_ref().is_some_and(|(c, _)| *c == client) {
                state.active = None;
            }
            self.next(&mut state);
        }
    }

    fn request(name: &str) -> PromptRequest {
        PromptRequest {
            title: format!("Unlock {name}"),
            message: format!("Enter the passphrase for {name}"),
            description: "/dev/sdb1".to_string(),
            choice_label: Some("Remember passphrase".to_string()),
            warning: None,
        }
    }

    fn ask(
        address: &str,
        request: PromptRequest,
        timeout: Duration,
    ) -> Result<PromptAnswer, PromptError> {
        let (prompt, _closer) = PasswordPrompt::new();
        prompt
            .spawn(Bus::Address(address.to_string()), request, timeout)
            .recv_blocking()
            .unwrap()
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Run `body` with a main context of its own as the thread default.
    fn on_main_loop(body: impl FnOnce(&glib::MainContext)) {
        let context = glib::MainContext::new();
        context.with_thread_default(|| body(&context)).unwrap();
    }

    fn iterate_until(context: &glib::MainContext, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            context.iteration(false);
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn answers_with_the_passphrase_and_the_choice() {
        let (_bus, address) = private_bus();
        let (fake, _conn) = FakePrompter::start(
            &address,
            vec![
                Answer::Password("pässwörd", true),
                Answer::Password("x", false),
            ],
        );
        let mut first = request("Stick");
        first.warning = Some("Wrong passphrase".to_string());
        let answer = ask(&address, first, Duration::from_secs(10)).unwrap();
        assert_eq!(
            answer,
            PromptAnswer {
                password: Some("pässwörd".to_string()),
                choice_chosen: true
            }
        );
        let answer = ask(&address, request("Stick"), Duration::from_secs(10)).unwrap();
        assert_eq!(answer.password.as_deref(), Some("x"));
        assert!(!answer.choice_chosen);

        fake.log(|log| {
            let shown = &log.shown[0];
            assert_eq!(shown["title"], "Unlock Stick");
            assert_eq!(shown["message"], "Enter the passphrase for Stick");
            assert_eq!(shown["description"], "/dev/sdb1");
            assert_eq!(shown["choice-label"], "Remember passphrase");
            assert_eq!(shown["warning"], "Wrong passphrase");
            assert!(!log.shown[1].contains_key("warning"));
            // Every prompt is stopped once answered.
            assert_eq!(log.stopped, log.begun);
        });
    }

    #[test]
    fn cancel_is_an_answer_without_passphrase() {
        let (_bus, address) = private_bus();
        let (_fake, _conn) = FakePrompter::start(&address, vec![Answer::Cancel]);
        let answer = ask(&address, request("Stick"), Duration::from_secs(10)).unwrap();
        assert_eq!(answer.password, None);
    }

    #[test]
    fn a_prompt_closed_by_the_prompter_ends() {
        let (_bus, address) = private_bus();
        let (_fake, _conn) = FakePrompter::start(&address, vec![Answer::Close]);
        let result = ask(&address, request("Stick"), Duration::from_secs(10));
        assert!(matches!(result, Err(PromptError::Closed)), "{result:?}");
    }

    #[test]
    fn no_prompter_is_an_error() {
        let (_bus, address) = private_bus();
        let result = ask(&address, request("Stick"), Duration::from_secs(10));
        assert!(matches!(result, Err(PromptError::Dbus(_))), "{result:?}");
    }

    /// Another program's prompt holds the prompter, so ours waits for its
    /// turn. That wait can be closed, and times out like the answer.
    #[test]
    fn waiting_for_the_turn_can_be_closed_and_times_out() {
        let (_bus, address) = private_bus();
        let (fake, _conn) = FakePrompter::start(&address, vec![]);
        fake.state.lock().unwrap().active = Some((
            (":other".to_string(), "/other".to_string()),
            SecretExchange::new(),
        ));

        let (prompt, closer) = PasswordPrompt::new();
        let answer = prompt.spawn(
            Bus::Address(address.clone()),
            request("Stick"),
            Duration::from_secs(60),
        );
        wait_until("BeginPrompting", || fake.log(|log| log.begun.len() == 1));
        let started = Instant::now();
        closer.close();
        let result = answer.recv_blocking().unwrap();
        assert!(matches!(result, Err(PromptError::Closed)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
        // And it left the prompter's queue.
        fake.log(|log| assert_eq!(log.stopped, log.begun));
        assert!(fake.state.lock().unwrap().waiting.is_empty());

        let started = Instant::now();
        let result = ask(&address, request("Stick"), Duration::from_millis(300));
        assert!(matches!(result, Err(PromptError::Timeout)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn ignores_callbacks_from_others() {
        let (_bus, address) = private_bus();
        let (fake, _conn) = FakePrompter::start(&address, vec![Answer::Later]);
        let (prompt, _closer) = PasswordPrompt::new();
        let answer = prompt.spawn(
            Bus::Address(address.clone()),
            request("Stick"),
            Duration::from_secs(10),
        );
        wait_until("PerformPrompt", || fake.log(|log| log.shown.len() == 1));

        // Someone else answers in the prompter's place.
        let (name, path) = fake
            .state
            .lock()
            .unwrap()
            .active
            .as_ref()
            .unwrap()
            .0
            .clone();
        let intruder = zbus::blocking::ConnectionBuilder::address(address.as_str())
            .unwrap()
            .build()
            .unwrap();
        let begin = SecretExchange::new().begin().unwrap();
        let no_properties: HashMap<&str, Value<'_>> = HashMap::new();
        for reply in ["yes", "no"] {
            intruder
                .call_method(
                    Some(name.as_str()),
                    path.as_str(),
                    Some(CALLBACK_INTERFACE),
                    "PromptReady",
                    &(reply, &no_properties, &begin),
                )
                .unwrap();
        }
        intruder
            .call_method(
                Some(name.as_str()),
                path.as_str(),
                Some(CALLBACK_INTERFACE),
                "PromptDone",
                &(),
            )
            .unwrap();
        thread::sleep(Duration::from_millis(200));
        assert!(answer.is_empty());

        fake.answer(Answer::Password("right", false));
        let result = answer.recv_blocking().unwrap().unwrap();
        assert_eq!(result.password.as_deref(), Some("right"));
    }

    #[test]
    fn the_queue_shows_one_prompt_at_a_time_without_blocking() {
        let (_bus, address) = private_bus();
        let (fake, _conn) = FakePrompter::start(
            &address,
            vec![
                Answer::Later,
                Answer::Password("again", false),
                Answer::Password("b", true),
            ],
        );
        on_main_loop(|context| {
            let queue = PromptQueue::new(Bus::Address(address.clone()), Duration::from_secs(10));
            let answers = Rc::new(RefCell::new(Vec::<(u32, Option<String>)>::new()));
            let reply = |key: u32| {
                let answers = answers.clone();
                move |result: Result<PromptAnswer, PromptError>| {
                    answers.borrow_mut().push((key, result.unwrap().password));
                }
            };
            queue.ask(1, request("A"), reply(1));
            queue.ask(2, request("B"), reply(2));
            iterate_until(context, "the first prompt", || {
                fake.log(|log| log.shown.len() == 1)
            });

            // The main loop runs while the prompt is open.
            let ticks = Rc::new(RefCell::new(0));
            let counter = ticks.clone();
            let ticker = context.spawn_local(async move {
                loop {
                    glib::timeout_future(Duration::from_millis(10)).await;
                    *counter.borrow_mut() += 1;
                }
            });
            iterate_until(context, "the main loop", || *ticks.borrow() >= 5);
            ticker.abort();
            fake.log(|log| assert_eq!(log.begun.len(), 1, "B waits for its turn"));

            // After a wrong passphrase A keeps the turn for its next prompt.
            fake.answer(Answer::Password("wrong", false));
            iterate_until(context, "A's answer", || answers.borrow().len() == 1);
            queue.ask(1, request("A"), reply(1));
            iterate_until(context, "A's second answer", || answers.borrow().len() == 2);
            fake.log(|log| assert_eq!(log.begun.len(), 2, "B still waits"));

            queue.release(&1);
            iterate_until(context, "B's answer", || answers.borrow().len() == 3);
            assert_eq!(
                *answers.borrow(),
                vec![
                    (1, Some("wrong".to_string())),
                    (1, Some("again".to_string())),
                    (2, Some("b".to_string()))
                ]
            );
            fake.log(|log| assert_eq!(log.shown[2]["title"], "Unlock B"));
        });
    }

    #[test]
    fn cancelling_closes_or_withdraws_prompts() {
        let (_bus, address) = private_bus();
        let (fake, _conn) = FakePrompter::start(&address, vec![Answer::Later, Answer::Later]);
        on_main_loop(|context| {
            let queue = PromptQueue::new(Bus::Address(address.clone()), Duration::from_secs(10));
            let results: Rc<RefCell<Vec<(u32, String)>>> = Rc::default();
            let reply = |key: u32| {
                let results = results.clone();
                move |result: Result<PromptAnswer, PromptError>| {
                    results.borrow_mut().push((key, format!("{result:?}")));
                }
            };
            queue.ask(1, request("A"), reply(1));
            queue.ask(2, request("B"), reply(2));
            queue.ask(3, request("C"), reply(3));
            iterate_until(context, "A's prompt", || {
                fake.log(|log| log.shown.len() == 1)
            });

            // A waiting prompt is dropped without being shown.
            queue.cancel(&2);
            assert_eq!(*results.borrow(), vec![(2, "Err(Closed)".to_string())]);

            // The prompt on screen is closed, and C gets the turn.
            queue.cancel(&1);
            assert_eq!(results.borrow()[1], (1, "Err(Closed)".to_string()));
            iterate_until(context, "C's prompt", || {
                fake.log(|log| log.shown.len() == 2)
            });
            fake.log(|log| {
                assert_eq!(log.begun.len(), 2);
                assert_eq!(log.stopped.len(), 1);
                assert_eq!(log.shown[1]["title"], "Unlock C");
            });
            fake.answer(Answer::Cancel);
            iterate_until(context, "C's answer", || results.borrow().len() == 3);
            assert!(results.borrow()[2].1.contains("password: None"));
        });
    }
}
