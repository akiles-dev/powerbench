use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use powerbench::acquire::{live, record, LiveConfig, LiveSummary, Progress, RecordConfig};
use powerbench::compare::{compare, CompareConfig, CompareResult};
use powerbench::device::Device;
use powerbench::format::Recording;
use powerbench::plot::{plot, PlotConfig, Scale};
use powerbench::stats::Stats;
use ppk2::types::MeasurementMode;

/// Structured low-power benchmarking with the Nordic Power Profiler Kit II.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    /// The PPK2 powers the device under test.
    Source,
    /// The PPK2 measures current in series (ammeter mode).
    Ampere,
}

#[derive(Subcommand)]
enum Cmd {
    /// Record a measurement to a sample file.
    Measure {
        /// Source voltage in millivolt (800..=5000).
        #[arg(short, long)]
        voltage: u16,
        /// Seconds to wait after enabling power before sampling starts,
        /// letting the device under test boot and settle.
        #[arg(long, default_value_t = 0.0)]
        settle: f64,
        /// Seconds to sample.
        #[arg(short, long)]
        duration: f64,
        /// Samples per second (must divide 100000, e.g. 10, 100, 1000, 10000, 100000).
        #[arg(short, long, default_value_t = 1000)]
        sps: u32,
        /// Output sample file.
        #[arg(short, long)]
        output: PathBuf,
        /// Serial port of the PPK2; autodetected when omitted.
        #[arg(short, long)]
        port: Option<String>,
        /// Measurement mode.
        #[arg(short, long, value_enum, default_value_t = Mode::Source)]
        mode: Mode,
        /// Keep the device powered after the recording finishes.
        #[arg(long)]
        keep_power: bool,
        /// Label stored in the sample file, e.g. a git revision.
        #[arg(short, long)]
        label: Option<String>,
        /// Print the resulting statistics as JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Monitor current draw live, showing a moving average over the last
    /// N seconds. Runs until interrupted (Ctrl-C), then prints a summary.
    Live {
        /// Source voltage in millivolt (800..=5000).
        #[arg(short, long)]
        voltage: u16,
        /// Moving-average window in seconds.
        #[arg(short, long, default_value_t = 10.0)]
        window: f64,
        /// Samples per second underlying the window (must divide 100000).
        /// The window's min/max are taken over these samples, so a higher
        /// rate resolves shorter spikes.
        #[arg(short, long, default_value_t = 1000)]
        sps: u32,
        /// Seconds between display updates.
        #[arg(short, long, default_value_t = 0.5)]
        interval: f64,
        /// Serial port of the PPK2; autodetected when omitted.
        #[arg(short, long)]
        port: Option<String>,
        /// Measurement mode.
        #[arg(short, long, value_enum, default_value_t = Mode::Source)]
        mode: Mode,
        /// Keep the device powered after monitoring stops.
        #[arg(long)]
        keep_power: bool,
        /// Print one line per update instead of rewriting a single status
        /// line, so earlier readings stay visible as a scrolling log. This
        /// is already the behavior when stdout is not a terminal.
        #[arg(long)]
        log: bool,
        /// Print one JSON object per update on stdout (JSON Lines) instead
        /// of the live status line.
        #[arg(long)]
        json: bool,
    },
    /// Turn device-under-test power on or off without sampling. The state is
    /// left as-is when the command exits.
    Power {
        #[command(subcommand)]
        state: PowerCmd,
    },
    /// Print statistics of one or more sample files.
    Stats {
        /// Sample files.
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Compare two sample files and test whether they differ significantly.
    Compare {
        /// Baseline sample file.
        baseline: PathBuf,
        /// New sample file to compare against the baseline.
        new: PathBuf,
        /// Confidence level in percent for the significance decision and the
        /// bootstrap interval.
        #[arg(short, long, default_value_t = 95.0)]
        confidence: f64,
        /// Block length in seconds; inference runs on per-block means so that
        /// blocks are approximately independent despite sample autocorrelation.
        #[arg(short, long, default_value_t = 1.0)]
        block_secs: f64,
        /// Number of bootstrap resamples.
        #[arg(long, default_value_t = 10_000)]
        bootstrap_iters: u32,
        /// Bootstrap RNG seed; fixed by default so runs are reproducible.
        #[arg(long, default_value_t = 0x9e3779b97f4a7c15)]
        seed: u64,
        /// Fail (exit code 2) if the mean increased significantly by more
        /// than this percentage. Use 0 to fail on any significant increase.
        /// Intended for CI gating.
        #[arg(long)]
        max_regression: Option<f64>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Render one or two sample files with gnuplot, to an image file or an
    /// interactive window.
    Plot {
        /// One or two sample files.
        #[arg(required = true, num_args = 1..=2)]
        files: Vec<PathBuf>,
        /// Output image; .svg renders as SVG, anything else as PNG. When
        /// omitted, the plot is shown in an interactive window instead.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Plot title; defaults to the file names.
        #[arg(short, long, default_value = "")]
        title: String,
        /// Number of time buckets for the min/mean/max envelope.
        #[arg(long, default_value_t = 1500)]
        buckets: usize,
        /// Force a logarithmic current axis. By default the axis switches to
        /// log automatically when the data spans a wide dynamic range, so a
        /// low sleep floor stays readable next to wake spikes.
        #[arg(long, conflicts_with = "linear")]
        log: bool,
        /// Force a linear current axis.
        #[arg(long)]
        linear: bool,
        /// Keep the generated gnuplot script and data files.
        #[arg(long)]
        keep_data: bool,
    },
}

#[derive(Subcommand)]
enum PowerCmd {
    /// Power the device under test at the given voltage and leave it on.
    On {
        /// Source voltage in millivolt (800..=5000).
        #[arg(short, long)]
        voltage: u16,
        /// Serial port of the PPK2; autodetected when omitted.
        #[arg(short, long)]
        port: Option<String>,
    },
    /// Turn power to the device under test off.
    Off {
        /// Serial port of the PPK2; autodetected when omitted.
        #[arg(short, long)]
        port: Option<String>,
    },
}

fn main() -> ExitCode {
    // Die quietly on a closed pipe (e.g. `powerbench stats ... | head`)
    // instead of panicking on the failed write.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    match run() {
        Ok(code) => code,
        Err(e)
            if matches!(
                e.downcast_ref::<powerbench::Error>(),
                Some(powerbench::Error::Cancelled)
            ) =>
        {
            eprintln!("cancelled; measurement stopped and PPK2 returned to idle");
            // Conventional exit code for termination by signal.
            ExitCode::from(130)
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    match Cli::parse().command {
        Cmd::Measure {
            voltage,
            settle,
            duration,
            sps,
            output,
            port,
            mode,
            keep_power,
            label,
            json,
        } => {
            let config = RecordConfig {
                port,
                voltage_mv: voltage,
                mode: match mode {
                    Mode::Source => MeasurementMode::Source,
                    Mode::Ampere => MeasurementMode::Ampere,
                },
                settle: Duration::from_secs_f64(settle),
                duration: Duration::from_secs_f64(duration),
                sps,
                keep_power,
                label,
            };

            let cancel = install_cancel_handler()?;

            eprintln!(
                "powering device at {voltage} mV, settling {settle} s, sampling {duration} s at {sps} sps"
            );
            let recording = record(&config, Some(&cancel), print_progress)?;
            end_progress();
            recording
                .save(&output)
                .with_context(|| format!("saving {}", output.display()))?;
            eprintln!(
                "wrote {} samples to {}",
                recording.samples_ua.len(),
                output.display()
            );
            if recording.meta.missed_samples > 0 {
                eprintln!(
                    "warning: {} raw samples were lost during capture (counter gaps in the \
                     stream); treat this recording with suspicion",
                    recording.meta.missed_samples
                );
            }

            let stats = Stats::compute(&recording);
            if json {
                print_stats_json(&output, &recording, &stats)?;
            } else {
                print_stats(&output, &recording, &stats);
            }
            Ok(ExitCode::SUCCESS)
        }

        Cmd::Live {
            voltage,
            window,
            sps,
            interval,
            port,
            mode,
            keep_power,
            log,
            json,
        } => {
            let config = LiveConfig {
                port,
                voltage_mv: voltage,
                mode: match mode {
                    Mode::Source => MeasurementMode::Source,
                    Mode::Ampere => MeasurementMode::Ampere,
                },
                sps,
                window: Duration::from_secs_f64(window),
                update_interval: Duration::from_secs_f64(interval),
                keep_power,
            };
            let cancel = install_cancel_handler()?;

            eprintln!(
                "powering device at {voltage} mV, monitoring at {sps} sps with a {window} s \
                 window; Ctrl-C to stop"
            );
            let interactive = !log && std::io::stdout().is_terminal();
            let summary = live(&config, Some(&cancel), |u| {
                if json {
                    // Fields are finite floats and integers; this cannot fail.
                    let line = serde_json::to_string(u).expect("serializing update");
                    println!("{line}");
                } else if interactive {
                    // Truncate to the terminal width: a wrapped line breaks
                    // the carriage-return rewrite.
                    let mut line = live_line(u);
                    if let Some(width) = terminal_width() {
                        line = line.chars().take(width).collect();
                    }
                    print!("\r\x1b[K{line}");
                    std::io::stdout().flush().ok();
                } else {
                    println!("{}", live_line(u));
                }
            })?;
            if interactive && !json {
                println!();
            }
            if json {
                println!("{}", serde_json::to_string(&summary)?);
            } else {
                print_live_summary(&summary);
            }
            Ok(ExitCode::SUCCESS)
        }

        Cmd::Power { state } => {
            match state {
                PowerCmd::On { voltage, port } => {
                    let mut dev = Device::open(port.as_deref(), MeasurementMode::Source)?;
                    // Leave the DUT powered when the handle drops on exit.
                    dev.keep_power_on_drop(true);
                    dev.set_source_voltage(voltage)?;
                    dev.set_device_power(true)?;
                    eprintln!("device power on at {voltage} mV; leaving it on");
                }
                PowerCmd::Off { port } => {
                    let mut dev = Device::open(port.as_deref(), MeasurementMode::Source)?;
                    dev.set_device_power(false)?;
                    eprintln!("device power off");
                }
            }
            Ok(ExitCode::SUCCESS)
        }

        Cmd::Stats { files, json } => {
            for file in &files {
                let recording =
                    Recording::load(file).with_context(|| format!("loading {}", file.display()))?;
                let stats = Stats::compute(&recording);
                if json {
                    print_stats_json(file, &recording, &stats)?;
                } else {
                    print_stats(file, &recording, &stats);
                }
            }
            Ok(ExitCode::SUCCESS)
        }

        Cmd::Compare {
            baseline,
            new,
            confidence,
            block_secs,
            bootstrap_iters,
            seed,
            max_regression,
            json,
        } => {
            let rec_a = Recording::load(&baseline)
                .with_context(|| format!("loading {}", baseline.display()))?;
            let rec_b =
                Recording::load(&new).with_context(|| format!("loading {}", new.display()))?;
            let config = CompareConfig {
                block_secs,
                confidence: confidence / 100.0,
                bootstrap_iters,
                seed,
            };
            let result = compare(&rec_a, &rec_b, &config)?;

            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print_compare(&baseline, &new, &result);
            }

            if let Some(threshold) = max_regression {
                if result.significant && result.mean_diff_pct > threshold {
                    eprintln!(
                        "FAIL: mean current increased {:.2}% (> {threshold}%) with {:.1}% confidence",
                        result.mean_diff_pct,
                        confidence
                    );
                    return Ok(ExitCode::from(2));
                }
            }
            Ok(ExitCode::SUCCESS)
        }

        Cmd::Plot {
            files,
            output,
            title,
            buckets,
            log,
            linear,
            keep_data,
        } => {
            let recordings: Vec<(String, Recording)> = files
                .iter()
                .map(|f| {
                    let rec =
                        Recording::load(f).with_context(|| format!("loading {}", f.display()))?;
                    let name = rec
                        .meta
                        .label
                        .clone()
                        .unwrap_or_else(|| f.display().to_string());
                    Ok((name, rec))
                })
                .collect::<anyhow::Result<_>>()?;
            let refs: Vec<(&str, &Recording)> = recordings
                .iter()
                .map(|(name, rec)| (name.as_str(), rec))
                .collect();
            let config = PlotConfig {
                title,
                buckets,
                scale: match (log, linear) {
                    (true, _) => Scale::Log,
                    (_, true) => Scale::Linear,
                    _ => Scale::Auto,
                },
                keep_data,
            };
            plot(&refs, output.as_deref(), &config)?;
            if let Some(output) = &output {
                eprintln!("wrote {}", output.display());
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Install a signal handler that requests a graceful stop via the returned
/// flag. With the "termination" feature this catches SIGINT, SIGTERM and
/// SIGHUP. The first signal sets the flag (the sampling loop notices within
/// ~500 ms, stops the stream and powers the DUT off); a second one
/// force-exits.
fn install_cancel_handler() -> anyhow::Result<Arc<AtomicBool>> {
    let cancel = Arc::new(AtomicBool::new(false));
    let c = cancel.clone();
    ctrlc::set_handler(move || {
        if c.swap(true, Ordering::Relaxed) {
            eprintln!("\nforced exit");
            std::process::exit(130);
        }
        eprintln!("\ninterrupted, shutting down...");
    })
    .context("installing signal handler")?;
    Ok(cancel)
}

fn print_progress(p: Progress) {
    let interactive = std::io::stderr().is_terminal();
    let line = match p {
        Progress::Settling { remaining_secs } => {
            format!("settling: {remaining_secs:.0} s remaining")
        }
        Progress::Sampling { collected, total } => format!(
            "sampling: {collected}/{total} ({:.0}%)",
            100.0 * collected as f64 / total as f64
        ),
    };
    if interactive {
        eprint!("\r\x1b[K{line}");
        std::io::stderr().flush().ok();
    } else {
        eprintln!("{line}");
    }
}

fn end_progress() {
    if std::io::stderr().is_terminal() {
        eprintln!();
    }
}

/// Format a current in µA with an adaptive unit.
fn fmt_ua(ua: f64) -> String {
    let abs = ua.abs();
    if !ua.is_finite() {
        format!("{ua}")
    } else if abs >= 1_000_000.0 {
        format!("{:.4} A", ua / 1_000_000.0)
    } else if abs >= 1000.0 {
        format!("{:.3} mA", ua / 1000.0)
    } else if abs >= 1.0 || ua == 0.0 {
        format!("{ua:.2} µA")
    } else {
        format!("{:.1} nA", ua * 1000.0)
    }
}

fn live_line(u: &powerbench::LiveUpdate) -> String {
    format!(
        "{:7.1}s now {:>10} avg({:.0}s) {:>10} min {:>10} p50 {:>10} p95 {:>10} \
         p99 {:>10} max {:>10} tot {:>10}",
        u.elapsed_secs,
        fmt_ua(u.now_ua),
        u.window_secs,
        fmt_ua(u.window_mean_ua),
        fmt_ua(u.window_min_ua),
        fmt_ua(u.window_p50_ua),
        fmt_ua(u.window_p95_ua),
        fmt_ua(u.window_p99_ua),
        fmt_ua(u.window_max_ua),
        fmt_ua(u.total_mean_ua),
    )
}

/// Width of the terminal on stdout, if it can be determined.
#[cfg(unix)]
fn terminal_width() -> Option<usize> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            Some(ws.ws_col as usize)
        } else {
            None
        }
    }
}

#[cfg(not(unix))]
fn terminal_width() -> Option<usize> {
    None
}

fn print_live_summary(s: &LiveSummary) {
    println!("session summary");
    println!(
        "  duration: {:.1} s ({} samples){}",
        s.duration_secs,
        s.samples,
        if s.missed_samples > 0 {
            format!(", {} raw samples missed", s.missed_samples)
        } else {
            String::new()
        }
    );
    println!(
        "  mean:     {:>12}   min:  {:>12}   max: {:>12}",
        fmt_ua(s.mean_ua),
        fmt_ua(s.min_ua),
        fmt_ua(s.max_ua)
    );
    println!(
        "  charge:   {:.4} µAh   avg power: {:.2} µW",
        s.charge_uah, s.avg_power_uw
    );
}

fn print_stats(file: &std::path::Path, rec: &Recording, s: &Stats) {
    println!("{}", file.display());
    if let Some(label) = &rec.meta.label {
        println!("  label:    {label}");
    }
    println!(
        "  capture:  {} samples at {} sps ({:.1} s), {} mV, mode {}{}",
        s.count,
        rec.meta.sps,
        s.duration_secs,
        rec.meta.voltage_mv,
        rec.meta.mode,
        if rec.meta.calibrated {
            ""
        } else {
            " (UNCALIBRATED)"
        },
    );
    println!(
        "  mean:     {:>12}   std:  {:>12}",
        fmt_ua(s.mean),
        fmt_ua(s.std_dev)
    );
    println!(
        "  min:      {:>12}   max:  {:>12}",
        fmt_ua(s.min),
        fmt_ua(s.max)
    );
    for chunk in s.percentiles.chunks(4) {
        let cells: Vec<String> = chunk
            .iter()
            .map(|(p, v)| format!("p{p:<5} {:>12}", fmt_ua(*v)))
            .collect();
        println!("  {}", cells.join("   "));
    }
    println!(
        "  charge:   {:.4} µAh   avg power: {:.2} µW",
        s.charge_uah, s.avg_power_uw
    );
    println!();
}

fn print_stats_json(file: &std::path::Path, rec: &Recording, s: &Stats) -> anyhow::Result<()> {
    let obj = serde_json::json!({
        "file": file.display().to_string(),
        "meta": rec.meta,
        "stats": s,
    });
    println!("{}", serde_json::to_string_pretty(&obj)?);
    Ok(())
}

fn print_compare(baseline: &std::path::Path, new: &std::path::Path, r: &CompareResult) {
    let conf_pct = r.confidence * 100.0;
    println!(
        "baseline: {}  mean {} ({} blocks of {} s)",
        baseline.display(),
        fmt_ua(r.mean_baseline),
        r.blocks_baseline,
        r.block_secs
    );
    println!(
        "new:      {}  mean {} ({} blocks of {} s)",
        new.display(),
        fmt_ua(r.mean_new),
        r.blocks_new,
        r.block_secs
    );
    println!();
    println!(
        "mean difference:   {} ({:+.2}%)",
        fmt_ua(r.mean_diff),
        r.mean_diff_pct
    );
    println!(
        "{conf_pct}% CI of diff:  [{}, {}]  ([{:+.2}%, {:+.2}%])",
        fmt_ua(r.diff_ci.0),
        fmt_ua(r.diff_ci.1),
        r.diff_ci_pct.0,
        r.diff_ci_pct.1
    );
    println!(
        "Welch's t-test:    t = {:.3}, dof = {:.1}, p = {:.2e}",
        r.t_statistic, r.degrees_of_freedom, r.p_value
    );
    println!();
    if r.significant {
        let direction = if r.mean_diff > 0.0 {
            "INCREASE"
        } else {
            "DECREASE"
        };
        println!("=> SIGNIFICANT {direction} in mean current at {conf_pct}% confidence");
    } else {
        println!("=> no significant change detected at {conf_pct}% confidence");
    }
    println!();
    println!(
        "{:<12} {:>14} {:>14} {:>14}",
        "percentile", "baseline", "new", "diff"
    );
    for p in &r.percentiles {
        println!(
            "{:<12} {:>14} {:>14} {:>14}",
            format!("p{}", p.percentile),
            fmt_ua(p.baseline),
            fmt_ua(p.new),
            fmt_ua(p.new - p.baseline)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_line_is_compact() {
        let u = powerbench::LiveUpdate {
            elapsed_secs: 1234.5,
            now_ua: 152.1,
            window_secs: 10.0,
            window_mean_ua: 148.73,
            window_min_ua: 0.0482,
            window_p50_ua: 51.2,
            window_p95_ua: 1200.0,
            window_p99_ua: 2100.0,
            window_max_ua: 2_500_000.0,
            total_mean_ua: 151.02,
            total_charge_uah: 0.5,
            missed_samples: 0,
        };
        let line = live_line(&u);
        for label in ["now", "avg(10s)", "min", "p50", "p95", "p99", "max", "tot"] {
            assert!(line.contains(label), "missing {label} in {line}");
        }
        // Fits in a typically-sized terminal so the in-place rewrite does
        // not have to truncate.
        assert!(line.chars().count() <= 140, "line too wide: {line}");
    }
}
