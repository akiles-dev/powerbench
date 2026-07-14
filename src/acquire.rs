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
    if config.sps == 0 || config.sps > PPK2_RAW_SPS {
        return Err(Error::InvalidConfig(format!(
            "sps must be in 1..={PPK2_RAW_SPS}, got {}",
            config.sps
        )));
    }
    if !PPK2_RAW_SPS.is_multiple_of(config.sps) {
        return Err(Error::InvalidConfig(format!(
            "sps must divide {PPK2_RAW_SPS} evenly (e.g. 10, 100, 1000, 10000, 100000), got {}",
            config.sps
        )));
    }
    if !(800..=5000).contains(&config.voltage_mv) {
        return Err(Error::InvalidConfig(format!(
            "voltage must be in 800..=5000 mV, got {}",
            config.voltage_mv
        )));
    }
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

trait DurationExt {
    fn mulf(&self, f: f64) -> Duration;
}

impl DurationExt for Duration {
    fn mulf(&self, f: f64) -> Duration {
        Duration::from_secs_f64(self.as_secs_f64() * f)
    }
}
