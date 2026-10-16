# Troubleshooting

Start with `recall-echo status` and `recall-echo graph daemon status`; the second says
whether background extraction is on, what it has done and, when it is off, why. The
daemon logs to `memory/graph/daemon.log`, and every failure names itself there.

- **`store locked`**: another process holds the embedded store. The daemon retries, then
  gives up with this error rather than a panic. Stop the other process, or switch to
  `[graph] mode = "server"` to share a SurrealDB server.
- **Nothing gets extracted**: the daemon never sees API keys, so with `anthropic` or
  `openai` it disables itself and says so once in the log. Use a CLI provider, or run
  `recall-echo graph extract --all` yourself with the key exported.
- **An archive keeps failing**: after two failures it goes to
  `graph/extraction-quarantine.txt` and is skipped, so it costs nothing more.
- **Episodes with no extracted flag** in `graph status`: run `recall-echo graph migrate --force`.
