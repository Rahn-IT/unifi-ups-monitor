#!/usr/bin/env bash
set -euo pipefail

REPOSITORY="Rahn-IT/unifi-ups-monitor"
ARCHIVE="unifi-ups-monitor-linux-x86_64.tar.gz"
DOWNLOAD_BASE="https://github.com/${REPOSITORY}/releases/latest/download"

if [[ "${EUID}" -ne 0 ]]; then
  echo "Please run this installer as root (for example: curl ... | sudo bash)."
  exit 1
fi

if [[ "$(uname -m)" != "x86_64" ]]; then
  echo "Unsupported architecture: $(uname -m). This release supports x86_64 only."
  exit 1
fi

for command in curl tar sha256sum; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "Required command is missing: ${command}"
    exit 1
  fi
done

TEMP_DIR="$(mktemp -d)"
trap 'rm -rf -- "${TEMP_DIR}"' EXIT

echo "Downloading latest unifi-ups-monitor release..."
curl --fail --show-error --location \
  "${DOWNLOAD_BASE}/${ARCHIVE}" \
  --output "${TEMP_DIR}/${ARCHIVE}"
curl --fail --show-error --location \
  "${DOWNLOAD_BASE}/${ARCHIVE}.sha256" \
  --output "${TEMP_DIR}/${ARCHIVE}.sha256"

echo "Verifying release checksum..."
(
  cd "${TEMP_DIR}"
  sha256sum --check "${ARCHIVE}.sha256"
)

mkdir "${TEMP_DIR}/package"
tar -xzf "${TEMP_DIR}/${ARCHIVE}" -C "${TEMP_DIR}/package"
bash "${TEMP_DIR}/package/scripts/install.sh"
