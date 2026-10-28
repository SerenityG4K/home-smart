# HomeSmart

Application de bureau **Windows / Linux** en Rust pour piloter tes appareils **Tuya / Smart Life**
directement **en Wi-Fi sur ton réseau local** : pas de compte, pas de cloud, pas de serveur.

- Connexion TCP permanente à chaque appareil : une commande part en quelques millisecondes,
  et les changements faits ailleurs (interrupteur physique…) s'affichent en direct.
- Protocoles Tuya **3.1, 3.2, 3.3, 3.4 et 3.5** (chiffrement AES-ECB/GCM, négociation de clé de session).
- Découverte automatique des appareils sur le Wi-Fi (annonces UDP), et suivi de leur adresse IP.
- Lampes (marche, luminosité, blanc chaud/froid, couleur), prises avec mesure de consommation,
  multiprises, volets, et un éditeur générique pour tout le reste (radiateurs, ventilateurs…).
- **Garde-fous** pour les appareils à risque (chauffage…), **minuterie** intégrée aux prises,
  **programmes horaires** avec vue semaine, **suivi de consommation** (kWh, €, graphiques interactifs).
- **Maison 3D** : plan éditable de tes pièces et maquette 3D où les appareils allumés s'illuminent.
- **Caméras IP** (Imou / Dahua d'abord, et toute caméra RTSP + ONVIF) : vidéo en direct décodée sur
  le PC, orientation, positions mémorisées, captures, recherche automatique sur le réseau.
- **Aspirateurs robots**, en local : Tuya / Smart Life, Valetudo (sans compte), Xiaomi / Roborock /
  Dreame d'avant 2022 en miIO (jeton) — lancer, pause, base, signal sonore, puissance, eau, mode,
  état, batterie, erreurs, usure ; programmes et scénarios (« maison vide → nettoyer »).
- **Journal complet** : chaque action est enregistrée, quelle que soit son origine.
- **Détection des problèmes** : extinctions et allumages non demandés (avec leur cause probable :
  bouton, autre application, minuterie de la prise, coupure de courant, protection), appareils hors
  ligne ou redémarrés, défauts signalés, tension anormale, consommation inhabituelle, caméras sans
  image, analyse ou écoute arrêtées, et arrêt brutal du PC sur lequel tourne HomeSmart.
- Un seul exécutable natif (egui + OpenGL), ~10 Mo, démarrage instantané.
- Interface simple et adaptée à la fenêtre : navigation à gauche (Maison, Caméras, Énergie,
  Problèmes, Activité, Programmes), repliée en icônes quand la fenêtre est étroite ; un clic sur une tuile
  allume ou éteint, le détail s'ouvre dans un panneau posé par-dessus l'écran ; thème sombre ou
  clair (Préférences › Apparence).

## Démarrer

```bash
cargo build --release
```

Les exécutables sont dans `target/release/` :

| Exécutable | Rôle |
|---|---|
| `homesmart` (`homesmart.exe`) | l'application |
| `homesmart-cli` | la même chose en ligne de commande, pour les scripts et raccourcis |

Pour essayer sans matériel, avec des appareils simulés :

```bash
homesmart --demo
```

### Linux

Pour compiler : Rust (via [rustup](https://rustup.rs)) et un compilateur C (`sudo apt install build-essential`),
utilisé par la bibliothèque TLS de l'import Smart Life.

Dépendances d'exécution habituelles d'une app OpenGL : `libxkbcommon`, `libGL`, et `libwayland`
ou `libxcb` selon la session. Sur Debian/Ubuntu :

```bash
sudo apt install libxkbcommon0 libgl1 libwayland-client0 libxcb1
```

Pour chiffrer les visages enregistrés (vision par IA), un trousseau compatible Secret Service :
celui du bureau suffit (GNOME Keyring, KWallet, ou KeePassXC avec son intégration Secret Service).

### Pare-feu

- **Windows** : au premier lancement, autorise HomeSmart sur le réseau **privé** (sinon la découverte ne voit rien).
- **Linux** : ouvre en entrée les ports UDP **6666, 6667 et 7000** (annonces des appareils).
- Le pilotage passe par le port TCP **6668** des appareils (sortant, rien à ouvrir).

## La clé locale : le point important

Chaque appareil Tuya chiffre ses échanges avec une **clé locale** de 16 caractères. HomeSmart en a
besoin, avec l'**identifiant** de l'appareil (que la découverte trouve toute seule).

Cette clé n'est pas inscrite sur l'appareil : elle est attribuée par le cloud Tuya **au moment de
l'appairage**. Il faut donc la récupérer **une seule fois**. Ensuite tout se passe en local, et tu
peux même couper l'accès Internet des appareils dans ta box : la clé ne change plus tant que
l'appareil n'est pas réinitialisé ou ré-appairé.

Selon ta situation :

1. **Tes appareils sont dans ton app Smart Life** (le cas le plus courant) : clique sur
   **« Importer depuis Smart Life »**.
   1. Dans Smart Life : *Moi › ⚙ Paramètres › Compte et sécurité*, copie le **Code utilisateur**.
   2. Colle-le dans HomeSmart, qui affiche un QR code.
   3. Dans Smart Life : *Accueil › « + » › Scanner*, scanne le QR code et confirme.

   HomeSmart récupère tous tes appareils avec leurs clés, **ferme la session**, et ne contacte plus
   jamais le cloud. C'est le mécanisme de l'intégration Tuya officielle de Home Assistant (la demande
   d'accès s'affiche d'ailleurs au nom de « Home Assistant ») ; aucun compte développeur n'est nécessaire.
   Même chose en ligne de commande : `homesmart-cli import-cloud <code utilisateur>`.

   Ensuite, **ne supprime pas** les appareils de l'app Smart Life : les retirer les réinitialise et
   change leur clé. Tu peux simplement ne plus ouvrir l'app, ou la désinstaller.
2. **Tu as déjà les clés** (tinytuya, Home Assistant…) : glisse le `devices.json` de
   [tinytuya](https://github.com/jasonacox/tinytuya) sur la fenêtre, ou saisis la clé avec « + Ajouter ».
3. **L'appareil est neuf** : le firmware Tuya d'origine refuse de fonctionner sans une activation
   initiale par le cloud ; aucune application ne peut l'éviter. Trois options :
   - un **compte jetable**, le temps d'appairer l'appareil puis d'importer ses clés (option 1) ;
   - [tuya-cloudcutter](https://github.com/tuya-cloudcutter/tuya-cloudcutter), qui configure
     certains modèles (puces BK7231 à firmware vulnérable) avec une clé que tu choisis, sans jamais
     passer par le cloud ;
   - flasher un firmware libre (OpenBeken, ESPHome…) : l'appareil ne parle alors plus le protocole
     Tuya, et HomeSmart ne le pilote plus.

Les clés sont enregistrées en clair dans le fichier de configuration (voir plus bas) : garde-le pour toi.

## Utilisation

- **+ Ajouter** : identifiant, clé locale, nom et pièce. L'IP et la version de protocole sont
  détectées automatiquement si l'appareil s'annonce sur le réseau.
- Un appareil **détecté** mais pas encore configuré apparaît en bas du tableau de bord ; « Ajouter »
  pré-remplit son identifiant, son IP et sa version.
- Clique sur une carte pour ouvrir ses commandes, la liste de tous ses DP et ses réglages
  (renommer, pièce, IP fixe, version, clé, suppression).
- Le petit « ⚡ 23 ms » sur une carte, c'est le temps mesuré entre ta commande et la confirmation de l'appareil.

### Protection des réglages de l'appareil

Toute écriture vers une prise passe par les mêmes contrôles, dans l'app comme en ligne de commande :

- **l'étalonnage d'usine est intouchable** : coefficients de tension, de courant, de puissance et
  d'énergie (DP 22 à 25 des prises à mesure), bit de test (DP 21). Ils sont affichés en lecture
  seule, et toute écriture est refusée, même sans description des DP ;
- **une liste de choix n'accepte que les valeurs que l'appareil connaît.** Exemple réel : tes prises
  emploient en local `off` / `on` / `memory` pour « Après une coupure de courant », alors que le cloud
  annonce `power_off` / `power_on` / `last`. HomeSmart utilise le vocabulaire de la prise, et bloque
  la modification s'il ne le reconnaît pas ;
- **un nombre doit rester dans la plage prévue** (minuterie de 0 à 86 400 s…).

### Garde-fous (Réglages d'un appareil, partie « Sécurité »)

Pour un appareil à risque, comme un chauffage branché sur une prise connectée :

| Garde-fou | Où il agit |
|---|---|
| Confirmation avant d'allumer depuis HomeSmart | dans l'application |
| Extinction automatique après une durée maximale | **dans la prise** (sa minuterie), donc même PC éteint, et HomeSmart vérifie aussi |
| Coupure si la puissance dépasse un seuil (deux mesures de suite) | HomeSmart, à chaque mesure |
| Plage horaire interdite (allumage refusé, extinction si allumé) | HomeSmart, y compris pour les programmes |
| Coupure si la prise signale un défaut (surintensité, surtension, surpuissance…) | HomeSmart |
| Comportement au retour du courant (« Rester éteinte » conseillé) | **dans la prise** |

Un appareil dont le nom évoque du chauffage (« Chauffage », « Radiateur »…) se voit proposer
des réglages recommandés en un clic. Chaque intervention apparaît dans **Activité**.
Les coefficients d'étalonnage d'usine des prises sont affichés en lecture seule, et toute
écriture dessus est refusée.

### Minuterie

« Éteindre dans… » / « Allumer dans… » (15 min à 24 h) : c'est la prise elle-même qui décompte,
HomeSmart peut être fermé.

### Programmes et calendrier

Plages (« allumé de 6 h 30 à 8 h en semaine », y compris à cheval sur minuit) ou actions ponctuelles
(« éteindre à 23 h 30 »), jour par jour. La rubrique **Programmes** montre la semaine type de tous les
appareils ; un clic sur un bloc le modifie. Les programmes sont exécutés par HomeSmart : il doit
rester ouvert (même réduit). Active **Préférences › Lancer à l'ouverture de session** pour qu'il
démarre tout seul, réduit. À la fermeture, HomeSmart propose de se réduire si des programmes ou
des garde-fous sont actifs. Après une mise en veille du PC, les déclenchements manqués ne sont pas
rattrapés (pas d'allumage surprise en retard).

### Énergie

Les mesures (puissance, courant, tension) sont demandées aux prises **toutes les 2 s** par défaut
(réglable : 1, 2, 5 ou 10 s dans les préférences). Testé sur une vraie prise : elle fournit bien une
nouvelle valeur toutes les 2 s. Pour vérifier le rythme d'une prise :
`homesmart-cli watch "Multiprise - Bureau" --interval 2`.

Pour chaque prise à mesure : puissance instantanée, kWh du jour, du mois et du mois précédent,
coût estimé, courbe de puissance du jour (par tranches de 5 min) et consommation des 14 derniers
jours. La vue **Énergie** cumule tous les appareils sur 30 jours. Le prix du kWh se règle dans les
préférences. Les mesures sont intégrées par HomeSmart pendant qu'il est ouvert ; l'historique est
gardé environ 13 mois dans `energy.json`, à côté de la configuration.

### Journal (rubrique « Activité »)

Chaque action est inscrite, **sans exception**, avec son origine :

| Origine | Exemples |
|---|---|
| Toi (app) | allumer, éteindre, minuterie, réglages, programmes, garde-fous, plan de la maison, préférences, refus |
| Ligne de commande | `on`, `off`, `set`, `add`, `remove`, imports |
| Programme | allumages et extinctions programmés |
| Garde-fou | extinctions de sécurité, minuterie de sécurité |
| Appareil | changement fait ailleurs : bouton physique, autre application, minuterie de la prise ; problèmes signalés par la prise (extinction non demandée, défaut, tension) |
| Système | démarrage, arrêt, connexions et pertes de connexion, appareils détectés, changement d'IP ; appareil hors ligne, caméra sans image, arrêt brutal du PC |
| Vision IA | personnes, chutes, gestes, zones, visages, animaux (analyse des caméras) |
| Écoute IA | sons de la maison, parole transcrite (micro des caméras) |
| Scénario | appareil commandé par ce que voient ou entendent les caméras, avec la raison |

Les mesures (puissance, tension…) ne sont pas des actions et n'y figurent pas. Un curseur que l'on
fait glisser donne une seule entrée, avec la valeur finale. Le journal est conservé dans
`journal.jsonl`, à côté de la configuration ; l'onglet permet de filtrer par niveau, origine,
appareil et texte. Les notifications ne s'affichent que pour ce qui mérite ton attention
(programmes, garde-fous, scénarios, avertissements), pas pour tes propres clics. Les **alertes
critiques** (chute, personne au sol, appel à l'aide, alarme incendie, verre brisé…) donnent en plus
une **notification du système** (Windows ou Linux), même fenêtre réduite ; désactivable dans les
Préférences.

### Problèmes (détection de ce qui n'a pas été demandé)

La rubrique **Problèmes** montre ce qui arrive en ce moment, avec la cause probable, puis
l'historique de la semaine. Sur **Maison**, un bandeau les résume et la tuile concernée prend la
couleur de la gravité ; le panneau de l'appareil les détaille. Chaque problème va au **journal**
avec son origine et sa gravité, écrit tout de suite sur le disque (il doit survivre à la coupure
qu'il annonce), et sa fin aussi (« Résolu — … »). Les problèmes **graves** donnent une notification
du système (Préférences › notifications).

| Problème | Comment HomeSmart le reconnaît | Gravité |
|---|---|---|
| Extinction / allumage non demandés | la prise change d'état sans commande de HomeSmart (toi, programme, garde-fou, scénario, minuterie posée par HomeSmart, ligne de commande) | avertissement ; **grave** pour un allumage d'appareil sous garde-fous (chauffage) ; information si c'est une minuterie réglée ailleurs |
| … avec sa cause probable | bouton de la prise ou autre application ; minuterie de la prise réglée hors de HomeSmart ; **coupure de courant** (revenue éteinte après une perte de liaison, selon « Après une coupure de courant », DP 38) ; protection (DP 26) | |
| Hors ligne | plus de liaison depuis 1 min ; cause : réseau de ce PC coupé, tous les appareils perdus en même temps (box, Wi-Fi), ou prise débranchée | avertissement ; **grave** si un chauffage allumé n'a plus de minuterie de sécurité |
| Redémarrage de la prise | revenue après une perte de liaison avec sa minuterie à zéro (le garde-fou la reprogramme) | avertissement |
| Défaut signalé | bits du DP 26 : surintensité, surtension, surpuissance (**grave**), sous-tension (avertissement), sous-intensité (information) | |
| Tension anormale | mesure du DP 20 hors de ± 10 % (230 V ou 120 V, reconnu tout seul) trois fois de suite, ou ± 15 % une fois | avertissement ; **grave** au-delà de + 15 % |
| Consommation inhabituelle | allumé depuis 15 min sans jamais atteindre sa puissance habituelle (historique `energy.json`) : le chauffage allumé qui ne tire que 25 W au lieu de 1 200 W. Un radiateur dont le thermostat coupe **après** avoir chauffé n'est pas signalé | information |
| Caméra injoignable / flux vidéo perdu | plus de réponse depuis 1 min, ou plus d'image depuis 20 s alors que le flux est demandé | avertissement |
| Analyse IA / écoute arrêtées | en erreur depuis 30 s | avertissement ; **grave** si la caméra surveillait les chutes, les appels à l'aide ou le mode absence |
| Arrêt brutal de ce PC | au démarrage, d'après le journal de Windows (démarrages, événements Kernel-Power 41 et 6008 : heure exacte, ni écran bleu, ni appui sur le bouton) : coupure pendant que HomeSmart tournait, ou coupures survenues HomeSmart fermé ; HomeSmart cite le creux de tension signalé juste avant, la dernière minute de mesures des prises et la prise qui alimente ce PC | avertissement |

Un événement (extinction non demandée…) reste « en cours » jusqu'à ce que tu cliques sur
**Compris** (24 h au plus) ; un état (hors ligne, tension…) disparaît de lui-même quand tout
redevient normal. La simple perte de connexion, aussitôt rétablie, n'est qu'une information.

**Une prise qui alimente ce PC** (Réglages de l'appareil › « Alimente ce PC ») : quand elle coupe,
HomeSmart s'arrête avec le PC et ne peut rien écrire à ce moment-là. Il garde donc en permanence,
toutes les 5 s, une « boîte noire » : la dernière minute de puissance et de tension de chaque prise.
Au démarrage suivant, il dit ce qui s'est passé : « ce PC a perdu son alimentation à 20:42:31
pendant que HomeSmart tournait : Windows a redémarré à 20:50 sans arrêt normal (3 coupures en
24 h) ; juste avant, « Chauffage » a signalé une sous-tension à 20:42:09 ; dernières mesures :
« Multiprise - Bureau » 290 W et 240 V à 20:42:26 ; si la prise qui l'alimente s'est éteinte puis
rallumée seule, elle a redémarré ; s'il a fallu appuyer sur son bouton, elle a reçu un ordre
d'extinction ; si son voyant est resté allumé, c'est ce PC lui-même qui s'est arrêté ».
Réglée sur « Rester éteinte » au retour du courant, la prise ne permet pas de trancher entre les
deux : « Reprendre l'état précédent » le permet. Les coupures survenues pendant que HomeSmart était
fermé sont dites aussi, avec leur heure exacte (Windows ; sous Linux, seulement si un creux de
tension a été vu juste avant).

En démo (`--demo`), la rubrique propose de **simuler chaque panne** sur les appareils simulés
(bouton, coupure de courant, sous-tension, creux de tension, chauffage qui ne chauffe pas, caméra
sans image, arrêt brutal du PC) ; les délais y sont raccourcis à quelques secondes.

### Maison 3D

Bouton **Plan 3D** (en haut de la page Maison), choix **Plan** : crée tes pièces (automatiquement à partir des pièces de tes appareils, ou à la
main), déplace-les, redimensionne-les par leur coin, et glisse chaque appareil à sa place ; sa
pièce est mise à jour en conséquence. Onglet **Maquette 3D** : la maison en volume. Glisser pour
tourner (avec inertie), molette pour zoomer, clic sur un appareil pour ouvrir ses commandes. Les
appareils allumés brillent de leur couleur, avec leur état ou leur consommation au-dessus.
- **Présence en direct** : une pièce où une caméra analysée voit quelqu'un s'éclaire en vert, avec une
  silhouette par personne et le compte (« 2 personnes ») ; une pièce surveillée mais vide affiche
  « vide · surveillée ». Il suffit que la caméra et la pièce du plan portent le même nom.

### Graphiques

Rubrique **Énergie** et page « Consommation » de chaque prise : jour (par heure), semaine et mois
(par jour), année (par mois). En tête, ce que la période a coûté en euros et les kWh ; un bâton
par intervalle (survol : le détail en kWh et en €) ; « Ce qui consomme » classe les appareils.

### Caméras

Rubrique **Caméras**. La vidéo va directement de la caméra au PC (RTSP), elle est décodée sur place
(OpenH264) ; l'orientation et les positions passent par ONVIF. Aucun compte, aucun cloud.

- **Recherche** : HomeSmart trouve les caméras par la découverte Dahua / Imou (modèle, n° de série,
  MAC, firmware), par la découverte TP-Link (caméras Tapo et leur modèle), par WS-Discovery ONVIF et
  en sondant le port RTSP, et les caméras HomeSmart fabriquées soi-même par mDNS. Une caméra qui
  répond sur deux adresses (les Dahua / Imou gardent leur IP d'usine 192.168.1.108) n'apparaît
  qu'une fois.
- **Tapo (TP-Link)** : en local, une Tapo n'accepte pas le mot de passe du compte TP-Link. Il lui
  faut un **compte de la caméra**, à créer une fois dans l'application Tapo (la caméra, Paramètres,
  Paramètres avancés, Compte de la caméra) ; tant qu'il n'existe pas, elle garde sa vidéo (RTSP,
  port 554) et son orientation (ONVIF, port 2020) fermées, et HomeSmart le dit. Ensuite tout passe
  en local, même sans Internet. Le formulaire d'ajout se règle tout seul pour une Tapo (port ONVIF
  2020, nom du compte demandé, aide affichée). Il reste lié à TP-Link : les réglages internes de la
  caméra (Wi-Fi, mode privé, carte SD), les mises à jour (mieux vaut couper la mise à jour
  automatique : une version peut fermer l'accès local) et une réinitialisation. On peut bloquer
  l'accès Internet de la caméra dans la box : HomeSmart continue de la voir.
  - **API locale Tapo** (alarme, suivi des mouvements, carte SD) : ces réglages passent par une
    autre voie que le compte de la caméra — l'**API locale chiffrée** (HTTPS, port 443), qui
    n'accepte que l'utilisateur `admin` avec le **mot de passe du compte TP-Link** (celui de
    l'application Tapo). La caméra le vérifie elle-même, sans Internet ; HomeSmart n'en garde que
    les empreintes (MD5 et SHA-256), jamais le mot de passe. Il se renseigne dans la section
    « Caméra Tapo » des réglages (ou `cam tapo`). Les firmwares les plus récents (connexion
    « TPAP ») ne sont pas encore gérés : HomeSmart le dit. Chaque essai de mot de passe comptant
    pour le verrouillage de la caméra, un refus bloque tout nouvel essai automatique 15 minutes.
- **Caméras sans mouvement continu** (certaines Tapo) : si la caméra refuse le pivotement continu
  ONVIF, HomeSmart le note une fois au journal et la tourne par **petits pas** répétés tant que la
  commande est maintenue (5 % de la course toutes les 0,3 s, 20 s au plus sans nouvel ordre) ; le
  suivi automatique en profite aussi.
- **Ajout** : l'adresse IP et, pour une Imou, le **code de sécurité** : 8 caractères après « SC » sur
  l'étiquette sous la caméra (utilisateur `admin`). « Tester la connexion » vérifie la vidéo et ONVIF
  avant d'enregistrer.
- **Mosaïque** en flux fluide ; clic sur une caméra pour l'ouvrir.
- **Une caméra ouverte** : la vidéo, avec cinq boutons ronds posés dessus (capture, son — clic
  droit pour le volume —, positions mémorisées, HD, vidéo seule) ; à côté, le panneau
  **Surveillance**, et ses pages « Avancé » (toute la vision IA) et « Réglages de la caméra »
  (orientation, son, image, qualité vidéo, nom et connexion, informations techniques). Dans une
  fenêtre étroite, le panneau passe sous la vidéo.
- **Orientation** : pavé circulaire posé dans le coin de la vidéo, on appuie dans une direction et
  on maintient (les flèches du clavier marchent aussi) ; le centre ramène à la position d'origine.
  **Positions mémorisées** :
  clic pour y aller, clic droit pour supprimer, « Mémoriser ici » pour en créer une. HomeSmart
  détecte ce que la caméra gère : l'Imou Ranger 2C n'accepte en ONVIF que le pivotement continu
  (ni positions mémorisées ni position d'origine), ces commandes sont alors masquées.
- **Zoom** (numérique) : molette sur la vidéo pour zoomer vers le point visé (jusqu'à 8×), glisser
  pour se déplacer, double-clic ou « 1× » pour revenir à l'image entière ; une petite carte en haut
  à gauche montre la partie affichée. Au-delà de 1,5×, le direct passe tout seul en HD pour rester
  net (noté au journal) et revient au flux fluide à 1×. Simple affichage : la caméra n'est pas
  commandée. Marche aussi en relecture des enregistrements ; zones et cadres de la vision IA suivent.
- **Qualité vidéo** : résolution, cadence et débit de chaque flux, dans les limites annoncées par
  la caméra (Ranger 2C : jusqu'à 2304×1296 à 25 i/s ; Tapo C210 : 2304×1296 à 30 i/s). Attention :
  un flux en H.265 enregistré par ONVIF passe en H.264. Débit : valeurs courantes en un clic
  (512 kbit/s à 8 Mbit/s) ou valeur exacte. Beaucoup de caméras n'annoncent pas leur débit maximal
  et plafonnent sans le dire : HomeSmart relit le réglage après l'avoir envoyé et signale ce que la
  caméra a gardé. La **Tapo C210** accepte le changement de résolution en ONVIF mais garde sa
  cadence et son débit figés (déjà au maximum, 2304×1296 à 30 i/s) : HomeSmart le signale.
- **Détection de sabotage** (objectif masqué, caméra détournée, image floutée), au choix par deux
  voies, combinables (section « Détection de sabotage » des réglages, ou `cam tamper`) :
  - **l'image dans HomeSmart** : quelques mesures par seconde sur le flux fluide (champ devenu
    uniforme, détails disparus, scène entièrement changée), sans aucun modèle d'IA ni carte
    graphique ; la vue de référence se réapprend quand la caméra est orientée ;
  - **les alertes de la caméra** (ONVIF) : la caméra signale elle-même le sabotage, sans rien
    décoder sur le PC. La Tapo C210 a un détecteur intégré que HomeSmart active pour elle ; l'Imou
    Ranger 2C n'en a pas de réglable mais émet un « changement de scène » qui sert d'alerte.
  - Sensibilité réglable (faible, moyenne, élevée). Une alerte va au **journal**, aux **problèmes**
    et aux **scénarios** (déclencheur « un sabotage de caméra »).
- **Alarme** (Tapo) : sirène et lumière de la caméra. Activer l'alarme automatique (sur la
  détection de la caméra), choisir sirène et/ou lumière, le volume et la durée, ou la **déclencher
  / arrêter tout de suite**. Via la section « Caméra Tapo » ou `cam alarm`.
- **Suivi des mouvements** (Tapo) : le suivi **intégré** à la caméra (elle pivote seule pour suivre
  ce qui bouge), activable dans « Caméra Tapo » ou `cam track`. À ne pas confondre avec le suivi par
  l'**IA de HomeSmart** (onglet Vision, `cam follow`), qui oriente la caméra en ONVIF.
- **Performances** : la chaîne vidéo tient 2560×1440 à 60 i/s sans perte (décodage ≈ 4,5 ms par
  image sur un cœur) ; la vue détaillée affiche la cadence reçue et la cadence affichée.
- **Verrouillage** : après plusieurs mots de passe erronés, une Imou bloque l'accès quelques
  minutes (RTSP 403). HomeSmart ne réessaie donc jamais tout seul après un refus d'identifiants :
  il attend un clic sur « Réessayer » ou un changement de réglages (après un verrouillage, un seul
  essai automatique au bout de 10 min). Plusieurs lectures simultanées, elles, sont acceptées.
- **Image** : luminosité, contraste, saturation, netteté, et vision de nuit, WDR (le « HDR » des
  caméras) et contre-jour **si la caméra les propose en ONVIF**. La Ranger 2C ne publie que les
  quatre premiers : son WDR ne se règle pas en local.
- **Capture** : PNG dans `Images\HomeSmart` (bouton « Dossier » pour l'ouvrir).
- **Enregistrements** (bascule « Direct / Enregistrements » en haut de la vue détaillée) : relecture
  de la carte SD de la caméra, en lecture seule.
  - Bande des 7 derniers jours (point bleu = il y a de la vidéo), résumé du jour (nombre de
    séquences, durée totale de vidéo).
  - **Frise** de la journée : chaque séquence à sa place, à la seconde près ; molette pour zoomer
    (jusqu'à 5 min), glisser pour se déplacer, survol pour l'heure exacte, clic pour relire à
    partir de là ; boutons « Ajuster » (cadre sur les séquences) et « Journée ».
  - **Liste des séquences** (la plus récente en haut, par heure) avec vignette, heure, durée.
  - **Relecture continue** : d'une séquence à l'autre sans attente (la caméra saute les trous),
    heure de l'image affichée en permanence, pause (`Espace` ou clic sur l'image), séquence
    précédente / suivante (`←` `→`).
  - **Son** de la séquence (bouton haut-parleur ou `S`, volume à côté) : le micro enregistré avec
    l'image, calé sur elle, sur la même connexion à la caméra. L'écoute reste active d'une
    séquence à l'autre et se coupe quand on quitte la vue ; une séquence muette est signalée.
  - **Export** de la séquence en `.mp4` (H.265 + son, sans ré-encodage ; un son G.711, que le
    .mp4 refuse, est converti en AAC) dans `Vidéos\HomeSmart`, avec progression et bouton « Annuler ». La caméra diffuse à vitesse réelle : compter la durée
    de la séquence.
  - Formatage, rétention et mode d'enregistrement de la carte passent par une interface que la
    Ranger 2C verrouille : ils restent hors de portée en local.
  - **Tapo** : la caméra donne l'**index** de sa carte SD (jours et séquences, par son API locale),
    affiché dans la liste. Mais la **lecture et l'export** de ces séquences passent par un flux de
    relecture propriétaire et chiffré, que HomeSmart ne sait pas ouvrir : sur une Tapo, seul l'index
    est disponible (pas de relecture ni d'export). La parole à distance (parler par le haut-parleur
    de la caméra) passe par ce même flux et n'est donc pas possible non plus.
- Le flux n'est ouvert que tant qu'il est affiché ; quitter l'onglet le ferme au bout de 2-3 s. Au
  repos, HomeSmart vérifie seulement une fois par minute que la caméra répond.
- Un flux HD en **H.265** (c'est le cas des Imou) se décode avec **FFmpeg**. S'il manque,
  HomeSmart affiche le flux fluide et propose, sous la vidéo et dans les Préférences, un bouton
  **« Installer FFmpeg »** : paquet officiel `Gyan.FFmpeg` via winget, sans droits administrateur,
  environ une minute, progression affichée ; la caméra repasse en HD toute seule ensuite. Sous
  Linux, la commande à lancer est proposée (bouton « Copier »). FFmpeg peut aussi être posé à côté
  de `HomeSmart.exe`. En ligne de commande : `homesmart-cli cam ffmpeg --install`.
- Tout est journalisé : ouverture et fermeture du direct (avec la durée), orientations, positions,
  captures, tests, réglages (le mot de passe lui-même n'est jamais écrit), caméra injoignable.
- **Son** : AAC, G.711, et PCM 16 bits sans perte (L16, celui des caméras HomeSmart).

### Caméra HomeSmart (fabriquée soi-même)

Une caméra 4K avec micro, à construire (≈ 90 à 110 € de pièces : Radxa ROCK 4D + Camera 4K Sony
IMX415 + micro USB ou I²S), dont tout le logiciel est ouvert et qui ne parle qu'à HomeSmart. Guide
complet (matériel, installation, sécurité, essai sans matériel) : [`cam/README.md`](cam/README.md).

- Sur la caméra, l'agent **`homesmart-cam`** lance le serveur RTSP (MediaMTX) et la chaîne
  d'encodage matériel : flux **HD** jusqu'en 4K (H.265 ou H.264) et flux **fluide** 720p H.264
  pour l'analyse IA, avec le **micro en PCM 16 bits à 16 kHz** (sans perte, au format de l'écoute).
- **Ajout** : HomeSmart la trouve tout seul (mDNS) ; on tape le **code d'appairage** donné à
  l'installation (« ABCD-EFGH »). Le code ne passe jamais sur le réseau (SPAKE2), chaque requête
  ensuite est signée, et le mot de passe RTSP est dérivé du secret né de l'appairage. Après 5 codes
  faux, la caméra refuse les essais 10 minutes.
- **Réglages de la caméra** : état (encodage, température, relances), définition, cadence, débit
  et codec des deux flux, micro et son gain, image retournée (caméra au plafond). Pas d'ONVIF :
  tout passe par l'agent, et tout est journalisé.
- **Supprimer** la caméra de HomeSmart lui fait oublier ce HomeSmart.
- En ligne de commande : `homesmart-cli cam pair <IP> <CODE>`, `homesmart-cli cam agent <caméra>`
  (`--res 4k --fps 25 --codec h265 --mic off --gain 6 --rotate on`).

### Prises et ampoules TP-Link (Tapo, Kasa)

En ligne de commande pour l'instant (`homesmart-cli tplink`) :

- `tplink scan` : les appareils TP-Link du réseau (découverte UDP 20002 des appareils récents, UDP
  9999 des anciennes Kasa) : modèle, MAC, protocole, et « pas encore installé » pour un appareil
  d'usine.
- `tplink info|on|off <IP>`, `tplink brightness <IP> <%>` : les prises et ampoules **Tapo** (et Kasa
  récentes) parlent **KLAP**, un protocole local chiffré (AES-128, poignée de main signée) qui
  demande l'**e-mail et le mot de passe du compte TP-Link** (`--email`, ou les variables
  `HOMESMART_TPLINK_EMAIL` et `HOMESMART_TPLINK_PASSWORD`) : la prise garde une empreinte de ces
  identifiants depuis son installation et ne contacte pas le cloud pour autant. Les **anciennes
  Kasa** (HS100, HS110…) répondent sans identifiants (protocole TCP 9999). Allumage et extinction
  vont au journal.
- Pas encore dans l'interface : les prises TP-Link n'ont ni carte, ni garde-fous, ni programmes.
- Vérifié : la découverte sur un vrai réseau (Tapo C210 trouvée) ; KLAP et le protocole Kasa
  seulement contre des appareils simulés, faute de prise TP-Link sous la main.

### Vision par IA (en local)

Panneau **Surveillance** d'une caméra ouverte : un interrupteur, l'état de l'analyse et trois
choix simples (chute, appel à l'aide, visage connu). Tout le reste est dans **Avancé**, rangé par
thème : « Ce que l'IA repère », « Réactions » (suivi, photos), « Écoute du micro », « Réglages
avancés » (image analysée, analyses par seconde, calcul, précision), zones, scénarios et visages
connus. Tout tourne sur le PC : aucune image ne sort.

- **Personnes et postures** : chaque personne est détectée, suivie d'une image à l'autre, et son
  squelette (17 points) est estimé. Cadres, noms et squelettes sont dessinés sur la vidéo.
- **Actions** : arrivée et départ d'une personne, **chute**, puis **personne au sol** si elle
  reste immobile (alerte critique), et **relevée**. Une chute, c'est : passer brusquement de
  debout à couché (tête nettement plus bas), ou une descente brusque de la tête et des hanches
  (chute vers la caméra ou dos à elle, où le corps au sol paraît encore « debout » à l'image) ;
  puis **rester au sol 1,5 s**. S'allonger lentement, s'asseoir, s'agenouiller, ramasser un objet
  n'en sont pas. Une personne au sol, moins bien vue par le modèle (score faible), garde sa piste :
  une détection faible prolonge une piste confirmée mais n'en ouvre jamais. Une chute ne rend pas
  la silhouette plus haute (au-delà de +25 %) ni bien plus étroite (moins de 0,6×) : sinon, la piste
  est passée à une autre silhouette (le buste seul d'une personne, puis son corps entier ; une
  autre personne toute proche) ; et « couchée » d'après la seule forme de la boîte (squelette peu
  sûr) ne compte pas si la personne debout était déjà coupée par le bas de l'image. Mesuré sur
  1 548 vidéos réelles : deux fois moins de fausses chutes dans la vie courante, aucune vraie chute
  perdue (voir « Qualité mesurée »).
- **Gestes** (squelette) : **main levée** (tenue un instant), **deux bras levés** tenus 2 s =
  signal d'appel à l'aide (alerte critique) — seulement chez une personne debout ou assise : couchée,
  « au-dessus de la tête » ne veut rien dire à l'image (sur les vidéos de chute, tous les « bras
  levés » venaient de personnes au sol).
- **Immobilité** (au choix : non, 10 min, 30 min, 1 h) : personne qui ne bouge plus du tout.
- **Violence** (désactivée par défaut) : coups répétés entre deux personnes — un poignet ou une
  cheville qui part vite vers une personne toute proche et l'atteint, au moins 4 fois en 4 s, sous le
  haut de sa silhouette (les accolades et les « tape là » se font à hauteur de tête). Alerte critique,
  photo, scénario « des coups ». Le coup de poing amical dans l'épaule ne compte pas (trop haut),
  une tape isolée non plus ; rien n'est cherché au-delà de 5 personnes à l'image (sport collectif,
  fête), et un coup de pied ne compte que si celui qui frappe est entier dans l'image. Avec
  l'**écoute des sons**, l'alerte attend 4 s : si l'on entend rire autour des coups (sans cri ni
  pleurs), c'est une « chamaillerie probable », notée au journal sans alerte. Réglée pour ne pas
  crier au loup, sur 1 548 vraies vidéos : **aucune fausse alerte** sur les 480 scènes calmes
  d'origine (accolades, « tape là », poignées de main, vie courante) ; sur 481 jeux et gestes
  amicaux filmés sur YouTube (Kinetics-700 : batailles d'oreillers, chatouilles, lutte, boxe, bras
  de fer, massages…), 11 fausses alertes, 9 avec les rires (boxe 4, lutte 3, massage, poignée de
  main) ; 4 sur 99 en capoeira et plaquages. En échange, 13 % seulement des 440 scènes violentes
  trouvées (18 % à 15 analyses/s, 5 % avec la précision élevée, presque rien sous 8). Lutte et
  boxe pour de faux, sans rires, restent impossibles à distinguer d'une vraie bagarre à l'image.
- **Suivre la personne** (caméras motorisées) : la caméra tourne pour garder la personne au centre
  (la plus grande, puis toujours la même), s'arrête si elle disparaît, revient à sa position de
  départ 10 s après l'avoir perdue, et fait une pause de 10 s dès que tu la pilotes toi-même. Sur
  une Ranger 2C : cible centrée en 1,2 s, sans dépassement ni va-et-vient. « Suivre aussi en
  hauteur » est désactivé par défaut : l'inclinaison de la Ranger 2C ne répond pas en ONVIF, et
  une commande qui en contient est ignorée en entier. Chaque mouvement est au journal.
- **Animaux** (chat, chien, oiseau, cheval, mouton, vache, animal sauvage) et **véhicules**
  (voiture, vélo, moto, camion, bus ; désactivés par défaut, utiles dehors) : apparition et départ,
  cadres sur la vidéo. Analysés deux fois par seconde au plus, même si les personnes le sont à 30.
  Chaque sorte a son seuil, réglé sur COCO (voir « Qualité mesurée ») ; un même animal vu à la fois
  « chat » et « chien », ou un même véhicule vu à la fois « voiture » et « camion », ne compte qu'une
  fois, sous la sorte la plus sûre (camions sur COCO : précision 0,56 → 0,65, images vides prises
  pour un camion 24 → 13 ; un camion est parfois annoncé comme voiture).
- **Deuxième avis sur les personnes** : quand le modèle des animaux est installé, une silhouette au
  score modeste (reflet, manteau, poster) n'est retenue que si ce second modèle voit lui aussi
  quelqu'un au même endroit ; une piste déjà admise n'a plus besoin d'avis. Sur COCO, mesuré comme
  dans l'application : deux fois moins d'images vides prises pour une présence de jour (53 → 27 sur
  1 020), presque trois fois moins en vision nocturne simulée (73 → 27), pour 1 % de vraies
  personnes en moins.
- **Photo des événements** (désactivée par défaut) : un PNG dans `Images\HomeSmart\Vision` à
  chaque événement marquant (arrivée, inconnu, chute, bras levés, animal…), au plus une toutes les
  5 s sauf alerte ; son nom est noté dans le journal.
- **Derniers événements** de la caméra dans la section, et **en vue maintenant** (personnes,
  animaux, véhicules).
- **Présence aujourd'hui** : minutes où quelqu'un était en vue, heure par heure (24 barres), gardées
  60 jours dans `presence.json` (que des durées) ; `homesmart-cli vision presence Salon --date …`.
- **Analyses par seconde** : 2 à 30, **8 par défaut** (chutes : 74 % trouvées à 8, 60 % à 4 ; au-delà de la cadence de la caméra, chaque image n'est
  analysée qu'une fois). Les règles se mesurent en durées, pas en nombre d'images : elles se
  comportent pareil à 2 ou à 30 analyses par seconde.
- **Zones** (porte, couloir, lit…), dessinées directement sur la vidéo : « Ajouter une zone », un
  clic par sommet, puis double-clic (ou Entrée, ou clic sur le premier point) pour fermer ; Retour
  arrière retire le dernier point, Échap annule. On lui donne un nom et, si on veut, un délai de
  **présence prolongée** (0 = jamais). Le journal note l'entrée, la sortie et la présence prolongée
  dans chaque zone, d'après le point d'appui au sol de la personne ; la zone occupée s'éclaire sur
  la vidéo. Liste « Zones » de la section : renommer (nom et délai), supprimer. Les zones restent valables en
  fluide comme en HD (coordonnées relatives à l'image) ; un tracé dont les côtés se croisent est refusé.
- **Visages** : détection, puis reconnaissance des personnes **que tu enregistres** (« Enregistrer
  ce visage » quand quelqu'un passe, ou **glisse une photo** JPEG/PNG sur la fenêtre ; plusieurs vues améliorent la reconnaissance — la photo n'est pas gardée, seule son empreinte). Une personne
  n'est nommée qu'après plusieurs reconnaissances concordantes, qui font au moins un tiers des
  visages nets vus sur sa piste : un inconnu qui reste longtemps ne finit plus par ressembler par
  hasard à un habitant (foyers simulés sur LFW : 6 % → 2 % d'inconnus nommés à tort sur un long
  passage, habitants toujours nommés) ; sinon « Visage inconnu ». Les
  empreintes (données biométriques : 128 nombres par vue, aucune photo) restent dans `faces.json`
  à côté de la configuration, **chiffrées** (voir plus bas) ; celles des inconnus ne sont jamais
  gardées. Renommer, supprimer : « Visages connus ».
- **Précision** (sous « Calcul ») : standard (RTMO-s) ou élevée (RTMO-m, téléchargé au premier
  choix). Mesuré : de jour presque pareil (AP50 0,905 → 0,909), **de nuit nettement mieux** — 29 % de
  fausses personnes en moins sur des images vides, chutes trouvées 68 % → 72 % avec 4 % de fausses
  alertes au lieu de 11 %. Deux fois plus de calcul sur le processeur (34 → 70 ms par image), à
  peine plus sur la carte graphique (13 → 17 ms).
- **Suivi robuste** : une détection faible ne prolonge une piste que là où était la personne (ou à
  ses pieds, quand elle tombe) ; un objet douteux du décor ne peut plus « capturer » la piste d'une
  personne passée près de lui, ce qui donnait de fausses chutes sur une vraie caméra de nuit. Une
  piste n'est confirmée (« Personne détectée ») qu'avec au moins deux détections sûres : un objet
  du décor vu une fois au-dessus du seuil, puis faiblement, ne devient plus une personne. Une piste
  **immobile** depuis 10 s (personne assise ou couchée, souvent mal vue ; silhouette du décor :
  photo, affiche, écran) attend 60 s sans la voir avant de la dire partie, et ne se reprend que sur
  place : plus de « Personne détectée » / « n'est plus en vue » (et « Visage inconnu ») toutes les
  minutes pour la même silhouette, comme vu sur une vraie caméra. Elle n'est ni montrée ni comptée
  comme présence pendant cette attente.
- **Calcul** : carte graphique si possible (DirectML sous Windows : NVIDIA, AMD, Intel), sinon
  processeur, avec repli automatique. Mesuré sur une Ranger 2C (image 2304×1296) : personnes 9 à
  13 ms, visage 4,5 ms, empreinte 1 à 2 ms, animaux 7 ms sur carte graphique ; personnes 36 ms,
  visage 6 ms, empreinte 6 ms sur processeur seul. À 30 analyses demandées, la carte graphique suit
  la caméra (≈ 23 analyses/s réelles sur sa relecture).
- **Flux analysé** : HD par défaut (visages reconnus de plus loin) ou fluide (moins de calcul).
  L'analyse garde le flux ouvert elle-même ; le journal l'indique (« Système », « Vision IA »).
- Tout est journalisé (source « Vision IA ») ; chute et personne au sol sont des alertes critiques.

**Modèles** (téléchargés une fois depuis leur source officielle, empreinte SHA-256 vérifiée, rangés
dans `%LOCALAPPDATA%\HomeSmart\models` ou `~/.local/share/homesmart/models` ; jamais dans le dépôt) :

| Modèle | Rôle | Licence | Source |
|---|---|---|---|
| RTMO-s (body7) | personnes + squelettes | Apache-2.0 | [OpenMMLab MMPose](https://github.com/open-mmlab/mmpose/tree/main/projects/rtmo) |
| RTMO-m (body7), au choix | idem, « Précision : élevée » (83 Mo) | Apache-2.0 | idem |
| YuNet (2023mar) | visages + 5 points | MIT | [OpenCV Zoo](https://github.com/opencv/opencv_zoo/tree/main/models/face_detection_yunet) |
| SFace (2021dec) | empreintes de visage | Apache-2.0 | [OpenCV Zoo](https://github.com/opencv/opencv_zoo/tree/main/models/face_recognition_sface) |
| YOLOX-s | animaux + véhicules (COCO) | Apache-2.0 | [Megvii YOLOX](https://github.com/Megvii-BaseDetection/YOLOX) |

Moteur : [ONNX Runtime](https://onnxruntime.ai) (MIT) via le crate [`ort`](https://ort.pyke.io). Les
binaires précompilés demandent un processeur x86-64 avec AVX2 (2013 et après). Sous Windows,
`DirectML.dll` est livré à côté de l'exécutable ; sans lui, Windows 11 fournit le sien.

**Protection des empreintes** (`faces.json`, tout le contenu : noms et empreintes) :

| Système | Chiffrement | Qui peut relire le fichier |
|---|---|---|
| Windows | DPAPI (`CryptProtectData`, portée utilisateur) | ton compte Windows, sur ce PC |
| Linux avec trousseau | AES-256-GCM ; la clé (256 bits tirés au hasard) est rangée dans le trousseau de ta session (Secret Service) | ta session, une fois le trousseau déverrouillé |
| Linux sans trousseau | **aucun** : fichier en clair, droits `600`, avertissement dans le journal | ton compte (et root) |

- **Protège contre** la copie du fichier seul (sauvegarde, synchronisation, clé USB, disque lu sur
  une autre machine) et contre les autres comptes de l'ordinateur.
- **Ne protège pas contre** un programme lancé sous ton compte, ou un administrateur, pendant que ta
  session est ouverte : il peut demander le déchiffrement comme HomeSmart le fait.
- Copié sur un autre PC ou sous un autre compte, le fichier est illisible : il faut réenregistrer les
  visages. De même si un administrateur **réinitialise** ton mot de passe Windows (le changer
  normalement ne gêne pas), ou si le trousseau Linux est effacé.
- Une galerie illisible (fichier abîmé, autre compte, trousseau verrouillé) n'est jamais écrasée : le
  journal le signale (source « Système »), personne n'est reconnu, et enregistrer un visage est refusé
  tant que le fichier n'est pas relu (trousseau déverrouillé) ou supprimé.
- Une ancienne galerie en clair est relue puis réécrite chiffrée à sa première lecture ; le journal
  l'indique. L'ancien contenu en clair n'est pas effacé physiquement du disque (SSD, journal du
  système de fichiers).
- Sans trousseau, HomeSmart ne chiffre pas pour la forme : la clé serait posée à côté du fichier, ce
  qui ne protégerait de rien.

> Filmer chez soi relève de l'usage personnel ; ne filme ni la voie publique ni chez les voisins,
> préviens les personnes qui travaillent chez toi, et n'enregistre le visage que de personnes
> d'accord.

### Écoute par IA (en local)

Dans **Surveillance › Avancé**, partie « Écoute du micro ». Désactivée par défaut, caméra
par caméra. **Le son n'est jamais enregistré** : il est analysé en mémoire par blocs de quelques
secondes, puis oublié ; seuls vont au journal (source « Écoute IA ») les sons reconnus et, si tu
l'as activée, la parole transcrite.

- **Sons de la maison** : alarme incendie (détecteur de fumée), alarme ou sonnerie, **bris de
  verre**, cri, pleurs (bébé ou adulte), détonation, aboiements, miaulements, sonnette, on frappe,
  porte qui claque, téléphone, toux, ronflements, bruits de pas, sirène. Alarme incendie, verre
  brisé et détonation sont des alertes critiques (avec photo si « Photo des événements » est
  activée) ; un même son n'est signalé qu'une fois par 30 s.
- **Transcrire la parole** : la voix est détectée, chaque phrase est transcrite en français, et
  le texte va au journal ; « au secours », « à l'aide », « aidez-moi »… déclenchent une **alerte
  critique**. Les phrases « fantômes » de Whisper (« Sous-titres réalisés par… », boucles) et ses
  descriptions de sons (« \*rires\* », « [Musique] ») sont écartées : sur 176 bruits sans parole
  (toux, rires, éternuements, sirènes…), aucune phrase n'arrive au journal.
- **Son amélioré** (bouton à côté du volume, en direct comme en relecture ; activé par défaut) :
  filtre des basses (90 Hz), volume automatique qui ramène la voix vers −20 dBFS sans faire monter le
  bruit de fond (les micros de caméra sont faibles : crête à 1 % sur une Imou), limiteur doux,
  rééchantillonnage de qualité vers la carte son, fondus de 5 ms au lieu des claquements quand le
  réseau saccade. L'analyse par IA, elle, reçoit toujours le son brut.
- Niveau du micro et derniers sons ou phrases entendus dans la section ; il faut que la caméra ait
  un micro actif (sinon la section le dit).
- Calcul : sons ≈ 7 ms par fenêtre de 3 s (une toutes les 0,5 s) ; transcription 8 fois plus
  rapide que le temps réel sur le processeur, un peu plus sur la carte graphique.

| Modèle | Rôle | Licence | Source |
|---|---|---|---|
| CED-mini (int8, export sherpa-onnx) | 527 sortes de sons (AudioSet) | Apache-2.0 (poids aussi publiés en CC-BY-4.0) | [CED](https://github.com/RicherMans/CED), [k2-fsa](https://huggingface.co/k2-fsa/sherpa-onnx-ced-mini-audio-tagging-2024-04-19) |
| Noms AudioSet | étiquettes des sons | CC-BY-4.0 | [Google AudioSet](https://research.google.com/audioset/) |
| Silero VAD v6 | détection de la voix | MIT | [snakers4/silero-vad](https://github.com/snakers4/silero-vad) |
| Whisper small (int8, export onnx-community) | parole → texte | MIT | [OpenAI Whisper](https://github.com/openai/whisper), [onnx-community](https://huggingface.co/onnx-community/whisper-small) |

≈ 264 Mo en tout (10 Mo pour les seuls sons), téléchargés et vérifiés comme ceux de la vision.

> Écouter chez soi relève, comme filmer, de l'usage personnel : préviens les personnes présentes
> (invités, personnes qui travaillent chez toi) avant d'activer la transcription.

### Mode absence (alarme)

En haut de la rubrique **Caméras** : « Mode absence » (armé, un bandeau le rappelle sur tous les écrans). Tant qu'il est activé, **toute personne vue**
par une caméra analysée déclenche une **alerte critique** (« Intrusion ? Personne détectée… ») avec
une **photo** (même si « Photo des événements » est désactivé) et une notification du système. Un
visage connu est nommé dans le journal juste après. Le scénario « une intrusion (mode absence) »
peut, par exemple, allumer toutes les lumières.

### Scénarios (la caméra commande tes appareils)

**Surveillance › Avancé** d'une caméra, partie « Scénarios » : « Quand [déclencheur], alors [allumer /
éteindre] [appareil] », éventuellement **seulement sur une plage horaire** (22:00 → 07:00…).

| Déclencheur | D'où il vient |
|---|---|
| quelqu'un arrive | personne qui entre dans le champ (piste confirmée) |
| personne depuis N min | pièce vide : aucune personne en vue depuis N minutes, **tant que l'analyse tourne** (sans elle, HomeSmart ne sait pas si la pièce est vide) ; une seule fois par absence |
| une chute | chute confirmée (personne restée au sol) |
| un appel à l'aide | deux bras levés, ou « au secours », « à l'aide »… entendu (si la transcription est activée) ; un simple cri ne suffit pas (enfants qui jouent) |
| une alarme incendie | détecteur de fumée entendu (« Sons de la maison ») |
| du verre brisé | bris de verre entendu (« Sons de la maison ») |
| une intrusion (mode absence) | quelqu'un arrive pendant que le mode absence est activé |
| des coups (violence) | coups répétés entre deux personnes (vision + son) |
| un sabotage de caméra | objectif masqué, caméra détournée ou image floutée (voir « Détection de sabotage ») |
| mode absence activé / désactivé | la maison entière : départ ou retour (pratique pour l'aspirateur) |

Un scénario **sans caméra** vaut pour toutes les caméras analysées : « personne depuis N min »
veut alors dire **la maison est vide** (personne sur aucune caméra), « quelqu'un arrive » vaut pour
n'importe laquelle. Les aspirateurs robots ont leurs propres actions : **nettoyer** ou **rentrer à
la base**.

- Exemples : éteindre le radiateur quand la chambre est vide depuis 30 min (proposé d'office si un
  appareil à risque est dans la même pièce que la caméra) ; allumer la lumière du couloir quand
  quelqu'un arrive la nuit ; allumer toutes les lumières en cas de chute ou d'appel à l'aide.
- Un même scénario sur événement ne se déclenche qu'une fois par minute.
- **Garde-fous respectés** : un appareil qui demande une confirmation pour être allumé (chauffage)
  ne l'est jamais par un scénario — le journal le dit.
- Tout est journalisé (source « Scénario ») : la raison (« Chambre : personne depuis 30 min →
  éteindre « Radiateur » »), puis la commande. Supprimer la caméra ou l'appareil supprime ses scénarios.

### Aspirateurs robots

Les robots s'affichent dans **Maison** comme les autres appareils : la tuile s'allume pendant le
nettoyage, montre l'activité et la batterie ; un clic lance le nettoyage (ou le met en pause), un
clic droit ouvre le panneau : activité en grand, batterie, surface et durée, boutons **Nettoyer /
Pause**, **Base**, **Arrêter**, **Trouver** (signal sonore), puis ce que le robot sait régler
(**puissance d'aspiration**, **mode**, **débit d'eau**, façons de nettoyer), l'**usure** des
brosses et du filtre, les **automatismes** et les **programmes**.

| Famille | Comment HomeSmart le joint | Ce qu'il faut une fois |
|---|---|---|
| Tuya / Smart Life (catégorie « sd ») | protocole local Tuya, comme les prises | la clé locale (import Smart Life par QR code, comme les autres appareils) |
| Valetudo (Dreame, Roborock, Xiaomi… au micrologiciel libre) | API REST locale (HTTP) | rien : ni compte ni clé ; identifiants seulement si l'accès HTTP de Valetudo est protégé |
| miIO (Xiaomi, Roborock, Dreame fabriqués avant 2022) | UDP 54321 local, chiffré | le **jeton** de l'appareil (32 caractères), extrait une fois depuis le compte Xiaomi |

- **Dreame récents** (application Dreamehome) : **aucune commande locale**, le robot ne répond
  qu'au cloud de Dreame. Pour s'en passer, il faut installer **Valetudo** sur le robot (souvent en
  l'ouvrant ; voir valetudo.cloud). Le cloud Dreamehome n'est pas proposé : son accès imite
  l'application Dreame (empreinte réseau, secrets de l'application) et peut cesser à tout moment.
- **Ajout** : Maison › « + » › « Aspirateur robot » : recherche sur le réseau (annonce mDNS de
  Valetudo, poignée de main miIO), bouton **Tester**, puis **Ajouter**.
- **Journal** : chaque commande avec son origine (toi, ligne de commande, programme, scénario) ;
  les nettoyages lancés hors de HomeSmart (bouton du robot, son application, son programme
  interne) et la fin de chaque nettoyage (« Nettoyage terminé : 23 m² en 41 min, retour à la
  base ») sont notés « Appareil ».
- **Problèmes** : les erreurs du robot ouvrent « Aspirateur en difficulté » avec ce qu'il faut
  faire (brosse bloquée, bac plein, robot coincé, soulevé, capteurs, batterie…), avec leur
  gravité : information (bac plein, réservoir), avertissement (blocages), critique (batterie
  défaillante, notification du système) ; « Résolu » quand l'erreur disparaît. Un robot qui ne
  répond plus passe en « Hors ligne ».
- **Automatismes** (panneau du robot) : nettoyer quand la maison est vide depuis N min (caméras
  analysées), nettoyer quand le mode absence est activé, rentrer à la base quand quelqu'un arrive
  ou à la fin du mode absence. **Programmes** : « nettoyer à 10:00 en semaine », ou une plage
  (« nettoyage de 10:00 à 11:30 », retour à la base à la fin).
- Démo (`--demo`) : un robot Tuya et un robot Valetudo simulés ; Problèmes › « Simuler une panne »
  y provoque une brosse bloquée ou un robot coincé.

### Qualité mesurée sur des données réelles

`homesmart-cli vision eval …` et `homesmart-cli listen eval …` mesurent les modèles sur des jeux
publics annotés, téléchargés à part (jamais dans le dépôt). Résultats sur le PC de développement :

| Mesure | Jeu | Résultat |
|---|---|---|
| Personnes (RTMO-s, moyennes et grandes) | COCO val2017, 1 952 images | AP50 0,905 ; au seuil 0,5 : précision 0,93, rappel 0,83 ; avec le deuxième avis, comme l'application : précision 0,95, rappel 0,82, 27 images vides sur 1 020 prises pour une présence (de nuit : rappel 0,72, 27 aussi) |
| Squelettes | idem, 2 259 personnes | OKS moyen 0,85 (84 % ≥ 0,75) |
| Chats / chiens (YOLOX-s) | idem | AP50 0,89 / 0,76 ; aux seuils de l'application : précision 0,91 / 0,83, rappel 0,86 / 0,75 |
| Visages (YuNet + SFace) | LFW, 6 000 paires | au seuil 0,36 : 0,07 % d'acceptés à tort, 1 % de refusés, 99,5 % juste ; visages de 34 px : 98,8 % ; nuit simulée : 98,0 % |
| Visages, décision finale | LFW, 400 foyers simulés (4 habitants enregistrés avec 3 photos, 4 inconnus de passage) | habitants nommés : 100 % ; inconnus nommés à tort : 1,8 % sur un passage de 4 visages, 2,0 % sur 200 (6,3 % avant le réglage du vote) ; de nuit 1,9 % (7,9 %) |
| Chutes (règles sur les squelettes) | CAUCAFall + UR Fall, 150 vidéos réelles | 74 % des chutes trouvées, 3 % de fausses alertes sur 70 gestes du quotidien (avant réglage : 50 % et 23 %) |
| Toute la chaîne d'alertes (suivi, chutes, gestes, violence) | 1 548 vidéos réelles (chutes, bagarres, jeux, vie courante : 738 vidéos, 1,4 h) | chutes : 59 sur 80, fausses chutes dans la vie courante 21 → 13 (de nuit : 50 sur 80, 26 → 17) ; « bras levés » à tort 37 → 16, plus aucun chez une personne au sol ; arrivées en double 144 → 130 ; violence 55 sur 440 (57 avant), 15 fausses alertes sur 1 060 |
| Sons de la maison (CED-mini) | ESC-50 (176 extraits) + 289 phrases (FLEURS) | 6 à 8 sur 8 trouvés selon le son, ≤ 2 fausses alertes sur 450 extraits neutres ; presque autant avec quelqu'un qui parle aussi fort par-dessus |
| Parole → texte (Whisper small) | FLEURS français, 40 phrases | 10 % de mots erronés au calme ; 23 % avec bruits de la maison à 10 dB ; 15 % en micro 8 kHz (G.711) |

Essayé et écarté d'après ces mesures : un débruitage (filtre de Wiener) avant Whisper (12 % d'erreurs
au calme au lieu de 10, 25 % au lieu de 23 dans le bruit : Whisper préfère le son brut) ; des fenêtres
d'écoute de 1 s (bien moins justes que 3 s). Les modèles audio quantifiés tournent sur le processeur :
CED en int8 fait planter DirectML, et Whisper n'y gagne que 30 %. Pour les chutes, abaisser les seuils
de descente ou de maintien au sol gagne 1 à 2 chutes pour 2 à 3 fausses alertes de plus : les seuils
sont restés ; une règle sur les pieds (qui descendent dans une chute vers la caméra) a été écartée,
car elle aurait aussi masqué les chutes dos à la caméra, absentes des vidéos. Le deuxième avis sur
les personnes n'a pas de meilleur réglage que 0,8 / 0,2.

Jeux utilisés : [COCO](https://cocodataset.org) (annotations CC-BY-4.0), [LFW](http://vis-www.cs.umass.edu/lfw/),
[ESC-50](https://github.com/karolpiczak/ESC-50) (CC-BY-NC), [FLEURS](https://huggingface.co/datasets/google/fleurs)
(CC-BY-4.0), [CAUCAFall](https://data.mendeley.com/datasets/7w7fccy7ky) (CC-BY-4.0) et
[UR Fall](https://fenix.ur.edu.pl/~mkepski/ds/uf.html) (chutes) ; pour la violence et la chaîne
d'alertes, [AIRTLab](https://github.com/airtlab/A-Dataset-for-Automatic-Violence-Detection-in-Videos),
[Surveillance Camera Fight](https://github.com/seymanurakti/fight-detection-surv-dataset) (MIT),
[UT-Interaction](https://cvrc.ece.utexas.edu/SDHA2010/Human_Interaction.html) et des extraits de
[Kinetics-700](https://github.com/cvdfoundation/kinetics-dataset) (jeux, sports, gestes amicaux).

### Interface dans le terminal (TUI)

```bash
homesmart-cli tui            # tes appareils et caméras
homesmart-cli tui --demo     # appareils et caméras simulés
```

Plein écran, au clavier, avec le même moteur que l'application (garde-fous, programmes, énergie,
journal ; les actions y sont notées « Ligne de commande ») :

- **Appareils** (F1) : état et mesure de chaque appareil, détail de tous ses points de données.
  `Espace` allume ou éteint (avec confirmation si un garde-fou le demande), `←` `→` règlent la
  luminosité, `1`-`9` les sorties d'une multiprise, `m` la minuterie, `r` redemande l'état.
- **Caméras** (F2) : état, flux, orientation avec `Maj` + flèches, capture avec `c`, et `v` pour
  un **aperçu vidéo en couleurs dans le terminal** (demi-blocs, ~12 images/s ; Windows Terminal
  ou tout terminal en couleurs 24 bits).
- **Énergie** (F3) : puissance, kWh et coût du jour et du mois, histogramme heure par heure.
- **Journal** (F4) : toutes les actions, en direct.
- **Aspirateurs** (F5) : activité, batterie, erreurs, usure ; `Espace` nettoie (ou met en pause),
  `b` renvoie à la base, `x` arrête, `t` fait sonner le robot, `p` change la puissance.

`Tab` change d'onglet, `q` quitte. Si l'application est déjà ouverte, la TUI le signale : les
prises n'acceptent souvent qu'une connexion à la fois.

### Ligne de commande

```bash
homesmart-cli scan                          # appareils Tuya présents sur le Wi-Fi
homesmart-cli add bf1234... "clé16caractères" --name "Lampe salon" --room Salon
homesmart-cli import-cloud <code>           # import unique depuis Smart Life (QR code dans le terminal)
homesmart-cli import devices.json           # import tinytuya
homesmart-cli list
homesmart-cli status "Lampe salon"
homesmart-cli on "Lampe salon"
homesmart-cli set "Lampe salon" 22 500      # DP 22 = luminosité (10 à 1000)
homesmart-cli watch "Multiprise - Bureau"   # mesures en direct

homesmart-cli cam scan                      # caméras du réseau (Imou / Dahua, ONVIF, RTSP)
homesmart-cli cam add 192.168.1.164 --name Salon   # le code de sécurité est demandé au clavier
homesmart-cli cam test Salon                # vidéo + ONVIF
homesmart-cli cam info Salon                # modèle, firmware, profils vidéo, positions
homesmart-cli cam snapshot Salon            # capture HD (--sub : flux fluide, -o fichier.png)
homesmart-cli cam move Salon gauche --ms 600
homesmart-cli cam goto Salon "Entrée"       # position mémorisée
homesmart-cli cam home Salon
homesmart-cli cam encoder Salon --fps 25 --kbps 4096   # sans option : affiche le réglage
homesmart-cli cam watch Salon --hd          # cadence, débit, temps de décodage, processeur
homesmart-cli cam recordings Salon          # séquences de la carte SD aujourd'hui (--date 2026-10-19)
homesmart-cli cam replay Salon --start "2026-10-20 20:52:59" --end "2026-10-20 20:53:44"
homesmart-cli cam export Salon --start "2026-10-20 20:52:59" --end "2026-10-20 20:53:44"
homesmart-cli cam tamper Salon --image on --sensitivity moyenne   # détection de sabotage (sans option : état)
homesmart-cli cam tapo Salon                # Tapo : mot de passe du compte TP-Link (demandé au clavier), puis état
homesmart-cli cam alarm Salon --start       # déclenche l'alarme (sans option : affiche ; --on/--siren/--light/--volume/--duration ; --stop)
homesmart-cli cam track Salon on            # suivi des mouvements intégré à la caméra (Tapo)

homesmart-cli vac scan                      # robots Valetudo et miIO du réseau (sans rien modifier)
homesmart-cli vac add-valetudo 192.168.1.59 --name "Aspirateur" --room Salon
homesmart-cli vac add-miio 192.168.1.60 --token <32 caractères> --name Roborock
homesmart-cli vac list
homesmart-cli vac status Aspirateur         # activité, batterie, réglages possibles, erreurs, usure
homesmart-cli vac start Aspirateur          # aussi : pause, stop, dock, locate
homesmart-cli vac run Aspirateur fan:max    # aussi : mode:…, water:…, pattern:…

homesmart-cli vision models --install       # modèles d'IA : téléchargement vérifié (≈ 75 Mo)
homesmart-cli vision info                   # accélération utilisée (--cpu : processeur seul)
homesmart-cli vision replay Salon --start "2026-10-20 20:52:59" --end "2026-10-20 20:53:44"   # --objects : aussi YOLOX
homesmart-cli vision eval coco --data <dossier>     # qualité mesurée (aussi : faces, identify, falls, images)
homesmart-cli vision eval alarms --data <dossier> --cache <dossier>   # toute la chaîne d'alertes sur les vidéos (--night, --model m)

homesmart-cli listen models --install       # modèles de l'écoute (≈ 264 Mo)
homesmart-cli listen tag son.wav            # sons reconnus dans un fichier
homesmart-cli listen transcribe parole.wav  # parole → texte (voix détectée, puis Whisper)
homesmart-cli listen eval sounds --data <dossier>   # aussi : speech (--snr 10, --phone)
```

Beaucoup d'appareils n'acceptent **qu'une connexion locale à la fois** : si l'application est
ouverte, la CLI peut être refusée par l'appareil (et inversement).

## Ressources

Mesuré sur la version optimisée, 5 appareils dont 2 prises à mesure interrogées toutes les 2 s,
sur un processeur à 16 cœurs :

| | Processeur | Mémoire |
|---|---|---|
| Fenêtre visible, sans interaction | 0,8 % d'un cœur | ~115 Mo |
| Fenêtre réduite | 0,16 % d'un cœur | ~90 Mo |

L'interface ne se redessine que lorsqu'une valeur change (ou toutes les 30 s pour l'horloge) ;
quand la fenêtre n'a pas le focus, ces redessins sont regroupés toutes les 3 s, et rien n'est
dessiné quand elle est réduite. Les animations et la 3D ne calculent que pendant qu'elles bougent. La mémoire sert surtout au contexte graphique OpenGL et aux
polices ; le réseau représente quelques centaines d'octets toutes les 2 s par prise.

## Configuration

Un simple fichier JSON, partagé par l'app et la CLI :

- Windows : `%APPDATA%\HomeSmart\homesmart.json`
- Linux : `~/.config/homesmart/homesmart.json`
- ailleurs : option `--config` ou variable `HOMESMART_CONFIG`

```json
{
  "devices": [
    {
      "id": "bf0123456789abcdefghij",
      "key": "xxxxxxxxxxxxxxxx",
      "name": "Lampe du salon",
      "room": "Salon",
      "ip": "192.168.1.112",
      "version": "3.4"
    }
  ]
}
```

`kind` (`light`, `plug`, `switch`, `cover`, `vacuum`, `generic`) force le type si la détection
automatique se trompe ; `vacuums` liste les aspirateurs robots hors Tuya (Valetudo, miIO, avec
leur adresse et, pour miIO, leur jeton) ; `schema` reprend le `mapping` de tinytuya pour afficher des libellés lisibles ;
`"powers_pc": true` marque la prise qui alimente le PC où tourne HomeSmart. À côté de la
configuration, `session.json` (rafraîchi toutes les 5 s, avec la dernière minute de mesures des
prises) permet de reconnaître au démarrage une session terminée net.

## Organisation du code

| Fichier | Contenu |
|---|---|
| `src/protocol/` | protocole Tuya : trames 55AA/6699, AES-ECB/GCM, HMAC, négociation 3.4/3.5 |
| `src/device.rs` | connexion à un appareil (lecture et écriture en parallèle) |
| `src/discovery.rs` | découverte UDP (ports 6666, 6667, 7000) |
| `src/hub.rs` | une tâche par appareil : connexion permanente, battement de cœur, reconnexion |
| `src/cloud.rs` | import unique des clés depuis Smart Life (connexion QR, requêtes chiffrées) |
| `src/kinds.rs` | reconnaissance des types d'appareils et formats Tuya (couleurs, mesures, défauts, minuterie) |
| `src/automation.rs` | garde-fous et programmes horaires (fonctions pures, testées) |
| `src/energy.rs` | historique de consommation (intégration de la puissance, cumuls) |
| `src/journal.rs` | journal permanent de toutes les actions |
| `src/problems.rs` | détection des problèmes : règles pures et testées (extinctions non demandées et leur cause, hors ligne, tension, consommation, arrêt brutal du PC) |
| `src/hub/problems.rs` | leur application aux appareils, aux caméras et à la session précédente ; essais contre des prises et une caméra simulées |
| `src/protect.rs` | chiffrement au repos des données sensibles (DPAPI sous Windows, trousseau Secret Service sous Linux) |
| `src/house.rs` | plan de la maison (pièces, position des appareils) |
| `src/camera/` | caméras : flux RTSP + décodage (`stream.rs`), ONVIF (`onvif.rs`), recherche (`discover.rs`), caméra simulée (`sim.rs`), caméras HomeSmart (`agent.rs`) |
| `cam/proto/` | protocole des caméras HomeSmart : réglages, appairage SPAKE2, requêtes signées |
| `cam/agent/` | `homesmart-cam`, l'agent qui tourne sur la caméra (serveur RTSP, encodage, API, mDNS) et son installation (`deploy/`) |
| `src/hub/cameras.rs` | une tâche par caméra : flux ouvert à la demande, présence, journal |
| `src/vacuum/` | aspirateurs robots : modèle commun (état, commandes, erreurs), Tuya « sd » (`tuya.rs`), Valetudo (`valetudo.rs`), miIO Roborock et MIoT (`miio.rs`), recherche (`scan.rs`), robots simulés (`sim.rs`) |
| `src/hub/vacuums.rs` | un acteur par robot hors Tuya ; vue commune, commandes journalisées, fin de nettoyage, erreurs → problèmes |
| `src/gui/` | l'interface egui (`three.rs` : petit moteur 3D logiciel, `fx.rs` : effets et animations) |
| `src/sim.rs` | appareils simulés (démo et tests) |
| `src/bin/cli.rs` | la ligne de commande |

```bash
cargo test   # dont des échanges complets contre un appareil simulé (3.1 → 3.5) et une caméra simulée (RTSP + ONVIF)
cargo test --workspace   # avec l'agent des caméras HomeSmart et leur protocole
```
