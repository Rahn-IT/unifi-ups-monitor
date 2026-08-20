# UniFi UPS Monitor

Small Rust service that connects directly to a UniFi NUT endpoint and triggers a local shutdown when the configured battery runtime or charge threshold is reached. It does not require `upsc` or other NUT client packages.

## Why this exists

The UniFi UPS NUT server exposes status data, but it does not behave like a full read/write NUT implementation for `FSD`-driven shutdown. This service uses local policy instead:

- check `ups.status`
- require `OB` by default
- shut down when `battery.runtime` is low enough
- optionally also shut down when `battery.charge` is low enough
- send a notification to `root` via `mail` before shutdown when `/root/.forward` exists

## Files

- `Cargo.toml`: Rust package definition
- `src/main.rs`: monitor loop
- `config.example.toml`: example configuration
- `install.sh`: bootstrap installer for the latest GitHub release
- `scripts/install.sh`: Linux installation helper
- `scripts/unifi-ups-monitor.service`: `systemd` service unit

## Example config

```toml
nut_host = "192.0.2.10"
nut_port = 3493
nut_ups_name = "unifi"
nut_username = "pbs"
nut_password = "CHANGE_ME"
connection_timeout_seconds = 5
poll_interval_seconds = 15
runtime_shutdown_seconds = 600
charge_shutdown_percent = 25
shutdown_command = "/sbin/shutdown -h now"
require_on_battery = true
```

## Install on the Debian/PBS host

### Install the latest GitHub release

Run this command on the Debian/PBS host:

```bash
curl -fsSL https://raw.githubusercontent.com/Rahn-IT/unifi-ups-monitor/main/install.sh | sudo bash
```

The bootstrap script downloads the latest prebuilt static Linux executable. It
loads the example configuration and systemd unit directly from GitHub, installs
all three files, and creates the configuration only if none exists. Rust and
`upsc` are not required on the server.

Then edit and start the service:

```bash
nano /etc/unifi-ups-monitor/config.toml
systemctl restart unifi-ups-monitor
systemctl status unifi-ups-monitor
journalctl -u unifi-ups-monitor -f
```

### Install from source

1. Copy or clone this repository to the target host.
2. Adjust `config.example.toml` or place your own config at `/etc/unifi-ups-monitor/config.toml`.
3. Run:

```bash
sudo ./scripts/install.sh
```

Every push to `main` also creates a temporary executable download under the
GitHub Actions run's Artifacts section. Pushing a version tag such as `v0.2.1`
creates a permanent GitHub Release containing only the executable.

If `/root/.forward` exists, the service sends the shutdown reason to `root` using
the system `mail` command. If the file is absent, notification is silently skipped.
A mail delivery error is logged but does not prevent the shutdown.

## Test without shutting down

For a safe dry run, temporarily replace:

```toml
shutdown_command = "/bin/echo shutdown would run now"
```

Then run the binary manually:

```bash
cargo run -- /path/to/config.toml
```
