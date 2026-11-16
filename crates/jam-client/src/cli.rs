//! Backend and device selection, performed outside the audio callbacks.

use std::{error::Error, io, time::Duration};

use cpal::traits::{DeviceTrait, HostTrait};

const HELP: &str = "Usage: jam-client [--host NAME] [--input NAME] [--output NAME] [--seconds N]
       jam-client [--host NAME] --list-devices
Default: system backend/devices, 300 seconds. Host names are case-insensitive.
Use exact device names from --list-devices. No fallback if a requested device is unavailable.
Windows: --host asio selects the vendor ASIO driver; --host wasapi selects WASAPI.
Linux: --host alsa; use a hardware device name for direct ALSA, default/pipewire for PipeWire.
macOS: --host coreaudio.";

pub struct Options {
    pub host: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub duration: Duration,
    pub list: bool,
}

impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Self>, Box<dyn Error>> {
        let mut args = args.into_iter();
        let mut options = Self {
            host: None,
            input: None,
            output: None,
            duration: Duration::from_secs(300),
            list: false,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => {
                    println!("{HELP}");
                    return Ok(None);
                }
                "--list-devices" => options.list = true,
                "--host" | "--input" | "--output" | "--seconds" => {
                    let value = args
                        .next()
                        .filter(|value| !value.is_empty() && !value.starts_with("--"))
                        .ok_or_else(|| io::Error::other(format!("{arg} requires a value")))?;
                    match arg.as_str() {
                        "--host" => options.host = Some(value),
                        "--input" => options.input = Some(value),
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
}
