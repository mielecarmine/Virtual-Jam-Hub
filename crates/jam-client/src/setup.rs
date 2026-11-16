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
    pub output: Option<String>,
    pub buffer_frames: u32,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            nickname: "musician".into(),
            server: "127.0.0.1:7777".into(),
            host: None,
            input: None,
            output: None,
            buffer_frames: 128,
        }
    }
}

impl Preferences {
    pub fn endpoint(&self) -> io::Result<SocketAddr> {
        if self.nickname.trim().is_empty() || self.nickname.len() > jam_protocol::NICKNAME_CAPACITY
        {
            return Err(io::Error::other(
                "Il nickname deve contenere 1–32 byte UTF-8.",
            ));
        }
        if !BUFFER_SIZES.contains(&self.buffer_frames) {
            return Err(io::Error::other("Scegli un buffer di 64, 128 o 256 frame."));
        }
        let endpoint: SocketAddr = self.server.trim().parse().map_err(|_| {
            io::Error::other("Inserisci il server come IP:porta oppure [IPv6]:porta.")
        })?;
        if endpoint.port() == 0 || endpoint.ip().is_unspecified() {
            return Err(io::Error::other(
                "Specifica un IP di destinazione e una porta non zero.",
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
}

pub fn config_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("JAM_CLIENT_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    directories::ProjectDirs::from("", "", "virtual-jam")
        .map(|dirs| dirs.config_dir().join("client.toml"))
        .ok_or_else(|| io::Error::other("Directory di configurazione locale non disponibile."))
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub name: String,
    pub buffers: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub hosts: Vec<String>,
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

fn info(name: &str, configs: &[cpal::SupportedStreamConfigRange]) -> DeviceInfo {
    DeviceInfo {
        name: name.into(),
        buffers: BUFFER_SIZES
            .into_iter()
            .filter(|frames| {
                audio::select_config_for_buffer(configs.iter().copied(), *frames).is_some()
            })
            .collect(),
    }
}

impl Catalog {
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
                    catalog.inputs.push(info(&name, &configs));
                }
            }
            if let Ok(configs) = device.supported_output_configs() {
                let configs: Vec<_> = configs.collect();
                if !configs.is_empty() {
                    catalog.outputs.push(info(&name, &configs));
                }
            }
        }
        Ok(catalog)
    }

    pub fn buffers(&self, preferences: &Preferences) -> Vec<u32> {
        let input = preferences.input.as_ref().or(self.default_input.as_ref());
        let output = preferences.output.as_ref().or(self.default_output.as_ref());
        BUFFER_SIZES
            .into_iter()
            .filter(|frames| {
                self.inputs
                    .iter()
                    .any(|d| Some(&d.name) == input && d.buffers.contains(frames))
                    && self
                        .outputs
                        .iter()
                        .any(|d| Some(&d.name) == output && d.buffers.contains(frames))
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests;
