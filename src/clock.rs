//! Local wall-clock time, for the header.
//!
//! Rust's standard library knows only UTC. Rather than pull in a date crate
//! for one clock, this reads the system zone file `/etc/localtime` (TZif,
//! RFC 8536) itself: the explicit transitions, and the POSIX rule in the
//! file's footer for times after them. Modern "slim" zone files rely on that
//! footer for every date since the rules last changed, so it is not optional.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds east of UTC at `t`, Unix seconds. Falls back to UTC on anything
/// unreadable.
pub struct Zone {
    /// (transition time, offset from then on)
    transitions: Vec<(i64, i32)>,
    initial: i32,
    rule: Option<Rule>,
}

impl Zone {
    pub fn local() -> Zone {
        std::fs::read("/etc/localtime").ok().and_then(|b| parse_tzif(&b)).unwrap_or(Zone {
            transitions: Vec::new(),
            initial: 0,
            rule: None,
        })
    }

    pub fn offset_at(&self, t: i64) -> i32 {
        match self.transitions.iter().rposition(|(at, _)| *at <= t) {
            Some(i) if i + 1 < self.transitions.len() || self.rule.is_none() => self.transitions[i].1,
            Some(i) => self.rule.as_ref().map(|r| r.offset_at(t)).unwrap_or(self.transitions[i].1),
            None => self.initial,
        }
    }

    /// "HH:MM:SS" now.
    pub fn now_hms(&self) -> String {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let local = (t + self.offset_at(t) as i64).rem_euclid(86_400);
        format!("{:02}:{:02}:{:02}", local / 3600, local / 60 % 60, local % 60)
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.b.get(self.at..self.at + n)?;
        self.at += n;
        Some(s)
    }
    fn be32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn be64(&mut self) -> Option<i64> {
        Some(i64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
}

fn parse_tzif(b: &[u8]) -> Option<Zone> {
    let mut r = Reader { b, at: 0 };
    let header = |r: &mut Reader| -> Option<(u8, [usize; 6])> {
        if r.take(4)? != b"TZif" {
            return None;
        }
        let version = r.take(1)?[0];
        r.take(15)?;
        let mut c = [0usize; 6];
        for v in c.iter_mut() {
            *v = r.be32()? as usize;
        }
        Some((version, c))
    };
    let (version, c) = header(&mut r)?;
    let [isut, isstd, leap, time, typ, chars] = c;
    let (wide, c) = if version >= b'2' {
        // Skip the 32-bit body; the 64-bit one follows with its own header.
        r.take(time * 5 + typ * 6 + chars + leap * 8 + isstd + isut)?;
        (true, header(&mut r)?.1)
    } else {
        (false, c)
    };
    let [isut, isstd, leap, time, typ, chars] = c;
    let mut times = Vec::with_capacity(time);
    for _ in 0..time {
        times.push(if wide { r.be64()? } else { r.be32()? as i64 });
    }
    let idx: Vec<usize> = r.take(time)?.iter().map(|i| *i as usize).collect();
    let mut types = Vec::with_capacity(typ);
    for _ in 0..typ {
        let off = r.be32()?;
        r.take(2)?;
        types.push(off);
    }
    r.take(chars + leap * if wide { 12 } else { 8 } + isstd + isut)?;
    let rule = if wide {
        // Footer: "\n<POSIX TZ string>\n".
        let rest = std::str::from_utf8(r.b.get(r.at..)?).ok()?;
        rest.trim_matches('\n').lines().next().and_then(Rule::parse)
    } else {
        None
    };
    let transitions = times.into_iter().zip(idx).filter_map(|(t, i)| Some((t, *types.get(i)?))).collect();
    Some(Zone { transitions, initial: types.first().copied().unwrap_or(0), rule })
}

/// A POSIX TZ rule such as `CET-1CEST,M3.5.0,M10.5.0/3`. Only the `Mm.w.d`
/// form of the change dates is understood; every zone in use today writes
/// its rules that way.
#[derive(Debug)]
struct Rule {
    std: i32,
    dst: Option<(i32, MDate, MDate)>,
}

#[derive(Debug, Clone, Copy)]
struct MDate {
    month: u32,
    week: u32,
    wday: u32,
    secs: i64,
}

impl Rule {
    fn parse(s: &str) -> Option<Rule> {
        let mut p = s;
        skip_name(&mut p)?;
        let std = -parse_offset(&mut p)?;
        if p.is_empty() {
            return Some(Rule { std, dst: None });
        }
        skip_name(&mut p)?;
        let dst = if p.starts_with(',') { std + 3600 } else { -parse_offset(&mut p)? };
        let mut parts = p.strip_prefix(',')?.split(',');
        let start = parse_mdate(parts.next()?)?;
        let end = parse_mdate(parts.next()?)?;
        Some(Rule { std, dst: Some((dst, start, end)) })
    }

    fn offset_at(&self, t: i64) -> i32 {
        let Some((dst, start, end)) = self.dst else { return self.std };
        let year = civil_from_days((t + self.std as i64).div_euclid(86_400)).0;
        // Change times are local: the start in standard time, the end in
        // daylight time.
        let s = start.at(year) - self.std as i64;
        let e = end.at(year) - dst as i64;
        let in_dst = if s < e { t >= s && t < e } else { t >= s || t < e };
        if in_dst {
            dst
        } else {
            self.std
        }
    }
}

impl MDate {
    /// Local seconds since the epoch at which this rule fires in `year`.
    fn at(&self, year: i64) -> i64 {
        let first = days_from_civil(year, self.month, 1);
        let first_wday = (first + 4).rem_euclid(7) as u32; // 1970-01-01 was a Thursday.
        let mut day = 1 + (self.wday + 7 - first_wday) % 7 + (self.week - 1) * 7;
        let len = days_in_month(year, self.month);
        while day > len {
            day -= 7;
        }
        (first + day as i64 - 1) * 86_400 + self.secs
    }
}

fn skip_name(p: &mut &str) -> Option<()> {
    if let Some(rest) = p.strip_prefix('<') {
        let end = rest.find('>')?;
        *p = &rest[end + 1..];
    } else {
        let n = p.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(p.len());
        if n < 3 {
            return None;
        }
        *p = &p[n..];
    }
    Some(())
}

/// `[+-]hh[:mm[:ss]]`, as seconds. POSIX offsets are west-positive.
fn parse_offset(p: &mut &str) -> Option<i32> {
    let (sign, rest) = match p.as_bytes().first()? {
        b'-' => (-1, &p[1..]),
        b'+' => (1, &p[1..]),
        _ => (1, *p),
    };
    let n = rest.find(|c: char| !(c.is_ascii_digit() || c == ':')).unwrap_or(rest.len());
    let mut v = rest[..n].split(':').map(|x| x.parse::<i32>().ok());
    let h = v.next()??;
    let m = v.next().flatten().unwrap_or(0);
    let s = v.next().flatten().unwrap_or(0);
    *p = &rest[n..];
    Some(sign * (h * 3600 + m * 60 + s))
}

fn parse_mdate(s: &str) -> Option<MDate> {
    let (date, time) = match s.split_once('/') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut f = date.strip_prefix('M')?.split('.').map(|x| x.parse::<u32>().ok());
    let (month, week, wday) = (f.next()??, f.next()??, f.next()??);
    let secs = match time {
        Some(t) => {
            let mut t = t;
            parse_offset(&mut t)? as i64
        }
        None => 7200,
    };
    ((1..=12).contains(&month) && (1..=5).contains(&week) && wday <= 6).then_some(MDate { month, week, wday, secs })
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 11, 15), 20_772);
        assert_eq!(civil_from_days(20_772), (2026, 11, 15));
    }

    #[test]
    fn central_european_rule() {
        let r = Rule::parse("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
        // 2026-07-01 12:00 UTC: summer, UTC+2.
        let summer = days_from_civil(2026, 7, 1) * 86_400 + 12 * 3600;
        assert_eq!(r.offset_at(summer), 7200);
        // 2026-11-15: winter, UTC+1.
        assert_eq!(r.offset_at(days_from_civil(2026, 11, 15) * 86_400), 3600);
        // Change on 2026-03-29 at 01:00 UTC (02:00 CET).
        let change = days_from_civil(2026, 3, 29) * 86_400 + 3600;
        assert_eq!(r.offset_at(change - 1), 3600);
        assert_eq!(r.offset_at(change), 7200);
    }

    #[test]
    fn southern_rule_wraps_the_year() {
        // Sydney: AEST-10AEDT,M10.1.0,M4.1.0/3
        let r = Rule::parse("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(r.offset_at(days_from_civil(2026, 1, 15) * 86_400), 11 * 3600);
        assert_eq!(r.offset_at(days_from_civil(2026, 7, 15) * 86_400), 10 * 3600);
    }

    #[test]
    fn rules_without_dst_and_with_angle_names() {
        assert_eq!(Rule::parse("UTC0").unwrap().offset_at(0), 0);
        assert_eq!(Rule::parse("<+0530>-5:30").unwrap().offset_at(0), 19_800);
    }

    #[test]
    fn system_zone_file_parses() {
        if let Ok(b) = std::fs::read("/etc/localtime") {
            assert!(parse_tzif(&b).is_some());
        }
    }
}
