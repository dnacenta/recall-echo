# Grounding

Pipeline v2 keeps a candidate only after the gates below. Each whole-candidate failure counts a reason on the report. A field failure does not drop the candidate.

The quote is checked first. The schema remap runs next, so a setting is bound on its canonical key. A project `uses` whose object is a component becomes that component `part_of` the session project, and value support then checks the component name. Value support, key binding and replaces follow. Confidence and the generic-object rule are last. A reply that is not the schema, an over-long string, or more than 200 items in one section drops before these gates (`parse`, `too_long`, `overflow`).

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

## Gate 5 — confidence and generic objects

A fact whose confidence is below 0.6 drops as `low_confidence`. A generic object, an object equal to its subject, or an object past the length cap drops as `generic_object`. Bugs and releases use the same confidence floor before their fields are read. A setting with a named key whose value is one to three characters and contains no alphanumeric character and no whitespace skips the generic-object check, and the confidence floor still applies.

## Symptom on a later turn

A bug symptom is a literal. The cited turn is checked first. When that turn is later than another turn of the same session, the cited turn and every earlier turn are checked together. A fix turn can therefore carry a symptom that was said earlier. Turns after the citation are not support. The opening turn cannot borrow a symptom from a later turn.

The quote still has to be in the cited turn (gate 1, including re-cite). Every number in the symptom has to occur in that supporting text. A number that occurs only inside a longer number does not. A symptom that appears nowhere there drops the whole bug as `value_support`.

A symptom the cited turn does not support is inferred (`self`). One the cited turn does support keeps that turn's role.

Cause, fix and release contents already use the cited turn and, failing that, the joined session, on every turn. A fix that restates the symptom is kept when that text supports it. When it does not, the field drops as `value_support_partial` and the bug stays. A named component is still checked only against the cited turn.

A fact stored in the current session stays live for the rest of that session, so a later turn leaves a setting that was opened earlier untouched. Issue status is the one exception. Pass 1 writes `issue_status` as `open` when no fix was kept and as `fixed` when a fix stayed, and that value is inferred (`self`). A decided `fixed` closes a live synthetic `open` of the same issue when that slot was opened in this session on a strictly earlier turn. The stamp is `valid_until` and `ended_by` with reason `superseded`, and the closed slot keeps its counts. A later `open` leaves an earlier `fixed` in place. Two entries that cite the same turn both stay. Every other predicate keeps the session protection.

## Reports

`kept` plus the whole-candidate drop counts equals `candidates`. Cause, fix, release contents, and a named `affects` edge that fail value support count as `value_support_partial`. They are listed separately and are not part of that sum. An empty optional field is not a drop. `unresolved` is counted at write time on a candidate that was already kept, so it sits outside the pass-1 sum.
