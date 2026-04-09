//! EPHEMERAL.md consumption — reads recent session context.
//!
//! Returns EPHEMERAL.md content as a String for injection into
//! the entity's context. Does not clear the file (archival handles that).

use std::fs;
use std::path::Path;

use crate::error::RecallError;

/// Read EPHEMERAL.md content without clearing it.
/// Returns None if the file doesn't exist or is empty.
pub fn consume(ephemeral_path: &Path) -> Result<Option<String>, RecallError> {
    if !ephemeral_path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(ephemeral_path)?;

    if content.trim().is_empty() {
        return Ok(None);
    }

    Ok(Some(content))
}

/// CLI command: print EPHEMERAL.md to stdout.
pub fn run(ephemeral_path: &Path) -> Result<(), RecallError> {
    match consume(ephemeral_path)? {
        Some(content) => {
            println!("{}", content.trim());
        }
        None => {
            // Silent if empty or missing
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let ephemeral = tmp.path().join("EPHEMERAL.md");
        (tmp, ephemeral)
    }

    #[test]
    fn consume_returns_content() {
        let (_tmp, ephemeral) = setup_test_dir();
        let content = "# Last Session\nWorked on recall-echo.\n";
        fs::write(&ephemeral, content).unwrap();

        let result = consume(&ephemeral).unwrap();
        assert_eq!(result, Some(content.to_string()));
    }

    #[test]
    fn consume_does_not_clear() {
        let (_tmp, ephemeral) = setup_test_dir();
        let content = "Session content.";
        fs::write(&ephemeral, content).unwrap();

        consume(&ephemeral).unwrap();
        let remaining = fs::read_to_string(&ephemeral).unwrap();
        assert_eq!(remaining, content);
    }

    #[test]
    fn consume_returns_none_on_empty() {
        let (_tmp, ephemeral) = setup_test_dir();
        fs::write(&ephemeral, "").unwrap();
        assert!(consume(&ephemeral).unwrap().is_none());
    }

    #[test]
    fn consume_returns_none_on_whitespace() {
        let (_tmp, ephemeral) = setup_test_dir();
        fs::write(&ephemeral, "   \n\n  \n").unwrap();
        assert!(consume(&ephemeral).unwrap().is_none());
    }

    #[test]
    fn consume_returns_none_on_missing() {
        let (_tmp, ephemeral) = setup_test_dir();
        assert!(consume(&ephemeral).unwrap().is_none());
    }

    #[test]
    fn consume_is_idempotent() {
        let (_tmp, ephemeral) = setup_test_dir();
        fs::write(&ephemeral, "Session content.").unwrap();

        consume(&ephemeral).unwrap();
        consume(&ephemeral).unwrap();
        let remaining = fs::read_to_string(&ephemeral).unwrap();
        assert_eq!(remaining, "Session content.");
    }
}
