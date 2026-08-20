use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const DEFAULT_CONFIG_PATH: &str = "/etc/unifi-ups-monitor/config.toml";

#[derive(Debug, Deserialize)]
struct Config {
    nut_host: String,
    nut_port: Option<u16>,
    nut_ups_name: String,
    nut_username: Option<String>,
    nut_password: Option<String>,
    connection_timeout_seconds: Option<u64>,
    poll_interval_seconds: Option<u64>,
    runtime_shutdown_seconds: Option<u64>,
    charge_shutdown_percent: Option<f64>,
    shutdown_command: Option<String>,
    require_on_battery: Option<bool>,
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
    if let Err(error) = run() {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config_path = env::args()
        .nth(1)
        .or_else(|| env::var("UNIFI_UPS_MONITOR_CONFIG").ok())
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());
    let config = load_config(&config_path)?;

    println!(
        "starting unifi-ups-monitor for '{}' at {}:{} using config {}",
        config.nut_ups_name,
        config.nut_host,
        config.nut_port.unwrap_or(3493),
        config_path
    );

    let mut client = None;
    loop {
        if client.is_none() {
            match NutClient::connect(&config) {
                Ok(connected) => {
                    println!("connected to NUT server");
                    client = Some(connected);
                }
                Err(error) => eprintln!("warning: NUT connection failed: {error}"),
            }
        }

        if let Some(connected) = client.as_mut() {
            match connected.read_snapshot(&config.nut_ups_name) {
                Ok(snapshot) => {
                    println!(
                        "status={:?} runtime={:?} charge={:?}",
                        snapshot.status, snapshot.battery_runtime, snapshot.battery_charge
                    );
                    if should_shutdown(&config, &snapshot) {
                        let reason = shutdown_reason(&config, &snapshot);
                        println!("shutdown condition reached: {reason}");
                        if let Err(error) = notify_shutdown(&reason) {
                            eprintln!("warning: failed to send shutdown notification: {error}");
                        }
                        run_shutdown(&config)?;
                        return Ok(());
                    }
                }
                Err(error) => {
                    eprintln!("warning: NUT query failed, reconnecting: {error}");
                    client = None;
                }
            }
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

fn notify_shutdown(reason: &str) -> Result<(), String> {
    if !Path::new("/root/.forward").exists() {
        return Ok(());
    }
    let mut child = Command::new("mail")
        .args(["-s", "UniFi UPS: Server wird heruntergefahren", "root"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start mail: {error}"))?;
    let body = format!(
        "Der Server wird wegen eines USV-Schwellwerts heruntergefahren.\n\nGrund: {reason}\n"
    );
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
    if status.success() {
        Ok(())
    } else {
        Err(format!("mail exited with {status}"))
    }
}

fn run_shutdown(config: &Config) -> Result<(), String> {
    let command = config
        .shutdown_command
        .as_deref()
        .unwrap_or("/sbin/shutdown -h now");
    let parts = command.split_whitespace().collect::<Vec<_>>();
    let (program, args) = parts
        .split_first()
        .ok_or_else(|| "shutdown_command is empty".to_string())?;
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("failed to execute shutdown command '{command}': {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("shutdown command '{command}' exited with {status}"))
    }
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
}
