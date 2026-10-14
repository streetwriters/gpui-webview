//! Headless end-to-end check of the Linux nested compositor.
//!
//! No host display is involved: GTK/WebKitGTK connect to the private Wayland
//! socket this crate serves, render the page, and commit buffers that come back
//! as frames the app would paint. That makes the risky parts of the Linux path
//! (GTK startup against our compositor, `xdg_toplevel` mapping and configure,
//! WebKit's first paint, buffer transport, teardown) testable on any Linux box
//! — including a headless VM.
//!
//! ```sh
//! # wl_shm (the universal fallback; also what runs on lavapipe):
//! cargo test -p gpui-webview --features nested --test nested_e2e -- --nocapture
//!
//! # force the shm path even where dma-buf works:
//! E2E_FORCE_SHM=1 cargo test -p gpui-webview --features nested --test nested_e2e
//!
//! # require a dma-buf frame, import it, and verify its pixels (GPU machine;
//! # needs the patched GPUI crates and the `dma-buf` feature):
//! E2E_DMABUF=1 cargo test -p gpui-webview --features nested,dma-buf --test nested_e2e
//! ```
//!
//! A headless `wgpu` device (no window, no display) builds the compositor's
//! `linux-dmabuf` feedback and, in `E2E_DMABUF=1`, imports the committed
//! dma-buf and samples it back on the GPU — the full frame path minus GPUI's
//! painting. On a machine whose driver cannot import dma-bufs (lavapipe, a
//! GPU-less VM) the capabilities are empty and WebKit stays on `wl_shm`, which
//! is exactly the fallback the app relies on. Without the `dma-buf` feature
//! the test runs the same walk on `wl_shm` only.
//!
//! It is `harness = false` so everything runs on the process's main thread,
//! which GTK requires. Exit code 0 = pass. `E2E_TIMEOUT_SECS` overrides the
//! 45-second budget for the first commit (a slow, GPU-less VM may need it).

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("nested_e2e is Linux-only; nothing to do.");
}

#[cfg(target_os = "linux")]
mod linux {
    /// The headless GPU context the dma-buf path imports through. Without the
    /// `dma-buf` feature there is no GPU code to call; the shm frames carry their
    /// pixels directly.
    #[cfg(feature = "dma-buf")]
    type Gpu = HeadlessGpu;
    #[cfg(not(feature = "dma-buf"))]
    type Gpu = ();


use std::time::{Duration, Instant};
use gpui_kit::{Bounds, point, px, size};
use gpui_webview::backend::nested::NestedBackend;
use gpui_webview::{Config, Placement, Policy, Source, SurfaceSpec, SurfaceContent, WebViews};
use std::os::unix::fs::PermissionsExt as _;

    /// The live state a step works on. `E2E_ONLY=<name>` runs one step (the
    /// prologue always runs first: a step needs an activated, committed page).
    struct Harness {
        webviews: gpui_webview::WebViews,
        live: Vec<(u64, gpui_webview::Placement)>,
        viewport: Bounds<gpui_kit::Pixels>,
        gpu: Option<Gpu>,
        width: u32,
        height: u32,
        html: String,
    }

    /// Ends the run with a named failure (the process exits non-zero).
    fn fail(what: &str) -> ! {
        eprintln!("FAIL: {what}");
        std::process::exit(1);
    }

    impl Harness {
        fn step_pointer_leave(&mut self) {
        let viewport = self.viewport;
        let live = &self.live;
        let webviews = &mut self.webviews;
        // -- pointer leave --------------------------------------------------------
        // A leave must release the protocol pointer focus (`wl_pointer.leave`), so
        // GDK stops routing to the page and a later motion re-enters cleanly. The
        // manager's contract is covered by
        // `manager::tests::pointer_events_are_positional_and_keys_wait_for_focus`;
        // here the path is exercised end-to-end (with `GPUI_WEBVIEW_DEBUG=1` the
        // compositor logs `input: pointer left block`, and the GTK widget receives
        // a `leave-notify`). Whether WebKit bubbles that into a DOM `mouseout` is
        // WebKit's business, not the compositor's.
        webviews.input(
            0,
            gpui_webview::WebInputEvent::PointerMotion { x: 120.0, y: 80.0 },
        );
        for _ in 0..5 {
            webviews.frame(&live, viewport, &[]);
            let _ = webviews.frame_image(0);
            std::thread::sleep(Duration::from_millis(20));
        }
        webviews.input(0, gpui_webview::WebInputEvent::PointerLeave);
        for _ in 0..5 {
            webviews.frame(&live, viewport, &[]);
            let _ = webviews.frame_image(0);
            std::thread::sleep(Duration::from_millis(20));
        }
        println!("pointer: a leave released the pointer focus (see the debug log)");

        }

        fn step_popups(&mut self) {
        let width = self.width;
        let height = self.height;
        let viewport = self.viewport;
        let live = &self.live;
        let gpu = &self.gpu;
        let mut webviews = &mut self.webviews;
        // -- native popups -------------------------------------------------------
        // The `<select>`'s dropdown is a GTK popup: an `xdg_popup` the compositor
        // composites itself and routes pointer input into. WebKit's context menus
        // (and date pickers) take the same path.
        let select = (245.0, 132.0);
        click_at(&mut webviews, select.0, select.1);
        let popups = wait_for_popups(&mut webviews, &live, viewport, true);
        let Some(popup) = popups.first().cloned() else {
            fail("clicking the <select> never produced a native popup (xdg_popup)");
        };
        println!(
            "popup: {} at ({}, {}) size {}x{}",
            popup.id, popup.position.0, popup.position.1, popup.frame.size.0, popup.frame.size.1
        );
        if popup.position.0 < 0
            || popup.position.1 < 0
            || popup.position.0 + popup.frame.size.0 as i32 > width as i32
            || popup.position.1 + popup.frame.size.1 as i32 > height as i32
        {
            fail("the popup was not fitted inside the page's rect");
        }
        let Some(first_pixels) = frame_pixels(&popup.frame, gpu.as_ref()) else {
            fail("the popup frame carried no readable pixels");
        };
        let distinct = distinct_colours(&first_pixels);
        if distinct < 3 {
            fail(&format!(
                "the popup frame looks blank ({distinct} distinct colours)"
            ));
        }
        write_bmp("/tmp/popup-sample.bmp", &first_pixels, popup.frame.size.0, popup.frame.size.1);
        println!("popup: {distinct} distinct colours inside it");

        // Input routes into the popup: hovering an item makes GTK highlight it,
        // which is a new commit with different pixels. Theme-dependent, so a miss
        // is reported rather than failed — the item click below is the real proof.
        // The client may re-lay the menu out between commits (the first frame can
        // carry padding later ones drop), so every point is measured against the
        // frame in hand: 60% down sits inside the second item either way.
        let item_point = |popup: &gpui_webview::PopupFrame| -> (f64, f64) {
            (
                popup.position.0 as f64 + popup.frame.size.0 as f64 / 2.0,
                popup.position.1 as f64 + popup.frame.size.1 as f64 * 0.6,
            )
        };
        let hover = item_point(&popup);
        webviews.input(
            0,
            gpui_webview::WebInputEvent::PointerMotion {
                x: hover.0,
                y: hover.1,
            },
        );
        let until = Instant::now() + Duration::from_secs(5);
        let mut hovered = false;
        while Instant::now() < until {
            webviews.frame(&live, viewport, &[]);
            if let Some(current) = webviews
                .popup_frames(0)
                .first()
                .and_then(|popup| frame_pixels(&popup.frame, gpu.as_ref()))
            {
                if current != first_pixels {
                    hovered = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !hovered {
            eprintln!("NOTE: hovering an item did not repaint the popup (theme?)");
        }

        // Choosing the first item changes the page (`<select onchange>`), which can
        // only happen if the click was delivered to the popup's own surface. GTK
        // closes the menu as soon as a choice is made, so the empty popup list plus
        // the page's new colour are the two halves of the proof.
        // The *second* item: the first is the page's selected option, so choosing
        // it would fire no `change` event and the check would be a coin flip.
        webviews.frame(&live, viewport, &[]);
        let measure = webviews
            .popup_frames(0)
            .first()
            .cloned()
            .unwrap_or(popup.clone());
        let item = item_point(&measure);
        click_at(&mut webviews, item.0, item.1);
        let popups = wait_for_popups(&mut webviews, &live, viewport, false);
        if !popups.is_empty() {
            fail("the popup never closed after an item was clicked");
        }
        let changed = wait_for_page_colour(
            &mut webviews,
            &live,
            viewport,
            gpu.as_ref(),
            |bytes| count_rgb(bytes, colour("changed")),
            10,
        );
        if changed <= 1000 {
            fail("choosing a popup item never reached the page (no reaction colour)");
        }
        println!("popup: the item click reached the page ({changed} pixels)");

        // Reopen and dismiss with an outside click: the press reaches the page and
        // the popup is told `popup_done` (what its grab asked for).
        click_at(&mut webviews, select.0, select.1);
        let popups = wait_for_popups(&mut webviews, &live, viewport, true);
        if popups.is_empty() {
            fail("the popup did not reopen for the dismissal check");
        }
        click_at(&mut webviews, 20.0, 20.0);
        let popups = wait_for_popups(&mut webviews, &live, viewport, false);
        if !popups.is_empty() {
            fail("an outside click did not dismiss the popup");
        }
        println!("popup: dismissed by an outside click");
        // A popup that comes and goes must not leave the surface deaf: GDK's
        // pointer focus once stuck to the destroyed popup window and swallowed
        // every later event. (The dismissal click itself is the menu's to swallow
        // — GTK's grab is still releasing — so the check is the *next* click.)
        click_at(&mut webviews, 150.0, 60.0);
        let blue = wait_for_page_colour(
            &mut webviews,
            &live,
            viewport,
            gpu.as_ref(),
            |bytes| count_rgb(bytes, colour("blue")),
            5,
        );
        if blue <= 1000 {
            fail("the page stopped reacting after a popup was dismissed");
        }
        println!("popup: the page still reacts after a popup ({blue} blue pixels)");

        }

        fn step_menu(&mut self) {
        let viewport = self.viewport;
        let live = &self.live;
        let gpu = &self.gpu;
        let mut webviews = &mut self.webviews;
        // -- the engine's own context menu ---------------------------------------
        // Right-clicking a page the *app* owns (Google, a clip) shows the engine's
        // menu, trimmed to what a note needs: no Back/Reload/Save Page As, no
        // downloads or new windows, no "Inspect Element" (devtools is off). It
        // composites like any other popup.
        right_click_at(&mut webviews, 40.0, 20.0); // over the <h1> text
        let menus = wait_for_popups(&mut webviews, &live, viewport, true);
        let Some(menu) = menus.first().cloned() else {
            fail("right-clicking the page never produced the engine's context menu");
        };
        println!(
            "menu: {} at ({}, {}) size {}x{}",
            menu.id, menu.position.0, menu.position.1, menu.frame.size.0, menu.frame.size.1
        );
        let Some(menu_pixels) = frame_pixels(&menu.frame, gpu.as_ref()) else {
            fail("the context menu frame carried no readable pixels");
        };
        let distinct = distinct_colours(&menu_pixels);
        if distinct < 3 {
            fail(&format!(
                "the context menu looks blank ({distinct} distinct colours)"
            ));
        }
        write_bmp("/tmp/menu-sample.bmp", &menu_pixels, menu.frame.size.0, menu.frame.size.1);
        println!("menu: {distinct} distinct colours inside it");
        // An outside press dismisses it, like the page's own menus — and it has to
        // land outside the menu itself (the menu's top-left region) and clear of
        // the page's scrollbar, which the taller page now has.
        click_at(&mut webviews, 180.0, 180.0);
        let menus = wait_for_popups(&mut webviews, &live, viewport, false);
        if !menus.is_empty() {
            fail("an outside click did not dismiss the engine's context menu");
        }
        println!("menu: the engine's context menu composites and dismisses");

        }

        fn step_wheel(&mut self) {
        let viewport = self.viewport;
        let live = &self.live;
        let gpu = &self.gpu;
        let webviews = &mut self.webviews;
        // -- the wheel -----------------------------------------------------------
        // The page is an iframe: a wheel over it scrolls it, whether or not it has
        // the keyboard. The page is taller than the viewport with a marker below
        // the fold, and this runs after every coordinate-sensitive check because
        // scrolling moves them.
        //
        // One tick: GPUI's wheel-down is a *negative* `dy` (the editor forwards it
        // raw and the compositor flips it for GDK). Sending exactly one tick and
        // requiring the marker to appear asserts both the sign and that the first
        // tick is delivered — the previous test sent twenty positive ticks and
        // passed on an inverted sign with tick one silently dropped.
        webviews.set_focus(0, false);
        webviews.input(
            0,
            gpui_webview::WebInputEvent::PointerMotion { x: 160.0, y: 100.0 },
        );
        let before = webviews
            .frame_image(0)
            .and_then(|frame| frame_pixels(&frame, gpu.as_ref()))
            .map(|bytes| count_rgb(&bytes, colour("marker")))
            .unwrap_or(0);
        if before > 0 {
            fail("the scroll marker was visible before the wheel");
        }
        webviews.input(
            0,
            gpui_webview::WebInputEvent::PointerAxis {
                x: 160.0,
                y: 100.0,
                dx: 0.0,
                dy: -60.0,
            },
        );
        let until = Instant::now() + Duration::from_secs(10);
        let mut marker = 0usize;
        while Instant::now() < until {
            webviews.frame(&live, viewport, &[]);
            if let Some(frame) = webviews.frame_image(0)
                && let Some(bytes) = frame_pixels(&frame, gpu.as_ref())
            {
                marker = count_rgb(&bytes, colour("marker"));
                if marker > 200 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if marker <= 200 {
            let wheel = webviews
                .frame_image(0)
                .and_then(|frame| frame_pixels(&frame, gpu.as_ref()))
                .map(|bytes| count_rgb(&bytes, colour("wheel")))
                .unwrap_or(0);
            eprintln!("NOTE: the DOM wheel reaction is {wheel} pixels");
            fail("a single wheel tick did not scroll the page (sign/first tick)");
        }
        println!("wheel: one tick scrolled the page ({marker} marker pixels)");
        // The reverse tick must scroll back up: the sign is not just "moves".
        webviews.input(
            0,
            gpui_webview::WebInputEvent::PointerAxis {
                x: 160.0,
                y: 100.0,
                dx: 0.0,
                dy: 60.0,
            },
        );
        let until = Instant::now() + Duration::from_secs(10);
        let mut back_up = false;
        while Instant::now() < until {
            webviews.frame(&live, viewport, &[]);
            if let Some(frame) = webviews.frame_image(0)
                && let Some(bytes) = frame_pixels(&frame, gpu.as_ref())
                && count_rgb(&bytes, colour("marker")) == 0
            {
                back_up = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !back_up {
            fail("the reverse wheel tick did not scroll the page back up");
        }
        println!("wheel: the reverse tick scrolled back");

        }

        fn step_resize(&mut self) {
        let width = self.width;
        let height = self.height;
        let viewport = self.viewport;
        let live = &self.live;
        let webviews = &mut self.webviews;
        // -- resize ----------------------------------------------------------------
        // Resizing a live embed must reconfigure the client (a new `xdg_toplevel`
        // configure), so the page re-lays out instead of being stretched by the
        // app's paint. The frame tracking the new size is the end-to-end proof.
        let smaller = (240u32, 150u32);
        let small_viewport = Bounds {
            origin: point(px(0.), px(0.)),
            size: gpui_kit::size(px(smaller.0 as f32), px(smaller.1 as f32)),
        };
        let small_live = vec![(
            0u64,
            Placement::clipped_to(small_viewport, small_viewport),
        )];
        let until = Instant::now() + Duration::from_secs(10);
        let mut resized = false;
        while Instant::now() < until {
            webviews.frame(&small_live, small_viewport, &[]);
            if let Some(frame) = webviews.frame_image(0)
                && frame.size == smaller
            {
                resized = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !resized {
            fail("the client never recommitted at the resized size");
        }
        println!("resize: the client re-laid out to {}x{}", smaller.0, smaller.1);
        // Back to the original size for the remaining checks.
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            webviews.frame(&live, viewport, &[]);
            if webviews
                .frame_image(0)
                .is_some_and(|frame| frame.size == (width, height))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        }

        fn step_navigation(&mut self) {
        let width = self.width;
        let height = self.height;
        let viewport = self.viewport;
        let live = &self.live;
        let webviews = &mut self.webviews;
        // -- navigation policy ----------------------------------------------------
        // The HTML surface just ran under `Policy::default()`'s `OpenExternal`.
        // Navigating the *same* engine to a remote URL must therefore be refused
        // and reported as `OpenExternal`: the app opens the browser, the engine
        // never leaves the page. (The remote URL is never fetched — the decision
        // happens first, which is what makes this work without a network.)
        webviews
            .activate(
                0,
                SurfaceSpec::new(
                    Source::Url("https://example.com/next".into()),
                    Policy::default(),
                    "navigation",
                )
                .with_size(width, height),
            )
            .unwrap_or_else(|error| fail(&format!("navigating to the URL failed: {error}")));
        let until = Instant::now() + Duration::from_secs(5);
        let mut navigated = false;
        while Instant::now() < until {
            webviews.frame(&live, viewport, &[]);
            // `frame_image` is what pumps GTK/WebKit on the main thread; the
            // decision cannot arrive without it.
            let _ = webviews.frame_image(0);
            for event in webviews.events() {
                if matches!(
                    event,
                    gpui_webview::WebViewEvent::OpenExternal { url, .. }
                        if url.contains("example.com/next")
                ) {
                    navigated = true;
                }
            }
            if navigated {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !navigated {
            fail("a remote navigation under OpenExternal was not refused and reported");
        }
        println!("navigation: the remote navigation was refused and reported as OpenExternal");

        }

        fn step_storage(&mut self) {
        let html = self.html.clone();
        let width = self.width;
        let height = self.height;
        let viewport = self.viewport;
        let live = &self.live;
        let webviews = &mut self.webviews;
        // -- shared storage -------------------------------------------------------
        // `Policy::media()` is `Storage::Shared`: the backend keeps one
        // `WebContext` for every embed. Switching a live engine's storage
        // recreates it (a WebKit context cannot change in place), so this also
        // exercises the recreate path.
        webviews
            .activate(
                0,
                SurfaceSpec::new(
                    Source::Html {
                        html: html.to_string(),
                        base: None,
                    },
                    Policy::media(),
                    "shared",
                )
                .with_size(width, height),
            )
            .unwrap_or_else(|error| fail(&format!("shared-storage activation failed: {error}")));
        let until = Instant::now() + Duration::from_secs(10);
        let mut shared_frame = false;
        while Instant::now() < until {
            webviews.frame(&live, viewport, &[]);
            if webviews.frame_image(0).is_some() {
                shared_frame = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !shared_frame {
            fail("a shared-storage engine never committed a frame");
        }
        println!("storage: a shared-context engine loaded and committed");

        // Re-activating with the same spec must reuse the engine (no recreate).
        webviews
            .activate(0, SurfaceSpec::new(
                Source::Html {
                    html: html.to_string(),
                    base: None,
                },
                Policy {
                    scripts: true,
                    ..Policy::default()
                },
                "nested e2e",
            )
            .with_size(width, height))
            .unwrap_or_else(|error| fail(&format!("re-activate failed: {error}")));

        // Deactivation must tear the client down and drop its frame.
        webviews.deactivate(0);
        let until = Instant::now() + Duration::from_secs(5);
        let mut gone = false;
        while Instant::now() < until {
            webviews.frame(&[], viewport, &[]);
            if webviews.frame_image(0).is_none() {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !gone {
            fail("the frame survived deactivation");
        }

        }
    }

    pub(super) fn main() {


        let (width, height) = (320u32, 200u32);
        // No network involved: a red page, so a committed frame is self-evidently
        // the page and not an empty surface.
        // Handlers live on `document`: `<body>`'s box covers only the `<h1>`'s
        // height, so events lower in the viewport target `<html>` and never reach
        // a body-level handler. (That mistake once hid a working input path.)
        let html = "<!doctype html><html><body style=\"margin:0;background:#c81e1e\">\
                    <h1 style=\"margin:0;color:#fff;font-family:sans-serif\">nested e2e</h1>\
                    <div style=\"height:200px\"></div>\
                    <div style=\"height:40px;background:#1e7fc8\"></div>\
                    <select id=\"pick\" style=\"position:absolute;left:200px;top:120px;\
                    width:90px;height:24px\"\
                    onchange=\"pageBody.style.background='#c81e7f'\">\
                    <option selected>one</option><option>two</option></select>\
                    <script>\
                    var pageBody = document.body;\
                    document.addEventListener('mousemove', function(){\
                    pageBody.style.background='#c8c81e';});\
                    document.addEventListener('click', function(){\
                    pageBody.style.background='#1e1ec8';});\
                    var keyCount = 0;\
                    document.addEventListener('keydown', function(e){\
                    keyCount++;\
                    if (e.key === '@') { pageBody.style.background = '#7fc81e'; return; }\
                    pageBody.style.background = keyCount === 1 ? '#c81ec8' : '#7f1e1e';});\
                    document.addEventListener('keyup', function(e){\
                    if (e.key === 'a') pageBody.style.background='#c87f1e';});\
                    // Right-click diagnostics: each button paints a colour, so a
                    // failure dump says whether the web process saw the click at
                    // all (`diagnose_frame` prints the top colours).
                    document.addEventListener('mousedown', function(e){\
                    pageBody.style.background = e.button === 2 ? '#7f1e7f' :\
                    (e.button === 0 ? '#1e7f7f' : '#7f7f1e');});\
                    document.addEventListener('contextmenu', function(){\
                    pageBody.style.background='#1e7f1e';});\
                    document.addEventListener('wheel', function(){\
                    pageBody.style.background='#1e1e7f';});\
                    setTimeout(function(){ pageBody.style.background =\
                    document.visibilityState === 'visible'\
                    ? (document.hasFocus() ? '#1ec81e' : '#1ec8c8') : '#c8c81e'; }, 400);\
                    </script>\
                    </body></html>";
        // Every colour the assertions use must still exist in the page.
        for (name, rgb) in PAGE_COLOURS {
            assert!(
                html.contains(&format!("#{rgb:06x}")),
                "the page lost the {name} colour"
            );
        }
        let timeout = std::env::var("E2E_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(45));


        let force_shm = std::env::var("E2E_FORCE_SHM").is_ok_and(|value| value == "1");
        let want_dmabuf = std::env::var("E2E_DMABUF").is_ok_and(|value| value == "1");

        if force_shm {
            // GTK3 WebKitGTK honours both switches; setting both makes the intent
            // unambiguous across versions.
            unsafe {
                std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
                std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
            }
        }

        // The Wayland socket lives in `XDG_RUNTIME_DIR`; make one when the
        // environment has none (CI, containers) so the test is self-contained.
        let owned_runtime = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => None,
            _ => {
                let dir = std::env::temp_dir().join(format!("nested-e2e-{}", std::process::id()));
                std::fs::create_dir_all(&dir).expect("create XDG_RUNTIME_DIR");
                #[cfg(unix)]
                {
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                        .expect("chmod 0700 XDG_RUNTIME_DIR");
                }
                unsafe {
                    std::env::set_var("XDG_RUNTIME_DIR", &dir);
                }
                Some(dir)
            }
        };

        // `E2E_WHEEL_CALIBRATE=1`: measure (and print) how far one forwarded axis
        // event moves a page, per input unit, then exit. Run from the repo root
        // with `cargo test -p gpui-webview --features nested --test nested_e2e`
        // and the env set; the same command runs on the GPU machine. It needs the
        // runtime dir above, not the GPU.
        if std::env::var("E2E_WHEEL_CALIBRATE").is_ok_and(|value| value == "1") {
            wheel_calibration();
            return;
        }

        // A headless GPU context: the compositor advertises exactly what the
        // renderer can import, and the test imports/samples the frames through the
        // same code the app uses. Only built with the `dma-buf` feature; without
        // it the run is shm-only.
        #[cfg(feature = "dma-buf")]
        let gpu = headless_gpu();
        #[cfg(not(feature = "dma-buf"))]
        let gpu: Option<Gpu> = None;

        #[cfg(feature = "dma-buf")]
        let capabilities = if force_shm {
            None
        } else {
            gpu.as_ref()
                .and_then(|gpu| gpui_wgpu::dmabuf::capabilities(&gpu.adapter))
        };
        #[cfg(not(feature = "dma-buf"))]
        let capabilities: Option<()> = None;

        #[cfg(feature = "dma-buf")]
        match (&capabilities, force_shm) {
            (Some(capabilities), _) => println!(
                "dmabuf: node {}:{}, {} importable format(s)",
                capabilities.device.0,
                capabilities.device.1,
                capabilities.formats.len()
            ),
            (None, true) => println!("dmabuf: disabled by E2E_FORCE_SHM=1"),
            (None, false) => {
                println!("dmabuf: none (this driver cannot import dma-bufs; wl_shm it is)")
            }
        }
        #[cfg(not(feature = "dma-buf"))]
        {
            let _ = force_shm;
            println!("dmabuf: not built (feature `dma-buf` off); wl_shm only");
        }
        if want_dmabuf && capabilities.is_none() {
            #[cfg(feature = "dma-buf")]
            fail("E2E_DMABUF=1 but this machine has no importable dma-buf formats");
            #[cfg(not(feature = "dma-buf"))]
            fail("E2E_DMABUF=1 requires `--features nested,dma-buf` (patched GPUI)");
        }

        #[cfg(feature = "dma-buf")]
        let backend = NestedBackend::new_with_dmabuf(capabilities)
            .unwrap_or_else(|error| fail(&format!("could not start the compositor: {error}")));
        #[cfg(not(feature = "dma-buf"))]
        let backend = NestedBackend::new()
            .unwrap_or_else(|error| fail(&format!("could not start the compositor: {error}")));
        let mut webviews = WebViews::new(backend, Config::default());

        let spec = SurfaceSpec::new(
            Source::Html {
                html: html.to_string(),
                base: None,
            },
            Policy {
                scripts: true,
                ..Policy::default()
            },
            "nested e2e",
        )
        .with_size(width, height);
        let viewport = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(width as f32), px(height as f32)),
        };
        let live = vec![(0u64, Placement::clipped_to(viewport, viewport))];

        webviews
            .activate(0, spec)
            .unwrap_or_else(|error| fail(&format!("activate failed: {error}")));

        // Pump the compositor/GTK until the page commits a frame *with the page in
        // it*. The first commits are GTK's pre-paint frames (an empty window), so
        // waiting for the red background is the real end-to-end signal. The
        // transport that carried the frame is reported, and `E2E_DMABUF=1` keeps
        // waiting for a dma-buf even if shm frames arrive first.
        let started = Instant::now();
        let mut first_commit = None;
        let mut first_commit_transport = None;
        let mut transport = None;
        let mut red = 0usize;
        let mut frame_bytes = 0usize;
        let mut size_seen = None;
        // Only the dma-buf arms mutate/read these; keep them visible in the tail
        // (which reports the best sample) with the feature off.
        #[allow(unused_mut, unused_variables)]
        let mut dmabuf_samples = 0usize;
        #[allow(unused_mut, unused_variables)]
        let mut best_ratio = 0.0f64;
        #[allow(unused_mut, unused_variables)]
        let mut best_bytes: Option<Vec<u8>> = None;
        #[allow(unused_mut, unused_variables)]
        let mut self_checked = false;
        while started.elapsed() < timeout {
            webviews.frame(&live, viewport, &[]);
            if let Some(frame) = webviews.frame_image(0) {
                if first_commit.is_none() {
                    first_commit = Some(started.elapsed());
                    first_commit_transport = Some(match frame.content {
                        SurfaceContent::Image(_) => "shm",
                        #[cfg(feature = "dma-buf")]
                        SurfaceContent::Dmabuf(_) => "dmabuf",
                    });
                }
                size_seen = Some(frame.size);
                match &frame.content {
                    SurfaceContent::Image(image) => {
                        if let Some(bytes) = image.as_bytes(0) {
                            red = count_rgb(bytes, colour("red"));
                            frame_bytes = bytes.len();
                            // Readback self-check: upload these *known-good* bytes
                            // and sample them back through the same render pass the
                            // dma-buf path uses. If this does not reproduce the shm
                            // frame, the readback — not the import — is wrong. Only
                            // worth doing once the page is actually painted.
                            #[cfg(feature = "dma-buf")]
                            if let Some(gpu) = &gpu
                                && red > 1000
                                && !self_checked
                            {
                                self_checked = true;
                                let texture = gpui_wgpu::external_texture_from_rgba(
                                    &gpu.device,
                                    &gpu.queue,
                                    (width, height),
                                    bytes,
                                );
                                match sample_external(gpu, &texture, width, height) {
                                    Ok(sampled) => {
                                        let sampled_red = count_rgb(&sampled, colour("red"));
                                        println!(
                                            "shm readback self-check: {sampled_red}/{} red \
                                             pixels (frame has {red})",
                                            sampled.len() / 4
                                        );
                                        if sampled_red < red * 9 / 10 {
                                            diagnose_frame(&sampled, width, height);
                                            fail(
                                                "the GPU readback does not reproduce the frame it \
                                                 sampled (a readback bug would make every dma-buf \
                                                 sample meaningless)",
                                            );
                                        }
                                    }
                                    Err(error) => {
                                        fail(&format!("shm readback self-check failed: {error:#}"))
                                    }
                                }
                            }
                        }
                        transport = Some("shm");
                        if red > 0 && !want_dmabuf {
                            break;
                        }
                    }
                    #[cfg(feature = "dma-buf")]
                    SurfaceContent::Dmabuf(dmabuf) => {
                        transport = Some("dmabuf");
                        frame_bytes = dmabuf.width as usize * dmabuf.height as usize * 4;
                        // The full import path: dup the planes, import the
                        // memory, wrap it, and sample it back on the GPU.
                        if let Some(gpu) = &gpu {
                            match import_and_sample(gpu, dmabuf) {
                                Ok(bytes) => {
                                    red = count_rgb(&bytes, colour("red"));
                                    let total = (frame_bytes / 4).max(1);
                                    let ratio = red as f64 / total as f64;
                                    dmabuf_samples += 1;
                                    if dmabuf_samples == 1 {
                                        // The shapes of the first samples say whether a later
                                        // frame could be better; print the details once and
                                        // leave an image for eyeballing.
                                        diagnose_frame(&bytes, dmabuf.width, dmabuf.height);
                                        write_bmp(
                                            "/tmp/dmabuf-sample.bmp",
                                            &bytes,
                                            dmabuf.width,
                                            dmabuf.height,
                                        );
                                    }
                                    if ratio > best_ratio {
                                        best_ratio = ratio;
                                        best_bytes = Some(bytes);
                                    }
                                    if dmabuf_samples <= 3 || dmabuf_samples % 40 == 0 || ratio >= 0.8 {
                                        let (offset, stride) = dmabuf
                                            .planes
                                            .first()
                                            .map(|plane| (plane.offset, plane.stride))
                                            .unwrap_or((0, 0));
                                        println!(
                                            "dmabuf sample {dmabuf_samples}: {:.1}% red ({red}/{total}), \
                                             fourcc {:#x} modifier {:#x}, {}x{} plane offset {offset} stride {stride}",
                                            ratio * 100.0,
                                            dmabuf.fourcc,
                                            dmabuf.modifier,
                                            dmabuf.width,
                                            dmabuf.height,
                                        );
                                    }
                                    if ratio >= 0.8 {
                                        break;
                                    }
                                }
                                Err(error) => {
                                    eprintln!("dmabuf import/sample failed: {error:#}");
                                }
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let Some(size) = size_seen else {
            fail(&format!(
                "no frame committed within {:?} (WebKit never painted)",
                timeout
            ));
        };
        let elapsed = started.elapsed();

        if size != (width, height) {
            fail(&format!(
                "committed size {}x{} != configured {width}x{height}",
                size.0, size.1
            ));
        }
        match transport {
            Some("shm") if want_dmabuf => fail(
                "E2E_DMABUF=1 but only wl_shm frames arrived (WebKitGTK did not switch \
                 to the linux-dmabuf path)",
            ),
            Some("shm") => {}
            Some("dmabuf") if force_shm => {
                fail("E2E_FORCE_SHM=1 but the frame came from a dma-buf")
            }
            Some("dmabuf") => {}
            _ => fail("no frame arrived"),
        }
        let expected = width as usize * height as usize * 4;
        if frame_bytes != expected {
            fail(&format!(
                "frame has {frame_bytes} bytes, expected {expected} ({}x{}x4)",
                width, height
            ));
        }
        // The dmabuf path is judged by its best sampled frame: WebKit's first
        // committed dma-buf may still be a partial paint, and the test keeps
        // sampling until the page settles (or the timeout).
        let (red_pixels, total_pixels) = match transport {
            Some("dmabuf") => {
                let bytes = best_bytes.as_ref().unwrap_or_else(|| {
                    fail("no dma-buf frame could be imported and sampled")
                });
                let ratio = count_rgb(bytes, colour("red")) as f64 / (bytes.len() / 4).max(1) as f64;
                if ratio < 0.6 {
                    diagnose_frame(bytes, width, height);
                }
                (count_rgb(bytes, colour("red")), frame_bytes / 4)
            }
            _ => (red, frame_bytes / 4),
        };
        let ratio = red_pixels as f64 / total_pixels.max(1) as f64;
        if ratio < 0.5 {
            fail(&format!(
                "only {:.1}% of the frame is the page's red ({red_pixels}/{total_pixels})",
                ratio * 100.0
            ));
        }
        println!(
            "frame: {width}x{height} over {}, first commit after {:.2?} ({}), \
             {red_pixels}/{total_pixels} red pixels ({:.1}%), {} dma-buf sample(s)",
            transport.unwrap_or("none"),
            elapsed,
            first_commit_transport.unwrap_or("none"),
            ratio * 100.0,
            dmabuf_samples,
        );

        // JavaScript must run at all: the page turns green 400ms after load. The
        // reads go through the frame's own transport, so the dma-buf path is
        // checked exactly like the shm one.
        let green = wait_for_page_colour(
            &mut webviews,
            &live,
            viewport,
            gpu.as_ref(),
            |bytes| count_rgb(bytes, colour("green")) + count_rgb(bytes, colour("cyan")),
            10,
        );
        if green <= 1000 {
            if let Some(frame) = webviews.frame_image(0)
                && let Some(bytes) = frame_pixels(&frame, gpu.as_ref())
            {
                diagnose_frame(&bytes, width, height);
            }
            fail("the page's script never ran (no marker colour from the page)");
        }
        println!("script: the page's script ran ({green} green pixels)");

        // A forwarded click must reach the page: the body turns blue on click, so
        // wait for blue pixels. This caught the compositor never sending
        // `wl_pointer.frame`, which made every motion/click invisible to WebKit
        // (scroll worked because its handler already flushed a frame).
        webviews.set_focus(0, true);
        for event in [
                gpui_webview::WebInputEvent::PointerMotion { x: 160.0, y: 100.0 },
                gpui_webview::WebInputEvent::PointerButton {
                    x: 160.0,
                    y: 100.0,
                    button: gpui_webview::PointerButton::Left,
                    pressed: true,
                },
                gpui_webview::WebInputEvent::PointerButton {
                    x: 160.0,
                    y: 100.0,
                    button: gpui_webview::PointerButton::Left,
                    pressed: false,
                },
        ] {
            webviews.input(0, event);
        }
        let blue = wait_for_page_colour(
            &mut webviews,
            &live,
            viewport,
            gpu.as_ref(),
            |bytes| count_rgb(bytes, colour("blue")),
            10,
        );
        if blue <= 1000 {
            if let Some(frame) = webviews.frame_image(0)
                && let Some(bytes) = frame_pixels(&frame, gpu.as_ref())
            {
                diagnose_frame(&bytes, width, height);
            }
            fail("a forwarded click never reached the page (the page should turn blue)");
        }
        println!("click: the page reacted ({blue} blue pixels)");

        // And a forwarded key: the body turns purple on keydown.
        let a = gpui_kit::Keystroke {
            modifiers: Default::default(),
            key: "a".into(),
            key_char: Some("a".into()),
        };
        webviews.input(
            0,
            gpui_webview::WebInputEvent::Key {
                keystroke: a.clone(),
                pressed: true,
            },
        );
        let purple = wait_for_page_colour(
            &mut webviews,
            &live,
            viewport,
            gpu.as_ref(),
            |bytes| count_rgb(bytes, colour("purple")),
            5,
        );
        if purple <= 1000 {
            fail("a forwarded key never reached the page (the page should turn purple)");
        }
        println!("key: the page reacted ({purple} purple pixels)");
        // A real release: the page has a `keyup` handler now, and apps need it to
        // tell a tap from a hold.
        webviews.input(
            0,
            gpui_webview::WebInputEvent::Key {
                keystroke: a,
                pressed: false,
            },
        );
        let keyup = wait_for_page_colour(
            &mut webviews,
            &live,
            viewport,
            gpu.as_ref(),
            |bytes| count_rgb(bytes, colour("keyup")),
            5,
        );
        if keyup <= 1000 {
            fail("a key release never reached the page (no keyup reaction)");
        }
        println!("key: the release reached the page ({keyup} keyup pixels)");
        // …and the key must not repeat by itself: smithay's auto-repeat needs the
        // calloop timer this compositor does not drive. The page paints `#7f1e1e`
        // on every keydown after the first, so waiting out the repeat delay tells
        // a tap from a storm.
        std::thread::sleep(Duration::from_millis(1500));
        webviews.frame(&live, viewport, &[]);
        let storm = webviews
            .frame_image(0)
            .and_then(|frame| frame_pixels(&frame, gpu.as_ref()))
            .map(|bytes| count_rgb(&bytes, colour("storm")))
            .unwrap_or(0);
        if storm > 0 {
            fail("a forwarded key repeated by itself (the compositor's auto-repeat)");
        }
        println!("key: the press stayed a tap (no auto-repeat)");
        // A shifted symbol: GPUI names punctuation by its shifted keysym (`@`),
        // which the keycode table used to drop because it only knew `2`. Hold Shift
        // (a modifier *transition*, no key event) and press the key; the page must
        // interpret it as `@`. The keystroke itself carries `shift: false` — that
        // is exactly what GPUI sends (`keystroke_from_xkb` strips Shift from
        // punctuation) — so a handler that "reconciles" modifiers from it would
        // release Shift and type `2`.
        let shift = gpui_kit::Modifiers {
            shift: true,
            ..Default::default()
        };
        webviews.input(0, gpui_webview::WebInputEvent::Modifiers { modifiers: shift });
        webviews.input(
            0,
            gpui_webview::WebInputEvent::Key {
                keystroke: gpui_kit::Keystroke {
                    modifiers: Default::default(),
                    key: "@".into(),
                    key_char: Some("@".into()),
                },
                pressed: true,
            },
        );
        let at = wait_for_page_colour(
            &mut webviews,
            &live,
            viewport,
            gpu.as_ref(),
            |bytes| count_rgb(bytes, colour("at")),
            5,
        );
        if at <= 1000 {
            fail("a shifted symbol was dropped (the page never saw `@`)");
        }
        println!("key: shifted punctuation reaches the page ({at} pixels)");
        webviews.input(
            0,
            gpui_webview::WebInputEvent::Key {
                keystroke: gpui_kit::Keystroke {
                    modifiers: Default::default(),
                    key: "@".into(),
                    key_char: Some("@".into()),
                },
                pressed: false,
            },
        );
        webviews.input(
            0,
            gpui_webview::WebInputEvent::Modifiers {
                modifiers: Default::default(),
            },
        );

        let mut harness = Harness {
            webviews,
            live,
            viewport,
            gpu,
            width,
            height,
            html: html.to_string(),
        };
        // `E2E_ONLY=<name>` runs one named step; the prologue above always
        // runs, because a step needs an activated, committed page.
        let only = std::env::var("E2E_ONLY").ok();
        let steps: [(&str, fn(&mut Harness)); 7] = [
            ("pointer-leave", Harness::step_pointer_leave),
            ("popups", Harness::step_popups),
            ("menu", Harness::step_menu),
            ("wheel", Harness::step_wheel),
            ("resize", Harness::step_resize),
            ("navigation", Harness::step_navigation),
            ("storage", Harness::step_storage),
        ];
        let mut ran = 0;
        for (name, step) in steps {
            if let Some(only) = &only
                && only != name
            {
                continue;
            }
            println!("step: {name}");
            step(&mut harness);
            ran += 1;
        }
        if let Some(only) = &only
            && ran == 0
        {
            fail(&format!("E2E_ONLY={only} names no step"));
        }
        if let Some(dir) = owned_runtime {
            let _ = std::fs::remove_dir_all(dir);
        }
        println!("PASS: nested compositor committed a correctly sized frame and tore down");
    }

    /// How many pixels of a BGRA frame are exactly the RGB triple `rgb`
    /// (`0xRRGGBB`): the frame is BGRA in memory, so the byte order is reversed.
    fn count_rgb(bytes: &[u8], rgb: u32) -> usize {
        let (b, g, r) = (
            (rgb & 0xff) as u8,
            ((rgb >> 8) & 0xff) as u8,
            ((rgb >> 16) & 0xff) as u8,
        );
        bytes
            .chunks_exact(4)
            .filter(|pixel| pixel[0] == b && pixel[1] == g && pixel[2] == r)
            .count()
    }

    /// The colour `name` from the page's palette (see [`PAGE_COLOURS`]).
    fn colour(name: &str) -> u32 {
        PAGE_COLOURS
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .unwrap_or_else(|| panic!("unknown page colour {name}"))
            .1
    }

    /// The page's colours, by the name the assertions count them under. One table
    /// for both sides: the page string is checked against every entry at startup,
    /// so an edit to either side cannot silently drift.
    const PAGE_COLOURS: &[(&str, u32)] = &[
        ("red", 0xc81e1e),
        ("purple", 0xc81ec8),
        ("cyan", 0x1ec8c8),
        ("green", 0x1ec81e),
        ("blue", 0x1e1ec8),
        ("wheel", 0x1e1e7f),
        ("marker", 0x1e7fc8),
        ("keyup", 0xc87f1e),
        ("at", 0x7fc81e),
        ("storm", 0x7f1e1e),
        ("changed", 0xc81e7f),
    ];

    /// How many distinct RGBA values a frame holds (a blank frame has one).
    fn distinct_colours(bytes: &[u8]) -> usize {
        let mut seen: std::collections::HashSet<[u8; 4]> = Default::default();
        for pixel in bytes.chunks_exact(4) {
            seen.insert([pixel[0], pixel[1], pixel[2], pixel[3]]);
            if seen.len() > 64 {
                break;
            }
        }
        seen.len()
    }

    /// A frame's pixels, whichever transport carried them: shm is a straight read,
    /// dma-buf goes through the same import + readback the main frame uses.
    fn frame_pixels(
        frame: &gpui_webview::SurfaceFrame,
        gpu: Option<&Gpu>,
    ) -> Option<Vec<u8>> {
        #[cfg(not(feature = "dma-buf"))]
        let _ = gpu;
        match &frame.content {
            gpui_webview::SurfaceContent::Image(image) => {
                image.as_bytes(0).map(|bytes| bytes.to_vec())
            }
            #[cfg(feature = "dma-buf")]
            gpui_webview::SurfaceContent::Dmabuf(dmabuf) => {
                gpu.and_then(|gpu| import_and_sample(gpu, dmabuf).ok())
            }
        }
    }

    /// Pumps frames until the page shows more than 1000 pixels of a colour, or
    /// `secs` pass; returns the last count. Pixels are read only when the page
    /// commits something new: a dma-buf import per poll would be wasteful (and
    /// slow), while every repaint has to be looked at, since the reaction of
    /// interest may be immediately overwritten by the next test's.
    fn wait_for_page_colour(
        webviews: &mut gpui_webview::WebViews,
        live: &[(u64, gpui_webview::Placement)],
        viewport: gpui_kit::Bounds<gpui_kit::Pixels>,
        gpu: Option<&Gpu>,
        count: impl Fn(&[u8]) -> usize,
        secs: u64,
    ) -> usize {
        use std::time::{Duration, Instant};

        let until = Instant::now() + Duration::from_secs(secs);
        let mut last: Option<gpui_webview::SurfaceFrame> = None;
        let mut seen = 0;
        loop {
            webviews.frame(live, viewport, &[]);
            if let Some(frame) = webviews.frame_image(0) {
                let changed = last.as_ref().map_or(true, |last| !frame.same_as(last));
                if changed {
                    seen = frame_pixels(&frame, gpu)
                        .map(|bytes| count(&bytes))
                        .unwrap_or(0);
                    last = Some(frame);
                    if seen > 1000 {
                        return seen;
                    }
                }
            }
            if Instant::now() >= until {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// `E2E_WHEEL_CALIBRATE=1`: measures how far one forwarded axis event moves a
    /// page, per input unit, and prints the table. The compositor's axis value is
    /// treated by GDK/Wayland exactly like a real mouse's smooth scroll, so the
    /// calibration target is "one GPUI tick (3 lines × 20 px = 60 units) should
    /// carry as much input as one libinput wheel click (15 units, the protocol's
    /// `wl_pointer.axis` convention since libinput 1.19)". The mode prints its
    /// measurements and exits 0; it is independent of the current scale, so it can
    /// run before and after a change (in the container and on the GPU machine).
    fn wheel_calibration() {
        use std::time::{Duration, Instant};

        use gpui_kit::{Bounds, point, px, size};
        use gpui_webview::backend::nested::NestedBackend;
        use gpui_webview::{Config, Placement, Policy, Source, SurfaceSpec, WebViews};

        let (width, height) = (320u32, 200u32);
        // A 20000px page of 1px ruler lines every 40px from document y=6000, each
        // with a unique colour. The scroll offset after an event is read back from
        // a line's screen row: `scrollY = docY - row`.
        let html = "<!doctype html><html><body style=\"margin:0;background:#202020\">\
                    <script>\
                    for (var k = 0; k < 60; k++) {\
                    var d = document.createElement('div');\
                    var r = 20 + k * 3, g = 240 - k * 3, b = 40 + k * 2;\
                    d.style.cssText = 'position:absolute;left:0;top:' + (6000 + k * 40) + 'px;width:320px;height:1px;background:rgb(' + r + ',' + g + ',' + b + ')';\
                    document.body.appendChild(d);\
                    }\
                    document.body.style.height = '20000px';\
                    document.documentElement.scrollTop = 6000;\
                    </script></body></html>";
        const MARKER_BASE: i32 = 6000;
        let markers: Vec<(i32, [u8; 3])> = (0..60)
            .map(|k| {
                (
                    MARKER_BASE + k * 40,
                    [
                        (20 + k * 3) as u8,
                        (240 - k * 3) as u8,
                        (40 + k * 2) as u8,
                    ],
                )
            })
            .collect();

        // The cumulative movement of every visible marker, averaged. `None` when
        // no marker is on screen (the read failed).
        let measure = |frame: &gpui_webview::SurfaceFrame| -> Option<f64> {
            let image = frame.image()?;
            let bytes = image.as_bytes(0)?;
            let row_bytes = width as usize * 4;
            let mut found: Vec<f64> = Vec::new();
            for row in 0..height as usize {
                let start = row * row_bytes;
                let end = start + row_bytes;
                if end > bytes.len() {
                    break;
                }
                let row_pixels = &bytes[start..end];
                for (doc_y, [r, g, b]) in &markers {
                    let count = row_pixels
                        .chunks_exact(4)
                        .filter(|pixel| pixel[0] == *b && pixel[1] == *g && pixel[2] == *r)
                        .count();
                    if count > 250 {
                        found.push(f64::from(*doc_y) - row as f64 - f64::from(MARKER_BASE));
                    }
                }
            }
            if found.is_empty() {
                None
            } else {
                Some(found.iter().sum::<f64>() / found.len() as f64)
            }
        };

        let backend = NestedBackend::new()
            .unwrap_or_else(|error| panic!("could not start the compositor: {error}"));
        let mut webviews = WebViews::new(backend, Config::default());
        let spec = SurfaceSpec::new(
            Source::Html {
                html: html.to_string(),
                base: None,
            },
            Policy {
                scripts: true,
                ..Policy::default()
            },
            "wheel calibration",
        )
        .with_size(width, height);
        let viewport = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(width as f32), px(height as f32)),
        };
        let live = vec![(0u64, Placement::clipped_to(viewport, viewport))];
        webviews
            .activate(0, spec)
            .unwrap_or_else(|error| panic!("activate failed: {error}"));

        // Pump until the committed frame has not changed for 300 ms (WebKit's
        // smooth scroll animates; the markers only settle when it stops).
        let settle = |webviews: &mut WebViews| -> Option<gpui_webview::SurfaceFrame> {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut last: Option<gpui_webview::SurfaceFrame> = None;
            let mut stable = 0u32;
            loop {
                webviews.frame(&live, viewport, &[]);
                if let Some(frame) = webviews.frame_image(0) {
                    let changed = last.as_ref().map_or(true, |previous| !frame.same_as(previous));
                    stable = if changed { 0 } else { stable + 1 };
                    last = Some(frame);
                    if stable >= 15 {
                        break;
                    }
                }
                if Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            last
        };

        let first = settle(&mut webviews).as_ref().and_then(measure);
        println!(
            "wheel-calibrate: initial scroll offset {} (expected 0)",
            first
                .map(|offset| format!("{offset:+.1}px"))
                .unwrap_or_else(|| "unread".to_string())
        );

        // One event at a time through the same path the app uses (the axis arm
        // establishes the pointer focus itself). Cumulative movement is read from
        // the markers; each line reports this event's contribution.
        let baseline = first.unwrap_or(0.0);
        let mut previous = baseline;
        for dy in [-15.0, -15.0, -30.0, -60.0, -60.0, 60.0, 15.0, 15.0] {
            webviews.input(
                0,
                gpui_webview::WebInputEvent::PointerAxis {
                    x: 160.0,
                    y: 100.0,
                    dx: 0.0,
                    dy,
                },
            );
            std::thread::sleep(Duration::from_millis(80));
            let measured = settle(&mut webviews).as_ref().and_then(measure);
            let cumulative = measured.unwrap_or(previous) - baseline;
            println!(
                "wheel-calibrate: dy={dy:+6.1} -> cumulative {cumulative:+.1}px (this event {:+.1}px)",
                cumulative - (previous - baseline)
            );
            previous = measured.unwrap_or(previous);
        }
        println!("wheel-calibrate: done");
    }

    /// Forwards a whole click (motion, press, release) at page-local `(x, y)`.
    fn click_at(webviews: &mut gpui_webview::WebViews, x: f64, y: f64) {
        for event in [
            gpui_webview::WebInputEvent::PointerMotion { x, y },
            gpui_webview::WebInputEvent::PointerButton {
                x,
                y,
                button: gpui_webview::PointerButton::Left,
                pressed: true,
            },
            gpui_webview::WebInputEvent::PointerButton {
                x,
                y,
                button: gpui_webview::PointerButton::Left,
                pressed: false,
            },
        ] {
            webviews.input(0, event);
        }
    }

    /// Forwards a whole right click (motion, press, release) at page-local
    /// `(x, y)` — the gesture that asks for the engine's context menu.
    fn right_click_at(webviews: &mut gpui_webview::WebViews, x: f64, y: f64) {
        for event in [
            gpui_webview::WebInputEvent::PointerMotion { x, y },
            gpui_webview::WebInputEvent::PointerButton {
                x,
                y,
                button: gpui_webview::PointerButton::Right,
                pressed: true,
            },
            gpui_webview::WebInputEvent::PointerButton {
                x,
                y,
                button: gpui_webview::PointerButton::Right,
                pressed: false,
            },
        ] {
            webviews.input(0, event);
        }
    }

    /// Pumps frames until the popup list is empty (or non-empty, per `want`) and
    /// returns the last state seen. Bounded, so a client that gets stuck fails the
    /// test instead of hanging it.
    fn wait_for_popups(
        webviews: &mut gpui_webview::WebViews,
        live: &[(u64, gpui_webview::Placement)],
        viewport: gpui_kit::Bounds<gpui_kit::Pixels>,
        want: bool,
    ) -> Vec<gpui_webview::PopupFrame> {
        use std::time::{Duration, Instant};

        let until = Instant::now() + Duration::from_secs(10);
        loop {
            webviews.frame(live, viewport, &[]);
            let popups = webviews.popup_frames(0);
            if popups.is_empty() != want || Instant::now() >= until {
                return popups;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Writes a BGRA frame as an uncompressed 32-bit BMP (bottom-up rows), so the
    /// machine's screenshot tooling is not needed to look at an import.
    fn write_bmp(path: &str, bytes: &[u8], width: u32, height: u32) {
        let row_bytes = width as usize * 4;
        let pixel_bytes = row_bytes * height as usize;
        let file_size = 54 + pixel_bytes;
        let mut file = Vec::with_capacity(file_size);
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&(file_size as u32).to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
        file.extend_from_slice(&width.to_le_bytes());
        file.extend_from_slice(&height.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes()); // planes
        file.extend_from_slice(&32u16.to_le_bytes()); // bpp
        file.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        file.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        for row in (0..height as usize).rev() {
            let start = row * row_bytes;
            file.extend_from_slice(&bytes[start..start + row_bytes]);
        }
        match std::fs::write(path, &file) {
            Ok(()) => println!("dmabuf sample written to {path}"),
            Err(error) => eprintln!("writing {path}: {error}"),
        }
    }

    /// Prints where a sampled frame's red is missing: a per-band distribution and
    /// the most common colours. The failure modes this distinguishes: a partial
    /// paint (some bands black), a layout/format mix-up (garbage colours), or a
    /// stale/uninitialized buffer.
    fn diagnose_frame(bytes: &[u8], width: u32, height: u32) {
        let row_bytes = width as usize * 4;
        let rows = height as usize;
        let mut bands = String::new();
        for band in 0..4 {
            let start = band * (rows / 4).max(1) * row_bytes;
            if start >= bytes.len() {
                break;
            }
            let end = ((band + 1) * (rows / 4).max(1)).min(rows) * row_bytes;
            let slice = &bytes[start..end.min(bytes.len())];
            let red = count_rgb(slice, colour("red"));
            let total = (slice.len() / 4).max(1);
            bands.push_str(&format!(
                " band{band} {:.0}%",
                100.0 * red as f64 / total as f64
            ));
        }
        let mut counts: std::collections::HashMap<[u8; 4], usize> = Default::default();
        for pixel in bytes.chunks_exact(4) {
            *counts
                .entry([pixel[0], pixel[1], pixel[2], pixel[3]])
                .or_default() += 1;
        }
        let mut top: Vec<_> = counts.into_iter().collect();
        top.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        let top: Vec<String> = top
            .iter()
            .take(4)
            .map(|(pixel, count)| {
                format!(
                    "#{:02x}{:02x}{:02x}{:02x} x{count}",
                    pixel[2], pixel[1], pixel[0], pixel[3]
                )
            })
            .collect();
        eprintln!(
            "dmabuf frame diagnostics:{bands}; top colours: {}",
            top.join(", ")
        );
        // A coarse map: one character per cell centre, so a shear, a partial paint
        // or a colour-space problem is visible without any image tooling.
        let cols = 32usize;
        let rows = 16usize;
        for row in 0..rows {
            let mut line = String::with_capacity(cols);
            for col in 0..cols {
                let x = (col * width as usize / cols).min(width as usize - 1);
                let y = (row * height as usize / rows).min(height as usize - 1);
                let i = (y * width as usize + x) * 4;
                if i + 3 >= bytes.len() {
                    line.push(' ');
                    continue;
                }
                let (b, g, r) = (bytes[i], bytes[i + 1], bytes[i + 2]);
                line.push(if r > 180 && g < 90 && b < 90 {
                    'R'
                } else if r > 200 && g > 200 && b > 200 {
                    'W'
                } else if r < 40 && g < 40 && b < 40 {
                    'K'
                } else {
                    '?'
                });
            }
            eprintln!("dmabuf map: {line}");
        }
    }

    /// A `wgpu` device with no window and no display, for the capability query and
    /// the dma-buf import. `None` when no adapter can be opened.
    #[cfg(feature = "dma-buf")]
    struct HeadlessGpu {
        device: gpui_wgpu::wgpu::Device,
        queue: gpui_wgpu::wgpu::Queue,
        adapter: gpui_wgpu::wgpu::Adapter,
    }

    #[cfg(feature = "dma-buf")]
    fn headless_gpu() -> Option<HeadlessGpu> {
        use gpui_wgpu::wgpu;
        use std::future::Future;
        use std::pin::pin;
        use std::task::{Context, Poll, Wake, Waker};
        use std::sync::Arc;

        /// A minimal executor: this test is single-threaded and the adapter/device
        /// futures complete on the first or second poll.
        struct ThreadWaker(std::thread::Thread);
        impl Wake for ThreadWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }

        fn block_on<T>(future: impl Future<Output = T>) -> T {
            let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
            let mut cx = Context::from_waker(&waker);
            let mut future = pin!(future);
            loop {
                match Future::poll(future.as_mut(), &mut cx) {
                    Poll::Ready(value) => return value,
                    Poll::Pending => std::thread::park(),
                }
            }
        }

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapters = block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
        let adapter = adapters.into_iter().next()?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;
        Some(HeadlessGpu {
            device,
            queue,
            adapter,
        })
    }

    /// Imports `frame` as a texture, draws it into a render target with a
    /// full-screen triangle, and reads the target back. This is the same import the
    /// app performs; the readback is what lets the test assert pixels.
    #[cfg(feature = "dma-buf")]
    fn import_and_sample(
        gpu: &HeadlessGpu,
        frame: &gpui_webview::DmaBufFrame,
    ) -> anyhow::Result<Vec<u8>> {
        let descriptor = gpui_kit::DmaBufDescriptor {
            width: frame.width,
            height: frame.height,
            fourcc: frame.fourcc,
            modifier: frame.modifier,
            planes: frame
                .planes
                .iter()
                .map(|plane| gpui_kit::DmaBufPlane {
                    fd: std::os::fd::AsRawFd::as_raw_fd(&*plane.fd),
                    offset: plane.offset,
                    stride: plane.stride,
                })
                .collect(),
        };
        let external = gpui_wgpu::dmabuf::import(&gpu.device, &descriptor)?;
        sample_external(gpu, &external, frame.width, frame.height)
    }

    /// Samples an [`gpui_kit::ExternalTexture`] through the readback pass; shared by
    /// the dma-buf import and the shm self-check.
    #[cfg(feature = "dma-buf")]
    fn sample_external(
        gpu: &HeadlessGpu,
        external: &gpui_kit::ExternalTexture,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Vec<u8>> {
        let surface = external
            .downcast::<gpui_wgpu::SurfaceTexture>()
            .ok_or_else(|| anyhow::anyhow!("the texture did not wrap a surface texture"))?;
        readback_texture(gpu, &surface.texture, width, height)
    }

    #[cfg(feature = "dma-buf")]
    fn readback_texture(
        gpu: &HeadlessGpu,
        texture: &gpui_wgpu::wgpu::Texture,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Vec<u8>> {
        use gpui_wgpu::wgpu;

        // The readback target; linear sampling would blur cell edges, nearest keeps
        // the page colour exact. Two explicit triangles (a strip's second triangle
        // came out missing on ANV/Mesa 26): (0,0),(1,0),(0,1) and (1,0),(1,1),(0,1).
        const SHADER: &str = r#"
            @group(0) @binding(0) var t_src: texture_2d<f32>;
            @group(0) @binding(1) var s_src: sampler;

            struct Varying {
                @builtin(position) position: vec4<f32>,
                @location(0) uv: vec2<f32>,
            }
            @vertex
            fn vs_main(@builtin(vertex_index) index: u32) -> Varying {
                var uv: vec2<f32>;
                if (index == 0u) { uv = vec2<f32>(0.0, 0.0); }
                else if (index == 1u) { uv = vec2<f32>(1.0, 0.0); }
                else if (index == 2u) { uv = vec2<f32>(0.0, 1.0); }
                else if (index == 3u) { uv = vec2<f32>(1.0, 0.0); }
                else if (index == 4u) { uv = vec2<f32>(1.0, 1.0); }
                else { uv = vec2<f32>(0.0, 1.0); }
                var out = Varying();
                out.position = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
                out.uv = uv;
                return out;
            }
            @fragment
            fn fs_main(in: Varying) -> @location(0) vec4<f32> {
                return textureSampleLevel(t_src, s_src, in.uv, 0.0);
            }
        "#;

        let module = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("dmabuf-readback"),
                source: wgpu::ShaderSource::Wgsl(SHADER.into()),
            });
        let layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("dmabuf-readback"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                        count: None,
                    },
                ],
            });
        let pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("dmabuf-readback"),
                layout: Some(
                    &gpu.device
                        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                            label: Some("dmabuf-readback"),
                            bind_group_layouts: &[Some(&layout)],
                            immediate_size: 0,
                        }),
                ),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Bgra8Unorm,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

        let source_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("dmabuf-readback"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("dmabuf-readback"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&source_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dmabuf-readback-target"),
            size: wgpu::Extent3d {
                width: width,
                height: height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        // `copy_texture_to_buffer` needs 256-byte aligned rows.
        let unpadded_row = width * 4;
        let padded_row = unpadded_row.div_ceil(256) * 256;
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dmabuf-readback-buffer"),
            size: u64::from(padded_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("dmabuf-readback"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("dmabuf-readback"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width: width,
                height: height,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit(std::iter::once(encoder.finish()));

        let (sender, receiver) = std::sync::mpsc::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
        let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
        receiver
            .recv()
            .map_err(|_| anyhow::anyhow!("the readback callback never ran"))?
            .map_err(|error| anyhow::anyhow!("mapping the readback buffer: {error}"))?;

        let mapped = readback.slice(..).get_mapped_range();
        let mut pixels = Vec::with_capacity((unpadded_row * height) as usize);
        for row in 0..height {
            let start = (row * padded_row) as usize;
            pixels.extend_from_slice(&mapped[start..start + unpadded_row as usize]);
        }
        drop(mapped);
        readback.unmap();
        Ok(pixels)
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

