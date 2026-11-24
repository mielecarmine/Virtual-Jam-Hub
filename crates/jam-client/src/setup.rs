//! Persisted preferences and hardware discovery. Never called from an audio callback.

use std::{
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};

use crate::{audio, cli};

pub const BUFFER_SIZES: [u32; 3] = [64, 128, 256];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub nickname: String,
    pub server: String,
    pub host: Option<String>,
    pub input: Option<String>,
    pub instrument_channel: u16,
    pub mic_channel: Option<u16>,
    pub output: Option<String>,
    pub buffer_frames: u32,
    pub share_telemetry: bool,
    pub telemetry_endpoint: Option<String>,
    pub telemetry_token: Option<String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            nickname: "musician".into(),
            server: "127.0.0.1:7777".into(),
            host: None,
            input: None,
            instrument_channel: 1,
            mic_channel: None,
            output: None,
            buffer_frames: 128,
            share_telemetry: false,
            telemetry_endpoint: None,
            telemetry_token: None,
        }
    }
}

impl Preferences {
    pub fn input_routing(&self) -> io::Result<audio::InputRouting> {
        audio::InputRouting::new(self.instrument_channel, self.mic_channel)
            .map_err(io::Error::other)
    }

    pub fn endpoint(&self) -> io::Result<SocketAddr> {
        self.input_routing()?;
        if self.nickname.trim().is_empty() || self.nickname.len() > jam_protocol::NICKNAME_CAPACITY
        {
            return Err(io::Error::other("Nickname must contain 1–32 UTF-8 bytes."));
        }
        if !BUFFER_SIZES.contains(&self.buffer_frames) {
            return Err(io::Error::other(
                "Choose a buffer of 64, 128 or 256 frames.",
            ));
        }
        let endpoint: SocketAddr = self
            .server
            .trim()
            .parse()
            .map_err(|_| io::Error::other("Enter the server as IP:port or [IPv6]:port."))?;
        if endpoint.port() == 0 || endpoint.ip().is_unspecified() {
            return Err(io::Error::other(
                "Specify a destination IP address and a nonzero port.",
            ));
        }
        Ok(endpoint)
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temporary, text)?;
        if let Err(error) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(temporary);
            return Err(error);
        }
        Ok(())
    }

    #[must_use]
    pub fn report_target(&self) -> Option<crate::session_report::ReportTarget> {
        if !self.share_telemetry {
            return None;
        }
        let endpoint = self
            .telemetry_endpoint
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                std::env::var("JAM_TELEMETRY_ENDPOINT")
                    .ok()
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_else(|| crate::session_report::DEFAULT_BETA_CLIENT_ENDPOINT.to_owned());
        let beta_token = self
            .telemetry_token
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .or_else(|| {
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
}

pub fn config_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("JAM_CLIENT_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    directories::ProjectDirs::from("", "", "virtual-jam")
        .map(|dirs| dirs.config_dir().join("client.toml"))
        .ok_or_else(|| io::Error::other("Local configuration directory unavailable."))
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub name: String,
    pub buffers: Vec<u32>,
    pub max_channels: u16,
    /// Maximum supported channels for each entry in `BUFFER_SIZES`, at 48 kHz.
    pub channels_by_buffer: [u16; 3],
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub hosts: Vec<String>,
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

#[derive(Clone, Copy)]
enum Direction {
    Input,
    Output,
}

fn info(
    name: &str,
    configs: &[cpal::SupportedStreamConfigRange],
    direction: Direction,
) -> DeviceInfo {
    DeviceInfo {
        name: name.into(),
        max_channels: configs
            .iter()
            .filter(|c| {
                c.min_sample_rate().0 <= audio::SAMPLE_RATE
                    && c.max_sample_rate().0 >= audio::SAMPLE_RATE
            })
            .map(cpal::SupportedStreamConfigRange::channels)
            .max()
            .unwrap_or(0),
        channels_by_buffer: BUFFER_SIZES.map(|frames| {
            configs
                .iter()
                .filter_map(|config| {
                    audio::select_input_config_for_buffer(std::iter::once(*config), frames)
                        .map(|selected| selected.stream.channels)
                })
                .max()
                .unwrap_or(0)
        }),
        buffers: BUFFER_SIZES
            .into_iter()
            .filter(|frames| {
                match direction {
                    Direction::Input => {
                        audio::select_input_config_for_buffer(configs.iter().copied(), *frames)
                    }
                    Direction::Output => {
                        audio::select_output_config_for_buffer(configs.iter().copied(), *frames)
                    }
                }
                .is_some()
            })
            .collect(),
    }
}

impl Catalog {
    pub fn selected_input(&self, preferences: &Preferences) -> Option<&DeviceInfo> {
        let name = preferences.input.as_ref().or(self.default_input.as_ref());
        self.inputs.iter().find(|device| Some(&device.name) == name)
    }

    pub fn validate_input(&self, preferences: &Preferences) -> io::Result<()> {
        let routing = preferences.input_routing()?;
        let input = self
            .selected_input(preferences)
            .ok_or_else(|| io::Error::other("Input device unavailable."))?;
        if routing.required_channels() > input.max_channels {
            return Err(io::Error::other(
                "Selected channels unavailable on this input at 48 kHz.",
            ));
        }
        Ok(())
    }

    pub fn discover(host_name: Option<&str>) -> io::Result<Self> {
        let host = cli::host(host_name).map_err(|error| io::Error::other(error.to_string()))?;
        let mut catalog = Self {
            hosts: cpal::available_hosts()
                .iter()
                .map(|id| id.name().into())
                .collect(),
            default_input: host.default_input_device().and_then(|d| d.name().ok()),
            default_output: host.default_output_device().and_then(|d| d.name().ok()),
            ..Self::default()
        };
        // Release each device before opening the next exclusive ALSA/ASIO handle.
        for device in host.devices().map_err(io::Error::other)? {
            let Ok(name) = device.name() else { continue };
            if let Ok(configs) = device.supported_input_configs() {
                let configs: Vec<_> = configs.collect();
                if !configs.is_empty() {
                    catalog.inputs.push(info(&name, &configs, Direction::Input));
                }
            }
            if let Ok(configs) = device.supported_output_configs() {
                let configs: Vec<_> = configs.collect();
                if !configs.is_empty() {
                    catalog
                        .outputs
                        .push(info(&name, &configs, Direction::Output));
                }
            }
        }
        Ok(catalog)
    }

    pub fn buffers(&self, preferences: &Preferences) -> Vec<u32> {
        let Ok(routing) = preferences.input_routing() else {
            return Vec::new();
        };
        let input = preferences.input.as_ref().or(self.default_input.as_ref());
        let output = preferences.output.as_ref().or(self.default_output.as_ref());
        BUFFER_SIZES
            .into_iter()
            .filter(|frames| {
                self.inputs.iter().any(|d| {
                    Some(&d.name) == input
                        && d.buffers.contains(frames)
                        && BUFFER_SIZES
                            .iter()
                            .position(|f| f == frames)
                            .is_some_and(|i| d.channels_by_buffer[i] >= routing.required_channels())
                }) && self
                    .outputs
                    .iter()
                    .any(|d| Some(&d.name) == output && d.buffers.contains(frames))
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests;
