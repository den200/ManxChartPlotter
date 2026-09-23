//! Unencrypted S-57 ENCs — the free charts NOAA and others publish.
//!
//! navcore draws charts from SENC, the compiled form OpenCPN builds and
//! o-charts ships encrypted. An S-57 cell is read here and turned into the
//! same thing, so everything downstream — rendering, picking, routing — is
//! unchanged.

/// Bumped whenever the SENC the converter writes changes, so cells cached
/// by an older build are converted again.
pub const ENCODER_VERSION: u32 = 1;

pub mod iso8211;
pub mod cell;
pub mod attr_types;
pub mod senc_encode;
pub mod basemap;

#[cfg(test)]
mod tests {
    /// A folder of S-57 cells becomes a catalog like a folder of o-charts
    /// cells — with or without the o-charts helper on the machine — and a
    /// cell in it loads as chart data.
    #[test]
    fn a_folder_of_s57_cells_catalogs_and_loads() {
        use crate::cache::CachedDecryptor;
        use crate::decrypt::KeyStore;
        use crate::senc::{ChartCatalog, ChartData};
        let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/s57"));
        let mut decryptor = CachedDecryptor::open("license");
        let catalog = ChartCatalog::from_directory(dir, &KeyStore::new(), &mut decryptor)
            .expect("the fixture folder catalogs");
        assert_eq!(catalog.charts.len(), 1);
        let info = &catalog.charts[0];
        assert_eq!(info.name, "US4MA1BD");
        assert_eq!(info.native_scale, 45_000);
        assert!(ChartCatalog::is_s57(&info.path));
        let chart = ChartData::parse(decryptor.s57_senc(&info.path).unwrap()).unwrap();
        assert_eq!(chart.features.len(), 22, "as OpenCPN's reader finds, updates applied");
    }
}
