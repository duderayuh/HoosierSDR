//! Decode diagnostics — the data you export from a real-signal test so it can
//! be replayed and the DSP refined offline.
//!
//! The accumulator rides along inside `ChannelDecoder` and records the health
//! signals that matter when a real capture decodes poorly: how strong each
//! frame-sync correlation was, how many BCH errors the NID needed, the
//! distribution of sliced symbol levels (a proxy for timing/gain/equalizer
//! health), and every grant / encryption event. Serialize it with
//! [`Diagnostics::to_json`] and share the file.

/// One frame-sync detection and how clean it was.
#[derive(Debug, Clone, Copy)]
pub struct SyncStat {
    /// Symbol index (count of symbols processed) at detection.
    pub at_symbol: u64,
    /// Bit errors in the 48-bit sync correlation (0 = perfect).
    pub bit_errors: u32,
}

/// One NID decode.
#[derive(Debug, Clone, Copy)]
pub struct LcStat {
    pub talkgroup: u16,
    pub source_unit: u32,
    pub emergency: bool,
}

#[derive(Debug, Clone)]
pub struct LocationStat {
    /// Logical Link ID of the reporting radio.
    pub llid: u32,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone)]
pub struct NidStat {
    pub nac: u16,
    pub duid: u8,
    pub bch_errors: u32,
}

/// A logged grant (clear or encrypted).
#[derive(Debug, Clone, Copy)]
pub struct GrantStat {
    pub talkgroup: u16,
    pub source_unit: u32,
    pub freq_hz: u64,
    pub encrypted: bool,
}

/// Running histogram of sliced symbol levels and soft-symbol amplitude, the
/// cheapest window into demod health on a real signal.
#[derive(Debug, Clone, Default)]
pub struct SymbolHealth {
    /// Count of each sliced dibit: [+3(01), +1(00), -1(10), -3(11)].
    pub level_counts: [u64; 4],
    /// Sum and sum-of-squares of the post-equalizer soft symbol, for mean and
    /// variance without storing every sample.
    pub soft_sum: f64,
    pub soft_sq_sum: f64,
    pub soft_n: u64,
    /// Mean absolute deviation of |soft| from the nearest nominal level —
    /// small = open eye, large = closed eye.
    pub eye_err_sum: f64,
    /// A bounded sample of raw soft-symbol values (post-equalizer), for the
    /// eye/level plot in visual tools. Capped so long captures stay small.
    pub soft_samples: Vec<f32>,
}

/// Max soft-symbol values retained for the eye plot.
pub const SOFT_SAMPLE_CAP: usize = 4000;

impl SymbolHealth {
    pub fn observe(&mut self, soft: f32, dibit: u8) {
        self.level_counts[(dibit & 3) as usize] += 1;
        self.soft_sum += soft as f64;
        self.soft_sq_sum += (soft as f64) * (soft as f64);
        self.soft_n += 1;
        let nominal = match dibit & 3 {
            0b01 => 3.0,
            0b00 => 1.0,
            0b10 => -1.0,
            _ => -3.0,
        };
        self.eye_err_sum += (soft as f64 - nominal).abs();
        if self.soft_samples.len() < SOFT_SAMPLE_CAP {
            self.soft_samples.push(soft);
        }
    }

    pub fn soft_mean(&self) -> f64 {
        if self.soft_n == 0 {
            0.0
        } else {
            self.soft_sum / self.soft_n as f64
        }
    }

    pub fn eye_error(&self) -> f64 {
        if self.soft_n == 0 {
            0.0
        } else {
            self.eye_err_sum / self.soft_n as f64
        }
    }
}

/// Full diagnostic record for a decode session.
#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    pub sample_rate: f64,
    pub modulation: crate::decoder::Modulation,
    pub equalizer: bool,
    pub symbols_processed: u64,
    pub syncs: Vec<SyncStat>,
    pub nids: Vec<NidStat>,
    /// Packet data units reassembled.
    pub packets: u64,
    /// Manufacturer-specific TSBKs seen, counted by (MFID, opcode). Which
    /// vendor messages a system emits says a lot about what features it runs.
    pub vendor_tsbks: Vec<(u8, u8, u32)>,
    /// Channel plans announced by IDEN_UP: (iden, base Hz, spacing Hz).
    pub idens: Vec<(u8, u64, u64)>,
    /// Talkgroup patches: supergroup and its member talkgroups.
    pub patches: Vec<(u16, Vec<u16>)>,
    /// A sample of raw argument words from vendor TSBKs. Manufacturer-specific
    /// opcodes are not decoded, but their arguments are the evidence needed to
    /// work out what they mean from a shared log — which is how the Motorola
    /// Group Regroup blocks on the Marion County control channel were
    /// identified as patch messages rather than corrupt grants.
    pub vendor_samples: Vec<(u8, u8, u64)>,
    /// Radio position reports decoded from packet data.
    pub locations: Vec<LocationStat>,
    /// Link Control words naming calls on a traffic channel.
    pub link_control: Vec<LcStat>,
    /// Vendor-defined Link Control opcodes, counted by (MFID, LCO).
    pub vendor_lc: Vec<(u8, u8, u32)>,
    /// Raw arguments from vendor Link Control words, for offline analysis.
    pub vendor_lc_samples: Vec<(u8, u8, [u8; 7])>,
    /// Raw 240-bit Link Control slot payloads, one per LDU1, packed MSB-first.
    /// Kept so the codes protecting them can be studied against real traffic.
    pub lc_raw: Vec<[u8; 30]>,
    pub grants: Vec<GrantStat>,
    pub encrypted_skips: Vec<u16>,
    /// NIDs that decoded with zero BCH errors. Decode correctness measured
    /// directly, which is what separates the right modulation from a
    /// cross-decode — kept as a counter so the comparison is O(1) however
    /// long the channel has run.
    pub clean_nids: u64,
    pub voice_frames: u64,
    pub pcm_samples: u64,
    pub health: SymbolHealth,
}

impl Diagnostics {
    pub fn new(sample_rate: f64, equalizer: bool) -> Self {
        Self {
            sample_rate,
            equalizer,
            ..Default::default()
        }
    }

    /// Mean bit-error of all frame-sync detections (lower is better).
    pub fn mean_sync_errors(&self) -> f64 {
        if self.syncs.is_empty() {
            return 0.0;
        }
        self.syncs.iter().map(|s| s.bit_errors as f64).sum::<f64>() / self.syncs.len() as f64
    }

    /// Serialize to a JSON string. Hand-rolled to keep hs-core dependency-free;
    /// the schema is stable and documented in docs/DIAGNOSTICS.md.
    pub fn to_json(&self) -> String {
        let mut s = String::with_capacity(2048);
        s.push_str("{\n");
        s.push_str("  \"schema\": \"hoosier-sdr/diagnostics/1\",\n");
        let modn = match self.modulation {
            crate::decoder::Modulation::C4fm => "C4FM",
            crate::decoder::Modulation::Cqpsk => "CQPSK",
        };
        s.push_str(&format!("  \"modulation\": \"{modn}\",\n"));
        s.push_str(&format!("  \"sample_rate\": {},\n", self.sample_rate));
        s.push_str(&format!("  \"equalizer\": {},\n", self.equalizer));
        s.push_str(&format!(
            "  \"symbols_processed\": {},\n",
            self.symbols_processed
        ));
        s.push_str(&format!("  \"voice_frames\": {},\n", self.voice_frames));
        s.push_str(&format!("  \"pcm_samples\": {},\n", self.pcm_samples));
        s.push_str(&format!("  \"clean_nids\": {},\n", self.clean_nids));
        s.push_str(&format!("  \"sync_count\": {},\n", self.syncs.len()));
        s.push_str(&format!(
            "  \"mean_sync_bit_errors\": {:.4},\n",
            self.mean_sync_errors()
        ));

        // Symbol health block.
        s.push_str("  \"symbol_health\": {\n");
        s.push_str(&format!(
            "    \"level_counts\": [{},{},{},{}],\n",
            self.health.level_counts[0],
            self.health.level_counts[1],
            self.health.level_counts[2],
            self.health.level_counts[3]
        ));
        s.push_str(&format!(
            "    \"soft_mean\": {:.5},\n",
            self.health.soft_mean()
        ));
        s.push_str(&format!(
            "    \"eye_error\": {:.5},\n",
            self.health.eye_error()
        ));
        s.push_str("    \"soft_samples\": [");
        for (i, v) in self.health.soft_samples.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("{v:.3}"));
        }
        s.push_str("]\n  },\n");

        // Syncs (capped to keep files small on long captures).
        s.push_str("  \"syncs\": [");
        for (i, sync) in self.syncs.iter().take(2000).enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"at\":{},\"err\":{}}}",
                sync.at_symbol, sync.bit_errors
            ));
        }
        s.push_str("],\n");

        // NIDs.
        s.push_str("  \"nids\": [");
        for (i, n) in self.nids.iter().take(2000).enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"nac\":\"{:03X}\",\"duid\":\"{:X}\",\"bch_err\":{}}}",
                n.nac, n.duid, n.bch_errors
            ));
        }
        s.push_str("],\n");

        // Manufacturer-specific TSBKs, by (MFID, opcode).
        s.push_str("  \"vendor_tsbks\": [");
        for (i, (mfid, op, n)) in self.vendor_tsbks.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"mfid\":\"{mfid:02X}\",\"opcode\":\"{op:02X}\",\"count\":{n}}}"
            ));
        }
        s.push_str("],\n");

        s.push_str("  \"lc_raw\": [");
        for (i, r) in self.lc_raw.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let hex: String = r.iter().map(|b| format!("{b:02X}")).collect();
            s.push_str(&format!("\"{hex}\""));
        }
        s.push_str("],\n");

        s.push_str("  \"link_control\": [");
        for (i, l) in self.link_control.iter().take(2000).enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"tg\":{},\"src\":{},\"emergency\":{}}}",
                l.talkgroup, l.source_unit, l.emergency
            ));
        }
        s.push_str("],\n");

        s.push_str("  \"vendor_lc\": [");
        for (i, (m, o, n)) in self.vendor_lc.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"mfid\":\"{m:02X}\",\"lco\":\"{o:02X}\",\"count\":{n}}}"
            ));
        }
        s.push_str("],\n");

        s.push_str("  \"vendor_lc_samples\": [");
        for (i, (m, o, a)) in self.vendor_lc_samples.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let hex: String = a.iter().map(|b| format!("{b:02X}")).collect();
            s.push_str(&format!("[\"{m:02X}\",\"{o:02X}\",\"{hex}\"]"));
        }
        s.push_str("],\n");

        s.push_str("  \"idens\": [");
        for (i, (id, base, sp)) in self.idens.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"iden\":{id},\"base_hz\":{base},\"spacing_hz\":{sp}}}"
            ));
        }
        s.push_str("],\n");

        s.push_str("  \"patches\": [");
        for (i, (sg, members)) in self.patches.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let list: Vec<String> = members.iter().map(|m| m.to_string()).collect();
            s.push_str(&format!(
                "{{\"supergroup\":{sg},\"talkgroups\":[{}]}}",
                list.join(",")
            ));
        }
        s.push_str("],\n");

        s.push_str("  \"vendor_samples\": [");
        for (i, (m, o, a)) in self.vendor_samples.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("[\"{m:02X}\",\"{o:02X}\",\"{a:016X}\"]"));
        }
        s.push_str("],\n");

        // Radio position reports (LRRP over packet data).
        s.push_str(&format!("  \"packets\": {},\n", self.packets));
        s.push_str("  \"locations\": [");
        for (i, l) in self.locations.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"llid\":{},\"lat\":{:.6},\"lon\":{:.6}}}",
                l.llid, l.lat, l.lon
            ));
        }
        s.push_str("],\n");

        // Grants.
        s.push_str("  \"grants\": [");
        for (i, g) in self.grants.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"tg\":{},\"src\":{},\"freq_hz\":{},\"enc\":{}}}",
                g.talkgroup, g.source_unit, g.freq_hz, g.encrypted
            ));
        }
        s.push_str("],\n");

        // Encrypted skips.
        s.push_str("  \"encrypted_talkgroups\": [");
        for (i, tg) in self.encrypted_skips.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&tg.to_string());
        }
        s.push_str("]\n}\n");
        s
    }
}
