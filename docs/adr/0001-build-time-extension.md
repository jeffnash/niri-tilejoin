# ADR 0001: Integrate tilejoin as a build-time extension

Status: accepted

## Context

Tilejoin needs access to niri's renderer, DRM surfaces, output lifecycle, event loop, configuration
types, and presentation state. Niri does not currently expose a runtime plugin ABI for any of those
interfaces. A dynamically loaded plugin would either duplicate private state or require a larger,
upstream-designed ABI than this project itself.

Keeping all implementation as patches also made the project difficult to review and maintain: the
domain code was mixed with the lines needed solely to connect it to one niri revision.

## Decision

The project owns the tilejoin implementation in `extension/niri-tiled`. During a build, the setup
script or Nix expression:

1. checks out the exact niri revision in `upstream.lock`;
2. applies the focused adapter/configuration/documentation commits under
   `integration/niri/patches`;
3. copies `extension/niri-tiled` into the checkout as the `niri-tiled` workspace crate;
4. compiles a separate `niri-tilejoin` binary.

The extension uses the host workspace's `niri-config` and `niri-ipc` packages. The adapter remains
responsible for niri-owned renderer, event-loop, and global-output integration; reusable planning,
geometry, KMS, frame, and recovery behavior belongs in the extension crate.

## Consequences

- Users do not need to maintain a long-lived niri fork. They install a reproducible custom binary
  from this repository and can keep their distribution niri for rollback.
- Tilejoin source is ordinary reviewable Rust rather than an opaque generated patch.
- The integration patches are smaller and describe the actual host seam.
- Every upstream update is explicit: update the lock, rebase the adapter commits, and pass CI and
  hardware validation before publishing.
- This remains a rebuild-time extension, not a runtime plugin. A true plugin becomes practical only
  if niri defines and stabilizes an output-backend ABI upstream.
