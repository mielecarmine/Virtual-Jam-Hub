//! Headless Virtual Jam server entry point.

use std::{
    env, io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use jam_server::telemetry::{DEFAULT_BETA_SERVER_ENDPOINT, TelemetryConfig};

const HELP: &str = "Usage: jam-server [BIND_ADDR] [OPTIONS]
Options:
  --bind IP:PORT                 UDP listen address (default: 0.0.0.0:7777)
  --share-logs                   Opt-in: ship operational logs to the central beta collector
                                 (default endpoint: https://185.198.245.40/api/v1/logs/server;
                                  env: JAM_SHARE_LOGS=1)
  --telemetry-endpoint URL       Custom collector URL (implies opt-in; env: JAM_TELEMETRY_ENDPOINT)
  --telemetry-token TOKEN        Optional beta key header (env: JAM_TELEMETRY_TOKEN)
  --telemetry-interval-secs N    Periodic upload interval in seconds (default: 60;
                                 env: JAM_TELEMETRY_INTERVAL_SECS)
  --help, -h                     Show this help";

#[derive(Debug, PartialEq, Eq)]
struct CliOptions {
    bind: String,
    telemetry: Option<TelemetryConfig>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_options(env::args().skip(1), |key| env::var(key).ok())? else {
        return Ok(());
    };
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    let _ = ctrlc::set_handler(move || signal_stop.store(true, Ordering::Relaxed));

    let mut server = jam_server::Server::bind_with_telemetry(&options.bind, options.telemetry)?;
    while !stop.load(Ordering::Relaxed) {
        server.poll()?;
    }
    Ok(())
}

fn parse_options(
    args: impl IntoIterator<Item = String>,
    get_env: impl Fn(&str) -> Option<String>,
) -> Result<Option<CliOptions>, Box<dyn std::error::Error>> {
    let mut bind: Option<String> = None;
    let mut share_logs = get_env("JAM_SHARE_LOGS")
        .is_some_and(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes"));
    let mut endpoint = get_env("JAM_TELEMETRY_ENDPOINT").filter(|v| !v.trim().is_empty());
    let mut token = get_env("JAM_TELEMETRY_TOKEN").filter(|v| !v.trim().is_empty());
    let mut interval_secs: u64 =
        match get_env("JAM_TELEMETRY_INTERVAL_SECS").filter(|v| !v.trim().is_empty()) {
            Some(raw) => raw.trim().parse()?,
            None => 60,
        };

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(None);
            }
            "--share-logs" => share_logs = true,
            "--bind"
            | "--telemetry-endpoint"
            | "--telemetry-token"
            | "--telemetry-interval-secs" => {
                let value = args
                    .next()
                    .filter(|v| !v.is_empty() && !v.starts_with("--"))
                    .ok_or_else(|| io::Error::other(format!("{arg} requires a value")))?;
                match arg.as_str() {
                    "--bind" => bind = Some(value),
                    "--telemetry-endpoint" => endpoint = Some(value),
                    "--telemetry-token" => token = Some(value),
                    "--telemetry-interval-secs" => {
                        interval_secs = value.parse()?;
                        if interval_secs == 0 {
                            return Err("--telemetry-interval-secs must be positive".into());
                        }
                    }
                    _ => {}
                }
            }
            other if !other.starts_with('-') && bind.is_none() => {
                bind = Some(other.to_owned());
            }
            other => return Err(format!("Unknown argument: {other}\n{HELP}").into()),
        }
    }

    if interval_secs == 0 {
        return Err("JAM_TELEMETRY_INTERVAL_SECS must be positive".into());
    }

    let telemetry = if let Some(url) = endpoint {
        let mut config = TelemetryConfig::new(url);
        config.beta_token = token;
        config.flush_interval = Duration::from_secs(interval_secs);
        Some(config)
    } else if share_logs {
        let mut config = TelemetryConfig::new(DEFAULT_BETA_SERVER_ENDPOINT);
        config.beta_token = token;
        config.flush_interval = Duration::from_secs(interval_secs);
        Some(config)
    } else {
        None
    };

    Ok(Some(CliOptions {
        bind: bind.unwrap_or_else(|| "0.0.0.0:7777".into()),
        telemetry,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_is_disabled_by_default_and_preserves_positional_bind() {
        let opts = parse_options(["127.0.0.1:9000".into()], |_| None)
            .unwrap()
            .unwrap();
        assert_eq!(opts.bind, "127.0.0.1:9000");
        assert!(opts.telemetry.is_none());
    }

    #[test]
    fn share_logs_enables_default_beta_endpoint_and_custom_endpoint_overrides() {
        let beta = parse_options(["--share-logs".into()], |_| None)
            .unwrap()
            .unwrap();
        assert_eq!(
            beta.telemetry.as_ref().map(|c| c.endpoint.as_str()),
            Some(DEFAULT_BETA_SERVER_ENDPOINT)
        );

        let custom = parse_options(
            [
                "--telemetry-endpoint".into(),
                "http://127.0.0.1:8080/api/v1/logs/server".into(),
                "--telemetry-token".into(),
                "tok".into(),
                "--telemetry-interval-secs".into(),
                "15".into(),
            ],
            |_| None,
        )
        .unwrap()
        .unwrap();
        let config = custom.telemetry.unwrap();
        assert_eq!(config.endpoint, "http://127.0.0.1:8080/api/v1/logs/server");
        assert_eq!(config.beta_token.as_deref(), Some("tok"));
        assert_eq!(config.flush_interval, Duration::from_secs(15));
    }
}
