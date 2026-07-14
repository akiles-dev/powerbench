//! Summary statistics over a recording.

use serde::Serialize;

use crate::format::Recording;

/// Percentiles reported by [`Stats`].
pub const PERCENTILES: [f64; 8] = [1.0, 5.0, 50.0, 90.0, 95.0, 99.0, 99.9, 99.99];

/// Summary statistics of a recording. All currents are in microampere.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    /// Number of samples.
    pub count: usize,
    /// Duration covered by the samples, in seconds.
    pub duration_secs: f64,
    /// Minimum current (µA).
    pub min: f64,
    /// Maximum current (µA).
    pub max: f64,
    /// Mean current (µA).
    pub mean: f64,
    /// Sample standard deviation (µA).
    pub std_dev: f64,
    /// Percentiles as `(percentile, current µA)` pairs; see [`PERCENTILES`].
    pub percentiles: Vec<(f64, f64)>,
    /// Total charge drawn during the recording, in microampere-hours.
    pub charge_uah: f64,
    /// Average power in microwatt, derived from the mean current and the
    /// source voltage stored in the file.
    pub avg_power_uw: f64,
}

impl Stats {
    /// Compute statistics for a recording.
    pub fn compute(rec: &Recording) -> Self {
        Self::from_samples(&rec.samples_ua, rec.meta.sps, rec.meta.voltage_mv)
    }

    /// Compute statistics from raw samples (µA) at the given sample rate.
    pub fn from_samples(samples: &[f32], sps: u32, voltage_mv: u16) -> Self {
        let count = samples.len();
        let duration_secs = count as f64 / sps as f64;
        if count == 0 {
            return Self {
                count,
                duration_secs,
                min: f64::NAN,
                max: f64::NAN,
                mean: f64::NAN,
                std_dev: f64::NAN,
                percentiles: PERCENTILES.iter().map(|&p| (p, f64::NAN)).collect(),
                charge_uah: 0.0,
                avg_power_uw: f64::NAN,
            };
        }

        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut sum = 0.0f64;
        for &s in samples {
            let s = s as f64;
            min = min.min(s);
            max = max.max(s);
            sum += s;
        }
        let mean = sum / count as f64;

        let var = if count > 1 {
            samples
                .iter()
                .map(|&s| {
                    let d = s as f64 - mean;
                    d * d
                })
                .sum::<f64>()
                / (count - 1) as f64
        } else {
            0.0
        };

        let mut sorted: Vec<f32> = samples.to_vec();
        sorted.sort_unstable_by(|a, b| a.total_cmp(b));
        let percentiles = PERCENTILES
            .iter()
            .map(|&p| (p, percentile_sorted(&sorted, p)))
            .collect();

        let charge_uah = mean * duration_secs / 3600.0;
        let avg_power_uw = mean * voltage_mv as f64 / 1000.0;

        Self {
            count,
            duration_secs,
            min,
            max,
            mean,
            std_dev: var.sqrt(),
            percentiles,
            charge_uah,
            avg_power_uw,
        }
    }
}

/// Percentile (0..=100) of an ascending-sorted slice, with linear
/// interpolation between ranks.
pub fn percentile_sorted(sorted: &[f32], pct: f64) -> f64 {
    match sorted.len() {
        0 => f64::NAN,
        1 => sorted[0] as f64,
        n => {
            let rank = (pct / 100.0).clamp(0.0, 1.0) * (n - 1) as f64;
            let lo = rank.floor() as usize;
            let hi = rank.ceil() as usize;
            let frac = rank - lo as f64;
            sorted[lo] as f64 * (1.0 - frac) + sorted[hi] as f64 * frac
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_interpolation() {
        let sorted = [0.0f32, 10.0, 20.0, 30.0, 40.0];
        assert_eq!(percentile_sorted(&sorted, 0.0), 0.0);
        assert_eq!(percentile_sorted(&sorted, 100.0), 40.0);
        assert_eq!(percentile_sorted(&sorted, 50.0), 20.0);
        assert_eq!(percentile_sorted(&sorted, 25.0), 10.0);
        assert!((percentile_sorted(&sorted, 90.0) - 36.0).abs() < 1e-9);
    }

    #[test]
    fn basic_stats() {
        let samples = [1.0f32, 2.0, 3.0, 4.0];
        let s = Stats::from_samples(&samples, 2, 3000);
        assert_eq!(s.count, 4);
        assert!((s.duration_secs - 2.0).abs() < 1e-9);
        assert_eq!(s.min, 1.0);
        assert_eq!(s.max, 4.0);
        assert!((s.mean - 2.5).abs() < 1e-9);
        // Sample std dev of 1,2,3,4 is sqrt(5/3).
        assert!((s.std_dev - (5.0f64 / 3.0).sqrt()).abs() < 1e-9);
        // 2.5 µA for 2 s = 5 µA·s = 5/3600 µAh.
        assert!((s.charge_uah - 5.0 / 3600.0).abs() < 1e-12);
        // 2.5 µA * 3.0 V = 7.5 µW.
        assert!((s.avg_power_uw - 7.5).abs() < 1e-9);
    }

    #[test]
    fn empty_is_nan_not_panic() {
        let s = Stats::from_samples(&[], 1000, 3000);
        assert_eq!(s.count, 0);
        assert!(s.mean.is_nan());
    }
}
