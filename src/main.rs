use anyhow::{Context, Result};
use chrono::Utc;
use dialoguer::Input;
use futures::executor::block_on;
use governor::{Quota, RateLimiter};
use nonzero_ext::nonzero;
use regex::Regex;
use serde::{Deserialize, Serialize};
use signal_hook::{consts::TERM_SIGNALS, iterator::Signals};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[derive(Serialize, Deserialize, Debug)]
struct Config {
    log_files: Vec<String>,
    endpoint: String,
    rate_limit: Option<u32>,
    service_name: Option<String>,
    host_name: Option<String>,
}

struct LogEntry {
    line: String,
    file: String,
    endpoint: String,
}

fn load_or_create_config<P: AsRef<Path>>(config_path: P) -> Result<Config> {
    if config_path.as_ref().exists() {
        let contents = fs::read_to_string(&config_path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    } else {
        println!("No config.toml found. Let's create one.");
        let log_files = Input::<String>::new()
            .with_prompt("Enter comma-separated log file paths")
            .interact_text()?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let endpoint = Input::<String>::new()
            .with_prompt("Enter SigNoz OTLP HTTP endpoint")
            .default("http://localhost:4318/v1/logs".into())
            .interact_text()?;

        let rate_limit = Input::<u32>::new()
            .with_prompt("Enter rate limit (logs per second, 0 for unlimited)")
            .default(100)
            .interact_text()?;

        let rate_limit = if rate_limit > 0 {
            Some(rate_limit)
        } else {
            None
        };

        let service_name = Input::<String>::new()
            .with_prompt("Enter service name (for SigNoz)")
            .default("rust-signoz-agent".into())
            .interact_text()?;

        let host_name = Input::<String>::new()
            .with_prompt("Enter host name (leave blank to auto-detect)")
            .default("".into())
            .interact_text()?;

        let service_name = if service_name.trim().is_empty() {
            None
        } else {
            Some(service_name)
        };
        let host_name = if host_name.trim().is_empty() {
            None
        } else {
            Some(host_name)
        };

        let config = Config {
            log_files,
            endpoint,
            rate_limit,
            service_name,
            host_name,
        };

        let toml_str = toml::to_string_pretty(&config)?;
        fs::write(&config_path, toml_str)?;
        println!("Saved config to {:?}", config_path.as_ref());
        Ok(config)
    }
}

fn validate_config(config: &Config) -> Result<()> {
    for log_file in &config.log_files {
        let path = Path::new(log_file);
        if !path.exists() {
            return Err(anyhow::anyhow!("Log file does not exist: {}", log_file));
        }
        if let Err(e) = fs::metadata(path) {
            return Err(anyhow::anyhow!(
                "Cannot access log file {}: {}",
                log_file,
                e
            ));
        }
    }

    if !config.endpoint.starts_with("http://") && !config.endpoint.starts_with("https://") {
        return Err(anyhow::anyhow!(
            "Endpoint URL must start with http:// or https://"
        ));
    }

    let url_regex = Regex::new(r"^https?://[^\s/$.?#].[^\s]*$").unwrap();
    if !url_regex.is_match(&config.endpoint) {
        return Err(anyhow::anyhow!(
            "Invalid endpoint URL format: {}",
            config.endpoint
        ));
    }

    Ok(())
}

fn tail_file<F>(path: String, mut handler: F) -> thread::JoinHandle<()>
where
    F: FnMut(String) + Send + 'static,
{
    thread::spawn(move || {
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Failed to open {}: {e}", path);
                return;
            }
        };

        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::End(0)).ok();

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    thread::sleep(Duration::from_millis(500));
                }
                Ok(_) => {
                    if !line.trim().is_empty() {
                        handler(line.trim_end().to_string());
                    }
                }
                Err(e) => {
                    eprintln!("Error reading {}: {e}", path);
                    thread::sleep(Duration::from_secs(5));
                    match fs::File::open(&path) {
                        Ok(f) => {
                            reader = BufReader::new(f);
                            reader.seek(SeekFrom::End(0)).ok();
                            println!("Successfully reopened {}", path);
                        }
                        Err(e) => {
                            eprintln!("Failed to reopen {}: {e}", path);
                            thread::sleep(Duration::from_secs(30));
                        }
                    }
                }
            }
        }
    })
}

#[derive(Serialize, Debug)]
struct OtlpLogRecord {
    resourceLogs: Vec<ResourceLog>,
}

#[derive(Serialize, Debug)]
struct ResourceLog {
    resource: Resource,
    scopeLogs: Vec<ScopeLog>,
}

#[derive(Serialize, Debug)]
struct Resource {
    attributes: Vec<KeyValue>,
}

#[derive(Serialize, Debug)]
struct ScopeLog {
    logRecords: Vec<LogRecord>,
}

#[derive(Serialize, Debug)]
struct LogRecord {
    timeUnixNano: String,
    severityText: String,
    severityNumber: u8,
    body: LogBody,
    attributes: Vec<KeyValue>,
}

#[derive(Serialize, Debug)]
struct LogBody {
    #[serde(rename = "stringValue")]
    string_value: String,
}

#[derive(Serialize, Debug)]
struct KeyValue {
    key: String,
    value: AttributeValue,
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
enum AttributeValue {
    StringValue {
        #[serde(rename = "stringValue")]
        value: String,
    },
}

fn build_otlp_payload(
    line: &str,
    file: &str,
    severity_text: &str,
    severity_number: u8,
    config: &Config,
) -> OtlpLogRecord {
    let service_name = config
        .service_name
        .as_deref()
        .unwrap_or("rust-signoz-agent");
    let host_name = config
        .host_name
        .clone()
        .or_else(|| hostname::get().ok().map(|h| h.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".to_string());

    OtlpLogRecord {
        resourceLogs: vec![ResourceLog {
            resource: Resource {
                attributes: vec![
                    KeyValue {
                        key: "service.name".into(),
                        value: AttributeValue::StringValue {
                            value: service_name.into(),
                        },
                    },
                    KeyValue {
                        key: "host.name".into(),
                        value: AttributeValue::StringValue {
                            value: host_name,
                        },
                    },
                ],
            },
            scopeLogs: vec![ScopeLog {
                logRecords: vec![LogRecord {
                    timeUnixNano: format!("{}", Utc::now().timestamp_nanos_opt().unwrap_or(0)),
                    severityText: severity_text.into(),
                    severityNumber: severity_number,
                    body: LogBody {
                        string_value: line.into(),
                    },
                    attributes: vec![KeyValue {
                        key: "log.file".into(),
                        value: AttributeValue::StringValue { value: file.into() },
                    }],
                }],
            }],
        }],
    }
}

fn send_to_signoz(client: &reqwest::blocking::Client, endpoint: &str, log_line: &str, file: &str, config: &Config) {
    const MAX_RETRIES: usize = 3;
    let (severity_text, severity_number) = detect_severity_generic(log_line);
    let payload = build_otlp_payload(log_line, file, severity_text, severity_number, &config);

    for attempt in 1..=MAX_RETRIES {
        match client.post(endpoint).json(&payload).send() {
            Ok(r) if r.status().is_success() => {
                println!(
                    "Successfully sent to SigNoz: [{}] ({}/{})",
                    log_line, severity_text, severity_number
                );
                return;
            }
            Ok(r) => {
                eprintln!(
                    "Failed to send log to SigNoz: HTTP {} (attempt {}/{})",
                    r.status(),
                    attempt,
                    MAX_RETRIES
                );
            }
            Err(e) => {
                eprintln!(
                    "HTTP error sending log to SigNoz: {} (attempt {}/{})",
                    e, attempt, MAX_RETRIES
                );
            }
        }

        if attempt < MAX_RETRIES {
            let backoff = Duration::from_millis(500 * 2u64.pow((attempt - 1) as u32));
            thread::sleep(backoff);
        }
    }

    eprintln!(
        "Failed to send log after {} attempts, discarding: {}",
        MAX_RETRIES, log_line
    );
}

fn detect_severity_generic(line: &str) -> (&'static str, u8) {
    let regex =
        Regex::new(r"(?i)\b(INFO|ERROR|WARN|WARNING|DEBUG|CRITICAL|FATAL|NOTICE|TRACE)\b").unwrap();
    if let Some(cap) = regex.captures(line) {
        let sev = cap.get(1).unwrap().as_str().to_uppercase();
        match sev.as_str() {
            "TRACE" => ("TRACE", 4),
            "DEBUG" => ("DEBUG", 8),
            "INFO" => ("INFO", 12),
            "NOTICE" => ("INFO", 12),
            "WARN" | "WARNING" => ("WARN", 13),
            "ERROR" => ("ERROR", 17),
            "CRITICAL" | "FATAL" => ("FATAL", 21),
            _ => ("INFO", 12),
        }
    } else {
        ("INFO", 12)
    }
}

fn create_systemd_service() -> Result<()> {
    let service_content = format!(
        r#"[Unit]
Description=Rust SigNoz Agent
After=network.target

[Service]
Type=simple
User={}
WorkingDirectory={}
ExecStart={}
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=multi-user.target
"#,
        whoami::username(),
        std::env::current_dir()?.display(),
        std::env::current_exe()?.display()
    );

    let service_path = "/tmp/rust-signoz-agent.service";
    fs::write(service_path, service_content)?;

    println!("Service file created at: {}", service_path);
    println!("To install the service, run:");
    println!("  sudo cp {} /etc/systemd/system/", service_path);
    println!("  sudo systemctl daemon-reload");
    println!("  sudo systemctl enable rust-signoz-agent");
    println!("  sudo systemctl start rust-signoz-agent");

    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--install-service" {
        return create_systemd_service().context("Failed to create systemd service");
    }

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    let mut signals = Signals::new(TERM_SIGNALS)?;
    thread::spawn(move || {
        for _ in signals.forever() {
            println!("Received termination signal, shutting down...");
            r.store(false, Ordering::SeqCst);
            break;
        }
    });

    let config_path = "./config.toml";
    let config = load_or_create_config(config_path)?;
    validate_config(&config).context("Invalid configuration")?;

    println!("Monitoring log files: {:?}", config.log_files);
    println!("SigNoz endpoint: {}", config.endpoint);

    let limiter = config.rate_limit.map(|limit| {
        let limit = NonZeroU32::new(limit).unwrap_or(nonzero!(100u32));
        println!("Rate limiting enabled: {} logs/second", limit);
        RateLimiter::direct(Quota::per_second(limit))
    });

    let (tx, rx) = mpsc::channel::<LogEntry>();
    let config = Arc::new(config);
    let sender_config = config.clone();
    let _sender_thread = thread::spawn(move || {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());

        while let Ok(entry) = rx.recv() {
            if let Some(ref limiter) = limiter {
                block_on(limiter.until_ready());
            }

            send_to_signoz(&client, &entry.endpoint, &entry.line, &entry.file, &sender_config);
        }
    });

    let mut handles = Vec::new();
    for log_path in &config.log_files {
        let path = log_path.clone();
        let endpoint = config.endpoint.clone();
        let file_id = path.clone();
        let tx = tx.clone();

        let handle = tail_file(path.clone(), move |line| {
            println!("[{}] {}", file_id, line);
            tx.send(LogEntry {
                line,
                file: file_id.clone(),
                endpoint: endpoint.clone(),
            })
            .unwrap_or_else(|e| eprintln!("Failed to send log to channel: {e}"));
        });

        handles.push(handle);
    }

    println!("rust-signoz-agent is running. Press Ctrl+C to exit.");
    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_secs(1));
    }

    println!("Shutting down gracefully...");
    thread::sleep(Duration::from_secs(2));

    Ok(())
}
