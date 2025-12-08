//! SENC feature extraction.
//!
//! Parses S-57 features from SENC records, focusing on:
//! - LNDARE (land area) - renders as tan
//! - DEPARE (depth area) - renders with depth-based blue gradient

use std::collections::HashMap;
use std::io::Cursor;
use byteorder::{LittleEndian, ReadBytesExt};

use super::geometry::{AreaGeometry, EdgeTable, LineGeometry, PointGeometry};
use super::reader::{RawRecord, SencError, SencReader};
use super::records::{ObjectClass, RecordType, SencHeader};

/// Convert S-57 attribute code to attribute name
/// Only the attributes we care about for MVP (depth values)
fn s57_attribute_name(code: u16) -> String {
    match code {
        // Depth-related attributes
        87 => "DRVAL1".to_string(),   // Depth range value 1 (shallow)
        88 => "DRVAL2".to_string(),   // Depth range value 2 (deep)
        174 => "VALDCO".to_string(),  // Value of depth contour
        178 => "VALSOU".to_string(),  // Value of sounding

        // Symbol-related attributes (for buoys, beacons, etc.)
        36 => "CATLAM".to_string(),   // Category of lateral mark (1=port, 2=starboard)
        75 => "COLOUR".to_string(),   // Colour (3=red, 4=green)
        179 => "WATLEV".to_string(),  // Water level effect (for rocks)

        // Other common attributes
        76 => "OBJNAM".to_string(),   // Object name
        113 => "INFORM".to_string(),  // Information
        116 => "NINFOM".to_string(),  // Information in national language
        142 => "SCAMIN".to_string(),  // Scale minimum
        143 => "SCAMAX".to_string(),  // Scale maximum

        // Return numeric code as string for unknown attributes
        _ => format!("ATTR_{}", code),
    }
}

/// Type of geometry a feature contains
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Parsed attributes (key -> value)
    pub attributes: HashMap<String, AttributeValue>,
    /// Area geometry (if feature_type == Area)
    pub area_geometry: Option<AreaGeometry>,
    /// Point geometry (if feature_type == Point)
    pub point_geometry: Option<PointGeometry>,
    /// Line geometry (if feature_type == Line)
    pub line_geometry: Option<LineGeometry>,
}

/// Attribute value types
#[derive(Debug, Clone)]
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

    /// Check if this is land
    pub fn is_land(&self) -> bool {
        self.object_class == ObjectClass::LandArea
    }

    /// Check if this is a depth area
    pub fn is_depth_area(&self) -> bool {
        self.object_class == ObjectClass::DepthArea
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
        matches!(self.object_class, ObjectClass::CableOverhead | ObjectClass::CableSubmarine)
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
        let header = reader.read_header()?;

        let mut features = Vec::new();
        let mut current_feature: Option<PartialFeature> = None;
        let mut edge_table = EdgeTable::new();

        // Parse remaining records
        while let Some(record) = reader.next_record()? {
            match record.record_type {
                RecordType::FeatureId => {
                    // Save previous feature if complete
                    if let Some(partial) = current_feature.take() {
                        if let Some(feature) = partial.finalize() {
                            features.push(feature);
                        }
                    }

                    // Start new feature
                    current_feature = Some(PartialFeature::from_record(&record)?);
                }
                RecordType::FeatureAttribute => {
                    if let Some(ref mut partial) = current_feature {
                        partial.add_attribute(&record)?;
                    }
                }
                RecordType::AreaGeometry => {
                    if let Some(ref mut partial) = current_feature {
                        partial.set_area_geometry(&record)?;
                    }
                }
                RecordType::PointGeometry => {
                    if let Some(ref mut partial) = current_feature {
                        partial.set_point_geometry(&record)?;
                    }
                }
                RecordType::LineGeometry => {
                    if let Some(ref mut partial) = current_feature {
                        partial.set_line_geometry(&record)?;
                    }
                }
                RecordType::MultipointGeometry => {
                    if let Some(ref mut partial) = current_feature {
                        partial.feature_type = Some(FeatureType::Multipoint);
                    }
                }
                RecordType::VectorEdge => {
                    // Edge table record contains multiple edges
                    if let Err(e) = edge_table.parse_edge_table(&record.payload) {
                        eprintln!("WARN: Edge table parse error: {}", e);
                    }
                }
                RecordType::VectorConnectedNode => {
                    // Node table record contains multiple nodes
                    if let Err(e) = edge_table.parse_node_table(&record.payload) {
                        eprintln!("WARN: Node table parse error: {}", e);
                    }
                }
                _ => {}
            }
        }

        // Don't forget the last feature
        if let Some(partial) = current_feature {
            if let Some(feature) = partial.finalize() {
                features.push(feature);
            }
        }

        Ok(Self { header, features, edge_table })
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
        self.features.iter().filter(|f| f.is_shoreline_construction())
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

    /// Summary stats for debugging
    pub fn summary(&self) -> String {
        let land_count = self.land_areas().count();
        let depth_count = self.depth_areas().count();
        let area_count = self.areas().count();
        let line_count = self.lines().count();
        let point_count = self.points().count();
        let contour_count = self.depth_contours().count();
        let coastline_count = self.coastlines().count();
        let total = self.features.len();

        format!(
            "Chart '{}' (scale 1:{}): {} features total\n\
             - Land areas: {}\n\
             - Depth areas: {}\n\
             - All areas: {}\n\
             - All lines: {}\n\
             - All points: {}\n\
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
    attributes: HashMap<String, AttributeValue>,
    area_geometry: Option<AreaGeometry>,
    point_geometry: Option<PointGeometry>,
    line_geometry: Option<LineGeometry>,
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
                "FeatureId payload too short: {} bytes (need 5)", payload.len()
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
            attributes: HashMap::new(),
            area_geometry: None,
            point_geometry: None,
            line_geometry: None,
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

        // Convert S-57 attribute code to name
        let name = s57_attribute_name(attr_code);

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
                let end = str_bytes.iter().position(|&b| b == 0).unwrap_or(str_bytes.len());
                let s = String::from_utf8_lossy(&str_bytes[..end]).to_string();
                AttributeValue::String(s)
            }
            _ => return Ok(()),
        };

        self.attributes.insert(name, value);
        Ok(())
    }

    fn set_area_geometry(&mut self, record: &RawRecord) -> Result<(), SencError> {
        self.feature_type = Some(FeatureType::Area);
        self.area_geometry = Some(
            AreaGeometry::parse(&record.payload)
                .map_err(|e| SencError::Format(e.to_string()))?,
        );
        Ok(())
    }

    fn set_point_geometry(&mut self, record: &RawRecord) -> Result<(), SencError> {
        self.feature_type = Some(FeatureType::Point);
        self.point_geometry = Some(
            PointGeometry::parse(&record.payload)
                .map_err(|e| SencError::Format(e.to_string()))?,
        );
        Ok(())
    }

    fn set_line_geometry(&mut self, record: &RawRecord) -> Result<(), SencError> {
        self.feature_type = Some(FeatureType::Line);
        self.line_geometry = Some(
            LineGeometry::parse(&record.payload)
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
        })
    }
}
