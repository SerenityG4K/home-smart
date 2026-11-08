# HomeSmart

*[Lire en français](README.fr.md)* · HomeSmart's interface is in French for now: on-screen labels are quoted
as they appear, followed by their English meaning.

A **Windows / Linux** desktop application written in Rust to control your **Tuya / Smart Life** devices
directly **over Wi-Fi on your local network**: no account, no cloud, no server.

- A persistent TCP connection to each device: a command goes out in a few milliseconds,
  and changes made elsewhere (physical switch…) show up live.
- Tuya protocols **3.1, 3.2, 3.3, 3.4 and 3.5** (AES-ECB/GCM encryption, session key negotiation).
- Automatic discovery of devices on the Wi-Fi (UDP broadcasts), and tracking of their IP address.
- Lights (on/off, brightness, warm/cool white, color), energy-monitoring plugs,
  power strips, shutters/blinds, and a generic editor for everything else (heaters, fans…).
- **Safeguards** for risky devices (heating…), the plugs' built-in **timer**,
  **schedules** with a week view, **energy tracking** (kWh, €, interactive charts).
- **3D house**: an editable floor plan of your rooms and a 3D model in which devices that are on light up.
- **IP cameras** (Imou / Dahua first, and any RTSP + ONVIF camera): live video decoded on
  the PC, pan/tilt, saved positions, snapshots, automatic search on the network.
- **Robot vacuums**, locally: Tuya / Smart Life, Valetudo (no account), pre-2022 Xiaomi / Roborock /
  Dreame over miIO (token) — start, pause, dock, beep, suction power, water, mode,
  status, battery, errors, wear; schedules and scenarios ("empty house → clean").
- **Complete log**: every action is recorded, whatever its source.
- **Problem detection**: unrequested switch-offs and switch-ons (with their likely cause:
  button, another app, the plug's timer, power cut, protection), devices offline
  or restarted, reported faults, abnormal voltage, unusual consumption, cameras with no
  picture, stopped analysis or listening, and a sudden shutdown of the PC HomeSmart runs on.
- A single native executable (egui + OpenGL), ~10 MB, instant startup.
- A simple interface that adapts to the window: navigation on the left (Maison (Home), Caméras (Cameras),
  Énergie (Energy), Problèmes (Problems), Activité (Activity), Programmes (Schedules)), collapsed to icons
  when the window is narrow; clicking a tile turns the device on or off, and its details open in a panel
  laid over the screen; dark or light theme, in Préférences › Apparence (Preferences › Appearance).

## Getting started

```bash
cargo build --release
```

The executables are in `target/release/`:

| Executable | Role |
|---|---|
| `homesmart` (`homesmart.exe`) | the application |
| `homesmart-cli` | the same thing on the command line, for scripts and shortcuts |

To try it without hardware, with simulated devices:

```bash
homesmart --demo
```

### Linux

To build: Rust (via [rustup](https://rustup.rs)), a C compiler, used by the TLS library
of the Smart Life import, and the ALSA headers for camera audio. On Debian/Ubuntu:

```bash
sudo apt install build-essential pkg-config libasound2-dev
```

The usual runtime dependencies of an OpenGL app: `libxkbcommon`, `libGL`, and `libwayland`
or `libxcb` depending on the session. On Debian/Ubuntu:

```bash
sudo apt install libxkbcommon0 libgl1 libwayland-client0 libxcb1
```

To encrypt device keys and passwords, and saved faces (AI vision),
a Secret Service-compatible keyring: your desktop's own is enough (GNOME Keyring, KWallet, or
KeePassXC with its Secret Service integration).

### Firewall

- **Windows**: on first launch, allow HomeSmart on **private** networks (otherwise discovery sees nothing).
- **Linux**: open inbound UDP ports **6666, 6667 and 7000** (device broadcasts).
- Control goes through the devices' TCP port **6668** (outbound, nothing to open).

## The local key: the important part

Every Tuya device encrypts its traffic with a 16-character **local key**. HomeSmart needs it,
along with the device's **ID** (which discovery finds on its own).

This key is not printed on the device: it is assigned by the Tuya cloud **at pairing
time**. So you only need to retrieve it **once**. After that, everything happens locally, and you
can even cut the devices' Internet access in your router: the key no longer changes as long as
the device is not reset or re-paired.

Depending on your situation:

1. **Your devices are in your Smart Life app** (the most common case): click
   **« Importer depuis Smart Life »** (Import from Smart Life).
   1. In Smart Life: *Me › ⚙ Settings › Account and Security*, copy the **User Code**.
   2. Paste it into HomeSmart, which displays a QR code.
   3. In Smart Life: *Home › "+" › Scan*, scan the QR code and confirm.

   HomeSmart retrieves all your devices with their keys, **closes the session**, and never contacts
   the cloud again. This is the mechanism of Home Assistant's official Tuya integration (in fact, the access
   request is shown under the name "Home Assistant"); no developer account is needed.
   Same thing on the command line: `homesmart-cli import-cloud <user code>`.

   Afterwards, **do not remove** the devices from the Smart Life app: removing them resets them and
   changes their key. You can simply stop opening the app, or uninstall it.
2. **You already have the keys** (tinytuya, Home Assistant…): drag the `devices.json` from
   [tinytuya](https://github.com/jasonacox/tinytuya) onto the window, or enter the key with « + Ajouter » (+ Add).
3. **The device is new**: the stock Tuya firmware refuses to work without an initial activation
   through the cloud; no application can get around that. Three options:
   - a **throwaway account**, just long enough to pair the device and then import its keys (option 1);
   - [tuya-cloudcutter](https://github.com/tuya-cloudcutter/tuya-cloudcutter), which sets up
     some models (BK7231 chips with vulnerable firmware) with a key you choose, without ever
     going through the cloud;
   - flash an open-source firmware (OpenBeken, ESPHome…): the device then no longer speaks the Tuya
     protocol, and HomeSmart can no longer control it.

The keys are not written to the configuration file: they are kept apart, encrypted, in
`homesmart.secrets.json` (see "Keys and passwords" below).

## Usage

- **+ Ajouter** (+ Add): ID, local key, name and room. The IP and protocol version are
  detected automatically if the device announces itself on the network.
- A device that is **detected** but not yet configured appears at the bottom of the dashboard; « Ajouter » (Add)
  pre-fills its ID, its IP and its version.
- Click a card to open its controls, the list of all its DPs and its settings
  (rename, room, static IP, version, key, delete).
- The small « ⚡ 23 ms » on a card is the time measured between your command and the device's confirmation.

### Protecting the device's settings

Every write to a plug goes through the same checks, in the app as well as on the command line:

- **the factory calibration is off-limits**: voltage, current, power and
  energy coefficients (DP 22 to 25 on energy-monitoring plugs), test bit (DP 21). They are shown
  read-only, and any write is refused, even without a DP description;
- **a choice list only accepts the values the device knows.** A real example: your plugs
  locally use `off` / `on` / `memory` for « Après une coupure de courant » (After a power cut), while the cloud
  advertises `power_off` / `power_on` / `last`. HomeSmart uses the plug's vocabulary, and blocks
  the change if it does not recognize it;
- **a number must stay within its intended range** (timer from 0 to 86,400 s…).

### Safeguards (a device's Réglages (Settings), « Sécurité » (Safety) section)

For a risky device, such as a heater plugged into a smart plug:

| Safeguard | Where it acts |
|---|---|
| Confirmation before switching on from HomeSmart | in the application |
| Automatic switch-off after a maximum duration | **in the plug** (its timer), so even with the PC off, and HomeSmart checks too |
| Cut-off if the power exceeds a threshold (two readings in a row) | HomeSmart, at every reading |
| Forbidden time window (switch-on refused, switched off if on) | HomeSmart, including for schedules |
| Cut-off if the plug reports a fault (overcurrent, overvoltage, overpower…) | HomeSmart |
| Behavior when power comes back (« Rester éteinte » (Stay off) recommended) | **in the plug** |

A device whose name suggests heating (« Chauffage » (Heating), « Radiateur » (Radiator)…) is offered
recommended settings in one click. Every intervention appears in **Activité** (Activity).
The plugs' factory calibration coefficients are shown read-only, and any
write to them is refused.

### Timer

« Éteindre dans… » (Turn off in…) / « Allumer dans… » (Turn on in…) (15 min to 24 h): the plug itself
does the countdown, so HomeSmart can be closed.

### Schedules and calendar

Time ranges ("on from 6:30 to 8:00 on weekdays", including ranges spanning midnight) or one-off actions
("turn off at 23:30"), day by day. The **Programmes** (Schedules) section shows the typical week of all
devices; clicking a block edits it. Schedules are run by HomeSmart: it must
stay open (even minimized). Turn on **Préférences › Lancer à l'ouverture de session** (Preferences › Launch at login)
so that it starts on its own, minimized. When you close it, HomeSmart offers to minimize itself if schedules or
safeguards are active. After the PC has been asleep, missed triggers are not
caught up (no surprise late switch-on).

### Energy

Readings (power, current, voltage) are requested from the plugs **every 2 s** by default
(adjustable: 1, 2, 5 or 10 s in the preferences). Tested on a real plug: it does provide a
new value every 2 s. To check a plug's rate:
`homesmart-cli watch "Multiprise - Bureau" --interval 2`.

For each energy-monitoring plug: instantaneous power, kWh for today, this month and last month,
estimated cost, today's power curve (in 5-min slots) and consumption over the last 14
days. The **Énergie** (Energy) view adds up all devices over 30 days. The price per kWh is set in the
preferences. Readings are integrated by HomeSmart while it is open; the history is
kept for about 13 months in `energy.json`, next to the configuration.

### Log (« Activité » (Activity) section)

Every action is recorded, **without exception**, with its source:

| Source | Examples |
|---|---|
| Toi (You, in the app) | switching on, switching off, timer, settings, schedules, safeguards, house plan, preferences, refusals |
| Ligne de commande (Command line) | `on`, `off`, `set`, `add`, `remove`, imports |
| Programme (Schedule) | scheduled switch-ons and switch-offs |
| Garde-fou (Safeguard) | safety switch-offs, safety timer |
| Appareil (Device) | change made elsewhere: physical button, another app, the plug's timer; problems reported by the plug (unrequested switch-off, fault, voltage) |
| Système (System) | startup, shutdown, connections and lost connections, detected devices, IP change; device offline, camera with no picture, sudden PC shutdown |
| Vision IA (AI vision) | people, falls, gestures, zones, faces, animals (camera analysis) |
| Écoute IA (AI listening) | household sounds, transcribed speech (camera microphones) |
| Scénario (Scenario) | device controlled by what the cameras see or hear, with the reason |

Readings (power, voltage…) are not actions and do not appear there. Dragging a slider
produces a single entry, with the final value. The log is kept in
`journal.jsonl`, next to the configuration; the tab lets you filter by level, source,
device and text. Notifications only appear for things that deserve your attention
(schedules, safeguards, scenarios, warnings), not for your own clicks. **Critical
alerts** (fall, person on the floor, call for help, fire alarm, broken glass…) also trigger
a **system notification** (Windows or Linux), even with the window minimized; this can be turned off in
Préférences (Preferences).

### Problems (detecting what was not requested)

The **Problèmes** (Problems) section shows what is happening right now, with the likely cause, then
the week's history. On **Maison** (Home), a banner summarizes them and the affected tile takes on the
color of the severity; the device's panel gives the details. Each problem goes to the **log**
with its source and severity, written to disk immediately (it has to survive the power cut
it announces), and so does its end (« Résolu — … » (Resolved — …)). **Serious** problems trigger a system
notification (Préférences › notifications (Preferences › notifications)).

| Problem | How HomeSmart recognizes it | Severity |
|---|---|---|
| Extinction / allumage non demandés (Unrequested switch-off / switch-on) | the plug changes state without a command from HomeSmart (you, schedule, safeguard, scenario, timer set by HomeSmart, command line) | warning; **serious** for a switch-on of a device under safeguards (heater); information if it is a timer set elsewhere |
| … with its likely cause | the plug's button or another app; the plug's timer set outside HomeSmart; **power cut** (came back off after a lost link, according to « Après une coupure de courant » (After a power cut), DP 38); protection (DP 26) | |
| Hors ligne (Offline) | no link for 1 min; cause: this PC's network down, all devices lost at the same time (router, Wi-Fi), or plug unplugged | warning; **serious** if a heater that is on no longer has a safety timer |
| Redémarrage de la prise (Plug restart) | came back after a lost link with its timer at zero (the safeguard reprograms it) | warning |
| Défaut signalé (Fault reported) | DP 26 bits: overcurrent, overvoltage, overpower (**serious**), undervoltage (warning), undercurrent (information) | |
| Tension anormale (Abnormal voltage) | DP 20 reading outside ±10% (230 V or 120 V, detected automatically) three times in a row, or ±15% once | warning; **serious** above +15% |
| Consommation inhabituelle (Unusual consumption) | on for 15 min without ever reaching its usual power (`energy.json` history): the heater that is on but draws only 25 W instead of 1,200 W. A radiator whose thermostat cuts out **after** heating up is not flagged | information |
| Caméra injoignable / flux vidéo perdu (Camera unreachable / video stream lost) | no response for 1 min, or no picture for 20 s while the stream is requested | warning |
| Analyse IA / écoute arrêtées (AI analysis / listening stopped) | in error for 30 s | warning; **serious** if the camera was watching for falls, calls for help or away mode |
| Arrêt brutal de ce PC (Sudden shutdown of this PC) | at startup, from the Windows event log (boots, Kernel-Power 41 and 6008 events: exact time, neither a blue screen nor a press of the power button): power cut while HomeSmart was running, or power cuts that happened while HomeSmart was closed; HomeSmart mentions the voltage dip reported just before, the last minute of plug readings and the plug that powers this PC | warning |

An event (unrequested switch-off…) stays « en cours » (ongoing) until you click
**Compris** (Got it), for 24 h at most; a state (offline, voltage…) clears by itself when everything
is back to normal. A mere loss of connection, restored right away, is only information.

**A plug that powers this PC** (the device's Réglages (Settings) › « Alimente ce PC » (Powers this PC)): when it cuts out,
HomeSmart stops along with the PC and cannot write anything at that moment. So it constantly keeps,
every 5 s, a "black box": the last minute of power and voltage for each plug.
On the next startup, it tells you what happened: « ce PC a perdu son alimentation à 20:42:31
pendant que HomeSmart tournait : Windows a redémarré à 20:50 sans arrêt normal (3 coupures en
24 h) ; juste avant, « Chauffage » a signalé une sous-tension à 20:42:09 ; dernières mesures :
« Multiprise - Bureau » 290 W et 240 V à 20:42:26 ; si la prise qui l'alimente s'est éteinte puis
rallumée seule, elle a redémarré ; s'il a fallu appuyer sur son bouton, elle a reçu un ordre
d'extinction ; si son voyant est resté allumé, c'est ce PC lui-même qui s'est arrêté » (this PC lost
power at 20:42:31 while HomeSmart was running: Windows restarted at 20:50 without a normal shutdown
(3 power cuts in 24 h); just before, "Chauffage" reported an undervoltage at 20:42:09; last readings:
"Multiprise - Bureau" 290 W and 240 V at 20:42:26; if the plug that powers it switched off and then back
on by itself, it restarted; if its button had to be pressed, it received a switch-off command; if its
LED stayed on, it is this PC itself that shut down).
When set to « Rester éteinte » (Stay off) for when power comes back, the plug cannot tell the
two apart: « Reprendre l'état précédent » (Restore previous state) can. Power cuts that happened while HomeSmart was
closed are reported too, with their exact time (Windows; on Linux, only if a voltage
dip was seen just before).

In demo mode (`--demo`), the section offers to **simulate each failure** on the simulated devices
(button, power cut, undervoltage, voltage dip, heater that does not heat, camera
with no picture, sudden PC shutdown); delays there are shortened to a few seconds.

### 3D house

**Plan 3D** (3D plan) button (at the top of the Maison (Home) page), **Plan** (Floor plan) option: create your rooms (automatically from your devices' rooms, or by
hand), move them, resize them by their corner, and drag each device into place; its
room is updated accordingly. **Maquette 3D** (3D model) tab: the house in three dimensions. Drag to
rotate (with inertia), scroll wheel to zoom, click a device to open its controls. Devices
that are on glow in their color, with their state or their consumption above them.
- **Live presence**: a room where an analyzed camera sees someone lights up in green, with one
  silhouette per person and the count (« 2 personnes » (2 people)); a room that is monitored but empty shows
  « vide · surveillée » (empty · monitored). All it takes is for the camera and the plan's room to have the same name.

### Charts

The **Énergie** (Energy) section and each plug's « Consommation » (Consumption) page: day (by hour), week and month
(by day), year (by month). At the top, what the period cost in euros and the kWh; one bar
per interval (hover: the detail in kWh and €); « Ce qui consomme » (What uses power) ranks the devices.
### Cameras

The **Caméras** (Cameras) section. Video goes straight from the camera to the PC (RTSP) and is decoded
on the spot (OpenH264); pan/tilt and preset positions go through ONVIF. No account, no cloud.

- **Search**: HomeSmart finds cameras through Dahua / Imou discovery (model, serial number, MAC,
  firmware), TP-Link discovery (Tapo cameras and their model), ONVIF WS-Discovery and by probing the
  RTSP port, and HomeSmart cameras (DIY) through mDNS. A camera that answers on two addresses
  (Dahua / Imou cameras keep their factory IP 192.168.1.108) appears only once.
- **Tapo (TP-Link)**: locally, a Tapo does not accept the TP-Link account password. It needs a
  **camera account**, created once in the Tapo app (the camera, Settings, Advanced Settings, Camera
  Account); until that account exists, the camera keeps its video (RTSP, port 554) and its pan/tilt
  (ONVIF, port 2020) closed, and HomeSmart says so. After that, everything runs locally, even
  without Internet. The add form adjusts itself for a Tapo (ONVIF port 2020, account name requested,
  help shown). What stays tied to TP-Link: the camera's internal settings (Wi-Fi, privacy mode, SD
  card), updates (better to turn off automatic updates: a new version can close local access) and a
  reset. You can block the camera's Internet access in your router: HomeSmart still sees it.
  - **Tapo local API** (alarm, motion tracking, SD card): these settings go through a different
    channel than the camera account — the **encrypted local API** (HTTPS, port 443), which only
    accepts the `admin` user with the **TP-Link account password** (the one used in the Tapo app).
    The camera checks it itself, without Internet; HomeSmart keeps only its hashes (MD5 and
    SHA-256), never the password. It is entered in the « Caméra Tapo » (Tapo camera) section of the
    settings (or `cam tapo`). The most recent firmwares ("TPAP" connection) are not supported yet:
    HomeSmart says so. Since every password attempt counts toward the camera's lockout, a rejection
    blocks any new automatic attempt for 15 minutes.
- **Cameras without continuous movement** (some Tapo models): if the camera rejects ONVIF continuous
  panning, HomeSmart notes it once in the log and turns the camera in repeated **small steps** as
  long as the control is held (5% of the travel every 0.3 s, at most 20 s without a new command);
  automatic tracking benefits from this too.
- **Adding**: the IP address and, for an Imou, the **safety code**: the 8 characters after "SC" on
  the label under the camera (user `admin`). « Tester la connexion » (Test connection) checks the
  video and ONVIF before saving.
- **Grid view** in the smooth stream; click a camera to open it.
- **An open camera**: the video, with five round buttons overlaid on it (snapshot, sound —
  right-click for the volume —, preset positions, HD, video only); next to it, the **Surveillance**
  (Monitoring) panel, and its pages « Avancé » (Advanced: all of the AI vision) and « Réglages de la
  caméra » (Camera settings: pan/tilt, sound, image, video quality, name and connection, technical
  information). In a narrow window, the panel moves below the video.
- **Pan/tilt**: a circular pad sits in the corner of the video; press in a direction and hold (the
  keyboard arrows work too); the center returns to the home position.
  **Preset positions**:
  click to go there, right-click to delete, « Mémoriser ici » (Save here) to create one. HomeSmart
  detects what the camera supports: over ONVIF, the Imou Ranger 2C only accepts continuous panning
  (no preset positions and no home position), so those controls are hidden.
- **Zoom** (digital): scroll wheel on the video to zoom toward the point under the cursor (up to
  8×), drag to move around, double-click or « 1× » to go back to the whole image; a small map in the
  top-left corner shows the displayed area. Beyond 1.5×, the live view switches to HD on its own to
  stay sharp (noted in the log) and goes back to the smooth stream at 1×. Display only: the camera
  is not controlled. Also works during playback of recordings; AI vision zones and boxes follow.
- **Video quality**: resolution, frame rate and bitrate of each stream, within the limits advertised
  by the camera (Ranger 2C: up to 2304×1296 at 25 fps; Tapo C210: 2304×1296 at 30 fps). Caution:
  saving an H.265 stream through ONVIF switches it to H.264. Bitrate: common values in one click
  (512 kbit/s to 8 Mbit/s) or an exact value. Many cameras do not advertise their maximum bitrate and
  cap it without saying so: HomeSmart reads the setting back after sending it and reports what the
  camera kept. The **Tapo C210** accepts resolution changes over ONVIF but keeps its frame rate and
  bitrate fixed (already at the maximum, 2304×1296 at 30 fps): HomeSmart reports it.
- **Tamper detection** (lens covered, camera turned away, blurred image), through either of two
  methods, which can be combined (« Détection de sabotage » (Tamper detection) section of the
  settings, or `cam tamper`):
  - **the image in HomeSmart**: a few measurements per second on the smooth stream (field of view
    turned uniform, details gone, scene completely changed), with no AI model and no graphics card;
    the reference view is relearned when the camera is panned or tilted;
  - **the camera's alerts** (ONVIF): the camera reports tampering itself, with nothing decoded on
    the PC. The Tapo C210 has a built-in detector that HomeSmart turns on for it; the Imou
    Ranger 2C has no adjustable one but emits a "scene change" event that serves as an alert.
  - Adjustable sensitivity (low, medium, high). An alert goes to the **log**, to **Problems** and to
    **Scenarios** (trigger « un sabotage de caméra » (a camera tampering)).
- **Alarm** (Tapo): the camera's siren and light. Turn on the automatic alarm (triggered by the
  camera's own detection), choose siren and/or light, the volume and the duration, or **trigger /
  stop it right away**. Through the « Caméra Tapo » section or `cam alarm`.
- **Motion tracking** (Tapo): the tracking **built into** the camera (it pans by itself to follow
  whatever moves), which can be turned on in « Caméra Tapo » or `cam track`. Not to be confused with
  tracking by **HomeSmart's AI** (Vision tab, `cam follow`), which steers the camera over ONVIF.
- **Performance**: the video pipeline handles 2560×1440 at 60 fps without dropping frames (decoding
  ≈ 4.5 ms per frame on one core); the detailed view shows the received frame rate and the displayed
  frame rate.
- **Lockout**: after several wrong passwords, an Imou blocks access for a few minutes (RTSP 403).
  So HomeSmart never retries on its own after its credentials are rejected: it waits for a click on
  « Réessayer » (Retry) or a change of settings (after a lockout, a single automatic attempt after
  10 min). Several simultaneous viewing sessions, on the other hand, are accepted.
- **Image**: brightness, contrast, saturation, sharpness, plus night vision, WDR (the cameras' "HDR")
  and backlight compensation **if the camera offers them over ONVIF**. The Ranger 2C only exposes
  the first four: its WDR cannot be adjusted locally.
- **Snapshot**: PNG in `Images\HomeSmart` (« Dossier » (Folder) button to open it).
- **Recordings** (« Direct / Enregistrements » (Live / Recordings) toggle at the top of the detailed
  view): playback of the camera's SD card, read-only.
  - Strip of the last 7 days (blue dot = there is video), summary of the day (number of recordings,
    total video duration).
  - **Timeline** of the day: each recording in its place, to the second; scroll wheel to zoom (down
    to 5 min), drag to move around, hover for the exact time, click to play back from that point;
    « Ajuster » (Fit: frames the recordings) and « Journée » (Day) buttons.
  - **Recording list** (most recent at the top, grouped by hour) with thumbnail, time, duration.
  - **Continuous playback**: from one recording to the next with no wait (the camera skips the
    gaps), time of the displayed frame always shown, pause (`Espace` (Space) or click on the image),
    previous / next recording (`←` `→`).
  - **Sound** of the recording (speaker button or `S`, volume next to it): the microphone audio
    recorded with the image, synced to it, over the same connection to the camera. Listening stays
    on from one recording to the next and stops when you leave the view; a silent recording is
    flagged.
  - **Export** of the recording to `.mp4` (H.265 + sound, no re-encoding; G.711 sound, which .mp4
    does not accept, is converted to AAC) into `Vidéos\HomeSmart`, with progress and an
    « Annuler » (Cancel) button. The camera streams in real time: expect the export to take as
    long as the recording.
  - Formatting, retention and recording mode of the card go through an interface that the
    Ranger 2C locks: they remain out of reach locally.
  - **Tapo**: the camera provides the **index** of its SD card (days and recordings, through its
    local API), shown in the list. But **playing and exporting** those recordings go through a
    proprietary, encrypted playback stream that HomeSmart cannot open: on a Tapo, only the index is
    available (no playback, no export). Two-way talk (speaking through the camera's speaker) goes
    through that same stream and is therefore not possible either.
- The stream is only open while it is displayed; leaving the tab closes it after 2-3 s. When idle,
  HomeSmart only checks once a minute that the camera responds.
- An HD stream in **H.265** (as on Imou cameras) is decoded with **FFmpeg**. If FFmpeg is missing,
  HomeSmart shows the smooth stream and offers, below the video and in Préférences (Preferences), an
  **« Installer FFmpeg »** (Install FFmpeg) button: official `Gyan.FFmpeg` package via winget, no
  administrator rights, about one minute, progress shown; the camera then switches back to HD on
  its own. On Linux, the command to run is offered (« Copier » (Copy) button). FFmpeg can also be
  placed next to `HomeSmart.exe`. From the command line: `homesmart-cli cam ffmpeg --install`.
- Everything is logged: opening and closing of the live view (with its duration), pan/tilt moves,
  positions, snapshots, tests, settings (the password itself is never written), camera unreachable.
- **Sound**: AAC, G.711, and lossless 16-bit PCM (L16, the one used by HomeSmart cameras).

### HomeSmart camera (DIY)

A 4K camera with a microphone, to build yourself (≈ €90 to €110 in parts: Radxa ROCK 4D + Camera 4K
Sony IMX415 + USB or I²S microphone), whose software is entirely open and which talks only to
HomeSmart. Full guide (hardware, installation, security, trying it without hardware):
[`cam/README.md`](cam/README.md).

- On the camera, the **`homesmart-cam`** agent runs the RTSP server (MediaMTX) and the hardware
  encoding pipeline: **HD** stream up to 4K (H.265 or H.264) and **smooth** 720p H.264 stream for AI
  analysis, with the **microphone in 16-bit PCM at 16 kHz** (lossless, in the format AI listening
  uses).
- **Adding**: HomeSmart finds it on its own (mDNS); you type the **pairing code** given during
  installation ("ABCD-EFGH"). The code never goes over the network (SPAKE2), every request after
  that is signed, and the RTSP password is derived from the secret created by the pairing. After 5
  wrong codes, the camera refuses attempts for 10 minutes.
- **Réglages de la caméra** (Camera settings): status (encoding, temperature, restarts), resolution,
  frame rate, bitrate and codec of both streams, microphone and its gain, flipped image
  (ceiling-mounted camera). No ONVIF: everything goes through the agent, and everything is logged.
- Removing the camera from HomeSmart with **Supprimer** (Delete) makes the camera forget that
  HomeSmart.
- From the command line: `homesmart-cli cam pair <IP> <CODE>`, `homesmart-cli cam agent <camera>`
  (`--res 4k --fps 25 --codec h265 --mic off --gain 6 --rotate on`).

### TP-Link plugs and bulbs (Tapo, Kasa)

Command line only for now (`homesmart-cli tplink`):

- `tplink scan`: the TP-Link devices on the network (UDP 20002 discovery for recent devices, UDP
  9999 for older Kasa devices): model, MAC, protocol, and « pas encore installé » (not set up yet)
  for a device still in its factory state.
- `tplink info|on|off <IP>`, `tplink brightness <IP> <%>`: **Tapo** plugs and bulbs (and recent Kasa
  ones) speak **KLAP**, an encrypted local protocol (AES-128, signed handshake) that requires the
  **TP-Link account e-mail and password** (`--email`, or the `HOMESMART_TPLINK_EMAIL` and
  `HOMESMART_TPLINK_PASSWORD` variables): the plug has kept a hash of these credentials since it was
  set up, yet it does not contact the cloud. **Older Kasa** devices (HS100, HS110…) answer without
  credentials (TCP 9999 protocol). Switching on and off is recorded in the log.
- Not in the interface yet: TP-Link plugs have no card, no Safeguards and no Schedules.
- Verified: discovery on a real network (Tapo C210 found); KLAP and the Kasa protocol only against
  simulated devices, for lack of a TP-Link plug at hand.

### AI vision (local)

The **Surveillance** (Monitoring) panel of an open camera: a switch, the analysis status and three
simple choices (fall, call for help, known face). Everything else is under **Avancé** (Advanced),
grouped by topic: « Ce que l'IA repère » (What the AI spots), « Réactions » (Reactions: auto-tracking,
photos), « Écoute du micro » (Microphone listening), « Réglages avancés » (Advanced settings: analyzed
image, analyses per second, computation, accuracy), zones, scenarios and known faces. Everything runs
on the PC: no image leaves it.

- **People and poses**: each person is detected, tracked from one frame to the next, and their
  skeleton (17 keypoints) is estimated. Boxes, names and skeletons are drawn on the video.
- **Actions**: a person arriving and leaving, **fall**, then **person on the floor** if they stay
  still (critical alert), and **got back up**. A fall means: going abruptly from standing to lying
  (head clearly lower), or an abrupt drop of the head and hips (a fall toward the camera or with the
  back to it, where the body on the floor still looks "standing" in the image); then **staying on
  the floor for 1.5 s**. Lying down slowly, sitting down, kneeling or picking something up do not
  count. A person on the floor, less well seen by the model (low score), keeps their track: a weak
  detection extends a confirmed track but never opens one. A fall does not make the silhouette taller
  (beyond +25%) or much narrower (less than 0.6×): if it does, the track has jumped to another
  silhouette (a person's upper body alone, then their whole body; another person very close by);
  and "lying" based on the box shape alone (unreliable skeleton) does not count if the standing
  person was already cut off by the bottom of the frame. Measured on 1,548 real videos: half as many
  false falls in everyday life, no real fall missed (see "Quality measured").
- **Gestes** (Gestures, from the skeleton): **raised hand** (held for a moment), **both arms raised**
  held for 2 s = call-for-help signal (critical alert) — only for a person standing or sitting: when
  lying down, "above the head" means nothing in the image (in the fall videos, all the "raised arms"
  came from people on the floor).
- **Immobilité** (Inactivity; choice of no, 10 min, 30 min or 1 h): a person who has stopped moving
  altogether.
- **Violence** (off by default): repeated blows between two people — a wrist or ankle that moves fast
  toward a person very close by and reaches them, at least 4 times in 4 s, below the top of their
  silhouette (hugs and high fives happen at head height). Critical alert, photo, « des coups » (blows)
  scenario. A friendly punch on the shoulder does not count (too high), nor does a single tap; nothing
  is looked for beyond 5 people in the image (team sports, parties), and a kick only counts if the
  person kicking is fully in the frame. With **sound listening**, the alert waits 4 s: if laughter is
  heard around the blows (with no screaming or crying), it is a « chamaillerie probable » (probable
  horseplay), noted in the log without an alert. Tuned not to cry wolf, on 1,548 real videos: **no
  false alarms** on the 480 original calm scenes (hugs, high fives, handshakes, everyday life); on 481
  games and friendly gestures filmed on YouTube (Kinetics-700: pillow fights, tickling, wrestling,
  boxing, arm wrestling, massages…), 11 false alarms, 9 with laughter taken into account (boxing 4,
  wrestling 3, massage, handshake); 4 out of 99 in capoeira and tackles. The trade-off: only 13% of the
  440 violent scenes are found (18% at 15 analyses/s, 5% with high accuracy, almost none below 8).
  Mock wrestling and boxing, without laughter, remain impossible to tell apart from a real fight in
  the image.
- **Suivre la personne** (Follow the person; motorized cameras): the camera turns to keep the person
  in the center (the largest one, then always the same one), stops if they disappear, returns to its
  starting position 10 s after losing them, and pauses for 10 s as soon as you steer it yourself. On
  a Ranger 2C: target centered in 1.2 s, with no overshoot or back-and-forth. « Suivre aussi en
  hauteur » (Also follow vertically) is off by default: the Ranger 2C's tilt does not respond over
  ONVIF, and a command that includes it is ignored entirely. Every movement is logged.
- **Animaux** (Animals: cat, dog, bird, horse, sheep, cow, wild animal) and **véhicules** (vehicles:
  car, bicycle, motorcycle, truck, bus; off by default, useful outdoors): appearing and leaving, boxes
  on the video. Analyzed at most twice per second, even when people are analyzed at 30. Each kind has
  its own threshold, tuned on COCO (see "Quality measured"); the same animal seen as both "cat" and
  "dog", or the same vehicle seen as both "car" and "truck", only counts once, under the most
  confident kind (trucks on COCO: precision 0.56 → 0.65, empty images mistaken for a truck 24 → 13; a
  truck is sometimes reported as a car).
- **Second opinion on people**: when the animal model is installed, a silhouette with a modest score
  (reflection, coat, poster) is only kept if this second model also sees someone in the same place; a
  track that has already been accepted no longer needs a second opinion. On COCO, measured as in the
  app: half as many empty images mistaken for a presence by day (53 → 27 out of 1,020), almost three
  times fewer in simulated night vision (73 → 27), at the cost of 1% fewer real people.
- **Photo des événements** (Event photos; off by default): a PNG in `Images\HomeSmart\Vision` for
  each notable event (arrival, stranger, fall, raised arms, animal…), at most one every 5 s except
  for alerts; its name is noted in the log.
- The camera's **Derniers événements** (Latest events) in the section, and **en vue maintenant** (In
  view now: people, animals, vehicles).
- **Présence aujourd'hui** (Presence today): minutes when someone was in view, hour by hour (24 bars),
  kept for 60 days in `presence.json` (durations only); `homesmart-cli vision presence Salon --date …`.
- **Analyses par seconde** (Analyses per second): 2 to 30, **8 by default** (falls: 74% found at 8, 60% at 4; beyond the camera's frame rate, each frame is
  analyzed only once). The rules are measured in durations, not in numbers of frames: they behave
  the same at 2 or at 30 analyses per second.
- **Zones** (door, hallway, bed…), drawn directly on the video: « Ajouter une zone » (Add a zone), one
  click per vertex, then double-click (or Enter, or click on the first point) to close; Backspace
  removes the last point, Escape cancels. You give it a name and, if you want, a **présence
  prolongée** (extended presence) delay (0 = never). The log notes entering, leaving and extended
  presence in each zone, based on the person's ground contact point; the occupied zone lights up on
  the video. « Zones » list in the section: rename (name and delay), delete. Zones remain valid on the
  smooth stream as on the HD stream (coordinates relative to the image); a shape whose sides cross is rejected.
- **Faces**: detection, then recognition of the people **you enroll** (« Enregistrer ce visage »
  (Enroll this face) when someone walks by, or **drop a photo** JPEG/PNG onto the window; several views improve recognition — the photo is not kept, only its face embedding). A person
  is only named after several matching recognitions, which make up at least a third of the sharp
  faces seen on their track: a stranger who stays a long time no longer ends up resembling a household
  member by chance (households simulated on LFW: 6% → 2% of strangers wrongly named over a long
  visit, household members still always named); otherwise « Visage inconnu » (Unknown face). The
  face embeddings (biometric data: 128 numbers per view, no photo) stay in `faces.json`
  next to the configuration, **encrypted** (see below); those of strangers are never
  kept. Rename, delete: « Visages connus » (Known faces).
- **Précision** (Accuracy; under « Calcul » (Computation)): standard (RTMO-s) or high (RTMO-m,
  downloaded the first time it is chosen). Measured: by day almost the same (AP50 0.905 → 0.909),
  **clearly better at night** — 29% fewer false people on empty images, falls found 68% → 72% with
  4% false alarms instead of 11%. Twice as much computation on the CPU (34 → 70 ms per frame),
  barely more on the GPU (13 → 17 ms).
- **Robust tracking**: a weak detection only extends a track where the person was (or at their
  feet, when they fall); a dubious object in the scene can no longer "capture" the track of a person
  who walked past it, which caused false falls on a real camera at night. A track is only confirmed
  (« Personne détectée » (Person detected)) with at least two confident detections: an object in the
  scene seen once above the threshold, then weakly, no longer becomes a person. A track that has been
  **stationary** for 10 s (a person sitting or lying down, often poorly seen; a silhouette in the
  scene: photo, poster, screen) waits 60 s without seeing it before declaring it gone, and is only
  picked up again in the same spot: no more « Personne détectée » / « n'est plus en vue » (is no
  longer in view) (and « Visage inconnu ») every minute for the same silhouette, as seen on a real
  camera. It is neither shown nor counted as presence during this wait.
- **Calcul**: GPU if possible (DirectML on Windows: NVIDIA, AMD, Intel), otherwise CPU, with
  automatic fallback. Measured on a Ranger 2C (2304×1296 image): people 9 to 13 ms, face 4.5 ms,
  face embedding 1 to 2 ms, animals 7 ms on GPU; people 36 ms, face 6 ms, face embedding 6 ms on CPU
  alone. With 30 analyses requested, the GPU keeps up with the camera (≈ 23 actual analyses/s on its
  recorded playback).
- **Analyzed stream**: HD by default (faces recognized from farther away) or smooth (less computation).
  The analysis keeps the stream open by itself; the log says so (« Système » (System), « Vision IA » (AI vision)).
- Everything is logged (source « Vision IA »); fall and person on the floor are critical alerts.

**Models** (downloaded once from their official source, SHA-256 checksum verified, stored in
`%LOCALAPPDATA%\HomeSmart\models` or `~/.local/share/homesmart/models`; never in the repository):

| Model | Role | License | Source |
|---|---|---|---|
| RTMO-s (body7) | people + skeletons | Apache-2.0 | [OpenMMLab MMPose](https://github.com/open-mmlab/mmpose/tree/main/projects/rtmo) |
| RTMO-m (body7), optional | same, « Précision : élevée » (Accuracy: high) (83 MB) | Apache-2.0 | same |
| YuNet (2023mar) | faces + 5 landmarks | MIT | [OpenCV Zoo](https://github.com/opencv/opencv_zoo/tree/main/models/face_detection_yunet) |
| SFace (2021dec) | face embeddings | Apache-2.0 | [OpenCV Zoo](https://github.com/opencv/opencv_zoo/tree/main/models/face_recognition_sface) |
| YOLOX-s | animals + vehicles (COCO) | Apache-2.0 | [Megvii YOLOX](https://github.com/Megvii-BaseDetection/YOLOX) |

Engine: [ONNX Runtime](https://onnxruntime.ai) (MIT) via the [`ort`](https://ort.pyke.io) crate. The
prebuilt binaries require an x86-64 CPU with AVX2 (2013 and later). On Windows, `DirectML.dll` ships
next to the executable; without it, Windows 11 provides its own.

**Face embedding protection** (`faces.json`, all of its contents: names and face embeddings):

| System | Encryption | Who can read the file back |
|---|---|---|
| Windows | DPAPI (`CryptProtectData`, user scope) | your Windows account, on this PC |
| Linux with keyring | AES-256-GCM; the key (256 random bits) is stored in your session's keyring (Secret Service) | your session, once the keyring is unlocked |
| Linux without keyring | **none**: plaintext file, `600` permissions, warning in the log | your account (and root) |

- **Protects against** copying the file alone (backup, sync, USB stick, disk read on another
  machine) and against the computer's other accounts.
- **Does not protect against** a program launched under your account, or an administrator, while
  your session is open: it can request decryption just as HomeSmart does.
- Copied to another PC or under another account, the file is unreadable: the faces have to be
  enrolled again. Same if an administrator **resets** your Windows password (changing it normally
  is not a problem), or if the Linux keyring is erased.
- An unreadable face gallery (damaged file, other account, locked keyring) is never overwritten: the
  log reports it (source « Système »), nobody is recognized, and enrolling a face is refused until
  the file is read back (keyring unlocked) or deleted.
- An old plaintext gallery is read and then rewritten encrypted the first time it is read; the log
  says so. The old plaintext content is not physically erased from the disk (SSD, file system
  journal).
- Without a keyring, HomeSmart does not encrypt just for show: the key would sit next to the file,
  which would protect against nothing.

> Filming at home falls under personal use; do not film the public street or your neighbors' homes,
> inform the people who work in your home, and only enroll the faces of people who agree.

### AI listening (local)

In **Surveillance › Avancé** (Monitoring › Advanced), the « Écoute du micro » (Microphone listening)
part. Off by default, camera by camera. **Sound is never recorded**: it is analyzed in memory in
blocks of a few seconds, then forgotten; only recognized sounds and, if you have enabled it,
transcribed speech go to the log (source « Écoute IA » (AI listening)).

- **Sons de la maison** (Household sounds): fire alarm (smoke detector), alarm or ringing, **breaking
  glass**, scream, crying (baby or adult), gunshot, barking, meowing, doorbell, knocking, door
  slamming, telephone, coughing, snoring, footsteps, siren. Fire alarm, breaking glass and gunshot
  are critical alerts (with a photo if « Photo des événements » (Event photos) is enabled); the same
  sound is only reported once per 30 s.
- **Transcrire la parole** (Transcribe speech): the voice is detected, each sentence is transcribed
  in French, and the text goes to the log; « au secours » (help!), « à l'aide » (help), « aidez-moi »
  (help me)… trigger a **critical alert**. Whisper's "phantom" sentences (« Sous-titres réalisés
  par… » (Subtitles by…), loops) and its sound descriptions (« \*rires\* » (\*laughter\*),
  « [Musique] » ([Music])) are discarded: out of 176 noises without speech (coughing, laughter,
  sneezing, sirens…), no sentence reaches the log.
- **Son amélioré** (Enhanced sound; button next to the volume, live as well as in playback; on by
  default): low-cut filter (90 Hz), automatic volume that brings the voice toward −20 dBFS without
  raising the background noise (camera microphones are quiet: peaking at 1% on an Imou), soft
  limiter, high-quality resampling to the sound card, 5 ms fades instead of clicks when the network
  stutters. The AI analysis, for its part, always receives the raw sound.
- Microphone level and the latest sounds or sentences heard, in the section; the camera must have
  an active microphone (otherwise the section says so).
- Computation: sounds ≈ 7 ms per 3 s window (one every 0.5 s); transcription 8 times faster than
  real time on the CPU, a little more on the GPU.

| Model | Role | License | Source |
|---|---|---|---|
| CED-mini (int8, sherpa-onnx export) | 527 kinds of sounds (AudioSet) | Apache-2.0 (weights also published under CC-BY-4.0) | [CED](https://github.com/RicherMans/CED), [k2-fsa](https://huggingface.co/k2-fsa/sherpa-onnx-ced-mini-audio-tagging-2024-04-19) |
| AudioSet names | sound labels | CC-BY-4.0 | [Google AudioSet](https://research.google.com/audioset/) |
| Silero VAD v6 | voice detection | MIT | [snakers4/silero-vad](https://github.com/snakers4/silero-vad) |
| Whisper small (int8, onnx-community export) | speech → text | MIT | [OpenAI Whisper](https://github.com/openai/whisper), [onnx-community](https://huggingface.co/onnx-community/whisper-small) |

≈ 264 MB in total (10 MB for the sounds alone), downloaded and verified like the vision models.

> Listening at home, like filming, falls under personal use: inform the people present
> (guests, people who work in your home) before enabling transcription.

### Away mode (alarm)

At the top of the **Caméras** (Cameras) section: « Mode absence » (Away mode; when armed, a banner reminds you of it on every screen). While it is on, **anyone seen**
by an analyzed camera triggers a **critical alert** (« Intrusion ? Personne détectée… » (Intrusion?
Person detected…)) with a **photo** (even if « Photo des événements » (Event photos) is off) and a
system notification. A known face is named in the log right after. The « une intrusion (mode
absence) » (an intrusion, away mode) scenario can, for example, turn on all the lights.

### Scenarios (the camera controls your devices)

A camera's **Surveillance › Avancé** (Monitoring › Advanced), « Scénarios » (Scenarios) part: « Quand
[déclencheur], alors [allumer / éteindre] [appareil] » (When [trigger], then [turn on / turn off]
[device]), optionally **only during a time window** (22:00 → 07:00…).

| Trigger | Where it comes from |
|---|---|
| quelqu'un arrive (someone arrives) | a person entering the field of view (confirmed track) |
| personne depuis N min (nobody for N min) | empty room: no person in view for N minutes, **as long as the analysis is running** (without it, HomeSmart cannot know whether the room is empty); only once per absence |
| une chute (a fall) | confirmed fall (person who stayed on the floor) |
| un appel à l'aide (a call for help) | both arms raised, or « au secours » (help!), « à l'aide » (help)… heard (if transcription is enabled); a mere scream is not enough (children playing) |
| une alarme incendie (a fire alarm) | smoke detector heard (« Sons de la maison » (Household sounds)) |
| du verre brisé (broken glass) | breaking glass heard (« Sons de la maison ») |
| une intrusion (mode absence) (an intrusion, away mode) | someone arrives while away mode is on |
| des coups (violence) (blows, violence) | repeated blows between two people (vision + sound) |
| un sabotage de caméra (camera tampering) | lens covered, camera turned away or image blurred (see « Détection de sabotage » (Tamper detection)) |
| mode absence activé / désactivé (away mode on / off) | the whole house: leaving or coming back (handy for the robot vacuum) |

A scenario **without a camera** applies to all analyzed cameras: « personne depuis N min » then
means **the house is empty** (nobody on any camera), and « quelqu'un arrive » applies to any of
them. Robot vacuums have their own actions: **nettoyer** (clean) or **rentrer à la base** (return to
the dock).

- Examples: turn off the heater when the bedroom has been empty for 30 min (suggested automatically
  if a risky device is in the same room as the camera); turn on the hallway light when someone
  arrives at night; turn on all the lights in case of a fall or a call for help.
- A given event-based scenario only triggers once per minute.
- **Safeguards respected**: a device that requires a confirmation to be turned on (heating) is never
  turned on by a scenario — the log says so.
- Everything is logged (source « Scénario » (Scenario)): the reason (« Chambre : personne depuis 30 min →
  éteindre « Radiateur » » (Bedroom: nobody for 30 min → turn off "Heater")), then the command. Deleting the camera or the device deletes its scenarios.

### Robot vacuums

Robots appear in **Maison** (Home) like the other devices: the tile lights up during cleaning,
shows the activity and the battery; a click starts cleaning (or pauses it), a right-click opens the
panel: activity in large type, battery, area and duration, the **Nettoyer / Pause** (Clean / Pause),
**Base** (Dock), **Arrêter** (Stop) and **Trouver** (Find: sound signal) buttons, then whatever the
robot can adjust (**puissance d'aspiration** (suction power), **mode**, **débit d'eau** (water
flow), ways of cleaning), the **usure** (wear) of the brushes and filter, the **automatismes**
(automations) and the **programmes** (schedules).

| Family | How HomeSmart reaches it | What you need once |
|---|---|---|
| Tuya / Smart Life (category « sd ») | Tuya local protocol, like the plugs | the local key (Smart Life import via QR code, like the other devices) |
| Valetudo (Dreame, Roborock, Xiaomi… running open-source firmware) | local REST API (HTTP) | nothing: no account, no key; credentials only if Valetudo's HTTP access is protected |
| miIO (Xiaomi, Roborock, Dreame made before 2022) | local UDP 54321, encrypted | the device's **token** (32 characters), extracted once from the Xiaomi account |

- **Recent Dreame** (Dreamehome app): **no local control**, the robot only answers Dreame's cloud.
  To do without it, you have to install **Valetudo** on the robot (often by opening it up; see
  valetudo.cloud). The Dreamehome cloud is not offered: accessing it means imitating the Dreame app
  (network fingerprint, app secrets) and it may stop working at any time.
- **Adding**: Maison › « + » › « Aspirateur robot » (Robot vacuum): network search (Valetudo mDNS
  announcement, miIO handshake), **Tester** (Test) button, then **Ajouter** (Add).
- **Log**: every command with its origin (you, command line, schedule, scenario); cleanings started
  outside HomeSmart (robot button, its app, its built-in schedule) and the end of each cleaning
  (« Nettoyage terminé : 23 m² en 41 min, retour à la base » (Cleaning finished: 23 m² in 41 min,
  returning to the dock)) are logged under « Appareil » (Device).
- **Problèmes** (Problems): the robot's errors open « Aspirateur en difficulté » (Vacuum in trouble)
  with what to do (brush stuck, bin full, robot stuck, lifted, sensors, battery…), with their
  severity: information (bin full, water tank), warning (blockages), critical (failing battery,
  system notification); « Résolu » (Resolved) when the error goes away. A robot that no longer
  responds switches to « Hors ligne » (Offline).
- **Automatismes** (robot panel): clean when the house has been empty for N min (analyzed cameras),
  clean when away mode is turned on, return to the dock when someone arrives or when away mode
  ends. **Programmes**: « nettoyer à 10:00 en semaine » (clean at 10:00 on weekdays), or a time window
  (« nettoyage de 10:00 à 11:30 » (cleaning from 10:00 to 11:30), returning to the dock at the end).
- Demo (`--demo`): a simulated Tuya robot and a simulated Valetudo robot; Problèmes › « Simuler une
  panne » (Simulate a fault) triggers a stuck brush or a stuck robot on them.
### Quality measured on real data

`homesmart-cli vision eval …` and `homesmart-cli listen eval …` measure the models on public annotated
datasets, downloaded separately (never in the repository). Results on the development PC:

| Measure | Dataset | Result |
|---|---|---|
| People (RTMO-s, medium and large) | COCO val2017, 1,952 images | AP50 0.905; at threshold 0.5: precision 0.93, recall 0.83; with the second opinion, as in the app: precision 0.95, recall 0.82, 27 of 1,020 empty images mistaken for a presence (at night: recall 0.72, also 27) |
| Skeletons | same, 2,259 people | mean OKS 0.85 (84% ≥ 0.75) |
| Cats / dogs (YOLOX-s) | same | AP50 0.89 / 0.76; at the app's thresholds: precision 0.91 / 0.83, recall 0.86 / 0.75 |
| Faces (YuNet + SFace) | LFW, 6,000 pairs | at threshold 0.36: 0.07% wrongly accepted, 1% rejected, 99.5% correct; 34 px faces: 98.8%; simulated night: 98.0% |
| Faces, final decision | LFW, 400 simulated households (4 residents enrolled with 3 photos, 4 passing strangers) | residents named: 100%; strangers wrongly named: 1.8% over a visit of 4 faces, 2.0% over 200 (6.3% before the vote was tuned); at night 1.9% (7.9%) |
| Falls (rules on the skeletons) | CAUCAFall + UR Fall, 150 real videos | 74% of falls detected, 3% false alarms over 70 everyday movements (before tuning: 50% and 23%) |
| The whole alert pipeline (tracking, falls, gestures, violence) | 1,548 real videos (falls, fights, play, everyday life: 738 videos, 1.4 h) | falls: 59 of 80, false falls in everyday life 21 → 13 (at night: 50 of 80, 26 → 17); wrong « bras levés » (raised arms) 37 → 16, none at all for a person on the floor; duplicate arrivals 144 → 130; violence 55 of 440 (57 before), 15 false alarms over 1,060 |
| Household sounds (CED-mini) | ESC-50 (176 clips) + 289 sentences (FLEURS) | 6 to 8 of 8 detected depending on the sound, ≤ 2 false alarms over 450 neutral clips; almost as many with someone talking just as loudly over them |
| Speech → text (Whisper small) | FLEURS French, 40 sentences | 10% of words wrong in quiet conditions; 23% with household noise at 10 dB; 15% with an 8 kHz microphone (G.711) |

Tried and rejected based on these measurements: denoising (Wiener filter) before Whisper (12% errors
in quiet conditions instead of 10, 25% instead of 23 in noise: Whisper prefers the raw sound); 1 s
listening windows (far less accurate than 3 s). The quantized audio models run on the CPU: CED in
int8 crashes DirectML, and Whisper only gains 30% from it. For falls, lowering the descent or
time-on-the-floor thresholds gains 1 to 2 falls for 2 to 3 more false alarms: the thresholds were
kept; a rule on the feet (which move down in a fall toward the camera) was rejected, because it would
also have masked falls with the back to the camera, which are absent from the videos. The second
opinion on people has no better setting than 0.8 / 0.2.

Datasets used: [COCO](https://cocodataset.org) (CC-BY-4.0 annotations), [LFW](http://vis-www.cs.umass.edu/lfw/),
[ESC-50](https://github.com/karolpiczak/ESC-50) (CC-BY-NC), [FLEURS](https://huggingface.co/datasets/google/fleurs)
(CC-BY-4.0), [CAUCAFall](https://data.mendeley.com/datasets/7w7fccy7ky) (CC-BY-4.0) and
[UR Fall](https://fenix.ur.edu.pl/~mkepski/ds/uf.html) (falls); for violence and the alert
pipeline, [AIRTLab](https://github.com/airtlab/A-Dataset-for-Automatic-Violence-Detection-in-Videos),
[Surveillance Camera Fight](https://github.com/seymanurakti/fight-detection-surv-dataset) (MIT),
[UT-Interaction](https://cvrc.ece.utexas.edu/SDHA2010/Human_Interaction.html) and clips from
[Kinetics-700](https://github.com/cvdfoundation/kinetics-dataset) (play, sports, friendly gestures).

### Terminal interface (TUI)

```bash
homesmart-cli tui            # your devices and cameras
homesmart-cli tui --demo     # simulated devices and cameras
```

Full screen, keyboard-driven, with the same engine as the app (safeguards, schedules, energy,
log; actions taken there are logged as « Ligne de commande » (Command line)):

- **Appareils** (Devices, F1): status and measurements of each device, details of all its data points.
  `Espace` (Space) switches on or off (with confirmation if a safeguard requires it), `←` `→` set the
  brightness, `1`-`9` the outlets of a power strip, `m` the timer, `r` requests the status again.
- **Caméras** (Cameras, F2): status, stream, pan/tilt with `Maj` (Shift) + arrows, snapshot with `c`,
  and `v` for a **color video preview in the terminal** (half blocks, ~12 frames/s; Windows Terminal
  or any 24-bit color terminal).
- **Énergie** (Energy, F3): power, kWh and cost for the day and the month, hour-by-hour histogram.
- **Journal** (Log, F4): all actions, live.
- **Aspirateurs** (Vacuums, F5): activity, battery, errors, wear; `Espace` starts cleaning (or pauses),
  `b` sends the robot back to the base, `x` stops, `t` makes the robot beep, `p` changes the suction power.

`Tab` switches tabs, `q` quits. If the app is already open, the TUI tells you so: plugs often
accept only one connection at a time.

### Command line

```bash
homesmart-cli scan                          # Tuya devices present on the Wi-Fi
homesmart-cli add bf1234... "clé16caractères" --name "Lampe salon" --room Salon
homesmart-cli import-cloud <code>           # one-time import from Smart Life (QR code in the terminal)
homesmart-cli import devices.json           # tinytuya import
homesmart-cli list
homesmart-cli status "Lampe salon"
homesmart-cli on "Lampe salon"
homesmart-cli set "Lampe salon" 22 500      # DP 22 = brightness (10 to 1000)
homesmart-cli watch "Multiprise - Bureau"   # live measurements

homesmart-cli cam scan                      # cameras on the network (Imou / Dahua, ONVIF, RTSP)
homesmart-cli cam add 192.168.1.164 --name Salon   # you are prompted to type the security code
homesmart-cli cam test Salon                # video + ONVIF
homesmart-cli cam info Salon                # model, firmware, video profiles, positions
homesmart-cli cam snapshot Salon            # HD snapshot (--sub: smooth stream, -o fichier.png)
homesmart-cli cam move Salon gauche --ms 600
homesmart-cli cam goto Salon "Entrée"       # preset position
homesmart-cli cam home Salon
homesmart-cli cam encoder Salon --fps 25 --kbps 4096   # with no option: shows the current setting
homesmart-cli cam watch Salon --hd          # frame rate, bitrate, decoding time, CPU
homesmart-cli cam recordings Salon          # today's SD card recordings (--date 2026-10-19)
homesmart-cli cam replay Salon --start "2026-10-20 20:52:59" --end "2026-10-20 20:53:44"
homesmart-cli cam export Salon --start "2026-10-20 20:52:59" --end "2026-10-20 20:53:44"
homesmart-cli cam tamper Salon --image on --sensitivity moyenne   # tamper detection (with no option: status)
homesmart-cli cam tapo Salon                # Tapo: TP-Link account password (typed when prompted), then status
homesmart-cli cam alarm Salon --start       # triggers the alarm (with no option: shows the settings; --on/--siren/--light/--volume/--duration; --stop)
homesmart-cli cam track Salon on            # motion tracking built into the camera (Tapo)

homesmart-cli vac scan                      # Valetudo and miIO robots on the network (without changing anything)
homesmart-cli vac add-valetudo 192.168.1.59 --name "Aspirateur" --room Salon
homesmart-cli vac add-miio 192.168.1.60 --token <32 characters> --name Roborock
homesmart-cli vac list
homesmart-cli vac status Aspirateur         # activity, battery, available settings, errors, wear
homesmart-cli vac start Aspirateur          # also: pause, stop, dock, locate
homesmart-cli vac run Aspirateur fan:max    # also: mode:…, water:…, pattern:…

homesmart-cli vision models --install       # AI models: verified download (≈ 75 MB)
homesmart-cli vision info                   # acceleration in use (--cpu: CPU only)
homesmart-cli vision replay Salon --start "2026-10-20 20:52:59" --end "2026-10-20 20:53:44"   # --objects: YOLOX too
homesmart-cli vision eval coco --data <folder>     # measured quality (also: faces, identify, falls, images)
homesmart-cli vision eval alarms --data <folder> --cache <folder>   # the whole alert pipeline on the videos (--night, --model m)

homesmart-cli listen models --install       # listening models (≈ 264 MB)
homesmart-cli listen tag son.wav            # sounds recognized in a file
homesmart-cli listen transcribe parole.wav  # speech → text (voice detection, then Whisper)
homesmart-cli listen eval sounds --data <folder>   # also: speech (--snr 10, --phone)

homesmart-cli secrets status                # where keys and passwords are stored, and how
homesmart-cli secrets audit <folder>       # are these files free of your configuration's private values?
```

Many devices accept **only one local connection at a time**: if the app is open, the device may
refuse the CLI (and vice versa).

## Resources

Measured on the optimized build, 5 devices including 2 metering plugs polled every 2 s, on a
16-core CPU:

| | CPU | Memory |
|---|---|---|
| Window visible, no interaction | 0.8% of one core | ~115 MB |
| Window minimized | 0.16% of one core | ~90 MB |

The interface only redraws when a value changes (or every 30 s for the clock); when the window is
not focused, these redraws are grouped every 3 s, and nothing is drawn while it is minimized.
Animations and 3D only compute while they are moving. Memory goes mostly to the OpenGL graphics
context and fonts; network traffic amounts to a few hundred bytes every 2 s per plug.

## Configuration

A plain JSON file, shared by the app and the CLI:

- Windows: `%APPDATA%\HomeSmart\homesmart.json`
- Linux: `~/.config/homesmart/homesmart.json`
- elsewhere: the `--config` option or the `HOMESMART_CONFIG` variable

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

`kind` (`light`, `plug`, `switch`, `cover`, `vacuum`, `generic`) forces the type if automatic
detection gets it wrong; `vacuums` lists non-Tuya robot vacuums (Valetudo, miIO, with their address
and, for miIO, their token); `schema` takes tinytuya's `mapping` to show readable labels;
`"powers_pc": true` marks the plug that powers the PC HomeSmart runs on. Next to the
configuration, `session.json` (refreshed every 5 s, with the last minute of plug measurements)
makes it possible to recognize at startup a session that ended abruptly.

### Keys and passwords: stored separately, encrypted

`homesmart.json` holds **no secrets**: you can show it or attach it to a request for help.
Tuya local keys, camera passwords and security codes, TP-Link (Tapo) password hashes, the pairing
secret of HomeSmart cameras, and vacuum passwords or tokens go into **`homesmart.secrets.json`**,
next to it (`%APPDATA%\HomeSmart`, `~/.config/homesmart`): never in the application folder or in
the code repository.

This file is encrypted like the face gallery (`src/protect.rs`): **DPAPI** on Windows
(readable only by your own Windows account, on this PC), **AES-256-GCM** on Linux with a key stored
in your session's keyring; without a keyring (none installed, or HomeSmart started outside a desktop
session: SSH, service, scheduled task), it stays in plain text with `600` permissions, and the log
says so. An already encrypted file, for its part, can only be read back from the session that has
access to the keyring. Same limits as for faces (see "Face embedding protection"): a program run under
your account can decrypt it just as HomeSmart does.

- A key written by hand in `homesmart.json` (as in the example above) or an old file where
  everything was in plain text: at the next launch, the secrets are moved out of it and stored in
  `homesmart.secrets.json`; the log records it (source « Système » (System)). The old plain-text
  content is not physically erased from the disk (SSD, file system journal).
- Secrets first, configuration second: a shutdown between the two loses none of them.
- An unreadable secrets file (damaged, or coming from another account or another PC) is never
  overwritten: HomeSmart refuses to start and tells you what to do (rename it, then re-enter the keys).
- The secrets file can only be read back on the PC and account that wrote it: on another PC,
  fetch the keys again (`import-cloud`) and re-enter the camera passwords.
- `homesmart-cli secrets status` tells you where it is and how it is protected.

## Contributing

The repository contains no private data (configuration, log, keys, models, executables:
see `.gitignore`). To keep any from slipping in while you work (a real key in a test, a
real password in an example…), enable the provided hook once:

```bash
git config core.hooksPath .githooks
```

Before each commit, it runs `homesmart-cli secrets audit --staged`: the committed files are
compared against the values in **your** configuration (keys, passwords, tokens, TP-Link password
hash, identifiers, serial numbers and MAC addresses of your devices). If one of them is found, the
commit is refused and only the location is shown, never the value. Tests and examples use dummy
values.

## Code layout

| File | Contents |
|---|---|
| `src/protocol/` | Tuya protocol: 55AA/6699 frames, AES-ECB/GCM, HMAC, 3.4/3.5 negotiation |
| `src/device.rs` | connection to a device (reading and writing in parallel) |
| `src/discovery.rs` | UDP discovery (ports 6666, 6667, 7000) |
| `src/hub.rs` | one task per device: permanent connection, heartbeat, reconnection |
| `src/cloud.rs` | one-time import of the keys from Smart Life (QR login, encrypted requests) |
| `src/kinds.rs` | recognition of device types and Tuya formats (colors, measurements, faults, timer) |
| `src/automation.rs` | safeguards and schedules (pure, tested functions) |
| `src/energy.rs` | consumption history (power integration, running totals) |
| `src/journal.rs` | permanent log of all actions |
| `src/problems.rs` | problem detection: pure, tested rules (unrequested switch-offs and their cause, offline, voltage, consumption, abrupt PC shutdown) |
| `src/hub/problems.rs` | applying them to devices, cameras and the previous session; tests against simulated plugs and a simulated camera |
| `src/protect.rs` | at-rest encryption of sensitive data (DPAPI on Windows, Secret Service keyring on Linux) |
| `src/config.rs` | the configuration; its secrets stored separately and encrypted (`homesmart.secrets.json`) |
| `src/house.rs` | house plan (rooms, device positions) |
| `src/camera/` | cameras: RTSP stream + decoding (`stream.rs`), ONVIF (`onvif.rs`), discovery (`discover.rs`), simulated camera (`sim.rs`), HomeSmart cameras (`agent.rs`) |
| `cam/proto/` | HomeSmart camera protocol: settings, SPAKE2 pairing, signed requests |
| `cam/agent/` | `homesmart-cam`, the agent that runs on the camera (RTSP server, encoding, API, mDNS) and its installation (`deploy/`) |
| `src/hub/cameras.rs` | one task per camera: stream opened on demand, presence, log |
| `src/vacuum/` | robot vacuums: common model (state, commands, errors), Tuya "sd" (`tuya.rs`), Valetudo (`valetudo.rs`), miIO Roborock and MIoT (`miio.rs`), discovery (`scan.rs`), simulated robots (`sim.rs`) |
| `src/hub/vacuums.rs` | one actor per non-Tuya robot; common view, logged commands, end of cleaning, errors → problems |
| `src/gui/` | the egui interface (`three.rs`: small software 3D engine, `fx.rs`: effects and animations) |
| `src/sim.rs` | simulated devices (demo and tests) |
| `src/bin/cli.rs` | the command line |

```bash
cargo test   # including full exchanges against a simulated device (3.1 → 3.5) and a simulated camera (RTSP + ONVIF)
cargo test --workspace   # with the HomeSmart camera agent and its protocol
```
