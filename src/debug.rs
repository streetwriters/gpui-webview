//! The crate's one debug switch.
//!
//! `GPUI_WEBVIEW_DEBUG=1` traces protocol milestones (connections, toplevel
//! mapping, first commits, dma-buf feedback, popup lifecycle) to stderr; `=2`
//! adds the per-event traces (every input event, every commit, frame-callback
//! counts) that make commit rates measurable. Everything logs through
//! [`debug_log!`]/[`debug_log2!`], so the prefix and the switch stay consistent.

/// Whether the switch is on at all (read once).
pub(crate) fn debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GPUI_WEBVIEW_DEBUG").is_some())
}

/// Whether the switch asks for per-event traces (`=2`). Linux-only: the
/// nested compositor is the backend with per-event volume.
#[cfg(not(target_os = "windows"))]
pub(crate) fn debug_verbose() -> bool {
    static VERBOSE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VERBOSE.get_or_init(|| std::env::var("GPUI_WEBVIEW_DEBUG").is_ok_and(|value| value == "2"))
}

/// Trace one milestone line when the switch is on.
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if $crate::debug::debug_enabled() {
            eprintln!("gpui-webview: {}", format_args!($($arg)*));
        }
    };
}
pub(crate) use debug_log;

/// Trace one per-event line when `GPUI_WEBVIEW_DEBUG=2` asks for them.
#[cfg(not(target_os = "windows"))]
macro_rules! debug_log2 {
    ($($arg:tt)*) => {
        if $crate::debug::debug_verbose() {
            eprintln!("gpui-webview: {}", format_args!($($arg)*));
        }
    };
}
#[cfg(not(target_os = "windows"))]
pub(crate) use debug_log2;
