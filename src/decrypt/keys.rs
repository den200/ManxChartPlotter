use std::collections::HashMap;
use std::fs;
use std::path::Path;

use log::{debug, trace};
use quick_xml::events::Event;
use quick_xml::Reader;

use super::error::{DecryptError, DecryptResult};

/// Parsed chart key entry from a keyList XML file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartKey {
    pub filename: String,
    pub install_key: String,
}

/// Lookup table for `.oesu` install keys.
#[derive(Debug, Default)]
pub struct KeyStore {
    keys: HashMap<String, String>,
}

impl KeyStore {
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Insert or update an entry.
    pub fn insert<N: Into<String>, K: Into<String>>(&mut self, filename: N, install_key: K) {
        let filename = filename.into();
        let normalized = normalize_chart_name(&filename);
        self.keys.insert(normalized, install_key.into());
    }

    /// Lookup helper accepting paths with or without `.oesu`.
    pub fn lookup(&self, name: &str) -> Option<&str> {
        let normalized = normalize_chart_name(name);
        self.keys.get(&normalized).map(|s| s.as_str())
    }

    /// Loads keys from a single XML file.
    ///
    /// Files without a `<keyList>` root are silently ignored which allows callers to point at
    /// directories that also contain metadata such as `ChartList.XML`.
    pub fn load_keylist_file<P: AsRef<Path>>(&mut self, path: P) -> DecryptResult<usize> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)?;

        if !content.to_ascii_lowercase().contains("<keylist") {
            trace!(
                "Skipping {} because it is not a keyList XML document",
                path.display()
            );
            return Ok(0);
        }

        let parsed = parse_keylist(&content, path)?;
        let mut added = 0;

        for entry in parsed {
            self.insert(entry.filename, entry.install_key);
            added += 1;
        }

        if added > 0 {
            debug!("Loaded {} chart keys from {}", added, path.display());
        }

        Ok(added)
    }

    /// Load every `*.xml` file under `dir`, aggregating all keys.
    pub fn load_keylists_in_dir<P: AsRef<Path>>(&mut self, dir: P) -> DecryptResult<usize> {
        let dir = dir.as_ref();
        let mut total = 0usize;

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            if !matches!(path.extension().and_then(|s| s.to_str()), Some(ext) if ext.eq_ignore_ascii_case("xml"))
            {
                continue;
            }

            match self.load_keylist_file(&path) {
                Ok(count) => total += count,
                Err(err) => {
                    debug!("Skipping {} ({})", path.display(), err);
                }
            }
        }

        Ok(total)
    }

    /// Iterator over stored keys.
    pub fn keys(&self) -> impl Iterator<Item = ChartKey> + '_ {
        self.keys.iter().map(|(name, key)| ChartKey {
            filename: name.clone(),
            install_key: key.clone(),
        })
    }
}

fn parse_keylist(content: &str, source: &Path) -> DecryptResult<Vec<ChartKey>> {
    let mut reader = Reader::from_str(content);
    reader.trim_text(true);

    let mut buf = Vec::new();
    let mut root_seen = false;
    let mut in_chart = false;
    let mut pending_tag = Pending::None;
    let mut current_name: Option<String> = None;
    let mut current_key: Option<String> = None;
    let mut keys = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Decl(_) | Event::Comment(_) => {}
            Event::Start(e) => {
                let qname = e.name();
                let name = qname.as_ref();

                if !root_seen {
                    if name != b"keyList" {
                        return Err(DecryptError::Protocol(format!(
                            "{} is not a keyList XML document",
                            source.display()
                        )));
                    }
                    root_seen = true;
                    buf.clear();
                    continue;
                }

                match name {
                    b"Chart" => {
                        in_chart = true;
                        current_name = None;
                        current_key = None;
                    }
                    b"FileName" if in_chart => pending_tag = Pending::FileName,
                    b"RInstallKey" if in_chart => pending_tag = Pending::InstallKey,
                    _ => {}
                }
            }
            Event::Text(text) => {
                if !in_chart {
                    continue;
                }

                let value = text.unescape()?.into_owned().trim().to_string();
                match pending_tag {
                    Pending::FileName => current_name = Some(value),
                    Pending::InstallKey => current_key = Some(value),
                    Pending::None => {}
                }
            }
            Event::End(e) => {
                let qname = e.name();
                let name = qname.as_ref();
                match name {
                    b"Chart" => {
                        if let (Some(name), Some(key)) = (current_name.take(), current_key.take()) {
                            if !name.is_empty() && !key.is_empty() {
                                keys.push(ChartKey {
                                    filename: name,
                                    install_key: key,
                                });
                            }
                        }
                        in_chart = false;
                        pending_tag = Pending::None;
                    }
                    b"FileName" | b"RInstallKey" => pending_tag = Pending::None,
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }

        buf.clear();
    }

    if !root_seen {
        return Err(DecryptError::Protocol(format!(
            "{} is not a keyList XML document",
            source.display()
        )));
    }

    Ok(keys)
}

#[derive(Copy, Clone, Debug)]
enum Pending {
    None,
    FileName,
    InstallKey,
}

fn normalize_chart_name(name: &str) -> String {
    let trimmed = name.trim();
    let file_name = Path::new(trimmed)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(trimmed);

    let without_ext = file_name
        .strip_suffix(".oesu")
        .or_else(|| file_name.strip_suffix(".OESU"))
        .unwrap_or(file_name);

    without_ext.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keylist_file() {
        let xml = r#"
            <keyList>
                <Chart>
                    <FileName>OC-45-TEST</FileName>
                    <RInstallKey>ABC123</RInstallKey>
                </Chart>
                <Chart>
                    <FileName>OC-45-OTHER.oesu</FileName>
                    <RInstallKey>XYZ789</RInstallKey>
                </Chart>
            </keyList>
        "#;

        let entries = parse_keylist(xml, Path::new("dummy.xml")).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].filename, "OC-45-TEST");
        assert_eq!(entries[1].filename, "OC-45-OTHER.oesu");

        let mut store = KeyStore::new();
        for entry in entries {
            store.insert(entry.filename, entry.install_key);
        }

        assert_eq!(store.lookup("OC-45-TEST.oesu"), Some("ABC123"));
        assert_eq!(store.lookup("OC-45-OTHER"), Some("XYZ789"));
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize_chart_name("OC-45-A.oesu"), "OC-45-A");
        assert_eq!(normalize_chart_name("/tmp/charts/OC-45-B"), "OC-45-B");
    }
}
