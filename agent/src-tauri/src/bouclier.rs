// CINQ Bouclier en un clic : DNS protecteur de l'espace sur ce poste.
//
// L'espace n'envoie que l'hôte DNS, le ClientID de l'abonnement et l'adresse de contrôle ; l'agent
// vérifie leur forme et construit lui-même la configuration. Rien n'est changé sans deux accords :
// celui de la personne devant l'ordinateur (bouton dans l'agent), puis celui du système (UAC sous
// Windows, mot de passe administrateur ou Réglages Système sous macOS). Les réglages DNS d'origine
// sont gardés pour pouvoir les remettre (« Retirer »).

use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Identifiant du profil Apple (le même que celui servi par la page Bouclier de l'espace).
pub const MAC_PROFILE_ID: &str = "fr.5pour100.bouclier";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Request {
    /// « proteger » ou « retirer ».
    pub action: String,
    #[serde(default)]
    pub host: String,
    #[serde(default, rename = "clientId")]
    pub client_id: String,
    #[serde(default, rename = "probeHost")]
    pub probe_host: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Iface {
    pub index: u32,
    /// Serveurs DNS fixés à la main avant le Bouclier ; vide = DNS automatique (DHCP).
    pub static_dns: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Applied {
    pub host: String,
    pub client_id: String,
    pub probe_host: String,
    /// Windows : adresse IPv4 du serveur DNS et interfaces modifiées.
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub interfaces: Vec<Iface>,
    /// La protection a été constatée (adresse de contrôle bloquée).
    #[serde(default)]
    pub verified: bool,
}

/// État local, gardé dans le dossier de configuration de l'agent.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Saved {
    /// Demande de l'espace en attente de l'accord de la personne devant l'ordinateur.
    #[serde(default)]
    pub pending: Option<Request>,
    #[serde(default)]
    pub applied: Option<Applied>,
}

#[derive(Debug, PartialEq)]
#[allow(dead_code)] // Refused n'existe pas sous Linux (pas de réglage automatique).
pub enum Outcome {
    /// Accord du système refusé (UAC annulé, mot de passe non saisi).
    Refused,
    Error(String),
}

/// Nom d'hôte DNS simple (lettres, chiffres, tirets, points), sans port ni chemin.
pub fn valid_host(h: &str) -> bool {
    let h = h.trim_end_matches('.');
    h.len() >= 3
        && h.len() <= 253
        && h.contains('.')
        && h.split('.').all(|l| {
            !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// ClientID Bouclier : « c » suivi de 20 caractères hexadécimaux (voir newClientId côté espace).
pub fn valid_client_id(id: &str) -> bool {
    id.len() == 21 && id.starts_with('c') && id[1..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Lit et vérifie une demande reçue de l'espace. Toute valeur hors forme est refusée.
pub fn parse_request(payload: &serde_json::Value) -> Result<Request, String> {
    let r: Request = serde_json::from_value(payload.clone()).map_err(|_| "demande Bouclier illisible".to_string())?;
    match r.action.as_str() {
        "retirer" => Ok(Request { action: r.action, host: String::new(), client_id: String::new(), probe_host: String::new() }),
        "proteger" => {
            if !valid_host(&r.host) || !valid_host(&r.probe_host) || !valid_client_id(&r.client_id) {
                return Err("demande Bouclier refusée : hôte ou identifiant invalide".into());
            }
            Ok(r)
        }
        _ => Err("action Bouclier inconnue".into()),
    }
}

pub fn doh_template(host: &str, client_id: &str) -> String {
    format!("https://{host}/dns-query/{client_id}")
}

/// UUID stable tiré du ClientID (32 caractères hexadécimaux), pour que réinstaller remplace le profil.
fn uuid_from(client_id: &str, salt: char) -> String {
    let hex: String = format!("{}{salt}{salt}{salt}{salt}{salt}{salt}{salt}{salt}{salt}{salt}{salt}{salt}", &client_id[1..]);
    format!("{}-{}-4{}-a{}-{}", &hex[0..8], &hex[8..12], &hex[13..16], &hex[17..20], &hex[20..32])
}

/// Profil Apple « DNS sur HTTPS » vers le ClientID (macOS).
pub fn mobileconfig(host: &str, client_id: &str) -> String {
    let doh = doh_template(host, client_id);
    let payload = uuid_from(client_id, 'b');
    let profile = uuid_from(client_id, 'c');
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>PayloadContent</key>
  <array>
    <dict>
      <key>DNSSettings</key>
      <dict>
        <key>DNSProtocol</key>
        <string>HTTPS</string>
        <key>ServerURL</key>
        <string>{doh}</string>
      </dict>
      <key>PayloadDisplayName</key>
      <string>CINQ Bouclier</string>
      <key>PayloadIdentifier</key>
      <string>{MAC_PROFILE_ID}.dns</string>
      <key>PayloadType</key>
      <string>com.apple.dnsSettings.managed</string>
      <key>PayloadUUID</key>
      <string>{payload}</string>
      <key>PayloadVersion</key>
      <integer>1</integer>
    </dict>
  </array>
  <key>PayloadDisplayName</key>
  <string>CINQ Bouclier</string>
  <key>PayloadIdentifier</key>
  <string>{MAC_PROFILE_ID}</string>
  <key>PayloadType</key>
  <string>Configuration</string>
  <key>PayloadUUID</key>
  <string>{profile}</string>
  <key>PayloadVersion</key>
  <integer>1</integer>
  <key>PayloadDescription</key>
  <string>DNS protecteur CINQ Bouclier, installé depuis l'agent Poste 5pour100.</string>
</dict>
</plist>
"#
    )
}

/* ------------------------------ Vérification ------------------------------ */

/// Résout un nom avec le résolveur du système, dans un délai borné.
fn resolve(host: &str, timeout: Duration) -> Option<Vec<IpAddr>> {
    let h = host.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = (h.as_str(), 443).to_socket_addrs().map(|it| it.map(|a| a.ip()).collect::<Vec<_>>()).ok();
        let _ = tx.send(r);
    });
    rx.recv_timeout(timeout).ok().flatten()
}

/// Verdict à partir des résolutions : témoin résolu et adresse de contrôle bloquée (échec ou
/// 0.0.0.0) = protégé ; contrôle résolu vers une vraie adresse = non protégé ; témoin muet = inconnu.
pub fn verdict(witness: Option<&[IpAddr]>, probe: Option<&[IpAddr]>) -> Option<bool> {
    match probe {
        Some(ips) if ips.iter().any(|ip| !ip.is_unspecified()) => Some(false),
        _ => match witness {
            Some(w) if !w.is_empty() => Some(true),
            _ => None,
        },
    }
}

/// Le Bouclier protège-t-il ce poste en ce moment ? (lecture seule)
pub fn is_protected(witness_host: &str, probe_host: &str) -> Option<bool> {
    let t = Duration::from_secs(5);
    let w = resolve(witness_host, t);
    let p = resolve(probe_host, t);
    verdict(w.as_deref(), p.as_deref())
}

/* --------------------------------- Windows --------------------------------- */

/// Lignes « index=serveurs » (serveurs fixés à la main, séparés par virgule ou espace).
pub fn parse_ifaces(out: &str) -> Vec<Iface> {
    out.lines()
        .filter_map(|l| {
            let (i, rest) = l.trim().split_once('=')?;
            let index = i.trim().parse::<u32>().ok()?;
            let static_dns = rest
                .split([',', ' ', ';'])
                .map(str::trim)
                .filter(|s| s.parse::<IpAddr>().is_ok())
                .map(String::from)
                .collect();
            Some(Iface { index, static_dns })
        })
        .collect()
}

/// Sauvegarde à garder quand le Bouclier est déjà en place : pour une carte déjà gérée, les DNS
/// d'origine sont ceux de la première sauvegarde (les DNS actuels sont ceux du Bouclier). Les cartes
/// gérées absentes aujourd'hui (débranchées) restent dans la sauvegarde pour être remises plus tard.
pub fn merge_backup(prev: Option<&Applied>, fresh: Vec<Iface>) -> Vec<Iface> {
    let Some(prev) = prev else { return fresh };
    let mut out: Vec<Iface> = fresh
        .into_iter()
        .map(|f| prev.interfaces.iter().find(|p| p.index == f.index).cloned().unwrap_or(f))
        .collect();
    for p in &prev.interfaces {
        if !out.iter().any(|o| o.index == p.index) {
            out.push(p.clone());
        }
    }
    out
}

/// Remet les DNS d'origine (fixes ou automatiques) et retire le modèle DoH du Bouclier.
pub fn win_restore_block(ip: &str, ifaces: &[Iface]) -> String {
    let mut s = String::new();
    for i in ifaces {
        if i.static_dns.is_empty() {
            s.push_str(&format!("  Set-DnsClientServerAddress -InterfaceIndex {} -ResetServerAddresses -ErrorAction SilentlyContinue\n", i.index));
        } else {
            let list = i.static_dns.iter().map(|d| format!("'{d}'")).collect::<Vec<_>>().join(",");
            s.push_str(&format!("  Set-DnsClientServerAddress -InterfaceIndex {} -ServerAddresses @({list}) -ErrorAction SilentlyContinue\n", i.index));
        }
    }
    s.push_str(&format!("  Remove-DnsClientDohServerAddress -ServerAddress '{ip}' -ErrorAction SilentlyContinue\n"));
    s.push_str("  Clear-DnsClientCache -ErrorAction SilentlyContinue\n");
    s
}

/// Script (lancé avec l'accord administrateur) : DNS chiffré uniquement vers le Bouclier, puis
/// contrôle. Si le DNS ne répond plus ou n'est pas filtré, les réglages d'origine sont remis.
/// Codes : 0 protégé, 3 Windows trop ancien (rien changé), 4 DNS muet, 5 erreur, 6 non chiffré.
pub fn win_apply_script(ip: &str, template: &str, ifaces: &[Iface], witness: &str, probe: &str) -> String {
    let idx = ifaces.iter().map(|i| i.index.to_string()).collect::<Vec<_>>().join(",");
    let restore = win_restore_block(ip, ifaces);
    format!(
        r#"$ErrorActionPreference='Stop'
if (-not (Get-Command Add-DnsClientDohServerAddress -ErrorAction SilentlyContinue)) {{ exit 3 }}
$ip='{ip}'; $tpl='{template}'
try {{
  if (Get-DnsClientDohServerAddress -ServerAddress $ip -ErrorAction SilentlyContinue) {{
    Set-DnsClientDohServerAddress -ServerAddress $ip -DohTemplate $tpl -AllowFallbackToUdp $false -AutoUpgrade $true | Out-Null
  }} else {{
    Add-DnsClientDohServerAddress -ServerAddress $ip -DohTemplate $tpl -AllowFallbackToUdp $false -AutoUpgrade $true | Out-Null
  }}
  foreach ($i in @({idx})) {{ Set-DnsClientServerAddress -InterfaceIndex $i -ServerAddresses $ip }}
  Clear-DnsClientCache
  Start-Sleep -Seconds 2
}} catch {{
{restore}  exit 5
}}
$ok = $false
try {{ Resolve-DnsName -Name '{witness}' -DnsOnly -QuickTimeout -ErrorAction Stop | Out-Null; $ok = $true }} catch {{}}
if (-not $ok) {{
{restore}  exit 4
}}
$open = $false
try {{ $open = @(Resolve-DnsName -Name '{probe}' -Type A -DnsOnly -QuickTimeout -ErrorAction Stop | Where-Object {{ $_.IPAddress -and $_.IPAddress -ne '0.0.0.0' }}).Count -gt 0 }} catch {{}}
if ($open) {{
{restore}  exit 6
}}
exit 0
"#
    )
}

pub fn win_remove_script(ip: &str, ifaces: &[Iface]) -> String {
    format!("$ErrorActionPreference='SilentlyContinue'\n{}exit 0\n", win_restore_block(ip, ifaces))
}

/// Interfaces physiques actives et leurs DNS fixés à la main (lecture seule, sans élévation).
#[cfg(target_os = "windows")]
const WIN_IFACES: &str = r#"
$ErrorActionPreference='SilentlyContinue'
Get-NetAdapter -Physical | Where-Object Status -eq 'Up' | ForEach-Object {
  $k = 'HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\' + $_.InterfaceGuid
  $ns = (Get-ItemProperty -Path $k -Name NameServer).NameServer
  "$($_.ifIndex)=$ns"
}
"#;

/// Encodage attendu par « powershell -EncodedCommand » : UTF-16LE en base64.
pub fn encode_ps(script: &str) -> String {
    use base64::Engine;
    let bytes: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Lance le script avec l'accord administrateur (fenêtre UAC). Renvoie le code de sortie du script,
/// ou Refused si la personne a refusé l'élévation.
#[cfg(target_os = "windows")]
fn win_elevated(script: &str) -> Result<i32, Outcome> {
    let enc = encode_ps(script);
    let outer = format!(
        "try {{ $p = Start-Process -FilePath powershell.exe -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-EncodedCommand','{enc}'; exit $p.ExitCode }} catch {{ exit 1223 }}"
    );
    match crate::posture::run_capture("powershell", &["-NoProfile", "-NonInteractive", "-Command", &outer], Duration::from_secs(300)) {
        Some((Some(1223), _)) => Err(Outcome::Refused),
        Some((Some(code), _)) => Ok(code),
        Some((None, _)) => Err(Outcome::Error("le réglage a été interrompu".into())),
        None => Err(Outcome::Error("pas de réponse à la demande d'accord administrateur (5 min)".into())),
    }
}

/* ------------------------------ Appliquer / retirer ------------------------------ */

/// Applique le Bouclier. `witness` = hôte de l'espace (doit rester joignable), `dir` = dossier où
/// déposer le profil macOS, `prev` = Bouclier déjà en place (sa sauvegarde d'origine est gardée),
/// `persist` = enregistre la sauvegarde sur disque AVANT toute modification du système.
#[allow(unused_variables)]
pub fn apply(req: &Request, witness: &str, dir: &std::path::Path, prev: Option<&Applied>, persist: &dyn Fn(&Applied) -> bool) -> Result<Applied, Outcome> {
    #[cfg(target_os = "windows")]
    {
        let ip = resolve(&req.host, Duration::from_secs(10))
            .and_then(|v| v.into_iter().find(|a| a.is_ipv4()))
            .ok_or_else(|| Outcome::Error(format!("adresse de {} introuvable", req.host)))?
            .to_string();
        let listed = crate::posture::run_capture("powershell", &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", WIN_IFACES], Duration::from_secs(20))
            .map(|(_, s)| s)
            .unwrap_or_default();
        let fresh = parse_ifaces(&listed);
        if fresh.is_empty() {
            return Err(Outcome::Error("aucune carte réseau active trouvée".into()));
        }
        let ifaces = merge_backup(prev, fresh);
        let applied = Applied { host: req.host.clone(), client_id: req.client_id.clone(), probe_host: req.probe_host.clone(), ip: Some(ip.clone()), interfaces: ifaces.clone(), verified: false };
        // Sans sauvegarde sur disque, pas de modification : un arrêt brutal ne doit pas perdre les DNS d'origine.
        if !persist(&applied) {
            return Err(Outcome::Error("sauvegarde des réglages d'origine impossible : rien n'a été changé".into()));
        }
        let script = win_apply_script(&ip, &doh_template(&req.host, &req.client_id), &ifaces, witness, &req.probe_host);
        return match win_elevated(&script)? {
            0 => Ok(Applied { verified: true, ..applied }),
            3 => Err(Outcome::Error("Windows 11 est nécessaire pour le DNS chiffré ; rien n'a été changé".into())),
            4 => Err(Outcome::Error("le DNS du Bouclier ne répondait pas depuis ce réseau : réglages d'origine remis".into())),
            6 => Err(Outcome::Error("le DNS n'était pas chiffré (filtrage non constaté) : réglages d'origine remis".into())),
            c => Err(Outcome::Error(format!("réglage impossible (code {c}) : réglages d'origine remis"))),
        };
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::fs::create_dir_all(dir);
        let path = dir.join("CINQ-Bouclier.mobileconfig");
        std::fs::write(&path, mobileconfig(&req.host, &req.client_id)).map_err(|e| Outcome::Error(format!("profil non écrit : {e}")))?;
        // macOS enregistre le profil puis demande de l'installer dans Réglages Système → Profils.
        let opened = std::process::Command::new("open").arg(&path).status().map(|s| s.success()).unwrap_or(false);
        if !opened {
            return Err(Outcome::Error("le profil n'a pas pu être ouvert".into()));
        }
        let _ = std::process::Command::new("open").arg("x-apple.systempreferences:com.apple.preferences.configurationprofiles").status();
        return Ok(Applied { host: req.host.clone(), client_id: req.client_id.clone(), probe_host: req.probe_host.clone(), ip: None, interfaces: vec![], verified: false });
    }
    #[allow(unreachable_code)]
    Err(Outcome::Error("Bouclier en un clic non disponible sur ce système : suivez le guide de la page Bouclier".into()))
}

/// Retire le Bouclier et remet les réglages d'origine.
#[allow(unused_variables)]
pub fn remove(applied: &Applied) -> Result<(), Outcome> {
    #[cfg(target_os = "windows")]
    {
        let Some(ip) = applied.ip.as_deref() else { return Ok(()) };
        return match win_elevated(&win_remove_script(ip, &applied.interfaces))? {
            0 => Ok(()),
            c => Err(Outcome::Error(format!("retrait incomplet (code {c})"))),
        };
    }
    #[cfg(target_os = "macos")]
    {
        // Mot de passe administrateur demandé par macOS ; annulation = erreur -128.
        let script = format!("do shell script \"/usr/bin/profiles remove -identifier {MAC_PROFILE_ID}\" with administrator privileges");
        return match crate::posture::run_capture("osascript", &["-e", &script], Duration::from_secs(300)) {
            Some((Some(0), _)) => Ok(()),
            Some((_, out)) if out.contains("-128") => Err(Outcome::Refused),
            _ => {
                let _ = std::process::Command::new("open").arg("x-apple.systempreferences:com.apple.preferences.configurationprofiles").status();
                Err(Outcome::Error("retirez le profil « CINQ Bouclier » dans Réglages Système → Profils".into()))
            }
        };
    }
    #[allow(unreachable_code)]
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CID: &str = "c0123456789abcdef0123";

    #[test]
    fn demande_verifiee() {
        let ok = parse_request(&json!({ "action": "proteger", "host": "dns.5pour100.fr", "clientId": CID, "probeHost": "test.bouclier.5pour100.fr" })).unwrap();
        assert_eq!(ok.client_id, CID);
        assert!(parse_request(&json!({ "action": "proteger", "host": "dns.5pour100.fr'; calc", "clientId": CID, "probeHost": "a.b" })).is_err());
        assert!(parse_request(&json!({ "action": "proteger", "host": "dns.5pour100.fr", "clientId": "c'+x", "probeHost": "a.b" })).is_err());
        assert!(parse_request(&json!({ "action": "proteger", "host": "evil.example/x", "clientId": CID, "probeHost": "a.b" })).is_err());
        assert!(parse_request(&json!({ "action": "formater" })).is_err());
        assert_eq!(parse_request(&json!({ "action": "retirer", "host": "x" })).unwrap().host, "");
    }

    #[test]
    fn profil_apple() {
        let p = mobileconfig("dns.5pour100.fr", CID);
        assert!(p.contains(&format!("https://dns.5pour100.fr/dns-query/{CID}")));
        assert!(p.contains("<string>fr.5pour100.bouclier</string>"));
        let u = uuid_from(CID, 'b');
        assert_eq!(u.len(), 36);
        assert_eq!(u.matches('-').count(), 4);
    }

    #[test]
    fn verdict_protection() {
        let w = ["1.2.3.4".parse::<IpAddr>().unwrap()];
        let blocked = ["0.0.0.0".parse::<IpAddr>().unwrap()];
        let real = ["5.6.7.8".parse::<IpAddr>().unwrap()];
        assert_eq!(verdict(Some(&w), Some(&blocked)), Some(true));
        assert_eq!(verdict(Some(&w), None), Some(true));
        assert_eq!(verdict(Some(&w), Some(&real)), Some(false));
        assert_eq!(verdict(None, None), None);
    }

    #[test]
    fn interfaces_et_scripts_windows() {
        let ifs = parse_ifaces("12=\r\n7=1.1.1.1,9.9.9.9\r\nbruit\r\n9=x; 8.8.8.8\r\n");
        assert_eq!(ifs.len(), 3);
        assert!(ifs[0].static_dns.is_empty());
        assert_eq!(ifs[1].static_dns, vec!["1.1.1.1", "9.9.9.9"]);
        assert_eq!(ifs[2].static_dns, vec!["8.8.8.8"]);
        let s = win_apply_script("203.0.113.5", &doh_template("dns.5pour100.fr", CID), &ifs, "my.5pour100.fr", "test.bouclier.5pour100.fr");
        assert!(s.contains("-AllowFallbackToUdp $false"));
        assert!(s.contains("foreach ($i in @(12,7,9))"));
        assert!(s.contains("Set-DnsClientServerAddress -InterfaceIndex 12 -ResetServerAddresses"));
        assert!(s.contains("-InterfaceIndex 7 -ServerAddresses @('1.1.1.1','9.9.9.9')"));
        assert!(s.contains("exit 6"));
        let r = win_remove_script("203.0.113.5", &ifs);
        assert!(r.contains("Remove-DnsClientDohServerAddress -ServerAddress '203.0.113.5'"));
        // Encodage UTF-16LE : « A » = 0x41 0x00.
        assert_eq!(encode_ps("A"), "QQA=");
    }

    #[test]
    fn sauvegarde_d_origine_gardee() {
        let first = parse_ifaces("12=\n7=1.1.1.1\n");
        let prev = Applied { ip: Some("203.0.113.5".into()), interfaces: first.clone(), ..Default::default() };
        // Deuxième application : les DNS actuels sont ceux du Bouclier, la carte 12 est débranchée, une 9 apparaît.
        let now = parse_ifaces("7=203.0.113.5\n9=8.8.8.8\n");
        let merged = merge_backup(Some(&prev), now);
        assert_eq!(merged.iter().find(|i| i.index == 7).unwrap().static_dns, vec!["1.1.1.1"]);
        assert_eq!(merged.iter().find(|i| i.index == 9).unwrap().static_dns, vec!["8.8.8.8"]);
        assert!(merged.iter().find(|i| i.index == 12).unwrap().static_dns.is_empty());
        assert_eq!(merge_backup(None, first.clone()), first);
    }
}
