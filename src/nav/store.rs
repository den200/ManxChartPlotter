//! Where routes live: a directory of GPX files.
//!
//! The files **are** the store — there is no database that the GPX merely
//! mirrors. That was a decision, not a default: the files are readable by any
//! other plotter as they sit, they survive navcore, and backup is `cp -r`.
//! One route per file, plus `waypoints.gpx` for the standalone marks. The
//! directory sits beside `settings.json`, so everything a user would want to
//! keep lives under one `navcore/` folder.
//!
//! Files navcore did not write are respected: on open, a foreign multi-route
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
        Some(dirs::config_dir()?.join("navcore").join("routes"))
    }

    /// Open (creating if absent) the store at `dir` and load everything in it.
    ///
    /// A file that fails to parse is reported and skipped, never deleted or
    /// rewritten: an unreadable file is the user's data in trouble, and the
    /// worst possible response is to make it navcore's data instead.
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
        let route_count = doc.routes.len();
        for route in doc.routes {
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
                    "route store: split {} into navcore files; original kept at {}",
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
            self.routes.retain(|s| s.route.id != route.id);
            let route_path = self.route_path(&route);
            self.routes.push(StoredRoute {
                route,
                path: route_path,
                root_attrs: doc.root_attrs.clone(),
            });
            self.save_route_at(self.routes.len() - 1)?;
        }
        if !doc.loose.is_empty() {
            self.save_loose()?;
        }
        Ok((n_routes, doc.loose.len()))
    }

    /// Write one route to an external path, for handing to another program.
    pub fn export_route(&self, id: Uuid, to: &Path) -> Result<(), StoreError> {
        let stored = self
            .routes
            .iter()
            .find(|s| s.route.id == id)
            .ok_or(StoreError::UnknownRoute(id))?;
        let xml = gpx::write_route(&stored.route, &self.waypoints, &stored.root_attrs);
        atomic_write(to, xml.as_bytes())?;
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
        // The original was moved aside, not destroyed.
        assert!(dir.path().join("dropped-in.gpx.imported").exists());
        assert!(!dir.path().join("dropped-in.gpx").exists());

        // And a second open finds two clean single-route files, no re-split.
        drop(store);
        let (store, problems) = RouteStore::open(dir.path()).unwrap();
        assert!(problems.is_empty());
        assert_eq!(store.routes().count(), 2);
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
        fresh.import(&exported).unwrap();
        fresh.import(&exported).unwrap();
        assert_eq!(fresh.routes().count(), 1, "same guid, same route");
    }

    #[test]
    fn an_unreadable_file_is_reported_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("broken.gpx");
        std::fs::write(&bad, "<gpx><rte><rtept></rte>").unwrap();
        let (store, problems) = RouteStore::open(dir.path()).unwrap();
        assert_eq!(problems.len(), 1);
        assert_eq!(store.routes().count(), 0);
        // Still there, still broken, still the user's.
        assert!(bad.exists());
    }

    #[test]
    fn slugs_are_tame() {
        assert_eq!(slug("Grenaa – Anholt"), "grenaa-anholt");
        assert_eq!(slug("Ærø / Øhavet!"), "ærø-øhavet");
        assert_eq!(slug(""), "route");
        assert_eq!(slug("   "), "route");
    }
}
