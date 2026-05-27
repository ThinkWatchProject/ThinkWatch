# Release runbook

How to cut a release of ThinkWatch from working in `dev` to a tagged
artifact on `main` with images on GHCR and a chart on the GitHub
Release page. The whole thing is ~8 git commands; this file exists so
nobody (including future-you) has to remember the order.

## Branch contract

- **`dev`** — default branch. Everything routine lands here: feature
  work, fixes, Renovate dep bumps, refactors. CI gates on every push
  (`Rust Check & Test` + `Frontend Build`); the docker-image jobs in
  `ci.yml` are deliberately scoped to `main` so dev pushes stay fast.
- **`main`** — release-only. Branch-protected on GitHub: requires PR,
  requires the two CI status checks, requires linear history, no
  force-push, no deletion. Every commit on `main` is a release
  snapshot tagged immediately after merge.

Don't push directly to `main` — the protection will reject it and
the wider tooling (release workflow, image `:latest` semantics,
CHANGELOG link refs) assumes the branch is monotonic.

## Versioning rules

SemVer applies from `v1.0.0` onwards. Pick the bump based on the
diff `v(previous)..dev`:

| Bump | Triggers | Examples |
|---|---|---|
| **Major** `X.0.0` | Breaking change to the committed surface: REST routes, MCP wire shapes, audit-row JSON keys, DB schema, public Rust APIs in published crates, Helm values keys, `dynamic_config` setting keys, env var names, the `:latest` Docker tag contract | Renaming `/api/admin/users` → `/api/v2/admin/users`; dropping a column from `gateway_logs`; removing a `dynamic_config` key |
| **Minor** `1.X.0` | New feature, additive, fully backward-compatible | New REST endpoint, new MCP tool, new optional setting |
| **Patch** `1.0.X` | Bug fix, no API change; or build-pipeline tweaks that don't change the runtime binary | Crash fix, perf improvement, dep bump, release workflow rewrite |

When in doubt, lean toward the higher bump. The pain of a "should
have been minor" patch is much smaller than the pain of a hidden
breaking change inside a patch.

## The release flow

### 1. Make sure `dev` is ready

```bash
git checkout dev
git pull
make precommit                    # exits 0 on a clean tree
```

A red precommit blocks the release — fix it on `dev` first.

### 2. Render the CHANGELOG entry

```bash
make changelog VERSION=1.0.2 WRITE=1
```

This invokes `git-cliff` (installed via `make tools`) against
Conventional Commits since the last `v*` tag, prepending a new
`## [1.0.2] — YYYY-MM-DD` section under `## [Unreleased]` in
`CHANGELOG.md`.

**Always hand-review the generated section.** git-cliff has no
context for *why* something matters to operators — it only sees
the commit subject. Rewrite for the audience: drop noise, promote
the operationally-significant items, add a short paragraph at the
top of the section if the release has a theme.

If the generated section is empty (e.g., the only commits since
the last tag are `chore(release)` / `ci:` / `docs:` skips), write
the section by hand. The CHANGELOG entry is mandatory — the
release workflow refuses to publish without one.

### 3. Bump the three version pins

Edit by hand or `sed`-replace `<previous>` → `<new>`:

- `Cargo.toml` — `[workspace.package].version`
- `web/package.json` — top-level `"version"`
- `deploy/helm/think-watch/Chart.yaml` — both `version:` AND `appVersion:`

`make precommit` once more after editing — catches obvious typos.

### 4. Commit on `dev`

```bash
git add CHANGELOG.md Cargo.toml web/package.json deploy/helm/think-watch/Chart.yaml
git commit -m "chore(release): tag X.Y.Z"
git push origin dev
```

The `chore(release):` prefix is what `cliff.toml` skips when
rendering the NEXT release's CHANGELOG. Don't deviate from that
prefix.

### 5. PR `dev` → `main`

```bash
gh pr create --base main --head dev \
  --title "release: vX.Y.Z" \
  --body "See CHANGELOG.md [X.Y.Z] for the full notes."
```

Wait for CI to go green (`Rust Check & Test` + `Frontend Build`).
The PR description is internal — the user-facing release notes
live in CHANGELOG.md and are extracted into the GitHub Release
body automatically. Don't duplicate them.

### 6. Squash-merge the PR

The branch protection requires linear history, so merge mode is
forced to squash or rebase. Squash is the default and the right
choice — every `dev`-side commit collapses into a single
`release: vX.Y.Z` commit on `main`. Use the PR title as the
commit subject; the auto-generated commit list goes in the body.

### 7. Tag the merge commit on `main`

```bash
git checkout main && git pull
git tag -a vX.Y.Z -m "ThinkWatch X.Y.Z

$(awk '/^## \[X\.Y\.Z\]/{f=1;next} /^## \[/{f=0} f' CHANGELOG.md)"
git push origin vX.Y.Z
```

The annotated tag's message gets attached to the GitHub Release
under the auto-extracted CHANGELOG body. Keeping the tag message
in sync with the CHANGELOG section is convention; the workflow
doesn't enforce it.

### 8. Watch the release workflow

```bash
gh run watch --repo ThinkWatchProject/ThinkWatch
```

Four parallel-ish jobs fire on tag push:

- `Build · server · linux/amd64` (ubuntu-latest, ~12 min)
- `Build · server · linux/arm64` (ubuntu-24.04-arm, ~10 min)
- `Build · web · {amd64,arm64}` (~1-2 min each)
- `Helm chart` (~6 s)

Then `Manifest · {server,web}` glue per-platform digests into
`:vX.Y.Z` + `:latest` (stable releases only), and `GitHub Release`
extracts the CHANGELOG section, prepends image + helm install
copy-paste blocks, and attaches the chart `.tgz` to a new Release
page.

Total wall-clock: ~13 min for a typical release.

## Pre-release tags

For `1.0.0-rc.1`, `1.1.0-beta.2`, `2.0.0-alpha.5`:

- The workflow's prerelease detector recognises `-rc.`, `-beta.`,
  `-alpha.` suffixes (and `v0.*` for the pre-1.0 era). The Docker
  images are NOT tagged `:latest` and the Release page is marked
  pre-release.
- Pin the chart's `appVersion` to the same tag string including
  the suffix; operators tracking pre-releases pull the exact
  version, not `:latest`.
- The CHANGELOG section header still uses `## [1.0.0-rc.1]`. Move
  the body content into `## [1.0.0]` when promoting; don't leave
  duplicate sections.

## Hotfix on main without going through dev

Don't, unless the dev branch has diverged so far from main that a
PR would carry unrelated changes. The branch protection still
requires a PR, so the procedure is:

```bash
git checkout -b hotfix/X.Y.Z+1 main
# ... fix, commit, run precommit ...
gh pr create --base main --head hotfix/X.Y.Z+1 --title "hotfix: vX.Y.Z+1"
# ... merge, tag, push tag ...
```

Then immediately `git checkout dev && git merge main` to re-sync
dev so the next normal release doesn't accidentally revert the
hotfix.

## Renovate / dep-bump PRs

Renovate opens PRs against `dev` (the default branch). The PRs
are auto-rebased on conflict, batched weekly on Sunday, and ride
the same CI gate as a human PR. Two policies live in `renovate.json`:

- Crypto crates (`jsonwebtoken`, `argon2`, `aes-gcm`, …) are
  pinned via `=X.Y.Z` in `Cargo.toml`. Renovate gates these
  behind `dependencyDashboardApproval: true` — they only open a
  PR after you tick them in the Dashboard issue. Review each one
  by hand against the upstream release notes.
- Major bumps for non-crypto deps also require dashboard approval.
  Patch + minor flow automatically (still no automerge — the
  human still merges).

When a Renovate PR is in your release range, the commit shows up
in `make changelog` output under `Changed` because the
`chore(deps): ...` prefix maps there. Edit it if the message
isn't operator-friendly.

## When release.yml fails halfway

The four image jobs are independent (`fail-fast: false`). A
re-run from the failed job is usually safe:

```bash
gh run list --workflow=release.yml --limit 3
gh run rerun <run-id> --failed
```

Caveats:

- **`Manifest` job failure** — the per-platform digests are in
  workflow artifacts (1-day retention). Rerunning within the
  retention window works; later you have to re-tag.
- **`GitHub Release` job failure** — `softprops/action-gh-release`
  is idempotent and updates an existing release. Safe to rerun.
- **Tag-immutability gotcha** — you cannot re-push a `vX.Y.Z`
  tag with different content. If the release was published with
  bad CHANGELOG / wrong image / etc., bump the patch
  (`vX.Y.Z+1`) — re-tagging an existing version is a worse
  cure than the disease.

## Quick reference

```bash
# Release X.Y.Z, full flow (~15 min including ~13 min workflow):
make precommit                            # green
make changelog VERSION=X.Y.Z WRITE=1
$EDITOR CHANGELOG.md                      # review + polish
$EDITOR Cargo.toml web/package.json deploy/helm/think-watch/Chart.yaml
make precommit                            # green again
git commit -am "chore(release): tag X.Y.Z" && git push origin dev
gh pr create --base main --head dev --title "release: vX.Y.Z"
gh pr merge --squash --auto                # waits for CI
git checkout main && git pull
git tag -a vX.Y.Z -m "ThinkWatch X.Y.Z" && git push origin vX.Y.Z
gh run watch                              # ~13 min
```

`v1.0.1` was the first release that exercised this entire flow
end-to-end; if you hit a step that doesn't match what's documented
here, the doc is wrong and should be corrected.
