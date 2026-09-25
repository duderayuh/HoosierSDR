/* Research: the intervals a study asks for, over every case in a window.
 *
 * Every statistic is computed in `research.rs`; this page only draws it and
 * takes in the ED's own record for a case (the phone call as logged, the
 * arrival from the chart), which the radio cannot hear.
 *
 * Slicing happens here: a filter or a breakdown picks which cases, and those
 * cases go back to `research_summary` for their medians and counts. What a
 * case *is* ("ROSC said", its minutes on each interval) arrives on the row
 * from Rust as `tags` and `minutes`, so no definition is restated here. The
 * only arithmetic on this page is counting cases into histogram bins.
 */
(() => {
  if (typeof invoke !== "function") return;

  let data = null, timer = null, busy = false, seq = 0;
  let filters = [];           // {kind: "tag"|"has"|"range"|"dim", key, dim, value, lo, hi, not}
  let query = "";
  let sort = { by: "dispatched", dir: -1 };
  let slice = [];             // the cases the filters keep, sorted
  let sums = null;            // research_summary for the slice, then for each group
  let groups = [];            // [[label, rows]] for the breakdown

  const pref = (k, d) => { try { return localStorage.getItem(`hs.rs.${k}`) || d; } catch (_) { return d; } };
  const keep = (k, v) => { try { localStorage.setItem(`hs.rs.${k}`, v); } catch (_) { /* private window */ } };
  let focus = pref("focus", "alert_to_call");
  let group = pref("group", "place");

  const shown = () => $("view-research") && $("view-research").style.display !== "none";
  const hm = (epoch) => new Date(epoch * 1000).toLocaleTimeString("en-US", { hour12: false, hour: "2-digit", minute: "2-digit" });
  const when = (epoch) => new Date(epoch * 1000).toLocaleString("en-US", { hour12: false, month: "short", day: "2-digit", hour: "2-digit", minute: "2-digit" });
  const min = (secs) => secs == null ? "" : (secs / 60).toFixed(1);
  const pct = (n, of) => of ? `${Math.round((n / of) * 100)}%` : "";
  const p2 = (n) => String(n).padStart(2, "0");
  // datetime-local wants local "YYYY-MM-DDTHH:MM"; epoch seconds come back.
  const toLocal = (epoch) => {
    if (epoch == null) return "";
    const d = new Date(epoch * 1000);
    return `${d.getFullYear()}-${p2(d.getMonth() + 1)}-${p2(d.getDate())}T${p2(d.getHours())}:${p2(d.getMinutes())}`;
  };
  const fromLocal = (v) => { if (!v) return null; const t = new Date(v).getTime(); return Number.isFinite(t) ? Math.round(t / 1000) : null; };
  const minutes = (r, k) => (r.minutes && r.minutes[k] != null ? r.minutes[k] : null);
  const tags = (r) => r.tags || [];
  const measureDef = (k) => data && data.measures.find((m) => m.key === k);
  const countDef = (k) => data && data.counts.find((c) => c.key === k);

  // What a case can be grouped by. Each is a reading of fields already on
  // the row, never a new definition.
  const DAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
  const DIMS = {
    place: { label: "Hospital", of: (r) => r.report_place || "No hospital heard" },
    title: { label: "Case type", of: (r) => r.title || "—" },
    state: { label: "State", of: (r) => r.state || "—" },
    alerted_how: { label: "Alert sent by", of: (r) => r.alerted_how || "No message sent" },
    call_how: { label: "Crew's call clock", of: (r) => r.call_how || "No call" },
    arrival_how: { label: "Arrival clock", of: (r) => r.arrival_how || "No arrival" },
    hour: { label: "Hour dispatched", of: (r) => r.dispatched == null ? "—" : `${p2(new Date(r.dispatched * 1000).getHours())}:00`, natural: true },
    weekday: { label: "Weekday", of: (r) => r.dispatched == null ? "—" : DAYS[new Date(r.dispatched * 1000).getDay()], order: (a) => DAYS.indexOf(a) },
    month: { label: "Month", of: (r) => { if (r.dispatched == null) return "—"; const d = new Date(r.dispatched * 1000); return `${d.getFullYear()}-${p2(d.getMonth() + 1)}`; }, natural: true },
  };

  // ---- filters -------------------------------------------------------------

  function passes(r, f) {
    let yes;
    if (f.kind === "tag") yes = tags(r).includes(f.key);
    else if (f.kind === "has") yes = minutes(r, f.key) != null;
    else if (f.kind === "range") { const v = minutes(r, f.key); yes = v != null && v >= f.lo && v < f.hi; }
    else if (f.kind === "dim") yes = DIMS[f.dim] && DIMS[f.dim].of(r) === f.value;
    else yes = true;
    return f.not ? !yes : yes;
  }

  function matches(r) {
    if (!query) return true;
    const hay = `${r.title} ${r.address} ${r.report_place} ${r.note} ${r.state} #${r.incident}`.toLowerCase();
    return query.toLowerCase().split(/\s+/).filter(Boolean).every((w) => hay.includes(w));
  }

  const same = (a, b) => a.kind === b.kind && a.key === b.key && a.dim === b.dim && a.value === b.value && a.lo === b.lo;

  function toggle(f) {
    const i = filters.findIndex((x) => same(x, f));
    if (i >= 0) filters.splice(i, 1); else filters.push(f);
    apply();
  }

  function chipLabel(f) {
    const m = f.key ? measureDef(f.key) : null;
    if (f.kind === "tag") { const c = countDef(f.key); return c ? c.label : f.key; }
    if (f.kind === "has") return `Has ${m ? m.label : f.key}`;
    if (f.kind === "range") return `${m ? m.label : f.key}: ${f.lo} to ${f.hi} min`;
    if (f.kind === "dim") return `${DIMS[f.dim] ? DIMS[f.dim].label : f.dim}: ${f.value}`;
    return "?";
  }

  function drawChips() {
    $("rsChips").innerHTML = filters.map((f, i) =>
      `<span class="chip on${f.not ? " rs-not" : ""}" data-i="${i}" title="${f.kind === "range" || f.kind === "dim" ? "Click to invert" : "Click to invert: cases without this"}">`
      + `${f.not ? "Not: " : ""}${esc(chipLabel(f))}<button class="rs-x" data-x="${i}" title="Remove this filter" aria-label="Remove">×</button></span>`).join("");
    $("rsChips").querySelectorAll("[data-x]").forEach((b) => b.onclick = (e) => { e.stopPropagation(); filters.splice(+b.dataset.x, 1); apply(); });
    $("rsChips").querySelectorAll("[data-i]").forEach((c) => c.onclick = () => { const f = filters[+c.dataset.i]; f.not = !f.not; apply(); });
    $("rsClear").style.display = filters.length || query ? "" : "none";
  }

  // The "+ Filter" menu: every count, every interval, and every value of
  // every grouping the window holds.
  function drawAdd() {
    if (!data) return;
    const counts = data.counts.filter((c) => c.key !== "cases");
    const dims = Object.entries(DIMS).filter(([k]) => !["hour", "month"].includes(k)).map(([k, d]) => {
      const vals = [...new Set(data.rows.map(d.of))].sort();
      return vals.length > 1 ? `<optgroup label="${esc(d.label)}">${vals.map((v) => `<option value="${esc(JSON.stringify({ kind: "dim", dim: k, value: v }))}">${esc(v)}</option>`).join("")}</optgroup>` : "";
    }).join("");
    $("rsAdd").innerHTML = `<option value="">+ Filter…</option>`
      + `<optgroup label="Said, sent or stated">${counts.map((c) => `<option value="${esc(JSON.stringify({ kind: "tag", key: c.key }))}">${esc(c.label)}</option>`).join("")}</optgroup>`
      + `<optgroup label="Has an interval">${data.measures.map((m) => `<option value="${esc(JSON.stringify({ kind: "has", key: m.key }))}">${esc(m.label)}</option>`).join("")}</optgroup>`
      + dims;
  }

  // ---- the slice -------------------------------------------------------------

  const SORTS = {
    dispatched: (r) => r.dispatched,
    title: (r) => (r.title || "").toLowerCase(),
    state: (r) => r.state,
    alerted: (r) => r.alerted,
    call: (r) => (r.phone_at != null ? r.phone_at : r.report),
    place: (r) => (r.report_place || "").toLowerCase(),
    score: (r) => (r.score && r.score.hi_pct != null ? r.score.hi_pct * 1000 + r.score.lo_pct : null),
  };
  const sortVal = (by) => (by.startsWith("m:") ? (r) => minutes(r, by.slice(2)) : SORTS[by] || SORTS.dispatched);

  function sorted(rows) {
    const g = sortVal(sort.by);
    return rows.slice().sort((a, b) => {
      const x = g(a), y = g(b);
      if (x == null && y == null) return 0;
      if (x == null) return 1;       // gaps last, either way
      if (y == null) return -1;
      return (x < y ? -1 : x > y ? 1 : 0) * sort.dir;
    });
  }

  function grouped(rows) {
    const d = DIMS[group] || DIMS.place;
    const by = new Map();
    rows.forEach((r) => { const k = d.of(r); if (!by.has(k)) by.set(k, []); by.get(k).push(r); });
    const out = [...by.entries()];
    if (d.order) out.sort((a, b) => d.order(a[0]) - d.order(b[0]));
    else if (d.natural) out.sort((a, b) => (a[0] < b[0] ? -1 : 1));
    else out.sort((a, b) => b[1].length - a[1].length || (a[0] < b[0] ? -1 : 1));
    return out;
  }

  // Re-slice and redraw. The statistics come back from Rust for the slice
  // and for each group in one call; a later apply() wins over a slow one.
  async function apply() {
    if (!data) return;
    slice = sorted(data.rows.filter((r) => matches(r) && filters.every((f) => passes(r, f))));
    groups = grouped(slice);
    drawChips();
    drawMeta();
    cases(slice);
    const mine = ++seq;
    try {
      const out = await invoke("research_summary", { groups: [slice].concat(groups.map((g) => g[1])) });
      if (mine !== seq || !Array.isArray(out)) return;
      sums = out;
      measures(out[0].measures, "rsMeasures", true);
      counts(out[0].counts);
      explore();
    } catch (e) { uiToast(`${e}`, "err"); }
  }

  function drawMeta() {
    const all = data.rows.length;
    const cut = slice.length !== all;
    $("rsMeta").textContent = `${cut ? `${slice.length} of ${all}` : all} cases · ${when(data.from)} → ${when(data.to)}`;
    $("rsCasesMeta").textContent = cut ? `${slice.length} of ${all} · filtered` : all ? `${all}` : "";
    $("rsExport").textContent = cut ? `Export ${slice.length} cases` : "Export CSV";
  }

  // ---- the statistics tables -----------------------------------------------

  function measures(rows, id, clickable) {
    const body = rows.map((m) => `<tr title="${esc(m.definition)}"${clickable ? ` data-mk="${esc(m.key)}"` : ""}${clickable && m.key === focus ? ' class="on"' : ""}><td>${esc(m.label)}</td><td class="mono">${m.n}</td>`
      + `<td class="mono"><b>${m.n ? m.median : "—"}</b></td><td class="mono">${m.n ? `${m.p25} – ${m.p75}` : ""}</td>`
      + `<td class="mono">${m.n ? m.min : ""}</td><td class="mono">${m.n ? m.max : ""}</td><td class="mono">${m.n ? m.mean : ""}</td>`
      + `<td class="mono">${m.negative ? m.negative : ""}</td></tr>`).join("");
    $(id).innerHTML = `<table class="cs-count rs-table${clickable ? " rs-click" : ""}"><thead><tr><th>Interval (minutes)</th><th title="Cases with both clocks">n</th><th title="The middle case">Median</th><th title="Interquartile range: the middle half of cases fall between these">IQR</th><th>Min</th><th>Max</th><th>Mean</th><th title="Cases where the later clock came first">Neg.</th></tr></thead><tbody>${body}</tbody></table>`;
    if (clickable) $(id).querySelectorAll("tr[data-mk]").forEach((tr) => tr.onclick = () => setFocus(tr.dataset.mk, true));
  }

  function counts(rows) {
    const on = (k) => filters.some((f) => f.kind === "tag" && f.key === k && !f.not);
    $("rsCounts").innerHTML = `<table class="cs-count rs-table rs-click"><thead><tr><th>Count <span class="faint">— click to keep only these cases</span></th><th>n</th><th>of</th><th>%</th></tr></thead><tbody>`
      + rows.map((c) => `<tr${c.key !== "cases" ? ` data-ck="${esc(c.key)}"` : ""}${on(c.key) ? ' class="on"' : ""}><td>${esc(c.label)}</td><td class="mono">${c.n}</td><td class="mono">${c.of}</td><td class="mono">${pct(c.n, c.of)}</td></tr>`).join("")
      + `</tbody></table>`;
    $("rsCounts").querySelectorAll("tr[data-ck]").forEach((tr) => tr.onclick = () => toggle({ kind: "tag", key: tr.dataset.ck }));
  }

  function library(s) {
    const l = s.library;
    $("rsLibrary").innerHTML = [
      ["Calls", l.calls], ["Hours of audio", l.hours], ["Transcribed", l.transcribed], ["Runs on the dispatch map", l.incidents],
      ["Hospital reports", l.reports], ["Joined to a run", `${l.reports_joined}${l.reports ? ` (${pct(l.reports_joined, l.reports)})` : ""}`],
      ["Joined by a learned radio", l.reports_joined_by_radio], ["Reports with facts", l.reports_with_facts], ["Tripwires sent", l.alerts_sent],
    ].map(([k, v]) => `<div><span class="k">${esc(k)}</span><span class="v mono">${esc(String(v))}</span></div>`).join("");
  }

  // ---- explore: the focused interval over the slice --------------------------

  function setFocus(k, scroll) {
    focus = k; keep("focus", k);
    if (sums) { measures(sums[0].measures, "rsMeasures", true); explore(); }
    cases(slice);
    if (scroll && $("rsExplore").scrollIntoView) $("rsExplore").scrollIntoView({ behavior: "smooth", block: "start" });
  }

  function explore() {
    if (!data || !sums) return;
    const m = sums[0].measures.find((x) => x.key === focus) || sums[0].measures[0];
    if (!m) return;
    focus = m.key;
    $("rsFocus").innerHTML = sums[0].measures.map((x) => `<option value="${esc(x.key)}"${x.key === focus ? " selected" : ""}>${esc(x.label)}</option>`).join("");
    $("rsGroup").innerHTML = Object.entries(DIMS).map(([k, d]) => `<option value="${k}"${k === group ? " selected" : ""}>By ${esc(d.label.toLowerCase())}</option>`).join("");
    $("rsExMeta").textContent = `${slice.length} case${slice.length === 1 ? "" : "s"}`;
    $("rsExDef").textContent = `${m.label}: ${m.definition}.`;
    const has = slice.filter((r) => minutes(r, focus) != null).length;
    const only = filters.some((f) => f.kind === "has" && f.key === focus && !f.not);
    $("rsKpi").innerHTML = [
      ["Cases with it", `${m.n}<span class="faint small"> of ${slice.length}</span>`],
      ["Median", m.n ? `${m.median} min` : "—"],
      ["Middle half (IQR)", m.n ? `${m.p25} – ${m.p75} min` : "—"],
      ["Range", m.n ? `${m.min} – ${m.max} min` : "—"],
      ["Mean", m.n ? `${m.mean} min` : "—"],
      ["Later clock first", m.n ? `${m.negative}<span class="faint small"> (${pct(m.negative, m.n) || "0%"})</span>` : "—"],
    ].map(([k, v]) => `<div><span class="k">${esc(k)}</span><span class="v mono">${v}</span></div>`).join("");
    $("rsOnly").style.display = has && has < slice.length && !only ? "" : "none";
    $("rsOnly").textContent = `Show only these ${has} cases`;
    hist(m);
    breakdown(m);
    overTime();
  }

  const W = () => Math.max(320, Math.round(($("rsHist") && $("rsHist").clientWidth) || 640));
  // A bar with its data end rounded and its base square on the axis.
  const bar = (x, y, w, h) => {
    const r = Math.min(4, w / 2, h);
    return `M${x},${y + h}V${y + r}Q${x},${y} ${x + r},${y}H${x + w - r}Q${x + w},${y} ${x + w},${y + r}V${y + h}Z`;
  };

  // Histogram of the focused interval, the middle half shaded and the median
  // marked. A bar is a range of minutes; clicking it keeps those cases.
  function hist(m) {
    const vals = slice.map((r) => minutes(r, m.key)).filter((v) => v != null);
    if (!vals.length) { $("rsHist").innerHTML = `<div class="empty small">No case in this slice has both clocks for this interval.</div>`; return; }
    const lo0 = Math.min(...vals), hi0 = Math.max(...vals);
    const step = [0.25, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 240].find((s) => (hi0 - lo0) / s <= 24) || 480;
    const lo = Math.floor(lo0 / step) * step;
    const nb = Math.max(1, Math.floor((hi0 - lo) / step) + 1);
    const bins = new Array(nb).fill(0);
    vals.forEach((v) => { bins[Math.min(nb - 1, Math.floor((v - lo) / step + 1e-9))]++; });
    const w = W(), h = 150, L = 30, R = 8, T = 10, B = 22;
    const pw = w - L - R, ph = h - T - B;
    const top = Math.max(...bins);
    const x = (v) => L + ((v - lo) / (nb * step)) * pw;
    const y = (n) => T + ph - (n / top) * ph;
    const bw = pw / nb;
    const every = Math.max(1, Math.ceil(nb / Math.floor(pw / 46)));
    const fmt = (v) => String(Math.round(v * 100) / 100);
    let s = `<svg viewBox="0 0 ${w} ${h}" width="${w}" height="${h}" role="img" aria-label="${esc(`${m.label}: ${vals.length} cases, median ${m.median} minutes`)}">`;
    s += `<line class="grid" x1="${L}" x2="${w - R}" y1="${T}" y2="${T}"/><text x="${L - 5}" y="${T + 3}" text-anchor="end">${top}</text>`;
    s += `<rect class="iqr" x="${x(m.p25)}" y="${T}" width="${Math.max(1, x(m.p75) - x(m.p25))}" height="${ph}"><title>Middle half: ${m.p25} – ${m.p75} min</title></rect>`;
    bins.forEach((n, i) => {
      const a = lo + i * step, b = a + step;
      s += `<g class="bin" data-lo="${fmt(a)}" data-hi="${fmt(b)}"><rect class="hit" x="${L + i * bw}" y="${T}" width="${bw}" height="${ph}"/>`
        + (n ? `<path class="bar" d="${bar(L + i * bw + 1, y(n), Math.max(1, bw - 2), T + ph - y(n))}"/>` : "")
        + `<title>${fmt(a)} to ${fmt(b)} min: ${n} case${n === 1 ? "" : "s"}${n ? " — click to keep only these" : ""}</title></g>`;
      if (i % every === 0) s += `<text x="${L + i * bw}" y="${h - 7}" text-anchor="middle">${fmt(a)}</text>`;
    });
    s += `<text x="${w - R}" y="${h - 7}" text-anchor="end">min</text>`;
    if (lo < 0 && lo + nb * step > 0) s += `<line class="zero" x1="${x(0)}" x2="${x(0)}" y1="${T}" y2="${T + ph}"><title>0: the two clocks at the same moment</title></line>`;
    s += `<line class="base" x1="${L}" x2="${w - R}" y1="${T + ph}" y2="${T + ph}"/>`;
    s += `<line class="med" x1="${x(m.median)}" x2="${x(m.median)}" y1="${T - 4}" y2="${T + ph}"><title>Median ${m.median} min</title></line>`;
    s += `</svg>`;
    $("rsHist").innerHTML = s;
    $("rsHist").querySelectorAll("g.bin").forEach((g) => g.onclick = () => {
      if (!g.querySelector(".bar")) return;
      // One range per interval: a new bar replaces the last, the same bar clears it.
      const f = { kind: "range", key: m.key, lo: +g.dataset.lo, hi: +g.dataset.hi };
      const again = filters.some((x) => same(x, f));
      filters = filters.filter((x) => !(x.kind === "range" && x.key === m.key));
      if (!again) filters.push(f);
      apply();
    });
  }

  // The slice broken down by one grouping: how many cases, and the focused
  // interval's median and middle half in each, from Rust.
  function breakdown(m) {
    const d = DIMS[group] || DIMS.place;
    $("rsGroupLab").textContent = d.label.toLowerCase();
    if (!groups.length) { $("rsGroups").innerHTML = ""; return; }
    const most = Math.max(...groups.map((g) => g[1].length));
    const hiMed = Math.max(0, ...groups.map((g, i) => { const x = sums[i + 1] && sums[i + 1].measures.find((y) => y.key === m.key); return x && x.n ? x.median : 0; }));
    const on = (v) => filters.some((f) => f.kind === "dim" && f.dim === group && f.value === v && !f.not);
    const rows = groups.map(([label, rs], i) => {
      const g = sums[i + 1] && sums[i + 1].measures.find((y) => y.key === m.key);
      const med = g && g.n ? g.median : null;
      return `<tr data-gv="${esc(label)}"${on(label) ? ' class="on"' : ""}><td>${esc(label)}</td>`
        + `<td class="mono"><span class="rs-bar" style="width:${Math.max(2, Math.round((rs.length / most) * 80))}px"></span> ${rs.length}</td>`
        + `<td class="mono">${pct(rs.length, slice.length)}</td>`
        + `<td class="mono">${g ? g.n : 0}</td>`
        + `<td class="mono">${med != null ? `<span class="rs-bar alt" style="width:${hiMed > 0 && med > 0 ? Math.max(2, Math.round((med / hiMed) * 80)) : 0}px"></span> <b>${med}</b>` : "—"}</td>`
        + `<td class="mono">${g && g.n ? `${g.p25} – ${g.p75}` : ""}</td></tr>`;
    }).join("");
    $("rsGroups").innerHTML = `<table class="cs-count rs-table rs-click"><thead><tr><th>${esc(d.label)}</th><th>Cases</th><th>Share</th>`
      + `<th title="Cases in this group with both clocks">n</th><th>Median, min</th><th>IQR</th></tr></thead><tbody>${rows}</tbody></table>`;
    $("rsGroups").querySelectorAll("tr[data-gv]").forEach((tr) => tr.onclick = () => toggle({ kind: "dim", dim: group, value: tr.dataset.gv }));
  }

  // Cases in the slice per day, week or month, whichever keeps the bars
  // readable over the window.
  function overTime() {
    if (!slice.length) { $("rsTime").innerHTML = ""; return; }
    const span = (data.to - data.from) / 86400;
    const unit = span <= 45 ? "day" : span <= 400 ? "week" : "month";
    const start = (t) => {
      const d = new Date(t * 1000); d.setHours(0, 0, 0, 0);
      if (unit === "week") d.setDate(d.getDate() - ((d.getDay() + 6) % 7));
      if (unit === "month") d.setDate(1);
      return d;
    };
    const next = (d) => { const e = new Date(d); if (unit === "day") e.setDate(e.getDate() + 1); else if (unit === "week") e.setDate(e.getDate() + 7); else e.setMonth(e.getMonth() + 1); return e; };
    const first = start(Math.max(data.from, Math.min(...slice.map((r) => r.dispatched || data.to))));
    const buckets = [];
    for (let d = first; d.getTime() / 1000 <= data.to; d = next(d)) buckets.push({ at: d, n: 0 });
    if (!buckets.length) return;
    slice.forEach((r) => {
      if (r.dispatched == null) return;
      const t = start(r.dispatched).getTime();
      const b = buckets.find((x) => x.at.getTime() === t);
      if (b) b.n++;
    });
    const w = W(), h = 110, L = 30, R = 8, T = 8, B = 20;
    const pw = w - L - R, ph = h - T - B;
    const top = Math.max(1, ...buckets.map((b) => b.n));
    const bw = pw / buckets.length;
    const every = Math.max(1, Math.ceil(buckets.length / Math.floor(pw / 56)));
    const name = (d) => unit === "month" ? d.toLocaleDateString("en-US", { month: "short", year: "2-digit" }) : d.toLocaleDateString("en-US", { month: "short", day: "numeric" });
    let s = `<svg viewBox="0 0 ${w} ${h}" width="${w}" height="${h}" role="img" aria-label="Cases per ${unit}">`;
    s += `<line class="grid" x1="${L}" x2="${w - R}" y1="${T}" y2="${T}"/><text x="${L - 5}" y="${T + 3}" text-anchor="end">${top}</text>`;
    buckets.forEach((b, i) => {
      const bh = (b.n / top) * ph;
      s += `<g class="bin rs-still"><rect class="hit" x="${L + i * bw}" y="${T}" width="${bw}" height="${ph}"/>`
        + (b.n ? `<path class="bar" d="${bar(L + i * bw + 1, T + ph - bh, Math.max(1, bw - 2), bh)}"/>` : "")
        + `<title>${unit === "week" ? "Week of " : ""}${name(b.at)}: ${b.n} case${b.n === 1 ? "" : "s"}</title></g>`;
      if (i % every === 0) s += `<text x="${L + i * bw + bw / 2}" y="${h - 6}" text-anchor="middle">${name(b.at)}</text>`;
    });
    s += `<line class="base" x1="${L}" x2="${w - R}" y1="${T + ph}" y2="${T + ph}"/></svg>`;
    $("rsTime").innerHTML = s;
    $("rsTimeLab").textContent = `Cases per ${unit}`;
  }

  // ---- the cases -------------------------------------------------------------

  const SHOWN = ["alert_to_call", "known_to_call", "dispatch_to_call", "call_to_arrival"];

  function caseRow(r) {
    const key = `${r.profile}|${r.incident}`;
    const call = r.phone_at != null ? r.phone_at : r.report;
    const extra = !SHOWN.includes(focus);
    return `<tr data-rs="${esc(key)}" class="${r.recorded ? "rs-recorded" : ""}">`
      + `<td class="mono small" title="${esc(r.recorded ? "From a snapshot: the case or its calls are no longer in the library" : `Incident #${r.incident}`)}">${esc(when(r.dispatched))}${r.recorded ? " ◌" : ""}</td>`
      + `<td><b>${esc(r.title)}</b><br><span class="faint small">${esc(r.address || "address not heard")}</span></td>`
      + `<td class="small">${esc(r.state)}</td>`
      + `<td class="mono" title="${esc(r.alerted_how ? `Sent ${hm(r.alerted)} by ${r.alerted_how}` : "No message was sent about this run")}">${r.alerted != null ? hm(r.alerted) : "—"}</td>`
      + `<td class="mono" title="${esc(r.call_how ? `From the ${r.call_how}` : "No report joined and nothing typed in")}">${call != null ? `${hm(call)}<span class="faint small"> ${r.call_how === "ED record" ? "ED" : ""}</span>` : "—"}</td>`
      + `<td class="mono${focus === "alert_to_call" ? " rs-focus" : ""}"><b>${min(r.alert_to_call) || "—"}</b></td>`
      + `<td class="mono${focus === "known_to_call" ? " rs-focus" : ""}">${min(r.known_to_call) || "—"}</td>`
      + `<td class="mono${focus === "dispatch_to_call" ? " rs-focus" : ""}">${min(r.dispatch_to_call) || "—"}</td>`
      + `<td class="mono${focus === "call_to_arrival" ? " rs-focus" : ""}" title="${esc(r.arrival_how ? `From the ${r.arrival_how}` : "")}">${min(r.call_to_arrival) || "—"}</td>`
      + (extra ? `<td class="mono rs-focus">${minutes(r, focus) != null ? minutes(r, focus).toFixed(1) : "—"}</td>` : "")
      + `<td class="small">${esc(r.report_place || "")}${r.eta_said ? `<br><span class="faint">said “${esc(r.eta_said)}”</span>` : ""}</td>`
      + scoreCell(r)
      + `<td><input type="datetime-local" class="rs-in" data-k="phone" value="${toLocal(r.phone_at)}" title="The phone call as the ED logged it" /></td>`
      + `<td><input type="datetime-local" class="rs-in" data-k="arrived" value="${toLocal(r.ed_arrived_at)}" title="Arrival from the chart" /></td>`
      + `<td><input type="text" class="rs-in rs-note" data-k="note" value="${esc(r.note || "")}" placeholder="note" /></td>`
      + `<td><button class="btn ghost sm" data-rssave title="Keep the ED's record for this case">Save</button></td>`
      + `</tr>`;
  }

  // The score as worked out, each criterion and what it rests on in the
  // tooltip, and the screen's own guess beside it when one ran.
  function scoreCell(r) {
    const s = r.score;
    if (!s) return `<td class="small faint" title="No crew report joined, so nothing to score">—</td>`;
    const lines = [`${s.name}`].concat((s.criteria || []).map((c) => `${c.label}: ${c.verdict} — ${c.why}`));
    if (r.screen) lines.push(`Screen “${r.screen.rule}” said ${r.screen.candidate || "?"}${r.screen.likelihood_pct ? `, ${r.screen.likelihood_pct}%` : ""}`);
    return `<td class="small${s.complete ? "" : " faint"}" title="${esc(lines.join("\n"))}">${esc(s.estimate)}</td>`;
  }

  function th(by, label, title) {
    const on = sort.by === by;
    return `<th data-sort="${esc(by)}"${on ? ' class="sorted"' : ""} title="${esc(title ? `${title}. ` : "")}Click to sort">${label}${on ? (sort.dir < 0 ? " ↓" : " ↑") : ""}</th>`;
  }

  function cases(rows) {
    $("rsCasesEmpty").style.display = rows.length ? "none" : "";
    $("rsCasesEmpty").textContent = data && data.rows.length
      ? "No case matches these filters."
      : "No cases in this window. Cases are built on the Cases tab; “Rebuild…” there builds them from calls already stored.";
    const fm = measureDef(focus);
    $("rsCases").innerHTML = rows.length ? `<table class="cs-count rs-table rs-cases"><thead><tr>`
      + th("dispatched", "Dispatched") + th("title", "Case") + th("state", "State")
      + th("alerted", "Alert", "The first message sent about this run") + th("call", "Crew's call", "The ED's logged phone call, else the radio report")
      + th("m:alert_to_call", "Lead", "Crew's call minus alert, minutes") + th("m:known_to_call", "Could have led", "Crew's call minus the moment the page's transcript landed")
      + th("m:dispatch_to_call", "From page", "Crew's call minus the page") + th("m:call_to_arrival", "Call → arr.", "Arrival minus the crew's call")
      + (!SHOWN.includes(focus) && fm ? th(`m:${focus}`, esc(fm.label), fm.definition) : "")
      + th("place", "Hospital") + th("score", "Score", "Worked out from the report's stated facts; hover a value for each criterion")
      + `<th>ED phone call</th><th>ED arrival</th><th>Note</th><th></th></tr></thead><tbody>`
      + rows.map(caseRow).join("") + `</tbody></table>` : "";
    $("rsCases").querySelectorAll("th[data-sort]").forEach((h) => h.onclick = () => {
      const by = h.dataset.sort;
      sort = sort.by === by ? { by, dir: -sort.dir } : { by, dir: by === "title" || by === "place" || by === "state" ? 1 : -1 };
      slice = sorted(slice);
      cases(slice);
    });
    $("rsCases").querySelectorAll("[data-rssave]").forEach((b) => b.onclick = () => save(b.closest("tr")));
    $("rsCases").querySelectorAll("tr[data-rs]").forEach((tr) => tr.querySelectorAll(".rs-in").forEach((i) => i.onkeydown = (e) => { if (e.key === "Enter") save(tr); }));
  }

  async function save(tr) {
    const [profile, incident] = tr.dataset.rs.split("|");
    const get = (k) => tr.querySelector(`[data-k="${k}"]`).value;
    const b = tr.querySelector("[data-rssave]"); b.disabled = true;
    try {
      await invoke("research_set_record", { profile, incident: +incident, phoneAt: fromLocal(get("phone")), edArrivedAt: fromLocal(get("arrived")), note: get("note") });
      uiToast("Kept");
      await load();
    } catch (e) { uiToast(`${e}`, "err"); }
    b.disabled = false;
  }

  function draw() {
    if (!data) return;
    library(data);
    measures(data.pipeline, "rsPipeline", false);
    drawAdd();
    $("rsNotes").innerHTML = (data.notes || []).map((n) => `<li>${esc(n)}</li>`).join("");
    $("rsMethods").innerHTML = data.measures.concat(data.pipeline).map((m) => `<li><b>${esc(m.label)}</b>: ${esc(m.definition)}.</li>`).join("");
    apply();
  }

  async function load() {
    if (busy) return;
    busy = true;
    $("rsRefresh").disabled = true;
    try {
      data = await invoke("research_stats", { days: +$("rsDays").value });
      draw();
    } catch (e) { uiToast(`${e}`, "err"); }
    busy = false;
    $("rsRefresh").disabled = false;
  }

  $("rsDays").onchange = load;
  $("rsRefresh").onclick = load;
  $("rsExport").onclick = async () => {
    if (!data) return;
    const b = $("rsExport"), was = b.textContent; b.disabled = true; b.textContent = "Writing…";
    try {
      const where = await invoke("research_export", { rows: slice });
      uiToast(`Saved ${slice.length} cases to ${where}`);
    } catch (e) { uiToast(`${e}`, "err"); }
    b.disabled = false; b.textContent = was;
  };
  // The reference standard: a packet out, sheets back, a comparison out.
  const busyButton = async (id, label, work) => {
    const b = $(id), was = b.textContent; b.disabled = true; b.textContent = label;
    try { await work(); } catch (e) { uiToast(`${e}`, "err"); }
    b.disabled = false; b.textContent = was;
  };
  $("rsPacket").onclick = () => {
    if (!slice.length) { uiToast("No cases shown to review", "err"); return; }
    return busyButton("rsPacket", "Writing…", async () => uiToast(`Review packet: ${await invoke("study_packet", { rows: slice })}`));
  };
  $("rsImport").onclick = () => $("rsImportFile").click();
  $("rsImportFile").onchange = () => {
    const files = Array.from($("rsImportFile").files || []);
    $("rsImportFile").value = "";
    if (!files.length) return;
    return busyButton("rsImport", "Reading…", async () => {
      for (const f of files) uiToast(`${f.name}: ${await invoke("study_import", { text: await f.text() })}`);
    });
  };
  $("rsCompare").onclick = () => {
    if (!slice.length) { uiToast("No cases shown to compare", "err"); return; }
    return busyButton("rsCompare", "Writing…", async () => uiToast(`Comparison: ${await invoke("study_compare", { rows: slice })}`));
  };
  $("rsAdd").onchange = () => {
    const v = $("rsAdd").value; $("rsAdd").value = "";
    if (!v) return;
    const f = JSON.parse(v);
    if (!filters.some((x) => same(x, f))) { filters.push(f); apply(); }
  };
  let typing = null;
  $("rsQ").oninput = () => { clearTimeout(typing); typing = setTimeout(() => { query = $("rsQ").value.trim(); apply(); }, 200); };
  $("rsClear").onclick = () => { filters = []; query = ""; $("rsQ").value = ""; apply(); };
  $("rsFocus").onchange = () => setFocus($("rsFocus").value, false);
  $("rsGroup").onchange = () => { group = $("rsGroup").value; keep("group", group); apply(); };
  $("rsOnly").onclick = () => toggle({ kind: "has", key: focus });
  if (listen) listen("cases", () => { if (!shown()) return; clearTimeout(timer); timer = setTimeout(load, 1500); });
  window.researchOnShow = () => { if (!data) load(); else explore(); };
})();
