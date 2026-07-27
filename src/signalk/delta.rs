//! Parsing Signal K delta messages.
//!
//! The stream is a sequence of deltas, each carrying updates for one vessel:
//!
//! ```json
//! {"context":"vessels.urn:mrn:signalk:uuid:…",
//!  "updates":[{"source":{"label":"n2k"},"timestamp":"2026-07-27T10:00:00Z",
//!              "values":[{"path":"navigation.speedOverGround","value":3.42},
//!                        {"path":"navigation.position",
//!                         "value":{"latitude":55.6,"longitude":12.6}}]}]}
//! ```
//!
//! Values are usually a bare number, but position is an object and some paths
//! carry strings. Anything that is not a number or a position is kept as text
//! rather than dropped — an unrecognised path is still worth showing.

use serde::Deserialize;

/// One `path: value` pair from a delta.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub path: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number(f64),
    Position { lat: f64, lon: f64 },
    Text(String),
    /// The path exists but has no reading — a sensor that has gone quiet says
    /// so with an explicit null, which is different from never having reported.
    Null,
}

impl Delta {
    /// Is this delta about our own boat?
    ///
    /// `None` context means the server's own vessel, and `vessels.self` is the
    /// alias for it. Anything else is another ship, and its position must
    /// never be mistaken for ours.
    pub fn is_self(&self, self_context: Option<&str>) -> bool {
        match self.context.as_deref() {
            None => true,
            Some("vessels.self") | Some("self") => true,
            Some(c) => self_context == Some(c),
        }
    }
}

impl Value {
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct RawDelta {
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    updates: Vec<RawUpdate>,
}

#[derive(Deserialize)]
struct RawUpdate {
    #[serde(default)]
    values: Vec<RawValue>,
}

#[derive(Deserialize)]
struct RawValue {
    path: String,
    #[serde(default)]
    value: serde_json::Value,
}

/// What a delta told us, with the vessel it was about.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Delta {
    /// `vessels.urn:…`, or `None` on a message that omits it — which the spec
    /// allows and which means "the server's own vessel".
    pub context: Option<String>,
    pub updates: Vec<Update>,
}

/// The server's greeting, which names the vessel it considers "self".
///
/// Worth catching: a stream carrying AIS traffic sends a delta per vessel, all
/// structurally identical. Without knowing which context is ours, every ship
/// in the harbour writes over our own position in turn — the boat marker
/// teleports between them and the instrument bar reads whichever vessel spoke
/// last.
#[derive(Deserialize)]
struct Hello {
    #[serde(rename = "self")]
    self_context: Option<String>,
}

/// The `self` context from a server greeting, if this is one.
pub fn parse_hello(text: &str) -> Option<String> {
    serde_json::from_str::<Hello>(text)
        .ok()?
        .self_context
        .filter(|s| !s.is_empty())
}

/// Parse one delta message.
///
/// Returns `None` for the messages that are not deltas at all: the hello
/// greeting the server opens with, and subscription acknowledgements. Those
/// are normal traffic, not errors, so they are skipped rather than logged.
pub fn parse(text: &str) -> Option<Delta> {
    let raw: RawDelta = serde_json::from_str(text).ok()?;
    if raw.updates.is_empty() {
        return None;
    }
    let mut updates = Vec::new();
    for update in raw.updates {
        for v in update.values {
            expand(&v.path, &v.value, &mut updates);
        }
    }
    (!updates.is_empty()).then_some(Delta {
        context: raw.context,
        updates,
    })
}

/// Turn one wire value into one or more readings.
///
/// Most values are scalars, but Signal K also sends small objects where the
/// members are themselves paths — `environment.current` carries `drift` and
/// `setTrue`, `navigation.gnss.satellitesInView` carries `count`. Flattening
/// them into `parent.child` yields exactly the paths the specification names,
/// and keeps real readings out of the bin: before this they all became `Null`,
/// which put a dead entry in the picker for every compound a boat sends.
///
/// Position is the exception, kept whole because it is a coordinate rather
/// than two unrelated numbers, and because it is what puts the boat on the
/// chart.
fn expand(path: &str, v: &serde_json::Value, out: &mut Vec<Update>) {
    // A delta may carry an empty path when it updates a whole subtree. There
    // is nothing to label or display, so it is not a reading.
    if path.is_empty() {
        return;
    }
    let push = |out: &mut Vec<Update>, value| {
        out.push(Update {
            path: path.to_string(),
            value,
        })
    };
    match v {
        serde_json::Value::Number(n) => {
            push(out, n.as_f64().map(Value::Number).unwrap_or(Value::Null))
        }
        serde_json::Value::Null => push(out, Value::Null),
        serde_json::Value::String(s) => push(out, Value::Text(s.clone())),
        serde_json::Value::Bool(b) => push(out, Value::Text(b.to_string())),
        serde_json::Value::Object(map) => {
            match (
                map.get("latitude").and_then(serde_json::Value::as_f64),
                map.get("longitude").and_then(serde_json::Value::as_f64),
            ) {
                (Some(lat), Some(lon)) => push(out, Value::Position { lat, lon }),
                _ => {
                    for (k, child) in map {
                        expand(&format!("{path}.{k}"), child, out);
                    }
                }
            }
        }
        // A list has no path per element and no sensible single reading.
        serde_json::Value::Array(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delta_yields_its_path_value_pairs() {
        let text = r#"{
          "context": "vessels.urn:mrn:signalk:uuid:abc",
          "updates": [{
            "source": {"label": "n2k"},
            "timestamp": "2026-07-27T10:00:00Z",
            "values": [
              {"path": "navigation.speedOverGround", "value": 3.42},
              {"path": "navigation.headingTrue", "value": 3.475}
            ]
          }]
        }"#;
        let d = parse(text).expect("a delta");
        assert_eq!(d.context.as_deref(), Some("vessels.urn:mrn:signalk:uuid:abc"));
        assert_eq!(d.updates.len(), 2);
        assert_eq!(d.updates[0].path, "navigation.speedOverGround");
        assert_eq!(d.updates[0].value.as_number(), Some(3.42));
    }

    #[test]
    fn position_is_understood_because_it_puts_the_boat_on_the_chart() {
        let text = r#"{"updates":[{"values":[
            {"path":"navigation.position","value":{"latitude":55.68,"longitude":12.57}}]}]}"#;
        let d = parse(text).expect("a delta");
        assert_eq!(
            d.updates[0].value,
            Value::Position { lat: 55.68, lon: 12.57 }
        );
        // No context is legal and means the server's own vessel.
        assert_eq!(d.context, None);
    }

    #[test]
    fn an_explicit_null_is_kept_apart_from_never_reported() {
        let text = r#"{"updates":[{"values":[
            {"path":"environment.depth.belowKeel","value":null}]}]}"#;
        let d = parse(text).expect("a delta");
        assert_eq!(d.updates[0].value, Value::Null);
        assert_eq!(d.updates[0].value.as_number(), None);
    }

    #[test]
    fn strings_survive_rather_than_being_dropped() {
        // An unrecognised path is still worth showing to whoever is debugging
        // a boat's wiring at two in the morning.
        let text = r#"{"updates":[{"values":[
            {"path":"navigation.gnss.methodQuality","value":"GNSS Fix"}]}]}"#;
        let d = parse(text).expect("a delta");
        assert_eq!(d.updates[0].value, Value::Text("GNSS Fix".into()));
    }

    #[test]
    fn non_deltas_are_skipped_quietly() {
        // The hello the server opens with — normal traffic, not an error.
        assert!(parse(r#"{"name":"signalk-server","version":"2.30.0","roles":["master"]}"#).is_none());
        // A subscription acknowledgement.
        assert!(parse(r#"{"requestId":"1","state":"COMPLETED","statusCode":200}"#).is_none());
        // An update carrying no values at all.
        assert!(parse(r#"{"updates":[{"values":[]}]}"#).is_none());
        // Not JSON.
        assert!(parse("<html>502 Bad Gateway</html>").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn compound_values_flatten_into_the_paths_the_spec_names() {
        // Observed live on demo.signalk.org. Before flattening, both of these
        // became a single Null and put a dead entry in the instrument picker.
        let text = r#"{"updates":[{"values":[
            {"path":"environment.current","value":{"drift":0.31,"setTrue":2.1}},
            {"path":"navigation.gnss.satellitesInView","value":{"count":11}}]}]}"#;
        let d = parse(text).expect("a delta");
        let mut got: Vec<_> = d
            .updates
            .iter()
            .map(|u| (u.path.as_str(), u.value.as_number()))
            .collect();
        got.sort_by_key(|(path, _)| *path);
        assert_eq!(
            got,
            vec![
                ("environment.current.drift", Some(0.31)),
                ("environment.current.setTrue", Some(2.1)),
                ("navigation.gnss.satellitesInView.count", Some(11.0)),
            ]
        );
    }

    #[test]
    fn another_ship_is_not_us() {
        // The bug this pins: a stream carrying AIS sends one delta per vessel,
        // identical in shape to our own. Applying them all made the boat
        // marker teleport between every ship in range, and the instrument bar
        // read whichever vessel had spoken last.
        let ours = "vessels.urn:mrn:signalk:uuid:mine";
        let mine = parse(&format!(
            r#"{{"context":"{ours}","updates":[{{"values":[
                {{"path":"navigation.speedOverGround","value":1.0}}]}}]}}"#
        ))
        .unwrap();
        let theirs = parse(
            r#"{"context":"vessels.urn:mrn:signalk:uuid:someone-else","updates":[{"values":[
                {"path":"navigation.speedOverGround","value":9.0}]}]}"#,
        )
        .unwrap();

        assert!(mine.is_self(Some(ours)));
        assert!(!theirs.is_self(Some(ours)));

        // No context means the server's own vessel, and `vessels.self` is its
        // alias — both are us whatever the greeting said.
        let bare = parse(r#"{"updates":[{"values":[{"path":"a.b","value":1}]}]}"#).unwrap();
        assert!(bare.is_self(Some(ours)));
        let alias = parse(
            r#"{"context":"vessels.self","updates":[{"values":[{"path":"a.b","value":1}]}]}"#,
        )
        .unwrap();
        assert!(alias.is_self(Some(ours)));

        // Before the greeting arrives, a named context cannot be confirmed as
        // ours — better to miss a reading than to plot someone else's boat.
        assert!(!mine.is_self(None));
    }

    #[test]
    fn the_greeting_names_our_own_vessel() {
        let hello = r#"{"name":"signalk-server","version":"2.30.0",
                        "self":"vessels.urn:mrn:signalk:uuid:abc","roles":["master"]}"#;
        assert_eq!(
            parse_hello(hello).as_deref(),
            Some("vessels.urn:mrn:signalk:uuid:abc")
        );
        // A delta is not a greeting.
        assert_eq!(
            parse_hello(r#"{"updates":[{"values":[{"path":"a.b","value":1}]}]}"#),
            None
        );
        assert_eq!(parse_hello("not json"), None);
    }

    #[test]
    fn an_empty_path_is_not_a_reading() {
        // Sent when a whole subtree is replaced; there is nothing to label.
        assert!(parse(r#"{"updates":[{"values":[{"path":"","value":null}]}]}"#).is_none());
    }

    #[test]
    fn several_updates_in_one_message_all_arrive() {
        // Servers batch by source; one message can carry a whole instrument set.
        let text = r#"{"updates":[
            {"source":{"label":"gps"},"values":[{"path":"navigation.speedOverGround","value":1.0}]},
            {"source":{"label":"wind"},"values":[
                {"path":"environment.wind.speedApparent","value":7.0},
                {"path":"environment.wind.angleApparent","value":0.66}]}]}"#;
        let d = parse(text).expect("a delta");
        assert_eq!(d.updates.len(), 3);
    }
}
