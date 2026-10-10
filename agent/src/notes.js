const KEY = "poste5-notes";
const ta = document.getElementById("notes");
const state = document.getElementById("state");
try { ta.value = localStorage.getItem(KEY) || ""; } catch {}
let t;
ta.addEventListener("input", () => {
  state.textContent = "…";
  state.className = "";
  clearTimeout(t);
  t = setTimeout(() => {
    try { localStorage.setItem(KEY, ta.value); } catch {}
    state.textContent = "Enregistré";
    state.className = "saved";
  }, 400);
});
document.getElementById("copy").addEventListener("click", async () => {
  try { await navigator.clipboard.writeText(ta.value); state.textContent = "Copié"; state.className = "saved"; } catch {}
});
document.getElementById("clear").addEventListener("click", () => {
  if (confirm("Effacer toutes les notes ?")) { ta.value = ""; try { localStorage.removeItem(KEY); } catch {} ta.focus(); }
});
