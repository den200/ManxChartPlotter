//! Parser for OpenCPN chartsymbols.xml

use std::fs;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::lookup::{DisplayCategory, DisplayPriority, GeometryType, LookupEntry, LookupTables, TableName};

/// Parse chartsymbols.xml and return lookup tables.
pub fn parse_chartsymbols<P: AsRef<Path>>(path: P) -> Result<LookupTables, String> {
    let content = fs::read_to_string(path.as_ref())
        .map_err(|e| format!("Failed to read chartsymbols.xml: {}", e))?;

    let mut tables = LookupTables::new();
    let mut reader = Reader::from_str(&content);
    reader.trim_text(true);

    // State for current color table (parse all palettes)
    let mut current_palette: Option<String> = None;

    // State for current lookup entry
    let mut in_lookup = false;
    let mut current_name = String::new();
    let mut current_seq: u32 = 0;
    let mut current_table_name: Option<TableName> = None;
    let mut current_type: Option<GeometryType> = None;
    let mut current_prio: Option<DisplayPriority> = None;
    let mut current_cat: Option<DisplayCategory> = None;
    let mut current_attribs: Vec<String> = Vec::new();
    let mut current_instruction = String::new();
    let mut current_comment: Option<String> = None;

    // State for text content
    let mut current_element = String::new();

    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                current_element = name.clone();

                match name.as_str() {
                    "color-table" => {
                        // Track which palette we're in
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"name" {
                                let value = String::from_utf8_lossy(&attr.value).to_string();
                                current_palette = Some(value);
                            }
                        }
                    }
                    "lookup" => {
                        in_lookup = true;
                        current_name.clear();
                        current_table_name = None;
                        current_type = None;
                        current_prio = None;
                        current_cat = None;
                        current_attribs.clear();
                        current_instruction.clear();
                        current_comment = None;

                        // Extract name and id ("nSequence" in OpenCPN terms)
                        current_seq = 0;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"name" => {
                                    current_name =
                                        String::from_utf8_lossy(&attr.value).to_string()
                                }
                                b"id" => {
                                    current_seq = String::from_utf8_lossy(&attr.value)
                                        .parse()
                                        .unwrap_or(0)
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();

                // Handle <color name="..." r="..." g="..." b="..."/>
                if name == "color" && current_palette.is_some() {
                    let mut color_name = String::new();
                    let mut r: u8 = 0;
                    let mut g: u8 = 0;
                    let mut b: u8 = 0;

                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"name" => {
                                color_name = String::from_utf8_lossy(&attr.value).to_string()
                            }
                            b"r" => {
                                r = String::from_utf8_lossy(&attr.value)
                                    .parse()
                                    .unwrap_or(0)
                            }
                            b"g" => {
                                g = String::from_utf8_lossy(&attr.value)
                                    .parse()
                                    .unwrap_or(0)
                            }
                            b"b" => {
                                b = String::from_utf8_lossy(&attr.value)
                                    .parse()
                                    .unwrap_or(0)
                            }
                            _ => {}
                        }
                    }

                    if !color_name.is_empty() {
                        let palette_name = current_palette.as_deref().unwrap_or("DAY_BRIGHT");
                        tables.add_palette_color(palette_name, &color_name, r, g, b);
                    }
                }

                // Handle <attrib-code index="0">CATSLC1</attrib-code> as empty tag
                if name == "attrib-code" && in_lookup {
                    // Some attrib-codes are empty, which means no filter
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_lookup {
                    let text = e.unescape().map(|s| s.to_string()).unwrap_or_default();

                    match current_element.as_str() {
                        "table-name" => {
                            current_table_name = TableName::from_str(&text);
                        }
                        "type" => {
                            current_type = GeometryType::from_str(&text);
                        }
                        "disp-prio" => {
                            current_prio = DisplayPriority::from_str(&text);
                        }
                        "display-cat" => {
                            current_cat = DisplayCategory::from_str(&text);
                        }
                        "attrib-code" => {
                            if !text.is_empty() {
                                current_attribs.push(text);
                            }
                        }
                        "instruction" => {
                            current_instruction = text;
                        }
                        "comment" => {
                            current_comment = Some(text);
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();

                match name.as_str() {
                    "color-table" => {
                        current_palette = None;
                    }
                    "lookup" => {
                        // Save the entry if we have required fields
                        // <display-cat /> and <disp-prio /> appear empty in a few
                        // rows (M_NPUB, M_VDAT, M_SDAT, SMCFAC): no text event
                        // fires, so the field stays None. chartsymbols.cpp
                        // defaults both — OTHER and "no data" — rather than
                        // dropping the row, which is what left those classes
                        // with no lookup at all.
                        let prio = current_prio.unwrap_or(DisplayPriority::NoData);
                        let cat = current_cat.unwrap_or(DisplayCategory::Other);
                        if let (Some(table_name), Some(geom_type)) =
                            (current_table_name, current_type)
                        {
                            // An empty <instruction> is a real lookup result —
                            // it means "render nothing" (e.g. TOPMAR in the
                            // Simplified table, whose topmarks are baked into
                            // the buoy symbol). Dropping those rows also breaks
                            // the S-52 fallback "first LUP with no attributes",
                            // which then lands on an arbitrary attributed row.
                            if !current_name.is_empty() {
                                tables.add_entry(LookupEntry {
                                    object_class: current_name.clone(),
                                    table_name,
                                    geometry_type: geom_type,
                                    display_priority: prio,
                                    display_category: cat,
                                    attribute_codes: current_attribs.clone(),
                                    instruction: current_instruction.clone(),
                                    comment: current_comment.clone(),
                                    seq: current_seq,
                                });
                            }
                        }
                        in_lookup = false;
                    }
                    _ => {}
                }

                current_element.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    tables.finalize_palettes();
    tables.finalize_lookups();

    Ok(tables)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "assets/s52/chartsymbols.xml";

    #[test]
    fn test_parse_chartsymbols() {
        let tables = parse_chartsymbols(PATH).expect("Failed to parse");

        // Check we got some data
        assert!(tables.entry_count() > 100, "Expected many lookup entries");
        assert!(tables.color_count() > 30, "Expected many colors");
        assert_eq!(tables.palette_color_count(), 63, "Expected 63 color tokens");
        let mut palettes = tables.palette_names();
        palettes.sort_unstable();
        assert_eq!(palettes, ["DAY_BLACKBACK", "DAY_BRIGHT", "DAY_WHITEBACK", "DUSK", "NIGHT"]);

        // Check specific color
        let landa = tables.get_color("LANDA").expect("LANDA color missing");
        assert_eq!(landa, [201, 185, 122]);

        // Check color index
        let landa_idx = tables.get_color_index("LANDA").expect("LANDA index missing");
        assert!(landa_idx < 63);

        // Check palette f32 output
        let palette = tables.active_palette_f32();
        assert_eq!(palette.len(), 63);
        let landa_f32 = palette[landa_idx as usize];
        assert!((landa_f32[0] - 201.0/255.0).abs() < 0.01);
    }

    /// Each palette keeps its own values under the same index: the dusk and
    /// night tables must not be overwritten by the day one.
    #[test]
    fn each_palette_keeps_its_own_colours() {
        let mut tables = parse_chartsymbols(PATH).unwrap();
        let idx = tables.get_color_index("LANDA").unwrap() as usize;
        let rgb = |p: &[[f32; 4]]| p[idx].map(|c| (c * 255.0).round() as u8);
        assert_eq!(rgb(&tables.palette_f32("DUSK"))[..3], [44, 41, 27]);
        assert_eq!(rgb(&tables.palette_f32("NIGHT"))[..3], [13, 10, 8]);

        tables.set_active_palette("NIGHT");
        assert_eq!(tables.get_color("LANDA"), Some([13, 10, 8]));
        assert_eq!(rgb(&tables.active_palette_f32())[..3], [13, 10, 8]);
    }

    #[test]
    fn lookup_rows_carry_every_field() {
        let tables = parse_chartsymbols(PATH).unwrap();
        let rows = tables
            .lookup("DEPARE", GeometryType::Area, TableName::Plain)
            .expect("DEPARE lookup missing");
        // DEPARE has the DRVAL1?/DRVAL2? "no data" row and the CS row.
        assert_eq!(rows.len(), 2);
        let attributed = &rows[0];
        assert_eq!(attributed.attribute_codes, ["DRVAL1?", "DRVAL2?"]);
        assert_eq!(attributed.instruction, "AC(NODTA);AP(PRTSUR01);LS(SOLD,2,CHGRD)");
        assert_eq!(attributed.display_category, DisplayCategory::Displaybase);
        let cs = &rows[1];
        assert!(cs.attribute_codes.is_empty());
        assert_eq!(cs.instruction, "CS(DEPARE01)");
        assert_eq!(cs.display_priority, DisplayPriority::Group1);

        let slcons = tables
            .lookup("SLCONS", GeometryType::Line, TableName::Lines)
            .expect("SLCONS lookup missing");
        assert!(slcons.iter().any(|e| e.instruction == "CS(SLCONS03)"));
    }

    /// Rows with an empty `<display-cat />` (M_NPUB and friends) default to
    /// OTHER rather than being dropped, and an empty `<instruction>` is kept
    /// as "draw nothing".
    #[test]
    fn empty_fields_keep_the_row() {
        let tables = parse_chartsymbols(PATH).unwrap();
        let npub = tables
            .lookup("M_NPUB", GeometryType::Area, TableName::Plain)
            .expect("M_NPUB row dropped");
        assert_eq!(npub[0].display_category, DisplayCategory::Other);
        assert_eq!(npub[0].display_priority, DisplayPriority::NoData);
        assert_eq!(npub[0].instruction, "");

        let topmar = tables
            .lookup("TOPMAR", GeometryType::Point, TableName::Simplified)
            .expect("TOPMAR Simplified rows");
        assert!(topmar.iter().any(|e| e.instruction.is_empty() && e.attribute_codes.is_empty()));
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(parse_chartsymbols("assets/s52/no-such-file.xml").is_err());
    }
}
