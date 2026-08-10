# Source provenance and licensing

The standalone extension and integration helpers are licensed under GPL-3.0-or-later. The adapter
patches modify niri and remain under niri's GPL-3.0-or-later license. Their base revision and source
repository are recorded exactly in `upstream.lock`; patch authorship and commit messages are
preserved by `git format-patch`.

Generated archives, build caches, binaries, dependency sources, and upstream Git metadata are not
committed to this repository. Files owned by this project carry SPDX identifiers where the format
supports comments.

CI verifies file-level SPDX identifiers and audits the pinned Cargo dependency graph against the
RustSec advisory database. Machine-readable compatibility metadata is validated against
`upstream.lock`; release archives are generated from the tagged Git tree with normalized gzip
timestamps and published with SHA-256 checksums.

The Nix assembly imports `integration/niri/Cargo.lock`, a byte-for-byte copy of the lock produced
by the published dependency patch. Nix resolves Cargo dependencies before applying source patches;
CI builds both declared architectures and fails if the copied lock drifts from what the compiler
uses.
