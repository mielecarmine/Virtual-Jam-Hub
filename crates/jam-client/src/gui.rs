//! Native setup screen. Hardware discovery and session work run on the controller.

use std::{
    io,
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use eframe::egui;

use crate::{
    controller::{Command, Controller, Event, MixerView, Phase},
    setup::{self, Catalog, DeviceInfo, Preferences},
};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = setup::config_path()?;
    let (preferences, warning) = match Preferences::load(&path) {
        Ok(preferences) => (preferences, None),
        Err(error) => (
            Preferences::default(),
            Some(format!("Configuration not loaded: {error}")),
        ),
    };
    let controller = Controller::start(preferences.clone(), path.clone())?;
    let app = AppState {
        metronome: None,
        telemetry: crate::network::Telemetry::default(),
        meters: std::array::from_fn(|_| Meter::default()),
        mixer: None,
        preferences,
        controller,
        path,
        catalog: Catalog::default(),
        loading: true,
        phase: Phase::Disconnected,
        disconnecting: false,
        error: warning,
        save_error: None,
        saved: false,
        changed_at: None,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 660.0])
            .with_min_inner_size([480.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native("Virtual Jam", options, Box::new(move |_| Ok(Box::new(app))))
        .map_err(|error| io::Error::other(error.to_string()).into())
}

struct AppState {
    metronome: Option<jam_protocol::MetronomeState>,
    telemetry: crate::network::Telemetry,
    meters: [Meter; jam_protocol::MAX_PEERS + 2],
    mixer: Option<MixerView>,
    preferences: Preferences,
    controller: Controller,
    path: PathBuf,
    catalog: Catalog,
    loading: bool,
    phase: Phase,
    disconnecting: bool,
    error: Option<String>,
    save_error: Option<String>,
    saved: bool,
    changed_at: Option<Instant>,
}

impl AppState {
    fn command(&mut self, command: Command) -> bool {
        if self.controller.commands.send(command).is_err() {
            self.error = Some("The controller has stopped. Restart the application.".into());
            return false;
        }
        true
    }

    fn events(&mut self) {
        while let Ok(event) = self.controller.events.try_recv() {
            match event {
                Event::Metronome(state) => self.metronome = Some(state),
                Event::Telemetry(metrics) => self.telemetry = metrics,
                Event::Mixer(view) => {
                    for peer in &view.peers {
                        if self
                            .mixer
                            .as_ref()
                            .is_none_or(|old| !old.peers.contains(peer))
                        {
                            self.meters[usize::from(peer.peer_id)] = Meter::default();
                        }
                    }
                    self.mixer = Some(view);
                }
                Event::Catalog(result) => {
                    self.loading = false;
                    match result {
                        Ok(catalog) => self.catalog = catalog,
                        Err(error) => {
                            self.catalog = Catalog::default();
                            self.error = Some(error);
                        }
                    }
                }
                Event::Phase(phase) => {
                    if phase == Phase::Disconnected {
                        self.mixer = None;
                        self.metronome = None;
                        self.telemetry = crate::network::Telemetry::default();
                        self.meters = std::array::from_fn(|_| Meter::default());
                    }
                    self.phase = phase;
                    self.disconnecting = false;
                }
                Event::Error(error) => self.error = Some(error),
                Event::Saved(result) => {
                    self.saved = result.is_ok();
                    self.save_error = result.err();
                }
            }
        }
    }

    fn audio_devices(&mut self, ui: &mut egui::Ui) {
        device_combo(
            ui,
            "input",
            "Input",
            &mut self.preferences.input,
            &self.catalog.inputs,
        );
        let max_channels = self
            .catalog
            .selected_input(&self.preferences)
            .map_or(0, |device| device.max_channels);
        channel_combos(ui, &mut self.preferences, max_channels);
        if let Err(error) = self.catalog.validate_input(&self.preferences) {
            ui.colored_label(egui::Color32::YELLOW, error.to_string());
        }
        device_combo(
            ui,
            "output",
            "Output",
            &mut self.preferences.output,
            &self.catalog.outputs,
        );
        ui.label("Buffer (frames at 48 kHz)");
        let supported = self.catalog.buffers(&self.preferences);
        egui::ComboBox::from_id_salt("buffer")
            .width(350.0)
            .selected_text(format!("{} frames", self.preferences.buffer_frames))
            .show_ui(ui, |ui| {
                for frames in setup::BUFFER_SIZES {
                    let available = supported.contains(&frames);
                    ui.add_enabled_ui(available, |ui| {
                        ui.selectable_value(
                            &mut self.preferences.buffer_frames,
                            frames,
                            format!("{frames} frames · {:.2} ms", f64::from(frames) / 48.0),
                        );
                    });
                }
            });
        if !supported.contains(&self.preferences.buffer_frames) {
            ui.colored_label(
                egui::Color32::YELLOW,
                "Buffer unavailable for this device pair at 48 kHz.",
            );
        }
    }

    fn setup(&mut self, ui: &mut egui::Ui) {
        let previous = self.preferences.clone();
        let idle = self.phase == Phase::Disconnected && !self.disconnecting;
        ui.add_enabled_ui(idle, |ui| {
            ui.heading("Your Session");
            ui.label("Nickname");
            ui.add(
                egui::TextEdit::singleline(&mut self.preferences.nickname)
                    .desired_width(f32::INFINITY),
            );
            ui.label("Server address (IP:port)");
            ui.add(
                egui::TextEdit::singleline(&mut self.preferences.server)
                    .hint_text("127.0.0.1:7777 or [::1]:7777")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(16.0);
            ui.heading("Audio Hardware");
            ui.label("Backend");
            egui::ComboBox::from_id_salt("host")
                .width(350.0)
                .selected_text(self.preferences.host.as_deref().unwrap_or("System default"))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.preferences.host, None, "System default");
                    for host in &self.catalog.hosts {
                        ui.selectable_value(&mut self.preferences.host, Some(host.clone()), host);
                    }
                });
            ui.add_enabled_ui(!self.loading, |ui| self.audio_devices(ui));
            ui.small(
                "If the driver does not report supported buffer sizes, \
                 compatibility is checked when connecting.",
            );
            if ui
                .add_enabled(!self.loading, egui::Button::new("Refresh devices"))
                .clicked()
            {
                self.refresh();
            }
            telemetry_opt_in(ui, &mut self.preferences.share_telemetry);
        });
        if previous != self.preferences {
            self.saved = false;
            self.changed_at = Some(Instant::now());
            if previous.host != self.preferences.host {
                self.preferences.input = None;
                self.preferences.output = None;
                self.refresh();
            }
            if previous.host != self.preferences.host
                || previous.input != self.preferences.input
                || previous.output != self.preferences.output
                || previous.buffer_frames != self.preferences.buffer_frames
                || previous.instrument_channel != self.preferences.instrument_channel
                || previous.mic_channel != self.preferences.mic_channel
            {
                self.command(Command::SelectDevice(self.preferences.clone()));
                self.changed_at = None;
            }
        }
    }

    fn mixer(&mut self, ui: &mut egui::Ui) {
        if self.phase != Phase::Connected {
            return;
        }
        ui.separator();
        ui.heading("Your Headphone Mix");
        let Some(view) = &self.mixer else {
            ui.label("Waiting for musician list…");
            return;
        };
        if view.peers.is_empty() {
            ui.label("No other musicians in the room.");
        }
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal_top(|ui| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(110.0);
                    ui.vertical_centered(|ui| {
                        ui.strong("Local Instrument");
                        self.meters[0].show(ui, &view.stats.input_peak);
                        monitoring_controls(ui, &view.local_control);
                    });
                });
                if view.mic_enabled {
                    ui.push_id("local_mic", |ui| {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.set_width(110.0);
                            ui.vertical_centered(|ui| {
                                ui.strong("Local Microphone");
                                self.meters[jam_protocol::MAX_PEERS + 1]
                                    .show(ui, &view.stats.mic_peak);
                                monitoring_controls(ui, &view.local_mic_control);
                                let mut muted = view.stats.mic_tx_mute.load(Ordering::Relaxed);
                                if ui.toggle_value(&mut muted, "Mute Mic (Net)").changed() {
                                    view.stats.mic_tx_mute.store(muted, Ordering::Relaxed);
                                }
                            });
                        });
                    });
                }
                for peer in &view.peers {
                    ui.push_id(peer.peer_id, |ui| {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.set_width(110.0);
                            ui.vertical_centered(|ui| {
                                ui.strong(String::from_utf8_lossy(
                                    &peer.nickname[..usize::from(peer.nickname_len)],
                                ));
                                let control = &view.controls[usize::from(peer.peer_id) - 1];
                                self.meters[usize::from(peer.peer_id)].show(ui, &control.peak);
                                monitoring_controls(ui, control);
                            });
                        });
                    });
                }
            });
        });
    }

    fn metronome(&mut self, ui: &mut egui::Ui) {
        if self.phase != Phase::Connected {
            return;
        }
        ui.separator();
        ui.heading("Metronome & Tempo-Drag Prevention");
        let Some(state) = self.metronome else {
            ui.label("Waiting for room metronome state…");
            return;
        };
        let mut config = state.config;
        ui.add_enabled_ui(!self.disconnecting, |ui| {
            ui.checkbox(&mut config.enabled, "Room Metronome On/Off");
            ui.horizontal_wrapped(|ui| {
                ui.label("BPM");
                ui.add(egui::DragValue::new(&mut config.bpm).range(30..=300));
                egui::ComboBox::from_id_salt("metronome_meter")
                    .selected_text(meter_label(config.beats_per_bar))
                    .show_ui(ui, |ui| {
                        for beats in 1..=16 {
                            ui.selectable_value(
                                &mut config.beats_per_bar,
                                beats,
                                meter_label(beats),
                            );
                        }
                    });
            });
            ui.small("Beats per bar; BPM counts click pulses (including in 6/8).");
            if let Some(view) = &self.mixer {
                ui.label("Reference / Tempo Leader");
                egui::ComboBox::from_id_salt("tempo_leader")
                    .selected_text(leader_label(
                        config.leader_peer_id,
                        view,
                        &self.preferences.nickname,
                    ))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut config.leader_peer_id, 0, "Absolute Clock (Room)");
                        ui.selectable_value(
                            &mut config.leader_peer_id,
                            view.local_peer_id,
                            format!("{} (you)", self.preferences.nickname),
                        );
                        for peer in &view.peers {
                            ui.selectable_value(
                                &mut config.leader_peer_id,
                                peer.peer_id,
                                String::from_utf8_lossy(
                                    &peer.nickname[..usize::from(peer.nickname_len)],
                                ),
                            );
                        }
                    });
                click_monitoring(ui, &view.metronome, state.config);
            }
        });
        if config != state.config {
            self.command(Command::Metronome(config));
        }
        ui.small("Room controls update for all musicians once confirmed by the server.");
        if tempo_drag_hint(self.telemetry.latency, state.config.enabled) {
            ui.label("Latency above 25 ms: enable the Room Metronome or choose a Tempo Leader to prevent tempo-drag.");
        }
    }

    fn refresh(&mut self) {
        self.error = None;
        self.loading = self.command(Command::Refresh(self.preferences.clone()));
    }

    fn connection(&mut self, ui: &mut egui::Ui) {
        ui.add_space(16.0);
        ui.separator();
        let label = if self.disconnecting {
            "Disconnecting…"
        } else {
            match self.phase {
                Phase::Disconnected => "Disconnected",
                Phase::Connecting => "Connecting to server…",
                Phase::Connected => "Connected · UDP audio active",
            }
        };
        ui.strong(label);
        if self.phase == Phase::Disconnected && !self.disconnecting {
            let validation = self
                .preferences
                .endpoint()
                .and_then(|_| self.catalog.validate_input(&self.preferences));
            if let Err(error) = &validation {
                ui.label(error.to_string());
            }
            let compatible = self
                .catalog
                .buffers(&self.preferences)
                .contains(&self.preferences.buffer_frames);
            if ui
                .add_enabled(
                    !self.loading && compatible && validation.is_ok(),
                    egui::Button::new("Connect").min_size(egui::vec2(140.0, 36.0)),
                )
                .clicked()
            {
                self.error = None;
                if self.command(Command::Connect(self.preferences.clone())) {
                    self.phase = Phase::Connecting;
                    self.changed_at = None;
                }
            }
        } else if ui
            .add_enabled(
                !self.disconnecting,
                egui::Button::new("Disconnect").min_size(egui::vec2(140.0, 36.0)),
            )
            .clicked()
        {
            self.disconnecting = self.command(Command::Disconnect);
        }
    }
}

fn telemetry_opt_in(ui: &mut egui::Ui, share_telemetry: &mut bool) {
    ui.add_space(12.0);
    ui.checkbox(
        share_telemetry,
        "Share anonymous end-of-session technical report (beta)",
    );
    ui.small(
        "Opt-in: sends only version, OS, audio backend, buffer size and RTT/jitter/loss statistics \
         to the beta server (185.198.245.40) on disconnect. No audio or personal data.",
    );
}

fn meter_label(beats: u8) -> String {
    match beats {
        2..=4 => format!("{beats}/4"),
        6 => "6/8".into(),
        _ => format!("{beats} beats"),
    }
}

fn leader_label(id: u8, view: &MixerView, nickname: &str) -> String {
    if id == 0 {
        "Absolute Clock (Room)".into()
    } else if id == view.local_peer_id {
        format!("{nickname} (you)")
    } else {
        view.peers
            .iter()
            .find(|peer| peer.peer_id == id)
            .map_or_else(
                || format!("Peer {id} (updating room…)"),
                |peer| {
                    String::from_utf8_lossy(&peer.nickname[..usize::from(peer.nickname_len)])
                        .into_owned()
                },
            )
    }
}

fn tempo_drag_hint(
    budget: Option<crate::network::telemetry::LatencyBudget>,
    enabled: bool,
) -> bool {
    !enabled
        && budget
            .and_then(|value| value.total_one_way_ms)
            .is_some_and(|ms| ms > 25.0)
}

fn click_monitoring(
    ui: &mut egui::Ui,
    controls: &crate::mixer::MetronomeControls,
    config: jam_protocol::MetronomeConfig,
) {
    let beat = controls.current_beat.load(Ordering::Relaxed);
    ui.horizontal_wrapped(|ui| {
        for index in 0..u32::from(config.beats_per_bar) {
            let active = config.enabled && index == beat;
            let color = if active {
                egui::Color32::LIGHT_GREEN
            } else {
                egui::Color32::GRAY
            };
            let text = if index == 0 { "◆" } else { "●" };
            ui.colored_label(color, text)
                .on_hover_text(format!("Beat {}", index + 1));
        }
    });
    let mut db = gain_to_db(f32::from_bits(controls.gain.load(Ordering::Relaxed)));
    if ui
        .add(
            egui::Slider::new(&mut db, -60.0..=6.0)
                .text("Local click volume")
                .suffix(" dB"),
        )
        .changed()
    {
        controls
            .gain
            .store(db_to_gain(db).to_bits(), Ordering::Relaxed);
    }
    let mut mute = controls.mute.load(Ordering::Relaxed);
    if ui.toggle_value(&mut mute, "Mute Click · local").changed() {
        controls.mute.store(mute, Ordering::Relaxed);
    }
    let delay_ms = f64::from(controls.phase_delay_samples.load(Ordering::Relaxed)) * 1000.0
        / f64::from(jam_protocol::SAMPLE_RATE);
    ui.small(format!("Tempo Leader click delay: ~{delay_ms:.1} ms")).on_hover_text(
        "Estimate: P2P RTT / 2 or full SFU RTT + leader jitter target + SPSC reserve + codec + local audio I/O. Remote hardware is estimated using local hardware.",
    );
}

fn channel_combos(ui: &mut egui::Ui, preferences: &mut Preferences, max_channels: u16) {
    let label = |channel: u16| {
        if channel == 0 || channel > max_channels {
            format!("Channel {channel} (unavailable)")
        } else {
            format!("Channel {channel}")
        }
    };
    ui.label("Instrument channel");
    egui::ComboBox::from_id_salt("instrument_channel")
        .width(350.0)
        .selected_text(label(preferences.instrument_channel))
        .show_ui(ui, |ui| {
            for channel in 1..=max_channels {
                ui.add_enabled_ui(preferences.mic_channel != Some(channel), |ui| {
                    ui.selectable_value(
                        &mut preferences.instrument_channel,
                        channel,
                        label(channel),
                    );
                });
            }
        });
    ui.label("Microphone channel");
    egui::ComboBox::from_id_salt("mic_channel")
        .width(350.0)
        .selected_text(
            preferences
                .mic_channel
                .map_or_else(|| "Disabled".into(), label),
        )
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut preferences.mic_channel, None, "Disabled");
            for channel in 1..=max_channels {
                ui.add_enabled_ui(preferences.instrument_channel != channel, |ui| {
                    ui.selectable_value(
                        &mut preferences.mic_channel,
                        Some(channel),
                        label(channel),
                    );
                });
            }
        });
}

fn device_combo(
    ui: &mut egui::Ui,
    id: &str,
    label: &str,
    selected: &mut Option<String>,
    devices: &[DeviceInfo],
) {
    ui.label(label);
    let missing = selected
        .as_ref()
        .is_some_and(|name| !devices.iter().any(|d| &d.name == name));
    let text = selected.as_deref().unwrap_or("System default");
    egui::ComboBox::from_id_salt(id)
        .width(350.0)
        .selected_text(if missing {
            format!("{text} (unavailable)")
        } else {
            text.into()
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(selected, None, "System default");
            for device in devices {
                ui.selectable_value(selected, Some(device.name.clone()), &device.name);
            }
        });
}

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.events();
        egui::TopBottomPanel::top("telemetry").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                let rtt = self.telemetry.rtt_ms.map_or_else(|| "—".into(), |ms| format!("{ms:.1}"));
                ui.label(format!("Ping RTT: {rtt} ms"));
                if self.phase == Phase::Connected {
                    if let Some(mixer) = &self.mixer {
                        for peer in &mixer.peers {
                            let route = if self.telemetry.direct_peers_mask & (1 << (peer.peer_id - 1)) != 0 { "P2P" } else { "SFU" };
                            ui.label(format!("Peer {}: {route}", peer.peer_id));
                        }
                    }
                    ui.label(format!("Jitter: {:.1} ms · Packet Loss: {:.1}% · Jitter Buffer: {:.1} ms",
                        self.telemetry.jitter_ms, self.telemetry.loss_percent, self.telemetry.buffer_ms))
                        .on_hover_text("Receive: maximum jitter across peers; cumulative packet loss after a 64-packet reorder window; buffer = maximum encoded audio currently queued, excluding hardware and SPSC buffers.");
                } else { ui.label("Jitter: — · Packet Loss: — · Jitter Buffer: —"); }
                latency_indicator(ui, if self.phase == Phase::Connected { self.telemetry.latency } else { None });
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Virtual Jam");
                ui.label("Connect your instrument and join the band.");
                ui.small(
                    "Use headphones. Gain, Mute and Solo adjust your headphone mix. \
                     Mute Mic (Net) removes the microphone from the signal sent to the band.",
                );
                ui.add_space(16.0);
                self.setup(ui);
                if self.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Scanning devices…");
                    });
                }
                self.connection(ui);
                self.mixer(ui);
                self.metronome(ui);
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                if let Some(error) = &self.save_error {
                    ui.colored_label(
                        egui::Color32::LIGHT_RED,
                        format!("Preferences not saved: {error}"),
                    );
                } else if self.saved {
                    ui.small("Preferences saved.");
                }
                ui.add_space(10.0);
                ui.small(format!("Configuration: {}", self.path.display()));
            });
        });
        if self
            .changed_at
            .is_some_and(|at| at.elapsed() >= Duration::from_millis(500))
        {
            self.command(Command::Save(self.preferences.clone()));
            self.changed_at = None;
        }
        ctx.request_repaint_after(Duration::from_secs_f64(1.0 / 60.0));
    }
}

fn latency_indicator(ui: &mut egui::Ui, budget: Option<crate::network::telemetry::LatencyBudget>) {
    use crate::network::telemetry::LatencyClass;
    let Some(budget) = budget else {
        ui.label("E2E Latency: —");
        return;
    };
    let color = match budget.class {
        Some(LatencyClass::Optimal) => egui::Color32::LIGHT_GREEN,
        Some(LatencyClass::Limit) => egui::Color32::YELLOW,
        Some(LatencyClass::TempoDrag) => egui::Color32::LIGHT_RED,
        None => egui::Color32::GRAY,
    };
    let text = budget.total_one_way_ms.map_or_else(
        || "E2E Latency: — (RTT unavailable)".into(),
        |ms| {
            format!(
                "E2E Latency: ~{ms:.1} ms · {}",
                budget.class.map_or("—", LatencyClass::label)
            )
        },
    );
    ui.colored_label(color, text).on_hover_ui(|ui| {
        ui.label(format!("Audio I/O: {:.1} ms (requested capture + last output block; falls back to the requested buffer)", budget.audio_io_ms));
        ui.label(format!("Codec: {:.1} ms (Opus frame + encoder lookahead)", budget.codec_ms));
        ui.label(budget.network_one_way_ms.map_or_else(
            || "Network: — (RTT unavailable)".into(),
            |ms| format!("Network: {ms:.1} ms (route RTT / 2)"),
        ));
        ui.label(format!("Jitter/RX Buffer: {:.1} ms (maximum across peers + mixer SPSC reserve)", budget.rx_buffer_ms));
        ui.label(budget.headroom_ms.map_or_else(
            || "Headroom relative to 25 ms: —".into(),
            |ms| format!("Headroom relative to 25 ms: {ms:+.1} ms"),
        ));
        ui.label("Optimal <20 ms · Borderline 20–25 ms · Tempo-Drag Risk >25 ms");
        ui.label("One-way estimate using local audio I/O and the slowest route (P2P/SFU): remote hardware is not measured.");
        if budget.class == Some(LatencyClass::TempoDrag) {
            ui.label("A synchronized metronome or a Tempo Leader is recommended as a shared rhythmic reference.");
        }
    });
}

impl Drop for AppState {
    fn drop(&mut self) {
        let pending = self.changed_at.map(|_| self.preferences.clone());
        self.controller.shutdown(pending);
    }
}

fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn gain_to_db(gain: f32) -> f32 {
    (20.0 * gain.log10()).clamp(-60.0, 6.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempo_drag_suggestion_requires_known_latency_above_25_and_room_click_off() {
        use crate::network::telemetry::LatencyBudget;
        assert!(!tempo_drag_hint(None, false));
        for (rtt, expected) in [(35.0, false), (35.002, true)] {
            let budget = Some(LatencyBudget::estimate(0, 0, 0, Some(rtt), 0.0));
            assert_eq!(tempo_drag_hint(budget, false), expected);
            assert!(!tempo_drag_hint(budget, true));
        }
        assert!(!tempo_drag_hint(
            Some(LatencyBudget::estimate(128, 128, 120, None, 7.5)),
            false
        ));
    }

    #[test]
    fn meter_attack_decay_and_peak_hold_follow_elapsed_time() {
        let mut meter = Meter::default();
        meter.advance(1.0, 0.0);
        meter.advance(0.1, 0.5);
        assert_eq!(meter.held, 1.0);
        assert!((meter.level - 0.1).abs() < 0.001);
        meter.advance(0.0, 1.5);
        assert!(meter.held < 0.2);
        assert!(meter.level < 0.001);
        meter.advance(1.2, 1.6);
        assert_eq!(meter.held, 1.2);
    }

    #[test]
    fn fader_conversion_covers_unity_and_endpoints() {
        assert!((db_to_gain(-60.0) - 0.001).abs() < 0.000001);
        assert_eq!(db_to_gain(0.0), 1.0);
        assert!((db_to_gain(6.0) - 1.995_262).abs() < 0.000001);
        for db in [-60.0, -30.0, 0.0, 6.0] {
            assert!((gain_to_db(db_to_gain(db)) - db).abs() < 0.00001);
        }
    }
}

#[derive(Default)]
struct Meter {
    level: f32,
    held: f32,
    hold_until: f64,
    last_time: Option<f64>,
}

impl Meter {
    fn advance(&mut self, peak: f32, now: f64) {
        #[allow(clippy::cast_possible_truncation)]
        let elapsed = self.last_time.map_or(0.0, |last| (now - last).max(0.0)) as f32;
        self.last_time = Some(now);
        self.level = peak.max(self.level * (-elapsed * 8.0).exp());
        if peak >= self.held {
            self.held = peak;
            self.hold_until = now + 1.0;
        } else if now > self.hold_until {
            self.held = self.level.max(self.held * (-elapsed * 2.0).exp());
        }
    }

    fn show(&mut self, ui: &mut egui::Ui, peak: &std::sync::atomic::AtomicU32) {
        let value = f32::from_bits(peak.swap(0, Ordering::Relaxed));
        self.advance(value, ui.input(|input| input.time));
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(18.0, 100.0), egui::Sense::hover());
        response.on_hover_text("Pre-fader, pre-mute peak · dBFS · 1 s peak hold");
        let painter = ui.painter();
        painter.rect_filled(rect, 2.0, egui::Color32::from_gray(25));
        let height =
            |value: f32| ((20.0 * value.max(0.000_001).log10() + 60.0) / 60.0).clamp(0.0, 1.0);
        let level = height(self.level);
        for (low, high, color) in [
            (0.0, 0.8, egui::Color32::GREEN),
            (0.8, 0.95, egui::Color32::YELLOW),
            (0.95, 1.0, egui::Color32::RED),
        ] {
            if level > low {
                let area = egui::Rect::from_min_max(
                    egui::pos2(rect.left(), rect.bottom() - rect.height() * level.min(high)),
                    egui::pos2(rect.right(), rect.bottom() - rect.height() * low),
                );
                painter.rect_filled(area, 0.0, color);
            }
        }
        let y = rect.bottom() - rect.height() * height(self.held);
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(
                2.0_f32,
                if self.held >= 1.0 {
                    egui::Color32::RED
                } else {
                    egui::Color32::WHITE
                },
            ),
        );
    }
}

fn monitoring_controls(ui: &mut egui::Ui, control: &crate::mixer::Controls) {
    let gain = f32::from_bits(control.gain.load(Ordering::Relaxed));
    let mut db = gain_to_db(gain);
    if ui
        .add(
            egui::Slider::new(&mut db, -60.0..=6.0)
                .vertical()
                .suffix(" dB"),
        )
        .changed()
    {
        control
            .gain
            .store(db_to_gain(db).to_bits(), Ordering::Relaxed);
    }
    let mut mute = control.mute.load(Ordering::Relaxed);
    let mut solo = control.solo.load(Ordering::Relaxed);
    if ui.toggle_value(&mut mute, "M · Mute").changed() {
        control.mute.store(mute, Ordering::Relaxed);
    }
    if ui.toggle_value(&mut solo, "S · Solo").changed() {
        control.solo.store(solo, Ordering::Relaxed);
    }
}
