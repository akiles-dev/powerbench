//! Recording samples from a PPK2.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use ppk2::measurement::{Measurement, MeasurementAccumulator};
use ppk2::types::MeasurementMode;

use crate::device::Device;
use crate::format::{Meta, Recording};
use crate::{Error, Result};

/// The PPK2 ADC sample rate; requested sample rates are produced by averaging
/// chunks of this stream.
pub const PPK2_RAW_SPS: u32 = 100_000;

/// Configuration for a recording.
#[derive(Debug, Clone)]
pub struct RecordConfig {
    /// Serial port of the PPK2. Autodetected when `None`.
    pub port: Option<String>,
    /// Source voltage in millivolt (800..=5000).
    pub voltage_mv: u16,
    /// Measurement mode. In `Source` mode the PPK2 powers the device under
    /// test; in `Ampere` mode it acts as an ammeter in series.
    pub mode: MeasurementMode,
    /// Time to wait between enabling power and starting to sample, letting the
    /// device under test boot and settle.
    pub settle: Duration,
    /// How long to sample.
    pub duration: Duration,
    /// Samples per second to record (1..=100_000). Each sample is the average
    /// of `100_000 / sps` raw ADC samples.
    pub sps: u32,
    /// Keep the device powered after the recording finishes.
    pub keep_power: bool,
    /// Optional label stored in the sample file.
    pub label: Option<String>,
}

impl Default for RecordConfig {
    fn default() -> Self {
        Self {
            port: None,
            voltage_mv: 3000,
            mode: MeasurementMode::Source,
            settle: Duration::from_secs(0),
            duration: Duration::from_secs(10),
            sps: 1000,
            keep_power: false,
            label: None,
        }
    }
}

/// Progress notifications delivered to the callback passed to [`record`].
#[derive(Debug, Clone, Copy)]
pub enum Progress {
    /// Power was enabled; waiting for the device to settle.
    Settling {
        /// Seconds of settle time remaining.
        remaining_secs: f64,
    },
    /// Sampling is in progress.
    Sampling {
        /// Samples collected so far.
        collected: usize,
        /// Total number of samples that will be collected.
        total: usize,
    },
}

fn validate_sps(sps: u32) -> Result<()> {
    if sps == 0 || sps > PPK2_RAW_SPS {
        return Err(Error::InvalidConfig(format!(
            "sps must be in 1..={PPK2_RAW_SPS}, got {sps}"
        )));
    }
    if !PPK2_RAW_SPS.is_multiple_of(sps) {
        return Err(Error::InvalidConfig(format!(
            "sps must divide {PPK2_RAW_SPS} evenly (e.g. 10, 100, 1000, 10000, 100000), got {sps}"
        )));
    }
    Ok(())
}

fn validate_voltage(voltage_mv: u16) -> Result<()> {
    if !(800..=5000).contains(&voltage_mv) {
        return Err(Error::InvalidConfig(format!(
            "voltage must be in 800..=5000 mV, got {voltage_mv}"
        )));
    }
    Ok(())
}

/// Record samples from a PPK2 according to `config`.
///
/// Timing: the settle period is measured from the moment power is enabled,
/// and the measurement stream is not started until it has fully elapsed —
/// the first sample in the recording is taken `settle` seconds after
/// power-on, so device boot-up current is never part of the capture (choose
/// a settle time longer than the device's boot time).
///
/// Blocks until the recording completes. If `cancel` is provided and becomes
/// `true`, the recording is aborted cleanly (measurement stopped, power turned
/// off unless `keep_power`) and [`Error::Cancelled`] is returned.
///
/// `progress` is invoked roughly once per second; pass `|_| {}` to ignore.
pub fn record(
    config: &RecordConfig,
    cancel: Option<&AtomicBool>,
    mut progress: impl FnMut(Progress),
) -> Result<Recording> {
    validate_sps(config.sps)?;
    validate_voltage(config.voltage_mv)?;
    let is_cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));

    let mut dev = Device::open(config.port.as_deref(), config.mode)?;
    // Safety net: if this function exits on any early error or panic, the
    // Device drop handler stops sampling and powers the DUT off.
    dev.keep_power_on_drop(config.keep_power);
    let calibrated = dev.metadata().calibrated;
    dev.set_source_voltage(config.voltage_mv)?;
    dev.set_device_power(true)?;

    let power_off = |dev: &mut Device| -> Result<()> {
        if !config.keep_power {
            dev.set_device_power(false)?;
        }
        Ok(())
    };

    // Settle phase: power is on, but we are not sampling yet.
    let settle_start = Instant::now();
    while settle_start.elapsed() < config.settle {
        if is_cancelled() {
            power_off(&mut dev)?;
            return Err(Error::Cancelled);
        }
        progress(Progress::Settling {
            remaining_secs: (config.settle - settle_start.elapsed()).as_secs_f64(),
        });
        let remaining = config.settle - settle_start.elapsed();
        std::thread::sleep(remaining.min(Duration::from_millis(200)));
    }

    let created_unix_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // Each recorded sample is the average of `chunk` raw stream samples.
    let chunk = (PPK2_RAW_SPS / config.sps) as usize;
    let total = (config.duration.as_secs_f64() * config.sps as f64).round() as usize;
    let mut samples_ua: Vec<f32> = Vec::with_capacity(total);
    let mut pending: VecDeque<Measurement> = VecDeque::with_capacity(chunk + 4096);
    let mut accumulator = MeasurementAccumulator::new(dev.metadata().clone());
    let mut missed_samples: u64 = 0;

    dev.start_sampling()?;

    let start = Instant::now();
    // Allow some slack beyond the nominal duration before declaring the
    // stream stalled: chunk timing jitters and the first chunk is delayed.
    let deadline = start + config.duration.mulf(1.25) + Duration::from_secs(2);
    let mut last_progress = Instant::now();
    let mut buf = [0u8; 4096];
    let mut raw_bytes: u64 = 0;
    let mut rate_checked = false;
    let result = loop {
        if samples_ua.len() >= total {
            break Ok(());
        }
        if is_cancelled() {
            break Err(Error::Cancelled);
        }
        if Instant::now() > deadline {
            break Err(Error::Incomplete {
                got: samples_ua.len(),
                expected: total,
                raw_bytes,
                missed: missed_samples,
            });
        }
        // The raw stream is nominally 400 KB/s regardless of the requested
        // sps. Bail out early with a diagnosis if it is far below that,
        // instead of timing out at the deadline.
        let elapsed = start.elapsed();
        if !rate_checked && elapsed >= Duration::from_secs(2) {
            rate_checked = true;
            let rate = raw_bytes / elapsed.as_secs().max(1);
            if rate < 4 * PPK2_RAW_SPS as u64 * 9 / 10 {
                break Err(Error::StreamRate {
                    bytes_per_sec: rate,
                });
            }
        }
        if last_progress.elapsed() >= Duration::from_secs(1) {
            last_progress = Instant::now();
            progress(Progress::Sampling {
                collected: samples_ua.len(),
                total,
            });
        }
        match dev.read_stream(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                raw_bytes += n as u64;
                missed_samples += accumulator.feed_into(&buf[..n], &mut pending) as u64;
                // One read may complete several chunks; average each full
                // chunk into one recorded sample.
                while pending.len() >= chunk && samples_ua.len() < total {
                    let sum: f64 = pending.drain(..chunk).map(|m| m.micro_amps as f64).sum();
                    samples_ua.push((sum / chunk as f64) as f32);
                }
            }
            Err(e) => break Err(e),
        }
    };

    // Best-effort stream stop; don't mask the loop result if the port is in
    // a bad state, but do propagate a failure to power the device down.
    dev.stop_sampling().ok();
    power_off(&mut dev)?;
    result?;

    Ok(Recording {
        meta: Meta {
            created_unix_ms,
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            label: config.label.clone(),
            voltage_mv: config.voltage_mv,
            sps: config.sps,
            settle_secs: config.settle.as_secs_f64(),
            duration_secs: config.duration.as_secs_f64(),
            mode: match config.mode {
                MeasurementMode::Source => "source".to_string(),
                MeasurementMode::Ampere => "ampere".to_string(),
            },
            calibrated,
            missed_samples,
        },
        samples_ua,
    })
}

/// Configuration for live monitoring ([`live`]).
#[derive(Debug, Clone)]
pub struct LiveConfig {
    /// Serial port of the PPK2. Autodetected when `None`.
    pub port: Option<String>,
    /// Source voltage in millivolt (800..=5000).
    pub voltage_mv: u16,
    /// Measurement mode. In `Source` mode the PPK2 powers the device under
    /// test; in `Ampere` mode it acts as an ammeter in series.
    pub mode: MeasurementMode,
    /// Samples per second underlying the window (1..=100_000). Each sample is
    /// the average of `100_000 / sps` raw ADC samples; the window's min/max
    /// are taken over these samples, so a higher sps resolves shorter spikes.
    pub sps: u32,
    /// Length of the moving-average window.
    pub window: Duration,
    /// How often the update callback is invoked.
    pub update_interval: Duration,
    /// Keep the device powered after monitoring stops.
    pub keep_power: bool,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            port: None,
            voltage_mv: 3000,
            mode: MeasurementMode::Source,
            sps: 1000,
            window: Duration::from_secs(10),
            update_interval: Duration::from_millis(500),
            keep_power: false,
        }
    }
}

/// A periodic snapshot delivered to the callback passed to [`live`].
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct LiveUpdate {
    /// Seconds since sampling started.
    pub elapsed_secs: f64,
    /// Mean current in µA over the samples since the previous update.
    pub now_ua: f64,
    /// Seconds of data currently in the window (less than the configured
    /// window length until it has filled once).
    pub window_secs: f64,
    /// Moving average in µA over the window.
    pub window_mean_ua: f64,
    /// Minimum sample in µA in the window.
    pub window_min_ua: f64,
    /// Maximum sample in µA in the window.
    pub window_max_ua: f64,
    /// Median (50th percentile) sample in µA in the window.
    pub window_p50_ua: f64,
    /// 95th percentile sample in µA in the window.
    pub window_p95_ua: f64,
    /// 99th percentile sample in µA in the window.
    pub window_p99_ua: f64,
    /// Mean current in µA since sampling started.
    pub total_mean_ua: f64,
    /// Charge in µAh consumed since sampling started.
    pub total_charge_uah: f64,
    /// Raw samples the stream's counter field reported as missed so far.
    pub missed_samples: u64,
}

/// Summary of a whole [`live`] monitoring session.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct LiveSummary {
    /// Seconds of data monitored.
    pub duration_secs: f64,
    /// Number of samples (at the configured sps).
    pub samples: u64,
    /// Mean current in µA over the whole session.
    pub mean_ua: f64,
    /// Minimum sample in µA over the whole session.
    pub min_ua: f64,
    /// Maximum sample in µA over the whole session.
    pub max_ua: f64,
    /// Charge in µAh consumed over the whole session.
    pub charge_uah: f64,
    /// Average power in µW over the whole session (at the source voltage).
    pub avg_power_uw: f64,
    /// Raw samples the stream's counter field reported as missed.
    pub missed_samples: u64,
}

/// Monitor current draw live, maintaining a moving average over the most
/// recent `config.window` of samples.
///
/// Powers the device, samples indefinitely, and invokes `on_update` every
/// `config.update_interval` with the window statistics. Runs until `cancel`
/// becomes `true` — unlike [`record`], cancellation is the normal way to end
/// a live session and is not an error. On return the stream is stopped and
/// power is turned off (unless `keep_power`), and statistics over the whole
/// session are returned.
pub fn live(
    config: &LiveConfig,
    cancel: Option<&AtomicBool>,
    mut on_update: impl FnMut(&LiveUpdate),
) -> Result<LiveSummary> {
    validate_sps(config.sps)?;
    validate_voltage(config.voltage_mv)?;
    let window_len = (config.window.as_secs_f64() * config.sps as f64).round() as usize;
    if window_len == 0 {
        return Err(Error::InvalidConfig(format!(
            "window of {} s holds no samples at {} sps",
            config.window.as_secs_f64(),
            config.sps
        )));
    }
    let is_cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));

    let mut dev = Device::open(config.port.as_deref(), config.mode)?;
    dev.keep_power_on_drop(config.keep_power);
    dev.set_source_voltage(config.voltage_mv)?;
    dev.set_device_power(true)?;

    let chunk = (PPK2_RAW_SPS / config.sps) as usize;
    let mut window: VecDeque<f32> = VecDeque::with_capacity(window_len);
    // Scratch buffer the window is sorted into for percentiles, reused
    // across updates.
    let mut sorted: Vec<f32> = Vec::with_capacity(window_len);
    let mut pending: VecDeque<Measurement> = VecDeque::with_capacity(chunk + 4096);
    let mut accumulator = MeasurementAccumulator::new(dev.metadata().clone());
    let mut missed_samples: u64 = 0;

    // Whole-session aggregates; the window only holds the recent past.
    let mut total_count: u64 = 0;
    let mut total_sum: f64 = 0.0;
    let mut total_min = f64::INFINITY;
    let mut total_max = f64::NEG_INFINITY;
    // Aggregates since the last update, for the "now" reading.
    let mut since_count: u64 = 0;
    let mut since_sum: f64 = 0.0;

    dev.start_sampling()?;

    let start = Instant::now();
    let mut last_update = Instant::now();
    let mut last_data = Instant::now();
    let mut buf = [0u8; 4096];
    let mut raw_bytes: u64 = 0;
    let mut rate_checked = false;
    let result = loop {
        if is_cancelled() {
            break Ok(());
        }
        // The raw stream is nominally 400 KB/s regardless of the requested
        // sps. Bail out early with a diagnosis if it is far below that.
        let elapsed = start.elapsed();
        if !rate_checked && elapsed >= Duration::from_secs(2) {
            rate_checked = true;
            let rate = raw_bytes / elapsed.as_secs().max(1);
            if rate < 4 * PPK2_RAW_SPS as u64 * 9 / 10 {
                break Err(Error::StreamRate {
                    bytes_per_sec: rate,
                });
            }
        }
        if last_data.elapsed() > Duration::from_secs(3) {
            break Err(Error::Device(
                "measurement stream stalled: no data for 3 s".into(),
            ));
        }
        match dev.read_stream(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                last_data = Instant::now();
                raw_bytes += n as u64;
                missed_samples += accumulator.feed_into(&buf[..n], &mut pending) as u64;
                while pending.len() >= chunk {
                    let sum: f64 = pending.drain(..chunk).map(|m| m.micro_amps as f64).sum();
                    let sample = (sum / chunk as f64) as f32;
                    if window.len() == window_len {
                        window.pop_front();
                    }
                    window.push_back(sample);
                    total_count += 1;
                    total_sum += sample as f64;
                    total_min = total_min.min(sample as f64);
                    total_max = total_max.max(sample as f64);
                    since_count += 1;
                    since_sum += sample as f64;
                }
            }
            Err(e) => break Err(e),
        }
        if since_count > 0 && last_update.elapsed() >= config.update_interval {
            last_update = Instant::now();
            sorted.clear();
            sorted.extend(window.iter().copied());
            sorted.sort_unstable_by(|a, b| a.total_cmp(b));
            let sum: f64 = sorted.iter().map(|&s| s as f64).sum();
            on_update(&LiveUpdate {
                elapsed_secs: start.elapsed().as_secs_f64(),
                now_ua: since_sum / since_count as f64,
                window_secs: window.len() as f64 / config.sps as f64,
                window_mean_ua: sum / sorted.len() as f64,
                window_min_ua: sorted[0] as f64,
                window_max_ua: sorted[sorted.len() - 1] as f64,
                window_p50_ua: crate::stats::percentile_sorted(&sorted, 50.0),
                window_p95_ua: crate::stats::percentile_sorted(&sorted, 95.0),
                window_p99_ua: crate::stats::percentile_sorted(&sorted, 99.0),
                total_mean_ua: total_sum / total_count as f64,
                total_charge_uah: total_sum / config.sps as f64 / 3600.0,
                missed_samples,
            });
            since_count = 0;
            since_sum = 0.0;
        }
    };

    dev.stop_sampling().ok();
    if !config.keep_power {
        dev.set_device_power(false)?;
    }
    result?;

    Ok(LiveSummary {
        duration_secs: total_count as f64 / config.sps as f64,
        samples: total_count,
        mean_ua: if total_count > 0 {
            total_sum / total_count as f64
        } else {
            0.0
        },
        min_ua: if total_count > 0 { total_min } else { 0.0 },
        max_ua: if total_count > 0 { total_max } else { 0.0 },
        charge_uah: total_sum / config.sps as f64 / 3600.0,
        avg_power_uw: if total_count > 0 {
            total_sum / total_count as f64 * config.voltage_mv as f64 / 1000.0
        } else {
            0.0
        },
        missed_samples,
    })
}

trait DurationExt {
    fn mulf(&self, f: f64) -> Duration;
}

impl DurationExt for Duration {
    fn mulf(&self, f: f64) -> Duration {
        Duration::from_secs_f64(self.as_secs_f64() * f)
    }
}
