//! Network quality estimation.
//!
//! This is a transport-agnostic estimator fed by RTT, jitter, loss, and
//! throughput samples. It gives UI/telemetry a stable quality grade and emits
//! low-latency pressure signals that batching, polling, and media codecs can
//! consume.

use std::time::Duration;

/// One network quality sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetworkQualitySample {
    /// Smoothed or raw RTT.
    pub rtt: Duration,
    /// RTT variation/jitter.
    pub jitter: Duration,
    /// Packet/frame loss in `[0, 1]`.
    pub loss: f64,
    /// Estimated throughput in bits per second.
    pub throughput_bps: u64,
}

/// User-facing link quality grade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkQualityGrade {
    /// No usable recent samples or link is considered unavailable.
    Offline,
    /// Excellent LAN/direct link.
    Excellent,
    /// Good interactive link.
    Good,
    /// Usable with reduced batching/bitrate.
    Fair,
    /// High latency/loss; prefer fallback/low-bitrate behavior.
    Poor,
}

/// Adaptation hints derived from quality estimation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkQualityRecommendation {
    /// Current grade.
    pub grade: NetworkQualityGrade,
    /// Prefer reduced batching delay for interactive traffic.
    pub prefer_low_latency: bool,
    /// Lower media/file bitrate or transfer chunk sizes.
    pub reduce_throughput: bool,
    /// Transport should consider fallback/reconnect if available.
    pub consider_transport_fallback: bool,
}

/// EWMA estimator for network quality.
#[derive(Debug, Clone)]
pub struct NetworkQualityEstimator {
    alpha: f64,
    sample_count: u64,
    rtt: Option<f64>,
    jitter: f64,
    loss: f64,
    throughput_bps: u64,
}

impl Default for NetworkQualityEstimator {
    fn default() -> Self {
        Self::new(0.20)
    }
}

impl NetworkQualityEstimator {
    /// Create an estimator with an EWMA gain clamped to `(0, 1]`.
    #[must_use]
    pub fn new(alpha: f64) -> Self {
        Self {
            alpha: alpha.clamp(f64::EPSILON, 1.0),
            sample_count: 0,
            rtt: None,
            jitter: 0.0,
            loss: 0.0,
            throughput_bps: 0,
        }
    }

    /// Record a sample and return the updated recommendation.
    pub fn record(&mut self, sample: NetworkQualitySample) -> NetworkQualityRecommendation {
        self.sample_count = self.sample_count.saturating_add(1);
        let rtt = sample.rtt.as_secs_f64();
        let jitter = sample.jitter.as_secs_f64();
        self.rtt = Some(match self.rtt {
            Some(current) => ewma(current, rtt, self.alpha),
            None => rtt,
        });
        self.jitter = if self.sample_count == 1 {
            jitter
        } else {
            ewma(self.jitter, jitter, self.alpha)
        };
        self.loss = if self.sample_count == 1 {
            sample.loss.clamp(0.0, 1.0)
        } else {
            ewma(self.loss, sample.loss.clamp(0.0, 1.0), self.alpha)
        };
        self.throughput_bps = if self.sample_count == 1 {
            sample.throughput_bps
        } else {
            ((self.throughput_bps as f64 * (1.0 - self.alpha))
                + (sample.throughput_bps as f64 * self.alpha)) as u64
        };
        self.recommendation()
    }

    /// Current grade.
    #[must_use]
    pub fn grade(&self) -> NetworkQualityGrade {
        let Some(rtt) = self.rtt else {
            return NetworkQualityGrade::Offline;
        };
        if self.loss >= 0.15 || self.throughput_bps < 64_000 {
            NetworkQualityGrade::Poor
        } else if rtt <= 0.020 && self.jitter <= 0.005 && self.loss < 0.005 {
            NetworkQualityGrade::Excellent
        } else if rtt <= 0.080 && self.jitter <= 0.020 && self.loss < 0.02 {
            NetworkQualityGrade::Good
        } else if rtt <= 0.180 && self.loss < 0.08 {
            NetworkQualityGrade::Fair
        } else {
            NetworkQualityGrade::Poor
        }
    }

    /// Current recommendation.
    #[must_use]
    pub fn recommendation(&self) -> NetworkQualityRecommendation {
        let grade = self.grade();
        NetworkQualityRecommendation {
            grade,
            prefer_low_latency: matches!(
                grade,
                NetworkQualityGrade::Excellent | NetworkQualityGrade::Good
            ),
            reduce_throughput: matches!(
                grade,
                NetworkQualityGrade::Fair | NetworkQualityGrade::Poor
            ),
            consider_transport_fallback: matches!(grade, NetworkQualityGrade::Poor),
        }
    }

    /// Smoothed RTT.
    #[must_use]
    pub fn rtt(&self) -> Option<Duration> {
        self.rtt.map(Duration::from_secs_f64)
    }

    /// Smoothed jitter.
    #[must_use]
    pub fn jitter(&self) -> Option<Duration> {
        (self.sample_count > 0).then_some(Duration::from_secs_f64(self.jitter))
    }
}

fn ewma(current: f64, sample: f64, alpha: f64) -> f64 {
    current * (1.0 - alpha) + sample * alpha
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excellent_link_is_low_latency() {
        let mut estimator = NetworkQualityEstimator::default();
        let rec = estimator.record(NetworkQualitySample {
            rtt: Duration::from_millis(8),
            jitter: Duration::from_millis(1),
            loss: 0.0,
            throughput_bps: 50_000_000,
        });
        assert_eq!(rec.grade, NetworkQualityGrade::Excellent);
        assert!(rec.prefer_low_latency);
    }

    #[test]
    fn lossy_link_recommends_fallback() {
        let mut estimator = NetworkQualityEstimator::new(1.0);
        let rec = estimator.record(NetworkQualitySample {
            rtt: Duration::from_millis(220),
            jitter: Duration::from_millis(80),
            loss: 0.20,
            throughput_bps: 500_000,
        });
        assert_eq!(rec.grade, NetworkQualityGrade::Poor);
        assert!(rec.reduce_throughput);
        assert!(rec.consider_transport_fallback);
    }
}
