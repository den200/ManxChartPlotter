# NavCore Automatic Sailing Route Optimization Engine — Implementation Specification

*A hand-off specification for a coding agent (Claude Code) implementing a weather-routing engine in Rust for the NavCore offline nautical plotter (Raspberry Pi 4/5, Mac ARM, wgpu). Prefer the explicit rules, predicates, formulas, pseudocode and named defaults below over prose.*

---

## TL;DR
- **Build a time-dependent recursive isochrone engine** (the algorithm proven in OpenCPN's `weather_routing_pi`, qtVlm, Expedition and LuckGrib), not a graph/Dijkstra grid, because it directly handles a state-dependent, time-varying boat speed from the polar and runs comfortably on a Pi 4. Grow the reachable-region polygon one time step at a time, propagating a fan of headings from every frontier node, and prune everything that falls inside the already-reached region.
- **The polar is the whole physics model:** boat speed = bilinear interpolation over a TWA×TWS grid; the no-go zone and the tack/gybe decision emerge naturally from it via VMG, so nothing about tacking needs hardcoding. Land, depth and hazard constraints come from rasterizing SENC features to a navigability mask plus a signed-distance "safe-offing" field; wind/current come from GRIB (decode with the pure-Rust `grib` crate) interpolated in U/V space.
- **Follow a strictly staged roadmap:** manual waypoints+GPX → route following with XTE/Signal K → great-circle with land avoidance → constant-wind polar routing → full GRIB isochrone routing → current/tide-aware routing. Each stage is independently shippable and verifiable against a synthetic constant-wind analytic optimum and against qtVlm/OpenCPN on the same GRIB+polar.

---

## Key Findings

1. **Isochrone method is the right primary algorithm.** It is O(steps × frontier_nodes × headings), memory-light, and is what essentially every production sailing router uses. Its two classic failure modes — "isochrone loops"/degeneracy from the non-convex sailboat polar, and poor convergence at the destination — are both solved by well-documented techniques (inside-region pruning, sector/angular pruning, and a reverse/closest-position final approach).
2. **Graph methods (Dijkstra/A*) are correct only under the FIFO property.** With time-dependent wind, edge cost depends on arrival time; the earliest-arrival problem is polynomial and Dijkstra/A* remain correct **iff** the network is FIFO (leaving later never arrives earlier). Sailing edge costs are effectively FIFO, so a time-dependent A* on a hex grid is a valid alternative — but it needs a large precomputed grid and more RAM than isochrones for equivalent quality.
3. **Fast Marching / Zermelo (level-set) is the theoretically elegant optimum** (continuous, globally optimal, handles currents natively) but is heavier to implement correctly and to keep stable under strong currents; recommended only as a much later sophistication path.
4. **The chart→constraint pipeline is the largest engineering surface** and must be built as its own layer: hard-obstacle predicate + cost raster + signed-distance offing field + TSS/COLREG-Rule-10 cost. This is more code than the router itself.
5. **All the numeric defaults you need already exist** in the tools and standards: isochrone time step 1 h (coastal) with 3-hourly GRIB steps, heading step 5°, tack/gybe penalty ~4 min day / ~10 min night for a cruiser, the ECDIS safety-contour default and the safety-depth formula, GFS 0.25° global GRIB, etc.

---

## Details

### PART 1 — WEATHER ROUTING ALGORITHMS: COMPARISON

#### 1.1 Isochrone method (RECOMMENDED PRIMARY)

**Concept.** An *isochrone* is the boundary of the set of all points reachable from the start in exactly *t* hours (a "time contour"). Starting from the departure point, you repeatedly expand the current isochrone forward by one time step Δt; the outer envelope of all these expansions is the next isochrone. The optimal route to any point is reconstructed by following parent links back from the isochrone that first reaches it.

**Provenance (exact citations).** The technique originates with **R. W. James, *Application of Wave Forecast to Marine Navigation* (US Navy Hydrographic Office, Washington, 1957)** for manual chartwork, was computerized and formalized in **H. Hagiwara, *Weather Routing of (Sail-assisted) Motor Vessels* (Delft University of Technology, Delft, 1989)**, and further refined by Spaans (1986), Hagiwara & Spaans (1987), and Wiśniewski (1991). Later variants change the equal-time contour to equal-fuel/energy ("Isopone", Klompstra et al. 1992) or equal-cost ("Isocost").

**Step-by-step (single time step):**
1. Start with isochrone *I(k)* = a polygon (closed ring) of `Position` nodes, each with a back-pointer to its parent on *I(k−1)*.
2. For every node *p* on *I(k)*:
   - Look up true wind (direction W, speed VW) at *p*'s position and the current sim time *t_k* (from GRIB, interpolated in space + time).
   - For each candidate heading in a fan (e.g. every 5° over an allowed arc, plus the two optimal VMG headings): compute TWA, look up boat speed VB from the polar, integrate a great-circle step of length VB·Δt (add current drift × Δt if enabled) to get a candidate child position.
   - Reject children that: cross land/obstacle (segment test), violate a hard constraint (max TWS, max wave, too shallow), or exceed max-diverted-course.
3. Collect all surviving children into a new raw polygon *I(k+1)*.
4. **Prune (the critical step):** delete any child that lies **inside** the union of all previous isochrones (i.e. inside *I(k)*'s reachable region). Such a point was already reachable sooner by another path. Only the outer frontier survives. This is what prevents the "isochrone loop" degeneracy caused by the concave (heart-shaped) sailboat polar. As Szłapczyńska & Śmierzchalski describe it verbatim: *"Such a loop is in fact an irregularity in shape of an isochrone caused by non-convexity of speed characteristic for given weather data. Unfortunately isochrone loops propagate with number of isochrones and as a result make the procedure not applicable for computer programs."*
5. **Normalize:** merge overlapping sub-polygons into unions, remove self-intersections, and promote inner loops to *holes/children* (inverted regions) — these represent islands of unreachability, e.g. an island the route must go around on both sides.
6. Repeat until an isochrone encloses the destination (or reaches within one Δt of it), then do a **direct final connection** to the destination and reconstruct the path via parent pointers.

**OpenCPN `weather_routing_pi` internal structure (GPLv3, by Sean D'Epagnier) — study, do not copy:**
- `RouteMap` holds an ordered `list<IsoChron*>`; each `IsoChron` = `list<IsoRoute*>` + timestamp + Δt; each `IsoRoute` = one closed polygon (circular doubly-linked list of `Position`) + `vector<IsoRoute*> children` for holes/inverted regions.
- Propagation call chain (verbatim signatures from the crash backtrace in issue #17): `IsoChron::PropagateIntoList(std::list<IsoRoute*>&, RouteMapConfiguration&)` → `IsoRoute::Propagate(...)` → `Position::Propagate(std::list<IsoRoute*>&, RouteMapConfiguration&)` → `Position::CrossesLand(double, double)`.
- `Position::Propagate` iterates candidate courses (config `DegreeSteps`, default sweep **0–180° by 5°**; docs: *"Degree Steps (5 is faster than 1)... From 0 to 180 by 5 degrees is fine"*; 0–360° when "Optimize Tacking" is on), computes boat speed via `Polar::Speed`, integrates position over Δt.
- `Polar::Speed(W, VW, bound, optimize_tacking)` (verbatim from `Polar.cpp`): normalizes W to [0,180] (`if(W>180) W=360-W;` — assumes port/starboard symmetry), returns `NAN` outside the polar's angular range (this *is* the no-go zone) unless `optimize_tacking`, in which case it substitutes the VMG angle and returns `Speed(vmgW,VW)*cos(deg2rad(vmgW))/cos(deg2rad(W))` (the tacking projection). Boat speed uses **bilinear interpolation** over the TWA×TWS grid.
- `ComputeBoatSpeed` fallback when wind exceeds the polar's top wind band (verbatim from issue #14): `while (c > 0) { VB = plan.Speed(H, c); if (!isnan(VB)) break; c -= 0.25; }` — step VW down by **0.25 kt** until a defined polar value is found.
- Land detection: `Position::CrossesLand` → OpenCPN `PlugIn_GSHHS_CrossesLand` → `gshhsCrossesLand(lat1,lon1,lat2,lon2)`, a per-segment great-circle intersection test against high-resolution GSHHS coastline polygons. Docs note it is "quite slow."
- A `ClosestPosition` scan gives the nearest frontier node to a target (used for cursor route and final approach). "Inverted Regions" mode (routing around islands) is noted in the plugin's own docs as buggy — flag this as a known hard case.
- Complexity: docs state halving the time step ≈ 4× the compute time.
- GPLv3 license header (verbatim): *"This program is free software; you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation; either version 3 of the License, or (at your option) any later version."* Copyright (C) 2016 Sean D'Epagnier.

**Handling land/obstacles inside expansion.** Two documented approaches:
- *Segment test* (OpenCPN): reject any propagation step whose great-circle segment crosses land. Simple, correct, but slow — accelerate with a spatial index (see Part 4).
- *Area partitioning* (Szłapczyńska & Śmierzchalski 2007, building on Hagiwara): partition the search area so resulting routes are guaranteed free of land crossings; handles narrow straits and is less non-convexity-error prone. More complex.

#### 1.2 Graph / grid shortest-path methods (Dijkstra, A*, time-dependent A*, Contraction Hierarchies)
- **Regular lat/lon grid, hex grid (Uber H3, pure-Rust `h3o`), nav-mesh / visibility graph.** Build a graph, run Dijkstra/A*.
- **The time-dependent edge-cost problem:** with a time-varying wind field, the cost of traversing an edge depends on the time you *start* traversing it (the wind you'll experience). This is the Time-Dependent Shortest Path Problem (TDSPP).
- **FIFO property and correctness:** Dreyfus (1969) extended Dijkstra to TDSPP; Halpern proved the generalization is correct only for **FIFO** networks: an edge (v,w) is FIFO iff `t1 ≤ t2 ⟹ t1 + w(t1) ≤ t2 + w(t2)` (departing later never lets you arrive earlier). Sailing traversal times are effectively FIFO, so Dijkstra/A* stay valid. **Without** FIFO (and without allowed waiting) TDSPP is NP-hard; Orda & Rom (1990) showed that with waiting allowed at nodes any instance converts to an equivalent FIFO instance (they gave a Bellman-Ford-based approach). For sailing, "waiting" = heaving-to / anchoring to catch a tidal gate — model it explicitly rather than relying on non-FIFO search.
- **Contraction Hierarchies / ALT landmarks:** large speedups for repeated queries on static road networks; overkill and awkward for a small, one-shot, time-dependent marine query. Not recommended.
- **Suitability for Pi 4:** a hex grid dense enough for coastal sailing needs a large node set and per-node arrival-time functions → higher RAM than isochrones. A* with an admissible heuristic (great-circle time at best boat speed) is feasible but offers no quality advantage over isochrones here.

#### 1.3 Dynamic programming / Bellman; 3D DP with time as a dimension
De Wit (1990) and Motte & Calvert (1990) applied DP to a grid of points; Wei & Zhou (2012, TransNav) developed a 3D DP with time as the third dimension; Chen (2013) announced a commercial 3D-DP router. 3D-DP is essentially the graph approach with an explicit time axis; robust but memory-hungry and generally "too complicated to solve within a reasonable time" for onboard use per the voyage-optimization literature.

#### 1.4 Continuous optimal control / Zermelo / Fast Marching (FMM) / Hamilton–Jacobi
- **Zermelo's Navigation Problem (ZNP, Zermelo 1931):** find the minimum-time steering law for a vehicle of fixed speed-through-medium in a position/time-dependent drift field (wind/current). Exact for constant wind; general case is a PDE (Zermelo's equation) solved via calculus of variations or numerically. This is the mathematically correct formulation of current-aware routing and directly yields the "current-corrected polar."
- **Fast Marching Method:** exact foundational citations — **James A. Sethian, "A fast marching level set method for monotonically advancing fronts," *Proceedings of the National Academy of Sciences* 93(4):1591–1595, 1996** and **John N. Tsitsiklis, "Efficient algorithms for globally optimal trajectories," *IEEE Trans. Autom. Control* 40(9):1528–1538, 1995.** FMM solves the Eikonal/Hamilton-Jacobi equation by monotonically advancing a front — a continuous analogue of Dijkstra — giving a globally optimal travel-time field on a grid, handling obstacles and currents natively. Alan & Bayındır ("Numerical solutions to Zermelo's navigation problem under variable ocean current fields," *Marine Science and Technology Bulletin*, 2026) report: *"Globally optimal 15-day routes for an Adriatic Sea mission are computed in under one minute on a standard workstation, confirming operational feasibility"* — hardware specified as an Intel Core i7 with 16 GB RAM (not a Pi). **Downside:** anisotropic (direction-dependent) speed from a sailboat polar makes it an *anisotropic* FMM / Ordered Upwind Method, materially harder to implement and to keep convergent under strong currents (documented convergence degradation). Recommend as a *later* research track, not the first engine.
- **Evolutionary / GA / PSO / ant-colony:** used for multi-objective routing (fastest vs safest vs fuel); good at Pareto fronts but stochastic, slower, harder to test deterministically. Often hybridized (Isochrone→GA to seed the population). Not recommended for the first implementation.
- **ML / RL:** emerging (RL for steering, ML to enhance the isochrone cost heuristic, e.g. Chen & Mao 2024's Isochrone-based Predictive Optimization). Not appropriate for an offline, deterministic, testable embedded engine now.

#### 1.5 Recommendation (KISS / Pareto)
**Primary: time-dependent recursive isochrone** — single polar, heading fan + VMG headings, inside-region pruning, angular/sector pruning, direct final approach. It gives most of the achievable quality for a fraction of the effort, is deterministic and testable, and fits the Pi 4. **Sophistication path:** (a) add current drift into the propagation integrator (still isochrone); (b) add tack/gybe state + penalties; (c) optionally add an anisotropic-FMM travel-time field for validation/comparison; (d) optionally a multi-objective GA seeded by the isochrone route for comfort/safety trade-offs.

*(Note: the frequently cited "shipping companies expect routing runtimes ≤1 min, preferably ≤15 s" appears in the voyage-optimization literature (e.g. citing a Simonsen et al. market survey) but a formal ≤15 s industry requirement could not be independently sourced — treat it as directional, not authoritative. The reliable operational-feasibility benchmark is "under one minute on a standard workstation" from Alan & Bayındır 2026.)*

---

### PART 2 — POLAR DIAGRAMS & BOAT PERFORMANCE MODEL

#### 2.1 What a polar is; file formats
A polar maps (TWA, TWS) → boat speed through water (STW). Standard interchange formats used by OpenCPN, qtVlm, Expedition, Seapilot:
- **`.pol` / `.csv` / `.txt` grid** (all the same structure): first row = TWS column headers (e.g. 0,4,6,8,10,12,14,16,20,25 kt); first column = TWA rows (e.g. 0,5,…,180°); cells = boat speed in knots. Delimiter is TAB or `;` or `,`. There is no TWA=0 row for sailboats (speed = 0 at head-to-wind, implied).
- **ORC VPP polars:** target speeds plus explicit beat/run angles and VMG. Seapilot "format 1" gives, per TWS, a beat-angle/beat-VMG row, the 52–180° boat speeds, and a gybe-angle/run-VMG row (VMG values, not boat speed, in the beat/run rows).
- Parse into a 2-D array `polar[twa_index][tws_index]` with the TWA and TWS axis vectors stored separately (they are irregular grids). OpenCPN accepts `.csv`, `.pol`, and `.txt`; all share this structure.

#### 2.2 Interpolation
- **Bilinear over the TWA×TWS grid** is the standard (OpenCPN `Polar::Speed`). Find the bracketing TWA rows and TWS columns, interpolate in TWS on each row, then in TWA between rows.
- **Missing cells:** treat as NaN and search a lower TWS band (OpenCPN steps VW down by 0.25 kt), or fill via a VPP heuristic. Upwind: linear from the first defined TWA to 0 kt at TWA=0. Downwind: many tools hold boat speed constant beyond the last defined TWA (David Burch notes this makes routing favor dead-downwind, usually wrong at low TWS — flag as a tuning decision).
- **Extrapolation beyond the polar's wind range:** below the lowest TWS, scale down; above the highest TWS, either clamp to the top band or apply a reefing/degradation curve (§2.8). Do **not** silently extrapolate to unrealistic speeds.
- **Smoothing:** optional spline smoothing to remove kinks; keep the raw grid as source of truth.

#### 2.3 VMG (velocity made good) and the best-VMG table
- VMG upwind = STW·cos(TWA); best upwind VMG angle = the TWA maximizing it. VMG downwind = STW·cos(180−TWA); best downwind (run) VMG angle maximizes it. Graphically, the highest/lowest points of the polar plotted in polar coordinates.
- **Precompute, per TWS band, a table:** {beat_TWA, beat_VMG, beat_boatspeed, run_TWA, run_VMG, run_boatspeed}. Routing engines need this so that when the direct course to the mark falls inside the no-go zone, the search still has the two optimal VMG headings available as candidates (instead of a slow near-head-to-wind course).

#### 2.4 No-go zone / dead angle — emerges from the polar
Do **not** hardcode a 45° no-go. The polar simply has no data (or ~0 speed) below the beat angle; `Polar::Speed` returns NaN/near-zero there, so those headings are naturally rejected. The router then either (a) uses the explicit beat/run VMG headings as candidates and *the isochrone geometry decides where to tack*, or (b) with "optimize tacking" enabled, allows a virtual 0–360° course whose achieved VMG toward that bearing is computed via the tacking projection, and materializes the actual tacks in post-processing.

#### 2.5 Tacking / gybing penalties and anti-sawtooth
- **Model a tack/gybe as lost time** (boat decelerates then rebuilds speed). Represent as a fixed time penalty added when a propagation step crosses head-to-wind (tack) or dead-downwind (gybe) relative to the parent's point of sail.
- **Typical values (LuckGrib/qtVlm practice):** cruiser ~4 min daytime, ~10 min at night; racing boats smaller. LuckGrib caps the applied penalty at ~½ the isochrone time interval (otherwise a large penalty vs a fine time step misbehaves and generates too many maneuvers). Also model **minimum time between tacks** and optionally a minimum leg length.
- **Anti-sawtooth techniques:** (1) tack penalty cost; (2) minimum tack duration / minimum leg length; (3) the OpenCPN "Optimize Tacking" approach — search on VMG and hide micro-tacks, materialize a small number of real tacks in post-processing; (4) path smoothing / heading-change limiting / hysteresis on the final waypoint list; (5) Douglas–Peucker + collinear-leg merge.
- Requires carrying a **tack/point-of-sail state** in the node so penalties apply only on genuine changes (see state space, Part 6).

#### 2.6 Apparent vs true wind (display and sail selection)
- AWS = √(TWS² + STW² + 2·TWS·STW·cos(TWA)); AWA = atan2(TWS·sin TWA, STW + TWS·cos TWA). (Matches OpenCPN `Polar::VelocityApparentWind`/`DirectionApparentWind` and standard wind-triangle sources.)
- Inverse (from instruments): TWS = √((AWS·cos AWA − STW)² + (AWS·sin AWA)²); TWA = atan2(AWS·sin AWA, AWS·cos AWA − STW).
- AWA ≤ TWA always (boat motion draws apparent wind forward). Use AWA/AWS for sail-plan selection tables (qtVlm: "sails selection according to TWS and TWA") and for an apparent-wind constraint (max AWS).

#### 2.7 True wind vs ground wind with current (current-corrected polar / Zermelo)
The sails feel the wind relative to the water (the boat's motion **through water**), but routing is over ground. When current is present: the polar gives velocity-through-water (STW along heading through the water); ground velocity = water-velocity-vector + current-vector. So propagate the boat's water velocity from the polar, then add the current vector before advancing position over ground. This is exactly Zermelo's problem: the "effective polar" over ground is the water polar translated by the current vector, deforming the reachable set. Compute TWA against the wind-over-water, not wind-over-ground, when current and wind differ.

#### 2.8 Reefing / heavy-weather polar degradation
Above configured TWS thresholds, degrade the polar (reduced speed reflecting reefed sail area), or switch to a separate heavy-weather sail-plan polar (Expedition/Nobeltec/Weather4D support multiple sail plans). qtVlm scales polar performance ± a % upwind/downwind without editing the polar. OpenCPN supports multiple sail plans with a crossover tab showing which polar applies per condition.

#### 2.9 Leeway
Leeway is the downwind sideways slip; course-over-water differs from heading by the leeway angle (larger upwind and in light air / big waves). For v1, either ignore or apply a small heading-dependent leeway offset to the achieved course. Flag as a later refinement.

#### 2.10 Wave / sea-state effect
Apply either (a) a **wave-height penalty** reducing boat speed as a function of significant wave height HTSGW (and sometimes the wind-wave/swell angle), or (b) a separate **wave polar**. qtVlm has a "cross seas correction" based on swell height and the angle between wind waves and swell. Model as a multiplicative speed factor `f(Hs, angle) ∈ (0,1]`, plus a hard `max_wave_height` constraint.

#### 2.11 Motoring / motor-sailing
- Model a fixed **motoring speed** and **fuel burn rate** → fuel range.
- Decision rule: if sail boat-speed VB < `motor_threshold_kt` (qtVlm/LuckGrib "minimum sailing speed before the engine comes on"), switch to `motor_speed_kt`. Motor-sailing = max(sail VB, motor speed) or a combined model. Track fuel consumed and enforce range as a constraint.
- Anchoring/heaving-to: OpenCPN "Anchoring" option lets the route sit in place when contrary current exceeds boat speed, to wait it out — the FIFO "waiting" case, useful for tidal gates.

---

### PART 3 — SAFETY & COMFORT CONSTRAINTS

#### 3.1 Hard constraints vs penalty costs
- **Hard (reject the node):** TWS > `max_tws`, AWS > `max_aws`, wave height > `max_wave_m`, latitude beyond `max_lat`, wind-vs-current opposition beyond a threshold (OpenCPN takes the dot product of wind and current vectors; e.g. value 60 rejects 30 kt wind opposing 2 kt current, or 20 kt opposing 3 kt). Depth < safety contour, inside prohibited area, crossing land — all hard.
- **Soft (add cost):** proximity to hazards (offing), night sailing (reduced polar %), point-of-sail discomfort, being close to a lee shore.

#### 3.2 Lee shores, offing, minimum distance to hazards
- Maintain a **minimum offing** via a signed-distance field; add rapidly rising cost as distance→0, hard-reject below `min_offing_nm`.
- **Lee shore** (wind blowing onto shore): raise offing cost when the shore is downwind of the boat.

#### 3.3 Night sailing, fatigue, watch-keeping
- Night: reduce polar efficiency % (crew reefs conservatively / sails less aggressively) and/or increase tack penalty at night (LuckGrib supports both — e.g. 4 min day, 10 min night).
- Long passages: model watch structure only as a comfort weighting; not a hard router constraint in v1.

#### 3.4 Multi-objective (fastest / safest / most comfortable)
Expose as a weighted cost `J = w_time·time + w_safety·safety_cost + w_comfort·comfort_cost`. Tools expose this via constraint sliders (max wind/wave) plus penalties. For a Pareto set, later add a GA seeded by the fastest isochrone route.

---

### PART 4 — CHART-DERIVED CONSTRAINTS FROM S-57 / SENC

#### 4.1 Hard-obstacle object classes (machine-checkable predicates)
Treat as **hard obstacles** (navigability = false for the cell/segment):
- `LNDARE` (land area) — always.
- `DEPARE` (depth area) where `DRVAL1 < safety_contour` (DRVAL1 = shallowest depth in the area).
- `DRGARE` (dredged area) where `DRVAL1 < safety_contour`.
- `UNSARE` (unsurveyed area) — treat as unsafe by default (configurable).
- `OBSTRN` (obstruction) — hard if `VALSOU < safety_contour`, dangerous `WATLEV`, or foul-area `CATOBS`.
- `WRECKS` — hard if `CATWRK` = dangerous wreck, `VALSOU < safety_contour`, or `WATLEV` ∈ {covers/uncovers, awash, always dry}.
- `UWTROC`/`ROCKS` — hard if `WATLEV` dangerous or `VALSOU < safety_contour`.
- `PONTON`, `DAMCON`, `SLCONS`, `FLODOC`, `HULKES`, `DYKCON`, `GATCON` — hard structures.
- `CTNARE` (caution area) — soft warning, not hard.

```
is_hard_obstacle(f) :=
    f.class == LNDARE
 || (f.class in {DEPARE,DRGARE} && f.DRVAL1 < safety_contour)
 || (f.class == WRECKS && (f.CATWRK == dangerous || f.VALSOU < safety_contour))
 || (f.class == OBSTRN && (f.VALSOU < safety_contour || watlev_dangerous(f.WATLEV)))
 || (f.class in {UWTROC,ROCKS} && (watlev_dangerous(f.WATLEV) || f.VALSOU < safety_contour))
 || f.class in {PONTON,DAMCON,SLCONS,FLODOC,HULKES}
```

#### 4.2 Safety contour from depth areas (ECDIS model)
Follow the ECDIS S-52 model:
- User configures three depths: **safety contour** (ECDIS default 30 m; for a yacht set draft-derived), **shallow contour** (default 2 m), **deep contour** (default 30 m). Plus a **safety depth** used only for sounding emphasis (soundings ≤ safety depth shown bold/black; it triggers no alarm).
- **Vessel safety draft = draft + dynamic squat + safety margin (UKC) − height of tide.** Set `safety_contour_depth = draft + squat + UKC − tide_height`.
- ECDIS selects, as the effective **safety contour**, the shallowest available DEPARE/DEPCNT contour that is **equal to or deeper** than the requested safety-contour depth (if the exact value isn't in the SENC it uses the next deeper one and warns).
- **Navigability predicate for a DEPARE:** safe iff `DRVAL1 ≥ effective_safety_contour`.
- Tide raises effective depth: `available_depth = charted_depth + tide_height(t)`; recompute safe/unsafe per time step when tide-aware.

#### 4.3 SOUNDG and DEPCNT
- `SOUNDG`: reject/flag any cell containing a sounding `< safety_depth` (after tide correction). Additional point constraints beyond DEPARE polygons.
- `DEPCNT`: the linear representation of depth contours; use to build the safe-water boundary polygon if DEPARE polygons are unavailable.

#### 4.4 Restricted / prohibited areas
- `RESARE` with `CATREA` (e.g. nature reserve, prohibited entry, no-anchoring): hard-reject where CATREA indicates entry prohibited; soft-cost otherwise.
- `MIPARE` (military practice area): soft-cost or time-window hard.
- `MARCUL` (marine farm/aquaculture): hard obstacle.
- `CBLARE`/`PIPARE` (cable/pipeline areas), `CBLSUB` (submarine cable): no-anchor constraint (not a transit block).
- Anchorage prohibitions (`ACHARE`/`ACHBRT` with restrictions): enforce in anchor-decision logic only.

#### 4.5 Traffic Separation Schemes and COLREGS Rule 10
S-57 objects: `TSSLPT` (traffic separation lane part, orientation attribute `ORIENT` = traffic direction), `TSEZNE` (separation zone), `TSSBND` (boundary), `ISTZNE` (inshore traffic zone), `TSSRON` (roundabout), `PRCARE` (precautionary area).
Encode **Rule 10** as routing cost/constraints:
- Inside a `TSSLPT`, prefer headings aligned with `ORIENT`; heavily penalize traveling against the lane — a vessel *"shall proceed in the appropriate traffic lane in the general direction of traffic flow for that lane."*
- If the route must **cross** lanes, force the crossing heading **as nearly as practicable to 90° to `ORIENT`** (Rule 10(c): *"cross on a heading as nearly as practicable at right angles to the general direction of traffic flow"*): add cost ∝ |cross_angle − 90°|, or hard-require |90° − angle| < tolerance.
- Do **not** route into a `TSEZNE`/across a separation line except in emergency (hard-reject).
- Sailing vessels **may** use `ISTZNE` (Rule 10(j): *"vessels of less than 20 m in length, sailing vessels... may use the inshore traffic zones"*) — allow, unlike big ships. Also Rule 10(j): a sailing vessel *"shall not impede the safe passage of a power-driven vessel following a traffic lane."*
- **Design nuance to flag:** the Rule 10 crossing angle is a *heading through the water* (aspect), and mariners are told **not** to allow for tide when crossing. An over-ground router models this only approximately.

#### 4.6 Bridges / overhead clearance
`BRIDGE` with `VERCLR` (vertical clearance to charted datum). Hard-reject if `air_draft (mast + antenna) > VERCLR + margin`. Tide reduces clearance: `available_clearance = VERCLR − (tide_height(t) − chart_datum_offset)`. A bridge may be passable only near low water — a **tidal gate on air draft**.

#### 4.7 Locks, ferries, other transit
- `FERYRT` (ferry route): soft-cost / caution.
- Locks: model as time-window/operating-hours transit nodes (later refinement).

#### 4.8 Buoyage, IALA lateral marks, channels
- Objects: `BOYLAT`/`BCNLAT` with `CATLAM` (**1 = port-hand, 2 = starboard-hand, 3 = preferred-channel-starboard, 4 = preferred-channel-port**), `BOYCAR`/`BCNCAR` (cardinal), `BOYSAW` (safe water), `BOYSPP` (special), plus `FAIRWY`, `NAVLNE`, `RECTRC` (recommended track, has direction), `DWRTPT` (deep water route).
- **Direction of buoyage** determines which side is "correct." IHO defines a conventional direction (generally: entering from seaward, going upstream, or clockwise around a landmass). IALA **Region A** (Europe, Africa, most of Asia, Australasia, India): port marks red, starboard marks green. **Region B** (Americas, Japan, Korea, Philippines): reversed ("red-right-returning"). NavCore must store a per-chart Region A/B flag. Cardinal, isolated-danger, safe-water and special marks are identical in both regions.
- **Determining the correct side geometrically:** given direction-of-travel vector **d** and buoy category:
  1. Determine whether the vessel travels *with* or *against* the direction of buoyage (dot product of **d** with the local buoyage-direction vector).
  2. Traveling *with* buoyage in Region A: port-hand (red) marks stay on the vessel's port side, starboard-hand (green) on starboard. The sign of the cross product of **d** with the vessel→buoy vector gives the side (port/starboard).
  3. Reverse for traveling *against* buoyage, and reverse red/green for Region B.
- **Do routers actually use buoyage?** In practice OpenCPN and qtVlm do **not** route by buoyage; they avoid land/shallows and leave channel-following to the navigator. Recommendation: v1 uses `FAIRWY`/depth to stay in navigable water and only *prefers* (soft cost) fairways; a later refinement can add a soft cost for passing lateral marks on the wrong side. Full IALA-correct channel following is a hard, low-ROI feature — flag as optional.

#### 4.9 Spatial data structures for the router
- **Navigability cost raster:** rasterize hard obstacles + safety contour to a boolean/cost grid. Fast O(1) point lookup; the workhorse on a Pi. Resolution trade-off: a boolean grid at 100 m over a 200×200 nm area ≈ 370M cells — too big; use **tiles + adaptive resolution**, or store as run-length/quadtree. Use `f32` and only the active bounding box at full resolution.
- **Polygon obstacle set** for visibility-graph / segment tests (land-avoidance great-circle stage). Store as `geo` polygons.
- **R-tree (Rust `rstar`) or quadtree** spatial index over SENC features for fast "features near segment" queries during propagation land tests.
- **Signed distance field (SDF):** precompute distance-to-nearest-hazard per cell (fast distance transform); the router reads it for smooth offing costs and gradients.
- **Precomputation:** build mask + SDF once per routing job over the start→finish bounding box (plus margin), at a resolution matched to the finest channel you must transit. On a Pi 4, prefer tiled rasters with lazy per-tile generation.

---

### PART 5 — ENVIRONMENTAL DATA (WIND, WAVE, CURRENT, TIDE)

#### 5.1 GRIB
- **GRIB1 vs GRIB2:** GRIB2 is current (GFS, most models); support both on read.
- **Parameters:** `UGRD`/`VGRD` at 10 m (wind U/V), `GUST`, `HTSGW` (sig. wave height), `DIRPW`/`PERPW` (wave dir/period), `PRMSL` (pressure), `APCP` (precip), currents `UOGRD`/`VOGRD`.
- **Sources / cadence / size:**
  - **NOAA GFS:** global **0.25°**, FV3 core (~28 km); **runs at 00/06/12/18 UTC daily**. Standard GFS product forecast steps are **3-hourly to 240 h, then 12-hourly to 384 h (16 days)**; hourly-to-120 h availability applies to the GFS-Wave/WW3 product per NCEP/EMC (some redistribution services expose hourly-to-120 h). Model output for a run is typically available from ~3:30 h after cycle time (e.g. the 00Z run arrives from ~03:30 UTC per Wetterzentrale), i.e. roughly 3.5–5 h latency.
  - **ECMWF open data**, **Copernicus Marine** (currents), **RTOFS** (ocean currents, ~5-day horizon), **Météo-France Arome/Arpège**, **DWD ICON**, **HRRR** (CONUS 3 km, hourly, 18 h horizon).
  - Delivery: **Saildocs** (email; syntax e.g. `send GFS:26N,20N,114W,105W|0.25,0.25|0,6..72|WIND,PRMSL,WAVES`; `sub` for a recurring subscription, default 14 days), zyGrib/Great Circle, openskiron, PredictWind. Files from tens of KB up.
- **Rust decoding:**
  - **`grib` (grib-rs by noritada):** pure-Rust GRIB2 reader (+ `grib-cli`); regular lat/lon grids, simple/complex packing, PNG/JPEG2000 unpacking; good for offline/embedded. **Recommended primary** — avoids a C toolchain on ARM.
  - **`gribberish` (Matthew Iannucci):** pure-Rust GRIB2 parser; good alternative.
  - **eccodes bindings** (`eccodes`/`eccodes-sys`): mature ECMWF C library, GRIB1+GRIB2+BUFR, but a C dependency (harder cross-compile to Pi, larger footprint). Use only for obscure templates.
  - `grib1_reader`/`grib2_reader`/`grib-reader` exist but are less complete.

#### 5.2 Wind interpolation (space + time) — do it in U/V
- **Never interpolate direction/speed directly** (0°/360° wrap and vector-averaging errors). Interpolate the **U and V components** bilinearly in space and linearly in time, then convert: `TWS = √(U²+V²)`, `TWD = atan2(-U,-V)` (meteorological "from" convention — be explicit about the sign convention).
- Bicubic in space is optional; bilinear is fine for a Pi. Time: linear between the two bracketing GRIB steps.

#### 5.3 Tidal current & height
- **Harmonic constituent model:** `h(t) = Z0 + Σ_i A_i·f_i·cos(ω_i·t + (V0+u)_i − g_i)`; constituents M2, S2, N2, K1, O1, etc.
- **Data:** XTide-compatible **`.tcd`** (Tide Constituent Database, libtcd format) + legacy `HARMONIC`/`HARMONIC.IDX` (IDX) files — the two formats OpenCPN supports. Free updated US data from flaterco.com (published ~each December); the non-free UK/NL set was last updated 2011. Global open set: openwatersio/tide-database (NOAA ~3400 stations + TICON-4 ~4200 global stations) distributed as an XTide-compatible `.tcd`.
- **OpenCPN model:** loads `.tcd`/IDX, computes harmonic predictions per station; current data is US-centric and lower resolution. For NavCore, reimplement harmonic prediction in Rust from `.tcd` (or port libtcd) → returns tide height and current (set/drift) for a station; interpolate spatially between stations (sparse — offshore, GRIB current fields (RTOFS/Copernicus) are often better). Note a known OpenCPN quirk: multi-depth current stations default to the deepest layer and may not draw an arrow.
- Compute current vector (set = direction toward which it flows, drift = speed) for a point/time; add to boat water-velocity per §2.7.

#### 5.4 Tidal gates & fair-tide planning
- Encode a **tidal gate** as a time-window constraint on a location: the passage is favorable (or only possible — bridge air-draft, lock hours, shallow bar) only within [t_open, t_close]. In the search this is a time-dependent cost that is high/∞ outside the window.
- "Carry a fair tide": since current already enters propagation, the router naturally favors timing legs to ride favorable current; explicit gates handle hard constraints (bar depth, bridge, lock).

#### 5.5 Combining water velocity + current → ground velocity
`V_ground = V_water(polar, heading) + V_current`. Vector addition. This feeds Zermelo (§2.7): the achievable ground-velocity set at a point is the polar curve (through water) translated by the current vector.

---

### PART 6 — THE ROUTING ENGINE SPECIFICATION

#### 6.1 Formal problem statement
Given start S, finish F, departure time t0, a time-dependent wind field W(x,t), optional current C(x,t) and wave field H(x,t), a polar P(TWA,TWS)→STW, and a navigability predicate nav(x,t): find the sequence of headings/positions minimizing arrival time (or weighted cost J) from S to F subject to nav and safety constraints, where instantaneous ground speed is state- and time-dependent:
```
minimize  T (arrival time)      [or J = w_t·T + Σ penalties]
subject to  dx/dt = V_water(P, W(x,t), heading(t)) + C(x,t)
            nav(x(t), t) = true  for all t
            TWS ≤ max_tws, wave ≤ max_wave, depth ≥ safety_contour, ...
```

#### 6.2 State space
Node state = **(lat, lon, time, tack/point-of-sail state, sail_config)**. Tack state (port/starboard, upwind/downwind) is required to apply tack/gybe penalties correctly; sail_config selects the active polar (heavy-weather, spinnaker). Store a parent pointer for path reconstruction and absolute time per node (GRIB/tide are absolute-time), even though time is implicit in the isochrone index at constant Δt.

#### 6.3 Primary algorithm — recursive isochrone (pseudocode)
```
PARAMETERS (defaults):
  dt              = 3600 s        # isochrone time step (coastal); 10800 s (3 h) offshore
  heading_step    = 5 deg         # candidate heading resolution
  max_diverted    = 100 deg       # max |heading - bearing_to_finish|
  tack_penalty    = 240 s (day) / 600 s (night)
  gybe_penalty    = 240 s (day) / 600 s (night)
  min_tack_dt     = dt            # no tack more often than one step
  arrival_radius  = 0.1 nm        # destination tolerance
  max_tws         = 35 kt         # hard
  max_wave        = 4.0 m         # hard
  offing_min      = 0.2 nm        # hard
  prune_cell      = 0.05 nm       # spatial-hash cell for frontier dedup

isochrones = [ {start_node(S, t0)} ]
loop k = 0,1,2,...:
  t_k1 = t0 + (k+1)*dt
  frontier_new = []
  for p in isochrones[k]:                       # each frontier node
    (Wd, Ws) = wind(p.pos, p.time)              # U/V interp
    cur = current(p.pos, p.time)                # optional
    bearing_fin = bearing(p.pos, F)
    headings = fan(bearing_fin, max_diverted, heading_step)
               ∪ {beat_TWA headings, run_TWA headings relative to Wd}
    for h in headings:
      twa = angle_between(h, Wd_over_water)
      stw = polar_speed(twa, Ws)                # bilinear; NaN in no-go
      if stw is NaN: continue
      stw *= wave_factor(H(p.pos,p.time))       # sea-state
      stw *= night_factor if night(p.time)
      v_ground = vector(h, stw) + cur
      child_pos = geodesic_step(p.pos, v_ground, dt)
      if crosses_land_or_obstacle(p.pos, child_pos): continue
      if !nav(child_pos, t_k1): continue        # depth, restricted, TSS
      if Ws > max_tws or wave > max_wave: continue
      pen = tack_or_gybe_penalty(p.tack_state, twa)
      child = Node(child_pos, t_k1 + pen, new_tack_state, parent=p)
      frontier_new.push(child)
  # PRUNE 1: inside-region (degeneracy fix)
  frontier_new = drop points inside union(isochrones[0..k])
  # PRUNE 2: spatial-hash / angular sector — keep the point that is
  #          furthest-advanced (min time) per (angular sector | grid cell)
  frontier_new = sector_prune(frontier_new, S, cell=prune_cell)
  isochrones.push( normalize_polygon(frontier_new) )   # merge, holes
  if any node within arrival_radius of F:
     connect_directly_to_F(); break
route = follow_parents_back(best_node_reaching_F)
```

#### 6.4 Cost function (explicit)
For a propagation step from node *p* along heading *h* over Δt:
```
stw          = P(TWA(h, Wd_water), TWS_water)     # polar, bilinear
stw_eff      = stw · f_wave(Hs, wave_angle) · f_night · f_perf%
v_water      = stw_eff · unit(h)
v_ground     = v_water + C(p.pos, p.time)
step_dist    = |v_ground| · Δt
time_cost    = Δt + tack_gybe_penalty(p.tack_state, TWA)
safety_cost  = w_off · offing_cost(SDF(child)) + w_tss · rule10_cost(...)
edge_cost    = time_cost + safety_cost            # pure isochrone uses time only
```
`nav(child,t)` must be true (hard): depth ≥ safety contour, not land, not prohibited, air-draft OK, offing ≥ min.

#### 6.5 Pruning & performance (Pi 4)
- **Inside-region pruning** (must-have): drop any child inside the already-reached region.
- **Angular/sector pruning:** bucket children by bearing-from-start into sectors (e.g. 1°); keep only the furthest-advanced per sector (classic Hagiwara sub-isochrone pruning). Alternative: spatial-hash grid, keep min-time node per cell.
- **Limit heading candidates:** fan over [bearing−max_diverted, bearing+max_diverted] every `heading_step`, **plus** the two VMG headings. Don't sweep 360° unless "optimize tacking" needs it.
- **Adaptive time step:** coarse Δt offshore, fine Δt (down to a few minutes) near coast / narrow channels.
- **Spatial index (`rstar`)** for land/obstacle segment tests; cache wind lookups; use `geographiclib-rs` or `geo` Haversine/Geodesic for the step math. `f32` rasters, tile the SDF, parallelize the per-frontier-node loop across the Pi's 4 cores (rayon).

#### 6.6 Termination & final approach
- Stop when an isochrone node comes within `arrival_radius` of F, or when the destination is enclosed. Then attempt a **direct feasible connection** to F (validated for land/depth). If the degeneracy collapses the frontier near F, use a **reverse/closest-position recovery** (as an OpenCPN refactor branch does): a bounded reverse connection from F back to the nearest frontier node.
- Guard against "impossible to reach" when S or F is too close to shore (qtVlm's known failure): auto-nudge endpoints offshore or warn.

#### 6.7 Route post-processing
- **Simplify** the raw isochrone path with Douglas–Peucker (tolerance ~0.1 nm) and **merge collinear legs**.
- **Materialize tacks:** if searching on VMG (hidden micro-tacks), insert a realistic number of tack waypoints (respect min leg length) rather than a sawtooth.
- Convert to a **waypoint list**; per leg emit: heading, TWA, point of sail, sail plan, expected boat speed, leg distance, ETA, cumulative time.
- Smooth heading changes; apply hysteresis so tiny wind shifts don't create spurious tacks.

#### 6.8 Re-optimization underway
Trigger a re-route when: actual position diverges > threshold from planned track (XTE), a new GRIB arrives, or ETA drifts beyond tolerance. Re-run from current position/time with the new forecast; keep the previous route for comparison. Cheap because the engine is fast.

#### 6.9 Validation & testing
- **Analytic unit test:** constant uniform wind + a simple polar with known best VMG. The optimal upwind route is a straight VMG beat with a single tack; verify arrival time and tack geometry. Zero-current beam reach = great-circle time at polar beam speed.
- **Zermelo constant-current test:** constant current + constant wind has a closed-form Zermelo solution; verify.
- **Cross-tool regression:** run the same GRIB + polar through qtVlm and OpenCPN Weather Routing and compare arrival times/tracks (close, not identical, due to Δt and interpolation differences).
- **Golden regression tests:** freeze known-good routes for a set of scenarios; fail on deviation beyond tolerance.

---

### PART 7 — WAYPOINT & ROUTE DATA MODEL AND INTERCHANGE

#### 7.1 Data model & formats
- **GPX 1.1** (`<wpt>`, `<rte>`/`<rtept>`, `<trk>`/`<trkpt>`) is the interchange lingua franca; OpenCPN uses the **Garmin GPX Extensions v3** schema plus its own extensions, and its backup file `navobj.xml` is GPX-structured (rte/rtept for routes, trk/trkpt for tracks, wpt for marks). Support GPX import/export with OpenCPN extensions for round-tripping (also used by Garmin, Raymarine, Navico/B&G/Simrad/Lowrance, Furuno, Navionics, Coastal Explorer, PredictWind, LuckGrib).
- **Signal K resources API:** routes/waypoints/notes/regions under `/signalk/v2/api/resources/{routes,waypoints,notes,regions}` (v1 was `/signalk/v1/api/resources/...`). Routes store geometry as a **GeoJSON** object; waypoints as `{position:{latitude,longitude}}`. CRUD via GET/POST/PUT/DELETE; a resource-provider plugin backs it (stores under `~/.signalk/.../resources/{routes,waypoints,...}`, one file per resource by UUID). The **Course API** (`/signalk/v2/api/vessels/self/navigation/course/{destination,activeRoute}`) activates a destination/route by `href` to a resource. Submitted resources are validated against the OpenAPI schema.
- **NMEA 0183:** `RTE` (route), `WPL` (waypoint location), plus `RMB`/`APB`/`XTE`/`RMC` for active navigation to an autopilot. OpenCPN outputs `$ECRMB`, `$ECRMC`, `$ECAPB`, `$ECXTE` when a route is active.
- **NMEA 2000 PGNs:** `129285` (route/waypoint information — waypoint names in the active route), `129284` (navigation data), `129283` (cross-track error). OpenCPN core does not natively emit/consume all route PGNs; gateways (Yacht Devices, Actisense) bridge 0183↔2000 (Yacht Devices caches routes/waypoints and uses PGN 129284 + proprietary Raymarine PGNs 130848/130918 for name resolution).

#### 7.2 Active-navigation math
- **XTE:** perpendicular distance from present position to the great-circle/rhumb leg (sign = left/right of track; `icao-wgs84` returns positive to the left, negative to the right). `geo`/`icao-wgs84` provide across-track distance.
- **BTW/DTW:** bearing/distance to waypoint. **VMG-to-waypoint** = SOG·cos(BTW−COG).
- **Arrival & waypoint advance:** trigger arrival when inside the **arrival circle** (default radius e.g. 0.05–0.2 nm) or when the perpendicular through the waypoint is crossed; then advance. "Activate route" sets the first leg active; support "next waypoint" and autopilot output. OpenCPN option "Advance route waypoint on arrival" mirrors this.

#### 7.3 Great circle vs rhumb line
- **Great circle** = shortest path on the sphere; bearing changes continuously; matters for long ocean legs. **Rhumb line (loxodrome)** = constant bearing; simpler to steer, longer; matters for short coastal legs and autopilot steering. Store a per-leg type; compute with `geo` Haversine/Geodesic (GC) or Rhumb methods. Isochrone steps use great-circle geodesic increments.

---

### PART 8 — OPEN-SOURCE IMPLEMENTATIONS TO STUDY (with licences)

- **OpenCPN `weather_routing_pi`** — github.com/seandepagnier/weather_routing_pi — **GPLv3**. The reference isochrone engine: `RouteMap`, `IsoChron`, `IsoRoute`, `Position`, `SkipPosition`, `Polar` classes; `Polar.cpp` has the exact speed/apparent-wind/VMG-tacking math. **GPL — study the algorithm, do not copy code into a non-GPL Rust project.** Active refactor discussion toward a headless/testable engine: OpenCPN discussion #4485, fork `pob220/weather_routing_pi` branch `routing-engine-refactor`.
- **OpenCPN core** — github.com/OpenCPN/OpenCPN — **GPLv2+**. Route/waypoint handling, `navobj.xml`, tide/current code (`data/tcdata`, XTide-derived harmonics in `README.harmonics`).
- **qtVlm** — closed-source freeware (Meltemus); no source, but excellent behavioral reference (isochrones, tack/gybe penalties, sail selection, reverse isochrones, batch mode); the documentation PDF is detailed and runs on Raspberry Pi.
- **libweatherrouting / gweatherrouting** (Davide Gessa "dakk") — github.com/dakk/libweatherrouting — **GPLv3**. 100% Python; clean `Routing`/`Polar`/`LinearBestIsoRouter` API with `get_wind_at`, `point_validity`, `line_validity` hooks; the clearest readable reference for the isochrone loop and polar parsing. Used by the wind_forecast_routing QGIS plugin.
- **richard-mackie/weather-router** — Python browser router; readable `isochrones.py` with an `isochrone_Node` class (lat, lng, time, parent, heading, TWA, dist_start, dist_finish) — a compact node model to mirror. Verify licence.
- **sailnavsim** — sailing simulation; boat/polar modeling reference. Verify licence.
- **iBoat / iboat-vpp**, **sailing-vmg / sailrouter**, assorted JS "boat-router"/"isochrones" repos — references; verify licences individually.
- **Rust crates (permissive — safe to depend on):**
  - `grib` (grib-rs, noritada) — GRIB2 decode; `gribberish` (MIT) — GRIB2.
  - `geographiclib-rs` (MIT) — geodesic direct/inverse (Karney port). `geo` / `geo-types` (MIT/Apache-2.0) — geometry, Haversine/Geodesic/Rhumb distance+bearing+destination, intersection, closest-point. `icao-wgs84` — WGS84 geodesic + across-track (XTE), `no_std`.
  - `rstar` (MIT/Apache-2.0) — R*-tree spatial index (interoperates with `geo`).
  - `proj` — PROJ bindings if projection is needed. `h3o` (MIT/Apache) — pure-Rust H3 hex grid if you ever try a grid approach.
- **Fast Marching (later FMM track):** scikit-fmm (Python, BSD) as reference; C++ FMM libraries exist; the *anisotropic* FMM/Ordered-Upwind variant is what's needed for a polar.

**GPL contamination note:** `weather_routing_pi`, `libweatherrouting`, and OpenCPN core are GPL — you may **read and learn** the algorithms and reimplement cleanly, but must not paste their code into NavCore unless NavCore is GPL-compatible. The Rust crates above are permissively licensed and safe.

---

### PART 9 — PREREQUISITES (ordered)

1. **Geodesy module** — great-circle & rhumb distance/bearing/destination, XTE, intersection. *Why:* every step and leg computation. *API:* `distance(a,b)`, `bearing(a,b)`, `destination(a,brg,dist)`, `cross_track(p,a,b)`. Use `geo` + `geographiclib-rs`.
2. **Queryable spatial index over SENC features** — R-tree (`rstar`) over parsed DEPARE/LNDARE/OBSTRN/etc. *Why:* fast "features near segment" for land/hazard tests. *API:* `features_intersecting(bbox)`, `nearest(point)`.
3. **Depth/obstacle rasterizer + navigability mask + SDF** — rasterize hard obstacles & safety contour to a tiled cost grid; compute distance-to-hazard field. *Why:* O(1) `nav(x,t)` and offing cost. *API:* `nav(pos,t)->bool`, `offing(pos)->nm`, `cost(pos)->f32`.
4. **Persistent waypoint/route store + GPX import/export** — CRUD over waypoints/routes/tracks; GPX (OpenCPN extensions) in/out; Signal K resources sync. *Why:* the route system the router feeds. *API:* `save/load/list/delete`, `import_gpx`, `export_gpx`.
5. **GRIB downloader + decoder + wind-field query API** — fetch (Saildocs/NOAA) + decode (`grib` crate) + U/V interpolation. *Why:* wind input. *API:* `wind(pos,t)->(dir,spd)`, `wave(pos,t)`, `current(pos,t)`.
6. **Polar parser + VMG table** — parse `.pol/.csv`, bilinear interpolation, precompute beat/run VMG per TWS. *Why:* boat physics. *API:* `speed(twa,tws)->Option<f32>`, `best_vmg(tws, upwind|downwind)->(twa,vmg,speed)`.
7. **Tide/current provider** — `.tcd`/IDX harmonic prediction. *Why:* depth correction, tidal gates, current drift. *API:* `tide_height(station,t)`, `current(pos,t)->(set,drift)`.
8. **Time-aware simulation clock** — absolute-time stepping consistent with GRIB/tide. *Why:* time-dependent search. *API:* `now()`, `step(dt)`.
9. **Route data model** — Node/Leg/Route structs with tack state, ETA, sail plan.

---

### PART 10 — PRIORITIZED IMPLEMENTATION ROADMAP

Each milestone is independently shippable (KISS, no stubs) with a definition of done (DoD) and a verification.

**M0 — Geodesy + route data model.** DoD: distance/bearing/destination/XTE functions; Route/Waypoint structs. Verify: unit tests vs known geodesic values (`geographiclib-rs`).

**M1 — Manual waypoints & routes with distance/bearing.** DoD: create/edit/delete waypoints & multi-leg routes on the chart; per-leg distance, bearing, total distance; GPX import/export (OpenCPN-compatible). Verify: round-trip a GPX through OpenCPN; distances match a known chart.

**M2 — Route following with XTE + Signal K.** DoD: "activate route," active leg, XTE/BTW/DTW/VMG-to-wp, arrival circle + auto-advance; publish/consume via Signal K resources + Course API; output `$ECRMB/$ECXTE/$ECAPB` (and/or N2K via gateway). Verify: simulate a track, confirm XTE sign/magnitude and waypoint advance; Signal K round-trip.

**M3 — Great-circle route + land avoidance.** DoD: given S,F, produce a great-circle (or rhumb) route avoiding land/hard obstacles using the SENC mask + a visibility-graph or waypoint-insertion around polygons. Verify: route around a known island/headland never crosses `LNDARE` or shallow DEPARE; compare to a hand-drawn safe route.

**M4 — Constant-wind polar routing.** DoD: isochrone engine with a single polar and a **constant uniform wind** (no GRIB yet); produces an optimal beat/run with tacks; VMG headings honored; tack penalty applied. Verify: matches the analytic constant-wind optimum (arrival time + single-tack geometry); no sawtooth after post-processing.

**M5 — Full GRIB isochrone routing.** DoD: plug in GRIB wind (U/V interpolation, time-varying), wave penalty, hard TWS/wave constraints, land/obstacle rejection, sector + inside-region pruning, Douglas–Peucker post-processing, per-leg ETA/sail-plan output; re-route on new GRIB. Verify: same GRIB+polar produces arrival time/track close to qtVlm and OpenCPN Weather Routing; runs in seconds on a Pi 4.

**M6 — Current & tide-aware routing.** DoD: add current drift into propagation (ground = water + current), tide-corrected safety contour and bridge clearance, tidal-gate time-window constraints; anchoring/heave-to to wait out foul current. Verify: constant-current Zermelo test matches closed form; a tidal-gate scenario correctly delays departure to carry fair tide; bridge passable only within the low-water window.

**Later refinements (post-M6):** TSS/Rule-10 crossing cost; IALA buoyage soft-cost channel following; multi-objective (fastest/safest/comfort) weighting + GA Pareto set; anisotropic Fast-Marching validation engine; leeway model; reefing/multi-sail-plan crossover.

---

## Recommendations

1. **Commit to the isochrone engine now; defer FMM/GA/RL.** Build M0–M2 (the route system) first — the auto-router is worthless without a solid waypoint/route/XTE/Signal K foundation, and those milestones ship value immediately.
2. **Build the chart-constraint layer as a first-class subsystem** (mask + SDF + hard predicate) before M3; it is the biggest and most reused piece.
3. **Use permissively-licensed Rust crates** (`grib`, `geo`, `geographiclib-rs`, `rstar`) and treat OpenCPN/qtVlm/libweatherrouting as *behavioral* references only (GPL) — reimplement, don't copy.
4. **Adopt these defaults and expose them as config:** Δt = 1 h coastal / 3 h offshore; heading step 5°; tack/gybe penalty 4 min day / 10 min night; max_diverted 100°; arrival radius 0.1 nm; safety contour = draft + squat + UKC − tide; max TWS 35 kt; max wave 4 m — all user-tunable.
5. **Bake in validation from M4:** the constant-wind analytic test and the qtVlm/OpenCPN cross-check are your regression safety net; keep golden routes.
6. **Thresholds that change the plan:** if Pi-4 route times exceed ~10 s for a coastal passage, coarsen Δt / tighten max_diverted / sector-prune harder before considering a grid method. If channel transits fail, drop Δt locally rather than globally. If tidal current accuracy matters and stations are sparse, switch to GRIB current fields (RTOFS/Copernicus) offshore.

---

## Caveats

- **OpenCPN internal class details are partly reconstructed** from the translation catalog, issues, and `Polar.cpp` (fetched verbatim); the exact `RouteMap.cpp`/`.h` default constants (DegreeStep, Δt, MaxDivertedCourse, TackingTime) should be confirmed by cloning the repo before finalizing constants. The `Polar::Speed` math, the `ComputeBoatSpeed` 0.25 kt fallback, the class call-chain, and the GPLv3 header are verbatim-confirmed.
- **OpenCPN "Inverted Regions" (routing around islands) is documented as buggy** — the two-sided-island case is a genuinely hard isochrone problem; test it explicitly.
- **The GFS step cadence claim was corrected during review:** the standard GFS product is 3-hourly to 240 h then 12-hourly to 384 h; the "hourly to 120 h" figure applies to the GFS-Wave product (and some redistributors), not the base GFS — verify against the specific feed you ingest, since NCEP upgrades change this.
- **The "≤15 s / ≤1 min routing runtime industry requirement" is directional, not authoritative** — it recurs in the literature citing a market survey but could not be independently confirmed; the reliable feasibility benchmark is "globally optimal 15-day route in under a minute on an Intel Core i7 / 16 GB workstation" (Alan & Bayındır 2026), which is *not* a Pi.
- **Tidal current data is sparse and US-centric** in the XTide/`.tcd` world; spatial interpolation between sparse stations is a real accuracy weakness — flag to users; offshore prefer GRIB currents.
- **Buoyage-based channel following is low-ROI and rarely implemented** by real routers; recommend soft-cost fairway preference over full IALA correctness.
- **Rule 10 crossing angle is defined through-the-water, not over-ground**; an over-ground router models it approximately — a genuine design compromise to document.
- **Anisotropic FMM for sailboat polars is materially harder** than the isotropic FMM in the literature; the "under a minute" globally-optimal results cited are for near-isotropic ship-speed models on a workstation, not a Pi with a concave sailing polar — don't assume that performance transfers.

---

## NavCore implementation notes — where the code departs from this spec

*Added 2026-09-28. The spec above is the original hand-off, unedited; these are the deliberate differences, each with its reason. Code: `src/nav/autoroute/`.*

| Spec | NavCore | Why |
| --- | --- | --- |
| §4.9 R-tree (`rstar`) over SENC features for segment tests | Not built; segments are tested by supercover traversal of the same raster the charts were stamped onto | A separate feature index would disagree with the quilt (finer charts overwrite coarser inside their coverage) exactly where quilting matters |
| §4.9 "tiles + adaptive resolution" | Two levels: a 20 m coarse grid over the whole passage box, read *loosely* (only hazard cell centres count) to pick the way; a 10 m fine grid held in 64×64-cell tiles only within 1 nm of that way, read *strictly*, finds the route. Where the fine grid finds no way, the coarse cells along the way that hold no fine water are closed and the coarse pass picks again (up to 6 times) | A strict raster erodes both shores of a channel by up to a cell, so channels narrower than 2–3 cells close; a fine grid over the whole box does not fit on a Pi. The route always comes from the strict fine search, so the two levels never make it less safe than one |
| §3.1, §6.3 `offing_min = 0.2 nm` hard | 0.005 nm hard floor; 0.2 nm is a steep cost (up to 4×) the route gives up only where the water is narrower; 0.5 nm a gentle one | A dredged channel can be 30–50 m wide (Nibe Bredning, west of Aalborg); a 0.2 nm floor closes every harbour approach and most Danish sounds |
| §6.6 nudge endpoints offshore | The offing floor and costs fade to nothing within 1 nm of each end | A berth is inside every offing by definition |
| §4.6 bridges by `VERCLR` only | Opening bridges (`CATBRG` 2, 3, 4, 5, 7) whose closed clearance is too low are *gates*: passable at a soft cost, the opening span carved open after every chart is stamped, as its box stretched 1.5 cells up and down the channel, with the offing faded within 0.2 nm | A 30 m bascule opening is a single cell, and the fixed spans and fenders either side are stamped over it (the Limfjord at Aalborg was unreachable). The carve opens cells that `SLCONS` fenders touched, only inside that box — a deliberate exception to §4.1. Opening *schedules* would fit §5.4's time-window gates; not modelled yet |
| §4.1 `CTNARE` soft cost | No cost; the route warns when it crosses a caution, restricted or exercise area | Caution areas are notes to the skipper (NOAA wraps whole bays in them) |
| §6.3 isochrones everywhere | Isochrones on open water only; confined water follows the grid corridor | Π₆: a 5° heading fan at sailing time steps steps straight over a 0.2 nm channel |
