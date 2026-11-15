//! Bilan de sécurité du poste : mises à jour, chiffrement du disque, pare-feu, antivirus et
//! verrouillage de l'écran. Lecture seule : uniquement des commandes qui CONSTATENT, sans droits
//! administrateur, avec un délai maximal. L'agent ne modifie jamais le poste.

use std::process::Command;
use std::time::{Duration, SystemTime};

use serde::Serialize;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Check {
    pub id: &'static str,
    pub status: &'static str, // "ok" | "alerte" | "inconnu"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn check(id: &'static str, status: &'static str, detail: impl Into<String>) -> Check {
    let d: String = detail.into();
    Check { id, status, detail: if d.is_empty() { None } else { Some(d.chars().take(280).collect()) } }
}

/// Au-delà, le système est considéré comme en retard de mises à jour.
pub const MAX_DAYS_WITHOUT_UPDATE: u64 = 45;

pub fn updates_check(days: Option<u64>) -> Check {
    match days {
        Some(d) if d <= MAX_DAYS_WITHOUT_UPDATE => check("mises_a_jour", "ok", format!("dernière mise à jour il y a {d} jour(s)")),
        Some(d) => check("mises_a_jour", "alerte", format!("aucune mise à jour depuis {d} jours")),
        None => check("mises_a_jour", "inconnu", ""),
    }
}

/// Délai maximal d'une commande du bilan : au-delà, elle est tuée (jamais de processus qui traîne).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

/// Lance une commande en lecture et renvoie sa sortie (stdout + stderr), ou None (absente, erreur,
/// ou délai dépassé : le processus est alors tué et récupéré avant de rendre la main).
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    run_with_timeout(cmd, args, COMMAND_TIMEOUT)
}

fn run_with_timeout(cmd: &str, args: &[&str], timeout: Duration) -> Option<String> {
    run_capture(cmd, args, timeout).map(|(_, s)| s)
}

/// Lance une commande sans console, la tue au-delà du délai, et renvoie son code de sortie et ses
/// sorties (stdout puis stderr). None si elle n'a pas pu démarrer ou a dépassé le délai.
pub(crate) fn run_capture(cmd: &str, args: &[&str], timeout: Duration) -> Option<(Option<i32>, String)> {
    use std::io::Read;
    use std::process::Stdio;
    let mut c = Command::new(cmd);
    c.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW : pas de fenêtre console qui clignote
    }
    let mut child = c.spawn().ok()?;
    // Lecture des sorties dans des fils dédiés : un tampon plein ne peut pas bloquer la commande.
    let mut out = child.stdout.take()?;
    let mut err = child.stderr.take()?;
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        b
    });
    let deadline = std::time::Instant::now() + timeout;
    let finished = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st.code()),
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => break None,
        }
    };
    let Some(code) = finished else {
        let _ = child.kill();
        let _ = child.wait();
        // Les fils de lecture se terminent d'eux-mêmes à la fermeture des tubes ; on ne les attend pas
        // (un sous-processus petit-enfant pourrait garder le tube ouvert).
        return None;
    };
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    let mut s = String::from_utf8_lossy(&stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&stderr));
    Some((code, s))
}

fn days_since(t: SystemTime) -> Option<u64> {
    SystemTime::now().duration_since(t).ok().map(|d| d.as_secs() / 86_400)
}

/* ------------------------------- Windows ------------------------------- */

/// Script PowerShell en lecture seule : une ligne « clé=valeur » par point.
#[cfg(target_os = "windows")]
const WIN_SCRIPT: &str = r#"
$ErrorActionPreference='SilentlyContinue'
try { $m = Get-MpComputerStatus; "av=$($m.RealTimeProtectionEnabled)" } catch { "av=" }
"fw=$(@(Get-NetFirewallProfile | Where-Object { -not $_.Enabled }).Count)/$(@(Get-NetFirewallProfile).Count)"
"bl=$((New-Object -ComObject Shell.Application).NameSpace($env:SystemDrive).Self.ExtendedProperty('System.Volume.BitLockerProtection'))"
$h = Get-HotFix | Where-Object { $_.InstalledOn } | Sort-Object InstalledOn -Descending | Select-Object -First 1
if ($h) { "upd=$([int]((Get-Date) - $h.InstalledOn).TotalDays)" } else { "upd=" }
"ssec=$((Get-ItemProperty 'HKCU:\Control Panel\Desktop').ScreenSaverIsSecure)"
"inact=$((Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System').InactivityTimeoutSecs)"
"os=$((Get-CimInstance Win32_OperatingSystem).Caption)"
"#;

fn kv<'a>(out: &'a str, key: &str) -> Option<&'a str> {
    out.lines()
        .filter_map(|l| l.trim().split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

/// Interprète la sortie du script Windows.
pub fn parse_windows(out: &str) -> (Vec<Check>, Option<String>) {
    let mut v = Vec::new();
    v.push(match kv(out, "av") {
        Some(x) if x.eq_ignore_ascii_case("true") => check("antivirus", "ok", "Microsoft Defender : protection en temps réel active"),
        Some(x) if x.eq_ignore_ascii_case("false") => check("antivirus", "alerte", "Microsoft Defender : protection en temps réel désactivée"),
        _ => check("antivirus", "inconnu", "Defender non lisible (autre antivirus ?)"),
    });
    v.push(match kv(out, "fw").and_then(|x| x.split_once('/')) {
        Some((off, total)) if total != "0" && off == "0" => check("pare_feu", "ok", "pare-feu actif sur tous les profils"),
        Some((off, total)) if total != "0" => check("pare_feu", "alerte", format!("pare-feu désactivé sur {off} profil(s) sur {total}")),
        _ => check("pare_feu", "inconnu", ""),
    });
    // System.Volume.BitLockerProtection : 1 activé, 3 chiffrement en cours, 2 désactivé, 5 suspendu.
    v.push(match kv(out, "bl") {
        Some("1") => check("chiffrement", "ok", "BitLocker actif sur le disque système"),
        Some("3") => check("chiffrement", "ok", "BitLocker : chiffrement en cours"),
        Some("2") => check("chiffrement", "alerte", "disque système non chiffré"),
        Some("5") => check("chiffrement", "alerte", "BitLocker suspendu"),
        _ => check("chiffrement", "inconnu", ""),
    });
    v.push(updates_check(kv(out, "upd").and_then(|x| x.parse().ok())));
    let secure_saver = kv(out, "ssec") == Some("1");
    let inactivity = kv(out, "inact").and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
    v.push(if inactivity > 0 {
        check("verrouillage", "ok", format!("verrouillage après {} min d'inactivité", inactivity.div_ceil(60)))
    } else if secure_saver {
        check("verrouillage", "ok", "économiseur d'écran avec mot de passe")
    } else {
        check("verrouillage", "inconnu", "aucun verrouillage automatique imposé")
    });
    (v, kv(out, "os").map(str::to_string))
}

/* -------------------------------- macOS -------------------------------- */

pub struct MacOutputs<'a> {
    pub filevault: Option<&'a str>,
    pub firewall: Option<&'a str>,
    pub gatekeeper: Option<&'a str>,
    pub screenlock: Option<&'a str>,
    pub update_days: Option<u64>,
}

pub fn parse_macos(o: &MacOutputs) -> Vec<Check> {
    let mut v = Vec::new();
    v.push(match o.filevault {
        Some(s) if s.contains("FileVault is On") => check("chiffrement", "ok", "FileVault actif"),
        Some(s) if s.contains("FileVault is Off") => check("chiffrement", "alerte", "FileVault désactivé"),
        _ => check("chiffrement", "inconnu", ""),
    });
    v.push(match o.firewall {
        Some(s) if s.contains("enabled") || s.contains("State = 1") || s.contains("State = 2") => check("pare_feu", "ok", "coupe-feu actif"),
        Some(s) if s.contains("disabled") || s.contains("State = 0") => check("pare_feu", "alerte", "coupe-feu désactivé"),
        _ => check("pare_feu", "inconnu", ""),
    });
    v.push(match o.gatekeeper {
        Some(s) if s.contains("assessments enabled") => check("antivirus", "ok", "XProtect et Gatekeeper actifs"),
        Some(s) if s.contains("assessments disabled") => check("antivirus", "alerte", "Gatekeeper désactivé"),
        _ => check("antivirus", "inconnu", ""),
    });
    v.push(match o.screenlock {
        Some(s) if s.contains("screenLock is off") => check("verrouillage", "alerte", "mot de passe non demandé après la veille"),
        Some(s) if s.contains("screenLock delay is") => check("verrouillage", "ok", "mot de passe demandé après la veille"),
        _ => check("verrouillage", "inconnu", ""),
    });
    v.push(updates_check(o.update_days));
    v
}

/* -------------------------------- Linux -------------------------------- */

pub struct LinuxOutputs<'a> {
    /// Types des périphériques sous la racine « / » (lsblk -s : la partition et ses parents).
    pub root_chain_types: Option<&'a str>,
    pub ufw_conf: Option<&'a str>,
    pub firewalld_active: bool,
    pub lock_enabled: Option<&'a str>,
    pub update_days: Option<u64>,
}

pub fn parse_linux(o: &LinuxOutputs) -> Vec<Check> {
    let mut v = Vec::new();
    // Seule compte la chaîne qui porte « / » : un swap ou une clé USB chiffrés ne protègent pas le système.
    v.push(match o.root_chain_types {
        Some(s) if s.lines().any(|l| l.trim() == "crypt") => check("chiffrement", "ok", "partition système chiffrée (LUKS)"),
        Some(s) if !s.trim().is_empty() => check("chiffrement", "alerte", "partition système non chiffrée"),
        _ => check("chiffrement", "inconnu", ""),
    });
    let ufw_on = o.ufw_conf.map(|c| c.lines().any(|l| l.trim() == "ENABLED=yes")).unwrap_or(false);
    v.push(if ufw_on || o.firewalld_active {
        check("pare_feu", "ok", if ufw_on { "pare-feu ufw actif" } else { "pare-feu firewalld actif" })
    } else {
        check("pare_feu", "alerte", "aucun pare-feu actif détecté (ufw ou firewalld)")
    });
    v.push(match o.lock_enabled.map(str::trim) {
        Some("true") => check("verrouillage", "ok", "verrouillage automatique actif"),
        Some("false") => check("verrouillage", "alerte", "verrouillage automatique désactivé"),
        _ => check("verrouillage", "inconnu", ""),
    });
    v.push(updates_check(o.update_days));
    v
}

/* ------------------------------ Collecte ------------------------------- */

/// Fait le bilan du poste (bloquant : à lancer hors de la boucle principale). Renvoie les points
/// et le nom du système.
pub fn collect() -> (Vec<Check>, String) {
    #[cfg(target_os = "windows")]
    {
        let out = run("powershell", &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", WIN_SCRIPT]).unwrap_or_default();
        let (checks, os) = parse_windows(&out);
        return (checks, os.unwrap_or_else(|| "Windows".into()));
    }
    #[cfg(target_os = "macos")]
    {
        let fv = run("fdesetup", &["status"]);
        let fw = run("/usr/libexec/ApplicationFirewall/socketfilterfw", &["--getglobalstate"]);
        let gk = run("spctl", &["--status"]);
        let sl = run("sysadminctl", &["-screenLock", "status"]);
        // Dernière mise à jour INSTALLÉE par softwareupdated (historique des installations), et non la
        // dernière recherche réussie : un Mac qui cherche sans installer doit rester en alerte.
        let history = run("plutil", &["-convert", "json", "-o", "-", "/Library/Receipts/InstallHistory.plist"]);
        let update_days = history.as_deref().and_then(last_mac_system_update).and_then(days_since);
        let checks = parse_macos(&MacOutputs {
            filevault: fv.as_deref(),
            firewall: fw.as_deref(),
            gatekeeper: gk.as_deref(),
            screenlock: sl.as_deref(),
            update_days,
        });
        let os = run("sw_vers", &["-productVersion"]).map(|v| format!("macOS {}", v.trim())).unwrap_or_else(|| "macOS".into());
        return (checks, os);
    }
    #[cfg(target_os = "linux")]
    {
        // Source de « / » (sans le sous-volume btrfs « [/@] »), puis ses périphériques parents.
        let root_src = run("findmnt", &["-no", "SOURCE", "/"])
            .map(|s| s.lines().next().unwrap_or("").split('[').next().unwrap_or("").trim().to_string())
            .filter(|s| s.starts_with("/dev/"));
        let lsblk = root_src.as_deref().and_then(|src| run("lsblk", &["-rnso", "TYPE", src]));
        let ufw = std::fs::read_to_string("/etc/ufw/ufw.conf").ok();
        let firewalld = run("systemctl", &["is-active", "firewalld"]).map(|s| s.trim() == "active").unwrap_or(false);
        let lock = run("gsettings", &["get", "org.gnome.desktop.screensaver", "lock-enabled"]);
        // Dernier changement de paquets (apt/dpkg ou rpm) : bon indicateur de la dernière mise à jour.
        let update_days = ["/var/lib/dpkg/status", "/var/lib/rpm/rpmdb.sqlite", "/var/lib/rpm/Packages", "/var/lib/pacman/local"]
            .iter()
            .find_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .and_then(days_since);
        let checks = parse_linux(&LinuxOutputs {
            root_chain_types: lsblk.as_deref(),
            ufw_conf: ufw.as_deref(),
            firewalld_active: firewalld,
            lock_enabled: lock.as_deref(),
            update_days,
        });
        let os = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())))
            .unwrap_or_else(|| "Linux".into());
        return (checks, os);
    }
    #[allow(unreachable_code)]
    (Vec::new(), String::new())
}

/// Date de la dernière installation faite par la mise à jour du système (« softwareupdated »), lue
/// dans InstallHistory.plist converti en JSON.
#[allow(dead_code)]
fn last_mac_system_update(json: &str) -> Option<SystemTime> {
    let items: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    items
        .iter()
        .filter(|i| i.get("processName").and_then(|p| p.as_str()) == Some("softwareupdated"))
        .filter_map(|i| i.get("date").and_then(|d| d.as_str()).and_then(parse_mac_date))
        .max()
}

/// « 2026-11-02T08:15:00Z » ou « 2026-11-02 08:15:00 +0000 » → instant (au jour près).
#[allow(dead_code)]
fn parse_mac_date(s: &str) -> Option<SystemTime> {
    let d = s.trim().get(0..10)?;
    let mut it = d.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (it.next()??, it.next()??, it.next()??);
    // Jours depuis 1970 (algorithme de Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    if days < 0 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(days as u64 * 86_400))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_tout_vert_ou_rouge() {
        let out = "av=True\nfw=0/3\nbl=1\nupd=12\nssec=0\ninact=600\nos=Microsoft Windows 11 Pro\n";
        let (c, os) = parse_windows(out);
        assert!(c.iter().all(|x| x.status == "ok"), "{c:?}");
        assert_eq!(os.as_deref(), Some("Microsoft Windows 11 Pro"));
        let out = "av=False\nfw=2/3\nbl=2\nupd=90\nssec=\ninact=\n";
        let (c, _) = parse_windows(out);
        let st: Vec<_> = c.iter().map(|x| (x.id, x.status)).collect();
        assert_eq!(st, vec![("antivirus", "alerte"), ("pare_feu", "alerte"), ("chiffrement", "alerte"), ("mises_a_jour", "alerte"), ("verrouillage", "inconnu")]);
    }

    #[test]
    fn windows_sortie_vide() {
        let (c, os) = parse_windows("");
        assert!(c.iter().all(|x| x.status == "inconnu"));
        assert!(os.is_none());
    }

    #[test]
    fn macos() {
        let c = parse_macos(&MacOutputs {
            filevault: Some("FileVault is Off.\n"),
            firewall: Some("Firewall is enabled. (State = 1)\n"),
            gatekeeper: Some("assessments enabled\n"),
            screenlock: Some("screenLock delay is immediate\n"),
            update_days: Some(3),
        });
        let st: Vec<_> = c.iter().map(|x| (x.id, x.status)).collect();
        assert_eq!(st, vec![("chiffrement", "alerte"), ("pare_feu", "ok"), ("antivirus", "ok"), ("verrouillage", "ok"), ("mises_a_jour", "ok")]);
    }

    #[test]
    fn linux() {
        let c = parse_linux(&LinuxOutputs {
            root_chain_types: Some("lvm\ncrypt\npart\ndisk\n"),
            ufw_conf: Some("# commentaire\nENABLED=no\n"),
            firewalld_active: false,
            lock_enabled: Some("true\n"),
            update_days: None,
        });
        let st: Vec<_> = c.iter().map(|x| (x.id, x.status)).collect();
        assert_eq!(st, vec![("chiffrement", "ok"), ("pare_feu", "alerte"), ("verrouillage", "ok"), ("mises_a_jour", "inconnu")]);
    }

    #[test]
    fn linux_racine_non_chiffree_malgre_un_swap_chiffre() {
        // La chaîne de « / » ne contient pas « crypt » : alerte, même si un autre volume est chiffré.
        let c = parse_linux(&LinuxOutputs {
            root_chain_types: Some("part\ndisk\n"),
            ufw_conf: Some("ENABLED=yes\n"),
            firewalld_active: false,
            lock_enabled: None,
            update_days: Some(2),
        });
        assert_eq!(c[0].status, "alerte");
        assert_eq!(c[1].status, "ok");
    }

    #[test]
    fn mac_derniere_installation_systeme() {
        let json = r#"[
            {"date":"2026-10-01T10:00:00Z","processName":"softwareupdated","displayName":"macOS 26.1"},
            {"date":"2026-11-10T10:00:00Z","processName":"App Store","displayName":"Pages"},
            {"date":"2026-09-01T10:00:00Z","processName":"softwareupdated","displayName":"XProtect"}
        ]"#;
        let t = last_mac_system_update(json).unwrap();
        assert_eq!(t, parse_mac_date("2026-10-01").unwrap());
        assert!(last_mac_system_update("[]").is_none());
    }

    #[test]
    fn commande_trop_longue_tuee() {
        #[cfg(unix)]
        {
            let start = std::time::Instant::now();
            assert!(run_with_timeout("sleep", &["30"], Duration::from_millis(300)).is_none());
            assert!(start.elapsed() < Duration::from_secs(5));
            assert_eq!(run_with_timeout("echo", &["bonjour"], Duration::from_secs(5)).as_deref(), Some("bonjour\n"));
        }
    }

    #[test]
    fn date_mac() {
        let t = parse_mac_date("1970-01-02 08:15:00 +0000").unwrap();
        assert_eq!(t, SystemTime::UNIX_EPOCH + Duration::from_secs(86_400));
        assert!(parse_mac_date("n'importe quoi").is_none());
    }

    #[test]
    fn collecte_reelle_ne_plante_pas() {
        let (c, os) = collect();
        assert!(!os.is_empty());
        assert!(c.len() >= 4);
    }
}
