//! S-52 Area Pattern Definitions
//!
//! Extracted from OpenCPN's chartsymbols.xml
//! Contains 30 patterns total: 25 vector (HPGL) + 5 raster-only

/// Pattern fill type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillType {
    /// Staggered - brick-like pattern with alternating row offset
    Staggered,
    /// Linear - aligned grid pattern
    Linear,
}

/// Pattern definition type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternType {
    /// Vector pattern defined by HPGL commands
    Vector,
    /// Raster pattern from bitmap atlas
    Raster,
}

/// S-52 Area Pattern Definition
#[derive(Debug, Clone)]
pub struct PatternDef {
    /// Pattern name (e.g., "DIAMOND1", "DRGARE01")
    pub name: &'static str,
    /// Pattern type: Vector (HPGL) or Raster (bitmap)
    pub pattern_type: PatternType,
    /// Fill type: Staggered (brick) or Linear (grid)
    pub fill_type: FillType,
    /// Tile width in HPGL units (for vector) or pixels (for raster)
    pub width: u32,
    /// Tile height in HPGL units (for vector) or pixels (for raster)
    pub height: u32,
    /// Pivot X coordinate
    pub pivot_x: i32,
    /// Pivot Y coordinate
    pub pivot_y: i32,
    /// Origin X coordinate
    pub origin_x: i32,
    /// Origin Y coordinate
    pub origin_y: i32,
    /// Color token reference (e.g., "ACHGRD", "ALANDF")
    pub color_ref: &'static str,
    /// HPGL drawing commands (for vector patterns)
    pub hpgl: Option<&'static str>,
    /// Bitmap atlas location (x, y) for raster patterns
    pub bitmap_location: Option<(u32, u32)>,
    /// Minimum display distance
    pub min_distance: u32,
    /// Maximum display distance
    pub max_distance: u32,
}

impl PatternDef {
    /// Get the stagger factor for rendering (0.0 for linear, 0.5 for staggered)
    pub const fn stagger_factor(&self) -> f32 {
        match self.fill_type {
            FillType::Staggered => 0.5,
            FillType::Linear => 0.0,
        }
    }

    /// Check if this pattern is staggered (brick-like)
    pub const fn is_staggered(&self) -> bool {
        matches!(self.fill_type, FillType::Staggered)
    }
}

/// All S-52 area patterns from chartsymbols.xml
/// Total: 30 patterns (25 vector + 5 raster-only)
pub static PATTERNS: &[PatternDef] = &[
    // ========== VECTOR PATTERNS (HPGL) ==========

    PatternDef {
        name: "AIRARE02",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 618,
        height: 528,
        pivot_x: 2259,
        pivot_y: 2256,
        origin_x: 435,
        origin_y: 452,
        color_ref: "ALANDF",
        hpgl: Some("SPA;SW1;PU623,980;PD859,980;PD790,901;PD790,801;PD1053,801;PD810,638;PD810,516;PD751,452;PD680,516;PD680,638;PD435,795;PD684,797;PD684,907;PD623,980;"),
        bitmap_location: None,
        min_distance: 2000,
        max_distance: 10000,
    },
    PatternDef {
        name: "DIAMOND1",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Linear,
        width: 2250,
        height: 4313,
        pivot_x: 2250,
        pivot_y: 2250,
        origin_x: 1125,
        origin_y: 93,
        color_ref: "DEPCN", // Note: Original has backslash prefix for color lookup
        hpgl: Some("SP\\;SW1;PU1125,93;PD3375,4406;PU1125,4406;PD3375,93;"),
        bitmap_location: None,
        min_distance: 0,
        max_distance: 0,
    },
    PatternDef {
        name: "DQUALA11",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 1697,
        height: 1184,
        pivot_x: 2423,
        pivot_y: 1417,
        origin_x: 1570,
        origin_y: 856,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW1;PU2779,1066;PD3080,1066;SPA;SW1;PU3021,947;PD2840,1187;SPA;SW1;PU2841,947;PD3021,1186;SPA;SW1;PU2257,1066;PD2558,1066;SPA;SW1;PU2499,947;PD2318,1187;SPA;SW1;PU2319,947;PD2499,1186;SPA;SW1;PU1728,1071;PD2029,1071;SPA;SW1;PU1970,952;PD1789,1192;SPA;SW1;PU1790,952;PD1970,1191;SPA;SW1;PU2009,1420;PD2310,1420;SPA;SW1;PU2251,1301;PD2070,1541;SPA;SW1;PU2071,1301;PD2251,1540;SPA;SW1;PU2537,1415;PD2838,1415;SPA;SW1;PU2779,1296;PD2598,1536;SPA;SW1;PU2599,1296;PD2779,1535;SPA;SW1;PU2287,1756;PD2588,1756;SPA;SW1;PU2529,1637;PD2348,1877;SPA;SW1;PU2349,1637;PD2529,1876;SPA;SW1;PU1600,1052;PD1570,971;PD1581,917;PD1600,879;PD1647,859;PD1678,856;PD3105,856;PD3186,863;PD3236,886;PD3256,914;PD3267,944;PD3267,968;PD3267,1006;PD3248,1037;PD2561,1951;PD2534,1982;PD2507,2013;PD2480,2029;PD2461,2032;PD2442,2040;PD2407,2036;PD2388,2032;PD2349,2021;PD2330,2005;PD2310,1982;PD2287,1955;PD1600,1052;"),
        bitmap_location: None,
        min_distance: 1400,
        max_distance: 10000,
    },
    PatternDef {
        name: "DQUALA21",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 1697,
        height: 1184,
        pivot_x: 2401,
        pivot_y: 1247,
        origin_x: 1570,
        origin_y: 856,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW1;PU2779,1066;PD3080,1066;SPA;SW1;PU3021,947;PD2840,1187;SPA;SW1;PU2841,947;PD3021,1186;SPA;SW1;PU2257,1066;PD2558,1066;SPA;SW1;PU2499,947;PD2318,1187;SPA;SW1;PU2319,947;PD2499,1186;SPA;SW1;PU1728,1071;PD2029,1071;SPA;SW1;PU1970,952;PD1789,1192;SPA;SW1;PU1790,952;PD1970,1191;SPA;SW1;PU2009,1420;PD2310,1420;SPA;SW1;PU2251,1301;PD2070,1541;SPA;SW1;PU2071,1301;PD2251,1540;SPA;SW1;PU2537,1415;PD2838,1415;SPA;SW1;PU2779,1296;PD2598,1536;SPA;SW1;PU2599,1296;PD2779,1535;SPA;SW1;PU1600,1052;PD1570,971;PD1581,917;PD1600,879;PD1647,859;PD1678,856;PD3105,856;PD3186,863;PD3236,886;PD3256,914;PD3267,944;PD3267,968;PD3267,1006;PD3248,1037;PD2561,1951;PD2534,1982;PD2507,2013;PD2480,2029;PD2461,2032;PD2442,2040;PD2407,2036;PD2388,2032;PD2349,2021;PD2330,2005;PD2310,1982;PD2287,1955;PD1600,1052;"),
        bitmap_location: None,
        min_distance: 1400,
        max_distance: 10000,
    },
    PatternDef {
        name: "DQUALB01",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 1697,
        height: 1184,
        pivot_x: 2401,
        pivot_y: 1247,
        origin_x: 1570,
        origin_y: 856,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW1;PU2779,1066;PD3080,1066;SPA;SW1;PU3021,947;PD2840,1187;SPA;SW1;PU2841,947;PD3021,1186;SPA;SW1;PU2257,1066;PD2558,1066;SPA;SW1;PU2499,947;PD2318,1187;SPA;SW1;PU2319,947;PD2499,1186;SPA;SW1;PU1728,1071;PD2029,1071;SPA;SW1;PU1970,952;PD1789,1192;SPA;SW1;PU1790,952;PD1970,1191;SPA;SW1;PU2253,1459;PD2554,1459;SPA;SW1;PU2495,1340;PD2314,1580;SPA;SW1;PU2315,1340;PD2495,1579;SPA;SW1;PU1600,1052;PD1570,971;PD1581,917;PD1600,879;PD1647,859;PD1678,856;PD3105,856;PD3186,863;PD3236,886;PD3256,914;PD3267,944;PD3267,968;PD3267,1006;PD3248,1037;PD2561,1951;PD2534,1982;PD2507,2013;PD2480,2029;PD2461,2032;PD2442,2040;PD2407,2036;PD2388,2032;PD2349,2021;PD2330,2005;PD2310,1982;PD2287,1955;PD1600,1052;"),
        bitmap_location: None,
        min_distance: 1400,
        max_distance: 10000,
    },
    PatternDef {
        name: "DQUALC01",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 1604,
        height: 430,
        pivot_x: 2407,
        pivot_y: 1062,
        origin_x: 1591,
        origin_y: 837,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW1;PU3021,947;PD2840,1187;SPA;SW1;PU2841,947;PD3021,1186;SPA;SW1;PU2499,947;PD2318,1187;SPA;SW1;PU2319,947;PD2499,1186;SPA;SW1;PU1970,952;PD1789,1192;SPA;SW1;PU1790,952;PD1970,1191;SPA;SW1;PU1783,837;PD1738,851;PD1703,865;PD1668,890;PD1640,925;PD1615,967;PD1601,1005;PD1591,1054;PD1598,1106;PD1608,1148;PD1626,1194;PD1671,1232;PD1727,1257;PD1776,1267;SPA;SW1;PU1769,841;PD3024,841;PD3066,855;PD3118,883;PD3150,914;PD3178,953;PD3185,981;PD3192,1019;PD3195,1054;PD3192,1099;PD3185,1141;PD3167,1183;PD3150,1208;PD3111,1243;PD3083,1260;PD3059,1267;PD3024,1264;PD1773,1264;SPA;SW1;PU1731,1069;PD2031,1069;SPA;SW1;PU2780,1066;PD3080,1066;SPA;SW1;PU2249,1064;PD2549,1064;"),
        bitmap_location: None,
        min_distance: 1600,
        max_distance: 10000,
    },
    PatternDef {
        name: "DQUALD01",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 1604,
        height: 430,
        pivot_x: 2268,
        pivot_y: 1078,
        origin_x: 1466,
        origin_y: 843,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW1;PU2739,942;PD2558,1182;SPA;SW1;PU2559,942;PD2739,1181;SPA;SW1;PU1970,952;PD1789,1192;SPA;SW1;PU1790,952;PD1970,1191;SPA;SW1;PU1658,843;PD1613,857;PD1578,871;PD1543,896;PD1515,931;PD1490,973;PD1476,1011;PD1466,1060;PD1473,1112;PD1483,1154;PD1501,1200;PD1546,1238;PD1602,1263;PD1651,1273;SPA;SW1;PU1644,847;PD2899,847;PD2941,861;PD2993,889;PD3025,920;PD3053,959;PD3060,987;PD3067,1025;PD3070,1060;PD3067,1105;PD3060,1147;PD3042,1189;PD3025,1214;PD2986,1249;PD2958,1266;PD2934,1273;PD2899,1270;PD1648,1270;SPA;SW1;PU1734,1069;PD2034,1067;SPA;SW1;PU2493,1064;PD2815,1064;"),
        bitmap_location: None,
        min_distance: 1600,
        max_distance: 10000,
    },
    PatternDef {
        name: "DQUALU01",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 1604,
        height: 430,
        pivot_x: 2927,
        pivot_y: 1059,
        origin_x: 2124,
        origin_y: 841,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW1;PU2316,841;PD2271,855;PD2236,869;PD2201,894;PD2173,929;PD2148,971;PD2134,1009;PD2124,1058;PD2131,1110;PD2141,1152;PD2159,1198;PD2204,1236;PD2260,1261;PD2309,1271;SPA;SW1;PU2302,845;PD3557,845;PD3599,859;PD3651,887;PD3683,918;PD3711,957;PD3718,985;PD3725,1023;PD3728,1058;PD3725,1103;PD3718,1145;PD3700,1187;PD3683,1212;PD3644,1247;PD3616,1264;PD3592,1271;PD3557,1268;PD2306,1268;SPA;SW1;PU2841,946;PD2842,1137;PD2852,1166;PD2873,1186;PD2904,1201;PD2938,1203;PD2961,1193;PD2990,1175;PD3008,1158;PD3018,1134;PD3018,946;"),
        bitmap_location: None,
        min_distance: 1600,
        max_distance: 10000,
    },
    PatternDef {
        name: "DRGARE01",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Linear,
        width: 200,
        height: 200,
        pivot_x: 1500,
        pivot_y: 1500,
        origin_x: 1500,
        origin_y: 1300,
        color_ref: "CHGRD", // Note: Original has D prefix
        hpgl: Some("SPD;SW1;PU1500,1300;PD;PU1700,1500;PD;"),
        bitmap_location: None,
        min_distance: 150,
        max_distance: 0,
    },
    PatternDef {
        name: "FOULAR11",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Linear,
        width: 570,
        height: 684,
        pivot_x: 837,
        pivot_y: 728,
        origin_x: 1020,
        origin_y: 850,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW1;PU1590,1294;PD1409,1534;SPA;SW1;PU1410,1294;PD1590,1533;SPA;SW1;PU1022,850;PD1200,1090;SPA;SW1;PU1196,853;PD1020,1093;"),
        bitmap_location: None,
        min_distance: 150,
        max_distance: 10000,
    },
    PatternDef {
        name: "FSHFAC03",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 604,
        height: 151,
        pivot_x: 4643,
        pivot_y: 2168,
        origin_x: 3135,
        origin_y: 2173,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW1;PU3135,2173;PD3135,2323;PD3739,2323;PD3739,2180;SPA;SW1;PU3290,2176;PD3290,2324;SPA;SW1;PU3438,2179;PD3438,2321;SPA;SW1;PU3590,2179;PD3590,2321;"),
        bitmap_location: None,
        min_distance: 2000,
        max_distance: 10000,
    },
    PatternDef {
        name: "FSHFAC04",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 511,
        height: 438,
        pivot_x: 1753,
        pivot_y: 189,
        origin_x: 492,
        origin_y: 416,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW1;PU492,854;PD1003,854;PD1003,632;PD494,632;PD494,853;SPA;SW1;PU558,416;PD776,634;"),
        bitmap_location: Some((257, 1007)), // Also has 40x40 bitmap
        min_distance: 2000,
        max_distance: 10000,
    },
    PatternDef {
        name: "FSHHAV02",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 790,
        height: 320,
        pivot_x: 2200,
        pivot_y: 1470,
        origin_x: 710,
        origin_y: 1335,
        color_ref: "CHGRD", // Note: Original has E prefix
        hpgl: Some("SPE;SW1;PU835,1490;PD725,1355;PU835,1500;PD710,1625;PU835,1500;PD890,1440;PD960,1390;PD1050,1350;PD1150,1335;PD1240,1340;PD1340,1360;PD1425,1400;PD1485,1445;PD1500,1475;PU840,1495;PD925,1565;PD1030,1625;PD1140,1650;PD1220,1655;PD1305,1640;PD1385,1610;PD1450,1550;PD1500,1480;PU1365,1380;PD1340,1445;PD1340,1535;PD1385,1600;"),
        bitmap_location: Some((305, 1007)), // Also has 40x40 bitmap
        min_distance: 2000,
        max_distance: 10000,
    },
    PatternDef {
        name: "ICEARE04",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Linear,
        width: 1434,
        height: 1331,
        pivot_x: 2250,
        pivot_y: 2250,
        origin_x: 2381,
        origin_y: 815,
        color_ref: "CHGRD", // Note: Original has E prefix
        hpgl: Some("SPE;SW1;PU2559,2146;PD2775,1978;PU2381,1153;PD2596,1396;PU2981,1537;PD3253,1603;PU2953,1059;PD3028,815;PU3131,2043;PD3412,1959;PU3665,1593;PD3731,1865;PU3553,1125;PD3815,1106;"),
        bitmap_location: None,
        min_distance: 0,
        max_distance: 0,
    },
    PatternDef {
        name: "MARCUL02",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 705,
        height: 403,
        pivot_x: 2409,
        pivot_y: 369,
        origin_x: 416,
        origin_y: 568,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW1;PU505,847;PD641,725;PD714,684;PD771,668;PD822,662;PD882,673;PD944,693;PD984,717;PD1018,744;PD1039,774;PD1004,803;PD968,831;PD914,855;PD871,869;PD819,871;PD763,869;PD709,857;PD670,835;PD505,698;SPA;SW1;PU416,669;PD416,570;PD1117,570;PD1117,666;SPA;SW1;PU416,871;PD416,971;PD1120,971;PD1121,871;SPA;SW1;PU564,871;PD564,971;SPA;SW1;PU965,871;PD965,971;SPA;SW1;PU765,870;PD765,968;SPA;SW1;PU564,571;PD564,672;SPA;SW1;PU965,569;PD965,670;SPA;SW1;PU764,568;PD764,668;"),
        bitmap_location: None,
        min_distance: 2000,
        max_distance: 10000,
    },
    PatternDef {
        name: "MARSHES1",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 400,
        height: 393,
        pivot_x: 750,
        pivot_y: 1016,
        origin_x: 550,
        origin_y: 499,
        color_ref: "CHBRN", // Note: Original has A prefix
        hpgl: Some("SPA;SW2;PU751,765;PD751,499;SPA;SW2;PU626,892;PD876,892;SPA;SW2;PU550,810;PD950,810;SPA;SW2;PU664,799;PD592,634;SPA;SW2;PU830,799;PD901,637;"),
        bitmap_location: Some((207, 1007)), // Also has 40x40 bitmap
        min_distance: 1500,
        max_distance: 15000,
    },
    PatternDef {
        name: "NODATA03",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 602,
        height: 396,
        pivot_x: 2942,
        pivot_y: 1040,
        origin_x: 2342,
        origin_y: 645,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW2;PU2342,1041;PD2542,1040;"),
        bitmap_location: None,
        min_distance: 100,
        max_distance: 10000,
    },
    PatternDef {
        name: "OVERSC01",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Linear,
        width: 0, // Note: Width is 0 - vertical line pattern
        height: 400,
        pivot_x: 4243,
        pivot_y: 1227,
        origin_x: 3443,
        origin_y: 827,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW1;PU3443,827;PD3443,1227;"),
        bitmap_location: None,
        min_distance: 0,
        max_distance: 10000,
    },
    PatternDef {
        name: "PRTSUR01",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 201,
        height: 0, // Note: Height is 0 - horizontal line pattern
        pivot_x: 2347,
        pivot_y: 640,
        origin_x: 2448,
        origin_y: 741,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW2;PU2448,741;PD2650,741;"),
        bitmap_location: None,
        min_distance: 1000,
        max_distance: 10000,
    },
    PatternDef {
        name: "QUESMRK1",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 220,
        height: 443,
        pivot_x: 1676,
        pivot_y: 1501,
        origin_x: 1568,
        origin_y: 1227,
        color_ref: "CHMGD", // Note: Original has A prefix
        hpgl: Some("SPA;SW1;PU1568,1323;PD1581,1295;PD1594,1270;PD1611,1253;PD1628,1236;PD1654,1227;PD1677,1229;PD1707,1238;PD1735,1244;PD1758,1278;PD1776,1297;PD1788,1319;PD1788,1344;PD1776,1368;PD1763,1389;PD1743,1413;PD1720,1438;PD1699,1464;PD1686,1483;PD1675,1500;PD1673,1522;PD1673,1545;PD1673,1562;PD1675,1584;PD1675,1586;SPA;SW2;PU1654,1670;PD1707,1670;"),
        bitmap_location: None,
        min_distance: 2000,
        max_distance: 10000,
    },
    PatternDef {
        name: "RCKLDG01",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Linear,
        width: 1541,
        height: 1332,
        pivot_x: 2319,
        pivot_y: 2152,
        origin_x: 2325,
        origin_y: 814,
        color_ref: "ALANDF",
        hpgl: Some("SPA;SW1;PU2559,2146;PD2775,1978;SPA;SW1;PU2490,1030;PD2593,1337;SPA;SW1;PU3102,1412;PD2942,1642;SPA;SW1;PU2953,1059;PD3028,815;SPA;SW1;PU3194,1797;PD3244,2086;SPA;SW1;PU3751,1761;PD3866,1506;SPA;SW1;PU3768,1160;PD3518,1079;SPA;SW1;PU2593,1331;PD2325,1193;SPA;SW1;PU2951,1640;PD2924,1374;SPA;SW1;PU2554,2146;PD2583,1889;SPA;SW1;PU3197,1797;PD3409,1967;SPA;SW1;PU3859,1507;PD3595,1618;SPA;SW1;PU3522,1083;PD3727,948;SPA;SW1;PU3028,814;PD3138,1049;"),
        bitmap_location: None,
        min_distance: 0,
        max_distance: 0,
    },
    PatternDef {
        name: "SNDWAV01",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 1179,
        height: 179,
        pivot_x: 29,
        pivot_y: 499,
        origin_x: 37,
        origin_y: 316,
        color_ref: "ACHGRD",
        hpgl: Some("SPA;SW1;PU37,495;PD121,495;PD163,473;PD228,328;PD263,328;PD320,473;PD347,495;PD523,495;PD554,468;PD600,331;PD626,331;PD677,468;PD710,495;PD856,495;PD906,495;PD937,476;PD998,316;PD1036,316;PD1093,473;PD1132,495;PD1216,495;"),
        bitmap_location: None,
        min_distance: 2000,
        max_distance: 10000,
    },
    PatternDef {
        name: "TSSJCT02",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Linear,
        width: 500,
        height: 500,
        pivot_x: 750,
        pivot_y: 750,
        origin_x: 750,
        origin_y: 250,
        color_ref: "TRFCF", // Note: Original has U prefix
        hpgl: Some("SPU;SW1;PU750,750;PD1250,250;"),
        bitmap_location: None,
        min_distance: 0,
        max_distance: 0,
    },
    PatternDef {
        name: "VEGATN03",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 362,
        height: 399,
        pivot_x: 749,
        pivot_y: 746,
        origin_x: 574,
        origin_y: 348,
        color_ref: "ALANDF",
        hpgl: Some("SPA;SW1;PU750,747;PD750,350;SPA;SW1;PU574,447;PD936,447;SPA;SW1;PU641,545;PD858,545;SPA;SW1;PU684,348;PD814,348;SPA;SW1;PU695,747;PD815,747;SPA;SW1;PU638,396;PD858,396;SPA;SW1;PU621,494;PD883,494;"),
        bitmap_location: None,
        min_distance: 1000,
        max_distance: 10000,
    },
    PatternDef {
        name: "VEGATN04",
        pattern_type: PatternType::Vector,
        fill_type: FillType::Staggered,
        width: 400,
        height: 300,
        pivot_x: 750,
        pivot_y: 750,
        origin_x: 550,
        origin_y: 450,
        color_ref: "ALANDF",
        hpgl: Some("SPA;SW1;PU750,600;CI150;SPA;SW1;PU550,750;PD950,750;SPA;SW1;PU750,748;PD750,587;"),
        bitmap_location: None,
        min_distance: 1000,
        max_distance: 10000,
    },

    // ========== RASTER PATTERNS (bitmap only) ==========

    PatternDef {
        name: "NODATA04",
        pattern_type: PatternType::Raster,
        fill_type: FillType::Staggered,
        width: 48,
        height: 24,
        pivot_x: 20,
        pivot_y: 0,
        origin_x: 0,
        origin_y: 0,
        color_ref: "ACHGRD",
        hpgl: None,
        bitmap_location: Some((68, 1007)),
        min_distance: 3,
        max_distance: 326,
    },
    PatternDef {
        name: "FOULAR02",
        pattern_type: PatternType::Raster,
        fill_type: FillType::Linear,
        width: 73,
        height: 142,
        pivot_x: 36,
        pivot_y: 69,
        origin_x: 0,
        origin_y: 0,
        color_ref: "ACHGRD",
        hpgl: None,
        bitmap_location: Some((121, 1007)),
        min_distance: 0,
        max_distance: 0,
    },
    PatternDef {
        name: "FOULAR01",
        pattern_type: PatternType::Raster,
        fill_type: FillType::Linear,
        width: 32,
        height: 32,
        pivot_x: 5,
        pivot_y: 5,
        origin_x: 0,
        origin_y: 0,
        color_ref: "ACHGRD",
        hpgl: None,
        bitmap_location: Some((68, 1039)),
        min_distance: 0,
        max_distance: 0,
    },
    PatternDef {
        name: "CROSSX01",
        pattern_type: PatternType::Raster,
        fill_type: FillType::Linear,
        width: 16,
        height: 16,
        pivot_x: 8,
        pivot_y: 8,
        origin_x: 0,
        origin_y: 0,
        color_ref: "CHBRN", // Note: Original has A prefix
        hpgl: None,
        bitmap_location: Some((400, 1040)),
        min_distance: 0,
        max_distance: 0,
    },
    PatternDef {
        name: "CROSSX02",
        pattern_type: PatternType::Raster,
        fill_type: FillType::Linear,
        width: 16,
        height: 16,
        pivot_x: 8,
        pivot_y: 8,
        origin_x: 0,
        origin_y: 0,
        color_ref: "CHBRN", // Note: Original has A prefix
        hpgl: None,
        bitmap_location: Some((430, 1040)),
        min_distance: 0,
        max_distance: 0,
    },
];

/// Look up a pattern by name
pub fn get_pattern(name: &str) -> Option<&'static PatternDef> {
    PATTERNS.iter().find(|p| p.name == name)
}

/// Total number of patterns
pub const PATTERN_COUNT: usize = 30;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pattern_count() {
        assert_eq!(PATTERNS.len(), PATTERN_COUNT);
    }

    #[test]
    fn test_lookup_diamond1() {
        let p = get_pattern("DIAMOND1").expect("DIAMOND1 should exist");
        assert_eq!(p.name, "DIAMOND1");
        assert_eq!(p.fill_type, FillType::Linear);
        assert_eq!(p.width, 2250);
        assert_eq!(p.height, 4313);
        assert!(!p.is_staggered());
        assert_eq!(p.stagger_factor(), 0.0);
    }

    #[test]
    fn test_lookup_drgare01() {
        let p = get_pattern("DRGARE01").expect("DRGARE01 should exist");
        assert_eq!(p.name, "DRGARE01");
        assert_eq!(p.fill_type, FillType::Linear);
        assert_eq!(p.width, 200);
        assert_eq!(p.height, 200);
    }

    #[test]
    fn test_staggered_pattern() {
        let p = get_pattern("DQUALA11").expect("DQUALA11 should exist");
        assert!(p.is_staggered());
        assert_eq!(p.stagger_factor(), 0.5);
    }

    #[test]
    fn test_raster_pattern() {
        let p = get_pattern("CROSSX01").expect("CROSSX01 should exist");
        assert_eq!(p.pattern_type, PatternType::Raster);
        assert!(p.bitmap_location.is_some());
        assert!(p.hpgl.is_none());
    }

    #[test]
    fn test_vector_with_bitmap() {
        let p = get_pattern("FSHFAC04").expect("FSHFAC04 should exist");
        assert_eq!(p.pattern_type, PatternType::Vector);
        assert!(p.bitmap_location.is_some()); // Has both
        assert!(p.hpgl.is_some());
    }
}
