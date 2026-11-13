//! Back off an automatic stream when its existing QUIC traffic shows congestion.

use std::time::{Duration, Instant};

use crate::{media::MIN_AUTO_BITRATE_KBPS, tunnel::UploadSample};

pub(crate) const SAMPLE_INTERVAL: Duration = Duration::from_millis(200);
const WARMUP: Duration = Duration::from_secs(2);
const CONGESTION_HOLD: Duration = Duration::from_secs(1);
const RTT_GROWTH: Duration = Duration::from_millis(20);
const LOSS_WINDOW: Duration = Duration::from_secs(1);

pub(crate) struct Backoff {
    baseline_rtt: Option<Duration>,
    previous: Option<(Instant, UploadSample)>,
    traffic: Option<(Instant, UploadSample)>,
    queue_since: Option<Instant>,
    loss_since: Option<Instant>,
    delivery: Option<(f64, f64)>,
    ready_at: Instant,
    cooldown_until: Instant,
}

impl Backoff {
    pub fn new(now: Instant, baseline_rtt: Option<Duration>) -> Self {
        Self {
            baseline_rtt,
            previous: None,
            traffic: None,
            queue_since: None,
            loss_since: None,
            delivery: None,
            ready_at: now + WARMUP,
            cooldown_until: now,
        }
    }

    pub fn reset(&mut self, now: Instant) {
        self.previous = None;
        self.traffic = None;
        self.queue_since = None;
        self.loss_since = None;
        self.delivery = None;
        self.ready_at = now + WARMUP;
    }

    pub fn resumed(&mut self, now: Instant) {
        self.reset(now);
        self.cooldown_until = now + Duration::from_secs(8);
    }

    pub fn sample(&mut self, now: Instant, sample: UploadSample, bitrate: u32) -> Option<u32> {
        if let Some(rtt) = sample.idle_rtt.or(sample.rtt) {
            self.baseline_rtt = Some(self.baseline_rtt.map_or(rtt, |baseline| baseline.min(rtt)));
        }
        let Some((then, previous)) = self.previous.replace((now, sample)) else {
            self.traffic = Some((now, sample));
            return None;
        };
        let elapsed = now.saturating_duration_since(then);
        let busy = sample.sent.saturating_sub(previous.sent) >= 8
            && sample.bytes.saturating_sub(previous.bytes) >= 3_276;
        if now < self.ready_at
            || now < self.cooldown_until
            || elapsed.is_zero()
            || elapsed > SAMPLE_INTERVAL * 3
            || bitrate <= MIN_AUTO_BITRATE_KBPS
        {
            self.traffic = Some((now, sample));
            self.queue_since = None;
            self.loss_since = None;
            self.delivery = None;
            return None;
        }
        let high_rtt = busy && self.baseline_rtt.zip(sample.rtt).is_some_and(|(baseline, rtt)| {
            rtt.saturating_sub(baseline) > RTT_GROWTH
        });
        let queued = sustained(&mut self.queue_since, high_rtt, now);
        let (window_start, window_sample) = *self.traffic.get_or_insert((then, previous));
        let duration = now.duration_since(window_start);
        if duration >= LOSS_WINDOW {
            self.traffic = Some((now, sample));
            let sent = sample.sent.saturating_sub(window_sample.sent);
            let lost = sample.lost.saturating_sub(window_sample.lost);
            let bytes = sample.bytes.saturating_sub(window_sample.bytes);
            self.delivery = (sent >= 40 && bytes >= 16_384).then(|| {
                let loss = (lost as f64 / sent as f64).min(1.0);
                let kbps = bytes as f64 * 8.0 * (1.0 - loss) / duration.as_secs_f64() / 1000.0;
                (loss, kbps)
            });
        }
        let lossy = sustained(&mut self.loss_since, self.delivery.is_some_and(|(loss, _)| loss >= 0.05), now);
        if !queued && !lossy {
            return None;
        }
        // Small first step for queue growth without significant loss. Once
        // packets are lost, use delivered throughput to cut more quickly.
        let next = match self.delivery {
            Some((loss, kbps)) if loss >= 0.05 => ((kbps * 0.75) as u32).min(bitrate * 3 / 4),
            _ => bitrate * 4 / 5,
        };
        self.reset(now);
        Some(next.max(MIN_AUTO_BITRATE_KBPS))
    }
}

fn sustained(since: &mut Option<Instant>, active: bool, now: Instant) -> bool {
    if !active {
        *since = None;
        return false;
    }
    now.duration_since(*since.get_or_insert(now)) >= CONGESTION_HOLD
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tick: u64, lost: u64, rtt_ms: u64) -> UploadSample {
        UploadSample {
            sent: tick * 200,
            lost,
            bytes: tick * 200_000,
            rtt: Some(Duration::from_millis(rtt_ms)),
            idle_rtt: None,
        }
    }

    #[test]
    fn persistent_loss_backs_off_and_transient_loss_does_not() {
        let start = Instant::now();
        let mut backoff = Backoff::new(start, Some(Duration::from_millis(20)));
        let mut cuts = Vec::new();
        for tick in 0..=26 {
            let lost = if tick < 6 { 0 } else if tick < 16 { 100 } else { 100 + (tick - 15) * 40 };
            if let Some(rate) = backoff.sample(start + SAMPLE_INTERVAL * tick as u32, sample(tick, lost, 20), 8_000) {
                cuts.push((tick, rate));
            }
        }
        assert_eq!(cuts, [(24, 4_800)]);
    }

    #[test]
    fn high_baseline_rtt_and_light_traffic_keep_the_stream_running() {
        let start = Instant::now();
        let mut backoff = Backoff::new(start, Some(Duration::from_millis(200)));
        for tick in 0..20 {
            assert_eq!(backoff.sample(
                start + SAMPLE_INTERVAL * tick as u32, sample(tick, 0, 220), 8_000,
            ), None);
        }
        for tick in 20..40 {
            let idle = UploadSample {
                sent: 4_000 + tick,
                lost: 0,
                bytes: 4_000_000 + tick * 200,
                rtt: None,
                idle_rtt: None,
            };
            assert_eq!(backoff.sample(start + SAMPLE_INTERVAL * tick as u32, idle, 8_000), None);
        }
    }

    #[test]
    fn sustained_queueing_triggers_before_packet_loss() {
        let start = Instant::now();
        let mut backoff = Backoff::new(start, Some(Duration::from_millis(20)));
        for tick in 0..15 {
            let rtt = if tick < 5 { 20 } else { 43 };
            assert_eq!(backoff.sample(
                start + SAMPLE_INTERVAL * tick as u32, sample(tick, 0, rtt), 8_000,
            ), None);
        }
        assert_eq!(backoff.sample(start + Duration::from_secs(3), sample(15, 0, 43), 8_000), Some(6_400));
    }

    #[test]
    fn a_resumed_stream_has_time_to_stabilize() {
        let start = Instant::now();
        let mut backoff = Backoff::new(start, Some(Duration::from_millis(20)));
        backoff.resumed(start);
        for tick in 0..45 {
            assert_eq!(backoff.sample(
                start + SAMPLE_INTERVAL * tick as u32, sample(tick, 0, 100), 8_000,
            ), None);
        }
        assert_eq!(
            backoff.sample(start + Duration::from_secs(9), sample(45, 0, 100), 8_000),
            Some(6_400)
        );
    }
}
