//! Sample file reading and writing.
//!
//! A powerbench sample file is a small binary container:
//!
//! ```text
//! magic    8 bytes   b"PBENCH1\n"
//! meta_len u32 LE    length of the JSON metadata that follows
//! meta     meta_len  JSON-encoded [`Meta`]
//! count    u64 LE    number of samples
//! samples  count * 4 current samples in microampere, f32 LE
//! ```
//!
//! The JSON metadata keeps the format extensible without breaking old readers;
//! unknown fields are ignored on load.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const MAGIC: &[u8; 8] = b"PBENCH1\n";

/// Metadata describing how a recording was made.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    /// Unix timestamp (milliseconds) of when the recording started.
    pub created_unix_ms: u64,
    /// Version of the tool that created the file.
    pub tool_version: String,
    /// Optional user-supplied label describing the recording.
    #[serde(default)]
    pub label: Option<String>,
    /// Source voltage in millivolt.
    pub voltage_mv: u16,
    /// Samples per second.
    pub sps: u32,
    /// Seconds waited between enabling power and starting to sample.
    pub settle_secs: f64,
    /// Requested sampling duration in seconds.
    pub duration_secs: f64,
    /// Measurement mode: "source" (PPK2 supplies power) or "ampere" (ammeter).
    pub mode: String,
    /// Whether the PPK2 reported itself as calibrated.
    #[serde(default)]
    pub calibrated: bool,
    /// Number of raw ADC samples the device reported as missed during the
    /// capture (detected via counter gaps in the stream). Should be 0.
    #[serde(default)]
    pub missed_samples: u64,
}

/// A recording: metadata plus current samples in microampere.
#[derive(Debug, Clone)]
pub struct Recording {
    /// Metadata describing the recording.
    pub meta: Meta,
    /// Current samples in microampere, at `meta.sps` samples per second.
    pub samples_ua: Vec<f32>,
}

impl Recording {
    /// Duration covered by the samples, in seconds.
    pub fn duration_secs(&self) -> f64 {
        self.samples_ua.len() as f64 / self.meta.sps as f64
    }

    /// Save the recording to a file.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let mut w = BufWriter::new(File::create(path)?);
        w.write_all(MAGIC)?;
        let meta = serde_json::to_vec(&self.meta)
            .map_err(|e| Error::InvalidFile(format!("metadata encoding failed: {e}")))?;
        w.write_all(&(meta.len() as u32).to_le_bytes())?;
        w.write_all(&meta)?;
        w.write_all(&(self.samples_ua.len() as u64).to_le_bytes())?;
        for s in &self.samples_ua {
            w.write_all(&s.to_le_bytes())?;
        }
        w.flush()?;
        Ok(())
    }

    /// Load a recording from a file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut r = BufReader::new(File::open(path)?);

        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(Error::InvalidFile(format!(
                "{}: bad magic, not a powerbench sample file",
                path.display()
            )));
        }

        let mut len = [0u8; 4];
        r.read_exact(&mut len)?;
        let mut meta = vec![0u8; u32::from_le_bytes(len) as usize];
        r.read_exact(&mut meta)?;
        let meta: Meta = serde_json::from_slice(&meta)
            .map_err(|e| Error::InvalidFile(format!("{}: bad metadata: {e}", path.display())))?;
        if meta.sps == 0 {
            return Err(Error::InvalidFile(format!(
                "{}: metadata has sps = 0",
                path.display()
            )));
        }

        let mut count = [0u8; 8];
        r.read_exact(&mut count)?;
        let count = u64::from_le_bytes(count) as usize;

        let mut samples_ua = Vec::with_capacity(count);
        let mut buf = [0u8; 4];
        for _ in 0..count {
            r.read_exact(&mut buf)?;
            samples_ua.push(f32::from_le_bytes(buf));
        }

        Ok(Self { meta, samples_ua })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_meta() -> Meta {
        Meta {
            created_unix_ms: 1_700_000_000_000,
            tool_version: "test".into(),
            label: Some("baseline".into()),
            voltage_mv: 3000,
            sps: 1000,
            settle_secs: 5.0,
            duration_secs: 2.0,
            mode: "source".into(),
            calibrated: true,
            missed_samples: 0,
        }
    }

    #[test]
    fn roundtrip() {
        let rec = Recording {
            meta: test_meta(),
            samples_ua: vec![0.5, 1.25, -0.125, 1e6],
        };
        let dir = std::env::temp_dir().join("powerbench-test-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.pbench");
        rec.save(&path).unwrap();
        let loaded = Recording::load(&path).unwrap();
        assert_eq!(loaded.samples_ua, rec.samples_ua);
        assert_eq!(loaded.meta.voltage_mv, 3000);
        assert_eq!(loaded.meta.sps, 1000);
        assert_eq!(loaded.meta.label.as_deref(), Some("baseline"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_bad_magic() {
        let dir = std::env::temp_dir().join("powerbench-test-magic");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.pbench");
        std::fs::write(&path, b"NOTPBENCHFILE").unwrap();
        assert!(matches!(Recording::load(&path), Err(Error::InvalidFile(_))));
        std::fs::remove_dir_all(&dir).ok();
    }
}
