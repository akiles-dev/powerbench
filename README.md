# powerbench

Powerbench is a tool to use with the Nordic Power Profiling Kit to do structured benchmarking of embedded applications measuring low power.

It records current measurements from a [PPK2](https://www.nordicsemi.com/Products/Development-hardware/Power-Profiler-Kit-2)
into sample files, computes statistics, and — most importantly — tells you whether
two recordings actually differ, or whether you are looking at noise. It is designed
to run headless (e.g. in CI as a hardware-in-the-loop power regression gate) and is
also usable as a Rust library.

Built on the [ppk2](https://crates.io/crates/ppk2) crate.

## Features

- **Measure**: enable device power at a chosen voltage, wait for the device to
  settle, sample for a fixed duration, and store the result in a compact,
  self-describing sample file.
- **Stats**: min / max / mean / standard deviation / percentiles, total charge
  (µAh) and average power.
- **Compare**: statistically sound comparison of two sample files with a
  p-value, a bootstrap confidence interval for the difference, and an exit-code
  gate for CI.
- **Plot**: render one or two recordings with gnuplot — to PNG/SVG (headless,
  no display needed) or to an interactive window when no output file is
  given: a min/mean/max envelope over time plus a percentile (CDF) view of
  the sample distribution. The current axis switches to log automatically
  when the data spans a wide dynamic range, so the sleep floor stays
  readable next to wake spikes (`--linear`/`--log` to override).

## Installation

```sh
cargo install --path .
```

The `plot` subcommand additionally needs the `gnuplot` executable on `PATH`.

On Linux, your user needs access to the PPK2's serial device (typically
membership in the `dialout` group, or a udev rule for USB VID/PID `1915:c00a`).

## Usage

### Record a baseline, then compare a change

```sh
# Power the device at 3.0 V, let it boot for 10 s, then sample 60 s at 1000 sps.
powerbench measure --voltage 3000 --settle 10 --duration 60 \
    --label "main @ abc1234" -o baseline.pbench

# ... flash the new firmware ...

powerbench measure --voltage 3000 --settle 10 --duration 60 \
    --label "fix-idle-power" -o new.pbench

powerbench compare baseline.pbench new.pbench
powerbench plot baseline.pbench new.pbench -o compare.png
```

`compare` prints something like:

```
baseline: baseline.pbench  mean 152.51 µA (60 blocks of 1 s)
new:      new.pbench  mean 156.47 µA (60 blocks of 1 s)

mean difference:   3.96 µA (+2.60%)
95% CI of diff:  [3.92 µA, 4.00 µA]  ([+2.57%, +2.62%])
Welch's t-test:    t = 193.892, dof = 57.9, p = 0.00e0

=> SIGNIFICANT INCREASE in mean current at 95% confidence

percentile         baseline            new           diff
p1                 50.10 µA       54.11 µA        4.01 µA
...
```

### Powering a device without measuring

Useful for flashing or manual testing with the PPK2 as the power source:

```sh
powerbench power on --voltage 3000   # stays on after the command exits
powerbench power off
```

### Statistics of a single file

```sh
powerbench stats baseline.pbench
powerbench stats --json baseline.pbench   # machine-readable
```

### CI power-regression gate

`compare --max-regression <pct>` exits with code `2` when the mean current
increased *significantly* (at the requested confidence level) by more than
`<pct>` percent. `--json` makes the full result machine-readable.

```sh
powerbench measure --voltage 3000 --settle 10 --duration 60 -o new.pbench
powerbench compare baseline.pbench new.pbench \
    --confidence 99 --max-regression 2 --json > result.json
```

Exit codes: `0` OK, `1` error, `2` regression beyond the threshold.

All interactive output goes to stderr and results go to stdout, and progress
reporting degrades gracefully when stderr is not a terminal.

On Ctrl-C, SIGTERM or SIGHUP, a running `measure` shuts down gracefully: the
measurement stream is stopped and power to the device under test is turned
off (unless `--keep-power` was given), returning the PPK2 to its initial
state; the exit code is 130 and no sample file is written. A second signal
force-exits. The same cleanup also runs if the recording fails with an
error.

### Trying it without hardware

```sh
cargo run --example synth   # writes synth-baseline.pbench and synth-new.pbench
powerbench compare synth-baseline.pbench synth-new.pbench
```

## Methodology

Consecutive PPK2 samples are strongly autocorrelated: a device sleeping at
50 µA that wakes to 2 mA every 100 ms produces long runs of similar samples.
Treating raw samples as independent observations would make any difference,
however tiny, look "statistically significant".

`compare` therefore splits each recording into fixed-length blocks (default
1 s, `--block-secs`) and performs inference on the *block means*, which are
approximately independent as long as the block length exceeds the device's
activity cycle. It then reports two views on the same question:

- **Welch's t-test** on the block means (does not assume equal variances).
- A **percentile bootstrap confidence interval** (default 10,000 resamples)
  for the difference of means. The bootstrap seed is fixed by default so CI
  runs are reproducible; override with `--seed`.

A difference is declared significant only when the p-value is below the
chosen alpha *and* the confidence interval excludes zero.

Rules of thumb:

- Pick `--settle` longer than your device's boot time: the settle period is
  measured from power-on, and sampling starts strictly after it has elapsed,
  so boot-up current is never part of the capture.
- Pick `--block-secs` comfortably larger than your device's periodic activity
  (wakeups, advertising intervals, ...). If your device advertises every 2 s,
  use `--block-secs 4` or more.
- Record long enough to get at least ~20 blocks per file.
- Keep voltage, sample rate and duration identical between the recordings you
  compare.

## Troubleshooting

**`measurement incomplete: got N of M samples`** — samples arrived slower
than real time, so the recording was aborted instead of writing a file with a
distorted time axis. The PPK2 streams 400 KB/s continuously; check the USB
connection (avoid hubs and VM USB passthrough) and host load. If any raw
samples were lost mid-capture, the recording is still written but flagged
(`missed_samples` in the metadata) with a warning.

powerbench drives the PPK2's serial stream directly with a large read buffer
instead of using the `ppk2` crate's measurement pipeline, which reads 4 bytes
per syscall and cannot sustain the stream rate on all systems.

## Sample file format

A `.pbench` file is a small binary container:

| field    | size     | content                              |
|----------|----------|--------------------------------------|
| magic    | 8 bytes  | `PBENCH1\n`                          |
| meta_len | u32 LE   | length of the JSON metadata          |
| meta     | meta_len | JSON metadata (voltage, sps, label, …) |
| count    | u64 LE   | number of samples                    |
| samples  | count×4  | current in µA, f32 LE                |

## Library usage

The CLI dependencies are behind the default `cli` feature; for embedding in
other tooling (e.g. factory provisioning), depend on the library only:

```toml
[dependencies]
powerbench = { version = "0.1", default-features = false }
```

```rust,no_run
use powerbench::acquire::{record, RecordConfig};
use powerbench::stats::Stats;
use std::time::Duration;

let config = RecordConfig {
    voltage_mv: 3000,
    settle: Duration::from_secs(5),
    duration: Duration::from_secs(30),
    ..Default::default()
};
let recording = record(&config, None, |_| {})?;
recording.save("baseline.pbench")?;
let stats = Stats::compute(&recording);
println!("mean: {:.1} µA", stats.mean);
# Ok::<(), powerbench::Error>(())
```

`record` takes an optional `&AtomicBool` cancellation flag and a progress
callback, so it can be embedded in interactive tools. See the API docs for
the `compare` and `plot` modules.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
