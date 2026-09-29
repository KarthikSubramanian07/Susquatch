# shellcheck shell=bash
# Shared helpers for ops/*.sh. Sourced, never executed. Works on bash 3.2 (macOS) and 5.x (Ubuntu).

# Resolve SUSQ_HOME (the checkout) from the calling script unless already set.
susq_init() {
  local here
  here="$(cd "$(dirname "${BASH_SOURCE[1]}")" && pwd)"
  SUSQ_HOME="${SUSQ_HOME:-$(cd "$here/.." && pwd)}"
  export SUSQ_HOME
  OPS_STATE="$SUSQ_HOME/ops/state"
  ACTIONS_DIR="$OPS_STATE/actions"
  mkdir -p "$ACTIONS_DIR"
  susq_load_env
  # Root-run helpers (the priority units) must not leave root-owned files the containers can't write.
  # (A script that sets its own EXIT trap replaces this one; only priority.sh runs as root and it has none.)
  if [ "$(id -u)" = 0 ]; then
    trap 'chown -R --reference="$SUSQ_HOME" "$OPS_STATE" 2>/dev/null || true' EXIT
  fi
}

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2; }
die() { log "ERROR: $*"; exit 1; }

# Export KEY=VALUE lines from $SUSQ_HOME/.env without letting an existing environment be overridden.
susq_load_env() {
  local env_file="$SUSQ_HOME/.env" line key
  [ -f "$env_file" ] || return 0
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in ''|\#*) continue ;; esac
    key="${line%%=*}"
    case "$key" in *[!A-Za-z0-9_]*|'') continue ;; esac
    if [ -z "${!key+x}" ]; then
      export "$key=${line#*=}"
    fi
  done < "$env_file"
}

# toml_get <section> <key> <default>: read a scalar from config/susq.toml (quotes and _ separators stripped).
toml_get() {
  local section="$1" key="$2" default="$3" value
  value="$(awk -v s="[$section]" -v k="$key" '
    /^\[/ { in_s = ($0 == s) ; next }
    in_s && $1 == k { sub(/^[^=]*=[ \t]*/, ""); sub(/[ \t]*#.*$/, ""); gsub(/"/, ""); print; exit }
  ' "$SUSQ_HOME/config/susq.toml" 2>/dev/null)"
  value="${value//_/}"
  printf '%s' "${value:-$default}"
}

# True inside the election-night priority window ([ops].priority_window_*; SUSQ_NOW_UTC overrides "now").
in_priority_window() {
  local start end now
  start="$(toml_get ops priority_window_start_utc 2026-11-03T23:00:00Z)"
  end="$(toml_get ops priority_window_end_utc 2026-11-04T17:00:00Z)"
  now="${SUSQ_NOW_UTC:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
  # ISO-8601 UTC strings of identical shape order correctly as text.
  [[ ! "$now" < "$start" ]] && [[ "$now" < "$end" ]]
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

file_bytes() { wc -c < "$1" | tr -d ' '; }

# ACTION NEEDED items: one file per item (see py/susq/ops/common.py).
action_raise() { printf '%s\n' "$2" > "$ACTIONS_DIR/$1.txt.tmp" && mv "$ACTIONS_DIR/$1.txt.tmp" "$ACTIONS_DIR/$1.txt"; }
action_resolve() { rm -f "$ACTIONS_DIR/$1.txt"; }
ops_event() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >> "$OPS_STATE/events.log"; }

# Configure the backup remote. Default: Cloudflare R2 via rclone's env-var config (no rclone.conf on disk).
# SUSQ_BACKUP_REMOTE overrides it entirely, e.g. a local directory or another rclone remote for drills.
backup_remote_init() {
  if [ -n "${SUSQ_BACKUP_REMOTE:-}" ]; then
    BACKUP_REMOTE="$SUSQ_BACKUP_REMOTE"
  else
    [ -n "${SUSQ_R2_ACCESS_KEY_ID:-}" ] || die "SUSQ_R2_ACCESS_KEY_ID not set (see docs/OPS.md, Secrets)"
    [ -n "${SUSQ_R2_SECRET_ACCESS_KEY:-}" ] || die "SUSQ_R2_SECRET_ACCESS_KEY not set"
    local endpoint="${SUSQ_R2_ENDPOINT:-}"
    if [ -z "$endpoint" ]; then
      [ -n "${CLOUDFLARE_ACCOUNT_ID:-}" ] || die "CLOUDFLARE_ACCOUNT_ID (or SUSQ_R2_ENDPOINT) not set"
      endpoint="https://${CLOUDFLARE_ACCOUNT_ID}.r2.cloudflarestorage.com"
    fi
    export RCLONE_CONFIG_SUSQR2_TYPE=s3
    export RCLONE_CONFIG_SUSQR2_PROVIDER=Cloudflare
    export RCLONE_CONFIG_SUSQR2_ACCESS_KEY_ID="$SUSQ_R2_ACCESS_KEY_ID"
    export RCLONE_CONFIG_SUSQR2_SECRET_ACCESS_KEY="$SUSQ_R2_SECRET_ACCESS_KEY"
    export RCLONE_CONFIG_SUSQR2_ENDPOINT="$endpoint"
    export RCLONE_CONFIG_SUSQR2_NO_CHECK_BUCKET=true
    BACKUP_REMOTE="susqr2:${SUSQ_R2_BUCKET:-susquatch-backups}"
  fi
  # shellcheck disable=SC2034  # consumed by the scripts that source this file
  BACKUP_PREFIX="$BACKUP_REMOTE/${SUSQ_BACKUP_PREFIX:-susquatch}"
  command -v rclone >/dev/null 2>&1 || die "rclone not installed (ops/install.sh installs it)"
}

# Bytes used in the whole bucket (every project shares the 10 GB free tier), or empty on failure.
remote_bytes() {
  rclone size --json "$BACKUP_REMOTE" 2>/dev/null | sed -n 's/.*"bytes":[ ]*\([0-9][0-9]*\).*/\1/p'
}

# docker compose for this stack. The project directory is the checkout, so paths in ops/compose.yaml are
# repo-relative and compose picks up $SUSQ_HOME/.env for interpolation on its own.
compose() {
  docker compose --project-directory "$SUSQ_HOME" -f "$SUSQ_HOME/ops/compose.yaml" "$@"
}
