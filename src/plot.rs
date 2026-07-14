//! Visual comparison of recordings using gnuplot.
//!
//! Rendering shells out to the `gnuplot` binary with a file-based terminal
//! (`pngcairo` or `svg`), so it works headless without a display.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::format::Recording;
use crate::stats::percentile_sorted;
use crate::{Error, Result};

/// Current-axis scaling for [`plot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scale {
    /// Logarithmic when the data spans a wide dynamic range (sleep floor vs
    /// wake spikes would otherwise be crushed on a linear axis), linear
    /// otherwise.
    #[default]
    Auto,
    /// Always linear.
    Linear,
    /// Always logarithmic.
    Log,
}

/// Configuration for [`plot`].
#[derive(Debug, Clone)]
pub struct PlotConfig {
    /// Plot title. Defaults to the file labels when empty.
    pub title: String,
    /// Number of time buckets for the min/mean/max envelope.
    pub buckets: usize,
    /// Current-axis scaling.
    pub scale: Scale,
    /// Keep the generated gnuplot script and data files next to the output
    /// image instead of deleting them.
    pub keep_data: bool,
}

impl Default for PlotConfig {
    fn default() -> Self {
        Self {
            title: String::new(),
            buckets: 1500,
            scale: Scale::Auto,
            keep_data: false,
        }
    }
}

/// Dynamic range (max / 5th percentile) beyond which [`Scale::Auto`] picks a
/// log axis: past this, the low-current detail occupies less than ~10% of a
/// linear axis.
const AUTO_LOG_RATIO: f64 = 10.0;

const COLORS: [&str; 2] = ["#1f77b4", "#d62728"];

/// Render one or two recordings to an image file.
///
/// The output format is chosen from the file extension: `.svg` renders with
/// gnuplot's `svg` terminal, anything else with `pngcairo`. When `output` is
/// `None`, the plot is instead shown in an interactive gnuplot window
/// (requires a display). The image shows a min/mean/max envelope over time
/// and the distribution of samples as a percentile curve.
pub fn plot(
    recordings: &[(&str, &Recording)],
    output: Option<&Path>,
    config: &PlotConfig,
) -> Result<()> {
    if recordings.is_empty() || recordings.len() > 2 {
        return Err(Error::InvalidConfig(
            "plot takes one or two recordings".into(),
        ));
    }
    if config.buckets < 2 {
        return Err(Error::InvalidConfig("need at least 2 buckets".into()));
    }

    // Script and data files live next to the output image, or in the temp
    // directory when displaying interactively.
    let base = match output {
        Some(o) => o.to_string_lossy().to_string(),
        None => std::env::temp_dir()
            .join(format!("powerbench-plot-{}", std::process::id()))
            .to_string_lossy()
            .to_string(),
    };
    let mut data_files: Vec<PathBuf> = Vec::new();

    let sorted: Vec<Vec<f32>> = recordings
        .iter()
        .map(|(_, rec)| {
            let mut s = rec.samples_ua.clone();
            s.sort_unstable_by(|a, b| a.total_cmp(b));
            s
        })
        .collect();
    let log = match config.scale {
        Scale::Linear => false,
        Scale::Log => true,
        Scale::Auto => auto_log(&sorted),
    };
    // A log axis cannot show values <= 0 (PPK2 noise dips slightly below
    // zero around 0 µA), so clamp them to the smallest positive sample for
    // display.
    let floor = if log {
        Some(
            sorted
                .iter()
                .filter_map(|s| s.iter().find(|&&v| v > 0.0))
                .map(|&v| v as f64)
                .fold(f64::INFINITY, f64::min)
                .min(1.0),
        )
    } else {
        None
    };

    // One envelope file and one percentile-curve file per recording.
    for (i, (name, rec)) in recordings.iter().enumerate() {
        let env_path = PathBuf::from(format!("{base}.{i}.env.dat"));
        let cdf_path = PathBuf::from(format!("{base}.{i}.cdf.dat"));
        std::fs::write(&env_path, envelope_data(rec, config.buckets, name, floor))?;
        std::fs::write(&cdf_path, percentile_data(&sorted[i], name, floor))?;
        data_files.push(env_path);
        data_files.push(cdf_path);
    }

    let script_path = PathBuf::from(format!("{base}.gp"));
    std::fs::write(&script_path, script(recordings, &base, output, log, config))?;

    let mut cmd = Command::new("gnuplot");
    if output.is_none() {
        // Keep the interactive window open after gnuplot exits.
        cmd.arg("--persist");
    }
    let run = cmd.arg(&script_path).output();
    let cleanup = |paths: &[PathBuf]| {
        if !config.keep_data {
            for p in paths {
                std::fs::remove_file(p).ok();
            }
        }
    };
    match run {
        Ok(out) if out.status.success() => {
            cleanup(&data_files);
            if !config.keep_data {
                std::fs::remove_file(&script_path).ok();
            }
            Ok(())
        }
        Ok(out) => Err(Error::Gnuplot(format!(
            "gnuplot exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
        Err(e) => Err(Error::Gnuplot(format!(
            "could not run gnuplot ({e}); is gnuplot installed and on PATH? \
             The script and data files were kept at {}",
            script_path.display()
        ))),
    }
}

/// Decide whether [`Scale::Auto`] should use a log axis: yes when the data
/// spans more than [`AUTO_LOG_RATIO`] between the (positive) 5th percentile
/// and the maximum across all recordings.
fn auto_log(sorted: &[Vec<f32>]) -> bool {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for s in sorted {
        if s.is_empty() {
            continue;
        }
        lo = lo.min(percentile_sorted(s, 5.0));
        hi = hi.max(s[s.len() - 1] as f64);
    }
    lo > 0.0 && hi / lo > AUTO_LOG_RATIO
}

/// Downsample to `buckets` time buckets of (t_mid, min, mean, max),
/// clamping values below `floor` (for log axes) when given.
fn envelope_data(rec: &Recording, buckets: usize, name: &str, floor: Option<f64>) -> String {
    let samples = &rec.samples_ua;
    let sps = rec.meta.sps as f64;
    let n = samples.len();
    let bucket_len = (n / buckets).max(1);
    let clamp = |v: f64| floor.map_or(v, |f| v.max(f));

    let mut out = format!("# {name}: t_mid_s min_ua mean_ua max_ua\n");
    for (i, chunk) in samples.chunks(bucket_len).enumerate() {
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut sum = 0.0;
        for &s in chunk {
            let s = s as f64;
            min = min.min(s);
            max = max.max(s);
            sum += s;
        }
        let t_mid = (i * bucket_len + chunk.len() / 2) as f64 / sps;
        let _ = writeln!(
            out,
            "{t_mid} {} {} {}",
            clamp(min),
            clamp(sum / chunk.len() as f64),
            clamp(max)
        );
    }
    out
}

/// Percentile curve: (current µA, cumulative percent), 0..=100 in 0.1 steps,
/// from ascending-sorted samples. Values below `floor` are clamped when given.
fn percentile_data(sorted: &[f32], name: &str, floor: Option<f64>) -> String {
    let clamp = |v: f64| floor.map_or(v, |f| v.max(f));
    let mut out = format!("# {name}: current_ua percent_of_samples_below\n");
    for i in 0..=1000 {
        let pct = i as f64 / 10.0;
        let _ = writeln!(out, "{} {pct}", clamp(percentile_sorted(sorted, pct)));
    }
    out
}

fn script(
    recordings: &[(&str, &Recording)],
    base: &str,
    output: Option<&Path>,
    log: bool,
    config: &PlotConfig,
) -> String {
    let base = gp_quote(base);
    let title = if config.title.is_empty() {
        recordings
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(" vs ")
    } else {
        config.title.clone()
    };

    let mut s = String::new();
    // With no output file, leave gnuplot on its default interactive
    // terminal (qt/wxt/x11) and let the caller pass --persist.
    if let Some(output) = output {
        let terminal = if output
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
        {
            // SVG is vector, so "resolution" is just the viewport size.
            "svg size 1400,900 dynamic font 'sans,11'"
        } else {
            // 2x the base 1400x900 layout with proportionally scaled font
            // and line widths: same composition, twice the resolution.
            "pngcairo size 2800,1800 font ',22' linewidth 2"
        };
        let _ = writeln!(s, "set terminal {terminal}");
        let _ = writeln!(s, "set output '{}'", gp_escape(output));
    }
    let _ = writeln!(s, "set multiplot layout 2,1 title '{}'", gp_quote(&title));
    let _ = writeln!(s, "set grid");
    let _ = writeln!(s, "set key left top");

    // Panel 1: min/mean/max envelope over time.
    let _ = writeln!(s, "set xlabel 'time (s)'");
    let _ = writeln!(s, "set ylabel 'current (µA)'");
    if log {
        let _ = writeln!(s, "set logscale y");
    }
    let mut plots = Vec::new();
    for (i, (name, _)) in recordings.iter().enumerate() {
        let color = COLORS[i];
        plots.push(format!(
            "'{base}.{i}.env.dat' using 1:2:4 with filledcurves fc rgb '{color}' \
             fs transparent solid 0.15 notitle"
        ));
        plots.push(format!(
            "'{base}.{i}.env.dat' using 1:3 with lines lc rgb '{color}' lw 1.5 \
             title '{} (mean)'",
            gp_quote(name)
        ));
    }
    let _ = writeln!(s, "plot {}", plots.join(", \\\n     "));

    // Panel 2: sample distribution as a percentile curve.
    let _ = writeln!(s, "set xlabel 'current (µA)'");
    let _ = writeln!(s, "set ylabel '% of samples below'");
    let _ = writeln!(s, "set yrange [0:100]");
    if log {
        let _ = writeln!(s, "unset logscale y");
        let _ = writeln!(s, "set logscale x");
    }
    let mut plots = Vec::new();
    for (i, (name, _)) in recordings.iter().enumerate() {
        plots.push(format!(
            "'{base}.{i}.cdf.dat' using 1:2 with lines lc rgb '{}' lw 1.5 title '{}'",
            COLORS[i],
            gp_quote(name)
        ));
    }
    let _ = writeln!(s, "plot {}", plots.join(", \\\n     "));
    let _ = writeln!(s, "unset multiplot");
    s
}

fn gp_escape(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

fn gp_quote(text: &str) -> String {
    text.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Meta;

    fn recording(samples: Vec<f32>) -> Recording {
        Recording {
            meta: Meta {
                created_unix_ms: 0,
                tool_version: "test".into(),
                label: None,
                voltage_mv: 3000,
                sps: 100,
                settle_secs: 0.0,
                duration_secs: samples.len() as f64 / 100.0,
                mode: "source".into(),
                calibrated: true,
                missed_samples: 0,
            },
            samples_ua: samples,
        }
    }

    #[test]
    fn envelope_covers_all_samples() {
        let rec = recording((0..1000).map(|i| i as f32).collect());
        let data = envelope_data(&rec, 10, "test", None);
        let lines: Vec<&str> = data.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(lines.len(), 10);
        // First bucket: samples 0..100.
        let first: Vec<f64> = lines[0]
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        assert_eq!(first[1], 0.0); // min
        assert!((first[2] - 49.5).abs() < 1e-9); // mean
        assert_eq!(first[3], 99.0); // max
    }

    #[test]
    fn script_mentions_all_series() {
        let a = recording(vec![1.0; 200]);
        let b = recording(vec![2.0; 200]);
        let s = script(
            &[("baseline", &a), ("new", &b)],
            "out.png",
            Some(Path::new("out.png")),
            false,
            &PlotConfig::default(),
        );
        assert!(s.contains("pngcairo"));
        assert!(s.contains("out.png.0.env.dat"));
        assert!(s.contains("out.png.1.env.dat"));
        assert!(s.contains("out.png.0.cdf.dat"));
        assert!(s.contains("baseline (mean)"));
    }

    #[test]
    fn svg_terminal_from_extension() {
        let a = recording(vec![1.0; 200]);
        let s = script(
            &[("a", &a)],
            "out.svg",
            Some(Path::new("out.svg")),
            false,
            &PlotConfig::default(),
        );
        assert!(s.contains("set terminal svg"));
    }

    #[test]
    fn auto_log_picks_log_for_sleepy_profile() {
        // 50 µA floor with 2 mA spikes: p5 = 50, max/p5 = 40 > 10.
        let mut samples = vec![50.0f32; 950];
        samples.extend(vec![2000.0f32; 50]);
        samples.sort_unstable_by(|a, b| a.total_cmp(b));
        assert!(auto_log(&[samples]));
    }

    #[test]
    fn auto_log_stays_linear_for_flat_profile() {
        // Steady current +-10%: no need for a log axis.
        let samples: Vec<f32> = (0..1000).map(|i| 100.0 + (i % 20) as f32).collect();
        let mut sorted = samples;
        sorted.sort_unstable_by(|a, b| a.total_cmp(b));
        assert!(!auto_log(&[sorted]));
    }

    #[test]
    fn auto_log_stays_linear_around_zero() {
        // Noise dipping below zero: log axis impossible, stay linear.
        let mut samples: Vec<f32> = (0..1000).map(|i| (i % 7) as f32 - 3.0).collect();
        samples.push(500.0);
        samples.sort_unstable_by(|a, b| a.total_cmp(b));
        assert!(!auto_log(&[samples]));
    }

    #[test]
    fn envelope_clamps_to_floor_for_log() {
        let rec = recording(vec![-1.0, -1.0, 2.0, 4.0]);
        let data = envelope_data(&rec, 2, "test", Some(0.5));
        for line in data.lines().filter(|l| !l.starts_with('#')) {
            let min: f64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
            assert!(min >= 0.5, "unclamped value in {line}");
        }
    }

    #[test]
    fn interactive_script_has_no_terminal() {
        let a = recording(vec![1.0; 200]);
        let s = script(
            &[("a", &a)],
            "/tmp/powerbench-plot-1",
            None,
            false,
            &PlotConfig::default(),
        );
        assert!(!s.contains("set terminal"));
        assert!(!s.contains("set output"));
        assert!(s.contains("/tmp/powerbench-plot-1.0.env.dat"));
    }
}
