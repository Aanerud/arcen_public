#!/usr/bin/env bash
# Generate or explicitly renew the operator-managed Linux Pier PEM pair.
set -euo pipefail

umask 077

TLS_DIRECTORY="${ARCEN_TLS_DIRECTORY:-/etc/arcen/tls}"
OPENSSL="${OPENSSL:-openssl}"
MODE="if-missing"
ADOPT_LEGACY=0
declare -a DNS_NAMES=()
declare -a IP_ADDRESSES=()

usage() {
  cat <<'EOF'
Usage: new-host-cert.sh [--renew [--adopt-legacy] | --new-key] [--directory DIR]
                        [--dns NAME]... [--ip ADDRESS]...

No arguments generates a P-256/SHA-256 pair only when both host.crt and
host.key are absent. --renew reissues with the existing key. --new-key
explicitly changes trust by replacing the key. The certificate is valid for
825 days and includes serverAuth EKU and digitalSignature key usage.
--adopt-legacy explicitly authorizes same-key renewal of unmarked output from
the previous Arcen helper; never use it for enterprise/custom PEM.
EOF
}

while (($#)); do
  case "$1" in
    --renew)
      [[ "$MODE" == "if-missing" ]] || { echo "choose only one issuance mode" >&2; exit 2; }
      MODE="renew"
      ;;
    --new-key)
      [[ "$MODE" == "if-missing" ]] || { echo "choose only one issuance mode" >&2; exit 2; }
      MODE="new-key"
      ;;
    --adopt-legacy)
      ADOPT_LEGACY=1
      ;;
    --directory)
      shift
      (($#)) || { echo "--directory requires a path" >&2; exit 2; }
      TLS_DIRECTORY="$1"
      ;;
    --dns)
      shift
      (($#)) || { echo "--dns requires a name" >&2; exit 2; }
      DNS_NAMES+=("$1")
      ;;
    --ip)
      shift
      (($#)) || { echo "--ip requires an address" >&2; exit 2; }
      IP_ADDRESSES+=("$1")
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

if ((ADOPT_LEGACY == 1)) && [[ "$MODE" != "renew" ]]; then
  echo "--adopt-legacy requires --renew" >&2
  exit 2
fi

command -v "$OPENSSL" >/dev/null 2>&1 || {
  echo "OpenSSL is required to issue Linux Pier certificates" >&2
  exit 1
}
command -v flock >/dev/null 2>&1 || {
  echo "util-linux flock is required to serialize Linux Pier certificate issuance" >&2
  exit 1
}

reject_symlink_chain() {
  local path="$1" current="" part
  if [[ "$path" == /* ]]; then
    current="/"
  fi
  IFS='/' read -r -a parts <<<"$path"
  for part in "${parts[@]}"; do
    [[ -n "$part" && "$part" != "." ]] || continue
    [[ "$part" != ".." ]] || { echo "unsafe parent traversal: $path" >&2; return 1; }
    if [[ "$current" == "/" ]]; then
      current="/$part"
    elif [[ -z "$current" ]]; then
      current="$part"
    else
      current="$current/$part"
    fi
    [[ ! -L "$current" ]] || { echo "refusing symbolic-link path component: $current" >&2; return 1; }
  done
}

reject_symlink_chain "$TLS_DIRECTORY"
install -d -m 0750 "$TLS_DIRECTORY"
reject_symlink_chain "$TLS_DIRECTORY"
directory_owner="$(stat -c '%u' "$TLS_DIRECTORY")"
directory_mode="$(stat -c '%a' "$TLS_DIRECTORY")"
if [[ "$directory_owner" != "$(id -u)" ]] || ((8#$directory_mode & 0022)); then
  echo "TLS directory must be owned by the invoking account and not group/other writable" >&2
  exit 1
fi

CERT="$TLS_DIRECTORY/host.crt"
KEY="$TLS_DIRECTORY/host.key"
CERT_PIN="$TLS_DIRECTORY/host.cert-sha256"
SPKI_PIN="$TLS_DIRECTORY/host.spki-sha256"
MARKER="$TLS_DIRECTORY/host.generated-by-arcen"
JOURNAL="$TLS_DIRECTORY/.arcen-cert.transaction"
LOCK_FILE="$TLS_DIRECTORY/.arcen-cert.lock"

for path in "$CERT" "$KEY" "$CERT_PIN" "$SPKI_PIN" "$MARKER" "$JOURNAL" "$LOCK_FILE"; do
  reject_symlink_chain "$path"
done
command -v flock >/dev/null 2>&1 || {
  echo "flock is required to serialize TLS material transactions" >&2
  exit 1
}
if [[ ! -e "$LOCK_FILE" ]]; then
  (set -o noclobber; : >"$LOCK_FILE") 2>/dev/null || true
fi
[[ -f "$LOCK_FILE" && ! -L "$LOCK_FILE" ]] || {
  echo "unsafe TLS transaction lock" >&2
  exit 1
}
chmod 0600 "$LOCK_FILE"
exec {LOCK_FD}<>"$LOCK_FILE"
flock -n "$LOCK_FD" || {
  echo "another host-certificate transaction is active" >&2
  exit 1
}

fsync_path() {
  sync -f "$1"
}

recover_transaction() {
  [[ -e "$JOURNAL" ]] || return 0
  [[ -f "$JOURNAL" && ! -L "$JOURNAL" ]] || {
    echo "unsafe TLS transaction journal" >&2
    return 1
  }
  local line transaction="" phase=""
  declare -A existed=()
  while IFS= read -r line; do
    case "$line" in
      transaction=*) transaction="${line#transaction=}" ;;
      phase=prepared|phase=committed) phase="${line#phase=}" ;;
      existed.host.key=0|existed.host.key=1) existed["host.key"]="${line##*=}" ;;
      existed.host.crt=0|existed.host.crt=1) existed["host.crt"]="${line##*=}" ;;
      existed.host.cert-sha256=0|existed.host.cert-sha256=1)
        existed["host.cert-sha256"]="${line##*=}"
        ;;
      existed.host.spki-sha256=0|existed.host.spki-sha256=1)
        existed["host.spki-sha256"]="${line##*=}"
        ;;
      existed.host.generated-by-arcen=0|existed.host.generated-by-arcen=1)
        existed["host.generated-by-arcen"]="${line##*=}"
        ;;
      *) echo "invalid TLS transaction journal" >&2; return 1 ;;
    esac
  done <"$JOURNAL"
  [[ "$transaction" =~ ^[0-9]+-[0-9]+$ ]] || {
    echo "invalid TLS transaction identifier" >&2
    return 1
  }
  [[ -n "$phase" ]] || { echo "TLS transaction journal has no phase" >&2; return 1; }
  local name final backup
  for name in host.key host.crt host.cert-sha256 host.spki-sha256 host.generated-by-arcen; do
    final="$TLS_DIRECTORY/$name"
    backup="$TLS_DIRECTORY/.arcen-cert.backup.$transaction.$name"
    reject_symlink_chain "$final"
    reject_symlink_chain "$backup"
    if [[ "$phase" == "committed" ]]; then
      [[ -f "$final" && ! -L "$final" ]] || {
        echo "committed TLS transaction is missing $name" >&2
        return 1
      }
    elif [[ -e "$backup" ]]; then
      [[ -f "$backup" && ! -L "$backup" ]] || { echo "unsafe TLS backup" >&2; return 1; }
      rm -f "$final"
      mv "$backup" "$final"
    elif [[ "${existed[$name]:-}" == "0" ]]; then
      rm -f "$final"
    elif [[ "${existed[$name]:-}" == "1" ]]; then
      [[ -f "$final" && ! -L "$final" ]] || {
        echo "uncommitted TLS transaction cannot restore $name" >&2
        return 1
      }
    else
      echo "incomplete TLS transaction journal" >&2
      return 1
    fi
  done
  rm -f "$TLS_DIRECTORY"/".arcen-cert.stage.$transaction."*
  rm -f "$TLS_DIRECTORY"/".arcen-cert.backup.$transaction."*
  rm -f "$JOURNAL"
  fsync_path "$TLS_DIRECTORY"
}

recover_transaction

certificate_pin() {
  "$OPENSSL" x509 -in "$1" -noout -fingerprint -sha256
}

spki_pin() {
  printf 'sha256/%s' "$(
    "$OPENSSL" x509 -in "$1" -pubkey -noout |
      "$OPENSSL" pkey -pubin -outform DER |
      "$OPENSSL" dgst -sha256 -binary |
      "$OPENSSL" base64 -A
  )"
}

verify_managed_pair() {
  local cert_key_digest key_digest
  cert_key_digest="$("$OPENSSL" x509 -in "$CERT" -pubkey -noout |
    "$OPENSSL" pkey -pubin -outform DER |
    "$OPENSSL" dgst -sha256)"
  key_digest="$("$OPENSSL" pkey -in "$KEY" -pubout -outform DER |
    "$OPENSSL" dgst -sha256)"
  [[ "$cert_key_digest" == "$key_digest" ]] || {
    echo "certificate and private key do not match" >&2
    return 1
  }
  if [[ ! -e "$MARKER" && "$ADOPT_LEGACY" == "1" ]]; then
    echo "adopting explicitly authorized legacy Arcen TLS material for same-key renewal" >&2
    return 0
  fi
  [[ -f "$MARKER" && ! -L "$MARKER" ]] || {
    echo "refusing to overwrite enterprise/custom PEM material" >&2
    return 1
  }
  local version="" marker_cert="" marker_spki="" line
  while IFS= read -r line; do
    case "$line" in
      version=3) version=3 ;;
      certificate=*) marker_cert="${line#certificate=}" ;;
      spki=*) marker_spki="${line#spki=}" ;;
      *) echo "invalid helper ownership marker" >&2; return 1 ;;
    esac
  done <"$MARKER"
  [[ "$version" == "3" \
    && "$marker_cert" == "$(certificate_pin "$CERT")" \
    && "$marker_spki" == "$(spki_pin "$CERT")" ]] || {
    echo "TLS material no longer matches its helper ownership marker" >&2
    return 1
  }
}

cert_exists=0
key_exists=0
[[ -e "$CERT" ]] && cert_exists=1
[[ -e "$KEY" ]] && key_exists=1
if ((cert_exists != key_exists)); then
  echo "refusing partial TLS material; restore or remove both host.crt and host.key" >&2
  exit 1
fi
if [[ "$MODE" == "if-missing" && "$cert_exists" == 1 ]]; then
  echo "existing complete TLS pair left untouched"
  exit 0
fi
if [[ "$MODE" != "if-missing" && "$cert_exists" == 0 ]]; then
  echo "explicit renewal/rekey requires an existing helper-managed pair" >&2
  exit 1
fi

if [[ "$cert_exists" == 1 ]]; then
  [[ -f "$CERT" && ! -L "$CERT" && -f "$KEY" && ! -L "$KEY" ]] || {
    echo "existing TLS material must be regular files" >&2
    exit 1
  }
  if [[ "$MODE" != "if-missing" ]]; then
    verify_managed_pair
  fi
fi

fqdn="$(hostname -f 2>/dev/null || hostname)"
[[ -n "$fqdn" && "$fqdn" != *[[:space:]]* && "$fqdn" != *","* ]] || {
  echo "could not determine a safe host FQDN" >&2
  exit 1
}
DNS_NAMES=("$fqdn" "${DNS_NAMES[@]}")

if ((${#IP_ADDRESSES[@]} == 0)); then
  while IFS= read -r address; do
    [[ -n "$address" ]] && IP_ADDRESSES+=("$address")
  done < <(hostname -I 2>/dev/null | tr ' ' '\n' | sed '/^$/d' || true)
fi

declare -a SAFE_DNS=()
declare -a SAFE_IPS=()
declare -A SEEN=()
for name in "${DNS_NAMES[@]}"; do
  [[ -n "$name" && ${#name} -le 253 && "$name" =~ ^[A-Za-z0-9.-]+$ \
    && "$name" != .* && "$name" != *. && "$name" != *..* ]] || {
    echo "invalid DNS SAN" >&2
    exit 1
  }
  if [[ -z "${SEEN["DNS:$name"]+x}" ]]; then
    SAFE_DNS+=("$name")
    SEEN["DNS:$name"]=1
  fi
done

valid_ip_address() {
  local address="$1" octet
  if [[ "$address" == *:* ]]; then
    [[ "$address" =~ ^[0-9A-Fa-f:]+$ && "$address" == *:* && "$address" != "::" ]]
    return
  fi
  [[ "$address" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || return 1
  IFS='.' read -r -a octets <<<"$address"
  ((${#octets[@]} == 4)) || return 1
  for octet in "${octets[@]}"; do
    ((10#$octet <= 255)) || return 1
  done
}

for address in "${IP_ADDRESSES[@]}"; do
  address="${address%%\%*}"
  [[ "$address" != 127.* && "$address" != "0.0.0.0" && "$address" != "::1" ]] \
    && valid_ip_address "$address" || {
    echo "loopback or invalid IP SAN rejected" >&2
    exit 1
  }
  if [[ -z "${SEEN["IP:$address"]+x}" ]]; then
    SAFE_IPS+=("$address")
    SEEN["IP:$address"]=1
  fi
done
((${#SAFE_IPS[@]} > 0)) || {
  echo "at least one explicit non-loopback IPv4 or IPv6 SAN is required" >&2
  exit 1
}

san=""
for name in "${SAFE_DNS[@]}"; do
  [[ -z "$san" ]] || san+=","
  san+="DNS:$name"
done
for address in "${SAFE_IPS[@]}"; do
  [[ -z "$san" ]] || san+=","
  san+="IP:$address"
done

transaction="$$-$(date +%s)"
STAGE_KEY="$TLS_DIRECTORY/.arcen-cert.stage.$transaction.host.key"
STAGE_CERT="$TLS_DIRECTORY/.arcen-cert.stage.$transaction.host.crt"
STAGE_CERT_PIN="$TLS_DIRECTORY/.arcen-cert.stage.$transaction.host.cert-sha256"
STAGE_SPKI_PIN="$TLS_DIRECTORY/.arcen-cert.stage.$transaction.host.spki-sha256"
STAGE_MARKER="$TLS_DIRECTORY/.arcen-cert.stage.$transaction.host.generated-by-arcen"
for path in "$STAGE_KEY" "$STAGE_CERT" "$STAGE_CERT_PIN" "$STAGE_SPKI_PIN" "$STAGE_MARKER"; do
  reject_symlink_chain "$path"
  [[ ! -e "$path" ]] || { echo "TLS stage collision" >&2; exit 1; }
done

transaction_started=0
cleanup_or_recover() {
  local status=$?
  trap - EXIT INT TERM HUP
  if ((transaction_started)); then
    recover_transaction || true
  else
    rm -f "$STAGE_KEY" "$STAGE_CERT" "$STAGE_CERT_PIN" "$STAGE_SPKI_PIN" "$STAGE_MARKER"
  fi
  exit "$status"
}
trap cleanup_or_recover EXIT INT TERM HUP

if [[ "$MODE" == "renew" ]]; then
  cp "$KEY" "$STAGE_KEY"
else
  "$OPENSSL" genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$STAGE_KEY"
fi
chmod 0600 "$STAGE_KEY"
"$OPENSSL" req -new -x509 -key "$STAGE_KEY" -sha256 -days 825 \
  -subj "/CN=$fqdn" \
  -addext "subjectAltName=$san" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "extendedKeyUsage=serverAuth" \
  -addext "keyUsage=critical,digitalSignature" \
  -out "$STAGE_CERT"
chmod 0644 "$STAGE_CERT"

"$OPENSSL" x509 -in "$STAGE_CERT" -noout >/dev/null
"$OPENSSL" pkey -in "$STAGE_KEY" -check -noout >/dev/null
cert_spki="$("$OPENSSL" x509 -in "$STAGE_CERT" -pubkey -noout |
  "$OPENSSL" pkey -pubin -outform DER |
  "$OPENSSL" dgst -sha256)"
key_spki="$("$OPENSSL" pkey -in "$STAGE_KEY" -pubout -outform DER |
  "$OPENSSL" dgst -sha256)"
[[ "$cert_spki" == "$key_spki" ]] || { echo "generated certificate and key do not match" >&2; exit 1; }

certificate_pin "$STAGE_CERT" >"$STAGE_CERT_PIN"
spki_pin "$STAGE_CERT" >"$STAGE_SPKI_PIN"
printf '\n' >>"$STAGE_SPKI_PIN"
printf 'version=3\ncertificate=%s\nspki=%s\n' \
  "$(tr -d '\r\n' <"$STAGE_CERT_PIN")" \
  "$(tr -d '\r\n' <"$STAGE_SPKI_PIN")" >"$STAGE_MARKER"
chmod 0644 "$STAGE_CERT_PIN" "$STAGE_SPKI_PIN" "$STAGE_MARKER"

for path in "$STAGE_KEY" "$STAGE_CERT" "$STAGE_CERT_PIN" "$STAGE_SPKI_PIN" "$STAGE_MARKER"; do
  [[ -f "$path" && ! -L "$path" ]] || { echo "unsafe TLS stage file" >&2; exit 1; }
  fsync_path "$path"
done

write_journal() {
  local phase="$1" temporary="$TLS_DIRECTORY/.arcen-cert.journal.$transaction.new"
  reject_symlink_chain "$temporary"
  [[ ! -e "$temporary" ]] || { echo "TLS journal stage collision" >&2; return 1; }
  (
    set -o noclobber
    {
      printf 'transaction=%s\n' "$transaction"
      printf 'phase=%s\n' "$phase"
      for name in host.key host.crt host.cert-sha256 host.spki-sha256 host.generated-by-arcen; do
        if [[ -e "$TLS_DIRECTORY/$name" ]]; then
          printf 'existed.%s=1\n' "$name"
        else
          printf 'existed.%s=0\n' "$name"
        fi
      done
    } >"$temporary"
  )
  chmod 0600 "$temporary"
  fsync_path "$temporary"
  mv "$temporary" "$JOURNAL"
  fsync_path "$TLS_DIRECTORY"
}

write_journal prepared
transaction_started=1

for name in host.key host.crt host.cert-sha256 host.spki-sha256 host.generated-by-arcen; do
  final="$TLS_DIRECTORY/$name"
  backup="$TLS_DIRECTORY/.arcen-cert.backup.$transaction.$name"
  reject_symlink_chain "$final"
  reject_symlink_chain "$backup"
  if [[ -e "$final" ]]; then
    [[ -f "$final" && ! -L "$final" ]] || { echo "unsafe TLS destination" >&2; exit 1; }
    mv "$final" "$backup"
    chmod 0600 "$backup"
  fi
done
fsync_path "$TLS_DIRECTORY"

mv "$STAGE_KEY" "$KEY"
mv "$STAGE_CERT" "$CERT"
mv "$STAGE_CERT_PIN" "$CERT_PIN"
mv "$STAGE_SPKI_PIN" "$SPKI_PIN"
mv "$STAGE_MARKER" "$MARKER"
chmod 0600 "$KEY"
chmod 0644 "$CERT" "$CERT_PIN" "$SPKI_PIN" "$MARKER"
for path in "$KEY" "$CERT" "$CERT_PIN" "$SPKI_PIN" "$MARKER"; do
  fsync_path "$path"
done
fsync_path "$TLS_DIRECTORY"
write_journal committed

rm -f "$TLS_DIRECTORY"/".arcen-cert.backup.$transaction."*
rm -f "$JOURNAL"
fsync_path "$TLS_DIRECTORY"
transaction_started=0
trap - EXIT INT TERM HUP

echo "issued Linux Pier certificate; reload explicitly with: systemctl reload arcen-pier"
