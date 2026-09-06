use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_CONFIG_PATH: &str = "/etc/unifi-ups-monitor/config.toml";
const DEFAULT_BATTERY_STATE_PATH: &str = "/var/lib/unifi-ups-monitor/battery-state.toml";
const DEFAULT_BATTERY_SERVICE_LIFE_DAYS: u64 = 365 * 3;

#[derive(Debug, Deserialize)]
struct Config {
    nut_host: String,
    nut_port: Option<u16>,
    nut_ups_name: String,
    nut_username: Option<String>,
    nut_password: Option<String>,
    connection_timeout_seconds: Option<u64>,
    poll_interval_seconds: Option<u64>,
    nut_connection_loss_shutdown_seconds: Option<u64>,
    runtime_shutdown_seconds: Option<u64>,
    charge_shutdown_percent: Option<f64>,
    shutdown_command: Option<String>,
    require_on_battery: Option<bool>,
    notification_queue_command: Option<String>,
    notification_wait_seconds: Option<u64>,
    battery_state_path: Option<String>,
    battery_service_life_days: Option<u64>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct BatteryState {
    installed_on: Option<String>,
    last_age_reminder_on: Option<String>,
    rb_alert_active: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CalendarDate {
    year: i32,
    month: u32,
    day: u32,
}

impl std::fmt::Display for CalendarDate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:04}-{:02}-{:02}",
            self.year, self.month, self.day
        )
    }
}

#[derive(Debug)]
struct UpsSnapshot {
    status: HashSet<String>,
    battery_runtime: Option<u64>,
    battery_charge: Option<f64>,
}

struct NutClient {
    stream: BufReader<TcpStream>,
}

#[derive(Default)]
struct ConnectionLossTimer {
    first_failure: Option<Instant>,
}

impl ConnectionLossTimer {
    fn observe(
        &mut self,
        config: &Config,
        result: &Result<UpsSnapshot, String>,
        now: Instant,
    ) -> Option<String> {
        if result.is_ok() {
            self.first_failure = None;
            return None;
        }
        let first_failure = *self.first_failure.get_or_insert(now);
        let limit = config.nut_connection_loss_shutdown_seconds.unwrap_or(300);
        let elapsed = now.duration_since(first_failure);
        if limit > 0 && elapsed >= Duration::from_secs(limit) {
            Some(format!(
                "NUT communication lost for {}s >= {limit}s; no successful status query",
                elapsed.as_secs()
            ))
        } else {
            None
        }
    }
}

impl NutClient {
    fn connect(config: &Config) -> Result<Self, String> {
        validate_token("nut_ups_name", &config.nut_ups_name)?;
        let username = config.nut_username.as_deref().unwrap_or("");
        validate_token("nut_username", username)?;
        validate_line("nut_password", config.nut_password.as_deref().unwrap_or(""))?;

        let port = config.nut_port.unwrap_or(3493);
        let timeout = Duration::from_secs(config.connection_timeout_seconds.unwrap_or(5));
        let addresses = (config.nut_host.as_str(), port)
            .to_socket_addrs()
            .map_err(|error| format!("unable to resolve {}:{port}: {error}", config.nut_host))?;

        let mut last_error = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, timeout) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(timeout))
                        .map_err(|error| format!("unable to set NUT read timeout: {error}"))?;
                    stream
                        .set_write_timeout(Some(timeout))
                        .map_err(|error| format!("unable to set NUT write timeout: {error}"))?;
                    let mut client = Self {
                        stream: BufReader::new(stream),
                    };
                    if !username.is_empty() {
                        client
                            .authenticate(username, config.nut_password.as_deref().unwrap_or(""))?;
                    }
                    return Ok(client);
                }
                Err(error) => last_error = Some(error),
            }
        }

        Err(format!(
            "unable to connect to {}:{port}: {}",
            config.nut_host,
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "host resolved to no addresses".to_string())
        ))
    }

    fn authenticate(&mut self, username: &str, password: &str) -> Result<(), String> {
        self.send_command(&format!("USERNAME {username}"))?;
        self.expect_ok()?;
        if !password.is_empty() {
            self.send_command(&format!("PASSWORD {password}"))?;
            self.expect_ok()?;
        }
        Ok(())
    }

    fn read_snapshot(&mut self, ups_name: &str) -> Result<UpsSnapshot, String> {
        self.send_command(&format!("LIST VAR {ups_name}"))?;
        let begin = format!("BEGIN LIST VAR {ups_name}");
        let end = format!("END LIST VAR {ups_name}");
        let first = self.read_line()?;
        if first != begin {
            return Err(format!("unexpected NUT response: {first}"));
        }

        let mut values = HashMap::new();
        loop {
            let line = self.read_line()?;
            if line == end {
                break;
            }
            if line.starts_with("ERR ") {
                return Err(format!("NUT server returned: {line}"));
            }
            if let Some((name, value)) = parse_var_line(&line, ups_name)? {
                values.insert(name, value);
            }
        }
        parse_snapshot(&values)
    }

    fn send_command(&mut self, command: &str) -> Result<(), String> {
        let stream = self.stream.get_mut();
        stream
            .write_all(command.as_bytes())
            .and_then(|_| stream.write_all(b"\n"))
            .and_then(|_| stream.flush())
            .map_err(|error| format!("unable to write to NUT server: {error}"))
    }

    fn read_line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        let bytes = self
            .stream
            .read_line(&mut line)
            .map_err(|error| format!("unable to read from NUT server: {error}"))?;
        if bytes == 0 {
            return Err("NUT server closed the connection".to_string());
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
    }

    fn expect_ok(&mut self) -> Result<(), String> {
        let line = self.read_line()?;
        if line == "OK" || line.starts_with("OK ") {
            Ok(())
        } else {
            Err(format!("NUT authentication failed: {line}"))
        }
    }
}

fn main() {
    if let Err(error) = dispatch() {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    }
}

fn dispatch() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let command_or_config = args.next();
    if command_or_config.as_deref() == Some("battery-reset") {
        let config_path = args
            .next()
            .or_else(|| env::var("UNIFI_UPS_MONITOR_CONFIG").ok())
            .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());
        return reset_battery_lifecycle(&config_path);
    }

    let config_path = command_or_config
        .or_else(|| env::var("UNIFI_UPS_MONITOR_CONFIG").ok())
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());
    run(&config_path)
}

fn run(config_path: &str) -> Result<(), String> {
    let config = load_config(config_path)?;
    let state_path = battery_state_path(&config);
    let mut battery_state = load_battery_state(&state_path)?;
    ensure_battery_install_date(&mut battery_state, today());
    save_battery_state(&state_path, &battery_state)?;

    println!(
        "starting unifi-ups-monitor for '{}' at {}:{} using config {}",
        config.nut_ups_name,
        config.nut_host,
        config.nut_port.unwrap_or(3493),
        config_path
    );

    let mut client = None;
    let mut connection_loss = ConnectionLossTimer::default();
    loop {
        let result = (|| {
            if client.is_none() {
                client = Some(NutClient::connect(&config)?);
                println!("connected to NUT server");
            }
            client.as_mut().unwrap().read_snapshot(&config.nut_ups_name)
        })();
        let mut reason = connection_loss.observe(&config, &result, Instant::now());
        match result {
            Ok(snapshot) => {
                println!(
                    "status={:?} runtime={:?} charge={:?}",
                    snapshot.status, snapshot.battery_runtime, snapshot.battery_charge
                );
                if let Err(error) = handle_battery_notifications(
                    &config,
                    &snapshot,
                    &state_path,
                    &mut battery_state,
                ) {
                    eprintln!("warning: failed to handle battery notification: {error}");
                }
                if should_shutdown(&config, &snapshot) {
                    reason = Some(shutdown_reason(&config, &snapshot));
                }
            }
            Err(error) => {
                eprintln!("warning: NUT communication failed, reconnecting: {error}");
                client = None;
            }
        }
        if let Some(reason) = reason {
            println!("shutdown condition reached: {reason}");
            if let Err(error) = notify_shutdown(&config, &reason) {
                eprintln!("warning: failed to send shutdown notification: {error}");
            }
            run_shutdown(&config)?;
            return Ok(());
        }

        thread::sleep(Duration::from_secs(
            config.poll_interval_seconds.unwrap_or(15),
        ));
    }
}

fn load_config(path: &str) -> Result<Config, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("unable to read config {path}: {error}"))?;
    toml::from_str(&content).map_err(|error| format!("invalid config {path}: {error}"))
}

fn reset_battery_lifecycle(config_path: &str) -> Result<(), String> {
    let config = load_config(config_path)?;
    let state_path = battery_state_path(&config);
    let installed_on = today().to_string();
    let state = BatteryState {
        installed_on: Some(installed_on.clone()),
        last_age_reminder_on: None,
        rb_alert_active: false,
    };
    save_battery_state(&state_path, &state)?;
    println!("battery lifecycle reset: installed_on={installed_on}");
    Ok(())
}

fn battery_state_path(config: &Config) -> String {
    config
        .battery_state_path
        .clone()
        .unwrap_or_else(|| DEFAULT_BATTERY_STATE_PATH.to_string())
}

fn load_battery_state(path: &str) -> Result<BatteryState, String> {
    match fs::read_to_string(path) {
        Ok(content) => toml::from_str(&content)
            .map_err(|error| format!("invalid battery state {path}: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BatteryState::default()),
        Err(error) => Err(format!("unable to read battery state {path}: {error}")),
    }
}

fn save_battery_state(path: &str, state: &BatteryState) -> Result<(), String> {
    let state_path = Path::new(path);
    if let Some(parent) = state_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("unable to create battery state directory: {error}"))?;
    }
    let content = toml::to_string_pretty(state)
        .map_err(|error| format!("unable to serialize battery state: {error}"))?;
    fs::write(state_path, content)
        .map_err(|error| format!("unable to write battery state {path}: {error}"))
}

fn today() -> CalendarDate {
    let days_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400;
    civil_from_days(days_since_epoch as i64)
}

fn ensure_battery_install_date(state: &mut BatteryState, current_day: CalendarDate) -> bool {
    if state.installed_on.is_none() {
        state.installed_on = Some(current_day.to_string());
        true
    } else {
        false
    }
}

fn handle_battery_notifications(
    config: &Config,
    snapshot: &UpsSnapshot,
    state_path: &str,
    state: &mut BatteryState,
) -> Result<(), String> {
    if update_battery_notifications(config, snapshot, state, today(), send_notification)? {
        save_battery_state(state_path, state)?;
    }
    Ok(())
}

fn update_battery_notifications(
    config: &Config,
    snapshot: &UpsSnapshot,
    state: &mut BatteryState,
    current_day: CalendarDate,
    mut notify: impl FnMut(&Config, &str, &str) -> Result<(), String>,
) -> Result<bool, String> {
    let current_day_string = current_day.to_string();
    let mut changed = ensure_battery_install_date(state, current_day);

    if battery_replacement_due(state, current_day, battery_service_life_days(config))?
        && state.last_age_reminder_on.as_deref() != Some(current_day_string.as_str())
    {
        println!("battery replacement interval has elapsed");
        if let Err(error) = notify(
            config,
            "UniFi UPS: Batterietausch faellig",
            &format!(
                "Die konfigurierte Batterielebensdauer von {} Tagen ist abgelaufen.\n\nInstallationsdatum: {}\nBitte die USV-Batterie zeitnah pruefen und bei Austausch anschliessend 'unifi-ups-monitor battery-reset /etc/unifi-ups-monitor/config.toml' ausfuehren.\n",
                battery_service_life_days(config),
                state.installed_on.as_deref().unwrap_or("unbekannt")
            ),
        ) {
            eprintln!("warning: failed to send battery age reminder: {error}");
        }
        state.last_age_reminder_on = Some(current_day_string);
        changed = true;
    }

    let rb_active = snapshot.status.contains("RB");
    if should_send_rb_alert(state, rb_active) {
        println!("NUT reports RB: battery replacement is needed");
        if let Err(error) = notify(
            config,
            "UniFi UPS: Sofortalarm Batterietausch (RB)",
            "Die USV meldet ueber NUT den Status RB (battery needs replacement).\n\nBitte die Batterie umgehend pruefen und nach einem Austausch den Batterielebenszyklus zuruecksetzen.\n",
        ) {
            eprintln!("warning: failed to send RB battery replacement alert: {error}");
        }
        state.rb_alert_active = true;
        changed = true;
    } else if !rb_active && state.rb_alert_active {
        state.rb_alert_active = false;
        changed = true;
    }

    Ok(changed)
}

fn should_send_rb_alert(state: &BatteryState, rb_active: bool) -> bool {
    rb_active && !state.rb_alert_active
}

fn battery_service_life_days(config: &Config) -> u64 {
    config
        .battery_service_life_days
        .unwrap_or(DEFAULT_BATTERY_SERVICE_LIFE_DAYS)
}

fn battery_replacement_due(
    state: &BatteryState,
    current_day: CalendarDate,
    service_life_days: u64,
) -> Result<bool, String> {
    let installed_on = state
        .installed_on
        .as_deref()
        .ok_or_else(|| "battery installation date is missing".to_string())?;
    let installed_on = parse_calendar_date(installed_on)?;
    let service_life_days = i64::try_from(service_life_days)
        .map_err(|_| "battery_service_life_days is too large".to_string())?;
    let replacement_due_on = days_from_civil(installed_on)
        .checked_add(service_life_days)
        .ok_or_else(|| "battery_service_life_days is too large".to_string())?;
    Ok(days_from_civil(current_day) >= replacement_due_on)
}

fn parse_calendar_date(value: &str) -> Result<CalendarDate, String> {
    let mut parts = value.split('-');
    let year = parts
        .next()
        .ok_or_else(|| format!("invalid battery installation date '{value}'"))?
        .parse::<i32>()
        .map_err(|error| format!("invalid battery installation date '{value}': {error}"))?;
    let month = parts
        .next()
        .ok_or_else(|| format!("invalid battery installation date '{value}'"))?
        .parse::<u32>()
        .map_err(|error| format!("invalid battery installation date '{value}': {error}"))?;
    let day = parts
        .next()
        .ok_or_else(|| format!("invalid battery installation date '{value}'"))?
        .parse::<u32>()
        .map_err(|error| format!("invalid battery installation date '{value}': {error}"))?;
    if parts.next().is_some() {
        return Err(format!("invalid battery installation date '{value}'"));
    }

    let date = CalendarDate { year, month, day };
    if civil_from_days(days_from_civil(date)) != date {
        return Err(format!("invalid battery installation date '{value}'"));
    }
    Ok(date)
}

// Gregorian calendar conversion, using Unix day zero (1970-01-01).
fn days_from_civil(date: CalendarDate) -> i64 {
    let adjusted_year = i64::from(date.year) - if date.month <= 2 { 1 } else { 0 };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month = i64::from(date.month);
    let day_of_year =
        (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(date.day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days_since_epoch: i64) -> CalendarDate {
    let day_zero = days_since_epoch + 719_468;
    let era = if day_zero >= 0 {
        day_zero
    } else {
        day_zero - 146_096
    } / 146_097;
    let day_of_era = day_zero - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = month_index + if month_index < 10 { 3 } else { -9 };
    CalendarDate {
        year: (year + if month <= 2 { 1 } else { 0 }) as i32,
        month: month as u32,
        day: day as u32,
    }
}

fn parse_var_line(line: &str, ups_name: &str) -> Result<Option<(String, String)>, String> {
    if !line.starts_with("VAR ") {
        return Ok(None);
    }
    let mut parts = line.splitn(4, ' ');
    let _var = parts.next();
    let response_ups = parts
        .next()
        .ok_or_else(|| format!("invalid NUT variable response: {line}"))?;
    let name = parts
        .next()
        .ok_or_else(|| format!("invalid NUT variable response: {line}"))?;
    let raw_value = parts
        .next()
        .ok_or_else(|| format!("invalid NUT variable response: {line}"))?;
    if response_ups != ups_name {
        return Ok(None);
    }
    Ok(Some((name.to_string(), unquote_nut(raw_value)?)))
}

fn unquote_nut(value: &str) -> Result<String, String> {
    if !value.starts_with('"') || !value.ends_with('"') || value.len() < 2 {
        return Ok(value.to_string());
    }

    let mut result = String::new();
    let mut escaped = false;
    for character in value[1..value.len() - 1].chars() {
        if escaped {
            result.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            result.push(character);
        }
    }
    if escaped {
        return Err(format!("invalid trailing escape in NUT value: {value}"));
    }
    Ok(result)
}

fn parse_snapshot(values: &HashMap<String, String>) -> Result<UpsSnapshot, String> {
    let status_raw = values
        .get("ups.status")
        .ok_or_else(|| "missing ups.status in NUT response".to_string())?;
    let status = status_raw
        .split_whitespace()
        .map(str::to_string)
        .collect::<HashSet<_>>();
    Ok(UpsSnapshot {
        status,
        battery_runtime: parse_u64(values.get("battery.runtime")),
        battery_charge: parse_f64(values.get("battery.charge")),
    })
}

fn parse_u64(value: Option<&String>) -> Option<u64> {
    value.and_then(|raw| raw.parse::<u64>().ok())
}

fn parse_f64(value: Option<&String>) -> Option<f64> {
    value.and_then(|raw| raw.parse::<f64>().ok())
}

fn validate_token(name: &str, value: &str) -> Result<(), String> {
    validate_line(name, value)?;
    if value.chars().any(char::is_whitespace) {
        return Err(format!("{name} must not contain whitespace"));
    }
    Ok(())
}

fn validate_line(name: &str, value: &str) -> Result<(), String> {
    if value.contains(['\r', '\n']) {
        return Err(format!("{name} must not contain line breaks"));
    }
    Ok(())
}

fn should_shutdown(config: &Config, snapshot: &UpsSnapshot) -> bool {
    if config.require_on_battery.unwrap_or(true) && !snapshot.status.contains("OB") {
        return false;
    }
    if let (Some(limit), Some(runtime)) =
        (config.runtime_shutdown_seconds, snapshot.battery_runtime)
    {
        if runtime <= limit {
            return true;
        }
    }
    if let (Some(limit), Some(charge)) = (config.charge_shutdown_percent, snapshot.battery_charge) {
        if charge <= limit {
            return true;
        }
    }
    false
}

fn shutdown_reason(config: &Config, snapshot: &UpsSnapshot) -> String {
    if let (Some(limit), Some(runtime)) =
        (config.runtime_shutdown_seconds, snapshot.battery_runtime)
    {
        if runtime <= limit {
            return format!("battery.runtime={runtime}s <= {limit}s");
        }
    }
    if let (Some(limit), Some(charge)) = (config.charge_shutdown_percent, snapshot.battery_charge) {
        if charge <= limit {
            return format!("battery.charge={charge}% <= {limit}%");
        }
    }
    "configured threshold matched".to_string()
}

fn notify_shutdown(config: &Config, reason: &str) -> Result<(), String> {
    if !Path::new("/root/.forward").exists() {
        return Ok(());
    }
    send_notification(
        config,
        "UniFi UPS: Server wird heruntergefahren",
        &format!(
            "Der Server wird durch den UniFi UPS Monitor heruntergefahren.\n\nGrund: {reason}\n"
        ),
    )?;

    let wait_seconds = config.notification_wait_seconds.unwrap_or(15);
    if wait_seconds > 0 {
        println!("waiting {wait_seconds}s for mail delivery before shutdown");
        thread::sleep(Duration::from_secs(wait_seconds));
    }
    Ok(())
}

fn send_notification(config: &Config, subject: &str, body: &str) -> Result<(), String> {
    if !Path::new("/root/.forward").exists() {
        return Ok(());
    }
    let mut child = Command::new("mail")
        .args(["-s", subject, "root"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start mail: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "mail stdin is unavailable".to_string())?;
    stdin
        .write_all(body.as_bytes())
        .map_err(|error| format!("failed to write mail body: {error}"))?;
    drop(stdin);
    let status = child
        .wait()
        .map_err(|error| format!("failed to wait for mail: {error}"))?;
    if !status.success() {
        return Err(format!("mail exited with {status}"));
    }

    let queue_command = config
        .notification_queue_command
        .as_deref()
        .unwrap_or("/usr/sbin/sendmail -q");
    if let Err(error) = run_command(queue_command) {
        eprintln!("warning: failed to flush mail queue: {error}");
    }

    Ok(())
}

fn run_command(command: &str) -> Result<(), String> {
    let parts = command.split_whitespace().collect::<Vec<_>>();
    let (program, args) = parts
        .split_first()
        .ok_or_else(|| "command is empty".to_string())?;
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("failed to execute '{command}': {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command '{command}' exited with {status}"))
    }
}

fn run_shutdown(config: &Config) -> Result<(), String> {
    let command = config
        .shutdown_command
        .as_deref()
        .unwrap_or("/sbin/shutdown -h now");
    run_command(command).map_err(|error| format!("shutdown failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nut_variable_line() {
        let parsed = parse_var_line(r#"VAR unifi ups.model "Tower \\"Pro\\"""#, "unifi")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.0, "ups.model");
        assert_eq!(parsed.1, r#"Tower \"Pro\""#);
    }

    #[test]
    fn parses_snapshot_values() {
        let values = HashMap::from([
            ("ups.status".to_string(), "OB DISCHRG".to_string()),
            ("battery.runtime".to_string(), "682".to_string()),
            ("battery.charge".to_string(), "48".to_string()),
        ]);
        let snapshot = parse_snapshot(&values).unwrap();
        assert!(snapshot.status.contains("OB"));
        assert_eq!(snapshot.battery_runtime, Some(682));
        assert_eq!(snapshot.battery_charge, Some(48.0));
    }

    #[test]
    fn battery_replacement_is_due_on_the_configured_date() {
        let state = BatteryState {
            installed_on: Some("2023-01-01".to_string()),
            ..BatteryState::default()
        };
        assert!(!battery_replacement_due(
            &state,
            CalendarDate {
                year: 2025,
                month: 12,
                day: 30,
            },
            1095
        )
        .unwrap());
        assert!(battery_replacement_due(
            &state,
            CalendarDate {
                year: 2025,
                month: 12,
                day: 31,
            },
            1095
        )
        .unwrap());
    }

    #[test]
    fn rb_alert_is_sent_only_when_the_condition_becomes_active() {
        let mut state = BatteryState::default();
        assert!(should_send_rb_alert(&state, true));
        state.rb_alert_active = true;
        assert!(!should_send_rb_alert(&state, true));
        state.rb_alert_active = false;
        assert!(!should_send_rb_alert(&state, false));
    }

    #[test]
    fn parses_only_valid_calendar_dates() {
        assert_eq!(
            parse_calendar_date("2024-02-29").unwrap(),
            CalendarDate {
                year: 2024,
                month: 2,
                day: 29
            }
        );
        assert!(parse_calendar_date("2023-02-29").is_err());
    }
    fn test_config() -> Config {
        toml::from_str("nut_host = '127.0.0.1'\nnut_ups_name = 'unifi'").unwrap()
    }

    #[test]
    fn connection_loss_defaults_to_300_seconds_from_first_failure() {
        let config = test_config();
        let mut timer = ConnectionLossTimer::default();
        let start = Instant::now();
        let failed = Err("connection refused".to_string());
        assert!(timer.observe(&config, &failed, start).is_none());
        assert!(timer
            .observe(&config, &failed, start + Duration::from_secs(299))
            .is_none());
        assert!(timer
            .observe(&config, &failed, start + Duration::from_secs(300))
            .unwrap()
            .contains("300s >= 300s"));
    }

    #[test]
    fn connection_loss_timeout_is_configurable_and_zero_disables_it() {
        for limit in [0, 12] {
            let mut config = test_config();
            config.nut_connection_loss_shutdown_seconds = Some(limit);
            let mut timer = ConnectionLossTimer::default();
            let start = Instant::now();
            let failed = Err("authentication failed".to_string());
            assert!(timer.observe(&config, &failed, start).is_none());
            assert!(timer
                .observe(&config, &failed, start + Duration::from_secs(11))
                .is_none());
            assert_eq!(
                timer
                    .observe(&config, &failed, start + Duration::from_secs(12))
                    .is_some(),
                limit > 0
            );
            assert_eq!(
                timer
                    .observe(&config, &failed, start + Duration::from_secs(86400))
                    .is_some(),
                limit > 0
            );
        }
    }

    #[test]
    fn successful_snapshot_clears_timer_and_next_outage_starts_fresh() {
        let config = test_config();
        let mut timer = ConnectionLossTimer::default();
        let start = Instant::now();
        let failed = Err("read failed".to_string());
        let success = parse_snapshot(&HashMap::from([("ups.status".into(), "OL".into())]));
        assert!(timer.observe(&config, &success, start).is_none());
        assert!(timer.first_failure.is_none());
        assert!(timer
            .observe(&config, &failed, start + Duration::from_secs(1000))
            .is_none());
        // Even recovery at the deadline must clear the outage.
        assert!(timer
            .observe(&config, &success, start + Duration::from_secs(1300))
            .is_none());
        assert!(timer.first_failure.is_none());
        assert!(timer
            .observe(&config, &failed, start + Duration::from_secs(1400))
            .is_none());
        assert!(timer
            .observe(&config, &failed, start + Duration::from_secs(1699))
            .is_none());
        assert!(timer
            .observe(&config, &failed, start + Duration::from_secs(1700))
            .is_some());
    }

    #[test]
    fn reconnect_and_incomplete_or_invalid_queries_do_not_clear_timer() {
        use std::net::TcpListener;

        let mut config = test_config();
        config.connection_timeout_seconds = Some(2);
        let mut timer = ConnectionLossTimer::default();
        let start = Instant::now();
        assert!(timer
            .observe(&config, &Err("connection failed".into()), start)
            .is_none());
        for (seconds, response, successful) in [
            (
                100,
                "BEGIN LIST VAR unifi\nVAR unifi ups.status \"OL\"\n",
                false,
            ),
            (200, "BEGIN LIST VAR unifi\nEND LIST VAR unifi\n", false),
            (300, "ERR DATA-STALE\n", false),
            (
                301,
                "BEGIN LIST VAR unifi\nVAR unifi ups.status \"OL\"\nEND LIST VAR unifi\n",
                true,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            config.nut_port = Some(listener.local_addr().unwrap().port());
            let server = thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert_eq!(request, "LIST VAR unifi\n");
                reader.get_mut().write_all(response.as_bytes()).unwrap();
            });
            let mut client = NutClient::connect(&config).unwrap();
            let result = client.read_snapshot(&config.nut_ups_name);
            server.join().unwrap();
            assert_eq!(result.is_ok(), successful);
            let reason = timer.observe(&config, &result, start + Duration::from_secs(seconds));
            assert_eq!(reason.is_some(), seconds == 300);
            assert_eq!(
                timer.first_failure,
                if successful { None } else { Some(start) }
            );
        }
    }

    #[test]
    fn battery_shutdown_policy_is_unchanged() {
        let mut config = test_config();
        config.runtime_shutdown_seconds = Some(600);
        config.charge_shutdown_percent = Some(25.0);
        for (status, runtime, charge, expected) in [
            ("OL", 100, 10.0, false),
            ("OB", 601, 26.0, false),
            ("OB", 600, 26.0, true),
            ("OB", 601, 25.0, true),
        ] {
            let snapshot = UpsSnapshot {
                status: HashSet::from([status.into()]),
                battery_runtime: Some(runtime),
                battery_charge: Some(charge),
            };
            assert_eq!(should_shutdown(&config, &snapshot), expected);
        }
    }

    #[test]
    fn example_config_enables_connection_loss_shutdown() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        assert_eq!(config.nut_connection_loss_shutdown_seconds, Some(300));
        assert!(toml::from_str::<Config>("nut_host = 'localhost'\nnut_ups_name = 'unifi'\nnut_connection_loss_shutdown_seconds = -1").is_err());
    }

    #[test]
    fn battery_notifications_survive_outage_and_restart_without_duplicates() {
        let config = test_config();
        let mut state = BatteryState {
            installed_on: Some("2020-01-01".into()),
            ..BatteryState::default()
        };
        let day = parse_calendar_date("2026-09-06").unwrap();
        let rb = parse_snapshot(&HashMap::from([("ups.status".into(), "OL RB".into())]));
        let online = parse_snapshot(&HashMap::from([("ups.status".into(), "OL".into())]));
        let mut messages = Vec::new();
        let mut capture = |_: &Config, subject: &str, body: &str| {
            assert!(body.contains('\n'));
            assert!(!body.contains("\\n"));
            messages.push(subject.to_string());
            Ok(())
        };
        assert!(update_battery_notifications(
            &config,
            rb.as_ref().unwrap(),
            &mut state,
            day,
            &mut capture
        )
        .unwrap());
        // Restart preserves both suppression markers.
        state = toml::from_str(&toml::to_string(&state).unwrap()).unwrap();
        assert!(!update_battery_notifications(
            &config,
            rb.as_ref().unwrap(),
            &mut state,
            day,
            &mut capture
        )
        .unwrap());
        let mut timer = ConnectionLossTimer::default();
        let now = Instant::now();
        let failed = Err("NUT disconnected".into());
        assert!(timer.observe(&config, &failed, now).is_none());
        assert!(timer
            .observe(&config, &failed, now + Duration::from_secs(300))
            .is_some());
        // Recovery clears only the connection timer, not the active RB marker.
        assert!(timer
            .observe(&config, &rb, now + Duration::from_secs(301))
            .is_none());
        assert!(!update_battery_notifications(
            &config,
            rb.as_ref().unwrap(),
            &mut state,
            day,
            &mut capture
        )
        .unwrap());
        let tomorrow = parse_calendar_date("2026-09-07").unwrap();
        assert!(update_battery_notifications(
            &config,
            rb.as_ref().unwrap(),
            &mut state,
            tomorrow,
            &mut capture
        )
        .unwrap());
        assert!(update_battery_notifications(
            &config,
            online.as_ref().unwrap(),
            &mut state,
            tomorrow,
            &mut capture
        )
        .unwrap());
        assert!(update_battery_notifications(
            &config,
            rb.as_ref().unwrap(),
            &mut state,
            tomorrow,
            &mut capture
        )
        .unwrap());
        assert_eq!(
            messages
                .iter()
                .filter(|s| s.contains("Batterietausch faellig"))
                .count(),
            2
        );
        assert_eq!(
            messages
                .iter()
                .filter(|s| s.contains("Sofortalarm"))
                .count(),
            2
        );
    }

    #[test]
    fn battery_reset_persists_new_date_and_clears_alert_markers() {
        let dir = env::temp_dir().join(format!(
            "unifi-ups-reset-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("battery.toml");
        let config_path = dir.join("config.toml");
        let state_path_str = state_path.to_str().unwrap();
        save_battery_state(
            state_path_str,
            &BatteryState {
                installed_on: Some("2020-01-01".into()),
                last_age_reminder_on: Some("2026-09-06".into()),
                rb_alert_active: true,
            },
        )
        .unwrap();
        fs::write(
            &config_path,
            format!(
                "nut_host = 'localhost'\nnut_ups_name = 'unifi'\nbattery_state_path = {}\n",
                toml::Value::String(state_path_str.to_string())
            ),
        )
        .unwrap();
        reset_battery_lifecycle(config_path.to_str().unwrap()).unwrap();
        let mut state = load_battery_state(state_path_str).unwrap();
        assert_eq!(state.installed_on, Some(today().to_string()));
        assert!(state.last_age_reminder_on.is_none());
        assert!(!state.rb_alert_active);
        assert!(!ensure_battery_install_date(
            &mut state,
            parse_calendar_date("2030-01-01").unwrap()
        ));
        assert_eq!(state.installed_on, Some(today().to_string()));
        fs::remove_file(state_path).unwrap();
        fs::remove_file(config_path).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
