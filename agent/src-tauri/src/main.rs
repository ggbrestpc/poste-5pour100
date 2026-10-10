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
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_notification::NotificationExt;
use tokio::sync::Mutex;

const DEFAULT_SERVER: &str = "https://my.5pour100.fr";
const POLL_SECONDS: u64 = 5;

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    server_url: String,
    token: Option<String>,
    device_name: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config { server_url: DEFAULT_SERVER.to_string(), token: None, device_name: None }
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

/// Ouvre un fichier avec l'application par défaut du système.
fn open_path(path: &Path) {
    let p = path.to_string_lossy().to_string();
    #[cfg(target_os = "windows")]
    let _ = Command::new("cmd").args(["/C", "start", "", &p]).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(&p).spawn();
    #[cfg(target_os = "linux")]
    let _ = Command::new("xdg-open").arg(&p).spawn();
}

/// Ouvre une URL dans le navigateur par défaut.
fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = Command::new("xdg-open").arg(url).spawn();
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

fn load_config(app: &AppHandle) -> Config {
    match std::fs::read_to_string(config_path(app)) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

fn save_config(app: &AppHandle, c: &Config) -> Result<(), String> {
    let s = serde_json::to_string_pretty(c).map_err(|e| e.to_string())?;
    std::fs::write(config_path(app), s).map_err(|e| e.to_string())
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

#[tauri::command]
fn get_status(app: AppHandle) -> Status {
    let c = load_config(&app);
    Status { paired: c.token.is_some(), server_url: c.server_url, device_name: c.device_name }
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
                let kind = job.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                let payload = job.get("payload").cloned().unwrap_or(json!({}));
                jobs_total += 1;
                last_action = match kind {
                    "notification" => "Notification reçue",
                    "impression_billet" => "Billet imprimé",
                    "partage_fichier" => "Fichier reçu",
                    "emargement" => "Émargement ouvert",
                    "affichage" => "Affichage à l'écran",
                    _ => "Tâche reçue",
                }
                .to_string();
                match kind {
                    "notification" => {
                        let title = payload.get("title").and_then(|v| v.as_str()).unwrap_or("5pour100");
                        let bodytxt = payload.get("body").and_then(|v| v.as_str()).unwrap_or("");
                        let _ = app.notification().builder().title(title).body(bodytxt).show();
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
                                open_path(&path);
                                let _ = app.notification().builder().title("5pour100 — fichier").body("Fichier reçu et ouvert.").show();
                                acks.push(json!({ "jobId": id, "result": { "ok": true, "path": path.to_string_lossy() } }));
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
                                        open_path(&path);
                                        let _ = app
                                            .notification()
                                            .builder()
                                            .title("5pour100 — support de classe")
                                            .body("Nouveau support reçu et ouvert.")
                                            .show();
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
            open_display
        ])
        .setup(|app| {
            // Icône de barre des tâches : afficher / quitter.
            let show = MenuItem::with_id(app, "show", "Ouvrir", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
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
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("échec du lancement de l'agent Poste 5pour100");
}
