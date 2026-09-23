//! egui integration.
//!
//! Deliberately shallow. egui owns no window, no event loop and no part of the
//! chart pipeline: it draws in its own pass over the already-resolved surface,
//! after the chart is finished with it. That keeps the two independent — the
//! chart pass keeps its multisampling and its depth buffer, egui needs neither
//! — and it means a mistake in the UI cannot change a pixel of the chart.
//!
//! Everything the UI needs to say about the world goes through [`UiState`], and
//! everything it wants done comes back as [`UiAction`]. The chart renderer
//! never calls egui and egui never calls the chart renderer.

use egui_wgpu::ScreenDescriptor;

/// What the UI asks the application to do.
///
/// Returned rather than executed so the UI stays a pure function of state:
/// nothing in here borrows the renderer, so a panel cannot accidentally reach
/// into the camera or the tile cache.
#[derive(Debug, Clone, PartialEq)]
pub enum UiAction {
    /// Close the object-query bubble.
    DismissPick,
    /// The display settings changed: re-apply them and save.
    DisplayChanged,
    /// Look up what is installed, and ask NOAA for sizes and dates.
    FreeChartsRefresh,
    /// Download (or update) a NOAA package.
    FreeChartsDownload { code: String },
    /// Stop the download in progress.
    FreeChartsCancel,
    /// Delete an installed NOAA package.
    FreeChartsRemove { code: String },
    /// Abandon the passage plan being computed.
    PlanCancel,
    /// Keep the boat centred on the chart, or stop.
    FollowSet { on: bool },
    /// Sign in to the chart shop and list what the account owns.
    ShopSignIn { email: String, password: String },
    /// Re-read the entitlement list.
    ShopRefresh,
    /// Forget the session.
    ShopSignOut,
    /// Register this machine with the account under a name.
    ShopRegister { system_name: String },
    /// Claim a slot for a chart and ask the shop for a download.
    ///
    /// `edition` names the edition to ask for; `None` means the shop's
    /// current one, which is right for a live subscription and wrong for a
    /// lapsed one.
    ShopDownload {
        chart_id: String,
        edition: Option<String>,
    },
    /// Put a lapsed chart's download to the user before sending it.
    ShopConfirmDownload { chart_id: String },
    /// Drop the pending confirmation.
    ShopCancelDownload,
    /// Open the Signal K stream at this address.
    SignalKConnect { url: String },
    /// Close it and stop reconnecting.
    SignalKDisconnect,
    /// The instrument layout changed and should be written to disk.
    SettingsChanged,
    /// Open (and lazily load) the routes window.
    RoutesOpen,
    /// Follow this route.
    RouteActivate { route_id: uuid::Uuid },
    /// Stop following.
    RouteDeactivate,
    /// PUT this route to the Signal K server's resources.
    RoutePublish { route_id: uuid::Uuid },
    /// Read the server's route resources into the store.
    RoutesFetchSignalK,
    /// Start a new, empty route and open it for editing.
    RouteCreate,
    /// Delete a route and its file.
    RouteDelete { route_id: uuid::Uuid },
    /// Give a route a new name.
    RouteRename { route_id: uuid::Uuid, name: String },
    /// Open a route for editing — while one is open, a chart tap appends a
    /// waypoint to it. `None` closes the editor.
    RouteEdit { route_id: Option<uuid::Uuid> },
    /// Drop this waypoint from the route. The mark itself survives: a plan
    /// changing its mind does not unmake a place.
    ///
    /// Carrying both the position and the identity is deliberate. The UI
    /// builds its actions from a snapshot of the row, so by the time one is
    /// applied an earlier action in the same batch may already have
    /// renumbered the list; the identity survives that. The index is still
    /// the primary key because a route may legally visit the same mark
    /// twice, and then only the position says which one was meant.
    RouteWaypointDelete {
        route_id: uuid::Uuid,
        index: usize,
        waypoint: uuid::Uuid,
    },
    /// Put back the waypoint removed last.
    RouteWaypointUndo,
    /// Move a waypoint one place earlier (`-1`) or later (`+1`).
    RouteWaypointMove {
        route_id: uuid::Uuid,
        index: usize,
        waypoint: uuid::Uuid,
        delta: i32,
    },
    /// Give this waypoint a new name.
    RouteWaypointRename {
        route_id: uuid::Uuid,
        index: usize,
        waypoint: uuid::Uuid,
        name: String,
    },
    /// Sail the route the other way round.
    RouteReverse { route_id: uuid::Uuid },
    /// Plan a passage between two typed positions, straight from the menu
    /// bar. `from` empty means "the boat, wherever she is"; `sail` picks the
    /// weather router, otherwise the shortest safe (motor) route.
    PlanRoute {
        from: String,
        to: String,
        sail: bool,
    },
    /// Load this folder of charts, replacing whatever is showing.
    ChartFolderOpen { path: String },
    /// Show or hide the weather sheet along the bottom of the chart.
    WeatherSheetToggle,
    /// Take the whole forecast for what is on screen now: the point forecast
    /// behind the sheet, and whichever chart fields are switched on.
    WeatherRefreshHere,
    /// Show or hide the wind field (the barbs) over the chart.
    WindToggle,
    /// Show or hide the wind as a wash of colour over the chart.
    WindFillToggle,
    /// Show or hide the surface current over the chart.
    CurrentFieldToggle,
    /// Search ORC certificates for a class or boat name.
    BoatSearch { query: String, country: String },
    /// Install the polar (and specs) of a search hit by index.
    BoatUsePolar { index: usize },
    /// Weather-route between a route's endpoints, on the current forecast.
    WeatherRoute { route_id: uuid::Uuid },
}

/// The Charts window's three ways to get charts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChartsTab {
    /// Buy o-charts cells.
    #[default]
    Shop,
    /// Download free NOAA charts.
    Free,
    /// Point at a folder of charts already on disk.
    Folder,
}

/// One NOAA package, as the Free charts tab lists it.
#[derive(Debug, Clone)]
pub struct FreeRow {
    pub code: &'static str,
    pub name: &'static str,
    /// What NOAA offers now; `None` until asked (or if it did not answer).
    pub remote: Option<crate::shop::noaa::Remote>,
    pub installed: Option<crate::shop::noaa::Installed>,
}

/// The Free charts tab.
#[derive(Debug, Clone, Default)]
pub struct FreeChartsView {
    pub rows: Vec<FreeRow>,
    pub filter: String,
    /// NOAA has been asked for sizes and dates this session.
    pub probed: bool,
    /// The package downloading now: code, bytes so far, total.
    pub active: Option<(String, u64, u64)>,
    /// A package whose removal is waiting for a second tap.
    pub confirm_remove: Option<String>,
    pub status: String,
}

/// The chart folder in use, and the browser for choosing another.
///
/// Only the chosen folder is written to disk; where the user happened to be
/// browsing when they picked it is nobody's business next session. A plotter
/// that boots on a boat should come up on its own charts without a command
/// line, which is the whole reason this is remembered at all.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ChartFolderView {
    /// The folder the charts came from. Empty until one is chosen.
    pub chosen: String,
    /// Which tab of the Charts window is showing.
    #[serde(skip)]
    pub tab: ChartsTab,
    /// The directory on show.
    #[serde(skip)]
    pub at: String,
    /// Which directory [`entries`](Self::entries) describes. The listing is
    /// re-read when this falls behind `at` and never per frame: a folder of
    /// six hundred cells is a syscall per entry, and this window is drawn
    /// sixty times a second.
    #[serde(skip)]
    pub scanned: Option<String>,
    /// Sub-directories of `at`, by name, in reading order.
    #[serde(skip)]
    pub entries: Vec<String>,
    /// How many `.oesu` cells sit in `at` — the catalogue reads no other
    /// extension, so this is exactly what would load.
    #[serde(skip)]
    pub cells: usize,
    /// The count stopped early: this folder holds a lot besides charts.
    #[serde(skip)]
    pub cells_partial: bool,
    #[serde(skip)]
    pub status: String,
    /// A folder is being loaded in the background.
    #[serde(skip)]
    pub loading: bool,
}

impl ChartFolderView {
    /// Re-read the listing if it has fallen behind the directory on show.
    pub fn rescan(&mut self) {
        if self.scanned.as_deref() == Some(self.at.as_str()) {
            return;
        }
        self.entries.clear();
        self.cells = 0;
        match std::fs::read_dir(&self.at) {
            Ok(rd) => {
                for entry in rd.flatten() {
                    let path = entry.path();
                    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    // Dot-directories are noise on a chart plotter.
                    if name.starts_with('.') {
                        continue;
                    }
                    if path.is_dir() {
                        self.entries.push(name.to_string());
                    } else if path
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("oesu"))
                    {
                        self.cells += 1;
                    }
                }
                self.entries.sort_by_key(|n| n.to_lowercase());
                // Free S-57 charts (NOAA's ENC_ROOT and the like) sit in a
                // folder per cell, so count those below this one too — the
                // same search the catalogue makes.
                // Bounded: this runs on the UI thread, and a home folder
                // holds far more than charts. A few thousand entries answer
                // for any chart folder in milliseconds.
                let (found, more) =
                    crate::senc::find_s57_cells_within(std::path::Path::new(&self.at), 5, 3_000);
                self.cells += found.len();
                self.cells_partial = more;
                self.status.clear();
            }
            Err(e) => self.status = format!("cannot read this folder: {e}"),
        }
        self.scanned = Some(self.at.clone());
    }

    /// Move to `dir` and mark the listing stale.
    pub fn go(&mut self, dir: String) {
        self.at = dir;
    }
}

#[cfg(test)]
mod chart_folder_tests {
    use super::ChartFolderView;

    /// The listing is read once per folder, not once per frame: this window
    /// is drawn sixty times a second and a chart set is six hundred entries.
    #[test]
    fn a_folder_is_read_once_and_then_left_alone() {
        let dir = std::env::temp_dir().join("navcore-chart-folder-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Denmark")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join("OC-45-A.oesu"), b"x").unwrap();
        std::fs::write(dir.join("OC-45-B.OESU"), b"x").unwrap();
        std::fs::write(dir.join("ChartList.XML"), b"x").unwrap();

        let mut v = ChartFolderView {
            at: dir.display().to_string(),
            ..Default::default()
        };
        v.rescan();
        // Sub-folders are offered; dot-folders are noise on a plotter.
        assert_eq!(v.entries, vec!["Denmark".to_string()]);
        // Counted the way the catalogue counts — `.oesu`, either case, and
        // nothing else. The XML is not a chart.
        assert_eq!(v.cells, 2);
        assert!(v.status.is_empty());

        // A second pass must not touch the disk, so a file appearing behind
        // its back changes nothing until the folder is left and returned to.
        std::fs::write(dir.join("OC-45-C.oesu"), b"x").unwrap();
        v.rescan();
        assert_eq!(v.cells, 2, "the listing was re-read when it need not be");
        v.go(dir.display().to_string() + "/Denmark");
        v.rescan();
        assert_eq!(v.cells, 0);
        assert!(v.entries.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A folder that is not there must say so rather than look empty: an
    /// unplugged memory stick and a stick full of holiday photographs are
    /// different problems and the user has to be able to tell them apart.
    #[test]
    fn an_unreadable_folder_says_so() {
        let mut v = ChartFolderView {
            at: "/no/such/folder/anywhere".into(),
            ..Default::default()
        };
        v.rescan();
        assert!(!v.status.is_empty(), "no complaint about a missing folder");
        assert_eq!(v.cells, 0);
    }
}

/// Weather-routing preferences, persisted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WeatherView {
    /// Forecast horizon requested from the source.
    pub hours: u32,
    /// A polar file of the user's own; empty means the built-in cruiser.
    pub polar_path: String,
    /// Fetch GFS waves and let sea state slow (and, over the limit, block)
    /// the boat.
    pub use_waves: bool,
    /// Fetch surface currents (Open-Meteo/SMOC) and let the water carry the
    /// boat.
    pub use_currents: bool,
    // Live state below.
    #[serde(skip)]
    pub busy: bool,
    #[serde(skip)]
    pub status: String,
}

impl Default for WeatherView {
    fn default() -> Self {
        Self {
            hours: 48,
            polar_path: String::new(),
            use_waves: true,
            use_currents: true,
            busy: false,
            status: String::new(),
        }
    }
}

/// The boat: who she is and what she needs under her and above her.
/// Persisted — a boat does not change between sessions. The draft and air
/// draft feed the routers' safety envelope; the polar found here feeds
/// their speed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BoatView {
    pub name: String,
    pub boat_type: String,
    pub loa_m: f64,
    pub beam_m: f64,
    pub draft_m: f64,
    pub air_draft_m: f64,
    /// Engine cruising speed, knots. Zero: a sailing boat that does not
    /// motor, and the planner never offers it.
    pub motor_kt: f64,
    // Live state below.
    #[serde(skip)]
    pub open: bool,
    #[serde(skip)]
    pub search: String,
    #[serde(skip)]
    pub country: String,
    #[serde(skip)]
    pub results: Vec<crate::nav::orc::OrcHit>,
    #[serde(skip)]
    pub busy: bool,
    #[serde(skip)]
    pub status: String,
}

impl Default for BoatView {
    fn default() -> Self {
        Self {
            name: String::new(),
            boat_type: String::new(),
            loa_m: 0.0,
            beam_m: 0.0,
            draft_m: 0.0,
            air_draft_m: 0.0,
            motor_kt: 0.0,
            open: false,
            search: String::new(),
            country: "DEN".into(),
            results: Vec::new(),
            busy: false,
            status: String::new(),
        }
    }
}

/// The wind *field* — the barbs on the water — and the fetch behind it.
///
/// Runtime only: the field belongs to a forecast and an area, and a plotter
/// that silently redisplayed yesterday's wind at boot would be worse than one
/// that asks.
///
/// Which hour the barbs show is deliberately *not* here. That is the weather
/// sheet's cursor, so the barbs and the sheet cannot disagree about the time.
#[derive(Default)]
pub struct WindView {
    /// Drawing the field over the chart as barbs.
    pub show: bool,
    /// Drawing it as a wash of colour as well — the two read the same
    /// forecast and are independently useful, so they are separate switches.
    pub fill: bool,
    /// Valid times of the loaded steps; empty until one is loaded.
    pub steps: Vec<i64>,
    pub source: String,
    pub busy: bool,
    pub status: String,
}

/// Which planner field the next chart tap should fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanPickTarget {
    From,
    To,
}

/// The passage-planning fields in the menu bar. Runtime only: half-typed
/// coordinates are not worth persisting.
#[derive(Default)]
pub struct PlanView {
    /// Departure, "lat, lon". Empty means the boat's own position.
    pub from: String,
    /// Destination, "lat, lon".
    pub to: String,
    /// Armed pin: the next chart tap fills this field instead of running
    /// the object query. One tap, then it disarms itself.
    pub picking: Option<PlanPickTarget>,
}

/// A planner endpoint, projected for drawing: the pin that shows where the
/// typed (or tapped) position actually is.
pub struct PlanPin {
    /// Logical points, same space as the route overlay.
    pub screen: [f32; 2],
    pub is_start: bool,
}

/// The routes window and the state of following.
#[derive(Default)]
pub struct RoutesView {
    pub open: bool,
    pub rows: Vec<super::ui_routes::RouteRow>,
    /// The route being followed, if any.
    pub active: Option<uuid::Uuid>,
    /// Routes drawn on the chart.
    pub visible: std::collections::HashSet<uuid::Uuid>,
    /// This frame's guidance, for the strip.
    pub guidance: Option<crate::nav::Guidance>,
    /// Why a route being followed has no guidance right now. The strip says
    /// so rather than vanishing, which read as "following is off".
    pub guidance_waiting: Option<String>,
    pub status: String,
    /// The route open in the editor. While one is open its waypoints are
    /// listed and a chart tap appends to it.
    pub editing: Option<uuid::Uuid>,
    /// The waypoint removed last, so a mistaken Del can be taken back:
    /// (route, where it was, the mark, its name).
    pub undo: Option<(uuid::Uuid, usize, uuid::Uuid, String)>,
    /// A publish or fetch is on the wire. Their buttons wait for it rather
    /// than inviting a second click that would send the same PUT twice.
    pub net_busy: bool,
}

/// A quarter of a nautical mile, and twelve minutes: tight enough not to cry
/// wolf, loose enough to leave time to act. Defaults, not rules.
fn default_cpa_nm() -> f32 { 0.25 }
fn default_tcpa_min() -> f32 { 12.0 }
fn default_true() -> bool { true }

/// Where the instrument strip sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum BarPosition {
    Top,
    /// The default. A chart plotter is looked at from above and in front, and
    /// the helm's own hand covers the bottom of a bracket-mounted screen far
    /// less often than it covers the top.
    #[default]
    Bottom,
    Hidden,
}

/// Day, dusk or night: the S-52 colour tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Palette {
    #[default]
    Day,
    Dusk,
    Night,
}

impl Palette {
    /// The colour table's name in chartsymbols.xml.
    pub fn table(self) -> &'static str {
        match self {
            Palette::Day => "DAY_BRIGHT",
            Palette::Dusk => "DUSK",
            Palette::Night => "NIGHT",
        }
    }
}

/// How much of the chart to show, as ECDIS names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ChartDetail {
    /// Display base: coastline, dangers, the safety contour. Never less.
    Base,
    Standard,
    /// Everything the chart carries.
    #[default]
    All,
}

/// How the chart itself is drawn: the palette, and the mariner's settings
/// that decide which water reads as safe.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DisplayView {
    #[serde(skip)]
    pub open: bool,
    pub palette: Palette,
    pub detail: ChartDetail,
    /// Safety depth = the boat's draft + `clearance_m`, and the safety
    /// contour follows it. Off, the two manual values below are used.
    pub depth_from_draft: bool,
    pub clearance_m: f32,
    pub safety_depth_m: f32,
    pub safety_contour_m: f32,
    pub shallow_contour_m: f32,
    pub deep_contour_m: f32,
    pub show_text: bool,
    pub show_soundings: bool,
}

impl Default for DisplayView {
    fn default() -> Self {
        let m = crate::s52::MarinerSettings::default();
        Self {
            open: false,
            palette: Palette::Day,
            detail: ChartDetail::All,
            depth_from_draft: true,
            // Half a metre under the keel: a common minimum for coastal
            // pilotage. Raise it for a swell.
            clearance_m: 0.5,
            safety_depth_m: m.safety_depth,
            safety_contour_m: m.safety_contour,
            shallow_contour_m: m.shallow_contour,
            deep_contour_m: m.deep_contour,
            show_text: m.show_text,
            show_soundings: m.show_soundings,
        }
    }
}

impl DisplayView {
    /// Apply these choices to `base`. A setting pinned by a `NAVCORE_*`
    /// variable is left alone, so a capture or a conformance run stays
    /// reproducible whatever the user last chose. `draft_m` is the boat's;
    /// zero means unknown, and then the manual depths are used.
    pub fn apply(&self, base: &crate::s52::MarinerSettings, draft_m: f64) -> crate::s52::MarinerSettings {
        let pinned = |k: &str| std::env::var(k).is_ok();
        let mut s = base.clone();
        if !pinned("NAVCORE_DISPLAY_CAT") {
            (s.show_standard, s.show_other) = match self.detail {
                ChartDetail::Base => (false, false),
                ChartDetail::Standard => (true, false),
                ChartDetail::All => (true, true),
            };
        }
        let (depth, contour) = if self.depth_from_draft && draft_m > 0.0 {
            // The chart then selects the next contour it actually has at or
            // below this depth as the safety contour.
            let d = draft_m as f32 + self.clearance_m.max(0.0);
            (d, d)
        } else {
            (self.safety_depth_m, self.safety_contour_m)
        };
        if !pinned("NAVCORE_SAFETY_DEPTH") {
            s.safety_depth = depth;
        }
        if !pinned("NAVCORE_SAFETY_CONTOUR") {
            s.safety_contour = contour;
        }
        if !pinned("NAVCORE_SHALLOW_CONTOUR") {
            s.shallow_contour = self.shallow_contour_m;
        }
        if !pinned("NAVCORE_DEEP_CONTOUR") {
            s.deep_contour = self.deep_contour_m;
        }
        // S-52's order: shallow <= safety <= deep, and a safety contour no
        // shallower than the safety depth — or the shading calls water safe
        // that the soundings call dangerous.
        s.safety_contour = s.safety_contour.max(s.safety_depth);
        s.shallow_contour = s.shallow_contour.min(s.safety_contour);
        s.deep_contour = s.deep_contour.max(s.safety_contour);
        s.show_text = self.show_text;
        s.show_soundings = self.show_soundings;
        s
    }
}

/// The instrument strip and the connection behind it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstrumentView {
    /// What the user typed, not the normalised stream URL — so the settings
    /// window shows it back exactly as they wrote it.
    pub url: String,
    pub position: BarPosition,
    /// The paths on the bar, in the order shown.
    pub tiles: Vec<String>,
    pub units: crate::signalk::UnitPrefs,
    /// Follow the boat: recentre the chart as the position moves.
    pub follow: bool,
    /// When a target counts as dangerous. Every ECDIS makes these settings,
    /// because the right answer depends on the water: a quarter-mile CPA is
    /// prudent offshore and unusable in a busy strait, where every ferry would
    /// trip it and the warning would stop meaning anything.
    #[serde(default = "default_cpa_nm")]
    pub cpa_alarm_nm: f32,
    #[serde(default = "default_tcpa_min")]
    pub tcpa_alarm_min: f32,
    /// Show AIS traffic at all.
    #[serde(default = "default_true")]
    pub show_ais: bool,

    // Everything below is live state, not preference.
    #[serde(skip)]
    pub open: bool,
    #[serde(skip)]
    pub status: String,
    #[serde(skip)]
    pub connected: bool,
    /// A connection is wanted: connecting, connected, or waiting to retry.
    /// What decides whether the button offers Connect or Stop — a server that
    /// keeps refusing must still be stoppable.
    #[serde(skip)]
    pub active: bool,
    /// Paths the server has actually sent, for the picker.
    #[serde(skip)]
    pub available: Vec<String>,
}

impl Default for InstrumentView {
    fn default() -> Self {
        Self {
            url: String::new(),
            position: BarPosition::default(),
            tiles: crate::signalk::catalog::DEFAULT_BAR
                .iter()
                .map(|s| s.to_string())
                .collect(),
            units: Default::default(),
            follow: true,
            cpa_alarm_nm: default_cpa_nm(),
            tcpa_alarm_min: default_tcpa_min(),
            show_ais: true,
            open: false,
            status: "Not connected".into(),
            connected: false,
            active: false,
            available: Vec::new(),
        }
    }
}

/// A download awaiting the user's word.
///
/// A lapsed subscription cannot have the shop's current edition, so navcore
/// asks for an older one — a decision worth showing rather than making
/// silently, because it is the user's licence and the user's money.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingDownload {
    pub chart_id: String,
    pub chart_name: String,
    /// The edition to ask for, when one could be worked out.
    pub edition: Option<String>,
    /// Where that edition came from, in words.
    pub because: String,
    /// The download claims a new licence slot for this machine (a live
    /// subscription this machine does not hold yet), rather than asking for
    /// an older edition of a lapsed one.
    pub new_slot: bool,
}

/// What the shop panel is showing.
///
/// Lives here because egui needs somewhere to keep the text a user is typing;
/// the renderer fills in the rest as the worker answers.
#[derive(Default)]
pub struct ShopView {
    pub open: bool,
    pub email: String,
    /// Kept only for as long as it takes to send. Never written to disk.
    pub password: String,
    pub status: String,
    pub busy: bool,
    pub signed_in: bool,
    pub system_name: Option<String>,
    pub systems: Vec<String>,
    pub charts: Vec<crate::shop::types::Chart>,
    /// Editions already installed, by chart id.
    pub installed: std::collections::HashMap<String, crate::shop::types::Edition>,
    /// A name being typed for this machine.
    pub new_system_name: String,
    /// What the shop said about the last download request, per chart id.
    pub grants: std::collections::HashMap<String, String>,
    /// A download the user has not yet confirmed.
    pub pending: Option<PendingDownload>,
    /// A standing problem with this machine — no chart licence found — kept
    /// apart from `status`, which every worker event overwrites. Set as a
    /// status, it was gone before anyone could read it.
    pub warning: String,
}

/// What the UI is allowed to see.
pub struct UiState<'a> {
    /// Objects under the last tap, if the bubble is open.
    pub picked: Option<&'a [crate::pick::PickedObject]>,
    /// Where the picked position currently is on screen, in *logical* points.
    pub pick_anchor: Option<[f32; 2]>,
    /// Which query the bubble answers. Each one gets its own window, so a
    /// new tap opens beside itself rather than where the first one did.
    pub pick_id: u64,
    /// The boat, already projected — the renderer owns the camera, not the UI.
    pub own_ship: Option<super::ui_ownship::OwnShip>,
    /// The AIS traffic, likewise, with its closest approaches already worked
    /// out on the ellipsoid.
    pub ais: Vec<super::ui_ais::AisTarget>,
    /// Metres per logical point, so a course vector is a real distance.
    pub mpp: f32,
    /// Visible routes, projected for the overlay.
    pub routes: Vec<super::ui_routes::RouteDisplay>,
    /// The planner's endpoints, projected, for the pins.
    pub plan_pins: Vec<PlanPin>,
    /// The wind field, projected onto the screen grid.
    pub wind: Vec<super::ui_wind::WindBarb>,
    /// The wind as a colour wash, under everything.
    pub wind_fill: super::ui_wind::WindFill,
    /// The surface current, likewise.
    pub current: Vec<super::ui_weather::CurrentArrow>,
    /// Where the sheet's point forecast was taken, projected. `None` when
    /// there is none, or when it has panned off the screen.
    pub weather_anchor: Option<[f32; 2]>,
}

pub struct Ui {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    /// Collected during a frame, drained by the caller.
    actions: Vec<UiAction>,
    /// egui asked to be drawn again, and how soon.
    repaint_after: Option<std::time::Duration>,
    /// The chart shop panel's own state.
    pub shop: ShopView,
    /// Which folder the charts came from, and the browser for changing it.
    pub charts: ChartFolderView,
    /// The instrument strip and its Signal K connection.
    pub instruments: InstrumentView,
    /// The routes window and following state.
    pub routes: RoutesView,
    /// Weather-routing preferences.
    pub weather: WeatherView,
    /// The menu bar's passage planner.
    pub plan: PlanView,
    /// The boat's specs and polar.
    pub boat: BoatView,
    /// The wind field over the chart.
    pub wind: WindView,
    /// The weather sheet: the time axis, the lanes, and the cursor that every
    /// other weather display on screen reads.
    pub sheet: super::ui_weather::SheetView,
    pub display: DisplayView,
    pub free: FreeChartsView,
}

impl Ui {
    pub fn new(
        device: &wgpu::Device,
        window: &winit::window::Window,
        surface_format: wgpu::TextureFormat,
    ) -> Self {
        let ctx = egui::Context::default();
        style(&ctx, false);
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        // No depth and no multisampling: this pass renders over the resolved
        // image, and egui antialiases its own geometry.
        let renderer = egui_wgpu::Renderer::new(device, surface_format, None, 1, false);
        Self {
            ctx,
            state,
            renderer,
            actions: Vec::new(),
            repaint_after: None,
            // Whatever was arranged last time, or the four readings a plotter
            // is expected to answer without being asked.
            instruments: crate::render::state::RenderState::load_settings().unwrap_or_default(),
            shop: ShopView::default(),
            charts: ChartFolderView::default(),
            routes: RoutesView::default(),
            weather: crate::render::state::RenderState::load_weather_settings(),
            plan: PlanView::default(),
            boat: crate::render::state::RenderState::load_boat_settings(),
            wind: WindView::default(),
            sheet: Default::default(),
            display: crate::render::state::RenderState::load_display_settings(),
            free: FreeChartsView::default(),
        }
    }

    /// Does egui still have work to finish?
    ///
    /// It animates — a window fades in, a hover highlight grows — and it
    /// reports how soon it wants the next frame. navcore only redraws on
    /// demand, so ignoring this froze every animation part-way: a panel that
    /// had faded to a third of its opacity simply stayed there, looking like a
    /// rendering fault rather than an unfinished fade.
    pub fn wants_repaint(&self) -> bool {
        matches!(self.repaint_after, Some(d) if d < std::time::Duration::from_millis(100))
    }

    /// Offer an event to the UI.
    ///
    /// Both halves of the answer matter. `consumed` is not enough on its own: a
    /// *press* is essentially never consumed, because egui decides consumption
    /// from `wants_pointer_input`, which is false the moment a button goes
    /// down and only becomes true once a frame has run with that press. Use
    /// [`pointer_over_ui`](Self::pointer_over_ui) to gate a press, and
    /// `repaint` to keep the state that answer depends on current.
    pub fn on_window_event(
        &mut self,
        window: &winit::window::Window,
        event: &winit::event::WindowEvent,
    ) -> egui_winit::EventResponse {
        self.state.on_window_event(window, event)
    }

    /// Follow the chart's palette: a light interface over the Day chart, a dark
    /// one over Dusk and Night. A bright panel at night ruins night vision,
    /// which is the whole point of the dark palettes.
    pub fn set_dark(&mut self, dark: bool) {
        style(&self.ctx, dark);
    }

    /// Is the pointer over any part of the interface — a panel, a window, a
    /// widget?
    ///
    /// This, not `wants_pointer_input`, is the gate for "should the chart get
    /// this click". `wants_pointer_input` is false whenever a button is down,
    /// so that a drag begun on the chart keeps working as it passes over a
    /// panel — useful, and exactly wrong for deciding whether a *press* was
    /// meant for the interface. It is why clicking the menu bar also dropped a
    /// pin on the chart behind it: the bar's background is not a widget, egui
    /// did not claim the click, and it fell through.
    pub fn pointer_over_ui(&self) -> bool {
        self.ctx.is_pointer_over_area()
    }

    /// Whether a physical-pixel position is under the interface: a window
    /// or area, or one of the panels round the edge. The same test egui's
    /// `is_pointer_over_area` makes, for a position rather than the pointer.
    pub fn covers(&self, x: f32, y: f32) -> bool {
        let ppp = self.ctx.pixels_per_point().max(0.01);
        let pos = egui::pos2(x / ppp, y / ppp);
        match self.ctx.layer_id_at(pos) {
            Some(layer) if layer.order == egui::Order::Background => {
                !self.ctx.available_rect().contains(pos)
            }
            Some(_) => true,
            None => false,
        }
    }

    /// The chart still showing between the panels, in logical points.
    ///
    /// The menu bar, the instrument strip and the weather sheet are all panels
    /// laid over the chart, and anything drawn under one of them is drawn for
    /// nobody. With the sheet open that is better than a third of the window,
    /// and the wind field is expensive enough per barb to be worth not
    /// drawing. It is last frame's rectangle — egui only knows what the panels
    /// took once they have run — which matters for exactly one frame after the
    /// sheet opens or the window resizes.
    pub fn chart_area(&self) -> egui::Rect {
        self.ctx.available_rect()
    }

    /// Build a frame, draw it over `view`, and return what the user asked for.
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &mut self,
        window: &winit::window::Window,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        size: [u32; 2],
        state: UiState<'_>,
        fleet: &crate::signalk::Fleet,
    ) -> Vec<UiAction> {
        self.actions.clear();
        let input = self.state.take_egui_input(window);
        let actions = &mut self.actions;
        let shop = &mut self.shop;
        let charts = &mut self.charts;
        let instruments = &mut self.instruments;
        let routes = &mut self.routes;
        let weather = &mut self.weather;
        let plan = &mut self.plan;
        let boat = &mut self.boat;
        let wind = &mut self.wind;
        let sheet = &mut self.sheet;
        let display = &mut self.display;
        let free = &mut self.free;
        let output = self.ctx.run(input, |ctx| {
            super::ui_panels::build(
                ctx, &state, shop, charts, instruments, routes, weather, plan, boat, wind, sheet,
                display, free, fleet, actions,
            );
        });
        self.state
            .handle_platform_output(window, output.platform_output);
        self.repaint_after = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay);

        let jobs = self
            .ctx
            .tessellate(output.shapes, output.pixels_per_point);
        for (id, delta) in &output.textures_delta.set {
            self.renderer.update_texture(device, queue, *id, delta);
        }
        let desc = ScreenDescriptor {
            size_in_pixels: size,
            pixels_per_point: output.pixels_per_point,
        };
        self.renderer
            .update_buffers(device, queue, encoder, &jobs, &desc);

        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Load, not Clear: the chart is already there.
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.renderer
                .render(&mut pass.forget_lifetime(), &jobs, &desc);
        }

        for id in &output.textures_delta.free {
            self.renderer.free_texture(id);
        }
        std::mem::take(&mut self.actions)
    }
}

/// navcore's look: bigger than egui's default, because this is read at arm's
/// length on a boat, often through spray and often through reading glasses.
fn style(ctx: &egui::Context, dark: bool) {
    use egui::{FontFamily, FontId, TextStyle};
    ctx.set_visuals(if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    });
    let mut style = (*ctx.style()).clone();
    // Points, so these track the display's scale factor.
    style.text_styles = [
        (TextStyle::Heading, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
    ]
    .into();
    // Touch first: a finger is about 9 mm, so controls get room. These are the
    // numbers to revisit if the UI ever feels cramped on the plotter.
    style.spacing.button_padding = egui::vec2(10.0, 8.0);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.interact_size.y = 32.0;
    style.visuals.window_rounding = egui::Rounding::same(6.0);
    ctx.set_style(style);
}

#[cfg(test)]
mod display_tests {
    use super::*;

    /// Safety depth follows the draft, and the S-52 ordering holds whatever
    /// was typed: shallow <= safety contour <= deep, contour >= depth.
    #[test]
    fn display_choices_become_consistent_mariner_settings() {
        let base = crate::s52::MarinerSettings::default();
        let d = DisplayView {
            clearance_m: 0.5,
            shallow_contour_m: 9.0,
            deep_contour_m: 1.0,
            detail: ChartDetail::Standard,
            show_text: false,
            ..DisplayView::default()
        };
        let s = d.apply(&base, 2.1);
        assert!((s.safety_depth - 2.6).abs() < 1e-6);
        assert!(s.safety_contour >= s.safety_depth);
        assert!(s.shallow_contour <= s.safety_contour);
        assert!(s.deep_contour >= s.safety_contour);
        assert!(s.show_standard && !s.show_other && s.show_displaybase);
        assert!(!s.show_text);

        // Unknown draft: the manual values are used instead.
        let manual = DisplayView { safety_depth_m: 4.0, safety_contour_m: 5.0, ..d };
        let s = manual.apply(&base, 0.0);
        assert_eq!((s.safety_depth, s.safety_contour), (4.0, 5.0));
    }
}
