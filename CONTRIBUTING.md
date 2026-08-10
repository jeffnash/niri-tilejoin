# Contributing

Changes should stay reviewable across the extension and its pinned niri adapter.
By submitting a contribution, you agree to license it under GPL-3.0-or-later, the project's license.

1. Make reusable planning, geometry, state-machine, KMS, and recovery changes in
   `extension/niri-tiled`.
2. Keep niri-owned renderer, event-loop, and output-publication integration in the focused host
   commits under `integration/niri/patches`.
3. Do not edit generated patches by hand. Rebase the corresponding commits on the revision in
   `upstream.lock`, run `git format-patch`, and replace the patch files.
4. Add deterministic tests for every failure transition. Hardware-only behavior must include a
   reproducible validation recipe and logs with connector names, DRM nodes, modes, and driver.
5. Run `./scripts/check-repository.sh` before submitting a change.

The lifecycle invariants are non-negotiable: partial member submission is never a logical
presentation; a CRTC event is authenticated before it drains Smithay state; public output lifetime
ends before failed hardware teardown enters quarantine; and any potentially scanned-out buffer or
gamma blob remains owned until retirement is confirmed. Changes at those boundaries must add a
one-fault-at-every-step test and assert both public-output state and retained-resource ownership.

Use `./scripts/setup.py --diagnose` in user-facing setup reports and
`./scripts/support-bundle.sh` for hardware evidence. Do not claim a GPU, driver, monitor, member
count, or cross-device topology as validated unless the exact lifecycle matrix is attached to a
hardware report.

Commits should each build and should represent one coherent concern. Keep mechanical moves,
behavioral fixes, performance changes, and documentation separate. Do not put a later bug fix on
top of the feature commit that introduced it when preparing release history; amend it into that
feature commit instead.

## Updating niri

Update `upstream.lock`, rebase the four host commits, regenerate the patches, update the
compatibility metadata, regenerate `flake.lock`, and rerun the full software and hardware checklist
in `docs/hardware-validation.md`. `./scripts/verify-upstream.sh` performs the clean software gate and
prints patch-concentration metrics. A release must never use a floating upstream revision.

## Releasing

1. Set the project version in `compatibility.json` and move the relevant `CHANGELOG.md` entries.
2. Run repository, pinned-source, Nix, dependency-audit, and hardware gates.
3. Sign a tag named `vX.Y.Z+niri-<first-eight-niri-revision>` on the verified commit.
4. Push the tag. CI verifies its compatibility suffix and publishes a deterministic source archive
   plus `SHA256SUMS`.
5. Record tested hardware, regressions, patch-size changes, and configuration migrations in the
   release notes.
