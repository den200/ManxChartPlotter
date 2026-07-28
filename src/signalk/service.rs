//! The Signal K connection, off the main thread.
//!
//! A boat's data arrives continuously and the chart must never wait for it.
//! The socket lives on a worker; deltas come back as events the UI drains each
//! frame — the same shape as the shop client, and for the same reason.
//!
//! The worker reconnects on its own. A Signal K server on a boat goes away
//! constantly: the Pi reboots, the wifi drops between the cockpit and the
//! saloon, the server restarts after a plugin update. Reconnection is the
//! normal case, not the exceptional one, so it backs off rather than either
//! giving up or hammering.

use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::delta;

/// What the UI asks of the connection.
pub enum Request {
    Connect { url: String },
    Disconnect,
}

/// What the connection tells the UI.
#[derive(Debug, Clone)]
pub enum Event {
    /// Where we are in the connection's life, in words fit for a status line.
    Status(Status),
    /// A parsed delta, ready to fold into the fleet.
    Delta(delta::Delta),
    /// The server's greeting named our own vessel. Must reach the fleet before
    /// any delta can be routed: without it every AIS target looks like us.
    SelfContext(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Disconnected,
    Connecting(String),
    Connected(String),
    /// Waiting before another attempt; carries what went wrong and how long.
    Retrying { because: String, in_seconds: u64 },
}

impl Status {
    /// One line for the status bar.
    pub fn summary(&self) -> String {
        match self {
            Status::Disconnected => "Not connected".into(),
            Status::Connecting(url) => format!("Connecting to {url}…"),
            Status::Connected(url) => format!("Connected to {url}"),
            Status::Retrying { because, in_seconds } => {
                format!("{because} — retrying in {in_seconds}s")
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(self, Status::Connected(_))
    }
}

pub struct SignalKService {
    tx: Sender<Request>,
    rx: Receiver<Event>,
    /// Set when the UI wants the current socket dropped. The worker blocks in
    /// `read()`, so it cannot poll the request channel; this is how a
    /// disconnect reaches it.
    stop: Arc<AtomicBool>,
    _thread: thread::JoinHandle<()>,
}

impl Default for SignalKService {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalKService {
    pub fn new() -> Self {
        let (tx, request_rx) = channel::<Request>();
        let (event_tx, rx) = channel::<Event>();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("signalk".into())
            .spawn(move || worker(request_rx, event_tx, worker_stop))
            .expect("spawn signalk thread");
        Self {
            tx,
            rx,
            stop,
            _thread: thread,
        }
    }

    pub fn connect(&self, url: String) {
        self.stop.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Request::Connect { url });
    }

    pub fn disconnect(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Request::Disconnect);
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

/// Turn whatever the user typed into a Signal K stream URL.
///
/// People paste what they have: a bare host from the boat's label, the admin
/// page they just had open, an `http://` address. All of them mean the same
/// server, so all of them are accepted rather than rejected with a lecture
/// about URL schemes.
pub fn normalise_url(input: &str) -> String {
    let raw = input.trim().trim_end_matches('/');
    if raw.is_empty() {
        return String::new();
    }
    // A simulated boat is not an address and must survive untouched, or it
    // becomes ws://sim:3000/ and spends its life failing to connect.
    if super::sim::parse_course(raw).is_some() {
        return raw.to_string();
    }

    // Scheme: ws/wss stay, http maps to ws, https to wss, bare host gets ws
    // unless it looks like the public demo, which is TLS only.
    let (scheme, rest) = match raw.split_once("://") {
        Some(("wss", r)) | Some(("https", r)) => ("wss", r),
        Some(("ws", r)) | Some(("http", r)) => ("ws", r),
        Some((_, r)) => ("ws", r),
        None => {
            if raw.contains("demo.signalk.org") {
                ("wss", raw)
            } else {
                ("ws", raw)
            }
        }
    };

    // Drop anything the admin UI leaves on the end, then add the stream path.
    let host_and_path = rest.split('#').next().unwrap_or(rest);
    let host_and_path = host_and_path.trim_end_matches('/');
    let base = host_and_path
        .strip_suffix("/admin")
        .unwrap_or(host_and_path);
    if base.contains("/signalk/") {
        return format!("{scheme}://{base}");
    }

    // A host with no port is the Signal K default, 3000 — except for a public
    // name, where the TLS port is implied and adding 3000 would break it.
    let has_port = base
        .split('/')
        .next()
        .is_some_and(|h| h.rsplit(':').next().is_some_and(|p| p.parse::<u16>().is_ok()));
    let authority = if has_port || scheme == "wss" {
        base.to_string()
    } else {
        format!("{base}:3000")
    };
    format!("{scheme}://{authority}/signalk/v1/stream?subscribe=none")
}

fn worker(requests: Receiver<Request>, events: Sender<Event>, stop: Arc<AtomicBool>) {
    let mut url: Option<String> = None;
    // Backoff doubles to a ceiling: a boat at anchor with the server off
    // should not spin, but a server coming back after a reboot should be
    // picked up within a few seconds, not a few minutes.
    const BACKOFF: &[u64] = &[1, 2, 5, 10, 20, 30];
    let mut attempt = 0usize;

    loop {
        // Take the latest instruction. Blocks when idle so the thread costs
        // nothing while disconnected.
        let next = if url.is_some() {
            requests.try_recv().ok()
        } else {
            requests.recv().ok()
        };
        match next {
            Some(Request::Connect { url: u }) => {
                url = Some(u);
                attempt = 0;
                stop.store(false, Ordering::Relaxed);
            }
            Some(Request::Disconnect) => {
                url = None;
                let _ = events.send(Event::Status(Status::Disconnected));
                continue;
            }
            None if url.is_none() => return, // channel closed
            None => {}
        }

        let Some(ref target) = url else { continue };

        // A simulated boat needs no socket, and must not be retried as though
        // it were a server that failed.
        if let Some(course) = super::sim::parse_course(target) {
            let _ = events.send(Event::Status(Status::Connected(format!(
                "simulated boat at {:.3}, {:.3}",
                course.lat, course.lon
            ))));
            simulate(course, &events, &stop);
            url = None;
            let _ = events.send(Event::Status(Status::Disconnected));
            continue;
        }

        let _ = events.send(Event::Status(Status::Connecting(target.clone())));

        match run_once(target, &events, &stop) {
            Ok(()) => {
                // A clean close still means reconnecting: the server restarted
                // or the subscription lapsed, and the boat is still moving.
                if stop.load(Ordering::Relaxed) {
                    url = None;
                    let _ = events.send(Event::Status(Status::Disconnected));
                    continue;
                }
                attempt = 0;
                let _ = events.send(Event::Status(Status::Retrying {
                    because: "The server closed the connection".into(),
                    in_seconds: BACKOFF[0],
                }));
                thread::sleep(Duration::from_secs(BACKOFF[0]));
            }
            Err(e) => {
                if stop.load(Ordering::Relaxed) {
                    url = None;
                    let _ = events.send(Event::Status(Status::Disconnected));
                    continue;
                }
                let wait = BACKOFF[attempt.min(BACKOFF.len() - 1)];
                attempt += 1;
                let _ = events.send(Event::Status(Status::Retrying {
                    because: e,
                    in_seconds: wait,
                }));
                // Sleep in slices so a disconnect is honoured promptly rather
                // than after the whole backoff has run.
                for _ in 0..wait * 10 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

/// Drive the simulated boat until told to stop.
///
/// One second of boat per tick, ticked four times a second — fast enough that
/// the marker moves visibly, slow enough that the numbers are readable.
fn simulate(course: super::sim::Course, events: &Sender<Event>, stop: &AtomicBool) {
    const TICK: Duration = Duration::from_millis(250);
    let mut sim = super::sim::Simulator::new(course);
    // Traffic too, so the AIS layer can be seen without standing up a server.
    // The own boat's deltas carry no context, which is how a server says "my
    // vessel", so the fleet routes everything correctly without a greeting.
    let mut traffic = super::sim::traffic_around(course);
    while !stop.load(Ordering::Relaxed) {
        let dt = TICK.as_secs_f64();
        if events.send(Event::Delta(sim.step(dt))).is_err() {
            return; // UI gone
        }
        for target in traffic.iter_mut() {
            if events.send(Event::Delta(target.step(dt))).is_err() {
                return;
            }
        }
        thread::sleep(TICK);
    }
}

/// One connection, from handshake to close.
fn run_once(url: &str, events: &Sender<Event>, stop: &AtomicBool) -> Result<(), String> {
    use tungstenite::Message;

    let (mut socket, _response) =
        tungstenite::connect(url).map_err(|e| describe(&e.to_string()))?;
    let _ = events.send(Event::Status(Status::Connected(url.to_string())));

    // Ask for everything at the fastest the server will give. A plotter wants
    // position and heading as often as they exist; the bar is redrawn from
    // whatever has arrived, so a fast stream costs nothing but bandwidth on a
    // local network.
    let subscribe = r#"{"context":"vessels.self","subscribe":[{"path":"*","period":500}]}"#;
    if let Err(e) = socket.send(Message::Text(subscribe.into())) {
        return Err(describe(&e.to_string()));
    }

    loop {
        if stop.load(Ordering::Relaxed) {
            let _ = socket.close(None);
            return Ok(());
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Some(ctx) = delta::parse_hello(&text) {
                    log::debug!("signalk: own vessel is {ctx}");
                    if events.send(Event::SelfContext(ctx)).is_err() {
                        return Ok(());
                    }
                }
                if let Some(d) = delta::parse(&text) {
                    // Every vessel is forwarded, ours and the AIS traffic
                    // alike; the fleet routes them on context. Filtering here
                    // would throw away the traffic the chart is meant to show.
                    if events.send(Event::Delta(d)).is_err() {
                        return Ok(()); // UI gone
                    }
                }
            }
            // The library answers pings itself; both are normal traffic.
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Binary(_)) => {}
            Ok(Message::Close(_)) => return Ok(()),
            Ok(Message::Frame(_)) => {}
            Err(e) => return Err(describe(&e.to_string())),
        }
    }
}

/// Turn a library error into something a user can act on.
///
/// "IO error: Connection refused (os error 61)" tells a developer what
/// happened and a skipper nothing at all.
fn describe(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("refused") {
        "Nothing is listening there — check the address and that Signal K is running".into()
    } else if lower.contains("dns") || lower.contains("resolve") || lower.contains("lookup") {
        "That host name could not be found — check the address, and the network".into()
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "The server did not answer in time".into()
    } else if lower.contains("certificate") || lower.contains("tls") || lower.contains("handshake") {
        "The secure connection could not be established".into()
    } else if lower.contains("http error") || lower.contains("404") {
        "The server answered, but not with a Signal K stream — check the address".into()
    } else {
        format!("Connection lost: {raw}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_becomes_a_stream_url_on_the_default_port() {
        assert_eq!(
            normalise_url("192.168.1.50"),
            "ws://192.168.1.50:3000/signalk/v1/stream?subscribe=none"
        );
        assert_eq!(
            normalise_url("openplotter.local"),
            "ws://openplotter.local:3000/signalk/v1/stream?subscribe=none"
        );
    }

    #[test]
    fn a_port_the_user_gave_is_respected() {
        assert_eq!(
            normalise_url("192.168.1.50:8080"),
            "ws://192.168.1.50:8080/signalk/v1/stream?subscribe=none"
        );
    }

    #[test]
    fn the_admin_page_someone_pasted_still_works() {
        // What you get by copying the browser's address bar.
        assert_eq!(
            normalise_url("https://demo.signalk.org/admin/#/dashboard"),
            "wss://demo.signalk.org/signalk/v1/stream?subscribe=none"
        );
        assert_eq!(
            normalise_url("http://192.168.1.50:3000/admin/"),
            "ws://192.168.1.50:3000/signalk/v1/stream?subscribe=none"
        );
    }

    #[test]
    fn the_public_demo_is_assumed_to_be_tls() {
        // Typed bare, it must not become ws://…:3000, which cannot connect.
        assert_eq!(
            normalise_url("demo.signalk.org"),
            "wss://demo.signalk.org/signalk/v1/stream?subscribe=none"
        );
    }

    #[test]
    fn a_full_stream_url_is_left_alone() {
        let full = "wss://demo.signalk.org/signalk/v1/stream";
        assert_eq!(normalise_url(full), full);
        assert_eq!(
            normalise_url("ws://boat.local:3000/signalk/v1/stream"),
            "ws://boat.local:3000/signalk/v1/stream"
        );
    }

    #[test]
    fn nothing_in_gives_nothing_out() {
        assert_eq!(normalise_url(""), "");
        assert_eq!(normalise_url("   "), "");
    }

    #[test]
    fn errors_are_phrased_for_a_skipper_not_a_developer() {
        assert!(describe("IO error: Connection refused (os error 61)").contains("Nothing is listening"));
        assert!(describe("failed to lookup address information").contains("could not be found"));
        assert!(describe("HTTP error: 404 Not Found").contains("not with a Signal K stream"));
        // Anything unrecognised still reaches the user rather than vanishing.
        assert!(describe("something odd").contains("something odd"));
    }

    #[test]
    fn a_service_starts_and_stops_without_a_server() {
        let s = SignalKService::new();
        assert!(s.poll().is_empty());
        s.disconnect();
    }
}
