# Agent de bureau « Poste 5pour100 »

Petit logiciel installable (Windows / macOS / Linux) qui relie un ordinateur à un espace 5pour100
et exécute les tâches reçues : notifications, impression de billets, partage de fichiers et émargement.
Construit avec **Tauri 2** (Rust + une petite interface web) : binaire léger, sans Electron.

Aux couleurs de **5pour100** : logo « 5 », barre des tâches, démarrage automatique au besoin, et une
fenêtre qui affiche en direct l'état de connexion, la dernière activité et le nombre de tâches traitées.

> Le **côté serveur** (API et écran d'appairage) est déjà en place dans la plateforme. Cet agent en
> est le client. Il n'a pas encore été compilé/signé : suivez les étapes ci-dessous, ou laissez la CI
> GitHub produire les installeurs.

## Comment ça marche

1. Dans l'espace : **Agent de bureau → Appairer un poste** → un **code** (valable 15 min).
2. Dans l'agent : coller le code → il appelle `POST /api/agent/pair`, reçoit un **jeton** (stocké
   dans le **coffre d'identifiants du système**, jamais renvoyé) et se connecte.
3. En fond, toutes les 5 s : `POST /api/agent/sync` (jeton `Bearer`) → récupère les tâches, les
   exécute, et les acquitte au cycle suivant. Révoquer le poste depuis l'espace coupe le jeton.

Confort d'usage :

- **Instance unique** : relancer l'agent ramène la fenêtre existante au premier plan (pas de doublon).
- **Fermeture = réduction** : fermer la fenêtre masque l'agent dans la barre des tâches ; il continue en
  fond. Le menu de la barre (« Ouvrir » / « Quitter ») permet de le ré-afficher ou de l'arrêter.
- **Démarrage automatique** : interrupteur dans la fenêtre pour lancer l'agent à l'ouverture de session.
- **État en direct** : point vert/gris (connecté / hors ligne), dernière activité, heure de dernière
  synchro et compteur de tâches, poussés par la boucle de synchronisation via un évènement `status`.
- **Dossier des fichiers reçus** : bouton pour ouvrir le dossier où billets et partages sont déposés.

Fichiers clés :

- `src-tauri/src/main.rs` — appairage, persistance du jeton, boucle de synchronisation, icône de
  barre des tâches, notifications natives, instance unique, démarrage automatique, évènements d'état.
- `src/` — interface (HTML/CSS/JS, sans bundler) : écran d'appairage et écran « connecté » (état en
  direct, interrupteur de démarrage automatique, ouverture du dossier des fichiers reçus).
- `src-tauri/tauri.conf.json` — configuration de l'app et des installeurs.
- `src-tauri/capabilities/default.json` — permissions (core, notification, autostart).
- `logo.png` — logo source ; les icônes de l'app en sont dérivées à la compilation.

## Prérequis

- **Rust** (stable) : <https://rustup.rs>
- **Node.js** 18+ (seulement pour générer les icônes via la CLI Tauri)
- **Linux** uniquement : `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libssl-dev libgtk-3-dev build-essential`
- **macOS** : Xcode Command Line Tools (`xcode-select --install`)
- **Windows** : « Desktop development with C++ » (Visual Studio Build Tools) + WebView2 (préinstallé sur Windows 10/11)

## Compiler en local

```bash
cd agent
# 1) Générer les icônes à partir du logo (une fois, ou après avoir changé logo.png)
npx @tauri-apps/cli@^2 icon logo.png --output src-tauri/icons
# 2) Lancer en développement (fenêtre + rechargement)
npx @tauri-apps/cli@^2 dev
# 3) Produire l'installeur de votre OS
npx @tauri-apps/cli@^2 build
```

Les installeurs sont écrits dans `src-tauri/target/release/bundle/` :

- **Windows** : `msi/…​.msi` et `nsis/…​-setup.exe`
- **macOS** : `dmg/…​.dmg` (et `.app`)
- **Linux** : `appimage/…​.AppImage` et `deb/…​.deb`

> Chaque installeur se compile **sur son propre système** (un `.dmg` se fait sur macOS, un `.exe` sur
> Windows). C'est pourquoi la CI utilise trois machines.

## Produire les 3 installeurs automatiquement (CI)

Le workflow `.github/workflows/agent-release.yml` compile sur macOS, Linux et Windows puis crée une
**release GitHub (brouillon)** avec les installeurs.

- Déclenchement manuel : onglet **Actions → Agent de bureau — installeurs → Run workflow**.
- Ou par tag :
  ```bash
  git tag agent-v0.1.0 && git push origin agent-v0.1.0
  ```

## Signature (recommandé pour la production)

Sans signature, Windows (SmartScreen) et macOS (Gatekeeper) affichent un avertissement « éditeur
inconnu ». Pour l'éviter :

- **macOS** : compte Apple Developer (~99 $/an). Renseigner en secrets GitHub `APPLE_CERTIFICATE`,
  `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`
  (notarisation gérée par `tauri-action`).
- **Windows** : certificat de signature de code (OV/EV) à configurer dans `tauri.conf.json` /
  variables d'environnement du runner.

On peut distribuer non signé au début (avec l'avertissement), puis ajouter la signature.

## Distribution

Publiez les installeurs là où vos clients les téléchargent : page de téléchargement du site, ou les
**releases GitHub** produites par la CI. L'utilisateur télécharge, installe, puis saisit son code.

## Étendre l'agent

Chaque type de tâche se branche dans `main.rs`, dans le `match kind { … }` de `sync_loop`. Types gérés :

- **`notification`** → notification native du système.
- **`impression_billet`** → télécharge le billet (champ `url` interne/authentifié, `url` externe, ou
  `base64`), tente une **impression directe** (Windows : verbe *Print* ; macOS/Linux : `lp`/`lpr`),
  sinon ouvre le fichier prêt à imprimer.
- **`partage_fichier`** → récupère le fichier et l'ouvre avec l'application par défaut.
- **`emargement`** → ouvre la feuille d'émargement (champ `url`) dans le navigateur.
- **`affichage`** → ouvre une page en plein écran (champ `url`, défaut `/ecran`) sur l'écran du poste —
  pratique pour une TV d'accueil ou de salle.
- **`diffusion`** → « séance guidée » : ouvre le contenu poussé par le formateur (quiz, formulaire, vidéo,
  champ `url`) dans une fenêtre **plein écran, toujours au premier plan, non fermable** — l'élève ne peut que
  suivre. `{"end": true}` ferme la séance. Verrouillage au mieux (empêche fermer/réduire ; pas les raccourcis
  système bas niveau).

Chaque tâche est **acquittée** au serveur (succès ou erreur), et rejouée en cas de coupure réseau.
Les tâches déjà exécutées sont mémorisées (`processed.json`) : si un acquittement se perd et que le
serveur renvoie la tâche, elle est **ré-acquittée sans être rejouée** (idempotence).

## Sécurité de l'agent

L'agent applique plusieurs protections (durcissement « P0/P1 ») :

- **Jeton dans le coffre système** : le jeton d'appairage est conservé dans le gestionnaire
  d'identifiants de l'OS (Windows Credential Manager, Trousseau macOS, Secret Service Linux), pas en
  clair dans un fichier. Un ancien jeton présent dans `config.json` est migré vers le coffre puis retiré
  du fichier au démarrage suivant.
- **Ouverture de fichiers filtrée** : les fichiers reçus ne sont ouverts qu'avec l'application par
  défaut via la bibliothèque `opener` (aucune commande shell n'est construite). Les types
  **exécutables/scripts/raccourcis** (`.exe`, `.bat`, `.cmd`, `.msi`, `.js`, `.vbs`, `.ps1`, `.jar`,
  `.sh`, `.lnk`, `.dll`, `.app`, `.dmg`, `.pkg`, `.deb`, `.apk`, `.iso`, `.reg`, …) et les fichiers
  **sans extension** sont **refusés** : le fichier est supprimé et la tâche renvoie une erreur.
- **URL filtrées** : seules les URL `http://`/`https://` sont ouvertes (dans le navigateur par défaut) ;
  les autres schémas (`file:`, `javascript:`, …) sont ignorés.
- **CSP stricte** : l'interface embarquée n'autorise que ses propres ressources
  (`script-src 'self'`) ; aucun script distant ne peut s'exécuter.
- **Idempotence** : voir ci-dessus — pas de double exécution si un acquittement est perdu.

## Mises à jour automatiques (Tauri Updater)

L'agent **vérifie et installe silencieusement** une mise à jour au démarrage, puis se relance. Le
mécanisme est **inactif tant qu'une clé de mise à jour n'est pas configurée** (`check()` échoue alors
sans conséquence) — c'est volontaire : la CI compile sans clé. Cette clé « updater » (minisign) est
**distincte de la signature OS** (Apple/Windows) et ne coûte rien.

Pour **activer** les mises à jour auto :

1. **Générer une paire de clés** (une seule fois, à conserver précieusement) :
   ```bash
   npx @tauri-apps/cli@^2 signer generate -w ~/.tauri/poste-5pour100.key
   ```
   Cela crée la clé privée (`~/.tauri/poste-5pour100.key`) et la clé publique (`…​.key.pub`).
2. **Clé publique** → la coller dans `src-tauri/tauri.conf.json`, champ `plugins.updater.pubkey`
   (actuellement vide).
3. **Clé privée** → l'ajouter en **secret GitHub** du dépôt :
   `Settings → Secrets and variables → Actions` :
   - `TAURI_SIGNING_PRIVATE_KEY` = contenu du fichier `.key`
   - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` = le mot de passe choisi (laisser vide s'il n'y en a pas)
4. **Activer la production des artefacts de mise à jour** : dans `src-tauri/tauri.conf.json`, ajouter
   sous `bundle` :
   ```json
   "createUpdaterArtifacts": true
   ```
   ⚠️ Ne l'activez **qu'après** avoir ajouté le secret `TAURI_SIGNING_PRIVATE_KEY` : sinon le build CI
   échoue (clé de signature manquante). Le workflow transmet déjà ces variables d'environnement ; elles
   restent sans effet tant que `createUpdaterArtifacts` n'est pas activé.

Ensuite, chaque release publiée par la CI contient les artefacts signés et un fichier `latest.json` ;
les agents installés se mettent à jour seuls vers la dernière version. L'endpoint interrogé est
`https://github.com/ggbrestpc/poste-5pour100/releases/latest/download/latest.json`.

En plus des tâches, à chaque cycle l'agent interroge **`/api/agent/classe/shares`** : les **supports des
partages de classe ouverts** sont téléchargés automatiquement (une seule fois chacun) dans le dossier des
fichiers reçus, puis ouverts. Le bouton **« Afficher l'écran d'état »** de la fenêtre ouvre `/<serveur>/ecran`
sur l'écran du poste (mettre le navigateur en plein écran avec F11 pour une TV).

Le bouton **« Ouvrir mon espace »** ouvre l'**espace 5pour100 complet** (assistant Léa, gestion, quiz des
élèves, etc.) dans le **navigateur par défaut** : fiable et toujours fermable, la session de l'utilisateur
y persiste. (Une webview intégrée restait blanche et non fermable sous Windows ; le navigateur évite ce
problème.) La barre des tâches offre une **sortie de secours « Fermer la séance guidée »** si une séance
verrouillée reste bloquée.

Pistes suivantes : impression **ESC/POS** fine (tickets 58/80 mm) via une liaison directe à
l'imprimante, et **signature OS** (Apple/Windows) pour supprimer les avertissements « éditeur inconnu »
à l'installation — prévue après validation complète et mise en vente officielle.
