//! How each S-57 attribute is typed, for the SENC encoder.
//!
//! Generated from the S-57 attribute catalogue (`s57attributes.csv`, as
//! OpenCPN ships it): enumerated (E) and integer (I) attributes are written
//! as integers, float (F) ones as doubles. Everything else — lists (L),
//! free text (A, S) — is a string, lists comma-joined, which is the one
//! list encoding navcore's SENC reader keeps.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrType {
    Int,
    Float,
    Str,
}

/// Numeric attributes by code, sorted for binary search.
const NUMERIC: &[(u16, AttrType)] = &[
    (2, AttrType::Int), // BCNSHP
    (3, AttrType::Int), // BUISHP
    (4, AttrType::Int), // BOYSHP
    (5, AttrType::Float), // BURDEP
    (10, AttrType::Int), // CATBUA
    (11, AttrType::Int), // CATCBL
    (12, AttrType::Int), // CATCAN
    (13, AttrType::Int), // CATCAM
    (14, AttrType::Int), // CATCHP
    (15, AttrType::Int), // CATCOA
    (16, AttrType::Int), // CATCTR
    (17, AttrType::Int), // CATCON
    (18, AttrType::Int), // CATCOV
    (19, AttrType::Int), // CATCRN
    (20, AttrType::Int), // CATDAM
    (21, AttrType::Int), // CATDIS
    (22, AttrType::Int), // CATDOC
    (24, AttrType::Int), // CATFNC
    (25, AttrType::Int), // CATFRY
    (26, AttrType::Int), // CATFIF
    (27, AttrType::Int), // CATFOG
    (28, AttrType::Int), // CATFOR
    (29, AttrType::Int), // CATGAT
    (32, AttrType::Int), // CATICE
    (33, AttrType::Int), // CATINB
    (36, AttrType::Int), // CATLAM
    (38, AttrType::Int), // CATMFA
    (40, AttrType::Int), // CATMOR
    (41, AttrType::Int), // CATNAV
    (42, AttrType::Int), // CATOBS
    (44, AttrType::Int), // CATOLB
    (45, AttrType::Int), // CATPLE
    (46, AttrType::Int), // CATPIL
    (48, AttrType::Int), // CATPRA
    (49, AttrType::Int), // CATPYL
    (50, AttrType::Int), // CATQUA
    (51, AttrType::Int), // CATRAS
    (52, AttrType::Int), // CATRTB
    (54, AttrType::Int), // CATTRK
    (57, AttrType::Int), // CATROD
    (58, AttrType::Int), // CATRUN
    (59, AttrType::Int), // CATSEA
    (60, AttrType::Int), // CATSLC
    (63, AttrType::Int), // CATSIL
    (64, AttrType::Int), // CATSLO
    (67, AttrType::Int), // CATTSS
    (69, AttrType::Int), // CATWAT
    (70, AttrType::Int), // CATWED
    (71, AttrType::Int), // CATWRK
    (72, AttrType::Int), // CATZOC
    (73, AttrType::Int), // $SPACE
    (78, AttrType::Float), // $CSIZE
    (80, AttrType::Int), // CSCALE
    (81, AttrType::Int), // CONDTN
    (82, AttrType::Int), // CONRAD
    (83, AttrType::Int), // CONVIS
    (84, AttrType::Float), // CURVEL
    (87, AttrType::Float), // DRVAL1
    (88, AttrType::Float), // DRVAL2
    (89, AttrType::Int), // DUNITS
    (90, AttrType::Float), // ELEVAT
    (91, AttrType::Float), // ESTRNG
    (92, AttrType::Int), // EXCLIT
    (93, AttrType::Int), // EXPSOU
    (95, AttrType::Float), // HEIGHT
    (96, AttrType::Int), // HUNITS
    (97, AttrType::Float), // HORACC
    (98, AttrType::Float), // HORCLR
    (99, AttrType::Float), // HORLEN
    (100, AttrType::Float), // HORWID
    (101, AttrType::Float), // ICEFAC
    (103, AttrType::Int), // JRSDTN
    (104, AttrType::Int), // $JUSTH
    (105, AttrType::Int), // $JUSTV
    (106, AttrType::Float), // LIFCAP
    (107, AttrType::Int), // LITCHR
    (109, AttrType::Int), // MARSYS
    (110, AttrType::Int), // MLTYLT
    (117, AttrType::Float), // ORIENT
    (127, AttrType::Float), // RADIUS
    (132, AttrType::Int), // SCAMAX
    (133, AttrType::Int), // SCAMIN
    (134, AttrType::Int), // SCVAL1
    (135, AttrType::Int), // SCVAL2
    (136, AttrType::Float), // SECTR1
    (137, AttrType::Float), // SECTR2
    (139, AttrType::Int), // SIGFRQ
    (140, AttrType::Int), // SIGGEN
    (142, AttrType::Float), // SIGPER
    (144, AttrType::Float), // SOUACC
    (145, AttrType::Int), // SDISMX
    (146, AttrType::Int), // SDISMN
    (154, AttrType::Float), // $SCALE
    (161, AttrType::Int), // T_ACWL
    (163, AttrType::Int), // T_MTOD
    (165, AttrType::Int), // T_TINT
    (170, AttrType::Int), // $TINTS
    (171, AttrType::Int), // TOPSHP
    (172, AttrType::Int), // TRAFIC
    (173, AttrType::Float), // VALACM
    (174, AttrType::Float), // VALDCO
    (175, AttrType::Float), // VALLMA
    (176, AttrType::Float), // VALMAG
    (177, AttrType::Float), // VALMXR
    (178, AttrType::Float), // VALNMR
    (179, AttrType::Float), // VALSOU
    (180, AttrType::Float), // VERACC
    (181, AttrType::Float), // VERCLR
    (182, AttrType::Float), // VERCCL
    (183, AttrType::Float), // VERCOP
    (184, AttrType::Float), // VERCSA
    (185, AttrType::Int), // VERDAT
    (186, AttrType::Float), // VERLEN
    (187, AttrType::Int), // WATLEV
    (188, AttrType::Int), // CAT_TS
    (189, AttrType::Int), // PUNITS
    (400, AttrType::Int), // HORDAT
    (401, AttrType::Float), // POSACC
    (402, AttrType::Int), // QUAPOS
    (17001, AttrType::Int), // catdis
    (17005, AttrType::Int), // verdat
    (17007, AttrType::Int), // catfry
    (17009, AttrType::Int), // marsys
    (17011, AttrType::Int), // catlam
    (17012, AttrType::Int), // catslc
    (17051, AttrType::Int), // catbnk
    (17052, AttrType::Int), // catnmk
    (17055, AttrType::Int), // clsdng
    (17057, AttrType::Float), // disbk1
    (17058, AttrType::Float), // disbk2
    (17059, AttrType::Float), // disipu
    (17060, AttrType::Float), // disipd
    (17061, AttrType::Float), // eleva1
    (17062, AttrType::Float), // eleva2
    (17063, AttrType::Int), // fnctnm
    (17064, AttrType::Float), // wtwdis
    (17065, AttrType::Int), // bunves
    (17074, AttrType::Float), // horcll
    (17075, AttrType::Float), // horclw
    (17080, AttrType::Float), // higwat
    (17082, AttrType::Float), // lowwat
    (17084, AttrType::Float), // meawat
    (17086, AttrType::Float), // othwat
    (17088, AttrType::Int), // reflev
    (17092, AttrType::Int), // cattab
    (17094, AttrType::Int), // useshp
    (17095, AttrType::Float), // curvhw
    (17096, AttrType::Float), // curvlw
    (17097, AttrType::Float), // curvmw
    (17098, AttrType::Float), // curvow
    (17100, AttrType::Int), // catexs
    (17101, AttrType::Int), // catcbl
    (17103, AttrType::Int), // hunits
    (17104, AttrType::Int), // watlev
    (17112, AttrType::Int), // catwwm
    (18001, AttrType::Float), // lg_spd
    (18003, AttrType::Float), // lg_bme
    (18004, AttrType::Float), // lg_lgs
    (18005, AttrType::Float), // lg_drt
    (18006, AttrType::Float), // lg_wdp
    (18007, AttrType::Int), // lg_wdu
    (18018, AttrType::Float), // lc_bm1
    (18019, AttrType::Float), // lc_bm2
    (18020, AttrType::Float), // lc_lg1
    (18021, AttrType::Float), // lc_lg2
    (18022, AttrType::Float), // lc_dr1
    (18023, AttrType::Float), // lc_dr2
    (18024, AttrType::Float), // lc_sp1
    (18025, AttrType::Float), // lc_sp2
    (18026, AttrType::Float), // lc_wd1
    (18027, AttrType::Float), // lc_wd2
    (33066, AttrType::Int), // shptyp
    (50000, AttrType::Int), // catgeo
];

/// The type of attribute `code`; unknown codes are strings.
pub fn attr_type(code: u16) -> AttrType {
    NUMERIC
        .binary_search_by_key(&code, |(c, _)| *c)
        .map(|i| NUMERIC[i].1)
        .unwrap_or(AttrType::Str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_attributes_have_their_types() {
        assert_eq!(attr_type(87), AttrType::Float); // DRVAL1
        assert_eq!(attr_type(75), AttrType::Str); // COLOUR: a list
        assert_eq!(attr_type(133), AttrType::Int); // SCAMIN
        assert_eq!(attr_type(42), AttrType::Int); // CATOBS: enumerated
        assert_eq!(attr_type(116), AttrType::Str); // OBJNAM
    }
}
