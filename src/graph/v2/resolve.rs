// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Deterministic entity resolution for one session, then one guarded call.
//!
//! Tiers run before any model call: same name and kind, then a stored alias,
//! then the same name at any kind. Only a leftover that is similar to some
//! stored entity is shown to the resolver, and a proposal is kept only when
//! the guard agrees. A new entity is the safe default.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::{json, Value};

use super::super::crud;
use super::super::error::GraphError;
use super::super::llm::{LlmProvider, TokenUsage};
use super::super::llm_json;
use super::super::types::{NewEntity, RejectedMatch, ResolveStats};
use super::super::GraphMemory;
use super::input::ProjectIdentity;
use super::schema::{self, Kind};

pub(crate) use schema::normalise;

/// Resolver system prompt. Verbatim, with no trailing newline.
pub(crate) const PASS2_SYSTEM: &str = r#"You link NEW MENTIONS from the latest coding session to a project memory STORE.
For each NEW MENTION, if it is certainly the same real-world thing as a STORE entity (other spelling, casing, abbreviation or alias), return that entity id as match_id. Otherwise return match_id "".
Never match different things just because they are related: a library is not the project that uses it, a component is not its project, two different libraries are different, a file is not the tool that reads it.
When unsure, return "" (a new entity is created; that is always safe). One entry per NEW MENTION."#;

/// A session asks the resolver at most once. Dry-run estimates add this bound.
pub(crate) const RESOLVER_CALL_BOUND: u32 = 1;

const RESOLVER_TOKEN_BASE: u32 = 256;
const RESOLVER_TOKENS_PER_MENTION: u32 = 64;
const RESOLVER_TOKEN_CAP: u32 = 4096;
/// Mentions that fit in [`RESOLVER_TOKEN_CAP`]. The rest become new entities.
const RESOLVER_MENTION_CAP: usize =
    ((RESOLVER_TOKEN_CAP - RESOLVER_TOKEN_BASE) / RESOLVER_TOKENS_PER_MENTION) as usize;
const SIMILAR_RATIO: f64 = 0.8;
const SUBSET_CHARS: usize = 4;

/// One name the session wants resolved.
#[derive(Debug, Clone)]
pub(crate) struct MentionDraft {
    pub name: String,
    pub kind: Kind,
    /// Example triple for the resolver prompt: subject, predicate, object.
    pub subject: String,
    pub predicate: String,
    pub object: String,
    /// False for a name that appears only as a `replaces` target.
    pub creates: bool,
}

/// What the session resolved, and the counts for its report.
#[derive(Debug, Clone)]
pub(crate) struct Resolution {
    pub bindings: Vec<Binding>,
    pub report: ResolveReport,
    pub entities_created: u32,
    /// Usage from the resolver call. `None` when it was not called or reported nothing.
    pub usage: Option<TokenUsage>,
}

/// A mention bound to the entity it became.
#[derive(Debug, Clone)]
pub(crate) struct Binding {
    pub norm: String,
    pub kind: Kind,
    pub entity: Resolved,
}

/// Entity state after resolution. Slot writes read `attributes` from here.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    pub id: String,
    pub attributes: Value,
}

/// Mentions, tier hits, and what the resolver was allowed to do.
pub(crate) type ResolveReport = ResolveStats;

#[derive(Clone)]
struct OpenMention {
    norm: String,
    name: String,
    kind: Kind,
    subject: String,
    predicate: String,
    object: String,
}

#[derive(Clone)]
struct Row {
    id: String,
    name: String,
    norm: String,
    kind: Kind,
    attributes: Value,
    aliases: Vec<String>,
}

struct Index {
    rows: Vec<Row>,
    by_id: HashMap<String, usize>,
    by_norm: HashMap<String, Vec<usize>>,
    alias_of: HashMap<String, String>,
}

struct PromptGroup {
    mention: OpenMention,
    rows: Vec<usize>,
}

struct PromptPlan {
    groups: Vec<PromptGroup>,
}

enum Tier {
    NameKind,
    Alias,
    NameAny,
}

/// Ratcliff/Obershelp ratio, as `difflib.SequenceMatcher.ratio` with autojunk off.
#[must_use]
pub(crate) fn ratio(left: &str, right: &str) -> f64 {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let matched: usize = matching_blocks(&left, &right)
        .iter()
        .map(|block| block.size)
        .sum();
    let length = left.len() + right.len();
    if length == 0 {
        1.0
    } else {
        (2.0 * matched as f64) / length as f64
    }
}

/// True when two names are the same thing under the guard.
///
/// A character substring of one token is not a token match: `uv` does not
/// share a token with `uvicorn`. The ratio can still hold for a longer prefix.
/// An acronym matches only when the long side has at least two tokens: plain
/// initials (`aws` / `amazon web services`, `ga` / `github actions`) or the
/// same initials with one compound hump (`gha` / `github actions`). A single
/// token is never an acronym, so `pt` is not `postgres`.
#[must_use]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn similar(left: &str, right: &str) -> bool {
    similar_norm(&normalise(left), &normalise(right))
}

/// `left` and `right` are already normalised.
fn similar_norm(left: &str, right: &str) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == right || same_tokens(left, right) {
        return true;
    }
    if token_subset(left, right) || initials_match(left, right) {
        return true;
    }
    // The ratio cannot reach 0.8 when the shorter string is under two thirds
    // of the longer one. Skipping it does not change the result: the other
    // clauses have already been tried. A first-token reject is not used,
    // because `aws` / `amazon web services` and `postgres` / `postgresql`
    // do not share a first token and are still similar.
    ratio_can_reach(left, right) && ratio(left, right) >= SIMILAR_RATIO
}

fn ratio_can_reach(left: &str, right: &str) -> bool {
    let left_len = left.chars().count();
    let right_len = right.chars().count();
    let (short, long) = if left_len <= right_len {
        (left_len, right_len)
    } else {
        (right_len, left_len)
    };
    3 * short >= 2 * long
}

/// Resolve every mention for this session. At most one resolver call.
pub(crate) async fn resolve_session(
    gm: &GraphMemory,
    llm: &dyn LlmProvider,
    project: &ProjectIdentity,
    mentions: &[MentionDraft],
) -> Result<Resolution, GraphError> {
    let mut index = Index::load(gm).await?;
    seed_project(gm, &mut index, project).await?;
    let mut report = ResolveStats {
        mentions: u32::try_from(mentions.len()).unwrap_or(u32::MAX),
        ..ResolveStats::default()
    };
    let project_norm = normalise(&project.name);
    let mut created = 0u32;
    let mut bound: HashMap<(String, Kind), String> = HashMap::new();
    let mut seen = HashSet::new();
    let mut pending = Vec::new();
    for mention in mentions {
        if !mention.creates {
            continue;
        }
        let open = open_mention(mention);
        if open.norm.is_empty() || !seen.insert((open.norm.clone(), open.kind)) {
            continue;
        }
        if take_tier(gm, &mut index, &mut bound, &mut report, &open).await? {
            continue;
        }
        pending.push(open);
    }
    let usage = ask_resolver(gm, llm, &mut index, &mut bound, &mut report, &mut pending).await?;
    pending.sort_by(|left, right| {
        right
            .kind
            .priority()
            .cmp(&left.kind.priority())
            .then_with(|| left.norm.cmp(&right.norm))
    });
    for mention in pending {
        if take_tier(gm, &mut index, &mut bound, &mut report, &mention).await? {
            continue;
        }
        create_entity(gm, &mut index, &mut bound, &mention).await?;
        created += 1;
        if mention.kind == Kind::Project && mention.norm == project_norm {
            seed_project(gm, &mut index, project).await?;
        }
    }
    Ok(Resolution {
        bindings: snapshot(&index, &bound),
        report,
        entities_created: created,
        usage,
    })
}

fn open_mention(mention: &MentionDraft) -> OpenMention {
    OpenMention {
        norm: normalise(&mention.name),
        name: mention.name.trim().to_string(),
        kind: mention.kind,
        subject: mention.subject.clone(),
        predicate: mention.predicate.clone(),
        object: mention.object.clone(),
    }
}

async fn take_tier(
    gm: &GraphMemory,
    index: &mut Index,
    bound: &mut HashMap<(String, Kind), String>,
    report: &mut ResolveReport,
    mention: &OpenMention,
) -> Result<bool, GraphError> {
    let Some((id, tier)) = index.hit(mention) else {
        return Ok(false);
    };
    commit(gm, index, bound, mention, &id).await?;
    match tier {
        Tier::NameKind => report.tier_name_kind += 1,
        Tier::Alias => report.tier_alias += 1,
        Tier::NameAny => report.tier_name_any += 1,
    }
    Ok(true)
}

async fn commit(
    gm: &GraphMemory,
    index: &mut Index,
    bound: &mut HashMap<(String, Kind), String>,
    mention: &OpenMention,
    id: &str,
) -> Result<(), GraphError> {
    maybe_upgrade(gm, index, id, mention.kind).await?;
    if mention.norm != index.norm_of(id) {
        remember_alias(gm, index, &mention.norm, id).await?;
    }
    bound.insert((mention.norm.clone(), mention.kind), id.to_string());
    Ok(())
}

async fn ask_resolver(
    gm: &GraphMemory,
    llm: &dyn LlmProvider,
    index: &mut Index,
    bound: &mut HashMap<(String, Kind), String>,
    report: &mut ResolveReport,
    pending: &mut Vec<OpenMention>,
) -> Result<Option<TokenUsage>, GraphError> {
    let plan = index.prompt_plan(pending);
    if plan.groups.is_empty() {
        return Ok(None);
    }
    let prompted = cap_prompt(plan.groups, report);
    let mut row_idxs = Vec::new();
    let mut seen_rows = HashSet::new();
    for group in &prompted {
        for idx in &group.rows {
            if seen_rows.insert(*idx) {
                row_idxs.push(*idx);
            }
        }
    }
    row_idxs.sort_by(|&left, &right| index.rows[left].id.cmp(&index.rows[right].id));
    if row_idxs.is_empty() {
        return Ok(None);
    }
    let prompted: Vec<OpenMention> = prompted.into_iter().map(|group| group.mention).collect();
    report.resolver_called = 1;
    let message = resolver_message(&index.rows_at(&row_idxs), &prompted);
    let tokens = resolver_tokens(prompted.len());
    let completion = match llm
        .complete_structured(PASS2_SYSTEM, &message, resolver_schema(), tokens)
        .await
    {
        Ok(completion) => completion,
        Err(_) => {
            report
                .notes
                .push("resolver unavailable; leftovers created as new entities".to_string());
            return Ok(None);
        }
    };
    // The call was spent even when the reply is not a resolver object.
    let usage = completion.usage;
    let Some(answers) = parse_answers(&completion.text) else {
        return Ok(usage);
    };
    let candidate_ids: HashSet<String> = row_idxs
        .iter()
        .map(|idx| index.rows[*idx].id.clone())
        .collect();
    apply_answers(
        gm,
        index,
        bound,
        report,
        pending,
        &prompted,
        &candidate_ids,
        &answers,
    )
    .await?;
    Ok(usage)
}

#[allow(clippy::too_many_arguments)]
async fn apply_answers(
    gm: &GraphMemory,
    index: &mut Index,
    bound: &mut HashMap<(String, Kind), String>,
    report: &mut ResolveReport,
    pending: &mut Vec<OpenMention>,
    prompted: &[OpenMention],
    candidate_ids: &HashSet<String>,
    answers: &[(String, String)],
) -> Result<(), GraphError> {
    for mention in prompted {
        let Some(match_id) = answer_for(answers, &mention.name) else {
            continue;
        };
        if match_id.is_empty() {
            continue;
        }
        if let Some(id) = accept_match(index, mention, &match_id, candidate_ids) {
            commit(gm, index, bound, mention, &id).await?;
            report.accepted += 1;
            pending.retain(|item| !(item.norm == mention.norm && item.kind == mention.kind));
        } else {
            report.rejected.push(RejectedMatch {
                mention: mention.name.clone(),
                match_id,
            });
        }
    }
    Ok(())
}

fn accept_match(
    index: &Index,
    mention: &OpenMention,
    match_id: &str,
    candidate_ids: &HashSet<String>,
) -> Option<String> {
    let id = index.find_id(match_id)?;
    if !candidate_ids.contains(&id) {
        return None;
    }
    let row = index.row(&id)?;
    if mention.kind != Kind::Project && row.kind == Kind::Project {
        return None;
    }
    let matched = similar_norm(&mention.norm, &row.norm)
        || row
            .aliases
            .iter()
            .any(|alias| similar_norm(&mention.norm, alias));
    matched.then_some(id)
}

fn sends_to_resolver(kind: Kind) -> bool {
    !matches!(kind, Kind::Issue | Kind::Release)
}

fn cap_prompt(mut groups: Vec<PromptGroup>, report: &mut ResolveStats) -> Vec<PromptGroup> {
    if groups.len() > RESOLVER_MENTION_CAP {
        let held = groups.len() - RESOLVER_MENTION_CAP;
        report.notes.push(format!(
            "{held} mentions were not sent to the resolver and were created as new entities"
        ));
        groups.truncate(RESOLVER_MENTION_CAP);
    }
    groups
}

fn resolver_tokens(mentions: usize) -> u32 {
    let mentions = u32::try_from(mentions).unwrap_or(u32::MAX);
    RESOLVER_TOKEN_BASE
        .saturating_add(mentions.saturating_mul(RESOLVER_TOKENS_PER_MENTION))
        .min(RESOLVER_TOKEN_CAP)
}

async fn create_entity(
    gm: &GraphMemory,
    index: &mut Index,
    bound: &mut HashMap<(String, Kind), String>,
    mention: &OpenMention,
) -> Result<(), GraphError> {
    let attributes = json!({ "v2": { "kind": mention.kind.as_str(), "slots": [] } });
    let entity = gm
        .add_entity(NewEntity {
            name: mention.name.clone(),
            entity_type: mention.kind.entity_type(),
            abstract_text: format!("{} {}", mention.kind.as_str(), mention.name),
            overview: None,
            content: None,
            attributes: Some(attributes.clone()),
            source: None,
        })
        .await?;
    let id = entity.id_string();
    index.push_row(Row {
        id: id.clone(),
        name: mention.name.clone(),
        norm: mention.norm.clone(),
        kind: mention.kind,
        attributes,
        aliases: Vec::new(),
    });
    bound.insert((mention.norm.clone(), mention.kind), id);
    Ok(())
}

async fn maybe_upgrade(
    gm: &GraphMemory,
    index: &mut Index,
    id: &str,
    kind: Kind,
) -> Result<(), GraphError> {
    let Some(current) = index.kind_of(id) else {
        return Ok(());
    };
    if kind.priority() <= current.priority() {
        return Ok(());
    }
    let mut attributes = index.attributes_of(id);
    set_v2_kind(&mut attributes, kind)?;
    let entity_type = kind.entity_type();
    let mutable = entity_type.is_mutable();
    gm.db()
        .query(
            "UPDATE type::record($id) SET entity_type = $entity_type, mutable = $mutable, attributes = $attributes, updated_at = time::now()",
        )
        .bind(("id", id.to_string()))
        .bind(("entity_type", entity_type.to_string()))
        .bind(("mutable", mutable))
        .bind(("attributes", attributes.clone()))
        .await?
        .check()?;
    index.set_kind(id, kind, attributes);
    Ok(())
}

fn set_v2_kind(attributes: &mut Value, kind: Kind) -> Result<(), GraphError> {
    if !attributes.is_object() {
        *attributes = json!({});
    }
    let root = attributes
        .as_object_mut()
        .ok_or_else(|| GraphError::Parse("entity attributes are not an object".into()))?;
    if !root.get("v2").is_some_and(Value::is_object) {
        root.insert("v2".into(), json!({ "kind": "", "slots": [] }));
    }
    let v2 = root
        .get_mut("v2")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| GraphError::Parse("entity attributes.v2 is not an object".into()))?;
    v2.insert("kind".into(), json!(kind.as_str()));
    Ok(())
}

async fn remember_alias(
    gm: &GraphMemory,
    index: &mut Index,
    folded: &str,
    entity_id: &str,
) -> Result<(), GraphError> {
    if folded.is_empty() {
        return Ok(());
    }
    crud::put_alias(gm.db(), folded, entity_id).await?;
    if let Some(stored) = crud::get_alias(gm.db(), folded).await? {
        index.note_alias(folded, &stored);
    }
    Ok(())
}

async fn seed_project(
    gm: &GraphMemory,
    index: &mut Index,
    project: &ProjectIdentity,
) -> Result<(), GraphError> {
    let project_norm = normalise(&project.name);
    if project_norm.is_empty() {
        return Ok(());
    }
    let Some(id) = index.project_id(&project_norm) else {
        return Ok(());
    };
    let mut names = vec![project.name.clone()];
    names.extend(project.aliases.iter().cloned());
    for name in names {
        let folded = normalise(&name);
        if folded.is_empty() || folded == project_norm {
            continue;
        }
        remember_alias(gm, index, &folded, &id).await?;
    }
    Ok(())
}

fn snapshot(index: &Index, bound: &HashMap<(String, Kind), String>) -> Vec<Binding> {
    let mut bindings = Vec::with_capacity(bound.len());
    for ((norm, kind), id) in bound {
        let Some(row) = index.row(id) else {
            continue;
        };
        bindings.push(Binding {
            norm: norm.clone(),
            kind: *kind,
            entity: Resolved {
                id: row.id.clone(),
                attributes: row.attributes.clone(),
            },
        });
    }
    bindings
}

fn resolver_message(candidates: &[&Row], mentions: &[OpenMention]) -> String {
    let mut lines = vec!["STORE ENTITIES (id | name | kind | aliases):".to_string()];
    for row in candidates {
        let aliases = row.aliases.join(", ");
        lines.push(format!(
            "{} | {} | {} | {aliases}",
            row.id,
            row.name,
            row.kind.as_str()
        ));
    }
    lines.push("NEW MENTIONS:".to_string());
    for mention in mentions {
        lines.push(format!(
            "- \"{}\" ({}) e.g. {} {} {}",
            mention.name,
            mention.kind.as_str(),
            mention.subject,
            mention.predicate,
            mention.object
        ));
    }
    lines.join("\n")
}

fn resolver_schema() -> &'static Value {
    static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["entities"],
            "properties": {
                "entities": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["mention", "match_id"],
                        "properties": {
                            "mention": {"type": "string"},
                            "match_id": {"type": "string"}
                        }
                    }
                }
            }
        })
    })
}

fn parse_answers(text: &str) -> Option<Vec<(String, String)>> {
    let object = llm_json::first_json_object(text).ok()?;
    let entities = object.get("entities")?.as_array()?;
    let mut answers = Vec::new();
    for item in entities {
        let mention = item.get("mention")?.as_str()?.to_string();
        let match_id = item.get("match_id")?.as_str()?.trim().to_string();
        answers.push((mention, match_id));
    }
    Some(answers)
}

fn answer_for(answers: &[(String, String)], name: &str) -> Option<String> {
    answers.iter().find_map(|(mention, match_id)| {
        (mention == name || normalise(mention) == normalise(name)).then(|| match_id.clone())
    })
}

fn same_tokens(left: &str, right: &str) -> bool {
    let left = token_set(left);
    let right = token_set(right);
    !left.is_empty() && left == right
}

fn token_subset(left: &str, right: &str) -> bool {
    let left = token_set(left);
    let right = token_set(right);
    let (small, large) = if left.len() <= right.len() {
        (&left, &right)
    } else {
        (&right, &left)
    };
    if small.is_empty() || !small.is_subset(large) {
        return false;
    }
    let shared: usize = small.iter().map(|token| token.chars().count()).sum();
    shared >= SUBSET_CHARS
}

fn token_set(text: &str) -> BTreeSet<&str> {
    text.split_whitespace()
        .filter(|token| !token.is_empty())
        .collect()
}

/// Acronym match. The long side must have at least two tokens. The short
/// side, spaces removed, equals either the plain initials of those tokens
/// or the initials where a compound token may also contribute its hump
/// letter. `github` contributes `g` and `h`, so `gha` matches `github
/// actions` and `ga` matches the plain initials. A one-token name is never
/// an acronym: `pt` does not match `postgres`.
fn initials_match(left: &str, right: &str) -> bool {
    acronym_of(left, right) || acronym_of(right, left)
}

fn acronym_of(short: &str, long: &str) -> bool {
    let tokens: Vec<&str> = long
        .split_whitespace()
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.len() < 2 {
        return false;
    }
    let flat: String = short.chars().filter(|ch| *ch != ' ').collect();
    if flat.is_empty() {
        return false;
    }
    flat == plain_initials(&tokens) || flat == hump_initials(&tokens)
}

fn plain_initials(tokens: &[&str]) -> String {
    tokens
        .iter()
        .filter_map(|token| token.chars().next())
        .collect()
}

/// First letter of each token, plus one compound hump when the token has one.
fn hump_initials(tokens: &[&str]) -> String {
    let mut out = String::new();
    for token in tokens {
        let chars: Vec<char> = token.chars().collect();
        let Some(first) = chars.first() else {
            continue;
        };
        out.push(*first);
        if let Some(hump) = compound_hump(&chars) {
            out.push(hump);
        }
    }
    out
}

fn compound_hump(chars: &[char]) -> Option<char> {
    if chars.len() < 6 {
        return None;
    }
    (3..chars.len()).find_map(|index| {
        let suffix = chars.len() - index;
        (suffix >= 3 && is_consonant(chars[index])).then_some(chars[index])
    })
}

fn is_consonant(ch: char) -> bool {
    ch.is_ascii_alphabetic() && !matches!(ch, 'a' | 'e' | 'i' | 'o' | 'u')
}

struct Span {
    size: usize,
}

fn matching_blocks(left: &[char], right: &[char]) -> Vec<Span> {
    let mut index: HashMap<char, Vec<usize>> = HashMap::new();
    for (position, ch) in right.iter().enumerate() {
        index.entry(*ch).or_default().push(position);
    }
    let mut queue = vec![(0, left.len(), 0, right.len())];
    let mut found = Vec::new();
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (start_a, start_b, size) = longest_match(left, right, &index, alo, ahi, blo, bhi);
        if size == 0 {
            continue;
        }
        found.push((start_a, start_b, size));
        if alo < start_a && blo < start_b {
            queue.push((alo, start_a, blo, start_b));
        }
        if start_a + size < ahi && start_b + size < bhi {
            queue.push((start_a + size, ahi, start_b + size, bhi));
        }
    }
    found.sort_by_key(|block| (block.0, block.1, block.2));
    collapse_blocks(found)
}

fn longest_match(
    left: &[char],
    right: &[char],
    index: &HashMap<char, Vec<usize>>,
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let mut best_i = alo;
    let mut best_j = blo;
    let mut best_size = 0;
    let mut lengths: HashMap<usize, usize> = HashMap::new();
    for (i, ch) in left
        .iter()
        .enumerate()
        .skip(alo)
        .take(ahi.saturating_sub(alo))
    {
        let mut next: HashMap<usize, usize> = HashMap::new();
        let Some(positions) = index.get(ch) else {
            lengths.clear();
            continue;
        };
        for &j in positions {
            if j < blo {
                continue;
            }
            if j >= bhi {
                break;
            }
            let previous = if j == 0 {
                0
            } else {
                lengths.get(&(j - 1)).copied().unwrap_or(0)
            };
            let size = previous + 1;
            next.insert(j, size);
            if size > best_size {
                best_i = i + 1 - size;
                best_j = j + 1 - size;
                best_size = size;
            }
        }
        lengths = next;
    }
    while best_i > alo && best_j > blo && left[best_i - 1] == right[best_j - 1] {
        best_i -= 1;
        best_j -= 1;
        best_size += 1;
    }
    while best_i + best_size < ahi
        && best_j + best_size < bhi
        && left[best_i + best_size] == right[best_j + best_size]
    {
        best_size += 1;
    }
    (best_i, best_j, best_size)
}

fn collapse_blocks(blocks: Vec<(usize, usize, usize)>) -> Vec<Span> {
    let mut collapsed: Vec<(usize, usize, usize)> = Vec::new();
    for (start_a, start_b, size) in blocks {
        if let Some(last) = collapsed.last_mut() {
            if last.0 + last.2 == start_a && last.1 + last.2 == start_b {
                last.2 += size;
                continue;
            }
        }
        collapsed.push((start_a, start_b, size));
    }
    collapsed
        .into_iter()
        .map(|(_, _, size)| Span { size })
        .collect()
}

impl Index {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            by_id: HashMap::new(),
            by_norm: HashMap::new(),
            alias_of: HashMap::new(),
        }
    }

    async fn load(gm: &GraphMemory) -> Result<Self, GraphError> {
        let entities = crud::list_entities(gm.db(), None).await?;
        let mut index = Self::new();
        for entity in entities {
            let Some(kind) = entity
                .attributes
                .as_ref()
                .and_then(|attrs| attrs.get("v2"))
                .and_then(|v2| v2.get("kind"))
                .and_then(Value::as_str)
                .and_then(Kind::parse)
            else {
                continue;
            };
            index.push_row(Row {
                id: entity.id_string(),
                norm: normalise(&entity.name),
                name: entity.name.clone(),
                kind,
                attributes: entity.attributes.clone().unwrap_or_else(|| json!({})),
                aliases: Vec::new(),
            });
        }
        for (name, entity) in crud::list_aliases(gm.db()).await? {
            index.note_alias(&name, &entity);
        }
        Ok(index)
    }

    fn push_row(&mut self, row: Row) {
        let idx = self.rows.len();
        self.by_id.insert(canonical_id(&row.id), idx);
        self.by_norm.entry(row.norm.clone()).or_default().push(idx);
        self.rows.push(row);
    }

    fn hit(&self, mention: &OpenMention) -> Option<(String, Tier)> {
        if let Some(indexes) = self.by_norm.get(&mention.norm) {
            if let Some(row) = indexes
                .iter()
                .map(|idx| &self.rows[*idx])
                .find(|row| row.kind == mention.kind)
            {
                return Some((row.id.clone(), Tier::NameKind));
            }
        }
        if let Some(id) = self.alias_of.get(&mention.norm) {
            return Some((id.clone(), Tier::Alias));
        }
        self.by_norm.get(&mention.norm).and_then(|indexes| {
            indexes
                .iter()
                .map(|idx| &self.rows[*idx])
                .max_by_key(|row| row.kind.priority())
                .map(|row| (row.id.clone(), Tier::NameAny))
        })
    }

    /// One pass: each leftover that has a candidate, with the row indexes it matched.
    fn prompt_plan(&self, pending: &[OpenMention]) -> PromptPlan {
        let mut groups = Vec::new();
        for mention in pending {
            if !sends_to_resolver(mention.kind) {
                continue;
            }
            let rows: Vec<usize> = self
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| self.is_candidate(row, mention))
                .map(|(idx, _)| idx)
                .collect();
            if rows.is_empty() {
                continue;
            }
            groups.push(PromptGroup {
                mention: mention.clone(),
                rows,
            });
        }
        PromptPlan { groups }
    }

    fn rows_at(&self, indexes: &[usize]) -> Vec<&Row> {
        indexes.iter().map(|idx| &self.rows[*idx]).collect()
    }

    fn is_candidate(&self, row: &Row, mention: &OpenMention) -> bool {
        if matches!(row.kind, Kind::Issue | Kind::Release) {
            return false;
        }
        if mention.kind != Kind::Project && row.kind == Kind::Project {
            return false;
        }
        similar_norm(&mention.norm, &row.norm)
            || row
                .aliases
                .iter()
                .any(|alias| similar_norm(&mention.norm, alias))
    }

    fn project_id(&self, norm: &str) -> Option<String> {
        self.by_norm.get(norm).and_then(|indexes| {
            indexes.iter().find_map(|idx| {
                let row = &self.rows[*idx];
                (row.kind == Kind::Project).then(|| row.id.clone())
            })
        })
    }

    fn row_index(&self, id: &str) -> Option<usize> {
        self.by_id.get(&canonical_id(id)).copied()
    }

    fn row(&self, id: &str) -> Option<&Row> {
        self.row_index(id).map(|idx| &self.rows[idx])
    }

    fn find_id(&self, match_id: &str) -> Option<String> {
        self.row(match_id).map(|row| row.id.clone())
    }

    fn norm_of(&self, id: &str) -> String {
        self.row(id).map(|row| row.norm.clone()).unwrap_or_default()
    }

    fn kind_of(&self, id: &str) -> Option<Kind> {
        self.row(id).map(|row| row.kind)
    }

    fn attributes_of(&self, id: &str) -> Value {
        self.row(id)
            .map(|row| row.attributes.clone())
            .unwrap_or_else(|| json!({}))
    }

    fn set_kind(&mut self, id: &str, kind: Kind, attributes: Value) {
        let Some(idx) = self.row_index(id) else {
            return;
        };
        self.rows[idx].kind = kind;
        self.rows[idx].attributes = attributes;
    }

    fn note_alias(&mut self, folded: &str, entity_id: &str) {
        let Some(idx) = self.row_index(entity_id) else {
            return;
        };
        let row = &mut self.rows[idx];
        if !row.aliases.iter().any(|alias| alias == folded) {
            row.aliases.push(folded.to_string());
        }
        let id = row.id.clone();
        self.alias_of.insert(folded.to_string(), id);
    }
}

fn canonical_id(raw: &str) -> String {
    raw.trim()
        .trim_matches('"')
        .chars()
        .filter(|ch| *ch != '⟨' && *ch != '⟩')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::confidence::{Evidence, ExtractionContext};
    use crate::graph::v2::schema::{Fact, Predicate, Role};
    use crate::graph::v2::write::{write_facts, WriteCtx};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn mention(name: &str, kind: Kind) -> MentionDraft {
        MentionDraft {
            name: name.into(),
            kind,
            subject: "app".into(),
            predicate: "uses".into(),
            object: name.into(),
            creates: true,
        }
    }

    fn empty_project() -> ProjectIdentity {
        ProjectIdentity {
            name: String::new(),
            aliases: Vec::new(),
        }
    }

    async fn open_store() -> (tempfile::TempDir, GraphMemory) {
        let tmp = tempfile::tempdir().unwrap();
        let gm = GraphMemory::open_embedded(&tmp.path().join("graph"))
            .await
            .unwrap();
        (tmp, gm)
    }

    struct Reply {
        body: String,
        calls: AtomicUsize,
    }

    impl Reply {
        fn new(body: &str) -> Self {
            Self {
                body: body.to_string(),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for Reply {
        async fn complete(
            &self,
            system_prompt: &str,
            _user_message: &str,
            _max_tokens: u32,
        ) -> Result<String, GraphError> {
            assert_eq!(system_prompt, PASS2_SYSTEM);
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.body.clone())
        }
    }

    struct FixedId {
        id: String,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for FixedId {
        async fn complete(
            &self,
            system_prompt: &str,
            user_message: &str,
            _max_tokens: u32,
        ) -> Result<String, GraphError> {
            assert_eq!(system_prompt, PASS2_SYSTEM);
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mention = first_mention(user_message).expect("mention listed");
            Ok(format!(
                r#"{{"entities":[{{"mention":"{mention}","match_id":"{}"}}]}}"#,
                self.id
            ))
        }
    }

    struct MatchNamed {
        target: String,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for MatchNamed {
        async fn complete(
            &self,
            system_prompt: &str,
            user_message: &str,
            _max_tokens: u32,
        ) -> Result<String, GraphError> {
            assert_eq!(system_prompt, PASS2_SYSTEM);
            self.calls.fetch_add(1, Ordering::SeqCst);
            let id = entity_id(user_message, &self.target).expect("target listed");
            let mention = first_mention(user_message).expect("mention listed");
            Ok(format!(
                r#"{{"entities":[{{"mention":"{mention}","match_id":"{id}"}}]}}"#
            ))
        }
    }

    fn entity_id(message: &str, name: &str) -> Option<String> {
        message.lines().find_map(|line| {
            let (id, rest) = line.split_once(" | ")?;
            let (entity_name, _) = rest.split_once(" | ")?;
            (entity_name == name).then(|| id.to_string())
        })
    }

    fn first_mention(message: &str) -> Option<String> {
        message.lines().find_map(|line| {
            let rest = line.trim().strip_prefix("- \"")?;
            rest.split_once('"').map(|(name, _)| name.to_string())
        })
    }

    async fn names(gm: &GraphMemory) -> Vec<String> {
        let mut found = crud::list_entities(gm.db(), None)
            .await
            .unwrap()
            .into_iter()
            .map(|entity| entity.name)
            .collect::<Vec<_>>();
        found.sort();
        found
    }

    async fn kind_of(gm: &GraphMemory, name: &str) -> Kind {
        let entity = crud::list_entities(gm.db(), None)
            .await
            .unwrap()
            .into_iter()
            .find(|entity| entity.name == name)
            .unwrap();
        entity
            .attributes
            .as_ref()
            .unwrap()
            .pointer("/v2/kind")
            .and_then(Value::as_str)
            .and_then(Kind::parse)
            .unwrap()
    }

    #[test]
    fn similar_truth_table() {
        assert!(!similar("uv", "uvicorn"));
        assert!(!similar("pg", "pgvector"));
        assert!(similar("postgres", "postgresql"));
        assert!(ratio("postgres", "postgresql") >= SIMILAR_RATIO);
        assert!(similar("gha", "github actions"));
        assert!(similar("typer", "typer cli"));
    }

    #[test]
    fn acronyms_need_two_tokens() {
        for (left, right) in [
            ("aws", "amazon web services"),
            ("gcp", "google cloud platform"),
            ("ci", "continuous integration"),
            ("ga", "github actions"),
            ("gha", "github actions"),
        ] {
            assert!(similar(left, right), "{left} / {right}");
            assert!(similar(right, left), "{right} / {left}");
        }
        for (left, right) in [
            ("pt", "postgres"),
            ("kr", "kubernetes"),
            ("uc", "uvicorn"),
            ("ft", "fastapi"),
            ("db", "database"),
            ("uv", "uvicorn"),
            ("pg", "pgvector"),
        ] {
            assert!(!similar(left, right), "{left} / {right}");
            assert!(!similar(right, left), "{right} / {left}");
        }
    }

    #[test]
    fn length_gate_does_not_change_similarity() {
        let words = [
            "uv",
            "uvicorn",
            "pg",
            "pgvector",
            "postgres",
            "postgresql",
            "gha",
            "ga",
            "github actions",
            "typer",
            "typer cli",
            "aws",
            "amazon web services",
            "gcp",
            "google cloud platform",
            "ci",
            "continuous integration",
            "pt",
            "kr",
            "kubernetes",
            "uc",
            "ft",
            "fastapi",
            "db",
            "database",
            "kitten",
            "sitting",
            "redis",
            "nginx",
            "linkshelf",
            "shelf",
            "a",
            "b",
            "",
            "abc",
        ];
        for left in words {
            for right in words {
                assert_eq!(
                    similar(left, right),
                    similar_without_length_gate(left, right),
                    "{left} / {right}"
                );
            }
        }
    }

    fn similar_without_length_gate(left: &str, right: &str) -> bool {
        let left = normalise(left);
        let right = normalise(right);
        if left.is_empty() || right.is_empty() {
            return false;
        }
        if left == right || same_tokens(&left, &right) {
            return true;
        }
        token_subset(&left, &right)
            || initials_match(&left, &right)
            || ratio(&left, &right) >= SIMILAR_RATIO
    }

    #[test]
    fn resolver_budget_scales_and_caps() {
        assert_eq!(resolver_tokens(1), 320);
        assert_eq!(resolver_tokens(RESOLVER_MENTION_CAP), RESOLVER_TOKEN_CAP);
        assert_eq!(
            resolver_tokens(RESOLVER_MENTION_CAP + 10),
            RESOLVER_TOKEN_CAP
        );
        let groups = (0..=RESOLVER_MENTION_CAP)
            .map(|index| PromptGroup {
                mention: OpenMention {
                    norm: format!("n{index}"),
                    name: format!("n{index}"),
                    kind: Kind::Technology,
                    subject: "app".into(),
                    predicate: "uses".into(),
                    object: format!("n{index}"),
                },
                rows: vec![0],
            })
            .collect();
        let mut report = ResolveStats::default();
        let capped = cap_prompt(groups, &mut report);
        assert_eq!(capped.len(), RESOLVER_MENTION_CAP);
        assert_eq!(report.notes.len(), 1);
        assert!(report.notes[0].contains("1 mentions"));
    }

    #[test]
    fn ratcliff_ratio_matches_sequence_matcher() {
        // 2 * matched / (len a + len b), longest blocks, no autojunk.
        assert!((ratio("", "") - 1.0).abs() < 1e-12);
        assert!(ratio("a", "b").abs() < 1e-12);
        assert!((ratio("abc", "abc") - 1.0).abs() < 1e-12);
        assert!((ratio("postgres", "postgresql") - (16.0 / 18.0)).abs() < 1e-12);
        assert!((ratio("uv", "uvicorn") - (4.0 / 9.0)).abs() < 1e-12);
        assert!((ratio("kitten", "sitting") - (8.0 / 13.0)).abs() < 1e-12);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn tier_name_and_kind() {
        let (_tmp, gm) = open_store().await;
        let llm = Reply::new("nope");
        let project = empty_project();
        resolve_session(&gm, &llm, &project, &[mention("Redis", Kind::Technology)])
            .await
            .unwrap();
        let again = resolve_session(&gm, &llm, &project, &[mention("redis", Kind::Technology)])
            .await
            .unwrap();
        assert_eq!(again.report.tier_name_kind, 1);
        assert_eq!(again.entities_created, 0);
        assert_eq!(llm.calls(), 0);
        assert_eq!(names(&gm).await, vec!["Redis".to_string()]);
        assert!(crud::list_aliases(gm.db()).await.unwrap().is_empty());
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn tier_alias_any_kind() {
        let (_tmp, gm) = open_store().await;
        let llm = Reply::new("nope");
        let project = empty_project();
        let first = resolve_session(&gm, &llm, &project, &[mention("Postgres", Kind::Project)])
            .await
            .unwrap();
        let id = first.bindings[0].entity.id.clone();
        crud::put_alias(gm.db(), "postgresql", &id).await.unwrap();
        let again = resolve_session(
            &gm,
            &llm,
            &project,
            &[mention("PostgreSQL", Kind::Technology)],
        )
        .await
        .unwrap();
        assert_eq!(again.report.tier_alias, 1);
        assert_eq!(again.report.resolver_called, 0);
        assert_eq!(again.entities_created, 0);
        assert_eq!(names(&gm).await, vec!["Postgres".to_string()]);
        assert_eq!(kind_of(&gm, "Postgres").await, Kind::Project);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn tier_name_any_kind_upgrades_kind() {
        let (_tmp, gm) = open_store().await;
        let llm = Reply::new("nope");
        let project = empty_project();
        resolve_session(
            &gm,
            &llm,
            &project,
            &[mention("linkshelf", Kind::Component)],
        )
        .await
        .unwrap();
        let upgraded = resolve_session(&gm, &llm, &project, &[mention("linkshelf", Kind::Project)])
            .await
            .unwrap();
        assert_eq!(upgraded.report.tier_name_any, 1);
        assert_eq!(upgraded.entities_created, 0);
        assert_eq!(names(&gm).await, vec!["linkshelf".to_string()]);
        assert_eq!(kind_of(&gm, "linkshelf").await, Kind::Project);
        let entity = crud::list_entities(gm.db(), None).await.unwrap();
        assert_eq!(
            entity[0].entity_type,
            crate::graph::types::EntityType::Project
        );

        let later = resolve_session(
            &gm,
            &llm,
            &project,
            &[mention("linkshelf", Kind::Component)],
        )
        .await
        .unwrap();
        assert_eq!(later.entities_created, 0);
        assert_eq!(kind_of(&gm, "linkshelf").await, Kind::Project);
        assert_eq!(names(&gm).await.len(), 1);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn project_never_absorbs_component() {
        let (_tmp, gm) = open_store().await;
        let llm = Reply::new("nope");
        let project = empty_project();
        resolve_session(&gm, &llm, &project, &[mention("typer", Kind::Project)])
            .await
            .unwrap();
        let resolved = resolve_session(
            &gm,
            &llm,
            &project,
            &[mention("typer cli", Kind::Component)],
        )
        .await
        .unwrap();
        assert_eq!(resolved.report.resolver_called, 0);
        assert_eq!(llm.calls(), 0);
        assert_eq!(resolved.entities_created, 1);
        assert_eq!(kind_of(&gm, "typer").await, Kind::Project);
        assert_eq!(kind_of(&gm, "typer cli").await, Kind::Component);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn resolver_answer_outside_candidates_is_ignored() {
        let (_tmp, gm) = open_store().await;
        let project = empty_project();
        let quiet = Reply::new("nope");
        resolve_session(&gm, &quiet, &project, &[mention("uv", Kind::Technology)])
            .await
            .unwrap();
        resolve_session(
            &gm,
            &quiet,
            &project,
            &[mention("uvicorn worker", Kind::Technology)],
        )
        .await
        .unwrap();
        let uv_id = crud::list_entities(gm.db(), None)
            .await
            .unwrap()
            .into_iter()
            .find(|entity| entity.name == "uv")
            .unwrap()
            .id_string();
        let llm = FixedId {
            id: uv_id.clone(),
            calls: AtomicUsize::new(0),
        };
        let resolved =
            resolve_session(&gm, &llm, &project, &[mention("uvicorn", Kind::Technology)])
                .await
                .unwrap();
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolved.report.accepted, 0);
        assert_eq!(resolved.report.rejected.len(), 1);
        assert_eq!(resolved.report.rejected[0].mention, "uvicorn");
        assert_eq!(resolved.report.rejected[0].match_id, uv_id);
        assert_eq!(resolved.entities_created, 1);
        let found = names(&gm).await;
        assert!(found.iter().any(|name| name == "uv"));
        assert!(found.iter().any(|name| name == "uvicorn"));
        assert_ne!(
            found
                .iter()
                .find(|name| *name == "uv")
                .map(|name| name.as_str()),
            found
                .iter()
                .find(|name| *name == "uvicorn")
                .map(|name| name.as_str())
        );
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn replaces_only_mentions_create_nothing() {
        let (_tmp, gm) = open_store().await;
        let llm = Reply::new("nope");
        let project = empty_project();
        let mut only = mention("PostgreSQL", Kind::Technology);
        only.creates = false;
        let resolved = resolve_session(&gm, &llm, &project, &[only]).await.unwrap();
        assert_eq!(resolved.entities_created, 0);
        assert_eq!(resolved.report.resolver_called, 0);
        assert_eq!(llm.calls(), 0);
        assert!(names(&gm).await.is_empty());
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn no_similar_candidate_skips_the_resolver() {
        let (_tmp, gm) = open_store().await;
        let llm = Reply::new("nope");
        let project = empty_project();
        resolve_session(&gm, &llm, &project, &[mention("Redis", Kind::Technology)])
            .await
            .unwrap();
        let resolved = resolve_session(&gm, &llm, &project, &[mention("nginx", Kind::Technology)])
            .await
            .unwrap();
        assert_eq!(resolved.report.resolver_called, 0);
        assert_eq!(llm.calls(), 0);
        assert_eq!(resolved.entities_created, 1);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn resolver_garbage_still_writes_the_fact() {
        let (_tmp, gm) = open_store().await;
        let project = empty_project();
        let quiet = Reply::new("nope");
        let first = WriteCtx::new(&quiet, &project, "s1", "2026-01-02", &[]);
        write_facts(&gm, &first, &[uses("Postgres")]).await.unwrap();
        let garbage = Reply::new("this is not json");
        let second = WriteCtx::new(&garbage, &project, "s1", "2026-01-02", &[]);
        let stats = write_facts(&gm, &second, &[uses("PostgreSQL")])
            .await
            .unwrap();
        assert_eq!(garbage.calls(), 1);
        assert_eq!(stats.edges, 1);
        assert!(names(&gm).await.iter().any(|name| name == "PostgreSQL"));
        assert!(names(&gm).await.iter().any(|name| name == "Postgres"));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn alias_retry_keeps_the_prior() {
        let (_tmp, gm) = open_store().await;
        let project = empty_project();
        let quiet = Reply::new("nope");
        let first = WriteCtx::new(&quiet, &project, "s1", "2026-01-02", &[]);
        write_facts(&gm, &first, &[uses("Postgres")]).await.unwrap();
        let matcher = MatchNamed {
            target: "Postgres".into(),
            calls: AtomicUsize::new(0),
        };
        let postgres_fact = uses("PostgreSQL");
        let second = WriteCtx::new(&matcher, &project, "s1", "2026-01-02", &[]);
        write_facts(&gm, &second, std::slice::from_ref(&postgres_fact))
            .await
            .unwrap();
        let prior = Evidence::from_prior(ExtractionContext::Explicit.prior());
        let before = edge_belief(&gm).await;
        assert!((before.0 - prior.alpha()).abs() < 1e-9);
        assert!((before.1 - prior.beta()).abs() < 1e-9);
        assert_eq!(crud::list_aliases(gm.db()).await.unwrap().len(), 1);

        let third = WriteCtx::new(&matcher, &project, "s1", "2026-01-02", &[]);
        let retry = write_facts(&gm, &third, &[postgres_fact]).await.unwrap();
        assert_eq!(retry.already_seen, 1);
        assert_eq!(retry.reinforced, 0);
        assert_eq!(matcher.calls.load(Ordering::SeqCst), 1);
        let after = edge_belief(&gm).await;
        assert_eq!(after, before);
        assert_eq!(crud::list_aliases(gm.db()).await.unwrap().len(), 1);
        assert!(!names(&gm).await.iter().any(|name| name == "PostgreSQL"));
    }

    fn uses(object: &str) -> Fact {
        Fact {
            subject_name: "app".into(),
            subject_kind: Kind::Project,
            predicate: Predicate::Uses,
            role: Some(Role::Database),
            key: String::new(),
            object_name: object.into(),
            object_kind: Some(Kind::Technology),
            status: "decided".into(),
            turn: 1,
            quote: "app uses the database".into(),
            confidence: 0.9,
            provenance: crate::graph::Provenance::User,
            replaces: String::new(),
        }
    }

    #[cfg(feature = "embedded")]
    async fn edge_belief(gm: &GraphMemory) -> (f64, f64) {
        let mut response = gm
            .db()
            .query("SELECT alpha, beta FROM relates_to")
            .await
            .unwrap();
        let rows: Vec<Value> = crate::graph::deserialize_take(&mut response, 0).unwrap();
        let edge = rows.first().expect("edge");
        (
            edge["alpha"].as_f64().unwrap(),
            edge["beta"].as_f64().unwrap(),
        )
    }

    struct Down;

    #[async_trait::async_trait]
    impl LlmProvider for Down {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<String, GraphError> {
            Err(GraphError::Llm("provider down".into()))
        }
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn resolver_provider_error_still_writes_the_fact() {
        let (_tmp, gm) = open_store().await;
        let project = empty_project();
        let quiet = Reply::new("nope");
        resolve_session(
            &gm,
            &quiet,
            &project,
            &[mention("uvicorn worker", Kind::Technology)],
        )
        .await
        .unwrap();
        let ctx = WriteCtx::new(&Down, &project, "s1", "2026-01-02", &[]);
        let stats = write_facts(&gm, &ctx, &[uses("uvicorn")]).await.unwrap();
        assert_eq!(stats.edges, 1);
        assert_eq!(stats.resolve.resolver_called, 1);
        assert!(stats
            .resolve
            .notes
            .iter()
            .any(|note| note.contains("unavailable")));
        assert!(names(&gm).await.iter().any(|name| name == "uvicorn"));
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn rejected_proposal_is_on_the_write_report() {
        let (_tmp, gm) = open_store().await;
        let project = empty_project();
        let quiet = Reply::new("nope");
        resolve_session(&gm, &quiet, &project, &[mention("uv", Kind::Technology)])
            .await
            .unwrap();
        resolve_session(
            &gm,
            &quiet,
            &project,
            &[mention("uvicorn worker", Kind::Technology)],
        )
        .await
        .unwrap();
        let uv_id = crud::list_entities(gm.db(), None)
            .await
            .unwrap()
            .into_iter()
            .find(|entity| entity.name == "uv")
            .unwrap()
            .id_string();
        let llm = FixedId {
            id: uv_id.clone(),
            calls: AtomicUsize::new(0),
        };
        let ctx = WriteCtx::new(&llm, &project, "s1", "2026-01-02", &[]);
        let stats = write_facts(&gm, &ctx, &[uses("uvicorn")]).await.unwrap();
        assert_eq!(stats.edges, 1);
        assert_eq!(stats.resolve.rejected.len(), 1);
        assert_eq!(stats.resolve.rejected[0].mention, "uvicorn");
        assert_eq!(stats.resolve.rejected[0].match_id, uv_id);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn alias_resolves_after_reopening_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("graph");
        {
            let gm = GraphMemory::open_embedded(&path).await.unwrap();
            let llm = Reply::new("nope");
            let project = empty_project();
            let first = resolve_session(&gm, &llm, &project, &[mention("Postgres", Kind::Project)])
                .await
                .unwrap();
            let id = first.bindings[0].entity.id.clone();
            crud::put_alias(gm.db(), "postgresql", &id).await.unwrap();
        }
        let gm = GraphMemory::open_embedded(&path).await.unwrap();
        let llm = Reply::new("nope");
        let project = empty_project();
        let again = resolve_session(
            &gm,
            &llm,
            &project,
            &[mention("PostgreSQL", Kind::Technology)],
        )
        .await
        .unwrap();
        assert_eq!(again.report.tier_alias, 1);
        assert_eq!(again.report.resolver_called, 0);
        assert_eq!(again.entities_created, 0);
        assert_eq!(llm.calls(), 0);
        assert_eq!(names(&gm).await, vec!["Postgres".to_string()]);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn project_identity_aliases_are_seeded_once() {
        let (_tmp, gm) = open_store().await;
        let llm = Reply::new("nope");
        let project = ProjectIdentity {
            name: "linkshelf".into(),
            aliases: vec!["shelf".into()],
        };
        resolve_session(&gm, &llm, &project, &[mention("linkshelf", Kind::Project)])
            .await
            .unwrap();
        let again = resolve_session(&gm, &llm, &project, &[mention("shelf", Kind::Component)])
            .await
            .unwrap();
        assert_eq!(again.report.tier_alias, 1);
        assert_eq!(again.entities_created, 0);
        assert_eq!(llm.calls(), 0);
        assert_eq!(names(&gm).await, vec!["linkshelf".to_string()]);
        assert_eq!(crud::list_aliases(gm.db()).await.unwrap().len(), 1);
    }

    #[cfg(feature = "embedded")]
    #[tokio::test]
    async fn resolver_match_upgrades_kind() {
        let (_tmp, gm) = open_store().await;
        let project = empty_project();
        let quiet = Reply::new("nope");
        resolve_session(
            &gm,
            &quiet,
            &project,
            &[mention("Postgres", Kind::Technology)],
        )
        .await
        .unwrap();
        let matcher = MatchNamed {
            target: "Postgres".into(),
            calls: AtomicUsize::new(0),
        };
        let upgraded = resolve_session(
            &gm,
            &matcher,
            &project,
            &[mention("PostgreSQL", Kind::Project)],
        )
        .await
        .unwrap();
        assert_eq!(upgraded.report.accepted, 1);
        assert_eq!(upgraded.entities_created, 0);
        assert_eq!(kind_of(&gm, "Postgres").await, Kind::Project);
        let entity = crud::list_entities(gm.db(), None).await.unwrap();
        assert_eq!(entity.len(), 1);
        assert_eq!(
            entity[0].entity_type,
            crate::graph::types::EntityType::Project
        );
    }
}
