//! SENC TLV record type definitions.
//!
//! Based on OpenCPN Osenc.h and the navcore_plan_v2.md documentation.

/// SENC record types (from OpenCPN Osenc.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum RecordType {
    /// Version header (record_type=1, length=8)
    /// Payload: u16 version (expected: 201)
    Header = 1,

    /// Cell name (record_type=2)
    CellName = 2,

    /// Cell publish date (record_type=3)
    CellPublishDate = 3,

    /// Cell edition (record_type=4)
    CellEdition = 4,

    /// Cell update date (record_type=5)
    CellUpdateDate = 5,

    /// Cell update (record_type=6)
    CellUpdate = 6,

    /// Cell native scale (record_type=7)
    CellNativeScale = 7,

    /// SENC creation date (record_type=8)
    CellSencCreateDate = 8,

    /// Sounding datum (record_type=9)
    CellSoundingDatum = 9,

    /// Cell extent (bounding box) (record_type=100, from OpenCPN CELL_EXTENT_RECORD)
    CellExtent = 100,

    /// Feature ID record (record_type=64)
    /// Marks the start of a new feature
    FeatureId = 64,

    /// Feature attribute record (record_type=65)
    /// Key-value pair for current feature
    FeatureAttribute = 65,

    /// Point geometry (record_type=80)
    PointGeometry = 80,

    /// Line geometry (record_type=81)
    LineGeometry = 81,

    /// Area geometry (record_type=82)
    /// Contains pre-triangulated data for rendering
    AreaGeometry = 82,

    /// Multipoint geometry (record_type=83)
    MultipointGeometry = 83,

    /// Vector edge (record_type=96)
    /// Edge table entry for topology
    VectorEdge = 96,

    /// Vector connected node (record_type=97)
    VectorConnectedNode = 97,

    /// Area geometry extended (record_type=84)
    AreaGeometryExt = 84,

    /// Vector edge extended (record_type=85)
    VectorEdgeExt = 85,

    /// Vector connected node extended (record_type=86)
    VectorConnectedNodeExt = 86,

    /// Cell coverage (record_type=98)
    CellCoverage = 98,

    /// Cell no-coverage (record_type=99)
    CellNoCoverage = 99,

    /// Cell text description info (record_type=101)
    CellTextDescInfo = 101,

    /// Server status (record_type=200)
    /// Contains decrypt_status and expire_status
    ServerStatus = 200,

    /// Unknown record type
    Unknown = 0xFFFF,
}

impl From<u16> for RecordType {
    fn from(value: u16) -> Self {
        match value {
            1 => RecordType::Header,
            2 => RecordType::CellName,
            3 => RecordType::CellPublishDate,
            4 => RecordType::CellEdition,
            5 => RecordType::CellUpdateDate,
            6 => RecordType::CellUpdate,
            7 => RecordType::CellNativeScale,
            8 => RecordType::CellSencCreateDate,
            9 => RecordType::CellSoundingDatum,
            100 => RecordType::CellExtent,
            64 => RecordType::FeatureId,
            65 => RecordType::FeatureAttribute,
            80 => RecordType::PointGeometry,
            81 => RecordType::LineGeometry,
            82 => RecordType::AreaGeometry,
            83 => RecordType::MultipointGeometry,
            84 => RecordType::AreaGeometryExt,
            85 => RecordType::VectorEdgeExt,
            86 => RecordType::VectorConnectedNodeExt,
            96 => RecordType::VectorEdge,
            97 => RecordType::VectorConnectedNode,
            98 => RecordType::CellCoverage,
            99 => RecordType::CellNoCoverage,
            101 => RecordType::CellTextDescInfo,
            200 => RecordType::ServerStatus,
            _ => RecordType::Unknown,
        }
    }
}

/// SENC file header information
#[derive(Debug, Clone)]
pub struct SencHeader {
    /// SENC format version (expected: 201)
    pub version: u16,
    /// Cell/chart name (e.g., "DK5HRBOL")
    pub cell_name: String,
    /// Native scale (e.g., 22000 for 1:22000)
    pub native_scale: u32,
    /// Bounding box (WGS84)
    pub extent: Option<CellExtent>,
    /// Reference latitude (centroid of extent) - used for SM coordinate conversion
    pub ref_lat: f64,
    /// Reference longitude (centroid of extent) - used for SM coordinate conversion
    pub ref_lon: f64,
}

impl Default for SencHeader {
    fn default() -> Self {
        Self {
            version: 0,
            cell_name: String::new(),
            native_scale: 0,
            extent: None,
            ref_lat: 0.0,
            ref_lon: 0.0,
        }
    }
}

/// Cell bounding box (WGS84)
#[derive(Debug, Clone, Copy)]
pub struct CellExtent {
    pub min_lat: f64,
    pub max_lat: f64,
    pub min_lon: f64,
    pub max_lon: f64,
}

impl CellExtent {
    /// Center latitude for projection origin
    pub fn center_lat(&self) -> f64 {
        (self.min_lat + self.max_lat) / 2.0
    }

    /// Center longitude for projection origin
    pub fn center_lon(&self) -> f64 {
        (self.min_lon + self.max_lon) / 2.0
    }
}

/// S-57 Object class codes for features we care about
/// Codes are from IHO S-57 specification (s57objectclasses.csv)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum ObjectClass {
    // === AREA FEATURES ===
    /// Anchorage area (ACHARE) - code 3
    AnchorageArea,
    /// Building, single (BUISGL) - code 12
    Building,
    /// Built-up area (BUAARE) - code 13
    BuiltUpArea,
    /// Depth area (DEPARE) - code 42
    DepthArea,
    /// Dredged area (DRGARE) - code 46
    DredgedArea,
    /// Fairway (FAIRWY) - code 51
    Fairway,
    /// Lake (LAKARE) - code 69
    Lake,
    /// Land area (LNDARE) - code 71
    LandArea,
    /// Obstruction (OBSTRN) - code 86 (can be area or point)
    Obstruction,
    /// Restricted area (RESARE) - code 112
    RestrictedArea,
    /// Sea area / named water area (SEAARE) - code 119
    SeaArea,
    /// Traffic separation zone (TSEZNE) - code 147
    TrafficSeparationZone,

    // === LINE FEATURES ===
    /// Coastline (COALNE) - code 30
    Coastline,
    /// Depth contour (DEPCNT) - code 43
    DepthContour,
    /// Cable, overhead (CBLOHD) - code 21
    CableOverhead,
    /// Cable, submarine (CBLSUB) - code 22
    CableSubmarine,
    /// Ferry route (FERYRT) - code 52
    FerryRoute,
    /// Pipeline, submarine/on land (PIPSOL) - code 94
    Pipeline,
    /// Recommended route centerline (RCRTCL) - code 108
    RecommendedRoute,
    /// Road (ROADWY) - code 116
    Road,
    /// River bank (RIVBNK) - code 115
    RiverBank,
    /// Shoreline construction (SLCONS) - code 122 (piers, jetties, seawalls)
    ShorelineConstruction,
    /// Traffic separation line (TSELNE) - code 145
    TrafficSeparationLine,

    // === POINT FEATURES (symbols) ===
    /// Anchor berth (ACHBRT) - code 2
    AnchorBerth,
    /// Beacon, cardinal (BCNCAR) - code 5
    BeaconCardinal,
    /// Beacon, isolated danger (BCNISD) - code 6
    BeaconIsolatedDanger,
    /// Beacon, lateral (BCNLAT) - code 7
    BeaconLateral,
    /// Beacon, safe water (BCNSAW) - code 8
    BeaconSafeWater,
    /// Beacon, special purpose (BCNSPP) - code 9
    BeaconSpecialPurpose,
    /// Berth (BERTHS) - code 10
    Berth,
    /// Buoy, cardinal (BOYCAR) - code 14
    BuoyCardinal,
    /// Buoy, installation (BOYINB) - code 15
    BuoyInstallation,
    /// Buoy, isolated danger (BOYISD) - code 16
    BuoyIsolatedDanger,
    /// Buoy, lateral (BOYLAT) - code 17
    BuoyLateral,
    /// Buoy, safe water (BOYSAW) - code 18
    BuoySafeWater,
    /// Buoy, special purpose (BOYSPP) - code 19
    BuoySpecialPurpose,
    /// Light (LIGHTS) - code 75
    Light,
    /// Mooring/warping facility (MORFAC) - code 84
    MooringFacility,
    /// Pile (PILPNT) - code 90
    Pile,
    /// Sounding (SOUNDG) - code 129
    Sounding,
    /// Underwater rock / awash rock (UWTROC) - code 153
    UnderwaterRock,
    /// Wreck (WRECKS) - code 159
    Wreck,

    // === META FEATURES (usually not rendered) ===
    /// Coverage meta feature (M_COVR) - code 302
    Coverage,
    /// Compilation scale (M_CSCL) - code 305
    CompilationScale,
    /// Quality of data (M_QUAL) - code 308
    QualityOfData,

    /// Unknown/unrecognized object class
    Other,
}

impl ObjectClass {
    /// Parse from S-57 numeric object class code
    /// Standard S-57 object codes from IHO S-57 specification
    pub fn from_code(code: u16) -> Self {
        match code {
            // Point features (symbols)
            2 => ObjectClass::AnchorBerth,           // ACHBRT
            5 => ObjectClass::BeaconCardinal,        // BCNCAR
            6 => ObjectClass::BeaconIsolatedDanger,  // BCNISD
            7 => ObjectClass::BeaconLateral,         // BCNLAT
            8 => ObjectClass::BeaconSafeWater,       // BCNSAW
            9 => ObjectClass::BeaconSpecialPurpose,  // BCNSPP
            10 => ObjectClass::Berth,                // BERTHS
            14 => ObjectClass::BuoyCardinal,         // BOYCAR
            15 => ObjectClass::BuoyInstallation,     // BOYINB
            16 => ObjectClass::BuoyIsolatedDanger,   // BOYISD
            17 => ObjectClass::BuoyLateral,          // BOYLAT
            18 => ObjectClass::BuoySafeWater,        // BOYSAW
            19 => ObjectClass::BuoySpecialPurpose,   // BOYSPP
            75 => ObjectClass::Light,                // LIGHTS
            84 => ObjectClass::MooringFacility,      // MORFAC
            90 => ObjectClass::Pile,                 // PILPNT
            129 => ObjectClass::Sounding,            // SOUNDG
            153 => ObjectClass::UnderwaterRock,      // UWTROC
            159 => ObjectClass::Wreck,               // WRECKS

            // Line features
            21 => ObjectClass::CableOverhead,        // CBLOHD
            22 => ObjectClass::CableSubmarine,       // CBLSUB
            30 => ObjectClass::Coastline,            // COALNE
            43 => ObjectClass::DepthContour,         // DEPCNT
            52 => ObjectClass::FerryRoute,           // FERYRT
            94 => ObjectClass::Pipeline,             // PIPSOL
            108 => ObjectClass::RecommendedRoute,    // RCRTCL
            115 => ObjectClass::RiverBank,           // RIVBNK
            116 => ObjectClass::Road,                // ROADWY
            122 => ObjectClass::ShorelineConstruction, // SLCONS
            145 => ObjectClass::TrafficSeparationLine, // TSELNE

            // Area features
            3 => ObjectClass::AnchorageArea,         // ACHARE
            12 => ObjectClass::Building,             // BUISGL
            13 => ObjectClass::BuiltUpArea,          // BUAARE
            42 => ObjectClass::DepthArea,            // DEPARE
            46 => ObjectClass::DredgedArea,          // DRGARE
            51 => ObjectClass::Fairway,              // FAIRWY
            69 => ObjectClass::Lake,                 // LAKARE
            71 => ObjectClass::LandArea,             // LNDARE
            86 => ObjectClass::Obstruction,          // OBSTRN
            112 => ObjectClass::RestrictedArea,      // RESARE
            119 => ObjectClass::SeaArea,             // SEAARE
            147 => ObjectClass::TrafficSeparationZone, // TSEZNE

            // Meta features
            302 => ObjectClass::Coverage,            // M_COVR
            305 => ObjectClass::CompilationScale,    // M_CSCL
            308 => ObjectClass::QualityOfData,       // M_QUAL

            _ => ObjectClass::Other,
        }
    }

    /// Parse from S-57 6-character acronym
    pub fn from_acronym(acronym: &str) -> Self {
        match acronym.trim() {
            // Point features
            "ACHBRT" => ObjectClass::AnchorBerth,
            "BCNCAR" => ObjectClass::BeaconCardinal,
            "BCNISD" => ObjectClass::BeaconIsolatedDanger,
            "BCNLAT" => ObjectClass::BeaconLateral,
            "BCNSAW" => ObjectClass::BeaconSafeWater,
            "BCNSPP" => ObjectClass::BeaconSpecialPurpose,
            "BERTHS" => ObjectClass::Berth,
            "BOYCAR" => ObjectClass::BuoyCardinal,
            "BOYINB" => ObjectClass::BuoyInstallation,
            "BOYISD" => ObjectClass::BuoyIsolatedDanger,
            "BOYLAT" => ObjectClass::BuoyLateral,
            "BOYSAW" => ObjectClass::BuoySafeWater,
            "BOYSPP" => ObjectClass::BuoySpecialPurpose,
            "LIGHTS" => ObjectClass::Light,
            "MORFAC" => ObjectClass::MooringFacility,
            "PILPNT" => ObjectClass::Pile,
            "SOUNDG" => ObjectClass::Sounding,
            "UWTROC" => ObjectClass::UnderwaterRock,
            "WRECKS" => ObjectClass::Wreck,

            // Line features
            "CBLOHD" => ObjectClass::CableOverhead,
            "CBLSUB" => ObjectClass::CableSubmarine,
            "COALNE" => ObjectClass::Coastline,
            "DEPCNT" => ObjectClass::DepthContour,
            "FERYRT" => ObjectClass::FerryRoute,
            "PIPSOL" => ObjectClass::Pipeline,
            "RCRTCL" => ObjectClass::RecommendedRoute,
            "RIVBNK" => ObjectClass::RiverBank,
            "ROADWY" => ObjectClass::Road,
            "SLCONS" => ObjectClass::ShorelineConstruction,
            "TSELNE" => ObjectClass::TrafficSeparationLine,

            // Area features
            "ACHARE" => ObjectClass::AnchorageArea,
            "BUISGL" => ObjectClass::Building,
            "BUAARE" => ObjectClass::BuiltUpArea,
            "DEPARE" => ObjectClass::DepthArea,
            "DRGARE" => ObjectClass::DredgedArea,
            "FAIRWY" => ObjectClass::Fairway,
            "LAKARE" => ObjectClass::Lake,
            "LNDARE" => ObjectClass::LandArea,
            "OBSTRN" => ObjectClass::Obstruction,
            "RESARE" => ObjectClass::RestrictedArea,
            "SEAARE" => ObjectClass::SeaArea,
            "TSEZNE" => ObjectClass::TrafficSeparationZone,

            // Meta features
            "M_COVR" => ObjectClass::Coverage,
            "M_CSCL" => ObjectClass::CompilationScale,
            "M_QUAL" => ObjectClass::QualityOfData,

            _ => ObjectClass::Other,
        }
    }

    /// Get the 6-character acronym
    pub fn acronym(&self) -> &'static str {
        match self {
            // Point features
            ObjectClass::AnchorBerth => "ACHBRT",
            ObjectClass::BeaconCardinal => "BCNCAR",
            ObjectClass::BeaconIsolatedDanger => "BCNISD",
            ObjectClass::BeaconLateral => "BCNLAT",
            ObjectClass::BeaconSafeWater => "BCNSAW",
            ObjectClass::BeaconSpecialPurpose => "BCNSPP",
            ObjectClass::Berth => "BERTHS",
            ObjectClass::BuoyCardinal => "BOYCAR",
            ObjectClass::BuoyInstallation => "BOYINB",
            ObjectClass::BuoyIsolatedDanger => "BOYISD",
            ObjectClass::BuoyLateral => "BOYLAT",
            ObjectClass::BuoySafeWater => "BOYSAW",
            ObjectClass::BuoySpecialPurpose => "BOYSPP",
            ObjectClass::Light => "LIGHTS",
            ObjectClass::MooringFacility => "MORFAC",
            ObjectClass::Pile => "PILPNT",
            ObjectClass::Sounding => "SOUNDG",
            ObjectClass::UnderwaterRock => "UWTROC",
            ObjectClass::Wreck => "WRECKS",

            // Line features
            ObjectClass::CableOverhead => "CBLOHD",
            ObjectClass::CableSubmarine => "CBLSUB",
            ObjectClass::Coastline => "COALNE",
            ObjectClass::DepthContour => "DEPCNT",
            ObjectClass::FerryRoute => "FERYRT",
            ObjectClass::Pipeline => "PIPSOL",
            ObjectClass::RecommendedRoute => "RCRTCL",
            ObjectClass::RiverBank => "RIVBNK",
            ObjectClass::Road => "ROADWY",
            ObjectClass::ShorelineConstruction => "SLCONS",
            ObjectClass::TrafficSeparationLine => "TSELNE",

            // Area features
            ObjectClass::AnchorageArea => "ACHARE",
            ObjectClass::Building => "BUISGL",
            ObjectClass::BuiltUpArea => "BUAARE",
            ObjectClass::DepthArea => "DEPARE",
            ObjectClass::DredgedArea => "DRGARE",
            ObjectClass::Fairway => "FAIRWY",
            ObjectClass::Lake => "LAKARE",
            ObjectClass::LandArea => "LNDARE",
            ObjectClass::Obstruction => "OBSTRN",
            ObjectClass::RestrictedArea => "RESARE",
            ObjectClass::SeaArea => "SEAARE",
            ObjectClass::TrafficSeparationZone => "TSEZNE",

            // Meta features
            ObjectClass::Coverage => "M_COVR",
            ObjectClass::CompilationScale => "M_CSCL",
            ObjectClass::QualityOfData => "M_QUAL",

            ObjectClass::Other => "UNKNWN",
        }
    }

    /// Returns true if this is a meta feature that shouldn't be rendered
    pub fn is_meta(&self) -> bool {
        matches!(self,
            ObjectClass::Coverage |
            ObjectClass::CompilationScale |
            ObjectClass::QualityOfData
        )
    }

    /// Returns true if this is an area feature
    pub fn is_area(&self) -> bool {
        matches!(self,
            ObjectClass::AnchorageArea |
            ObjectClass::BuiltUpArea |
            ObjectClass::DepthArea |
            ObjectClass::DredgedArea |
            ObjectClass::Fairway |
            ObjectClass::Lake |
            ObjectClass::LandArea |
            ObjectClass::Obstruction |
            ObjectClass::RestrictedArea |
            ObjectClass::SeaArea |
            ObjectClass::TrafficSeparationZone
        )
    }

    /// Returns true if this is a line feature
    pub fn is_line(&self) -> bool {
        matches!(self,
            ObjectClass::CableOverhead |
            ObjectClass::CableSubmarine |
            ObjectClass::Coastline |
            ObjectClass::DepthContour |
            ObjectClass::FerryRoute |
            ObjectClass::Pipeline |
            ObjectClass::RecommendedRoute |
            ObjectClass::RiverBank |
            ObjectClass::Road |
            ObjectClass::ShorelineConstruction |
            ObjectClass::TrafficSeparationLine
        )
    }

    /// Returns true if this is a point/symbol feature
    pub fn is_point(&self) -> bool {
        matches!(self,
            ObjectClass::AnchorBerth |
            ObjectClass::BeaconCardinal |
            ObjectClass::BeaconIsolatedDanger |
            ObjectClass::BeaconLateral |
            ObjectClass::BeaconSafeWater |
            ObjectClass::BeaconSpecialPurpose |
            ObjectClass::Berth |
            ObjectClass::BuoyCardinal |
            ObjectClass::BuoyInstallation |
            ObjectClass::BuoyIsolatedDanger |
            ObjectClass::BuoyLateral |
            ObjectClass::BuoySafeWater |
            ObjectClass::BuoySpecialPurpose |
            ObjectClass::Light |
            ObjectClass::MooringFacility |
            ObjectClass::Pile |
            ObjectClass::Sounding |
            ObjectClass::UnderwaterRock |
            ObjectClass::Wreck
        )
    }
}

/// Map ANY S-57 numeric object class code to its 6-char acronym.
/// Source of truth: IHO S-57 specification / s57objectclasses.csv (251 entries).
/// Returns "UNKNWN" for truly unrecognized codes.
pub fn s57_code_to_acronym(code: u16) -> &'static str {
    match code {
        // Geo features (1-163)
        1 => "ADMARE",
        2 => "AIRARE",
        3 => "ACHBRT",
        4 => "ACHARE",
        5 => "BCNCAR",
        6 => "BCNISD",
        7 => "BCNLAT",
        8 => "BCNSAW",
        9 => "BCNSPP",
        10 => "BERTHS",
        11 => "BRIDGE",
        12 => "BUISGL",
        13 => "BUAARE",
        14 => "BOYCAR",
        15 => "BOYINB",
        16 => "BOYISD",
        17 => "BOYLAT",
        18 => "BOYSAW",
        19 => "BOYSPP",
        20 => "CBLARE",
        21 => "CBLOHD",
        22 => "CBLSUB",
        23 => "CANALS",
        24 => "CANBNK",
        25 => "CTSARE",
        26 => "CAUSWY",
        27 => "CTNARE",
        28 => "CHKPNT",
        29 => "CGUSTA",
        30 => "COALNE",
        31 => "CONZNE",
        32 => "COSARE",
        33 => "CTRPNT",
        34 => "CONVYR",
        35 => "CRANES",
        36 => "CURENT",
        37 => "CUSZNE",
        38 => "DAMCON",
        39 => "DAYMAR",
        40 => "DWRTCL",
        41 => "DWRTPT",
        42 => "DEPARE",
        43 => "DEPCNT",
        44 => "DISMAR",
        45 => "DOCARE",
        46 => "DRGARE",
        47 => "DRYDOC",
        48 => "DMPGRD",
        49 => "DYKCON",
        50 => "EXEZNE",
        51 => "FAIRWY",
        52 => "FNCLNE",
        53 => "FERYRT",
        54 => "FSHZNE",
        55 => "FSHFAC",
        56 => "FSHGRD",
        57 => "FLODOC",
        58 => "FOGSIG",
        59 => "FORSTC",
        60 => "FRPARE",
        61 => "GATCON",
        62 => "GRIDRN",
        63 => "HRBARE",
        64 => "HRBFAC",
        65 => "HULKES",
        66 => "ICEARE",
        67 => "ICNARE",
        68 => "ISTZNE",
        69 => "LAKARE",
        70 => "LAKSHR",
        71 => "LNDARE",
        72 => "LNDELV",
        73 => "LNDRGN",
        74 => "LNDMRK",
        75 => "LIGHTS",
        76 => "LITFLT",
        77 => "LITVES",
        78 => "LOCMAG",
        79 => "LOKBSN",
        80 => "LOGPON",
        81 => "MAGVAR",
        82 => "MARCUL",
        83 => "MIPARE",
        84 => "MORFAC",
        85 => "NAVLNE",
        86 => "OBSTRN",
        87 => "OFSPLF",
        88 => "OSPARE",
        89 => "OILBAR",
        90 => "PILPNT",
        91 => "PILBOP",
        92 => "PIPARE",
        93 => "PIPOHD",
        94 => "PIPSOL",
        95 => "PONTON",
        96 => "PRCARE",
        97 => "PRDARE",
        98 => "PYLONS",
        99 => "RADLNE",
        100 => "RADRNG",
        101 => "RADRFL",
        102 => "RADSTA",
        103 => "RTPBCN",
        104 => "RDOCAL",
        105 => "RDOSTA",
        106 => "RAILWY",
        107 => "RAPIDS",
        108 => "RCRTCL",
        109 => "RECTRC",
        110 => "RCTLPT",
        111 => "RSCSTA",
        112 => "RESARE",
        113 => "RETRFL",
        114 => "RIVERS",
        115 => "RIVBNK",
        116 => "ROADWY",
        117 => "RUNWAY",
        118 => "SNDWAV",
        119 => "SEAARE",
        120 => "SPLARE",
        121 => "SBDARE",
        122 => "SLCONS",
        123 => "SISTAT",
        124 => "SISTAW",
        125 => "SILTNK",
        126 => "SLOTOP",
        127 => "SLOGRD",
        128 => "SMCFAC",
        129 => "SOUNDG",
        130 => "SPRING",
        131 => "SQUARE",
        132 => "STSLNE",
        133 => "SUBTLN",
        134 => "SWPARE",
        135 => "TESARE",
        136 => "TS_PRH",
        137 => "TS_PNH",
        138 => "TS_PAD",
        139 => "TS_TIS",
        140 => "T_HMON",
        141 => "T_NHMN",
        142 => "T_TIMS",
        143 => "TIDEWY",
        144 => "TOPMAR",
        145 => "TSELNE",
        146 => "TSSBND",
        147 => "TSSCRS",
        148 => "TSSLPT",
        149 => "TSSRON",
        150 => "TSEZNE",
        151 => "TUNNEL",
        152 => "TWRTPT",
        153 => "UWTROC",
        154 => "UNSARE",
        155 => "VEGATN",
        156 => "WATTUR",
        157 => "WATFAL",
        158 => "WEDKLP",
        159 => "WRECKS",
        160 => "TS_FEB",
        163 => "NEWOBJ",

        // Extended codes (1000+)
        1003 => "ACHPNT",
        1012 => "BUIREL",
        1075 => "_extgn",
        1083 => "MONUMT",
        1144 => "TOWERS",
        1159 => "ZEMCNT",

        // Meta features (300+)
        300 => "M_ACCY",
        301 => "M_CSCL",
        302 => "M_COVR",
        303 => "M_HDAT",
        304 => "M_HOPA",
        305 => "M_NPUB",
        306 => "M_NSYS",
        307 => "M_PROD",
        308 => "M_QUAL",
        309 => "M_SDAT",
        310 => "M_SREL",
        311 => "M_UNIT",
        312 => "M_VDAT",

        // Collection objects (400+)
        400 => "C_AGGR",
        401 => "C_ASSO",
        402 => "C_STAC",

        // Cartographic objects (500+)
        500 => "$AREAS",
        501 => "$LINES",
        502 => "$CSYMB",
        503 => "$COMPS",
        504 => "$TEXTS",

        // Inland waterway extensions (17000+)
        17000 => "achbrt",
        17001 => "achare",
        17002 => "canbnk",
        17003 => "depare",
        17004 => "dismar",
        17005 => "resare",
        17006 => "rivbnk",
        17007 => "sistat",
        17008 => "sistaw",
        17009 => "topmar",
        17010 => "berths",
        17011 => "bridge",
        17012 => "cblohd",
        17013 => "feryrt",
        17014 => "hrbare",
        17015 => "hrbfac",
        17016 => "lokbsn",
        17017 => "rdocal",
        17018 => "m_nsys",
        17019 => "curent",
        17020 => "hulkes",
        17021 => "ponton",
        17022 => "m_sdat",
        17023 => "m_vdat",
        17024 => "pipohd",
        17025 => "flodoc",
        17027 => "chkpnt",
        17028 => "bcnlat",
        17029 => "boylat",
        17030 => "cranes",
        17031 => "gatcon",
        17032 => "slcons",
        17033 => "uwtroc",
        17034 => "convyr",
        17050 => "notmrk",
        17051 => "wtwaxs",
        17052 => "wtwprf",
        17053 => "brgare",
        17054 => "bunsta",
        17055 => "comare",
        17056 => "hrbbsn",
        17057 => "lokare",
        17058 => "lkbspt",
        17059 => "prtare",
        17060 => "bcnwtw",
        17061 => "boywtw",
        17062 => "refdmp",
        17063 => "rtplpt",
        17064 => "termnl",
        17065 => "trnbsn",
        17066 => "wtware",
        17067 => "wtwgag",
        17068 => "tisdge",
        17069 => "vehtrf",
        17070 => "excnst",

        // Other extended codes
        18001 => "lg_sdm",
        18002 => "lg_vsp",
        22001 => "ANNOTA",
        22009 => "RESTRC",
        22010 => "TRFLNE",
        22011 => "GENNAV",

        _ => "UNKNWN",
    }
}
