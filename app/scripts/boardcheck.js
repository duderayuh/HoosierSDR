// Boots the shared board page under jsdom with a fake server, so the page
// that other machines are pointed at is checked the way the desktop page is.
//   cd app && npm i --no-save jsdom@24 && node scripts/boardcheck.js
// Exits non-zero on any failure.
const { JSDOM } = require("jsdom");
const fs = require("fs");
const path = require("path");

const html = fs.readFileSync(path.join(__dirname, "..", "src", "web", "board.html"), "utf8");
const render = fs.readFileSync(path.join(__dirname, "..", "dist", "board-render.js"), "utf8");
const page = html.slice(html.indexOf("<script>\n/* A board"));
const inline = page.slice(page.indexOf("\n") + 1, page.indexOf("</script>"));

let failed = false;
const bad = (m) => { failed = true; console.log("BOARD ERROR: " + m); };

// A board with two cards: one whose body carries a marked ETA, one whose does
// not, because those are written by different branches. Both bodies are
// hostile — this is a page served to someone else's screen.
const BOARD = {
  id: "b1", name: "Methodist <script>alert(1)</script>", footer: "QI attestation — peer review only.",
  refresh_secs: 5, at: 1000,     // the floor, so a second poll comes quickly
  panes: [{
    id: "p1", kind: "reports", title: "Inbound", sub: "Methodist", width: 1, total: 2,
    cards: [
      { id: 1, at: 990, emoji: "", title: "Chest Pain", style: "alarm", note: "arrest",
        meta: ["Medic 21"], body: "Inbound <script>alert(2)</script>. The ETA is 15 minutes.",
        eta: "ETA is 15 minutes" },
      { id: 2, at: 980, emoji: "", title: "No ETA", style: "", note: "", meta: [],
        body: "Second unit <img src=x onerror=alert(3)> no time given.", eta: null },
    ],
  }],
};

async function boot(responder) {
  const dom = new JSDOM("<!doctype html><html><body></body></html>", {
    runScripts: "outside-only",
    url: "http://100.64.0.1:8042/board/b1?key=abc123",
  });
  const w = dom.window;
  w.document.body.innerHTML = html.slice(html.indexOf("<body>") + 6, html.indexOf("<script src="));
  w.fetch = responder;
  w.eval(render);
  w.eval(inline);
  await new Promise((r) => setTimeout(r, 60));
  return w;
}

const ok = (body) => async () => ({ ok: true, status: 200, json: async () => body });

(async () => {
  // 1. A board draws, and nothing in it reaches the parser as markup.
  {
    const w = await boot(ok(BOARD));
    const panes = w.document.getElementById("bdPanes");
    if (panes.querySelectorAll(".dbcard").length !== 2) bad("drew " + panes.querySelectorAll(".dbcard").length + " cards, wanted 2");
    if (panes.querySelector("script") || panes.querySelector("img")) bad("a card's markup reached the DOM");
    if (panes.innerHTML.includes("<script") || panes.innerHTML.includes("<img")) bad("a card's markup was written unescaped");
    if (!panes.innerHTML.includes('<mark class="dbeta">ETA is 15 minutes</mark>')) bad("the ETA was not marked");
    // The name is set with textContent, so markup in it is text, not a node.
    if (w.document.getElementById("bdName").querySelector("script")) bad("the board name reached the parser");
    if (w.document.getElementById("bdFoot").textContent !== BOARD.footer) bad("the footer is missing");
    if (w.document.getElementById("bdStale").style.display !== "none") bad("a board that just arrived is not stale");
  }

  // A board on screen, then the answer changes. Both cases below only happen
  // on a *later* poll, so the test has to wait one out — a single-shot fake
  // server leaves the interesting code unrun, which is how the first version
  // of these two checks passed against a page that did the wrong thing.
  const POLL_MS = 5000;
  async function afterSecondPoll(second) {
    let first = true;
    const w = await boot(async () => {
      if (first) { first = false; return { ok: true, status: 200, json: async () => BOARD }; }
      return second();
    });
    if (w.document.getElementById("bdPanes").querySelectorAll(".dbcard").length !== 2)
      bad("the first board never drew");
    await new Promise((r) => setTimeout(r, POLL_MS + 1200));
    return w;
  }

  // 2. Un-shared means stop. Somebody decided this screen should go dark, so
  //    it goes dark rather than showing a board nobody is sharing any more.
  {
    const w = await afterSecondPoll(async () => ({ ok: false, status: 404 }));
    if (w.document.getElementById("bdGone").style.display === "none") bad("a withdrawn board says nothing");
    if (w.document.getElementById("bdPanes").innerHTML !== "") bad("a withdrawn board is still on screen");
    if (w.document.getElementById("bdName").textContent !== "") bad("a withdrawn board keeps its name");
  }

  // 3. Unreachable is NOT the same as withdrawn. The board stays up — a blank
  //    screen tells a passing clinician nothing — but it must say it is old,
  //    or a 40-minute-old ETA reads as current.
  {
    const w = await afterSecondPoll(async () => { throw new Error("network down"); });
    if (w.document.getElementById("bdPanes").querySelectorAll(".dbcard").length !== 2)
      bad("an unreachable radio blanked the board");
    if (w.document.getElementById("bdGone").style.display !== "none") bad("unreachable was reported as withdrawn");
    // Wind the page's own clock past the threshold and let the ticker run.
    const real = w.Date.now;
    w.Date.now = () => real() + 600000;
    await new Promise((r) => setTimeout(r, 5200));
    w.Date.now = real;
    if (w.document.getElementById("bdStale").style.display === "none") bad("a board that stopped updating does not say so");
    if (w.document.getElementById("bdPanes").querySelectorAll(".dbcard").length !== 2)
      bad("the board was lost while it was stale");
  }

  // 4. The page can reach two URLs and no others, and nothing carries the key
  //    off it.
  {
    const urls = [];
    await boot(async (u) => { urls.push(u); return { ok: true, status: 200, json: async () => BOARD }; });
    if (!urls.every((u) => u.startsWith("/api/board/"))) bad("the page fetched something else: " + urls.join(" "));
    const srcs = [...html.matchAll(/(?:src|href)="([^"]+)"/g)].map((m) => m[1]);
    const allowed = ["/desktop/style.css", "/desktop/board-render.js"];
    for (const s of srcs) if (!allowed.includes(s)) bad("the page loads " + s);
    if (/<a\s/i.test(html)) bad("a link would send the key in a Referer");
    if (!/name="referrer" content="no-referrer"/.test(html)) bad("no referrer policy");
    // Against the code, not the prose: the file's own comments explain which
    // routes it deliberately does not reach.
    const code = html.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "")
      .replace(/<!--[\s\S]*?-->/g, "");
    if (/api\/command|api\/events|api\/audio|api\/file|api\/snapshot/.test(code))
      bad("the page reaches for a route it must not");
  }

  // 5. A cases pane: the timeline draws as lines, the map as a picture the
  //    board carries, and nothing else becomes a picture — not a link, and
  //    not a data address with markup smuggled into it.
  {
    const CASES = Object.assign({}, BOARD, { panes: [{
      id: "p2", kind: "cases", title: "Cardiac arrests", sub: "", width: 1, total: 3,
      cards: [
        { id: 7, at: 990, emoji: "🫀", title: "Cardiac arrest · ROSC", style: "calm", note: "ETA about 19:40", meta: ["1200 Example St", "Medic 7"],
          body: "Expected at Example General: about 19:40\nWitnessed: yes", eta: null,
          lines: ["18:55 Dispatched as Unconscious", "19:14 ROSC <script>alert(4)</script> (dispatcher)"], image: "data:image/png;base64,iVBORw0KGgo=" },
        { id: 8, at: 980, emoji: "🫀", title: "Cardiac arrest · working", style: "alarm", note: "", meta: [], body: "", eta: null,
          lines: ["19:02 Working arrest"], image: "https://example.invalid/track.png" },
        { id: 9, at: 970, emoji: "🫀", title: "Cardiac arrest · dispatched", style: "warn", note: "", meta: [], body: "", eta: null,
          lines: [], image: "data:image/png;base64,AAAA\" onerror=\"alert(5)" },
      ],
    }] });
    const w = await boot(ok(CASES));
    const panes = w.document.getElementById("bdPanes");
    const cards = panes.querySelectorAll(".dbcard");
    if (cards.length !== 3) bad("the cases pane drew " + cards.length + " cards, wanted 3");
    else {
      const img = cards[0].querySelector("img.dbmap");
      if (!img || !img.getAttribute("src").startsWith("data:image/png;base64,")) bad("a case's map was not drawn");
      if (cards[0].querySelectorAll(".dblines li").length !== 2) bad("a case's timeline did not draw as its lines");
      if (cards[1].querySelector("img")) bad("a picture from an address was drawn");
      if (cards[2].querySelector("img") || panes.querySelector("[onerror]")) bad("a data address carrying markup was drawn");
    }
    if (panes.querySelector("script")) bad("a timeline line's markup reached the DOM");
  }

  console.log(failed ? "board: FAILED" : "board: ok");
  process.exit(failed ? 1 : 0);
})();
