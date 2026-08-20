#!/usr/bin/env bash
set -euo pipefail

APP_NAME="unifi-ups-monitor"
REPOSITORY="Rahn-IT/unifi-ups-monitor"
EXECUTABLE="unifi-ups-monitor-linux-x86_64"
RELEASE_BASE="https://github.com/${REPOSITORY}/releases/latest/download"
RAW_BASE="https://raw.githubusercontent.com/${REPOSITORY}/main"
CONFIG_DIR="/etc/${APP_NAME}"
CONFIG_PATH="${CONFIG_DIR}/config.toml"
BIN_PATH="/usr/local/bin/${APP_NAME}"
SERVICE_PATH="/etc/systemd/system/${APP_NAME}.service"

if [[ "${EUID}" -ne 0 ]]; then
  echo "Please run this installer as root (for example: curl ... | sudo bash)."
  exit 1
fi

if [[ "$(uname -m)" != "x86_64" ]]; then
  echo "Unsupported architecture: $(uname -m). This release supports x86_64 only."
  exit 1
fi

for command in curl install systemctl; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "Required command is missing: ${command}"
    exit 1
  fi
done

TEMP_DIR="$(mktemp -d)"
trap 'rm -rf -- "${TEMP_DIR}"' EXIT

echo "Downloading latest unifi-ups-monitor executable..."
curl --fail --show-error --location \
  "${RELEASE_BASE}/${EXECUTABLE}" \
  --output "${TEMP_DIR}/${EXECUTABLE}"
curl --fail --show-error --location \
  "${RAW_BASE}/config.example.toml" \
  --output "${TEMP_DIR}/config.example.toml"
curl --fail --show-error --location \
  "${RAW_BASE}/scripts/${APP_NAME}.service" \
  --output "${TEMP_DIR}/${APP_NAME}.service"

install -m 0755 "${TEMP_DIR}/${EXECUTABLE}" "${BIN_PATH}"
install -d -m 0755 "${CONFIG_DIR}"
if [[ ! -f "${CONFIG_PATH}" ]]; then
  install -m 0600 "${TEMP_DIR}/config.example.toml" "${CONFIG_PATH}"
else
  echo "Keeping existing config at ${CONFIG_PATH}"
fi
install -m 0644 "${TEMP_DIR}/${APP_NAME}.service" "${SERVICE_PATH}"

systemctl daemon-reload
systemctl enable "${APP_NAME}.service"

echo
echo "Installed ${APP_NAME}."
echo "Edit the config, then start the service with:"
echo "  nano ${CONFIG_PATH}"
echo "  systemctl restart ${APP_NAME}.service"
echo "  systemctl status ${APP_NAME}.service"
