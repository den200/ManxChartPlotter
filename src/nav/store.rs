//! Where routes live: a directory of GPX files.
//!
//! The files **are** the store — there is no database that the GPX merely
//! mirrors. That was a decision, not a default: the files are readable by any
//! other plotter as they sit, they survive Manx, and backup is `cp -r`.
//! One route per file, plus `waypoints.gpx` for the standalone marks. The
//! directory sits beside `settings.json`, so everything a user would want to
//! keep lives under one `manx/` folder.
//!
//! Files Manx did not write are respected: on open, a foreign multi-route
//! file is split into store-shaped files and the original renamed aside — its
//! content preserved across the copies, extensions and all — never rewritten
//! in place.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::gpx;
use super::model::{Route, Waypoint, WaypointSet};

/// The file the standalone waypoints live in.
const WAYPOINTS_FILE: &str = "waypoints.gpx";

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Gpx(PathBuf, gpx::GpxError),
    /// The waypoint is used by routes and cannot simply vanish.
    WaypointInUse { routes: Vec<String> },
    UnknownRoute(Uuid),
    /// A second file in the folder holds a route already read from another.
    /// The first is used; this one is left where it is, unread.
    Duplicate { path: PathBuf, name: String },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "route store: {e}"),
            StoreError::Gpx(p, e) => write!(f, "{}: {e}", p.display()),
            StoreError::WaypointInUse { routes } => {
                write!(f, "waypoint is used by: {}", routes.join(", "))
            }
            StoreError::UnknownRoute(id) => write!(f, "no route {id}"),
            StoreError::Duplicate { path, name } => write!(
                f,
                "{}: another file already holds the route \"{name}\"; this one was not read",
                path.display()
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

struct StoredRoute {
    route: Route,
    path: PathBuf,
    /// Foreign root attributes of the file this came from, re-emitted on save.
    root_attrs: Vec<(String, String)>,
}

/// All the user's waypoints and routes, backed by a directory of GPX files.
pub struct RouteStore {
    dir: PathBuf,
    pub waypoints: WaypointSet,
    loose: Vec<Uuid>,
    routes: Vec<StoredRoute>,
}

impl RouteStore {
    /// The conventional location: beside `settings.json`.
    pub fn default_dir() -> Option<PathBuf> {
        Some(crate::paths::config_dir()?.join("routes"))
    }

    /// Open (creating if absent) the store at `dir` and load everything in it.
    ///
    /// A file that fails to parse is reported and skipped, never deleted or
    /// rewritten: an unreadable file is the user's data in trouble, and the
    /// worst possible response is to make it Manx's data instead.
    pub fn open(dir: impl Into<PathBuf>) -> Result<(Self, Vec<StoreError>), StoreError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let mut store = Self {
            dir: dir.clone(),
            waypoints: WaypointSet::default(),
            loose: Vec::new(),
            routes: Vec::new(),
        };
        let mut problems = Vec::new();

        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("gpx")))
            .collect();
        paths.sort();

        for path in paths {
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    problems.push(StoreError::Io(e));
                    continue;
                }
            };
            let doc = match gpx::parse(&text) {
                Ok(d) => d,
                Err(e) => {
                    problems.push(StoreError::Gpx(path.clone(), e));
                    continue;
                }
            };
            store.absorb(doc, &path, &mut problems);
        }
        Ok((store, problems))
    }

    fn absorb(&mut self, doc: gpx::GpxDocument, path: &Path, problems: &mut Vec<StoreError>) {
        let is_waypoints_file = path.file_name().is_some_and(|n| n == WAYPOINTS_FILE);
        let foreign_shape = doc.routes.len() > 1
            || (!doc.routes.is_empty() && !doc.loose.is_empty())
            || (!doc.loose.is_empty() && !is_waypoints_file);

        for wp in doc.waypoints.iter() {
            self.waypoints.insert(wp.clone());
        }
        for id in &doc.loose {
            if !self.loose.contains(id) {
                self.loose.push(*id);
            }
        }
        // One route, one file. Two files with the same route would each be
        // "the" route to a different operation — a save would go to one, a
        // delete would remove one and the other would bring the route back
        // at the next start. The first read is kept; the other is reported.
        let mut routes = Vec::new();
        for route in doc.routes {
            if self.routes.iter().any(|s| s.route.id == route.id) {
                problems.push(StoreError::Duplicate { path: path.to_path_buf(), name: route.name.clone() });
            } else {
                routes.push(route);
            }
        }
        let route_count = routes.len();
        for route in routes {
            let target = if foreign_shape {
                self.route_path(&route)
            } else {
                path.to_path_buf()
            };
            self.routes.push(StoredRoute {
                route,
                path: target,
                root_attrs: doc.root_attrs.clone(),
            });
        }

        if foreign_shape {
            // Re-home the content into store-shaped files, then move the
            // original aside untouched. Nothing is lost: every route and
            // waypoint, extensions included, is now in the new files, and the
            // original still exists byte-for-byte under `.imported`.
            let n = self.routes.len();
            for i in n - route_count..n {
                if let Err(e) = self.save_route_at(i) {
                    problems.push(e);
                    return; // leave the original in place if we could not copy
                }
            }
            if !doc.loose.is_empty() {
                if let Err(e) = self.save_loose() {
                    problems.push(e);
                    return;
                }
            }
            let aside = path.with_extension("gpx.imported");
            if let Err(e) = std::fs::rename(path, &aside) {
                problems.push(StoreError::Io(e));
            } else {
                log::info!(
                    "route store: split {} into Manx files; original kept at {}",
                    path.display(),
                    aside.display()
                );
            }
        }
    }

    pub fn routes(&self) -> impl Iterator<Item = &Route> {
        self.routes.iter().map(|s| &s.route)
    }

    pub fn route(&self, id: Uuid) -> Option<&Route> {
        self.routes.iter().find(|s| s.route.id == id).map(|s| &s.route)
    }

    pub fn loose_waypoints(&self) -> impl Iterator<Item = &Waypoint> {
        self.loose.iter().filter_map(|id| self.waypoints.get(*id))
    }

    /// Add or replace a route, recompute its legs, and write its file.
    pub fn upsert_route(&mut self, mut route: Route) -> Result<(), StoreError> {
        route.recompute_legs(&self.waypoints);
        let i = match self.routes.iter().position(|s| s.route.id == route.id) {
            Some(i) => {
                self.routes[i].route = route;
                i
            }
            None => {
                let path = self.route_path(&route);
                self.routes.push(StoredRoute {
                    route,
                    path,
                    root_attrs: Vec::new(),
                });
                self.routes.len() - 1
            }
        };
        self.save_route_at(i)
    }

    /// Take in a route read from the Signal K server, with its waypoints.
    ///
    /// Publish and fetch round-trip the same id, and the server keeps only
    /// names and positions. So there are three cases:
    ///
    /// - unknown here: stored as it came;
    /// - known and unchanged (same name, same marks in the same places):
    ///   left exactly as it is — writing the server's copy over it would
    ///   strip symbols, arrival circles, leg kinds and the weather router's
    ///   plan, none of which the server ever had;
    /// - known and different: the local route is kept, and the server's is
    ///   stored beside it as "… (Signal K)". Answers the kept route's name.
    pub fn merge_fetched(
        &mut self,
        mut route: Route,
        mut wps: Vec<Waypoint>,
    ) -> Result<Option<String>, StoreError> {
        type Mark<'a> = (crate::geo::LatLon, &'a str);
        let mut kept = None;
        if let Some(local) = self.route(route.id) {
            let fetched: Vec<Mark> = route
                .waypoints
                .iter()
                .filter_map(|id| wps.iter().find(|w| w.id == *id))
                .map(|w| (w.position, w.name.as_str()))
                .collect();
            let mine: Vec<Mark> = local
                .waypoints
                .iter()
                .filter_map(|id| self.waypoints.get(*id))
                .map(|w| (w.position, w.name.as_str()))
                .collect();
            let same = local.name == route.name
                && mine.len() == fetched.len()
                && mine.iter().zip(&fetched).all(|((a, an), (b, bn))| {
                    (a.lat - b.lat).abs() < 1e-7 && (a.lon - b.lon).abs() < 1e-7 && an == bn
                });
            if same {
                return Ok(None);
            }
            kept = Some(local.name.clone());
            let copy = format!("{} (Signal K)", route.name);
            // A later fetch refreshes the same copy rather than stacking
            // up new ones.
            route.id = self
                .routes()
                .find(|r| r.name == copy)
                .map(|r| r.id)
                .unwrap_or_else(Uuid::new_v4);
            route.name = copy;
            // Its marks come back under the local marks' ids; inserted as
            // they are they would move the kept route's waypoints.
            for wp in wps.iter_mut() {
                let fresh = Uuid::new_v4();
                for id in route.waypoints.iter_mut() {
                    if *id == wp.id {
                        *id = fresh;
                    }
                }
                wp.id = fresh;
            }
        }
        for wp in wps {
            self.waypoints.insert(wp);
        }
        self.upsert_route(route)?;
        Ok(kept)
    }

    /// Delete a route and its file. Its waypoints stay: they may be shared,
    /// and a mark on the water does not stop existing because a plan did.
    pub fn delete_route(&mut self, id: Uuid) -> Result<(), StoreError> {
        let i = self
            .routes
            .iter()
            .position(|s| s.route.id == id)
            .ok_or(StoreError::UnknownRoute(id))?;
        let stored = self.routes.remove(i);
        match std::fs::remove_file(&stored.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Add or replace a standalone waypoint and write the waypoints file.
    pub fn upsert_waypoint(&mut self, wp: Waypoint) -> Result<(), StoreError> {
        let id = wp.id;
        if !self.loose.contains(&id) {
            self.loose.push(id);
        }
        self.update_waypoint(wp)?;
        self.save_loose()
    }

    /// Change a mark that already exists, and rewrite every route carrying
    /// it.
    ///
    /// Each route's GPX inlines its marks by value, so a mark edited in one
    /// route stays stale in its siblings until they are written too — and
    /// the divergence only shows after a restart, when the files are read
    /// back. Unlike [`upsert_waypoint`](Self::upsert_waypoint) this does not
    /// make the mark standalone: a route's own waypoint has no business
    /// appearing in the loose waypoint file.
    pub fn update_waypoint(&mut self, wp: Waypoint) -> Result<(), StoreError> {
        let id = wp.id;
        self.waypoints.insert(wp);
        // A moved mark changes the measure of every leg through it.
        let affected: Vec<Uuid> = self
            .routes
            .iter()
            .filter(|s| s.route.waypoints.contains(&id))
            .map(|s| s.route.id)
            .collect();
        for rid in affected {
            let i = self.routes.iter().position(|s| s.route.id == rid).unwrap();
            self.routes[i].route.recompute_legs(&self.waypoints);
            self.save_route_at(i)?;
        }
        if self.loose.contains(&id) {
            self.save_loose()?;
        }
        Ok(())
    }

    /// Move a mark without writing anything: for a waypoint being dragged
    /// across the chart, which moves sixty times a second and must be
    /// written once, when it is let go ([`update_waypoint`](Self::update_waypoint)).
    /// The legs through it are measured again, so the chart and the route's
    /// length follow the finger.
    pub fn move_waypoint_unsaved(&mut self, id: Uuid, to: crate::geo::LatLon) {
        let Some(wp) = self.waypoints.get_mut(id) else { return };
        wp.position = to;
        for stored in self.routes.iter_mut().filter(|s| s.route.waypoints.contains(&id)) {
            stored.route.recompute_legs(&self.waypoints);
        }
    }

    /// Remove a standalone waypoint. Refused while a route still uses it —
    /// deleting a mark out from under a plan is a decision the caller must
    /// make explicitly, by editing the route first.
    pub fn delete_waypoint(&mut self, id: Uuid) -> Result<(), StoreError> {
        let using: Vec<String> = self
            .routes
            .iter()
            .filter(|s| s.route.waypoints.contains(&id))
            .map(|s| s.route.name.clone())
            .collect();
        if !using.is_empty() {
            return Err(StoreError::WaypointInUse { routes: using });
        }
        self.loose.retain(|w| *w != id);
        self.waypoints.remove(id);
        self.save_loose()
    }

    /// Copy the routes and waypoints of an external GPX file into the store.
    /// The source file is not touched.
    pub fn import(&mut self, path: &Path) -> Result<(usize, usize), StoreError> {
        let text = std::fs::read_to_string(path)?;
        let doc = gpx::parse(&text).map_err(|e| StoreError::Gpx(path.to_path_buf(), e))?;
        for wp in doc.waypoints.iter() {
            self.waypoints.insert(wp.clone());
        }
        for id in &doc.loose {
            if !self.loose.contains(id) {
                self.loose.push(*id);
            }
        }
        let n_routes = doc.routes.len();
        for route in doc.routes {
            // Same id replaces — importing the same file twice is idempotent,
            // which the guid adoption in the GPX layer exists to make true.
            // And it replaces *in its file*: a route that comes back under
            // another name would otherwise be written to a second file with
            // the first left behind, and both read at the next start.
            let at = match self.routes.iter().position(|s| s.route.id == route.id) {
                Some(i) => {
                    self.routes[i].route = route;
                    self.routes[i].root_attrs = doc.root_attrs.clone();
                    i
                }
                None => {
                    let route_path = self.route_path(&route);
                    self.routes.push(StoredRoute {
                        route,
                        path: route_path,
                        root_attrs: doc.root_attrs.clone(),
                    });
                    self.routes.len() - 1
                }
            };
            self.save_route_at(at)?;
        }
        if !doc.loose.is_empty() {
            self.save_loose()?;
        }
        Ok((n_routes, doc.loose.len()))
    }

    /// One route as GPX, exactly as the store keeps it.
    pub fn route_gpx(&self, id: Uuid) -> Result<String, StoreError> {
        let stored = self
            .routes
            .iter()
            .find(|s| s.route.id == id)
            .ok_or(StoreError::UnknownRoute(id))?;
        Ok(gpx::write_route(&stored.route, &self.waypoints, &stored.root_attrs))
    }

    /// Write one route to an external path, for handing to another program.
    pub fn export_route(&self, id: Uuid, to: &Path) -> Result<(), StoreError> {
        atomic_write(to, self.route_gpx(id)?.as_bytes())?;
        Ok(())
    }

    fn save_route_at(&mut self, i: usize) -> Result<(), StoreError> {
        let stored = &self.routes[i];
        let xml = gpx::write_route(&stored.route, &self.waypoints, &stored.root_attrs);
        atomic_write(&stored.path, xml.as_bytes())?;
        Ok(())
    }

    fn save_loose(&self) -> Result<(), StoreError> {
        let path = self.dir.join(WAYPOINTS_FILE);
        let wps: Vec<&Waypoint> = self.loose_waypoints().collect();
        if wps.is_empty() {
            match std::fs::remove_file(&path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
        let xml = gpx::write_waypoints(&wps, &[]);
        atomic_write(&path, xml.as_bytes())?;
        Ok(())
    }

    /// A fresh, collision-free path for a route's file.
    fn route_path(&self, route: &Route) -> PathBuf {
        let taken: BTreeSet<&Path> = self.routes.iter().map(|s| s.path.as_path()).collect();
        let short = &route.id.simple().to_string()[..8];
        let name = format!("route-{}-{short}.gpx", slug(&route.name));
        let candidate = self.dir.join(name);
        if !taken.contains(candidate.as_path()) {
            return candidate;
        }
        // Same name and same id-prefix twice is vanishing rare; fall back to
        // the full id rather than inventing counters.
        self.dir
            .join(format!("route-{}-{}.gpx", slug(&route.name), route.id.simple()))
    }
}

/// Write via a temporary file and rename, so a crash mid-write leaves the old
/// file whole rather than a truncated one. Routes are the user's passage
/// planning; half a file is worse than an old file.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("gpx.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// A filesystem-safe rendering of a route name.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "route".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::model::{LegKind, LegPlan, PointOfSail, TackState};

    fn count_gpx(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "gpx"))
            .count()
    }

    fn kattegat_route(store: &mut RouteStore) -> Route {
        let points = [
            ("Grenaa out", 56.4162345, 10.9367890),
            ("Fornæs", 56.4438272, 10.9645061),
            ("Mid Kattegat", 56.5500000, 11.3000000),
            ("Anholt W", 56.6987654, 11.5087654),
            ("Anholt N", 56.7500001, 11.5666667),
            ("Anholt harbour", 56.7160494, 11.5098765),
        ];
        let mut route = Route::new("Grenaa – Anholt");
        for (name, lat, lon) in points {
            route
                .waypoints
                .push(store.waypoints.insert(Waypoint::new(name, lat, lon)));
        }
        route
    }

    /// What Signal K hands back for a route: its id and name, and marks
    /// that carry a name and a position and nothing else.
    fn as_the_server_returns_it(store: &RouteStore, id: Uuid) -> (Route, Vec<Waypoint>) {
        let local = store.route(id).unwrap();
        let mut route = Route::new(local.name.clone());
        route.id = local.id;
        route.waypoints = local.waypoints.clone();
        let wps = local
            .waypoints
            .iter()
            .map(|w| {
                let w = store.waypoints.get(*w).unwrap();
                let mut bare = Waypoint::new(w.name.clone(), w.position.lat, w.position.lon);
                bare.id = w.id;
                bare
            })
            .collect();
        (route, wps)
    }

    /// Fetching back a route that was published and not changed since must
    /// leave it alone: the server never held the symbol, the arrival circle,
    /// the leg kind or the weather plan, so its copy cannot improve on ours.
    #[test]
    fn fetching_an_unchanged_route_keeps_what_the_server_never_had() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = RouteStore::open(dir.path()).unwrap();
        let route = kattegat_route(&mut store);
        let id = route.id;
        let first = route.waypoints[0];
        {
            let wp = store.waypoints.get_mut(first).unwrap();
            wp.symbol = Some("diamond".into());
            wp.arrival_radius_nm = Some(0.25);
        }
        store.upsert_route(route).unwrap();
        let mut planned = store.route(id).unwrap().clone();
        planned.legs[0].kind = LegKind::RhumbLine;
        planned.legs[0].plan = Some(LegPlan {
            eta: chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
            twa_deg: 60.0,
            tws_kt: 12.0,
            point_of_sail: PointOfSail::CloseReach,
            expected_stw_kt: 6.0,
            tack_state: TackState::Port,
        });
        store.upsert_route(planned.clone()).unwrap();
        let before = store.route(id).unwrap().clone();
        assert_eq!(before.legs[0].kind, LegKind::RhumbLine, "the fixture holds");

        let (route, wps) = as_the_server_returns_it(&store, id);
        assert_eq!(store.merge_fetched(route, wps).unwrap(), None);

        assert_eq!(store.route(id).unwrap(), &before);
        let wp = store.waypoints.get(first).unwrap();
        assert_eq!(wp.symbol.as_deref(), Some("diamond"));
        assert_eq!(wp.arrival_radius_nm, Some(0.25));
        assert_eq!(store.routes().count(), 1);
    }

    /// A route changed here since it was published is kept, and the
    /// server's version arrives beside it under its own marks.
    #[test]
    fn fetching_a_route_changed_here_keeps_ours_and_adds_theirs() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = RouteStore::open(dir.path()).unwrap();
        let route = kattegat_route(&mut store);
        let id = route.id;
        store.upsert_route(route).unwrap();
        let (route, wps) = as_the_server_returns_it(&store, id);

        // Our copy loses its last mark after the publish.
        let mut mine = store.route(id).unwrap().clone();
        mine.waypoints.pop();
        store.upsert_route(mine).unwrap();

        let kept = store.merge_fetched(route, wps).unwrap();
        assert_eq!(kept.as_deref(), Some("Grenaa – Anholt"));
        assert_eq!(store.route(id).unwrap().waypoints.len(), 5);
        let theirs = store.routes().find(|r| r.name == "Grenaa – Anholt (Signal K)").unwrap();
        assert_eq!(theirs.waypoints.len(), 6);
        assert!(theirs.waypoints.iter().all(|w| !store.route(id).unwrap().waypoints.contains(w)));
    }

    #[test]
    fn a_five_leg_route_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, problems) = RouteStore::open(dir.path()).unwrap();
        assert!(problems.is_empty());

        let route = kattegat_route(&mut store);
        let id = route.id;
        let originals: Vec<_> = route
            .waypoints
            .iter()
            .map(|w| store.waypoints.get(*w).unwrap().clone())
            .collect();
        store.upsert_route(route).unwrap();
        drop(store);

        // The restart.
        let (store, problems) = RouteStore::open(dir.path()).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        let back = store.route(id).expect("the route is still there");
        assert_eq!(back.name, "Grenaa – Anholt");
        assert_eq!(back.legs.len(), 5);
        for (i, (orig, id)) in originals.iter().zip(&back.waypoints).enumerate() {
            let wp = store.waypoints.get(*id).unwrap();
            // The DoD's tolerance is 1e-7°; the writer round-trips exactly.
            assert!(
                (wp.position.lat - orig.position.lat).abs() < 1e-7
                    && (wp.position.lon - orig.position.lon).abs() < 1e-7,
                "point {i} moved"
            );
            assert_eq!(wp.name, orig.name, "order or name lost at {i}");
        }
    }

    #[test]
    fn editing_a_shared_waypoint_updates_every_route_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = RouteStore::open(dir.path()).unwrap();
        let route = kattegat_route(&mut store);
        let id = route.id;
        let moved = route.waypoints[2];
        let before = {
            let mut r = route.clone();
            r.recompute_legs(&store.waypoints);
            r.total_distance_nm()
        };
        store.upsert_route(route).unwrap();

        let mut wp = store.waypoints.get(moved).unwrap().clone();
        wp.position = crate::geo::LatLon::new(56.60, 11.20);
        store.upsert_waypoint(wp).unwrap();

        let after = store.route(id).unwrap().total_distance_nm();
        assert!((after - before).abs() > 0.1, "legs did not re-measure");

        // And the change is on disk, not just in memory.
        drop(store);
        let (store, _) = RouteStore::open(dir.path()).unwrap();
        assert!((store.route(id).unwrap().total_distance_nm() - after).abs() < 1e-9);
    }

    #[test]
    fn deleting_a_route_removes_its_file_but_not_its_marks() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = RouteStore::open(dir.path()).unwrap();
        let route = kattegat_route(&mut store);
        let id = route.id;
        let a_mark = route.waypoints[0];
        store.upsert_route(route).unwrap();
        assert_eq!(count_gpx(dir.path()), 1);

        store.delete_route(id).unwrap();
        assert_eq!(count_gpx(dir.path()), 0);
        assert!(store.waypoints.get(a_mark).is_some(), "marks outlive plans");
    }

    /// Two routes sharing a mark: editing it must rewrite both files, or the
    /// sibling silently reverts to the old name at the next restart — each
    /// route's GPX carries its own copy of the mark.
    #[test]
    fn editing_a_shared_mark_rewrites_every_route_that_carries_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = RouteStore::open(dir.path()).unwrap();
        let a = kattegat_route(&mut store);
        let shared = a.waypoints[1];
        let mut b = Route::new("the other way");
        b.waypoints = a.waypoints.iter().rev().copied().collect();
        store.upsert_route(a).unwrap();
        store.upsert_route(b).unwrap();

        let mut mark = store.waypoints.get(shared).unwrap().clone();
        mark.name = "Renamed".to_string();
        store.update_waypoint(mark).unwrap();

        // Re-read from disk: both files must carry the new name.
        let (fresh, problems) = RouteStore::open(dir.path()).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(fresh.routes().count(), 2);
        for route in fresh.routes() {
            let names: Vec<&str> = route
                .waypoints
                .iter()
                .filter_map(|w| fresh.waypoints.get(*w))
                .map(|w| w.name.as_str())
                .collect();
            assert!(
                names.contains(&"Renamed"),
                "route '{}' kept a stale copy: {names:?}",
                route.name
            );
        }
        // And a route's own mark must not have become a standalone one.
        assert_eq!(fresh.loose_waypoints().count(), 0);
    }

    #[test]
    fn a_used_waypoint_refuses_deletion_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = RouteStore::open(dir.path()).unwrap();
        let route = kattegat_route(&mut store);
        let used = route.waypoints[1];
        store.upsert_route(route).unwrap();

        match store.delete_waypoint(used) {
            Err(StoreError::WaypointInUse { routes }) => {
                assert_eq!(routes, vec!["Grenaa – Anholt".to_string()])
            }
            other => panic!("expected WaypointInUse, got {other:?}"),
        }
        // Refused means untouched: the mark is still there for the route.
        assert!(store.waypoints.get(used).is_some());
    }

    #[test]
    fn a_foreign_multi_route_file_is_split_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        // Two routes in one file, as another program might drop in.
        let (mut scratch, _) = RouteStore::open(dir.path().join("scratch")).unwrap();
        let r1 = kattegat_route(&mut scratch);
        let mut r2 = Route::new("Second");
        r2.waypoints.push(scratch.waypoints.insert(Waypoint::new("a", 56.0, 11.0)));
        r2.waypoints.push(scratch.waypoints.insert(Waypoint::new("b", 56.1, 11.1)));
        let mut both = gpx::write_route(&r1, &scratch.waypoints, &[]);
        let second = gpx::write_route(&r2, &scratch.waypoints, &[]);
        // Concatenate the <rte> of the second into the first document.
        let insert_at = both.rfind("</gpx>").unwrap();
        let rte_start = second.find("<rte>").unwrap();
        let rte_end = second.find("</rte>").unwrap() + "</rte>".len();
        both.insert_str(insert_at, &second[rte_start..rte_end]);
        std::fs::write(dir.path().join("dropped-in.gpx"), &both).unwrap();

        let (store, problems) = RouteStore::open(dir.path()).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(store.routes().count(), 2);
        // The original was moved aside, not destroyed — byte for byte.
        let aside = dir.path().join("dropped-in.gpx.imported");
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), both);
        assert!(!dir.path().join("dropped-in.gpx").exists());
        assert_eq!(count_gpx(dir.path()), 2, "one file per route");

        // And a second open finds two clean single-route files, no re-split,
        // with both routes whole.
        drop(store);
        let (store, problems) = RouteStore::open(dir.path()).unwrap();
        assert!(problems.is_empty());
        assert_eq!(store.routes().count(), 2);
        for (id, name, legs) in [(r1.id, "Grenaa – Anholt", 5), (r2.id, "Second", 1)] {
            let r = store.route(id).expect("both routes keep their identity");
            assert_eq!((r.name.as_str(), r.legs.len()), (name, legs));
        }
    }

    #[test]
    fn importing_the_same_file_twice_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = RouteStore::open(dir.path().join("store")).unwrap();
        let route = kattegat_route(&mut store);
        store.upsert_route(route.clone()).unwrap();
        let exported = dir.path().join("exported.gpx");
        store.export_route(route.id, &exported).unwrap();

        let (mut fresh, _) = RouteStore::open(dir.path().join("fresh")).unwrap();
        assert_eq!(fresh.import(&exported).unwrap(), (1, 0));
        fresh.import(&exported).unwrap();
        assert_eq!(fresh.routes().count(), 1, "same guid, same route");
        assert!(fresh.route(route.id).is_some());
        // On disk too: a second file for the same route would come back as
        // a duplicate at the next start.
        assert_eq!(count_gpx(&dir.path().join("fresh")), 1);
        drop(fresh);
        let (fresh, _) = RouteStore::open(dir.path().join("fresh")).unwrap();
        assert_eq!(fresh.routes().count(), 1);
    }

    /// A mark moved in passing changes the route's length at once and
    /// nothing on the disk; writing it then makes it last.
    #[test]
    fn a_dragged_mark_is_measured_at_once_and_written_when_let_go() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = RouteStore::open(dir.path()).unwrap();
        let route = kattegat_route(&mut store);
        let id = route.id;
        let last = *route.waypoints.last().unwrap();
        store.upsert_route(route).unwrap();
        let before = store.route(id).unwrap().total_distance_nm();

        // One minute of latitude further north: the last leg changes.
        let at = store.waypoints.get(last).unwrap().position;
        let moved = crate::geo::LatLon::new(at.lat + 1.0 / 60.0, at.lon);
        store.move_waypoint_unsaved(last, moved);
        let after = store.route(id).unwrap().total_distance_nm();
        assert!((after - before).abs() > 0.5, "{before} -> {after}");
        let (reread, _) = RouteStore::open(dir.path()).unwrap();
        assert!((reread.route(id).unwrap().total_distance_nm() - before).abs() < 1e-9, "not written yet");

        let wp = store.waypoints.get(last).unwrap().clone();
        store.update_waypoint(wp).unwrap();
        let (reread, _) = RouteStore::open(dir.path()).unwrap();
        assert!((reread.route(id).unwrap().total_distance_nm() - after).abs() < 1e-6);
    }

    /// A route exported, renamed in another program and brought back is
    /// the same route under its new name — in one file, so that it is still
    /// one route, and still the renamed one, after a restart.
    #[test]
    fn a_route_reimported_under_a_new_name_stays_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("store");
        let (mut store, _) = RouteStore::open(&home).unwrap();
        let route = kattegat_route(&mut store);
        let id = route.id;
        store.upsert_route(route).unwrap();
        let exported = dir.path().join("exported.gpx");
        store.export_route(id, &exported).unwrap();
        let text = std::fs::read_to_string(&exported).unwrap();
        assert!(text.contains("Grenaa – Anholt"));
        std::fs::write(&exported, text.replace("Grenaa – Anholt", "To Anholt, by the north")).unwrap();

        store.import(&exported).unwrap();
        assert_eq!(store.route(id).unwrap().name, "To Anholt, by the north");
        assert_eq!(count_gpx(&home), 1);

        drop(store);
        let (mut store, problems) = RouteStore::open(&home).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(store.routes().count(), 1);
        assert_eq!(store.route(id).unwrap().name, "To Anholt, by the north");
        // And deleting it deletes it: nothing left to come back.
        store.delete_route(id).unwrap();
        drop(store);
        let (store, _) = RouteStore::open(&home).unwrap();
        assert_eq!(store.routes().count(), 0);
    }

    /// Two files holding the same route (copied by hand, say): one route is
    /// read, the other file is reported and neither is touched.
    #[test]
    fn a_second_file_with_the_same_route_is_reported_not_loaded_twice() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("store");
        let (mut store, _) = RouteStore::open(&home).unwrap();
        let route = kattegat_route(&mut store);
        let id = route.id;
        store.upsert_route(route).unwrap();
        let original = std::fs::read_dir(&home)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "gpx"))
            .unwrap();
        let copy = home.join("zz-copy.gpx");
        std::fs::copy(&original, &copy).unwrap();
        drop(store);

        let (store, problems) = RouteStore::open(&home).unwrap();
        assert_eq!(store.routes().count(), 1);
        assert!(store.route(id).is_some());
        assert!(
            matches!(problems.as_slice(), [StoreError::Duplicate { path, .. }] if *path == copy),
            "{problems:?}"
        );
        assert!(original.exists() && copy.exists());
    }

    #[test]
    fn an_unreadable_file_is_reported_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("broken.gpx");
        std::fs::write(&bad, "<gpx><rte><rtept></rte>").unwrap();
        let (store, problems) = RouteStore::open(dir.path()).unwrap();
        assert_eq!(problems.len(), 1);
        assert!(matches!(&problems[0], StoreError::Gpx(p, _) if *p == bad), "{problems:?}");
        assert_eq!(store.routes().count(), 0);
        // Still there, still broken, still the user's.
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "<gpx><rte><rtept></rte>");
    }

    #[test]
    fn slugs_are_tame() {
        assert_eq!(slug("Grenaa – Anholt"), "grenaa-anholt");
        assert_eq!(slug("Ærø / Øhavet!"), "ærø-øhavet");
        assert_eq!(slug(""), "route");
        assert_eq!(slug("   "), "route");
    }
}
