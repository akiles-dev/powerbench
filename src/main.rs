use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use powerbench::acquire::{record, Progress, RecordConfig};
use powerbench::compare::{compare, CompareConfig, CompareResult};
use powerbench::format::Recording;
use powerbench::plot::{plot, PlotConfig};
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
    /// Render one or two sample files to an image using gnuplot.
    Plot {
        /// One or two sample files.
        #[arg(required = true, num_args = 1..=2)]
        files: Vec<PathBuf>,
        /// Output image; .svg renders as SVG, anything else as PNG.
        #[arg(short, long)]
        output: PathBuf,
        /// Plot title; defaults to the file names.
        #[arg(short, long, default_value = "")]
        title: String,
        /// Number of time buckets for the min/mean/max envelope.
        #[arg(long, default_value_t = 1500)]
        buckets: usize,
        /// Logarithmic current axis.
        #[arg(long)]
        log: bool,
        /// Keep the generated gnuplot script and data files.
        #[arg(long)]
        keep_data: bool,
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

            // With the "termination" feature this handler catches SIGINT,
            // SIGTERM and SIGHUP. The first signal requests a graceful stop
            // (the recording loop notices within ~500 ms, stops sampling and
            // powers the DUT off); a second one force-exits.
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
                log_scale: log,
                keep_data,
            };
            plot(&refs, &output, &config)?;
            eprintln!("wrote {}", output.display());
            Ok(ExitCode::SUCCESS)
        }
    }
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
