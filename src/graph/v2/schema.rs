// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Closed schema for the second extraction pipeline.
//!
//! Kinds, predicates, roles and statuses are fixed. A candidate is remapped
//! or rejected against that table before anything is written, so a literal
//! never becomes an entity and a free-form relation never becomes an edge.

use std::sync::OnceLock;

use regex::Regex;

use super::super::confidence::Provenance;
use super::super::types::EntityType;
use super::input::{ProjectIdentity, TurnRole};

/// Why a candidate was not written. Stable names, counted on the report.
pub(crate) const DROP_REJECTED: &str = "rejected";
pub(crate) const DROP_GENERIC: &str = "generic_object";
pub(crate) const DROP_CONFIDENCE: &str = "low_confidence";
pub(crate) const DROP_QUOTE: &str = "quote";
pub(crate) const DROP_VALUE_SUPPORT: &str = "value_support";
/// A cause, fix, release contents field, or named `affects` edge failed value
/// support. The candidate stayed. Left out of `kept + whole drops`.
pub(crate) const DROP_VALUE_SUPPORT_PARTIAL: &str = "value_support_partial";
pub(crate) const DROP_KEY_BINDING: &str = "key_binding";
pub(crate) const DROP_VERSION: &str = "bad_version";
pub(crate) const DROP_PARSE: &str = "parse";
pub(crate) const DROP_TOO_LONG: &str = "too_long";
pub(crate) const DROP_OVERFLOW: &str = "overflow";
pub(crate) const DROP_UNRESOLVED: &str = "unresolved";

/// Confidence below this is not a durable fact.
const MIN_CONFIDENCE: f64 = 0.6;
/// Strings from the model longer than this are not stored. The object cap
/// uses the same limit and is counted as [`DROP_GENERIC`].
const MAX_OBJECT_CHARS: usize = 300;
/// Facts, bugs or releases past this in one section are not stored.
pub(crate) const MAX_ITEMS: usize = 200;

/// The six kinds v2 stores. Each one maps onto an existing [`EntityType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Project,
    Component,
    Technology,
    Person,
    Issue,
    Release,
}

impl Kind {
    /// Higher wins when the same normalised name is seen as two kinds.
    #[must_use]
    pub(crate) fn priority(self) -> u8 {
        match self {
            Self::Project => 6,
            Self::Person => 5,
            Self::Release => 4,
            Self::Issue => 3,
            Self::Component => 2,
            Self::Technology => 1,
        }
    }

    /// Existing entity type this kind is stored as. No new variants.
    #[must_use]
    pub(crate) fn entity_type(self) -> EntityType {
        match self {
            Self::Project => EntityType::Project,
            Self::Component => EntityType::Concept,
            Self::Technology => EntityType::Tool,
            Self::Person => EntityType::Person,
            Self::Issue => EntityType::Case,
            Self::Release => EntityType::Event,
        }
    }

    #[must_use]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Component => "component",
            Self::Technology => "technology",
            Self::Person => "person",
            Self::Issue => "issue",
            Self::Release => "release",
        }
    }

    #[must_use]
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "project" => Self::Project,
            "component" => Self::Component,
            "technology" => Self::Technology,
            "person" => Self::Person,
            "issue" => Self::Issue,
            "release" => Self::Release,
            _ => return None,
        })
    }
}

/// Closed predicates. The first five are edges; the rest are slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Predicate {
    Uses,
    PartOf,
    DependsOn,
    Affects,
    WorksOn,
    Setting,
    Rule,
    Prefers,
    IssueSymptom,
    IssueStatus,
    IssueCause,
    IssueFix,
    ReleaseStatus,
    ReleaseContents,
}

impl Predicate {
    #[must_use]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Uses => "uses",
            Self::PartOf => "part_of",
            Self::DependsOn => "depends_on",
            Self::Affects => "affects",
            Self::WorksOn => "works_on",
            Self::Setting => "setting",
            Self::Rule => "rule",
            Self::Prefers => "prefers",
            Self::IssueSymptom => "issue_symptom",
            Self::IssueStatus => "issue_status",
            Self::IssueCause => "issue_cause",
            Self::IssueFix => "issue_fix",
            Self::ReleaseStatus => "release_status",
            Self::ReleaseContents => "release_contents",
        }
    }

    #[must_use]
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "uses" => Self::Uses,
            "part_of" => Self::PartOf,
            "depends_on" => Self::DependsOn,
            "affects" => Self::Affects,
            "works_on" => Self::WorksOn,
            "setting" => Self::Setting,
            "rule" => Self::Rule,
            "prefers" => Self::Prefers,
            "issue_symptom" => Self::IssueSymptom,
            "issue_status" => Self::IssueStatus,
            "issue_cause" => Self::IssueCause,
            "issue_fix" => Self::IssueFix,
            "release_status" => Self::ReleaseStatus,
            "release_contents" => Self::ReleaseContents,
            _ => return None,
        })
    }

    #[must_use]
    pub(crate) fn is_edge(self) -> bool {
        matches!(
            self,
            Self::Uses | Self::PartOf | Self::DependsOn | Self::Affects | Self::WorksOn
        )
    }
}

/// What a `uses` edge is for. `none` is what the model says when it does not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Language,
    Runtime,
    PackageManager,
    Framework,
    CliFramework,
    WebFramework,
    Database,
    TestFramework,
    Linter,
    Formatter,
    TypeChecker,
    BuildTool,
    Ci,
    Hosting,
    Library,
    Other,
    None,
}

impl Role {
    #[must_use]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Language => "language",
            Self::Runtime => "runtime",
            Self::PackageManager => "package_manager",
            Self::Framework => "framework",
            Self::CliFramework => "cli_framework",
            Self::WebFramework => "web_framework",
            Self::Database => "database",
            Self::TestFramework => "test_framework",
            Self::Linter => "linter",
            Self::Formatter => "formatter",
            Self::TypeChecker => "type_checker",
            Self::BuildTool => "build_tool",
            Self::Ci => "ci",
            Self::Hosting => "hosting",
            Self::Library => "library",
            Self::Other => "other",
            Self::None => "none",
        }
    }

    #[must_use]
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "language" => Self::Language,
            "runtime" => Self::Runtime,
            "package_manager" => Self::PackageManager,
            "framework" => Self::Framework,
            "cli_framework" => Self::CliFramework,
            "web_framework" => Self::WebFramework,
            "database" => Self::Database,
            "test_framework" => Self::TestFramework,
            "linter" => Self::Linter,
            "formatter" => Self::Formatter,
            "type_checker" => Self::TypeChecker,
            "build_tool" => Self::BuildTool,
            "ci" => Self::Ci,
            "hosting" => Self::Hosting,
            "library" => Self::Library,
            "other" => Self::Other,
            "none" => Self::None,
            _ => return None,
        })
    }
}

/// One candidate after the remap table, before the generic filters.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fact {
    pub subject_name: String,
    pub subject_kind: Kind,
    pub predicate: Predicate,
    pub role: Option<Role>,
    pub key: String,
    pub object_name: String,
    pub object_kind: Option<Kind>,
    pub status: String,
    pub turn: usize,
    pub quote: String,
    pub confidence: f64,
    /// Who the verified quote is credited to. A fact that is not quotable
    /// from one turn is [`Provenance::SelfGenerated`].
    pub provenance: Provenance,
    /// The value this fact replaces, when the model named one. Empty when
    /// it does not. A name that appears only here is not an entity.
    pub replaces: String,
}

/// A model mention, before the remap table runs.
#[derive(Debug, Clone)]
pub(crate) struct Mention {
    pub subject: String,
    pub subject_kind: Kind,
    pub predicate: Predicate,
    pub role: Role,
    pub key: String,
    pub object: String,
    pub object_kind: Option<Kind>,
    pub status: String,
    pub turn: usize,
    pub quote: String,
    pub confidence: f64,
    pub provenance: TurnRole,
}

/// Apply the remap / reject table in the specified order.
///
/// Row 3 sees the object kind the model sent. A literal object type is
/// promoted to technology only at row 5, after that reject has run, so
/// `technology uses "1.75"` is not a technology-to-technology edge.
///
/// `Err(DROP_REJECTED)` is a counted drop. A `Fact` has already had its key
/// canonicalised when it is a setting, and its role cleared when it is not a
/// `uses` edge.
pub(crate) fn remap(mention: Mention) -> Result<Fact, &'static str> {
    let mut mention = mention;
    // 1. uses + component object => depends_on
    if mention.predicate == Predicate::Uses && mention.object_kind == Some(Kind::Component) {
        mention.predicate = Predicate::DependsOn;
    }
    // 2. depends_on + technology object => uses, role library when none
    if mention.predicate == Predicate::DependsOn && mention.object_kind == Some(Kind::Technology) {
        mention.predicate = Predicate::Uses;
        if mention.role == Role::None {
            mention.role = Role::Library;
        }
    }
    // 3. uses technology → technology => reject, on the original object kind
    if mention.predicate == Predicate::Uses
        && mention.subject_kind == Kind::Technology
        && mention.object_kind == Some(Kind::Technology)
    {
        return Err(DROP_REJECTED);
    }
    // 4. rule / setting / prefers objects are literals
    if matches!(
        mention.predicate,
        Predicate::Rule | Predicate::Setting | Predicate::Prefers
    ) {
        mention.object_kind = None;
    }
    // 5. uses + literal object type => technology
    if mention.predicate == Predicate::Uses && mention.object_kind.is_none() {
        mention.object_kind = Some(Kind::Technology);
    }
    // 6. part_of + project subject => reject
    if mention.predicate == Predicate::PartOf && mention.subject_kind == Kind::Project {
        return Err(DROP_REJECTED);
    }
    // 7. part_of + technology object => reject
    if mention.predicate == Predicate::PartOf && mention.object_kind == Some(Kind::Technology) {
        return Err(DROP_REJECTED);
    }
    // 8. subject kind not allowed => reject
    if !subject_allowed(mention.predicate, mention.subject_kind) {
        return Err(DROP_REJECTED);
    }
    // 9. uses role none => other. 10. non-setting clears key, non-uses clears role.
    adjust_role_and_key(&mut mention);
    // 11. a uses whose object looks literal becomes a setting
    literal_uses_becomes_setting(&mut mention);
    Ok(finish(mention))
}

/// `uses`, `setting` and `rule` also accept project, component and technology.
fn subject_allowed(predicate: Predicate, kind: Kind) -> bool {
    let listed = match predicate {
        Predicate::Uses => matches!(kind, Kind::Project | Kind::Component),
        Predicate::PartOf => matches!(kind, Kind::Component | Kind::Release),
        Predicate::DependsOn => matches!(kind, Kind::Component),
        Predicate::Affects => matches!(kind, Kind::Issue),
        Predicate::WorksOn => matches!(kind, Kind::Person),
        Predicate::Setting => matches!(kind, Kind::Project | Kind::Component | Kind::Technology),
        Predicate::Rule => matches!(kind, Kind::Project | Kind::Component),
        Predicate::Prefers => matches!(kind, Kind::Person),
        Predicate::IssueSymptom
        | Predicate::IssueStatus
        | Predicate::IssueCause
        | Predicate::IssueFix => matches!(kind, Kind::Issue),
        Predicate::ReleaseStatus | Predicate::ReleaseContents => matches!(kind, Kind::Release),
    };
    if listed {
        return true;
    }
    matches!(
        predicate,
        Predicate::Uses | Predicate::Setting | Predicate::Rule
    ) && matches!(kind, Kind::Project | Kind::Component | Kind::Technology)
}

fn adjust_role_and_key(mention: &mut Mention) {
    if mention.predicate == Predicate::Uses && mention.role == Role::None {
        mention.role = Role::Other;
    }
    if mention.predicate != Predicate::Setting {
        mention.key.clear();
    }
    if mention.predicate != Predicate::Uses {
        mention.role = Role::None;
    }
}

/// A `uses` whose object is a port, version or path is a setting.
fn literal_uses_becomes_setting(mention: &mut Mention) {
    if mention.predicate != Predicate::Uses || !looks_literal(&mention.object) {
        return;
    }
    // Role was captured before non-uses roles were cleared: this mention is
    // still a `uses` here, so `role` is the one the model gave.
    let key = match mention.role {
        Role::None | Role::Other | Role::Library => "value",
        other => other.as_str(),
    };
    mention.predicate = Predicate::Setting;
    mention.key = key.to_string();
    mention.object_kind = None;
    mention.role = Role::None;
}

fn finish(mention: Mention) -> Fact {
    let key = if mention.predicate == Predicate::Setting {
        canon_key(&mention.key)
    } else {
        String::new()
    };
    let role = (mention.predicate == Predicate::Uses).then_some(mention.role);
    Fact {
        subject_name: mention.subject,
        subject_kind: mention.subject_kind,
        predicate: mention.predicate,
        role,
        key,
        object_name: mention.object,
        object_kind: mention.object_kind,
        status: mention.status,
        turn: mention.turn,
        quote: mention.quote,
        confidence: mention.confidence,
        provenance: match mention.provenance {
            TurnRole::User => Provenance::User,
            TurnRole::Assistant => Provenance::Assistant,
        },
        replaces: String::new(),
    }
}

/// True when `object` is a number, version, path or similar literal.
#[must_use]
pub(crate) fn looks_literal(object: &str) -> bool {
    literal_shape().is_some_and(|re| re.is_match(object))
}

fn literal_shape() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[\d.]+[a-z%]*$|[/~=(%]|^v?\d+(\.\d+)+").ok())
        .as_ref()
}

/// Lower snake case; one listed prefix and one listed suffix, when a remainder exists.
#[must_use]
pub(crate) fn canon_key(raw: &str) -> String {
    let snake = to_snake(raw);
    let without_prefix = strip_one(&snake, &["default_", "current_", "new_"], Strip::Prefix);
    let without_suffix = strip_one(
        &without_prefix,
        &["_value", "_number", "_setting", "_used"],
        Strip::Suffix,
    );
    if without_suffix.is_empty() {
        "value".to_string()
    } else {
        without_suffix
    }
}

enum Strip {
    Prefix,
    Suffix,
}

fn strip_one(value: &str, affixes: &[&str], side: Strip) -> String {
    for affix in affixes {
        let next = match side {
            Strip::Prefix => value.strip_prefix(affix),
            Strip::Suffix => value.strip_suffix(affix),
        };
        if let Some(rest) = next {
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    value.to_string()
}

fn to_snake(raw: &str) -> String {
    let mut out = String::new();
    let mut underscore = false;
    for ch in raw.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            underscore = false;
        } else if !underscore && !out.is_empty() {
            out.push('_');
            underscore = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// Subjects that mean the project, after [`normalise`].
const PROJECT_SUBJECTS: &[&str] = &[
    "project",
    "the project",
    "this project",
    "our project",
    "app",
    "the app",
    "codebase",
    "repo",
    "repository",
    "the tool",
    "cli",
    "the cli",
    "it",
    "this",
    "we",
    "our",
    "us",
    "code",
    "the code",
    "tool",
    "package",
    "the package",
    "library",
    "the library",
];

/// Subjects that mean the person `user`, for `prefers` and `works_on` only.
const USER_SUBJECTS: &[&str] = &[
    "user",
    "the user",
    "i",
    "me",
    "my",
    "developer",
    "the developer",
    "owner",
];

/// Point generic subjects at the project, and the speaker at person `user`.
pub(crate) fn map_subject(fact: &mut Fact, project: &ProjectIdentity) {
    let norm = normalise(&fact.subject_name);
    if is_project_name(&norm, project) {
        if !project.name.is_empty() {
            fact.subject_name = project.name.clone();
        }
        fact.subject_kind = Kind::Project;
        return;
    }
    if matches!(fact.predicate, Predicate::Prefers | Predicate::WorksOn)
        && USER_SUBJECTS.contains(&norm.as_str())
    {
        fact.subject_name = "user".to_string();
        fact.subject_kind = Kind::Person;
    }
}

fn is_project_name(norm: &str, project: &ProjectIdentity) -> bool {
    if PROJECT_SUBJECTS.contains(&norm) {
        return true;
    }
    let name = normalise(&project.name);
    if !name.is_empty() && norm == name {
        return true;
    }
    project.aliases.iter().any(|alias| normalise(alias) == norm)
}

/// Lower case, non-alphanumerics collapsed to a single space.
#[must_use]
pub(crate) fn normalise(raw: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for ch in raw.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            space = false;
        } else if !space && !out.is_empty() {
            out.push(' ');
            space = true;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// A model string past [`MAX_OBJECT_CHARS`].
#[must_use]
pub(crate) fn over_long(value: &str) -> bool {
    value.chars().count() > MAX_OBJECT_CHARS
}

/// Generic object, object equal to the subject, or an object past the length cap.
#[must_use]
pub(crate) fn generic_object(object: &str, subject: &str) -> bool {
    if over_long(object) {
        return true;
    }
    let norm = normalise(object);
    if norm.is_empty() || GENERIC_OBJECTS.contains(&norm.as_str()) {
        return true;
    }
    !subject.is_empty() && norm == normalise(subject)
}

const GENERIC_OBJECTS: &[&str] = &[
    "it",
    "this",
    "that",
    "the project",
    "project",
    "app",
    "the app",
    "code",
    "codebase",
    "the code",
    "user",
    "the user",
    "assistant",
    "we",
    "i",
    "you",
    "file",
    "files",
    "tests",
    "test",
    "repo",
    "repository",
    "thing",
    "stuff",
    "something",
    "none",
    "n a",
    "unknown",
];

/// `Err` is the drop reason. Confidence and the generic-object rule.
pub(crate) fn filter_fact(fact: &Fact) -> Result<(), &'static str> {
    if fact.confidence < MIN_CONFIDENCE {
        return Err(DROP_CONFIDENCE);
    }
    if generic_object(&fact.object_name, &fact.subject_name) {
        return Err(DROP_GENERIC);
    }
    Ok(())
}

/// Version with one leading `v` removed. It must contain a digit and occur
/// in the cited turn. The stored release is `<project> v<version>`.
pub(crate) fn release_name(project: &str, version: &str, turn_text: &str) -> Option<String> {
    let version = version.trim();
    let version = version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
        .unwrap_or(version);
    if version.is_empty() || !version.chars().any(|ch| ch.is_ascii_digit()) {
        return None;
    }
    if !turn_text.contains(version) {
        return None;
    }
    let name = if project.is_empty() {
        format!("v{version}")
    } else {
        format!("{project} v{version}")
    };
    Some(name)
}

/// Component name that is generic or the project itself affects the project.
#[must_use]
pub(crate) fn affects_target(component: &str, project: &ProjectIdentity) -> (String, Kind) {
    let norm = normalise(component);
    let generic = component.trim().is_empty()
        || GENERIC_OBJECTS.contains(&norm.as_str())
        || is_project_name(&norm, project);
    if generic && !project.name.is_empty() {
        (project.name.clone(), Kind::Project)
    } else if generic {
        (component.trim().to_string(), Kind::Project)
    } else {
        (component.trim().to_string(), Kind::Component)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mention(predicate: Predicate, subject_kind: Kind, object_kind: Option<Kind>) -> Mention {
        Mention {
            subject: "subj".into(),
            subject_kind,
            predicate,
            role: Role::None,
            key: String::new(),
            object: "obj".into(),
            object_kind,
            status: "decided".into(),
            turn: 0,
            quote: "quote".into(),
            confidence: 0.9,
            provenance: TurnRole::User,
        }
    }

    #[test]
    fn remap_table() {
        // uses + component object => depends_on
        let mut uses_component = mention(Predicate::Uses, Kind::Component, Some(Kind::Component));
        uses_component.object = "cache".into();
        let fact = remap(uses_component).unwrap();
        assert_eq!(fact.predicate, Predicate::DependsOn);
        assert_eq!(fact.object_kind, Some(Kind::Component));

        // depends_on + technology => uses, role library when none
        let mut depends = mention(
            Predicate::DependsOn,
            Kind::Component,
            Some(Kind::Technology),
        );
        depends.object = "Redis".into();
        let fact = remap(depends).unwrap();
        assert_eq!(fact.predicate, Predicate::Uses);
        assert_eq!(fact.role, Some(Role::Library));

        // an existing role is kept
        let mut depends_role = mention(
            Predicate::DependsOn,
            Kind::Component,
            Some(Kind::Technology),
        );
        depends_role.object = "Redis".into();
        depends_role.role = Role::Database;
        let fact = remap(depends_role).unwrap();
        assert_eq!(fact.role, Some(Role::Database));

        // rule / setting / prefers objects are literals
        for predicate in [Predicate::Rule, Predicate::Setting, Predicate::Prefers] {
            let kind = if predicate == Predicate::Prefers {
                Kind::Person
            } else {
                Kind::Project
            };
            let mut row = mention(predicate, kind, Some(Kind::Technology));
            row.object = "Never log bodies".into();
            let fact = remap(row).unwrap();
            assert_eq!(fact.object_kind, None, "{predicate:?}");
        }

        // uses + literal object type => technology, when the text is not a literal
        let mut uses_literal = mention(Predicate::Uses, Kind::Project, None);
        uses_literal.object = "chi".into();
        let fact = remap(uses_literal).unwrap();
        assert_eq!(fact.predicate, Predicate::Uses);
        assert_eq!(fact.object_kind, Some(Kind::Technology));

        // uses role none => other
        let mut uses_none = mention(Predicate::Uses, Kind::Project, Some(Kind::Technology));
        uses_none.object = "chi".into();
        uses_none.role = Role::None;
        let fact = remap(uses_none).unwrap();
        assert_eq!(fact.role, Some(Role::Other));

        // non-setting clears key, non-uses clears role
        let mut part = mention(Predicate::PartOf, Kind::Component, Some(Kind::Project));
        part.key = "port".into();
        part.role = Role::Library;
        part.object = "tidepool".into();
        let fact = remap(part).unwrap();
        assert_eq!(fact.key, "");
        assert_eq!(fact.role, None);

        // uses whose object looks literal => setting, key from the role
        let mut port = mention(Predicate::Uses, Kind::Project, Some(Kind::Technology));
        port.object = "9000".into();
        port.role = Role::Database;
        let fact = remap(port).unwrap();
        assert_eq!(fact.predicate, Predicate::Setting);
        assert_eq!(fact.key, "database");
        assert_eq!(fact.object_kind, None);
        assert_eq!(fact.role, None);

        // role none/other/library => key value
        let mut bare = mention(Predicate::Uses, Kind::Project, None);
        bare.object = "1.22".into();
        bare.role = Role::None;
        let fact = remap(bare).unwrap();
        assert_eq!(fact.predicate, Predicate::Setting);
        assert_eq!(fact.key, "value");

        // uses / setting / rule from technology pass the subject check
        let mut from_tool = mention(Predicate::Rule, Kind::Technology, Some(Kind::Technology));
        from_tool.object = "Pin the toolchain".into();
        assert!(remap(from_tool).is_ok());
    }

    #[test]
    fn reject_table() {
        let mut tech_tech = mention(Predicate::Uses, Kind::Technology, Some(Kind::Technology));
        tech_tech.object = "Redis".into();
        assert_eq!(remap(tech_tech).unwrap_err(), DROP_REJECTED);

        let mut project_part = mention(Predicate::PartOf, Kind::Project, Some(Kind::Project));
        project_part.object = "other".into();
        assert_eq!(remap(project_part).unwrap_err(), DROP_REJECTED);

        let mut part_tech = mention(Predicate::PartOf, Kind::Component, Some(Kind::Technology));
        part_tech.object = "Redis".into();
        assert_eq!(remap(part_tech).unwrap_err(), DROP_REJECTED);

        // subject kind not allowed for the predicate
        let person_part = mention(Predicate::PartOf, Kind::Person, Some(Kind::Project));
        assert_eq!(remap(person_part).unwrap_err(), DROP_REJECTED);
        let person_uses = mention(Predicate::Uses, Kind::Person, Some(Kind::Technology));
        assert_eq!(remap(person_uses).unwrap_err(), DROP_REJECTED);

        // technology + uses + literal object type is not row 3. A version
        // becomes a setting; a name stays a uses edge.
        let mut version = mention(Predicate::Uses, Kind::Technology, None);
        version.subject = "Rust".into();
        version.object = "1.75".into();
        let fact = remap(version).unwrap();
        assert_eq!(fact.predicate, Predicate::Setting);
        assert_eq!(fact.key, "value");
        assert_eq!(fact.object_kind, None);

        let mut named = mention(Predicate::Uses, Kind::Technology, None);
        named.subject = "Typer".into();
        named.object = "Click".into();
        let fact = remap(named).unwrap();
        assert_eq!(fact.predicate, Predicate::Uses);
        assert_eq!(fact.object_kind, Some(Kind::Technology));
        assert_eq!(fact.object_name, "Click");
    }

    #[test]
    fn canon_key() {
        assert_eq!(super::canon_key("Port"), "port");
        assert_eq!(super::canon_key("Go Version"), "go_version");
        assert_eq!(super::canon_key("default_port"), "port");
        assert_eq!(super::canon_key("current_timeout"), "timeout");
        assert_eq!(super::canon_key("new_port_value"), "port");
        assert_eq!(super::canon_key("line_length_setting"), "line_length");
        assert_eq!(
            super::canon_key("max_concurrency_number"),
            "max_concurrency"
        );
        assert_eq!(super::canon_key("formatter_used"), "formatter");
        assert_eq!(super::canon_key("Default_Port_Setting"), "port");
        assert_eq!(super::canon_key(""), "value");
        assert_eq!(super::canon_key("default_"), "default");
        assert_eq!(super::canon_key("port_value_number"), "port_value");
    }

    #[test]
    fn subject_map() {
        let project = ProjectIdentity {
            name: "linkshelf".into(),
            aliases: vec!["linkshelf".into(), "shelf".into()],
        };

        let mut generic = fact(Predicate::Uses, Kind::Component, "the project");
        map_subject(&mut generic, &project);
        assert_eq!(generic.subject_name, "linkshelf");
        assert_eq!(generic.subject_kind, Kind::Project);

        let mut alias = fact(Predicate::Setting, Kind::Component, "Shelf");
        map_subject(&mut alias, &project);
        assert_eq!(alias.subject_name, "linkshelf");
        assert_eq!(alias.subject_kind, Kind::Project);

        let mut speaker = fact(Predicate::Prefers, Kind::Project, "the user");
        map_subject(&mut speaker, &project);
        assert_eq!(speaker.subject_name, "user");
        assert_eq!(speaker.subject_kind, Kind::Person);

        let mut works = fact(Predicate::WorksOn, Kind::Person, "I");
        map_subject(&mut works, &project);
        assert_eq!(works.subject_name, "user");

        // the user set only applies to prefers and works_on
        let mut other = fact(Predicate::Uses, Kind::Person, "the user");
        map_subject(&mut other, &project);
        assert_eq!(other.subject_name, "the user");
        assert_eq!(other.subject_kind, Kind::Person);

        let mut named = fact(Predicate::Uses, Kind::Technology, "SQLite");
        map_subject(&mut named, &project);
        assert_eq!(named.subject_name, "SQLite");
        assert_eq!(named.subject_kind, Kind::Technology);
    }

    #[test]
    fn filters_drop_generic_low_confidence_and_long_values() {
        let mut low = fact(Predicate::Setting, Kind::Project, "linkshelf");
        low.confidence = 0.59;
        low.object_name = "9000".into();
        assert_eq!(filter_fact(&low).unwrap_err(), DROP_CONFIDENCE);

        let mut kept = low.clone();
        kept.confidence = 0.6;
        assert!(filter_fact(&kept).is_ok());

        let mut generic = fact(Predicate::Uses, Kind::Project, "linkshelf");
        generic.object_name = "the project".into();
        assert_eq!(filter_fact(&generic).unwrap_err(), DROP_GENERIC);

        let mut same = fact(Predicate::Uses, Kind::Project, "Linkshelf");
        same.object_name = "linkshelf".into();
        assert_eq!(filter_fact(&same).unwrap_err(), DROP_GENERIC);

        let mut slash = fact(Predicate::Uses, Kind::Project, "linkshelf");
        slash.object_name = "N/A".into();
        assert_eq!(filter_fact(&slash).unwrap_err(), DROP_GENERIC);

        let mut long = fact(Predicate::Setting, Kind::Project, "linkshelf");
        long.object_name = "x".repeat(301);
        assert_eq!(filter_fact(&long).unwrap_err(), DROP_GENERIC);
    }

    fn fact(predicate: Predicate, kind: Kind, subject: &str) -> Fact {
        Fact {
            subject_name: subject.into(),
            subject_kind: kind,
            predicate,
            role: None,
            key: String::new(),
            object_name: "obj".into(),
            object_kind: Some(Kind::Technology),
            status: "decided".into(),
            turn: 0,
            quote: "quote".into(),
            confidence: 0.9,
            provenance: Provenance::User,
            replaces: String::new(),
        }
    }
}
