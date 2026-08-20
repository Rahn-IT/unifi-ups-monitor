# UniFi UPS Monitor

Small Rust service that polls a UniFi NUT endpoint via `upsc` and triggers a local shutdown when the configured battery runtime or charge threshold is reached.

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
- `scripts/install.sh`: Linux installation helper
- `scripts/unifi-ups-monitor.service`: `systemd` service unit

## Example config

```toml
ups_name = "unifi@192.0.2.10:3493"
upsc_path = "/usr/bin/upsc"
poll_interval_seconds = 15
runtime_shutdown_seconds = 600
charge_shutdown_percent = 25
shutdown_command = "/sbin/shutdown -h now"
require_on_battery = true
```

## Install on the Debian/PBS host

1. Copy this folder to the target host.
2. Adjust `config.example.toml` or place your own config at `/etc/unifi-ups-monitor/config.toml`.
3. Run:

```bash
sudo ./scripts/install.sh
```

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
