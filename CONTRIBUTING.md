# Contributing

Contributions are welcome. This document explains the workflow.

## Licensing your contribution

recall-echo is [MPL-2.0](LICENSE). By opening a pull request you agree that
your contribution is licensed under MPL-2.0, and that the project may also
offer it under other terms — a commercial licence, a future relicense, or a
dual-licensed release.

That second part is the one worth being explicit about. The project has had a
single copyright holder so far, which is what made the move from AGPL-3.0 to
MPL-2.0 possible at all. Once code lands from several people with no shared
understanding about it, nobody can change the terms without tracking every
contributor down. Saying so up front keeps the option open without a signed
CLA, a bot, or any paperwork on your side.

You keep the copyright to what you write. If you would rather your
contribution stay MPL-2.0 only, say so in the PR — that is a perfectly
reasonable position, and it is better said before the merge than after.

## Reporting bugs or requesting features

Open an [issue](https://github.com/dnacenta/recall-echo/issues). Use a clear title and include enough context to reproduce the problem or understand the request.

## Making changes

1. Fork the repo
2. Create a branch from `development` (see naming below)
3. Make your changes
4. Open a PR targeting `development`

`main` is protected. All changes go through `development` first.

## Branch naming

Branches follow this pattern:

```
<type>/<issue-number>-<short-description>
```

| Type       | When to use                          | Example                              |
|------------|--------------------------------------|--------------------------------------|
| `feat`     | New functionality                    | `feat/5-add-topic-files`             |
| `fix`      | Bug fix                              | `fix/3-precompact-hook-merge`        |
| `refactor` | Code restructure, no behavior change | `refactor/8-simplify-init`           |
| `docs`     | Documentation only                   | `docs/2-usage-examples`              |
| `chore`    | Maintenance, deps, CI                | `chore/10-update-dependencies`       |

If there's no issue yet, create one first so there's a number to reference.

## Commit messages

This project uses [Conventional Commits](https://www.conventionalcommits.org/) (lowercase):

```
<type>(<scope>): <description>
```

Examples:

```
fix(init): prevent overwrite of existing memory files
feat(protocol): add topic file distillation rules
docs: add installation examples
refactor(install): split bash and npx paths
```

Rules:
- Lowercase everything
- Imperative, present tense ("add" not "added")
- No period at the end
- Reference the issue in the body or footer: `Closes #7`

## Pull request titles

PR titles follow the same convention, referencing the issue number as scope:

```
fix(#3): prevent precompact hook merge failure
feat(#5): add topic file support
docs(#2): expand usage examples
```

## Code style

- Run `cargo fmt && cargo clippy && cargo test` before submitting — no warnings or test failures
- Keep changes focused — one issue per PR
