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
    Metronome(jam_protocol::MetronomeConfig),
    Connect(Preferences),
    Disconnect,
    SelectDevice(Preferences),
    Refresh(Preferences),
    Save(Preferences),
    Shutdown(Option<Preferences>),
}

pub struct MixerView {
    pub local_peer_id: u8,
    pub metronome: Arc<mixer::MetronomeControls>,
    pub stats: Arc<Stats>,
    pub local_control: Arc<mixer::Controls>,
    pub peers: Vec<jam_protocol::PeerInfo>,
    pub controls: Arc<[mixer::Controls; jam_protocol::MAX_PEERS]>,
}

pub enum Event {
    Metronome(jam_protocol::MetronomeState),
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
                let _priority = match crate::scheduling::controller() {
                    Ok(priority) => Some(priority),
                    Err(error) => {
                        let _ =
                            sender.send(Event::Error(format!("Controller scheduling: {error}")));
                        None
                    }
                };
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
    published_metronome: Option<jam_protocol::MetronomeState>,
    buffer_frames: u32,
    #[cfg(feature = "telemetry-log")]
    log_preferences: Preferences,
    #[cfg(feature = "telemetry-log")]
    log_audio: crate::telemetry_log::AudioInfo,
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
            #[cfg(feature = "telemetry-log")]
            let log_preferences = Preferences {
                host: Some(host.id().name().into()),
                input: Some(input.name()?),
                output: Some(output.name()?),
                ..preferences.clone()
            };
            let input_config =
                audio::select_input_config_for_buffer(input.supported_input_configs()?, frames)
                    .ok_or_else(|| {
                        io::Error::other("Input incompatibile con 48 kHz e il buffer scelto.")
                    })?;
            let output_config =
                audio::select_output_config_for_buffer(output.supported_output_configs()?, frames)
                    .ok_or_else(|| {
                        io::Error::other("Output incompatibile con 48 kHz e il buffer scelto.")
                    })?;
            let stats = Arc::new(Stats::default());
            let (producer, mic) = RingBuffer::new(audio::RING_CAPACITY);
            let (rx, mut mixer) = mixer::channels(frames as usize);
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
                published_metronome: None,
                buffer_frames: frames,
                #[cfg(feature = "telemetry-log")]
                log_preferences,
                #[cfg(feature = "telemetry-log")]
                log_audio: crate::telemetry_log::AudioInfo {
                    input_channels: input_config.stream.channels,
                    output_channels: output_config.stream.channels,
                    input_sample_rate: input_config.stream.sample_rate.0,
                    output_sample_rate: output_config.stream.sample_rate.0,
                    last_output_frames: 0,
                },
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
    fn update_metronome(&mut self, _: jam_protocol::MetronomeConfig) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Metronome unavailable",
        ))
    }
    fn metronome_update(&mut self) -> Option<jam_protocol::MetronomeState> {
        None
    }
    #[cfg(feature = "telemetry-log")]
    fn log_audio(&self) -> crate::telemetry_log::AudioInfo {
        crate::telemetry_log::AudioInfo::default()
    }
    #[cfg(feature = "telemetry-log")]
    fn log_preferences(&self, requested: &Preferences) -> Preferences {
        requested.clone()
    }
    #[cfg(feature = "telemetry-log")]
    fn xruns(&self) -> [u32; 2] {
        [0; 2]
    }
    fn poll(&mut self) -> io::Result<bool>;
    fn telemetry_update(&mut self) -> Option<crate::network::Telemetry> {
        None
    }
    fn mixer_update(&mut self) -> Option<MixerView> {
        None
    }
}

impl ActiveSession for Session {
    fn update_metronome(&mut self, config: jam_protocol::MetronomeConfig) -> io::Result<()> {
        self.network.update_metronome(config)
    }

    fn metronome_update(&mut self) -> Option<jam_protocol::MetronomeState> {
        let state = self.network.metronome_state()?;
        if self.published_metronome == Some(state) {
            return None;
        }
        self.published_metronome = Some(state);
        Some(state)
    }
    #[cfg(feature = "telemetry-log")]
    fn log_audio(&self) -> crate::telemetry_log::AudioInfo {
        crate::telemetry_log::AudioInfo {
            last_output_frames: self.stats.telemetry().last_output_frames,
            ..self.log_audio
        }
    }
    #[cfg(feature = "telemetry-log")]
    fn log_preferences(&self, _: &Preferences) -> Preferences {
        self.log_preferences.clone()
    }
    #[cfg(feature = "telemetry-log")]
    fn xruns(&self) -> [u32; 2] {
        use std::sync::atomic::Ordering;
        [
            self.stats.input_overflows.load(Ordering::Relaxed),
            self.stats.output_underflows.load(Ordering::Relaxed),
        ]
    }
    fn telemetry_update(&mut self) -> Option<crate::network::Telemetry> {
        self.network.take_telemetry().map(|mut metrics| {
            metrics.audio = self.stats.telemetry();
            metrics.latency = Some(crate::network::telemetry::LatencyBudget::estimate(
                self.buffer_frames,
                metrics.audio.last_output_frames,
                self.network.codec_lookahead_samples,
                metrics.route_rtt_ms,
                metrics.buffer_ms,
            ));
            metrics
        })
    }
    fn mixer_update(&mut self) -> Option<MixerView> {
        self.network.take_roster().map(|peers| MixerView {
            local_peer_id: self.network.local_peer_id().unwrap_or(0),
            metronome: Arc::clone(&self.rx.metronome),
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

// Keep command handling and phase transitions together so teardown order stays visible.
#[allow(clippy::too_many_lines)]
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
    // Declaration order also stops streams before joining the logger during unwinding.
    #[cfg(feature = "telemetry-log")]
    let mut log: Option<crate::telemetry_log::AsyncSessionLog> = None;
    let mut session: Option<S> = None;
    let mut phase = Phase::Disconnected;
    #[cfg(feature = "telemetry-log")]
    let mut remote_peers = 0;
    loop {
        let wait = if session.is_some() {
            Duration::from_micros(250)
        } else {
            Duration::from_millis(50)
        };
        match receive_command(commands, wait, session.is_some()) {
            Ok(command) => match command {
                Command::Metronome(config) => {
                    if let Some(active) = &mut session {
                        if let Err(error) = active.update_metronome(config) {
                            send(Event::Error(format!("Metronomo: {error}")));
                        }
                    }
                }
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
                    #[cfg(feature = "telemetry-log")]
                    drop(log.take());
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
                    #[cfg(feature = "telemetry-log")]
                    drop(log.take());
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
                    #[cfg(feature = "telemetry-log")]
                    {
                        remote_peers = 0;
                        match crate::telemetry_log::AsyncSessionLog::start(
                            path,
                            &active.log_preferences(&preferences),
                            active.log_audio(),
                            events.clone(),
                        ) {
                            Ok(writer) => log = Some(writer),
                            Err(error) => send(Event::Error(format!("Telemetry log: {error}"))),
                        }
                    }
                    send(Event::Phase(phase));
                }
                Err(error) => {
                    drop(session.take());
                    #[cfg(feature = "telemetry-log")]
                    drop(log.take());
                    phase = Phase::Disconnected;
                    send(Event::Error(error.to_string()));
                    send(Event::Phase(phase));
                }
                _ => {}
            }
            if let Some(active) = &mut session {
                publish_updates(
                    active,
                    events,
                    #[cfg(feature = "telemetry-log")]
                    &mut log,
                    #[cfg(feature = "telemetry-log")]
                    &mut remote_peers,
                );
            }
        }
    }
    drop(session.take());
    #[cfg(feature = "telemetry-log")]
    drop(log.take());
}

fn publish_updates<S: ActiveSession>(
    active: &mut S,
    events: &Sender<Event>,
    #[cfg(feature = "telemetry-log")] log: &mut Option<crate::telemetry_log::AsyncSessionLog>,
    #[cfg(feature = "telemetry-log")] remote_peers: &mut u32,
) {
    if let Some(state) = active.metronome_update() {
        let _ = events.send(Event::Metronome(state));
    }
    if let Some(view) = active.mixer_update() {
        #[cfg(feature = "telemetry-log")]
        {
            *remote_peers = u32::try_from(view.peers.len()).unwrap_or(u32::MAX);
        }
        let _ = events.send(Event::Mixer(view));
    }
    if let Some(metrics) = active.telemetry_update() {
        #[cfg(feature = "telemetry-log")]
        if let Some(writer) = log.as_mut() {
            writer.record(*remote_peers, metrics, active.xruns());
        }
        let _ = events.send(Event::Telemetry(metrics));
    }
}

#[cfg(test)]
mod tests;

// Rust's Windows sleep uses a high-resolution waitable timer (Windows 10 1803+).
// A channel timeout uses WaitOnAddress instead and can round up to the 15.625 ms tick.
fn receive_command(
    commands: &Receiver<Command>,
    wait: Duration,
    active: bool,
) -> Result<Command, crossbeam_channel::RecvTimeoutError> {
    #[cfg(target_os = "windows")]
    if active {
        return match commands.try_recv() {
            Ok(command) => Ok(command),
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                Err(crossbeam_channel::RecvTimeoutError::Disconnected)
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {
                thread::sleep(wait);
                commands.try_recv().map_err(|error| match error {
                    crossbeam_channel::TryRecvError::Disconnected => {
                        crossbeam_channel::RecvTimeoutError::Disconnected
                    }
                    crossbeam_channel::TryRecvError::Empty => {
                        crossbeam_channel::RecvTimeoutError::Timeout
                    }
                })
            }
        };
    }
    let _ = active;
    commands.recv_timeout(wait)
}
