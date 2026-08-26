//! RadioReference / trunk-recorder talkgroup CSV import.
//!
//! This is the design doc's first catalog path (§6): the user downloads their
//! own premium talkgroup CSV from RadioReference and imports it locally — no
//! app key, no stored credentials, and it always works. The parser accepts the
//! standard export column set, in any order, with or without a header row:
//!
//! ```text
//! Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category,Priority
//! 12179,2F93,MEDIC 4,D,Medic dispatch,EMS Dispatch,EMS,1
//! 12180,2F94,PD DISPATCH,DE,Encrypted PD,Law Dispatch,Police,1
//! ```
//!
//! No RadioReference data is committed to this repository; the tests use
//! synthetic fixtures only.

use crate::{Catalog, CatalogError, Talkgroup};
use std::collections::HashMap;

/// A talkgroup catalog loaded from a CSV export. Lookups are by 16-bit
/// talkgroup ID (the P25 group address).
#[derive(Debug, Default, Clone)]
pub struct CsvCatalog {
    talkgroups: HashMap<u16, Talkgroup>,
}

impl CsvCatalog {
    /// Parse a talkgroup CSV from text. Rows that don't parse (blank lines,
    /// bad IDs) are skipped rather than failing the whole import.
    pub fn parse(text: &str) -> Self {
        let mut lines = text.lines().peekable();
        // Detect and consume a header row if present.
        let cols = match lines.peek() {
            Some(first) if looks_like_header(first) => {
                let hdr = parse_csv_line(first);
                lines.next();
                ColumnMap::from_header(&hdr)
            }
            _ => ColumnMap::default_order(),
        };

        let mut talkgroups = HashMap::new();
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let fields = parse_csv_line(line);
            if let Some(tg) = cols.row_to_talkgroup(&fields) {
                talkgroups.insert(tg.id, tg);
            }
        }
        Self { talkgroups }
    }

    /// Add every talkgroup from `other`, replacing same-id entries — so
    /// several systems' catalogs can be in force at once.
    pub fn merge(&mut self, other: &CsvCatalog) {
        for (id, tg) in &other.talkgroups {
            self.talkgroups.insert(*id, tg.clone());
        }
    }

    pub fn len(&self) -> usize {
        self.talkgroups.len()
    }

    pub fn is_empty(&self) -> bool {
        self.talkgroups.is_empty()
    }

    /// Look up a talkgroup by ID.
    pub fn get(&self, id: u16) -> Option<&Talkgroup> {
        self.talkgroups.get(&id)
    }

    /// The display label for a talkgroup ID: its alias, else "TG <id>".
    pub fn label(&self, id: u16) -> String {
        match self.get(id).and_then(|t| t.alias.as_deref()) {
            Some(a) => a.to_string(),
            None => format!("TG {id}"),
        }
    }
}

impl Catalog for CsvCatalog {
    fn talkgroups(&self, _sys_id: u16) -> Result<Vec<Talkgroup>, CatalogError> {
        Ok(self.talkgroups.values().cloned().collect())
    }
    fn sites(&self, _sys_id: u16) -> Result<Vec<crate::Site>, CatalogError> {
        // Site/frequency data lives in a separate RR export; not handled here.
        Ok(Vec::new())
    }
}

/// Column indices for the fields we care about.
struct ColumnMap {
    decimal: usize,
    alpha: Option<usize>,
    mode: Option<usize>,
    description: Option<usize>,
    tag: Option<usize>,
    category: Option<usize>,
    priority: Option<usize>,
}

impl ColumnMap {
    /// The canonical RadioReference export order.
    fn default_order() -> Self {
        Self {
            decimal: 0,
            alpha: Some(2),
            mode: Some(3),
            description: Some(4),
            tag: Some(5),
            category: Some(6),
            priority: Some(7),
        }
    }

    fn from_header(hdr: &[String]) -> Self {
        // Try each candidate name in priority order (so "Category" wins over
        // "Tag" when both columns are present).
        let find = |names: &[&str]| {
            names
                .iter()
                .find_map(|n| hdr.iter().position(|h| h.trim().eq_ignore_ascii_case(n)))
        };
        Self {
            decimal: find(&["Decimal", "TGID", "Talkgroup"]).unwrap_or(0),
            alpha: find(&["Alpha Tag", "Alpha", "Name"]),
            mode: find(&["Mode"]),
            description: find(&["Description"]),
            tag: find(&["Tag"]),
            category: find(&["Category"]),
            priority: find(&["Priority"]),
        }
    }

    fn row_to_talkgroup(&self, fields: &[String]) -> Option<Talkgroup> {
        let raw = fields.get(self.decimal)?.trim();
        let id: u16 = raw.parse().ok()?;
        let pick = |idx: Option<usize>| {
            idx.and_then(|i| fields.get(i))
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        // RadioReference "Mode": A/D/M plus an 'E' suffix for encrypted
        // (DE, TE, AE). Treat any mode containing 'E' as encrypted.
        let encrypted = self
            .mode
            .and_then(|i| fields.get(i))
            .map(|m| m.trim().to_ascii_uppercase().contains('E'))
            .unwrap_or(false);
        let priority = self
            .priority
            .and_then(|i| fields.get(i))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<u8>().ok());
        Some(Talkgroup {
            id,
            alias: pick(self.alpha),
            description: pick(self.description),
            tag: pick(self.tag),
            category: pick(self.category),
            encrypted,
            priority,
        })
    }
}

/// True if a line looks like a header (contains a known header token and no
/// leading numeric ID).
fn looks_like_header(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    (lower.contains("decimal") || lower.contains("alpha tag") || lower.contains("tgid"))
        && line
            .split(',')
            .next()
            .map(|c| c.trim().parse::<u32>().is_err())
            .unwrap_or(true)
}

/// Minimal CSV line splitter with double-quote handling (fields may contain
/// commas inside quotes; "" is an escaped quote).
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic fixtures only — never real RadioReference table data.
    const CSV: &str = "\
Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category,Priority
12179,2F93,MEDIC 4,D,Medic dispatch,EMS Dispatch,EMS,1
12180,2F94,PD DISPATCH,DE,\"Encrypted, primary\",Law Dispatch,Police,1
12181,2F95,FIRE OPS,D,,Fireground,Fire,2";

    #[test]
    fn parses_header_and_rows() {
        let cat = CsvCatalog::parse(CSV);
        assert_eq!(cat.len(), 3);
        let medic = cat.get(12179).unwrap();
        assert_eq!(medic.alias.as_deref(), Some("MEDIC 4"));
        assert_eq!(medic.tag.as_deref(), Some("EMS Dispatch"));
        assert_eq!(medic.category.as_deref(), Some("EMS"));
        assert!(!medic.encrypted);
        assert_eq!(medic.priority, Some(1));
        assert_eq!(cat.label(12179), "MEDIC 4");
        assert_eq!(cat.label(9999), "TG 9999");
    }

    #[test]
    fn detects_encryption_and_quoted_commas() {
        let cat = CsvCatalog::parse(CSV);
        let pd = cat.get(12180).unwrap();
        assert!(pd.encrypted, "DE mode should be encrypted");
        assert_eq!(pd.description.as_deref(), Some("Encrypted, primary"));
    }

    #[test]
    fn headerless_default_order() {
        let cat = CsvCatalog::parse("12179,2F93,MEDIC 4,D,Medic dispatch,EMS Dispatch,EMS");
        assert_eq!(cat.get(12179).unwrap().alias.as_deref(), Some("MEDIC 4"));
    }

    #[test]
    fn empty_and_bad_rows_skipped() {
        let cat = CsvCatalog::parse("Decimal,Alpha Tag\n\nnotanumber,foo\n42,Valid");
        assert_eq!(cat.len(), 1);
        assert_eq!(cat.get(42).unwrap().alias.as_deref(), Some("Valid"));
    }
}
