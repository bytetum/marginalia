#!/usr/bin/env bash
# Nightly Marginalia backup: JSON snapshot of the library (GET /export) plus the blob files.
# Install as ~/.local/bin/marginalia-backup (see README, "Backup & restore"); run by marginalia-backup.timer.
#
# Settings come from the environment, else from ~/.config/marginalia/env (KEY=VALUE lines, as for the service):
#   MARGINALIA_BIND         address of `marginalia serve`        (default 127.0.0.1:7878)
#   MARGINALIA_TOKEN        bearer token, if the server has one
#   MARGINALIA_DATA         data dir holding blobs/               (default ~/.local/share/marginalia/data)
#   MARGINALIA_BACKUP_DIR   where dated backups go                (default ~/.local/share/marginalia/backups)
#   MARGINALIA_ENV_FILE     env file to read                      (default ~/.config/marginalia/env)
set -euo pipefail
umask 077

KEEP=14

die() {
  echo "marginalia-backup: $*" >&2
  exit 1
}

ENV_FILE="${MARGINALIA_ENV_FILE:-$HOME/.config/marginalia/env}"

# setting KEY DEFAULT: the environment wins, then the last KEY=VALUE line of the env file, then DEFAULT.
# The file is parsed, never sourced. Surrounding quotes on the value are dropped.
setting() {
  local key=$1 default=$2 value="" line
  if [[ -n "${!key:-}" ]]; then
    printf '%s\n' "${!key}"
    return
  fi
  if [[ -r "$ENV_FILE" ]]; then
    line=$(grep -E "^[[:space:]]*${key}=" "$ENV_FILE" | tail -n 1 || true)
    if [[ -n "$line" ]]; then
      value=${line#*=}
      value=${value%"${value##*[![:space:]]}"}
      value=${value#"${value%%[![:space:]]*}"}
      if [[ "$value" == \"*\" || "$value" == \'*\' ]]; then
        value=${value:1:${#value}-2}
      fi
    fi
  fi
  printf '%s\n' "${value:-$default}"
}

BIND=$(setting MARGINALIA_BIND 127.0.0.1:7878)
TOKEN=$(setting MARGINALIA_TOKEN "")
DATA_DIR=$(setting MARGINALIA_DATA "$HOME/.local/share/marginalia/data")
BACKUP_ROOT=$(setting MARGINALIA_BACKUP_DIR "$HOME/.local/share/marginalia/backups")

# A server bound to every interface is still reachable on loopback.
case "$BIND" in
  0.0.0.0:* | "[::]:"*) BIND="127.0.0.1:${BIND##*:}" ;;
esac

command -v curl >/dev/null || die "curl is required"

# Valid JSON, non-empty, and shaped like an export. python3 or jq, whichever exists.
check_export() {
  local file=$1
  [[ -s "$file" ]] || die "export is empty"
  if command -v python3 >/dev/null; then
    python3 -I - "$file" 2>/dev/null <<'PY' || die "export is not a valid library snapshot"
import json, sys
with open(sys.argv[1], encoding="utf-8") as f:
    d = json.load(f)
ok = isinstance(d, dict) and isinstance(d.get("version"), int) and all(
    isinstance(d.get(k), list) for k in ("articles", "highlights", "syntheses")
)
sys.exit(0 if ok else 1)
PY
  elif command -v jq >/dev/null; then
    jq -e '(.version | type == "number") and (.articles | type == "array") and (.highlights | type == "array") and (.syntheses | type == "array")' \
      "$file" >/dev/null || die "export is not a valid library snapshot"
  else
    die "need python3 or jq to validate the export"
  fi
}

# Work in scratch paths under the backup root; the dated directory only appears once the export is good,
# so a failed run never leaves an empty directory that would count towards (and evict) real backups.
mkdir -p "$BACKUP_ROOT"
TMP=$(mktemp "$BACKUP_ROOT/.library.json.XXXXXX")
BLOBS_NEW="$BACKUP_ROOT/.blobs.new.$$"
cleanup() {
  rm -f -- "$TMP"
  rm -rf -- "$BLOBS_NEW"
}
trap cleanup EXIT

# The token goes through a curl config on stdin, so it never shows up in the process list.
auth_config=""
if [[ -n "$TOKEN" ]]; then
  esc=${TOKEN//\\/\\\\}
  esc=${esc//\"/\\\"}
  auth_config=$(printf 'header = "Authorization: Bearer %s"' "$esc")
fi
printf '%s\n' "$auth_config" |
  curl --config - --fail --silent --show-error --max-time 600 --output "$TMP" "http://$BIND/export" ||
  die "could not fetch http://$BIND/export (is marginalia serve running? is the token right?)"

check_export "$TMP"
chmod 600 "$TMP"
DEST="$BACKUP_ROOT/$(date +%F)"
mkdir -p "$DEST"
mv -f -- "$TMP" "$DEST/library.json"

# Blobs are immutable and content-addressed, so hard links are as good as copies and cost no space.
# Built beside the old copy and swapped in, so re-running on the same day does not nest directories.
if [[ -d "$DATA_DIR/blobs" ]]; then
  if ! cp -al -- "$DATA_DIR/blobs/." "$BLOBS_NEW" 2>/dev/null; then
    rm -rf -- "$BLOBS_NEW"
    cp -a -- "$DATA_DIR/blobs/." "$BLOBS_NEW"
  fi
else
  mkdir -p "$BLOBS_NEW"
  echo "marginalia-backup: no blobs directory at $DATA_DIR/blobs (nothing saved yet?)" >&2
fi
rm -rf -- "$DEST/blobs"
mv -- "$BLOBS_NEW" "$DEST/blobs"

# Keep the newest $KEEP dated directories.
dirs=()
for d in "$BACKUP_ROOT"/????-??-??; do
  [[ -d "$d" && "${d##*/}" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] && dirs+=("$d")
done
if ((${#dirs[@]} > KEEP)); then
  for d in "${dirs[@]:0:${#dirs[@]}-KEEP}"; do
    rm -rf -- "$d"
  done
fi

echo "marginalia-backup: wrote $DEST/library.json ($(wc -c <"$DEST/library.json") bytes), kept $((${#dirs[@]} > KEEP ? KEEP : ${#dirs[@]})) backup(s)"
