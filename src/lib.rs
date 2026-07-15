//! Structured low-power benchmarking with the Nordic Power Profiler Kit II (PPK2).
//!
//! This crate provides both a library and a CLI (the `powerbench` binary) for:
//!
//! - Recording current measurements from a PPK2 into a self-describing sample
//!   file ([`acquire`], [`format`]).
//! - Live monitoring with a moving average over the last N seconds
//!   ([`acquire::live`]).
//! - Computing summary statistics over a recording ([`stats`]).
//! - Comparing two recordings and deciding whether the difference is
//!   statistically significant ([`compare`]).
//! - Plotting two recordings for visual comparison using gnuplot ([`plot`]).
//!
//! To use as a library without the CLI dependencies:
//!
//! ```toml
//! [dependencies]
//! powerbench = { version = "0.1", default-features = false }
//! ```
//!
//! Example: record a 30 second benchmark at 3.0 V and print the mean current.
//!
//! ```no_run
//! use powerbench::acquire::{record, RecordConfig};
//! use powerbench::stats::Stats;
//!
//! let config = RecordConfig {
//!     voltage_mv: 3000,
//!     settle: std::time::Duration::from_secs(5),
//!     duration: std::time::Duration::from_secs(30),
//!     ..Default::default()
//! };
//! let recording = record(&config, None, |_| {}).unwrap();
//! recording.save("baseline.pbench").unwrap();
//! let stats = Stats::compute(&recording);
//! println!("mean: {:.1} uA", stats.mean);
//! ```

pub mod acquire;
pub mod compare;
pub mod device;
pub mod format;
pub mod plot;
pub mod stats;

pub use acquire::{live, record, LiveConfig, LiveSummary, LiveUpdate, RecordConfig};
pub use compare::{compare, CompareConfig, CompareResult};
pub use device::Device;
pub use format::{Meta, Recording};
pub use stats::Stats;

/// Errors returned by this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error communicating with the PPK2.
    #[error("PPK2 error: {0}")]
    Ppk2(#[from] ppk2::Error),
    /// Serial port error.
    #[error("serial port error: {0}")]
    Serial(#[from] serialport::Error),
    /// Error talking to the PPK2 device.
    #[error("device error: {0}")]
    Device(String),
    /// File I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not a powerbench sample file, or uses an unsupported version.
    #[error("invalid sample file: {0}")]
    InvalidFile(String),
    /// Invalid configuration or arguments.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    /// The recording was cancelled before completing.
    #[error("recording cancelled")]
    Cancelled,
    /// The measurement stream stalled or ended before enough samples arrived.
    ///
    /// `raw_bytes` and `missed` help tell apart a paused stream (few bytes,
    /// no misses: the device stopped sending) from a lossy one (plenty of
    /// bytes but many misses: data was dropped between device and host).
    #[error(
        "measurement incomplete: got {got} of {expected} samples \
         ({raw_bytes} stream bytes read, {missed} raw samples missed)"
    )]
    Incomplete {
        /// Number of samples received.
        got: usize,
        /// Number of samples that were expected.
        expected: usize,
        /// Raw stream bytes read from the device.
        raw_bytes: u64,
        /// Raw samples the stream's counter field reported as missed.
        missed: u64,
    },
    /// The device is delivering its sample stream far below the nominal
    /// 400 KB/s (100 ksps), so the recording cannot complete in real time.
    #[error(
        "PPK2 stream rate too low: {bytes_per_sec} B/s instead of ~400000 B/s (100 ksps). \
         The USB path or device is pacing the stream. Try a different USB port — plug the \
         PPK2 directly into a root port; full-speed devices behind USB 2.0 hubs/docks are \
         commonly throttled exactly like this."
    )]
    StreamRate {
        /// Observed stream rate in bytes per second.
        bytes_per_sec: u64,
    },
    /// Not enough data to perform the requested analysis.
    #[error("not enough data: {0}")]
    NotEnoughData(String),
    /// Error running gnuplot.
    #[error("gnuplot failed: {0}")]
    Gnuplot(String),
}

/// Convenience result type.
pub type Result<T> = std::result::Result<T, Error>;
