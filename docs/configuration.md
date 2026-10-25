# Configuration

`.recall-echo.toml` lives in the memory directory (`{pulse_root}/memory/`). Every key is
optional; a missing file or an invalid value falls back to the default, silently.

```toml
[ephemeral]
max_entries = 5               # rolling window of session summaries, 1-50

[llm]
provider = "anthropic"        # default when unset; `init` writes the CLI it found
model = ""                    # empty = the provider's default
api_base = ""                 # HTTP providers only

[llm.cli]                     # CLI providers; each key defaults to the preset
# command = "mycli"           # binary name or path
# args = ["chat"]             # fixed args before the generated flags
# prompt_delivery = "flag"    # stdin | flag | arg
# prompt_flag = "--ask"       # used when prompt_delivery = "flag"
# model_flag = "--model"      # omitted when empty, or when no model is set
# output_format_flag = "--format"   # passed alone when the value is empty
# output_format_value = "json"
# system_prompt_flag = ""     # empty prepends the system prompt to the message
# output_mode = "single-json" # raw | single-json | ndjson
# result_json_path = "data.text"    # dotted path; empty = stdout is the answer
# ndjson_match = []           # path=value line selectors, ndjson mode only
# usage_input_path = ""       # where the CLI reports token counts; empty = estimated
# usage_output_path = ""
# extra_args = ["--quiet"]    # appended after the generated flags
timeout_secs = 300            # per call; 0 waits forever

[graph]
mode = "embedded"             # embedded (single-process SurrealKV) | server (shared SurrealDB)
url = "ws://localhost:8787"   # server mode only

[graph.dedup]
certain_similarity = 0.92     # at or above: same entity, no model call
review_similarity = 0.82      # below: new entity, no model call
max_candidates = 3            # existing entities compared per candidate

[graph.provenance]
weight_external = 1.0         # independent sources
weight_user = 0.8             # the human, and `graph correct --wrong`
weight_assistant = 0.3        # an assistant turn
weight_self = 0.05            # the agent restating its own belief

[serve]
socket_path = ""              # default: $XDG_RUNTIME_DIR/recall-echo/
idle_timeout_secs = 3600      # daemon exits after this much inactivity; 0 = never

[extraction]
background_enabled = true     # let the daemon extract when the machine is quiet
idle_after_secs = 120         # quiet period before a batch
batch_size = 3                # archives per batch

[capture]
enabled = true                # let the daemon import transcripts from hook-less CLIs
# sources = ["claude-code", "codex", "grok"]   # default: every CLI with sessions here
settle_secs = 300             # a transcript untouched this long counts as finished

[pipeline]                    # pulse-null stack only
docs_dir = ""                 # LEARNING.md, THOUGHTS.md, CURIOSITY.md, REFLECTIONS.md, PRAXIS.md
auto_sync = false             # sync them into the graph on every archive
```

`ephemeral.max_entries` also sets how much the SessionStart hook prints. Claude Code
inlines hook output up to about 10,000 characters, so keep the window well under that.
At about 400 characters per entry, the default 5 entries come to roughly 2 KB.

`recall-echo config show` prints the effective settings and the exact command a CLI
provider will run. `recall-echo config set KEY VALUE` changes one (`provider`, `model`,
`api_base`, `llm.cli.*`, `ephemeral.max_entries`, `graph.provenance.*`, …).

## Providers

| `provider` | How it runs | What it costs |
|---|---|---|
| `claude-code` | spawns `claude` | Claude subscription |
| `gemini` | spawns `gemini` | Gemini subscription or free tier |
| `grok` | spawns `grok` | Grok subscription |
| `codex` | spawns `codex exec` | ChatGPT/Codex subscription |
| `cli` | spawns whatever `[llm.cli]` describes | whatever that CLI costs |
| `ollama` | HTTP to `localhost:11434/v1` | free, fully local |
| `openai` | HTTP to any OpenAI-compatible endpoint | per token |
| `anthropic` | HTTP to the Anthropic API | per token |

Embeddings are always local (fastembed/ONNX, no network after the model downloads once),
so semantic search, HNSW indexing and traversal cost nothing whatever this is set to.

The five CLI providers are one implementation. A provider name selects a preset, a set
of defaults for the `[llm.cli]` keys, and every key can be overridden, so a CLI with no
preset is configuration rather than a new release. The presets, exactly as they are
called (`<prompt>` is the message, `<system>` the system prompt, `<` means stdin):

```text
claude-code  claude -p --model <M> --output-format text --system-prompt <system> \
                    --no-session-persistence  < <prompt>
gemini       gemini -m <M> -o json -p "<system>\n\n<prompt>"
grok         grok -m <M> --output-format json -p "<system>\n\n<prompt>"
codex        codex exec -m <M> --json --skip-git-repo-check  < "<system>\n\n<prompt>"
```

Only `claude` takes the system prompt as a flag; the others get it prepended. The model
flag is dropped when no model is configured, so each CLI uses its own default. Point at a
binary somewhere unusual with `CLAUDE_BIN`, `GEMINI_BIN`, `GROK_BIN`, `CODEX_BIN`,
`RECALL_CLI_BIN` or `[llm.cli] command`.

Output shapes differ, so `output_mode` is part of the config: `claude` prints prose
(`raw`), `gemini` and `grok` print one JSON document (`single-json` plus
`result_json_path`), and `codex --json` prints one event per line (`ndjson`), where the
answer is the last event matching `type=item.completed` and `item.type=agent_message`:

```toml
[llm.cli]
output_mode = "ndjson"
ndjson_match = ["type=item.completed", "item.type=agent_message"]
result_json_path = "item.text"
```

Output handling is forgiving: JSON that does not parse, a missing path or a non-string
value falls back to the CLI's stdout verbatim. A non-zero exit is an error carrying the
CLI's stderr, and so is a JSON envelope that reports its own failure while exiting zero.

## Caveats

- **codex**: `--skip-git-repo-check` is required, because codex refuses to run outside a
  trusted git directory and a memory directory usually is not one. Its `-p` is
  `--profile`, not the prompt, so the prompt goes in on stdin.
- **gemini's result field is unverified.** The flags were checked against `gemini` 0.27.3,
  but no authenticated call was made, so the preset tries `response`, then `result`, then
  raw stdout. If yours wraps the answer elsewhere, set `result_json_path`, or clear
  `output_format_flag` to take plain text.
- **Token counts are measured where the CLI reports them and estimated where it does
  not.** `codex`, `grok` and both HTTP APIs report usage, so `graph extract` quotes their
  numbers ("14K measured"). `claude-code` answers in prose with no envelope, so its calls
  are a length heuristic labelled `~ estimated`; a mixed run prints both rather than
  adding a guess to a measurement. Any CLI can be measured by setting `usage_input_path`
  and `usage_output_path`.
- **The background daemon only sees file-based CLI auth.** It starts with an allowlisted
  environment (`PATH`, `HOME`, …) that excludes API keys. See
  [internals: background work](internals.md#background-work).
