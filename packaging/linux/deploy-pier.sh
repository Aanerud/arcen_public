#!/usr/bin/env bash
# Deploy Arcen Pier (Linux) to a workstation host over SSH.
#
# Builds on the target box (needs its GPU + NVENC), installs the binaries under
# /opt/arcen/bin/, prepares TLS, and
# installs the arcen-pier systemd unit and starts QUIC on UDP 18444.
#
# Usage: HOST=root@<your-pier-host> packaging/linux/deploy-pier.sh
#   (pier-linux.example.internal = root@<your-pier-host>, key ~/.ssh/id_ecdsa)
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: HOST=user@ip packaging/linux/deploy-pier.sh

  -h, --help                 Show this help.
EOF
}

while (($#)); do
  case "$1" in
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "deploy-pier.sh: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

HOST="${HOST:?set HOST=user@ip}"
KEY="${KEY:-$HOME/.ssh/id_ecdsa}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SSH="ssh -i $KEY -o BatchMode=yes"

echo "==> rsync source → $HOST:/root/arcen/"
rsync -az -e "$SSH" \
  --exclude target --exclude .git --exclude 'Arcen Deck.app' --exclude .disk-guardian.log \
  "$REPO/" "$HOST:/root/arcen/"

echo "==> build fused Pier on $HOST"
$SSH "$HOST" 'export PATH=$HOME/.cargo/bin:$PATH; cd /root/arcen &&
  command -v c++ >/dev/null &&
  c++ --version &&
  command -v nasm >/dev/null &&
  nasm -v &&
  command -v readelf >/dev/null &&
  bash scripts/verify-opusic-source.sh &&
  cargo build --locked --release -p arcen-pier-linux &&
  bash scripts/verify-openh264-assembly.sh target/release &&
  pier_dynamic="$(readelf -d target/release/arcen-pier)" &&
  ! printf "%s\n" "$pier_dynamic" | grep -F "Shared library: [libopus.so" &&
  ! printf "%s\n" "$pier_dynamic" | grep -Ei "Shared library: \[(lib)?openh264" &&
  ! find target/release -maxdepth 2 -type f \
     \( -iname "libopenh264*.so*" -o -iname "libopenh264*.a" -o -iname "openh264*.dll" \) \
    -print -quit | grep -q . &&
  python3 scripts/verify_quic_product_binary.py target/release/arcen-pier &&
  target/release/arcen-pier validate-config --config packaging/linux/arcen-pier.json'

echo "==> install binaries + TLS helper + logging policy + unit"
$SSH "$HOST" 'set -e; cd /root/arcen
  mkdir -p /opt/arcen/bin /usr/share/doc/arcen /etc/arcen
  command -v openssl >/dev/null
  command -v flock >/dev/null
  if [ -s /etc/arcen/host.crt ] &&
     ! openssl x509 -in /etc/arcen/host.crt -noout -ext subjectAltName 2>/dev/null |
       grep -Eq "DNS:|IP Address:"; then
    echo "Existing certificate has no DNS/IP SAN and cannot start this Pier release." >&2
    echo "Enterprise: replace it with a SAN-bearing CA-issued pair before upgrade." >&2
    echo "Legacy Arcen SMB same-key renewal:" >&2
    echo "  packaging/linux/new-host-cert.sh --renew --adopt-legacy --directory /etc/arcen" >&2
    exit 1
  fi
  install -m 0755 target/release/arcen-pier /opt/arcen/bin/arcen-pier
  install -m 0755 packaging/linux/new-host-cert.sh /opt/arcen/bin/arcen-new-host-cert
  /opt/arcen/bin/arcen-new-host-cert --directory /etc/arcen
  # Persistent single-head NVIDIA Xorg template for the dedicated-Xorg session model.
  mkdir -p /var/log/arcen
  install -d -m 0700 /var/lib/arcen/support
  chmod 0750 /var/log/arcen
  install -m 0644 packaging/linux/arcen-xorg.conf /etc/arcen/xorg.conf
  install -m 0640 packaging/linux/arcen-pier.json /etc/arcen/pier.json
  rm -f /etc/arcen/logging.json
  /opt/arcen/bin/arcen-pier validate-config --config /etc/arcen/pier.json
  install -m 0644 packaging/linux/arcen-pier.logrotate /etc/logrotate.d/arcen-pier
  install -m 0644 packaging/linux/arcen-pier.service /etc/systemd/system/arcen-pier.service
  install -m 0644 legal/THIRD_PARTY_NOTICES.md /usr/share/doc/arcen/THIRD_PARTY_NOTICES.md
  command -v logrotate >/dev/null
  logrotate --debug /etc/logrotate.d/arcen-pier >/dev/null
  systemctl daemon-reload
  systemctl enable arcen-pier.service
  systemctl restart arcen-pier.service
  bash packaging/linux/deploy-pier-preflight.sh /opt/arcen/bin/arcen-pier'

echo "==> done. Arcen Pier live on $HOST (QUIC 18444/udp)"
