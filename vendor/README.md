# Patched copies of GPUI crates

`gpui-pre-macos` and `gpui-pre-apple` 0.3.12 (Apache-2.0, a snapshot of `gpui` from zed-industries/zed), wired in
through `[patch.crates-io]` in `Cargo.toml`. There is one change: how macOS tells the app about a new frame can be
chosen.

- `gpui-pre-macos/src/window.rs`: `GPUI_FRAME_SOURCE=cvdisplaylink` drives frames from `CVDisplayLink` (what GPUI did
  before macOS 14) instead of `CAMetalDisplayLink`.
- `gpui-pre-apple/src/display_link.rs`: `GPUI_FRAME_RATE=<frames per second>` sets `preferredFrameRateRange` for
  `CAMetalDisplayLink`; the crate's `Cargo.toml` gets the `CAFrameRateRange` feature.

Pult sets the variables itself (`choose_frame_source` in `src/main.rs`): `cvdisplaylink` in a virtual machine.

Why: in a virtual machine `CAMetalDisplayLink` gives about 12 frames per second, and a typed letter appears after
about 80 ms; with `CVDisplayLink` after about 18 ms (the `trace-input.log` journal, turned on by a `trace-input` file
in Pult's data folder).

Updating GPUI Kit: if it pulls another version of `gpui-pre-*`, the `[patch]` silently stops applying (cargo warns
"patch was not used"). Then copy the new versions from `~/.cargo/registry/src` and make the change again (search for
`Pult:` in the two files).
