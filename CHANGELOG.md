# Changelog

## 0.1.0 — unreleased

Initial release.

- `WebViews` manager: capacity, reuse by navigation, scroll-out teardown,
  overlay `Lower`/`Freeze`, skip-unchanged `frame`, focus hand-off, event
  drain, and `platform_overlay_behavior()` reporting what the backend can
  honour.
- `WebViewBackend` contract with `attach`/`create`/`navigate`/`place`/
  `set_focus`/`present`/`destroy`. Sources are a URL or inline HTML.
- `WebInputEvent` with pointer enter/leave/motion, the platform-neutral
  `PointerButton`, real key-up forwarding and modifier transitions;
  `physical_key(&Keystroke)` is the crate's one keystroke→physical-key table.
- `MockBackend` recording backend (default feature), so the whole contract is
  testable with no engine on every target.
- `web` backend (wasm): clip `<div>` + `<iframe>`, never reparented, blob URLs
  revoked on navigate/teardown, sandbox/allow/referrer derived from `Policy`.
- `wry` macOS backend: `WKWebView` inside a layer-backed `masksToBounds`
  container; content-JavaScript/incognito/navigation policy enforcement.
- `wry` Windows backend: a WebView2 child HWND inside a clip-container HWND;
  `prepare_process` switches GPUI to a flip-model swapchain so the surface is
  visible.
- `nested` Linux backend: an in-process smithay compositor hosting
  GTK/WebKitGTK clients on their own thread. Frames arrive as `wl_shm` copies
  or, with the `dma-buf` feature, as imported GPU textures (`ReadyFrame`/
  `ReadyPopup` + `paint_frame`). Popups and menus, clipboard and primary
  selection bridging, host-scale rendering, keymap matching the host layout,
  and `TitleChanged`/`LoadFailed`/`Crashed`/`FullscreenRequested`/`Closed`
  events.
- `GPUI_WEBVIEW_DEBUG=1` milestone traces and `=2` per-event traces.
- End-to-end tests: `nested_e2e` (Linux, headless) and `windows_e2e`
  (Windows, real window + `Windows.Graphics.Capture`).
- `examples/demo.rs`: a standalone GPUI app showing engine reuse.
