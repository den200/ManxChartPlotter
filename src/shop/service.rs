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
    /// Register this machine with the account under a name of the user's choosing.
    Register {
        system_name: String,
        fingerprint: Fingerprint,
    },
    /// Claim a slot and ask the shop for a download link.
    Download {
        chart_id: String,
        /// What is on disk already, if anything.
        installed: Option<super::types::Edition>,
    },
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
    /// The shop's answer to a download request, for one chart.
    Grant { chart_id: String, summary: String },
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
            Request::Register {
                system_name,
                fingerprint,
            } => {
                let _ = events.send(Event::Status(format!("Registering as \"{system_name}\"…")));
                match session.as_ref() {
                    Some(s) => match client.register_system(
                        s,
                        &system_name,
                        &fingerprint.bytes,
                        &fingerprint.name,
                    ) {
                        Ok(()) => {
                            if let Some(s) = session.as_mut() {
                                s.system_name = Some(system_name.clone());
                            }
                            let _ = events.send(Event::SignedIn {
                                system_name: Some(system_name),
                            });
                            list(&client, session.as_ref(), &events);
                        }
                        Err(e) => {
                            let _ = events.send(Event::Failed(e.to_string()));
                        }
                    },
                    None => {
                        let _ = events.send(Event::Failed("not signed in".into()));
                    }
                }
            }
            Request::Download {
                chart_id,
                installed,
            } => download(&client, session.as_ref(), &chart_id, installed, &events),
            Request::Refresh => list(&client, session.as_ref(), &events),
            Request::SignOut => {
                session = None;
                let _ = events.send(Event::SignedOut);
            }
        }
    }
}

/// Claim a slot for a chart, then ask the shop what it will give us.
///
/// Stops at the grant rather than fetching: what the shop is willing to hand
/// over for a lapsed subscription is its decision, and the useful thing is to
/// see the answer in its own words.
fn download(
    client: &ShopClient,
    session: Option<&Session>,
    chart_id: &str,
    installed: Option<super::types::Edition>,
    events: &Sender<Event>,
) {
    let Some(session) = session else {
        let _ = events.send(Event::Failed("not signed in".into()));
        return;
    };
    let Some(system_name) = session.system_name.clone() else {
        let _ = events.send(Event::Failed(
            "register this machine with the account first".into(),
        ));
        return;
    };

    let _ = events.send(Event::Status("Looking up the chart…".into()));
    let (charts, _) = match client.list_charts(session) {
        Ok(v) => v,
        Err(e) => {
            let _ = events.send(Event::Failed(e.to_string()));
            return;
        }
    };
    let Some(chart) = charts.iter().find(|c| c.id == chart_id) else {
        let _ = events.send(Event::Failed(format!("chart {chart_id} is no longer listed")));
        return;
    };

    // A slot is this machine's claim on one copy of the chart. If we already
    // hold one, reuse it; the shop counts assignments, and burning a second is
    // a real cost to the user.
    let slot_uuid = match chart.slot_for(&system_name) {
        Some((_, slot)) => slot.uuid.clone(),
        None => {
            let Some(quantity) = chart.free_quantity() else {
                let _ = events.send(Event::Failed(format!(
                    "no free slot for {}: all {} are assigned to other machines",
                    chart.name, chart.max_slots
                )));
                return;
            };
            let _ = events.send(Event::Status("Claiming a slot for this machine…".into()));
            match client.assign(session, chart, &quantity.id, &system_name) {
                Ok(uuid) => uuid,
                Err(e) => {
                    let _ = events.send(Event::Failed(e.to_string()));
                    return;
                }
            }
        }
    };

    let target = super::protocol::choose_download(installed, chart.edition);
    let _ = events.send(Event::Status(format!(
        "Asking for the {} package…",
        target.requested_file().unwrap_or("nothing")
    )));
    match client.request_download(
        session,
        &slot_uuid,
        &system_name,
        target,
        chart.edition,
        installed,
    ) {
        Ok(grant) if grant.files.is_empty() => {
            let _ = events.send(Event::Grant {
                chart_id: chart_id.to_string(),
                summary: "the shop granted no files".into(),
            });
        }
        Ok(grant) => {
            let summary = grant
                .files
                .iter()
                .map(|f| {
                    format!(
                        "{} MB, edition {}{}",
                        f.size / 1_000_000,
                        if f.edition_result.is_empty() { "?" } else { &f.edition_result },
                        if f.substituted_base() { " (full set)" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            let _ = events.send(Event::Grant {
                chart_id: chart_id.to_string(),
                summary,
            });
        }
        Err(e) => {
            let _ = events.send(Event::Grant {
                chart_id: chart_id.to_string(),
                summary: format!("refused: {e}"),
            });
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
