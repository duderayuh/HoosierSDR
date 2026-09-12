//! Backups: a copy of the library that survives this machine.
//!
//! One archive holds everything the app cannot rebuild — the call database,
//! every setting, the talkgroup catalogs, and as much audio as the listener
//! chooses. It is a plain `tar.gz` so it can be opened with the tools any
//! computer already has, sealed with [age] when it is going anywhere but a
//! local folder.
//!
//! Three rules shape the design:
//!
//! - **The snapshot is consistent.** SQLite's `VACUUM INTO` writes a whole,
//!   valid database while the app keeps recording; copying `calls.db` out
//!   from under a live connection does not.
//! - **`secrets.json` never goes in.** It holds the Telegram bot token and
//!   the RadioReference password. A backup travels — to a NAS, to a bucket,
//!   to whoever later reads that bucket — so the one file whose loss is
//!   worst is the one file left out. The restore notes say so plainly.
//! - **Nothing leaves in the clear.** Every destination but a local folder
//!   is encrypted, without asking, with a passphrase held in the secret
//!   store and shown to the listener once. Cloud storage misconfigured to
//!   world-readable is ordinary; an archive that is useless to a stranger
//!   makes that survivable.
//!
//! A restore never overwrites anything that is open. Files are unpacked
//! beside their live counterparts with a `.restored` suffix and a marker;
//! [`apply_pending`] swaps them in at the next start, keeping what was
//! there as `.replaced-<stamp>`.
//!
//! [age]: https://age-encryption.org

use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::AppState;

/// Bumped when the layout inside an archive changes in a way a restore has
/// to know about.
const FORMAT: u32 = 1;
const MANIFEST: &str = "hoosier-backup.json";
const CHECKSUMS: &str = "hoosier-checksums.json";
/// Left behind by a staged restore; read at the next start.
const PENDING: &str = "restore-pending.json";

pub type Shared = Mutex<Settings>;

// ------------------------------------------------------------- settings

/// Somewhere backups are sent. A `folder` is any path this computer can
/// write — an external disk, a mounted NAS share, an iCloud or Dropbox
/// folder. An `s3` destination is any S3-compatible store (see [`crate::s3`]).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Destination {
    pub id: String,
    pub name: String,
    /// `"folder"` or `"s3"`.
    pub kind: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub bucket: String,
    /// Key prefix, so one bucket can hold more than this.
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub path_style: bool,
    /// Seal the archive before it leaves. Always true for `s3`.
    #[serde(default)]
    pub encrypt: bool,
    /// Backups kept here; older ones are removed after a successful run.
    /// 0 = keep every one.
    #[serde(default)]
    pub keep: u32,
    #[serde(default)]
    pub enabled: bool,
    /// What to include, overriding the shared setting. Empty = shared.
    #[serde(default)]
    pub tier: String,
}

/// How much of the library an archive carries.
///
/// - `records` — database and settings only. Megabytes; enough to bring
///   back every call's details, transcript, incident and tripwire history.
/// - `kept` — the above plus the audio retention protects (starred calls,
///   calls a tripwire sent about, calls attached to an incident).
/// - `everything` — every recording on disk.
pub const TIERS: [&str; 3] = ["records", "kept", "everything"];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub destinations: Vec<Destination>,
    /// Shared tier for destinations that do not name their own.
    pub tier: String,
    /// Hours between automatic backups; 0 = only when asked.
    pub every_hours: u32,
    /// The last run per destination id.
    #[serde(default)]
    pub last: HashMap<String, Outcome>,
    /// Set once the listener has confirmed they wrote the passphrase down.
    #[serde(default)]
    pub passphrase_ack: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            destinations: Vec::new(),
            tier: "kept".into(),
            every_hours: 0,
            last: HashMap::new(),
            passphrase_ack: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Outcome {
    pub at: i64,
    pub ok: bool,
    pub name: String,
    pub bytes: u64,
    pub files: usize,
    pub secs: f64,
    pub error: String,
}

/// What the page needs to draw the panel: the settings, plus the facts
/// about credentials that must not travel with them.
#[derive(Serialize, Clone, Debug, Default)]
pub struct View {
    pub settings: Settings,
    /// Destination id → an access key is stored for it.
    pub keys: HashMap<String, bool>,
    pub passphrase_set: bool,
    /// A restore waiting for the next start.
    pub pending: Option<Pending>,
}

/// What a backup would weigh, so the tier choice is an informed one.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Sizes {
    pub db_bytes: u64,
    pub config_bytes: u64,
    pub kept_bytes: u64,
    pub kept_files: usize,
    pub all_bytes: u64,
    pub all_files: usize,
    pub calls: i64,
}

fn config_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let d = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d)
}

fn path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(config_dir(app)?.join("backup.json"))
}

pub fn load(app: &AppHandle) -> Settings {
    path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn store(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let p = path(app)?;
    std::fs::write(
        p,
        serde_json::to_string_pretty(s).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

// ------------------------------------------------------- credentials

fn key_id(dest: &str) -> String {
    format!("backup-s3-id:{dest}")
}
fn key_secret(dest: &str) -> String {
    format!("backup-s3-secret:{dest}")
}
const PASS_KEY: &str = "backup-passphrase";

/// A passphrase worth writing on paper: 120 bits in six typable groups,
/// from an alphabet with no character that can be misread (no 0/O, 1/I/l).
pub fn new_passphrase() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    let pick = |rng: &mut dyn rand::RngCore| {
        (0..4)
            .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
            .collect::<String>()
    };
    (0..6)
        .map(|_| pick(&mut rng))
        .collect::<Vec<_>>()
        .join("-")
}

fn bucket_for(d: &Destination) -> Result<crate::s3::Bucket, String> {
    Ok(crate::s3::Bucket {
        endpoint: d.endpoint.clone(),
        region: if d.region.trim().is_empty() {
            "us-east-1".into()
        } else {
            d.region.clone()
        },
        bucket: d.bucket.clone(),
        path_style: d.path_style,
        access: crate::secrets::get(&key_id(&d.id))
            .ok_or("no access key saved for this destination")?,
        secret: crate::secrets::get(&key_secret(&d.id))
            .ok_or("no secret key saved for this destination")?,
    })
}

// --------------------------------------------------------- the plan

/// The files one archive will carry, worked out before anything is written
/// so the page can say what a run would cost and the run can report
/// progress against a known total.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub tier: String,
    /// Config files, as (name inside the archive, path on disk).
    pub config: Vec<(String, PathBuf)>,
    pub audio: Vec<(String, PathBuf)>,
    pub audio_bytes: u64,
    pub db_bytes: u64,
    pub calls: i64,
}

/// Settings that hold a credential in the clear, and where. `secrets.json`
/// is left out of an archive entirely; these files are wanted for
/// everything *else* they hold, so the credential is blanked on the way in.
/// A plaintext archive sitting on a shared drive is a browsable file.
const REDACT: &[(&str, &[&str])] = &[
    (
        "uploads.json",
        &["rdio.key", "openmhz.api_key", "broadcastify.api_key"],
    ),
    ("stream.json", &["password"]),
];

/// Key names that are a credential wherever they appear. `key` is not in
/// the list on purpose: an analyzer's extract field is called `key` too,
/// and blanking those would quietly break the rules they belong to — the
/// two real `key` credentials are named in [`REDACT`] instead.
const CREDENTIAL_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "token",
    "password",
    "passwd",
    "secret",
    "passphrase",
    "access_key",
    "secret_key",
];

fn at<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for p in path.split('.') {
        cur = cur.get(p)?;
    }
    Some(cur)
}

fn set_at(v: &mut serde_json::Value, path: &str, to: serde_json::Value) -> bool {
    let mut cur = v;
    let mut parts = path.split('.').peekable();
    while let Some(p) = parts.next() {
        let Some(next) = cur.get_mut(p) else {
            return false;
        };
        if parts.peek().is_none() {
            *next = to;
            return true;
        }
        cur = next;
    }
    false
}

/// A settings file coming back out of an archive has its credentials
/// blanked. On the machine it was backed up from, the live file still has
/// them — so they are carried into the restored copy rather than being
/// quietly lost, which would stop uploads with nothing on screen to say
/// why. On a new machine there is no live file and the note stands: enter
/// them again. `redacted` is the manifest's own list.
pub fn carry_credentials(live: Option<&[u8]>, staged: &[u8], file: &str, redacted: &[String]) -> Vec<u8> {
    let paths: Vec<&str> = redacted
        .iter()
        .filter_map(|r| r.split_once(": "))
        .filter(|(f, _)| *f == file)
        .map(|(_, p)| p)
        .collect();
    if paths.is_empty() {
        return staged.to_vec();
    }
    let (Some(live), Ok(mut out)) = (
        live.and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok()),
        serde_json::from_slice::<serde_json::Value>(staged),
    ) else {
        return staged.to_vec();
    };
    let mut carried = false;
    for p in paths {
        if let Some(v) = at(&live, p) {
            if v.as_str().map(|s| !s.is_empty()).unwrap_or(false) && set_at(&mut out, p, v.clone())
            {
                carried = true;
            }
        }
    }
    if !carried {
        return staged.to_vec();
    }
    serde_json::to_vec_pretty(&out).unwrap_or_else(|_| staged.to_vec())
}

fn blank_at(v: &mut serde_json::Value, path: &str) -> bool {
    let mut cur = v;
    let mut parts = path.split('.').peekable();
    while let Some(p) = parts.next() {
        let Some(next) = cur.get_mut(p) else {
            return false;
        };
        if parts.peek().is_none() {
            if next.as_str().map(|s| !s.is_empty()).unwrap_or(false) {
                *next = serde_json::Value::String(String::new());
                return true;
            }
            return false;
        }
        cur = next;
    }
    false
}

/// Blank every credential in one settings file: the ones named for it, and
/// any other field whose name says credential, so a field added later does
/// not leak by being forgotten here. Returns what was blanked.
pub fn redact(name: &str, body: &[u8]) -> (Vec<u8>, Vec<String>) {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (body.to_vec(), Vec::new());
    };
    let mut blanked = Vec::new();
    for (file, paths) in REDACT {
        if *file != name {
            continue;
        }
        for p in *paths {
            if blank_at(&mut v, p) {
                blanked.push(format!("{name}: {p}"));
            }
        }
    }
    sweep(&mut v, name, "", &mut blanked);
    match serde_json::to_vec_pretty(&v) {
        Ok(out) => (out, blanked),
        Err(_) => (body.to_vec(), blanked),
    }
}

fn sweep(v: &mut serde_json::Value, file: &str, at: &str, blanked: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                let here = if at.is_empty() {
                    k.clone()
                } else {
                    format!("{at}.{k}")
                };
                if CREDENTIAL_KEYS.contains(&k.to_ascii_lowercase().as_str()) {
                    if val.as_str().map(|s| !s.is_empty()).unwrap_or(false) {
                        *val = serde_json::Value::String(String::new());
                        blanked.push(format!("{file}: {here}"));
                    }
                    continue;
                }
                sweep(val, file, &here, blanked);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, val) in items.iter_mut().enumerate() {
                sweep(val, file, &format!("{at}[{i}]"), blanked);
            }
        }
        _ => {}
    }
}

/// Every setting file worth keeping — and not `secrets.json`.
pub fn config_members(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut names: Vec<_> = rd
            .flatten()
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".json"))
            .filter(|n| n != "secrets.json")
            .filter(|n| !n.ends_with(".restored"))
            .filter(|n| n != PENDING)
            .collect();
        names.sort();
        out.extend(
            names
                .into_iter()
                .map(|n| (format!("config/{n}"), dir.join(&n))),
        );
    }
    // The talkgroup catalogs are the listener's own downloads and are not
    // re-fetchable without a RadioReference subscription.
    let cats = dir.join("catalogs");
    if let Ok(rd) = std::fs::read_dir(&cats) {
        let mut files: Vec<_> = rd
            .flatten()
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        files.sort();
        out.extend(
            files
                .into_iter()
                .map(|n| (format!("config/catalogs/{n}"), cats.join(&n))),
        );
    }
    out
}

/// Which recordings a tier takes, given every recording in the library and
/// the set retention protects.
pub fn audio_for(
    tier: &str,
    all: &[(i64, String)],
    protected: &std::collections::HashSet<i64>,
) -> Vec<(i64, String)> {
    match tier {
        "records" => Vec::new(),
        "kept" => all
            .iter()
            .filter(|(id, _)| protected.contains(id))
            .cloned()
            .collect(),
        _ => all.to_vec(),
    }
}

fn plan_for(app: &AppHandle, state: &AppState, tier: &str) -> Result<Plan, String> {
    let db = state.db.lock().unwrap().clone().ok_or("no library open")?;
    let (all, protected, calls) = {
        let c = db.lock().unwrap();
        let all = crate::library::all_audio(&c)?;
        let p = crate::retention::protected(&c);
        let mut set: std::collections::HashSet<i64> = p.fired;
        set.extend(p.incidents);
        // Starred calls are protected too, and are not in that struct.
        if let Ok(mut st) = c.prepare("SELECT id FROM calls WHERE starred = 1") {
            if let Ok(rows) = st.query_map([], |r| r.get::<_, i64>(0)) {
                set.extend(rows.flatten());
            }
        }
        let calls: i64 = c
            .query_row("SELECT COUNT(*) FROM calls", [], |r| r.get(0))
            .unwrap_or(0);
        (all, set, calls)
    };
    let chosen = audio_for(tier, &all, &protected);
    let dir = config_dir(app)?;
    let mut audio = Vec::new();
    let mut audio_bytes = 0u64;
    for (_, p) in &chosen {
        let path = PathBuf::from(p);
        let Ok(md) = std::fs::metadata(&path) else {
            continue;
        };
        // Kept relative to the recordings folder, dated directories and
        // all, so a restore puts each file back where the library expects
        // it rather than in one flat heap.
        let rel = relative_audio(p);
        let Some(name) = rel.to_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        audio_bytes += md.len();
        audio.push((format!("audio/{name}"), path));
    }
    let config = config_members(&dir);
    Ok(Plan {
        tier: tier.to_string(),
        config,
        audio,
        audio_bytes,
        db_bytes: 0,
        calls,
    })
}

fn sizes(app: &AppHandle, state: &AppState) -> Sizes {
    let mut out = Sizes::default();
    if let Ok(d) = config_dir(app) {
        out.config_bytes = config_members(&d)
            .iter()
            .filter_map(|(_, p)| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
    }
    if let Some(lib) = state.library_dir.lock().unwrap().clone() {
        if let Some(parent) = lib.parent() {
            out.db_bytes = std::fs::metadata(parent.join("calls.db"))
                .map(|m| m.len())
                .unwrap_or(0);
        }
    }
    for tier in ["kept", "everything"] {
        if let Ok(p) = plan_for(app, state, tier) {
            if tier == "kept" {
                out.kept_bytes = p.audio_bytes;
                out.kept_files = p.audio.len();
            } else {
                out.all_bytes = p.audio_bytes;
                out.all_files = p.audio.len();
                out.calls = p.calls;
            }
        }
    }
    out
}

// ----------------------------------------------------- writing an archive

/// The header entry, first in the archive so it can be read without
/// unpacking the rest.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Manifest {
    pub format: u32,
    pub app: String,
    /// Unix seconds.
    pub at: i64,
    pub name: String,
    pub tier: String,
    pub encrypted: bool,
    pub calls: i64,
    pub recordings: usize,
    pub config_files: usize,
    /// Total size of the members, before compression.
    pub bytes: u64,
    /// Credential fields blanked on the way in, as `file: path`. Shown at
    /// restore time so they can be entered again.
    #[serde(default)]
    pub redacted: Vec<String>,
    /// Said plainly, because a restore is usually a bad day: the archive
    /// deliberately does not contain the API tokens.
    pub note: String,
}

/// A tar sink that may be sealed. A boxed `dyn Write` would lose the
/// `finish` that closes age's stream, and an unfinished stream is an
/// archive that cannot be decrypted.
enum Sink<W: Write> {
    Plain(W),
    Sealed(Box<age::stream::StreamWriter<W>>),
}

impl<W: Write> Write for Sink<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Sink::Plain(w) => w.write(buf),
            Sink::Sealed(w) => w.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sink::Plain(w) => w.flush(),
            Sink::Sealed(w) => w.flush(),
        }
    }
}

impl<W: Write> Sink<W> {
    fn seal(w: W, passphrase: Option<&str>) -> Result<Self, String> {
        match passphrase {
            None => Ok(Sink::Plain(w)),
            Some(p) => {
                let e = age::Encryptor::with_user_passphrase(age::secrecy::SecretString::from(
                    p.to_string(),
                ));
                Ok(Sink::Sealed(Box::new(
                    e.wrap_output(w).map_err(|e| e.to_string())?,
                )))
            }
        }
    }
    fn finish(self) -> Result<(), String> {
        match self {
            Sink::Plain(mut w) => w.flush().map_err(|e| e.to_string()),
            Sink::Sealed(w) => w.finish().map(|_| ()).map_err(|e| e.to_string()),
        }
    }
}

/// A reader that hashes what passes through it, so each member's digest
/// costs no extra pass over the disk.
struct Hashing<R> {
    inner: R,
    hash: Sha256,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
}

fn add_file<W: Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    from: &Path,
) -> Result<(u64, String), String> {
    let md = std::fs::metadata(from).map_err(|e| format!("{}: {e}", from.display()))?;
    let f = std::fs::File::open(from).map_err(|e| format!("{}: {e}", from.display()))?;
    let mut h = tar::Header::new_gnu();
    h.set_size(md.len());
    h.set_mode(0o644);
    h.set_mtime(
        md.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    h.set_cksum();
    let mut hashing = Hashing {
        inner: f,
        hash: Sha256::new(),
    };
    tar.append_data(&mut h, name, &mut hashing)
        .map_err(|e| format!("{name}: {e}"))?;
    Ok((md.len(), crate::library::hex(&hashing.hash.finalize())))
}

fn add_bytes<W: Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    body: &[u8],
) -> Result<(u64, String), String> {
    let mut h = tar::Header::new_gnu();
    h.set_size(body.len() as u64);
    h.set_mode(0o600);
    h.set_mtime(crate::library::now() as u64);
    h.set_cksum();
    tar.append_data(&mut h, name, body)
        .map_err(|e| format!("{name}: {e}"))?;
    Ok((body.len() as u64, crate::s3::sha256_hex(body)))
}

/// The settings files as they will go in: credentials blanked, and read up
/// front so the manifest — which is the archive's first entry — can already
/// say what was blanked. They are a few hundred kilobytes between them.
/// A catalog is CSV and holds no credential, so it streams from disk.
type Redacted = (Vec<(String, Vec<u8>)>, Vec<String>);

fn redact_config(config: &[(String, PathBuf)]) -> Redacted {
    let mut bodies = Vec::new();
    let mut blanked = Vec::new();
    for (name, from) in config {
        if !name.ends_with(".json") {
            continue;
        }
        let Ok(body) = std::fs::read(from) else {
            continue;
        };
        let short = from
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let (body, found) = redact(&short, &body);
        blanked.extend(found);
        bodies.push((name.clone(), body));
    }
    (bodies, blanked)
}

/// A whole, valid copy of the database, taken while the app keeps running.
pub fn snapshot(db: &rusqlite::Connection, to: &Path) -> Result<u64, String> {
    if to.exists() {
        std::fs::remove_file(to).map_err(|e| e.to_string())?;
    }
    // The target is an expression, not a bound parameter, so the path is
    // quoted the way SQLite quotes string literals.
    let quoted = to.to_string_lossy().replace('\'', "''");
    db.execute_batch(&format!("VACUUM INTO '{quoted}'"))
        .map_err(|e| format!("snapshot: {e}"))?;
    std::fs::metadata(to)
        .map(|m| m.len())
        .map_err(|e| e.to_string())
}

/// Build the archive at `out`. `passphrase` seals it. Returns its manifest.
pub fn write_archive(
    plan: &Plan,
    db_snapshot: &Path,
    out: &Path,
    passphrase: Option<&str>,
    progress: &mut dyn FnMut(u64, u64, &str),
) -> Result<Manifest, String> {
    let total = plan.audio_bytes
        + plan.db_bytes
        + plan
            .config
            .iter()
            .filter_map(|(_, p)| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum::<u64>();
    let name = out
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let (bodies, blanked) = redact_config(&plan.config);
    let mut manifest = Manifest {
        format: FORMAT,
        app: env!("CARGO_PKG_VERSION").to_string(),
        at: crate::library::now(),
        name,
        tier: plan.tier.clone(),
        encrypted: passphrase.is_some(),
        calls: plan.calls,
        recordings: plan.audio.len(),
        config_files: plan.config.len(),
        bytes: total,
        redacted: blanked,
        note: "API tokens and passwords are deliberately not in this archive — secrets.json is \
               left out entirely and the credential fields in other settings are blanked. \
               After a restore, enter them again in Settings."
            .into(),
    };
    let f = std::fs::File::create(out).map_err(|e| e.to_string())?;
    let sink = Sink::seal(std::io::BufWriter::new(f), passphrase)?;
    let gz = GzEncoder::new(sink, Compression::new(6));
    let mut tar = tar::Builder::new(gz);
    let mut sums: Vec<(String, u64, String)> = Vec::new();
    let mut done = 0u64;

    add_bytes(
        &mut tar,
        MANIFEST,
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )?;

    progress(done, total, "database");
    let (n, sum) = add_file(&mut tar, "calls.db", db_snapshot)?;
    sums.push(("calls.db".into(), n, sum));
    done += n;

    for (name, body) in &bodies {
        progress(done, total, "settings");
        match add_bytes(&mut tar, name, body) {
            Ok((n, sum)) => {
                sums.push((name.clone(), n, sum));
                done += n;
            }
            Err(e) => eprintln!("backup: {e}"),
        }
    }
    // The catalogs, which are CSV and stream from disk.
    for (name, from) in plan.config.iter().filter(|(n, _)| !n.ends_with(".json")) {
        match add_file(&mut tar, name, from) {
            Ok((n, sum)) => {
                sums.push((name.clone(), n, sum));
                done += n;
            }
            // A file deleted between the plan and the run is not worth
            // failing a whole backup over.
            Err(e) => eprintln!("backup: {e}"),
        }
    }
    for (i, (name, from)) in plan.audio.iter().enumerate() {
        if i % 25 == 0 {
            progress(done, total, "recordings");
        }
        match add_file(&mut tar, name, from) {
            Ok((n, sum)) => {
                sums.push((name.clone(), n, sum));
                done += n;
            }
            Err(e) => eprintln!("backup: {e}"),
        }
    }
    let checks: Vec<serde_json::Value> = sums
        .iter()
        .map(|(p, n, s)| serde_json::json!({"path": p, "bytes": n, "sha256": s}))
        .collect();
    add_bytes(
        &mut tar,
        CHECKSUMS,
        serde_json::to_string(&checks)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )?;

    let gz = tar.into_inner().map_err(|e| e.to_string())?;
    let sink = gz.finish().map_err(|e| e.to_string())?;
    sink.finish()?;
    manifest.recordings = sums.iter().filter(|(p, _, _)| p.starts_with("audio/")).count();
    progress(total, total, "done");
    Ok(manifest)
}

/// Open an archive for reading, unsealing it if it was sealed.
fn read_archive(
    from: &Path,
    passphrase: Option<&str>,
) -> Result<tar::Archive<Box<dyn Read>>, String> {
    let f = std::fs::File::open(from).map_err(|e| e.to_string())?;
    let plain: Box<dyn Read> = if passphrase.is_some() {
        let id = age::scrypt::Identity::new(age::secrecy::SecretString::from(
            passphrase.unwrap_or_default().to_string(),
        ));
        let d = age::Decryptor::new_buffered(std::io::BufReader::new(f)).map_err(|e| match e {
            age::DecryptError::InvalidHeader => {
                "this file is not an age-encrypted archive".to_string()
            }
            e => e.to_string(),
        })?;
        Box::new(
            d.decrypt(std::iter::once(&id as &dyn age::Identity))
                .map_err(|e| match e {
                    // A wrong passphrase is the overwhelmingly likely cause
                    // of both, and "Decryption failed" does not say so.
                    age::DecryptError::NoMatchingKeys | age::DecryptError::DecryptionFailed => {
                        "that passphrase does not open this archive".to_string()
                    }
                    e => e.to_string(),
                })?,
        )
    } else {
        Box::new(std::io::BufReader::new(f))
    };
    Ok(tar::Archive::new(Box::new(flate2::read::GzDecoder::new(
        plain,
    )) as Box<dyn Read>))
}

/// The manifest alone, without unpacking. Cheap: it is the first entry.
pub fn read_manifest(from: &Path, passphrase: Option<&str>) -> Result<Manifest, String> {
    let mut a = read_archive(from, passphrase)?;
    for e in a.entries().map_err(|e| e.to_string())? {
        let mut e = e.map_err(|e| e.to_string())?;
        let is_manifest = e
            .path()
            .map(|p| p.to_string_lossy() == MANIFEST)
            .unwrap_or(false);
        if is_manifest {
            let mut s = String::new();
            e.read_to_string(&mut s).map_err(|e| e.to_string())?;
            return serde_json::from_str(&s).map_err(|e| e.to_string());
        }
    }
    Err("this archive has no manifest".into())
}

// -------------------------------------------------------- destinations

/// One archive sitting at a destination.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Archive {
    pub name: String,
    pub bytes: u64,
    /// Unix seconds where the destination reports one, else 0.
    pub at: i64,
    pub encrypted: bool,
}

fn stamp_name(tier: &str, sealed: bool) -> String {
    let when = chrono::Local::now().format("%Y%m%d-%H%M%S");
    format!(
        "hoosier-{when}-{tier}.tar.gz{}",
        if sealed { ".age" } else { "" }
    )
}

/// A name is only an archive if it is a bare file name. Archive names
/// arrive from the page — and through the web mirror, from another
/// machine — and are joined onto a folder path, so a name that can walk
/// out of that folder is not one.
fn is_archive(n: &str) -> bool {
    n.starts_with("hoosier-")
        && (n.ends_with(".tar.gz") || n.ends_with(".tar.gz.age"))
        && !n.contains('/')
        && !n.contains('\\')
        && !n.contains("..")
}

/// Sort newest first by the stamp in the name, which sorts lexically.
fn newest_first(mut v: Vec<Archive>) -> Vec<Archive> {
    v.sort_by(|a, b| b.name.cmp(&a.name));
    v
}

pub fn list_at(d: &Destination) -> Result<Vec<Archive>, String> {
    match d.kind.as_str() {
        "folder" => {
            let dir = PathBuf::from(&d.path);
            let rd = std::fs::read_dir(&dir)
                .map_err(|e| format!("{}: {e}", dir.display()))?;
            Ok(newest_first(
                rd.flatten()
                    .filter_map(|e| {
                        let n = e.file_name().into_string().ok()?;
                        if !is_archive(&n) {
                            return None;
                        }
                        let md = e.metadata().ok()?;
                        Some(Archive {
                            at: md
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|x| x.as_secs() as i64)
                                .unwrap_or(0),
                            bytes: md.len(),
                            encrypted: n.ends_with(".age"),
                            name: n,
                        })
                    })
                    .collect(),
            ))
        }
        "s3" => {
            let b = bucket_for(d)?;
            let prefix = prefix_of(d);
            Ok(newest_first(
                crate::s3::list(&b, &prefix)?
                    .into_iter()
                    .filter_map(|o| {
                        let n = o.key.rsplit('/').next()?.to_string();
                        if !is_archive(&n) {
                            return None;
                        }
                        Some(Archive {
                            at: chrono::DateTime::parse_from_rfc3339(&o.modified)
                                .map(|t| t.timestamp())
                                .unwrap_or(0),
                            bytes: o.bytes,
                            encrypted: n.ends_with(".age"),
                            name: n,
                        })
                    })
                    .collect(),
            ))
        }
        k => Err(format!("{k} is not a kind of destination")),
    }
}

fn prefix_of(d: &Destination) -> String {
    let p = d.prefix.trim().trim_matches('/');
    if p.is_empty() {
        String::new()
    } else {
        format!("{p}/")
    }
}

fn remove_at(d: &Destination, name: &str) -> Result<(), String> {
    if !is_archive(name) {
        return Err("that is not a backup archive".into());
    }
    match d.kind.as_str() {
        "folder" => std::fs::remove_file(PathBuf::from(&d.path).join(name))
            .map_err(|e| e.to_string()),
        "s3" => crate::s3::delete(&bucket_for(d)?, &format!("{}{name}", prefix_of(d))),
        k => Err(format!("{k} is not a kind of destination")),
    }
}

/// Send a finished archive, and drop the oldest ones past `keep`.
fn deliver(
    d: &Destination,
    file: &Path,
    name: &str,
    progress: &mut dyn FnMut(u64, u64, &str),
) -> Result<(), String> {
    match d.kind.as_str() {
        "folder" => {
            let dir = PathBuf::from(&d.path);
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            // Write beside the target and rename, so an interrupted copy
            // never looks like a finished backup.
            let part = dir.join(format!("{name}.part"));
            let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
            progress(0, size, "copying");
            std::fs::copy(file, &part).map_err(|e| format!("{}: {e}", part.display()))?;
            std::fs::rename(&part, dir.join(name)).map_err(|e| e.to_string())?;
            progress(size, size, "copied");
            Ok(())
        }
        "s3" => {
            let b = bucket_for(d)?;
            let key = format!("{}{name}", prefix_of(d));
            crate::s3::put_file(&b, &key, file, &mut |done, total| {
                progress(done, total, "uploading")
            })
        }
        k => Err(format!("{k} is not a kind of destination")),
    }
}

fn prune(d: &Destination) -> Vec<String> {
    if d.keep == 0 {
        return Vec::new();
    }
    let Ok(have) = list_at(d) else {
        return Vec::new();
    };
    have.into_iter()
        .skip(d.keep as usize)
        .filter(|a| remove_at(d, &a.name).is_ok())
        .map(|a| a.name)
        .collect()
}

// ------------------------------------------------------------- running

fn say(app: &AppHandle, dest: &str, phase: &str, detail: String, done: u64, total: u64) {
    let _ = app.emit(
        "backup",
        serde_json::json!({
            "dest": dest, "phase": phase, "detail": detail,
            "done": done, "total": total,
        }),
    );
}

/// One backup at a time: the timer and a hand-pressed Back up now would
/// otherwise write the same snapshot file from two threads. Losing that
/// race is not a failed backup, so it is not recorded as one — otherwise
/// the timer would mark every due destination failed while a hand-pressed
/// run was still going.
const BUSY: &str = "a backup is already running";

static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Take a backup to one destination. Blocking; the command wraps it in a
/// thread.
pub fn run_to(app: &AppHandle, state: &AppState, dest: &Destination) -> Result<Outcome, String> {
    if RUNNING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err(BUSY.into());
    }
    let _guard = Guard;
    let began = std::time::Instant::now();
    let tier = if dest.tier.trim().is_empty() {
        state.backup.lock().unwrap().tier.clone()
    } else {
        dest.tier.clone()
    };
    let tier = if TIERS.contains(&tier.as_str()) {
        tier
    } else {
        "kept".to_string()
    };
    let sealed = dest.encrypt || dest.kind == "s3";
    let passphrase = if sealed {
        // A sealed archive is unreadable without the passphrase, so the
        // first one does not go out until the listener has said they have
        // it somewhere other than this machine.
        if !state.backup.lock().unwrap().passphrase_ack {
            return Err("write the backup passphrase down first — a sealed backup cannot be \
                        opened without it. Backups → Show passphrase."
                .into());
        }
        Some(crate::secrets::get(PASS_KEY).ok_or(
            "this destination is encrypted but no passphrase is set — set one in Backups",
        )?)
    } else {
        None
    };
    let work = staging(app)?;
    let snap = work.join("calls.db");
    let name = stamp_name(&tier, sealed);
    let archive = work.join(&name);
    // A failed run should not leave a half-written archive or a copy of the
    // database in the staging folder.
    let clean = |a: &Path, s: &Path| {
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(s);
    };

    say(app, &dest.id, "planning", "Looking at the library".into(), 0, 0);
    let mut plan = plan_for(app, state, &tier)?;

    say(app, &dest.id, "snapshot", "Copying the database".into(), 0, 0);
    let db = state.db.lock().unwrap().clone().ok_or("no library open")?;
    plan.db_bytes = {
        let c = db.lock().unwrap();
        snapshot(&c, &snap)?
    };

    let did = (|| -> Result<Manifest, String> {
        let m = write_archive(
            &plan,
            &snap,
            &archive,
            passphrase.as_deref(),
            &mut |done, total, what| {
                say(
                    app,
                    &dest.id,
                    "packing",
                    format!("Packing {what}"),
                    done,
                    total,
                )
            },
        )?;
        deliver(dest, &archive, &name, &mut |done, total, what| {
            say(
                app,
                &dest.id,
                "sending",
                format!("{} to {}", cap(what), dest.name),
                done,
                total,
            )
        })?;
        Ok(m)
    })();
    let bytes = std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
    clean(&archive, &snap);
    let m = did?;
    let dropped = prune(dest);
    if !dropped.is_empty() {
        say(
            app,
            &dest.id,
            "pruning",
            format!("Removed {} older backup(s)", dropped.len()),
            0,
            0,
        );
    }
    let out = Outcome {
        at: crate::library::now(),
        ok: true,
        name,
        bytes,
        files: m.recordings + m.config_files + 1,
        secs: began.elapsed().as_secs_f64(),
        error: String::new(),
    };
    say(app, &dest.id, "done", format!("{} — {}", out.name, human(bytes)), 0, 0);
    Ok(out)
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

pub fn human(b: u64) -> String {
    const K: f64 = 1024.0;
    let b = b as f64;
    if b < K {
        format!("{b:.0} B")
    } else if b < K * K {
        format!("{:.0} KB", b / K)
    } else if b < K * K * K {
        format!("{:.1} MB", b / (K * K))
    } else {
        format!("{:.2} GB", b / (K * K * K))
    }
}

fn staging(app: &AppHandle) -> Result<PathBuf, String> {
    let d = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("backup-work");
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d)
}

// ------------------------------------------------------------- restore

/// A restore that has been unpacked and is waiting for the next start.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pending {
    pub at: i64,
    pub from: String,
    /// Archive members staged as `.restored`, by their live path.
    pub files: Vec<String>,
    pub recordings: usize,
    pub tier: String,
}

/// Files a restore has put in place but not yet committed to. Dropping
/// this removes them, so every way out of a restore that does not write
/// the marker — a refused member, a bad checksum, an error partway — ends
/// with the library exactly as it was.
struct Staged(Vec<String>);

impl Drop for Staged {
    fn drop(&mut self) {
        for f in &self.0 {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// What a restore brought back.
#[derive(Serialize, Clone, Debug)]
pub struct Restored {
    pub manifest: Manifest,
    pub database: bool,
    pub config_files: usize,
    pub recordings: usize,
    /// Members whose checksum did not match. Empty is the good case.
    pub bad: Vec<String>,
    pub note: String,
}

/// Members a restore will not unpack, whatever the archive says.
///
/// No archive this app writes contains any of them, so one that does was
/// edited by somebody — and an unencrypted archive sitting on a shared
/// drive is an editable file. A planted `secrets.json` would hand over the
/// web token at the next start; a member already named `.restored` would
/// be swapped into place without ever having been checked; a path with
/// `..` in it writes outside the library. Any of these fails the whole
/// restore rather than being skipped quietly.
pub fn refused(name: &str) -> Option<&'static str> {
    if name.contains("..") || name.starts_with('/') || name.starts_with("./") {
        return Some("a path that points outside the library");
    }
    if name.contains(".restored") || name.ends_with(PENDING) {
        return Some("a name only a restore in progress uses");
    }
    if name.strip_prefix("config/") == Some("secrets.json") {
        return Some("a backup never contains the secret store");
    }
    None
}

/// Where a recording sits relative to the recordings folder. The library
/// files calls under dated directories (`calls/2026/09/11/…`), so the tail
/// after the last `calls/` is what has to be kept: flattening it to the
/// bare file name loses the shape of the library, and points restored rows
/// at files that are not there.
pub fn relative_audio(old: &str) -> PathBuf {
    let p = Path::new(old);
    let tail = old
        .rfind("/calls/")
        .map(|i| &old[i + "/calls/".len()..])
        .filter(|t| !t.is_empty() && !t.contains("..") && !t.starts_with('/'));
    match tail {
        Some(t) => PathBuf::from(t),
        None => PathBuf::from(p.file_name().unwrap_or_default()),
    }
}

/// Point every call's `audio` column at this library's recordings folder,
/// keeping where the recording sits inside it. Returns how many rows moved.
pub fn repoint_audio(c: &rusqlite::Connection, lib: &Path) -> Result<usize, String> {
    let rows: Vec<(i64, String)> = {
        let mut st = c
            .prepare("SELECT id, audio FROM calls WHERE audio IS NOT NULL AND audio <> ''")
            .map_err(|e| e.to_string())?;
        let it = st
            .query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        it.filter_map(Result::ok).collect()
    };
    let mut moved = 0;
    for (id, old) in rows {
        let want = lib.join(relative_audio(&old));
        if want == Path::new(&old) {
            continue;
        }
        c.execute(
            "UPDATE calls SET audio = ?1 WHERE id = ?2",
            rusqlite::params![want.to_string_lossy(), id],
        )
        .map_err(|e| e.to_string())?;
        moved += 1;
    }
    Ok(moved)
}

/// Fetch an archive from a destination into the staging folder.
fn fetch(app: &AppHandle, d: &Destination, name: &str) -> Result<PathBuf, String> {
    if !is_archive(name) {
        return Err("that is not a backup archive".into());
    }
    match d.kind.as_str() {
        "folder" => Ok(PathBuf::from(&d.path).join(name)),
        "s3" => {
            let to = staging(app)?.join(name);
            let b = bucket_for(d)?;
            crate::s3::get_to_file(&b, &format!("{}{name}", prefix_of(d)), &to)?;
            Ok(to)
        }
        k => Err(format!("{k} is not a kind of destination")),
    }
}

/// Unpack an archive next to the live files, without replacing anything
/// that is open. The database and the settings land as `.restored` and a
/// marker is written; [`apply_pending`] swaps them in at the next start.
/// Recordings are copied straight in — nothing holds them open — and are
/// never overwritten.
#[allow(clippy::too_many_arguments)]
pub fn restore_from(
    app: &AppHandle,
    state: &AppState,
    from: &Path,
    passphrase: Option<&str>,
    want_db: bool,
    want_config: bool,
    want_audio: bool,
) -> Result<Restored, String> {
    let sealed = from
        .file_name()
        .map(|n| n.to_string_lossy().ends_with(".age"))
        .unwrap_or(false);
    let pass = if sealed {
        Some(
            passphrase
                .map(|s| s.to_string())
                .or_else(|| crate::secrets::get(PASS_KEY))
                .ok_or("this archive is encrypted — the passphrase is needed to open it")?,
        )
    } else {
        None
    };
    let manifest = read_manifest(from, pass.as_deref())?;
    if manifest.format > FORMAT {
        return Err(format!(
            "this archive was written by a newer version (format {}); update first",
            manifest.format
        ));
    }
    let cfg = config_dir(app)?;
    let lib = state
        .library_dir
        .lock()
        .unwrap()
        .clone()
        .ok_or("no library open")?;
    let db_dir = lib.parent().unwrap_or(&lib).to_path_buf();
    std::fs::create_dir_all(&lib).map_err(|e| e.to_string())?;

    let mut sums: HashMap<String, String> = HashMap::new();
    // Anything staged is removed unless the restore reaches the end and
    // hands the list over to the marker: a half-unpacked restore must not
    // leave files behind for a later start to swap in.
    let mut staged = Staged(Vec::new());
    let mut bad: Vec<String> = Vec::new();
    let mut database = false;
    let mut config_files = 0usize;
    let mut recordings = 0usize;

    let mut a = read_archive(from, pass.as_deref())?;
    for e in a.entries().map_err(|e| e.to_string())? {
        let mut e = e.map_err(|e| e.to_string())?;
        // Only regular files are unpacked. This app's own archives hold
        // nothing else, but tar carries directory entries, symlinks and
        // hard links too, and a hand-made archive will have them — a
        // directory entry called `config/catalogs/` used to fail the whole
        // restore, and a link is something to refuse rather than follow.
        if !e.header().entry_type().is_file() {
            continue;
        }
        let name = e
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        // Nothing in an archive may point outside where it is being
        // unpacked, whoever wrote it.
        if let Some(why) = refused(&name) {
            bad.push(format!("{name} — {why}"));
            continue;
        }
        if name == CHECKSUMS {
            let mut s = String::new();
            e.read_to_string(&mut s).map_err(|e| e.to_string())?;
            if let Ok(v) = serde_json::from_str::<Vec<serde_json::Value>>(&s) {
                for x in v {
                    if let (Some(p), Some(h)) = (x["path"].as_str(), x["sha256"].as_str()) {
                        sums.insert(p.to_string(), h.to_string());
                    }
                }
            }
            continue;
        }
        let to = if name == "calls.db" {
            if !want_db {
                continue;
            }
            database = true;
            Some(db_dir.join("calls.db.restored"))
        } else if let Some(rest) = name.strip_prefix("config/") {
            if !want_config {
                continue;
            }
            if rest.contains('/') {
                // config/catalogs/<file> — catalogs are additive, so they
                // go straight in.
                let p = cfg.join(rest);
                if let Some(d) = p.parent() {
                    std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
                }
                config_files += 1;
                Some(p)
            } else {
                config_files += 1;
                Some(cfg.join(format!("{rest}.restored")))
            }
        } else if let Some(rest) = name.strip_prefix("audio/") {
            if !want_audio {
                continue;
            }
            let p = lib.join(rest);
            if p.exists() {
                continue;
            }
            // Dated directories, which a fresh library does not have yet.
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
            }
            recordings += 1;
            Some(p)
        } else {
            None
        };
        let Some(to) = to else { continue };
        let mut h = Sha256::new();
        // A settings file is small and needs the live credentials folded
        // back into it, so it is buffered; everything else streams. The
        // digest is taken over the archive's own bytes either way, or it
        // would not match the checksum recorded when it was packed.
        let settings_json = name.starts_with("config/")
            && name.ends_with(".json")
            && name.matches('/').count() == 1;
        if settings_json {
            let mut body = Vec::new();
            e.read_to_end(&mut body).map_err(|e| e.to_string())?;
            h.update(&body);
            let short = name.trim_start_matches("config/").to_string();
            let live = std::fs::read(cfg.join(&short)).ok();
            let body = carry_credentials(live.as_deref(), &body, &short, &manifest.redacted);
            std::fs::write(&to, &body).map_err(|e| format!("{}: {e}", to.display()))?;
        } else {
            let mut f = std::fs::File::create(&to).map_err(|e| format!("{}: {e}", to.display()))?;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = e.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            }
        }
        let got = crate::library::hex(&h.finalize());
        if to.to_string_lossy().ends_with(".restored") {
            staged.0.push(to.to_string_lossy().to_string());
        }
        sums.insert(format!("@{name}"), got);
    }
    // A restored database carries the paths of the machine it was backed
    // up from. Recordings land in this library's own folder, so the rows
    // are pointed at them; without this every call in a restore onto a new
    // machine plays nothing.
    let mut repointed = 0usize;
    if database {
        let staged = db_dir.join("calls.db.restored");
        match rusqlite::Connection::open(&staged) {
            Ok(c) => match repoint_audio(&c, &lib) {
                Ok(n) => repointed = n,
                Err(e) => bad.push(format!("pointing the recordings at {}: {e}", lib.display())),
            },
            Err(e) => bad.push(format!("opening the restored database: {e}")),
        }
    }
    // Checksums arrive last, so verification happens after the whole walk.
    for (k, got) in sums.iter().filter(|(k, _)| k.starts_with('@')) {
        let name = &k[1..];
        if let Some(want) = sums.get(name) {
            if want != got {
                bad.push(name.to_string());
            }
        }
    }
    // A refused member, or one that did not survive the trip, means no
    // marker is written — and the staged files are left in the guard,
    // which removes them as it drops, so the live library is exactly as it
    // was. Only a clean walk hands the list over.
    if bad.is_empty() && !staged.0.is_empty() {
        let p = Pending {
            at: crate::library::now(),
            from: from
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            files: std::mem::take(&mut staged.0),
            recordings,
            tier: manifest.tier.clone(),
        };
        std::fs::write(
            cfg.join(PENDING),
            serde_json::to_string_pretty(&p).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    let note = if bad.is_empty() {
        format!(
            "Everything checked out{}. Restart the app to finish: the restored database and \
             settings are swapped in at the next start, and what is there now is kept beside \
             them. API tokens and passwords are not in a backup — enter them again in Settings.",
            if repointed > 0 {
                format!(", and {repointed} recording(s) were pointed at this library")
            } else {
                String::new()
            }
        )
    } else {
        format!(
            "{} file(s) did not check out, so nothing was staged and the library is untouched.",
            bad.len()
        )
    };
    Ok(Restored {
        manifest,
        database,
        config_files,
        recordings,
        bad,
        note,
    })
}

/// Called first at start: swap in whatever a restore staged. What was
/// there is kept as `.replaced-<stamp>` rather than deleted, so a restore
/// from the wrong archive is not the end of the story.
pub fn apply_pending(app: &AppHandle) {
    let Ok(cfg) = config_dir(app) else { return };
    let marker = cfg.join(PENDING);
    let Ok(text) = std::fs::read_to_string(&marker) else {
        return;
    };
    let Ok(p) = serde_json::from_str::<Pending>(&text) else {
        let _ = std::fs::remove_file(&marker);
        return;
    };
    swap_in(&p.files);
    let _ = std::fs::remove_file(&marker);
}

/// Put each staged `.restored` file in place of the live one, keeping what
/// was there. Returns the files that moved.
fn swap_in(files: &[String]) -> Vec<PathBuf> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut moved = Vec::new();
    for staged in files {
        let staged = PathBuf::from(staged);
        let Some(live) = staged
            .to_string_lossy()
            .strip_suffix(".restored")
            .map(PathBuf::from)
        else {
            continue;
        };
        if !staged.exists() {
            continue;
        }
        if live.exists() {
            let base = live.file_name().unwrap_or_default().to_string_lossy().to_string();
            let keep = live.with_file_name(format!("{base}.replaced-{stamp}"));
            if let Err(e) = std::fs::rename(&live, &keep) {
                eprintln!("restore: keeping {} failed: {e}", live.display());
                continue;
            }
            // The library runs in WAL mode. A `-wal` left behind by a
            // process that did not close cleanly would be replayed onto
            // whatever `calls.db` it finds beside it — which after a swap
            // is the restored database, not the one those frames came
            // from. The pair moves with the file it belongs to, so the
            // old trio stays openable and the new file starts clean (a
            // `VACUUM INTO` snapshot has no WAL of its own).
            for tail in ["-wal", "-shm"] {
                let from = live.with_file_name(format!("{base}{tail}"));
                if from.exists() {
                    let to = live.with_file_name(format!("{base}.replaced-{stamp}{tail}"));
                    if let Err(e) = std::fs::rename(&from, &to) {
                        eprintln!("restore: moving {} failed: {e}", from.display());
                    }
                }
            }
        }
        match std::fs::rename(&staged, &live) {
            Ok(()) => {
                eprintln!("restore: {} is in place", live.display());
                moved.push(live);
            }
            Err(e) => eprintln!("restore: {} failed: {e}", live.display()),
        }
    }
    moved
}

// ------------------------------------------------------------ schedule

/// Hourly check: run any destination whose last good backup is older than
/// the interval. Backups are I/O-heavy, so they go one at a time.
pub fn spawn_ticker(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(15 * 60));
        let state = app.state::<AppState>();
        let s = state.backup.lock().unwrap().clone();
        if s.every_hours == 0 {
            continue;
        }
        let now = crate::library::now();
        let due: Vec<Destination> = s
            .destinations
            .iter()
            .filter(|d| d.enabled)
            .filter(|d| {
                s.last
                    .get(&d.id)
                    .filter(|o| o.ok)
                    .map(|o| now - o.at >= s.every_hours as i64 * 3600)
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        for d in due {
            let out = match run_to(&app, &state, &d) {
                Ok(o) => o,
                Err(e) => {
                    say(&app, &d.id, "failed", e.clone(), 0, 0);
                    Outcome {
                        at: now,
                        ok: false,
                        error: e,
                        ..Default::default()
                    }
                }
            };
            remember(&app, &state, &d.id, out);
        }
    });
}

fn remember(app: &AppHandle, state: &AppState, dest: &str, out: Outcome) {
    if !out.ok && out.error == BUSY {
        return;
    }
    let mut s = state.backup.lock().unwrap();
    s.last.insert(dest.to_string(), out);
    let _ = store(app, &s);
}

// ------------------------------------------------------------ commands

fn clean_dest(mut d: Destination) -> Destination {
    d.id = crate::analyzers::clean_line(&d.id, 40);
    d.name = crate::analyzers::clean_line(&d.name, 60);
    d.kind = if d.kind == "s3" { "s3" } else { "folder" }.into();
    d.path = crate::analyzers::clean_line(&d.path, 400);
    d.endpoint = crate::analyzers::clean_line(&d.endpoint, 200);
    d.region = crate::analyzers::clean_line(&d.region, 40);
    d.bucket = crate::analyzers::clean_line(&d.bucket, 80);
    d.prefix = crate::analyzers::clean_line(&d.prefix, 120);
    d.tier = if TIERS.contains(&d.tier.as_str()) {
        d.tier
    } else {
        String::new()
    };
    // Anything that leaves this machine over a network is encrypted; the
    // page does not get to turn that off.
    if d.kind == "s3" {
        d.encrypt = true;
    }
    d.keep = d.keep.min(1000);
    d
}

fn find(state: &AppState, id: &str) -> Result<Destination, String> {
    state
        .backup
        .lock()
        .unwrap()
        .destinations
        .iter()
        .find(|d| d.id == id)
        .cloned()
        .ok_or_else(|| "no such destination".to_string())
}

#[tauri::command]
pub fn backup_get(app: AppHandle, state: State<AppState>) -> View {
    let settings = state.backup.lock().unwrap().clone();
    let keys = settings
        .destinations
        .iter()
        .map(|d| {
            (
                d.id.clone(),
                crate::secrets::get(&key_id(&d.id)).is_some()
                    && crate::secrets::get(&key_secret(&d.id)).is_some(),
            )
        })
        .collect();
    let pending = config_dir(&app)
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join(PENDING)).ok())
        .and_then(|t| serde_json::from_str(&t).ok());
    View {
        settings,
        keys,
        passphrase_set: crate::secrets::get(PASS_KEY).is_some(),
        pending,
    }
}

/// What a backup would weigh. Separate from [`backup_get`] because working
/// it out stats every recording in the library, and the page should draw
/// before it waits for that.
#[tauri::command]
pub fn backup_sizes(app: AppHandle, state: State<AppState>) -> Sizes {
    sizes(&app, &state)
}

#[tauri::command]
pub fn backup_set(
    app: AppHandle,
    state: State<AppState>,
    settings: Settings,
) -> Result<Settings, String> {
    let mut s = settings;
    s.destinations = s.destinations.into_iter().map(clean_dest).collect();
    s.destinations.retain(|d| !d.id.is_empty());
    if !TIERS.contains(&s.tier.as_str()) {
        s.tier = "kept".into();
    }
    s.every_hours = s.every_hours.min(24 * 14);
    // Keep the outcomes the page never sees.
    let ids: Vec<String> = s.destinations.iter().map(|d| d.id.clone()).collect();
    let old = state.backup.lock().unwrap().last.clone();
    s.last = old.into_iter().filter(|(k, _)| ids.contains(k)).collect();
    store(&app, &s)?;
    *state.backup.lock().unwrap() = s.clone();
    Ok(s)
}

/// Save (or forget, with empty strings) the keys for an S3 destination.
/// They go in the secret store, never in `backup.json`.
#[tauri::command]
pub fn backup_credentials(dest: String, access: String, secret: String) -> Result<(), String> {
    let dest = crate::analyzers::clean_line(&dest, 40);
    if dest.is_empty() {
        return Err("which destination?".into());
    }
    if access.trim().is_empty() && secret.trim().is_empty() {
        crate::secrets::remove(&key_id(&dest))?;
        return crate::secrets::remove(&key_secret(&dest));
    }
    crate::secrets::set(&key_id(&dest), access.trim())?;
    crate::secrets::set(&key_secret(&dest), secret.trim())
}

/// The passphrase every sealed archive uses. Returns it, because the
/// listener has to write it down — without it a backup is a wall of noise.
/// `set` empty generates a fresh one.
#[tauri::command]
pub fn backup_passphrase(
    app: AppHandle,
    state: State<AppState>,
    set: String,
    show: bool,
) -> Result<String, String> {
    if !set.trim().is_empty() {
        if set.trim().chars().count() < 12 {
            return Err("a backup passphrase needs at least 12 characters".into());
        }
        crate::secrets::set(PASS_KEY, set.trim())?;
    } else if crate::secrets::get(PASS_KEY).is_none() {
        crate::secrets::set(PASS_KEY, &new_passphrase())?;
        // A freshly generated one nobody has seen yet. The acknowledgement
        // travels in backup.json, so a restore onto a new machine would
        // otherwise arrive already saying yes.
        let mut s = state.backup.lock().unwrap();
        if s.passphrase_ack {
            s.passphrase_ack = false;
            let _ = store(&app, &s);
        }
    }
    // Whether the listener has actually written it down is theirs to say —
    // it arrives through backup_set when they confirm the modal, not from
    // the act of looking at it.
    if show {
        crate::secrets::get(PASS_KEY).ok_or_else(|| "no passphrase saved".to_string())
    } else {
        Ok(String::new())
    }
}

/// Can this destination be written? Catches a wrong region, a read-only
/// key or an unmounted share at setup instead of at the first backup.
#[tauri::command]
pub fn backup_check(state: State<AppState>, dest: String) -> Result<String, String> {
    let d = find(&state, &dest)?;
    match d.kind.as_str() {
        "folder" => {
            let dir = PathBuf::from(&d.path);
            if d.path.trim().is_empty() {
                return Err("choose a folder first".into());
            }
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let probe = dir.join(".hoosier-write-test");
            std::fs::write(&probe, b"hoosier").map_err(|e| format!("{}: {e}", dir.display()))?;
            let _ = std::fs::remove_file(&probe);
            let have = list_at(&d).map(|v| v.len()).unwrap_or(0);
            Ok(format!(
                "{} is writable. {have} backup(s) already there.",
                dir.display()
            ))
        }
        "s3" => {
            let b = bucket_for(&d)?;
            crate::s3::check(&b, &prefix_of(&d))
        }
        k => Err(format!("{k} is not a kind of destination")),
    }
}

#[tauri::command]
pub fn backup_run(app: AppHandle, state: State<AppState>, dest: String) -> Result<(), String> {
    let d = find(&state, &dest)?;
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let out = match run_to(&app, &state, &d) {
            Ok(o) => o,
            Err(e) => {
                say(&app, &d.id, "failed", e.clone(), 0, 0);
                Outcome {
                    at: crate::library::now(),
                    ok: false,
                    error: e,
                    ..Default::default()
                }
            }
        };
        remember(&app, &state, &d.id, out);
    });
    Ok(())
}

#[tauri::command]
pub fn backup_list(state: State<AppState>, dest: String) -> Result<Vec<Archive>, String> {
    list_at(&find(&state, &dest)?)
}

#[tauri::command]
pub fn backup_forget(state: State<AppState>, dest: String, name: String) -> Result<(), String> {
    remove_at(&find(&state, &dest)?, &name)
}

/// What is inside an archive, without unpacking it.
#[tauri::command]
pub fn backup_peek(
    app: AppHandle,
    state: State<AppState>,
    dest: String,
    name: String,
) -> Result<Manifest, String> {
    let d = find(&state, &dest)?;
    let f = fetch(&app, &d, &name)?;
    let pass = if name.ends_with(".age") {
        crate::secrets::get(PASS_KEY)
    } else {
        None
    };
    read_manifest(&f, pass.as_deref())
}

#[tauri::command]
pub fn backup_restore(
    app: AppHandle,
    state: State<AppState>,
    dest: String,
    name: String,
    passphrase: String,
    database: bool,
    config: bool,
    audio: bool,
) -> Result<Restored, String> {
    if !database && !config && !audio {
        return Err("choose what to bring back".into());
    }
    let d = find(&state, &dest)?;
    let f = fetch(&app, &d, &name)?;
    let pass = if passphrase.trim().is_empty() {
        None
    } else {
        Some(passphrase.trim().to_string())
    };
    let out = restore_from(&app, &state, &f, pass.as_deref(), database, config, audio);
    // A bucket archive was downloaded to get here; it is no longer needed
    // and can be gigabytes.
    if d.kind == "s3" {
        let _ = std::fs::remove_file(&f);
    }
    out
}

/// Throw away a staged restore that has not been applied yet.
#[tauri::command]
pub fn backup_cancel_restore(app: AppHandle) -> Result<(), String> {
    let cfg = config_dir(&app)?;
    if let Ok(t) = std::fs::read_to_string(cfg.join(PENDING)) {
        if let Ok(p) = serde_json::from_str::<Pending>(&t) {
            for f in p.files {
                let _ = std::fs::remove_file(f);
            }
        }
    }
    let _ = std::fs::remove_file(cfg.join(PENDING));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hs-backup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn library(dir: &Path) -> rusqlite::Connection {
        let c = crate::library::open(dir).unwrap();
        for i in 1..=3 {
            c.execute(
                "INSERT INTO calls (start, secs, tg, tg_name, unit, freq_hz, modulation, system, audio, transcript, starred)
                 VALUES (?1, 3.0, 1001, 'Example Dispatch', 0, 851000000, 'p25', 'Example', ?2, 'testing', ?3)",
                rusqlite::params![
                    1_700_000_000i64 + i,
                    format!("{}/c{i}.wav", dir.join("calls").display()),
                    i == 1
                ],
            )
            .unwrap();
        }
        c
    }

    #[test]
    fn a_snapshot_is_a_whole_database_taken_while_the_app_runs() {
        let d = tmp("snap");
        let c = library(&d);
        let to = d.join("copy.db");
        let n = snapshot(&c, &to).unwrap();
        assert!(n > 0);
        // The original is still usable afterwards.
        let live: i64 = c
            .query_row("SELECT COUNT(*) FROM calls", [], |r| r.get(0))
            .unwrap();
        let copy = rusqlite::Connection::open(&to).unwrap();
        let copied: i64 = copy
            .query_row("SELECT COUNT(*) FROM calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(live, copied);
        let ok: String = copy
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
        // And the search index came with it.
        let hits: i64 = copy
            .query_row(
                "SELECT COUNT(*) FROM calls_fts WHERE calls_fts MATCH 'testing'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    // The one file whose contents must never travel.
    #[test]
    fn the_secret_store_is_not_in_the_archive() {
        let d = tmp("secrets");
        std::fs::write(d.join("secrets.json"), r#"{"bot-token":"12345:shh"}"#).unwrap();
        std::fs::write(d.join("alerts.json"), "{}").unwrap();
        std::fs::write(d.join("backup.json"), "{}").unwrap();
        let members = config_members(&d);
        let names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"config/alerts.json"), "{names:?}");
        assert!(
            !names.iter().any(|n| n.contains("secrets")),
            "secrets.json must never be a member: {names:?}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn catalogs_come_along_because_they_cannot_be_re_downloaded_for_free() {
        let d = tmp("cats");
        std::fs::create_dir_all(d.join("catalogs")).unwrap();
        std::fs::write(d.join("catalogs/1234.csv"), "tg,name\n1001,Example\n").unwrap();
        let members = config_members(&d);
        assert!(members
            .iter()
            .any(|(n, _)| n == "config/catalogs/1234.csv"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_tier_decides_which_recordings_go() {
        let all = vec![
            (1, "/a/1.wav".to_string()),
            (2, "/a/2.wav".to_string()),
            (3, "/a/3.wav".to_string()),
        ];
        let kept: std::collections::HashSet<i64> = [2].into_iter().collect();
        assert!(audio_for("records", &all, &kept).is_empty());
        assert_eq!(audio_for("kept", &all, &kept).len(), 1);
        assert_eq!(audio_for("everything", &all, &kept).len(), 3);
    }

    /// Pack a library, unpack it somewhere else, and check that what comes
    /// out is the database that went in — the whole point of the feature.
    fn round_trip(passphrase: Option<&str>) {
        let d = tmp(if passphrase.is_some() { "rt-sealed" } else { "rt-plain" });
        let c = library(&d);
        std::fs::create_dir_all(d.join("calls")).unwrap();
        for i in 1..=3 {
            std::fs::write(d.join(format!("calls/c{i}.wav")), format!("audio {i}")).unwrap();
        }
        std::fs::write(d.join("alerts.json"), r#"{"destinations":[]}"#).unwrap();
        let snap = d.join("snap.db");
        snapshot(&c, &snap).unwrap();
        let plan = Plan {
            tier: "everything".into(),
            config: vec![("config/alerts.json".into(), d.join("alerts.json"))],
            audio: (1..=3)
                .map(|i| {
                    (
                        format!("audio/c{i}.wav"),
                        d.join(format!("calls/c{i}.wav")),
                    )
                })
                .collect(),
            audio_bytes: 21,
            db_bytes: std::fs::metadata(&snap).unwrap().len(),
            calls: 3,
        };
        let out = d.join(if passphrase.is_some() {
            "hoosier-20260912-000000-everything.tar.gz.age"
        } else {
            "hoosier-20260912-000000-everything.tar.gz"
        });
        let m = write_archive(&plan, &snap, &out, passphrase, &mut |_, _, _| {}).unwrap();
        assert_eq!(m.calls, 3);
        assert_eq!(m.recordings, 3);
        assert_eq!(m.encrypted, passphrase.is_some());
        assert!(std::fs::metadata(&out).unwrap().len() > 0);

        // The manifest reads back without unpacking the rest.
        let back = read_manifest(&out, passphrase).unwrap();
        assert_eq!(back.tier, "everything");
        assert_eq!(back.format, FORMAT);

        // Unpack by hand into a fresh directory and check the database.
        let into = d.join("out");
        std::fs::create_dir_all(&into).unwrap();
        let mut a = read_archive(&out, passphrase).unwrap();
        let mut seen = Vec::new();
        for e in a.entries().unwrap() {
            let mut e = e.unwrap();
            let name = e.path().unwrap().to_string_lossy().to_string();
            seen.push(name.clone());
            let to = into.join(name.replace('/', "_"));
            let mut f = std::fs::File::create(&to).unwrap();
            std::io::copy(&mut e, &mut f).unwrap();
        }
        assert_eq!(seen[0], MANIFEST, "the manifest has to come first");
        assert!(seen.contains(&"calls.db".to_string()));
        assert!(seen.contains(&CHECKSUMS.to_string()));
        assert_eq!(
            std::fs::read_to_string(into.join("audio_c2.wav")).unwrap(),
            "audio 2"
        );
        let restored = rusqlite::Connection::open(into.join("calls.db")).unwrap();
        let n: i64 = restored
            .query_row("SELECT COUNT(*) FROM calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
        let ok: String = restored
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
        let starred: i64 = restored
            .query_row("SELECT COUNT(*) FROM calls WHERE starred = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(starred, 1);

        // Every member's recorded checksum matches what came out.
        let sums: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(into.join(CHECKSUMS)).unwrap()).unwrap();
        assert_eq!(sums.len(), 5); // db + one config + three recordings
        for s in sums {
            let p = s["path"].as_str().unwrap();
            let body = std::fs::read(into.join(p.replace('/', "_"))).unwrap();
            assert_eq!(
                crate::s3::sha256_hex(&body),
                s["sha256"].as_str().unwrap(),
                "{p} did not survive the round trip"
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_plain_archive_round_trips() {
        round_trip(None);
    }

    #[test]
    fn a_sealed_archive_round_trips() {
        round_trip(Some("correct-horse-battery-staple"));
    }

    #[test]
    fn the_wrong_passphrase_does_not_open_a_sealed_archive() {
        let d = tmp("wrongpass");
        let c = library(&d);
        let snap = d.join("snap.db");
        snapshot(&c, &snap).unwrap();
        let out = d.join("hoosier-20260912-000000-records.tar.gz.age");
        let plan = Plan {
            tier: "records".into(),
            calls: 3,
            db_bytes: std::fs::metadata(&snap).unwrap().len(),
            ..Default::default()
        };
        write_archive(&plan, &snap, &out, Some("the-real-one"), &mut |_, _, _| {}).unwrap();
        let e = read_manifest(&out, Some("not-the-real-one")).unwrap_err();
        assert!(e.contains("does not open"), "{e}");
        // And it is not readable as a plain archive either.
        assert!(read_manifest(&out, None).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_archives_are_listed_and_only_archives_can_be_removed() {
        let d = tmp("listing");
        std::fs::write(d.join("hoosier-20260912-010000-kept.tar.gz"), "x").unwrap();
        std::fs::write(d.join("hoosier-20260911-010000-kept.tar.gz.age"), "xx").unwrap();
        std::fs::write(d.join("notes.txt"), "no").unwrap();
        std::fs::write(d.join("hoosier-20260910-010000-kept.tar.gz.part"), "half").unwrap();
        let dest = Destination {
            id: "d".into(),
            kind: "folder".into(),
            path: d.to_string_lossy().to_string(),
            ..Default::default()
        };
        let have = list_at(&dest).unwrap();
        assert_eq!(have.len(), 2, "{have:?}");
        assert_eq!(have[0].name, "hoosier-20260912-010000-kept.tar.gz");
        assert!(have[1].encrypted);
        assert!(remove_at(&dest, "notes.txt").is_err());
        assert!(remove_at(&dest, "../../etc/hosts").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn keeping_three_removes_the_fourth() {
        let d = tmp("prune");
        for day in 9..=13 {
            std::fs::write(d.join(format!("hoosier-202609{day:02}-010000-kept.tar.gz")), "x")
                .unwrap();
        }
        let dest = Destination {
            id: "d".into(),
            kind: "folder".into(),
            path: d.to_string_lossy().to_string(),
            keep: 3,
            ..Default::default()
        };
        let dropped = prune(&dest);
        assert_eq!(dropped.len(), 2, "{dropped:?}");
        let left = list_at(&dest).unwrap();
        assert_eq!(left.len(), 3);
        assert_eq!(left[0].name, "hoosier-20260913-010000-kept.tar.gz");
        // keep = 0 means keep everything.
        let dest = Destination { keep: 0, ..dest };
        assert!(prune(&dest).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_bucket_destination_is_always_encrypted_whatever_the_page_says() {
        let d = clean_dest(Destination {
            id: "b".into(),
            kind: "s3".into(),
            encrypt: false,
            ..Default::default()
        });
        assert!(d.encrypt, "an S3 destination may not send in the clear");
        // A folder may choose.
        let f = clean_dest(Destination {
            id: "f".into(),
            kind: "folder".into(),
            encrypt: false,
            ..Default::default()
        });
        assert!(!f.encrypt);
        // An unknown kind becomes a folder rather than a broken destination.
        let x = clean_dest(Destination {
            id: "x".into(),
            kind: "ftp".into(),
            ..Default::default()
        });
        assert_eq!(x.kind, "folder");
    }

    #[test]
    fn a_generated_passphrase_is_long_and_unambiguous() {
        let a = new_passphrase();
        let b = new_passphrase();
        assert_ne!(a, b);
        assert_eq!(a.split('-').count(), 6);
        assert_eq!(a.chars().filter(|c| *c != '-').count(), 24);
        // The characters that can be misread off paper. Only the offending
        // ones are named if this fails — a passphrase, even a thrown-away
        // one from a test, does not belong in a log.
        let ambiguous: Vec<char> = a.chars().filter(|c| "01lo".contains(*c)).collect();
        assert!(
            ambiguous.is_empty(),
            "a generated passphrase used characters that can be misread: {ambiguous:?}"
        );
    }

    // secrets.json is left out; these files are wanted for everything else
    // they hold, so the credential inside them is blanked instead.
    #[test]
    fn credentials_in_other_settings_are_blanked_on_the_way_in() {
        let (body, blanked) = redact(
            "uploads.json",
            br#"{"rdio":{"enabled":true,"url":"http://nas:3000","key":"rdio-secret","system":7},
                 "openmhz":{"enabled":true,"short_name":"example","api_key":"om-secret"},
                 "broadcastify":{"enabled":false,"api_key":"bc-secret","system_id":42}}"#,
        );
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["rdio"]["key"], "");
        assert_eq!(v["openmhz"]["api_key"], "");
        assert_eq!(v["broadcastify"]["api_key"], "");
        // Everything that is not a credential survives, or a restore brings
        // back a broken configuration.
        assert_eq!(v["rdio"]["url"], "http://nas:3000");
        assert_eq!(v["rdio"]["system"], 7);
        assert_eq!(v["openmhz"]["short_name"], "example");
        assert_eq!(v["broadcastify"]["system_id"], 42);
        assert_eq!(blanked.len(), 3, "{blanked:?}");
        assert!(!String::from_utf8_lossy(&body).contains("secret"));

        let (body, blanked) = redact("stream.json", br#"{"user":"dj","password":"hunter2"}"#);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["password"], "");
        assert_eq!(v["user"], "dj");
        assert_eq!(blanked.len(), 1);
    }

    // The table above is a list someone has to remember to add to. The
    // sweep is the backstop: a credential field added to any settings file
    // later is blanked because of what it is called.
    #[test]
    fn a_credential_field_nobody_listed_is_still_blanked() {
        let (body, blanked) = redact(
            "someday.json",
            br#"{"nested":{"deeper":[{"token":"t-abc","name":"keep me"}]},"secret":"s"}"#,
        );
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["nested"]["deeper"][0]["token"], "");
        assert_eq!(v["nested"]["deeper"][0]["name"], "keep me");
        assert_eq!(v["secret"], "");
        assert_eq!(blanked.len(), 2, "{blanked:?}");
    }

    // An analyzer's extract field is also called `key`. Blanking those
    // would quietly break every rule that uses one.
    // Restoring settings on the machine they came from must not quietly
    // switch the uploads off: the blanked field is filled back in from the
    // live file, using the manifest's own list of what it blanked.
    #[test]
    fn a_restore_on_the_same_machine_keeps_the_credentials_it_blanked() {
        let live = br#"{"openmhz":{"short_name":"example","api_key":"live-key"},"min_secs":1}"#;
        let staged = br#"{"openmhz":{"short_name":"example","api_key":""},"min_secs":1}"#;
        let redacted = vec!["uploads.json: openmhz.api_key".to_string()];
        let out = carry_credentials(Some(live), staged, "uploads.json", &redacted);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["openmhz"]["api_key"], "live-key");
        assert_eq!(v["min_secs"], 1);

        // On a new machine there is no live file, so nothing is carried and
        // the field stays blank for the listener to fill in.
        let out = carry_credentials(None, staged, "uploads.json", &redacted);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["openmhz"]["api_key"], "");

        // Only what the manifest says was blanked is touched — a restore
        // does not go rummaging through the live file for anything else.
        let live = br#"{"openmhz":{"short_name":"CHANGED","api_key":"live-key"}}"#;
        let out = carry_credentials(Some(live), staged, "uploads.json", &redacted);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["openmhz"]["short_name"], "example");
        // And a file the manifest never mentions is passed through as-is.
        assert_eq!(
            carry_credentials(Some(live), staged, "stream.json", &redacted),
            staged.to_vec()
        );
    }

    #[test]
    fn a_rule_field_named_key_is_not_mistaken_for_a_credential() {
        let (body, blanked) = redact(
            "analyzers.json",
            br#"{"rules":[{"fields":[{"key":"candidate","desc":"yes|no"}],"keywords":["cpr"]}]}"#,
        );
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["rules"][0]["fields"][0]["key"], "candidate");
        assert_eq!(v["rules"][0]["keywords"][0], "cpr");
        assert!(blanked.is_empty(), "{blanked:?}");
    }

    #[test]
    fn the_archive_carries_no_credential_and_says_which_it_blanked() {
        let d = tmp("redacted-archive");
        let c = library(&d);
        std::fs::write(
            d.join("uploads.json"),
            r#"{"openmhz":{"api_key":"om-secret"}}"#,
        )
        .unwrap();
        let snap = d.join("snap.db");
        snapshot(&c, &snap).unwrap();
        let plan = Plan {
            tier: "records".into(),
            config: vec![("config/uploads.json".into(), d.join("uploads.json"))],
            calls: 3,
            db_bytes: std::fs::metadata(&snap).unwrap().len(),
            ..Default::default()
        };
        let out = d.join("hoosier-20260912-000000-records.tar.gz");
        let m = write_archive(&plan, &snap, &out, None, &mut |_, _, _| {}).unwrap();
        assert_eq!(m.redacted, vec!["uploads.json: openmhz.api_key"]);
        // And the key is nowhere in the bytes that were written.
        let mut a = read_archive(&out, None).unwrap();
        let mut found = String::new();
        for e in a.entries().unwrap() {
            let mut e = e.unwrap();
            if e.path().unwrap().to_string_lossy() == "config/uploads.json" {
                e.read_to_string(&mut found).unwrap();
            }
        }
        assert!(found.contains("api_key"), "{found}");
        assert!(!found.contains("om-secret"), "{found}");
        let _ = std::fs::remove_dir_all(&d);
    }

    // A restored database carries the paths of the machine it came from.
    #[test]
    fn a_restored_database_is_pointed_at_this_librarys_recordings() {
        let d = tmp("repoint");
        let c = library(&d);
        c.execute(
            "UPDATE calls SET audio = '/Volumes/other-machine/library/calls/' || id || '.wav'",
            [],
        )
        .unwrap();
        let here = d.join("calls");
        std::fs::create_dir_all(&here).unwrap();
        let moved = repoint_audio(&c, &here).unwrap();
        assert_eq!(moved, 3);
        let paths: Vec<String> = c
            .prepare("SELECT audio FROM calls ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        for p in &paths {
            assert!(
                p.starts_with(here.to_str().unwrap()),
                "{p} still points at the old machine"
            );
            assert!(p.ends_with(".wav"));
        }
        // Running it twice moves nothing: the rows already point here.
        assert_eq!(repoint_audio(&c, &here).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    // Every way out of a restore that does not write the marker has to
    // leave the library as it was. Getting this backwards once left
    // nineteen staged files behind after a refused restore.
    #[test]
    fn a_restore_that_does_not_finish_leaves_nothing_behind() {
        let d = tmp("guard");
        let a = d.join("alerts.json.restored");
        std::fs::write(&a, "staged").unwrap();
        {
            let _s = Staged(vec![a.to_string_lossy().to_string()]);
        }
        assert!(!a.exists(), "a staged file outlived a restore that failed");

        // A restore that does reach the end hands the list to the marker,
        // and then the files must stay.
        let b = d.join("places.json.restored");
        std::fs::write(&b, "staged").unwrap();
        {
            let mut s = Staged(vec![b.to_string_lossy().to_string()]);
            let handed = std::mem::take(&mut s.0);
            assert_eq!(handed.len(), 1);
        }
        assert!(b.exists(), "a committed restore lost its staged file");
        let _ = std::fs::remove_dir_all(&d);
    }

    // An archive is a file, and an unencrypted one on a shared drive is an
    // editable file. These are the members a restore will not unpack.
    #[test]
    fn an_edited_archive_cannot_plant_anything() {
        // The one that matters: secrets.json holds the web token, and a
        // planted one would hand over control of the app at next start.
        assert!(refused("config/secrets.json").is_some());
        // A member already named .restored would be swapped in unchecked.
        assert!(refused("config/alerts.json.restored").is_some());
        assert!(refused("calls.db.restored").is_some());
        assert!(refused("config/restore-pending.json").is_some());
        // And nothing may point outside where it is unpacked.
        assert!(refused("../../../etc/hosts").is_some());
        assert!(refused("audio/../../../../tmp/x").is_some());
        assert!(refused("/etc/hosts").is_some());
        // What a real archive holds is fine.
        for ok in [
            "hoosier-backup.json",
            "hoosier-checksums.json",
            "calls.db",
            "config/alerts.json",
            "config/catalogs/1234.csv",
            "audio/2026/09/11/a.m4a",
        ] {
            assert!(refused(ok).is_none(), "{ok} should be allowed");
        }
    }

    // The library files calls under dated directories. Flattening them to
    // bare file names put restored recordings in one heap and pointed the
    // rows at files that were not there — and orphaned the ones already on
    // disk, which is how this was found: on a real library, 579 recordings
    // lost their rows.
    #[test]
    fn a_recording_keeps_its_place_in_the_library() {
        let lib = Path::new("/Users/x/Library/App Support/com.hoosiersdr.app/library/calls");
        assert_eq!(
            relative_audio(
                "/Users/x/Library/App Support/com.hoosiersdr.app/library/calls/2026/09/11/a.m4a"
            ),
            PathBuf::from("2026/09/11/a.m4a")
        );
        // A row already pointing at this library is left alone.
        let same = lib.join("2026/09/11/a.m4a");
        assert_eq!(lib.join(relative_audio(same.to_str().unwrap())), same);
        // A path from another machine keeps the same shape under this one.
        assert_eq!(
            lib.join(relative_audio(
                "/Volumes/other/com.hoosiersdr.test/library/calls/2026/09/11/a.m4a"
            )),
            same
        );
        // Nothing clever for a bare name, and nothing that escapes.
        assert_eq!(relative_audio("/tmp/flat.wav"), PathBuf::from("flat.wav"));
        assert_eq!(
            relative_audio("/x/library/calls/../../etc/passwd"),
            PathBuf::from("passwd")
        );
    }

    #[test]
    fn dated_directories_survive_the_round_trip() {
        let d = tmp("dated");
        let c = library(&d);
        let lib = d.join("calls");
        std::fs::create_dir_all(lib.join("2026/09/11")).unwrap();
        for i in 1..=3 {
            std::fs::write(lib.join(format!("2026/09/11/c{i}.wav")), format!("audio {i}")).unwrap();
        }
        c.execute(
            "UPDATE calls SET audio = ?1 || '/2026/09/11/c' || id || '.wav'",
            rusqlite::params![lib.to_string_lossy()],
        )
        .unwrap();
        let snap = d.join("snap.db");
        snapshot(&c, &snap).unwrap();
        let plan = Plan {
            tier: "everything".into(),
            audio: (1..=3)
                .map(|i| {
                    (
                        format!("audio/2026/09/11/c{i}.wav"),
                        lib.join(format!("2026/09/11/c{i}.wav")),
                    )
                })
                .collect(),
            audio_bytes: 21,
            db_bytes: std::fs::metadata(&snap).unwrap().len(),
            calls: 3,
            ..Default::default()
        };
        let out = d.join("hoosier-20260912-000000-everything.tar.gz");
        write_archive(&plan, &snap, &out, None, &mut |_, _, _| {}).unwrap();

        // Unpack into a fresh library the way a restore does, and check
        // each row's path resolves to a file that is actually there.
        let into = d.join("fresh").join("calls");
        std::fs::create_dir_all(&into).unwrap();
        let mut a = read_archive(&out, None).unwrap();
        for e in a.entries().unwrap() {
            let mut e = e.unwrap();
            let name = e.path().unwrap().to_string_lossy().to_string();
            let to = if let Some(rest) = name.strip_prefix("audio/") {
                into.join(rest)
            } else {
                into.join(name.replace('/', "_"))
            };
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::io::copy(&mut e, &mut std::fs::File::create(&to).unwrap()).unwrap();
        }
        assert!(into.join("2026/09/11/c2.wav").exists(), "the dates were flattened");
        let restored = rusqlite::Connection::open(into.join("calls.db")).unwrap();
        assert_eq!(repoint_audio(&restored, &into).unwrap(), 3);
        let paths: Vec<String> = restored
            .prepare("SELECT audio FROM calls")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        for p in &paths {
            assert!(Path::new(p).exists(), "{p} does not exist after a restore");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    // The library runs in WAL mode. A stale -wal replayed onto a restored
    // database would write the previous database's frames into it.
    #[test]
    fn swapping_a_database_takes_its_wal_with_the_file_it_belongs_to() {
        let d = tmp("wal");
        let live = d.join("calls.db");
        std::fs::write(&live, "old database").unwrap();
        std::fs::write(d.join("calls.db-wal"), "old frames").unwrap();
        std::fs::write(d.join("calls.db-shm"), "old shm").unwrap();
        std::fs::write(d.join("calls.db.restored"), "new database").unwrap();
        let moved = swap_in(&[d.join("calls.db.restored").to_string_lossy().to_string()]);
        assert_eq!(moved.len(), 1);
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "new database");
        // No -wal or -shm is left beside the restored file.
        assert!(!d.join("calls.db-wal").exists(), "a stale WAL was left behind");
        assert!(!d.join("calls.db-shm").exists());
        // The old trio is still there, together, under one stamp.
        let kept: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.contains(".replaced-"))
            .collect();
        assert_eq!(kept.len(), 3, "{kept:?}");
        let stem = kept
            .iter()
            .find(|n| !n.ends_with("-wal") && !n.ends_with("-shm"))
            .expect("the replaced database itself")
            .clone();
        assert_eq!(
            std::fs::read_to_string(d.join(&stem)).unwrap(),
            "old database"
        );
        assert!(kept.contains(&format!("{stem}-wal")), "{kept:?}");
        assert!(kept.contains(&format!("{stem}-shm")), "{kept:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sizes_are_spoken_in_units_a_person_reads() {
        assert_eq!(human(900), "900 B");
        assert_eq!(human(2048), "2 KB");
        assert_eq!(human(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.00 GB");
    }
}
