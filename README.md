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
| `wry`            | Windows/Linux | reports `Error::Platform` (see status)                        |
| `nested`         | Linux/Wayland | scaffold for the smithay nested compositor (see status)        |
| `window`         | any           | separate-window/fullscreen presentation (not wired yet)        |

`--no-default-features` builds the interface only (specs, manager, backend
trait) with no platform code at all, which is what an application's
platform-independent core should depend on.

## Status

- **macOS (`wry`)**: implemented. `WKWebView` in a `masksToBounds` container;
  script/incognito/navigation policies from `Policy`; overlays freeze. Compile
  verified; the manual GUI checklist (below) is still required.
- **wasm (`web`)**: implemented. Compile verified; the per-frame placement and
  z-index assertions live in the host's page tests.
- **Linux (`nested`)**: not implemented. The design and the library choice
  (**smithay**, user decision 2026-10-12) are recorded in
  `src/backend/nested.rs`; it returns `Unsupported` until the compositor lands.
  X11's tiers (`§10.4`) are not started.
- **Windows**: not started.
- **GPUI patches** (`dma-buf` external textures, `wayland-surface`): required by
  the Linux fast paths only; documented in EMBEDS.md §14. Every backend must
  work without them.

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
