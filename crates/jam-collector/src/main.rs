//! Headless Virtual Jam central log collector entry point.

use std::{
    env, io,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

use jam_collector::{Collector, CollectorConfig};

const HELP: &str = "Usage: jam-collector [OPTIONS]
Options:
  --bind IP:PORT              Listen address (default: 127.0.0.1:8080, env: JAM_COLLECTOR_BIND)
  --storage-dir DIR           Root log directory (default: /var/log/jam-collector, env: JAM_COLLECTOR_STORAGE_DIR)
  --beta-token TOKEN          Optional shared beta key (env: JAM_COLLECTOR_BETA_TOKEN)
  --no-trust-proxy            Ignore X-Real-IP / X-Forwarded-For even from loopback
  --rate-limit-per-min N      Max requests per minute per IP (default: 30, env: JAM_COLLECTOR_RATE_LIMIT)
  --retention-days N          Delete date folders older than N days (default: 30, env: JAM_COLLECTOR_RETENTION_DAYS)
  --max-disk-mb N             Max storage quota in MiB, 0 = unlimited (default: 2048, env: JAM_COLLECTOR_MAX_DISK_MB)
  --help, -h                  Show this help";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(config) = parse_config(env::args().skip(1))? else {
        return Ok(());
    };
    let collector = Collector::bind(config.clone())?;
    let local = collector.local_addr()?;
    eprintln!(
        "jam-collector listening on {local}, storing logs in {}",
        config.storage_dir.display()
    );
    let stop = Arc::new(AtomicBool::new(false));
    collector.run(&stop)?;
    Ok(())
}

fn parse_config(
    args: impl IntoIterator<Item = String>,
) -> Result<Option<CollectorConfig>, Box<dyn std::error::Error>> {
    let mut config = CollectorConfig::default();

    if let Ok(value) = env::var("JAM_COLLECTOR_BIND") {
        if !value.trim().is_empty() {
            config.bind_addr = value.trim().parse()?;
        }
    }
    if let Ok(value) = env::var("JAM_COLLECTOR_STORAGE_DIR") {
        if !value.trim().is_empty() {
            config.storage_dir = PathBuf::from(value);
        }
    }
    if let Ok(value) = env::var("JAM_COLLECTOR_BETA_TOKEN") {
        if !value.trim().is_empty() {
            config.beta_token = Some(value.trim().to_owned());
        }
    }
    if let Ok(value) = env::var("JAM_COLLECTOR_TRUST_PROXY") {
        config.trust_proxy_headers = matches!(value.trim(), "1" | "true" | "TRUE" | "yes");
    }
    if let Ok(value) = env::var("JAM_COLLECTOR_RATE_LIMIT") {
        if !value.trim().is_empty() {
            config.rate_limit_per_min = value.trim().parse()?;
        }
    }
    if let Ok(value) = env::var("JAM_COLLECTOR_RETENTION_DAYS") {
        if !value.trim().is_empty() {
            config.retention_days = value.trim().parse()?;
        }
    }
    if let Ok(value) = env::var("JAM_COLLECTOR_MAX_DISK_MB") {
        if !value.trim().is_empty() {
            let mb: u64 = value.trim().parse()?;
            config.max_disk_bytes = mb.saturating_mul(1024 * 1024);
        }
    }

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(None);
            }
            "--no-trust-proxy" => config.trust_proxy_headers = false,
            "--bind"
            | "--storage-dir"
            | "--beta-token"
            | "--rate-limit-per-min"
            | "--retention-days"
            | "--max-disk-mb" => {
                let value = args
                    .next()
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| io::Error::other(format!("{arg} requires a value")))?;
                match arg.as_str() {
                    "--bind" => config.bind_addr = value.parse()?,
                    "--storage-dir" => config.storage_dir = PathBuf::from(value),
                    "--beta-token" => config.beta_token = Some(value),
                    "--rate-limit-per-min" => config.rate_limit_per_min = value.parse()?,
                    "--retention-days" => config.retention_days = value.parse()?,
                    "--max-disk-mb" => {
                        let mb: u64 = value.parse()?;
                        config.max_disk_bytes = mb.saturating_mul(1024 * 1024);
                    }
                    _ => {}
                }
            }
            _ => return Err(format!("Unknown argument: {arg}\n{HELP}").into()),
        }
    }
    Ok(Some(config))
}
