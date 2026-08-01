//! GPX 1.1, read and written by hand.
//!
//! Hand-rolled on `quick-xml` rather than the `gpx` crate for one reason the
//! spec makes non-negotiable: **round-trips through other plotters must be
//! lossless**, and the crate drops `<extensions>` elements it does not
//! recognise. OpenCPN keeps its route bookkeeping — guids, visibility,
//! planned speeds — in exactly those elements, so importing a route and
//! exporting it back must return them verbatim or OpenCPN sees a stranger.
//!
//! The split is: extension children in the `navcore:` namespace are *ours* —
//! parsed into typed fields on import, regenerated on export. Everything else
//! is foreign — carried as raw XML and re-emitted untouched. One foreign
//! element is peeked at without being consumed: `opencpn:guid`, adopted as
//! the object's id when we have none of our own, so re-importing the same
//! OpenCPN route twice yields the same identity rather than a duplicate.

use quick_xml::events::Event;
use quick_xml::name::QName;
use quick_xml::{Reader, Writer};
use uuid::Uuid;

use super::model::{LegKind, LegPlan, Route, RouteProvenance, Waypoint, WaypointSet};

/// The navcore GPX extension namespace.
pub const NAVCORE_NS: &str = "https://navcore.io/gpx/1";

#[derive(Debug)]
pub enum GpxError {
    Xml(String),
    /// Structurally XML, but not a GPX we can read.
    Malformed(String),
}

impl std::fmt::Display for GpxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpxError::Xml(e) => write!(f, "not well-formed XML: {e}"),
            GpxError::Malformed(e) => write!(f, "not a readable GPX: {e}"),
        }
    }
}

impl std::error::Error for GpxError {}

/// One GPX file, parsed.
#[derive(Debug, Default)]
pub struct GpxDocument {
    /// Every waypoint in the file — standalone `<wpt>` and route `<rtept>`
    /// alike, deduplicated by id.
    pub waypoints: WaypointSet,
    /// The standalone `<wpt>` ids, in file order.
    pub loose: Vec<Uuid>,
    /// The routes, legs computed, in file order.
    pub routes: Vec<Route>,
    /// Root-element attributes other than the ones navcore writes itself —
    /// chiefly foreign namespace declarations (`xmlns:opencpn=…`), which must
    /// come back on export or the preserved extensions dangle unprefixed.
    pub root_attrs: Vec<(String, String)>,
}

/// Parse one GPX file.
pub fn parse(text: &str) -> Result<GpxDocument, GpxError> {
    let mut reader = Reader::from_str(text);
    reader.trim_text(true);
    let mut doc = GpxDocument::default();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"gpx" => {
                    for attr in e.attributes().flatten() {
                        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
                        // Ours are regenerated on write; keeping them here
                        // would duplicate attributes in the output.
                        if matches!(key.as_str(), "version" | "creator" | "xmlns" | "xmlns:navcore")
                        {
                            continue;
                        }
                        let value = attr
                            .unescape_value()
                            .map_err(|e| GpxError::Xml(e.to_string()))?
                            .into_owned();
                        doc.root_attrs.push((key, value));
                    }
                }
                b"wpt" => {
                    let wp = parse_point(&mut reader, &e, "wpt")?.waypoint;
                    doc.loose.push(wp.id);
                    doc.waypoints.insert(wp);
                }
                b"rte" => parse_route(&mut reader, &mut doc)?,
                _ => {}
            },
            // Self-closing points are legal GPX: all position, no children.
            Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"wpt" => {
                    let wp = point_from_attrs(&e, "wpt")?;
                    doc.loose.push(wp.id);
                    doc.waypoints.insert(wp);
                }
                // Tracks are a recorder's output, not a plan; navcore neither
                // edits nor rewrites files containing them (see the store),
                // so skipping is safe rather than lossy.
                b"trk" => {
                    reader
                        .read_to_end(QName(b"trk"))
                        .map_err(|e| GpxError::Xml(e.to_string()))?;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(GpxError::Xml(e.to_string())),
            _ => {}
        }
    }
    Ok(doc)
}

/// What a `<wpt>`/`<rtept>` parse yields beyond the waypoint itself.
struct ParsedPoint {
    waypoint: Waypoint,
    /// The plan for the leg *starting* at this route point, if one was saved.
    plan: Option<LegPlan>,
    /// The kind of the leg starting here.
    leg_kind: Option<LegKind>,
}

/// A waypoint from a point element's attributes alone.
fn point_from_attrs(
    start: &quick_xml::events::BytesStart<'_>,
    tag: &str,
) -> Result<Waypoint, GpxError> {
    let mut lat: Option<f64> = None;
    let mut lon: Option<f64> = None;
    for attr in start.attributes().flatten() {
        let v = attr
            .unescape_value()
            .map_err(|e| GpxError::Xml(e.to_string()))?;
        match attr.key.as_ref() {
            b"lat" => lat = v.trim().parse().ok(),
            b"lon" => lon = v.trim().parse().ok(),
            _ => {}
        }
    }
    match (lat, lon) {
        (Some(lat), Some(lon)) => Ok(Waypoint::new("", lat, lon)),
        _ => Err(GpxError::Malformed(format!("<{tag}> without lat/lon"))),
    }
}

fn parse_point(
    reader: &mut Reader<&[u8]>,
    start: &quick_xml::events::BytesStart<'_>,
    tag: &str,
) -> Result<ParsedPoint, GpxError> {
    let mut wp = point_from_attrs(start, tag)?;
    let mut plan = None;
    let mut leg_kind = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"name" => wp.name = text_of(reader, "name")?,
                b"desc" => wp.description = Some(text_of(reader, "desc")?),
                b"sym" => wp.symbol = Some(text_of(reader, "sym")?),
                b"extensions" => {
                    let raw = reader
                        .read_text(QName(b"extensions"))
                        .map_err(|e| GpxError::Xml(e.to_string()))?;
                    let split = split_extensions(&raw)?;
                    if let Some(id) = split.identity() {
                        wp.id = id;
                    }
                    wp.arrival_radius_nm = split.arrival_radius_nm;
                    plan = split.plan;
                    leg_kind = split.leg_kind;
                    wp.foreign_extensions = split.foreign;
                }
                other => {
                    // <time>, <cmt>, <link>… — valid GPX navcore has no field
                    // for. Skipped, not preserved: they are per-export
                    // metadata, unlike extensions, which are another program's
                    // state.
                    let name = other.to_vec();
                    reader
                        .read_to_end(QName(&name))
                        .map_err(|e| GpxError::Xml(e.to_string()))?;
                }
            },
            Ok(Event::End(e)) if e.name().as_ref() == tag.as_bytes() => break,
            Ok(Event::Eof) => return Err(GpxError::Malformed(format!("unclosed <{tag}>"))),
            Err(e) => return Err(GpxError::Xml(e.to_string())),
            _ => {}
        }
    }
    Ok(ParsedPoint {
        waypoint: wp,
        plan,
        leg_kind,
    })
}

fn parse_route(reader: &mut Reader<&[u8]>, doc: &mut GpxDocument) -> Result<(), GpxError> {
    let mut route = Route::new("");
    let mut points: Vec<ParsedPoint> = Vec::new();
    let mut provenance: Option<RouteProvenance> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"name" => route.name = text_of(reader, "name")?,
                b"rtept" => points.push(parse_point(reader, &e, "rtept")?),
                b"extensions" => {
                    let raw = reader
                        .read_text(QName(b"extensions"))
                        .map_err(|e| GpxError::Xml(e.to_string()))?;
                    let split = split_extensions(&raw)?;
                    if let Some(id) = split.identity() {
                        route.id = id;
                    }
                    provenance = split.provenance;
                    route.foreign_extensions = split.foreign;
                }
                other => {
                    let name = other.to_vec();
                    reader
                        .read_to_end(QName(&name))
                        .map_err(|e| GpxError::Xml(e.to_string()))?;
                }
            },
            Ok(Event::Empty(e)) if e.name().as_ref() == b"rtept" => {
                points.push(ParsedPoint {
                    waypoint: point_from_attrs(&e, "rtept")?,
                    plan: None,
                    leg_kind: None,
                });
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"rte" => break,
            Ok(Event::Eof) => return Err(GpxError::Malformed("unclosed <rte>".into())),
            Err(e) => return Err(GpxError::Xml(e.to_string())),
            _ => {}
        }
    }

    route.generated = provenance;
    for p in &points {
        route.waypoints.push(p.waypoint.id);
    }
    let plans: Vec<_> = points.iter().map(|p| (p.plan.clone(), p.leg_kind)).collect();
    for p in points {
        doc.waypoints.insert(p.waypoint);
    }
    route.recompute_legs(&doc.waypoints);
    // Leg i starts at route point i; its saved plan and kind rode on that
    // point. Setting a kind changes the measure, and recompute_legs preserves
    // kind and plan by (from, to) — so one more pass re-measures correctly.
    let mut kinds_changed = false;
    for (i, leg) in route.legs.iter_mut().enumerate() {
        if let Some((plan, kind)) = plans.get(i) {
            leg.plan = plan.clone();
            if let Some(k) = kind {
                kinds_changed |= leg.kind != *k;
                leg.kind = *k;
            }
        }
    }
    if kinds_changed {
        route.recompute_legs(&doc.waypoints);
    }
    doc.routes.push(route);
    Ok(())
}

/// Plain text content of a simple element, entities unescaped.
fn text_of(reader: &mut Reader<&[u8]>, tag: &str) -> Result<String, GpxError> {
    let mut out = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Text(t)) => {
                out.push_str(&t.unescape().map_err(|e| GpxError::Xml(e.to_string()))?)
            }
            Ok(Event::End(e)) if e.name().as_ref() == tag.as_bytes() => break,
            Ok(Event::Eof) => return Err(GpxError::Malformed(format!("unclosed <{tag}>"))),
            Err(e) => return Err(GpxError::Xml(e.to_string())),
            _ => {}
        }
    }
    Ok(out.trim().to_string())
}

/// The result of dividing an `<extensions>` blob into ours and everyone
/// else's.
#[derive(Default)]
struct SplitExtensions {
    guid: Option<String>,
    opencpn_guid: Option<String>,
    arrival_radius_nm: Option<f64>,
    plan: Option<LegPlan>,
    leg_kind: Option<LegKind>,
    provenance: Option<RouteProvenance>,
    /// Everything foreign, re-serialized verbatim.
    foreign: String,
}

impl SplitExtensions {
    /// The id to adopt: our own first, then OpenCPN's — so a route imported
    /// from OpenCPN twice is the same route, not a duplicate.
    fn identity(&self) -> Option<Uuid> {
        self.guid
            .as_deref()
            .and_then(|s| Uuid::parse_str(s.trim()).ok())
            .or_else(|| {
                self.opencpn_guid
                    .as_deref()
                    .and_then(|s| Uuid::parse_str(s.trim()).ok())
            })
    }
}

fn split_extensions(raw: &str) -> Result<SplitExtensions, GpxError> {
    let mut reader = Reader::from_str(raw);
    let mut writer = Writer::new(Vec::new());
    let mut out = SplitExtensions::default();
    // Set when the passthrough just wrote <opencpn:guid> — the next text event
    // is peeked (adopted as identity) while still being preserved.
    let mut peeking_opencpn_guid = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let name_str = String::from_utf8_lossy(name.as_ref()).into_owned();
                if let Some(local) = name_str.strip_prefix("navcore:") {
                    // Ours: consume the subtree into a typed field.
                    let text = reader
                        .read_text(QName(name_str.as_bytes()))
                        .map_err(|e| GpxError::Xml(e.to_string()))?;
                    apply_navcore(&mut out, local, text.trim());
                } else {
                    peeking_opencpn_guid = name_str == "opencpn:guid";
                    writer
                        .write_event(Event::Start(e))
                        .map_err(|e| GpxError::Xml(e.to_string()))?;
                }
            }
            Ok(Event::Empty(e)) => {
                let name_str = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if !name_str.starts_with("navcore:") {
                    writer
                        .write_event(Event::Empty(e))
                        .map_err(|e| GpxError::Xml(e.to_string()))?;
                }
            }
            Ok(Event::Text(t)) => {
                if peeking_opencpn_guid {
                    out.opencpn_guid = Some(
                        t.unescape()
                            .map_err(|e| GpxError::Xml(e.to_string()))?
                            .into_owned(),
                    );
                    peeking_opencpn_guid = false;
                }
                writer
                    .write_event(Event::Text(t))
                    .map_err(|e| GpxError::Xml(e.to_string()))?;
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(GpxError::Xml(e.to_string())),
            Ok(ev) => {
                writer
                    .write_event(ev)
                    .map_err(|e| GpxError::Xml(e.to_string()))?;
            }
        }
    }
    out.foreign = String::from_utf8(writer.into_inner())
        .map_err(|e| GpxError::Xml(e.to_string()))?
        .trim()
        .to_string();
    Ok(out)
}

fn apply_navcore(out: &mut SplitExtensions, local: &str, text: &str) {
    // `read_text` hands back the text as it sits in the file — entities still
    // escaped. JSON full of `&quot;` is not JSON yet.
    let text: String = quick_xml::escape::unescape(text)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| text.to_string());
    let text = text.trim();
    match local {
        "guid" => out.guid = Some(text.to_string()),
        "arrival_radius_nm" => out.arrival_radius_nm = text.parse().ok(),
        "leg_kind" => {
            out.leg_kind = match text {
                "great-circle" => Some(LegKind::GreatCircle),
                "rhumb-line" => Some(LegKind::RhumbLine),
                _ => None,
            }
        }
        "plan" => match serde_json::from_str(text) {
            Ok(p) => out.plan = Some(p),
            Err(e) => log::warn!("gpx: unreadable navcore:plan ignored: {e}"),
        },
        "provenance" => match serde_json::from_str(text) {
            Ok(p) => out.provenance = Some(p),
            Err(e) => log::warn!("gpx: unreadable navcore:provenance ignored: {e}"),
        },
        // A navcore element this build does not know — from a newer navcore.
        // Dropping it is the price of the namespace split; log so it is not
        // silent.
        other => log::warn!("gpx: unknown navcore:{other} ignored"),
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Serialize one route (with its waypoints inlined) to a GPX file.
pub fn write_route(route: &Route, set: &WaypointSet, root_attrs: &[(String, String)]) -> String {
    let mut out = String::with_capacity(2048);
    write_header(&mut out, root_attrs);
    push_route(&mut out, route, set);
    out.push_str("</gpx>\n");
    out
}

/// Serialize standalone waypoints to a GPX file.
pub fn write_waypoints(
    waypoints: &[&Waypoint],
    root_attrs: &[(String, String)],
) -> String {
    let mut out = String::with_capacity(1024);
    write_header(&mut out, root_attrs);
    for wp in waypoints {
        push_point(&mut out, "wpt", wp, None, None, "  ");
    }
    out.push_str("</gpx>\n");
    out
}

fn write_header(out: &mut String, root_attrs: &[(String, String)]) {
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<gpx version=\"1.1\" creator=\"navcore\" \
         xmlns=\"http://www.topografix.com/GPX/1/1\" ",
    );
    out.push_str(&format!("xmlns:navcore=\"{NAVCORE_NS}\""));
    // OpenCPN's namespace is declared even when no extensions reference it:
    // a file that gains an OpenCPN route later must not become invalid, and
    // a spurious namespace declaration costs nothing.
    let mut wrote_opencpn = false;
    for (k, v) in root_attrs {
        out.push(' ');
        out.push_str(&format!("{k}=\"{}\"", esc(v)));
        if k == "xmlns:opencpn" {
            wrote_opencpn = true;
        }
    }
    if !wrote_opencpn {
        out.push_str(" xmlns:opencpn=\"http://www.opencpn.org\"");
    }
    out.push_str(">\n");
}

fn push_route(out: &mut String, route: &Route, set: &WaypointSet) {
    out.push_str("  <rte>\n");
    out.push_str(&format!("    <name>{}</name>\n", esc(&route.name)));
    out.push_str("    <extensions>\n");
    out.push_str(&format!(
        "      <navcore:guid>{}</navcore:guid>\n",
        route.id
    ));
    if let Some(p) = &route.generated {
        match serde_json::to_string(p) {
            Ok(json) => out.push_str(&format!(
                "      <navcore:provenance>{}</navcore:provenance>\n",
                esc(&json)
            )),
            Err(e) => log::warn!("gpx: provenance not serializable: {e}"),
        }
    }
    push_foreign(out, &route.foreign_extensions, "      ");
    out.push_str("    </extensions>\n");

    for (i, id) in route.waypoints.iter().enumerate() {
        let Some(wp) = set.get(*id) else {
            log::warn!("gpx: route {} references missing waypoint {id}", route.name);
            continue;
        };
        // The leg starting at point i carries its plan and kind on that point.
        let leg = route.legs.get(i);
        push_point(
            out,
            "rtept",
            wp,
            leg.and_then(|l| l.plan.as_ref()),
            leg.map(|l| l.kind),
            "    ",
        );
    }
    out.push_str("  </rte>\n");
}

fn push_point(
    out: &mut String,
    tag: &str,
    wp: &Waypoint,
    plan: Option<&LegPlan>,
    leg_kind: Option<LegKind>,
    indent: &str,
) {
    // `{}` on f64 prints the shortest string that parses back to the same
    // bits — positions round-trip exactly, not merely to some decimal count.
    out.push_str(&format!(
        "{indent}<{tag} lat=\"{}\" lon=\"{}\">\n",
        wp.position.lat, wp.position.lon
    ));
    if !wp.name.is_empty() {
        out.push_str(&format!("{indent}  <name>{}</name>\n", esc(&wp.name)));
    }
    if let Some(d) = &wp.description {
        out.push_str(&format!("{indent}  <desc>{}</desc>\n", esc(d)));
    }
    if let Some(s) = &wp.symbol {
        out.push_str(&format!("{indent}  <sym>{}</sym>\n", esc(s)));
    }
    out.push_str(&format!("{indent}  <extensions>\n"));
    out.push_str(&format!(
        "{indent}    <navcore:guid>{}</navcore:guid>\n",
        wp.id
    ));
    if let Some(r) = wp.arrival_radius_nm {
        out.push_str(&format!(
            "{indent}    <navcore:arrival_radius_nm>{r}</navcore:arrival_radius_nm>\n"
        ));
    }
    // The default kind is not written: files stay minimal and a hand-edited
    // GPX without the element means what it should.
    if let Some(LegKind::RhumbLine) = leg_kind {
        out.push_str(&format!(
            "{indent}    <navcore:leg_kind>rhumb-line</navcore:leg_kind>\n"
        ));
    }
    if let Some(p) = plan {
        match serde_json::to_string(p) {
            Ok(json) => out.push_str(&format!(
                "{indent}    <navcore:plan>{}</navcore:plan>\n",
                esc(&json)
            )),
            Err(e) => log::warn!("gpx: leg plan not serializable: {e}"),
        }
    }
    push_foreign(out, &wp.foreign_extensions, &format!("{indent}    "));
    out.push_str(&format!("{indent}  </extensions>\n"));
    out.push_str(&format!("{indent}</{tag}>\n"));
}

fn push_foreign(out: &mut String, foreign: &str, indent: &str) {
    if foreign.is_empty() {
        return;
    }
    // Verbatim, one line: re-indenting someone else's XML risks changing
    // whitespace-sensitive content for a purely cosmetic gain.
    out.push_str(indent);
    out.push_str(foreign);
    out.push('\n');
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn five_leg_route() -> (Route, WaypointSet) {
        // A plausible little Kattegat passage — six points, five legs, with
        // awkward content: a name needing escaping, a description, a symbol,
        // a per-waypoint arrival radius.
        let mut set = WaypointSet::default();
        let points = [
            ("Grenaa out", 56.4162345, 10.9367890),
            ("Fornæs", 56.4438272, 10.9645061),
            ("Mid Kattegat", 56.5500000, 11.3000000),
            ("Anholt W & <mark>", 56.6987654, 11.5087654),
            ("Anholt N", 56.7500001, 11.5666667),
            ("Anholt harbour", 56.7160494, 11.5098765),
        ];
        let mut route = Route::new("Grenaa – Anholt");
        for (i, (name, lat, lon)) in points.iter().enumerate() {
            let mut wp = Waypoint::new(*name, *lat, *lon);
            if i == 5 {
                wp.arrival_radius_nm = Some(0.05);
                wp.symbol = Some("harbor".into());
                wp.description = Some("Small harbour, 2 m at the entrance".into());
            }
            route.waypoints.push(set.insert(wp));
        }
        route.recompute_legs(&set);
        assert_eq!(route.legs.len(), 5);
        (route, set)
    }

    #[test]
    fn a_five_leg_route_round_trips_exactly() {
        let (route, set) = five_leg_route();
        let xml = write_route(&route, &set, &[]);
        let doc = parse(&xml).expect("our own output parses");

        assert_eq!(doc.routes.len(), 1);
        let back = &doc.routes[0];
        assert_eq!(back.id, route.id);
        assert_eq!(back.name, route.name);
        assert_eq!(back.waypoints.len(), 6);
        assert_eq!(back.legs.len(), 5);

        // The DoD asks 1e-7°; `{}` round-trips f64 exactly, so assert that.
        for (i, id) in route.waypoints.iter().enumerate() {
            let a = set.get(*id).unwrap();
            let b = doc.waypoints.get(back.waypoints[i]).unwrap();
            assert_eq!(a.position, b.position, "point {i} moved");
            assert_eq!(a.name, b.name);
            assert_eq!(a.id, b.id, "identity lost at point {i}");
        }
        let last = doc.waypoints.get(back.waypoints[5]).unwrap();
        assert_eq!(last.arrival_radius_nm, Some(0.05));
        assert_eq!(last.symbol.as_deref(), Some("harbor"));
        // Leg order: same from→to sequence.
        for (a, b) in route.legs.iter().zip(&back.legs) {
            assert_eq!((a.from, a.to), (b.from, b.to));
        }
    }

    /// Hand-written in the exact shape OpenCPN 5.x exports, opencpn:*
    /// bookkeeping and all.
    const OPENCPN_FILE: &str = r#"<?xml version="1.0"?>
<gpx version="1.1" creator="OpenCPN" xmlns="http://www.topografix.com/GPX/1/1" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:opencpn="http://www.opencpn.org" xsi:schemaLocation="http://www.topografix.com/GPX/1/1 http://www.topografix.com/GPX/1/1/gpx.xsd">
  <rte>
    <name>OpenCPN test route</name>
    <extensions>
      <opencpn:guid>2f0d80a2-91c1-4a17-8f4e-9b0a3c5d7e11</opencpn:guid>
      <opencpn:viz>1</opencpn:viz>
      <opencpn:planned_speed>6.00</opencpn:planned_speed>
      <opencpn:time_display>PC</opencpn:time_display>
    </extensions>
    <rtept lat="56.1234567" lon="11.7654321">
      <time>2026-07-01T10:00:00Z</time>
      <name>001</name>
      <sym>diamond</sym>
      <extensions>
        <opencpn:guid>7a1b2c3d-0000-4a17-8f4e-9b0a3c5d7e22</opencpn:guid>
        <opencpn:viz_name>0</opencpn:viz_name>
      </extensions>
    </rtept>
    <rtept lat="56.2000001" lon="11.8000002">
      <name>002</name>
      <sym>diamond</sym>
      <extensions>
        <opencpn:guid>7a1b2c3d-0000-4a17-8f4e-9b0a3c5d7e33</opencpn:guid>
      </extensions>
    </rtept>
  </rte>
</gpx>
"#;

    #[test]
    fn an_opencpn_route_survives_the_round_trip_with_its_bookkeeping() {
        let doc = parse(OPENCPN_FILE).expect("OpenCPN's shape parses");
        assert_eq!(doc.routes.len(), 1);
        let route = &doc.routes[0];

        // OpenCPN's guid is adopted as identity, so re-import is idempotent…
        assert_eq!(
            route.id,
            Uuid::parse_str("2f0d80a2-91c1-4a17-8f4e-9b0a3c5d7e11").unwrap()
        );
        // …and still preserved verbatim for OpenCPN's own benefit.
        assert!(route.foreign_extensions.contains("opencpn:guid"));
        assert!(route.foreign_extensions.contains("planned_speed"));

        let xml = write_route(route, &doc.waypoints, &doc.root_attrs);
        assert!(xml.contains("<opencpn:guid>2f0d80a2-91c1-4a17-8f4e-9b0a3c5d7e11</opencpn:guid>"));
        assert!(xml.contains("<opencpn:planned_speed>6.00</opencpn:planned_speed>"));
        assert!(xml.contains("<opencpn:viz_name>0</opencpn:viz_name>"));
        // The foreign namespace declaration came along too.
        assert!(xml.contains("xmlns:opencpn="));
        assert!(xml.contains("xmlns:xsi="));

        // And a second parse of our own output holds position to the bit.
        let again = parse(&xml).expect("round-tripped output parses");
        let b = &again.routes[0];
        assert_eq!(b.id, route.id);
        for (i, id) in route.waypoints.iter().enumerate() {
            assert_eq!(
                doc.waypoints.get(*id).unwrap().position,
                again.waypoints.get(b.waypoints[i]).unwrap().position,
                "point {i}"
            );
        }
    }

    #[test]
    fn loose_waypoints_round_trip() {
        let mut wp = Waypoint::new("Fishing spot & anchor", 56.5, 11.25);
        wp.symbol = Some("anchor".into());
        wp.arrival_radius_nm = Some(0.02);
        let xml = write_waypoints(&[&wp], &[]);
        let doc = parse(&xml).unwrap();
        assert_eq!(doc.loose.len(), 1);
        let back = doc.waypoints.get(doc.loose[0]).unwrap();
        assert_eq!(back.id, wp.id);
        assert_eq!(back.name, wp.name);
        assert_eq!(back.position, wp.position);
        assert_eq!(back.arrival_radius_nm, Some(0.02));
    }

    #[test]
    fn plans_and_leg_kinds_ride_the_route_points() {
        use crate::nav::model::{PointOfSail, TackState};
        let (mut route, set) = five_leg_route();
        route.legs[1].kind = LegKind::RhumbLine;
        route.legs[2].plan = Some(LegPlan {
            eta: chrono::DateTime::from_timestamp(1_780_000_000, 0).unwrap(),
            twa_deg: 60.0,
            tws_kt: 14.0,
            point_of_sail: PointOfSail::CloseReach,
            expected_stw_kt: 6.8,
            tack_state: TackState::Port,
        });
        let xml = write_route(&route, &set, &[]);
        let doc = parse(&xml).unwrap();
        let back = &doc.routes[0];
        assert_eq!(back.legs[1].kind, LegKind::RhumbLine);
        assert_eq!(back.legs[0].kind, LegKind::GreatCircle);
        let plan = back.legs[2].plan.as_ref().expect("plan survived");
        assert_eq!(plan.tws_kt, 14.0);
        assert_eq!(plan.tack_state, TackState::Port);
        assert!(back.legs[3].plan.is_none());
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        assert!(parse("not xml at all").is_err() || parse("not xml at all").unwrap().routes.is_empty());
        assert!(parse("<gpx><rte><rtept></rte></gpx>").is_err());
        assert!(parse("<gpx><rte><rtept lat=\"x\" lon=\"y\"/></rte></gpx>").is_err());
        // A track is skipped whole, not half-parsed.
        let with_track = "<gpx><trk><name>t</name><trkseg><trkpt lat=\"1\" lon=\"2\"/></trkseg></trk></gpx>";
        let doc = parse(with_track).unwrap();
        assert!(doc.routes.is_empty() && doc.loose.is_empty());
    }
}
