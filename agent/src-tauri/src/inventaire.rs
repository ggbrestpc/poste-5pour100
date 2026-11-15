// Inventaire du poste, en lecture seule : pour l'instant la liste des imprimantes installées.
// L'espace s'en sert pour choisir l'imprimante des billets et badges de ce poste.

use std::time::Duration;

use serde::Serialize;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Printer {
    pub name: String,
    pub default: bool,
}

const TIMEOUT: Duration = Duration::from_secs(20);

/// Windows : lignes « True|Nom » ou « False|Nom ».
pub fn parse_windows(out: &str) -> Vec<Printer> {
    out.lines()
        .filter_map(|l| {
            let (d, name) = l.trim().split_once('|')?;
            let name = name.trim();
            (!name.is_empty()).then(|| Printer { name: name.to_string(), default: d.trim().eq_ignore_ascii_case("true") })
        })
        .collect()
}

/// CUPS : `lpstat -e` (une destination par ligne) et `lpstat -d` (« …: nom », message traduit selon
/// la langue, d'où la lecture après le dernier « : »).
pub fn parse_cups(list: &str, default_line: &str) -> Vec<Printer> {
    let def = default_line.rsplit(':').next().map(str::trim).unwrap_or("");
    list.lines()
        .map(str::trim)
        .filter(|n| !n.is_empty() && !n.contains(char::is_whitespace))
        .map(|n| Printer { name: n.to_string(), default: n == def })
        .collect()
}

pub fn printers() -> Vec<Printer> {
    #[cfg(target_os = "windows")]
    {
        // Sortie en UTF-8 : un nom accentué (« Imprimante Étage ») doit revenir intact pour PrintTo.
        let script = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; Get-CimInstance Win32_Printer | ForEach-Object { \"$($_.Default)|$($_.Name)\" }";
        let out = crate::posture::run_capture("powershell", &["-NoProfile", "-NonInteractive", "-Command", script], TIMEOUT)
            .map(|(_, s)| s)
            .unwrap_or_default();
        return parse_windows(&out);
    }
    #[allow(unreachable_code)]
    {
        let list = crate::posture::run_capture("lpstat", &["-e"], TIMEOUT).map(|(_, s)| s).unwrap_or_default();
        let def = crate::posture::run_capture("lpstat", &["-d"], TIMEOUT).map(|(_, s)| s).unwrap_or_default();
        // Sans imprimante, lpstat -d dit « no system default destination » : aucune ligne de la liste n'y correspond.
        parse_cups(&list, def.lines().next().unwrap_or(""))
    }
}

/// Nom utilisable sans risque dans la commande d'impression (pas de guillemets ni de caractères de contrôle).
pub fn safe_printer_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 200 && !name.chars().any(|c| c.is_control() || c == '"' || c == '`')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows() {
        let p = parse_windows("False|Microsoft Print to PDF\r\nTrue|Brother QL-820NWB\r\n|\r\nbruit\r\n");
        assert_eq!(p.len(), 2);
        assert_eq!(p[1], Printer { name: "Brother QL-820NWB".into(), default: true });
    }

    #[test]
    fn cups() {
        let p = parse_cups("Brother_QL\nHP_LaserJet\n", "destination système par défaut : HP_LaserJet");
        assert_eq!(p, vec![Printer { name: "Brother_QL".into(), default: false }, Printer { name: "HP_LaserJet".into(), default: true }]);
        assert!(parse_cups("", "no system default destination").is_empty());
    }

    #[test]
    fn noms_surs() {
        assert!(safe_printer_name("HP LaserJet (bureau)"));
        assert!(!safe_printer_name("x\" -Verb RunAs"));
        assert!(!safe_printer_name(""));
    }
}
