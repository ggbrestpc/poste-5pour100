# Poste 5pour100 — agent de bureau

Logiciel léger (Windows / macOS / Linux) qui relie un ordinateur à un espace **5pour100** et exécute
les tâches reçues : notifications, impression de billets, partage de fichiers et émargement.
Construit avec **Tauri 2** (Rust + une petite interface web) : binaire léger, sans Electron.

> Ce dépôt ne contient que **l'agent** (le logiciel installé sur le poste). La plateforme et son
> serveur restent privés. L'agent ne contient aucune donnée ni aucun secret : il ne fait rien tant
> qu'il n'a pas été relié à un espace par un **code d'appairage**.

## Télécharger

Les installeurs sont publiés dans l'onglet **[Releases](../../releases)** :

- **Windows** : `.msi` ou `.exe` (programme d'installation)
- **macOS** : `.dmg`
- **Linux** : `.AppImage` ou `.deb`

> Application non signée au départ : Windows (SmartScreen) et macOS (Gatekeeper) peuvent afficher un
> avertissement « éditeur inconnu ». C'est normal ; la signature s'ajoute ensuite.

## Se connecter

1. Dans votre espace 5pour100 : **Agent de bureau → Appairer un poste** → un **code** (valable 15 min).
2. Ouvrez l'agent, collez le code, validez. Le poste est relié ; il reçoit ensuite ses tâches en
   arrière-plan (barre des tâches), même après une coupure réseau passagère.

Vos données restent chez vous : l'agent ne stocke qu'un jeton d'accès local, jamais renvoyé.

## Construire soi-même / contribuer à la CI

Voir [`agent/README.md`](agent/README.md) : prérequis, compilation locale, génération des installeurs
par GitHub Actions (workflow **« Agent de bureau — installeurs »**) et signature.

## Licence

© 5pour100 — tous droits réservés. Voir [`LICENSE`](LICENSE).
