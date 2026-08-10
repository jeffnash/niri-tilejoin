# Dependency audit policy

CI runs `cargo audit` against the exact post-patch `Cargo.lock`. Vulnerability findings fail the
build; RustSec informational warnings remain visible but do not fail until an upstream-compatible
replacement exists.

The audited lock refresh currently removes the actionable `crossbeam-epoch` and `quick-xml`
vulnerabilities reported against the pinned niri tree. RustSec still reports these inherited
warnings:

- `cgmath` (`RUSTSEC-2026-0196`, `RUSTSEC-2026-0197`) through pinned Smithay;
- `paste` (`RUSTSEC-2024-0436`) through Smithay's pixman dependency;
- `proc-macro-error` (`RUSTSEC-2024-0370`) through niri-config's knuffel derive;
- `anyhow` (`RUSTSEC-2026-0190`), with no newer compatible release at the time of audit;
- `event-listener` (`RUSTSEC-2026-0221`) through zbus/accessibility dependencies;
- `memmap2` (`RUSTSEC-2026-0186`) through Smithay/winit/xkbcommon.

These are not silently allow-listed: each CI run prints them from the current advisory database.
Pin updates must retry compatible dependency upgrades and update this list. A warning becomes a
release blocker if tilejoin or its host exercises the affected API, if RustSec promotes it to a
vulnerability, or when a compatible fixed version is available.
