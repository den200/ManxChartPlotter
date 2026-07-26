//! What the shop talks about: charts, slots, editions and download grants.

/// A chart set's edition, as the shop states it: `year/major-minor`.
///
/// Ordering is the point of this type. The shop reports an edition per chart and
/// the client compares it against what is installed to decide between a patch, a
/// full download and nothing at all.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edition {
    pub year: u32,
    pub major: u32,
    pub minor: u32,
}

impl Edition {
    /// Parse `2025/1-20`. Tolerant: a missing part reads as zero, because an
    /// unparsable edition should degrade to "download the base" rather than
    /// abort the listing.
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        let (year, rest) = match s.split_once('/') {
            Some((y, r)) => (y.trim().parse().unwrap_or(0), r),
            None => (0, s),
        };
        let (major, minor) = match rest.split_once('-') {
            Some((a, b)) => (a.trim().parse().unwrap_or(0), b.trim().parse().unwrap_or(0)),
            None => (rest.trim().parse().unwrap_or(0), 0),
        };
        Self { year, major, minor }
    }

    pub fn is_known(&self) -> bool {
        self.year != 0 || self.major != 0 || self.minor != 0
    }
}

impl std::fmt::Display for Edition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}-{}", self.year, self.major, self.minor)
    }
}

/// One machine's claim on one chart.
#[derive(Debug, Default, Clone)]
pub struct Slot {
    pub uuid: String,
    /// Which machine holds it; empty when the slot is free.
    pub assigned_system: String,
    pub last_requested: String,
}

impl Slot {
    pub fn is_free(&self) -> bool {
        self.assigned_system.trim().is_empty()
    }
}

/// Charts bought together under one order line share a quantity.
#[derive(Debug, Default, Clone)]
pub struct Quantity {
    pub id: String,
    pub slots: Vec<Slot>,
}

/// A chart set the account is entitled to.
#[derive(Debug, Default, Clone)]
pub struct Chart {
    pub id: String,
    pub name: String,
    /// `oeuSENC` for vector, `oeRNC` for raster.
    pub chart_type: String,
    pub order: String,
    pub purchased: String,
    pub expires: String,
    pub expired: bool,
    /// The edition the shop has.
    pub edition: Edition,
    pub edition_date: String,
    pub max_slots: u32,
    pub thumbnail: String,
    pub quantities: Vec<Quantity>,
}

impl Chart {
    /// The slot this machine already holds, if any.
    pub fn slot_for(&self, system_name: &str) -> Option<(&Quantity, &Slot)> {
        self.quantities.iter().find_map(|q| {
            q.slots
                .iter()
                .find(|s| s.assigned_system == system_name)
                .map(|s| (q, s))
        })
    }

    /// A quantity with a slot free to claim.
    pub fn free_quantity(&self) -> Option<&Quantity> {
        self.quantities
            .iter()
            .find(|q| q.slots.iter().any(Slot::is_free) || q.slots.is_empty())
    }

    pub fn is_vector(&self) -> bool {
        self.chart_type.eq_ignore_ascii_case("oeuSENC")
    }
}

/// What to ask the shop for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadTarget {
    /// Everything; the only option when nothing is installed.
    Base,
    /// Only the cells that changed since the installed edition.
    Update,
    /// The same edition again, to repair an install.
    Reinstall,
    /// Nothing to do.
    UpToDate,
}

impl DownloadTarget {
    /// The value the `requestedFile` field takes. `None` when there is nothing
    /// to request.
    pub fn requested_file(self) -> Option<&'static str> {
        match self {
            DownloadTarget::Base | DownloadTarget::Reinstall => Some("base"),
            DownloadTarget::Update => Some("update"),
            DownloadTarget::UpToDate => None,
        }
    }

    /// How to describe the state of this chart to the user.
    pub fn label(self) -> &'static str {
        match self {
            DownloadTarget::Base => "Not installed",
            DownloadTarget::Update => "Update available",
            DownloadTarget::Reinstall => "Installed",
            DownloadTarget::UpToDate => "Installed",
        }
    }
}

/// One downloadable package plus the keys that unlock it.
#[derive(Debug, Default, Clone)]
pub struct FileGrant {
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub keys_url: String,
    pub keys_sha256: String,
    /// Empty when the shop substituted a base package for a requested update.
    pub edition_target: String,
    pub edition_result: String,
}

impl FileGrant {
    /// The shop answered an update request with a full base package.
    pub fn substituted_base(&self) -> bool {
        self.edition_target.trim().is_empty()
    }
}

/// The reply to a download request: one or more packages.
#[derive(Debug, Default, Clone)]
pub struct DownloadGrant {
    pub files: Vec<FileGrant>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editions_parse_and_order() {
        assert_eq!(
            Edition::parse("2025/1-20"),
            Edition { year: 2025, major: 1, minor: 20 }
        );
        // Ordering is year, then major, then minor — so a 2026 edition beats a
        // 2025 one whatever its major, which is what the shop means by it.
        assert!(Edition::parse("2026/1-0") > Edition::parse("2025/9-99"));
        assert!(Edition::parse("2025/2-0") > Edition::parse("2025/1-99"));
        assert!(Edition::parse("2025/1-20") > Edition::parse("2025/1-19"));
        // Junk degrades rather than panicking.
        assert!(!Edition::parse("").is_known());
        assert!(!Edition::parse("nonsense").is_known());
        assert_eq!(Edition::parse("2025/1-20").to_string(), "2025/1-20");
    }

    #[test]
    fn a_chart_finds_this_machines_slot_and_a_free_one() {
        let chart = Chart {
            quantities: vec![
                Quantity {
                    id: "1".into(),
                    slots: vec![Slot {
                        uuid: "A".into(),
                        assigned_system: "saloon-mac".into(),
                        ..Default::default()
                    }],
                },
                Quantity {
                    id: "2".into(),
                    slots: vec![Slot { uuid: "B".into(), ..Default::default() }],
                },
            ],
            ..Default::default()
        };
        let (q, s) = chart.slot_for("saloon-mac").expect("our slot");
        assert_eq!((q.id.as_str(), s.uuid.as_str()), ("1", "A"));
        assert!(chart.slot_for("other-boat").is_none());
        assert_eq!(chart.free_quantity().map(|q| q.id.as_str()), Some("2"));
    }

    #[test]
    fn targets_map_to_the_wire_value_and_a_label() {
        assert_eq!(DownloadTarget::Base.requested_file(), Some("base"));
        assert_eq!(DownloadTarget::Update.requested_file(), Some("update"));
        assert_eq!(DownloadTarget::Reinstall.requested_file(), Some("base"));
        assert_eq!(DownloadTarget::UpToDate.requested_file(), None);
        assert_eq!(DownloadTarget::Update.label(), "Update available");
    }
}
