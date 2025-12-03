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
            96 => RecordType::VectorEdge,
            97 => RecordType::VectorConnectedNode,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum ObjectClass {
    /// Land area (LNDARE)
    LandArea = 0,
    /// Depth area (DEPARE)
    DepthArea = 1,
    /// Depth contour (DEPCNT)
    DepthContour = 2,
    /// Coastline (COALNE)
    Coastline = 3,
    /// Sounding (SOUNDG)
    Sounding = 4,
    /// Buoy lateral (BOYLAT)
    BuoyLateral = 5,
    /// Beacon lateral (BCNLAT)
    BeaconLateral = 6,
    /// Underwater rock (UWTROC)
    UnderwaterRock = 7,
    /// Unknown object class
    Other = 0xFFFF,
}

impl ObjectClass {
    /// Parse from S-57 numeric object class code
    /// Standard S-57 object codes from IHO S-57 specification
    pub fn from_code(code: u16) -> Self {
        match code {
            4 => ObjectClass::BeaconLateral,    // BCNLAT
            17 => ObjectClass::BuoyLateral,     // BOYLAT
            30 => ObjectClass::Coastline,       // COALNE
            42 => ObjectClass::DepthArea,       // DEPARE
            43 => ObjectClass::DepthContour,    // DEPCNT
            71 => ObjectClass::LandArea,        // LNDARE
            129 => ObjectClass::Sounding,       // SOUNDG
            153 => ObjectClass::UnderwaterRock, // UWTROC
            _ => ObjectClass::Other,
        }
    }

    /// Parse from S-57 6-character acronym
    pub fn from_acronym(acronym: &str) -> Self {
        match acronym.trim() {
            "LNDARE" => ObjectClass::LandArea,
            "DEPARE" => ObjectClass::DepthArea,
            "DEPCNT" => ObjectClass::DepthContour,
            "COALNE" => ObjectClass::Coastline,
            "SOUNDG" => ObjectClass::Sounding,
            "BOYLAT" => ObjectClass::BuoyLateral,
            "BCNLAT" => ObjectClass::BeaconLateral,
            "UWTROC" => ObjectClass::UnderwaterRock,
            _ => ObjectClass::Other,
        }
    }

    /// Get the 6-character acronym
    pub fn acronym(&self) -> &'static str {
        match self {
            ObjectClass::LandArea => "LNDARE",
            ObjectClass::DepthArea => "DEPARE",
            ObjectClass::DepthContour => "DEPCNT",
            ObjectClass::Coastline => "COALNE",
            ObjectClass::Sounding => "SOUNDG",
            ObjectClass::BuoyLateral => "BOYLAT",
            ObjectClass::BeaconLateral => "BCNLAT",
            ObjectClass::UnderwaterRock => "UWTROC",
            ObjectClass::Other => "UNKNWN",
        }
    }
}
