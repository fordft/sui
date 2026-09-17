---
name: release-verification
description: Ship-ready checks — does the published artifact install and run? Use for releases, version bumps, packaging, install scripts, and upgrade paths.
cues: [release, ship, publish, version, tag, package, install, upgrade, deploy, artifact, brew, formula]
roles: [lead, auditor]
---

# Release verification

A merged PR or a green CI run is not a shipped product. Verify the
artifact a user would actually receive.

## The candidate, not the source tree

- Build/install from the packaged artifact (release binary, formula,
  installer) — not just `cargo run` in the dev checkout.
- Version reporting matches the tag everywhere: binary `--version`,
  package metadata, installer output.
- Fresh-environment install: missing runtime deps surface as clear
  errors, not crashes or silent skips.

## Upgrade and recovery

- Upgrade from the previous release — config/state migrates or is
  preserved; downgrading doesn't corrupt it.
- Interrupted install/upgrade leaves a recoverable state.
- Uninstall path exists and is honest about what it removes.

## Evidence

- Exact commands run and their output for: install, first launch,
  version check, one real operation.
- Platforms actually tested listed; untested platforms marked unverified.
- Rollback plan for a bad release — what does the user run?

## Do not

- Do not claim "released" from a passing build job alone.
- Do not bump versions in code without verifying the artifact reports it.
