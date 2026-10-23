# linkshelf write fixture

A **synthetic corpus** used as a **regression fixture** for the write path. It is
**not a benchmark** and **not a tuning target**.

- Never tune prompt text, gate thresholds or extraction rules to raise a score on it.
  A change is justified by a failing unit test on a generic case, not by this fixture.
- Scores on it are **optimistic**. An earlier extractor prototype was iterated against
  this same corpus, so it overstates how well extraction does on unseen sessions.

## What it is

Fourteen agent sessions about one made-up project, `linkshelf` (a Python bookmark-manager
CLI). Every message was written by hand. There are no real people, emails, tokens, hostnames
or user paths. `~/.local/share/linkshelf` and `~/.config/linkshelf` are generic XDG paths,
the transcript `cwd` is `/work/linkshelf`, and `github.com` appears only as a site that
rate-limits.

After the sessions are ingested **in order (s01..s14)**, the store should hold the end state
described in `script.toml`:

- 46 facts: 39 live, 6 superseded, 1 rejected (F42, a dependency that was tried and removed).
- 6 superseded pairs (`old` must be retired in `retire_in` or later, never earlier).
- 7 noise items (chit-chat, undecided ideas, transient tool output) that must not be stored
  as decided or current facts.

## Files

| path | what |
|---|---|
| `script.toml` | The fact script: transcript sessions with claims, facts with `end_state` and regex `match` patterns, superseded pairs, noise. |
| `sessions/sNN.jsonl` | The 14 transcripts. User text `content` is a plain string; assistant messages and tool results are block lists. `script.toml` points here. |
| `sessions-blocks/sNN.jsonl` | The same 14 sessions with every message `content` as a list of blocks (`{"type": "text"}`, `tool_use`, `tool_result`). |

The transcripts use the JSON Lines format read by `src/jsonl.rs`: one entry per line with
`type`, `timestamp`, `message.role` and `message.content`, including tool calls and tool output.

| session | first timestamp (UTC) | lines |
|---|---|---|
| s01 | 2026-09-28T09:10:37Z | 11 |
| s02 | 2026-09-29T17:40:37Z | 16 |
| s03 | 2026-09-30T18:05:37Z | 13 |
| s04 | 2026-10-02T08:30:37Z | 14 |
| s05 | 2026-10-03T19:00:37Z | 8 |
| s06 | 2026-10-05T10:15:37Z | 15 |
| s07 | 2026-10-06T16:20:37Z | 11 |
| s08 | 2026-10-08T18:45:37Z | 10 |
| s09 | 2026-10-09T09:00:37Z | 10 |
| s10 | 2026-10-11T11:30:37Z | 10 |
| s11 | 2026-10-12T20:10:37Z | 10 |
| s12 | 2026-10-13T18:00:37Z | 12 |
| s13 | 2026-10-15T07:50:37Z | 15 |
| s14 | 2026-10-16T17:30:37Z | 14 |

## Grading notes

- Each claim's `quote` is a verbatim substring of one user or assistant text message in its
  transcript (never tool output). The quote's speaker is the fact's provenance: 30 facts come
  from the user, 16 from the assistant.
- Patterns are case-insensitive and run against stored records rendered as
  `subject | relation | object`. They are a starting point, not an oracle: expect a few false
  hits, for example an old value quoted inside a bug description.
- `unmatched` (live records that match no fact pattern) is only a proxy for junk.

## Grading caveats

- A rejected fact whose core matches a live record is `wrong` with reason `rejected_live`, and it is not `found`. F42 is that case on the current writer: the dependency is stored even though the user rejected it. A rejected fact with no live match is `rejected.absent` and counts as neither found nor missing.
- Records are assigned one-to-one. The record that matches the most detail patterns is preferred, then the fact with the fewest candidates. A record used for one fact is not another fact's find. Core matches left over are reported as `shared` (F01 and F03 can still see F32's record, F13 and F14 can still see F43 and F46, F39 and F40 can still see F41).
