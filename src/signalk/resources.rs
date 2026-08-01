//! Signal K route resources: publish navcore's routes, consume everyone
//! else's.
//!
//! The v1 resources API is plain HTTP beside the WebSocket stream:
//! `GET/PUT /signalk/v1/api/resources/routes[/{id}]`. A route resource is a
//! GeoJSON `LineString` plus a name — which loses navcore's waypoint
//! identities, so those ride in `feature.properties.navcore` where the spec
//! allows arbitrary properties. A navcore route published and consumed again
//! comes back with the same waypoint uuids and names; a route drawn by some
//! other program gets fresh ones, which is all it can ask for.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::nav::model::{Route, Waypoint, WaypointSet};

/// The v1 routes collection path.
const ROUTES_PATH: &str = "/signalk/v1/api/resources/routes";

/// Derive the HTTP base — `http(s)://host[:port]` — from whatever the user
/// typed as their Signal K server, which is the same forgiving set of shapes
/// the stream connector accepts.
pub fn http_base(input: &str) -> Option<String> {
    let raw = input.trim().trim_end_matches('/');
    if raw.is_empty() || crate::signalk::sim::parse_course(raw).is_some() {
        return None;
    }
    let (scheme, rest) = match raw.split_once("://") {
        Some(("wss", r)) | Some(("https", r)) => ("https", r),
        Some((_, r)) => ("http", r),
        None => {
            if raw.contains("demo.signalk.org") {
                ("https", raw)
            } else {
                ("http", raw)
            }
        }
    };
    let host = rest
        .split(['/', '#'])
        .next()
        .filter(|h| !h.is_empty())?;
    let has_port = host.rsplit(':').next().is_some_and(|p| p.parse::<u16>().is_ok());
    Some(if has_port || scheme == "https" {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:3000")
    })
}

/// A route as a Signal K v1 resource document.
pub fn route_to_resource(route: &Route, set: &WaypointSet) -> Value {
    let mut coords = Vec::new();
    let mut names = Vec::new();
    let mut guids = Vec::new();
    for id in &route.waypoints {
        if let Some(wp) = set.get(*id) {
            // GeoJSON is lon, lat — the reverse of everything nautical, and
            // the single most classic way to put a route in the Sahara.
            coords.push(json!([wp.position.lon, wp.position.lat]));
            names.push(json!(wp.name));
            guids.push(json!(wp.id.to_string()));
        }
    }
    json!({
        "name": route.name,
        "description": "",
        "distance": route.total_distance_nm() * crate::geo::METRES_PER_NM,
        "feature": {
            "type": "Feature",
            "geometry": { "type": "LineString", "coordinates": coords },
            "properties": {
                "navcore": { "waypointNames": names, "waypointGuids": guids }
            }
        }
    })
}

/// A Signal K resource back into a route plus its waypoints.
///
/// `resource_id` is the key the server filed it under; it becomes the route's
/// id when it parses as a uuid, so publish-then-consume is identity-stable.
pub fn resource_to_route(resource_id: &str, value: &Value) -> Option<(Route, Vec<Waypoint>)> {
    let coords = value
        .pointer("/feature/geometry/coordinates")?
        .as_array()?;
    if coords.len() < 2 {
        return None;
    }
    let names = value
        .pointer("/feature/properties/navcore/waypointNames")
        .and_then(Value::as_array);
    let guids = value
        .pointer("/feature/properties/navcore/waypointGuids")
        .and_then(Value::as_array);

    let mut route = Route::new(
        value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Signal K route"),
    );
    if let Ok(id) = Uuid::parse_str(resource_id) {
        route.id = id;
    }

    let mut waypoints = Vec::new();
    for (i, c) in coords.iter().enumerate() {
        let lon = c.get(0)?.as_f64()?;
        let lat = c.get(1)?.as_f64()?;
        let name = names
            .and_then(|n| n.get(i))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{:03}", i + 1));
        let mut wp = Waypoint::new(name, lat, lon);
        if let Some(id) = guids
            .and_then(|g| g.get(i))
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
        {
            wp.id = id;
        }
        route.waypoints.push(wp.id);
        waypoints.push(wp);
    }
    Some((route, waypoints))
}

/// The HTTP client for the resources API. Blocking, on the caller's thread —
/// which is expected to be a worker, same as every other network call here.
pub struct ResourcesClient {
    base: String,
    agent: ureq::Agent,
}

impl ResourcesClient {
    pub fn new(base: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(10)))
            .build();
        Self {
            base: base.into(),
            agent: config.into(),
        }
    }

    /// Every route the server holds, as `(id, resource)`.
    pub fn list_routes(&self) -> Result<Vec<(String, Value)>, String> {
        let mut response = self
            .agent
            .get(format!("{}{ROUTES_PATH}", self.base))
            .call()
            .map_err(|e| e.to_string())?;
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        let doc: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Ok(doc
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default())
    }

    /// PUT one route under its uuid.
    pub fn put_route(&self, route: &Route, set: &WaypointSet) -> Result<(), String> {
        let body = route_to_resource(route, set);
        self.agent
            .put(format!("{}{ROUTES_PATH}/{}", self.base, route.id))
            .content_type("application/json")
            .send(body.to_string().as_str())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// GET one route back.
    pub fn get_route(&self, id: Uuid) -> Result<Option<(Route, Vec<Waypoint>)>, String> {
        let mut response = self
            .agent
            .get(format!("{}{ROUTES_PATH}/{id}", self.base))
            .call()
            .map_err(|e| e.to_string())?;
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        let doc: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Ok(resource_to_route(&id.to_string(), &doc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route() -> (Route, WaypointSet) {
        let mut set = WaypointSet::default();
        let mut route = Route::new("Grenaa – Anholt");
        for (n, lat, lon) in [
            ("Grenaa out", 56.4162345, 10.9367890),
            ("Mid Kattegat", 56.55, 11.30),
            ("Anholt harbour", 56.7160494, 11.5098765),
        ] {
            route.waypoints.push(set.insert(Waypoint::new(n, lat, lon)));
        }
        route.recompute_legs(&set);
        (route, set)
    }

    /// The M2 verification, at the layer a unit test can hold: what a PUT
    /// sends, GET back through the same conversion, identical.
    #[test]
    fn publish_then_consume_is_identity_stable() {
        let (route, set) = route();
        let resource = route_to_resource(&route, &set);
        let (back, wps) = resource_to_route(&route.id.to_string(), &resource).unwrap();

        assert_eq!(back.id, route.id);
        assert_eq!(back.name, route.name);
        assert_eq!(back.waypoints, route.waypoints, "waypoint identity survived");
        for (id, wp) in route.waypoints.iter().zip(&wps) {
            let orig = set.get(*id).unwrap();
            assert_eq!(orig.position, wp.position);
            assert_eq!(orig.name, wp.name);
        }
    }

    #[test]
    fn geojson_order_is_lon_lat() {
        let (route, set) = route();
        let resource = route_to_resource(&route, &set);
        let first = resource
            .pointer("/feature/geometry/coordinates/0")
            .unwrap();
        // Kattegat: lon ≈ 11, lat ≈ 56. If these swap, the route is in Chad.
        assert!((first[0].as_f64().unwrap() - 10.9367890).abs() < 1e-9);
        assert!((first[1].as_f64().unwrap() - 56.4162345).abs() < 1e-9);
        // And distance is metres, not miles.
        let d = resource["distance"].as_f64().unwrap();
        assert!((30_000.0..80_000.0).contains(&d), "{d} m");
    }

    #[test]
    fn a_foreign_resource_without_navcore_properties_still_loads() {
        let foreign = serde_json::json!({
            "name": "qtVlm route",
            "feature": { "type": "Feature",
                "geometry": { "type": "LineString",
                    "coordinates": [[11.0, 56.0], [11.5, 56.5]] },
                "properties": {} }
        });
        let (route, wps) = resource_to_route("not-a-uuid", &foreign).unwrap();
        assert_eq!(route.name, "qtVlm route");
        assert_eq!(wps.len(), 2);
        assert_eq!(wps[0].name, "001");
        assert!((wps[1].position.lat - 56.5).abs() < 1e-12);

        // Degenerate ones are refused, not half-loaded.
        let one_point = serde_json::json!({
            "feature": { "geometry": { "coordinates": [[11.0, 56.0]] } }
        });
        assert!(resource_to_route("x", &one_point).is_none());
    }

    #[test]
    fn http_bases_derive_from_what_users_type() {
        assert_eq!(http_base("192.168.1.50").as_deref(), Some("http://192.168.1.50:3000"));
        assert_eq!(http_base("192.168.1.50:8080").as_deref(), Some("http://192.168.1.50:8080"));
        assert_eq!(
            http_base("wss://demo.signalk.org/signalk/v1/stream").as_deref(),
            Some("https://demo.signalk.org")
        );
        assert_eq!(http_base("demo.signalk.org").as_deref(), Some("https://demo.signalk.org"));
        assert_eq!(
            http_base("http://boat.local:3000/admin/#/dashboard").as_deref(),
            Some("http://boat.local:3000")
        );
        // A simulated boat has no HTTP API.
        assert_eq!(http_base("sim"), None);
        assert_eq!(http_base(""), None);
    }
}
