//! File telemetry for prerelease GUI sessions. Called only by jam-controller.

use std::{
    fs::{File, OpenOptions},
    io::{self, BufWriter, Write},
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crate::{network::Telemetry, setup::Preferences};

const NAMES: [&str; 7] = [
    "remote_peers",
    "rtt_ms",
    "jitter_ms",
    "loss_percent",
    "buffer_ms",
    "input_overflows",
    "output_underflows",
];

pub struct SessionLog {
    writer: BufWriter<File>,
    started: Instant,
    // Exact nearest-rank p95 over the whole session, including missing-RTT handling.
    values: [Vec<f64>; 7],
}

impl SessionLog {
    pub fn start(config: &Path, preferences: &Preferences) -> io::Result<Self> {
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
            "interval_ms=500; remote_peers excludes self; p95=nearest rank; missing RTT=N/A (excluded); xruns=cumulative counters; buffer excludes hardware/SPSC queues"
        )?;
        writeln!(writer, "elapsed_s\t{}", NAMES.join("\t"))?;
        writer.flush()?;
        Ok(Self {
            writer,
            started: Instant::now(),
            values: std::array::from_fn(|_| Vec::new()),
        })
    }

    pub fn record(&mut self, peers: u32, metrics: Telemetry, xruns: [u32; 2]) -> io::Result<()> {
        let row = [
            Some(f64::from(peers)),
            metrics.rtt_ms,
            Some(metrics.jitter_ms),
            Some(metrics.loss_percent),
            Some(metrics.buffer_ms),
            Some(f64::from(xruns[0])),
            Some(f64::from(xruns[1])),
        ];
        write!(self.writer, "{:.3}", self.started.elapsed().as_secs_f64())?;
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

impl Drop for SessionLog {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("Cannot finalize telemetry log: {error}");
        }
    }
}

#[cfg(test)]
mod tests;
