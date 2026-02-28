# Contributing

Contributions are welcome. This document explains the workflow.

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
