# UniFi UPS Monitor

Small Rust service that connects directly to a UniFi NUT endpoint and triggers a local shutdown when the configured battery runtime or charge threshold is reached. It does not require `upsc` or other NUT client packages.

## ⚠️WARNING⚠️

> [!WARNING]
> **This repository is heavily vibe coded.**
>
> It's a very simple app and we use it ourselves, but I still feel like it should be openly disclosed in my opinion
> as it will always influence code quality.
>
> Feel free to check out the code if you're unsure

## Why this exists

The UniFi UPS NUT server exposes status data, but it does not behave like a full read/write NUT implementation for `FSD`-driven shutdown. This service uses local policy instead:

- check `ups.status`
- require `OB` by default for battery threshold shutdowns
- shut down when `battery.runtime` is low enough
- optionally also shut down when `battery.charge` is low enough
- send a notification to `root` via `mail` before shutdown when `/root/.forward` exists
- shut down after sustained NUT communication loss (300 seconds by default)
- persist the battery installation date and send a daily replacement reminder after its service life
- send one immediate, clearly marked replacement alert when NUT reports `RB`

## Files

- `Cargo.toml`: Rust package definition
- `src/main.rs`: monitor loop
- `config.example.toml`: example configuration
- `install.sh`: bootstrap installer for the latest GitHub release
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
nut_connection_loss_shutdown_seconds = 300
runtime_shutdown_seconds = 600
charge_shutdown_percent = 25
shutdown_command = "/sbin/shutdown -h now"
require_on_battery = true
notification_queue_command = "/usr/sbin/sendmail -q"
notification_wait_seconds = 15
battery_state_path = "/var/lib/unifi-ups-monitor/battery-state.toml"
battery_service_life_days = 1095
```

## NUT communication loss

`nut_connection_loss_shutdown_seconds` sets the safety shutdown delay after NUT
communication fails. It defaults to `300` seconds, including for existing configs
that omit this setting; set it to `0` to disable this safety shutdown.
The monotonic timer starts at the first failed connection, authentication, or
status query, even if no successful query has occurred since service startup.
Repeated failures and successful reconnections do not restart or clear it: only
a complete, successfully parsed status query resets it. A later outage starts a
new timer. Restarting the service also clears the timer.

The timeout is checked after each connection/status query attempt, so polling
and network timeouts can delay detection beyond the configured duration.
This shutdown applies regardless of `require_on_battery` or the last battery
readings. It uses the same mail notification, delivery wait, and
`shutdown_command` as a battery threshold shutdown.

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
3. Build and install the binary and systemd unit using your deployment process.

```bash
cargo build --release --locked
sudo install -m 0755 target/release/unifi-ups-monitor /usr/local/bin/unifi-ups-monitor
sudo install -m 0644 scripts/unifi-ups-monitor.service /etc/systemd/system/unifi-ups-monitor.service
sudo install -d -m 0755 /etc/unifi-ups-monitor
if ! sudo test -f /etc/unifi-ups-monitor/config.toml; then
  sudo install -m 0600 config.example.toml /etc/unifi-ups-monitor/config.toml
fi
sudo systemctl daemon-reload
sudo systemctl enable --now unifi-ups-monitor
```

Every push to `main` also creates a temporary executable download under the
GitHub Actions run's Artifacts section. Pushing a version tag such as `v0.2.1`
creates a permanent GitHub Release containing only the executable.

If `/root/.forward` exists, the service sends its shutdown and battery alerts to
`root` using the system `mail` command. Before shutdown, it also triggers the
mail queue and waits for the configured delivery window. The other alerts flush
the queue without the configured shutdown delivery wait. If the file is absent, notification is
silently skipped. A mail delivery error is logged but does not prevent shutdown.

## Battery replacement reminders

On the first start, the monitor stores the current date in
`battery_state_path` as the battery installation date. The default service life
is 1,095 days (three years), configurable with `battery_service_life_days`.
Once that date is reached, it sends at most one replacement reminder per
UTC calendar day until the lifecycle is reset, following a successful NUT query.

NUT status `RB` means the UPS itself requests a battery replacement. This sends
an immediate `Sofortalarm Batterietausch (RB)` email independently of the
three-year reminder. It is sent once while `RB` remains present; after NUT no
longer reports `RB`, a later new `RB` condition produces a new alert.

After replacing the battery, reset the stored lifecycle date on the host:

```bash
sudo systemctl stop unifi-ups-monitor
sudo unifi-ups-monitor battery-reset /etc/unifi-ups-monitor/config.toml
sudo systemctl start unifi-ups-monitor
```

The reset records today's date, clears the daily-reminder marker and clears any
active `RB` alert state. Stop the service first so its in-memory state cannot overwrite the reset.

## Test without shutting down

For a safe dry run, temporarily replace:

```toml
shutdown_command = "/bin/echo shutdown would run now"
```

Then run the binary manually:

```bash
cargo run -- /path/to/config.toml
```

## Automated checks

Run cargo fmt --check, cargo test --offline --locked, cargo build --offline --locked,
and cargo clippy --offline --locked --all-targets -- -D warnings.
The tests use simulated time, a local NUT test server, captured notifications,
and temporary battery-state files; they do not send mail or shut down the host.
