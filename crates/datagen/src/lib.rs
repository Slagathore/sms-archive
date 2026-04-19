//! Synthetic test data generator

use chrono::{Duration, Utc};
use rand::Rng;
use rand::SeedableRng;
use sms_errors::Result;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

pub struct DataGenConfig {
    pub target_size_gb: f64,
    pub avg_message_size_bytes: usize,
    pub seed: Option<u64>,
    pub mms_ratio: f64,
    pub burstiness: f64,
}

pub fn generate_xml(config: DataGenConfig, output: &Path) -> Result<()> {
    let mut writer = BufWriter::new(File::create(output)?);
    writer.write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n")?;
    writer.write_all(b"<smses>\n")?;

    let total_messages =
        (config.target_size_gb * 1e9) as usize / config.avg_message_size_bytes.max(1);
    let mut rng = match config.seed {
        Some(seed) => rand::rngs::StdRng::seed_from_u64(seed),
        None => rand::rngs::StdRng::from_entropy(),
    };
    let mut timestamp = Utc::now() - Duration::days(365);
    let mut burst_remaining: usize = 0;

    for i in 0..total_messages {
        let in_burst = if burst_remaining > 0 {
            burst_remaining -= 1;
            true
        } else if rng.gen_bool(config.burstiness.clamp(0.0, 1.0)) {
            burst_remaining = rng.gen_range(10..100);
            true
        } else {
            false
        };

        let delta = if in_burst {
            rng.gen_range(1..6)
        } else {
            rng.gen_range(120..3600)
        };
        timestamp += Duration::seconds(delta);

        let body = format!("msg_{}", i);
        let address = format!("+1555{:07}", rng.gen_range(0..10_000_000));
        if rng.gen_bool(config.mms_ratio.clamp(0.0, 1.0)) {
            let msg = format!(
                "  <mms address=\"{}\" date=\"{}\"><part ct=\"text/plain\" text=\"{}\" /></mms>\n",
                address,
                timestamp.timestamp_millis(),
                body
            );
            writer.write_all(msg.as_bytes())?;
        } else {
            let msg = format!(
                "  <sms address=\"{}\" date=\"{}\" body=\"{}\" />\n",
                address,
                timestamp.timestamp_millis(),
                body
            );
            writer.write_all(msg.as_bytes())?;
        }
    }

    writer.write_all(b"</smses>\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_small_xml() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        generate_xml(
            DataGenConfig {
                target_size_gb: 0.000001,
                avg_message_size_bytes: 64,
                seed: Some(42),
                mms_ratio: 0.1,
                burstiness: 0.2,
            },
            tmp.path(),
        )
        .unwrap();

        let metadata = std::fs::metadata(tmp.path()).unwrap();
        assert!(metadata.len() > 0);
    }
}
