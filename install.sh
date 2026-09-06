#!/usr/bin/env bash
set -euo pipefail

APP_NAME="unifi-ups-monitor"
REPOSITORY="Rahn-IT/unifi-ups-monitor"
EXECUTABLE="unifi-ups-monitor-linux-x86_64"
RELEASE_BASE="https://github.com/${REPOSITORY}/releases/latest/download"
RAW_BASE="https://raw.githubusercontent.com/${REPOSITORY}/main"
CONFIG_DIR="/etc/${APP_NAME}"
CONFIG_PATH="${CONFIG_DIR}/config.toml"
STATE_DIR="/var/lib/${APP_NAME}"
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

for command in curl install systemctl python3; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "Required command is missing: ${command}"
    exit 1
  fi
done

python3 -c 'import tomllib' || { echo 'Python 3.11 or newer is required for safe configuration updates.'; exit 1; }

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

install -d -m 0755 "${CONFIG_DIR}"
install -d -m 0755 "${STATE_DIR}"
python3 - "${CONFIG_PATH}" "${TEMP_DIR}/config.example.toml" <<'PY_CONFIG'
import os
from pathlib import Path
import tempfile
import tomllib

# Only migrate newly introduced options. Older omitted settings may be intentional.
new_options = (
    "nut_connection_loss_shutdown_seconds",
    "battery_state_path",
    "battery_service_life_days",
    "notification_queue_command",
)

def update_config(config_path, example_path):
    target = Path(config_path).resolve()
    example = Path(example_path).read_bytes()
    defaults = tomllib.loads(example.decode("utf-8"))
    exists = target.exists()
    original = target.read_bytes() if exists else b""
    settings = tomllib.loads(original.decode("utf-8"))
    if exists:
        missing = [key for key in new_options if key not in settings]
        if not missing:
            print("Existing configuration is up to date; no changes.")
            return
        lines = {}
        for line in example.decode("utf-8").splitlines():
            key = line.split("=", 1)[0].strip()
            if key in missing:
                lines[key] = line
        additions = "\n".join(lines[key] for key in missing)
        # Prepend at the root, so any existing TOML tables keep their meaning.
        updated = ("# Added by unifi-ups-monitor installer\n" + additions + "\n\n").encode() + original
        parsed = tomllib.loads(updated.decode("utf-8"))
        assert all(parsed[key] == value for key, value in settings.items())
        assert all(parsed[key] == defaults[key] for key in missing)
    else:
        updated = example
    metadata = target.stat() if exists else None
    if exists:
        fd, backup = tempfile.mkstemp(prefix=target.name + ".bak.", dir=target.parent)
        with os.fdopen(fd, "wb") as handle:
            handle.write(original)
        print(f"Configuration backup: {backup}")
    fd, temporary = tempfile.mkstemp(prefix=target.name + ".tmp.", dir=target.parent)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(updated)
            handle.flush()
            os.fsync(handle.fileno())
            if metadata is not None and hasattr(os, "fchown"):
                os.fchown(handle.fileno(), metadata.st_uid, metadata.st_gid)
        os.replace(temporary, target)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    print("Added missing settings: " + ", ".join(missing) if exists else "Created initial configuration.")

if __name__ == "__main__":
    import sys
    update_config(*sys.argv[1:])
PY_CONFIG
install -m 0755 "${TEMP_DIR}/${EXECUTABLE}" "${BIN_PATH}"
install -m 0644 "${TEMP_DIR}/${APP_NAME}.service" "${SERVICE_PATH}"

systemctl daemon-reload
systemctl enable "${APP_NAME}.service"

echo
echo "Installed ${APP_NAME}."
echo "Edit the config, then start the service with:"
echo "  nano ${CONFIG_PATH}"
echo "  systemctl restart ${APP_NAME}.service"
echo "  systemctl status ${APP_NAME}.service"
