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
///
/// A slot only exists once it is claimed. The shop lists the claims, never the
/// vacancies, so there is no such thing here as a free `Slot` — how many are
/// left is [`Chart::free_slots`], arithmetic against `maxSlots`.
#[derive(Debug, Default, Clone)]
pub struct Slot {
    pub uuid: String,
    /// Which machine holds it.
    pub assigned_system: String,
    pub last_requested: String,
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
    /// Links to the current edition's `ChartList.xml` files. Their paths carry
    /// the set's family name, which is the only thing tying a shop listing to
    /// a directory on disk.
    pub base_chart_lists: Vec<String>,
}

/// Is this the shop's name for a USB key?
///
/// o-charts names a dongle `sgl` followed by its serial in eight hex digits,
/// and a licence assigned to one travels with the key rather than being tied to
/// a computer. There is no field saying "this is a dongle" — the name is the
/// only signal, and the reference client reads it exactly this way.
pub fn is_dongle_name(name: &str) -> bool {
    name.len() == 11
        && name.starts_with("sgl")
        && name[3..].chars().all(|c| c.is_ascii_hexdigit())
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

    /// The set's family name — `oeuSENC-DK` — as used by both the shop's URLs
    /// and the directory names it ships. `None` when the shop sent no links,
    /// in which case there is nothing to key an installed set to.
    pub fn set_stem(&self) -> Option<String> {
        self.base_chart_lists
            .iter()
            .find_map(|url| super::installed::set_stem(url))
    }

    /// A slot held by a USB key, if the licence is on one.
    pub fn dongle_slot(&self) -> Option<&Slot> {
        self.quantities
            .iter()
            .flat_map(|q| q.slots.iter())
            .find(|s| is_dongle_name(&s.assigned_system))
    }

    /// How many slots the licence carries in total.
    ///
    /// `maxSlots` is per quantity, and buying the same country twice in one
    /// order yields two quantities, so the capacity is the product.
    pub fn total_slots(&self) -> usize {
        self.quantities.len() * self.max_slots as usize
    }

    /// How many are already claimed.
    ///
    /// The shop sends one `<slot>` per *assignment*, not one per available
    /// slot: a five-slot licence with two machines on it arrives carrying two
    /// slots, and a brand-new one carries none. Counting the elements is
    /// therefore the whole of it — there is nothing to subtract a free marker
    /// from, because free slots are not sent at all.
    pub fn assigned_slots(&self) -> usize {
        self.quantities
            .iter()
            .flat_map(|q| q.slots.iter())
            .filter(|s| !s.uuid.trim().is_empty())
            .count()
    }

    pub fn free_slots(&self) -> usize {
        self.total_slots().saturating_sub(self.assigned_slots())
    }

    /// Every machine holding one of this chart's slots.
    pub fn holders(&self) -> Vec<&str> {
        self.quantities
            .iter()
            .flat_map(|q| q.slots.iter())
            .map(|s| s.assigned_system.trim())
            .filter(|n| !n.is_empty())
            .collect()
    }

    /// `None` when the shop did not state a capacity, which is not the same as
    /// a capacity of zero: it means we cannot prove the licence is full, so we
    /// must let the request through and let the shop refuse it.
    fn slot_capacity(&self) -> Option<usize> {
        (self.max_slots > 0).then_some(self.max_slots as usize)
    }

    /// Every slot spoken for, with none left for this machine.
    pub fn is_fully_assigned(&self) -> bool {
        let Some(cap) = self.slot_capacity() else {
            return false;
        };
        !self.quantities.is_empty() && self.quantities.iter().all(|q| q.slots.len() >= cap)
    }

    /// A quantity with room to claim a slot in.
    pub fn free_quantity(&self) -> Option<&Quantity> {
        match self.slot_capacity() {
            Some(cap) => self.quantities.iter().find(|q| q.slots.len() < cap),
            None => self.quantities.first(),
        }
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
        // Two quantities of one slot each: the first taken, the second not yet
        // sent by the shop at all, because free slots are never listed.
        let chart = Chart {
            max_slots: 1,
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
                    slots: Vec::new(),
                },
            ],
            ..Default::default()
        };
        let (q, s) = chart.slot_for("saloon-mac").expect("our slot");
        assert_eq!((q.id.as_str(), s.uuid.as_str()), ("1", "A"));
        assert!(chart.slot_for("other-boat").is_none());
        assert_eq!(chart.free_quantity().map(|q| q.id.as_str()), Some("2"));
    }

    /// One quantity, five slots, two of them taken.
    fn denmark() -> Chart {
        Chart {
            name: "Denmark".into(),
            max_slots: 5,
            quantities: vec![Quantity {
                id: "1".into(),
                slots: vec![
                    Slot {
                        uuid: "A".into(),
                        assigned_system: "macbook".into(),
                        ..Default::default()
                    },
                    Slot {
                        uuid: "B".into(),
                        assigned_system: "sgl001ECF71".into(),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn free_slots_are_the_capacity_less_what_the_shop_actually_sent() {
        // The shop sends one <slot> per assignment and none for the free ones,
        // so five slots with two machines on them arrive as a list of two.
        // Reporting `maxSlots` as the free count claimed five were free when
        // three were.
        let c = denmark();
        assert_eq!(c.total_slots(), 5);
        assert_eq!(c.assigned_slots(), 2);
        assert_eq!(c.free_slots(), 3);
        assert_eq!(c.holders(), vec!["macbook", "sgl001ECF71"]);
        assert!(!c.is_fully_assigned());
        // And a slot can still be claimed, in the quantity that has room.
        assert_eq!(c.free_quantity().map(|q| q.id.as_str()), Some("1"));
    }

    #[test]
    fn a_full_licence_offers_no_quantity() {
        let mut c = denmark();
        for n in ["c", "d", "e"] {
            c.quantities[0].slots.push(Slot {
                uuid: n.into(),
                assigned_system: n.into(),
                ..Default::default()
            });
        }
        assert_eq!(c.free_slots(), 0);
        assert!(c.is_fully_assigned());
        assert!(c.free_quantity().is_none());
    }

    #[test]
    fn an_unstated_capacity_lets_the_shop_decide() {
        // maxSlots absent is not maxSlots zero: refusing locally would turn a
        // parse gap into "you own no slots", which is a lie we can't support.
        let mut c = denmark();
        c.max_slots = 0;
        assert!(!c.is_fully_assigned());
        assert!(c.free_quantity().is_some());
    }

    #[test]
    fn a_usb_key_is_recognised_by_its_name() {
        assert!(is_dongle_name("sgl001ECF71"));
        assert!(!is_dongle_name("macbook"));
        // Right prefix, wrong length; and right shape, but not hex.
        assert!(!is_dongle_name("sgl001EC"));
        assert!(!is_dongle_name("sglZZZZZZZZ"));

        let c = denmark();
        assert_eq!(
            c.dongle_slot().map(|s| s.assigned_system.as_str()),
            Some("sgl001ECF71")
        );
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
