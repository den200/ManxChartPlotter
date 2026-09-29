//! SENC feature extraction.
//!
//! Parses S-57 features from SENC records, focusing on:
//! - LNDARE (land area) - renders as tan
//! - DEPARE (depth area) - renders with depth-based blue gradient

use byteorder::{LittleEndian, ReadBytesExt};
use std::collections::HashMap;
use std::io::Cursor;

use super::geometry::{AreaGeometry, EdgeTable, LineGeometry, MultipointGeometry, PointGeometry};
use super::reader::{RawRecord, SencError, SencReader};
use super::records::{ObjectClass, RecordType, SencHeader};

/// Convert S-57 attribute code to attribute name.
/// Every S-57 attribute, code and acronym, from IHO S-57 Ed 3.1 Appendix A
/// (`s57attributes.csv` as shipped with OpenCPN).
///
/// A feature stores the *code* the SENC gave it; lookups arrive as an acronym
/// and go through [`s57_attribute_code`]. The table must stay sorted by code —
/// a test asserts it, and both lookups binary-search.
///
/// Completeness matters more than it looks. Manx named 51 of these, and 34
/// of the 60 acronyms the lookup tables key on were among the missing: every
/// LUP row selecting on CATLMK, CATSPM, CATHAF, NATSUR and the rest could
/// never match, so those features fell through to their class default symbol.
/// The conformance harness could not see it either, because it feeds the
/// OpenCPN oracle the attributes Manx parsed — both engines agreed, on the
/// same incomplete input.
const S57_ATTRIBUTES: &[(u16, &str)] = &[
    (1, "AGENCY"), // Agency responsible for production
    (2, "BCNSHP"), // Beacon shape
    (3, "BUISHP"), // Building shape
    (4, "BOYSHP"), // Buoy shape
    (5, "BURDEP"), // Buried depth
    (6, "CALSGN"), // Call sign
    (7, "CATAIR"), // Category of airport/airfield
    (8, "CATACH"), // Category of anchorage
    (9, "CATBRG"), // Category of bridge
    (10, "CATBUA"), // Category of built-up area
    (11, "CATCBL"), // Category of cable
    (12, "CATCAN"), // Category of canal
    (13, "CATCAM"), // Category of cardinal mark
    (14, "CATCHP"), // Category of checkpoint
    (15, "CATCOA"), // Category of coastline
    (16, "CATCTR"), // Category of control point
    (17, "CATCON"), // Category of conveyor
    (18, "CATCOV"), // Category of coverage
    (19, "CATCRN"), // Category of crane
    (20, "CATDAM"), // Category of dam
    (21, "CATDIS"), // Category of distance mark
    (22, "CATDOC"), // Category of dock
    (23, "CATDPG"), // Category of dumping ground
    (24, "CATFNC"), // Category of fence/wall
    (25, "CATFRY"), // Category of ferry
    (26, "CATFIF"), // Category of fishing facility
    (27, "CATFOG"), // Category of fog signal
    (28, "CATFOR"), // Category of fortified structure
    (29, "CATGAT"), // Category of gate
    (30, "CATHAF"), // Category of harbour facility
    (31, "CATHLK"), // Category of hulk
    (32, "CATICE"), // Category of ice
    (33, "CATINB"), // Category of installation buoy
    (34, "CATLND"), // Category of land region
    (35, "CATLMK"), // Category of landmark
    (36, "CATLAM"), // Category of lateral mark
    (37, "CATLIT"), // Category of light
    (38, "CATMFA"), // Category of marine farm/culture
    (39, "CATMPA"), // Category of military practice area
    (40, "CATMOR"), // Category of mooring/warping facility
    (41, "CATNAV"), // Category of navigation line
    (42, "CATOBS"), // Category of obstruction
    (43, "CATOFP"), // Category of offshore platform
    (44, "CATOLB"), // Category of oil barrier
    (45, "CATPLE"), // Category of pile
    (46, "CATPIL"), // Category of pilot boarding place
    (47, "CATPIP"), // Category of pipeline / pipe
    (48, "CATPRA"), // Category of production area
    (49, "CATPYL"), // Category of pylon
    (50, "CATQUA"), // Category of quality of data
    (51, "CATRAS"), // Category of radar station
    (52, "CATRTB"), // Category of radar transponder beacon
    (53, "CATROS"), // Category of radio station
    (54, "CATTRK"), // Category of recommended track
    (55, "CATRSC"), // Category of rescue station
    (56, "CATREA"), // Category of restricted area
    (57, "CATROD"), // Category of road
    (58, "CATRUN"), // Category of runway
    (59, "CATSEA"), // Category of sea area
    (60, "CATSLC"), // Category of shoreline construction
    (61, "CATSIT"), // Category of signal station - traffic
    (62, "CATSIW"), // Category of signal station - warning
    (63, "CATSIL"), // Category of silo/tank
    (64, "CATSLO"), // Category of slope
    (65, "CATSCF"), // Category of small craft facility
    (66, "CATSPM"), // Category of special purpose mark
    (67, "CATTSS"), // Category of Traffic Separation Scheme
    (68, "CATVEG"), // Category of vegetation
    (69, "CATWAT"), // Category of water turbulence
    (70, "CATWED"), // Category of weed/kelp
    (71, "CATWRK"), // Category of wreck
    (72, "CATZOC"), // Category of zone of confidence data
    (73, "$SPACE"), // Character spacing
    (74, "$CHARS"), // Character specification
    (75, "COLOUR"), // Colour
    (76, "COLPAT"), // Colour pattern
    (77, "COMCHA"), // Communication channel
    (78, "$CSIZE"), // Compass size
    (79, "CPDATE"), // Compilation date
    (80, "CSCALE"), // Compilation scale
    (81, "CONDTN"), // Condition
    (82, "CONRAD"), // Conspicuous - Radar
    (83, "CONVIS"), // Conspicuous - Visual
    (84, "CURVEL"), // Current velocity
    (85, "DATEND"), // Date end
    (86, "DATSTA"), // Date start
    (87, "DRVAL1"), // Depth range value 1
    (88, "DRVAL2"), // Depth range value 2
    (89, "DUNITS"), // Depth units
    (90, "ELEVAT"), // Elevation
    (91, "ESTRNG"), // Estimated range of transmission
    (92, "EXCLIT"), // Exhibition condition of light
    (93, "EXPSOU"), // Exposition of sounding
    (94, "FUNCTN"), // Function
    (95, "HEIGHT"), // Height
    (96, "HUNITS"), // Height/length units
    (97, "HORACC"), // Horizontal accuracy
    (98, "HORCLR"), // Horizontal clearance
    (99, "HORLEN"), // Horizontal length
    (100, "HORWID"), // Horizontal width
    (101, "ICEFAC"), // Ice factor
    (102, "INFORM"), // Information
    (103, "JRSDTN"), // Jurisdiction
    (104, "$JUSTH"), // Justification - horizontal
    (105, "$JUSTV"), // Justification - vertical
    (106, "LIFCAP"), // Lifting capacity
    (107, "LITCHR"), // Light characteristic
    (108, "LITVIS"), // Light visibility
    (109, "MARSYS"), // Marks navigational - System of
    (110, "MLTYLT"), // Multiplicity of lights
    (111, "NATION"), // Nationality
    (112, "NATCON"), // Nature of construction
    (113, "NATSUR"), // Nature of surface
    (114, "NATQUA"), // Nature of surface - qualifying terms
    (115, "NMDATE"), // Notice to Mariners date
    (116, "OBJNAM"), // Object name
    (117, "ORIENT"), // Orientation
    (118, "PEREND"), // Periodic date end
    (119, "PERSTA"), // Periodic date start
    (120, "PICREP"), // Pictorial representation
    (121, "PILDST"), // Pilot district
    (122, "PRCTRY"), // Producing country
    (123, "PRODCT"), // Product
    (124, "PUBREF"), // Publication reference
    (125, "QUASOU"), // Quality of sounding measurement
    (126, "RADWAL"), // Radar wave length
    (127, "RADIUS"), // Radius
    (128, "RECDAT"), // Recording date
    (129, "RECIND"), // Recording indication
    (130, "RYRMGV"), // Reference year for magnetic variation
    (131, "RESTRN"), // Restriction
    (132, "SCAMAX"), // Scale maximum
    (133, "SCAMIN"), // Scale minimum
    (134, "SCVAL1"), // Scale value one
    (135, "SCVAL2"), // Scale value two
    (136, "SECTR1"), // Sector limit one
    (137, "SECTR2"), // Sector limit two
    (138, "SHIPAM"), // Shift parameters
    (139, "SIGFRQ"), // Signal frequency
    (140, "SIGGEN"), // Signal generation
    (141, "SIGGRP"), // Signal group
    (142, "SIGPER"), // Signal period
    (143, "SIGSEQ"), // Signal sequence
    (144, "SOUACC"), // Sounding accuracy
    (145, "SDISMX"), // Sounding distance - maximum
    (146, "SDISMN"), // Sounding distance - minimum
    (147, "SORDAT"), // Source date
    (148, "SORIND"), // Source indication
    (149, "STATUS"), // Status
    (150, "SURATH"), // Survey authority
    (151, "SUREND"), // Survey date - end
    (152, "SURSTA"), // Survey date - start
    (153, "SURTYP"), // Survey type
    (154, "$SCALE"), // Symbol scaling factor
    (155, "$SCODE"), // Symbolization code
    (156, "TECSOU"), // Technique of sounding measurement
    (157, "$TXSTR"), // Text string
    (158, "TXTDSC"), // Textual description
    (159, "TS_TSP"), // Tidal stream - panel values
    (160, "TS_TSV"), // Tidal stream current - time series values
    (161, "T_ACWL"), // Tide - accuracy of water level
    (162, "T_HWLW"), // Tide - high and low water values
    (163, "T_MTOD"), // Tide - method of tidal prediction
    (164, "T_THDF"), // Tide - time and height differences
    (165, "T_TINT"), // Tide current - time interval of values
    (166, "T_TSVL"), // Tide - time series values
    (167, "T_VAHC"), // Tide - value of harmonic constituents
    (168, "TIMEND"), // Time end
    (169, "TIMSTA"), // Time start
    (170, "$TINTS"), // Tint
    (171, "TOPSHP"), // Topmark/daymark shape
    (172, "TRAFIC"), // Traffic flow
    (173, "VALACM"), // Value of annual change in magnetic variation
    (174, "VALDCO"), // Value of depth contour
    (175, "VALLMA"), // Value of local magnetic anomaly
    (176, "VALMAG"), // Value of magnetic variation
    (177, "VALMXR"), // Value of maximum range
    (178, "VALNMR"), // Value of nominal range
    (179, "VALSOU"), // Value of sounding
    (180, "VERACC"), // Vertical accuracy
    (181, "VERCLR"), // Vertical clearance
    (182, "VERCCL"), // Vertical clearance - closed
    (183, "VERCOP"), // Vertical clearance - open
    (184, "VERCSA"), // Vertical clearance - safe
    (185, "VERDAT"), // Vertical datum
    (186, "VERLEN"), // Vertical length
    (187, "WATLEV"), // Water level effect
    (188, "CAT_TS"), // Category of Tidal stream
    (189, "PUNITS"), // Positional accuracy units
    (190, "CLSDEF"), // Object class definition
    (191, "CLSNAM"), // Object class name
    (192, "SYMINS"), // Symbol instruction
    (300, "NINFOM"), // Information in national language
    (301, "NOBJNM"), // Object name in national language
    (302, "NPLDST"), // Pilot district in national language
    (303, "$NTXST"), // Text string in national language
    (304, "NTXTDS"), // Textual description in national language
    (400, "HORDAT"), // Horizontal datum
    (401, "POSACC"), // Positional Accuracy
    (402, "QUAPOS"), // Quality of position
    (17000, "catach"), // Category of anchorage
    (17001, "catdis"), // Category of distance mark
    (17002, "catsit"), // Category of signal station. traffic
    (17003, "catsiw"), // Category of signal station. warning
    (17004, "restrn"), // Restriction
    (17005, "verdat"), // Vertical datum
    (17006, "catbrg"), // Category of bridge
    (17007, "catfry"), // Category of ferry
    (17008, "cathaf"), // Category of harbour facility
    (17009, "marsys"), // Marks navigational - System of
    (17010, "catchp"), // Category of checkpoint
    (17011, "catlam"), // Category of lateral mark
    (17012, "catslc"), // Category of shoreline construction
    (17050, "addmrk"), // additional mark
    (17051, "catbnk"), // Category of bank
    (17052, "catnmk"), // category of notice mark
    (17055, "clsdng"), // class of dangerous cargo
    (17056, "dirimp"), // direction of impact
    (17057, "disbk1"), // Distance from notice mark. first
    (17058, "disbk2"), // Distance from notice mark. second
    (17059, "disipu"), // Distance of impact. upstream
    (17060, "disipd"), // Distance of impact. downstream
    (17061, "eleva1"), // Elevation 1 of surface (m)
    (17062, "eleva2"), // Elevation 2 of surface (m)
    (17063, "fnctnm"), // Function of notice mark
    (17064, "wtwdis"), // waterway distance
    (17065, "bunves"), // bunker vessel. availability
    (17066, "catbrt"), // category of berth
    (17067, "catbun"), // category of bunker station
    (17068, "catccl"), // category of CEMT class
    (17069, "catcom"), // Category of communication
    (17070, "cathbr"), // category of harbour area
    (17071, "catrfd"), // category of refuse dump
    (17072, "cattml"), // Category of terminal
    (17073, "comctn"), // Communication
    (17074, "horcll"), // Horizontal clearance length
    (17075, "horclw"), // Horizontal clearance width
    (17076, "trshgd"), // transshipping goods
    (17077, "unlocd"), // UN location code
    (17078, "catgag"), // Category of waterway gauge
    (17080, "higwat"), // Value at relevant high water level
    (17081, "hignam"), // Name of relevant high water level
    (17082, "lowwat"), // Value at relevant low water level
    (17083, "lownam"), // Name of relevant low water level
    (17084, "meawat"), // Value at relevant mean water level
    (17085, "meanam"), // Name of relevant mean water level
    (17086, "othwat"), // Value at other locally relevant water level
    (17087, "othnam"), // Name of other locally relevant water level
    (17088, "reflev"), // Reference gravitational level
    (17089, "sdrlev"), // Name of Sounding datum reference level
    (17090, "vcrlev"), // Name of vertical river datum reference level
    (17091, "catvtr"), // Category of vehicle transfer
    (17092, "cattab"), // Category of time and behaviour
    (17093, "schref"), // Time Schedule Reference
    (17094, "useshp"), // Use of Ship
    (17095, "curvhw"), // Current velocity at high water level
    (17096, "curvlw"), // Current velocity at low water level
    (17097, "curvmw"), // Current velocity at mean water level
    (17098, "curvow"), // Current velocity at other water level
    (17099, "aptref"), // Average Passing Time Reference
    (17100, "catexs"), // Category of exceptional structure
    (17101, "catcbl"), // Category of cable
    (17102, "cathlk"), // Category of hulk
    (17103, "hunits"), // Height/length units
    (17104, "watlev"), // Water level effect
    (17112, "catwwm"), // Category of waterway mark
    (18001, "lg_spd"), // Maximal permitted speed
    (18002, "lg_spr"), // speed reference
    (18003, "lg_bme"), // Maximal permitted beam
    (18004, "lg_lgs"), // Maximal permitted length
    (18005, "lg_drt"), // Maximal permitted draught
    (18006, "lg_wdp"), // Maximal permitted water displacement
    (18007, "lg_wdu"), // water displacement unit
    (18008, "lg_rel"), // related issue
    (18009, "lg_fnc"), // Function of legal conditions
    (18010, "lg_des"), // Description of legal conditions
    (18011, "lg_pbr"), // Publication reference
    (18012, "lc_csi"), // category of ship (including)
    (18013, "lc_cse"), // category of ship (excluding)
    (18014, "lc_asi"), // Assemblies of ship (including)
    (18015, "lc_ase"), // Assemblies of ship (excluding)
    (18016, "lc_cci"), // Category of cargo (including)
    (18017, "lc_cce"), // Category of cargo (excluding)
    (18018, "lc_bm1"), // Beam range value 1
    (18019, "lc_bm2"), // Beam range value 2
    (18020, "lc_lg1"), // Length range value 1
    (18021, "lc_lg2"), // Length range value 2
    (18022, "lc_dr1"), // Draught range value 1
    (18023, "lc_dr2"), // Draught range value 2
    (18024, "lc_sp1"), // Speed range value 1
    (18025, "lc_sp2"), // Speed range value 2
    (18026, "lc_wd1"), // Water displacement range value 1
    (18027, "lc_wd2"), // Water displacement value 2
    (22031, "ANATR1"), // Annotation Attribute 1
    (22032, "ANATR2"), // Annotation Attribute 2
    (22033, "ANATR3"), // Annotation Attribute 3
    (22034, "ANATR4"), // Annotation Attribute 4
    (22035, "ANATR5"), // Annotation Attribute 5
    (22036, "ANATR6"), // Annotation Attribute 6
    (22037, "ANATR7"), // Annotation Attribute 7
    (22038, "ANATR8"), // Annotation Attribute 8
    (22039, "ANATR9"), // Annotation Attribute 9
    (22040, "ANATRA"), // Annotation Attribute a
    (22076, "ANTXT1"), // Annotation Text
    (22135, "ANLYR1"), // Annotation Layer
    (22227, "NEWTY1"), // New type 1
    (33066, "shptyp"), // Type of Ship
    (40000, "updmsg"), // Update message
    (50000, "catgeo"), // Geometry Primitive Category
];

/// The same table ordered by acronym, built once.
///
/// Derived rather than written out a second time: two hand-maintained tables
/// can disagree, and this one cannot.
fn s57_attributes_by_name() -> &'static [(&'static str, u16)] {
    static BY_NAME: std::sync::OnceLock<Vec<(&'static str, u16)>> = std::sync::OnceLock::new();
    BY_NAME.get_or_init(|| {
        let mut v: Vec<(&'static str, u16)> =
            S57_ATTRIBUTES.iter().map(|(c, n)| (*n, *c)).collect();
        v.sort_unstable_by_key(|(n, _)| *n);
        v
    })
}

/// Acronym for an S-57 attribute code, or `None` if Manx has no name for it.
pub fn s57_attribute_name(code: u16) -> Option<&'static str> {
    S57_ATTRIBUTES
        .binary_search_by_key(&code, |(c, _)| *c)
        .ok()
        .map(|i| S57_ATTRIBUTES[i].1)
}

/// Code for an S-57 attribute acronym, or `None` if it is not in the table.
///
/// The table is sorted by acronym so this is a binary search: six string
/// comparisons, no allocation and no hashing, against a `HashMap<String, _>`
/// that had to allocate the key at parse time and hash it at every lookup.
///
/// Returning `None` is the same answer Manx gave before: an attribute the
/// table does not name was stored under a placeholder key that no lookup could
/// match either.
pub fn s57_attribute_code(name: &str) -> Option<u16> {
    let by_name = s57_attributes_by_name();
    by_name
        .binary_search_by(|(n, _)| (*n).cmp(name))
        .ok()
        .map(|i| by_name[i].1)
}

/// A feature's S-57 attributes, keyed by code.
///
/// A `HashMap<String, AttributeValue>` cost an allocated key per attribute and
/// a `HashMap` per feature — for a 3800-feature cell, tens of thousands of
/// allocations to parse and a string hash on every lookup afterwards. The SENC
/// hands us a numeric code; keeping it is both smaller and faster, and a
/// feature carries so few attributes that a linear scan beats any index.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Attributes(Vec<(u16, AttributeValue)>);

impl Attributes {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Look up by S-57 acronym. Unknown acronyms have no value, as before.
    pub fn get(&self, name: &str) -> Option<&AttributeValue> {
        self.get_code(s57_attribute_code(name)?)
    }

    pub fn get_code(&self, code: u16) -> Option<&AttributeValue> {
        self.0.iter().find(|(c, _)| *c == code).map(|(_, v)| v)
    }

    /// Set an attribute by acronym.
    ///
    /// An acronym missing from [`S57_ATTRIBUTES`] cannot be stored, because
    /// nothing could look it up again; that is a bug in the caller rather than
    /// a runtime condition, so it trips a debug assertion.
    pub fn insert(&mut self, name: &str, value: AttributeValue) {
        match s57_attribute_code(name) {
            Some(code) => self.insert_code(code, value),
            None => debug_assert!(false, "unknown S-57 attribute acronym: {name}"),
        }
    }

    pub fn insert_code(&mut self, code: u16, value: AttributeValue) {
        match self.0.iter_mut().find(|(c, _)| *c == code) {
            Some(slot) => slot.1 = value,
            None => self.0.push((code, value)),
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Attributes as (code, value) pairs, in the order the cell listed them.
    pub fn iter_codes(&self) -> impl Iterator<Item = (u16, &AttributeValue)> {
        self.0.iter().map(|(c, v)| (*c, v))
    }

    /// Attributes Manx has a name for, as (acronym, value). Anything the
    /// table does not name is skipped — it was unreachable by name anyway.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &AttributeValue)> {
        self.0
            .iter()
            .filter_map(|(c, v)| s57_attribute_name(*c).map(|n| (n, v)))
    }
}

impl<'a> IntoIterator for &'a Attributes {
    type Item = (&'static str, &'a AttributeValue);
    type IntoIter = Box<dyn Iterator<Item = (&'static str, &'a AttributeValue)> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

/// Type of geometry a feature contains
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeatureType {
    Point,
    Line,
    Area,
    Multipoint,
}

/// A parsed S-57 feature with geometry
#[derive(Debug, Clone)]
pub struct Feature {
    /// Feature type code from S-57
    pub type_code: u16,
    /// S-57 object class (LNDARE, DEPARE, etc.)
    pub object_class: ObjectClass,
    /// Geometry type
    pub feature_type: FeatureType,
    /// Parsed attributes, keyed by S-57 attribute code
    pub attributes: Attributes,
    /// Area geometry (if feature_type == Area)
    pub area_geometry: Option<AreaGeometry>,
    /// Point geometry (if feature_type == Point)
    pub point_geometry: Option<PointGeometry>,
    /// Line geometry (if feature_type == Line)
    pub line_geometry: Option<LineGeometry>,
    /// Multipoint geometry (if feature_type == Multipoint, e.g., SOUNDG)
    pub multipoint_geometry: Option<MultipointGeometry>,
}

/// Attribute value types
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    Integer(i32),
    Float(f64),
    String(String),
}

impl Feature {
    /// Get depth value 1 (shallow limit) for DEPARE
    pub fn drval1(&self) -> Option<f64> {
        match self.attributes.get("DRVAL1") {
            Some(AttributeValue::Float(v)) => Some(*v),
            Some(AttributeValue::Integer(v)) => Some(*v as f64),
            _ => None,
        }
    }

    /// Get depth value 2 (deep limit) for DEPARE
    pub fn drval2(&self) -> Option<f64> {
        match self.attributes.get("DRVAL2") {
            Some(AttributeValue::Float(v)) => Some(*v),
            Some(AttributeValue::Integer(v)) => Some(*v as f64),
            _ => None,
        }
    }

    /// Average depth for coloring
    pub fn avg_depth(&self) -> Option<f64> {
        match (self.drval1(), self.drval2()) {
            (Some(d1), Some(d2)) => Some((d1 + d2) / 2.0),
            (Some(d), None) | (None, Some(d)) => Some(d),
            _ => None,
        }
    }

    /// Get an integer attribute by name
    pub fn attribute_int(&self, name: &str) -> Option<i32> {
        match self.attributes.get(name) {
            Some(AttributeValue::Integer(v)) => Some(*v),
            Some(AttributeValue::Float(v)) => Some(*v as i32),
            _ => None,
        }
    }

    /// Get a float attribute by name.
    ///
    /// Accepts Float, Integer, and numeric Strings. Some SENC encoders stringify
    /// float attributes (e.g. SECTR1/SECTR2 in lights); fall back to `str::parse`
    /// so sector arcs and similar features don't silently disappear.
    pub fn attribute_float(&self, name: &str) -> Option<f64> {
        match self.attributes.get(name) {
            Some(AttributeValue::Float(v)) => Some(*v),
            Some(AttributeValue::Integer(v)) => Some(*v as f64),
            Some(AttributeValue::String(s)) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    }

    /// Get a string attribute by name
    pub fn attribute_str(&self, name: &str) -> Option<&str> {
        match self.attributes.get(name) {
            Some(AttributeValue::String(s)) => Some(s.as_str()),
            // Also handle integer/float as strings for comma-separated lists
            Some(AttributeValue::Integer(_v)) => None, // Can't return reference to temp
            Some(AttributeValue::Float(_)) => None,
            None => None,
        }
    }

    /// Get attributes as a map of integer values (for S-52 lookup matching).
    ///
    /// Returns attribute name → list of integer values. String attributes are
    /// parsed as comma-separated lists. Non-numeric strings are stored as empty
    /// lists to preserve attribute presence checks.
    pub fn attributes_as_map(&self) -> HashMap<&str, Vec<i32>> {
        let mut map = HashMap::new();
        for (name, value) in self.attributes.iter() {
            match value {
                AttributeValue::Integer(v) => {
                    map.insert(name, vec![*v]);
                }
                AttributeValue::Float(v) => {
                    map.insert(name, vec![*v as i32]);
                }
                AttributeValue::String(s) => {
                    let mut values = Vec::new();
                    for part in s.split(',') {
                        if let Ok(v) = part.trim().parse::<i32>() {
                            values.push(v);
                        }
                    }
                    map.insert(name, values);
                }
            }
        }
        map
    }

    /// Get SCAMIN (scale minimum) - the smallest scale at which feature should display.
    /// Scale is denominator of 1:N, so larger SCAMIN = feature hidden at higher zoom levels.
    /// Returns None if attribute not present (feature always visible).
    pub fn scamin(&self) -> Option<f64> {
        match self.attributes.get("SCAMIN") {
            Some(AttributeValue::Integer(v)) => Some(*v as f64),
            Some(AttributeValue::Float(v)) => Some(*v),
            _ => None,
        }
    }

    /// Get SCAMAX (scale maximum) - the largest scale at which feature should display.
    /// Returns None if attribute not present (no upper limit).
    pub fn scamax(&self) -> Option<f64> {
        match self.attributes.get("SCAMAX") {
            Some(AttributeValue::Integer(v)) => Some(*v as f64),
            Some(AttributeValue::Float(v)) => Some(*v),
            _ => None,
        }
    }

    /// Check if this is land
    pub fn is_land(&self) -> bool {
        self.object_class == ObjectClass::LandArea
    }

    /// Check if this is a depth area
    pub fn is_depth_area(&self) -> bool {
        self.object_class == ObjectClass::DepthArea || self.object_class == ObjectClass::DredgedArea
    }

    /// Check if this is a depth contour
    pub fn is_depth_contour(&self) -> bool {
        self.object_class == ObjectClass::DepthContour
    }

    /// Check if this is a coastline
    pub fn is_coastline(&self) -> bool {
        self.object_class == ObjectClass::Coastline
    }

    /// Check if this is shoreline construction (piers, jetties, seawalls)
    pub fn is_shoreline_construction(&self) -> bool {
        self.object_class == ObjectClass::ShorelineConstruction
    }

    /// Check if this is a road
    pub fn is_road(&self) -> bool {
        self.object_class == ObjectClass::Road
    }

    /// Check if this is an overhead cable
    pub fn is_cable_overhead(&self) -> bool {
        self.object_class == ObjectClass::CableOverhead
    }

    /// Check if this is a submarine cable
    pub fn is_cable_submarine(&self) -> bool {
        self.object_class == ObjectClass::CableSubmarine
    }

    /// Check if this is any type of cable
    pub fn is_cable(&self) -> bool {
        matches!(
            self.object_class,
            ObjectClass::CableOverhead | ObjectClass::CableSubmarine
        )
    }

    /// Check if this is a traffic separation line
    pub fn is_traffic_separation(&self) -> bool {
        self.object_class == ObjectClass::TrafficSeparationLine
    }

    /// Check if this is a recommended route
    pub fn is_recommended_route(&self) -> bool {
        self.object_class == ObjectClass::RecommendedRoute
    }

    /// Check if this is a coverage meta feature
    pub fn is_coverage(&self) -> bool {
        self.object_class == ObjectClass::Coverage
    }

    /// Check if this is a sounding (SOUNDG)
    pub fn is_sounding(&self) -> bool {
        self.object_class == ObjectClass::Sounding
    }

    /// Get depth contour value (VALDCO attribute)
    pub fn valdco(&self) -> Option<f64> {
        match self.attributes.get("VALDCO") {
            Some(AttributeValue::Float(v)) => Some(*v),
            Some(AttributeValue::Integer(v)) => Some(*v as f64),
            _ => None,
        }
    }
}

/// Complete parsed chart data
#[derive(Debug)]
pub struct ChartData {
    /// SENC header info
    pub header: SencHeader,
    /// All parsed features
    pub features: Vec<Feature>,
    /// Edge table for resolving line geometry
    pub edge_table: EdgeTable,
}

impl ChartData {
    /// Parse chart data from decrypted SENC bytes
    pub fn parse(data: Vec<u8>) -> Result<Self, SencError> {
        let mut reader = SencReader::from_bytes(data);
        let (header, overflow) = reader.read_header()?;
        let senc_version = header.version;

        let mut features = Vec::new();
        let mut current_feature: Option<PartialFeature> = None;
        let mut edge_table = EdgeTable::new();

        // Helper closure to process a single record
        let process_record = |record: &RawRecord,
                              current_feature: &mut Option<PartialFeature>,
                              features: &mut Vec<Feature>,
                              edge_table: &mut EdgeTable|
         -> Result<(), SencError> {
            match record.record_type {
                RecordType::FeatureId => {
                    // Save previous feature if complete
                    if let Some(partial) = current_feature.take() {
                        if let Some(feature) = partial.finalize() {
                            features.push(feature);
                        }
                    }

                    // Start new feature
                    *current_feature = Some(PartialFeature::from_record(record)?);
                }
                RecordType::FeatureAttribute => {
                    if let Some(ref mut partial) = current_feature {
                        partial.add_attribute(record)?;
                    }
                }
                RecordType::AreaGeometry => {
                    if let Some(ref mut partial) = current_feature {
                        partial.set_area_geometry(record, senc_version)?;
                    }
                }
                RecordType::PointGeometry => {
                    if let Some(ref mut partial) = current_feature {
                        partial.set_point_geometry(record)?;
                    }
                }
                RecordType::LineGeometry => {
                    if let Some(ref mut partial) = current_feature {
                        partial.set_line_geometry(record, senc_version)?;
                    }
                }
                RecordType::MultipointGeometry => {
                    if let Some(ref mut partial) = current_feature {
                        if let Err(e) = partial.set_multipoint_geometry(record) {
                            log::warn!("WARN: Multipoint geometry parse error: {}", e);
                        }
                    }
                }
                RecordType::VectorEdge => {
                    // Edge table record contains multiple edges
                    if let Err(e) = edge_table.parse_edge_table(&record.payload) {
                        log::warn!("WARN: Edge table parse error: {}", e);
                    }
                }
                RecordType::VectorConnectedNode => {
                    // Node table record contains multiple nodes
                    if let Err(e) = edge_table.parse_node_table(&record.payload) {
                        log::warn!("WARN: Node table parse error: {}", e);
                    }
                }
                _ => {}
            }
            Ok(())
        };

        // Process the overflow record from read_header (first non-header record)
        if let Some(ref record) = overflow {
            process_record(record, &mut current_feature, &mut features, &mut edge_table)?;
        }

        // Parse remaining records
        while let Some(record) = reader.next_record()? {
            process_record(
                &record,
                &mut current_feature,
                &mut features,
                &mut edge_table,
            )?;
        }

        // Don't forget the last feature
        if let Some(partial) = current_feature {
            if let Some(feature) = partial.finalize() {
                features.push(feature);
            }
        }

        log::info!(
            "Parsed '{}': {} features, {} edges, {} nodes",
            header.cell_name,
            features.len(),
            edge_table.edge_count(),
            edge_table.node_count(),
        );

        Ok(Self {
            header,
            features,
            edge_table,
        })
    }

    /// Get all land area features
    pub fn land_areas(&self) -> impl Iterator<Item = &Feature> {
        self.features.iter().filter(|f| f.is_land())
    }

    /// Get all depth area features
    pub fn depth_areas(&self) -> impl Iterator<Item = &Feature> {
        self.features.iter().filter(|f| f.is_depth_area())
    }

    /// Get all area features (for rendering)
    pub fn areas(&self) -> impl Iterator<Item = &Feature> {
        self.features
            .iter()
            .filter(|f| f.feature_type == FeatureType::Area)
    }

    /// Get all line features (for rendering)
    pub fn lines(&self) -> impl Iterator<Item = &Feature> {
        self.features
            .iter()
            .filter(|f| f.feature_type == FeatureType::Line)
    }

    /// Get all point features (for symbol rendering)
    pub fn points(&self) -> impl Iterator<Item = &Feature> {
        self.features
            .iter()
            .filter(|f| f.feature_type == FeatureType::Point)
    }

    /// Get all depth contour features
    pub fn depth_contours(&self) -> impl Iterator<Item = &Feature> {
        self.features.iter().filter(|f| f.is_depth_contour())
    }

    /// Get all coastline features
    pub fn coastlines(&self) -> impl Iterator<Item = &Feature> {
        self.features.iter().filter(|f| f.is_coastline())
    }

    /// Get all shoreline construction features (piers, jetties, seawalls)
    pub fn shoreline_constructions(&self) -> impl Iterator<Item = &Feature> {
        self.features
            .iter()
            .filter(|f| f.is_shoreline_construction())
    }

    /// Get all road features
    pub fn roads(&self) -> impl Iterator<Item = &Feature> {
        self.features.iter().filter(|f| f.is_road())
    }

    /// Get all cable features (overhead and submarine)
    pub fn cables(&self) -> impl Iterator<Item = &Feature> {
        self.features.iter().filter(|f| f.is_cable())
    }

    /// Get all traffic separation lines
    pub fn traffic_separations(&self) -> impl Iterator<Item = &Feature> {
        self.features.iter().filter(|f| f.is_traffic_separation())
    }

    /// Get all recommended routes
    pub fn recommended_routes(&self) -> impl Iterator<Item = &Feature> {
        self.features.iter().filter(|f| f.is_recommended_route())
    }

    /// Get all sounding features (SOUNDG with multipoint geometry)
    pub fn soundings(&self) -> impl Iterator<Item = &Feature> {
        self.features.iter().filter(|f| f.is_sounding())
    }

    /// Summary stats for debugging
    pub fn summary(&self) -> String {
        let land_count = self.land_areas().count();
        let depth_count = self.depth_areas().count();
        let area_count = self.areas().count();
        let line_count = self.lines().count();
        let point_count = self.points().count();
        let contour_count = self.depth_contours().count();
        let coastline_count = self.coastlines().count();
        let sounding_count = self.soundings().count();
        let multipoint_count = self
            .features
            .iter()
            .filter(|f| f.feature_type == FeatureType::Multipoint)
            .count();
        let total = self.features.len();

        format!(
            "Chart '{}' (scale 1:{}): {} features total\n\
             - Land areas: {}\n\
             - Depth areas: {}\n\
             - All areas: {}\n\
             - All lines: {}\n\
             - All points: {}\n\
             - Soundings (SOUNDG): {}\n\
             - Multipoint features: {}\n\
             - Depth contours: {}\n\
             - Coastlines: {}\n\
             - Edge table: {} edges, {} nodes",
            self.header.cell_name,
            self.header.native_scale,
            total,
            land_count,
            depth_count,
            area_count,
            line_count,
            point_count,
            sounding_count,
            multipoint_count,
            contour_count,
            coastline_count,
            self.edge_table.edge_count(),
            self.edge_table.node_count()
        )
    }
}

/// Feature being built from multiple records
struct PartialFeature {
    type_code: u16,
    object_class: ObjectClass,
    feature_type: Option<FeatureType>,
    attributes: Attributes,
    area_geometry: Option<AreaGeometry>,
    point_geometry: Option<PointGeometry>,
    line_geometry: Option<LineGeometry>,
    multipoint_geometry: Option<MultipointGeometry>,
}

impl PartialFeature {
    fn from_record(record: &RawRecord) -> Result<Self, SencError> {
        let payload = &record.payload;

        // OSENC Feature ID record payload format (from OpenCPN Osenc.h):
        // - 2 bytes: feature_type_code (u16) - S-57 object class code
        // - 2 bytes: feature_ID (u16)
        // - 1 byte:  feature_primitive (u8) - 1=point, 2=line, 3=area, 4=multipoint
        // Total: 5 bytes minimum

        if payload.len() < 5 {
            return Err(SencError::Format(format!(
                "FeatureId payload too short: {} bytes (need 5)",
                payload.len()
            )));
        }

        let type_code = u16::from_le_bytes([payload[0], payload[1]]);
        // let feature_id = u16::from_le_bytes([payload[2], payload[3]]);
        let primitive = payload[4];

        // Convert S-57 numeric code to ObjectClass
        let object_class = ObjectClass::from_code(type_code);

        // Determine feature type from primitive byte
        let feature_type = match primitive {
            1 => Some(FeatureType::Point),
            2 => Some(FeatureType::Line),
            3 => Some(FeatureType::Area),
            4 => Some(FeatureType::Multipoint),
            _ => None,
        };

        Ok(Self {
            type_code,
            object_class,
            feature_type,
            attributes: Attributes::new(),
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
            multipoint_geometry: None,
        })
    }

    fn add_attribute(&mut self, record: &RawRecord) -> Result<(), SencError> {
        let payload = &record.payload;

        // OSENC Attribute record payload format (from OpenCPN Osenc.h):
        // - 2 bytes: attribute_type (u16) - S-57 attribute code
        // - 1 byte:  attribute_value_type (u8):
        //   0 = Integer (4 bytes)
        //   1 = Integer List (N × 4 bytes) - skip for now
        //   2 = Double (8 bytes)
        //   3 = Double List (N × 8 bytes) - skip for now
        //   4 = String (null-terminated UTF-8)
        // - variable: value data

        if payload.len() < 3 {
            return Ok(()); // Skip invalid attributes
        }

        let attr_code = u16::from_le_bytes([payload[0], payload[1]]);
        let value_type = payload[2];


        let value_start = 3;
        let value = match value_type {
            0 => {
                // Integer (4 bytes)
                if payload.len() >= value_start + 4 {
                    let v = i32::from_le_bytes([
                        payload[value_start],
                        payload[value_start + 1],
                        payload[value_start + 2],
                        payload[value_start + 3],
                    ]);
                    AttributeValue::Integer(v)
                } else {
                    return Ok(());
                }
            }
            2 => {
                // Double (8 bytes)
                if payload.len() >= value_start + 8 {
                    let mut cursor = Cursor::new(&payload[value_start..]);
                    let v = cursor.read_f64::<LittleEndian>().unwrap_or(0.0);
                    AttributeValue::Float(v)
                } else {
                    return Ok(());
                }
            }
            4 => {
                // String (null-terminated UTF-8)
                let str_bytes = &payload[value_start..];
                let end = str_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(str_bytes.len());
                let s = String::from_utf8_lossy(&str_bytes[..end]).to_string();
                AttributeValue::String(s)
            }
            _ => return Ok(()),
        };

        self.attributes.insert_code(attr_code, value);
        Ok(())
    }

    fn set_area_geometry(
        &mut self,
        record: &RawRecord,
        senc_version: u16,
    ) -> Result<(), SencError> {
        self.feature_type = Some(FeatureType::Area);
        self.area_geometry = Some(
            AreaGeometry::parse(&record.payload, senc_version)
                .map_err(|e| SencError::Format(e.to_string()))?,
        );
        Ok(())
    }

    fn set_point_geometry(&mut self, record: &RawRecord) -> Result<(), SencError> {
        self.feature_type = Some(FeatureType::Point);
        self.point_geometry = Some(
            PointGeometry::parse(&record.payload).map_err(|e| SencError::Format(e.to_string()))?,
        );
        Ok(())
    }

    fn set_line_geometry(
        &mut self,
        record: &RawRecord,
        senc_version: u16,
    ) -> Result<(), SencError> {
        self.feature_type = Some(FeatureType::Line);
        self.line_geometry = Some(
            LineGeometry::parse(&record.payload, senc_version)
                .map_err(|e| SencError::Format(e.to_string()))?,
        );
        Ok(())
    }

    fn set_multipoint_geometry(&mut self, record: &RawRecord) -> Result<(), SencError> {
        self.feature_type = Some(FeatureType::Multipoint);
        self.multipoint_geometry = Some(
            MultipointGeometry::parse(&record.payload)
                .map_err(|e| SencError::Format(e.to_string()))?,
        );
        Ok(())
    }

    fn finalize(self) -> Option<Feature> {
        let feature_type = self.feature_type?;

        Some(Feature {
            type_code: self.type_code,
            object_class: self.object_class,
            feature_type,
            attributes: self.attributes,
            area_geometry: self.area_geometry,
            point_geometry: self.point_geometry,
            line_geometry: self.line_geometry,
            multipoint_geometry: self.multipoint_geometry,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_table_is_sorted_and_bijective() {
        // `s57_attribute_code` binary-searches, so the table must stay sorted
        // by acronym; and the two directions must agree, because a feature is
        // stored by code and every lookup arrives as a name.
        for pair in S57_ATTRIBUTES.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{} then {}", pair[0].0, pair[1].0);
        }
        for pair in s57_attributes_by_name().windows(2) {
            assert!(pair[0].0 < pair[1].0, "{} then {}", pair[0].0, pair[1].0);
        }
        assert_eq!(S57_ATTRIBUTES.len(), s57_attributes_by_name().len());
        for &(code, name) in S57_ATTRIBUTES {
            assert_eq!(s57_attribute_code(name), Some(code), "{name}");
            assert_eq!(s57_attribute_name(code), Some(name), "{code}");
        }
        assert_eq!(s57_attribute_code("NOSUCH"), None);
        assert_eq!(s57_attribute_name(60000), None);
    }

    #[test]
    fn attributes_written_by_cs_procedures_are_in_the_table() {
        // `Attributes::insert` cannot store an acronym the table does not know,
        // so anything a conditional-symbology procedure synthesises has to be
        // there or it would vanish silently.
        for name in [
            "COLOUR", "VALNMR", "CATLIT", "SECTR1", "SECTR2", "LITVIS", "ORIENT",
        ] {
            assert!(s57_attribute_code(name).is_some(), "{name} missing");
        }
    }

    #[test]
    fn attributes_round_trip_by_name_and_code() {
        let mut a = Attributes::new();
        a.insert("DRVAL1", AttributeValue::Float(3.5));
        a.insert_code(s57_attribute_code("SCAMIN").unwrap(), AttributeValue::Integer(45000));
        assert_eq!(a.get("DRVAL1"), Some(&AttributeValue::Float(3.5)));
        assert_eq!(a.get("SCAMIN"), Some(&AttributeValue::Integer(45000)));
        assert_eq!(a.get("DRVAL2"), None);
        assert_eq!(a.len(), 2);
        // Re-inserting replaces rather than duplicating.
        a.insert("DRVAL1", AttributeValue::Float(9.0));
        assert_eq!(a.len(), 2);
        assert_eq!(a.get("DRVAL1"), Some(&AttributeValue::Float(9.0)));
        let names: Vec<&str> = a.iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["DRVAL1", "SCAMIN"]);
    }

    #[test]
    fn maps_light_attributes_needed_for_open_cpn_portrayal() {
        assert_eq!(s57_attribute_name(37), Some("CATLIT"));
        assert_eq!(s57_attribute_name(95), Some("HEIGHT"));
        assert_eq!(s57_attribute_name(107), Some("LITCHR"));
        assert_eq!(s57_attribute_name(108), Some("LITVIS"));
        assert_eq!(s57_attribute_name(117), Some("ORIENT"));
        assert_eq!(s57_attribute_name(136), Some("SECTR1"));
        assert_eq!(s57_attribute_name(137), Some("SECTR2"));
        assert_eq!(s57_attribute_name(141), Some("SIGGRP"));
        assert_eq!(s57_attribute_name(142), Some("SIGPER"));
        assert_eq!(s57_attribute_name(178), Some("VALNMR"));
    }

    #[test]
    fn maps_cs_procedure_attributes() {
        // Codes verified against doc/reference projects/OpenCPN/data/s57data/s57attributes.csv
        assert_eq!(s57_attribute_name(71), Some("CATWRK"));  // wrecks.rs
        assert_eq!(s57_attribute_name(42), Some("CATOBS"));  // obstrn.rs
        assert_eq!(s57_attribute_name(171), Some("TOPSHP")); // topmar.rs
        assert_eq!(s57_attribute_name(125), Some("QUASOU")); // sndfrm.rs, wrecks.rs
        assert_eq!(s57_attribute_name(156), Some("TECSOU")); // sndfrm.rs
        assert_eq!(s57_attribute_name(149), Some("STATUS")); // sndfrm.rs
        assert_eq!(s57_attribute_name(131), Some("RESTRN")); // restrn.rs, resare.rs
        assert_eq!(s57_attribute_name(93), Some("EXPSOU"));  // obstrn.rs, wrecks.rs
        assert_eq!(s57_attribute_name(82), Some("CONRAD"));  // qualin.rs, quapos.rs
        assert_eq!(s57_attribute_name(56), Some("CATREA"));  // LUP matching
    }

    #[test]
    fn maps_buoy_beacon_shape_attributes() {
        assert_eq!(s57_attribute_name(2), Some("BCNSHP"));
        assert_eq!(s57_attribute_name(4), Some("BOYSHP"));
        assert_eq!(s57_attribute_name(13), Some("CATCAM"));
    }

    #[test]
    fn maps_vertical_clearance_attributes() {
        assert_eq!(s57_attribute_name(181), Some("VERCLR"));
        assert_eq!(s57_attribute_name(182), Some("VERCCL"));
        assert_eq!(s57_attribute_name(183), Some("VERCOP"));
    }
}
