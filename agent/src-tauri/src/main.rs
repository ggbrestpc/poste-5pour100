// Agent de bureau « Poste 5pour100 ».
// Relie l'ordinateur à un espace 5pour100 : appairage par code, puis boucle de synchronisation
// qui récupère les tâches (notifications, impression de billets, …) et les acquitte.
// Le jeton est stocké localement ; il ne quitte jamais la machine.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use keyring::Entry;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::Mutex;

const DEFAULT_SERVER: &str = "https://my.5pour100.fr";
const POLL_SECONDS: u64 = 5;

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    server_url: String,
    token: Option<String>,
    device_name: Option<String>,
    /// Nom de l'utilisateur connecté sur ce poste (le jeton de session reste dans le coffre OS).
    #[serde(default)]
    user_name: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config { server_url: DEFAULT_SERVER.to_string(), token: None, device_name: None, user_name: None }
    }
}

#[derive(Serialize)]
struct Status {
    paired: bool,
    server_url: String,
    device_name: Option<String>,
}

/// Acquittements à envoyer au prochain cycle de synchronisation.
struct AppState {
    acks: Arc<Mutex<Vec<serde_json::Value>>>,
}

fn platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// Télécharge (champ `url`) ou décode (`base64`/`dataBase64`) le contenu d'une tâche vers un fichier
/// temporaire. Une URL relative est complétée avec le serveur et authentifiée par le jeton.
async fn fetch_payload_file(
    client: &reqwest::Client,
    payload: &serde_json::Value,
    token: &str,
    server: &str,
    default_name: &str,
) -> Option<PathBuf> {
    let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or(default_name);
    let safe: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
        .collect();
    let safe = if safe.trim().is_empty() { default_name.to_string() } else { safe };
    let dir = std::env::temp_dir().join("poste-5pour100");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(&safe);

    if let Some(b64) = payload
        .get("base64")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("dataBase64").and_then(|v| v.as_str()))
    {
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        std::fs::write(&path, bytes).ok()?;
        return Some(path);
    }
    if let Some(url) = payload.get("url").and_then(|v| v.as_str()) {
        let internal = !url.starts_with("http");
        let full = if internal { format!("{}{}", server.trim_end_matches('/'), url) } else { url.to_string() };
        let mut req = client.get(&full).timeout(Duration::from_secs(30));
        if internal {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        std::fs::write(&path, &bytes).ok()?;
        return Some(path);
    }
    None
}

/// Extensions de fichiers refusées (exécutables, scripts, raccourcis) : on n'ouvre jamais ces types,
/// même reçus du serveur. Un fichier sans extension n'est pas ouvert automatiquement non plus.
const BLOCKED_EXT: &[&str] = &[
    "exe", "bat", "cmd", "com", "scr", "msi", "msix", "js", "jse", "vbs", "vbe", "ps1", "psm1", "jar",
    "sh", "bash", "lnk", "dll", "cpl", "hta", "wsf", "wsh", "reg", "app", "dmg", "pkg", "deb", "apk",
    "gadget", "iso", "scf", "inf",
];
fn ext_allowed(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => !BLOCKED_EXT.contains(&e.to_lowercase().as_str()),
        None => false,
    }
}

/// Ouvre un fichier avec l'application par défaut, via `opener` (pas de construction de commande shell).
/// Refuse les types dangereux. Renvoie false si le fichier n'a pas été ouvert.
fn open_path(path: &Path) -> bool {
    if !path.is_dir() && !ext_allowed(path) {
        return false;
    }
    opener::open(path).is_ok()
}

/// Ouvre une URL http(s) dans le navigateur par défaut. Les autres schémas sont refusés.
fn open_url(url: &str) {
    if url.starts_with("https://") || url.starts_with("http://") {
        let _ = opener::open_browser(url);
    }
}

/// Impression : tente une impression directe, sinon ouvre le fichier pour impression manuelle.
/// Renvoie true si une impression directe a pu être lancée.
fn print_file(path: &Path) -> bool {
    let p = path.to_string_lossy().to_string();
    #[cfg(target_os = "windows")]
    {
        let cmd = format!("Start-Process -FilePath '{}' -Verb Print", p.replace('\'', "''"));
        if Command::new("powershell").args(["-NoProfile", "-Command", &cmd]).spawn().is_ok() {
            return true;
        }
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        if Command::new("lp").arg(&p).spawn().is_ok() {
            return true;
        }
        if Command::new("lpr").arg(&p).spawn().is_ok() {
            return true;
        }
    }
    open_path(path);
    false
}

fn config_path(app: &AppHandle) -> std::path::PathBuf {
    let dir = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(&dir);
    dir.join("config.json")
}

/* --- Jeton stocké dans le coffre d'identifiants du système (DPAPI/Keychain/Secret Service) --- */
fn token_entry() -> Option<Entry> {
    Entry::new("fr.cinqpourcent.poste", "device-token").ok()
}
fn keyring_get() -> Option<String> {
    token_entry().and_then(|e| e.get_password().ok()).filter(|s| !s.is_empty())
}

/// Jeton de session utilisateur (connexion « vraie Léa »), stocké dans le coffre OS comme le jeton du poste.
fn user_token_entry() -> Option<Entry> {
    Entry::new("fr.cinqpourcent.poste", "user-token").ok()
}
fn user_token_get() -> Option<String> {
    user_token_entry().and_then(|e| e.get_password().ok()).filter(|s| !s.is_empty())
}
fn user_token_set(tok: Option<&str>) {
    if let Some(e) = user_token_entry() {
        match tok {
            Some(t) => {
                let _ = e.set_password(t);
            }
            None => {
                let _ = e.delete_credential();
            }
        }
    }
}
/// Écrit (ou efface) le jeton dans le coffre. Renvoie true si le coffre a accepté l'opération.
fn keyring_set(tok: Option<&str>) -> bool {
    match token_entry() {
        Some(e) => match tok {
            Some(t) => e.set_password(t).is_ok(),
            None => {
                let _ = e.delete_credential();
                true
            }
        },
        None => false,
    }
}

fn load_config(app: &AppHandle) -> Config {
    let mut c = match std::fs::read_to_string(config_path(app)) {
        Ok(s) => serde_json::from_str::<Config>(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    };
    // Priorité au coffre système ; sinon migre un éventuel jeton hérité du fichier vers le coffre.
    if let Some(t) = keyring_get() {
        c.token = Some(t);
    } else if c.token.is_some() {
        let _ = keyring_set(c.token.as_deref());
    }
    c
}

fn save_config(app: &AppHandle, c: &Config) -> Result<(), String> {
    // Le jeton va dans le coffre système. S'il l'accepte, on ne l'écrit PAS en clair dans le fichier.
    let secured = keyring_set(c.token.as_deref());
    let mut to_write = c.clone();
    if secured {
        to_write.token = None;
    }
    let s = serde_json::to_string_pretty(&to_write).map_err(|e| e.to_string())?;
    std::fs::write(config_path(app), s).map_err(|e| e.to_string())
}

fn processed_path(app: &AppHandle) -> std::path::PathBuf {
    let dir = app.path().app_config_dir().unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(&dir);
    dir.join("processed.json")
}
fn load_processed(app: &AppHandle) -> Vec<String> {
    std::fs::read_to_string(processed_path(app))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}
fn save_processed(app: &AppHandle, list: &[String]) {
    if let Ok(s) = serde_json::to_string(list) {
        let _ = std::fs::write(processed_path(app), s);
    }
}

fn trim_url(u: &str) -> String {
    let u = u.trim().trim_end_matches('/');
    if u.is_empty() { DEFAULT_SERVER.to_string() } else { u.to_string() }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Démarrage automatique avec la session.
#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: AppHandle, enable: bool) -> Result<(), String> {
    let m = app.autolaunch();
    if enable { m.enable() } else { m.disable() }.map_err(|e| e.to_string())
}

/// Ouvre le dossier où l'agent dépose les fichiers reçus (billets, partages).
#[tauri::command]
fn open_received_folder() {
    let dir = std::env::temp_dir().join("poste-5pour100");
    let _ = std::fs::create_dir_all(&dir);
    open_path(&dir);
}

/// Ouvre l'écran d'état de l'espace (tableau de bord) dans le navigateur, pour l'afficher sur une TV.
/// Astuce utilisateur : passer ensuite le navigateur en plein écran (F11).
#[tauri::command]
fn open_display(app: AppHandle) {
    let c = load_config(&app);
    let url = format!("{}/ecran", c.server_url.trim_end_matches('/'));
    open_url(&url);
}

/// Ouvre l'espace 5pour100 complet (assistant Léa, gestion, quiz des élèves…) dans le navigateur
/// par défaut. On évite une webview intégrée (page blanche / fenêtre non fermable sous Windows) :
/// le navigateur est fiable et toujours fermable, et la session de l'utilisateur y persiste.
/// `path` facultatif (ex. "/apprendre/...") pour ouvrir directement une page.
#[tauri::command]
fn open_space(app: AppHandle, path: Option<String>) {
    let c = load_config(&app);
    let base = c.server_url.trim_end_matches('/').to_string();
    let p = path.unwrap_or_default();
    let p = if p.starts_with('/') { p } else { String::new() };
    open_url(&format!("{base}{p}"));
}

/// Connexion d'un utilisateur sur ce poste pour obtenir la vraie Léa. Stocke le jeton de session.
/// Renvoie le nom de l'utilisateur. Erreur « 2FA_REQUIS » si un code de validation est attendu.
#[tauri::command]
async fn lea_login(app: AppHandle, email: String, password: String, code: Option<String>) -> Result<String, String> {
    let c = load_config(&app);
    let device = c.token.ok_or("Poste non relié à un espace.")?;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/agent/login", c.server_url.trim_end_matches('/')))
        .bearer_auth(&device)
        .json(&json!({ "email": email.trim(), "password": password, "code": code.unwrap_or_default() }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("Connexion impossible : {e}"))?;
    let ok = resp.status().is_success();
    let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    if !ok {
        if data.get("need2fa").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err("2FA_REQUIS".to_string());
        }
        return Err(data.get("error").and_then(|v| v.as_str()).unwrap_or("Connexion refusée").to_string());
    }
    let tok = data.get("token").and_then(|v| v.as_str()).ok_or("Réponse inattendue du serveur")?.to_string();
    let name = data.get("userName").and_then(|v| v.as_str()).unwrap_or("").to_string();
    user_token_set(Some(&tok));
    let mut cfg = load_config(&app);
    cfg.user_name = if name.is_empty() { None } else { Some(name.clone()) };
    let _ = save_config(&app, &cfg);
    Ok(name)
}

/// Nom de l'utilisateur connecté sur ce poste, ou chaîne vide si personne.
#[tauri::command]
fn lea_me(app: AppHandle) -> String {
    if user_token_get().is_some() {
        load_config(&app).user_name.unwrap_or_default()
    } else {
        String::new()
    }
}

/// Déconnexion de l'utilisateur : invalide la session côté serveur et efface le jeton local.
#[tauri::command]
async fn lea_logout(app: AppHandle) {
    let c = load_config(&app);
    if let Some(ut) = user_token_get() {
        let client = reqwest::Client::new();
        let _ = client
            .delete(format!("{}/api/agent/lea", c.server_url.trim_end_matches('/')))
            .bearer_auth(&ut)
            .timeout(Duration::from_secs(15))
            .send()
            .await;
    }
    user_token_set(None);
    let mut cfg = load_config(&app);
    cfg.user_name = None;
    let _ = save_config(&app, &cfg);
}

/// Vraie Léa, avec les droits de l'utilisateur connecté. Les actions sensibles restent à valider
/// dans l'espace (mentionnées en fin de réponse).
#[tauri::command]
async fn lea_ask_full(app: AppHandle, message: String) -> Result<String, String> {
    let c = load_config(&app);
    let device = c.token.ok_or("Poste non relié.")?;
    let user = user_token_get().ok_or("Non connecté. Connectez-vous pour utiliser la vraie Léa.")?;
    let msg = message.trim().to_string();
    if msg.is_empty() {
        return Err("Écrivez un message.".to_string());
    }
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/agent/lea", c.server_url.trim_end_matches('/')))
        .bearer_auth(&user)
        .header("x-device-token", &device)
        .json(&json!({ "message": msg }))
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("Connexion impossible : {e}"))?;
    let ok = resp.status().is_success();
    let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    if !ok {
        return Err(data.get("error").and_then(|v| v.as_str()).unwrap_or("Erreur").to_string());
    }
    let mut out = data.get("reply").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if let Some(p) = data.get("pending").and_then(|v| v.as_array()) {
        if !p.is_empty() {
            out.push_str(&format!("\n\n({} action(s) à valider dans votre espace.)", p.len()));
        }
    }
    if out.trim().is_empty() {
        out = "—".to_string();
    }
    Ok(out)
}

#[tauri::command]
fn get_status(app: AppHandle) -> Status {
    let c = load_config(&app);
    Status { paired: c.token.is_some(), server_url: c.server_url, device_name: c.device_name }
}

/// Pose une question à Léa (lecture seule) depuis le poste. Réponse à partir des connaissances
/// publiques de l'espace ; authentifiée par le jeton du poste. Aucune donnée privée, aucune action.
#[tauri::command]
async fn ask_lea(app: AppHandle, question: String) -> Result<String, String> {
    let c = load_config(&app);
    let token = c.token.ok_or("Poste non relié à un espace.")?;
    let q = question.trim().to_string();
    if q.is_empty() {
        return Err("Posez une question.".to_string());
    }
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/agent/assistant", c.server_url.trim_end_matches('/')))
        .bearer_auth(&token)
        .json(&json!({ "question": q }))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("Connexion impossible : {e}"))?;
    let ok = resp.status().is_success();
    let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    if !ok {
        return Err(data.get("error").and_then(|v| v.as_str()).unwrap_or("Erreur").to_string());
    }
    Ok(data.get("answer").and_then(|v| v.as_str()).unwrap_or("").to_string())
}

#[tauri::command]
fn set_server(app: AppHandle, url: String) -> Result<(), String> {
    let mut c = load_config(&app);
    c.server_url = trim_url(&url);
    save_config(&app, &c)
}

#[tauri::command]
fn unpair(app: AppHandle) -> Result<(), String> {
    let mut c = load_config(&app);
    c.token = None;
    c.device_name = None;
    save_config(&app, &c)
}

/// Échange un code d'appairage contre un jeton, puis le stocke.
#[tauri::command]
async fn pair(app: AppHandle, code: String, server_url: String, name: String) -> Result<String, String> {
    let server = trim_url(&server_url);
    let name = name.trim().to_string();
    let body = json!({
        "code": code.trim().to_uppercase(),
        "platform": platform(),
        "name": if name.is_empty() { serde_json::Value::Null } else { json!(name) },
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{server}/api/agent/pair"))
        .json(&body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("Connexion impossible : {e}"))?;
    let ok = resp.status().is_success();
    let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    if !ok {
        return Err(data.get("error").and_then(|v| v.as_str()).unwrap_or("Code invalide ou expiré").to_string());
    }
    let token = data.get("token").and_then(|v| v.as_str()).ok_or("Réponse inattendue du serveur")?.to_string();
    let mut c = load_config(&app);
    c.server_url = server;
    c.token = Some(token);
    c.device_name = if name.is_empty() { None } else { Some(name) };
    save_config(&app, &c)?;
    Ok("Poste relié à votre espace.".to_string())
}

/// Ouvre une fenêtre « séance guidée » verrouillée : plein écran, toujours au premier plan, non
/// fermable (l'élève ne peut que suivre le contenu poussé par le formateur). Le verrouillage est au
/// mieux : il empêche de fermer/réduire la fenêtre, pas les raccourcis système bas niveau (Alt+Tab…).
fn open_locked(app: &AppHandle, url: &str, notes: bool) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("diffusion") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let u = url::Url::parse(url).map_err(|e| format!("Adresse invalide : {e}"))?;
    let w = WebviewWindowBuilder::new(app, "diffusion", WebviewUrl::External(u))
        .title("Séance guidée — 5pour100")
        .fullscreen(true)
        .always_on_top(true)
        .closable(false)
        .build()
        .map_err(|e| e.to_string())?;
    let wc = w.clone();
    w.on_window_event(move |e| {
        if let WindowEvent::CloseRequested { api, .. } = e {
            api.prevent_close();
            let _ = wc.set_focus();
        }
    });
    // Bandeau de notes : petite fenêtre locale déplaçable et réductible, au-dessus du contenu.
    if notes && app.get_webview_window("notes").is_none() {
        let _ = WebviewWindowBuilder::new(app, "notes", WebviewUrl::App("notes.html".into()))
            .title("Mes notes")
            .inner_size(360.0, 480.0)
            .min_inner_size(260.0, 200.0)
            .always_on_top(true)
            .resizable(true)
            .minimizable(true)
            .build();
    }
    Ok(())
}

/// Ferme la séance guidée (fin poussée par le formateur) et le bandeau de notes.
fn close_locked(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("diffusion") {
        let _ = w.destroy();
    }
    if let Some(w) = app.get_webview_window("notes") {
        let _ = w.destroy();
    }
}

/// Pousse l'état courant vers l'interface (point vert, dernière synchro, dernière action).
fn emit_status(app: &AppHandle, connected: bool, last_action: &str, jobs_total: u64) {
    let _ = app.emit(
        "status",
        json!({
            "connected": connected,
            "lastSyncMs": now_ms(),
            "lastAction": last_action,
            "jobsTotal": jobs_total,
        }),
    );
}

/// Boucle de fond : synchronise les tâches tant qu'un jeton est présent.
async fn sync_loop(app: AppHandle, state: Arc<Mutex<Vec<serde_json::Value>>>) {
    let client = reqwest::Client::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(POLL_SECONDS));
    let mut jobs_total: u64 = 0;
    // Supports de classe déjà téléchargés (évite de les reprendre à chaque cycle).
    let mut classe_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Tâches déjà exécutées (idempotence), persistées entre les redémarrages.
    let mut processed: Vec<String> = load_processed(&app);
    loop {
        ticker.tick().await;
        let c = load_config(&app);
        let (server, token) = match c.token {
            Some(t) => (c.server_url, t),
            None => continue,
        };
        // Acquittements accumulés au cycle précédent.
        let done: Vec<serde_json::Value> = {
            let mut guard = state.lock().await;
            std::mem::take(&mut *guard)
        };
        let resp = client
            .post(format!("{server}/api/agent/sync"))
            .bearer_auth(&token)
            .json(&json!({ "done": done }))
            .timeout(Duration::from_secs(20))
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(_) => {
                // Réseau indisponible : on remet les acquittements en file pour réessayer.
                let mut guard = state.lock().await;
                guard.splice(0..0, done);
                emit_status(&app, false, "Hors ligne — nouvelle tentative…", jobs_total);
                continue;
            }
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Poste révoqué : on efface le jeton local.
            let mut cfg = load_config(&app);
            cfg.token = None;
            let _ = save_config(&app, &cfg);
            emit_status(&app, false, "Poste déconnecté", jobs_total);
            continue;
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(b) => b,
            Err(_) => {
                emit_status(&app, true, "Réponse illisible du serveur", jobs_total);
                continue;
            }
        };
        let mut last_action = String::from("À l'écoute");
        if let Some(jobs) = body.get("jobs").and_then(|v| v.as_array()) {
            let mut acks = Vec::new();
            for job in jobs {
                let id = job.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if id.is_empty() {
                    continue;
                }
                // Idempotence : si la tâche a déjà été exécutée (ack perdu, renvoi par le serveur),
                // on ne la rejoue pas — on se contente de la ré-acquitter.
                if processed.iter().any(|x| x == &id) {
                    acks.push(json!({ "jobId": id, "result": { "ok": true, "idempotent": true } }));
                    continue;
                }
                let id_done = id.clone();
                let kind = job.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                let payload = job.get("payload").cloned().unwrap_or(json!({}));
                jobs_total += 1;
                last_action = match kind {
                    "notification" => "Notification reçue",
                    "impression_billet" => "Billet imprimé",
                    "partage_fichier" => "Fichier reçu",
                    "emargement" => "Émargement ouvert",
                    "affichage" => "Affichage à l'écran",
                    "diffusion" => "Séance guidée",
                    _ => "Tâche reçue",
                }
                .to_string();
                match kind {
                    "notification" => {
                        let title = payload.get("title").and_then(|v| v.as_str()).unwrap_or("5pour100");
                        let bodytxt = payload.get("body").and_then(|v| v.as_str()).unwrap_or("");
                        let _ = app.notification().builder().title(title).body(bodytxt).show();
                        // Bannière persistante : on remonte la fenêtre de l'agent au premier plan pour que le message soit vu.
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                        // L'interface affiche la bannière (visible même sans son), lit à voix haute et émet un son
                        // selon les réglages du poste et les options du message (persistant, son, répétition).
                        let _ = app.emit("notify", payload.clone());
                        acks.push(json!({ "jobId": id, "result": { "ok": true } }));
                    }
                    "impression_billet" => {
                        match fetch_payload_file(&client, &payload, &token, &server, "billet.pdf").await {
                            Some(path) => {
                                let printed = print_file(&path);
                                let msg = if printed { "Billet envoyé à l'imprimante." } else { "Billet ouvert, prêt à imprimer." };
                                let _ = app.notification().builder().title("5pour100 — billet").body(msg).show();
                                acks.push(json!({ "jobId": id, "result": { "ok": true, "printed": printed } }));
                            }
                            None => acks.push(json!({ "jobId": id, "error": "billet introuvable (champ url ou base64 manquant)" })),
                        }
                    }
                    "partage_fichier" => {
                        match fetch_payload_file(&client, &payload, &token, &server, "fichier").await {
                            Some(path) => {
                                if open_path(&path) {
                                    let _ = app.notification().builder().title("5pour100 — fichier").body("Fichier reçu et ouvert.").show();
                                    acks.push(json!({ "jobId": id, "result": { "ok": true, "path": path.to_string_lossy() } }));
                                } else {
                                    let _ = std::fs::remove_file(&path);
                                    acks.push(json!({ "jobId": id, "error": "type de fichier non autorisé (refusé par l'agent)" }));
                                }
                            }
                            None => acks.push(json!({ "jobId": id, "error": "fichier introuvable" })),
                        }
                    }
                    "emargement" => {
                        // Ouvre la feuille d'émargement (web) sur ce poste.
                        match payload.get("url").and_then(|v| v.as_str()) {
                            Some(url) => {
                                let full = if url.starts_with("http") { url.to_string() } else { format!("{}{}", server.trim_end_matches('/'), url) };
                                open_url(&full);
                                acks.push(json!({ "jobId": id, "result": { "ok": true } }));
                            }
                            None => acks.push(json!({ "jobId": id, "error": "url d'émargement manquante" })),
                        }
                    }
                    "diffusion" => {
                        // Séance guidée : l'élève ne peut que suivre le contenu (quiz, formulaire, vidéo…).
                        // payload.end = true ferme la séance ; sinon payload.url ouvre la fenêtre verrouillée.
                        if payload.get("end").and_then(|v| v.as_bool()).unwrap_or(false) {
                            close_locked(&app);
                            acks.push(json!({ "jobId": id, "result": { "ok": true, "ended": true } }));
                        } else {
                            let u = payload.get("url").and_then(|v| v.as_str()).unwrap_or("");
                            if u.is_empty() {
                                acks.push(json!({ "jobId": id, "error": "url de la séance manquante" }));
                            } else {
                                let full = if u.starts_with("http") { u.to_string() } else { format!("{}{}", server.trim_end_matches('/'), u) };
                                let notes = payload.get("notes").and_then(|v| v.as_bool()).unwrap_or(true);
                                match open_locked(&app, &full, notes) {
                                    Ok(_) => acks.push(json!({ "jobId": id, "result": { "ok": true } })),
                                    Err(e) => acks.push(json!({ "jobId": id, "error": e })),
                                }
                            }
                        }
                    }
                    "affichage" => {
                        // Affiche une page en plein écran sur l'écran du poste (TV d'accueil/salle).
                        // payload.url : adresse interne (/ecran) ou externe. Défaut : écran d'état.
                        let u = payload.get("url").and_then(|v| v.as_str()).unwrap_or("/ecran");
                        let full = if u.starts_with("http") { u.to_string() } else { format!("{}{}", server.trim_end_matches('/'), u) };
                        open_url(&full);
                        acks.push(json!({ "jobId": id, "result": { "ok": true } }));
                    }
                    other => {
                        acks.push(json!({ "jobId": id, "error": format!("type « {other} » non pris en charge par cette version de l'agent") }));
                    }
                }
                // Tâche traitée : on la mémorise pour ne pas la rejouer si le serveur la renvoie.
                processed.push(id_done);
                if processed.len() > 1000 {
                    processed.remove(0);
                }
                save_processed(&app, &processed);
            }
            if !acks.is_empty() {
                let mut guard = state.lock().await;
                guard.extend(acks);
            }
        }

        // Partage de fichiers en classe : récupère automatiquement les supports des partages ouverts
        // et les ouvre sur le poste de la salle. Chaque support n'est téléchargé qu'une fois.
        if let Ok(resp) = client
            .post(format!("{server}/api/agent/classe/shares"))
            .bearer_auth(&token)
            .timeout(Duration::from_secs(20))
            .send()
            .await
        {
            if let Ok(cb) = resp.json::<serde_json::Value>().await {
                if let Some(shares) = cb.get("shares").and_then(|v| v.as_array()) {
                    for share in shares {
                        let supports = share.get("supports").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                        for f in &supports {
                            let fid = f.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            if fid.is_empty() || classe_seen.contains(&fid) {
                                continue;
                            }
                            let got = client
                                .post(format!("{server}/api/agent/classe/file"))
                                .bearer_auth(&token)
                                .json(&json!({ "fileId": fid }))
                                .timeout(Duration::from_secs(60))
                                .send()
                                .await;
                            if let Ok(r) = got {
                                if let Ok(fb) = r.json::<serde_json::Value>().await {
                                    // fetch_payload_file décode le champ dataBase64 vers le dossier des fichiers reçus.
                                    if let Some(path) = fetch_payload_file(&client, &fb, &token, &server, "support").await {
                                        if open_path(&path) {
                                            let _ = app
                                                .notification()
                                                .builder()
                                                .title("5pour100 — support de classe")
                                                .body("Nouveau support reçu et ouvert.")
                                                .show();
                                        } else {
                                            let _ = std::fs::remove_file(&path);
                                        }
                                        classe_seen.insert(fid);
                                        last_action = "Support de classe reçu".to_string();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        emit_status(&app, true, &last_action, jobs_total);
    }
}

/// Vérifie et installe une mise à jour si disponible, puis relance l'agent. Silencieux et tolérant :
/// toute erreur (pas de clé updater, hors ligne, aucune release) est ignorée.
async fn check_update(app: AppHandle) {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(_) => return,
    };
    if let Ok(Some(update)) = updater.check().await {
        let _ = app
            .notification()
            .builder()
            .title("Poste 5pour100")
            .body("Mise à jour en cours d'installation…")
            .show();
        if update.download_and_install(|_, _| {}, || {}).await.is_ok() {
            app.restart();
        }
    }
}

fn main() {
    tauri::Builder::default()
        // Une seule instance : relancer l'agent ramène la fenêtre existante au premier plan.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(AppState { acks: Arc::new(Mutex::new(Vec::new())) })
        .invoke_handler(tauri::generate_handler![
            get_status,
            set_server,
            unpair,
            pair,
            get_autostart,
            set_autostart,
            open_received_folder,
            open_display,
            open_space,
            ask_lea,
            lea_login,
            lea_me,
            lea_logout,
            lea_ask_full
        ])
        .setup(|app| {
            // Icône de barre des tâches : afficher / fermer une séance guidée / quitter.
            let show = MenuItem::with_id(app, "show", "Ouvrir", true, None::<&str>)?;
            let endseance = MenuItem::with_id(app, "endseance", "Fermer la séance guidée", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &endseance, &quit])?;
            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .tooltip("Poste 5pour100")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    // Sortie de secours locale : ferme une séance guidée restée bloquée (contenu non chargé).
                    "endseance" => close_locked(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // Fermer la fenêtre = réduire dans la barre des tâches (l'agent continue en fond).
            if let Some(w) = app.get_webview_window("main") {
                let wc = w.clone();
                w.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = wc.hide();
                    }
                });
            }

            let handle = app.handle().clone();
            let state: State<AppState> = app.state();
            let acks = state.acks.clone();
            tauri::async_runtime::spawn(async move {
                sync_loop(handle, acks).await;
            });

            // Mise à jour automatique au démarrage (silencieuse). Inactive tant que la clé « updater »
            // (plugins.updater.pubkey) n'est pas renseignée : check() échoue alors et on ne fait rien.
            let up = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                check_update(up).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("échec du lancement de l'agent Poste 5pour100");
}
