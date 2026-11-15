// Agent de bureau « Poste 5pour100 ».
// Relie l'ordinateur à un espace 5pour100 : appairage par code, puis boucle de synchronisation
// qui récupère les tâches (notifications, impression de billets, …) et les acquitte.
// Le jeton est stocké localement ; il ne quitte jamais la machine.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod bouclier;
mod dossiers;
mod inventaire;
mod posture;

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
/// Attente maximale entre deux tentatives quand le serveur est injoignable.
const MAX_BACKOFF_SECONDS: u64 = 60;
/// Intervalle entre deux recherches de mise à jour (l'agent reste souvent allumé des semaines).
const UPDATE_EVERY_SECONDS: u64 = 6 * 3600;

/// Attente avant la prochaine synchro : 5 s en temps normal, puis 10, 20, 40, 60 s après des
/// échecs successifs (serveur injoignable), pour ne pas marteler le réseau ni le serveur.
fn next_delay(failures: u32) -> u64 {
    if failures == 0 {
        return POLL_SECONDS;
    }
    POLL_SECONDS.saturating_mul(1u64 << failures.min(10)).min(MAX_BACKOFF_SECONDS)
}

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

/// Adresse envoyée par le serveur : chemin interne (« /… ») ou adresse externe (« https://… »).
#[derive(Debug, PartialEq)]
enum Target {
    /// Page ou fichier du serveur de l'espace : seule cible qui reçoit le jeton du poste.
    Internal(String),
    /// Adresse externe : jamais de jeton.
    External(String),
}

/// Interprète une adresse reçue dans une tâche. Un chemin interne doit commencer par un seul « / » :
/// on refuse « //hôte », « @hôte », « .hôte »… qui, collés derrière l'adresse du serveur, pourraient
/// désigner un autre hôte et lui envoyer le jeton du poste. Le résultat interne est revérifié : même
/// schéma, même hôte et même port que le serveur. Les autres schémas (file:, javascript:…) sont refusés.
fn resolve_target(server: &str, raw: &str) -> Option<Target> {
    let raw = raw.trim();
    if raw.starts_with("https://") || raw.starts_with("http://") {
        let u = url::Url::parse(raw).ok()?;
        return Some(Target::External(u.to_string()));
    }
    if !raw.starts_with('/') || raw.starts_with("//") || raw.contains('\\') {
        return None;
    }
    let base = url::Url::parse(server.trim_end_matches('/')).ok()?;
    let full = url::Url::parse(&format!("{}{}", server.trim_end_matches('/'), raw)).ok()?;
    let same = full.scheme() == base.scheme()
        && full.host_str() == base.host_str()
        && full.port_or_known_default() == base.port_or_known_default();
    if same { Some(Target::Internal(full.to_string())) } else { None }
}

/// Adresse complète à ouvrir (navigateur ou fenêtre), ou None si l'adresse est refusée.
fn target_url(server: &str, raw: &str) -> Option<String> {
    match resolve_target(server, raw)? {
        Target::Internal(u) | Target::External(u) => Some(u),
    }
}

/// Nom de fichier sûr : lettres, chiffres, « . - _ » et espaces, sans « .. » ni point initial.
fn safe_file_name(name: &str, default_name: &str) -> String {
    let safe: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
        .collect();
    let safe = safe.replace("..", ".");
    let safe = safe.trim().trim_start_matches('.').trim().to_string();
    if safe.is_empty() { default_name.to_string() } else { safe }
}

/// Chemin libre dans `dir` : « billet.pdf », sinon « billet (2).pdf », « billet (3).pdf »…
/// Un fichier reçu n'écrase jamais un fichier précédent.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = p.extension().and_then(|s| s.to_str());
    for i in 2..10_000 {
        let candidate = match ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let path = dir.join(candidate);
        if !path.exists() {
            return path;
        }
    }
    first
}

/// Dossier des fichiers reçus : « Téléchargements/5pour100 » de l'utilisateur (privé, facile à
/// retrouver), sinon le dossier de données de l'agent. Plus le dossier temporaire partagé.
fn received_dir(app: &AppHandle) -> PathBuf {
    let dir = app
        .path()
        .download_dir()
        .map(|d| d.join("5pour100"))
        .or_else(|_| app.path().app_data_dir().map(|d| d.join("fichiers-recus")))
        .unwrap_or_else(|_| std::env::temp_dir().join("poste-5pour100"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Télécharge (champ `url`) ou décode (`base64`/`dataBase64`) le contenu d'une tâche vers le dossier
/// des fichiers reçus. Un chemin interne est complété avec le serveur et authentifié par le jeton.
async fn fetch_payload_file(
    client: &reqwest::Client,
    payload: &serde_json::Value,
    token: &str,
    server: &str,
    dir: &Path,
    default_name: &str,
) -> Option<PathBuf> {
    let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or(default_name);
    let path = unique_path(dir, &safe_file_name(name, default_name));

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
        let mut req = match resolve_target(server, url)? {
            Target::Internal(full) => client.get(full).bearer_auth(token),
            Target::External(full) => client.get(full),
        };
        req = req.timeout(Duration::from_secs(30));
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
fn print_file(path: &Path, printer: Option<&str>) -> bool {
    // Jamais d'impression (ni d'ouverture) d'un exécutable ou d'un script, même reçu du serveur.
    if !ext_allowed(path) {
        return false;
    }
    let p = path.to_string_lossy().to_string();
    #[cfg(target_os = "windows")]
    {
        let cmd = match printer {
            Some(name) => format!(
                "Start-Process -FilePath '{}' -Verb PrintTo -ArgumentList '\"{}\"'",
                p.replace('\'', "''"),
                name.replace('\'', "''")
            ),
            None => format!("Start-Process -FilePath '{}' -Verb Print", p.replace('\'', "''")),
        };
        if Command::new("powershell").args(["-NoProfile", "-Command", &cmd]).spawn().is_ok() {
            return true;
        }
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut lp = Command::new("lp");
        if let Some(name) = printer {
            lp.arg("-d").arg(name);
        }
        if lp.arg(&p).spawn().is_ok() {
            return true;
        }
        let mut lpr = Command::new("lpr");
        if let Some(name) = printer {
            lpr.arg("-P").arg(name);
        }
        if lpr.arg(&p).spawn().is_ok() {
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
fn open_received_folder(app: AppHandle) {
    open_path(&received_dir(&app));
}

/// Ouvre l'écran d'état de l'espace (tableau de bord) dans le navigateur, pour l'afficher sur une TV.
/// Astuce utilisateur : passer ensuite le navigateur en plein écran (F11).
#[tauri::command]
fn open_display(app: AppHandle) {
    let c = load_config(&app);
    let url = format!("{}/ecran", c.server_url.trim_end_matches('/'));
    open_url(&url);
}

/// Page ouverte par défaut dans la fenêtre de l'espace : le bureau my5 (toutes les apps, Léa).
const SPACE_HOME: &str = "/bureau";

/// Adresse interne de l'espace pour `path` (« /… » sur le serveur de l'espace), sinon le bureau.
fn space_url(server: &str, path: Option<&str>) -> Option<String> {
    let p = path.filter(|p| p.starts_with('/')).unwrap_or(SPACE_HOME);
    match resolve_target(server, p)? {
        Target::Internal(u) => Some(u),
        Target::External(_) => None,
    }
}

/// Vrai si `u` est sur le serveur de l'espace (même schéma, hôte et port).
fn same_origin(u: &url::Url, server: &url::Url) -> bool {
    u.scheme() == server.scheme()
        && u.host_str() == server.host_str()
        && u.port_or_known_default() == server.port_or_known_default()
}

/// Ouvre l'espace 5pour100 complet (bureau, Léa, toutes les apps) dans une fenêtre de l'application.
/// - Commande ASYNCHRONE : sous Windows, créer une fenêtre depuis une commande synchrone bloque
///   (c'était la cause de la page blanche impossible à fermer de la 0.2.0).
/// - Le site n'a AUCUN accès au poste : les capacités Tauri ne sont accordées qu'à la fenêtre
///   locale « main » (capabilities/default.json), jamais à cette fenêtre distante.
/// - Seul le serveur de l'espace s'affiche ici ; tout autre lien part dans le navigateur.
/// - Chaque utilisateur s'y connecte avec son propre compte : il n'y voit que ses droits.
/// `path` facultatif (ex. "/apprendre/...") pour ouvrir directement une page.
#[tauri::command]
async fn open_space(app: AppHandle, path: Option<String>) -> Result<(), String> {
    let c = load_config(&app);
    let url = space_url(&c.server_url, path.as_deref()).ok_or("Adresse de l'espace refusée.")?;
    open_space_window(&app, &c.server_url, &url, path.is_some())
}

/// Ouvre l'espace dans le navigateur par défaut (secours : connexion par clé d'accès, page à part).
#[tauri::command]
fn open_space_browser(app: AppHandle, path: Option<String>) {
    let c = load_config(&app);
    if let Some(url) = space_url(&c.server_url, path.as_deref()) {
        open_url(&url);
    }
}

fn open_space_window(app: &AppHandle, server: &str, url: &str, navigate: bool) -> Result<(), String> {
    let target = url::Url::parse(url).map_err(|e| format!("Adresse invalide : {e}"))?;
    if let Some(w) = app.get_webview_window("espace") {
        if navigate {
            let _ = w.navigate(target);
        }
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return Ok(());
    }
    let origin = url::Url::parse(server).map_err(|e| format!("Adresse du serveur invalide : {e}"))?;
    let nav_origin = origin.clone();
    let win_origin = origin.clone();
    let downloads = received_dir(app);
    WebviewWindowBuilder::new(app, "espace", WebviewUrl::External(target))
        .title("Mon espace 5pour100")
        .inner_size(1280.0, 820.0)
        .min_inner_size(800.0, 560.0)
        .on_navigation(move |u| {
            if same_origin(u, &nav_origin) || matches!(u.scheme(), "about" | "blob" | "data") {
                return true;
            }
            // Paiement, documentation, sites externes : dans le navigateur, jamais dans l'application.
            open_url(u.as_str());
            false
        })
        .on_new_window(move |u, _features| {
            // Liens « nouvel onglet » : la page de l'espace s'ouvre ici, le reste dans le navigateur.
            if !same_origin(&u, &win_origin) {
                open_url(u.as_str());
            } else if let Some(w) = APP_HANDLE.get().and_then(|a| a.get_webview_window("espace")) {
                let _ = w.navigate(u);
            }
            tauri::webview::NewWindowResponse::Deny
        })
        .on_download(move |_webview, event| {
            if let tauri::webview::DownloadEvent::Requested { url, destination } = event {
                // Téléchargements de l'espace (factures, exports…) : dans Téléchargements/5pour100,
                // sans écraser un fichier existant ; jamais d'exécutable.
                let name = destination
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                    .or_else(|| url.path_segments().and_then(|mut s| s.next_back()).map(str::to_string))
                    .unwrap_or_default();
                let path = unique_path(&downloads, &safe_file_name(&name, "document"));
                if !ext_allowed(&path) {
                    return false;
                }
                *destination = path;
            }
            true
        })
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Accès global à l'application pour les rappels de fenêtre qui n'en reçoivent pas.
static APP_HANDLE: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();

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

/// Oublie l'utilisateur connecté sur ce poste (jeton local + nom), sans appel serveur.
/// Utilisé quand le serveur répond 401 (session expirée ou révoquée) ou que le poste est délié.
fn lea_forget(app: &AppHandle) {
    user_token_set(None);
    let mut cfg = load_config(app);
    if cfg.user_name.is_some() {
        cfg.user_name = None;
        let _ = save_config(app, &cfg);
    }
}

/// Marqueur d'erreur reconnu par l'interface : session expirée → écran de connexion.
const SESSION_EXPIREE: &str = "SESSION_EXPIREE";

/// Vérifie la session de l'utilisateur auprès du serveur (et la prolonge : expiration glissante).
/// Renvoie le nom de l'utilisateur, "" si personne n'est connecté, ou l'erreur SESSION_EXPIREE
/// (jeton local effacé) si le serveur la refuse. Hors ligne : on garde l'état local.
#[tauri::command]
async fn lea_check(app: AppHandle) -> Result<String, String> {
    let c = load_config(&app);
    let Some(user) = user_token_get() else { return Ok(String::new()) };
    let Some(device) = c.token.clone() else {
        lea_forget(&app);
        return Err(SESSION_EXPIREE.to_string());
    };
    let resp = reqwest::Client::new()
        .get(format!("{}/api/agent/lea", c.server_url.trim_end_matches('/')))
        .bearer_auth(&user)
        .header("x-device-token", &device)
        .timeout(Duration::from_secs(20))
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(_) => return Ok(c.user_name.unwrap_or_default()),
    };
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        lea_forget(&app);
        return Err(SESSION_EXPIREE.to_string());
    }
    if !resp.status().is_success() {
        return Ok(c.user_name.unwrap_or_default());
    }
    let data: serde_json::Value = resp.json().await.unwrap_or_default();
    let name = data.get("userName").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !name.is_empty() && c.user_name.as_deref() != Some(name.as_str()) {
        let mut cfg = load_config(&app);
        cfg.user_name = Some(name.clone());
        let _ = save_config(&app, &cfg);
    }
    Ok(if name.is_empty() { c.user_name.unwrap_or_default() } else { name })
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
/// Taille maximale d'un fichier partagé (identique à la limite de l'espace).
const SHARE_MAX_BYTES: u64 = 15 * 1024 * 1024;

/// Partage choisi : la personne choisit elle-même un fichier sur cet ordinateur (fenêtre du système)
/// et le partage avec son espace, à son nom (session de son compte), pour elle seule ou pour l'équipe.
/// L'espace ne peut rien lire d'autre sur le poste.
#[tauri::command]
async fn share_file(app: AppHandle, visibilite: String) -> Result<serde_json::Value, String> {
    use tauri_plugin_dialog::DialogExt;
    let vis = if visibilite == "equipe" { "equipe" } else { "moi" };
    let c = load_config(&app);
    let device = c.token.ok_or("Poste non relié.")?;
    let user = user_token_get().ok_or("Connectez-vous avec votre compte (section Léa) pour partager un fichier à votre nom.")?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().set_title("Fichier à partager avec mon espace").pick_file(move |f| {
        let _ = tx.send(f);
    });
    let Some(picked) = rx.await.map_err(|_| "Fenêtre de choix fermée.".to_string())? else {
        return Ok(json!({ "ok": false, "cancelled": true }));
    };
    let path = picked.into_path().map_err(|_| "Ce fichier ne peut pas être lu.".to_string())?;
    let meta = std::fs::metadata(&path).map_err(|e| format!("Lecture impossible : {e}"))?;
    if !meta.is_file() {
        return Err("Choisissez un fichier.".into());
    }
    if meta.len() == 0 || meta.len() > SHARE_MAX_BYTES {
        return Err("Le fichier doit faire entre 1 octet et 15 Mo.".into());
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("Lecture impossible : {e}"))?;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "fichier".into());
    let body = json!({
        "name": name,
        "dataBase64": base64::engine::general_purpose::STANDARD.encode(&bytes),
        "visibilite": vis,
    });
    let resp = reqwest::Client::new()
        .post(format!("{}/api/agent/partage", c.server_url.trim_end_matches('/')))
        .bearer_auth(&user)
        .header("x-device-token", &device)
        .json(&body)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("Connexion impossible : {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        lea_forget(&app);
        return Err(SESSION_EXPIREE.to_string());
    }
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err("Votre espace n'accepte pas encore les partages depuis l'agent (mise à jour en cours).".into());
    }
    let ok = resp.status().is_success();
    let data: serde_json::Value = resp.json().await.unwrap_or(json!({}));
    if !ok {
        return Err(data.get("error").and_then(|v| v.as_str()).unwrap_or("Partage refusé").to_string());
    }
    Ok(json!({ "ok": true, "name": name, "visibilite": vis }))
}

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
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        // Session expirée ou révoquée : on efface le jeton local, l'interface repasse en « déconnecté ».
        lea_forget(&app);
        return Err(SESSION_EXPIREE.to_string());
    }
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
    lea_forget(&app);
    // Poste délié : les dossiers connectés sont oubliés (l'espace les coupe aussi quand il révoque le poste).
    let _ = std::fs::remove_file(dossiers_path(&app));
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
    let mut jobs_total: u64 = 0;
    // Échecs réseau consécutifs : allongent l'attente entre deux tentatives.
    let mut failures: u32 = 0;
    // Supports de classe déjà téléchargés (évite de les reprendre à chaque cycle).
    let mut classe_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Tâches déjà exécutées (idempotence), persistées entre les redémarrages.
    let mut processed: Vec<String> = load_processed(&app);
    loop {
        tokio::time::sleep(Duration::from_secs(next_delay(failures))).await;
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
                failures = failures.saturating_add(1);
                let wait = next_delay(failures);
                emit_status(&app, false, &format!("Hors ligne — nouvelle tentative dans {wait} s"), jobs_total);
                continue;
            }
        };
        failures = 0;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Poste révoqué : on efface le jeton local (et la session utilisateur liée au poste).
            lea_forget(&app);
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
                    "bouclier" => "Demande Bouclier",
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
                        match fetch_payload_file(&client, &payload, &token, &server, &received_dir(&app), "billet.pdf").await {
                            Some(path) => {
                                // Imprimante choisie dans l'espace : utilisée seulement si elle est bien installée ici.
                                let wanted = payload.get("printer").and_then(|v| v.as_str()).map(str::to_string);
                                let printer = match wanted {
                                    Some(w) if inventaire::safe_printer_name(&w) => {
                                        let w2 = w.clone();
                                        let installed = tauri::async_runtime::spawn_blocking(move || inventaire::printers().iter().any(|p| p.name == w2))
                                            .await
                                            .unwrap_or(false);
                                        installed.then_some(w)
                                    }
                                    _ => None,
                                };
                                let printed = print_file(&path, printer.as_deref());
                                let msg = if printed { "Billet envoyé à l'imprimante." } else { "Billet ouvert, prêt à imprimer." };
                                let _ = app.notification().builder().title("5pour100 — billet").body(msg).show();
                                acks.push(json!({ "jobId": id, "result": { "ok": true, "printed": printed, "printer": printer } }));
                            }
                            None => acks.push(json!({ "jobId": id, "error": "billet introuvable (champ url ou base64 manquant)" })),
                        }
                    }
                    "partage_fichier" => {
                        match fetch_payload_file(&client, &payload, &token, &server, &received_dir(&app), "fichier").await {
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
                            Some(url) => match target_url(&server, url) {
                                Some(full) => {
                                    open_url(&full);
                                    acks.push(json!({ "jobId": id, "result": { "ok": true } }));
                                }
                                None => acks.push(json!({ "jobId": id, "error": "adresse d'émargement refusée" })),
                            },
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
                                let notes = payload.get("notes").and_then(|v| v.as_bool()).unwrap_or(true);
                                match target_url(&server, u) {
                                    Some(full) => match open_locked(&app, &full, notes) {
                                        Ok(_) => acks.push(json!({ "jobId": id, "result": { "ok": true } })),
                                        Err(e) => acks.push(json!({ "jobId": id, "error": e })),
                                    },
                                    None => acks.push(json!({ "jobId": id, "error": "adresse de la séance refusée" })),
                                }
                            }
                        }
                    }
                    "affichage" => {
                        // Affiche une page en plein écran sur l'écran du poste (TV d'accueil/salle).
                        // payload.url : adresse interne (/ecran) ou externe. Défaut : écran d'état.
                        let u = payload.get("url").and_then(|v| v.as_str()).unwrap_or("/ecran");
                        match target_url(&server, u) {
                            Some(full) => {
                                open_url(&full);
                                acks.push(json!({ "jobId": id, "result": { "ok": true } }));
                            }
                            None => acks.push(json!({ "jobId": id, "error": "adresse d'affichage refusée" })),
                        }
                    }
                    "bouclier" => match bouclier::parse_request(&payload) {
                        Ok(req) => {
                            let mut saved = load_bouclier(&app);
                            if req.action == "retirer" && saved.applied.is_none() {
                                // Rien n'a été réglé sur ce poste : le retrait est immédiat.
                                saved.pending = None;
                                save_bouclier(&app, &saved);
                                let a = app.clone();
                                tauri::async_runtime::spawn(async move {
                                    let _ = report_bouclier(&a, "retire", None).await;
                                });
                            } else {
                                saved.pending = Some(req);
                                save_bouclier(&app, &saved);
                                let _ = app.notification().builder().title("5pour100 — Bouclier").body("Votre espace demande votre accord dans l'agent (Sécurité de ce poste).").show();
                                if let Some(w) = app.get_webview_window("main") {
                                    let _ = w.show();
                                    let _ = w.unminimize();
                                    let _ = w.set_focus();
                                }
                            }
                            let _ = app.emit("bouclier", bouclier_view(&saved));
                            acks.push(json!({ "jobId": id, "result": { "ok": true, "pending": saved.pending.is_some() } }));
                        }
                        Err(e) => acks.push(json!({ "jobId": id, "error": e })),
                    },
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
                                    if let Some(path) = fetch_payload_file(&client, &fb, &token, &server, &received_dir(&app), "support").await {
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

/// Intervalle entre deux bilans de sécurité envoyés à l'espace.
const POSTURE_EVERY_SECONDS: u64 = 24 * 3600;

/// Fait le bilan de sécurité du poste (lecture seule ; chaque commande est tuée après 20 s) et
/// l'envoie à l'espace si le poste est relié. Le bilan est aussi affiché dans la fenêtre de l'agent
/// (évènement « posture »).
async fn report_posture(app: &AppHandle) -> Result<serde_json::Value, String> {
    let collected = tauri::async_runtime::spawn_blocking(posture::collect).await.map_err(|e| e.to_string())?;
    let (checks, os) = collected;
    let body = json!({ "agentVersion": env!("CARGO_PKG_VERSION"), "os": os, "checks": checks });
    let _ = app.emit("posture", body.clone());
    let c = load_config(app);
    if let Some(token) = c.token {
        let resp = reqwest::Client::new()
            .post(format!("{}/api/agent/securite", c.server_url.trim_end_matches('/')))
            .bearer_auth(&token)
            .json(&body)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| format!("envoi impossible : {e}"))?;
        // 404 : espace pas encore à jour (route absente) ; on garde le bilan local sans erreur.
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(format!("envoi refusé ({})", resp.status()));
        }
    }
    Ok(body)
}

/// Envoie la liste des imprimantes du poste à l'espace (lecture seule).
async fn report_inventaire(app: &AppHandle) -> Result<(), String> {
    let printers = tauri::async_runtime::spawn_blocking(inventaire::printers).await.map_err(|e| e.to_string())?;
    let c = load_config(app);
    let Some(token) = c.token else { return Ok(()) };
    reqwest::Client::new()
        .post(format!("{}/api/agent/inventaire", c.server_url.trim_end_matches('/')))
        .bearer_auth(&token)
        .json(&json!({ "printers": printers }))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("envoi impossible : {e}"))?;
    Ok(())
}

/// Bilan au démarrage (après une minute, pour ne pas ralentir l'ouverture de session) puis chaque jour.
async fn posture_loop(app: AppHandle) {
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        let _ = report_posture(&app).await;
        let _ = report_inventaire(&app).await;
        // Profil Bouclier installé plus tard (macOS) : constaté au bilan suivant.
        let _ = bouclier_verify(&app).await;
        tokio::time::sleep(Duration::from_secs(POSTURE_EVERY_SECONDS)).await;
    }
}

/* ------------------------------ Dossiers connectés ------------------------------ */

/// Un seul accès à la fois à dossiers.json (boucle d'envoi et commandes de l'interface).
static DOSSIERS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// Envoi toutes les minutes ; un fichier doit être stable depuis 15 s ; au plus 10 envois par dossier et par passage.
const DOSSIERS_EVERY_SECONDS: u64 = 60;
const DOSSIERS_SETTLE_SECONDS: u64 = 15;
const DOSSIERS_MAX_PER_PASS: usize = 10;

fn dossiers_path(app: &AppHandle) -> PathBuf {
    config_path(app).with_file_name("dossiers.json")
}

fn load_dossiers(app: &AppHandle) -> dossiers::Saved {
    std::fs::read_to_string(dossiers_path(app)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_dossiers(app: &AppHandle, s: &dossiers::Saved) {
    if let Ok(txt) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(dossiers_path(app), txt);
    }
}

fn dossiers_view(s: &dossiers::Saved) -> serde_json::Value {
    json!(s
        .items
        .iter()
        .map(|d| json!({
            "id": d.id,
            "label": d.label,
            "path": d.path.to_string_lossy(),
            "visibilite": d.visibilite,
            "sent": d.sent,
            "lastError": d.last_error,
        }))
        .collect::<Vec<_>>())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[tauri::command]
async fn dossiers_list(app: AppHandle) -> serde_json::Value {
    // Lecture seule, sans attendre un envoi en cours.
    dossiers_view(&load_dossiers(&app))
}

/// Connexion d'un dossier : la personne le choisit dans la fenêtre du système. Les fichiers déjà
/// présents ne sont pas envoyés ; seuls ceux qui arrivent ensuite le sont, à son nom.
#[tauri::command]
async fn dossier_connect(app: AppHandle, visibilite: String) -> Result<serde_json::Value, String> {
    use tauri_plugin_dialog::DialogExt;
    let vis = if visibilite == "equipe" { "equipe" } else { "moi" };
    let c = load_config(&app);
    let device = c.token.ok_or("Poste non relié.")?;
    let user = user_token_get().ok_or("Connectez-vous avec votre compte (section Léa) pour connecter un dossier à votre nom.")?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().set_title("Dossier à connecter à mon espace").pick_folder(move |f| {
        let _ = tx.send(f);
    });
    let Some(picked) = rx.await.map_err(|_| "Fenêtre de choix fermée.".to_string())? else {
        return Ok(json!({ "ok": false, "cancelled": true }));
    };
    let path = picked.into_path().map_err(|_| "Ce dossier ne peut pas être lu.".to_string())?;
    let snap = dossiers::snapshot(&path).map_err(|e| format!("Lecture du dossier impossible : {e}"))?;
    let label: String = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Dossier".into()).chars().take(80).collect();
    {
        let _g = DOSSIERS_LOCK.lock().await;
        if load_dossiers(&app).items.iter().any(|d| d.path == path) {
            return Err("Ce dossier est déjà connecté.".into());
        }
    }
    let resp = reqwest::Client::new()
        .post(format!("{}/api/agent/connexions", c.server_url.trim_end_matches('/')))
        .bearer_auth(&user)
        .header("x-device-token", &device)
        .json(&json!({ "label": label, "visibilite": vis }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("Connexion impossible : {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        lea_forget(&app);
        return Err(SESSION_EXPIREE.to_string());
    }
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err("Votre espace n'accepte pas encore les dossiers connectés (mise à jour en cours).".into());
    }
    let ok = resp.status().is_success();
    let data: serde_json::Value = resp.json().await.unwrap_or(json!({}));
    if !ok {
        return Err(data.get("error").and_then(|v| v.as_str()).unwrap_or("Connexion refusée").to_string());
    }
    let id = data.get("id").and_then(|v| v.as_str()).ok_or("Réponse inattendue du serveur")?.to_string();
    let _g = DOSSIERS_LOCK.lock().await;
    let mut s = load_dossiers(&app);
    s.items.push(dossiers::Dossier { id, label: label.clone(), path, visibilite: vis.into(), seen: snap, sent: 0, last_error: None });
    save_dossiers(&app, &s);
    Ok(json!({ "ok": true, "label": label, "dossiers": dossiers_view(&s) }))
}

/// Déconnexion depuis le poste : oublié ici tout de suite, et coupé dans l'espace si possible.
#[tauri::command]
async fn dossier_disconnect(app: AppHandle, id: String) -> Result<serde_json::Value, String> {
    let c = load_config(&app);
    let mut note = None;
    if let (Some(device), Some(user)) = (c.token.clone(), user_token_get()) {
        let r = reqwest::Client::new()
            .delete(format!("{}/api/agent/connexions/{}", c.server_url.trim_end_matches('/'), id))
            .bearer_auth(&user)
            .header("x-device-token", &device)
            .timeout(Duration::from_secs(20))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() || r.status() == reqwest::StatusCode::GONE => {}
            _ => note = Some("Dossier oublié sur ce poste. S'il apparaît encore dans l'espace, déconnectez-le aussi depuis la page Agent de bureau."),
        }
    } else {
        note = Some("Dossier oublié sur ce poste. S'il apparaît encore dans l'espace, déconnectez-le aussi depuis la page Agent de bureau.");
    }
    let _g = DOSSIERS_LOCK.lock().await;
    let mut s = load_dossiers(&app);
    s.items.retain(|d| d.id != id);
    save_dossiers(&app, &s);
    Ok(json!({ "ok": true, "note": note, "dossiers": dossiers_view(&s) }))
}

/// Issue de l'envoi d'un fichier.
enum Envoi {
    Ok,
    /// Refus définitif pour ce fichier (antivirus, taille…) : on ne le renvoie pas.
    Refus(String),
    /// Connexion coupée dans l'espace : le dossier est oublié.
    Coupe,
    /// Session expirée : plus aucun envoi jusqu'à la reconnexion.
    Session,
    /// Réseau ou serveur indisponible : on réessaiera au passage suivant.
    Plus,
}

async fn dossier_push(server: &str, user: &str, device: &str, id: &str, name: &str, bytes: &[u8]) -> Envoi {
    let body = json!({
        "name": name,
        "mime": dossiers::mime_for(name),
        "dataBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
    });
    let r = reqwest::Client::new()
        .post(format!("{server}/api/agent/connexions/{id}"))
        .bearer_auth(user)
        .header("x-device-token", device)
        .json(&body)
        .timeout(Duration::from_secs(120))
        .send()
        .await;
    let Ok(r) = r else { return Envoi::Plus };
    match r.status().as_u16() {
        200..=299 => Envoi::Ok,
        401 => Envoi::Session,
        410 => Envoi::Coupe,
        400 | 403 | 413 => {
            let d: serde_json::Value = r.json().await.unwrap_or(json!({}));
            Envoi::Refus(d.get("error").and_then(|v| v.as_str()).unwrap_or("Envoi refusé").to_string())
        }
        _ => Envoi::Plus,
    }
}

/// Un passage : liste les connexions de la personne connectée, oublie celles coupées, envoie les nouveaux fichiers.
async fn dossiers_pass(app: &AppHandle) {
    let _g = DOSSIERS_LOCK.lock().await;
    let mut s = load_dossiers(app);
    if s.items.is_empty() {
        return;
    }
    let c = load_config(app);
    let (Some(device), Some(user)) = (c.token.clone(), user_token_get()) else { return };
    let server = c.server_url.trim_end_matches('/').to_string();
    let known = s.items.iter().map(|d| d.id.clone()).collect::<Vec<_>>().join(",");
    let Ok(r) = reqwest::Client::new()
        .get(format!("{server}/api/agent/connexions"))
        .query(&[("known", known)])
        .bearer_auth(&user)
        .header("x-device-token", &device)
        .timeout(Duration::from_secs(20))
        .send()
        .await
    else {
        return;
    };
    if r.status() == reqwest::StatusCode::UNAUTHORIZED {
        lea_forget(app);
        return;
    }
    if !r.status().is_success() {
        return;
    }
    let data: serde_json::Value = r.json().await.unwrap_or(json!({}));
    let ids = |k: &str| -> Vec<String> {
        data.get(k)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.get("id").and_then(|i| i.as_str()).or_else(|| x.as_str()).map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let gone = ids("gone");
    // Les dossiers d'une autre personne restent en sommeil : seuls ceux de la personne connectée sont envoyés.
    let mine = ids("connexions");
    s.items.retain(|d| !gone.contains(&d.id));

    let mut stop = false;
    let mut cut: Vec<String> = vec![];
    for d in s.items.iter_mut().filter(|d| mine.contains(&d.id)) {
        if stop {
            break;
        }
        let snap = match dossiers::snapshot(&d.path) {
            Ok(x) => x,
            Err(_) => {
                d.last_error = Some("Dossier introuvable sur ce poste (déplacé ou supprimé ?).".into());
                continue;
            }
        };
        for name in dossiers::candidates(&d.seen, &snap, now_secs(), DOSSIERS_SETTLE_SECONDS, DOSSIERS_MAX_PER_PASS) {
            let seen = snap[&name];
            let p = d.path.join(&name);
            if !ext_allowed(&p) {
                d.seen.insert(name, seen);
                continue;
            }
            if seen.len > SHARE_MAX_BYTES {
                d.last_error = Some(format!("« {name} » dépasse 15 Mo : non envoyé."));
                d.seen.insert(name, seen);
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else { continue };
            match dossier_push(&server, &user, &device, &d.id, &name, &bytes).await {
                Envoi::Ok => {
                    d.sent += 1;
                    d.last_error = None;
                    d.seen.insert(name, seen);
                }
                Envoi::Refus(e) => {
                    d.last_error = Some(format!("« {name} » : {e}"));
                    d.seen.insert(name, seen);
                }
                Envoi::Coupe => {
                    cut.push(d.id.clone());
                    break;
                }
                Envoi::Session => {
                    stop = true;
                    break;
                }
                Envoi::Plus => break,
            }
        }
        dossiers::prune(&mut d.seen, &snap);
    }
    s.items.retain(|d| !cut.contains(&d.id));
    save_dossiers(app, &s);
    if stop {
        lea_forget(app);
    }
    let _ = app.emit("dossiers", dossiers_view(&s));
}

async fn dossiers_loop(app: AppHandle) {
    tokio::time::sleep(Duration::from_secs(30)).await;
    loop {
        dossiers_pass(&app).await;
        tokio::time::sleep(Duration::from_secs(DOSSIERS_EVERY_SECONDS)).await;
    }
}

/* ------------------------------ Bouclier en un clic ------------------------------ */

fn bouclier_path(app: &AppHandle) -> PathBuf {
    config_path(app).with_file_name("bouclier.json")
}

fn load_bouclier(app: &AppHandle) -> bouclier::Saved {
    std::fs::read_to_string(bouclier_path(app)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_bouclier(app: &AppHandle, s: &bouclier::Saved) {
    if let Ok(txt) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(bouclier_path(app), txt);
    }
}

fn current_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

fn bouclier_view(s: &bouclier::Saved) -> serde_json::Value {
    json!({
        "pending": s.pending.as_ref().map(|p| json!({ "action": p.action })),
        "applied": s.applied.is_some(),
        "verified": s.applied.as_ref().map(|a| a.verified).unwrap_or(false),
        "os": current_os(),
    })
}

/// Hôte de l'espace : il doit rester joignable une fois le Bouclier en place (témoin).
fn witness_host(server: &str) -> String {
    url::Url::parse(server).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_else(|| "my.5pour100.fr".into())
}

/// Déclare à l'espace l'issue du Bouclier sur CE poste (jeton du poste).
async fn report_bouclier(app: &AppHandle, state: &str, detail: Option<String>) -> Result<(), String> {
    let c = load_config(app);
    let Some(token) = c.token else { return Ok(()) };
    reqwest::Client::new()
        .post(format!("{}/api/agent/bouclier", c.server_url.trim_end_matches('/')))
        .bearer_auth(&token)
        .json(&json!({ "state": state, "detail": detail }))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("envoi impossible : {e}"))?;
    Ok(())
}

/// Constate la protection (lecture seule) et la déclare une fois établie.
async fn bouclier_verify(app: &AppHandle) -> bool {
    let saved = load_bouclier(app);
    let Some(applied) = saved.applied.clone() else { return false };
    if applied.verified {
        return true;
    }
    let witness = witness_host(&load_config(app).server_url);
    let probe = applied.probe_host.clone();
    let ok = tauri::async_runtime::spawn_blocking(move || bouclier::is_protected(&witness, &probe)).await.ok().flatten() == Some(true);
    if ok {
        let mut s = load_bouclier(app);
        if let Some(a) = s.applied.as_mut() {
            a.verified = true;
        }
        save_bouclier(app, &s);
        let _ = report_bouclier(app, "actif", None).await;
        let _ = app.emit("bouclier", bouclier_view(&s));
    }
    ok
}

#[tauri::command]
fn bouclier_status(app: AppHandle) -> serde_json::Value {
    bouclier_view(&load_bouclier(&app))
}

/// « Accepter » dans l'agent : applique (ou retire) le Bouclier avec l'accord du système.
#[tauri::command]
async fn bouclier_accept(app: AppHandle) -> Result<serde_json::Value, String> {
    let mut saved = load_bouclier(&app);
    let Some(req) = saved.pending.clone() else { return Ok(json!({ "status": bouclier_view(&saved) })) };
    let message;
    if req.action == "retirer" {
        let applied = saved.applied.clone();
        let res = match applied {
            Some(a) => tauri::async_runtime::spawn_blocking(move || bouclier::remove(&a)).await.map_err(|e| e.to_string())?,
            None => Ok(()),
        };
        match res {
            Ok(()) => {
                saved = bouclier::Saved::default();
                let _ = report_bouclier(&app, "retire", None).await;
                message = "Réglages d'origine remis.".to_string();
            }
            Err(bouclier::Outcome::Refused) => message = "Accord administrateur refusé : rien n'a changé.".to_string(),
            Err(bouclier::Outcome::Error(e)) => message = e,
        }
    } else {
        let witness = witness_host(&load_config(&app).server_url);
        let dir = received_dir(&app);
        let r = req.clone();
        let prev = saved.applied.clone();
        let a2 = app.clone();
        let prev2 = prev.clone();
        let res = tauri::async_runtime::spawn_blocking(move || {
            // Sauvegarde écrite AVANT la modification du système, et relue pour s'en assurer.
            let persist = |applied: &bouclier::Applied| {
                let mut s = load_bouclier(&a2);
                s.pending = None;
                s.applied = Some(applied.clone());
                save_bouclier(&a2, &s);
                load_bouclier(&a2).applied.as_ref() == Some(applied)
            };
            bouclier::apply(&r, &witness, &dir, prev2.as_ref(), &persist)
        })
        .await
        .map_err(|e| e.to_string())?;
        saved.pending = None;
        // Refus ou échec : la sauvegarde d'avant reste la référence (les scripts remettent l'origine).
        saved.applied = prev;
        let ok_verified = res.as_ref().map(|a| a.verified).ok();
        if let Ok(applied) = &res {
            saved.applied = Some(applied.clone());
        }
        // État enregistré tout de suite, avant l'envoi à l'espace (qui peut prendre 20 s).
        save_bouclier(&app, &saved);
        match res {
            Ok(_) => {
                let verified = ok_verified.unwrap_or(false);
                if verified {
                    let _ = report_bouclier(&app, "actif", None).await;
                    message = "Bouclier actif sur ce poste.".to_string();
                } else {
                    message = "Profil ouvert : installez « CINQ Bouclier » dans Réglages Système → Profils.".to_string();
                    let a = app.clone();
                    tauri::async_runtime::spawn(async move {
                        // Le profil s'installe à la main : on constate pendant 15 minutes.
                        for _ in 0..60 {
                            tokio::time::sleep(Duration::from_secs(15)).await;
                            if bouclier_verify(&a).await {
                                break;
                            }
                        }
                    });
                }
            }
            Err(bouclier::Outcome::Refused) => {
                let _ = report_bouclier(&app, "refuse", Some("Accord administrateur refusé sur le poste".into())).await;
                message = "Accord administrateur refusé : rien n'a changé.".to_string();
            }
            Err(bouclier::Outcome::Error(e)) => {
                let _ = report_bouclier(&app, "erreur", Some(e.chars().take(300).collect())).await;
                message = e;
            }
        }
    }
    save_bouclier(&app, &saved);
    let view = bouclier_view(&saved);
    let _ = app.emit("bouclier", view.clone());
    Ok(json!({ "status": view, "message": message }))
}

/// « Refuser » dans l'agent : rien n'est changé, l'espace en est informé.
#[tauri::command]
async fn bouclier_refuse(app: AppHandle) -> Result<serde_json::Value, String> {
    let mut saved = load_bouclier(&app);
    if let Some(req) = saved.pending.take() {
        let detail = if req.action == "retirer" { "Retrait refusé sur le poste" } else { "Refusé sur le poste" };
        let _ = report_bouclier(&app, "refuse", Some(detail.into())).await;
    }
    save_bouclier(&app, &saved);
    let view = bouclier_view(&saved);
    let _ = app.emit("bouclier", view.clone());
    Ok(json!({ "status": view, "message": "Rien n'a été changé." }))
}

/// Bilan à la demande (bouton « Refaire le bilan » de la fenêtre de l'agent).
#[tauri::command]
async fn security_check(app: AppHandle) -> Result<serde_json::Value, String> {
    let _ = report_inventaire(&app).await;
    report_posture(&app).await
}

/// Recherche une mise à jour au démarrage puis toutes les 6 heures : un poste allumé en continu
/// (démarrage automatique) se met ainsi à jour sans attendre un redémarrage.
async fn update_loop(app: AppHandle) {
    loop {
        check_update(&app).await;
        tokio::time::sleep(Duration::from_secs(UPDATE_EVERY_SECONDS)).await;
    }
}

/// Vérifie et installe une mise à jour si disponible, puis relance l'agent. Silencieux et tolérant :
/// toute erreur (pas de clé updater, hors ligne, aucune release) est ignorée. Jamais pendant une
/// séance guidée : la relance fermerait la fenêtre de l'élève ; on réessaie au cycle suivant.
async fn check_update(app: &AppHandle) {
    if app.get_webview_window("diffusion").is_some() {
        return;
    }
    let updater = match app.updater() {
        Ok(u) => u,
        Err(_) => return,
    };
    if let Ok(Some(update)) = updater.check().await {
        if app.get_webview_window("diffusion").is_some() {
            return;
        }
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
        .plugin(tauri_plugin_dialog::init())
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
            open_space_browser,
            security_check,
            bouclier_status,
            bouclier_accept,
            bouclier_refuse,
            ask_lea,
            lea_login,
            lea_me,
            lea_logout,
            lea_ask_full,
            share_file,
            dossiers_list,
            dossier_connect,
            dossier_disconnect,
            lea_check
        ])
        .setup(|app| {
            // Icône de barre des tâches : afficher / fermer une séance guidée / quitter.
            let _ = APP_HANDLE.set(app.handle().clone());
            let show = MenuItem::with_id(app, "show", "Ouvrir", true, None::<&str>)?;
            let espace = MenuItem::with_id(app, "espace", "Mon espace", true, None::<&str>)?;
            let endseance = MenuItem::with_id(app, "endseance", "Fermer la séance guidée", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&espace, &show, &endseance, &quit])?;
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
                    "espace" => {
                        let a = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let c = load_config(&a);
                            if c.token.is_some() {
                                if let Some(url) = space_url(&c.server_url, None) {
                                    let _ = open_space_window(&a, &c.server_url, &url, false);
                                }
                            } else if let Some(w) = a.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        });
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

            // Bilan de sécurité du poste : une minute après le démarrage, puis chaque jour.
            let pst = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                posture_loop(pst).await;
            });

            // Dossiers connectés : nouveaux fichiers envoyés à l'espace, au nom de la personne connectée.
            let dos = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                dossiers_loop(dos).await;
            });

            // Mise à jour automatique (silencieuse) au démarrage puis toutes les 6 heures.
            let up = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                update_loop(up).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("échec du lancement de l'agent Poste 5pour100");
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "https://my.5pour100.fr";

    #[test]
    fn chemin_interne_garde_le_serveur() {
        assert_eq!(
            resolve_target(S, "/api/agent/billet/1.pdf"),
            Some(Target::Internal("https://my.5pour100.fr/api/agent/billet/1.pdf".into()))
        );
    }

    #[test]
    fn chemin_piege_refuse() {
        // Collés derrière le serveur, ces chemins visaient un autre hôte avec le jeton du poste.
        for raw in ["@evil.example/x", ".evil.example/x", "//evil.example/x", "evil.example/x", "/\\evil.example", "file:///etc/passwd", "javascript:alert(1)", ""] {
            assert_eq!(resolve_target(S, raw), None, "{raw}");
        }
    }

    #[test]
    fn serveur_avec_port() {
        assert_eq!(
            resolve_target("http://localhost:3000/", "/ecran"),
            Some(Target::Internal("http://localhost:3000/ecran".into()))
        );
    }

    #[test]
    fn adresse_externe_sans_jeton() {
        assert_eq!(
            resolve_target(S, "https://exemple.fr/billet.pdf"),
            Some(Target::External("https://exemple.fr/billet.pdf".into()))
        );
    }

    #[test]
    fn nom_de_fichier_nettoye() {
        assert_eq!(safe_file_name("../../.bashrc", "f"), "bashrc");
        assert_eq!(safe_file_name("Billet n°12.pdf", "f"), "Billet n12.pdf");
        assert_eq!(safe_file_name("///", "billet.pdf"), "billet.pdf");
    }

    #[test]
    fn fichier_jamais_ecrase() {
        let dir = std::env::temp_dir().join(format!("poste-test-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = unique_path(&dir, "billet.pdf");
        std::fs::write(&a, b"1").unwrap();
        let b = unique_path(&dir, "billet.pdf");
        assert_eq!(b.file_name().unwrap(), "billet (2).pdf");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn espace_toujours_sur_le_serveur() {
        assert_eq!(space_url(S, None).as_deref(), Some("https://my.5pour100.fr/bureau"));
        assert_eq!(space_url(S, Some("/factures")).as_deref(), Some("https://my.5pour100.fr/factures"));
        // Chemin invalide : on retombe sur le bureau ; chemin piégé : refusé.
        assert_eq!(space_url(S, Some("https://ailleurs.fr")).as_deref(), Some("https://my.5pour100.fr/bureau"));
        assert_eq!(space_url(S, Some("//ailleurs.fr/x")), None);
        let srv = url::Url::parse(S).unwrap();
        assert!(same_origin(&url::Url::parse("https://my.5pour100.fr/x").unwrap(), &srv));
        assert!(!same_origin(&url::Url::parse("https://my.5pour100.fr.evil.example/").unwrap(), &srv));
        assert!(!same_origin(&url::Url::parse("http://my.5pour100.fr/").unwrap(), &srv));
    }

    #[test]
    fn attente_progressive_hors_ligne() {
        let seq: Vec<u64> = (0..7).map(next_delay).collect();
        assert_eq!(seq, vec![5, 10, 20, 40, 60, 60, 60]);
        assert_eq!(next_delay(u32::MAX), 60);
    }

    #[test]
    fn executable_jamais_imprime() {
        assert!(!print_file(Path::new("/tmp/x.exe"), None));
        assert!(!print_file(Path::new("/tmp/script.sh"), Some("HP")));
    }
}
