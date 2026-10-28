# Caméra HomeSmart (à fabriquer soi-même)

Une caméra 4K avec micro, dont tout le logiciel est ouvert, qui ne parle qu'à HomeSmart et à rien
d'autre sur Internet. HomeSmart la trouve seul sur le réseau, l'appaire avec un code, affiche son
image et son son, l'analyse (vision et écoute IA) et règle sa qualité.

## Matériel

| Pièce | Rôle | Prix relevé (fin 2025) |
|---|---|---|
| **Radxa ROCK 4D** (RK3576), 2 ou 4 Go | encodage matériel H.264 / H.265 jusqu'en 4K à 60 i/s, Wi-Fi 6, PoE en option, NPU 6 TOPS | ≈ 31–37 $ |
| **Radxa Camera 4K** (Sony IMX415, 8 Mpx, 4 voies MIPI CSI) | le capteur ; objectif M12 interchangeable, 88° en diagonale | ≈ 27–36 € |
| Micro : **USB** (prototype) ou **I²S** INMP441 / ICS-43434 (version finale) | le son ; la prise jack de la carte n'a pas d'entrée micro | 3–10 € |
| microSD, alimentation USB-C (ou module PoE), dissipateur, antennes Wi-Fi, boîtier imprimé en 3D | — | ≈ 20–30 € |

Pas de Raspberry Pi pour de la 4K : l'encodeur des Pi Zero 2 W / Pi 4 s'arrête au 1080p, le Pi 5
n'en a pas, et la Camera Module 3 ne dépasse pas 14 i/s en pleine définition.

**Vie privée** : prévoir un interrupteur qui coupe l'alimentation du micro (et un voyant) ; le micro
se coupe aussi depuis HomeSmart. **Nuit** : l'IMX415 voit bien en faible lumière, mais le noir
complet demande des LED infrarouges 850 nm et un filtre infrarouge amovible (non fourni par Radxa).

## Comment ça marche

```
capteur ─ ISP ─┬─ encodeur HD (H.265, 4K) ──┐
               └─ encodeur fluide (H.264) ──┤
micro ─ PCM 16 bits, 16 kHz (L16) ──────────┴─▶ MediaMTX (RTSP :8554, /main et /sub)
                                                    ▲ lecture : HomeSmart appairés seulement
homesmart-cam (agent) : lance et surveille les deux processus, API HTTP :8780, annonce mDNS
```

- **`cam/agent`** (`homesmart-cam`) tourne sur la caméra : il écrit la configuration de
  [MediaMTX](https://github.com/bluenviron/mediamtx) (MIT), lance la chaîne d'encodage, la relance
  si elle s'arrête (délai croissant, dernière erreur gardée), s'annonce en mDNS
  (`_homesmart-cam._tcp`) et répond à HomeSmart.
- **`cam/proto`** : le protocole partagé (réglages, appairage, signatures).
- Côté HomeSmart : `src/camera/agent.rs` (client), découverte dans `src/camera/discover.rs`,
  section « Caméra HomeSmart » des réglages dans l'interface.

Chaînes d'encodage (backends) :

| Backend | Où | Image | Son |
|---|---|---|---|
| `rockchip` | carte Rockchip (détectée) | GStreamer + encodeurs matériels MPP (`mpph265enc`, `mpph264enc`) | ALSA → L16 |
| `v4l2` | tout Linux avec une webcam | FFmpeg, encodage logiciel (x264 / x265) | ALSA → L16 |
| `test` | partout, même Windows | mire FFmpeg `testsrc2` en 4K | un la à 440 Hz |
| `custom` | carte pas encore prise en charge | ta commande, avec des repères `{main_url}`, `{width}`… | — |

## Sécurité, sans TLS ni compte

- **Appairage** : un code de 8 caractères (≈ 40 bits, sans 0/O/1/I/L) est tiré à l'installation.
  HomeSmart et la caméra se prouvent qu'ils le connaissent en **SPAKE2** : le code ne passe jamais
  sur le réseau et un espion ne peut pas le deviner hors ligne. Chaque essai coûte une requête ;
  après 5 échecs, la caméra refuse les essais pendant 10 minutes.
- **Ensuite**, chaque requête est **signée** (HMAC-SHA256 du secret partagé, de la méthode, du
  chemin, de l'heure, d'un nonce et du corps) : ni falsification ni rejeu. Une caméra dont
  l'horloge dérive renvoie son heure ; HomeSmart corrige l'écart et recommence.
- **RTSP** : seuls les HomeSmart appairés lisent les flux (Digest), avec un mot de passe dérivé du
  secret, qui ne circule donc jamais. La publication des flux n'est permise que depuis la caméra
  elle-même. HLS, WebRTC, RTMP, SRT et l'API de MediaMTX sont fermés.
- Sur la caméra, l'agent tourne sous un utilisateur dédié (`homesmart-cam`, groupes `video` et
  `audio`), sans droits root, et n'écrit que dans `/var/lib/homesmart-cam` (fichiers en 600).
- Supprimer la caméra de HomeSmart lui fait oublier ce HomeSmart ; `homesmart-cam reset` sur la
  caméra efface tous les appairages et tire un nouveau code.

## Installer sur la caméra

1. **Système** : Radxa OS (Debian) sur la carte microSD, Wi-Fi ou Ethernet configuré, accès SSH.
   Brancher la Camera 4K sur le connecteur **4 voies**, puis l'activer : `sudo rsetup`, Overlays,
   Manage overlays, cocher la caméra, redémarrer.
2. **L'agent** pour la carte (ARM 64 bits), au choix :
   - sur la carte : installer Rust (`curl https://sh.rustup.rs -sSf | sh`), copier ce dépôt, puis
     `cargo build --release -p homesmart-cam` ;
   - sur le PC : `cargo zigbuild --release -p homesmart-cam --target aarch64-unknown-linux-gnu`
     (avec [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) ; la compilation pour
     ARM 64 bits est vérifiée, l'édition des liens par zig pas encore).
3. Copier `homesmart-cam` avec `deploy/install.sh` et `deploy/homesmart-cam.service` dans un même
   dossier sur la carte, puis `sudo ./install.sh` : paquets GStreamer / FFmpeg / ALSA, MediaMTX
   v1.21.3 (empreinte SHA-256 vérifiée), utilisateur dédié, service systemd. Le script affiche le
   **code d'appairage**.
4. Dans HomeSmart : **Caméras, Ajouter**, choisir la caméra trouvée, taper le code, **Appairer**.
   Ou : `homesmart-cli cam pair <IP> <CODE>`.

Commandes utiles sur la caméra :

```bash
journalctl -u homesmart-cam -f                     # ce que fait l'agent
sudo -u homesmart-cam HOMESMART_CAM_CONFIG=/var/lib/homesmart-cam/config.json homesmart-cam code      # revoir le code
sudo -u homesmart-cam HOMESMART_CAM_CONFIG=/var/lib/homesmart-cam/config.json homesmart-cam pipeline  # commandes lancées
```

La configuration (`/var/lib/homesmart-cam/config.json`) permet de forcer ce que la détection
manquerait : `video_device`, `audio_device` (`hw:1,0`, `plughw:CARD=Mic`…), `has_mic`, `caps`
(définition, cadence et codecs maximaux), les chemins des outils, ou une commande `custom`.

## Essayer sans matériel (PC Windows ou Linux)

Il faut FFmpeg (HomeSmart l'installe depuis ses Préférences) et MediaMTX (posé à côté de
`homesmart-cam`, ou chemin dans `tools.mediamtx` de la configuration) :

```bash
cargo run -p homesmart-cam -- run --sim
```

L'agent diffuse une mire 4K en H.265, un flux 720p en H.264 et un la à 440 Hz en PCM 16 kHz, avec
une configuration à part (`config-sim.json`). HomeSmart le trouve comme une vraie caméra.

## Ce qui est vérifié, et ce qui reste à valider sur la carte

Vérifié (26/10/2026), sur PC avec le backend `test`, le vrai MediaMTX v1.21.3 et le vrai HomeSmart :
découverte mDNS, appairage (code faux refusé, bon code accepté), flux HD **3840×2160 H.265 à
30 i/s (≈ 8 Mbit/s)** et flux fluide 720p H.264, **son PCM 16 kHz** reçu dans les deux, capture 4K,
réglages changés à distance (1080p H.264 sans son, puis retour en 4K), suppression qui fait oublier
HomeSmart. Tests automatiques : protocole, agent (API, signatures, rejeu, verrouillage, commandes des
4 backends, relance des processus) et parcours complet dans le hub de HomeSmart.

Pas encore vérifié, faute de carte :

- la chaîne `rockchip` elle-même : nom du périphérique vidéo (`/dev/video-camera0` supposé), besoin
  éventuel du service 3A de Rockchip (`rkaiq`) pour l'exposition et la balance des blancs, noms des
  propriétés des encodeurs MPP (`bps`, `gop`, `rc-mode`, `rotation`, `width` / `height`), et le
  paquet du greffon GStreamer MPP sur Radxa OS ;
- les droits de l'utilisateur `homesmart-cam` sur `/dev/mpp_service`, `/dev/rga` et `/dev/dma_heap` ;
- le micro I²S (superposition de l'arbre des périphériques à activer) ;
- la tenue en température dans un boîtier fermé.

Si la chaîne Rockchip ne démarre pas, `homesmart-cam pipeline` montre la commande lancée et
`journalctl -u homesmart-cam` la dernière erreur ; HomeSmart l'affiche aussi dans les réglages de
la caméra.

## Licences

Agent et protocole : MIT, comme HomeSmart. Dépendances : MediaMTX (MIT), axum (MIT), mdns-sd
(MIT / Apache-2.0), spake2 (MIT / Apache-2.0), GStreamer et FFmpeg (LGPL, lancés comme programmes
séparés). Aucun modèle ni binaire n'est inclus : MediaMTX est téléchargé à l'installation, empreinte
vérifiée.
