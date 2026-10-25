# Grounding

Pipeline v2 keeps a candidate only after the gates below. Each whole-candidate failure counts a reason on the report. A field failure does not drop the candidate.

The quote is checked first. The schema remap runs next, so a setting is bound on its canonical key. Value support, key binding and replaces follow. Confidence and the generic-object rule are last. A reply that is not the schema, an over-long string, or more than 200 items in one section drops before these gates (`parse`, `too_long`, `overflow`).

## Gate 1 — quote

The trimmed quote must be at least three characters and contain an alphanumeric character. It must be an exact, case-sensitive substring of a turn. Tool lines are not turns and never match. A quote in the cited turn keeps that turn. A quote found only in another turn is re-cited to the first such turn in transcript order, and the fact takes that turn's role. Otherwise the candidate drops as `quote`.

## Gate 2 — value support

The value is checked against the cited turn. Every number in the value has to occur there as a number, and the token rule has to hold. Failure drops the candidate as `value_support`.

A number is a maximal run of digits with optional `.digits` groups. A `-` immediately before the first digit is part of the number when the character before the hyphen is absent or is not alphanumeric. A hyphen after a letter or a digit is a range or an identifier, so `3-5` yields 3 and 5. `8080` does not occur inside `18080`, `0.2` does not occur inside `0.2.0`, and `-1` does not occur as `1`.

Content tokens are the normalised tokens longer than one character, or digits, minus the stop list: the, a, an, of, and, or, to, in, on, for, with, is, are, be, by, at, as, it, its, use, uses, using, used, from, that, this, now, all, any, not, no. A token longer than three characters may match as a case-insensitive substring of the normalised text. A shorter token has to equal a whole normalised token. A digit-only token also matches when that same number occurs in the raw text. A value with no content tokens fails.

Three content tokens or fewer need at least half of them in the turn and at least one in the quote. A longer literal, and a longer entity name, need at least half of their content tokens in the turn.

A release whose version is not in the cited turn drops as `bad_version`. That is the release form of this gate: the version string has to occur in the cited turn, and the drop is still a whole candidate. The reason stays `bad_version`.

## Gate 3 — key binding

A setting whose value contains a number has to name its key in the quote or in the sentence that contains the quote. The key is split on `_`. At least one token that is not generic (`value`, `number`, `setting`, `default`, `current`, `new`, `min`, `max`) has to occur there as a whole token. A key whose tokens are all generic has nothing to bind, so the setting drops as `key_binding`. A key ending in `_version` also needs every stem token, and an empty stem fails.

`.` `!` and `?` end a sentence only when the next byte is whitespace or the text ends. A newline always does. The window stops at a boundary the quote itself ends on.

## Gate 4 — replaces

`replaces` stays only on a decided fact, and only when one of its content tokens occurs in the cited turn. Otherwise the field is cleared and the fact stays.

## Gate 5 — confidence and generic objects

A fact whose confidence is below 0.6 drops as `low_confidence`. A generic object, an object equal to its subject, or an object past the length cap drops as `generic_object`. Bugs and releases use the same confidence floor before their fields are read.

## Symptom on a later turn

A bug symptom is a literal. The cited turn is checked first. When that turn is later than another turn of the same session, the joined session text is checked too. A fix turn can therefore carry a symptom that was said earlier in the session. The opening turn cannot: the symptom has to be in that turn.

The quote still has to be in the cited turn (gate 1, including re-cite). Every number in the symptom has to occur in the session text. A number that occurs only inside a longer number does not. A symptom that appears nowhere in the session drops the whole bug as `value_support`.

Support may sit in any turn of the session, including a turn after the citation. A symptom the cited turn does not support is inferred (`self`). One the cited turn does support keeps that turn's role.

Cause, fix and release contents already use the cited turn and, failing that, the joined session, on every turn. A fix that restates the symptom is kept when that text supports it. When it does not, the field drops as `value_support_partial` and the bug stays. A named component is still checked only against the cited turn.

## Reports

`kept` plus the whole-candidate drop counts equals `candidates`. Cause, fix, release contents, and a named `affects` edge that fail value support count as `value_support_partial`. They are listed separately and are not part of that sum. An empty optional field is not a drop. `unresolved` is counted at write time on a candidate that was already kept, so it sits outside the pass-1 sum.
