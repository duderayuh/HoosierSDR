/* Drawing a board.
 *
 * Loaded by two pages that share nothing else: the desktop app, and the
 * read-only page served to another machine over Tailscale. Both are handed
 * the same finished `RenderedBoard` from `dashboards::render`, so the only
 * thing that happens here is writing it out — no matching, no filtering, no
 * decision about what belongs on the board.
 *
 * It carries its own `esc` on purpose. The served page does not load app.js,
 * and every string below is either a summary written by a language model or a
 * title typed by the listener; both reach a machine that is not this one.
 */
(() => {
  const esc = (v) => String(v ?? "").replace(/[&<>"']/g, (ch) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch]));

  // Ages are measured against the clock of the machine that drew the board,
  // not the one showing it. A wall display with a wrong clock would otherwise
  // report every row as hours old, or as arriving in the future.
  const since = (board) => Math.floor(Date.now() / 1000) - (board && board.at ? board.at : 0);

  function ago(t, skew) {
    const s = Math.max(0, Math.floor(Date.now() / 1000) - (skew || 0) - t);
    if (s < 60) return s + "s ago";
    if (s < 3600) return Math.floor(s / 60) + " min ago";
    if (s < 86400) return Math.floor(s / 3600) + " h ago";
    return Math.floor(s / 86400) + " d ago";
  }

  // The stated time to arrival, marked where it sits in the prose. `eta` is
  // the exact phrase Rust found in this text, so the span is located by
  // looking the phrase back up rather than by matching again out here — one
  // matcher, no second opinion to drift from it.
  function body(card) {
    const s = card.body || "";
    const at = card.eta ? s.indexOf(card.eta) : -1;
    if (at < 0) return esc(s);
    return esc(s.slice(0, at)) + `<mark class="dbeta">${esc(card.eta)}</mark>` +
      esc(s.slice(at + card.eta.length));
  }

  // Only a PNG the board itself carries, never a link: this page is on
  // another machine, and an address in `src` would be a request it makes on
  // the board's say-so.
  function picture(src) {
    if (typeof src !== "string" || !/^data:image\/png;base64,[A-Za-z0-9+/=]+$/.test(src)) return "";
    return `<img class="dbmap" alt="Map of the scene" src="${src}" />`;
  }

  function cardHtml(c, skew) {
    const meta = (c.meta || []).map(esc).join(" · ");
    return `<article class="dbcard${c.style ? " em-" + esc(c.style) : ""}">
      <div class="dbcardhead">
        <span class="dbtitle">${esc(c.emoji || "")} ${esc(c.title || "")}</span>
        ${c.note ? `<span class="dbtag">${esc(c.note)}</span>` : ""}
        <span class="spacer"></span>
        <span class="dbid mono">#${esc(c.id)}</span>
      </div>
      <div class="dbmeta mono faint">
        <span class="ago" data-t="${esc(c.at)}">${esc(ago(c.at, skew))}</span>
        ${meta ? " · " + meta : ""}
        ${c.eta ? ` · <span class="dbetachip">${esc(c.eta)}</span>` : ""}
      </div>
      ${c.body ? `<div class="dbbody">${body(c)}</div>` : ""}
      ${(c.lines || []).length ? `<ol class="dblines mono">${c.lines.map((l) => `<li>${esc(l)}</li>`).join("")}</ol>` : ""}
      ${picture(c.image)}
    </article>`;
  }

  function paneHtml(p, skew) {
    const cards = (p.cards || []).map((c) => cardHtml(c, skew)).join("");
    return `<section class="dbpane" style="flex:${Math.max(1, +p.width || 1)}">
      <div class="dbpanehead">
        <span class="eyebrow">${esc(p.title || "")}</span>
        ${p.sub ? `<span class="faint">${esc(p.sub)}</span>` : ""}
        <span class="spacer"></span>
        <span class="mono faint">${esc(p.total || 0)}</span>
      </div>
      <div class="dbpanebody">${cards || `<div class="empty small">Nothing matching yet.</div>`}</div>
    </section>`;
  }

  window.HSBoard = {
    esc,
    since,
    ago,
    /// Write a board into `el`. Returns the clock skew, so a caller ticking
    /// the timestamps between draws ages them the same way.
    paint(el, board) {
      const skew = since(board);
      el.innerHTML = (board.panes || []).map((p) => paneHtml(p, skew)).join("");
      return skew;
    },
    /// Re-age the "12 min ago" labels in place, without redrawing.
    tick(el, skew) {
      el.querySelectorAll(".ago[data-t]").forEach((n) => {
        n.textContent = ago(+n.dataset.t, skew);
      });
    },
  };
})();
