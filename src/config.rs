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
}

pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("iztop").join("iztop.conf"))
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
",
            self.pool_order.join(", "),
            self.pool_hidden.join(", "),
            self.window.map(crate::app::window_label).unwrap_or_default(),
        )
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
        }
        let tmp = path.with_extension("conf.tmp");
        std::fs::write(&tmp, self.render()).map_err(|e| format!("{}: {}", tmp.display(), e))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {}", path.display(), e))
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
        let c = Config { pool_order: vec!["fast".into(), "bulk".into()], pool_hidden: vec!["boot".into()], window: Some(600) };
        let (back, errors) = Config::parse(&c.render());
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(back, c);
        let (d, errors) = Config::parse(&Config::default().render());
        assert!(errors.is_empty());
        assert_eq!(d, Config::default());
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
