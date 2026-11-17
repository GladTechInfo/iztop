//! Pool state: joins the iostat stream, the status feed and the disks' busy
//! figures, and keeps history and the damped copies the bars are drawn from.

use crate::disks;
use crate::zpool::{self, Feed, IoRow, IoStream, PoolStatus, VdevStatus};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// History columns, one per second of sampled time: ten minutes, whatever the
/// sampling rate.
pub const HISTORY: usize = 600;

/// Asymmetric damping, in the manner of a shock absorber: quick to compress,
/// slow to rebound. A rising edge reaches the bar almost at once so a spike is
/// never missed, whilst the fall is damped so the bar settles rather than
/// flickering.
///
/// These are **time constants in seconds**, not fixed per-sample weights, so
/// the feel is the same at any refresh rate.
const ATTACK_RATIO: f64 = 3.0;
pub const DEFAULT_DAMPING_S: f64 = 4.0;

/// Percentage points below which a bar counts as having arrived.
const SETTLED: f64 = 0.1;

/// How often health, errors and scrub progress are re-read.
const STATUS_EVERY: Duration = Duration::from_secs(5);
/// How long to wait before restarting a `zpool iostat` that ended.
const RESPAWN_AFTER: Duration = Duration::from_secs(5);
/// Quiet time after which the rows received make a complete interval.
const LULL: Duration = Duration::from_millis(150);

/// Seconds-to-90% converted to the time constant the filter actually uses.
pub fn damping_tau(seconds: f64) -> f64 {
    (seconds.clamp(0.2, 30.0)) / std::f64::consts::LN_10
}

/// A figure with a damped display copy. The raw value is never altered, so
/// `--once`, the history and every printed figure stay honest.
#[derive(Clone, Copy, Default, Debug)]
pub struct Damped {
    pub raw: f64,
    pub shown: f64,
}

impl Damped {
    pub fn get(&self, smooth: bool) -> f64 {
        if smooth {
            self.shown
        } else {
            self.raw
        }
    }

    /// One frame of the filter. Returns whether it is still visibly travelling.
    fn glide(&mut self, dt: f64, attack: f64, decay: f64) -> bool {
        let tau = if self.raw > self.shown { attack } else { decay };
        self.shown += (self.raw - self.shown) * (1.0 - (-dt / tau).exp());
        if (self.raw - self.shown).abs() < SETTLED {
            self.shown = self.raw;
            false
        } else {
            true
        }
    }
}

/// One column per second of sampled time: each column is the time-weighted
/// mean of the samples in that second.
#[derive(Clone, Debug, Default)]
pub struct History {
    pub ring: VecDeque<f64>,
    acc: f64,
    acc_t: f64,
}

impl History {
    pub fn add(&mut self, value: f64, dt: f64) {
        self.acc += value * dt;
        self.acc_t += dt;
        while self.acc_t >= 1.0 {
            let mean = self.acc / self.acc_t;
            if self.ring.len() == HISTORY {
                self.ring.pop_front();
            }
            self.ring.push_back(mean);
            self.acc_t -= 1.0;
            self.acc = mean * self.acc_t;
        }
    }

    pub fn last(&self, n: usize) -> impl Iterator<Item = f64> + '_ {
        self.ring.iter().skip(self.ring.len().saturating_sub(n)).copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Pool,
    /// special, logs, cache, spares, dedup.
    Group,
    /// mirror, raidz, draid: a vdev with members.
    Vdev,
    Disk,
}

pub struct Vdev {
    /// As iostat names it; the key into the status.
    pub key: String,
    pub depth: usize,
    pub kind: Kind,
    pub io: IoRow,
    /// Whole-disk block device, for a disk.
    pub dev: Option<String>,
    /// Busy %: the disk's own, or the busiest disk beneath.
    pub busy: Option<f64>,
    pub bar: Damped,
}

impl Vdev {
    /// A short name: a disk's path without the directory or a `-partN` tail.
    pub fn short_name(&self, full: bool) -> String {
        if self.kind != Kind::Disk {
            return self.key.clone();
        }
        let base = self.key.rsplit('/').next().unwrap_or(&self.key);
        if full {
            return base.to_string();
        }
        let base = match base.rfind("-part") {
            Some(i) if base[i + 5..].bytes().all(|b| b.is_ascii_digit()) && i + 5 < base.len() => &base[..i],
            _ => base,
        };
        for pre in ["ata-", "nvme-", "scsi-", "usb-", "sas-"] {
            if let Some(rest) = base.strip_prefix(pre) {
                return rest.to_string();
            }
        }
        base.to_string()
    }
}

pub struct Pool {
    pub name: String,
    /// Row 0 is the pool itself; the rest in iostat order.
    pub rows: Vec<Vdev>,
    pub status: Option<PoolStatus>,
    pub rd_hist: History,
    pub wr_hist: History,
}

impl Pool {
    pub fn vdev_status(&self, key: &str) -> Option<&VdevStatus> {
        self.status.as_ref().and_then(|s| s.vdevs.get(key))
    }

    pub fn read_bps(&self) -> f64 {
        self.rows.first().and_then(|r| r.io.rbw).unwrap_or(0.0)
    }

    pub fn write_bps(&self) -> f64 {
        self.rows.first().and_then(|r| r.io.wbw).unwrap_or(0.0)
    }
}

pub struct App {
    pub host: String,
    pub pools: Vec<Pool>,
    /// Pools named on the command line; empty for all.
    pub only: Vec<String>,
    pub interval_ms: u64,
    stream: Option<IoStream>,
    spawned: Instant,
    /// Rows of the interval now arriving.
    pending: Vec<IoRow>,
    pending_at: Instant,
    status_rx: Receiver<Result<Vec<PoolStatus>, String>>,
    statuses: Option<Vec<PoolStatus>>,
    /// Why there is nothing to show, when there is nothing to show.
    pub error: Option<String>,
    pub blocks: u64,
    prev_busy: HashMap<String, u64>,
    prev_busy_t: Option<Instant>,
    devs: HashMap<String, Option<String>>,
    decay_tau: f64,
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_default()
}

impl App {
    pub fn new(interval_ms: u64, only: Vec<String>) -> Self {
        let mut a = App {
            host: hostname(),
            pools: Vec::new(),
            only,
            interval_ms,
            stream: None,
            spawned: Instant::now(),
            pending: Vec::new(),
            pending_at: Instant::now(),
            status_rx: zpool::status_feed(STATUS_EVERY),
            statuses: None,
            error: None,
            blocks: 0,
            prev_busy: HashMap::new(),
            prev_busy_t: None,
            devs: HashMap::new(),
            decay_tau: damping_tau(DEFAULT_DAMPING_S),
        };
        a.spawn();
        a
    }

    pub fn set_damping(&mut self, seconds: f64) {
        self.decay_tau = damping_tau(seconds);
    }

    fn spawn(&mut self) {
        self.spawned = Instant::now();
        self.prev_busy_t = None;
        self.pending.clear();
        match IoStream::spawn(self.interval_ms, &self.only) {
            Ok(s) => self.stream = Some(s),
            Err(e) => {
                self.stream = None;
                self.error = Some(format!("cannot run zpool: {}", e));
            }
        }
    }

    pub fn set_interval(&mut self, ms: u64) {
        self.interval_ms = ms;
        self.stream = None;
        self.spawn();
    }

    /// After a pause, the next busy figure would average the whole pause.
    pub fn reset_rates(&mut self) {
        self.prev_busy_t = None;
    }

    /// Takes whatever the feeds have delivered. Returns whether anything
    /// changed. While `paused`, iostat blocks are read and dropped, so the
    /// first one after the pause is current.
    pub fn poll(&mut self, paused: bool) -> bool {
        let mut changed = false;
        let mut ended = None;
        let mut done = Vec::new();
        if let Some(s) = &self.stream {
            loop {
                match s.rx.try_recv() {
                    Ok(Feed::Stamp) => {
                        if !self.pending.is_empty() {
                            done.push(std::mem::take(&mut self.pending));
                        }
                    }
                    Ok(Feed::Rows(r)) => {
                        self.pending.extend(r);
                        self.pending_at = Instant::now();
                    }
                    Ok(Feed::Exited(e)) => {
                        ended = Some(e);
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        // An interval's rows come in a burst; a lull ends it without waiting
        // a whole interval for the next timestamp.
        if !self.pending.is_empty() && (ended.is_some() || self.pending_at.elapsed() >= LULL) {
            done.push(std::mem::take(&mut self.pending));
        }
        // Should more than one interval have queued up, only the newest counts.
        if let Some(b) = done.pop().filter(|_| !paused) {
            self.apply_block(b);
            changed = true;
        }
        if let Some(e) = ended {
            self.stream = None;
            self.error = Some(if e.is_empty() { "zpool iostat ended".into() } else { e });
            changed = true;
        }
        if self.stream.is_none() && self.spawned.elapsed() >= RESPAWN_AFTER {
            self.spawn();
        }
        while let Ok(r) = self.status_rx.try_recv() {
            if let Ok(st) = r {
                self.apply_status(st);
                changed = true;
            }
        }
        changed
    }

    fn apply_status(&mut self, st: Vec<PoolStatus>) {
        if !st.is_empty() || self.pools.is_empty() {
            // A pool exported since the last block goes; one imported is
            // picked up by iostat itself.
            self.pools.retain(|p| st.iter().any(|s| s.name == p.name));
        }
        for p in self.pools.iter_mut() {
            p.status = st.iter().find(|s| s.name == p.name).cloned();
        }
        self.statuses = Some(st);
    }

    pub fn apply_block(&mut self, rows: Vec<IoRow>) {
        self.error = None;
        self.blocks += 1;
        let now = Instant::now();
        let busy_ms = disks::read_busy_ms();
        let dt = self.prev_busy_t.map(|t| now.duration_since(t).as_secs_f64());
        let interval = self.interval_ms as f64 / 1000.0;

        // Split into one section per pool: a depth-0 row that is not a group
        // heading starts a pool.
        let mut sections: Vec<Vec<IoRow>> = Vec::new();
        for r in rows {
            if r.depth == 0 && !r.group {
                sections.push(Vec::new());
            }
            if let Some(s) = sections.last_mut() {
                s.push(r);
            }
        }

        for sec in sections {
            let name = sec[0].name.clone();
            let idx = match self.pools.iter().position(|p| p.name == name) {
                Some(i) => i,
                None => {
                    let status = self.statuses.as_ref().and_then(|s| s.iter().find(|s| s.name == name).cloned());
                    self.pools.push(Pool {
                        name: name.clone(),
                        rows: Vec::new(),
                        status,
                        rd_hist: History::default(),
                        wr_hist: History::default(),
                    });
                    self.pools.sort_by(|a, b| a.name.cmp(&b.name));
                    self.pools.iter().position(|p| p.name == name).unwrap_or(0)
                }
            };
            let pool = &mut self.pools[idx];
            let n = sec.len();
            let mut out: Vec<Vdev> = Vec::with_capacity(n);
            for (i, io) in sec.iter().enumerate() {
                let leaf = sec.get(i + 1).is_none_or(|next| next.depth <= io.depth);
                let kind = if i == 0 {
                    Kind::Pool
                } else if io.group {
                    Kind::Group
                } else if leaf {
                    Kind::Disk
                } else {
                    Kind::Vdev
                };
                let dev = if kind == Kind::Disk {
                    self.devs.entry(io.name.clone()).or_insert_with(|| disks::whole_disk(&io.name)).clone()
                } else {
                    None
                };
                let busy = match (&dev, dt) {
                    (Some(d), Some(dt)) => match (self.prev_busy.get(d), busy_ms.get(d)) {
                        (Some(p), Some(c)) => Some(disks::busy_pct(*p, *c, dt)),
                        _ => None,
                    },
                    _ => None,
                };
                let bar = pool.rows.iter().find(|v| v.key == io.name && v.depth == io.depth).map(|v| v.bar);
                out.push(Vdev { key: io.name.clone(), depth: io.depth, kind, io: io.clone(), dev, busy, bar: bar.unwrap_or_default() });
            }
            // Anything with members shows its busiest disk: one saturated
            // disk holds up the whole vdev.
            for i in (0..n).rev() {
                if out[i].kind == Kind::Disk {
                    continue;
                }
                let d = out[i].depth;
                let top = i == 0;
                let max = out[i + 1..]
                    .iter()
                    .take_while(|v| top || v.depth > d)
                    .filter(|v| v.kind == Kind::Disk)
                    .filter_map(|v| v.busy)
                    .fold(None, |m: Option<f64>, b| Some(m.map_or(b, |m| m.max(b))));
                out[i].busy = max;
            }
            for v in out.iter_mut() {
                v.bar.raw = v.busy.unwrap_or(0.0);
            }
            pool.rows = out;
            pool.rd_hist.add(pool.read_bps(), interval);
            pool.wr_hist.add(pool.write_bps(), interval);
        }
        self.prev_busy = busy_ms;
        self.prev_busy_t = Some(now);
    }

    /// Advances the damped values toward the latest sample. Returns whether any
    /// bar is still moving.
    pub fn animate(&mut self, dt: f64) -> bool {
        let decay = self.decay_tau;
        let attack = decay / ATTACK_RATIO;
        let mut moving = false;
        for v in self.pools.iter_mut().flat_map(|p| p.rows.iter_mut()) {
            moving |= v.bar.glide(dt, attack, decay);
        }
        moving
    }

    pub fn snap(&mut self) {
        for v in self.pools.iter_mut().flat_map(|p| p.rows.iter_mut()) {
            v.bar.shown = v.bar.raw;
        }
    }

    /// What the animated bars would show, reduced to eighth-cells, so a gliding
    /// frame that moves nothing visibly is not drawn. Errs towards drawing.
    pub fn motion_signature(&self, width: u16) -> Vec<i64> {
        let cells = (width as f64 / 3.0).max(8.0);
        self.pools
            .iter()
            .flat_map(|p| p.rows.iter())
            .map(|v| (v.bar.shown / 100.0 * cells * 8.0).round() as i64)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_drop_prefix_and_partition() {
        let v = |key: &str, kind| Vdev {
            key: key.into(),
            depth: 2,
            kind,
            io: IoRow::default(),
            dev: None,
            busy: None,
            bar: Damped::default(),
        };
        assert_eq!(v("/dev/disk/by-id/ata-ST8000VN004-2M2101_WKD0A1B2-part1", Kind::Disk).short_name(false), "ST8000VN004-2M2101_WKD0A1B2");
        assert_eq!(v("/dev/disk/by-id/ata-X-part1", Kind::Disk).short_name(true), "ata-X-part1");
        assert_eq!(v("/dev/sdb", Kind::Disk).short_name(false), "sdb");
        assert_eq!(v("mirror-0", Kind::Vdev).short_name(false), "mirror-0");
    }

    #[test]
    fn damping_is_quick_up_and_slow_down() {
        let tau = damping_tau(DEFAULT_DAMPING_S);
        let mut d = Damped { raw: 100.0, shown: 0.0 };
        d.glide(0.5, tau / ATTACK_RATIO, tau);
        let up = d.shown;
        let mut e = Damped { raw: 0.0, shown: 100.0 };
        e.glide(0.5, tau / ATTACK_RATIO, tau);
        assert!(up > 100.0 - e.shown);
    }
}
