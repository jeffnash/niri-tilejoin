# Changelog

This project uses semantic version tags once hardware validation is complete. Until the first
tagged release, `main` is an integration branch and may be rebased while the adapter is developed.

## Unreleased

- Generalized display groups for two to four outputs, including cross-DRM-device groups.
- Native dual-SST TILE joining for panels such as the LG UltraFine 5K.
- Partial-damage composition and conservative all-member direct scanout.
- Setup assistant with topology detection, configuration validation, backups, and rollback.
- Persistent CRTC event generations, partial-submission recovery, and failed-disable quarantine.
- Versioned generated configuration, candidate diagnostics, privacy-reviewed support bundles, and
  a clean temporary-worktree upstream verification command.
- Locked dual-architecture Nix builds, dependency auditing, compatibility metadata, and
  deterministic release archives with checksums.
