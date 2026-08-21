//! RadioReference database web service client (SOAP).
//!
//! This is the design doc's Phase 4 catalog path, and it exists to answer one
//! question the radio cannot answer for itself: *which frequency is the
//! control channel?* Picking it from a power sweep does not work — the first
//! field capture for this project was tuned to the strongest signal in the
//! band, which turned out not to be P25 at all, with the real carrier 50 kHz
//! away. RadioReference already knows every site's control and alternate
//! channels, its NAC and RFSS, and the talkgroup list, so we ask it.
//!
//! ## What the service requires
//!
//! * An **application key**, issued per application to a registered developer
//!   at <https://www.radioreference.com/apps/account/?tab=api>. HoosierSDR
//!   ships no key: keys identify an application to the service, and using
//!   another project's key would misrepresent this one. Register the app once
//!   and supply the key.
//! * **Per-user credentials with an active Premium subscription.** The service
//!   authenticates the end user on every call, so each user supplies their own
//!   RadioReference login; the subscription requirement passes through to them
//!   rather than to the developer.
//!
//! ## Provenance
//!
//! Written from RadioReference's own published service documentation. No code
//! or schema was taken from any existing client library — notably not
//! SDRTrunk's, which is GPL-licensed and would be incompatible with this
//! project's Apache-2.0 licence (see CONTRIBUTING.md).
//!
//! ## Field-name tolerance
//!
//! Element names are matched case-insensitively with namespace prefixes
//! stripped, and each field is looked up under every spelling the service has
//! been documented to use. Where a response still fails to map, [`RrClient`]
//! can dump the raw XML (see [`RrClient::with_dump_dir`]) so the mapping can be
//! corrected against a real payload rather than guessed at.
//!
//! **No RadioReference data is committed to this repository.** The tests below
//! use entirely synthetic responses describing an invented system.

use crate::xml::{self, Node};
use crate::{Catalog, CatalogError, Site, Talkgroup};

/// Default service endpoint.
pub const ENDPOINT: &str = "https://api.radioreference.com/soap2/";

/// Service version requested in `authInfo`.
const SERVICE_VERSION: &str = "latest";

/// Credentials for the service. Never logged, never serialized to the cache,
/// and never committed — see [`Credentials::from_env`].
#[derive(Clone)]
pub struct Credentials {
    /// Application key issued to a registered developer.
    pub app_key: String,
    /// End user's RadioReference username.
    pub username: String,
    /// End user's RadioReference password.
    pub password: String,
}

impl Credentials {
    pub fn new(
        app_key: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            app_key: app_key.into(),
            username: username.into(),
            password: password.into(),
        }
    }

    /// Read credentials from the environment:
    /// `RR_APP_KEY`, `RR_USERNAME`, `RR_PASSWORD`.
    ///
    /// The environment is used rather than a config file so a password never
    /// lands on disk by default. A keyring-backed store is the design doc's
    /// eventual home for these.
    pub fn from_env() -> Result<Self, RrError> {
        let get = |k: &str| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| RrError::MissingCredentials(k.to_string()))
        };
        Ok(Self {
            app_key: get("RR_APP_KEY")?,
            username: get("RR_USERNAME")?,
            password: get("RR_PASSWORD")?,
        })
    }
}

/// Deliberately opaque: a `Debug` that printed the password would leak it into
/// any log line or panic message that formats a client.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("app_key", &"<redacted>")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug)]
pub enum RrError {
    /// A required credential env var was absent or empty.
    MissingCredentials(String),
    /// Transport failure.
    Http(String),
    /// The service returned a SOAP Fault.
    Fault { code: String, message: String },
    /// The response parsed but held none of the expected records.
    Empty(&'static str),
}

impl std::fmt::Display for RrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCredentials(k) => write!(
                f,
                "{k} is not set. The RadioReference service needs an application \
                 key (register at https://www.radioreference.com/apps/account/?tab=api) \
                 and a user login with an active Premium subscription. Set \
                 RR_APP_KEY, RR_USERNAME and RR_PASSWORD."
            ),
            Self::Http(e) => write!(f, "RadioReference request failed: {e}"),
            Self::Fault { code, message } => {
                write!(f, "RadioReference returned a fault ({code}): {message}")
            }
            Self::Empty(what) => write!(
                f,
                "RadioReference response contained no {what}. If the system ID is \
                 right, the response field names may differ from what this client \
                 expects — re-run with a dump directory set and share the XML."
            ),
        }
    }
}

impl std::error::Error for RrError {}

/// A P25 site as RadioReference describes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RrSite {
    pub site_id: u32,
    pub rfss: Option<u32>,
    /// Network Access Code, as decoded off the air in the NID.
    pub nac: Option<u16>,
    pub description: Option<String>,
    pub county: Option<String>,
    /// Tower position, when the database has one. Decimal degrees, WGS84.
    /// Combined with the NAC decoded off the air, this puts a capture on the
    /// map: the NAC identifies which site was heard, and the site carries its
    /// own coordinates.
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// Published coverage radius in miles, when present.
    pub range_mi: Option<f64>,
    /// Control channels, primary first, then alternates.
    pub control_channels_hz: Vec<u64>,
    /// Every frequency on the site, control and voice alike.
    pub frequencies_hz: Vec<u64>,
    /// The control channel uses Phase II TDMA.
    pub tdma_control: bool,
}

impl RrSite {
    /// Tower position, if the database records one.
    pub fn position(&self) -> Option<(f64, f64)> {
        Some((self.lat?, self.lon?))
    }
}

/// A whole trunked system: identity, sites, talkgroups.
#[derive(Debug, Clone, Default)]
pub struct RrSystem {
    pub id: u32,
    pub name: Option<String>,
    /// P25 System ID.
    pub sysid: Option<u16>,
    /// Wide Area Communications Network ID.
    pub wacn: Option<u32>,
    pub sites: Vec<RrSite>,
    pub talkgroups: Vec<Talkgroup>,
}

impl RrSystem {
    /// The talkgroups in RadioReference's CSV export format — the one
    /// [`crate::CsvCatalog`] parses — so a download can be saved and reloaded
    /// without the network.
    pub fn talkgroup_csv(&self) -> String {
        fn field(v: Option<&str>) -> String {
            let v = v.unwrap_or("");
            if v.contains(',') || v.contains('"') {
                format!("\"{}\"", v.replace('"', "\"\""))
            } else {
                v.to_string()
            }
        }
        let mut out =
            String::from("Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category,Priority\n");
        for tg in &self.talkgroups {
            out.push_str(&format!(
                "{},{:X},{},{},{},{},{},\n",
                tg.id,
                tg.id,
                field(tg.alias.as_deref()),
                if tg.encrypted { "DE" } else { "D" },
                field(tg.description.as_deref()),
                field(tg.category.as_deref()),
                field(tg.category.as_deref()),
            ));
        }
        out
    }

    /// Every control channel across every site, primary channels first.
    ///
    /// This is the list to tune: each entry is a frequency the site is
    /// documented to transmit a control channel on.
    pub fn control_channels_hz(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for s in &self.sites {
            for &f in &s.control_channels_hz {
                if !out.contains(&f) {
                    out.push(f);
                }
            }
        }
        out
    }

    /// The site whose NAC matches one decoded off the air, if any. Lets a
    /// capture identify which site it was actually hearing — and, when the
    /// database has coordinates, where that site is.
    pub fn site_by_nac(&self, nac: u16) -> Option<&RrSite> {
        self.sites.iter().find(|s| s.nac == Some(nac))
    }

    /// Position of the site transmitting a given NAC, in decimal degrees.
    pub fn locate_nac(&self, nac: u16) -> Option<(f64, f64)> {
        self.site_by_nac(nac)?.position()
    }
}

/// Transport used to reach the service. Abstracted so the request/response
/// mapping is testable without a network — the tests drive the same parsing
/// path the real client uses, against synthetic payloads.
pub trait SoapTransport {
    fn post(&self, endpoint: &str, action: &str, body: &str) -> Result<String, RrError>;
}

/// Blocking HTTPS transport.
pub struct HttpTransport {
    timeout: std::time::Duration,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self {
            timeout: std::time::Duration::from_secs(30),
        }
    }
}

impl SoapTransport for HttpTransport {
    fn post(&self, endpoint: &str, action: &str, body: &str) -> Result<String, RrError> {
        // SOAP faults come back with HTTP 500 and the reason in the body;
        // don't let the status short-circuit before the fault is read.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(self.timeout))
            .http_status_as_error(false)
            .build()
            .into();
        let mut resp = agent
            .post(endpoint)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("SOAPAction", action)
            .send(body)
            .map_err(|e| RrError::Http(e.to_string()))?;
        resp.body_mut()
            .read_to_string()
            .map_err(|e| RrError::Http(e.to_string()))
    }
}

pub struct RrClient<T: SoapTransport = HttpTransport> {
    creds: Credentials,
    endpoint: String,
    transport: T,
    dump_dir: Option<std::path::PathBuf>,
}

impl RrClient<HttpTransport> {
    pub fn new(creds: Credentials) -> Self {
        Self::with_transport(creds, HttpTransport::default())
    }
}

/// A state (or province) in the database's geography tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RrState {
    pub stid: u32,
    pub name: String,
    pub code: String,
}

/// A county within a state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RrCounty {
    pub ctid: u32,
    pub name: String,
}

/// A trunked system as listed under a state or county — enough to pick it;
/// [`RrClient::system`] fetches the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RrSystemRef {
    pub sid: u32,
    pub name: String,
    /// RadioReference system type id (its "Project 25" family has its own
    /// ids). Kept raw; shown, not interpreted.
    pub stype: Option<u32>,
    pub sflavor: Option<u32>,
    pub svoice: Option<u32>,
    pub city: Option<String>,
}

/// What a state holds: its counties and its statewide systems.
#[derive(Debug, Clone, Default)]
pub struct RrStateInfo {
    pub counties: Vec<RrCounty>,
    pub systems: Vec<RrSystemRef>,
}

/// Where a ZIP code sits in the geography tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RrZip {
    pub city: Option<String>,
    pub stid: u32,
    pub ctid: u32,
}

impl<T: SoapTransport> RrClient<T> {
    pub fn with_transport(creds: Credentials, transport: T) -> Self {
        Self {
            creds,
            endpoint: ENDPOINT.to_string(),
            transport,
            dump_dir: None,
        }
    }

    pub fn endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = url.into();
        self
    }

    /// Write every raw response to this directory. The payloads contain
    /// RadioReference table data, so they are written only where the user
    /// asks and are never committed.
    pub fn with_dump_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.dump_dir = Some(dir.into());
        self
    }

    /// Fetch a system's identity, sites and talkgroups. The three calls are
    /// independent: a system whose site list fails (or is empty) still gets
    /// its talkgroups, and vice versa; only a system with neither is an error.
    pub fn system(&self, sys_id: u32) -> Result<RrSystem, RrError> {
        self.system_with_progress(sys_id, &mut |_, _, _| {})
    }

    /// As [`system`](Self::system), reporting `(step, done, total)` as it
    /// goes — "details", "sites", "talkgroups", and "talkgroups: <category>"
    /// when the per-category fallback runs.
    pub fn system_with_progress(
        &self,
        sys_id: u32,
        progress: &mut dyn FnMut(&str, usize, usize),
    ) -> Result<RrSystem, RrError> {
        progress("details", 0, 3);
        let mut sys = self.details(sys_id).unwrap_or(RrSystem {
            id: sys_id,
            ..Default::default()
        });
        progress("sites", 1, 3);
        let sites = self.sites(sys_id);
        progress("talkgroups", 2, 3);
        let tgs = self.talkgroups_with_progress(sys_id, progress);
        match (sites, tgs) {
            (Err(e), Err(_)) => return Err(e),
            (sites, tgs) => {
                sys.sites = sites.unwrap_or_default();
                sys.talkgroups = tgs.unwrap_or_default();
            }
        }
        Ok(sys)
    }

    /// Talkgroup categories, for fetching a large system piecewise.
    pub fn talkgroup_categories(&self, sys_id: u32) -> Result<Vec<(u32, String)>, RrError> {
        let root = self.call("getTrsTalkgroupCats", "sid", &sys_id.to_string())?;
        Ok(collect_records(&root, "tgCid")
            .iter()
            .filter_map(|n| {
                Some((
                    n.get_u64("tgCid")? as u32,
                    n.get_any(&["tgCname", "tgCatName", "name"])
                        .unwrap_or("")
                        .to_string(),
                ))
            })
            .collect())
    }

    fn call(&self, method: &str, arg_name: &str, arg: &str) -> Result<Node, RrError> {
        self.call_args(method, &[(arg_name, arg)])
    }

    fn call_args(&self, method: &str, args: &[(&str, &str)]) -> Result<Node, RrError> {
        let body = envelope_args(method, args, &self.creds);
        let raw = self.transport.post(&self.endpoint, method, &body)?;
        if let Some(dir) = &self.dump_dir {
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(dir.join(format!("{method}.xml")), &raw);
        }
        let root = xml::parse(&raw).ok_or_else(|| {
            // Not SOAP at all — usually an HTML error page. Keep enough of it
            // to say what the server said, minus markup.
            let text: String = raw
                .split('<')
                .filter_map(|s| s.split_once('>').map(|(_, t)| t))
                .collect::<Vec<_>>()
                .join(" ");
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            RrError::Http(format!(
                "{method}: non-XML reply ({} bytes): {}",
                raw.len(),
                text.chars().take(240).collect::<String>()
            ))
        })?;
        if let Some(fault) = root.find("Fault") {
            return Err(RrError::Fault {
                code: fault
                    .get_any(&["faultcode", "Code"])
                    .unwrap_or("unknown")
                    .to_string(),
                message: fault
                    .get_any(&["faultstring", "Reason", "detail"])
                    .unwrap_or("no detail")
                    .to_string(),
            });
        }
        Ok(root)
    }

    pub fn details(&self, sys_id: u32) -> Result<RrSystem, RrError> {
        let root = self.call("getTrsDetails", "sid", &sys_id.to_string())?;
        let n = root.find("sysid").and(root.find("return")).unwrap_or(&root);
        Ok(RrSystem {
            id: sys_id,
            name: n.get_any(&["sName", "sysName", "name"]).map(str::to_string),
            sysid: n
                .get_any(&["sysid", "sysId", "systemId"])
                .and_then(parse_radix16),
            wacn: n.get_any(&["wacn", "wacnId"]).and_then(|s| {
                parse_radix16(s)
                    .map(u32::from)
                    .or_else(|| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
            }),
            ..Default::default()
        })
    }

    pub fn sites(&self, sys_id: u32) -> Result<Vec<RrSite>, RrError> {
        let root = self.call("getTrsSites", "sid", &sys_id.to_string())?;
        let mut nodes = Vec::new();
        // Sites arrive as repeated records; the wrapper element name varies by
        // service version, so collect whichever is present.
        for key in ["siteId", "siteNumber"] {
            let mut hits = Vec::new();
            root.find_all(key, &mut hits);
            if !hits.is_empty() {
                // Each hit is the id field; its parent is the site record.
                nodes = collect_records(&root, key);
                break;
            }
        }
        let sites: Vec<RrSite> = nodes.iter().filter_map(|n| parse_site(n)).collect();
        if sites.is_empty() {
            return Err(RrError::Empty("sites"));
        }
        Ok(sites)
    }

    pub fn talkgroups(&self, sys_id: u32) -> Result<Vec<Talkgroup>, RrError> {
        self.talkgroups_with_progress(sys_id, &mut |_, _, _| {})
    }

    fn talkgroups_with_progress(
        &self,
        sys_id: u32,
        progress: &mut dyn FnMut(&str, usize, usize),
    ) -> Result<Vec<Talkgroup>, RrError> {
        // Whole list first. RadioReference's server has answered HTTP 500
        // with an empty body for some large systems (MESA, sid 5737); the
        // same data comes back fine one category at a time, so fall back to
        // that before giving up.
        let sid = sys_id.to_string();
        let whole = self
            .call_args(
                "getTrsTalkgroups",
                &[
                    ("sid", &sid),
                    ("tgCid", "0"),
                    ("tgTag", "0"),
                    ("tgDec", "0"),
                ],
            )
            .map(|root| parse_talkgroups(&root));
        if let Ok(tgs) = &whole {
            if !tgs.is_empty() {
                return Ok(tgs.clone());
            }
        }
        let cats = self.talkgroup_categories(sys_id)?;
        let mut all: Vec<Talkgroup> = Vec::new();
        let mut last_err: Option<RrError> = whole.err();
        for (i, (cid, cat_name)) in cats.iter().enumerate() {
            progress(&format!("talkgroups: {cat_name}"), i, cats.len());
            match self.call_args(
                "getTrsTalkgroups",
                &[("sid", &sid), ("tgCid", &cid.to_string())],
            ) {
                Ok(root) => {
                    let mut tgs = parse_talkgroups(&root);
                    // The per-category reply doesn't repeat the category;
                    // we asked for it, so we know it.
                    for t in tgs.iter_mut() {
                        if t.category.is_none() && !cat_name.is_empty() {
                            t.category = Some(cat_name.clone());
                        }
                    }
                    all.extend(tgs);
                }
                Err(e) => last_err = Some(e),
            }
        }
        if all.is_empty() {
            return Err(last_err.unwrap_or(RrError::Empty("talkgroups")));
        }
        all.sort_by_key(|t| t.id);
        all.dedup_by_key(|t| t.id);
        Ok(all)
    }
    /// United States' states (RadioReference country id 1), for browsing.
    pub fn states(&self) -> Result<Vec<RrState>, RrError> {
        let root = self.call("getCountryInfo", "coid", "1")?;
        let out: Vec<RrState> = collect_records(&root, "stid")
            .iter()
            .filter_map(|n| {
                Some(RrState {
                    stid: n.get_u64("stid")? as u32,
                    name: n.get_any(&["stateName", "name"])?.to_string(),
                    code: n.get_any(&["stateCode", "code"]).unwrap_or("").to_string(),
                })
            })
            .collect();
        if out.is_empty() {
            return Err(RrError::Empty("states"));
        }
        Ok(out)
    }

    /// A state's counties and statewide systems.
    pub fn state_info(&self, stid: u32) -> Result<RrStateInfo, RrError> {
        let root = self.call("getStateInfo", "stid", &stid.to_string())?;
        let counties: Vec<RrCounty> = collect_records(&root, "ctid")
            .iter()
            .filter_map(|n| {
                Some(RrCounty {
                    ctid: n.get_u64("ctid")? as u32,
                    name: n.get_any(&["countyName", "name"])?.to_string(),
                })
            })
            .collect();
        let systems = parse_system_refs(&root);
        if counties.is_empty() && systems.is_empty() {
            return Err(RrError::Empty("state"));
        }
        Ok(RrStateInfo { counties, systems })
    }

    /// Trunked systems listed under a county.
    pub fn county_systems(&self, ctid: u32) -> Result<Vec<RrSystemRef>, RrError> {
        let root = self.call("getCountyInfo", "ctid", &ctid.to_string())?;
        Ok(parse_system_refs(&root))
    }

    /// The state and county a ZIP code falls in — the quick way in.
    pub fn zipcode(&self, zip: u32) -> Result<RrZip, RrError> {
        let root = self.call("getZipcodeInfo", "zipcode", &zip.to_string())?;
        let n = root.find("stid").map(|_| &root).unwrap_or(&root);
        let stid = n.find("stid").and_then(|x| x.text.trim().parse().ok());
        let ctid = n.find("ctid").and_then(|x| x.text.trim().parse().ok());
        match (stid, ctid) {
            (Some(stid), Some(ctid)) => Ok(RrZip {
                city: root.find("city").map(|c| c.text.trim().to_string()),
                stid,
                ctid,
            }),
            _ => Err(RrError::Empty("zipcode")),
        }
    }
}

/// Every `sid`-bearing record under `root`, as a system reference.
fn parse_system_refs(root: &Node) -> Vec<RrSystemRef> {
    collect_records(root, "sid")
        .iter()
        .filter_map(|n| {
            Some(RrSystemRef {
                sid: n.get_u64("sid")? as u32,
                name: n.get_any(&["sName", "name"])?.to_string(),
                stype: n.get_u64("sType").map(|v| v as u32),
                sflavor: n.get_u64("sFlavor").map(|v| v as u32),
                svoice: n.get_u64("sVoice").map(|v| v as u32),
                city: n.get("sCity").map(str::to_string).filter(|c| !c.is_empty()),
            })
        })
        .collect()
}

fn parse_talkgroups(root: &Node) -> Vec<Talkgroup> {
    collect_records(root, "tgDec")
        .iter()
        .filter_map(parse_talkgroup)
        .collect()
}

/// Collect the parent element of every occurrence of `key` — i.e. every record
/// that carries that field. Robust to whatever the repeating wrapper is called.
fn collect_records<'a>(root: &'a Node, key: &str) -> Vec<&'a Node> {
    fn walk<'a>(n: &'a Node, key: &str, out: &mut Vec<&'a Node>) {
        if n.child(key).is_some() {
            out.push(n);
            // A record's fields are leaves; no need to descend further.
            return;
        }
        for c in &n.children {
            walk(c, key, out);
        }
    }
    let mut out = Vec::new();
    walk(root, key, &mut out);
    out
}

fn parse_site(n: &Node) -> Option<RrSite> {
    let site_id = n.get_u64_any(&["siteId", "siteNumber", "siteRfss"])? as u32;
    let mut site = RrSite {
        site_id,
        rfss: n.get_u64_any(&["rfss", "siteRfss"]).map(|v| v as u32),
        nac: n.get_any(&["nac", "siteNac"]).and_then(parse_radix16),
        description: n
            .get_any(&["siteDescr", "siteDescription", "description"])
            .map(str::to_string),
        county: n.get_any(&["ctyName", "county"]).map(str::to_string),
        // Coordinate field spellings vary across service versions; accept the
        // documented forms and treat a literal 0/0 as "not recorded" rather
        // than as a point in the Gulf of Guinea.
        lat: n
            .get_f64_any(&["lat", "siteLat", "latitude"])
            .filter(nonzero),
        lon: n
            .get_f64_any(&["lon", "lng", "siteLon", "longitude"])
            .filter(nonzero),
        range_mi: n.get_f64_any(&["range", "siteRange"]).filter(nonzero),
        tdma_control: n
            .get_any(&["tdma_cc", "tdmaCc"])
            .map(|v| v.trim() != "0" && !v.trim().is_empty())
            .unwrap_or(false),
        ..Default::default()
    };

    // Frequencies are child records; each may be flagged as a control channel.
    for f in collect_records(n, "freq") {
        let Some(hz) = f.get_f64_any(&["freq"]).map(mhz_to_hz) else {
            continue;
        };
        if !site.frequencies_hz.contains(&hz) {
            site.frequencies_hz.push(hz);
        }
        // `use`: "d" marks the primary control channel, "a" an alternate.
        // Some versions expose a numeric `channelType` instead.
        let flag = f
            .get_any(&["use", "channelType", "cc"])
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let is_control = matches!(flag.as_str(), "d" | "a" | "1" | "2");
        if is_control && !site.control_channels_hz.contains(&hz) {
            // Primary ("d"/"1") ahead of alternates.
            if flag == "d" || flag == "1" {
                site.control_channels_hz.insert(0, hz);
            } else {
                site.control_channels_hz.push(hz);
            }
        }
    }
    Some(site)
}

fn parse_talkgroup(n: &&Node) -> Option<Talkgroup> {
    let id = n.get_u64_any(&["tgDec", "tgId", "decimal"])?;
    let mode = n.get_any(&["tgMode", "mode"]).unwrap_or("");
    Some(Talkgroup {
        id: u16::try_from(id).ok()?,
        alias: n
            .get_any(&["tgAlpha", "alphaTag", "alpha"])
            .map(str::to_string),
        description: n
            .get_any(&["tgDescr", "tgDescription", "description"])
            .map(str::to_string),
        category: n.get_any(&["tgCat", "category", "tag"]).map(str::to_string),
        // RR encodes encryption in the mode string: a trailing "E".
        encrypted: mode.to_ascii_uppercase().contains('E'),
    })
}

/// A coordinate of exactly zero means the database has no position, not a
/// location on the equator.
fn nonzero(v: &f64) -> bool {
    v.abs() > f64::EPSILON
}

/// RadioReference reports frequencies in MHz. Round to the nearest hertz.
fn mhz_to_hz(mhz: f64) -> u64 {
    (mhz * 1_000_000.0).round() as u64
}

/// Parse a value that may be decimal or `0x`-prefixed hex (NAC and System ID
/// are quoted either way depending on the field).
fn parse_radix16(s: &str) -> Option<u16> {
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u16::from_str_radix(hex, 16).ok(),
        None => t.parse().ok(),
    }
}

/// Build the SOAP envelope. `authInfo` carries the app key and the end user's
/// credentials; the service authenticates the user on every call.
fn envelope_args(method: &str, args: &[(&str, &str)], c: &Credentials) -> String {
    let args_xml: String = args
        .iter()
        .map(|(k, v)| format!("   <{k}>{}</{k}>\n", esc(v)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
 <soap:Body>
  <{method}>
{args_xml}   <authInfo>
    <appKey>{key}</appKey>
    <username>{user}</username>
    <password>{pass}</password>
    <version>{ver}</version>
    <style>rpc</style>
   </authInfo>
  </{method}>
 </soap:Body>
</soap:Envelope>"#,
        method = method,
        args_xml = args_xml,
        key = esc(&c.app_key),
        user = esc(&c.username),
        pass = esc(&c.password),
        ver = SERVICE_VERSION,
    )
}

#[allow(dead_code)]
fn envelope(method: &str, arg_name: &str, arg: &str, c: &Credentials) -> String {
    envelope_args(method, &[(arg_name, arg)], c)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

impl Catalog for RrSystem {
    fn talkgroups(&self, _sys_id: u16) -> Result<Vec<Talkgroup>, CatalogError> {
        Ok(self.talkgroups.clone())
    }

    fn sites(&self, _sys_id: u16) -> Result<Vec<Site>, CatalogError> {
        Ok(self
            .sites
            .iter()
            .map(|s| Site {
                site_id: s.site_id,
                name: s.description.clone(),
                control_channels_hz: s.control_channels_hz.clone(),
                simulcast: false,
                tdma_control: s.tdma_control,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records the request and replays a canned response. Every payload here
    /// is synthetic and describes an invented system — no RadioReference data
    /// is committed to this repository.
    struct Fake {
        response: String,
        seen: RefCell<Vec<String>>,
    }

    impl SoapTransport for Fake {
        fn post(&self, _e: &str, _a: &str, body: &str) -> Result<String, RrError> {
            self.seen.borrow_mut().push(body.to_string());
            Ok(self.response.clone())
        }
    }

    fn client(response: &str) -> RrClient<Fake> {
        RrClient::with_transport(
            Credentials::new("APPKEY", "user", "pw"),
            Fake {
                response: response.to_string(),
                seen: RefCell::new(Vec::new()),
            },
        )
    }

    const SITES: &str = r#"<?xml version="1.0"?>
    <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/">
     <SOAP-ENV:Body><ns1:getTrsSitesResponse><return>
      <item>
        <siteId>3</siteId><rfss>1</rfss><nac>0x261</nac>
        <siteDescr>Test Site North</siteDescr><ctyName>Testerton</ctyName>
        <lat>39.7684</lat><lon>-86.1581</lon><range>25</range>
        <tdma_cc>0</tdma_cc>
        <siteFreqs>
          <item><freq>858.9875</freq><use>d</use></item>
          <item><freq>859.2625</freq><use>a</use></item>
          <item><freq>856.2125</freq><use></use></item>
        </siteFreqs>
      </item>
      <item>
        <siteId>4</siteId><rfss>1</rfss><nac>0x1AB</nac>
        <siteDescr>Test Site South</siteDescr>
        <lat>0</lat><lon>0</lon>
        <tdma_cc>1</tdma_cc>
        <siteFreqs>
          <item><freq>851.5375</freq><use>d</use></item>
        </siteFreqs>
      </item>
     </return></ns1:getTrsSitesResponse></SOAP-ENV:Body></SOAP-ENV:Envelope>"#;

    #[test]
    fn parses_sites_with_control_channels_and_nac() {
        let sites = client(SITES).sites(999).expect("sites parse");
        assert_eq!(sites.len(), 2);
        let north = &sites[0];
        assert_eq!(north.site_id, 3);
        assert_eq!(north.nac, Some(0x261));
        assert_eq!(north.county.as_deref(), Some("Testerton"));
        // Primary control channel first, then the alternate; the plain voice
        // frequency is not a control channel.
        assert_eq!(north.control_channels_hz, vec![858_987_500, 859_262_500]);
        assert_eq!(north.frequencies_hz.len(), 3);
        assert!(!north.tdma_control);
        assert!(sites[1].tdma_control);
    }

    #[test]
    fn locates_a_site_from_an_off_air_nac() {
        let sys = RrSystem {
            sites: client(SITES).sites(999).unwrap(),
            ..Default::default()
        };
        let (lat, lon) = sys.locate_nac(0x261).expect("site has coordinates");
        assert!((lat - 39.7684).abs() < 1e-6 && (lon + 86.1581).abs() < 1e-6);
        assert_eq!(sys.sites[0].range_mi, Some(25.0));
        // 0/0 means "not recorded", not a point off the coast of Africa.
        assert_eq!(
            sys.sites[1].position(),
            None,
            "0/0 must not read as located"
        );
    }

    #[test]
    fn identifies_a_site_from_an_off_air_nac() {
        // The decoder reports NAC 0x261 off the air; the catalog names the site.
        let sys = RrSystem {
            sites: client(SITES).sites(999).unwrap(),
            ..Default::default()
        };
        let hit = sys.site_by_nac(0x261).expect("NAC matches a site");
        assert_eq!(hit.description.as_deref(), Some("Test Site North"));
        assert!(sys.site_by_nac(0xFFF).is_none());
    }

    #[test]
    fn collects_control_channels_across_sites_without_duplicates() {
        let sys = RrSystem {
            sites: client(SITES).sites(999).unwrap(),
            ..Default::default()
        };
        let cc = sys.control_channels_hz();
        assert_eq!(cc, vec![858_987_500, 859_262_500, 851_537_500]);
    }

    #[test]
    fn parses_talkgroups_including_the_encryption_flag() {
        let resp = r#"<Envelope><Body><getTrsTalkgroupsResponse><return>
          <item><tgDec>12179</tgDec><tgAlpha>MEDIC 4</tgAlpha>
                <tgDescr>Medic dispatch</tgDescr><tgCat>EMS</tgCat><tgMode>D</tgMode></item>
          <item><tgDec>12180</tgDec><tgAlpha>PD DISP</tgAlpha>
                <tgCat>Police</tgCat><tgMode>DE</tgMode></item>
        </return></getTrsTalkgroupsResponse></Body></Envelope>"#;
        let tgs = client(resp).talkgroups(999).expect("talkgroups parse");
        assert_eq!(tgs.len(), 2);
        assert_eq!(tgs[0].id, 12179);
        assert_eq!(tgs[0].alias.as_deref(), Some("MEDIC 4"));
        assert!(!tgs[0].encrypted);
        // "DE" = digital, encrypted. These are badged and never tuned.
        assert!(tgs[1].encrypted);
    }

    #[test]
    fn sends_credentials_in_the_auth_info_block() {
        let c = client(SITES);
        c.sites(4949).unwrap();
        let body = c.transport.seen.borrow()[0].clone();
        assert!(body.contains("<appKey>APPKEY</appKey>"));
        assert!(body.contains("<username>user</username>"));
        assert!(body.contains("<sid>4949</sid>"));
        assert!(body.contains("getTrsSites"));
    }

    #[test]
    fn surfaces_a_soap_fault_rather_than_decoding_nothing() {
        let resp = r#"<Envelope><Body><Fault>
            <faultcode>SOAP-ENV:Client</faultcode>
            <faultstring>Authentication failed</faultstring>
        </Fault></Body></Envelope>"#;
        match client(resp).sites(1) {
            Err(RrError::Fault { message, .. }) => assert!(message.contains("Authentication")),
            other => panic!("expected a fault, got {other:?}"),
        }
    }

    #[test]
    fn an_unmapped_response_reports_empty_not_success() {
        // A response whose field names this client does not know must fail
        // loudly, so it can be dumped and the mapping corrected.
        let resp = "<Envelope><Body><return><item><somethingElse>1</somethingElse></item></return></Body></Envelope>";
        assert!(matches!(
            client(resp).sites(1),
            Err(RrError::Empty("sites"))
        ));
    }

    #[test]
    fn credentials_debug_never_reveals_the_password() {
        let c = Credentials::new("k", "someone", "hunter2");
        let s = format!("{c:?}");
        assert!(!s.contains("hunter2"), "password leaked into Debug: {s}");
        assert!(!s.contains('k') || !s.contains("app_key: \"k\""));
        assert!(s.contains("someone"));
    }

    #[test]
    fn escapes_credentials_that_contain_xml_metacharacters() {
        let body = envelope(
            "getTrsSites",
            "sid",
            "1",
            &Credentials::new("a&b", "u<v", "p\"w"),
        );
        assert!(body.contains("a&amp;b"));
        assert!(body.contains("u&lt;v"));
        assert!(!body.contains("p\"w"));
    }

    const COUNTRY: &str = r#"<Envelope><Body><getCountryInfoResponse><return>
        <coid>1</coid><countryName>United States</countryName>
        <stateList><item><stid>17</stid><stateName>Indiana</stateName><stateCode>IN</stateCode></item>
                   <item><stid>18</stid><stateName>Iowa</stateName><stateCode>IA</stateCode></item></stateList>
        </return></getCountryInfoResponse></Body></Envelope>"#;

    const STATE: &str = r#"<Envelope><Body><getStateInfoResponse><return>
        <stid>17</stid><stateName>Indiana</stateName>
        <trsList><item><sid>100</sid><sName>Example Statewide</sName><sType>8</sType><sFlavor>1</sFlavor><sVoice>2</sVoice><sCity></sCity></item></trsList>
        <countyList><item><ctid>901</ctid><countyName>Example County</countyName><countyHeader>Counties</countyHeader></item>
                    <item><ctid>902</ctid><countyName>Other County</countyName></item></countyList>
        </return></getStateInfoResponse></Body></Envelope>"#;

    const COUNTY: &str = r#"<Envelope><Body><getCountyInfoResponse><return>
        <ctid>901</ctid><countyName>Example County</countyName><stid>17</stid>
        <trsList><item><sid>200</sid><sName>Example County Public Safety</sName><sType>9</sType><sCity>Exampleville</sCity></item></trsList>
        </return></getCountyInfoResponse></Body></Envelope>"#;

    const ZIP: &str = r#"<Envelope><Body><getZipcodeInfoResponse><return>
        <zipCode>46000</zipCode><lat>39.9</lat><lon>-86.1</lon><city>Exampleville</city><stid>17</stid><ctid>901</ctid>
        </return></getZipcodeInfoResponse></Body></Envelope>"#;

    #[test]
    fn browses_states_counties_systems_and_zipcodes() {
        let st = client(COUNTRY).states().unwrap();
        assert_eq!(st.len(), 2);
        assert_eq!(
            st[0],
            RrState {
                stid: 17,
                name: "Indiana".into(),
                code: "IN".into()
            }
        );

        let info = client(STATE).state_info(17).unwrap();
        assert_eq!(info.counties.len(), 2);
        assert_eq!(
            info.counties[0],
            RrCounty {
                ctid: 901,
                name: "Example County".into()
            }
        );
        assert_eq!(info.systems.len(), 1);
        assert_eq!(info.systems[0].sid, 100);
        assert_eq!(info.systems[0].stype, Some(8));
        assert_eq!(info.systems[0].city, None);

        let sys = client(COUNTY).county_systems(901).unwrap();
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0].name, "Example County Public Safety");
        assert_eq!(sys[0].city.as_deref(), Some("Exampleville"));

        let z = client(ZIP).zipcode(46000).unwrap();
        assert_eq!(
            z,
            RrZip {
                city: Some("Exampleville".into()),
                stid: 17,
                ctid: 901
            }
        );

        let c = client(COUNTY);
        c.county_systems(901).unwrap();
        let body = c.transport.seen.borrow()[0].clone();
        assert!(body.contains("getCountyInfo") && body.contains("<ctid>901</ctid>"));
    }

    /// A transport that answers each call from a queue, so a failing whole-
    /// list request followed by per-category requests can be scripted.
    struct Seq {
        replies: std::cell::RefCell<std::collections::VecDeque<Result<String, RrError>>>,
        seen: std::cell::RefCell<Vec<String>>,
    }
    impl SoapTransport for Seq {
        fn post(&self, _e: &str, _a: &str, body: &str) -> Result<String, RrError> {
            self.seen.borrow_mut().push(body.to_string());
            self.replies
                .borrow_mut()
                .pop_front()
                .unwrap_or(Err(RrError::Empty("script")))
        }
    }

    #[test]
    fn talkgroups_fall_back_to_per_category_fetches() {
        let cats = r#"<Envelope><Body><r><item><tgCid>1</tgCid><tgCname>Law</tgCname></item><item><tgCid>2</tgCid><tgCname>Fire</tgCname></item></r></Body></Envelope>"#;
        let law = r#"<Envelope><Body><r><item><tgDec>101</tgDec><tgAlpha>PD 1</tgAlpha><tgMode>D</tgMode></item></r></Body></Envelope>"#;
        let fire = r#"<Envelope><Body><r><item><tgDec>201</tgDec><tgAlpha>FD 1</tgAlpha><tgMode>D</tgMode></item></r></Body></Envelope>"#;
        let t = Seq {
            replies: std::cell::RefCell::new(
                vec![
                    Err(RrError::Http("http status: 500".into())),
                    Ok(cats.into()),
                    Ok(law.into()),
                    Ok(fire.into()),
                ]
                .into(),
            ),
            seen: Default::default(),
        };
        let c = RrClient::with_transport(Credentials::new("k", "u", "p"), t);
        let tgs = c.talkgroups(5737).unwrap();
        assert_eq!(tgs.iter().map(|t| t.id).collect::<Vec<_>>(), vec![101, 201]);
        assert_eq!(tgs[0].category.as_deref(), Some("Law"));
        assert_eq!(tgs[1].category.as_deref(), Some("Fire"));
        let seen = c.transport.seen.borrow();
        assert!(
            seen[0].contains("<tgCid>0</tgCid>"),
            "whole-list request carries the optional args"
        );
        assert!(seen[1].contains("getTrsTalkgroupCats"));
        assert!(seen[2].contains("<tgCid>1</tgCid>") && seen[3].contains("<tgCid>2</tgCid>"));
    }

    #[test]
    fn a_system_with_talkgroups_but_no_sites_still_loads() {
        let details = r#"<Envelope><Body><return><sName>Solo</sName><sysid>0x1</sysid></return></Body></Envelope>"#;
        let tgs = r#"<Envelope><Body><r><item><tgDec>7</tgDec><tgAlpha>X</tgAlpha><tgMode>D</tgMode></item></r></Body></Envelope>"#;
        let t = Seq {
            replies: std::cell::RefCell::new(
                vec![
                    Ok(details.into()),
                    Err(RrError::Empty("sites")),
                    Ok(tgs.into()),
                ]
                .into(),
            ),
            seen: Default::default(),
        };
        let c = RrClient::with_transport(Credentials::new("k", "u", "p"), t);
        let sys = c.system(1).unwrap();
        assert_eq!(sys.name.as_deref(), Some("Solo"));
        assert!(sys.sites.is_empty() && sys.talkgroups.len() == 1);
    }
}
