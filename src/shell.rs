// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One quoting rule for shell text a person might paste.
//!
//! A plain word is left as it is. Anything else, including the empty word,
//! is wrapped in single quotes, so `$`, backticks, `;` and spaces stay
//! literal. An embedded single quote is closed, escaped and reopened.
//! Hook paths, the MCP line and the rebuild commands all go through here.

/// Whether `s` is safe to interpolate bare: non-empty, and every character
/// in the install-path alphabet.
#[must_use]
pub(crate) fn is_plain(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+-".contains(c))
}

/// Single-quote `s`. An embedded `'` is closed, escaped and reopened.
#[must_use]
pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `s` when it is plain, otherwise [`quote`]. The empty string is `''`.
#[must_use]
pub(crate) fn arg(s: &str) -> String {
    if is_plain(s) {
        s.to_string()
    } else {
        quote(s)
    }
}

/// One pasteable line: each argument through [`arg`], joined by one space.
#[must_use]
pub(crate) fn line(argv: &[String]) -> String {
    argv.iter()
        .map(|part| arg(part))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_is_the_install_path_alphabet() {
        assert!(is_plain("/usr/local/bin/recall-echo"));
        assert!(is_plain("--pulse-root"));
        assert!(is_plain("A.Z_0-9/+"));
        for word in [
            "",
            " ",
            "alpha beta",
            "o'brien",
            "$(x)",
            "`x`",
            "alpha;beta",
            "say\"hi\"",
            "=",
            "~",
            ":",
            "a=b",
            "~/alpha",
            "a:b",
        ] {
            assert!(!is_plain(word), "{word}");
        }
    }

    /// Today's path quoting: always single quotes, `'` written as `'\''`.
    #[test]
    fn quote_matches_the_existing_path_escape() {
        assert_eq!(quote("/home/d/o'brien"), r"'/home/d/o'\''brien'");
        assert_eq!(quote("/tmp/alpha beta"), "'/tmp/alpha beta'");
    }

    #[test]
    fn arg_leaves_plain_words_bare_and_quotes_the_empty_word() {
        assert_eq!(arg("alpha"), "alpha");
        assert_eq!(arg("/tmp/alpha"), "/tmp/alpha");
        assert_eq!(arg(""), "''");
        assert_eq!(arg("alpha beta"), "'alpha beta'");
        assert_eq!(arg("o'brien"), r"'o'\''brien'");
        for word in ["=", "~", ":"] {
            assert_eq!(arg(word), quote(word), "{word}");
        }
    }

    #[test]
    fn line_joins_quoted_args_with_one_space() {
        let argv = ["alpha".to_string(), "o'brien".to_string(), String::new()];
        assert_eq!(line(&argv), r"alpha 'o'\''brien' ''");
    }

    /// The line is a script: `sh` prints every argument back and runs nothing
    /// that was hiding inside one.
    #[cfg(unix)]
    #[test]
    fn a_line_round_trips_through_sh_and_runs_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let touch = format!("touch {}", marker.display());
        let args = [
            "alpha".to_string(),
            "o'brien".to_string(),
            "/tmp/alpha beta".to_string(),
            "say \"hi\"".to_string(),
            format!("$({touch})"),
            format!("`{touch}`"),
            format!(";{touch}"),
            String::new(),
        ];
        let mut argv = vec!["printf".to_string(), "%s\\n".to_string()];
        argv.extend(args.iter().cloned());
        let script = line(&argv);

        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .current_dir(dir.path())
            .output()
            .expect("sh");
        assert!(
            output.status.success(),
            "line: {script}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut expected = args.join("\n");
        expected.push('\n');
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        assert!(!marker.exists(), "an argument was executed as a command");
    }
}
