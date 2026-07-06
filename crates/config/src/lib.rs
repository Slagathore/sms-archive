//! Configuration and resource detection

use sms_errors::{AppError, Result};
use std::path::Path;
use sysinfo::{DiskKind, Disks, System};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Clone, Copy)]
pub enum ResourceProfile {
    Low,    // RAM < 8GB
    Medium, // RAM 8-16GB
    High,   // RAM > 16GB
}

impl ResourceProfile {
    pub fn detect() -> Self {
        let mut sys = System::new_all();
        sys.refresh_memory();

        let total_ram_mb = sys.total_memory() / 1024;

        match total_ram_mb {
            0..=8192 => Self::Low,
            8193..=16384 => Self::Medium,
            _ => Self::High,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SystemResources {
    pub total_ram_bytes: u64,
    pub available_ram_bytes: u64,
    pub cpu_cores: usize,
}

impl SystemResources {
    pub fn detect() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();

        Self {
            total_ram_bytes: sys.total_memory() * 1024,
            available_ram_bytes: sys.available_memory() * 1024,
            cpu_cores: sys.physical_core_count().unwrap_or(1),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ResourceRequirements {
    pub min_ram: u64,
    pub recommended_ram: u64,
    pub min_disk: u64,
}

pub fn calculate_minimum_resources(xml_size_bytes: u64) -> Result<ResourceRequirements> {
    let estimated_db_size = (xml_size_bytes as f64 * 1.2) as u64;
    let wal_headroom = (estimated_db_size as f64 * 0.5) as u64;
    let vacuum_space = estimated_db_size * 2;
    let thumbnail_cache = 10 * 1024_u64.pow(3);
    let safety_margin = 5 * 1024_u64.pow(3);

    Ok(ResourceRequirements {
        min_ram: 4 * 1024_u64.pow(3),
        recommended_ram: 8 * 1024_u64.pow(3),
        min_disk: xml_size_bytes
            + estimated_db_size
            + wal_headroom
            + vacuum_space
            + thumbnail_cache
            + safety_margin,
    })
}

pub fn detect_resource_limits() -> SystemResources {
    SystemResources::detect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageType {
    Hdd,
    Ssd,
    Unknown,
}

pub fn detect_storage_type(path: &Path) -> StorageType {
    let disks = Disks::new_with_refreshed_list();
    let path_str = path.to_string_lossy().to_lowercase();
    let mut best_match: Option<(usize, DiskKind)> = None;

    for disk in disks.iter() {
        let mount = disk.mount_point();
        let mount_str = mount.to_string_lossy().to_lowercase();
        if path_str.starts_with(&mount_str) {
            let len = mount_str.len();
            if best_match
                .map(|(best_len, _)| len > best_len)
                .unwrap_or(true)
            {
                best_match = Some((len, disk.kind()));
            }
        }
    }

    match best_match.map(|(_, kind)| kind) {
        Some(DiskKind::HDD) => StorageType::Hdd,
        Some(DiskKind::SSD) => StorageType::Ssd,
        _ => StorageType::Unknown,
    }
}

pub fn available_disk_bytes(path: &Path) -> Result<u64> {
    fs2::available_space(path).map_err(AppError::Io)
}

/// Delete rotated daily log files (`sms-archive.log.YYYY-MM-DD`) beyond the
/// most recent `keep`. tracing-appender's daily rotation never prunes on its
/// own, so logs accumulated to hundreds of MB. Date suffixes sort
/// lexicographically, so keeping the lexically-largest `keep` keeps the newest.
fn prune_old_logs(log_dir: &Path, prefix: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return;
    };
    let mut logs: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.len() > prefix.len())
        })
        .collect();
    if logs.len() <= keep {
        return;
    }
    logs.sort();
    let remove_count = logs.len() - keep;
    for path in logs.into_iter().take(remove_count) {
        let _ = std::fs::remove_file(path);
    }
}

pub fn init_logging(log_dir: &Path) -> Result<WorkerGuard> {
    std::fs::create_dir_all(log_dir)?;
    prune_old_logs(log_dir, "sms-archive.log.", 7);
    let file_appender = tracing_appender::rolling::daily(log_dir, "sms-archive.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(file_writer);

    let stdout_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stdout_layer)
        .init();

    Ok(guard)
}
