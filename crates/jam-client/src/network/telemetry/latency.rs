//! Deterministic desktop estimate, evaluated outside the CPAL callbacks.
use jam_protocol::SAMPLE_RATE;

use crate::{codec::FRAME_SAMPLES, mixer::RX_TARGET_SAMPLES};

const OPTIMAL_MS: f64 = 20.0;
const LIMIT_MS: f64 = 25.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatencyClass {
    Optimal,
    Limit,
    TempoDrag,
}

impl LatencyClass {
    fn from_total(total: f64) -> Self {
        if total < OPTIMAL_MS {
            Self::Optimal
        } else if total <= LIMIT_MS {
            Self::Limit
        } else {
            Self::TempoDrag
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Optimal => "Ottimale",
            Self::Limit => "Limite",
            Self::TempoDrag => "Rischio Tempo-Drag",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LatencyBudget {
    pub audio_io_ms: f64,
    pub codec_ms: f64,
    pub network_one_way_ms: Option<f64>,
    pub rx_buffer_ms: f64,
    pub total_one_way_ms: Option<f64>,
    pub headroom_ms: Option<f64>,
    pub class: Option<LatencyClass>,
}

impl LatencyBudget {
    /// Capture uses the requested block; playback uses the observed driver block,
    /// falling back to the requested block before the first output callback.
    /// RTT is the server round-trip proxy, not a measured peer-to-peer audio path.
    pub fn estimate(
        buffer_frames: u32,
        last_output_frames: u32,
        lookahead_samples: u32,
        rtt_ms: Option<f64>,
        buffer_ms: f64,
    ) -> Self {
        let sample_ms = 1000.0 / f64::from(SAMPLE_RATE);
        let output_frames = if last_output_frames == 0 {
            buffer_frames
        } else {
            last_output_frames
        };
        let audio_io_ms = (f64::from(buffer_frames) + f64::from(output_frames)) * sample_ms;
        // Fixed small sample counts are exactly representable in f64.
        #[allow(clippy::cast_precision_loss)]
        let codec_ms = (FRAME_SAMPLES as f64 + f64::from(lookahead_samples)) * sample_ms;
        #[allow(clippy::cast_precision_loss)]
        let rx_buffer_ms = buffer_ms + RX_TARGET_SAMPLES as f64 * sample_ms;
        let network_one_way_ms = rtt_ms
            .filter(|ms| ms.is_finite() && *ms >= 0.0)
            .map(|ms| ms / 2.0);
        let total_one_way_ms =
            network_one_way_ms.map(|network| audio_io_ms + codec_ms + network + rx_buffer_ms);
        Self {
            audio_io_ms,
            codec_ms,
            network_one_way_ms,
            rx_buffer_ms,
            total_one_way_ms,
            headroom_ms: total_one_way_ms.map(|total| LIMIT_MS - total),
            class: total_one_way_ms.map(LatencyClass::from_total),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_use_actual_output_and_current_mixer_reserve() {
        let budget = LatencyBudget::estimate(64, 256, 120, Some(24.0), 7.5);
        assert!((budget.audio_io_ms - 20.0 / 3.0).abs() < 1e-10);
        assert_eq!(budget.codec_ms, 5.0);
        assert_eq!(budget.network_one_way_ms, Some(12.0));
        assert_eq!(budget.rx_buffer_ms, 25.0);
        let total = budget.total_one_way_ms.unwrap();
        assert!((total - (20.0 / 3.0 + 5.0 + 12.0 + 25.0)).abs() < 1e-10);
        assert_eq!(budget.headroom_ms, Some(25.0 - total));
        assert!(budget.headroom_ms.unwrap() < 0.0);
    }

    #[test]
    fn classification_includes_both_limit_boundaries() {
        // Zero hardware frames and lookahead isolate 2.5 ms codec + 17.5 ms reserve.
        for (rtt, class) in [
            (0.0, LatencyClass::Limit),
            (9.999, LatencyClass::Limit),
            (10.0, LatencyClass::Limit),
            (10.001, LatencyClass::TempoDrag),
        ] {
            assert_eq!(
                LatencyBudget::estimate(0, 0, 0, Some(rtt), 0.0).class,
                Some(class)
            );
        }
        for (total, class) in [
            (0.0, LatencyClass::Optimal),
            (19.999, LatencyClass::Optimal),
            (20.0, LatencyClass::Limit),
            (25.0, LatencyClass::Limit),
            (25.001, LatencyClass::TempoDrag),
        ] {
            assert_eq!(LatencyClass::from_total(total), class);
        }
    }

    #[test]
    fn missing_or_invalid_rtt_never_becomes_a_green_zero_latency() {
        for rtt in [None, Some(f64::NAN), Some(f64::INFINITY), Some(-1.0)] {
            let budget = LatencyBudget::estimate(128, 0, 120, rtt, 5.0);
            assert!((budget.audio_io_ms - 16.0 / 3.0).abs() < 1e-10);
            assert_eq!(budget.codec_ms, 5.0);
            assert_eq!(budget.rx_buffer_ms, 22.5);
            assert_eq!(budget.network_one_way_ms, None);
            assert_eq!(budget.total_one_way_ms, None);
            assert_eq!(budget.headroom_ms, None);
            assert_eq!(budget.class, None);
        }
    }

    #[test]
    fn configured_opus_lookahead_is_part_of_codec_budget() {
        let mut encoder = crate::codec::encoder().unwrap();
        let lookahead = u32::try_from(encoder.get_lookahead().unwrap()).unwrap();
        assert!(lookahead > 0);
        let budget = LatencyBudget::estimate(64, 64, lookahead, Some(0.0), 0.0);
        assert!((budget.codec_ms - (2.5 + f64::from(lookahead) / 48.0)).abs() < 1e-10);
    }
}
