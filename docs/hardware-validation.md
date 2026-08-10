# Hardware validation

Record the niri-tilejoin revision, pinned niri revision, kernel, Smithay revision, GPU/driver,
connector topology, member modes, scale, and refresh policy for every run.

Run `scripts/support-bundle.sh` after the scenario. Configuration and journal logs are opt-in;
review the bundle's privacy notice and every collected file before attaching it to a report.

## Required lifecycle matrix

- Cold start and hotplug, including members appearing several seconds apart.
- DPMS off/on while idle and with a frame pending.
- VT switch and suspend/resume with connector cleanup success and injected failure.
- Unplug each member independently, then unplug the whole group.
- Replug with connector numbering or member discovery order changed.
- Config reload changing only scale, member transform, mode, or primary role.
- Inject preparation, first/middle/last member submission, watchdog, disable, gamma allocation,
  gamma commit, and gamma rollback failures.
- Static desktop, cursor-only damage, a window crossing every seam, animation, screen capture, and
  fullscreen direct-to-composited transitions.

Verify that public output withdrawal is immediate on teardown, member CRTCs cannot be reused while
quarantined, partial submissions never produce logical presentation feedback, stale events never
retire a newer compositor frame, and retained framebuffers/property blobs do not grow over repeated
cycles.

## Current evidence tiers

| Topology | Status |
| --- | --- |
| LG UltraFine 5K dual-SST TILE panel | Hardware-tested target |
| Two portrait DisplayLink/EVDI outputs | Hardware-tested target |
| Other rectangular two-to-four-output groups | Software-supported; hardware reports welcome |
| Nonrectangular, overlapping, or negative-origin layouts | Rejected |

Cross-device presentation is best-effort synchronization: each DRM node commits independently, the
logical frame completes only after all submitted members report completion, and a partial submit is
discarded logically and repaired with a full composited redraw. The configured primary member
supplies the group's stable output identity for its lifetime; it is never re-elected during hotplug,
and generalized groups report their own logical presentation sequence. The reported presentation
refresh is the slowest selected member refresh.
