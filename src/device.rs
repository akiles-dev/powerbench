//! Direct PPK2 serial device handling.
//!
//! This module deliberately does not use the `ppk2` crate's built-in
//! measurement pipeline: that pipeline reads the sample stream 4 bytes at a
//! time, which needs upwards of 100k syscalls per second to sustain the
//! PPK2's continuous 100 ksps (400 KB/s) stream. On hosts that cannot keep
//! up, the OS serial buffer overflows, bytes are dropped, the 4-byte sample
//! framing desyncs, and most of the stream is discarded. Instead, this
//! module drives the serial port itself and reads with a large buffer
//! (reading in large chunks loses nothing: samples are parsed individually
//! regardless of how the bytes arrive). The `ppk2` crate is still used for
//! command encoding, metadata parsing, and the calibration math.

use std::time::{Duration, Instant};

use ppk2::cmd::Command;
use ppk2::types::{DevicePower, MeasurementMode, Metadata, SourceVoltage};
use serialport::{ClearBuffer, FlowControl, SerialPort};

use crate::{Error, Result};

/// A PPK2 device.
///
/// Dropping the handle restores the PPK2 to its initial state as a safety
/// net: the measurement stream is stopped and power to the device under test
/// is turned off (unless [`Device::keep_power_on_drop`] was set). This runs
/// on every exit path — errors, panics, and graceful signal shutdown alike.
pub struct Device {
    port: Box<dyn SerialPort>,
    metadata: Metadata,
    sampling: bool,
    powered: bool,
    keep_power_on_drop: bool,
}

impl Device {
    /// Open the PPK2 at the given serial port, or autodetect it when `None`,
    /// and configure the measurement mode.
    pub fn open(path: Option<&str>, mode: MeasurementMode) -> Result<Self> {
        let path = match path {
            Some(p) => p.to_string(),
            None => ppk2::try_find_ppk2_port()?,
        };
        // Match Nordic's own app (pc-nrfconnect-ppk): 115200 baud, no flow
        // control. The transport is USB CDC so the baud value is nominally
        // meaningless, but the line coding is delivered to the firmware, so
        // stick to the value the official app uses. Hardware flow control
        // must stay off: it lets the tty layer throttle the 400 KB/s stream
        // regardless of how fast we read.
        let mut port = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(500))
            .flow_control(FlowControl::None)
            .open()?;
        port.clear(ClearBuffer::All).ok();
        // Required for the device to talk to us on Windows.
        port.write_data_terminal_ready(true).ok();

        let mut dev = Self {
            port,
            metadata: Metadata::default(),
            sampling: false,
            powered: false,
            keep_power_on_drop: false,
        };
        // Quiesce the device: if a previous session died without cleanup the
        // PPK2 is still streaming, and stale samples (from before this
        // session, possibly including a device boot) could leak into the
        // capture. Stop the stream, let in-flight data arrive, discard it.
        dev.send(Command::AverageStop)?;
        std::thread::sleep(Duration::from_millis(100));
        dev.port.clear(ClearBuffer::All)?;

        dev.metadata = dev.read_metadata()?;
        dev.send(Command::SetPowerMode(mode))?;
        Ok(dev)
    }

    /// The device metadata (calibration values, mode, ...).
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Set the source voltage in millivolt.
    pub fn set_source_voltage(&mut self, mv: u16) -> Result<()> {
        self.send(Command::RegulatorSet(SourceVoltage::from_millivolts(mv)))
    }

    /// Enable or disable power to the device under test.
    pub fn set_device_power(&mut self, on: bool) -> Result<()> {
        self.send(Command::DeviceRunningSet(if on {
            DevicePower::Enabled
        } else {
            DevicePower::Disabled
        }))?;
        self.powered = on;
        Ok(())
    }

    /// Leave power to the device under test on when this handle is dropped,
    /// instead of turning it off.
    pub fn keep_power_on_drop(&mut self, keep: bool) {
        self.keep_power_on_drop = keep;
    }

    /// Start the measurement stream. After this, drain the stream with
    /// [`Device::read_stream`] continuously.
    pub fn start_sampling(&mut self) -> Result<()> {
        // Discard stale input so the stream starts frame-aligned.
        self.port.clear(ClearBuffer::Input)?;
        self.send(Command::AverageStart)?;
        self.sampling = true;
        Ok(())
    }

    /// Stop the measurement stream.
    pub fn stop_sampling(&mut self) -> Result<()> {
        self.send(Command::AverageStop)?;
        self.sampling = false;
        Ok(())
    }

    /// Read available raw stream bytes. Returns 0 when no data arrived
    /// within the port timeout (500 ms).
    pub fn read_stream(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    fn send(&mut self, cmd: Command) -> Result<()> {
        let bytes: Vec<u8> = cmd.bytes().collect();
        self.port.write_all(&bytes)?;
        Ok(())
    }

    fn read_metadata(&mut self) -> Result<Metadata> {
        // The metadata command occasionally fails; retry a few times.
        let mut result = Err(Error::Device("no metadata attempt made".into()));
        for _ in 0..3 {
            result = self.try_read_metadata();
            if result.is_ok() {
                break;
            }
        }
        result
    }

    fn try_read_metadata(&mut self) -> Result<Metadata> {
        self.port.clear(ClearBuffer::Input)?;
        self.send(Command::GetMetaData)?;
        let mut response = Vec::with_capacity(512);
        let mut buf = [0u8; 256];
        let deadline = Instant::now() + Duration::from_secs(3);
        while !response.ends_with(b"END\n") {
            if Instant::now() > deadline {
                return Err(Error::Device("timed out reading device metadata".into()));
            }
            match self.port.read(&mut buf) {
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Metadata::from_bytes(&response)?)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // Best-effort restore of the PPK2's initial state; errors are
        // ignored because drop may run while the port is already broken.
        if self.sampling {
            let _ = self.send(Command::AverageStop);
        }
        if self.powered && !self.keep_power_on_drop {
            let _ = self.send(Command::DeviceRunningSet(DevicePower::Disabled));
        }
    }
}
