//! Back off an automatic stream when its existing QUIC traffic shows congestion.

use std::time::{Duration, Instant};

use crate::{media::MIN_AUTO_BITRATE_KBPS, tunnel::UploadSample};

pub(crate) const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const WARMUP: Duration = Duration::from_secs(2);
const CONGESTED_WINDOWS: u8 = 2;

pub(crate) struct Backoff {
    baseline_rtt: Option<Duration>,
    previous: Option<(Instant, UploadSample)>,
    congested: u8,
    ready_at: Instant,
    cooldown_until: Instant,
}

impl Backoff {
    pub fn new(now: Instant, baseline_rtt: Option<Duration>) -> Self {
        Self {
            baseline_rtt,
            previous: None,
            congested: 0,
            ready_at: now + WARMUP,
            cooldown_until: now,
        }
    }

    pub fn reset(&mut self, now: Instant) {
        self.previous = None;
        self.congested = 0;
        self.ready_at = now + WARMUP;
    }

    pub fn resumed(&mut self, now: Instant) {
        self.reset(now);
        self.cooldown_until = now + Duration::from_secs(8);
    }

    pub fn sample(&mut self, now: Instant, sample: UploadSample, bitrate: u32) -> Option<u32> {
        if let Some(rtt) = sample.rtt {
            self.baseline_rtt = Some(self.baseline_rtt.map_or(rtt, |baseline| baseline.min(rtt)));
        }
        let Some((then, previous)) = self.previous.replace((now, sample)) else {
            self.congested = 0;
            return None;
        };
        let elapsed = now.saturating_duration_since(then);
        let sent = sample.sent.saturating_sub(previous.sent);
        let lost = sample.lost.saturating_sub(previous.lost);
        let bytes = sample.bytes.saturating_sub(previous.bytes);
        let busy = sent >= 40 && bytes >= 16_384;
        if now < self.ready_at
            || now < self.cooldown_until
            || elapsed.is_zero()
            || elapsed > SAMPLE_INTERVAL * 2
            || !busy
            || bitrate <= MIN_AUTO_BITRATE_KBPS
        {
            self.congested = 0;
            return None;
        }
        let loss = (lost as f64 / sent as f64).min(1.0);
        let queue = self.baseline_rtt.zip(sample.rtt).is_some_and(|(baseline, rtt)| {
            rtt.saturating_sub(baseline) >= Duration::from_millis(35).max(baseline / 3)
        });
        if !queue && loss < 0.05 {
            self.congested = 0;
            return None;
        }
        self.congested += 1;
        if self.congested < CONGESTED_WINDOWS {
            return None;
        }
        self.reset(now);
        // This estimate uses sent bytes and QUIC's inferred loss for the same window.
        let delivered_kbps = bytes as f64 * 8.0 * (1.0 - loss) / elapsed.as_secs_f64() / 1000.0;
        let budget = (delivered_kbps * 0.75) as u32;
        Some(budget.min(bitrate * 3 / 4).max(MIN_AUTO_BITRATE_KBPS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(second: u64, lost: u64, rtt_ms: u64) -> UploadSample {
        UploadSample {
            sent: second * 1000,
            lost,
            bytes: second * 1_000_000,
            rtt: Some(Duration::from_millis(rtt_ms)),
        }
    }

    #[test]
    fn persistent_loss_backs_off_and_transient_loss_does_not() {
        let start = Instant::now();
        let mut backoff = Backoff::new(start, Some(Duration::from_millis(20)));
        let mut tick = |second, loss, rtt| backoff.sample(
            start + Duration::from_secs(second), sample(second, loss, rtt), 8_000,
        );
        assert_eq!(tick(0, 0, 20), None);
        assert_eq!(tick(1, 0, 25), None);
        assert_eq!(tick(2, 120, 100), None);
        assert_eq!(tick(3, 120, 20), None);
        assert_eq!(tick(4, 200, 95), None);
        assert_eq!(tick(5, 450, 120), Some(4_500));
        assert_eq!(tick(6, 700, 120), None);
    }

    #[test]
    fn high_baseline_rtt_and_light_traffic_keep_the_stream_running() {
        let start = Instant::now();
        let mut backoff = Backoff::new(start, Some(Duration::from_millis(200)));
        for second in 0..6 {
            assert_eq!(backoff.sample(
                start + Duration::from_secs(second), sample(second, 0, 220), 8_000,
            ), None);
        }
        for second in 6..12 {
            let idle = UploadSample {
                sent: 5000 + second * 20,
                lost: second * 4,
                bytes: 5_000_000 + second * 1000,
                rtt: None,
            };
            assert_eq!(backoff.sample(start + Duration::from_secs(second), idle, 8_000), None);
        }
    }

    #[test]
    fn sustained_queueing_triggers_before_packet_loss() {
        let start = Instant::now();
        let mut backoff = Backoff::new(start, Some(Duration::from_millis(20)));
        for second in 0..3 {
            assert_eq!(backoff.sample(
                start + Duration::from_secs(second), sample(second, 0, 100), 8_000,
            ), None);
        }
        assert_eq!(backoff.sample(start + Duration::from_secs(3), sample(3, 0, 100), 8_000), Some(6_000));
    }

    #[test]
    fn a_resumed_stream_has_time_to_stabilize() {
        let start = Instant::now();
        let mut backoff = Backoff::new(start, Some(Duration::from_millis(20)));
        backoff.resumed(start);
        for second in 0..=8 {
            assert_eq!(backoff.sample(
                start + Duration::from_secs(second), sample(second, second * 100, 100), 8_000,
            ), None);
        }
        assert_eq!(
            backoff.sample(start + Duration::from_secs(9), sample(9, 900, 100), 8_000),
            Some(5_400)
        );
    }
}
