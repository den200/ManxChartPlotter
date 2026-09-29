//! A Signal K server with a boat and some traffic on it.
//!
//! Testing a plotter against the public demo means sailing in the Gulf of
//! Finland, where nobody's charts are. This serves the same protocol on
//! localhost, with a vessel and a handful of AIS targets wherever you point
//! it — by default the Kattegat, which is on the Danish charts.
//!
//! It is a *server*, not a shortcut: Manx connects to it over a real
//! WebSocket and parses real delta messages, so everything between the socket
//! and the screen is exercised. The in-process `sim` mode skips all of that,
//! which makes it useful for a quick look and useless for finding the bugs
//! that live in the wire format.
//!
//! ```text
//! cargo run --bin signalk-sim -- 56.55,11.60
//! cargo run -- charts/…          # then connect to 127.0.0.1:3000
//! ```

use std::net::TcpListener;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::Duration;

use manx::signalk::delta::{Delta, Value};
use manx::signalk::sim::{traffic_around, Course, Simulator, Traffic};

/// The port a Signal K server listens on by convention.
const PORT: u16 = 3000;
/// How often the fleet moves and reports.
const TICK: Duration = Duration::from_millis(500);
/// What the server calls our own vessel.
const SELF_CONTEXT: &str = "vessels.urn:mrn:signalk:uuid:manx-simulated-boat";

fn main() {
    let course = std::env::args()
        .nth(1)
        .and_then(|a| parse_position(&a))
        .unwrap_or_default();

    let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not listen on 127.0.0.1:{PORT}: {e}");
            eprintln!("is a Signal K server already running?");
            std::process::exit(1);
        }
    };

    println!("Signal K simulator on ws://127.0.0.1:{PORT}/signalk/v1/stream");
    println!("  own vessel at {:.4}, {:.4}", course.lat, course.lon);
    let traffic = traffic_around(course);
    for t in &traffic {
        println!("  AIS  {:<16} {}", t.name, t.context);
    }
    println!("\nIn manx: Instruments → 127.0.0.1  (or MANX_SIGNALK=127.0.0.1)");

    // One fleet, many viewers: the deltas are generated once and broadcast, so
    // two Manx windows see the same sea.
    let (subscribe_tx, subscribe_rx) = channel::<Sender<String>>();
    thread::spawn(move || fleet(course, traffic, subscribe_rx));

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (tx, rx) = channel::<String>();
        if subscribe_tx.send(tx).is_err() {
            return;
        }
        thread::spawn(move || serve(stream, rx));
    }
}

/// Move the fleet and broadcast what it did.
fn fleet(course: Course, mut traffic: Vec<Traffic>, subscribers: Receiver<Sender<String>>) {
    let mut own = Simulator::new(course);
    let mut clients: Vec<Sender<String>> = Vec::new();

    loop {
        while let Ok(client) = subscribers.try_recv() {
            clients.push(client);
        }

        let dt = TICK.as_secs_f64();
        let mut messages = vec![to_json(&own.step(dt), Some(SELF_CONTEXT))];
        for t in traffic.iter_mut() {
            let d = t.step(dt);
            let context = d.context.clone();
            messages.push(to_json(&d, context.as_deref()));
        }

        // Drop clients whose socket has gone.
        clients.retain(|c| {
            messages
                .iter()
                .all(|m| c.send(m.clone()).is_ok())
        });

        thread::sleep(TICK);
    }
}

/// One connected client.
fn serve(stream: std::net::TcpStream, rx: Receiver<String>) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    let mut socket = match tungstenite::accept(stream) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("handshake with {peer} failed: {e}");
            return;
        }
    };
    println!("client connected: {peer}");

    // The greeting. Its `self` field is what lets a client tell our boat from
    // the AIS traffic, and a client that ignores it plots every ship as its
    // own — so a simulator that omitted it would hide exactly the bug worth
    // testing for.
    let hello = format!(
        r#"{{"name":"manx-signalk-sim","version":"1.0.0",
             "self":"{SELF_CONTEXT}","roles":["master","main"]}}"#
    );
    if socket.send(tungstenite::Message::Text(hello)).is_err() {
        return;
    }

    while let Ok(message) = rx.recv() {
        if socket.send(tungstenite::Message::Text(message)).is_err() {
            break;
        }
    }
    println!("client gone: {peer}");
}

/// Render a delta the way a Signal K server would put it on the wire.
fn to_json(delta: &Delta, context: Option<&str>) -> String {
    let values: Vec<String> = delta
        .updates
        .iter()
        .map(|u| {
            let value = match &u.value {
                Value::Number(n) => format!("{n}"),
                Value::Position { lat, lon } => {
                    format!(r#"{{"latitude":{lat},"longitude":{lon}}}"#)
                }
                Value::Text(t) => format!("{:?}", t),
                Value::Null => "null".into(),
            };
            format!(r#"{{"path":"{}","value":{value}}}"#, u.path)
        })
        .collect();
    let context = context.unwrap_or(SELF_CONTEXT);
    format!(
        r#"{{"context":"{context}","updates":[{{"source":{{"label":"sim"}},"values":[{}]}}]}}"#,
        values.join(",")
    )
}

/// `56.55,11.60` or `56.55,11.60,200,6.5`.
fn parse_position(arg: &str) -> Option<Course> {
    let parts: Vec<f64> = arg.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    let base = Course::default();
    match parts.len() {
        2 => Some(Course {
            lat: parts[0],
            lon: parts[1],
            ..base
        }),
        4 => Some(Course {
            lat: parts[0],
            lon: parts[1],
            heading_deg: parts[2],
            speed_kn: parts[3],
        }),
        _ => None,
    }
}
