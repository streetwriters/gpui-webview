# Changelog

## 0.1.0 — unreleased

Initial incubation in the Notesnook GPUI spike (EMBEDS.md §7–§15).

- `WebViews` manager: capacity, reuse by navigation, scroll-out teardown,
  overlay Lower/Freeze, skip-unchanged `frame`, focus hand-off, event drain.
- `WebViewBackend` contract with `attach`/`create`/`navigate`/`place`/
  `set_focus`/`present`/`destroy` and a `SchemeLoader` for local streaming
  sources.
- `MockBackend` recording backend (default feature) so the whole contract is
  testable with no engine on every target.
- `web` backend (wasm): clip `<div>` + `<iframe>`, never reparented, blob URLs
  revoked on navigate/teardown, sandbox/allow/referrer derived from `Policy`.
- `wry` macOS backend: `WKWebView` inside a layer-backed `masksToBounds`
  container; content-JavaScript/incognito/navigation policy enforcement.
- `nested` Linux scaffold (smithay chosen).
- `examples/demo.rs`: a standalone GPUI app proving reuse without any editor
  dependency.

### Input/event audit fixes (2026-10-13, `WEBVIEW_INPUT_AUDIT.md`)

- `WebInputEvent` grew `PointerEnter`, `PointerLeave` and `Modifiers`;
  motion/enter/leave are position-routed, so hover works before a click and
  leaving releases the protocol pointer focus.
- Real key-up forwarding and modifier transitions for self-compositing
  backends (no more synthetic taps); the nested backend applies held
  modifiers and covers shifted punctuation, F-keys and `key_char`.
- `WebViewBackend::is_focused` removed (the manager owns focus);
  `wants_animation_frame` added so the nested pump cannot stall between
  commits.
- Nested backend emits `TitleChanged`, `LoadFailed`, `Crashed`,
  `FullscreenRequested` and `Closed`; navigation policy is enforced on the
  live engine and `navigate` re-applies the whole policy; `Storage` maps to
  incognito/shared `WebContext`s.
- Wheel deltas stay in GPUI's convention and are flipped by the nested
  backend; the motion axis and the wheel go into separate frames, so a single
  tick is never dropped.
- Round 2: the dma-buf API is gated behind the `dma-buf` feature (the patch
  table lives in a generated `.cargo/config.toml`, so a fresh checkout builds
  vanilla GPUI); the import cache keys on a per-buffer generation; popups
  honour `set_window_geometry` and clipped dma-buf popups paint under a
  content mask; fullscreen requests are intercepted and reported; held keys
  are released on blur/defocus; and one nested backend per process is
  enforced.
- Structural pass (review follow-up):
  - `WebInputEvent::PointerButton` carries the platform-neutral
    `PointerButton` enum; each backend maps it to its own codes.
  - `physical_key(&Keystroke)` is the crate's one keystroke→physical-key
    table; the editor pairs held-key releases by it, and `Fn` is no longer
    mistyped as the Menu key.
  - The dma-buf texture and failed-import cache is one `DmaBufCache` keyed by
    the backend `SurfaceId`, so a `rekey` cannot strand entries; every surface
    teardown goes through `WebViews::remove_surface`.
  - `WebViews::platform_overlay_behavior()` (and the backend's
    `default_overlay_behavior()`) let the crate report what the backend can
    honour instead of the app guessing.
- Structural pass, round 2:
  - The nested backend is split into `backend/nested/{mod,backend,buffers,compositor,input,policy,popups}.rs`;
    every surface stores its role (toplevel id or popup id/block) on the
    surface itself, and new toplevels are matched by their
    `gpui-webview-{id}` title.
  - The editor's live-surface state is one `LiveSurface`: each frame stores its
    imported texture with it, `sync` processes a commit in one place, and the
    page/popup paint path is shared.
  - Every forwarded button goes through one press path, and the E2E is one
    Linux module with `count_rgb`, a shared colour table, named harness steps
    and an `E2E_ONLY=` filter.
