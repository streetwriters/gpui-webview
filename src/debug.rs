//! The crate's one debug switch.
//!
//! `GPUI_WEBVIEW_DEBUG=1` traces the protocol side (connections, toplevel
//! mapping, commits, input routing, dma-buf imports) to stderr. It is the first
//! thing to reach for when a surface stays blank on a new machine. Everything
//! that logs under it goes through [`debug_log!`], so the prefix and the switch
//! stay consistent.

/// Whether the switch is on (read once).
pub(crate) fn debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GPUI_WEBVIEW_DEBUG").is_some())
}

/// Trace one diagnostics line when the switch is on.
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if $crate::debug::debug_enabled() {
            eprintln!("gpui-webview: {}", format_args!($($arg)*));
        }
    };
}
pub(crate) use debug_log;
