//! Dynamic bandwidth adaptation for internet/mobile links.
//!
//! The adapter is a pure state machine fed by transport metrics. It intentionally
//! avoids owning timers/tasks; callers sample network telemetry and apply the
//! returned [`BandwidthRecommendation`] to codecs, batching, and transfer chunk
//! sizes.

use std::time::Duration;

/// Network profile used to bias bandwidth decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkProfile {
    /// LAN or unconstrained Wi-Fi.
    Lan,
    /// Typical broadband internet.
    Internet,
    /// Mobile/cellular network where radio wakeups, data use, and jitter matter.
    Mobile,
}

/// One telemetry sample for adaptation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandwidthSample {
    /// Measured RTT.
    pub rtt: Duration,
    /// RTT variation.
    pub jitter: Duration,
    /// Packet/frame loss in `[0, 1]`.
    pub loss: f64,
    /// Estimated available throughput in bits/sec.
    pub throughput_bps: u64,
}

/// Recommendation emitted by the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BandwidthRecommendation {
    /// Target bitrate for continuous streams.
    pub target_bitrate_bps: u64,
    /// Maximum file-transfer chunk size.
    pub max_chunk_bytes: usize,
    /// Whether latency-sensitive traffic should reduce batching.
    pub prefer_low_latency: bool,
    /// Whether mobile optimizations should be active.
    pub mobile_optimized: bool,
}

/// Tunables for the bandwidth adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BandwidthPolicy {
    /// Minimum bitrate.
    pub min_bitrate_bps: u64,
    /// Maximum bitrate.
    pub max_bitrate_bps: u64,
    /// Default bitrate before samples arrive.
    pub initial_bitrate_bps: u64,
    /// Maximum chunk size on stable links.
    pub max_chunk_bytes: usize,
    /// Conservative chunk size for mobile/high-jitter links.
    pub mobile_chunk_bytes: usize,
}

impl Default for BandwidthPolicy {
    fn default() -> Self {
        Self {
            min_bitrate_bps: 96_000,
            max_bitrate_bps: 8_000_000,
            initial_bitrate_bps: 1_500_000,
            max_chunk_bytes: 256 * 1024,
            mobile_chunk_bytes: 48 * 1024,
        }
    }
}

/// EWMA-like bandwidth adapter.
#[derive(Debug, Clone)]
pub struct BandwidthAdapter {
    policy: BandwidthPolicy,
    profile: NetworkProfile,
    target_bitrate_bps: u64,
}

impl BandwidthAdapter {
    /// Create an adapter.
    #[must_use]
    pub fn new(policy: BandwidthPolicy, profile: NetworkProfile) -> Self {
        Self {
            target_bitrate_bps: policy.initial_bitrate_bps,
            policy,
            profile,
        }
    }

    /// Current recommendation without recording a new sample.
    #[must_use]
    pub fn recommendation(&self) -> BandwidthRecommendation {
        self.recommend_for(None)
    }

    /// Record a sample and return the updated recommendation.
    pub fn record(&mut self, sample: BandwidthSample) -> BandwidthRecommendation {
        let loss_penalty = if sample.loss >= 0.10 {
            0.55
        } else if sample.loss >= 0.03 {
            0.75
        } else {
            1.08
        };
        let jitter_penalty = if sample.jitter > Duration::from_millis(40) {
            0.80
        } else {
            1.0
        };
        let profile_penalty = match self.profile {
            NetworkProfile::Lan => 1.20,
            NetworkProfile::Internet => 1.0,
            NetworkProfile::Mobile => 0.70,
        };
        let throughput_ceiling = (sample.throughput_bps as f64 * 0.80) as u64;
        let proposed =
            (self.target_bitrate_bps as f64 * loss_penalty * jitter_penalty * profile_penalty)
                as u64;
        self.target_bitrate_bps = proposed
            .min(throughput_ceiling.max(self.policy.min_bitrate_bps))
            .clamp(self.policy.min_bitrate_bps, self.policy.max_bitrate_bps);
        self.recommend_for(Some(sample))
    }

    fn recommend_for(&self, sample: Option<BandwidthSample>) -> BandwidthRecommendation {
        let mobile_optimized = self.profile == NetworkProfile::Mobile;
        let high_jitter = sample.is_some_and(|s| s.jitter > Duration::from_millis(40));
        let max_chunk_bytes = if mobile_optimized || high_jitter {
            self.policy.mobile_chunk_bytes
        } else {
            self.policy.max_chunk_bytes
        };
        BandwidthRecommendation {
            target_bitrate_bps: self.target_bitrate_bps,
            max_chunk_bytes,
            prefer_low_latency: mobile_optimized || high_jitter,
            mobile_optimized,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(loss: f64, jitter_ms: u64, throughput_bps: u64) -> BandwidthSample {
        BandwidthSample {
            rtt: Duration::from_millis(80),
            jitter: Duration::from_millis(jitter_ms),
            loss,
            throughput_bps,
        }
    }

    #[test]
    fn loss_reduces_bitrate() {
        let mut adapter =
            BandwidthAdapter::new(BandwidthPolicy::default(), NetworkProfile::Internet);
        let before = adapter.recommendation().target_bitrate_bps;
        let after = adapter
            .record(sample(0.15, 5, 10_000_000))
            .target_bitrate_bps;
        assert!(after < before);
    }

    #[test]
    fn clean_link_increases_but_respects_throughput() {
        let mut adapter =
            BandwidthAdapter::new(BandwidthPolicy::default(), NetworkProfile::Internet);
        let rec = adapter.record(sample(0.0, 5, 1_000_000));
        assert!(rec.target_bitrate_bps <= 800_000);
    }

    #[test]
    fn mobile_profile_uses_smaller_chunks() {
        let mut adapter = BandwidthAdapter::new(BandwidthPolicy::default(), NetworkProfile::Mobile);
        let rec = adapter.record(sample(0.0, 10, 5_000_000));
        assert!(rec.mobile_optimized);
        assert_eq!(
            rec.max_chunk_bytes,
            BandwidthPolicy::default().mobile_chunk_bytes
        );
        assert!(rec.prefer_low_latency);
    }
}
