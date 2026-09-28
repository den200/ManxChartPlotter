//! NavCore - Rust nautical chart plotter for Raspberry Pi and Mac.
//!
//! ## Modules
//!
//! - [`decrypt`] - Interface to `oexserverd` for decrypting OESU charts
//! - [`senc`] - SENC (System ENC) TLV parser for chart geometry
//!
//! ## Quick Start
//!
//! ```no_run
//! use navcore2::{ChartDecryptor, KeyStore, senc::ChartData};
//!
//! // Load keys
//! let mut keys = KeyStore::new();
//! keys.load_keylists_in_dir("charts/oeuSENC-DK-2025-1-20-base-macbook").unwrap();
//!
//! // Decrypt chart
//! let mut decryptor = ChartDecryptor::new("license.fpr").unwrap();
//! let install_key = keys.lookup("OC-45-D54503").unwrap();
//! let senc_bytes = decryptor.decrypt_chart("charts/.../OC-45-D54503.oesu", install_key).unwrap();
//!
//! // Parse features
//! let chart = ChartData::parse(senc_bytes).unwrap();
//! log::debug!("{}", chart.summary());
//! ```

pub mod cache;
pub mod decrypt;
pub mod export;
pub mod geo;
pub mod nav;
pub mod pick;
pub mod render;
pub mod shop;
pub mod signalk;
pub mod sound;
pub mod s52;
pub mod s57;
pub mod senc;
pub mod tiles;

pub use cache::{CachedDecryptor, SencCache};
pub use decrypt::{ChartDecryptor, ChartKey, DecryptError, KeyStore};
