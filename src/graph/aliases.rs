// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which stored entity each extracted name ended up as.
//!
//! A relationship names its endpoints the way the model wrote them in that
//! chunk. By the time it is stored, the entity may live under another name:
//!
//! ```text
//! "synth"        ── same-name local merge ──▶ candidate "Synth"   ──▶ stored "Synth"
//! "Synth pulse"  ── dedup: skip / merge ────────────────────────────▶ stored "Synth"
//! ```
//!
//! Every candidate name is recorded here, case-folded, against the entity it
//! resolved to, and relationship endpoints are looked up through it before
//! the store is asked.

use std::collections::HashMap;

/// Extracted name (case-folded) → the name of the stored entity it became.
#[derive(Debug, Default, Clone)]
pub struct EntityAliases {
    stored_by_alias: HashMap<String, String>,
}

impl EntityAliases {
    /// Record that `alias` resolved to the stored entity named `stored`.
    ///
    /// The stored name becomes an alias of itself too, unless it already
    /// resolves elsewhere: a candidate spelled like another entity's stored
    /// name must not redirect that entity.
    pub fn record(&mut self, alias: &str, stored: &str) {
        self.stored_by_alias.insert(fold(alias), stored.to_string());
        self.stored_by_alias
            .entry(fold(stored))
            .or_insert_with(|| stored.to_string());
    }

    /// The stored name `name` resolved to, compared case-insensitively and
    /// ignoring runs of whitespace.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&str> {
        self.stored_by_alias.get(&fold(name)).map(String::as_str)
    }
}

/// A name reduced to what two spellings of the same name share: lowercase,
/// single spaces, no surrounding whitespace.
#[must_use]
pub fn fold(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_case_variant_resolves_to_the_stored_name() {
        let mut aliases = EntityAliases::default();
        aliases.record("Synth", "Synth");
        assert_eq!(aliases.resolve("synth"), Some("Synth"));
        assert_eq!(aliases.resolve("  SYNTH "), Some("Synth"));
    }

    #[test]
    fn a_duplicate_resolves_to_the_entity_it_duplicates() {
        let mut aliases = EntityAliases::default();
        aliases.record("Synth pulse", "Synth");
        assert_eq!(aliases.resolve("synth  pulse"), Some("Synth"));
        assert_eq!(aliases.resolve("Synth"), Some("Synth"));
    }

    /// A stored name recorded as the target of a later resolution does not
    /// take over a spelling an earlier candidate already resolved.
    #[test]
    fn a_target_name_does_not_redirect_an_earlier_alias() {
        let mut aliases = EntityAliases::default();
        aliases.record("Synth pulse", "Synth");
        aliases.record("The Synth pulse", "Synth pulse");
        assert_eq!(aliases.resolve("synth pulse"), Some("Synth"));
        assert_eq!(aliases.resolve("the synth pulse"), Some("Synth pulse"));
    }

    #[test]
    fn an_unknown_name_resolves_to_nothing() {
        assert_eq!(EntityAliases::default().resolve("User"), None);
    }
}
