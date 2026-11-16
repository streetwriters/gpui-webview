# gpui-webview

[![Tests](https://github.com/streetwriters/gpui-webview/actions/workflows/tests.yml/badge.svg)](https://github.com/streetwriters/gpui-webview/actions/workflows/tests.yml)

Pooled, clipped **live web surfaces** for [GPUI](https://www.gpui.rs)
applications: WKWebView on macOS, WebView2 on Windows, WebKitGTK inside an
in-process nested Wayland compositor on Linux, and a DOM `<iframe>` overlay on
wasm. The crate knows nothing about your documents. Surfaces are keyed by
opaque `u64`s, and their content is a URL or HTML bytes.

## Why a pool

A web engine is expensive: tens of MB of memory and about 100 ms to create.
On every desktop platform the live view also sits in the platform's own layer
stack, where GPUI can't freely clip it or draw over it. So the crate manages
engines like this:

- At most `Config::capacity` engines stay alive. The default is 1, and 2 is
  the practical maximum on desktop.
- An idle engine is **reused by navigating it, never recreated**.
- A surface is **clipped to the viewport**. The page never relayouts while
  scrolling; only the visible part changes.
- When a GPUI overlay intersects a surface, the surface is either **lowered**
  (`OverlayBehavior::Lower`) or **frozen** back to the application's poster
  (`Freeze`).
- A surface that scrolls fully out of view is **torn down**.

Applications render a cheap native poster by default and activate a live
surface on demand. When the target, session or runtime can't host a surface,
the crate returns `Error::Unsupported` instead of panicking, so the
application can always fall back to the poster and opening the URL.

## Features

| Feature          | Target  | What it enables                                                         |
|------------------|---------|-------------------------------------------------------------------------|
| `mock` (default) | any     | Recording backend with no engine; every call is recorded                |
| `web`            | wasm32  | A clip `<div>` + `<iframe>` stacked over GPUI's canvas                  |
| `wry`            | macOS   | `WKWebView` in a layer-backed clip container under GPUI's view          |
| `wry`            | Windows | A WebView2 child HWND inside a clip-container HWND                      |
| `nested`         | Linux   | Nested Wayland compositor (smithay) hosting a GTK/WebKitGTK client      |
| `dma-buf`        | Linux   | Imports committed dma-bufs as GPU textures instead of copying `wl_shm`  |
| `window`         | any     | Separate-window/fullscreen presentation (not wired yet)                 |
| `test-support`   | any     | Hooks used by the platform end-to-end tests (e.g. `page_snapshot`)      |

`--no-default-features` builds only the interface (specs, manager and backend
trait) with no platform code. Depend on it this way from an application's
platform-independent core.

## Platform status

- **macOS (`wry`)**: `WKWebView` in a `masksToBounds` container. Script,
  incognito and navigation policies come from `Policy`, and engine toggles
  come from `Policy::engine` (`with_devtools` follows `engine.devtools`).
  Overlays freeze the surface.
- **Windows (`wry`)**: a WebView2 child HWND inside a clip-container HWND. The
  container is the visible clip, and the webview keeps its full size and
  slides inside it, so scrolling never relayouts the page. GPUI must present
  through a flip-model swapchain (`prepare_process`); otherwise its
  DirectComposition target draws above every child HWND. Overlays freeze the
  surface.
- **Linux (`nested`)**: the compositor runs on its own thread, because a
  same-thread design deadlocks in `gtk_init`. Each surface's GTK/WebKitGTK
  window is an `xdg_toplevel` sized to its block, and every commit becomes a
  `SurfaceFrame` that the app paints. Frames are cropped to
  `set_window_geometry`, so decoration shadows never reach the block. GPUI
  input is forwarded as `wl_seat` events.
  - **Frame transports**: with the `dma-buf` feature, buffers the renderer's
    Vulkan driver can import arrive as `SurfaceContent::Dmabuf` and are
    imported as GPU textures with no CPU copy. Everything else uses `wl_shm`,
    with one CPU copy per commit. The compositor advertises only the formats
    the renderer can import.
  - **Clipboard and selection**: the clipboard and primary selection are
    bridged as text.
  - **Drag and drop**: in-page drag-and-drop works, but the drag icon isn't
    painted and file drops from the host aren't forwarded.
  - **Not yet supported**: IME (`zwp_text_input_v3`), touch (`wl_touch`), and
    CSS cursor images (needs a GPUI custom-cursor API).
- **wasm (`web`)**: implemented; the crate only checks that it compiles, and
  the application tests placement and z-index.

### GPUI fork

The `dma-buf` path needs an external-texture import API that upstream GPUI
doesn't have. This crate patches GPUI Kit's `gpui-pre` crates onto
[`streetwriters/gpui-fast`](https://github.com/streetwriters/gpui-fast) (see
`[patch.crates-io]` in `Cargo.toml`). Applications that use the crate need the
same patch. Every backend works without `dma-buf`.

## Debugging

`GPUI_WEBVIEW_DEBUG=1` traces protocol milestones (connections, toplevel
mapping, first commits, dma-buf feedback, popup lifecycle) to stderr.
`GPUI_WEBVIEW_DEBUG=2` adds per-event traces: input events, commits, and
frame-callback counts.

## Example

```sh
cargo run --example demo                       # 6 posters
cargo run --features wry --example demo -- 20  # live surfaces on macOS
```

## Testing

```sh
cargo test                                   # policy + manager contract (mock backend)
cargo test --features wry                    # + the native backend compiles
cargo build --no-default-features            # interface only
cargo test --features nested --test nested_e2e   # Linux, needs GTK3 + WebKitGTK 4.1
```

`nested_e2e` runs the real compositor headless and needs no display. It
passes in a bare container on `wl_shm`. On a GPU machine, run it with the
`dma-buf` feature and `E2E_DMABUF=1` to assert the imported pixels.

`windows_e2e` (`--features wry,test-support`) needs an interactive Windows
session and the WebView2 Runtime. It hosts a live surface in a real GPUI
window, then checks the page with WebView2's own snapshot and the window
with `Windows.Graphics.Capture`.

Checklist for testing a backend by hand: create a surface, place it, clip it
at the viewport, scroll it while clipped, open a menu over it, and tear it
down. Memory (RSS) should return to its starting level, and a second
activation should reuse the engine without starting another process. Repeat
at 1x and 2x scale. Also check security: a clip containing `<script>` must run
nothing, and links must leave through `OpenExternal`.

## License

GPL-3.0-or-later
