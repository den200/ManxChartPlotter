//! S-52 render instruction parser.
//!
//! Parses instruction strings like "LS(DASH,2,CSTLN);AC(LANDA)" from chartsymbols.xml
//! into structured render commands.

/// Line pattern type for LS instructions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LinePattern {
    /// SOLD - solid line
    Solid,
    /// DASH - dashed line
    Dashed,
    /// DOTT - dotted line
    Dotted,
    /// DASD - dash-dot line
    DashDot,
}

/// Hashable key for a line style (for batching lines with identical styles)
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LineStyleKey {
    pub pattern: LinePattern,
    pub width: u8,
    pub color_token: String,
}

impl LineStyleKey {
    pub fn new(pattern: LinePattern, width: u8, color_token: impl Into<String>) -> Self {
        Self {
            pattern,
            width,
            color_token: color_token.into(),
        }
    }
}

/// Line-specific operation parsed from instruction string
#[derive(Debug, Clone, PartialEq)]
pub enum LineOp {
    /// Direct style from LS() instruction
    Style(LineStyleKey),
    /// Conditional symbology - needs procedure execution
    CS(String),
    /// Complex line pattern - needs pattern rendering
    LC(String),
}

impl LinePattern {
    /// Parse from S-52 pattern string
    pub fn from_str(s: &str) -> Self {
        match s.trim() {
            "SOLD" => Self::Solid,
            "DASH" => Self::Dashed,
            "DOTT" => Self::Dotted,
            "DASD" => Self::DashDot,
            _ => Self::Solid, // Default to solid
        }
    }
}

/// Parsed S-52 render instruction
#[derive(Debug, Clone)]
pub enum RenderInstruction {
    /// LS(pattern, width, color) - Line symbolization
    LineStyle {
        pattern: LinePattern,
        width: u8,     // S-52 units (0.5mm each)
        color: String, // Color token like "CSTLN"
    },

    /// AC(color) - Area color fill
    AreaColor { color: String },

    /// AP(pattern) - Area pattern fill (hatching)
    AreaPattern { pattern: String },

    /// SY(symbol) - Point symbol
    Symbol { name: String },

    /// TX/TE - Text label
    Text {
        attribute: String, // Attribute name to display (e.g. "OBJNAM")
        format: Option<String>, // TE format string (e.g. "%4.1lf"), None for TX
        hjust: u8,         // Horizontal justification: 1=center, 2=right, 3=left
        vjust: u8,         // Vertical justification: 1=bottom, 2=center, 3=top
        xoffs: i16,        // X offset in chars
        yoffs: i16,        // Y offset in chars
        color: String,     // Text color token
        /// Font style code (CHARS[0]). 1 = standard alphabetic.
        style: u8,
        /// Font weight code (CHARS[1]). 4 = light, 5 = normal, 6 = bold, 7 = heavy.
        weight: u8,
        /// Font width code (CHARS[2]). 1 = normal.
        width: u8,
        /// Body size in points (CHARS[3..]). Typical 8-20.
        bsize: u8,
        /// Text display group (last TX/TE argument). 21-29, lower = more important.
        /// Filtered by ShowImportantTextOnly (hide dis >= 20).
        dis: u8,
    },

    /// LC(line_class) - Line complex (predefined line style)
    LineComplex { name: String },

    /// CS(procedure) - Conditional symbology
    ConditionalSymbology { procedure: String },
}

impl RenderInstruction {
    /// Parse instruction string like "LS(DASH,2,CSTLN);AC(LANDA)"
    /// Returns all successfully parsed instructions.
    pub fn parse_all(s: &str) -> Vec<Self> {
        s.split(';')
            .filter_map(|part| Self::parse_single(part.trim()))
            .collect()
    }

    /// Parse a single instruction like "LS(DASH,2,CSTLN)"
    pub fn parse_single(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }

        // Find instruction type and arguments
        let open = s.find('(')?;
        let close = s.rfind(')')?;
        if open >= close {
            return None;
        }

        let instr_type = &s[..open];
        let args_str = &s[open + 1..close];

        // Split args, handling quoted strings
        let args = parse_args(args_str);

        match instr_type {
            "LS" if args.len() >= 3 => Some(Self::LineStyle {
                pattern: LinePattern::from_str(&args[0]),
                width: args[1].trim().parse().unwrap_or(1),
                color: args[2].trim().to_string(),
            }),

            "AC" if !args.is_empty() => Some(Self::AreaColor {
                color: args[0].trim().to_string(),
            }),

            "AP" if !args.is_empty() => Some(Self::AreaPattern {
                pattern: args[0].trim().to_string(),
            }),

            "SY" if !args.is_empty() => Some(Self::Symbol {
                name: args[0].trim().to_string(),
            }),

            "LC" if !args.is_empty() => Some(Self::LineComplex {
                name: args[0].trim().to_string(),
            }),

            "CS" if !args.is_empty() => Some(Self::ConditionalSymbology {
                procedure: args[0].trim().to_string(),
            }),

            // TX(attribute, hjust, vjust, space, chars, xoffs, yoffs, color, dis)
            "TX" if args.len() >= 9 => {
                let (style, weight, width, bsize) = parse_chars(&args[4]);
                Some(Self::Text {
                    attribute: args[0].trim().trim_matches('\'').to_string(),
                    format: None,
                    hjust: args[1].trim().parse().unwrap_or(1),
                    vjust: args[2].trim().parse().unwrap_or(1),
                    xoffs: args[5].trim().parse().unwrap_or(0),
                    yoffs: args[6].trim().parse().unwrap_or(0),
                    color: args[7].trim().to_string(),
                    style,
                    weight,
                    width,
                    bsize,
                    dis: args[8].trim().parse().unwrap_or(21),
                })
            }

            // TE('format', attribute, hjust, vjust, space, chars, xoffs, yoffs, color, dis)
            "TE" if args.len() >= 10 => {
                let (style, weight, width, bsize) = parse_chars(&args[5]);
                Some(Self::Text {
                    attribute: args[1].trim().trim_matches('\'').to_string(),
                    format: Some(args[0].trim().trim_matches('\'').to_string()),
                    hjust: args[2].trim().parse().unwrap_or(1),
                    vjust: args[3].trim().parse().unwrap_or(1),
                    xoffs: args[6].trim().parse().unwrap_or(0),
                    yoffs: args[7].trim().parse().unwrap_or(0),
                    color: args[8].trim().to_string(),
                    style,
                    weight,
                    width,
                    bsize,
                    dis: args[9].trim().parse().unwrap_or(21),
                })
            }

            _ => None, // Unknown instruction type
        }
    }

    /// Check if this is a LineStyle instruction
    pub fn is_line_style(&self) -> bool {
        matches!(self, Self::LineStyle { .. })
    }

    /// Check if this is an AreaColor instruction
    pub fn is_area_color(&self) -> bool {
        matches!(self, Self::AreaColor { .. })
    }

    /// Get the color token if this instruction specifies a color
    pub fn color_token(&self) -> Option<&str> {
        match self {
            Self::LineStyle { color, .. } => Some(color),
            Self::AreaColor { color } => Some(color),
            Self::Text { color, .. } => Some(color),
            _ => None,
        }
    }
}

/// Parse line instructions from S-52 instruction string.
///
/// Extracts only line-relevant operations (LS, CS, LC) from an instruction string
/// like "LS(SOLD,1,CSTLN)" or "CS(SLCONS03)".
///
/// Returns a vector of LineOp representing line operations in order.
pub fn parse_instructions(instruction: &str) -> Vec<LineOp> {
    let mut ops = Vec::new();

    for part in instruction.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        // Find instruction type and arguments
        let Some(open) = part.find('(') else {
            continue;
        };
        let Some(close) = part.rfind(')') else {
            continue;
        };
        if open >= close {
            continue;
        }

        let instr_type = &part[..open];
        let args_str = &part[open + 1..close];
        let args = parse_args(args_str);

        match instr_type {
            "LS" if args.len() >= 3 => {
                let pattern = LinePattern::from_str(&args[0]);
                let width = args[1].trim().parse().unwrap_or(1);
                let color_token = args[2].trim().to_string();
                ops.push(LineOp::Style(LineStyleKey::new(pattern, width, color_token)));
            }
            "CS" if !args.is_empty() => {
                ops.push(LineOp::CS(args[0].trim().to_string()));
            }
            "LC" if !args.is_empty() => {
                ops.push(LineOp::LC(args[0].trim().to_string()));
            }
            _ => {} // Ignore non-line instructions (AC, AP, SY, TX, TE)
        }
    }

    ops
}

/// Parse the CHARS field of a TX/TE instruction (e.g. '15110').
/// Returns (style, weight, width, bsize). Defaults to (1,5,1,11) on parse failure
/// — matching OpenCPN's default alphabetic normal-weight 11pt font.
fn parse_chars(raw: &str) -> (u8, u8, u8, u8) {
    let trimmed: &str = raw.trim().trim_matches('\'');
    let digits: Vec<u8> = trimmed
        .chars()
        .filter_map(|c| c.to_digit(10).map(|d| d as u8))
        .collect();
    // Expected: [style, weight, width, bsize_tens, bsize_ones, ...]
    let style = digits.first().copied().unwrap_or(1);
    let weight = digits.get(1).copied().unwrap_or(5);
    let width = digits.get(2).copied().unwrap_or(1);
    let bsize = if digits.len() >= 5 {
        digits[3] * 10 + digits[4]
    } else if digits.len() == 4 {
        digits[3]
    } else {
        11
    };
    (style, weight, width, bsize)
}

/// Parse comma-separated arguments, handling quoted strings
fn parse_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for c in s.chars() {
        match c {
            '\'' => {
                in_quotes = !in_quotes;
                // Include quotes in the argument for format strings
                current.push(c);
            }
            ',' if !in_quotes => {
                args.push(current.trim().to_string());
                current.clear();
            }
            _ => {
                current.push(c);
            }
        }
    }

    if !current.is_empty() {
        args.push(current.trim().to_string());
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ls() {
        let instr = RenderInstruction::parse_single("LS(DASH,2,CSTLN)").unwrap();
        if let RenderInstruction::LineStyle {
            pattern,
            width,
            color,
        } = instr
        {
            assert_eq!(pattern, LinePattern::Dashed);
            assert_eq!(width, 2);
            assert_eq!(color, "CSTLN");
        } else {
            panic!("Expected LineStyle");
        }
    }

    #[test]
    fn test_parse_ac() {
        let instr = RenderInstruction::parse_single("AC(LANDA)").unwrap();
        if let RenderInstruction::AreaColor { color } = instr {
            assert_eq!(color, "LANDA");
        } else {
            panic!("Expected AreaColor");
        }
    }

    #[test]
    fn test_parse_sy() {
        let instr = RenderInstruction::parse_single("SY(ACHARE02)").unwrap();
        if let RenderInstruction::Symbol { name } = instr {
            assert_eq!(name, "ACHARE02");
        } else {
            panic!("Expected Symbol");
        }
    }

    #[test]
    fn test_parse_multiple() {
        let instructions = RenderInstruction::parse_all("AC(LANDA);AP(AIRARE02);LS(SOLD,1,CHBLK)");
        assert_eq!(instructions.len(), 3);
        assert!(instructions[0].is_area_color());
        assert!(matches!(instructions[1], RenderInstruction::AreaPattern { .. }));
        assert!(instructions[2].is_line_style());
    }

    #[test]
    fn test_parse_with_quotes() {
        // TE instruction with quoted format string
        let instr =
            RenderInstruction::parse_single("TE('clr %4.1lf',VERCLR,3,1,2,'15110',1,1,CHBLK,11)");
        assert!(instr.is_some());
        if let Some(RenderInstruction::Text {
            attribute,
            color,
            format,
            hjust,
            vjust,
            weight,
            bsize,
            dis,
            ..
        }) = instr
        {
            assert_eq!(attribute, "VERCLR");
            assert_eq!(color, "CHBLK");
            assert_eq!(format, Some("clr %4.1lf".to_string()));
            assert_eq!(hjust, 3);
            assert_eq!(vjust, 1);
            assert_eq!(weight, 5);
            assert_eq!(bsize, 10);
            assert_eq!(dis, 11);
        }
    }

    #[test]
    fn test_parse_chars_field() {
        assert_eq!(parse_chars("'15110'"), (1, 5, 1, 10));
        assert_eq!(parse_chars("15110"), (1, 5, 1, 10));
        assert_eq!(parse_chars("'16120'"), (1, 6, 1, 20));
        assert_eq!(parse_chars("'14108'"), (1, 4, 1, 8));
        assert_eq!(parse_chars(""), (1, 5, 1, 11));
    }

    #[test]
    fn test_line_pattern() {
        assert_eq!(LinePattern::from_str("SOLD"), LinePattern::Solid);
        assert_eq!(LinePattern::from_str("DASH"), LinePattern::Dashed);
        assert_eq!(LinePattern::from_str("DOTT"), LinePattern::Dotted);
        assert_eq!(LinePattern::from_str("DASD"), LinePattern::DashDot);
        assert_eq!(LinePattern::from_str("UNKNOWN"), LinePattern::Solid);
    }

    #[test]
    fn test_parse_cs() {
        let instr = RenderInstruction::parse_single("CS(SLCONS03)").unwrap();
        if let RenderInstruction::ConditionalSymbology { procedure } = instr {
            assert_eq!(procedure, "SLCONS03");
        } else {
            panic!("Expected ConditionalSymbology");
        }
    }

    #[test]
    fn test_parse_instructions_ls() {
        let ops = parse_instructions("LS(SOLD,1,CSTLN)");
        assert_eq!(ops.len(), 1);
        if let LineOp::Style(key) = &ops[0] {
            assert_eq!(key.pattern, LinePattern::Solid);
            assert_eq!(key.width, 1);
            assert_eq!(key.color_token, "CSTLN");
        } else {
            panic!("Expected LineOp::Style");
        }
    }

    #[test]
    fn test_parse_instructions_cs() {
        let ops = parse_instructions("CS(SLCONS03)");
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0], LineOp::CS("SLCONS03".into()));
    }

    #[test]
    fn test_parse_instructions_lc() {
        let ops = parse_instructions("LC(LOWACC21)");
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0], LineOp::LC("LOWACC21".into()));
    }

    #[test]
    fn test_parse_instructions_ignores_non_line() {
        // AC (area color) should be ignored
        let ops = parse_instructions("AC(LANDA);LS(DASH,2,CSTLN)");
        assert_eq!(ops.len(), 1);
        if let LineOp::Style(key) = &ops[0] {
            assert_eq!(key.pattern, LinePattern::Dashed);
            assert_eq!(key.width, 2);
            assert_eq!(key.color_token, "CSTLN");
        } else {
            panic!("Expected LineOp::Style");
        }
    }

    #[test]
    fn test_parse_instructions_multiple() {
        let ops = parse_instructions("LS(SOLD,1,CSTLN);CS(QUAPOS01)");
        assert_eq!(ops.len(), 2);
        assert!(matches!(&ops[0], LineOp::Style(_)));
        assert_eq!(ops[1], LineOp::CS("QUAPOS01".into()));
    }

    #[test]
    fn test_line_style_key_hash() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        let key1 = LineStyleKey::new(LinePattern::Solid, 1, "CSTLN");
        let key2 = LineStyleKey::new(LinePattern::Solid, 1, "CSTLN");
        let key3 = LineStyleKey::new(LinePattern::Dashed, 1, "CSTLN");

        set.insert(key1.clone());
        assert!(set.contains(&key2));
        assert!(!set.contains(&key3));
    }

    #[test]
    fn test_instruction_ordering_ls_lc_ls() {
        // Regression test: LS(...);LC(...);LS(...) should produce 3 ops with pass indices 0, 1, 2
        let ops = parse_instructions("LS(SOLD,2,OUTLW);LC(LOWACC21);LS(SOLD,1,CHBLK)");

        // Should have exactly 3 operations
        assert_eq!(ops.len(), 3, "Should have 3 line operations");

        // Verify operation types in order
        assert!(matches!(&ops[0], LineOp::Style(_)), "First op should be LS");
        assert!(matches!(&ops[1], LineOp::LC(_)), "Second op should be LC");
        assert!(matches!(&ops[2], LineOp::Style(_)), "Third op should be LS");

        // Verify first LS style
        if let LineOp::Style(key) = &ops[0] {
            assert_eq!(key.pattern, LinePattern::Solid);
            assert_eq!(key.width, 2);
            assert_eq!(key.color_token, "OUTLW");
        }

        // Verify LC name
        if let LineOp::LC(name) = &ops[1] {
            assert_eq!(name, "LOWACC21");
        }

        // Verify second LS style
        if let LineOp::Style(key) = &ops[2] {
            assert_eq!(key.pattern, LinePattern::Solid);
            assert_eq!(key.width, 1);
            assert_eq!(key.color_token, "CHBLK");
        }

        // The pass indices (0, 1, 2) are assigned during enumeration in builder.rs
        // Here we just verify the ordering is preserved from parsing
        // The actual test that pass indices map correctly is done in integration tests
    }

    #[test]
    fn test_instruction_ordering_casing_pattern() {
        // Common casing pattern: outer casing, inner line
        let ops = parse_instructions("LS(SOLD,4,CHGRD);LS(SOLD,2,CHBLK)");

        assert_eq!(ops.len(), 2, "Should have 2 line operations for casing");

        // First pass (0): thick outer casing
        if let LineOp::Style(key) = &ops[0] {
            assert_eq!(key.width, 4);
            assert_eq!(key.color_token, "CHGRD");
        }

        // Second pass (1): thin inner line
        if let LineOp::Style(key) = &ops[1] {
            assert_eq!(key.width, 2);
            assert_eq!(key.color_token, "CHBLK");
        }
    }
}
