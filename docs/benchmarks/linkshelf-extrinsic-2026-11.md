# linkshelf: store state vs. what recall returns (scripted, 2026-11)

`bench writes` grades the store a fact script leaves behind (intrinsic). The
script's derived questions add an extrinsic side: each answer is looked for as
an exact value in two places.

- **store**: a live decided row of the final store (`final_edges`, v2 slots
  included).
- **recall**: the text `recall_query` returns for the question, through the MCP
  server with the tool's defaults, read on a copy of the store. `recall 8k` is
  the first 8,000 characters (the read budget); `recall 24k` is the whole
  result, which the server stops at 24,000.

A current-value question also records `stale 8k` / `stale 24k`: whether the
value it replaced shows up anywhere in that window, not only in the first
result. `graph` is the existing check (needed facts live, `absent` facts not)
and `keyword` the existing archive search.

Matching is literal, case-insensitive and whole-span (`8080` does not hit
inside `18080`). Each answer is the gold object of the fact it needs, and each
stale value is the old object of its superseded pair, filled in by the script
loader. Nothing is worded by hand except the question text.

## Setup

- Fixture `tests/fixtures/writes/linkshelf/script.toml`, 14 sessions, 9
  questions: the 6 superseded pairs as current-value questions, 2 stable
  settings, 1 refused value.
- Scripted extractor, no provider call. The configuration is the one the CI
  baselines pin (`linkshelf_scripted_v1_baseline`, `linkshelf_scripted_v2`):
  every switch off. A run with no switch flags gave the same question columns.
- The recall renders were 14,512 to 16,247 characters on v1 and 13,795 to
  15,492 on v2, so the 8k window holds about half of each.

Intrinsic summary: v1 complete 44 of 46, wrong 7 (the six old values and F42
stay live). v2 complete 35, wrong 1, 3 of 6 pairs retired.

## v1

| question | kind | graph | keyword | store | recall 8k | recall 24k | stale 8k | stale 24k |
|---|---|---|---|---|---|---|---|---|
| storage (F14, was F04) | current | no | no | yes | no | no | yes | yes |
| tag storage (F15, was F09) | current | no | no | yes | yes | yes | no | no |
| formatter (F22, was F10) | current | no | no | yes | no | no | yes | yes |
| test command (F33, was F05) | current | no | no | yes | no | no | yes | yes |
| serve port (F35, was F26) | current | no | yes | yes | yes | yes | yes | yes |
| http client (F38, was F27) | current | no | no | yes | no | no | yes | yes |
| python version (F01) | exact | yes | yes | yes | yes | yes | | |
| title fetch timeout (F28) | exact | yes | yes | yes | yes | yes | | |
| retry library (refused F42) | exact | no | no | yes | no | no | | |

## v2

| question | kind | graph | keyword | store | recall 8k | recall 24k | stale 8k | stale 24k |
|---|---|---|---|---|---|---|---|---|
| storage (F14, was F04) | current | yes | no | yes | no | no | no | no |
| tag storage (F15, was F09) | current | yes | no | yes | no | no | no | no |
| formatter (F22, was F10) | current | yes | no | yes | no | no | yes | yes |
| test command (F33, was F05) | current | no | no | no | no | no | yes | yes |
| serve port (F35, was F26) | current | no | yes | no | yes | yes | yes | yes |
| http client (F38, was F27) | current | yes | no | yes | no | no | no | no |
| python version (F01) | exact | yes | yes | yes | yes | yes | | |
| title fetch timeout (F28) | exact | yes | yes | yes | yes | yes | | |
| retry library (refused F42) | exact | yes | no | no | no | no | | |

Totals, store / recall 8k / stale 8k (of the 6 current-value questions):
v1 9 / 4 / 5, v2 6 / 3 / 3.

## Reading it

- **The two channels diverge.** v2 holds the current storage, tag storage and
  http client values only in slots. Today's query render prints entity
  abstracts and conversation fragments, not slots, so those rows are store yes,
  recall no. That gap belongs to the open read-path work; this grader records
  it and does not fix it.
- **Recall can answer what the store lost.** v2 dropped both serve-port
  settings, yet recall finds `8765` in a fragment that says it. The old `8080`
  is in the same window.
- **Long gold values never appear verbatim in a transcript**, so recall finds
  them only when an entity named by the value is returned. On v1, objects
  become entity names; on v2 they do not.
- **Stale in budget is not stale at rank 1.** Old values reach the window
  through fragments as well as entities. The old test command
  (`uv run pytest -q`) is also part of the new one, so a fragment that shows
  the new command counts as stale too.
- **The refused-value row reads inverted**: its answer is the value that was
  tried and removed, so yes means memory still offers it. v2 refuses it; v1
  keeps it live.
- On this fixture the 8k and 24k columns agree on every row.

## Silent setting change (eval case)

`tests/fixtures/writes/silent-change/script.toml` is an eval case, not a
close. s1 states a 12-second client timeout and a 50-item batch size; s2
states a 48-second timeout with no replacement cue and without naming the old
value. The gold wants only the new timeout and the batch size live. Its one
current-value question takes its stale value (`12 seconds`) from the
superseded pair. On current main the pipeline is expected to leave the old
value live, so a real run is expected to miss this case. Unit tests keep s2
cue-free and pin the grader on two synthetic stores, a clean close and one
with the old value still live. No run is recorded here.
