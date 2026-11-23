//! Backend and device selection, performed outside the audio callbacks.

use std::{error::Error, io, net::IpAddr, path::PathBuf, time::Duration};

use cpal::traits::{DeviceTrait, HostTrait};

const HELP: &str = "Usage: jam-client (no arguments opens the desktop setup screen)
       jam-client [--server IP:PORT] [--nickname NAME] [--host NAME] [--input NAME] [--output NAME] [--seconds N]
       jam-client --server IP:PORT --synthetic [--buffer-frames 64|128|256] [--telemetry-dir DIR] [--bind-ip IP]
       jam-client [--host NAME] --list-devices
Input routing: --instrument-channel N (default 1), --mic-channel N|off (default off).
Without --server: local loopback. With --server: UDP Opus audio (2.5 ms, 96 kbps) and local monitoring.
Default: system backend/devices, 300 seconds. Host names are case-insensitive.
Use exact device names from --list-devices. No fallback if a requested device is unavailable.
Windows: --host asio selects the vendor ASIO driver; --host wasapi selects WASAPI.
Linux: --host alsa; use a hardware device name for direct ALSA, default/pipewire for PipeWire.
macOS: --host coreaudio.
UDP: --jitter-chunks 3 (1..=16), --simulate-loss 0.02 (0..=1), --simulate-jitter-ms 10 (0..=1000),
     --simulate-delay-ms 15 (0..=1000), --bind-ip IP, --metronome-bpm 120 (30..=300).
Telemetry (opt-in): --share-logs (or JAM_SHARE_LOGS=1), --telemetry-endpoint URL, --telemetry-token TOKEN.";

#[allow(clippy::struct_excessive_bools)]
pub struct Options {
    pub network: crate::network::Settings,
    pub bind_ip: Option<IpAddr>,
    pub host: Option<String>,
    pub server: Option<std::net::SocketAddr>,
    pub nickname: String,
    pub input: Option<String>,
    pub instrument_channel: u16,
    pub mic_channel: Option<u16>,
    pub output: Option<String>,
    pub duration: Duration,
    pub buffer_frames: u32,
    pub metronome_bpm: Option<u16>,
    pub telemetry_dir: Option<PathBuf>,
    pub share_logs: bool,
    pub telemetry_endpoint: Option<String>,
    pub telemetry_token: Option<String>,
    pub synthetic: bool,
    pub list: bool,
}

impl Options {
    #[must_use]
    pub fn report_target(&self) -> Option<crate::session_report::ReportTarget> {
        let env_share = std::env::var("JAM_SHARE_LOGS")
            .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes"));
        let env_endpoint = std::env::var("JAM_TELEMETRY_ENDPOINT")
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        let endpoint = self
            .telemetry_endpoint
            .clone()
            .or(env_endpoint)
            .or_else(|| {
                (self.share_logs || env_share)
                    .then(|| crate::session_report::DEFAULT_BETA_CLIENT_ENDPOINT.to_owned())
            })?;
        let beta_token = self.telemetry_token.clone().or_else(|| {
            std::env::var("JAM_TELEMETRY_TOKEN")
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        });
        Some(crate::session_report::ReportTarget {
            endpoint,
            beta_token,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Self>, Box<dyn Error>> {
        let mut args = args.into_iter();
        let mut options = Self {
            network: crate::network::Settings::default(),
            bind_ip: None,
            host: None,
            server: None,
            nickname: "musician".into(),
            input: None,
            instrument_channel: 1,
            mic_channel: None,
            output: None,
            duration: Duration::from_secs(300),
            buffer_frames: 128,
            metronome_bpm: None,
            telemetry_dir: None,
            share_logs: false,
            telemetry_endpoint: None,
            telemetry_token: None,
            synthetic: false,
            list: false,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => {
                    println!("{HELP}");
                    return Ok(None);
                }
                "--list-devices" => options.list = true,
                "--synthetic" => options.synthetic = true,
                "--share-logs" => options.share_logs = true,
                "--jitter-chunks"
                | "--simulate-loss"
                | "--simulate-jitter-ms"
                | "--simulate-delay-ms"
                | "--bind-ip"
                | "--buffer-frames"
                | "--metronome-bpm"
                | "--telemetry-dir"
                | "--telemetry-endpoint"
                | "--telemetry-token"
                | "--host"
                | "--input"
                | "--instrument-channel"
                | "--mic-channel"
                | "--output"
                | "--seconds"
                | "--server"
                | "--nickname" => {
                    let value = args
                        .next()
                        .filter(|value| !value.is_empty() && !value.starts_with("--"))
                        .ok_or_else(|| io::Error::other(format!("{arg} requires a value")))?;
                    match arg.as_str() {
                        "--jitter-chunks" => {
                            let chunks = value.parse::<usize>()?;
                            if !(1..=16).contains(&chunks) {
                                return Err("--jitter-chunks must be 1..=16".into());
                            }
                            options.network.target_chunks = chunks;
                        }
                        "--simulate-loss" => {
                            let loss = value.parse::<f64>()?;
                            if !loss.is_finite() || !(0.0..=1.0).contains(&loss) {
                                return Err("--simulate-loss must be 0..=1".into());
                            }
                            options.network.loss = loss;
                        }
                        "--simulate-jitter-ms" => {
                            let jitter = value.parse::<u32>()?;
                            if jitter > 1000 {
                                return Err("--simulate-jitter-ms must be 0..=1000".into());
                            }
                            options.network.jitter_ms = jitter;
                        }
                        "--simulate-delay-ms" => {
                            let delay = value.parse::<u32>()?;
                            if delay > 1000 {
                                return Err("--simulate-delay-ms must be 0..=1000".into());
                            }
                            options.network.delay_ms = delay;
                        }
                        "--bind-ip" => options.bind_ip = Some(value.parse()?),
                        "--buffer-frames" => {
                            let frames = value.parse::<u32>()?;
                            if !crate::setup::BUFFER_SIZES.contains(&frames) {
                                return Err("--buffer-frames must be 64, 128 or 256".into());
                            }
                            options.buffer_frames = frames;
                        }
                        "--metronome-bpm" => {
                            let bpm = value.parse::<u16>()?;
                            if !(30..=300).contains(&bpm) {
                                return Err("--metronome-bpm must be 30..=300".into());
                            }
                            options.metronome_bpm = Some(bpm);
                        }
                        "--telemetry-dir" => options.telemetry_dir = Some(PathBuf::from(value)),
                        "--telemetry-endpoint" => options.telemetry_endpoint = Some(value),
                        "--telemetry-token" => options.telemetry_token = Some(value),
                        "--host" => options.host = Some(value),
                        "--server" => options.server = Some(value.parse()?),
                        "--nickname" => {
                            if value.len() > 32 {
                                return Err("Nickname must contain 1..=32 UTF-8 bytes".into());
                            }
                            options.nickname = value;
                        }
                        "--input" => options.input = Some(value),
                        "--instrument-channel" => options.instrument_channel = value.parse()?,
                        "--mic-channel" => {
                            options.mic_channel = if value == "off" {
                                None
                            } else {
                                Some(value.parse()?)
                            };
                        }
                        "--output" => options.output = Some(value),
                        _ => {
                            let seconds = value.parse::<u64>()?;
                            if seconds == 0 {
                                return Err("--seconds must be positive".into());
                            }
                            options.duration = Duration::from_secs(seconds);
                        }
                    }
                }
                _ => return Err(format!("Unknown argument: {arg}\n{HELP}").into()),
            }
        }
        let routing =
            crate::audio::InputRouting::new(options.instrument_channel, options.mic_channel)?;
        if options.synthetic && routing != crate::audio::InputRouting::default() {
            return Err("--synthetic does not support hardware input channel routing".into());
        }
        if options.synthetic && options.server.is_none() && !options.list {
            return Err("--synthetic requires --server IP:PORT".into());
        }
        if let (Some(ip), Some(server)) = (options.bind_ip, options.server) {
            if ip.is_ipv4() != server.is_ipv4() {
                return Err("--bind-ip address family must match --server".into());
            }
        }
        Ok(Some(options))
    }
}

pub fn host(name: Option<&str>) -> Result<cpal::Host, Box<dyn Error>> {
    let Some(name) = name else {
        return Ok(cpal::default_host());
    };
    let id = cpal::available_hosts()
        .into_iter()
        .find(|id| id.name().eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            io::Error::other(format!(
                "Backend '{name}' is unavailable in this build/system"
            ))
        })?;
    Ok(cpal::host_from_id(id)?)
}

#[derive(Clone, Copy)]
pub enum Direction {
    Input,
    Output,
}

pub fn device(
    host: &cpal::Host,
    name: Option<&str>,
    direction: Direction,
) -> Result<cpal::Device, Box<dyn Error>> {
    let device = if let Some(name) = name {
        let mut selected = None;
        // Drop unrelated devices immediately: ALSA enumeration opens exclusive handles.
        for device in host.devices()? {
            if device.name()? == name && selected.replace(device).is_some() {
                return Err(format!("Device name '{name}' is ambiguous").into());
            }
        }
        selected
    } else {
        match direction {
            Direction::Input => host.default_input_device(),
            Direction::Output => host.default_output_device(),
        }
    };
    device.ok_or_else(|| {
        io::Error::other("Audio device unavailable or busy; use --list-devices").into()
    })
}

pub fn devices(
    host: &cpal::Host,
    input: Option<&str>,
    output: Option<&str>,
) -> Result<(cpal::Device, cpal::Device), Box<dyn Error>> {
    let input = device(host, input, Direction::Input)?;
    let default_output = if output.is_none() {
        host.default_output_device()
    } else {
        None
    };
    let output_name = output.map(str::to_owned).or_else(|| {
        default_output
            .as_ref()
            .and_then(|device| device.name().ok())
    });
    // Reuse the duplex device, rather than reopening the same exclusive ALSA/ASIO driver.
    let output = if output_name.as_deref() == Some(input.name()?.as_str()) {
        input.clone()
    } else if output.is_some() {
        device(host, output, Direction::Output)?
    } else {
        default_output.ok_or_else(|| io::Error::other("Default output device unavailable"))?
    };
    Ok((input, output))
}

pub fn list_devices(host: &cpal::Host) -> Result<(), Box<dyn Error>> {
    println!("Available backends:");
    for id in cpal::available_hosts() {
        println!("  {}", id.name());
    }
    println!("Selected backend: {}", host.id().name());
    println!("Input devices:");
    for device in host.input_devices()? {
        println!("  {}", device.name()?);
    }
    println!("Output devices:");
    for device in host.output_devices()? {
        println!("  {}", device.name()?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_options_validate_routing_and_keep_synthetic_hardware_independent() {
        let defaults = Options::parse(std::iter::empty()).unwrap().unwrap();
        assert_eq!(
            (defaults.instrument_channel, defaults.mic_channel),
            (1, None)
        );
        let options =
            Options::parse(["--instrument-channel", "4", "--mic-channel", "2"].map(str::to_owned))
                .unwrap()
                .unwrap();
        assert_eq!(
            (options.instrument_channel, options.mic_channel),
            (4, Some(2))
        );
        let off = Options::parse(["--mic-channel", "off"].map(str::to_owned))
            .unwrap()
            .unwrap();
        assert_eq!(off.mic_channel, None);
        for args in [
            vec!["--instrument-channel", "0"],
            vec!["--mic-channel", "0"],
            vec!["--mic-channel", "1"],
            vec!["--instrument-channel", "-1"],
            vec!["--mic-channel", "65536"],
            vec!["--mic-channel"],
            vec!["--instrument-channel", "3", "--mic-channel", "3"],
            vec![
                "--server",
                "127.0.0.1:7777",
                "--synthetic",
                "--mic-channel",
                "2",
            ],
            vec![
                "--server",
                "127.0.0.1:7777",
                "--synthetic",
                "--instrument-channel",
                "2",
            ],
        ] {
            assert!(Options::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn validates_jitter_and_simulator_ranges() {
        let options = Options::parse(
            [
                "--jitter-chunks",
                "2",
                "--simulate-loss",
                "0.02",
                "--simulate-jitter-ms",
                "10",
            ]
            .map(str::to_owned),
        )
        .unwrap()
        .unwrap();
        assert_eq!(options.network.target_chunks, 2);
        assert!((options.network.loss - 0.02).abs() < f64::EPSILON);
        assert_eq!(options.network.jitter_ms, 10);
        for (flag, value) in [
            ("--jitter-chunks", "0"),
            ("--jitter-chunks", "17"),
            ("--simulate-loss", "NaN"),
            ("--simulate-loss", "inf"),
            ("--simulate-loss", "-0.1"),
            ("--simulate-loss", "1.1"),
            ("--simulate-jitter-ms", "1001"),
            ("--simulate-jitter-ms", "-1"),
        ] {
            assert!(Options::parse([flag, value].map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn parses_explicit_backend_and_devices_without_changing_names() {
        let options = Options::parse(
            [
                "--host",
                "ASIO",
                "--input",
                "Focusrite USB ASIO",
                "--output",
                "Focusrite USB ASIO",
                "--seconds",
                "10",
                "--list-devices",
            ]
            .map(str::to_owned),
        )
        .expect("valid arguments")
        .expect("options");
        assert_eq!(options.host.as_deref(), Some("ASIO"));
        assert_eq!(options.input.as_deref(), Some("Focusrite USB ASIO"));
        assert_eq!(options.output.as_deref(), Some("Focusrite USB ASIO"));
        assert_eq!(options.duration, Duration::from_secs(10));
        assert!(options.list);
    }

    #[test]
    fn parses_udp_options_and_rejects_invalid_endpoints_and_nicknames() {
        let options =
            Options::parse(["--server", "[::1]:7777", "--nickname", "chitarra"].map(str::to_owned))
                .unwrap()
                .unwrap();
        assert_eq!(options.server, Some("[::1]:7777".parse().unwrap()));
        assert_eq!(options.nickname, "chitarra");
        for args in [
            vec!["--server".to_owned()],
            vec!["--server".into(), "localhost".into()],
            vec!["--nickname".into(), "x".repeat(33)],
            vec!["--nickname".into(), String::new()],
        ] {
            assert!(Options::parse(args).is_err());
        }
        let defaults = Options::parse(std::iter::empty()).unwrap().unwrap();
        assert!(defaults.server.is_none());
    }

    #[test]
    fn rejects_missing_values_invalid_durations_and_unknown_arguments() {
        for args in [
            vec!["--input"],
            vec!["--host", "--list-devices"],
            vec!["--seconds", "0"],
            vec!["--seconds", "-1"],
            vec!["--seconds", "no"],
            vec!["--unknown"],
        ] {
            assert!(Options::parse(args.into_iter().map(str::to_owned)).is_err());
        }
        assert!(host(Some("nonexistent-backend")).is_err());
    }

    #[test]
    fn parses_synthetic_beta_tester_options_and_validates_constraints() {
        let options = Options::parse(
            [
                "--server",
                "127.0.0.1:7777",
                "--synthetic",
                "--buffer-frames",
                "64",
                "--simulate-delay-ms",
                "15",
                "--bind-ip",
                "127.0.0.1",
                "--metronome-bpm",
                "120",
                "--telemetry-dir",
                "logs/beta",
            ]
            .map(str::to_owned),
        )
        .unwrap()
        .unwrap();
        assert!(options.synthetic);
        assert_eq!(options.buffer_frames, 64);
        assert_eq!(options.network.delay_ms, 15);
        assert_eq!(options.bind_ip, Some("127.0.0.1".parse().unwrap()));
        assert_eq!(options.metronome_bpm, Some(120));
        assert_eq!(options.telemetry_dir, Some(PathBuf::from("logs/beta")));

        for invalid in [
            vec!["--synthetic"],
            vec!["--server", "127.0.0.1:7777", "--buffer-frames", "120"],
            vec!["--server", "127.0.0.1:7777", "--simulate-delay-ms", "1001"],
            vec!["--server", "127.0.0.1:7777", "--bind-ip", "::1"],
            vec!["--server", "127.0.0.1:7777", "--metronome-bpm", "29"],
            vec!["--server", "127.0.0.1:7777", "--metronome-bpm", "301"],
        ] {
            assert!(
                Options::parse(invalid.into_iter().map(str::to_owned)).is_err(),
                "expected error"
            );
        }
    }
}
