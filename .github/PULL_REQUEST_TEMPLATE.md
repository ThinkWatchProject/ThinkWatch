<!--
Base branch: `dev`, not `main`.

GitHub pre-fills the base with this repo's default branch (`main`),
which is the release-only line — protected, linear, and tagged on every
commit. A PR opened against it will be asked to retarget. Use the "Edit"
button next to the title to switch the base to `dev`; with the CLI, pass
`--base dev`.

The only PRs that belong on `main` are the release PR (`release/X.Y.Z` -> `main`,
titled `release: vX.Y.Z`) and a `hotfix/*` branch. See
docs/operations/release.md for the branch contract.
-->

## What this changes

<!-- One or two sentences. What behavior is different after this merges? -->

## Why

<!-- The problem, not the patch. If it fixes an issue, link it. -->

## How it was verified

<!--
What you actually ran, and what it printed. "cargo test passes" is
weaker than "the 3 new tests in crates/auth/src/oidc.rs cover the
multi-audience case; full workspace suite green".

If it touches auth, the gateway data path, audit rows, or anything that
handles a key or a token, say what you did to convince yourself it
doesn't leak or weaken a check.
-->

## Notes for review

<!-- Anything you're unsure about, deliberately left out, or want argued with. -->
