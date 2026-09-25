/* Email: the one mail account every email goes out through.
 *
 * Where each email goes, and what it says, is set beside the Telegram
 * message it goes with — on a tripwire, a case profile, a place. This is
 * only the account. The password goes to the secret store and never comes
 * back to the page.
 */
(() => {
  if (typeof invoke !== "function") return;
  let view = null;

  function draw() {
    if (!view || !$("emHost")) return;
    const s = view.settings;
    $("emOn").checked = !!s.enabled;
    $("emHost").value = s.host || "";
    $("emPort").value = s.port || "";
    $("emSec").value = s.security || "starttls";
    $("emUser").value = s.username || "";
    $("emFromName").value = s.from_name || "";
    $("emFrom").value = s.from_addr || "";
    $("emPass").value = "";
    $("emPwState").textContent = view.has_password ? "saved" : "none saved";
    $("emMeta").textContent = !s.enabled ? "off" : s.host ? `${s.host}` : "not set up";
    if (!$("emTestTo").value) $("emTestTo").value = s.from_addr || s.username || "";
  }

  async function load() {
    try { view = await invoke("email_get"); draw(); } catch (e) { log(`email: ${e}`); }
  }

  async function save() {
    const settings = {
      enabled: $("emOn").checked,
      host: $("emHost").value.trim(),
      port: parseInt($("emPort").value, 10) || 0,
      security: $("emSec").value,
      username: $("emUser").value.trim(),
      from_name: $("emFromName").value.trim(),
      from_addr: $("emFrom").value.trim(),
    };
    const pw = $("emPass").value;
    if (pw) await invoke("email_save_password", { password: pw });
    view = await invoke("email_set", { settings });
    draw();
  }

  if ($("emSave")) {
    $("emSave").onclick = async () => {
      try { await save(); uiToast("Email saved"); $("emSaid").textContent = ""; } catch (e) { uiToast(`${e}`, "err"); }
    };
    $("emTest").onclick = async () => {
      const b = $("emTest"); b.disabled = true; $("emSaid").textContent = "Sending…";
      try {
        await save();
        $("emSaid").textContent = await invoke("email_test", { to: $("emTestTo").value });
      } catch (e) { $("emSaid").textContent = `${e}`; }
      b.disabled = false;
    };
  }
  window.emailSettings = () => (view ? view.settings : null);
  load();
})();
