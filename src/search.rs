use std::fs;
use std::io::BufRead;
use std::path::Path;

use crate::error::RecallError;
use crate::paths;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Query tokens shorter than this never qualify a file on their own. They are
/// almost always function words, and because matching is substring-based they
/// also hit inside unrelated words ("i" matches every file ever written).
const MIN_TOKEN_LEN: usize = 3;

/// A token present in at least this share of the archive cannot tell one
/// conversation from another, so it neither qualifies a file nor scores.
/// Chosen above 0.5 so that a genuinely topical token — an entity that talks
/// about one project in most of its sessions — survives.
const DOCUMENT_FREQUENCY_CEILING: f64 = 0.8;

/// Document frequency needs a corpus to mean anything: with two files every
/// token sits at 0.0, 0.5 or 1.0, and the ceiling would discard real terms.
const MIN_FILES_FOR_FREQUENCY_FILTER: usize = 5;

pub struct SearchResult {
    pub file: String,
    pub line_num: usize,
    pub line: String,
}

/// A file-level ranked search result.
pub struct RankedFile {
    pub file: String,
    pub match_count: usize,
    pub score: f64,
    pub preview_lines: Vec<String>,
}

/// One archive file, read once so document frequencies can be measured across
/// the corpus before any file is scored.
struct ArchiveFile {
    name: String,
    text: String,
}

pub fn run(query: &str, context_lines: usize) -> Result<(), RecallError> {
    let base = paths::memory_dir()?;
    let results = search_with_base(query, &base, context_lines)?;

    if results.is_empty() {
        eprintln!("No matches found for \"{query}\"");
        return Ok(());
    }

    eprintln!(
        "{BOLD}{} match{} across conversation archives{RESET}\n",
        results.len(),
        if results.len() == 1 { "" } else { "es" }
    );

    let mut current_file = String::new();
    for result in &results {
        if result.file != current_file {
            eprintln!("{CYAN}{}{RESET}", result.file);
            current_file = result.file.clone();
        }
        eprintln!("  {DIM}{:>4}{RESET}  {}", result.line_num, result.line);
    }

    Ok(())
}

/// Ranked search: returns files sorted by relevance score.
///
/// A file qualifies if it contains *any* discriminative query token — natural
/// language questions always carry a token no single archive has, so requiring
/// all of them returns nothing. Ranking then separates the wheat from the
/// chaff: files matching more distinct tokens, more often, more recently,
/// score higher.
pub fn ranked_search(
    query: &str,
    base: &Path,
    max_results: usize,
) -> Result<Vec<RankedFile>, RecallError> {
    let files = read_archive_files(base)?;
    let lowered: Vec<String> = files.iter().map(|f| f.text.to_lowercase()).collect();
    let query_lower = query.to_lowercase();
    let tokens = discriminative_tokens(&query_lower, &lowered);
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let total_files = files.len();
    let mut ranked: Vec<RankedFile> = Vec::new();

    for (idx, (file, content_lower)) in files.iter().zip(&lowered).enumerate() {
        let hits: Vec<usize> = tokens
            .iter()
            .map(|t| content_lower.matches(t).count())
            .collect();
        let matched_tokens = hits.iter().filter(|&&n| n > 0).count();
        if matched_tokens == 0 {
            continue;
        }

        let word_match_count: usize = hits.iter().sum();

        // Lucene's coordination factor: how much of the query this file covers.
        // Under the old all-words gate every survivor covered the whole query,
        // so this term is 1.0 for anything the previous implementation would
        // have returned — it only separates the newly admitted partial matches.
        let coverage = matched_tokens as f64 / tokens.len() as f64;

        let recency = if total_files > 1 {
            0.5 + 0.5 * (idx as f64 / (total_files - 1) as f64)
        } else {
            1.0
        };

        let content_boost = if content_lower.contains(&format!(
            "### user\n\n{}",
            query_lower.chars().take(20).collect::<String>()
        )) {
            1.5
        } else {
            1.0
        };

        let score = word_match_count as f64 * coverage * recency * content_boost;

        ranked.push(RankedFile {
            file: file.name.clone(),
            match_count: word_match_count,
            score,
            preview_lines: preview_lines(&file.text, &query_lower, &tokens),
        });
    }

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked.truncate(max_results);

    Ok(ranked)
}

/// Read every `conversation-NNN.md` under `base/conversations`, in filename
/// order (which is chronological, and is what the recency term assumes).
fn read_archive_files(base: &Path) -> Result<Vec<ArchiveFile>, RecallError> {
    let conversations_dir = base.join("conversations");
    if !conversations_dir.exists() {
        return Err(RecallError::NotInitialized(
            "conversations/ directory not found. Run `recall-echo init` first.".into(),
        ));
    }

    let mut entries: Vec<_> = fs::read_dir(&conversations_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("conversation-") && name.ends_with(".md")
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());

    Ok(entries
        .iter()
        .filter_map(|entry| {
            fs::read_to_string(entry.path())
                .ok()
                .map(|text| ArchiveFile {
                    name: entry.file_name().to_string_lossy().to_string(),
                    text,
                })
        })
        .collect())
}

/// The query tokens worth searching on, in query order and without duplicates.
///
/// Three filters, each falling back to the previous stage if it would leave
/// nothing to search for:
/// 1. outer punctuation stripped, so `bowl?` and `[current` match their words;
/// 2. tokens under [`MIN_TOKEN_LEN`], or with no letter at all, dropped;
/// 3. tokens above [`DOCUMENT_FREQUENCY_CEILING`] dropped — a corpus-derived
///    stop list, which needs no hard-coded word list and works in any language.
///
/// `lowered` is the archive corpus, already lowercased, one entry per file.
fn discriminative_tokens<'a>(query_lower: &'a str, lowered: &[String]) -> Vec<&'a str> {
    let normalized = dedup_preserving_order(
        query_lower
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|w| !w.is_empty()),
    );

    let meaningful: Vec<&str> = normalized
        .iter()
        .copied()
        .filter(|w| w.chars().count() >= MIN_TOKEN_LEN && w.chars().any(char::is_alphabetic))
        .collect();
    let meaningful = if meaningful.is_empty() {
        normalized
    } else {
        meaningful
    };

    if lowered.len() < MIN_FILES_FOR_FREQUENCY_FILTER {
        return meaningful;
    }

    let ceiling = DOCUMENT_FREQUENCY_CEILING * lowered.len() as f64;
    let discriminative: Vec<&str> = meaningful
        .iter()
        .copied()
        .filter(|token| {
            let document_frequency = lowered.iter().filter(|text| text.contains(token)).count();
            (document_frequency as f64) < ceiling
        })
        .collect();

    if discriminative.is_empty() {
        meaningful
    } else {
        discriminative
    }
}

fn dedup_preserving_order<'a>(tokens: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = std::collections::HashSet::new();
    tokens.filter(|t| seen.insert(*t)).collect()
}

/// Up to three prose lines from the file that carry the query phrase or one of
/// its tokens. Headings, rules and fences are skipped: they are archive
/// structure, not conversation content.
fn preview_lines(content: &str, query_lower: &str, tokens: &[&str]) -> Vec<String> {
    let mut previews = Vec::new();
    for line in content.lines() {
        let line_lower = line.to_lowercase();
        if !line_lower.contains(query_lower) && !tokens.iter().any(|t| line_lower.contains(t)) {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("---")
            || trimmed.starts_with("```")
        {
            continue;
        }
        previews.push(trimmed.to_string());
        if previews.len() >= 3 {
            break;
        }
    }
    previews
}

/// Run ranked search and display results.
pub fn run_ranked(query: &str, max_results: usize) -> Result<(), RecallError> {
    let base = paths::memory_dir()?;
    let results = ranked_search(query, &base, max_results)?;

    if results.is_empty() {
        eprintln!("No matches found for \"{query}\"");
        return Ok(());
    }

    eprintln!(
        "{BOLD}{} conversation{} matching \"{query}\"{RESET}\n",
        results.len(),
        if results.len() == 1 { "" } else { "s" }
    );

    for (i, result) in results.iter().enumerate() {
        eprintln!(
            "  {CYAN}{}. {}{RESET}  {DIM}({} matches, score {:.1}){RESET}",
            i + 1,
            result.file,
            result.match_count,
            result.score
        );
        for preview in &result.preview_lines {
            let highlighted = highlight_match(preview, query);
            eprintln!("     {highlighted}");
        }
        if i < results.len() - 1 {
            eprintln!();
        }
    }

    Ok(())
}

pub fn search_with_base(
    query: &str,
    base: &Path,
    context_lines: usize,
) -> Result<Vec<SearchResult>, RecallError> {
    let conversations_dir = base.join("conversations");
    if !conversations_dir.exists() {
        return Err(RecallError::NotInitialized(
            "conversations/ directory not found. Run `recall-echo init` first.".into(),
        ));
    }

    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    let mut files: Vec<_> = fs::read_dir(&conversations_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("conversation-") && name.ends_with(".md")
        })
        .collect();
    files.sort_by_key(|e| e.file_name());

    for entry in &files {
        let file = std::io::BufReader::new(fs::File::open(entry.path())?);

        let lines: Vec<String> = file.lines().map_while(Result::ok).collect();
        let filename = entry.file_name().to_string_lossy().to_string();

        for (i, line) in lines.iter().enumerate() {
            if line.to_lowercase().contains(&query_lower) {
                let start = i.saturating_sub(context_lines);
                for (ci, ctx_line) in lines.iter().enumerate().take(i).skip(start) {
                    results.push(SearchResult {
                        file: filename.clone(),
                        line_num: ci + 1,
                        line: format!("{DIM}{ctx_line}{RESET}"),
                    });
                }

                let highlighted = highlight_match(line, query);
                results.push(SearchResult {
                    file: filename.clone(),
                    line_num: i + 1,
                    line: highlighted,
                });

                let end = (i + context_lines + 1).min(lines.len());
                for (ci, ctx_line) in lines.iter().enumerate().take(end).skip(i + 1) {
                    results.push(SearchResult {
                        file: filename.clone(),
                        line_num: ci + 1,
                        line: format!("{DIM}{ctx_line}{RESET}"),
                    });
                }
            }
        }
    }

    Ok(results)
}

fn highlight_match(line: &str, query: &str) -> String {
    let lower_line = line.to_lowercase();
    let lower_query = query.to_lowercase();

    let mut result = String::new();
    let mut pos = 0;

    while let Some(found) = lower_line[pos..].find(&lower_query) {
        let abs_pos = pos + found;
        result.push_str(&line[pos..abs_pos]);
        result.push_str(YELLOW);
        result.push_str(BOLD);
        result.push_str(&line[abs_pos..abs_pos + query.len()]);
        result.push_str(RESET);
        pos = abs_pos + query.len();
    }
    result.push_str(&line[pos..]);

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_finds_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let conv_dir = base.join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();

        fs::write(
            conv_dir.join("conversation-001.md"),
            "# Conversation 001\n\n### User\n\nHow do I refactor auth?\n\n### Assistant\n\nLet me check the auth module.\n",
        ).unwrap();

        let results = search_with_base("auth", base, 0).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let conv_dir = base.join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();

        fs::write(
            conv_dir.join("conversation-001.md"),
            "JWT tokens are great\n",
        )
        .unwrap();

        let results = search_with_base("jwt", base, 0).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let conv_dir = base.join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();

        fs::write(conv_dir.join("conversation-001.md"), "hello world\n").unwrap();

        let results = search_with_base("nonexistent", base, 0).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_with_context() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let conv_dir = base.join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();

        fs::write(
            conv_dir.join("conversation-001.md"),
            "line one\nline two\nfind this\nline four\nline five\n",
        )
        .unwrap();

        let results = search_with_base("find this", base, 1).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn search_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let result = search_with_base("test", tmp.path(), 0);
        assert!(result.is_err());
    }

    // ── ranked_search ────────────────────────────────────────────────────

    /// Write `contents[i]` as `conversation-{i+1:03}.md` and return the base.
    fn archive_with(contents: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let conv_dir = tmp.path().join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();
        for (i, body) in contents.iter().enumerate() {
            fs::write(conv_dir.join(format!("conversation-{:03}.md", i + 1)), body).unwrap();
        }
        tmp
    }

    fn ranked_files(results: &[RankedFile]) -> Vec<&str> {
        results.iter().map(|r| r.file.as_str()).collect()
    }

    /// The defect this replaces: `ranked_search` required *every* whitespace
    /// token to be present, so a natural-language question carrying a date
    /// prefix and punctuation matched nothing at all. Partial matches must now
    /// come back.
    #[test]
    fn ranked_search_admits_partial_token_matches() {
        let tmp = archive_with(&[
            "### User\n\nI tried a new barbecue sauce today.\n",
            "### User\n\nMy favourite barbecue sauce is Kansas City Masterpiece.\n",
            "### User\n\nWe talked about bicycle maintenance.\n",
        ]);

        let results = ranked_search(
            "[Current date: 2023-05-30T15:43:00Z] What is my favourite barbecue sauce?",
            tmp.path(),
            5,
        )
        .unwrap();

        assert_eq!(
            ranked_files(&results)[0],
            "conversation-002.md",
            "the file covering the most query tokens must rank first"
        );
        assert!(
            ranked_files(&results).contains(&"conversation-001.md"),
            "a file matching only some tokens must still be returned"
        );
    }

    /// Outer punctuation must not be part of the token: `sauce?` has to match
    /// `sauce`.
    #[test]
    fn ranked_search_strips_punctuation_from_tokens() {
        let tmp = archive_with(&["### User\n\nThe barbecue sauce was excellent.\n"]);
        let results = ranked_search("barbecue sauce?", tmp.path(), 5).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].match_count >= 2);
    }

    /// Covering more distinct query tokens beats repeating one of them at the
    /// same raw match count — enough to overcome a recency disadvantage.
    #[test]
    fn ranked_search_prefers_broader_token_coverage() {
        let tmp = archive_with(&[
            "sourdough baguette convection\n",
            "sourdough sourdough sourdough\n",
        ]);

        let results = ranked_search("sourdough baguette convection", tmp.path(), 5).unwrap();
        assert_eq!(results[0].match_count, results[1].match_count);
        assert_eq!(ranked_files(&results)[0], "conversation-001.md");
    }

    /// A token in every archive discriminates nothing, so it must not qualify
    /// files on its own. Needs at least MIN_FILES_FOR_FREQUENCY_FILTER files
    /// for document frequency to be measurable.
    #[test]
    fn ranked_search_ignores_corpus_wide_tokens() {
        let mut bodies = vec!["### User\n\nToday the weather was fine.\n"; 5];
        bodies.push("### User\n\nToday the weather was fine and I bought emerald earrings.\n");
        let tmp = archive_with(&bodies);

        let results =
            ranked_search("Today what about the emerald earrings", tmp.path(), 10).unwrap();

        assert_eq!(
            ranked_files(&results),
            vec!["conversation-006.md"],
            "only `emerald`/`earrings` discriminate; `today`, `the` and `about` are everywhere"
        );
    }

    /// Nothing shared with the corpus still means no results — OR-matching
    /// loosens the gate, it does not remove it.
    #[test]
    fn ranked_search_returns_nothing_without_a_token_match() {
        let tmp = archive_with(&["### User\n\nWe discussed rust lifetimes.\n"]);
        let results = ranked_search("photosynthesis chlorophyll", tmp.path(), 5).unwrap();
        assert!(results.is_empty());
    }

    /// A query made entirely of sub-MIN_TOKEN_LEN words must fall back to those
    /// words rather than degrade to "no discriminative tokens, no results".
    #[test]
    fn ranked_search_falls_back_to_short_tokens() {
        let tmp = archive_with(&["### User\n\nThe ok signal arrived.\n"]);
        let results = ranked_search("ok", tmp.path(), 5).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn ranked_search_honours_max_results() {
        let tmp = archive_with(&[
            "### User\n\nbarbecue one\n",
            "### User\n\nbarbecue two\n",
            "### User\n\nbarbecue three\n",
        ]);
        let results = ranked_search("barbecue", tmp.path(), 2).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn ranked_search_previews_only_prose() {
        let tmp = archive_with(&["# Conversation 001\n\n---\n\n### User\n\nbarbecue sauce here\n"]);
        let results = ranked_search("barbecue", tmp.path(), 5).unwrap();
        assert_eq!(results[0].preview_lines, vec!["barbecue sauce here"]);
    }

    #[test]
    fn ranked_search_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ranked_search("anything", tmp.path(), 5).is_err());
    }
}
