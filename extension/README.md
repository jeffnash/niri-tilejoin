# Tilejoin extension source

`niri-tiled` is the implementation of the tilejoin build-time extension. It owns the reusable
display-group model, planning, geometry, frame aggregation, swapchain, KMS, retry, gamma, and
direct-scanout logic.

The crate deliberately depends on the pinned niri workspace's `niri-config` and `niri-ipc` crates.
It is therefore copied to `niri-tiled/` inside a compatible niri checkout before Cargo resolves the
workspace. This preserves one set of Rust types and avoids vendoring or forking niri's other crates.

Host-specific responsibilities remain in the integration patches:

- registering the crate as a workspace dependency;
- decoding tilejoin configuration in `niri-config`;
- adapting niri's renderer, DRM devices, event loop, outputs, and presentation feedback;
- documenting the additional configuration.

This is not a stable Rust API or a runtime plugin ABI. The exact compatible niri revision is recorded
in `upstream.lock`, and CI proves that the source injection and adapter patches still compile and
test together.
