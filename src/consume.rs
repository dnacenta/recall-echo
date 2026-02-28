use std::fs;
use std::path::Path;

use crate::paths;

fn run_with_path(ephemeral: &Path) -> Result<(), String> {
    if !ephemeral.exists() {
        return Ok(());
    }

    let content =
        fs::read_to_string(ephemeral).map_err(|e| format!("Failed to read EPHEMERAL.md: {e}"))?;

    if content.trim().is_empty() {
        return Ok(());
    }

    // Output for Claude to ingest via hook stdout
    println!("[MEMORY — Today's Session Log (from EPHEMERAL.md)]");
    println!("{}", content.trim());
    println!(
        "[END MEMORY — EPHEMERAL.md is accumulative. Append your session summary at session end.]"
    );

    Ok(())
}

pub fn run() -> Result<(), String> {
    let ephemeral = paths::ephemeral_file()?;
    run_with_path(&ephemeral)
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
    fn consumes_without_clearing_ephemeral() {
        let (_tmp, ephemeral) = setup_test_dir();

        let content = "# Last Session\nWorked on recall-echo.\n\n## Action Items\n- Add consume\n";
        fs::write(&ephemeral, content).unwrap();

        run_with_path(&ephemeral).unwrap();

        let remaining = fs::read_to_string(&ephemeral).unwrap();
        assert_eq!(remaining, content);
    }

    #[test]
    fn silent_on_empty_file() {
        let (_tmp, ephemeral) = setup_test_dir();
        fs::write(&ephemeral, "").unwrap();
        run_with_path(&ephemeral).unwrap();
    }

    #[test]
    fn silent_on_whitespace_only() {
        let (_tmp, ephemeral) = setup_test_dir();
        fs::write(&ephemeral, "   \n\n  \n").unwrap();
        run_with_path(&ephemeral).unwrap();
    }

    #[test]
    fn silent_on_missing_file() {
        let (_tmp, ephemeral) = setup_test_dir();
        run_with_path(&ephemeral).unwrap();
    }

    #[test]
    fn idempotent_double_consume() {
        let (_tmp, ephemeral) = setup_test_dir();
        fs::write(&ephemeral, "Session content.").unwrap();

        run_with_path(&ephemeral).unwrap();
        run_with_path(&ephemeral).unwrap();

        let remaining = fs::read_to_string(&ephemeral).unwrap();
        assert_eq!(remaining, "Session content.");
    }
}
