//! Statistical comparison of two recordings.
//!
//! Consecutive PPK2 samples are strongly autocorrelated (the device under
//! test cycles through activity states much slower than the sample rate), so
//! testing raw samples against each other would wildly overstate confidence.
//! Instead, each recording is split into fixed-length blocks (default 1 s)
//! and inference is done on the block means, which are approximately
//! independent as long as the block length exceeds the device's activity
//! cycle. Two views on the same question are reported:
//!
//! - Welch's t-test on the block means (p-value).
//! - A bootstrap confidence interval for the difference of means, using a
//!   deterministic seed by default so CI runs are reproducible.

use serde::Serialize;
use statrs::distribution::{ContinuousCDF, StudentsT};

use crate::format::Recording;
use crate::stats::percentile_sorted;
use crate::{Error, Result};

/// Configuration for [`compare`].
#[derive(Debug, Clone)]
pub struct CompareConfig {
    /// Block length in seconds used to derive approximately independent
    /// observations from the sample stream.
    pub block_secs: f64,
    /// Confidence level for the bootstrap interval and significance decision,
    /// e.g. 0.95 or 0.99.
    pub confidence: f64,
    /// Number of bootstrap resamples.
    pub bootstrap_iters: u32,
    /// Seed for the bootstrap RNG. Fixed by default for reproducibility.
    pub seed: u64,
}

impl Default for CompareConfig {
    fn default() -> Self {
        Self {
            block_secs: 1.0,
            confidence: 0.95,
            bootstrap_iters: 10_000,
            seed: 0x9e3779b97f4a7c15,
        }
    }
}

/// Side-by-side percentile comparison, in µA.
#[derive(Debug, Clone, Serialize)]
pub struct PercentileDiff {
    /// The percentile (0..=100).
    pub percentile: f64,
    /// Value in the baseline recording (µA).
    pub baseline: f64,
    /// Value in the new recording (µA).
    pub new: f64,
}

/// Result of comparing two recordings. All currents are in microampere.
/// "Difference" always means `new - baseline`.
#[derive(Debug, Clone, Serialize)]
pub struct CompareResult {
    /// Mean current of the baseline recording.
    pub mean_baseline: f64,
    /// Mean current of the new recording.
    pub mean_new: f64,
    /// Difference of means (µA).
    pub mean_diff: f64,
    /// Difference of means as a percentage of the baseline mean.
    pub mean_diff_pct: f64,
    /// Number of blocks in the baseline recording.
    pub blocks_baseline: usize,
    /// Number of blocks in the new recording.
    pub blocks_new: usize,
    /// Block length in seconds.
    pub block_secs: f64,
    /// Welch's t statistic on the block means.
    pub t_statistic: f64,
    /// Welch–Satterthwaite degrees of freedom.
    pub degrees_of_freedom: f64,
    /// Two-sided p-value for the hypothesis "the means are equal".
    pub p_value: f64,
    /// Confidence level used for the interval and the significance decision.
    pub confidence: f64,
    /// Bootstrap confidence interval for the difference of means (µA).
    pub diff_ci: (f64, f64),
    /// Bootstrap confidence interval for the difference, as a percentage of
    /// the baseline mean.
    pub diff_ci_pct: (f64, f64),
    /// Whether the difference is statistically significant at the requested
    /// confidence level (p-value below alpha and the bootstrap interval
    /// excludes zero).
    pub significant: bool,
    /// Percentile comparison of the raw sample distributions.
    pub percentiles: Vec<PercentileDiff>,
}

/// Compare two recordings; see the module documentation for methodology.
pub fn compare(
    baseline: &Recording,
    new: &Recording,
    config: &CompareConfig,
) -> Result<CompareResult> {
    if !(config.confidence > 0.5 && config.confidence < 1.0) {
        return Err(Error::InvalidConfig(format!(
            "confidence must be in (0.5, 1.0), got {}",
            config.confidence
        )));
    }
    if config.block_secs <= 0.0 {
        return Err(Error::InvalidConfig("block length must be positive".into()));
    }

    let a = block_means(&baseline.samples_ua, baseline.meta.sps, config.block_secs)?;
    let b = block_means(&new.samples_ua, new.meta.sps, config.block_secs)?;

    let mean_a = mean(&a);
    let mean_b = mean(&b);
    let mean_diff = mean_b - mean_a;

    let (t, dof, p_value) = welch_t_test(&a, &b);

    let (ci_lo, ci_hi) = bootstrap_diff_ci(
        &a,
        &b,
        config.bootstrap_iters,
        config.confidence,
        config.seed,
    );

    let alpha = 1.0 - config.confidence;
    let significant = p_value < alpha && (ci_lo > 0.0 || ci_hi < 0.0);

    let pct_of_baseline = |x: f64| {
        if mean_a != 0.0 {
            100.0 * x / mean_a
        } else {
            f64::NAN
        }
    };

    let mut sorted_a = baseline.samples_ua.clone();
    let mut sorted_b = new.samples_ua.clone();
    sorted_a.sort_unstable_by(|x, y| x.total_cmp(y));
    sorted_b.sort_unstable_by(|x, y| x.total_cmp(y));
    let percentiles = crate::stats::PERCENTILES
        .iter()
        .map(|&p| PercentileDiff {
            percentile: p,
            baseline: percentile_sorted(&sorted_a, p),
            new: percentile_sorted(&sorted_b, p),
        })
        .collect();

    Ok(CompareResult {
        mean_baseline: mean_a,
        mean_new: mean_b,
        mean_diff,
        mean_diff_pct: pct_of_baseline(mean_diff),
        blocks_baseline: a.len(),
        blocks_new: b.len(),
        block_secs: config.block_secs,
        t_statistic: t,
        degrees_of_freedom: dof,
        p_value,
        confidence: config.confidence,
        diff_ci: (ci_lo, ci_hi),
        diff_ci_pct: (pct_of_baseline(ci_lo), pct_of_baseline(ci_hi)),
        significant,
        percentiles,
    })
}

/// Split samples into consecutive blocks of `block_secs` and return the mean
/// of each full block.
fn block_means(samples: &[f32], sps: u32, block_secs: f64) -> Result<Vec<f64>> {
    let block_len = (sps as f64 * block_secs).round() as usize;
    if block_len == 0 {
        return Err(Error::InvalidConfig(format!(
            "block length {block_secs} s is shorter than one sample at {sps} sps"
        )));
    }
    let means: Vec<f64> = samples
        .chunks_exact(block_len)
        .map(|c| c.iter().map(|&s| s as f64).sum::<f64>() / block_len as f64)
        .collect();
    if means.len() < 2 {
        return Err(Error::NotEnoughData(format!(
            "need at least 2 full blocks of {block_secs} s ({} samples at {sps} sps) per file, \
             got {}; record longer or use a smaller --block-secs",
            block_len,
            means.len()
        )));
    }
    Ok(means)
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

fn variance(xs: &[f64], mean: f64) -> f64 {
    xs.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / (xs.len() - 1) as f64
}

/// Welch's unequal-variances t-test. Returns (t, degrees of freedom,
/// two-sided p-value).
fn welch_t_test(a: &[f64], b: &[f64]) -> (f64, f64, f64) {
    let (na, nb) = (a.len() as f64, b.len() as f64);
    let (ma, mb) = (mean(a), mean(b));
    let (va, vb) = (variance(a, ma), variance(b, mb));

    let se2 = va / na + vb / nb;
    if se2 <= 0.0 {
        // Both groups constant: identical means -> p = 1, otherwise the
        // difference is exact -> p = 0.
        let p = if ma == mb { 1.0 } else { 0.0 };
        return (f64::INFINITY * (mb - ma).signum(), na + nb - 2.0, p);
    }
    let t = (mb - ma) / se2.sqrt();
    let dof = se2 * se2 / ((va / na) * (va / na) / (na - 1.0) + (vb / nb) * (vb / nb) / (nb - 1.0));

    let dist = StudentsT::new(0.0, 1.0, dof).expect("dof is positive and finite");
    let p = 2.0 * (1.0 - dist.cdf(t.abs()));
    (t, dof, p.clamp(0.0, 1.0))
}

/// Percentile-method bootstrap confidence interval for `mean(b) - mean(a)`.
fn bootstrap_diff_ci(a: &[f64], b: &[f64], iters: u32, confidence: f64, seed: u64) -> (f64, f64) {
    let mut rng = SplitMix64::new(seed);
    let mut diffs = Vec::with_capacity(iters as usize);
    for _ in 0..iters {
        let ra = resample_mean(a, &mut rng);
        let rb = resample_mean(b, &mut rng);
        diffs.push(rb - ra);
    }
    diffs.sort_unstable_by(|x, y| x.total_cmp(y));
    let alpha = 1.0 - confidence;
    let lo_rank = (alpha / 2.0) * (diffs.len() - 1) as f64;
    let hi_rank = (1.0 - alpha / 2.0) * (diffs.len() - 1) as f64;
    (interp(&diffs, lo_rank), interp(&diffs, hi_rank))
}

fn interp(sorted: &[f64], rank: f64) -> f64 {
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = rank - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}

fn resample_mean(xs: &[f64], rng: &mut SplitMix64) -> f64 {
    let n = xs.len();
    let mut sum = 0.0;
    for _ in 0..n {
        sum += xs[(rng.next() % n as u64) as usize];
    }
    sum / n as f64
}

/// Small deterministic PRNG (SplitMix64); good enough for bootstrap
/// resampling and keeps the dependency tree small.
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{Meta, Recording};

    fn recording(samples: Vec<f32>, sps: u32) -> Recording {
        Recording {
            meta: Meta {
                created_unix_ms: 0,
                tool_version: "test".into(),
                label: None,
                voltage_mv: 3000,
                sps,
                settle_secs: 0.0,
                duration_secs: samples.len() as f64 / sps as f64,
                mode: "source".into(),
                calibrated: true,
                missed_samples: 0,
            },
            samples_ua: samples,
        }
    }

    /// Deterministic pseudo-noise in [-0.5, 0.5).
    fn noise(i: usize) -> f32 {
        let mut rng = SplitMix64::new(i as u64 + 42);
        (rng.next() % 1000) as f32 / 1000.0 - 0.5
    }

    #[test]
    fn detects_real_shift() {
        // 20 blocks of 100 samples each; new is 10% higher than baseline.
        let a: Vec<f32> = (0..2000).map(|i| 100.0 + noise(i)).collect();
        let b: Vec<f32> = (0..2000).map(|i| 110.0 + noise(i + 7777)).collect();
        let res = compare(
            &recording(a, 100),
            &recording(b, 100),
            &CompareConfig::default(),
        )
        .unwrap();
        assert!(res.significant, "10% shift must be significant: {res:?}");
        assert!(res.mean_diff > 9.0 && res.mean_diff < 11.0);
        assert!(res.p_value < 0.001);
        assert!(res.diff_ci.0 > 0.0);
    }

    #[test]
    fn no_false_positive_on_identical_distributions() {
        let a: Vec<f32> = (0..2000).map(|i| 100.0 + noise(i)).collect();
        let b: Vec<f32> = (0..2000).map(|i| 100.0 + noise(i + 31337)).collect();
        let res = compare(
            &recording(a, 100),
            &recording(b, 100),
            &CompareConfig {
                confidence: 0.99,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !res.significant,
            "same distribution flagged as significant: {res:?}"
        );
    }

    #[test]
    fn too_short_recording_is_an_error() {
        let a: Vec<f32> = vec![1.0; 150]; // 1.5 blocks at 100 sps
        let b: Vec<f32> = vec![1.0; 500];
        let res = compare(
            &recording(a, 100),
            &recording(b, 100),
            &CompareConfig::default(),
        );
        assert!(matches!(res, Err(Error::NotEnoughData(_))));
    }

    #[test]
    fn bootstrap_is_deterministic() {
        let a: Vec<f32> = (0..1000).map(|i| 100.0 + noise(i)).collect();
        let b: Vec<f32> = (0..1000).map(|i| 101.0 + noise(i + 5)).collect();
        let cfg = CompareConfig::default();
        let r1 = compare(&recording(a.clone(), 100), &recording(b.clone(), 100), &cfg).unwrap();
        let r2 = compare(&recording(a, 100), &recording(b, 100), &cfg).unwrap();
        assert_eq!(r1.diff_ci, r2.diff_ci);
    }
}
