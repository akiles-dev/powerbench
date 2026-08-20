# powerbench

Powerbench is a library and tool to use with the Nordic Power Profiling Kit to do structured benchmarking of embedded applications measuring low power.

Built on the [ppk2](https://crates.io/crates/ppk2) crate, it can be used both as a CLI and a Rust library crate.

Written using Claude Code.

## Features

- **Measure**: enable device power at a chosen voltage, wait for the device to
  settle, sample for a fixed duration, and store the result for later inspection.
- **Live**: monitor current draw interactively with a moving average over the
  last N seconds.
- **Compare**: comparison of two sample files with a p-value, a bootstrap confidence interval for the difference.
- **Plot**: render one or two recordings with gnuplot to get a visual comparison.

## Installation

```sh
cargo install --path .
```

The `plot` subcommand additionally needs the `gnuplot` executable on `PATH`.

On Linux, your user needs access to the PPK2's serial device (typically
membership in the `dialout` group, or a udev rule for USB VID/PID `1915:c00a`).

## Usage

Here are some usages of `powerbench`, invoke the command with `--help` to find more options.

### Record a baseline, then compare a change

```sh
powerbench measure --voltage 3000 --settle 10 --duration 60 \
    --label "main @ abc1234" -o baseline.pbench

# ... flash the new firmware ...

powerbench measure --voltage 3000 --settle 10 --duration 60 \
    --label "fix-idle-power" -o new.pbench

# statistical comparison
powerbench compare baseline.pbench new.pbench

# visual comparision
powerbench plot baseline.pbench new.pbench -o compare.png
```

### Watching a device live


```sh
powerbench live --voltage 3000 --window 10
```

```
   42.3s now  152.10 µA avg(10s)  148.73 µA min   48.20 µA p50   51.20 µA p95   1.200 mA p99   2.100 mA max   2.410 mA tot  151.02 µA
```

### Powering a device without measuring

Useful for flashing or manual testing.

```sh
powerbench power on --voltage 3000   # stays on after the command exits
powerbench power off
```

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
