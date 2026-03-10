use std::fs;
use std::path::Path;

use serde::Deserialize;

const DEFAULT_MAX_ENTRIES: usize = 5;

#[derive(Deserialize, Debug, Default)]
pub struct Config {
    #[serde(default)]
    pub ephemeral: EphemeralConfig,
}

#[derive(Deserialize, Debug)]
pub struct EphemeralConfig {
    #[serde(default = "default_max_entries")]
    pub max_entries: usize,
}

impl Default for EphemeralConfig {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }
}

fn default_max_entries() -> usize {
    DEFAULT_MAX_ENTRIES
}

/// Load config from .recall-echo.toml in the given directory.
/// Returns defaults if file doesn't exist or is malformed.
pub fn load_from_dir(dir: &Path) -> Config {
    load(dir)
}

/// Load config from .recall-echo.toml in the base dir.
/// Returns defaults if file doesn't exist or is malformed.
pub fn load(base: &Path) -> Config {
    let config_path = base.join(".recall-echo.toml");
    if !config_path.exists() {
        return Config::default();
    }

    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return Config::default(),
    };

    parse_config(&content)
}

fn parse_config(content: &str) -> Config {
    let mut max_entries = DEFAULT_MAX_ENTRIES;
    let mut in_ephemeral = false;

    for line in content.lines() {
        let line = line.trim();
        if line == "[ephemeral]" {
            in_ephemeral = true;
            continue;
        }
        if line.starts_with('[') {
            in_ephemeral = false;
            continue;
        }
        if in_ephemeral {
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim();
                if key == "max_entries" {
                    if let Ok(n) = val.parse::<usize>() {
                        if (1..=50).contains(&n) {
                            max_entries = n;
                        }
                    }
                }
            }
        }
    }

    Config {
        ephemeral: EphemeralConfig { max_entries },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.ephemeral.max_entries, 5);
    }

    #[test]
    fn parse_custom_entries() {
        let cfg = parse_config("[ephemeral]\nmax_entries = 10\n");
        assert_eq!(cfg.ephemeral.max_entries, 10);
    }

    #[test]
    fn parse_out_of_range_uses_default() {
        let cfg = parse_config("[ephemeral]\nmax_entries = 0\n");
        assert_eq!(cfg.ephemeral.max_entries, 5);

        let cfg = parse_config("[ephemeral]\nmax_entries = 100\n");
        assert_eq!(cfg.ephemeral.max_entries, 5);
    }

    #[test]
    fn parse_missing_section_uses_default() {
        let cfg = parse_config("# just a comment\n");
        assert_eq!(cfg.ephemeral.max_entries, 5);
    }

    #[test]
    fn load_nonexistent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load(tmp.path());
        assert_eq!(cfg.ephemeral.max_entries, 5);
    }

    #[test]
    fn load_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(".recall-echo.toml"),
            "[ephemeral]\nmax_entries = 3\n",
        )
        .unwrap();
        let cfg = load(tmp.path());
        assert_eq!(cfg.ephemeral.max_entries, 3);
    }
}
