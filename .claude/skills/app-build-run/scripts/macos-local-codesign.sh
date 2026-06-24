#!/usr/bin/env bash
set -euo pipefail

FORCE=0
if [[ "${1:-}" == "--force" ]]; then
  FORCE=1
  shift
fi

APP_PATH="${1:-src-tauri/target/release/bundle/macos/prmonitor.app}"
CODESIGN_DIR="${PRMONITOR_CODESIGN_DIR:-$HOME/Library/Application Support/prmonitor/codesign}"
NAME_FILE="$CODESIGN_DIR/current-name.txt"
CERT_PATH_FILE="$CODESIGN_DIR/current-cert-path.txt"
P12_PATH_FILE="$CODESIGN_DIR/current-p12-path.txt"
P12_PASSWORD="${PRMONITOR_CODESIGN_P12_PASSWORD:-prmonitor-local}"

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

read_required_file() {
  local path="$1"
  [[ -f "$path" ]] || die "missing $path"
  local value
  value="$(tr -d '\r\n' < "$path")"
  [[ -n "$value" ]] || die "empty $path"
  printf '%s\n' "$value"
}

identity_exists() {
  local name="$1"
  security find-identity -v -p codesigning | grep -Fq "\"$name\""
}

signed_by_expected_identity() {
  local app_path="$1"
  local name="$2"
  codesign --verify --deep --strict "$app_path" >/dev/null 2>&1 || return 1
  codesign -dv --verbose=4 "$app_path" 2>&1 | grep -Fq "Authority=$name"
}

[[ -d "$APP_PATH" ]] || die "app bundle not found: $APP_PATH"

SIGNING_NAME="$(read_required_file "$NAME_FILE")"
CERT_PATH="$(read_required_file "$CERT_PATH_FILE")"
P12_PATH="$(read_required_file "$P12_PATH_FILE")"

[[ -f "$CERT_PATH" ]] || die "saved certificate not found: $CERT_PATH"
[[ -f "$P12_PATH" ]] || die "saved p12 not found: $P12_PATH"

if [[ "$FORCE" -eq 0 ]] && signed_by_expected_identity "$APP_PATH" "$SIGNING_NAME"; then
  printf 'signature already valid for %s with "%s"; skipping codesign\n' "$APP_PATH" "$SIGNING_NAME"
  exit 0
fi

if ! identity_exists "$SIGNING_NAME"; then
  printf 'codesigning identity not found, importing saved p12: %s\n' "$SIGNING_NAME"
  security import "$P12_PATH" \
    -k "$HOME/Library/Keychains/login.keychain-db" \
    -P "$P12_PASSWORD" \
    -A
fi

identity_exists "$SIGNING_NAME" || die "codesigning identity unavailable after import: $SIGNING_NAME"

if ! security verify-cert -c "$CERT_PATH" -p codeSign >/dev/null 2>&1; then
  cat >&2 <<EOF
warning: certificate is not trusted for code signing yet.
Run this once and enter the macOS admin password:

  sudo security add-trusted-cert -d -r trustRoot -p codeSign -k /Library/Keychains/System.keychain "$CERT_PATH"

EOF
fi

codesign --force --deep --options runtime --timestamp=none --sign "$SIGNING_NAME" "$APP_PATH"
codesign --verify --deep --strict --verbose=4 "$APP_PATH"

printf 'signed %s with "%s"\n' "$APP_PATH" "$SIGNING_NAME"
