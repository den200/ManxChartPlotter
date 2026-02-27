//! Parser for OpenCPN chartsymbols.xml

use std::fs;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::lookup::{DisplayCategory, DisplayPriority, GeometryType, LookupEntry, LookupTables};

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
                        current_type = None;
                        current_prio = None;
                        current_cat = None;
                        current_attribs.clear();
                        current_instruction.clear();
                        current_comment = None;

                        // Extract name attribute
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"name" {
                                current_name = String::from_utf8_lossy(&attr.value).to_string();
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
                        if let (Some(geom_type), Some(prio), Some(cat)) =
                            (current_type, current_prio, current_cat)
                        {
                            if !current_name.is_empty() && !current_instruction.is_empty() {
                                tables.add_entry(LookupEntry {
                                    object_class: current_name.clone(),
                                    geometry_type: geom_type,
                                    display_priority: prio,
                                    display_category: cat,
                                    attribute_codes: current_attribs.clone(),
                                    instruction: current_instruction.clone(),
                                    comment: current_comment.clone(),
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

    Ok(tables)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_chartsymbols() {
        let path = "assets/s52/chartsymbols.xml";
        if !Path::new(path).exists() {
            eprintln!("Skipping test - chartsymbols.xml not found");
            return;
        }

        let tables = parse_chartsymbols(path).expect("Failed to parse");
        println!("Loaded {} lookup entries", tables.entry_count());
        println!("Loaded {} colors", tables.color_count());
        println!("Loaded {} palette tokens, {} palettes", tables.palette_color_count(), tables.palette_names().len());

        // Check we got some data
        assert!(tables.entry_count() > 100, "Expected many lookup entries");
        assert!(tables.color_count() > 30, "Expected many colors");
        assert_eq!(tables.palette_color_count(), 63, "Expected 63 color tokens");
        assert_eq!(tables.palette_names().len(), 5, "Expected 5 palettes");

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

        // Check a lookup entry
        let entries = tables
            .lookup("SLCONS", GeometryType::Line)
            .expect("SLCONS lookup missing");
        assert!(!entries.is_empty());
    }
}
