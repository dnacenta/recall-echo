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
use super::super::llm::LlmProvider;
use super::super::llm_json;
use super::super::types::NewEntity;
use super::super::GraphMemory;
use super::input::ProjectIdentity;
use super::schema::{self, Kind};

pub(crate) use schema::normalise;

/// Resolver system prompt. Verbatim, with no trailing newline.
pub(crate) const PASS2_SYSTEM: &str = r#"You link NEW MENTIONS from the latest coding session to a project memory STORE.
For each NEW MENTION, if it is certainly the same real-world thing as a STORE entity (other spelling, casing, abbreviation or alias), return that entity id as match_id. Otherwise return match_id "".
Never match different things just because they are related: a library is not the project that uses it, a component is not its project, two different libraries are different, a file is not the tool that reads it.
When unsure, return "" (a new entity is created; that is always safe). One entry per NEW MENTION."#;

const RESOLVER_MAX_TOKENS: u32 = 1024;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolveReport {
    pub mentions: u32,
    pub tier_name_kind: u32,
    pub tier_alias: u32,
    pub tier_name_any: u32,
    pub resolver_called: u32,
    pub accepted: u32,
    pub rejected: Vec<RejectedProposal>,
}

/// A model proposal the guard refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RejectedProposal {
    pub mention: String,
    pub match_id: String,
}

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
    alias_of: HashMap<String, String>,
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

/// True when two normalised names are the same thing under the guard.
///
/// A character substring of one token is not a token match: `uv` does not
/// share a token with `uvicorn`. The ratio can still hold for a longer prefix.
#[must_use]
pub(crate) fn similar(left: &str, right: &str) -> bool {
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

/// Resolve every mention for this session. At most one resolver call.
pub(crate) async fn resolve_session(
    gm: &GraphMemory,
    llm: &dyn LlmProvider,
    project: &ProjectIdentity,
    mentions: &[MentionDraft],
) -> Result<Resolution, GraphError> {
    let mut index = Index::load(gm).await?;
    seed_project(gm, &mut index, project).await?;
    let mut report = ResolveReport {
        mentions: u32::try_from(mentions.len()).unwrap_or(u32::MAX),
        tier_name_kind: 0,
        tier_alias: 0,
        tier_name_any: 0,
        resolver_called: 0,
        accepted: 0,
        rejected: Vec::new(),
    };
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
    ask_resolver(gm, llm, &mut index, &mut bound, &mut report, &mut pending).await?;
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
        create_entity(gm, &mut index, &mut bound, project, &mention).await?;
        created += 1;
    }
    Ok(Resolution {
        bindings: snapshot(&index, &bound),
        report,
        entities_created: created,
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
) -> Result<(), GraphError> {
    let prompted: Vec<OpenMention> = pending
        .iter()
        .filter(|mention| sends_to_resolver(mention.kind) && index.has_candidate(mention))
        .cloned()
        .collect();
    if prompted.is_empty() {
        return Ok(());
    }
    let candidates = index.candidate_rows(&prompted);
    if candidates.is_empty() {
        return Ok(());
    }
    report.resolver_called = 1;
    let message = resolver_message(&candidates, &prompted);
    let completion = llm
        .complete_structured(
            PASS2_SYSTEM,
            &message,
            resolver_schema(),
            RESOLVER_MAX_TOKENS,
        )
        .await?;
    let Some(answers) = parse_answers(&completion.text) else {
        return Ok(());
    };
    let candidate_ids: HashSet<String> = candidates.iter().map(|row| row.id.clone()).collect();
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
    .await
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
            report.rejected.push(RejectedProposal {
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
    let names = row.names();
    names
        .iter()
        .any(|name| similar(&mention.norm, name))
        .then_some(id)
}

fn sends_to_resolver(kind: Kind) -> bool {
    !matches!(kind, Kind::Issue | Kind::Release)
}

async fn create_entity(
    gm: &GraphMemory,
    index: &mut Index,
    bound: &mut HashMap<(String, Kind), String>,
    project: &ProjectIdentity,
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
    index.rows.push(Row {
        id: id.clone(),
        name: mention.name.clone(),
        norm: mention.norm.clone(),
        kind: mention.kind,
        attributes,
        aliases: Vec::new(),
    });
    bound.insert((mention.norm.clone(), mention.kind), id);
    seed_project(gm, index, project).await
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

fn initials_match(left: &str, right: &str) -> bool {
    let left_flat: String = left.chars().filter(|ch| *ch != ' ').collect();
    let right_flat: String = right.chars().filter(|ch| *ch != ' ').collect();
    let left_initials = initials_of(left);
    let right_initials = initials_of(right);
    (!left_initials.is_empty() && left_initials == right_flat)
        || (!right_initials.is_empty() && right_initials == left_flat)
}

/// First letter of each word. A closed compound also contributes the first
/// letter of its trailing piece, so "github actions" yields "gha".
fn initials_of(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        let chars: Vec<char> = word.chars().collect();
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
    async fn load(gm: &GraphMemory) -> Result<Self, GraphError> {
        let entities = crud::list_entities(gm.db(), None).await?;
        let mut rows = Vec::new();
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
            rows.push(Row {
                id: entity.id_string(),
                norm: normalise(&entity.name),
                name: entity.name.clone(),
                kind,
                attributes: entity.attributes.clone().unwrap_or_else(|| json!({})),
                aliases: Vec::new(),
            });
        }
        let mut alias_of = HashMap::new();
        for (name, entity) in crud::list_aliases(gm.db()).await? {
            if let Some(row) = rows.iter_mut().find(|row| ids_match(&row.id, &entity)) {
                row.aliases.push(name.clone());
                alias_of.insert(name, row.id.clone());
            }
        }
        Ok(Self { rows, alias_of })
    }

    fn hit(&self, mention: &OpenMention) -> Option<(String, Tier)> {
        if let Some(row) = self
            .rows
            .iter()
            .find(|row| row.norm == mention.norm && row.kind == mention.kind)
        {
            return Some((row.id.clone(), Tier::NameKind));
        }
        if let Some(id) = self.alias_of.get(&mention.norm) {
            return Some((id.clone(), Tier::Alias));
        }
        self.rows
            .iter()
            .filter(|row| row.norm == mention.norm)
            .max_by_key(|row| row.kind.priority())
            .map(|row| (row.id.clone(), Tier::NameAny))
    }

    fn has_candidate(&self, mention: &OpenMention) -> bool {
        self.rows.iter().any(|row| self.is_candidate(row, mention))
    }

    fn candidate_rows(&self, mentions: &[OpenMention]) -> Vec<&Row> {
        let mut rows: Vec<&Row> = self
            .rows
            .iter()
            .filter(|row| {
                mentions
                    .iter()
                    .any(|mention| self.is_candidate(row, mention))
            })
            .collect();
        rows.sort_by(|left, right| left.id.cmp(&right.id));
        rows.dedup_by(|left, right| left.id == right.id);
        rows
    }

    fn is_candidate(&self, row: &Row, mention: &OpenMention) -> bool {
        if matches!(row.kind, Kind::Issue | Kind::Release) {
            return false;
        }
        if mention.kind != Kind::Project && row.kind == Kind::Project {
            return false;
        }
        row.names().iter().any(|name| similar(&mention.norm, name))
    }

    fn project_id(&self, norm: &str) -> Option<String> {
        self.rows
            .iter()
            .find(|row| row.kind == Kind::Project && row.norm == norm)
            .map(|row| row.id.clone())
    }

    fn row(&self, id: &str) -> Option<&Row> {
        self.rows.iter().find(|row| ids_match(&row.id, id))
    }

    fn find_id(&self, match_id: &str) -> Option<String> {
        self.rows
            .iter()
            .find(|row| ids_match(&row.id, match_id))
            .map(|row| row.id.clone())
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
        if let Some(row) = self.rows.iter_mut().find(|row| ids_match(&row.id, id)) {
            row.kind = kind;
            row.attributes = attributes;
        }
    }

    fn note_alias(&mut self, folded: &str, entity_id: &str) {
        let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| ids_match(&row.id, entity_id))
        else {
            return;
        };
        if !row.aliases.iter().any(|alias| alias == folded) {
            row.aliases.push(folded.to_string());
        }
        let id = row.id.clone();
        self.alias_of.insert(folded.to_string(), id);
    }
}

impl Row {
    fn names(&self) -> Vec<String> {
        let mut names = vec![self.norm.clone()];
        names.extend(self.aliases.iter().cloned());
        names
    }
}

fn ids_match(stored: &str, other: &str) -> bool {
    canonical_id(stored) == canonical_id(other)
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
    use crate::graph::v2::input::TurnRole;
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
            id: uv_id,
            calls: AtomicUsize::new(0),
        };
        let resolved =
            resolve_session(&gm, &llm, &project, &[mention("uvicorn", Kind::Technology)])
                .await
                .unwrap();
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolved.report.accepted, 0);
        assert_eq!(resolved.report.rejected.len(), 1);
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
        let first = WriteCtx {
            llm: &quiet,
            project: &project,
            session_id: "s1",
            date: "2026-01-02",
        };
        write_facts(&gm, &first, &[uses("Postgres")]).await.unwrap();
        let garbage = Reply::new("this is not json");
        let second = WriteCtx {
            llm: &garbage,
            project: &project,
            session_id: "s1",
            date: "2026-01-02",
        };
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
        let first = WriteCtx {
            llm: &quiet,
            project: &project,
            session_id: "s1",
            date: "2026-01-02",
        };
        write_facts(&gm, &first, &[uses("Postgres")]).await.unwrap();
        let matcher = MatchNamed {
            target: "Postgres".into(),
            calls: AtomicUsize::new(0),
        };
        let postgres_fact = uses("PostgreSQL");
        let second = WriteCtx {
            llm: &matcher,
            project: &project,
            session_id: "s1",
            date: "2026-01-02",
        };
        write_facts(&gm, &second, std::slice::from_ref(&postgres_fact))
            .await
            .unwrap();
        let prior = Evidence::from_prior(ExtractionContext::Explicit.prior());
        let before = edge_belief(&gm).await;
        assert!((before.0 - prior.alpha()).abs() < 1e-9);
        assert!((before.1 - prior.beta()).abs() < 1e-9);
        assert_eq!(crud::list_aliases(gm.db()).await.unwrap().len(), 1);

        let third = WriteCtx {
            llm: &matcher,
            project: &project,
            session_id: "s1",
            date: "2026-01-02",
        };
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
            provenance: TurnRole::User,
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
}
