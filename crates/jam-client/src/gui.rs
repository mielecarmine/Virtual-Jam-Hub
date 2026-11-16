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
                Event::Mixer(view) => self.mixer = Some(view),
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

    fn mixer(&self, ui: &mut egui::Ui) {
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
                for peer in &view.peers {
                    ui.push_id(peer.peer_id, |ui| {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.set_width(110.0);
                            ui.vertical_centered(|ui| {
                                ui.strong(String::from_utf8_lossy(
                                    &peer.nickname[..usize::from(peer.nickname_len)],
                                ));
                                let control = &view.controls[usize::from(peer.peer_id) - 1];
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
                            });
                        });
                    });
                }
            });
        });
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
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Virtual Jam");
                ui.label("Collega il tuo strumento e raggiungi la band.");
                ui.small(
                    "Usa le cuffie. Il tuo ingresso viene inviato ai peer, \
                     senza monitoraggio locale.",
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
        ctx.request_repaint_after(Duration::from_millis(50));
    }
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
    fn fader_conversion_covers_unity_and_endpoints() {
        assert!((db_to_gain(-60.0) - 0.001).abs() < 0.000001);
        assert_eq!(db_to_gain(0.0), 1.0);
        assert!((db_to_gain(6.0) - 1.995_262).abs() < 0.000001);
        for db in [-60.0, -30.0, 0.0, 6.0] {
            assert!((gain_to_db(db_to_gain(db)) - db).abs() < 0.00001);
        }
    }
}
