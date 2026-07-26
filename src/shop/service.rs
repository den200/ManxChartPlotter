//! The chart shop, off the main thread.
//!
//! Every shop call is a network round trip, and some are slow: a marina's wifi
//! answering a chart listing must never stall the chart. Requests go to a
//! worker; results come back as events the UI drains each frame.

use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::thread;

use super::types::Chart;
use super::{Fingerprint, ShopClient, Session};

/// What the UI asks the shop to do.
pub enum Request {
    /// Sign in, then identify this machine and list what the account owns.
    /// One request rather than three, because that is one user action.
    SignIn {
        email: String,
        password: String,
        fingerprint: Option<Fingerprint>,
    },
    /// Re-read the entitlement list on the current session.
    Refresh,
    /// Forget the session.
    SignOut,
}

/// What the shop tells the UI.
#[derive(Debug, Clone)]
pub enum Event {
    /// A step finished; the string is for the user, not the log.
    Status(String),
    SignedIn {
        /// What the shop calls this machine, if it knows it yet.
        system_name: Option<String>,
    },
    Charts {
        charts: Vec<Chart>,
        systems: Vec<String>,
    },
    SignedOut,
    Failed(String),
}

pub struct ShopService {
    tx: Sender<Request>,
    rx: Receiver<Event>,
    _thread: thread::JoinHandle<()>,
}

impl Default for ShopService {
    fn default() -> Self {
        Self::new()
    }
}

impl ShopService {
    pub fn new() -> Self {
        let (tx, request_rx) = channel::<Request>();
        let (event_tx, rx) = channel::<Event>();
        let thread = thread::Builder::new()
            .name("chart-shop".into())
            .spawn(move || worker(request_rx, event_tx))
            .expect("spawn chart-shop thread");
        Self {
            tx,
            rx,
            _thread: thread,
        }
    }

    pub fn send(&self, request: Request) {
        let _ = self.tx.send(request);
    }

    /// Everything that has arrived since the last call.
    pub fn poll(&self) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(e) => out.push(e),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }
}

fn worker(requests: Receiver<Request>, events: Sender<Event>) {
    let client = ShopClient::new();
    let mut session: Option<Session> = None;

    while let Ok(request) = requests.recv() {
        match request {
            Request::SignIn {
                email,
                password,
                fingerprint,
            } => {
                let _ = events.send(Event::Status("Signing in…".into()));
                match client.login(&email, &password) {
                    Ok(s) => {
                        // Identify the machine before listing, so the list can
                        // say which charts are usable *here* rather than just
                        // which are owned.
                        let mut system_name = None;
                        if let Some(fpr) = fingerprint {
                            let _ = events.send(Event::Status("Identifying this machine…".into()));
                            match client.identify_system(&s, &fpr.bytes, &fpr.name) {
                                Ok(name) => system_name = name,
                                Err(e) => {
                                    let _ = events.send(Event::Status(format!(
                                        "Could not identify this machine: {e}"
                                    )));
                                }
                            }
                        }
                        let _ = events.send(Event::SignedIn {
                            system_name: system_name.clone(),
                        });
                        session = Some(Session { system_name, ..s });
                        list(&client, session.as_ref(), &events);
                    }
                    Err(e) => {
                        let _ = events.send(Event::Failed(e.to_string()));
                    }
                }
            }
            Request::Refresh => list(&client, session.as_ref(), &events),
            Request::SignOut => {
                session = None;
                let _ = events.send(Event::SignedOut);
            }
        }
    }
}

fn list(client: &ShopClient, session: Option<&Session>, events: &Sender<Event>) {
    let Some(session) = session else {
        let _ = events.send(Event::Failed("not signed in".into()));
        return;
    };
    let _ = events.send(Event::Status("Fetching your charts…".into()));
    match client.list_charts(session) {
        Ok((charts, systems)) => {
            let _ = events.send(Event::Charts { charts, systems });
        }
        Err(e) => {
            let _ = events.send(Event::Failed(e.to_string()));
        }
    }
}
