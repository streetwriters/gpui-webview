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
