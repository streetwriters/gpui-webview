//! The clipboard and primary-selection bridge between the page and the host.
//!
//! WebKitGTK's clipboard is this compositor's data device, so nothing reaches
//! the system unless we carry it:
//!
//! * **Page → host.** A client `set_selection` (a copy; for primary, a new
//!   selection) is read back as text on a worker thread and reported as
//!   [`WebViewEvent::SelectionChanged`]; the app writes it to GPUI's clipboard.
//! * **Host → page.** The app offers the host's text
//!   ([`Command::HostSelection`](super::Command)) when a page gains focus or
//!   is clicked; it becomes a server-side selection the client reads on paste.
//!   While the page still owns a selection with that same text (its own copy,
//!   echoed back), the offer is skipped, so an in-page copy/paste keeps its
//!   rich formats.
//!
//! Data-device and primary focus follow keyboard focus (`focus_changed`), or
//! the client never sees an offer.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::sync::Arc;

use smithay::input::Seat;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::selection::SelectionSource;
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::selection::data_device::{
    clear_data_device_selection, request_data_device_client_selection, set_data_device_focus,
    set_data_device_selection,
};
use smithay::wayland::selection::primary_selection::{
    clear_primary_selection, request_primary_client_selection, set_primary_focus,
    set_primary_selection,
};

use super::compositor::NestedState;
use super::lock;
use crate::debug::debug_log;
use crate::spec::{Selection, WebViewEvent};

/// The text types offered and accepted, most specific first (GTK asks for
/// `text/plain;charset=utf-8`; X11-era clients for `UTF8_STRING`).
const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

/// A page selection larger than this is truncated on its way to the host.
const MAX_SELECTION_BYTES: u64 = 16 * 1024 * 1024;

/// Selection ownership as the compositor thread sees it.
#[derive(Default)]
pub(super) struct SelectionBridge {
    /// The generation of the page's current selection, while the page owns it.
    page_owned: HashMap<Selection, u64>,
    /// The host text currently offered as a server selection.
    offered: HashMap<Selection, Option<String>>,
    /// Page selections to read once the dispatch that set them is done.
    pending_reads: Vec<(SelectionTarget, u64, &'static str)>,
    next_generation: u64,
}

/// The page's selections read back so far, by generation (written by the
/// reader threads, so it lives in [`super::Shared`]).
pub(super) type PageSelections = HashMap<Selection, (u64, String)>;

fn selection_of(target: SelectionTarget) -> Selection {
    match target {
        SelectionTarget::Clipboard => Selection::Clipboard,
        SelectionTarget::Primary => Selection::Primary,
    }
}

impl NestedState {
    /// Keyboard focus moved: the focused client (if any) gets the data
    /// device's and primary selection's offers.
    pub(super) fn selection_focus(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| surface.client());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
    }

    /// The page set (or cleared) a selection: queue a read of its text.
    ///
    /// smithay calls `new_selection` *before* it stores the source on the
    /// seat, so a read requested here would hit the previous selection (or
    /// none); [`read_page_selections`](Self::read_page_selections) runs after
    /// the dispatch.
    pub(super) fn page_selection(
        &mut self,
        target: SelectionTarget,
        source: Option<SelectionSource>,
    ) {
        let selection = selection_of(target);
        self.selection.offered.remove(&selection);
        let Some(source) = source else {
            self.selection.page_owned.remove(&selection);
            return;
        };
        self.selection.next_generation += 1;
        let generation = self.selection.next_generation;
        self.selection.page_owned.insert(selection, generation);
        let offered = source.mime_types();
        let Some(mime) = TEXT_MIMES
            .into_iter()
            .find(|mime| offered.iter().any(|offer| offer == mime))
        else {
            debug_log!("{selection:?}: the page offers no text ({offered:?})");
            return;
        };
        self.selection
            .pending_reads
            .push((target, generation, mime));
    }

    /// Reads the page selections set during the last dispatch, each on a
    /// worker, and reports them. Called after `dispatch_clients`, before the
    /// flush that delivers the requests.
    pub(super) fn read_page_selections(&mut self) {
        for (target, generation, mime) in std::mem::take(&mut self.selection.pending_reads) {
            // A newer selection superseded this one in the same batch.
            if self.selection.page_owned.get(&selection_of(target)) != Some(&generation) {
                continue;
            }
            self.read_page_selection(target, generation, mime);
        }
    }

    fn read_page_selection(&mut self, target: SelectionTarget, generation: u64, mime: &str) {
        let selection = selection_of(target);
        let (reader, writer) = match std::io::pipe() {
            Ok(pipe) => pipe,
            Err(error) => {
                eprintln!("gpui-webview: selection pipe: {error}");
                return;
            }
        };
        let writer = OwnedFd::from(writer);
        let requested = match target {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.seat, mime.to_owned(), writer)
                    .map_err(|error| format!("{error:?}"))
            }
            SelectionTarget::Primary => {
                request_primary_client_selection(&self.seat, mime.to_owned(), writer)
                    .map_err(|error| format!("{error:?}"))
            }
        };
        if let Err(error) = requested {
            debug_log!("{selection:?}: reading the page's selection failed: {error}");
            return;
        }
        // The client writes once the request is flushed (after this dispatch)
        // and closes its end; a worker reads to EOF so the loop never blocks.
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if reader
                .take(MAX_SELECTION_BYTES)
                .read_to_end(&mut bytes)
                .is_err()
            {
                return;
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            debug_log!("{selection:?}: the page set {} bytes", text.len());
            lock(&shared.page_selections).insert(selection, (generation, text.clone()));
            lock(&shared.events).push(WebViewEvent::SelectionChanged { selection, text });
            let waker = lock(&shared.waker).clone();
            if let Some(waker) = waker {
                waker();
            }
        });
    }

    /// The app offered the host's text for `selection`.
    pub(super) fn offer_host_selection(&mut self, selection: Selection, text: Option<String>) {
        // The page's own selection, echoed back through the host: keep the
        // client's (it may carry HTML as well as text).
        if let Some(generation) = self.selection.page_owned.get(&selection)
            && lock(&self.shared.page_selections)
                .get(&selection)
                .is_some_and(|(read, page)| read == generation && Some(page) == text.as_ref())
        {
            return;
        }
        if self.selection.offered.get(&selection) == Some(&text) {
            return;
        }
        debug_log!(
            "{selection:?}: offering the host's text ({} bytes)",
            text.as_ref().map_or(0, String::len)
        );
        self.selection.page_owned.remove(&selection);
        self.selection.offered.insert(selection, text.clone());
        let handle = self.display_handle.clone();
        let seat = self.seat.clone();
        match (selection, text) {
            (Selection::Clipboard, Some(text)) => {
                set_data_device_selection(&handle, &seat, mimes(), Arc::new(text))
            }
            (Selection::Primary, Some(text)) => {
                set_primary_selection(&handle, &seat, mimes(), Arc::new(text))
            }
            (Selection::Clipboard, None) => clear_data_device_selection(&handle, &seat),
            (Selection::Primary, None) => clear_primary_selection(&handle, &seat),
        }
    }
}

fn mimes() -> Vec<String> {
    TEXT_MIMES.into_iter().map(str::to_owned).collect()
}

/// The client reads the host's text: write it on a worker (a large paste
/// fills the pipe before the client reads), then close.
pub(super) fn serve_host_selection(mime: &str, fd: OwnedFd, text: &Arc<String>) {
    debug_log!("the page reads the host's text as {mime}");
    if !TEXT_MIMES.contains(&mime) {
        return;
    }
    let text = text.clone();
    std::thread::spawn(move || {
        let _ = std::fs::File::from(fd).write_all(text.as_bytes());
    });
}
