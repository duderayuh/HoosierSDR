//! Filename formatter: how a stored call is named, from a template the
//! listener writes. `{date}-{time}_tg{tg}_{freqk}` (the default) gives the
//! `20260821-143012_tg20308_8518125` names earlier versions wrote, so an
//! existing library stays consistent unless the template is changed.
//!
//! The template yields a *stem*: the audio file, its JSON sidecar and any
//! transcode all derive from it. A `/` in the template makes sub-folders
//! (inside the library's `YYYY/MM/DD/`); every segment is sanitised and `..`
//! is refused, so a template cannot write outside the library.

use serde::{Deserialize, Serialize};

pub const DEFAULT_TEMPLATE: &str = "{date}-{time}_tg{tg}_{freqk}";

/// What the tokens can see.
#[derive(Debug, Clone, Default)]
pub struct NameContext<'a> {
    /// `YYYYMMDD-HHMMSS`, UTC.
    pub stamp: &'a str,
    pub tg: u16,
    pub tg_name: &'a str,
    pub unit: u32,
    pub unit_name: &'a str,
    pub freq_hz: u64,
    pub system: &'a str,
    pub site: &'a str,
    pub modulation: &'a str,
    pub secs: f64,
    pub emergency: bool,
}

/// The tokens, for the settings page.
pub const TOKENS: &[(&str, &str)] = &[
    ("{date}", "YYYYMMDD (UTC)"),
    ("{time}", "HHMMSS (UTC)"),
    ("{epoch}", "seconds since 1970"),
    ("{tg}", "talkgroup number"),
    ("{tgname}", "talkgroup alias"),
    ("{unit}", "radio ID"),
    ("{unitname}", "radio alias (or ID)"),
    ("{freq}", "frequency in MHz, 851.8125"),
    ("{freqk}", "frequency ×10000, 8518125"),
    ("{freqhz}", "frequency in Hz"),
    ("{system}", "system name"),
    ("{site}", "site name"),
    ("{mod}", "C4FM / CQPSK"),
    ("{secs}", "length in seconds"),
    ("{emg}", "EMERGENCY or empty"),
];

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    pub template: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            template: DEFAULT_TEMPLATE.into(),
        }
    }
}

/// Render the template to a relative path stem (no extension). The
/// template is split into path segments first, so a `/` inside a token's
/// value (a talkgroup named "Patrol/North") cannot create a folder; unknown
/// tokens lose their braces in sanitisation; an empty result falls back to
/// the default template.
pub fn render(template: &str, c: &NameContext<'_>) -> String {
    let tpl = if template.trim().is_empty() {
        DEFAULT_TEMPLATE
    } else {
        template
    };
    let segs: Vec<String> = tpl
        .split(['/', '\\'])
        .map(|seg| sanitize_segment(&expand(seg, c)))
        .filter(|s| !s.is_empty())
        .collect();
    let joined = segs.join("/");
    if joined.is_empty() {
        render(DEFAULT_TEMPLATE, c)
    } else {
        joined
    }
}

fn expand(seg: &str, c: &NameContext<'_>) -> String {
    let (date, time) = c.stamp.split_once('-').unwrap_or((c.stamp, ""));
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let unit_name = if c.unit_name.is_empty() {
        c.unit.to_string()
    } else {
        c.unit_name.to_string()
    };
    let tg_name = if c.tg_name.is_empty() {
        format!("TG {}", c.tg)
    } else {
        c.tg_name.to_string()
    };
    seg.replace("{date}", date)
        .replace("{time}", time)
        .replace("{epoch}", &epoch.to_string())
        .replace("{tg}", &c.tg.to_string())
        .replace("{tgname}", &tg_name)
        .replace("{unit}", &c.unit.to_string())
        .replace("{unitname}", &unit_name)
        .replace("{freq}", &format!("{:.4}", c.freq_hz as f64 / 1e6))
        .replace(
            "{freqk}",
            &((c.freq_hz as f64 / 1e6 * 10_000.0).round() as u64).to_string(),
        )
        .replace("{freqhz}", &c.freq_hz.to_string())
        .replace("{system}", c.system)
        .replace("{site}", c.site)
        .replace("{mod}", c.modulation)
        .replace("{secs}", &format!("{:.0}", c.secs))
        .replace("{emg}", if c.emergency { "EMERGENCY" } else { "" })
}

/// One path segment: printable, no separators or control characters, no
/// leading dots (so `..` and hidden files cannot be produced), bounded.
fn sanitize_segment(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '{' | '}' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.').trim();
    trimmed.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> NameContext<'static> {
        NameContext {
            stamp: "20260821-143012",
            tg: 20308,
            tg_name: "Sheriff: Patrol/North",
            unit: 790065,
            unit_name: "",
            freq_hz: 851_812_500,
            system: "SAFE-T",
            site: "Marion",
            modulation: "CQPSK",
            secs: 4.6,
            emergency: true,
        }
    }

    #[test]
    fn default_template_matches_the_legacy_names() {
        assert_eq!(
            render(DEFAULT_TEMPLATE, &ctx()),
            "20260821-143012_tg20308_8518125"
        );
        assert_eq!(render("", &ctx()), "20260821-143012_tg20308_8518125");
    }

    #[test]
    fn tokens_expand_and_segments_are_sanitised() {
        let s = render(
            "{system}/{tgname}/{date}_{time}_{unitname}_{freq}_{mod}_{secs}s_{emg}",
            &ctx(),
        );
        assert_eq!(
            s,
            "SAFE-T/Sheriff_ Patrol_North/20260821_143012_790065_851.8125_CQPSK_5s_EMERGENCY"
        );
    }

    #[test]
    fn cannot_escape_the_library() {
        let s = render("../../{tg}/../x", &ctx());
        assert!(!s.contains(".."), "{s}");
        assert_eq!(s, "20308/x");
        assert_eq!(render("///", &ctx()), "20260821-143012_tg20308_8518125");
    }
}
