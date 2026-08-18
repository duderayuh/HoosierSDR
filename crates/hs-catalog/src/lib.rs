//! Talkgroup/site catalog behind one trait, three backends (built in this
//! order): CSV import (Phase 3) → RadioReference SOAP (Phase 4) → on-air
//! self-discovery.
//!
//! Hard rules: user credentials never touch this repository or its output —
//! they come from the environment today (`RR_APP_KEY`, `RR_USERNAME`,
//! `RR_PASSWORD`) and the OS keyring eventually, are redacted from `Debug`, and
//! are never written to the cache. No RadioReference table data is ever
//! committed here; every fixture below is synthetic.

pub mod csv;
pub mod xml;

#[cfg(feature = "radioreference")]
pub mod radioreference;

pub use csv::CsvCatalog;
#[cfg(feature = "radioreference")]
pub use radioreference::{Credentials, RrClient, RrSystem};

#[derive(Debug, Clone, Default)]
pub struct Talkgroup {
    pub id: u16,
    /// Short display name (RadioReference "Alpha Tag").
    pub alias: Option<String>,
    /// Longer description.
    pub description: Option<String>,
    /// Service tag / category (e.g. "Law Dispatch", "EMS").
    pub category: Option<String>,
    /// From RR's `enc` attribute or on-air ALGID observation. Encrypted
    /// talkgroups are greyed out and never tuned for audio.
    pub encrypted: bool,
}

#[derive(Debug, Clone)]
pub struct Site {
    pub site_id: u32,
    pub name: Option<String>,
    pub control_channels_hz: Vec<u64>,
    pub simulcast: bool,
    /// RR v18 `tdma_cc`: control channel is Phase II TDMA (e.g. the Fort
    /// Wayne / Westville pilots).
    pub tdma_control: bool,
}

#[derive(Debug)]
pub enum CatalogError {
    NotFound,
    Backend(String),
}

/// A source of system/site/talkgroup metadata.
pub trait Catalog {
    fn talkgroups(&self, sys_id: u16) -> Result<Vec<Talkgroup>, CatalogError>;
    fn sites(&self, sys_id: u16) -> Result<Vec<Site>, CatalogError>;
}
