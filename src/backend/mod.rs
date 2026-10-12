//! The backend modules, one per platform/mechanism (EMBEDS.md §9).
//!
//! Which modules compile is decided by cargo features, so the interface build
//! (`--no-default-features`) contains no platform code at all:
//!
//! | Feature   | Target        | Backend                                    |
//! |-----------|---------------|--------------------------------------------|
//! | `mock`    | any           | [`mock`](crate::mock): recording, no engine |
//! | `web`     | wasm32        | [`web`]: a clip `<div>` + `<iframe>`        |
//! | `wry`     | macOS/Windows | [`native`]: a wry child surface             |
//! | `wry`     | Linux/X11     | [`native`] (Tier 1/2), `nested` compositor  |
//! | `nested`  | Linux/Wayland | [`nested`]: smithay compositor (scaffold)   |

#[cfg(all(feature = "web", target_arch = "wasm32"))]
pub mod web;

#[cfg(all(feature = "wry", not(target_arch = "wasm32")))]
pub mod native;

#[cfg(all(feature = "nested", target_os = "linux"))]
pub mod nested;
