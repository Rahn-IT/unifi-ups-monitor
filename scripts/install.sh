#!/usr/bin/env bash
set -euo pipefail

APP_NAME="unifi-ups-monitor"
INSTALL_ROOT="/opt/${APP_NAME}"
CONFIG_DIR="/etc/${APP_NAME}"
BIN_PATH="/usr/local/bin/${APP_NAME}"
SERVICE_PATH="/etc/systemd/system/${APP_NAME}.service"

if [[ "${EUID}" -ne 0 ]]; then
  echo "Please run as root."
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required on the target machine."
  exit 1
fi

mkdir -p "${INSTALL_ROOT}"
mkdir -p "${CONFIG_DIR}"

echo "Building release binary..."
cargo build --release --manifest-path "${PROJECT_ROOT}/Cargo.toml"

install -m 0755 "${PROJECT_ROOT}/target/release/${APP_NAME}" "${BIN_PATH}"

if [[ ! -f "${CONFIG_DIR}/config.toml" ]]; then
  install -m 0644 "${PROJECT_ROOT}/config.example.toml" "${CONFIG_DIR}/config.toml"
else
  echo "Keeping existing config at ${CONFIG_DIR}/config.toml"
fi

install -m 0644 "${PROJECT_ROOT}/scripts/${APP_NAME}.service" "${SERVICE_PATH}"

systemctl daemon-reload
systemctl enable --now "${APP_NAME}.service"

echo
echo "Installed ${APP_NAME}."
echo "Config: ${CONFIG_DIR}/config.toml"
echo "Service: systemctl status ${APP_NAME}.service"
