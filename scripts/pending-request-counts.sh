#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"

BASE_URL="${BASE_URL:-http://localhost:3001}"
ENDPOINT="$BASE_URL/admin/api/v1/monitoring/pending-request-counts"

read_config_value() {
  local key="$1"
  sed -n "s/^[[:space:]]*${key}:[[:space:]]*\"\\(.*\\)\"[[:space:]]*$/\\1/p" "$ROOT_DIR/config.yaml" | head -n 1
}

SESSION="${DWCTL_SESSION:-}"
TEMP_FILES=()
cleanup() {
  if [ "${#TEMP_FILES[@]}" -gt 0 ]; then
    rm -f "${TEMP_FILES[@]}"
  fi
}
trap cleanup EXIT

if [ -z "$SESSION" ]; then
  EMAIL="${EMAIL:-$(read_config_value admin_email)}"
  PASSWORD="${PASSWORD:-$(read_config_value admin_password)}"

  if [ -z "$EMAIL" ] || [ -z "$PASSWORD" ]; then
    echo "Set DWCTL_SESSION, or set EMAIL and PASSWORD for a PlatformManager/admin user." >&2
    exit 1
  fi

  LOGIN_COOKIE_JAR="$(mktemp)"
  LOGIN_BODY="$(mktemp)"
  TEMP_FILES+=("$LOGIN_COOKIE_JAR" "$LOGIN_BODY")

  HTTP_STATUS="$(curl -sS -c "$LOGIN_COOKIE_JAR" -o "$LOGIN_BODY" -w "%{http_code}" \
    -X POST \
    -H "Content-Type: application/json" \
    -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" \
    "$BASE_URL/authentication/login")"

  if [ "$HTTP_STATUS" != "200" ]; then
    echo "Login failed for $EMAIL (HTTP $HTTP_STATUS)" >&2
    cat "$LOGIN_BODY" >&2
    echo >&2
    exit 1
  fi

  SESSION="$(awk '/dwctl_session/ {print $7; exit}' "$LOGIN_COOKIE_JAR")"
  if [ -z "$SESSION" ]; then
    echo "Login succeeded but no dwctl_session cookie was returned." >&2
    exit 1
  fi
fi

RESPONSE_BODY="$(mktemp)"
TEMP_FILES+=("$RESPONSE_BODY")
HTTP_STATUS="$(curl -sS -b "dwctl_session=$SESSION" -o "$RESPONSE_BODY" -w "%{http_code}" "$ENDPOINT")"

if [ "$HTTP_STATUS" != "200" ]; then
  echo "Request failed (HTTP $HTTP_STATUS): $ENDPOINT" >&2
  cat "$RESPONSE_BODY" >&2
  echo >&2
  exit 1
fi

if command -v jq >/dev/null 2>&1; then
  jq . "$RESPONSE_BODY"
else
  cat "$RESPONSE_BODY"
  echo
fi
