//! Non-RT controller owns CPAL streams and UDP; only audio rings cross into callbacks.

use std::{
    io,
    path::PathBuf,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use cpal::traits::{DeviceTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};
use rtrb::{Consumer, RingBuffer};

use crate::{
    audio::{self, Stats},
    cli, mixer,
    network::Network,
    setup::{Catalog, Preferences},
    streams,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Disconnected,
    Connecting,
    Connected,
}

pub enum Command {
    Connect(Preferences),
    Disconnect,
    SelectDevice(Preferences),
    Refresh(Preferences),
    Save(Preferences),
    Shutdown(Option<Preferences>),
}

pub struct MixerView {
    pub stats: Arc<Stats>,
    pub local_control: Arc<mixer::Controls>,
    pub peers: Vec<jam_protocol::PeerInfo>,
    pub controls: Arc<[mixer::Controls; jam_protocol::MAX_PEERS]>,
}

pub enum Event {
    Mixer(MixerView),
    Telemetry(crate::network::Telemetry),
    Catalog(Result<Catalog, String>),
    Phase(Phase),
    Error(String),
    Saved(Result<(), String>),
}

pub struct Controller {
    pub commands: Sender<Command>,
    pub events: Receiver<Event>,
    worker: Option<JoinHandle<()>>,
}

impl Controller {
    pub fn start(preferences: Preferences, path: PathBuf) -> io::Result<Self> {
        let (commands, receiver) = crossbeam_channel::unbounded();
        let (sender, events) = crossbeam_channel::unbounded();
        let worker = thread::Builder::new()
            .name("jam-controller".into())
            .spawn(move || {
                control_loop(
                    &receiver,
                    &sender,
                    preferences,
                    &path,
                    Session::start,
                    |p| Catalog::discover(p.host.as_deref()),
                );
            })?;
        Ok(Self {
            commands,
            events,
            worker: Some(worker),
        })
    }

    pub fn shutdown(&mut self, preferences: Option<Preferences>) {
        let _ = self.commands.send(Command::Shutdown(preferences));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// A session is created and destroyed on its owning thread (CPAL streams can be !Send).
struct Session {
    input: Option<cpal::Stream>,
    output: Option<cpal::Stream>,
    mic: Consumer<f32>,
    rx: mixer::RxChannels,
    stats: Arc<Stats>,
    network: Network,
}

impl Session {
    fn start(preferences: &Preferences) -> io::Result<Self> {
        let endpoint = preferences.endpoint()?;
        let build = || -> Result<Self, Box<dyn std::error::Error>> {
            let host = cli::host(preferences.host.as_deref())?;
            let (input, output) = cli::devices(
                &host,
                preferences.input.as_deref(),
                preferences.output.as_deref(),
            )?;
            let frames = preferences.buffer_frames;
            let input_config =
                audio::select_config_for_buffer(input.supported_input_configs()?, frames)
                    .ok_or_else(|| {
                        io::Error::other("Input incompatibile con 48 kHz e il buffer scelto.")
                    })?;
            let output_config =
                audio::select_config_for_buffer(output.supported_output_configs()?, frames)
                    .ok_or_else(|| {
                        io::Error::other("Output incompatibile con 48 kHz e il buffer scelto.")
                    })?;
            let stats = Arc::new(Stats::default());
            let (producer, mic) = RingBuffer::new(audio::RING_CAPACITY);
            let (rx, mut mixer) = mixer::channels();
            let monitor = mixer.enable_local_monitor();
            let input = streams::input(
                &input,
                &input_config,
                producer,
                Some(monitor),
                Arc::clone(&stats),
            )?;
            let output = streams::output(
                &output,
                &output_config,
                streams::Playback::Mix(Box::new(mixer)),
                Arc::clone(&stats),
            )?;
            let session = Self {
                input: Some(input),
                output: Some(output),
                mic,
                rx,
                stats,
                network: Network::connect(endpoint, &preferences.nickname)?,
            };
            // RAII stops both streams if either play operation fails.
            if let Some(output) = &session.output {
                output.play()?;
            }
            if let Some(input) = &session.input {
                input.play()?;
            }
            Ok(session)
        };
        build().map_err(|error| io::Error::other(error.to_string()))
    }
}

trait ActiveSession {
    fn poll(&mut self) -> io::Result<bool>;
    fn telemetry_update(&mut self) -> Option<crate::network::Telemetry> {
        None
    }
    fn mixer_update(&mut self) -> Option<MixerView> {
        None
    }
}

impl ActiveSession for Session {
    fn telemetry_update(&mut self) -> Option<crate::network::Telemetry> {
        self.network.take_telemetry()
    }
    fn mixer_update(&mut self) -> Option<MixerView> {
        self.network.take_roster().map(|peers| MixerView {
            peers,
            stats: Arc::clone(&self.stats),
            controls: Arc::clone(&self.rx.controls),
            local_control: Arc::clone(&self.rx.local_control),
        })
    }

    fn poll(&mut self) -> io::Result<bool> {
        if self.stats.has_hardware_errors() {
            return Err(io::Error::other(
                "Errore audio: verifica scheda, driver e buffer.",
            ));
        }
        self.network
            .tick(&mut self.mic, &mut self.rx, Instant::now())?;
        Ok(self.network.is_connected())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Stop callbacks before dropping their rings and notifying the server.
        drop(self.output.take());
        drop(self.input.take());
        self.network.disconnect();
    }
}

fn control_loop<S: ActiveSession>(
    commands: &Receiver<Command>,
    events: &Sender<Event>,
    mut preferences: Preferences,
    path: &std::path::Path,
    mut connect: impl FnMut(&Preferences) -> io::Result<S>,
    mut discover: impl FnMut(&Preferences) -> io::Result<Catalog>,
) {
    let send = |event| {
        let _ = events.send(event);
    };
    let save = |preferences: &Preferences| {
        send(Event::Saved(
            preferences.save(path).map_err(|error| error.to_string()),
        ));
    };
    send(Event::Catalog(
        discover(&preferences).map_err(|error| error.to_string()),
    ));
    let mut session: Option<S> = None;
    let mut phase = Phase::Disconnected;
    loop {
        let wait = if session.is_some() {
            Duration::from_micros(250)
        } else {
            Duration::from_millis(50)
        };
        match commands.recv_timeout(wait) {
            Ok(command) => match command {
                Command::Connect(selected) => {
                    if session.is_none() {
                        preferences = selected;
                        save(&preferences);
                        send(Event::Phase(Phase::Connecting));
                        match connect(&preferences) {
                            Ok(active) => {
                                session = Some(active);
                                phase = Phase::Connecting;
                            }
                            Err(error) => {
                                send(Event::Error(error.to_string()));
                                send(Event::Phase(Phase::Disconnected));
                            }
                        }
                    }
                }
                Command::Disconnect => {
                    drop(session.take());
                    phase = Phase::Disconnected;
                    send(Event::Phase(phase));
                }
                Command::SelectDevice(selected) | Command::Save(selected) => {
                    preferences = selected;
                    save(&preferences);
                }
                Command::Refresh(selected) => {
                    if session.is_none() {
                        preferences = selected;
                        send(Event::Catalog(
                            discover(&preferences).map_err(|error| error.to_string()),
                        ));
                    }
                }
                Command::Shutdown(selected) => {
                    drop(session.take());
                    if let Some(selected) = selected {
                        save(&selected);
                    }
                    break;
                }
            },
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
        if let Some(active) = &mut session {
            match active.poll() {
                Ok(true) if phase != Phase::Connected => {
                    phase = Phase::Connected;
                    send(Event::Phase(phase));
                }
                Err(error) => {
                    drop(session.take());
                    phase = Phase::Disconnected;
                    send(Event::Error(error.to_string()));
                    send(Event::Phase(phase));
                }
                _ => {}
            }
            if let Some(metrics) = session.as_mut().and_then(ActiveSession::telemetry_update) {
                send(Event::Telemetry(metrics));
            }
            if let Some(view) = session.as_mut().and_then(ActiveSession::mixer_update) {
                send(Event::Mixer(view));
            }
        }
    }
}

#[cfg(test)]
mod tests;
