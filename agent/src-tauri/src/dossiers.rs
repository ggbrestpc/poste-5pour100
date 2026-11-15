//! Dossiers connectés : relier une application locale (caisse, comptabilité, scanner…) à l'espace
//! par son dossier d'export. La personne choisit elle-même le dossier ; seuls les fichiers qui y
//! arrivent APRÈS la connexion sont envoyés, à son nom, avec sa session. Logique pure ici (testée),
//! les appels réseau et la boucle sont dans main.rs.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Un fichier vu dans le dossier : taille et date de modification (secondes).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Seen {
    pub len: u64,
    pub mtime: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Dossier {
    pub id: String,
    pub label: String,
    pub path: PathBuf,
    pub visibilite: String,
    #[serde(default)]
    pub seen: BTreeMap<String, Seen>,
    #[serde(default)]
    pub sent: u64,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Saved {
    #[serde(default)]
    pub items: Vec<Dossier>,
}

/// Fichiers temporaires ou cachés : jamais envoyés (fichier en cours d'écriture, verrou Office…).
pub fn ignored(name: &str) -> bool {
    let l = name.to_lowercase();
    l.starts_with('.')
        || l.starts_with("~$")
        || l.starts_with('~')
        || ["thumbs.db", "desktop.ini"].contains(&l.as_str())
        || [".tmp", ".temp", ".part", ".partial", ".crdownload", ".download", ".lock", ".swp", ".lnk"]
            .iter()
            .any(|e| l.ends_with(e))
}

/// Fichiers ordinaires au premier niveau du dossier (pas de sous-dossier, pas de lien symbolique).
pub fn snapshot(dir: &Path) -> std::io::Result<BTreeMap<String, Seen>> {
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir(dir)? {
        let Ok(e) = e else { continue };
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
        if !meta.file_type().is_file() {
            continue;
        }
        let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
        if ignored(&name) {
            continue;
        }
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
        out.insert(name, Seen { len: meta.len(), mtime });
    }
    Ok(out)
}

/// Fichiers nouveaux ou modifiés depuis le dernier passage, dont l'écriture est terminée
/// (pas modifiés depuis `settle` secondes et non vides). Au plus `max`, les plus anciens d'abord.
pub fn candidates(seen: &BTreeMap<String, Seen>, now: &BTreeMap<String, Seen>, now_secs: u64, settle: u64, max: usize) -> Vec<String> {
    let mut v: Vec<(&String, &Seen)> = now
        .iter()
        .filter(|(n, s)| seen.get(*n) != Some(*s) && s.len > 0 && s.mtime + settle <= now_secs)
        .collect();
    v.sort_by_key(|(n, s)| (s.mtime, (*n).clone()));
    v.into_iter().take(max).map(|(n, _)| n.clone()).collect()
}

/// Oublie les fichiers qui ne sont plus dans le dossier (la mémoire ne grossit pas indéfiniment).
pub fn prune(seen: &mut BTreeMap<String, Seen>, now: &BTreeMap<String, Seen>) {
    seen.retain(|n, _| now.contains_key(n));
}

/// Type de fichier deviné par l'extension, pour l'affichage dans l'espace (téléchargé en pièce jointe de toute façon).
pub fn mime_for(name: &str) -> &'static str {
    match Path::new(name).extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref() {
        Some("pdf") => "application/pdf",
        Some("csv") => "text/csv",
        Some("txt") => "text/plain",
        Some("xml") => "application/xml",
        Some("json") => "application/json",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("xls") => "application/vnd.ms-excel",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("odt") => "application/vnd.oasis.opendocument.text",
        Some("ods") => "application/vnd.oasis.opendocument.spreadsheet",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: &[(&str, u64, u64)]) -> BTreeMap<String, Seen> {
        v.iter().map(|(n, l, t)| (n.to_string(), Seen { len: *l, mtime: *t })).collect()
    }

    #[test]
    fn seuls_les_fichiers_nouveaux_ou_modifies_et_stables() {
        let seen = m(&[("a.csv", 10, 100), ("b.pdf", 5, 100)]);
        let now = m(&[("a.csv", 10, 100), ("b.pdf", 6, 150), ("c.csv", 3, 120), ("d.csv", 4, 995), ("vide.csv", 0, 100)]);
        // b modifié, c nouveau ; d encore en cours d'écriture ; vide ignoré ; a inchangé.
        assert_eq!(candidates(&seen, &now, 1000, 10, 10), vec!["c.csv", "b.pdf"]);
        assert_eq!(candidates(&seen, &now, 1000, 10, 1), vec!["c.csv"]);
    }

    #[test]
    fn fichiers_temporaires_ignores() {
        for n in [".DS_Store", "~$classeur.xlsx", "export.csv.part", "x.tmp", "Thumbs.db", "raccourci.lnk", "f.crdownload"] {
            assert!(ignored(n), "{n}");
        }
        for n in ["Z-2026.csv", "facture 12.pdf", "scan.jpg"] {
            assert!(!ignored(n), "{n}");
        }
    }

    #[test]
    fn instantane_et_oubli() {
        let dir = std::env::temp_dir().join(format!("dossiers-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sous-dossier")).unwrap();
        std::fs::write(dir.join("a.csv"), b"x").unwrap();
        std::fs::write(dir.join("~$verrou.xlsx"), b"x").unwrap();
        std::fs::write(dir.join("sous-dossier").join("b.csv"), b"x").unwrap();
        let snap = snapshot(&dir).unwrap();
        assert_eq!(snap.keys().collect::<Vec<_>>(), vec!["a.csv"]);
        let mut seen = m(&[("a.csv", 1, 1), ("parti.csv", 1, 1)]);
        prune(&mut seen, &snap);
        assert_eq!(seen.keys().collect::<Vec<_>>(), vec!["a.csv"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn types() {
        assert_eq!(mime_for("Z.CSV"), "text/csv");
        assert_eq!(mime_for("sans"), "application/octet-stream");
    }
}
