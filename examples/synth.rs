//! Generate synthetic sample files, useful for trying out the `stats`,
//! `compare` and `plot` subcommands without a PPK2 attached.
//!
//! ```sh
//! cargo run --example synth
//! powerbench compare synth-baseline.pbench synth-new.pbench
//! ```

use powerbench::format::{Meta, Recording};

const SPS: u32 = 1000;
const DURATION_SECS: usize = 30;

fn main() {
    save("synth-baseline.pbench", "baseline", 0.0, 1);
    save("synth-new.pbench", "new", 4.0, 2);
    eprintln!("wrote synth-baseline.pbench and synth-new.pbench");
}

/// A sleepy device: ~50 µA floor with a ~5 ms wakeup burst to ~2 mA every
/// 100 ms, plus noise. `extra_ua` adds a constant offset to emulate a
/// regression.
fn save(path: &str, label: &str, extra_ua: f32, seed: u64) {
    let mut rng = seed;
    let mut rand = move || {
        // xorshift64
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng % 1000) as f32 / 1000.0
    };

    let n = DURATION_SECS * SPS as usize;
    let samples_ua = (0..n)
        .map(|i| {
            let base = if i % 100 < 5 { 2000.0 } else { 50.0 };
            base + extra_ua + rand() * 10.0
        })
        .collect();

    let rec = Recording {
        meta: Meta {
            created_unix_ms: 1_700_000_000_000,
            tool_version: env!("CARGO_PKG_VERSION").into(),
            label: Some(label.into()),
            voltage_mv: 3000,
            sps: SPS,
            settle_secs: 0.0,
            duration_secs: DURATION_SECS as f64,
            mode: "source".into(),
            calibrated: true,
            missed_samples: 0,
        },
        samples_ua,
    };
    rec.save(path).unwrap();
}
