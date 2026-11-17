//! How busy each disk under a pool is, from `/proc/diskstats`.
//!
//! ZFS reports what it asked of a vdev, not how hard the device worked to do
//! it. The kernel's block layer counts the milliseconds each device had I/O
//! in flight; over an interval that is its busy share — the figure that says
//! which disk is holding a pool back.
//!
//! A pool member is usually a partition. Busy is read from the **whole disk**
//! under it: another partition on the same disk (a boot partition, a second
//! pool) competes for the same spindle or controller, and the disk's figure
//! is the honest one.

use std::collections::HashMap;
use std::path::Path;

/// Milliseconds with I/O in flight, per block device name.
pub fn read_busy_ms() -> HashMap<String, u64> {
    let mut m = HashMap::new();
    let Ok(s) = std::fs::read_to_string("/proc/diskstats") else { return m };
    for l in s.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        // major minor name, then 11 or more counters; io_ticks is the 10th.
        if f.len() >= 13 {
            if let Ok(t) = f[12].parse() {
                m.insert(f[2].to_string(), t);
            }
        }
    }
    m
}

/// The whole-disk block device under a vdev path, e.g.
/// `/dev/disk/by-id/ata-…-part1` → `sdh`. `None` for a file vdev or a path
/// that no longer resolves.
pub fn whole_disk(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }
    let dev = std::fs::canonicalize(path).ok()?;
    let name = dev.file_name()?.to_str()?.to_string();
    let sys = Path::new("/sys/class/block").join(&name);
    if !sys.exists() {
        return None;
    }
    if sys.join("partition").exists() {
        // The partition's sysfs directory sits inside its disk's.
        let real = std::fs::canonicalize(&sys).ok()?;
        return real.parent()?.file_name()?.to_str().map(String::from);
    }
    Some(name)
}

/// Busy share, 0-100, over `dt` seconds between two readings.
pub fn busy_pct(prev: u64, cur: u64, dt: f64) -> f64 {
    if dt <= 0.0 {
        return 0.0;
    }
    (cur.saturating_sub(prev) as f64 / 10.0 / dt).clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_is_a_share_of_the_interval() {
        assert_eq!(busy_pct(1000, 1500, 1.0), 50.0);
        assert_eq!(busy_pct(1000, 5000, 1.0), 100.0);
        // A counter that went backwards (device re-added) reads as idle.
        assert_eq!(busy_pct(5000, 10, 1.0), 0.0);
    }

    #[test]
    fn relative_names_are_not_disks() {
        assert_eq!(whole_disk("mirror-0"), None);
    }
}
