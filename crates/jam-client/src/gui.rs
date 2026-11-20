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
            Some(format!("Configurazione non caricata: {error}")),
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
    meters: [Meter; jam_protocol::MAX_PEERS + 1],
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
            self.error = Some("Il controller si è arrestato. Riavvia l’applicazione.".into());
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

    fn setup(&mut self, ui: &mut egui::Ui) {
        let previous = self.preferences.clone();
        let idle = self.phase == Phase::Disconnected && !self.disconnecting;
        ui.add_enabled_ui(idle, |ui| {
            ui.heading("La tua sessione");
            ui.label("Nickname");
            ui.add(
                egui::TextEdit::singleline(&mut self.preferences.nickname)
                    .desired_width(f32::INFINITY),
            );
            ui.label("Indirizzo server (IP:porta)");
            ui.add(
                egui::TextEdit::singleline(&mut self.preferences.server)
                    .hint_text("127.0.0.1:7777 oppure [::1]:7777")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(16.0);
            ui.heading("Hardware audio");
            ui.label("Backend");
            egui::ComboBox::from_id_salt("host")
                .width(350.0)
                .selected_text(
                    self.preferences
                        .host
                        .as_deref()
                        .unwrap_or("Predefinito di sistema"),
                )
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.preferences.host, None, "Predefinito di sistema");
                    for host in &self.catalog.hosts {
                        ui.selectable_value(&mut self.preferences.host, Some(host.clone()), host);
                    }
                });
            ui.add_enabled_ui(!self.loading, |ui| {
                device_combo(
                    ui,
                    "input",
                    "Ingresso",
                    &mut self.preferences.input,
                    &self.catalog.inputs,
                );
                device_combo(
                    ui,
                    "output",
                    "Uscita",
                    &mut self.preferences.output,
                    &self.catalog.outputs,
                );
                ui.label("Buffer (frame a 48 kHz)");
                let supported = self.catalog.buffers(&self.preferences);
                egui::ComboBox::from_id_salt("buffer")
                    .width(350.0)
                    .selected_text(format!("{} frame", self.preferences.buffer_frames))
                    .show_ui(ui, |ui| {
                        for frames in setup::BUFFER_SIZES {
                            let available = supported.contains(&frames);
                            ui.add_enabled_ui(available, |ui| {
                                ui.selectable_value(
                                    &mut self.preferences.buffer_frames,
                                    frames,
                                    format!("{frames} frame · {:.2} ms", f64::from(frames) / 48.0),
                                );
                            });
                        }
                    });
                if !supported.contains(&self.preferences.buffer_frames) {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        "Buffer non disponibile per questa coppia di dispositivi a 48 kHz.",
                    );
                }
            });
            ui.small(
                "Se il driver non dichiara i buffer supportati, \
                 la verifica avviene alla connessione.",
            );
            if ui
                .add_enabled(!self.loading, egui::Button::new("Aggiorna dispositivi"))
                .clicked()
            {
                self.refresh();
            }
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
        ui.heading("Il tuo mix in cuffia");
        let Some(view) = &self.mixer else {
            ui.label("In attesa della lista dei musicisti…");
            return;
        };
        if view.peers.is_empty() {
            ui.label("Nessun altro musicista nella stanza.");
        }
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal_top(|ui| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(110.0);
                    ui.vertical_centered(|ui| {
                        ui.strong("Ingresso locale");
                        self.meters[0].show(ui, &view.stats.input_peak);
                        monitoring_controls(ui, &view.local_control);
                    });
                });
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
        ui.heading("Metronomo & Prevenzione Tempo-Drag");
        let Some(state) = self.metronome else {
            ui.label("In attesa dello stato del metronomo di stanza…");
            return;
        };
        let mut config = state.config;
        ui.add_enabled_ui(!self.disconnecting, |ui| {
            ui.checkbox(&mut config.enabled, "Metronomo On/Off · stanza");
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
            ui.small("Battiti per misura; i BPM contano gli impulsi del click (anche in 6/8).");
            if let Some(view) = &self.mixer {
                ui.label("Riferimento / Tempo Leader");
                egui::ComboBox::from_id_salt("tempo_leader")
                    .selected_text(leader_label(
                        config.leader_peer_id,
                        view,
                        &self.preferences.nickname,
                    ))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut config.leader_peer_id,
                            0,
                            "Clock Assoluto (Stanza)",
                        );
                        ui.selectable_value(
                            &mut config.leader_peer_id,
                            view.local_peer_id,
                            format!("{} (tu)", self.preferences.nickname),
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
        ui.small(
            "I controlli di stanza si aggiornano alla conferma del server per tutti i musicisti.",
        );
        if tempo_drag_hint(self.telemetry.latency, state.config.enabled) {
            ui.label("Latenza oltre 25 ms: attiva il Metronomo di stanza o scegli un Tempo Leader per evitare il tempo-drag.");
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
            "Disconnessione…"
        } else {
            match self.phase {
                Phase::Disconnected => "Disconnesso",
                Phase::Connecting => "Connessione al server…",
                Phase::Connected => "Connesso · audio UDP attivo",
            }
        };
        ui.strong(label);
        if self.phase == Phase::Disconnected && !self.disconnecting {
            let validation = self.preferences.endpoint();
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
                    egui::Button::new("Connetti").min_size(egui::vec2(140.0, 36.0)),
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
                egui::Button::new("Disconnetti").min_size(egui::vec2(140.0, 36.0)),
            )
            .clicked()
        {
            self.disconnecting = self.command(Command::Disconnect);
        }
    }
}

fn meter_label(beats: u8) -> String {
    match beats {
        2..=4 => format!("{beats}/4"),
        6 => "6/8".into(),
        _ => format!("{beats} battiti"),
    }
}

fn leader_label(id: u8, view: &MixerView, nickname: &str) -> String {
    if id == 0 {
        "Clock Assoluto (Stanza)".into()
    } else if id == view.local_peer_id {
        format!("{nickname} (tu)")
    } else {
        view.peers
            .iter()
            .find(|peer| peer.peer_id == id)
            .map_or_else(
                || format!("Peer {id} (aggiornamento stanza…)"),
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
                .on_hover_text(format!("Battito {}", index + 1));
        }
    });
    let mut db = gain_to_db(f32::from_bits(controls.gain.load(Ordering::Relaxed)));
    if ui
        .add(
            egui::Slider::new(&mut db, -60.0..=6.0)
                .text("Volume click locale")
                .suffix(" dB"),
        )
        .changed()
    {
        controls
            .gain
            .store(db_to_gain(db).to_bits(), Ordering::Relaxed);
    }
    let mut mute = controls.mute.load(Ordering::Relaxed);
    if ui.toggle_value(&mut mute, "Mute Click · locale").changed() {
        controls.mute.store(mute, Ordering::Relaxed);
    }
    let delay_ms = f64::from(controls.phase_delay_samples.load(Ordering::Relaxed)) * 1000.0
        / f64::from(jam_protocol::SAMPLE_RATE);
    ui.small(format!("Ritardo click Tempo Leader: ~{delay_ms:.1} ms")).on_hover_text(
        "Stima: RTT server / 2 + coda jitter del solo Leader + riserva SPSC del mixer. Il protocollo non misura l’uplink e l’hardware del Leader.",
    );
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
    let text = selected.as_deref().unwrap_or("Predefinito di sistema");
    egui::ComboBox::from_id_salt(id)
        .width(350.0)
        .selected_text(if missing {
            format!("{text} (non disponibile)")
        } else {
            text.into()
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(selected, None, "Predefinito di sistema");
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
                    ui.label(format!("Jitter: {:.1} ms · Packet Loss: {:.1}% · Jitter Buffer: {:.1} ms",
                        self.telemetry.jitter_ms, self.telemetry.loss_percent, self.telemetry.buffer_ms))
                        .on_hover_text("Ricezione: jitter massimo tra peer; perdita cumulativa dopo una finestra di riordino di 64 pacchetti; buffer = massimo audio codificato attualmente in coda, esclusi buffer hardware e SPSC.");
                } else { ui.label("Jitter: — · Packet Loss: — · Jitter Buffer: —"); }
                latency_indicator(ui, if self.phase == Phase::Connected { self.telemetry.latency } else { None });
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Virtual Jam");
                ui.label("Collega il tuo strumento e raggiungi la band.");
                ui.small(
                    "Usa le cuffie. I controlli di Ingresso locale regolano solo il tuo ascolto; \
                     il segnale inviato ai peer resta invariato.",
                );
                ui.add_space(16.0);
                self.setup(ui);
                if self.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Ricerca dispositivi…");
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
                        format!("Preferenze non salvate: {error}"),
                    );
                } else if self.saved {
                    ui.small("Preferenze salvate.");
                }
                ui.add_space(10.0);
                ui.small(format!("Configurazione: {}", self.path.display()));
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
        ui.label("Latenza E2E: —");
        return;
    };
    let color = match budget.class {
        Some(LatencyClass::Optimal) => egui::Color32::LIGHT_GREEN,
        Some(LatencyClass::Limit) => egui::Color32::YELLOW,
        Some(LatencyClass::TempoDrag) => egui::Color32::LIGHT_RED,
        None => egui::Color32::GRAY,
    };
    let text = budget.total_one_way_ms.map_or_else(
        || "Latenza E2E: — (RTT non disponibile)".into(),
        |ms| {
            format!(
                "Latenza E2E: ~{ms:.1} ms · {}",
                budget.class.map_or("—", LatencyClass::label)
            )
        },
    );
    ui.colored_label(color, text).on_hover_ui(|ui| {
        ui.label(format!("Audio I/O: {:.1} ms (cattura richiesta + ultimo blocco output; fallback al buffer richiesto)", budget.audio_io_ms));
        ui.label(format!("Codec: {:.1} ms (frame Opus + lookahead encoder)", budget.codec_ms));
        ui.label(budget.network_one_way_ms.map_or_else(
            || "Rete: — (RTT non disponibile)".into(),
            |ms| format!("Rete: {ms:.1} ms (RTT server / 2)"),
        ));
        ui.label(format!("Jitter/RX Buffer: {:.1} ms (massimo tra peer + riserva SPSC mixer)", budget.rx_buffer_ms));
        ui.label(budget.headroom_ms.map_or_else(
            || "Margine residuo: —".into(),
            |ms| format!("Margine residuo rispetto a 25 ms: {ms:+.1} ms"),
        ));
        ui.label("Ottimale <20 ms · Limite 20–25 ms · Rischio Tempo-Drag >25 ms");
        ui.label("Stima one-way con audio I/O locale e RTT server: non misura il percorso audio completo tra due musicisti né l'hardware remoto.");
        if budget.class == Some(LatencyClass::TempoDrag) {
            ui.label("Consigliato un metronomo sincronizzato o un Tempo Leader come riferimento ritmico condiviso.");
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
        response.on_hover_text("Picco prima di fader e mute · dBFS · peak hold 1 s");
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
