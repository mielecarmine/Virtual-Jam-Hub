//! File telemetry owned exclusively by the dedicated telemetry thread.

use std::{
    fs::{File, OpenOptions},
    io::{self, BufWriter, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{network::Telemetry, setup::Preferences};

const NAMES: [&str; 20] = [
    "remote_peers",
    "rtt_ms",
    "jitter_ms",
    "loss_percent",
    "buffer_ms",
    "input_overflows",
    "output_underflows",
    "input_callbacks",
    "output_callbacks",
    "not_ready_samples",
    "align_dropped_samples",
    "last_output_frames",
    "late_dropped",
    "late_dropped_far",
    "plc_frames",
    "skipped_frames",
    "rx_push_dropped",
    "resyncs",
    "worker_max_gap_ms",
    "rtt_max_ms",
];

/// Actual CPAL stream configuration and the latest observed callback size.
#[derive(Clone, Copy, Default)]
pub struct AudioInfo {
    pub input_channels: u16,
    pub output_channels: u16,
    pub input_sample_rate: u32,
    pub output_sample_rate: u32,
    pub last_output_frames: u32,
}

pub struct SessionLog {
    writer: BufWriter<File>,
    #[cfg(test)]
    started: std::time::Instant,
    header_pending: bool,
    // Exact nearest-rank p95 over the whole session, including missing-RTT handling.
    values: [Vec<f64>; 20],
}

impl SessionLog {
    pub fn start(config: &Path, preferences: &Preferences, audio: AudioInfo) -> io::Result<Self> {
        let directory = config
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(directory)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        // create_new prevents overwriting even if clocks collide or move backwards.
        let mut suffix = 0_u32;
        let file = loop {
            let path = directory.join(format!(
                "telemetry-{stamp}-{}-{suffix}.txt",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(file) => break file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => suffix += 1,
                Err(error) => return Err(error),
            }
        };
        let mut writer = BufWriter::new(file);
        writeln!(
            writer,
            "Virtual Jam telemetry\nversion={}\nos={}\nbackend={:?}\ninput={:?}\noutput={:?}\nbuffer_frames={}\nserver={:?}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            preferences.host,
            preferences.input,
            preferences.output,
            preferences.buffer_frames,
            preferences.server
        )?;
        writeln!(
            writer,
            "input_channels={}\noutput_channels={}\ninput_sample_rate={}\noutput_sample_rate={}",
            audio.input_channels,
            audio.output_channels,
            audio.input_sample_rate,
            audio.output_sample_rate
        )?;
        let mut log = Self {
            writer,
            #[cfg(test)]
            started: std::time::Instant::now(),
            header_pending: true,
            values: std::array::from_fn(|_| Vec::new()),
        };
        // If connection precedes the first callback, finish the header at the first sample.
        if audio.last_output_frames != 0 {
            log.write_header(audio.last_output_frames)?;
        }
        log.writer.flush()?;
        Ok(log)
    }

    fn write_header(&mut self, frames: u32) -> io::Result<()> {
        if !self.header_pending {
            return Ok(());
        }
        writeln!(
            self.writer,
            "last_output_frames={}",
            if frames == 0 {
                "N/A".into()
            } else {
                frames.to_string()
            }
        )?;
        writeln!(
            self.writer,
            "interval_ms=500; remote_peers excludes self; p95=nearest rank; missing RTT=N/A (excluded); xruns=cumulative counters; buffer excludes hardware/SPSC queues"
        )?;
        writeln!(
            self.writer,
            "counters=cumulative since session start (audio u32 wraps; worker u32 saturates); input_callbacks/output_callbacks=driver calls; not_ready_samples=silent mono samples summed over active unready peers; align_dropped_samples=mono samples discarded including stale epochs; last_output_frames=last valid driver block (0=unobserved); late_dropped=all late packets including late_dropped_far (>=64 frames); plc_frames=Opus PLC decodes including holds, corrupt payload fallback and skipped missing frames; skipped_frames=cursor advances by skip/trim; rx_push_dropped=whole rejected chunks; resyncs=cursor reanchors and discontinuity resets (startup excluded); worker_max_gap_ms=peak tick gap in interval; rtt_max_ms=peak new Pong RTT in interval (N/A=no Pong); input_channels/output_channels=selected stream channels; sample rates=CPAL stream rates in Hz"
        )?;
        writeln!(self.writer, "elapsed_s\t{}", NAMES.join("\t"))?;
        self.header_pending = false;
        Ok(())
    }

    #[cfg(test)]
    pub fn record(&mut self, peers: u32, metrics: Telemetry, xruns: [u32; 2]) -> io::Result<()> {
        self.record_at(self.started.elapsed().as_secs_f64(), peers, metrics, xruns)
    }

    pub(super) fn record_at(
        &mut self,
        elapsed: f64,
        peers: u32,
        metrics: Telemetry,
        xruns: [u32; 2],
    ) -> io::Result<()> {
        self.write_header(metrics.audio.last_output_frames)?;
        let row = [
            Some(f64::from(peers)),
            metrics.rtt_ms,
            Some(metrics.jitter_ms),
            Some(metrics.loss_percent),
            Some(metrics.buffer_ms),
            Some(f64::from(xruns[0])),
            Some(f64::from(xruns[1])),
            Some(f64::from(metrics.audio.input_callbacks)),
            Some(f64::from(metrics.audio.output_callbacks)),
            Some(f64::from(metrics.audio.not_ready_samples)),
            Some(f64::from(metrics.audio.align_dropped_samples)),
            Some(f64::from(metrics.audio.last_output_frames)),
            Some(f64::from(metrics.diagnostics.late_dropped)),
            Some(f64::from(metrics.diagnostics.late_dropped_far)),
            Some(f64::from(metrics.diagnostics.plc_frames)),
            Some(f64::from(metrics.diagnostics.skipped_frames)),
            Some(f64::from(metrics.diagnostics.rx_push_dropped)),
            Some(f64::from(metrics.diagnostics.resyncs)),
            Some(metrics.worker_max_gap_ms),
            metrics.rtt_max_ms,
        ];
        write!(self.writer, "{elapsed:.3}")?;
        for (values, value) in self.values.iter_mut().zip(row) {
            if let Some(value) = value.filter(|value| value.is_finite()) {
                values.push(value);
                write!(self.writer, "\t{value:.3}")?;
            } else {
                write!(self.writer, "\tN/A")?;
            }
        }
        writeln!(self.writer)?;
        self.writer.flush()
    }

    fn finish(&mut self) -> io::Result<()> {
        self.write_header(0)?;
        writeln!(self.writer, "Summary\nmetric\tsamples\tmin\tavg\tp95\tmax")?;
        for (name, values) in NAMES.iter().zip(&mut self.values) {
            values.sort_unstable_by(f64::total_cmp);
            let count = values.len();
            if count == 0 {
                writeln!(self.writer, "{name}\t0\tN/A\tN/A\tN/A\tN/A")?;
            } else {
                // Session lengths are far below f64's exact integer range.
                #[allow(clippy::cast_precision_loss)]
                let avg = values.iter().sum::<f64>() / count as f64;
                let rank = count - count / 20 - 1;
                writeln!(
                    self.writer,
                    "{name}\t{count}\t{:.3}\t{avg:.3}\t{:.3}\t{:.3}",
                    values[0],
                    values[rank],
                    values[count - 1]
                )?;
            }
        }
        self.writer.flush()
    }
}

mod asynchronous;
pub use asynchronous::AsyncSessionLog;

impl Drop for SessionLog {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("Cannot finalize telemetry log: {error}");
        }
    }
}

#[cfg(test)]
mod tests;
