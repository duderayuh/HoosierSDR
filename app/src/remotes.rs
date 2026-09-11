//! Other HoosierSDR instances on the user's tailnet.
//!
//! "Linking" the app to a Tailscale account is the Tailscale login already on
//! each machine: `tailscale status --json` lists every device the account can
//! see, and each device's `UserID`. This module asks the CLI for that list,
//! probes each online device for the app's web server (`/api/health` on the
//! same port this instance listens on), and lets the desktop open a live
//! window onto any instance it finds. The remote view is the phone page the
//! instance already serves: its feed, calls, and audio over SSE.
//!
//! Authentication between two instances of one account can be automatic:
//! with "trust my tailnet" on, a request from a Tailscale address is accepted
//! when `tailscale whois` says the peer belongs to the same Tailscale user as
//! this machine. Loopback and LAN addresses never get that shortcut, so
//! `tailscale serve`/Funnel traffic (which arrives from 127.0.0.1) still
//! needs the token.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::AppState;

/// Persisted in `remotes.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Accept requests from devices on this Tailscale account without a token.
    #[serde(default)]
    pub trust_tailnet: bool,
}

pub type Shared = Mutex<Settings>;

fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let d = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir: {e}"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d.join("remotes.json"))
}

pub fn load(app: &AppHandle) -> Settings {
    path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let p = path(app)?;
    let t = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(&p, t).map_err(|e| format!("{}: {e}", p.display()))
}

/// Whether this instance trusts same-account tailnet peers. Read per request
/// by the web server's auth extractor.
pub fn trust_enabled(app: &AppHandle) -> bool {
    app.state::<AppState>().remotes.lock().unwrap().trust_tailnet
}

// ---------------------------------------------------------------------------
// Tailscale CLI

/// Candidate CLI locations: PATH, the Mac App Store bundle, and Homebrew on
/// Intel Macs (Apple Silicon Homebrew puts `tailscale` in PATH).
const TAILSCALE_BINS: [&str; 4] = [
    "tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/usr/local/bin/tailscale",
    "/opt/homebrew/bin/tailscale",
];

fn tailscale(args: &[&str]) -> Result<String, String> {
    let mut last = String::from("tailscale CLI not found");
    for bin in TAILSCALE_BINS {
        match std::process::Command::new(bin).args(args).output() {
            Ok(out) if out.status.success() => {
                return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                last = if err.is_empty() {
                    format!("tailscale {} exited {}", args.join(" "), out.status)
                } else {
                    err
                };
            }
            Err(_) => continue,
        }
    }
    Err(last)
}

/// One device from `tailscale status --json` (Self or a peer).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Node {
    pub host: String,
    /// MagicDNS name without the trailing dot, e.g. `radionova.tail1234.ts.net`.
    pub dns: String,
    /// First Tailscale IPv4, if the device has one.
    pub ip: Option<String>,
    pub os: String,
    pub online: bool,
    pub user_id: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Tailnet {
    pub me: Node,
    pub peers: Vec<Node>,
    /// Login name of this machine's Tailscale user, when the status lists it.
    pub login: String,
}

fn node(v: &serde_json::Value) -> Option<Node> {
    let ip = v
        .get("TailscaleIPs")
        .and_then(|a| a.as_array())
        .and_then(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .find(|s| s.parse::<std::net::Ipv4Addr>().is_ok())
        })
        .map(|s| s.to_string());
    let dns = v
        .get("DNSName")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim_end_matches('.')
        .to_string();
    let host = v
        .get("HostName")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    if host.is_empty() && dns.is_empty() {
        return None;
    }
    Some(Node {
        host,
        dns,
        ip,
        os: v
            .get("OS")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        online: v.get("Online").and_then(|b| b.as_bool()).unwrap_or(false),
        user_id: v.get("UserID").and_then(|n| n.as_u64()).unwrap_or(0),
    })
}

pub fn parse_status(json: &str) -> Result<Tailnet, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("status: {e}"))?;
    let me = v
        .get("Self")
        .and_then(node)
        .ok_or("tailscale status has no Self (not logged in?)")?;
    let mut peers: Vec<Node> = v
        .get("Peer")
        .and_then(|p| p.as_object())
        .map(|m| m.values().filter_map(node).collect())
        .unwrap_or_default();
    peers.sort_by(|a, b| {
        b.online
            .cmp(&a.online)
            .then_with(|| a.dns.to_lowercase().cmp(&b.dns.to_lowercase()))
    });
    let login = v
        .get("User")
        .and_then(|u| u.get(me.user_id.to_string()))
        .and_then(|u| u.get("LoginName"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    Ok(Tailnet { me, peers, login })
}

pub fn tailnet() -> Result<Tailnet, String> {
    parse_status(&tailscale(&["status", "--json"])?)
}

/// The Tailscale user id owning `ip`, per `tailscale whois`.
pub fn parse_whois_user(json: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("UserProfile")?.get("ID")?.as_u64()
}

/// Tailscale's address ranges: CGNAT `100.64.0.0/10` and `fd7a:115c:a1e0::/48`.
pub fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (64..=127).contains(&o[1])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

// ---------------------------------------------------------------------------
// Same-account trust for the web server

const TRUST_TTL: Duration = Duration::from_secs(600);

struct TrustCache {
    my_user: Option<(u64, Instant)>,
    peers: HashMap<IpAddr, (bool, Instant)>,
}

static TRUST: Mutex<Option<TrustCache>> = Mutex::new(None);

fn my_user_id() -> Option<u64> {
    {
        let g = TRUST.lock().unwrap();
        if let Some((id, at)) = g.as_ref().and_then(|c| c.my_user) {
            if at.elapsed() < TRUST_TTL {
                return Some(id);
            }
        }
    }
    let t = tailnet().ok()?;
    let mut g = TRUST.lock().unwrap();
    let c = g.get_or_insert_with(|| TrustCache {
        my_user: None,
        peers: HashMap::new(),
    });
    c.my_user = Some((t.me.user_id, Instant::now()));
    Some(t.me.user_id)
}

/// Whether a request from `ip` may skip the token: trust is on, the address
/// is a Tailscale address, and `tailscale whois` puts it on this machine's
/// own account. Blocking (runs the CLI); answers are cached for ten minutes.
pub fn peer_trusted(app: &AppHandle, ip: IpAddr) -> bool {
    if !trust_enabled(app) || !is_tailnet_ip(ip) {
        return false;
    }
    {
        let g = TRUST.lock().unwrap();
        if let Some((ok, at)) = g.as_ref().and_then(|c| c.peers.get(&ip)) {
            if at.elapsed() < TRUST_TTL {
                return *ok;
            }
        }
    }
    let ok = match my_user_id() {
        Some(me) => tailscale(&["whois", "--json", &ip.to_string()])
            .ok()
            .and_then(|j| parse_whois_user(&j))
            .map_or(false, |u| u == me),
        None => false,
    };
    let mut g = TRUST.lock().unwrap();
    let c = g.get_or_insert_with(|| TrustCache {
        my_user: None,
        peers: HashMap::new(),
    });
    c.peers.insert(ip, (ok, Instant::now()));
    ok
}

/// Forget cached whois answers (after the trust switch changes).
fn reset_trust_cache() {
    if let Some(c) = TRUST.lock().unwrap().as_mut() {
        c.peers.clear();
    }
}

// ---------------------------------------------------------------------------
// Probing peers for the app

/// What `/api/health` says about an instance.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Health {
    pub version: String,
    pub host: String,
    pub trust_tailnet: bool,
}

/// Recognise a HoosierSDR health reply. Older builds answer the bare text
/// `ok`; current builds answer JSON with `app: "HoosierSDR"`. Anything else
/// on the port is some other program.
pub fn detect_instance(status: u16, body: &str) -> Option<Health> {
    if status != 200 {
        return None;
    }
    let body = body.trim();
    if body == "ok" {
        return Some(Health::default());
    }
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("app").and_then(|s| s.as_str()) != Some("HoosierSDR") {
        return None;
    }
    Some(Health {
        version: v
            .get("version")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        host: v
            .get("host")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        trust_tailnet: v
            .get("trust_tailnet")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
    })
}

/// One row of the "Instances on your tailnet" list.
#[derive(Clone, Debug, Serialize)]
pub struct Instance {
    pub host: String,
    pub dns: String,
    pub ip: String,
    pub os: String,
    pub online: bool,
    pub is_self: bool,
    pub same_account: bool,
    /// The app's web server answered on the port.
    pub found: bool,
    pub version: String,
    /// The instance will accept us without a token (it trusts its tailnet).
    pub trusts_tailnet: bool,
    /// A token for this host is saved locally.
    pub has_token: bool,
    /// `/api/status` accepted us (saved token, or the instance trusts us).
    pub authorized: bool,
    pub running: Option<bool>,
    pub catalog_len: Option<usize>,
    pub url: String,
    pub error: String,
}

fn token_key(dns: &str) -> String {
    format!("remote_token:{dns}")
}

fn probe(n: &Node, is_self: bool, my_user: u64, port: u16) -> Instance {
    let ip = n.ip.clone().unwrap_or_default();
    let url = format!("http://{ip}:{port}");
    let token = crate::secrets::get(&token_key(&n.dns));
    let mut inst = Instance {
        host: n.host.clone(),
        dns: n.dns.clone(),
        ip: ip.clone(),
        os: n.os.clone(),
        online: n.online,
        is_self,
        same_account: n.user_id == my_user,
        found: false,
        version: String::new(),
        trusts_tailnet: false,
        has_token: token.is_some(),
        authorized: false,
        running: None,
        catalog_len: None,
        url: url.clone(),
        error: String::new(),
    };
    if ip.is_empty() {
        inst.error = "no Tailscale IPv4".into();
        return inst;
    }
    if !n.online {
        return inst;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(1500)))
        .http_status_as_error(false)
        .build()
        .into();
    let health = match agent.get(format!("{url}/api/health")).call() {
        Ok(mut r) => {
            let status = r.status().as_u16();
            let text = r.body_mut().read_to_string().unwrap_or_default();
            detect_instance(status, &text)
        }
        Err(e) => {
            // Connection refused / timeout: no server there. That is the
            // normal answer for most devices, not an error worth showing.
            let msg = e.to_string();
            if !(msg.contains("refused") || msg.contains("timed out") || msg.contains("timeout")) {
                inst.error = msg;
            }
            None
        }
    };
    let Some(h) = health else {
        return inst;
    };
    inst.found = true;
    inst.version = h.version;
    inst.trusts_tailnet = h.trust_tailnet;
    let mut req = agent.get(format!("{url}/api/status"));
    if let Some(t) = &token {
        req = req.header("x-token", t);
    }
    if let Ok(mut r) = req.call() {
        if r.status().as_u16() == 200 {
            let text = r.body_mut().read_to_string().unwrap_or_default();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                inst.authorized = true;
                inst.running = v.get("running").and_then(|b| b.as_bool());
                inst.catalog_len = v
                    .get("catalog_len")
                    .and_then(|n| n.as_u64())
                    .map(|n| n as usize);
            }
        }
    }
    inst
}

// ---------------------------------------------------------------------------
// Commands

#[derive(Serialize)]
pub struct RemotesInfo {
    pub settings: Settings,
    /// `None` when the Tailscale CLI is missing or not logged in (with why).
    pub tailnet: Option<TailnetInfo>,
    pub error: String,
    pub port: u16,
}

#[derive(Serialize)]
pub struct TailnetInfo {
    pub login: String,
    pub host: String,
    pub dns: String,
    pub ip: String,
    pub devices: usize,
    pub online: usize,
}

#[tauri::command]
pub async fn remotes_get(app: AppHandle) -> RemotesInfo {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = app.state::<AppState>().remotes.lock().unwrap().clone();
        let (tailnet, error) = match tailnet() {
            Ok(t) => (
                Some(TailnetInfo {
                    login: t.login,
                    host: t.me.host,
                    dns: t.me.dns,
                    ip: t.me.ip.unwrap_or_default(),
                    devices: t.peers.len(),
                    online: t.peers.iter().filter(|p| p.online).count(),
                }),
                String::new(),
            ),
            Err(e) => (None, e),
        };
        RemotesInfo {
            settings,
            tailnet,
            error,
            port: crate::web::port(),
        }
    })
    .await
    .unwrap_or_else(|e| RemotesInfo {
        settings: Settings::default(),
        tailnet: None,
        error: e.to_string(),
        port: crate::web::port(),
    })
}

#[tauri::command]
pub fn remotes_set(app: AppHandle, settings: Settings) -> Result<(), String> {
    *app.state::<AppState>().remotes.lock().unwrap() = settings.clone();
    reset_trust_cache();
    save(&app, &settings)
}

/// Probe every online device on the account for a running instance. This
/// machine is included as "this Mac" so the list shows the local server too.
#[tauri::command]
pub async fn remotes_scan(_app: AppHandle) -> Result<Vec<Instance>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let t = tailnet()?;
        let port = crate::web::port();
        let me = t.me.user_id;
        let mut handles = Vec::new();
        let mine = t.me.clone();
        handles.push(std::thread::spawn(move || probe(&mine, true, me, port)));
        for p in t.peers.into_iter().filter(|p| p.online && p.ip.is_some()) {
            handles.push(std::thread::spawn(move || probe(&p, false, me, port)));
        }
        let mut out: Vec<Instance> = handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .collect();
        // Found instances first, then this Mac, then the rest by name.
        out.sort_by(|a, b| {
            b.found
                .cmp(&a.found)
                .then_with(|| b.is_self.cmp(&a.is_self))
                .then_with(|| a.dns.to_lowercase().cmp(&b.dns.to_lowercase()))
        });
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Save (or clear, with an empty value) the token used for one remote host.
#[tauri::command]
pub fn remote_token_set(dns: String, token: String) -> Result<(), String> {
    let key = token_key(&dns);
    if token.trim().is_empty() {
        crate::secrets::remove(&key)
    } else {
        crate::secrets::set(&key, token.trim())
    }
}

fn window_label(dns: &str) -> String {
    let s: String = dns
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    format!("remote-{s}")
}

/// Open (or focus) a window onto a remote instance's live view. The saved
/// token, if any, rides along in the URL once; the page stores it and strips
/// it from the address bar.
#[tauri::command]
pub fn remote_open(app: AppHandle, dns: String, url: String, host: String) -> Result<(), String> {
    let label = window_label(&dns);
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.set_focus();
        return Ok(());
    }
    let mut target = url.trim_end_matches('/').to_string();
    target.push('/');
    if let Some(t) = crate::secrets::get(&token_key(&dns)) {
        target.push_str("?token=");
        target.push_str(&t);
    }
    let parsed: tauri::Url = target.parse().map_err(|e| format!("{url}: {e}"))?;
    let title = if host.is_empty() {
        format!("Remote · {dns}")
    } else {
        format!("Remote · {host}")
    };
    tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::External(parsed))
        .title(title)
        .inner_size(1100.0, 820.0)
        .min_inner_size(520.0, 480.0)
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
      "Self": {"HostName": "meganova’s Mac Studio", "DNSName": "meganova.tail1234.ts.net.",
               "TailscaleIPs": ["100.118.155.34", "fd7a:115c:a1e0::3201:9b22"], "UserID": 1560297009701221,
               "OS": "macOS", "Online": true},
      "Peer": {
        "nodekey:a": {"HostName": "radionova", "DNSName": "radionova.tail1234.ts.net.",
                      "TailscaleIPs": ["100.88.221.75"], "UserID": 1560297009701221, "OS": "macOS", "Online": true},
        "nodekey:b": {"HostName": "bjorn", "DNSName": "bjorn.tail1234.ts.net.",
                      "TailscaleIPs": ["fd7a:115c:a1e0::1", "100.123.173.33"], "UserID": 1560297009701221, "OS": "linux", "Online": false},
        "nodekey:c": {"HostName": "guest", "DNSName": "guest.tail1234.ts.net.",
                      "TailscaleIPs": ["100.64.0.9"], "UserID": 42, "OS": "windows", "Online": true}
      },
      "User": {"1560297009701221": {"ID": 1560297009701221, "LoginName": "someone@example.com"}}
    }"#;

    #[test]
    fn status_parses_self_peers_and_login() {
        let t = parse_status(STATUS).unwrap();
        assert_eq!(t.me.dns, "meganova.tail1234.ts.net");
        assert_eq!(t.me.ip.as_deref(), Some("100.118.155.34"));
        assert_eq!(t.login, "someone@example.com");
        // online first, then by name; the IPv4 is picked even when listed second
        let names: Vec<&str> = t.peers.iter().map(|p| p.host.as_str()).collect();
        assert_eq!(names, ["guest", "radionova", "bjorn"]);
        assert_eq!(t.peers[2].ip.as_deref(), Some("100.123.173.33"));
        assert!(!t.peers[2].online);
        assert_eq!(t.peers[0].user_id, 42);
    }

    #[test]
    fn status_without_self_is_an_error() {
        assert!(parse_status(r#"{"Peer": {}}"#).is_err());
        assert!(parse_status("not json").is_err());
    }

    #[test]
    fn whois_yields_the_user_id() {
        let j = r#"{"Node": {"ID": 1}, "UserProfile": {"ID": 1560297009701221, "LoginName": "x@y"}}"#;
        assert_eq!(parse_whois_user(j), Some(1560297009701221));
        assert_eq!(parse_whois_user(r#"{"Node": {}}"#), None);
        assert_eq!(parse_whois_user("peer not found"), None);
    }

    #[test]
    fn tailnet_ranges() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(is_tailnet_ip(ip("100.64.0.1")));
        assert!(is_tailnet_ip(ip("100.118.155.34")));
        assert!(is_tailnet_ip(ip("100.127.255.255")));
        assert!(!is_tailnet_ip(ip("100.128.0.1")));
        assert!(!is_tailnet_ip(ip("100.63.255.255")));
        assert!(!is_tailnet_ip(ip("127.0.0.1")));
        assert!(!is_tailnet_ip(ip("192.168.1.10")));
        assert!(is_tailnet_ip(ip("fd7a:115c:a1e0::3201:9b22")));
        assert!(!is_tailnet_ip(ip("::1")));
        assert!(!is_tailnet_ip(ip("fd7a:115c:a1e1::1")));
    }

    #[test]
    fn health_detection_is_strict() {
        assert_eq!(detect_instance(200, "ok\n"), Some(Health::default()));
        let h = detect_instance(
            200,
            r#"{"app":"HoosierSDR","version":"0.1.0","host":"radionova","trust_tailnet":true}"#,
        )
        .unwrap();
        assert_eq!(h.version, "0.1.0");
        assert_eq!(h.host, "radionova");
        assert!(h.trust_tailnet);
        assert_eq!(detect_instance(200, r#"{"app":"Other"}"#), None);
        assert_eq!(detect_instance(200, "<html>hi</html>"), None);
        assert_eq!(detect_instance(404, "ok"), None);
    }

    #[test]
    fn window_labels_are_plain() {
        assert_eq!(window_label("radionova.tail1234.ts.net"), "remote-radionova-tail1234-ts-net");
    }

    /// Runs the real CLI; ignored because it needs Tailscale installed.
    #[test]
    #[ignore]
    fn live_status() {
        let t = tailnet().unwrap();
        eprintln!("{} peers, me = {:?}", t.peers.len(), t.me);
    }
}
