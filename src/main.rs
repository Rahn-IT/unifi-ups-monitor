use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const DEFAULT_CONFIG_PATH: &str = "/etc/unifi-ups-monitor/config.toml";

#[derive(Debug, Deserialize)]
struct Config {
    ups_name: String,
    upsc_path: Option<String>,
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
        "starting unifi-ups-monitor for '{}' using config {}",
        config.ups_name, config_path
    );

    loop {
        match read_ups_snapshot(&config) {
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
            Err(error) => eprintln!("warning: failed to query UPS state: {error}"),
        }

        thread::sleep(Duration::from_secs(
            config.poll_interval_seconds.unwrap_or(15),
        ));
    }
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

fn load_config(path: &str) -> Result<Config, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("unable to read config {path}: {error}"))?;
    toml::from_str(&content).map_err(|error| format!("invalid config {path}: {error}"))
}

fn read_ups_snapshot(config: &Config) -> Result<UpsSnapshot, String> {
    let upsc_path = config.upsc_path.as_deref().unwrap_or("upsc");
    let output = Command::new(upsc_path)
        .arg(&config.ups_name)
        .output()
        .map_err(|error| format!("failed to start {upsc_path}: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "{upsc_path} exited with {}. stdout='{}' stderr='{}'",
            output.status,
            stdout.trim(),
            stderr.trim()
        ));
    }

    let response = String::from_utf8(output.stdout)
        .map_err(|error| format!("upsc output is not valid utf-8: {error}"))?;
    parse_snapshot(&response)
}

fn parse_snapshot(response: &str) -> Result<UpsSnapshot, String> {
    let values = parse_key_values(response);
    let status_raw = values
        .get("ups.status")
        .ok_or_else(|| "missing ups.status in upsc output".to_string())?;

    let status = status_raw
        .split_whitespace()
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect::<HashSet<_>>();

    Ok(UpsSnapshot {
        status,
        battery_runtime: parse_u64(values.get("battery.runtime")),
        battery_charge: parse_f64(values.get("battery.charge")),
    })
}

fn parse_key_values(response: &str) -> HashMap<String, String> {
    response
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

fn parse_u64(value: Option<&String>) -> Option<u64> {
    value.and_then(|raw| raw.parse::<u64>().ok())
}

fn parse_f64(value: Option<&String>) -> Option<f64> {
    value.and_then(|raw| raw.parse::<f64>().ok())
}

fn should_shutdown(config: &Config, snapshot: &UpsSnapshot) -> bool {
    if config.require_on_battery.unwrap_or(true) && !snapshot.status.contains("OB") {
        return false;
    }

    if let Some(limit) = config.runtime_shutdown_seconds {
        if let Some(runtime) = snapshot.battery_runtime {
            if runtime <= limit {
                return true;
            }
        }
    }

    if let Some(limit) = config.charge_shutdown_percent {
        if let Some(charge) = snapshot.battery_charge {
            if charge <= limit {
                return true;
            }
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

fn run_shutdown(config: &Config) -> Result<(), String> {
    let command = config
        .shutdown_command
        .as_deref()
        .unwrap_or("/sbin/shutdown -h now");
    let (program, args) = split_command(command)?;

    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("failed to execute shutdown command '{command}': {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "shutdown command '{}' exited with {}",
            command, status
        ))
    }
}

fn split_command(command: &str) -> Result<(&str, Vec<&str>), String> {
    let parts = command.split_whitespace().collect::<Vec<_>>();
    let (program, args) = parts
        .split_first()
        .ok_or_else(|| "shutdown_command is empty".to_string())?;
    Ok((program, args.to_vec()))
}

#[allow(dead_code)]
fn ensure_parent_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}
