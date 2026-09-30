# Contributing to ThinkWatch

Thanks for taking the time. This file covers the two things that most
often need a second round on a PR: **which branch to target** and **what
"verified" means here**.

## Open PRs against `dev`

```bash
gh pr create --base dev --head your-branch
```

`main` is a release-only line: branch-protected, linear history, no
force-push, and every commit on it is a release snapshot that gets
tagged immediately. `dev` is where everything routine lands — features,
fixes, refactors, dependency bumps.

GitHub pre-fills a new PR's base with the repo's default branch, which
is `main`, so **the default is not the one you want**. If you've already
opened against `main`, no need to close anything: click *Edit* next to
the PR title and change the base to `dev`. A bot will remind you.

Two exceptions, both maintainer-only: the release PR (`release/X.Y.Z` → `main`,
titled `release: vX.Y.Z`) and a `hotfix/*` branch when `dev` has
diverged too far to carry a fix cleanly. The full branch contract is in
[docs/operations/release.md](docs/operations/release.md).

Automation follows the same rule — Renovate is pinned to `dev` via
`baseBranches` in `renovate.json`.

## Commit messages

Conventional Commits (`fix(scope): subject`), because `git-cliff`
renders the CHANGELOG from them and `chore(release):` is the prefix it
skips. Write them in **English**; this is a public repository and the
history is documentation.

Say *why* in the body, not just *what* — the diff already shows what
changed. A commit that explains the reasoning behind a non-obvious
choice saves the next person from re-deriving it or "fixing" it back.

## What CI will check

```bash
make precommit
```

That is the same gate CI runs, and it exits 0 on a clean tree. Two
required status checks: `Rust Check & Test` and `Frontend Build`.

Warnings are errors — `clippy -D warnings` across the workspace, all
targets. Note that the toolchain is `stable`, so a newer stable than
your local one can surface lints you can't see; `rustup update stable`
before blaming CI.

## Security-relevant changes

If a change touches authentication, the gateway data path, audit rows,
redaction, or anything that handles a key or a token, the PR description
should say how you convinced yourself it doesn't leak a credential or
weaken an existing check. "The tests pass" doesn't answer that — the
tests didn't know about the hole either.

Please don't describe a vulnerability in a public issue, PR, commit or
comment. The organization's
[security policy](https://github.com/ThinkWatchProject/.github/blob/main/SECURITY.md)
explains how to report one privately.
