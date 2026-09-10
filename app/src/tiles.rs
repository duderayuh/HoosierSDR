//! Map tiles for the Dispatch and Discovery maps, served to the webview over
//! the app's own `tiles://` scheme.
//!
//! OpenStreetMap's tile usage policy asks every client to identify itself
//! with a User-Agent (or Referer) naming the application; a page loaded from
//! `tauri://localhost` cannot promise either. So the webview asks for
//! `tiles://localhost/{z}/{x}/{y}.png`, the fetch happens here with the app's
//! User-Agent, and every tile is kept on disk in the app cache directory —
//! panning back is instant and yesterday's incidents still have a map when
//! the machine is offline.

use std::sync::{Condvar, Mutex, OnceLock};
use tauri::{AppHandle, Manager};

pub const USER_AGENT: &str = "HoosierSDR/0.1 (+https://github.com/duderayuh/HoosierSDR)";
const UPSTREAM: &str = "https://tile.openstreetmap.org";
/// At most this many tile fetches in flight at once (the policy frowns on
/// bulk pulls; a browser would use about this many connections anyway).
const PARALLEL: usize = 6;
const MAX_TILE_BYTES: u64 = 2 * 1024 * 1024;

/// `/z/x/y.png` → (z, x, y), refusing anything outside the tile grid.
pub fn parse_path(path: &str) -> Option<(u8, u32, u32)> {
    let p = path.trim_start_matches('/').strip_suffix(".png")?;
    let mut it = p.split('/');
    let z: u8 = it.next()?.parse().ok()?;
    let x: u32 = it.next()?.parse().ok()?;
    let y: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() || z > 19 {
        return None;
    }
    let n = 1u32 << z;
    (x < n && y < n).then_some((z, x, y))
}

fn gate() -> &'static (Mutex<usize>, Condvar) {
    static G: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
    G.get_or_init(|| (Mutex::new(0), Condvar::new()))
}

struct Slot;
impl Slot {
    fn acquire() -> Slot {
        let (m, cv) = gate();
        let mut n = m.lock().unwrap();
        while *n >= PARALLEL {
            n = cv.wait(n).unwrap();
        }
        *n += 1;
        Slot
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        let (m, cv) = gate();
        *m.lock().unwrap() -= 1;
        cv.notify_one();
    }
}

/// The tile's PNG bytes: from the disk cache, else fetched and cached.
pub fn fetch(app: &AppHandle, path: &str) -> Result<Vec<u8>, String> {
    let (z, x, y) = parse_path(path).ok_or("not a tile path")?;
    let file = app.path().app_cache_dir().ok().map(|d| {
        d.join("tiles")
            .join(z.to_string())
            .join(x.to_string())
            .join(format!("{y}.png"))
    });
    if let Some(f) = &file {
        if let Ok(b) = std::fs::read(f) {
            if !b.is_empty() {
                return Ok(b);
            }
        }
    }
    let _slot = Slot::acquire();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .get(&format!("{UPSTREAM}/{z}/{x}/{y}.png"))
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("tile: {e}"))?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(format!("tile HTTP {status}"));
    }
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(MAX_TILE_BYTES)
        .read_to_vec()
        .map_err(|e| format!("tile body: {e}"))?;
    if bytes.is_empty() {
        return Err("empty tile".into());
    }
    if let Some(f) = &file {
        if let Some(d) = f.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(f, &bytes);
    }
    Ok(bytes)
}

/// Register the `tiles` scheme on the Tauri builder.
pub fn register(b: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    b.register_asynchronous_uri_scheme_protocol("tiles", |ctx, request, responder| {
        let app = ctx.app_handle().clone();
        let path = request.uri().path().to_string();
        std::thread::spawn(move || {
            let resp = match fetch(&app, &path) {
                Ok(b) => tauri::http::Response::builder()
                    .status(200)
                    .header("Content-Type", "image/png")
                    .header("Cache-Control", "max-age=86400")
                    // The Discovery canvas draws tiles with crossOrigin set,
                    // and tiles://localhost is a different origin from the
                    // page; the tiles are public, so allow any page to read them.
                    .header("Access-Control-Allow-Origin", "*")
                    .body(b),
                Err(e) => tauri::http::Response::builder()
                    .status(404)
                    .header("Content-Type", "text/plain")
                    .body(e.into_bytes()),
            };
            if let Ok(r) = resp {
                responder.respond(r);
            }
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_paths() {
        assert_eq!(parse_path("/12/1043/1567.png"), Some((12, 1043, 1567)));
        assert_eq!(parse_path("/0/0/0.png"), Some((0, 0, 0)));
        assert!(parse_path("/20/1/1.png").is_none());
        assert!(parse_path("/3/8/1.png").is_none());
        assert!(parse_path("/3/1/1.jpg").is_none());
        assert!(parse_path("/../etc/passwd").is_none());
        assert!(parse_path("/3/1/1/2.png").is_none());
    }
}
