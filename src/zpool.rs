//! Everything read from the `zpool` command.
//!
//! Two feeds, each on its own thread so a slow or hung pool never stalls the
//! screen:
//!
//! * **iostat** — one long-lived `zpool iostat -p -v -l -P -y -T u <interval>`
//!   child. It prints a timestamp, then one indented block per pool, once per
//!   interval, and flushes after each. Operations, bandwidth and latency are
//!   already rates over the interval, worked out by ZFS from its own counters,
//!   so the figures are exactly those `zpool iostat` would print.
//! * **status** — `zpool status -j` and `zpool list -H`, every few seconds:
//!   health, error counters, fragmentation and scrub or resilver progress.
//!
//! The command is used rather than the kernel interface because per-vdev
//! statistics reach user space only as a packed nvlist through an ioctl on
//! `/dev/zfs`, whose layout is private to OpenZFS and changes between
//! releases; the command's output is its stable, documented face.

use crate::json::{self, Json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One line of `zpool iostat -v`: a pool, a group heading (special, logs,
/// cache, spares, dedup), a vdev or a disk.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IoRow {
    /// Indent level: 0 for a pool or group heading, then one per tree level.
    pub depth: usize,
    /// As printed: with -P, a disk's full path.
    pub name: String,
    /// A group heading carries no figures of its own.
    pub group: bool,
    pub alloc: Option<u64>,
    pub free: Option<u64>,
    pub rops: Option<f64>,
    pub wops: Option<f64>,
    pub rbw: Option<f64>,
    pub wbw: Option<f64>,
    /// total_wait, nanoseconds: queue plus disk, as an application sees it.
    pub rwait: Option<f64>,
    pub wwait: Option<f64>,
    /// disk_wait, nanoseconds: the device alone.
    pub rdisk: Option<f64>,
    pub wdisk: Option<f64>,
}

pub enum Feed {
    /// A timestamp line: the start of an interval's output.
    Stamp,
    Rows(Vec<IoRow>),
    /// The child ended; the text is its last error line, if any.
    Exited(String),
}

fn num(s: &str) -> Option<f64> {
    if s == "-" {
        None
    } else {
        s.parse::<f64>().ok().filter(|v| v.is_finite())
    }
}

/// Parses one line of output. `None` for headers, separators and anything
/// else that is not a row.
pub fn parse_row(line: &str) -> Option<IoRow> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let toks: Vec<&str> = line.split_whitespace().collect();
    if toks.len() < 7 || toks[0].starts_with('-') || toks[0] == "capacity" {
        return None;
    }
    if toks[0] == "pool" && toks[1] == "alloc" {
        return None;
    }
    // The figures are the trailing run of numbers and dashes; the name is
    // everything before them, so a path with a space in it survives.
    let nvals = toks.iter().rev().take_while(|t| **t == "-" || num(t).is_some()).count().min(toks.len() - 1);
    if nvals < 6 {
        return None;
    }
    let first = toks.len() - nvals;
    let vals: Vec<Option<f64>> = toks[first..].iter().map(|t| num(t)).collect();
    let v = |i: usize| vals.get(i).copied().flatten();
    Some(IoRow {
        depth: indent / 2,
        name: toks[..first].join(" "),
        group: vals.iter().all(|x| x.is_none()) && indent == 0,
        alloc: v(0).map(|x| x as u64),
        free: v(1).map(|x| x as u64),
        rops: v(2),
        wops: v(3),
        rbw: v(4),
        wbw: v(5),
        rwait: v(6),
        wwait: v(7),
        rdisk: v(8),
        wdisk: v(9),
    })
}

/// A timestamp line from `-T u`: one bare number.
fn is_stamp(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) && !line.starts_with(' ')
}

/// The running `zpool iostat`. Killed when dropped.
pub struct IoStream {
    child: Child,
    pub rx: Receiver<Feed>,
}

impl Drop for IoStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn interval_arg(ms: u64) -> String {
    let s = format!("{:.3}", ms as f64 / 1000.0);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub fn iostat_command(interval_ms: u64, count: Option<u32>, pools: &[String]) -> Command {
    let mut c = Command::new("zpool");
    c.args(["iostat", "-p", "-v", "-l", "-P", "-y", "-T", "u"])
        .args(pools)
        .arg(interval_arg(interval_ms))
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(n) = count {
        c.arg(n.to_string());
    }
    c
}

impl IoStream {
    pub fn spawn(interval_ms: u64, pools: &[String]) -> std::io::Result<IoStream> {
        let mut child = iostat_command(interval_ms, None, pools).spawn()?;
        let out = child.stdout.take().expect("stdout is piped");
        let err = child.stderr.take().expect("stderr is piped");
        let (tx, rx) = mpsc::channel();
        let last_err = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let le = last_err.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    *le.lock().unwrap_or_else(|e| e.into_inner()) = line;
                }
            }
        });
        std::thread::spawn(move || read_blocks(BufReader::with_capacity(1 << 18, out), &tx, &last_err));
        Ok(IoStream { child, rx })
    }
}

/// Passes rows on as they arrive, batched by what one read delivered.
/// zpool flushes as it goes, not once per interval, so a batch may be part of
/// an interval; the receiver closes an interval on the next timestamp or
/// after a lull (see `App::poll`).
fn read_blocks<R: std::io::Read>(mut r: BufReader<R>, tx: &Sender<Feed>, last_err: &std::sync::Mutex<String>) {
    let mut rows = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if is_stamp(&line) {
            if !rows.is_empty() && tx.send(Feed::Rows(std::mem::take(&mut rows))).is_err() {
                return;
            }
            if tx.send(Feed::Stamp).is_err() {
                return;
            }
        } else if let Some(row) = parse_row(line.trim_end()) {
            rows.push(row);
        }
        if r.buffer().is_empty() && !rows.is_empty() && tx.send(Feed::Rows(std::mem::take(&mut rows))).is_err() {
            return;
        }
    }
    if !rows.is_empty() {
        let _ = tx.send(Feed::Rows(rows));
    }
    // Give the stderr reader a moment to catch the reason.
    std::thread::sleep(Duration::from_millis(100));
    let e = last_err.lock().map(|e| e.clone()).unwrap_or_default();
    let _ = tx.send(Feed::Exited(e));
}

// ───────────────────────────── status ─────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct VdevStatus {
    pub state: String,
    pub read_errors: Option<u64>,
    pub write_errors: Option<u64>,
    pub checksum_errors: Option<u64>,
}

impl VdevStatus {
    pub fn all_clear(&self) -> bool {
        [self.read_errors, self.write_errors, self.checksum_errors] == [Some(0); 3]
    }

    pub fn error_counts(&self) -> String {
        [self.read_errors, self.write_errors, self.checksum_errors]
            .map(|v| v.map_or_else(|| "-".into(), |n| n.to_string())).join("/")
    }
}

#[derive(Clone, Debug, Default)]
pub struct Scan {
    /// SCRUB, RESILVER or ERRORSCRUB.
    pub function: String,
    /// SCANNING, FINISHED or CANCELED.
    pub state: String,
    pub to_examine: u64,
    pub issued: u64,
    pub pass_issued: u64,
    pub pass_start: u64,
    pub spent_paused: u64,
    pub paused: bool,
    pub errors: u64,
}

impl Scan {
    pub fn running(&self) -> bool {
        self.state == "SCANNING"
    }

    /// Share done, as `zpool status` counts it: bytes issued against bytes to
    /// examine.
    pub fn pct(&self) -> f64 {
        if self.to_examine == 0 {
            0.0
        } else {
            (self.issued as f64 / self.to_examine as f64 * 100.0).clamp(0.0, 100.0)
        }
    }

    /// Seconds left at this pass's issue rate, as `zpool status` estimates.
    pub fn eta_s(&self, now: u64) -> Option<f64> {
        let elapsed = now.saturating_sub(self.pass_start).saturating_sub(self.spent_paused).max(1);
        let rate = self.pass_issued as f64 / elapsed as f64;
        (rate > 0.0 && !self.paused).then(|| self.to_examine.saturating_sub(self.issued) as f64 / rate)
    }

    pub fn rate(&self, now: u64) -> f64 {
        let elapsed = now.saturating_sub(self.pass_start).saturating_sub(self.spent_paused).max(1);
        self.pass_issued as f64 / elapsed as f64
    }
}

/// Space on one vdev or disk, from `zpool list -v`. ZFS records allocated
/// space only for a pool and its top-level vdevs; a disk inside a mirror or
/// raidz has a size and nothing else.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Space {
    pub size: u64,
    pub alloc: Option<u64>,
    pub free: Option<u64>,
    pub frag: Option<u64>,
    /// Percentage used, truncated, as zpool list prints it.
    pub cap: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct PoolStatus {
    pub name: String,
    pub health: String,
    pub size: u64,
    pub alloc: u64,
    pub frag: Option<u64>,
    pub scan: Option<Scan>,
    /// Keyed as iostat prints the vdev, as `vdevs` is.
    pub space: HashMap<String, Space>,
    /// Keyed as iostat prints the vdev: full path for a disk, name otherwise.
    pub vdevs: HashMap<String, VdevStatus>,
}

/// Reads `zpool list` and `zpool status -j`. The list is required; the JSON
/// status (OpenZFS 2.3 and later) only adds to it.
pub struct StatusReport {
    pub sampled_at: Instant,
    pub pools: Vec<PoolStatus>,
    pub warning: Option<String>,
}

const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

fn read_pipe(mut pipe: impl std::io::Read) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn output(args: &[&str], stop: &AtomicBool) -> Result<String, String> {
    use std::os::unix::process::CommandExt;
    let cmd = format!("zpool {}", args[0]);
    if stop.load(Ordering::Relaxed) {
        return Err(format!("{cmd}: stopped"));
    }
    let mut child = Command::new("zpool").args(args).env("LC_ALL", "C").process_group(0)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map_err(|e| format!("cannot run {cmd}: {e}"))?;
    let deadline = Instant::now() + STATUS_TIMEOUT;
    std::thread::scope(|scope| {
        // Drain both pipes during the wait. Large pool trees can fill a pipe.
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let out = scope.spawn(move || read_pipe(stdout));
        let err = scope.spawn(move || read_pipe(stderr));
        let status = loop {
            // Keep the child unreaped until its pipes close. A helper may still
            // hold a pipe after the command exits and must share its deadline.
            let waited = if out.is_finished() && err.is_finished() { child.try_wait() } else { Ok(None) };
            let failure = match waited {
                Ok(Some(status)) => break status,
                Ok(None) if stop.load(Ordering::Relaxed) => "stopped".to_string(),
                Ok(None) if Instant::now() >= deadline => "timed out after 5s".to_string(),
                Ok(None) => {
                    std::thread::park_timeout(Duration::from_millis(25));
                    continue;
                }
                Err(e) => e.to_string(),
            };
            if let Some(group) = rustix::process::Pid::from_raw(child.id() as i32) {
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::Kill);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{cmd}: {failure}"));
        };
        let collect = |thread: std::thread::ScopedJoinHandle<'_, std::io::Result<String>>| {
            thread.join().map_err(|_| format!("{cmd}: pipe reader failed"))?
                .map_err(|e| format!("{cmd}: {e}"))
        };
        let stdout = collect(out)?;
        let stderr = collect(err)?;
        if !status.success() {
            return Err(format!("{cmd}: {} ({status})", stderr.lines().next().unwrap_or("command failed")));
        }
        Ok(stdout)
    })
}

fn unsupported_option(e: &str) -> bool {
    ["invalid option", "unrecognized option", "unknown option", "illegal option"]
        .iter().any(|msg| e.to_ascii_lowercase().contains(msg))
}

fn read_status(stop: &AtomicBool) -> Result<StatusReport, String> {
    let sampled_at = Instant::now();
    let mut warnings = Vec::new();
    let mut pools = match output(&["list", "-H", "-p", "-v", "-P", "-o", LIST_COLUMNS], stop) {
        Ok(s) => parse_list(&s)?,
        Err(e) if unsupported_option(&e) => {
            warnings.push(e);
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    if pools.is_empty() {
        let list = output(&["list", "-H", "-p", "-o", LIST_COLUMNS], stop)?;
        pools = parse_list(&list)?;
        if !list.trim().is_empty() && pools.is_empty() {
            return Err("zpool list: unrecognised pool summary".into());
        }
    }
    match output(&["status", "-j", "-p", "--json-int"], stop) {
        Ok(s) => {
            let j = json::parse(&s).map_err(|e| format!("zpool status: invalid JSON: {e}"))?;
            if pools.iter().any(|p| j.get("pools").and_then(|all| all.get(&p.name)).is_none()) {
                return Err("zpool status: missing pool in JSON output".into());
            }
            apply_status_json(&mut pools, &j);
        }
        Err(e) if unsupported_option(&e) => warnings.push(e),
        Err(e) => return Err(e),
    }
    let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
    Ok(StatusReport { sampled_at, pools, warning })
}

const LIST_COLUMNS: &str = "name,size,alloc,free,frag,cap,health";

/// Parses `zpool list -H -p [-v -P] -o name,size,alloc,free,frag,cap,health`.
/// A pool's line starts with its name; each vdev and disk under it starts
/// with a tab. Group headings (special, logs, cache) are printed with spaces,
/// not tabs, and are skipped.
pub fn parse_list(s: &str) -> Result<Vec<PoolStatus>, String> {
    // A figure, or None for "-". frag and cap carry a % without -p on some
    // versions.
    let n = |f: &str| f.trim_end_matches('%').parse::<u64>().ok();
    let mut pools: Vec<PoolStatus> = Vec::new();
    for l in s.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        if !f[0].is_empty() && f.len() > 1 {
            let field = |i: usize, key: &str| f.get(i).and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| format!("zpool list: {}: invalid or missing {key}", f[0]));
            let size = field(1, "size")?;
            let alloc = field(2, "alloc")?;
            if f.len() != 7 || f[6].is_empty() || f[6] == "-" {
                return Err(format!("zpool list: {}: incomplete pool summary or missing health", f[0]));
            }
            pools.push(PoolStatus {
                name: f[0].to_string(),
                size,
                alloc,
                frag: n(f[4]),
                health: f[6].to_string(),
                ..Default::default()
            });
        } else if f.len() == 8 && f[0].is_empty() {
            // `if let ... else { continue }`: a vdev line before any pool
            // line has nowhere to go.
            let Some(p) = pools.last_mut() else { continue };
            let Some(size) = n(f[2]) else { continue };
            p.space.insert(
                f[1].to_string(),
                Space { size, alloc: n(f[3]), free: n(f[4]), frag: n(f[5]), cap: n(f[6]) },
            );
        }
    }
    Ok(pools)
}

pub fn apply_status_json(pools: &mut [PoolStatus], j: &Json) {
    let Some(all) = j.get("pools") else { return };
    for p in pools.iter_mut() {
        let Some(pj) = all.get(&p.name) else { continue };
        if let Some(s) = pj.get("scan_stats") {
            let n = |k: &str| s.get(k).and_then(Json::u64).unwrap_or(0);
            let st = |k: &str| s.get(k).and_then(Json::str).unwrap_or("").to_string();
            p.scan = Some(Scan {
                function: st("function"),
                state: st("state"),
                to_examine: n("to_examine"),
                issued: n("issued"),
                pass_issued: n("issued_bytes_per_scan"),
                pass_start: n("pass_start"),
                spent_paused: n("scrub_spent_paused"),
                paused: n("scrub_pause") != 0,
                errors: n("errors"),
            })
            .filter(|s| !s.function.is_empty() && s.state != "NONE");
        }
        walk_vdevs(pj, &mut p.vdevs, 0);
    }
}

/// Every object with a `vdev_type`, wherever it sits — under `vdevs`, or in
/// the special, log, cache, spare and dedup sections — keyed as iostat -P
/// names it.
fn walk_vdevs(j: &Json, out: &mut HashMap<String, VdevStatus>, depth: usize) {
    if depth > 16 {
        return;
    }
    if j.get("vdev_type").is_some() {
        let n = |k: &str| j.get(k).and_then(Json::u64);
        let key = j.get("path").or_else(|| j.get("name")).and_then(Json::str).unwrap_or("").to_string();
        out.insert(
            key,
            VdevStatus {
                state: j.get("state").and_then(Json::str).unwrap_or("").to_string(),
                read_errors: n("read_errors"),
                write_errors: n("write_errors"),
                checksum_errors: n("checksum_errors"),
            },
        );
    }
    for (_, v) in j.members() {
        if matches!(v, Json::Obj(_)) {
            walk_vdevs(v, out, depth + 1);
        }
    }
}

pub struct StatusFeed {
    pub rx: Receiver<Result<StatusReport, String>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for StatusFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

pub fn status_feed(every: Duration) -> StatusFeed {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        while !cancelled.load(Ordering::Relaxed) {
            if tx.send(read_status(&cancelled)).is_err() || cancelled.load(Ordering::Relaxed) {
                break;
            }
            // Joining the pipe readers can leave an unpark token. Always
            // measure the delay against the clock, including after a wake.
            let next = Instant::now() + every;
            while !cancelled.load(Ordering::Relaxed) {
                let Some(left) = next.checked_duration_since(Instant::now()) else { break };
                std::thread::park_timeout(left);
            }
        }
    });
    StatusFeed { rx, stop, thread: Some(thread) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
1800000000
                                                     capacity        operations   bandwidth  total_wait  disk_wait
pool                                              alloc   free   read  write   read  write   read  write   read  write
------------------------------------------------  -----  -----  -----  -----  -----  -----  -----  -----  -----  -----
tank                                              80000000000000  50000000000000  12  30  1048576  2097152  150000  2000000  100000  900000
  raidz1-0                                        40000000000000  25000000000000  6  15  524288  1048576  -  -  -  -
    /dev/disk/by-id/ata-DISK_A-part1                       0      0  3  7  262144  524288  -  -  -  -
special                                                    -      -  -  -  -  -  -  -  -  -
  mirror-2                                        60000000000  340000000000  0  0  0  0  -  -  -  -
    /dev/disk/by-id/ata-SSD A-part1                        0      0  0  0  0  0  -  -  -  -
------------------------------------------------  -----  -----  -----  -----  -----  -----  -----  -----  -----  -----
";

    #[test]
    fn parses_the_indented_tree() {
        let rows: Vec<IoRow> = SAMPLE.lines().filter_map(parse_row).collect();
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].name, "tank");
        assert_eq!(rows[0].depth, 0);
        assert!(!rows[0].group);
        assert_eq!(rows[0].rbw, Some(1048576.0));
        assert_eq!(rows[0].wwait, Some(2_000_000.0));
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].rwait, None);
        assert_eq!(rows[2].depth, 2);
        assert_eq!(rows[2].name, "/dev/disk/by-id/ata-DISK_A-part1");
        assert!(rows[3].group);
        assert_eq!(rows[3].name, "special");
        // A space in a path does not shift the figures.
        assert_eq!(rows[5].name, "/dev/disk/by-id/ata-SSD A-part1");
        assert_eq!(rows[5].alloc, Some(0));
        assert_eq!(rows[3].alloc, None);
        assert!(is_stamp("1800000000"));
        assert!(!is_stamp("  12"));
    }

    #[test]
    fn interval_is_written_in_seconds() {
        assert_eq!(interval_arg(1000), "1");
        assert_eq!(interval_arg(500), "0.5");
        assert_eq!(interval_arg(2500), "2.5");
    }

    #[test]
    fn reads_list_and_status() {
        let mut p = parse_list("tank\t1000\t600\t400\t12\t60\tDEGRADED\nfast\t10\t1\t9\t-\t10\tONLINE\n").unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].frag, Some(12));
        assert_eq!(p[1].frag, None);
        let j = json::parse(
            r#"{"pools":{"tank":{"scan_stats":{"function":"SCRUB","state":"SCANNING","to_examine":1000,
            "issued":250,"issued_bytes_per_scan":250,"pass_start":100,"scrub_pause":0,"scrub_spent_paused":0},
            "vdevs":{"tank":{"vdev_type":"root","name":"tank","state":"DEGRADED","vdevs":{
            "mirror-0":{"vdev_type":"mirror","name":"mirror-0","state":"DEGRADED","vdevs":{
            "d1":{"vdev_type":"disk","name":"d1","path":"/dev/d1","state":"FAULTED","read_errors":3}}}}}}}}}"#,
        )
        .unwrap();
        apply_status_json(&mut p, &j);
        let s = p[0].scan.as_ref().unwrap();
        assert!(s.running());
        assert_eq!(s.pct(), 25.0);
        assert_eq!(s.eta_s(110), Some(30.0));
        assert_eq!(p[0].vdevs["/dev/d1"].state, "FAULTED");
        assert_eq!(p[0].vdevs["/dev/d1"].error_counts(), "3/-/-");
        assert_eq!(p[0].vdevs["mirror-0"].state, "DEGRADED");
    }

    #[test]
    fn reads_vdev_space_from_list_v() {
        let text = "tank\t1200\t700\t500\t20\t58\tONLINE\n\
                    \tmirror-0\t1000\t690\t310\t21\t69\tONLINE\n\
                    \t/dev/disk/by-id/d1-part1\t1010\t-\t-\t-\t-\tONLINE\n\
                    special                -     -     -     -     -  -\n\
                    \tmirror-1\t200\t10\t190\t3\t5\tONLINE\n";
        let p = parse_list(text).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].size, 1200);
        assert_eq!(p[0].frag, Some(20));
        let m = &p[0].space["mirror-0"];
        assert_eq!((m.size, m.alloc, m.free, m.frag, m.cap), (1000, Some(690), Some(310), Some(21), Some(69)));
        let d = &p[0].space["/dev/disk/by-id/d1-part1"];
        assert_eq!((d.size, d.alloc, d.cap), (1010, None, None));
        assert_eq!(p[0].space["mirror-1"].cap, Some(5));
    }
}
