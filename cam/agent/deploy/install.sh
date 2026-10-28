#!/bin/sh
# Installe l'agent homesmart-cam sur la caméra (Radxa ROCK 4D sous Radxa OS / Debian, ou tout
# Linux avec une webcam). À lancer sur la caméra, dans un dossier qui contient ce script,
# homesmart-cam.service et le binaire homesmart-cam (voir cam/README.md) :
#   sudo ./install.sh
set -eu

MEDIAMTX_VERSION=v1.21.3
# Empreintes SHA-256 publiées avec cette version (github.com/bluenviron/mediamtx/releases).
SHA_ARM64=27f038237cb5f77a724f6afc58eca2798f2bc4edf9989895631a80eeb41a4fe2
SHA_AMD64=0a2ea7eba96023631efa1bea7804823a9145b043b6646a8a05798d4ac24ed635
STATE=/var/lib/homesmart-cam

[ "$(id -u)" -eq 0 ] || { echo "À lancer avec sudo." >&2; exit 1; }
HERE=$(cd "$(dirname "$0")" && pwd)
BIN="$HERE/homesmart-cam"
[ -x "$BIN" ] || { echo "Binaire homesmart-cam introuvable à côté du script (voir cam/README.md)." >&2; exit 1; }

case "$(uname -m)" in
  aarch64|arm64) ARCH=arm64; SHA=$SHA_ARM64 ;;
  x86_64|amd64) ARCH=amd64; SHA=$SHA_AMD64 ;;
  *) echo "Architecture $(uname -m) non prise en charge." >&2; exit 1 ;;
esac

echo "1/5 Paquets (GStreamer, FFmpeg, ALSA)…"
apt-get update -qq
apt-get install -y -qq gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-alsa gstreamer1.0-rtsp ffmpeg alsa-utils curl ca-certificates
# Encodeurs matériels Rockchip (MPP), sur les cartes Rockchip seulement.
if grep -q rockchip /proc/device-tree/compatible 2>/dev/null; then
  apt-get install -y -qq gstreamer1.0-rockchip1 \
    || echo "  ! Paquet gstreamer1.0-rockchip1 introuvable : installe le greffon GStreamer MPP de ta distribution (voir cam/README.md)."
fi

echo "2/5 Serveur RTSP MediaMTX $MEDIAMTX_VERSION ($ARCH)…"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
TAR="mediamtx_${MEDIAMTX_VERSION}_linux_${ARCH}.tar.gz"
curl -fsSL -o "$TMP/$TAR" "https://github.com/bluenviron/mediamtx/releases/download/$MEDIAMTX_VERSION/$TAR"
echo "$SHA  $TMP/$TAR" | sha256sum -c - >/dev/null || { echo "Empreinte SHA-256 incorrecte : téléchargement refusé." >&2; exit 1; }
tar -xzf "$TMP/$TAR" -C "$TMP" mediamtx
install -m 0755 "$TMP/mediamtx" /usr/local/bin/mediamtx

echo "3/5 Agent homesmart-cam…"
install -m 0755 "$BIN" /usr/local/bin/homesmart-cam
id homesmart-cam >/dev/null 2>&1 || useradd --system --home-dir "$STATE" --shell /usr/sbin/nologin homesmart-cam
# Caméra (video), micro (audio), encodeurs matériels (render, s'il existe).
for g in video audio render; do
  if getent group "$g" >/dev/null; then usermod -aG "$g" homesmart-cam; fi
done
mkdir -p "$STATE"
chown homesmart-cam:homesmart-cam "$STATE"
chmod 700 "$STATE"

echo "4/5 Code d'appairage…"
# Crée la configuration (identifiant, code, matériel détecté) avant le premier démarrage.
HOMESMART_CAM_CONFIG="$STATE/config.json" runuser -u homesmart-cam -- /usr/local/bin/homesmart-cam code

echo "5/5 Service systemd…"
install -m 0644 "$HERE/homesmart-cam.service" /etc/systemd/system/homesmart-cam.service
systemctl daemon-reload
systemctl enable --now homesmart-cam
systemctl restart homesmart-cam

echo
echo "Caméra prête. Dans HomeSmart : Caméras, Ajouter, choisis « Caméra HomeSmart » et tape le code ci-dessus."
echo "État : journalctl -u homesmart-cam -f    Diagnostic : sudo -u homesmart-cam HOMESMART_CAM_CONFIG=$STATE/config.json homesmart-cam pipeline"
