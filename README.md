# gpui-webview

Pooled, clipped **live web surfaces** for GPUI applications: WKWebView on
macOS, WebView2 (planned) on Windows, WebKitGTK + a nested Wayland compositor
(planned) on Linux, and a DOM `<iframe>` overlay on wasm/web. It knows nothing
about any application's documents — surfaces are keyed by opaque `u64`s and
content is a URL, HTML bytes, or a host-provided local source.

This crate is the platform layer behind the Notesnook spike's embed implementation
(`apps/desktop-gpui-spike/EMBEDS.md`, §7–§15). It lives inside that workspace
while it incubates; it has **no dependency on the editor or on Notesnook**, and
a move to its own repository needs no code change.

## Why a pool

A web engine is expensive (tens of MB, ~100 ms to create), and on every desktop
platform the live view sits in the platform's own layer stack, where GPUI cannot
freely clip it or draw over it. So:

- at most `Config::capacity` engines stay alive (1 by default, 2 is the
  practical desktop maximum);
- an idle engine is **reused by navigation, never recreated**;
- a surface is **clipped to the viewport** (the page never relayouts while
  scrolling — only the visible window changes);
- an intersecting GPUI overlay **lowers** the surface (`OverlayBehavior::Lower`)
  or **freezes** it back to the application's poster (`Freeze`);
- a surface that scrolls fully out is **torn down**.

Applications render a cheap native poster by default and activate a live
surface on demand. `Error::Unsupported` is returned — never a panic — when the
target, session, or runtime cannot host a surface, so the poster + `openUrl`
fallback always exists.

## Features

| Feature          | Target        | Backend                                                       |
|------------------|---------------|---------------------------------------------------------------|
| `mock` (default) | any           | recording backend: no engine, every call recorded             |
| `web`            | wasm32        | clip `<div>` + `<iframe>` over GPUI's canvas                  |
| `wry`            | macOS         | `WKWebView` in a layer-backed clip container under GPUI's view |
| `nested`         | Linux         | in-process nested Wayland compositor (smithay) + GTK/WebKitGTK client |
| `wry`            | Windows       | reports `Error::Platform` (not implemented)                   |
| `window`         | any           | separate-window/fullscreen presentation (not wired yet)        |

`--no-default-features` builds the interface only (specs, manager, backend
trait) with no platform code at all, which is what an application's
platform-independent core should depend on.

## Status

- **macOS (`wry`)**: implemented. `WKWebView` in a `masksToBounds` container;
  script/incognito/navigation policies from `Policy`, and the engine toggles
  from `Policy::engine` (`with_devtools` follows `engine.devtools`); overlays
  freeze. Compile verified; the manual GUI checklist (below) is still required.
- **wasm (`web`)**: implemented. Compile verified; the per-frame placement and
  z-index assertions live in the host's page tests.
- **Linux (`nested`)**: implemented and **runtime-verified headless**. The
  compositor runs on its own thread (a same-thread design deadlocks in
  `gtk_init`); each surface's GTK/WebKitGTK window is an `xdg_toplevel`
  configured to the block's size, its commits become `SurfaceFrame`s the app
  paints (cropped to `set_window_geometry`, so decoration shadows never reach
  the block), and GPUI input is forwarded as `wl_seat` events. Two transports,
  the second behind the `dma-buf` feature + patched GPUI crates:
  `linux-dmabuf` when the renderer's Vulkan driver can import the buffer
  ([`SurfaceContent::Dmabuf`] → an imported GPU texture, no CPU copy) and
  `wl_shm` everywhere else (a CPU copy per commit). The compositor advertises
  exactly the renderer's importable formats, so a client never sends a buffer
  the app cannot sample. `cargo test -p gpui-webview --features nested` runs a
  real end-to-end test (`tests/nested_e2e.rs`) that needs no display; it passes
  in a bare container (shm) and asserts the imported pixels on a GPU machine
  (`E2E_DMABUF=1`).
  Limits: no `wl_data_device` clipboard bridge, no IME (`zwp_text_input_v3`)
  and no touch (`wl_touch`) forwarding yet; CSS cursor images
  (`wl_pointer.set_cursor`) need a GPUI custom-cursor API. The dma-buf
  transport is behind the `dma-buf` feature and needs the patched GPUI crates
  (`scripts/gpui-patch.sh`); without that setup the backend runs `wl_shm`.
- **Windows**: not started.
- **GPUI patches** (`dma-buf` external textures): required by the Linux
  dma-buf transport only, gated behind the `dma-buf` feature and set up by
  `scripts/gpui-patch.sh`; documented in EMBEDS.md §14. Every backend works
  without them on `wl_shm`, and vanilla GPUI builds are unaffected.

## Manual checklist (per backend)

Create → place → clip at the viewport → scroll a clipped surface → overlay a
menu → teardown → RSS back to baseline → second activation reuses the engine
(no extra process) at 1x and 2x scale. Security: a clip with `<script>` runs
nothing, and links leave through `OpenExternal`.

## Example

```sh
cargo run -p gpui-webview --example demo                       # 6 posters
cargo run -p gpui-webview --features wry --example demo -- 20  # live on macOS
```

## Testing

```sh
cargo test -p gpui-webview                       # policy + manager contract
cargo test -p gpui-webview --features wry        # + the native backend compiles
cargo build -p gpui-webview --no-default-features
cargo tree -p gpui-webview                       # no editor/Notesnook dependency
```
