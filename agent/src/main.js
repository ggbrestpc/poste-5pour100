const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const $ = (id) => document.getElementById(id);

function fmtTime(ms) {
  if (!ms) return "—";
  try {
    return new Date(ms).toLocaleTimeString("fr-FR", { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  } catch {
    return "—";
  }
}

function applyStatus(st) {
  if (!st) return;
  const chip = $("status-chip");
  const txt = $("status-text");
  if (st.connected) {
    chip.classList.remove("offline");
    txt.textContent = "Connecté";
  } else {
    chip.classList.add("offline");
    txt.textContent = "Hors ligne";
  }
  if (st.lastAction) $("last-action").textContent = st.lastAction;
  if (st.lastSyncMs) $("last-sync").textContent = fmtTime(st.lastSyncMs);
  if (typeof st.jobsTotal === "number") $("jobs-total").textContent = String(st.jobsTotal);
}

async function refresh() {
  const s = await invoke("get_status");
  const paired = $("connected-view");
  const pairing = $("pair-view");
  if (s.paired) {
    pairing.classList.add("hidden");
    paired.classList.remove("hidden");
    $("device-name").textContent = s.device_name || "Ce poste";
    $("server-label").textContent = s.server_url;
    try {
      $("autostart").checked = await invoke("get_autostart");
    } catch {}
  } else {
    paired.classList.add("hidden");
    pairing.classList.remove("hidden");
    if (!$("server").value) $("server").value = s.server_url || "https://my.5pour100.fr";
  }
}

async function doPair() {
  const code = $("code").value.trim();
  const name = $("name").value.trim();
  const server = $("server").value.trim() || "https://my.5pour100.fr";
  const msg = $("pair-msg");
  if (code.length < 6) {
    msg.textContent = "Saisissez le code d'appairage.";
    msg.className = "msg err";
    return;
  }
  const btn = $("pair-btn");
  btn.disabled = true;
  msg.textContent = "Connexion…";
  msg.className = "msg";
  try {
    const ok = await invoke("pair", { code, serverUrl: server, name });
    msg.textContent = ok;
    msg.className = "msg ok";
    await refresh();
  } catch (e) {
    msg.textContent = String(e);
    msg.className = "msg err";
  } finally {
    btn.disabled = false;
  }
}

async function doUnpair() {
  await invoke("unpair");
  const m = $("conn-msg");
  m.textContent = "Poste déconnecté.";
  m.className = "msg";
  await refresh();
}

async function toggleAutostart(e) {
  try {
    await invoke("set_autostart", { enable: e.target.checked });
  } catch (err) {
    e.target.checked = !e.target.checked;
    const m = $("conn-msg");
    m.textContent = "Impossible de modifier le démarrage automatique : " + String(err);
    m.className = "msg err";
  }
}

window.addEventListener("DOMContentLoaded", () => {
  $("pair-btn").addEventListener("click", doPair);
  $("unpair-btn").addEventListener("click", doUnpair);
  $("autostart").addEventListener("change", toggleAutostart);
  $("folder-btn").addEventListener("click", () => invoke("open_received_folder"));
  $("code").addEventListener("keydown", (e) => {
    if (e.key === "Enter") doPair();
  });
  // Mises à jour en direct émises par la boucle de synchronisation.
  listen("status", (ev) => applyStatus(ev.payload));
  refresh();
});
