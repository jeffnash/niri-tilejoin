# niri-tilejoin

`niri-tilejoin` joins two to four physical displays into one logical niri output. It supports both
dual-link tiled panels such as the LG UltraFine 5K and ordinary monitor walls, including outputs on
different DRM devices such as DisplayLink/EVDI.

The project is an out-of-tree **build-time extension**, not a runtime plugin and not a full niri
fork. This repository owns the Rust implementation, pins a compatible upstream niri revision, and
keeps four focused integration patches. The setup assistant assembles and installs a separate
`niri-tilejoin` binary; your distribution's normal `niri` binary remains available for rollback.

## Why this exists

As the owner of an LG 5K display, my 31-year-old eyeballs did not initially notice that it was not
running at full resolution. The moment I realized it was not, however, I began obsessing over that
fact, lest I become like my parents, who cannot tell the difference between 480p and 4K.

I tried two approaches: the first complex, and the second much simpler.

The first was a proxy at the kernel DRM layer, with a userspace relay daemon handling configuration
and startup. It had the benefit of being compositor-agnostic, but that benefit was negated by the
fact that many other non-Wayland compositors already support dual SST. It was also essentially tied
to my particular AMD GPU setup.

Since I cannot envision myself ever switching to anything else, I figured it would be much simpler
to bite the bullet and patch niri. I already have to carry a patch that backports the Smithay bugfix
in commit `298ebc9a` for my portrait displays. I do, in fact, use Arch btw, so patching is really nbd:
it takes me approximately three working days to get my system back to full functionality whenever I
update.

## Setup assistant

Clone this repository and run:

```sh
git clone https://github.com/jeffnash/niri-tilejoin.git
cd niri-tilejoin
./scripts/setup.py DVI-I-1 DVI-I-2 --name portrait-wall
```

The assistant:

1. reads the current output modes, transforms, scale, and physical arrangement from niri;
2. generates explicit, normalized physical-pixel member positions and stable EDID selectors where
   those selectors are unambiguous;
3. clones the pinned niri revision into a content-addressed cache and accepts a cache hit only when
   its patched commit, Git tree, extension digest, and cleanliness all match;
4. applies the adapter commits and injects the extension crate;
5. builds and installs `~/.local/bin/niri-tilejoin`.

It prints the generated configuration but does not edit your config by default. After reviewing it,
install it with validation and timestamped backups:

```sh
./scripts/setup.py DVI-I-1 DVI-I-2 --name portrait-wall --write-config
```

This writes `~/.config/niri/tilejoin.kdl` and adds `include "tilejoin.kdl"` to `config.kdl`. The
candidate is validated with the newly built binary before either file is replaced. The include is
validated at its existing position so include-order semantics do not change. Both files are staged
and synced as one rollback-protected transaction; a failure replacing either restores both prior
files. The assistant does not restart the running compositor; start a test session with
`niri-tilejoin` yourself.

Useful discovery and safety options:

```sh
./scripts/setup.py --list
./scripts/setup.py --diagnose
./scripts/setup.py --dry-run DVI-I-1 DVI-I-2
./scripts/detect-display-group.py DVI-I-1 DVI-I-2
./scripts/setup.py --help
```

With no connector arguments, discovery succeeds only when exactly one compatible touching pair is
present. Explicitly name two to four connectors for larger or ambiguous layouts. Current members
must have one common scale and cover one gap-free rectangle; the helper refuses to guess around
overlap, fractional-pixel boundaries, gaps, or nonrectangular arrangements.

`--diagnose` evaluates every two-to-four-output candidate with the same refresh and geometry policy
used by generation. It reports the concrete refresh, layout, and global selector-ambiguity reason
for each acceptance or rejection.

## Nix

The flake applies the same adapter patches and injects the same extension source:

```sh
nix run github:jeffnash/niri-tilejoin
nix profile install github:jeffnash/niri-tilejoin
```

The installed executable inside the Nix package is named `niri`; the setup assistant deliberately
uses `niri-tilejoin` when installing into `~/.local/bin` so both builds can coexist.

## Manual integration

The exact upstream base is in [`upstream.lock`](./upstream.lock). From a clean checkout of that
revision:

```sh
./scripts/apply-patches.sh --output /tmp/niri-tilejoin-src /path/to/niri
cd /tmp/niri-tilejoin-src
cargo check --all-targets
cargo test --lib
cargo test -p niri-config
cargo test -p niri-tiled
```

The script refuses a different revision or dirty checkout, creates an isolated detached worktree,
applies the four host commits, and copies this repository's `extension/niri-tiled` into that
worktree. The source checkout remains byte-for-byte untouched. `--in-place` exists only for
disposable build trees created by the setup and verification helpers.

`setup.py --source /path/to/niri` also leaves that checkout untouched. It verifies the exact pinned
revision and clean state, creates a detached sibling worktree, and applies the integration there.

## Repository structure

- `extension/niri-tiled/`: owned Rust implementation;
- `integration/niri/patches/`: pinned host adapter, configuration, and documentation commits;
- `scripts/setup.py`: detection, build, installation, validation, backup, and config assistant;
- `scripts/tilejoin_config.py`: reusable output-layout/config generator;
- `scripts/apply-patches.sh`: guarded source-injection helper;
- `scripts/verify-upstream.sh`: clean temporary-worktree update/rebase verification gate;
- `scripts/support-bundle.sh`: privacy-gated hardware and lifecycle evidence collector;
- `flake.nix`: reproducible Nix assembly;
- `docs/adr/`: architectural boundary and tradeoffs.

## Configuration example

```kdl
// Generated by niri-tilejoin; config-schema=1
output "portrait-wall" {
    display-group {
        member "DVI-I-1" {
            mode "3840x2160@60.000"
            transform "270"
            position x=2160 y=0
        }
        member "LG Electronics LG ULTRAFINE 0x01010101" {
            mode "3840x2160@59.997"
            transform "90"
            position x=0 y=0
        }
        primary "DVI-I-1"
        refresh-sync "strict"
        render-policy "auto"
    }
    scale 1.25
}
```

The legacy dual-link form remains available for hardware TILE topology:

```kdl
output "LG UltraFine 5K" {
    tiled-group "DP-4" "DP-5"
    scale 2
}
```

## Compatibility

| niri-tilejoin | niri revision | Smithay revision | Status |
| --- | --- | --- | --- |
| `main` / unreleased | `feb3e43f1475e0865bb89cbd1e898b34d1d2ccf6` | `ff5fa7df392cecfba049ffed55cdaa4e98a8e7ef` | Development and hardware validation |

The table is an exact compatibility contract, not a minimum-version claim. The setup assistant and
Nix flake refuse or avoid unpinned niri sources. The same machine-readable contract is available in
[`compatibility.json`](./compatibility.json). Generated configuration starts with a schema marker so
future setup versions can diagnose migrations without guessing which semantics produced a file.

## Support and release tools

Create a reviewable diagnostic archive with:

```sh
./scripts/support-bundle.sh
```

The default archive excludes configuration and journal logs. Add `--include-config` or
`--include-logs` only after reading the privacy warning, and inspect the archive before sharing it.

Maintainers can validate the pinned adapter from a fresh temporary checkout with
`./scripts/verify-upstream.sh`. Releases use tags shaped like `v0.x.y+niri-feb3e43f`; automation
publishes a deterministic source archive and checksum only when the tag matches the pinned niri
revision. A pin change is incomplete until the software gate and the hardware checklist both pass.

## Support boundary

Tilejoin supports explicit rectangular groups of two to four ordinary monitors, including members
on different DRM devices, in addition to hardware TILE panels. The physical hardware matrix tested
so far is narrower: an LG UltraFine 5K dual-SST panel and a two-monitor EVDI/DisplayLink portrait
wall. Other 2–4-monitor layouts use the same generalized planner and frame state, but have not yet
been exercised across every GPU, driver, and monitor combination. Every upstream niri update
requires rebasing the host adapter and re-running software plus hardware lifecycle tests. See
[`docs/adr/0001-build-time-extension.md`](./docs/adr/0001-build-time-extension.md) for why this is a
build-time extension rather than a dynamically loaded plugin.

Cross-device presentation is best-effort synchronization: each DRM node commits independently, the
logical frame completes only after every submitted member reports completion, and a partial submit
is discarded logically and repaired with a full composited redraw. The configured primary member
supplies the group's stable output identity for its lifetime and is never re-elected during hotplug;
generalized groups report their own logical presentation sequence. The presentation refresh reported
to clients is the slowest selected member refresh.

See [`docs/hardware-validation.md`](./docs/hardware-validation.md) for the lifecycle matrix and
current evidence tiers and [`docs/dependency-audit.md`](./docs/dependency-audit.md) for inherited
RustSec warnings. `CHANGELOG.md`, `CONTRIBUTING.md`, and `SECURITY.md` define release, contribution,
and private-reporting policy.

The project is licensed GPL-3.0-or-later, following niri.
