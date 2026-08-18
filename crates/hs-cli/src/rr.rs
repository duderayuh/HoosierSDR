//! `--rr-system`: pull a trunked system's sites, control channels and
//! talkgroups from the RadioReference database web service.
//!
//! This exists because the alternative — finding the control channel by
//! sweeping for the strongest signal — does not work. The first field capture
//! for this project was tuned that way and landed on a strong non-P25 signal
//! with the real carrier 50 kHz off. RadioReference already holds every site's
//! control and alternate channels, so the reliable move is to ask it and tune
//! what it says.
//!
//! Output is (a) a printed site/channel plan and (b) a talkgroup CSV written
//! to disk in the same format `--catalog` already reads, so the download path
//! and the manual-export path converge on one file format. That CSV is
//! RadioReference table data: it stays on the user's machine and is never
//! committed (the repo's `.gitignore` covers the default name).

use hs_core::catalog::radioreference::{Credentials, RrClient, RrError, RrSystem};

/// Run the download and report. Returns the exit code.
pub fn run(sys_id: u32, cache: Option<&str>, dump: Option<&str>) -> i32 {
    let creds = match Credentials::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };

    let mut client = RrClient::new(creds);
    if let Some(d) = dump {
        client = client.with_dump_dir(d);
        eprintln!("note: raw responses will be written to {d}/");
    }

    eprintln!("fetching system {sys_id} from RadioReference…");
    let sys = match client.system(sys_id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            if matches!(e, RrError::Empty(_)) && dump.is_none() {
                eprintln!(
                    "hint: re-run with --rr-dump <dir> to capture the raw XML, which \
                     shows which field names the service actually returned."
                );
            }
            return 1;
        }
    };

    report(&sys);

    let path = cache.unwrap_or("talkgroups.csv");
    match write_talkgroup_csv(&sys, path) {
        Ok(n) => println!("\nwrote {n} talkgroups to {path} (use it with --catalog {path})"),
        Err(e) => eprintln!("warning: could not write {path}: {e}"),
    }
    0
}

fn report(sys: &RrSystem) {
    println!("\n── {} ──", sys.name.as_deref().unwrap_or("system"));
    if let Some(id) = sys.sysid {
        print!("system id: 0x{id:03X}");
    }
    if let Some(w) = sys.wacn {
        print!("   wacn: 0x{w:05X}");
    }
    println!(
        "\nsites: {}   talkgroups: {}",
        sys.sites.len(),
        sys.talkgroups.len()
    );

    println!("\nCONTROL CHANNELS — tune these, primary first:");
    for site in &sys.sites {
        if site.control_channels_hz.is_empty() {
            continue;
        }
        let name = site
            .description
            .as_deref()
            .or(site.county.as_deref())
            .unwrap_or("site");
        let nac = match site.nac {
            Some(n) => format!("NAC 0x{n:03X}"),
            None => "NAC ?".to_string(),
        };
        let tdma = if site.tdma_control { "  [TDMA CC]" } else { "" };
        println!("  site {:>3}  {name}  ({nac}){tdma}", site.site_id);
        if let Some((lat, lon)) = site.position() {
            let range = match site.range_mi {
                Some(r) => format!("  ~{r:.0} mi"),
                None => String::new(),
            };
            println!("      {lat:.5}, {lon:.5}{range}    https://maps.google.com/?q={lat},{lon}");
        }
        for (i, hz) in site.control_channels_hz.iter().enumerate() {
            let kind = if i == 0 { "primary  " } else { "alternate" };
            println!("      {kind} {:.4} MHz    --freq {}", *hz as f64 / 1e6, hz);
        }
    }
}

/// Write talkgroups in the RadioReference CSV export format, which
/// `hs_catalog::CsvCatalog` already parses.
fn write_talkgroup_csv(sys: &RrSystem, path: &str) -> std::io::Result<usize> {
    use std::io::Write;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(
        f,
        "Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category,Priority"
    )?;
    for tg in &sys.talkgroups {
        writeln!(
            f,
            "{},{:X},{},{},{},{},{},",
            tg.id,
            tg.id,
            csv_field(tg.alias.as_deref()),
            if tg.encrypted { "DE" } else { "D" },
            csv_field(tg.description.as_deref()),
            csv_field(tg.category.as_deref()),
            csv_field(tg.category.as_deref()),
        )?;
    }
    f.flush()?;
    Ok(sys.talkgroups.len())
}

/// Quote a field that contains a comma or quote, per the CSV the parser reads.
fn csv_field(v: Option<&str>) -> String {
    let v = v.unwrap_or("");
    if v.contains(',') || v.contains('"') {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}
