#!/bin/bash
# Pull Marginalia's nightly backups from shared-server to this Mac (the off-site copy).
# Runs hourly from launchd; a run while the server or tailnet is unreachable just fails and the next one retries.
# -H keeps the hard links between dated backups' blobs, so 14 days of blobs cost one copy.
set -euo pipefail
SRC="${MARGINALIA_PULL_SRC:-bytetum@shared-server:.local/share/marginalia/backups/}"
DEST="${MARGINALIA_PULL_DEST:-$HOME/Backups/marginalia/}"
mkdir -p "$DEST"
rsync -aH --delete -e "ssh -o BatchMode=yes -o ConnectTimeout=10" "$SRC" "$DEST"
date -u +%FT%TZ > "$DEST/.last-pull"
