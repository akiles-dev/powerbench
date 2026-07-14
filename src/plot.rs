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

/// Configuration for [`plot`].
#[derive(Debug, Clone)]
pub struct PlotConfig {
    /// Plot title. Defaults to the file labels when empty.
    pub title: String,
    /// Number of time buckets for the min/mean/max envelope.
    pub buckets: usize,
    /// Use a logarithmic current axis; useful when currents span decades.
    pub log_scale: bool,
    /// Keep the generated gnuplot script and data files next to the output
    /// image instead of deleting them.
    pub keep_data: bool,
}

impl Default for PlotConfig {
    fn default() -> Self {
        Self {
            title: String::new(),
            buckets: 1500,
            log_scale: false,
            keep_data: false,
        }
    }
}

const COLORS: [&str; 2] = ["#1f77b4", "#d62728"];

/// Render one or two recordings to an image file.
///
/// The output format is chosen from the file extension: `.svg` renders with
/// gnuplot's `svg` terminal, anything else with `pngcairo`. The image shows a
/// min/mean/max envelope over time and the distribution of samples as a
/// percentile curve.
pub fn plot(
    recordings: &[(&str, &Recording)],
    output: impl AsRef<Path>,
    config: &PlotConfig,
) -> Result<()> {
    let output = output.as_ref();
    if recordings.is_empty() || recordings.len() > 2 {
        return Err(Error::InvalidConfig(
            "plot takes one or two recordings".into(),
        ));
    }
    if config.buckets < 2 {
        return Err(Error::InvalidConfig("need at least 2 buckets".into()));
    }

    let base = output.to_string_lossy().to_string();
    let mut data_files: Vec<PathBuf> = Vec::new();

    // One envelope file and one percentile-curve file per recording.
    for (i, (name, rec)) in recordings.iter().enumerate() {
        let env_path = PathBuf::from(format!("{base}.{i}.env.dat"));
        let cdf_path = PathBuf::from(format!("{base}.{i}.cdf.dat"));
        std::fs::write(&env_path, envelope_data(rec, config.buckets, name))?;
        std::fs::write(&cdf_path, percentile_data(rec, name))?;
        data_files.push(env_path);
        data_files.push(cdf_path);
    }

    let script_path = PathBuf::from(format!("{base}.gp"));
    std::fs::write(&script_path, script(recordings, output, config))?;

    let run = Command::new("gnuplot").arg(&script_path).output();
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

/// Downsample to `buckets` time buckets of (t_mid, min, mean, max).
fn envelope_data(rec: &Recording, buckets: usize, name: &str) -> String {
    let samples = &rec.samples_ua;
    let sps = rec.meta.sps as f64;
    let n = samples.len();
    let bucket_len = (n / buckets).max(1);

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
        let _ = writeln!(out, "{t_mid} {min} {} {max}", sum / chunk.len() as f64);
    }
    out
}

/// Percentile curve: (current µA, cumulative percent), 0..=100 in 0.1 steps.
fn percentile_data(rec: &Recording, name: &str) -> String {
    let mut sorted = rec.samples_ua.clone();
    sorted.sort_unstable_by(|a, b| a.total_cmp(b));
    let mut out = format!("# {name}: current_ua percent_of_samples_below\n");
    for i in 0..=1000 {
        let pct = i as f64 / 10.0;
        let _ = writeln!(out, "{} {pct}", percentile_sorted(&sorted, pct));
    }
    out
}

fn script(recordings: &[(&str, &Recording)], output: &Path, config: &PlotConfig) -> String {
    let base = gp_escape(output);
    let terminal = if output
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
    {
        "svg size 1400,900 dynamic font 'sans,11'"
    } else {
        "pngcairo size 1400,900 font ',11'"
    };
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
    let _ = writeln!(s, "set terminal {terminal}");
    let _ = writeln!(s, "set output '{}'", gp_escape(output));
    let _ = writeln!(s, "set multiplot layout 2,1 title '{}'", gp_quote(&title));
    let _ = writeln!(s, "set grid");
    let _ = writeln!(s, "set key left top");

    // Panel 1: min/mean/max envelope over time.
    let _ = writeln!(s, "set xlabel 'time (s)'");
    let _ = writeln!(s, "set ylabel 'current (µA)'");
    if config.log_scale {
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
    if config.log_scale {
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
        let data = envelope_data(&rec, 10, "test");
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
            Path::new("out.png"),
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
        let s = script(&[("a", &a)], Path::new("out.svg"), &PlotConfig::default());
        assert!(s.contains("set terminal svg"));
    }
}
