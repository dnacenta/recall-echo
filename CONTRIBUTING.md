# Contributing

Contributions are welcome. This document explains the workflow.

## Licensing your contribution

recall-echo is [MPL-2.0](LICENSE). By opening a pull request and by
signing off each commit you agree that your contribution is licensed under
MPL-2.0, and that the project may also offer it under other terms — a
commercial licence, a future relicense, or a dual-licensed release.

That second part is the one worth being explicit about. The project has had a
single copyright holder so far, which is what made the move from AGPL-3.0 to
MPL-2.0 possible at all. Once code lands from several people with no shared
understanding about it, nobody can change the terms without tracking every
contributor down. Saying so up front keeps the option open.

You keep the copyright to what you write. If you would rather your
contribution stay MPL-2.0 only, say so in the PR — that is a perfectly
reasonable position, and it is better said before the merge than after.

## Developer Certificate of Origin

Contributions use the [Developer Certificate of Origin](https://developercertificate.org/)
(version 1.1). Adding the sign-off trailer certifies these four clauses:

```
By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```

Every commit needs a `Signed-off-by` trailer whose value is exactly the commit
author, `Name <email>`. The comparison is case-sensitive, including the email.
Other trailers on the same commit are fine.

```
Signed-off-by: Your Name <you@example.com>
```

Add it on every commit:

```
git commit -s
```

An alias such as `git config alias.cs 'commit -s'` makes that `git cs`.

Pull requests are checked: a non-merge commit with no matching trailer fails
the check. To fix a missing sign-off:

- Last commit: `git commit --amend -s --no-edit`
- Several commits: `git rebase --signoff main`, or `git rebase -i` with
  `exec git commit --amend -s --no-edit` on the commits that need it
- Then update the pull request branch with `git push --force-with-lease`

Do not force-push `main`.

## Reporting bugs or requesting features

Open an [issue](https://github.com/dnacenta/recall-echo/issues). Use a clear title and include enough context to reproduce the problem or understand the request.

## Making changes

1. Fork the repo
2. Create a branch from `main` (see naming below)
3. Make your changes
4. Open a PR targeting `main`

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

Issues are expected for features and bugs. Small docs or chore fixes can go straight to a PR.

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
