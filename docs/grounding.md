# Grounding

Pipeline v2 keeps a candidate only after the gates below. Each whole-candidate failure counts a reason on the report. A field failure does not drop the candidate.

The quote is checked first. The schema remap runs next, so a setting is bound on its canonical key. Both a project `uses` and a decided `part_of` from the project to a component are swapped to that component `part_of` the session project, and value support then checks the component name. Value support, key binding and replaces follow. Confidence and the generic-object rule are last. A reply that is not the schema, an over-long string, or more than 200 items in one section drops before these gates (`parse`, `too_long`, `overflow`).

## Gate 1 — quote

The trimmed quote must be at least three characters and contain an alphanumeric character. It must be an exact, case-sensitive substring of a turn. Tool lines are not turns and never match. A quote in the cited turn keeps that turn. A quote found only in another turn is re-cited to the first such turn in transcript order, and the fact takes that turn's role. Otherwise the candidate drops as `quote`. When that exact search misses, an underscore with an alphanumeric character on both sides may match that underscore or one ASCII space, and every other character must still match exactly. On such a match the stored quote is the turn's text.

**Bug field fallback.** Bugs only, and only after that search has already failed:

1. The reply quote has missed the exact check, re-cite and the separator pass, so the candidate would drop as `quote`.
2. The bug's own fields are tried in order: symptom, then cause, then fix. No other turn is examined.
3. A field is used only when, after trimming, it is at least three characters, has at least three content tokens, is within the length cap, and is an exact case-sensitive substring of the cited turn. A change of case, a hyphen, or an underscore standing for a space does not match.
4. The cited turn and its role stay, and the stored quote is that field. If no field qualifies, the bug still drops as `quote`. Facts and releases never use this fallback.

A bug cited to "Confirmed: the TTL was ignored by the reader, so expired rows were served. I changed the reader to check expires_at on read." with quote "the cache returned stale rows after the reader" stores the cause "the TTL was ignored by the reader, so expired rows were served".

## Gate 2 — value support

The value is checked against the cited turn. Every number in the value has to occur there as a number, and the token rule has to hold. Failure drops the candidate as `value_support`.

A number is a maximal run of digits with optional `.digits` groups. A `-` immediately before the first digit is part of the number when the character before the hyphen is absent or is not alphanumeric. A hyphen after a letter or a digit is a range or an identifier, so `3-5` yields 3 and 5. `8080` does not occur inside `18080`, `0.2` does not occur inside `0.2.0`, and `-1` does not occur as `1`.

Content tokens are the normalised tokens longer than one character, or digits, minus the stop list: the, a, an, of, and, or, to, in, on, for, with, is, are, be, by, at, as, it, its, use, uses, using, used, from, that, this, now, all, any, not, no. A token longer than three characters may match as a case-insensitive substring of the normalised text. A shorter token has to equal a whole normalised token. A digit-only token also matches when that same number occurs in the raw text. A value with no content tokens fails. A setting whose trimmed value is one to three characters and contains no alphanumeric character and no whitespace is supported when the trimmed quote wraps that value in backticks or a matching single or double quote (a mark that occurs in the value does not count as its wrapper), the cited turn exists, and at least one specific key token occurs as a whole token in the trimmed quote or in the sentence that contains the quote.

Three content tokens or fewer need at least half of them in the turn and at least one in the quote. A longer literal, and a longer entity name, need at least half of their content tokens in the turn.

A release whose version does not occur in the cited turn drops as `bad_version`. The drop is a whole candidate. One leading `v` or `V` is removed. The remainder must be non-empty, contain a digit, and occur as a plain substring of the cited turn, so the non-numeric part of a version such as `1.4.0-rc1` or `2024.10b` is still stated. At least one of those occurrences must not run into a digit, a letter other than a leading `v` or `V`, or a dotted number. Every number in that remainder must also occur there as a maximal number, under the number rule above. `1.0` therefore drops inside `Ship 1.0.0`.

## Gate 3 — key binding

A setting whose value contains a number has to name its key in the quote or in the sentence that contains the quote. The key is split on `_`. At least one token that is not generic (`value`, `number`, `setting`, `default`, `current`, `new`, `min`, `max`) has to occur there as a whole token. A key whose tokens are all generic has nothing to bind, so the setting drops as `key_binding`. A key ending in `_version` also needs every stem token, and an empty stem fails.

`.` `!` and `?` end a sentence only when the next byte is whitespace or the text ends. A newline always does. The window stops at a boundary the quote itself ends on.

**Agreement.** When that check fails, a user turn can still bind the setting from the assistant turn immediately before or after it. Every number of the value has to occur in the trimmed quote. The sentence that contains the quote has the same limit: every number of the value, and no other number. Repeats of the same number are allowed. The word immediately before a value number blocks the fallback unless it is a stop word or an agreement word: `yes`, `sure`, `ok`, `okay`, `yeah`, `yep`, `alright`, `please`, `fine`, `agreed`, `great`, `perfect`, `thanks`, `good`, `sounds`, `right`, `then`, `let`, `lets`, `go`, `do`, `try`, `keep`, `pick`, `choose`, `take`, `make`, `set`, `switch`, `change`, `prefer`, `say`, `works`. `not`, `no`, `don't`, `never` and `avoid` block even though `not` and `no` are stop words. A number that starts the sentence does not block. Only the two neighbouring turns are examined, and the neighbour has to be an assistant turn. One of its sentences has to contain every specific key token as a whole token (the key needs at least one; a `_version` key does not require the trailing `version` token when another specific token is present), every stem token of a `_version` key (an empty stem fails), and every number of the value, with no other number in that sentence. The fact keeps the cited turn, the quote and the provenance. A user turn "9123 is good." followed by "Default port is now 9123." keeps `port` = `9123`. Beside "Should the port be 9123?", "Not 9123, use 9124.", "No, 9123 is taken.", "Never 9123.", "Avoid 9123." and "Timeout 9123 ms is fine." drop, as do "Use 30 and 9123." and "9123 or 9124?" because of the extra number. "Yes, 9123.", "Sure, 9123 works.", "Yes please, 9123.", "I prefer 9123.", "Fine, 9123.", "Let's do 9123." and "Keep 9123." stay. An assistant citation beside another assistant turn does not bind.

## Gate 4 — replaces

`replaces` stays only on a decided fact, and only when one of its content tokens occurs in the cited turn. Otherwise the field is cleared and the fact stays.

## Lists are not replacements

A decided fact whose value is one member of a list does not retire a live row. `issue_status` is never treated as a list member. When the sentence that contains the quote mentions the new value, only that sentence is judged. Otherwise any sentence of the cited turn that mentions the value can list it. A value mentioned nowhere is not a list, and today's retirement applies.

The value is listed when one mention of it meets any of these:

- **Bracket list.** The mention sits in the innermost `[...]`, `(...)` or `{...}`, and inside that pair, at depth 1, there is a comma, pipe, semicolon, or the word `and` or `or`, plus another alphanumeric run. `runtime: [4.1, 4.2, 4.3]` qualifies. `(was 4.2)` does not.
- **Sibling across a list gap.** The gap may hold whitespace, `,` `|` `&` `+`, the words `and` `or` `nor` `plus`, or a `/` with whitespace or digits on both sides, and it must contain one of those. Pure whitespace is not a gap. `to`, `->`, `=>`, `from`, `with`, `instead` and `-` are never gaps. A numbered neighbour qualifies when its first run has a digit, or when that run repeats the label just before the value and the next run has a digit. A numberless chain qualifies at three items, or at two items plus a list cue (`matrix`, `versions`, `variants`, `targets`, `tested`, `test`, `tests`, `across`, `each`, `every`, `all`, `supported`, `supports`, `both`, `either`). A path such as `src/bin/main.rs` is not a list.
- **Additive cue.** `also`, `too`, `as well`, `additionally` or `in addition` within three words of the mention, unless `to` or a departure word is one of the three words immediately before the value.

The fact then retires nothing: not through `replaces`, not through a single-valued supersession, and not through a matched edge. Each row that would have closed counts as `retirement_skips.enumeration`. A setting that already has a different live value is not written; the drop reason is `enumeration`. An edge is still written. Promotion, rejection, a same-session issue-status close, and a row opened in this session are unchanged.

`Test on runtime 4.2 and 4.3.` leaves `4.2` live and does not store `4.3`. `Bump the runtime from 4.2 to 4.3.` still retires `4.2`. `Keep 4.2 and also test 4.3.` does not. `Builds run on Alpha, Beta and Gamma.` does not retire a live `Beta`.

## Known-row match (opt-in)

`known_match` is off by default, in config, on the bench command, and in the writer. Pass 1 is not shown stored rows and its request does not change. With the switch on, after pass 1, kept facts are matched to the scoped live rows. The cap used for the known-facts block does not apply. The matcher never changes a value, a quote, a turn, a status or a provenance, and it never creates a fact.

A decided setting with an empty key takes the key of the one stored decided setting of the same normalised subject whose required key tokens all occur as whole tokens in the cited sentence, and whose value contains a number only when the new value does. Zero matches do nothing. Two or more are skipped as ambiguous. A shape mismatch is skipped and counted.

A decided setting whose key is not already the same family as a live key is rewritten onto the one stored key that has the same token list after splitting separators and camel case and dropping generic affixes. The lists must be equal. A shorter list is not a match. Two equal lists are skipped as ambiguous.

A decided `uses` with an empty `replaces` and a role other than `other`, `none` or `library` can name one stored live `uses` of the same subject and role. The old value has to be mentioned in the cited turn, and a departure cue has to sit within three words of that mention. The new object must not already be a live `uses` of that subject and role. The close is that edge id only. A second row with the same value stays.

While the switch is on, every superseded or replaced close a decided fact would make also has to pass two guards. The cited turn has to contain a specific key token as a whole token, or mention the old value. When both values sit in one sentence and the text between them is only a list connector, or a keep cue sits within three words of the old value, the old row stays. Rejected closes, promoted closes, and a same-session issue-status close are unchanged. An ambiguous match, a missing target, or a row that changed after the snapshot skips that retirement and still writes the new fact. A row opened in the current session is not closed. A retirement stamps only `valid_until` and `ended_by`.

## Consolidation pass (opt-in, prototype)

`consolidate` is off by default, in config and on `bench writes`. Off does not define `consolidation_cache`, does not read the store for this pass, and does not call the provider. The pass-1 prompt, schema and request stay as they are.

With `[graph] consolidate = true` or `--consolidate`, after the session's writes the pass looks at rows this session closed, at a grounded `replaces` that closed nothing, and at a decided setting or rule created this session that closed nothing. Partners are live decided rows in the project neighbourhood that this session did not open. A fixed pre-filter can mark a duplicate, a supersede, or a drop. The pairs that remain go to one structured call (`CONSOLIDATE_V2`, `max_tokens` 400). The reply is one JSON object; a markdown fence or surrounding text still parses, and prose closes nothing. A supersede or a duplicate is kept only when `quote` is a verbatim span of the evidence that was sent, at least 12 characters and two word tokens, and a duplicate may instead quote either stored value. A supersede also needs a departure cue. The existing cues still have to sit beside the stored value or the replaces text in the cited turn. When the quote does not name that stored value, it can instead carry `is now`, `are now`, `now uses`, or `new <word>:` at the start of the quote or a sentence. That quote has to name the new value and a token of the anchor's key, and the pair has to share a subject and a key, or a component linked by `part_of` and a key. A phrase outside the quote does not count. A question mark rejects the quote. So does a negation or a hedge anywhere in it (`not`, `no`, `never`, `nor`, `neither`, a word ending in `n't`, `if`, `unless`, `whether`, `maybe`, `perhaps`, `might`, `could`, `would`, `should`, `may`, `suppose`, `suggest`, `propose`, `proposal`, `idea`, `consider`), and so does an addition (`also`, `too`, `well`, `additionally`, `available`, `plus`). A comma, or `and`, `or`, `nor`, or `plus`, rejects it as a list. Two supersedes for one anchor close nothing. A close writes `valid_until` and `ended_by` only. The row opened in the creating session is not closed. A provider or parse error is counted and the ingest still succeeds.

A verdict the guards do not apply is counted on `consolidate.dropped`, and the bench report sums those counts as `consolidate_drops`. The reasons are `quote too short`, `quote not verbatim in the evidence`, `no departure cue`, `second supersede on the same anchor`, `no kept live copy (duplicate)`, `row opened this session`, and `anchor or pair no longer present`. While consolidation is on, every ingest prints one line per drop on stderr: the reason, the stored row's subject and key, the anchor's subject and key, and the first 80 characters of the quote. Off, both counters are absent and nothing is printed.

## Gate 5 — confidence and generic objects

A fact whose confidence is below 0.6 drops as `low_confidence`. A generic object, an object equal to its subject, or an object past the length cap drops as `generic_object`. Bugs and releases use the same confidence floor before their fields are read. A setting with a named key whose value is one to three characters and contains no alphanumeric character and no whitespace skips the generic-object check, and the confidence floor still applies.

## Symptom on a later turn

A bug symptom is a literal. The cited turn is checked first. When that turn is later than another turn of the same session, the cited turn and every earlier turn are checked together. A fix turn can therefore carry a symptom that was said earlier. Turns after the citation are not support. The opening turn cannot borrow a symptom from a later turn.

The quote still has to be in the cited turn (gate 1, including re-cite). Every number in the symptom has to occur in that supporting text. A number that occurs only inside a longer number does not. A symptom that appears nowhere there drops the whole bug as `value_support`.

A symptom the token rule does not support on the cited turn still keeps that turn's role when its normalised words, stop words included, are one contiguous run of whole words in the cited turn and it has at least three content words. Normalisation is the same one the other gate-2 checks use: case-folded, with punctuation as separators, so "Froze on a LARGE archive" matches "froze on a large archive". There is no stemming and no skipped stop word inside the run. A span cut mid-word does not match. Words that sit in the cited turn but not as one run, a span of fewer than three content words, and a number the cited turn does not state still drop the bug as `value_support`. Other turns are not searched for this run. Cause, fix, release contents and settings do not use it.

A symptom the cited turn does not support is inferred (`self`). One the cited turn does support, including one kept by that run, keeps that turn's role.

Cause, fix and release contents already use the cited turn and, failing that, the joined session, on every turn. A fix that restates the symptom is kept when that text supports it. When it does not, the field drops as `value_support_partial` and the bug stays. A named component is still checked only against the cited turn.

A fact stored in the current session stays live for the rest of that session, so a later turn leaves a setting that was opened earlier untouched. Issue status is the one exception. Pass 1 writes `issue_status` as `open` when no fix was kept and as `fixed` when a fix stayed, and that value is inferred (`self`). A decided `fixed` closes a live synthetic `open` of the same issue when that slot was opened in this session on a strictly earlier turn. The stamp is `valid_until` and `ended_by` with reason `superseded`, and the closed slot keeps its counts. A later `open` leaves an earlier `fixed` in place. Two entries that cite the same turn both stay. Every other predicate keeps the session protection.

## Reports

`kept` plus the whole-candidate drop counts equals `candidates`. Cause, fix, release contents, and a named `affects` edge that fail value support count as `value_support_partial`. They are listed separately and are not part of that sum. An empty optional field is not a drop. `unresolved` is counted at write time on a candidate that was already kept, so it sits outside the pass-1 sum.

## Known facts

The known-facts block is experimental and off by default. Pass 1 is not shown stored rows, and its request does not mention them, unless `[graph] known_facts_context` is true or `bench writes` is run with `--known-facts-context`. With the switch on, live rows from earlier sessions are listed after the session date as data. A quote still has to come from a turn; text that appears only in the block is not evidence.
