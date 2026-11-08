# HomeSmart camera (DIY)

*[Lire en français](README.fr.md)*

A 4K camera with a microphone, whose software is entirely open, which talks only to HomeSmart and to
nothing else on the Internet. HomeSmart finds it on the network by itself, pairs with it using a
code, shows its picture and sound, analyzes it (AI vision and AI listening) and adjusts its quality.

## Hardware

| Part | Role | Price noted (late 2025) |
|---|---|---|
| **Radxa ROCK 4D** (RK3576), 2 or 4 GB | hardware H.264 / H.265 encoding up to 4K at 60 fps, Wi-Fi 6, optional PoE, 6 TOPS NPU | ≈ $31–37 |
| **Radxa Camera 4K** (Sony IMX415, 8 MP, 4-lane MIPI CSI) | the sensor; interchangeable M12 lens, 88° diagonal | ≈ €27–36 |
| Microphone: **USB** (prototype) or **I²S** INMP441 / ICS-43434 (final version) | the sound; the board's jack socket has no microphone input | €3–10 |
| microSD, USB-C power supply (or PoE module), heatsink, Wi-Fi antennas, 3D-printed case | — | ≈ €20–30 |

No Raspberry Pi for 4K: the encoder of the Pi Zero 2 W / Pi 4 stops at 1080p, the Pi 5 has none,
and the Camera Module 3 does not go beyond 14 fps at full resolution.

**Privacy**: plan for a switch that cuts power to the microphone (and an indicator light); the
microphone can also be muted from HomeSmart. **Night**: the IMX415 sees well in low light, but
complete darkness requires 850 nm infrared LEDs and a removable infrared filter (not supplied by
Radxa).

## How it works

```
sensor ── ISP ─┬─ HD encoder (H.265, 4K) ───┐
               └─ smooth encoder (H.264) ───┤
microphone ─ 16-bit PCM, 16 kHz (L16) ──────┴─▶ MediaMTX (RTSP :8554, /main and /sub)
                                                    ▲ reading: paired HomeSmart instances only
homesmart-cam (agent): starts and watches both processes, HTTP API :8780, mDNS announcement
```

- **`cam/agent`** (`homesmart-cam`) runs on the camera: it writes the configuration of
  [MediaMTX](https://github.com/bluenviron/mediamtx) (MIT), starts the encoding pipeline, restarts
  it if it stops (increasing delay, last error kept), announces itself over mDNS
  (`_homesmart-cam._tcp`) and answers HomeSmart.
- **`cam/proto`**: the shared protocol (settings, pairing, signatures).
- On the HomeSmart side: `src/camera/agent.rs` (client), discovery in `src/camera/discover.rs`, and
  the « Caméra HomeSmart » (HomeSmart camera) section of the settings in the interface.

Encoding pipelines (backends):

| Backend | Where | Image | Sound |
|---|---|---|---|
| `rockchip` | Rockchip board (detected) | GStreamer + MPP hardware encoders (`mpph265enc`, `mpph264enc`) | ALSA → L16 |
| `v4l2` | any Linux with a webcam | FFmpeg, software encoding (x264 / x265) | ALSA → L16 |
| `test` | anywhere, even Windows | FFmpeg `testsrc2` test pattern in 4K | a 440 Hz A tone |
| `custom` | board not supported yet | your own command, with placeholders `{main_url}`, `{width}`… | — |

## Security, without TLS or an account

- **Pairing**: an 8-character code (≈ 40 bits, without 0/O/1/I/L) is randomly drawn at
  installation. HomeSmart and the camera prove to each other that they know it using **SPAKE2**: the
  code never goes over the network and an eavesdropper cannot guess it offline. Each attempt costs
  one request; after 5 failures, the camera refuses attempts for 10 minutes.
- **After that**, every request is **signed** (HMAC-SHA256 with the shared secret, covering the
  method, the path, the time, a nonce and the body): no forgery and no replay. A camera whose clock
  drifts sends back its own time; HomeSmart corrects the offset and tries again.
- **RTSP**: only paired HomeSmart instances can read the streams (Digest), with a password derived
  from the secret, which therefore never travels over the network. Publishing streams is only
  allowed from the camera itself. HLS, WebRTC, RTMP, SRT and the MediaMTX API are closed.
- On the camera, the agent runs as a dedicated user (`homesmart-cam`, groups `video` and `audio`),
  without root privileges, and only writes to `/var/lib/homesmart-cam` (files with mode 600).
- Removing the camera from HomeSmart makes the camera forget that HomeSmart; `homesmart-cam reset`
  on the camera erases all pairings and draws a new code.

## Installing on the camera

1. **System**: Radxa OS (Debian) on the microSD card, Wi-Fi or Ethernet configured, SSH access.
   Connect the Camera 4K to the **4-lane** connector, then enable it: `sudo rsetup`, Overlays,
   Manage overlays, check the camera, reboot.
2. **The agent** for the board (64-bit ARM), either:
   - on the board: install Rust (`curl https://sh.rustup.rs -sSf | sh`), copy this repository, then
     `cargo build --release -p homesmart-cam`;
   - on the PC: `cargo zigbuild --release -p homesmart-cam --target aarch64-unknown-linux-gnu`
     (with [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild); compiling for 64-bit ARM
     has been verified, linking with zig not yet).
3. Copy `homesmart-cam` together with `deploy/install.sh` and `deploy/homesmart-cam.service` into
   the same folder on the board, then run `sudo ./install.sh`: GStreamer / FFmpeg / ALSA packages,
   MediaMTX v1.21.3 (SHA-256 hash verified), dedicated user, systemd service. The script displays
   the **pairing code**.
4. In HomeSmart: **Caméras, Ajouter** (Cameras, Add), choose the camera found, type the code,
   **Appairer** (Pair). Or: `homesmart-cli cam pair <IP> <CODE>`.

Useful commands on the camera:

```bash
journalctl -u homesmart-cam -f                     # what the agent is doing
sudo -u homesmart-cam HOMESMART_CAM_CONFIG=/var/lib/homesmart-cam/config.json homesmart-cam code      # show the code again
sudo -u homesmart-cam HOMESMART_CAM_CONFIG=/var/lib/homesmart-cam/config.json homesmart-cam pipeline  # commands launched
```

The configuration (`/var/lib/homesmart-cam/config.json`) lets you force what detection might miss:
`video_device`, `audio_device` (`hw:1,0`, `plughw:CARD=Mic`…), `has_mic`, `caps` (maximum
resolution, frame rate and codecs), the tool paths, or a `custom` command.

## Trying it without hardware (Windows or Linux PC)

You need FFmpeg, which HomeSmart installs from its Préférences (Preferences), and MediaMTX (placed
next to `homesmart-cam`, or its path set in `tools.mediamtx` in the configuration):

```bash
cargo run -p homesmart-cam -- run --sim
```

The agent streams a 4K test pattern in H.265, a 720p stream in H.264 and a 440 Hz A tone in 16 kHz
PCM, with a separate configuration (`config-sim.json`). HomeSmart finds it just like a real camera.

## What has been verified, and what remains to be validated on the board

Verified (2026-10-26), on a PC with the `test` backend, the real MediaMTX v1.21.3 and the real
HomeSmart: mDNS discovery, pairing (wrong code rejected, correct code accepted), HD stream
**3840×2160 H.265 at 30 fps (≈ 8 Mbit/s)** and 720p H.264 smooth stream, **16 kHz PCM sound**
received in both, 4K snapshot, settings changed remotely (1080p H.264 without sound, then back to
4K), removal that makes the camera forget HomeSmart. Automated tests: protocol, agent (API,
signatures, replay, lockout, commands of the 4 backends, process restarts) and a complete run
through HomeSmart's hub.

Not verified yet, for lack of a board:

- the `rockchip` pipeline itself: name of the video device (`/dev/video-camera0` assumed), possible
  need for Rockchip's 3A service (`rkaiq`) for exposure and white balance, property names of the
  MPP encoders (`bps`, `gop`, `rc-mode`, `rotation`, `width` / `height`), and the package for the
  GStreamer MPP plugin on Radxa OS;
- the permissions of the `homesmart-cam` user on `/dev/mpp_service`, `/dev/rga` and `/dev/dma_heap`;
- the I²S microphone (device tree overlay to enable);
- thermal behavior in a closed case.

If the Rockchip pipeline does not start, `homesmart-cam pipeline` shows the command launched and
`journalctl -u homesmart-cam` the last error; HomeSmart also shows it in the camera's settings.

## Licenses

Agent and protocol: MIT, like HomeSmart. Dependencies: MediaMTX (MIT), axum (MIT), mdns-sd
(MIT / Apache-2.0), spake2 (MIT / Apache-2.0), GStreamer and FFmpeg (LGPL, run as separate
programs). No model or binary is included: MediaMTX is downloaded at installation, with its hash
verified.
