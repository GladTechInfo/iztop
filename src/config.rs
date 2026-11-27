//! Settings file: `~/.config/iztop/iztop.conf`, `key = value`, as istop's.
//!
//! Only what is chosen interactively lives here — the pools' order, which
//! are hidden and the history window — and it is written only when `W` is
//! pressed.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// Pools in this order first; any other follows alphabetically, so a newly
    /// imported pool still appears.
    pub pool_order: Vec<String>,
    pub pool_hidden: Vec<String>,
    /// Seconds of history the graphs span; None: the default.
    pub window: Option<usize>,
    /// The capacity view (true) or the I/O view (false), if saved.
    pub capacity: Option<bool>,
}

pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("iztop").join("iztop.conf"))
}

/// `io` or `capacity`, as the config file and --view spell the view:
/// `Some(true)` for the capacity view.
pub fn parse_view(v: &str) -> Option<bool> {
    match v {
        "io" => Some(false),
        "capacity" => Some(true),
        _ => None,
    }
}

fn parse_list(v: &str) -> Vec<String> {
    v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

impl Config {
    /// Parses a config file's text over the defaults. Every problem is
    /// reported, with its line number; the rest of the file still applies.
    pub fn parse(text: &str) -> (Config, Vec<String>) {
        let mut c = Config::default();
        let mut errors = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                errors.push(format!("line {}: expected key = value", n + 1));
                continue;
            };
            let v = v.trim().trim_matches('"');
            match k.trim() {
                "pool_order" => c.pool_order = parse_list(v),
                "pool_hidden" => c.pool_hidden = parse_list(v),
                "view" if v.is_empty() => c.capacity = None,
                "view" => match parse_view(v) {
                    Some(cap) => c.capacity = Some(cap),
                    None => errors.push(format!("line {}: view takes io or capacity", n + 1)),
                },
                "window" if v.is_empty() => c.window = None,
                "window" => match crate::app::parse_window(v) {
                    Some(w) => c.window = Some(w),
                    None => errors.push(format!("line {}: window takes 1m, 10m or 60m", n + 1)),
                },
                other => errors.push(format!("line {}: unknown key \"{}\"", n + 1, other)),
            }
        }
        (c, errors)
    }

    pub fn load(path: &Path) -> Result<(Config, Vec<String>), String> {
        match std::fs::read_to_string(path) {
            Ok(t) => Ok(Config::parse(&t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Config::default(), Vec::new())),
            Err(e) => Err(format!("{}: {}", path.display(), e)),
        }
    }

    pub fn render(&self) -> String {
        format!(
            "\
# iztop configuration. `key = value`; `#` starts a comment.
# Written when W is pressed.

# Pools in this order first, comma-separated; any other follows by name.
pool_order = {}

# Pools not shown.
pool_hidden = {}

# History the graphs span: 1m, 10m or 60m; empty for the default.
window = {}

# View at start: io or capacity; empty for the default (io).
view = {}
",
            self.pool_order.join(", "),
            self.pool_hidden.join(", "),
            self.window.map(crate::app::window_label).unwrap_or_default(),
            match self.capacity {
                Some(true) => "capacity",
                Some(false) => "io",
                None => "",
            },
        )
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
        }
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let (tmp, mut file) = loop {
            let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let tmp = path.with_extension(format!("conf.{}.{seq}.tmp", std::process::id()));
            if tmp == path {
                continue;
            }
            match std::fs::File::create_new(&tmp) {
                Ok(file) => break (tmp, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("{}: {}", tmp.display(), e)),
            }
        };
        let result = file.write_all(self.render().as_bytes()).and_then(|()| std::fs::rename(&tmp, path));
        if result.is_err() {
            let _ = std::fs::remove_file(tmp);
        }
        result.map_err(|e| format!("{}: {}", path.display(), e))
    }

    /// Indices into `names` in display order: listed names first, as listed,
    /// then the rest as they came. Hidden ones are included.
    pub fn rank(&self, names: &[&str]) -> Vec<usize> {
        let pos = |n: &str| self.pool_order.iter().position(|o| o == n).unwrap_or(usize::MAX);
        let mut idx: Vec<usize> = (0..names.len()).collect();
        // Stable, so the unlisted keep their own order.
        idx.sort_by_key(|&i| pos(names[i]));
        idx
    }

    pub fn shown(&self, name: &str) -> bool {
        !self.pool_hidden.iter().any(|h| h == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_config_parses_back() {
        let c = Config {
            pool_order: vec!["fast".into(), "bulk".into()],
            pool_hidden: vec!["boot".into()],
            window: Some(600),
            capacity: Some(true),
        };
        let (back, errors) = Config::parse(&c.render());
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(back, c);
        let (d, errors) = Config::parse(&Config::default().render());
        assert!(errors.is_empty());
        assert_eq!(d, Config::default());
    }

    #[test]
    fn concurrent_saves_publish_a_complete_config() {
        let dir = std::env::temp_dir().join(format!("iztop-config-test-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("iztop.conf");
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for id in 0..8 {
                let (path, barrier) = (&path, &barrier);
                scope.spawn(move || {
                    let cfg = Config { pool_order: vec![id.to_string(); 64], ..Default::default() };
                    barrier.wait();
                    cfg.save(path).unwrap();
                });
            }
        });
        let (saved, warnings) = Config::load(&path).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(saved.pool_order.len(), 64);
        assert!(saved.pool_order.iter().all(|n| n == &saved.pool_order[0]));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn listed_first_then_the_rest_in_order() {
        let (c, errors) = Config::parse("pool_order = c, a\npool_hidden = b\nbogus = 1");
        assert_eq!(errors.len(), 1);
        assert_eq!(c.rank(&["a", "b", "c", "d"]), [2, 0, 1, 3]);
        assert!(!c.shown("b"));
        assert!(c.shown("a"));
    }

    #[test]
    fn window_parses_and_rejects() {
        let (c, errors) = Config::parse("window = 60m");
        assert!(errors.is_empty());
        assert_eq!(c.window, Some(3600));
        let (c, errors) = Config::parse("window = 5m");
        assert_eq!(errors.len(), 1);
        assert_eq!(c.window, None);
    }
}
