const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const $ = (id) => document.getElementById(id);

// ------- Réglages locaux (parole, son, volume) mémorisés sur ce poste -------
const settings = {
  speak: localStorage.getItem("poste.speak") === "1",
  sound: localStorage.getItem("poste.sound") !== "0", // son activé par défaut
  volume: Number(localStorage.getItem("poste.volume") ?? "70"),
};

function saveSettings() {
  localStorage.setItem("poste.speak", settings.speak ? "1" : "0");
  localStorage.setItem("poste.sound", settings.sound ? "1" : "0");
  localStorage.setItem("poste.volume", String(settings.volume));
}

/** Petit son de notification synthétisé (aucun fichier à embarquer). */
function playBeep() {
  if (!settings.sound) return;
  try {
    const Ctx = window.AudioContext || window.webkitAudioContext;
    const ctx = new Ctx();
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    const vol = Math.max(0, Math.min(1, settings.volume / 100)) * 0.3;
    osc.type = "sine";
    osc.frequency.setValueAtTime(880, ctx.currentTime);
    osc.frequency.setValueAtTime(660, ctx.currentTime + 0.12);
    gain.gain.setValueAtTime(vol, ctx.currentTime);
    gain.gain.exponentialRampToValueAtTime(0.0001, ctx.currentTime + 0.35);
    osc.connect(gain);
    gain.connect(ctx.destination);
    osc.start();
    osc.stop(ctx.currentTime + 0.36);
    osc.onended = () => ctx.close();
  } catch {}
}

/** Lecture à voix haute (voix française si disponible). */
function speakText(text) {
  if (!settings.speak || !text || !("speechSynthesis" in window)) return;
  try {
    const u = new SpeechSynthesisUtterance(text);
    u.lang = "fr-FR";
    u.volume = Math.max(0, Math.min(1, settings.volume / 100));
    const fr = speechSynthesis.getVoices().find((v) => v.lang && v.lang.toLowerCase().startsWith("fr"));
    if (fr) u.voice = fr;
    speechSynthesis.cancel();
    speechSynthesis.speak(u);
  } catch {}
}

function onNotify(payload) {
  const title = payload?.title || "5pour100";
  const body = payload?.body || "";
  playBeep();
  speakText(body ? `${title}. ${body}` : title);
}

function initSettingsUI() {
  const sp = $("opt-speak");
  const sd = $("opt-sound");
  const vol = $("opt-volume");
  if (sp) {
    sp.checked = settings.speak;
    sp.addEventListener("change", () => {
      settings.speak = sp.checked;
      saveSettings();
    });
  }
  if (sd) {
    sd.checked = settings.sound;
    sd.addEventListener("change", () => {
      settings.sound = sd.checked;
      saveSettings();
      if (sd.checked) playBeep();
    });
  }
  if (vol) {
    vol.value = String(settings.volume);
    vol.addEventListener("input", () => {
      settings.volume = Number(vol.value);
      saveSettings();
    });
  }
}

let leaUser = ""; // nom de l'utilisateur connecté sur ce poste, "" si personne

async function refreshLeaAuth() {
  try {
    leaUser = (await invoke("lea_me")) || "";
  } catch {
    leaUser = "";
  }
  const logged = !!leaUser;
  $("lea-login")?.classList.toggle("hidden", logged);
  $("lea-account")?.classList.toggle("hidden", !logged);
  if (logged && $("lea-user")) $("lea-user").textContent = leaUser;
  if ($("lea-mode")) $("lea-mode").textContent = logged ? "— votre Léa complète" : "(réponses publiques)";
  if ($("lea-q")) $("lea-q").placeholder = logged ? "Demandez à Léa…" : "Question (infos publiques)…";
}

async function doLeaLogin() {
  const email = $("lea-email").value.trim();
  const password = $("lea-pass").value;
  const code = $("lea-code").value.trim();
  const msg = $("lea-login-msg");
  const btn = $("lea-login-btn");
  if (!email || !password) {
    msg.textContent = "E-mail et mot de passe requis.";
    msg.className = "msg err";
    return;
  }
  btn.disabled = true;
  msg.textContent = "Connexion…";
  msg.className = "msg";
  try {
    await invoke("lea_login", { email, password, code: code || null });
    $("lea-pass").value = "";
    $("lea-code").value = "";
    $("lea-code").classList.add("hidden");
    msg.textContent = "";
    await refreshLeaAuth();
  } catch (e) {
    const err = String(e);
    if (err.includes("2FA_REQUIS")) {
      $("lea-code").classList.remove("hidden");
      $("lea-code").focus();
      msg.textContent = "Entrez votre code de validation (2FA).";
      msg.className = "msg";
    } else {
      msg.textContent = err;
      msg.className = "msg err";
    }
  } finally {
    btn.disabled = false;
  }
}

async function doLeaLogout() {
  try {
    await invoke("lea_logout");
  } catch {}
  await refreshLeaAuth();
}

async function askLea() {
  const q = $("lea-q").value.trim();
  if (!q) return;
  const out = $("lea-answer");
  out.textContent = "Léa réfléchit…";
  try {
    const answer = leaUser
      ? (await invoke("lea_ask_full", { message: q })) || "—"
      : (await invoke("ask_lea", { question: q })) || "—";
    out.textContent = answer;
    speakText(answer); // lecture à voix haute de la réponse (selon le réglage)
  } catch (e) {
    out.textContent = String(e);
  }
}

// ------- Dictée au micro (best-effort : dépend du moteur de la webview) -------
let micRec = null;
function toggleMic() {
  const Ctor = window.SpeechRecognition || window.webkitSpeechRecognition;
  const btn = $("lea-mic");
  if (!Ctor) {
    if (btn) {
      btn.disabled = true;
      btn.title = "Dictée non disponible sur ce poste";
    }
    return;
  }
  if (micRec) {
    micRec.stop();
    return;
  }
  const rec = new Ctor();
  rec.lang = "fr-FR";
  rec.interimResults = true;
  rec.continuous = false;
  let finalText = "";
  rec.onresult = (e) => {
    let interim = "";
    for (let i = e.resultIndex; i < e.results.length; i++) {
      const tr = e.results[i][0].transcript;
      if (e.results[i].isFinal) finalText += tr;
      else interim += tr;
    }
    $("lea-q").value = (finalText + interim).trimStart();
  };
  const stop = () => {
    micRec = null;
    if (btn) btn.classList.remove("primary");
  };
  rec.onend = stop;
  rec.onerror = stop;
  micRec = rec;
  if (btn) btn.classList.add("primary");
  try {
    rec.start();
  } catch {
    stop();
  }
}

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
  $("display-btn").addEventListener("click", () => invoke("open_display"));
  $("space-btn").addEventListener("click", () => invoke("open_space", { path: "/bureau" }).catch((e) => {
    const m = $("conn-msg");
    m.textContent = "Impossible d'ouvrir l'espace : " + String(e);
    m.className = "msg err";
  }));
  $("lea-send")?.addEventListener("click", askLea);
  $("lea-mic")?.addEventListener("click", toggleMic);
  $("lea-q")?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") askLea();
  });
  $("lea-login-btn")?.addEventListener("click", doLeaLogin);
  $("lea-logout-btn")?.addEventListener("click", doLeaLogout);
  $("lea-pass")?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") doLeaLogin();
  });
  $("lea-code")?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") doLeaLogin();
  });
  refreshLeaAuth();
  initSettingsUI();
  $("code").addEventListener("keydown", (e) => {
    if (e.key === "Enter") doPair();
  });
  // Mises à jour en direct émises par la boucle de synchronisation.
  listen("status", (ev) => applyStatus(ev.payload));
  // Notifications : son + lecture vocale selon les réglages.
  listen("notify", (ev) => onNotify(ev.payload));
  refresh();
});
