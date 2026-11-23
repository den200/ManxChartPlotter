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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::delta;

/// What the UI asks of the connection.
pub enum Request {
    Connect { url: String, order: u64 },
    Disconnect { order: u64 },
}

/// Whether the worker should drop what it is doing.
///
/// The worker blocks — in a socket read, in a back-off sleep — and cannot
/// watch its request channel from there. Every instruction from the UI is
/// numbered, and the newest number is kept where the worker can glance at
/// it: when it is no longer the one being carried out, there is a newer
/// instruction waiting, and whatever is in hand stops.
///
/// A plain "stop" flag could not tell a Connect from a Disconnect: a new
/// address typed while connected was not acted on at all, and the
/// Disconnect that followed it then connected to that address.
struct Orders<'a> {
    newest: &'a AtomicU64,
    mine: u64,
}

impl Orders<'_> {
    fn superseded(&self) -> bool {
        self.newest.load(Ordering::Relaxed) != self.mine
    }
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
    /// The number of the newest instruction sent. See [`Orders`].
    newest: Arc<AtomicU64>,
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
        let newest = Arc::new(AtomicU64::new(0));
        let worker_newest = Arc::clone(&newest);
        let thread = thread::Builder::new()
            .name("signalk".into())
            .spawn(move || worker(request_rx, event_tx, worker_newest))
            .expect("spawn signalk thread");
        Self {
            tx,
            rx,
            newest,
            _thread: thread,
        }
    }

    pub fn connect(&self, url: String) {
        let order = self.newest.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.tx.send(Request::Connect { url, order });
    }

    pub fn disconnect(&self) {
        let order = self.newest.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.tx.send(Request::Disconnect { order });
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

fn worker(requests: Receiver<Request>, events: Sender<Event>, newest: Arc<AtomicU64>) {
    let mut url: Option<String> = None;
    // Backoff doubles to a ceiling: a boat at anchor with the server off
    // should not spin, but a server coming back after a reboot should be
    // picked up within a few seconds, not a few minutes.
    const BACKOFF: &[u64] = &[1, 2, 5, 10, 20, 30];
    let mut attempt = 0usize;
    let mut mine = 0u64;

    loop {
        // Take the latest instruction — the last of whatever is queued, for
        // only the newest still means anything. Blocks when idle so the
        // thread costs nothing while disconnected.
        let mut next = if url.is_some() {
            requests.try_recv().ok()
        } else {
            requests.recv().ok()
        };
        while let Ok(later) = requests.try_recv() {
            next = Some(later);
        }
        match next {
            Some(Request::Connect { url: u, order }) => {
                url = Some(u);
                attempt = 0;
                mine = order;
            }
            Some(Request::Disconnect { order }) => {
                url = None;
                mine = order;
                let _ = events.send(Event::Status(Status::Disconnected));
                continue;
            }
            None if url.is_none() => return, // channel closed
            None => {}
        }
        let orders = Orders { newest: &newest, mine };
        if orders.superseded() {
            // A newer instruction is on its way down the channel.
            thread::sleep(Duration::from_millis(5));
            continue;
        }

        let Some(ref target) = url else { continue };

        // A simulated boat needs no socket, and must not be retried as though
        // it were a server that failed.
        if let Some(course) = super::sim::parse_course(target) {
            let _ = events.send(Event::Status(Status::Connected(format!(
                "simulated boat at {:.3}, {:.3}",
                course.lat, course.lon
            ))));
            simulate(course, &events, &orders);
            // Only a newer instruction ends a simulation; it says what next.
            continue;
        }

        let _ = events.send(Event::Status(Status::Connecting(target.clone())));

        let mut connected = false;
        let outcome = run_once(target, &events, &orders, &mut connected);
        if orders.superseded() {
            continue;
        }
        // Having got through once, the next failure is a fresh one: the
        // wait starts short again instead of at wherever the trouble before
        // the connection had pushed it.
        if connected {
            attempt = 0;
        }
        let (because, wait) = match outcome {
            // A clean close still means reconnecting: the server restarted
            // or the subscription lapsed, and the boat is still moving.
            Ok(()) => ("The server closed the connection".to_string(), BACKOFF[0]),
            Err(e) => {
                let wait = BACKOFF[attempt.min(BACKOFF.len() - 1)];
                attempt += 1;
                (e, wait)
            }
        };
        let _ = events.send(Event::Status(Status::Retrying {
            because,
            in_seconds: wait,
        }));
        // Sleep in slices so a new instruction is honoured promptly rather
        // than after the whole backoff has run.
        for _ in 0..wait * 10 {
            if orders.superseded() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Drive the simulated boat until told to stop.
///
/// One second of boat per tick, ticked four times a second — fast enough that
/// the marker moves visibly, slow enough that the numbers are readable.
fn simulate(course: super::sim::Course, events: &Sender<Event>, orders: &Orders) {
    const TICK: Duration = Duration::from_millis(250);
    let mut sim = super::sim::Simulator::new(course);
    // Traffic too, so the AIS layer can be seen without standing up a server.
    // The own boat's deltas carry no context, which is how a server says "my
    // vessel", so the fleet routes everything correctly without a greeting.
    let mut traffic = super::sim::traffic_around(course);
    while !orders.superseded() {
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

/// What is asked of the server once the stream is open (it is opened with
/// `subscribe=none`, so nothing arrives unasked).
///
/// Our own boat, everything, at the fastest the server will give: a plotter
/// wants position and heading as often as they exist, and on a local network
/// a fast stream costs nothing but bandwidth.
///
/// And every other vessel — the AIS traffic. A subscription to
/// `vessels.self` alone brings no other vessel at all: checked against the
/// public demo server, it delivered one context where this pair delivers
/// 161. Traffic is asked for more slowly; a ship's AIS report is seconds
/// old by the time it is received anyway.
const SUBSCRIPTIONS: &[&str] = &[
    r#"{"context":"vessels.self","subscribe":[{"path":"*","period":500}]}"#,
    r#"{"context":"vessels.*","subscribe":[{"path":"*","period":2000}]}"#,
];

/// One connection, from handshake to close.
fn run_once(
    url: &str,
    events: &Sender<Event>,
    orders: &Orders,
    connected: &mut bool,
) -> Result<(), String> {
    use tungstenite::Message;

    let (mut socket, _response) =
        tungstenite::connect(url).map_err(|e| describe(&e.to_string()))?;

    // A read timeout is what makes a dead link detectable. Without one,
    // `read()` blocks for ever the moment the peer goes quiet without closing
    // the connection — the wifi bridge between cockpit and saloon dropping,
    // or a router forgetting the NAT entry. The loop below never got back to
    // its stop check, so the retry never ran, Disconnect did nothing, and the
    // bar kept saying Connected over a frozen position.
    set_read_timeout(&mut socket, READ_TIMEOUT);
    *connected = true;
    let _ = events.send(Event::Status(Status::Connected(url.to_string())));

    for subscribe in SUBSCRIPTIONS {
        if let Err(e) = socket.send(Message::Text((*subscribe).into())) {
            return Err(describe(&e.to_string()));
        }
    }

    let mut silent_reads = 0u32;
    loop {
        if orders.superseded() {
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
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // Quiet, not dead — go round, which re-checks the stop flag.
                // A server we asked for everything every 500 ms has nothing
                // to say for a whole minute only if the link is gone.
                silent_reads += 1;
                if silent_reads >= SILENT_READS_BEFORE_DEAD {
                    return Err("the connection went quiet — reconnecting".into());
                }
                continue;
            }
            Err(e) => return Err(describe(&e.to_string())),
        }
        silent_reads = 0;
    }
}

/// How long a single read may block before the loop looks up.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// Consecutive silent reads that mean the link, not the server, is quiet.
const SILENT_READS_BEFORE_DEAD: u32 = 3;

/// Put a read deadline on the TCP socket underneath the WebSocket, plain or
/// TLS. Failure to set it is logged and tolerated: a socket without a
/// deadline is the old behaviour, not a reason to refuse the connection.
fn set_read_timeout(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    timeout: std::time::Duration,
) {
    use tungstenite::stream::MaybeTlsStream;
    let tcp: Option<&std::net::TcpStream> = match socket.get_ref() {
        MaybeTlsStream::Plain(s) => Some(s),
        MaybeTlsStream::Rustls(t) => Some(&t.sock),
        _ => None,
    };
    match tcp {
        Some(s) => {
            if let Err(e) = s.set_read_timeout(Some(timeout)) {
                log::warn!("signalk: could not set a read timeout: {e}");
            }
        }
        None => log::warn!("signalk: unknown stream type; no read timeout set"),
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

    /// The Signal K subscription protocol addresses vessels by context:
    /// `vessels.self` is our own, `vessels.*` every vessel. Without the
    /// second there is no AIS traffic, so no targets and no collision alarm.
    #[test]
    fn the_stream_asks_for_our_boat_and_for_the_traffic() {
        let contexts: Vec<String> = SUBSCRIPTIONS
            .iter()
            .map(|m| {
                let v: serde_json::Value = serde_json::from_str(m).expect("valid JSON");
                assert_eq!(v["subscribe"][0]["path"], "*");
                assert!(v["subscribe"][0]["period"].as_u64().is_some());
                v["context"].as_str().unwrap().to_string()
            })
            .collect();
        assert!(contexts.iter().any(|c| c == "vessels.self"));
        assert!(contexts.iter().any(|c| c == "vessels.*"));
    }

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
    fn other_schemes_and_a_simulated_boat_are_understood() {
        // http maps to ws, and a host without a port gets Signal K's 3000.
        assert_eq!(
            normalise_url("http://boat.local"),
            "ws://boat.local:3000/signalk/v1/stream?subscribe=none"
        );
        // https is TLS on its own port: adding 3000 would break it.
        assert_eq!(
            normalise_url("https://boat.example.org/"),
            "wss://boat.example.org/signalk/v1/stream?subscribe=none"
        );
        // A simulator address is not a host and must reach the worker as
        // typed, or it becomes ws://sim:3000/ and fails for ever.
        assert_eq!(normalise_url(" sim:56.1,11.2 "), "sim:56.1,11.2");
        assert_eq!(normalise_url("sim"), "sim");
    }

    /// Wait for the worker to say something matching `want`, collecting
    /// everything it said on the way.
    fn wait_for(s: &SignalKService, want: impl Fn(&Event) -> bool) -> Vec<Event> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            let batch = s.poll();
            let done = batch.iter().any(&want);
            seen.extend(batch);
            if done {
                return seen;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("the worker never said what was expected; it said {seen:?}");
    }

    /// A new address given while connected is gone to at once, and a
    /// disconnect after it disconnects — it does not start the connection
    /// that was asked for before it.
    #[test]
    fn a_new_address_while_connected_is_gone_to_and_a_disconnect_stays_disconnected() {
        let s = SignalKService::new();
        s.connect("sim:56.1,11.2".into());
        wait_for(&s, |e| matches!(e, Event::Status(st) if st.is_connected()));

        s.connect("sim:57.5,10.0".into());
        let seen = wait_for(
            &s,
            |e| matches!(e, Event::Status(st) if st.summary().contains("57.500, 10.000")),
        );
        assert!(
            !seen.iter().any(|e| matches!(e, Event::Status(Status::Disconnected))),
            "going to a new address is not a disconnect"
        );

        // Two instructions at once: only the last counts.
        s.connect("sim:58.0,11.0".into());
        s.disconnect();
        wait_for(&s, |e| matches!(e, Event::Status(Status::Disconnected)));
        thread::sleep(Duration::from_millis(600));
        let after = s.poll();
        assert!(
            !after.iter().any(|e| matches!(e, Event::Delta(_) | Event::Status(Status::Connected(_)))),
            "connected again after the disconnect: {after:?}"
        );
    }

    /// The whole worker loop, end to end, without a socket: a simulated boat
    /// connects, streams our own vessel and the traffic around it, and a
    /// disconnect actually stops it.
    #[test]
    fn a_simulated_boat_streams_until_disconnected() {
        let s = SignalKService::new();
        assert!(s.poll().is_empty(), "nothing is said before anything is asked");

        s.connect("sim:56.1,11.2".into());
        let seen = wait_for(&s, |e| matches!(e, Event::Delta(d) if d.context.is_some()));
        let connected = seen.iter().find_map(|e| match e {
            Event::Status(st) if st.is_connected() => Some(st.summary()),
            _ => None,
        });
        assert_eq!(
            connected.as_deref(),
            Some("Connected to simulated boat at 56.100, 11.200")
        );
        // Our own boat speaks with no context, which the fleet reads as us,
        // and sails where it was told to.
        let own = seen
            .iter()
            .find_map(|e| match e {
                Event::Delta(d) if d.context.is_none() => Some(d),
                _ => None,
            })
            .expect("a delta for our own boat");
        let pos = own
            .updates
            .iter()
            .find_map(|u| match u.value {
                delta::Value::Position { lat, lon } => Some((lat, lon)),
                _ => None,
            })
            .expect("a position");
        assert!((pos.0 - 56.1).abs() < 0.01 && (pos.1 - 11.2).abs() < 0.01, "{pos:?}");

        s.disconnect();
        wait_for(&s, |e| matches!(e, Event::Status(Status::Disconnected)));
        // Stopped means stopped: no more of the boat after the last word.
        thread::sleep(Duration::from_millis(600));
        assert!(
            !s.poll().iter().any(|e| matches!(e, Event::Delta(_))),
            "the simulator kept sailing after the disconnect"
        );
    }
}
