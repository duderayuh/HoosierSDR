/* Research: the intervals a study asks for, over every case in a window.
 *
 * Every number is computed in `research.rs`; this page only draws it and
 * takes in the ED's own record for a case (the phone call as logged, the
 * arrival from the chart), which the radio cannot hear.
 */
(() => {
  if (typeof invoke !== "function") return;

  let data = null, timer = null, busy = false;
  const shown = () => $("view-research") && $("view-research").style.display !== "none";
  const hm = (epoch) => new Date(epoch * 1000).toLocaleTimeString("en-US", { hour12: false, hour: "2-digit", minute: "2-digit" });
  const when = (epoch) => new Date(epoch * 1000).toLocaleString("en-US", { hour12: false, month: "short", day: "2-digit", hour: "2-digit", minute: "2-digit" });
  const min = (secs) => secs == null ? "" : (secs / 60).toFixed(1);
  const pct = (n, of) => of ? `${Math.round((n / of) * 100)}%` : "";
  // datetime-local wants local "YYYY-MM-DDTHH:MM"; epoch seconds come back.
  const toLocal = (epoch) => {
    if (epoch == null) return "";
    const d = new Date(epoch * 1000);
    const p = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`;
  };
  const fromLocal = (v) => { if (!v) return null; const t = new Date(v).getTime(); return Number.isFinite(t) ? Math.round(t / 1000) : null; };

  function measures(rows, id) {
    const body = rows.map((m) => `<tr title="${esc(m.definition)}"><td>${esc(m.label)}</td><td class="mono">${m.n}</td>`
      + `<td class="mono"><b>${m.n ? m.median : "—"}</b></td><td class="mono">${m.n ? `${m.p25} – ${m.p75}` : ""}</td>`
      + `<td class="mono">${m.n ? m.min : ""}</td><td class="mono">${m.n ? m.max : ""}</td><td class="mono">${m.n ? m.mean : ""}</td>`
      + `<td class="mono">${m.negative ? m.negative : ""}</td></tr>`).join("");
    $(id).innerHTML = `<table class="cs-count rs-table"><thead><tr><th>Interval (minutes)</th><th>n</th><th>Median</th><th>IQR</th><th>Min</th><th>Max</th><th>Mean</th><th title="Cases where the later clock came first">Neg.</th></tr></thead><tbody>${body}</tbody></table>`;
  }

  function counts(rows) {
    $("rsCounts").innerHTML = `<table class="cs-count rs-table"><thead><tr><th>Count</th><th>n</th><th>of</th><th>%</th></tr></thead><tbody>`
      + rows.map((c) => `<tr><td>${esc(c.label)}</td><td class="mono">${c.n}</td><td class="mono">${c.of}</td><td class="mono">${pct(c.n, c.of)}</td></tr>`).join("")
      + `</tbody></table>`;
  }

  function library(s) {
    const l = s.library;
    $("rsLibrary").innerHTML = [
      ["Calls", l.calls], ["Hours of audio", l.hours], ["Transcribed", l.transcribed], ["Runs on the dispatch map", l.incidents],
      ["Hospital reports", l.reports], ["Joined to a run", `${l.reports_joined}${l.reports ? ` (${pct(l.reports_joined, l.reports)})` : ""}`],
      ["Joined by a learned radio", l.reports_joined_by_radio], ["Reports with facts", l.reports_with_facts], ["Tripwires sent", l.alerts_sent],
    ].map(([k, v]) => `<div><span class="k">${esc(k)}</span><span class="v mono">${esc(String(v))}</span></div>`).join("");
  }

  function caseRow(r) {
    const key = `${r.profile}|${r.incident}`;
    const call = r.phone_at != null ? r.phone_at : r.report;
    return `<tr data-rs="${esc(key)}" class="${r.recorded ? "rs-recorded" : ""}">`
      + `<td class="mono small" title="${esc(r.recorded ? "From a snapshot: the case or its calls are no longer in the library" : `Incident #${r.incident}`)}">${esc(when(r.dispatched))}${r.recorded ? " ◌" : ""}</td>`
      + `<td><b>${esc(r.title)}</b><br><span class="faint small">${esc(r.address || "address not heard")}</span></td>`
      + `<td class="small">${esc(r.state)}</td>`
      + `<td class="mono" title="${esc(r.alerted_how ? `Sent ${hm(r.alerted)} by ${r.alerted_how}` : "No message was sent about this run")}">${r.alerted != null ? hm(r.alerted) : "—"}</td>`
      + `<td class="mono" title="${esc(r.call_how ? `From the ${r.call_how}` : "No report joined and nothing typed in")}">${call != null ? `${hm(call)}<span class="faint small"> ${r.call_how === "ED record" ? "ED" : ""}</span>` : "—"}</td>`
      + `<td class="mono"><b>${min(r.alert_to_call) || "—"}</b></td>`
      + `<td class="mono">${min(r.known_to_call) || "—"}</td>`
      + `<td class="mono">${min(r.dispatch_to_call) || "—"}</td>`
      + `<td class="mono" title="${esc(r.arrival_how ? `From the ${r.arrival_how}` : "")}">${min(r.call_to_arrival) || "—"}</td>`
      + `<td class="small">${esc(r.report_place || "")}${r.eta_said ? `<br><span class="faint">said “${esc(r.eta_said)}”</span>` : ""}</td>`
      + `<td><input type="datetime-local" class="rs-in" data-k="phone" value="${toLocal(r.phone_at)}" title="The phone call as the ED logged it" /></td>`
      + `<td><input type="datetime-local" class="rs-in" data-k="arrived" value="${toLocal(r.ed_arrived_at)}" title="Arrival from the chart" /></td>`
      + `<td><input type="text" class="rs-in rs-note" data-k="note" value="${esc(r.note || "")}" placeholder="note" /></td>`
      + `<td><button class="btn ghost sm" data-rssave title="Keep the ED's record for this case">Save</button></td>`
      + `</tr>`;
  }

  function cases(rows) {
    $("rsCasesMeta").textContent = rows.length ? `${rows.length}` : "";
    $("rsCasesEmpty").style.display = rows.length ? "none" : "";
    $("rsCases").innerHTML = rows.length ? `<table class="cs-count rs-table rs-cases"><thead><tr>`
      + `<th>Dispatched</th><th>Case</th><th>State</th><th title="The first message sent about this run">Alert</th><th title="The crew's call: the ED's logged phone call, else the radio report">Crew's call</th>`
      + `<th title="Crew's call minus alert, minutes">Lead</th><th title="Crew's call minus the moment the page's transcript landed">Could have led</th><th title="Crew's call minus the page">From page</th>`
      + `<th title="Arrival minus the crew's call">Call → arr.</th><th>Hospital</th><th>ED phone call</th><th>ED arrival</th><th>Note</th><th></th></tr></thead><tbody>`
      + rows.map(caseRow).join("") + `</tbody></table>` : "";
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
    $("rsMeta").textContent = `${data.rows.length} cases · ${when(data.from)} → ${when(data.to)}`;
    library(data);
    measures(data.measures, "rsMeasures");
    measures(data.pipeline, "rsPipeline");
    counts(data.counts);
    cases(data.rows);
    $("rsNotes").innerHTML = (data.notes || []).map((n) => `<li>${esc(n)}</li>`).join("");
    $("rsMethods").innerHTML = data.measures.concat(data.pipeline).map((m) => `<li><b>${esc(m.label)}</b>: ${esc(m.definition)}.</li>`).join("");
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
    const b = $("rsExport"); b.disabled = true; b.textContent = "Writing…";
    try {
      const where = await invoke("research_export", { days: +$("rsDays").value });
      uiToast(`Saved ${where}`);
    } catch (e) { uiToast(`${e}`, "err"); }
    b.disabled = false; b.textContent = "Export CSV";
  };
  if (listen) listen("cases", () => { if (!shown()) return; clearTimeout(timer); timer = setTimeout(load, 1500); });
  window.researchOnShow = () => { if (!data) load(); };
})();
