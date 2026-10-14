//! The nested Linux compositor end to end (EMBEDS.md §10.3, DMABUF_PLAN.md).
//!
//! One page, every path asserted by the page's own reaction; the first commit is
//! a fixed prelude, `E2E_ONLY=<step>` runs one named step on top, and
//! `E2E_DMABUF=1` re-runs everything over linux-dmabuf.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("nested_e2e is Linux-only; nothing to do.");
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(dmabuf)]
#[path = "support/nested_gpu.rs"]
mod nested_gpu;

#[cfg(target_os = "linux")]
mod linux {
    /// The GPU context the dma-buf path imports through (`dma-buf` feature only).
    #[cfg(dmabuf)]
    use super::nested_gpu::{HeadlessGpu, headless_gpu, import_and_sample, sample_external};
    #[cfg(dmabuf)]
    type Gpu = HeadlessGpu;
    #[cfg(not(dmabuf))]
    type Gpu = ();

    use gpui_kit::{Bounds, point, px, size};
    use gpui_webview::backend::nested::NestedBackend;
    use gpui_webview::{Config, Placement, Policy, Source, SurfaceContent, SurfaceSpec, WebViews};
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};
    /// The live state a step works on; the first commit always runs first.
    struct Harness {
        webviews: WebViews,
        live: Vec<(u64, Placement)>,
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
        /// One paint pass plus the main-context pump it drives.
        fn pump(&mut self) {
            self.webviews.frame(&self.live, self.viewport, &[]);
            self.webviews.pump();
        }
        /// The current frame's pixels, through the frame's own transport.
        fn pixels(&mut self) -> Option<Vec<u8>> {
            let frame = self.webviews.frame_image(0)?;
            frame_pixels(&frame, self.gpu.as_ref())
        }
        /// How many exact-`name` pixels the current frame shows.
        fn count(&mut self, name: &str) -> usize {
            self.pixels()
                .map(|bytes| count_rgb(&bytes, colour(name)))
                .unwrap_or(0)
        }
        /// Pumps until `check` holds (or `secs` pass); returns whether it held.
        fn until(&mut self, secs: u64, mut check: impl FnMut(&mut Self) -> bool) -> bool {
            let until = Instant::now() + Duration::from_secs(secs);
            loop {
                self.pump();
                if check(self) {
                    return true;
                }
                if Instant::now() >= until {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        /// Waits for a page reaction (dumping the frame on failure).
        fn expect(
            &mut self,
            label: &str,
            name: &str,
            secs: u64,
            count: impl Fn(&[u8]) -> usize,
        ) -> usize {
            let seen = wait_for_page_colour(
                &mut self.webviews,
                &self.live,
                self.viewport,
                self.gpu.as_ref(),
                count,
                secs,
            );
            if seen <= 1000 {
                if let Some(bytes) = self.pixels() {
                    dump_frame(label, &bytes, self.width, self.height);
                }
                fail(&format!(
                    "{label}: the page never showed its {name} reaction"
                ));
            }
            println!("{label}: {seen} {name} pixels");
            seen
        }
        /// Gives the page the keyboard (idempotent; keys are focus-gated).
        fn focus(&mut self) {
            self.webviews.set_focus(0, true);
        }

        /// A whole click (motion, press, release) at page-local `(x, y)`.
        fn click(&mut self, button: gpui_webview::PointerButton, x: f64, y: f64) {
            click_at(&mut self.webviews, button, x, y);
        }
        /// A key press/release for `key` (its keysym name), no modifiers.
        fn key(&mut self, key: &str, pressed: bool) {
            self.webviews.input(
                0,
                gpui_webview::WebInputEvent::Key {
                    keystroke: gpui_kit::Keystroke {
                        modifiers: Default::default(),
                        key: key.into(),
                        key_char: Some(key.into()),
                    },
                    pressed,
                },
            );
        }
        /// Pointer motion at page-local `(x, y)`.
        fn motion(&mut self, x: f64, y: f64) {
            self.webviews
                .input(0, gpui_webview::WebInputEvent::PointerMotion { x, y });
        }
        /// The pointer left the surface.
        fn leave(&mut self) {
            self.webviews
                .input(0, gpui_webview::WebInputEvent::PointerLeave);
        }
        /// One scroll axis update at page-local `(x, y)`.
        fn axis(&mut self, x: f64, y: f64, dx: f64, dy: f64) {
            self.webviews
                .input(0, gpui_webview::WebInputEvent::PointerAxis { x, y, dx, dy });
        }
        /// A modifier transition.
        fn mods(&mut self, modifiers: gpui_kit::Modifiers) {
            self.webviews
                .input(0, gpui_webview::WebInputEvent::Modifiers { modifiers });
        }

        fn step_script(&mut self) {
            // T26: JS must run at all; the page turns green 400 ms after load.
            self.expect("script", "green", 10, |bytes| {
                count_rgb(bytes, colour("green")) + count_rgb(bytes, colour("cyan"))
            });
        }
        fn step_click(&mut self) {
            // A forwarded click must reach the page (T7: motion needs its own frame).
            self.focus();
            self.click(gpui_webview::PointerButton::Left, 160.0, 100.0);
            self.expect("click", "blue", 10, |bytes| {
                count_rgb(bytes, colour("blue"))
            });
        }
        fn step_key(&mut self) {
            self.focus();
            // Real press and release: `purple` on keydown, `keyup` on keyup (T9).
            self.key("a", true);
            self.expect("key", "purple", 5, |bytes| {
                count_rgb(bytes, colour("purple"))
            });
            self.key("a", false);
            self.expect("keyup", "keyup", 5, |bytes| {
                count_rgb(bytes, colour("keyup"))
            });
            // …and the key must not self-repeat (`storm` paints on later keydowns).
            std::thread::sleep(Duration::from_millis(1500));
            if self.count("storm") > 0 {
                fail("a forwarded key repeated by itself (the compositor's auto-repeat)");
            }
            println!("key: the press stayed a tap (no auto-repeat)");
        }
        fn step_shifted_at(&mut self) {
            self.focus();
            // T9: GPUI strips Shift from punctuation; it arrives as a transition.
            let shift = gpui_kit::Modifiers {
                shift: true,
                ..Default::default()
            };
            self.mods(shift);
            self.key("@", true);
            self.expect("at", "at", 5, |bytes| count_rgb(bytes, colour("at")));
            self.key("@", false);
            self.mods(Default::default());
        }

        fn step_pointer_leave(&mut self) {
            // A leave releases the protocol pointer focus (see `GPUI_WEBVIEW_DEBUG=1`).
            self.motion(120.0, 80.0);
            for _ in 0..5 {
                self.pump();
                let _ = self.webviews.frame_image(0);
                std::thread::sleep(Duration::from_millis(20));
            }
            self.leave();
            for _ in 0..5 {
                self.pump();
                let _ = self.webviews.frame_image(0);
                std::thread::sleep(Duration::from_millis(20));
            }
            println!("pointer: a leave released the pointer focus (see the debug log)");
        }
        fn step_popups(&mut self) {
            // The `<select>`'s dropdown is an `xdg_popup` the compositor composites.
            let select = (245.0, 132.0);
            self.click(gpui_webview::PointerButton::Left, select.0, select.1);
            let popups = wait_for_popups(&mut self.webviews, &self.live, self.viewport, true);
            let Some(popup) = popups.first().cloned() else {
                fail("clicking the <select> never produced a native popup (xdg_popup)");
            };
            println!(
                "popup: {} at {:?} size {:?}",
                popup.id, popup.position, popup.frame.size
            );
            if popup.position.0 < 0
                || popup.position.1 < 0
                || popup.position.0 + popup.frame.size.0 as i32 > self.width as i32
                || popup.position.1 + popup.frame.size.1 as i32 > self.height as i32
            {
                fail("the popup was not fitted inside the page's rect");
            }
            let Some(first_pixels) = frame_pixels(&popup.frame, self.gpu.as_ref()) else {
                fail("the popup frame carried no readable pixels");
            };
            let distinct = distinct_colours(&first_pixels);
            if distinct < 3 {
                dump_frame(
                    "popup",
                    &first_pixels,
                    popup.frame.size.0,
                    popup.frame.size.1,
                );
                fail(&format!(
                    "the popup frame looks blank ({distinct} distinct colours)"
                ));
            }
            println!("popup: {distinct} distinct colours inside it");

            // Hovering highlights an item (a miss is reported, not failed). T27:
            // measure the freshest frame; 60% down is item two.
            let item_point = |popup: &gpui_webview::PopupFrame| -> (f64, f64) {
                (
                    popup.position.0 as f64 + popup.frame.size.0 as f64 / 2.0,
                    popup.position.1 as f64 + popup.frame.size.1 as f64 * 0.6,
                )
            };
            let hover = item_point(&popup);
            self.motion(hover.0, hover.1);
            let hovered = self.until(5, |h| {
                h.webviews
                    .popup_frames(0)
                    .first()
                    .and_then(|popup| frame_pixels(&popup.frame, h.gpu.as_ref()))
                    .is_some_and(|current| current != first_pixels)
            });
            if !hovered {
                eprintln!("NOTE: hovering an item did not repaint the popup (theme?)");
            }

            // Choosing the *second* item changes the page (`onchange`), which only
            // a click on the popup's own surface can cause.
            self.pump();
            let measure = self
                .webviews
                .popup_frames(0)
                .first()
                .cloned()
                .unwrap_or_else(|| popup.clone());
            let item = item_point(&measure);
            self.click(gpui_webview::PointerButton::Left, item.0, item.1);
            let popups = wait_for_popups(&mut self.webviews, &self.live, self.viewport, false);
            if !popups.is_empty() {
                fail("the popup never closed after an item was clicked");
            }
            self.expect("popup", "changed", 10, |bytes| {
                count_rgb(bytes, colour("changed"))
            });

            // Reopen and dismiss outside; T8: the *next* click must still reach the page.
            self.click(gpui_webview::PointerButton::Left, select.0, select.1);
            let popups = wait_for_popups(&mut self.webviews, &self.live, self.viewport, true);
            if popups.is_empty() {
                fail("the popup did not reopen for the dismissal check");
            }
            self.click(gpui_webview::PointerButton::Left, 20.0, 20.0);
            let popups = wait_for_popups(&mut self.webviews, &self.live, self.viewport, false);
            if !popups.is_empty() {
                fail("an outside click did not dismiss the popup");
            }
            println!("popup: dismissed by an outside click");
            self.click(gpui_webview::PointerButton::Left, 150.0, 60.0);
            self.expect("popup", "blue", 5, |bytes| count_rgb(bytes, colour("blue")));
        }
        fn step_menu(&mut self) {
            // Right-clicking shows the engine's menu, trimmed (T23), composited.
            self.click(gpui_webview::PointerButton::Right, 40.0, 20.0); // over the <h1>
            let menus = wait_for_popups(&mut self.webviews, &self.live, self.viewport, true);
            let Some(menu) = menus.first().cloned() else {
                fail("right-clicking the page never produced the engine's context menu");
            };
            println!(
                "menu: {} at {:?} size {:?}",
                menu.id, menu.position, menu.frame.size
            );
            let Some(menu_pixels) = frame_pixels(&menu.frame, self.gpu.as_ref()) else {
                fail("the context menu frame carried no readable pixels");
            };
            let distinct = distinct_colours(&menu_pixels);
            if distinct < 3 {
                dump_frame("menu", &menu_pixels, menu.frame.size.0, menu.frame.size.1);
                fail(&format!(
                    "the context menu looks blank ({distinct} distinct colours)"
                ));
            }
            println!("menu: {distinct} distinct colours inside it");
            // An outside press dismisses it (clear of the menu and scrollbar).
            self.click(gpui_webview::PointerButton::Left, 180.0, 180.0);
            let menus = wait_for_popups(&mut self.webviews, &self.live, self.viewport, false);
            if !menus.is_empty() {
                fail("an outside click did not dismiss the engine's context menu");
            }
            println!("menu: the engine's context menu composites and dismisses");
        }
        fn step_wheel(&mut self) {
            // A wheel over the iframe scrolls it without focus: one tick in GPUI's
            // convention (negative `dy` is down) must move the marker, and the
            // reverse tick must bring it back (T12).
            self.webviews.set_focus(0, false);
            self.motion(160.0, 100.0);
            if self.count("marker") > 0 {
                fail("the scroll marker was visible before the wheel");
            }
            self.axis(160.0, 100.0, 0.0, -60.0);
            if !self.until(10, |h| h.count("marker") > 200) {
                eprintln!(
                    "NOTE: the DOM wheel reaction is {} pixels",
                    self.count("wheel")
                );
                fail("a single wheel tick did not scroll the page (sign/first tick)");
            }
            println!(
                "wheel: one tick scrolled the page ({} marker pixels)",
                self.count("marker")
            );
            self.axis(160.0, 100.0, 0.0, 60.0);
            if !self.until(10, |h| h.count("marker") == 0) {
                fail("the reverse wheel tick did not scroll the page back up");
            }
            println!("wheel: the reverse tick scrolled back");
        }
        fn step_resize(&mut self) {
            // A resize reconfigures the client; the frame tracks the new size.
            let smaller = (240u32, 150u32);
            let small_viewport = Bounds {
                origin: point(px(0.), px(0.)),
                size: gpui_kit::size(px(smaller.0 as f32), px(smaller.1 as f32)),
            };
            let small_live = vec![(0u64, Placement::clipped_to(small_viewport, small_viewport))];
            let until = Instant::now() + Duration::from_secs(10);
            let mut resized = false;
            while Instant::now() < until {
                self.webviews.frame(&small_live, small_viewport, &[]);
                self.webviews.pump();
                if self
                    .webviews
                    .frame_image(0)
                    .is_some_and(|frame| frame.size == smaller)
                {
                    resized = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if !resized {
                fail("the client never recommitted at the resized size");
            }
            println!(
                "resize: the client re-laid out to {}x{}",
                smaller.0, smaller.1
            );
            // Back to the original size for the remaining checks.
            let until = Instant::now() + Duration::from_secs(10);
            while Instant::now() < until {
                self.pump();
                if self
                    .webviews
                    .frame_image(0)
                    .is_some_and(|frame| frame.size == (self.width, self.height))
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        fn step_navigation(&mut self) {
            // Under `OpenExternal` a remote navigation is refused and reported; the
            // URL is never fetched.
            self.webviews
                .activate(
                    0,
                    SurfaceSpec::new(
                        Source::Url("https://example.com/next".into()),
                        Policy::default(),
                        "navigation",
                    )
                    .with_size(self.width, self.height),
                )
                .unwrap_or_else(|error| fail(&format!("navigating to the URL failed: {error}")));
            let navigated = self.until(5, |h| {
                h.webviews.events().iter().any(|event| {
                    matches!(
                        event,
                        gpui_webview::WebViewEvent::OpenExternal { url, .. }
                            if url.contains("example.com/next")
                    )
                })
            });
            if !navigated {
                fail("a remote navigation under OpenExternal was not refused and reported");
            }
            println!("navigation: the remote navigation was refused and reported as OpenExternal");
        }
        /// `Source::Html` spec for this page, sized like the viewport.
        fn html_spec(&self, policy: Policy, title: &'static str) -> SurfaceSpec {
            SurfaceSpec::new(
                Source::Html {
                    html: self.html.clone(),
                    base: None,
                },
                policy,
                title,
            )
            .with_size(self.width, self.height)
        }
        fn step_storage(&mut self) {
            // `Policy::media()` is `Storage::Shared` (one `WebContext` per embed);
            // switching storage recreates the engine, exercised here.
            let spec = self.html_spec(Policy::media(), "shared");
            self.webviews.activate(0, spec).unwrap_or_else(|error| {
                fail(&format!("shared-storage activation failed: {error}"))
            });
            if !self.until(10, |h| h.webviews.frame_image(0).is_some()) {
                fail("a shared-storage engine never committed a frame");
            }
            println!("storage: a shared-context engine loaded and committed");
            // Re-activating with the same spec must reuse the engine.
            let spec = self.html_spec(
                Policy {
                    scripts: true,
                    ..Policy::default()
                },
                "nested e2e",
            );
            self.webviews
                .activate(0, spec)
                .unwrap_or_else(|error| fail(&format!("re-activate failed: {error}")));
            // Deactivation must tear the client down and drop its frame.
            self.webviews.deactivate(0);
            if !self.until(5, |h| h.webviews.frame_image(0).is_none()) {
                fail("the frame survived deactivation");
            }
        }
    }
    /// The first committed frame, whichever transport carried it.
    struct FirstFrame {
        elapsed: Duration,
        transport: &'static str,
        first_transport: &'static str,
        red: usize,
        total: usize,
        samples: usize,
    }
    /// Fixed prelude: the first real frame (GTK commits empty frames first), the transport.
    #[allow(clippy::too_many_arguments)]
    fn first_commit(
        webviews: &mut WebViews,
        live: &[(u64, Placement)],
        viewport: Bounds<gpui_kit::Pixels>,
        gpu: Option<&Gpu>,
        width: u32,
        height: u32,
        want_dmabuf: bool,
        force_shm: bool,
        timeout: Duration,
    ) -> FirstFrame {
        #[cfg(not(dmabuf))]
        let _ = gpu;
        let started = Instant::now();
        let mut first_commit = None;
        let mut first_commit_transport = None;
        let mut transport = None;
        let mut red = 0usize;
        let mut frame_bytes = 0usize;
        let mut size_seen = None;
        // Only the dma-buf arms mutate/read these; the feature-off tail ignores them.
        #[allow(unused_mut, unused_variables)]
        let mut dmabuf_samples = 0usize;
        #[allow(unused_mut, unused_variables)]
        let mut best_ratio = 0.0f64;
        #[allow(unused_mut, unused_variables)]
        let mut best_bytes: Option<Vec<u8>> = None;
        #[allow(unused_mut, unused_variables)]
        let mut self_checked = false;
        while started.elapsed() < timeout {
            webviews.frame(live, viewport, &[]);
            webviews.pump();
            if let Some(frame) = webviews.frame_image(0) {
                if first_commit.is_none() {
                    first_commit = Some(started.elapsed());
                    first_commit_transport = Some(match frame.content {
                        SurfaceContent::Image(_) => "shm",
                        #[cfg(dmabuf)]
                        SurfaceContent::Dmabuf(_) => "dmabuf",
                    });
                }
                size_seen = Some(frame.size);
                match &frame.content {
                    SurfaceContent::Image(image) => {
                        if let Some(bytes) = image.as_bytes(0) {
                            red = count_rgb(bytes, colour("red"));
                            frame_bytes = bytes.len();
                            // T28: sample known-good shm bytes through the dma-buf pass.
                            #[cfg(dmabuf)]
                            if let Some(gpu) = gpu
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
                                            dump_frame("shm-readback", &sampled, width, height);
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
                    #[cfg(dmabuf)]
                    SurfaceContent::Dmabuf(dmabuf) => {
                        transport = Some("dmabuf");
                        frame_bytes = dmabuf.width as usize * dmabuf.height as usize * 4;
                        if let Some(gpu) = gpu {
                            match import_and_sample(gpu, dmabuf) {
                                Ok(bytes) => {
                                    red = count_rgb(&bytes, colour("red"));
                                    let total = (frame_bytes / 4).max(1);
                                    let ratio = red as f64 / total as f64;
                                    dmabuf_samples += 1;
                                    if ratio > best_ratio {
                                        best_ratio = ratio;
                                        best_bytes = Some(bytes);
                                    }
                                    if dmabuf_samples <= 3
                                        || dmabuf_samples % 40 == 0
                                        || ratio >= 0.8
                                    {
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
                "no frame committed within {timeout:?} (WebKit never painted)"
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
        // The dma-buf path is judged by its best sampled frame (the first commit
        // may be a partial paint).
        let (red_pixels, total_pixels) = match transport {
            Some("dmabuf") => {
                let bytes = best_bytes
                    .as_ref()
                    .unwrap_or_else(|| fail("no dma-buf frame could be imported and sampled"));
                (count_rgb(bytes, colour("red")), frame_bytes / 4)
            }
            _ => (red, frame_bytes / 4),
        };
        let ratio = red_pixels as f64 / total_pixels.max(1) as f64;
        if ratio < 0.5 {
            if let Some(bytes) = best_bytes.as_ref() {
                dump_frame("dmabuf", bytes, width, height);
            }
            fail(&format!(
                "only {:.1}% of the frame is the page's red ({red_pixels}/{total_pixels})",
                ratio * 100.0
            ));
        }
        FirstFrame {
            elapsed,
            transport: transport.unwrap_or("none"),
            first_transport: first_commit_transport.unwrap_or("none"),
            red: red_pixels,
            total: total_pixels,
            samples: dmabuf_samples,
        }
    }

    pub(super) fn main() {
        let (width, height) = (320u32, 200u32);
        // No network: a red page, so a committed frame is self-evidently the page.
        // T26: handlers live on `document` (a `<body>` handler misses lower clicks).
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
                    </body></html>"
            .to_string();
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
            // GTK3 WebKitGTK honours both switches; set both for clarity.
            unsafe {
                std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
                std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
            }
        }
        // The Wayland socket needs `XDG_RUNTIME_DIR`; make one if there is none.
        let owned_runtime = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => None,
            _ => {
                let dir = std::env::temp_dir().join(format!("nested-e2e-{}", std::process::id()));
                std::fs::create_dir_all(&dir).expect("create XDG_RUNTIME_DIR");
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                    .expect("chmod 0700 XDG_RUNTIME_DIR");
                unsafe {
                    std::env::set_var("XDG_RUNTIME_DIR", &dir);
                }
                Some(dir)
            }
        };

        // The compositor advertises exactly what the renderer can import, and the
        // test imports/samples through the same code the app uses (shm-only without
        // `dma-buf`).
        #[cfg(dmabuf)]
        let gpu = headless_gpu();
        #[cfg(not(dmabuf))]
        let gpu: Option<Gpu> = None;
        #[cfg(dmabuf)]
        let capabilities = if force_shm {
            None
        } else {
            gpu.as_ref()
                .and_then(|gpu| gpui_wgpu::dmabuf::capabilities(&gpu.adapter))
        };
        #[cfg(not(dmabuf))]
        let capabilities: Option<()> = None;
        #[cfg(dmabuf)]
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
        #[cfg(not(dmabuf))]
        {
            let _ = force_shm;
            println!("dmabuf: not built (feature `dma-buf` off); wl_shm only");
        }
        if want_dmabuf && capabilities.is_none() {
            #[cfg(dmabuf)]
            fail("E2E_DMABUF=1 but this machine has no importable dma-buf formats");
            #[cfg(not(dmabuf))]
            fail("E2E_DMABUF=1 requires `--features nested,dma-buf` (patched GPUI)");
        }

        #[cfg(dmabuf)]
        let backend = NestedBackend::new_with_dmabuf(capabilities)
            .unwrap_or_else(|error| fail(&format!("could not start the compositor: {error}")));
        #[cfg(not(dmabuf))]
        let backend = NestedBackend::new()
            .unwrap_or_else(|error| fail(&format!("could not start the compositor: {error}")));
        let mut webviews = WebViews::new(backend, Config::default());
        let spec = SurfaceSpec::new(
            Source::Html {
                html: html.clone(),
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

        // Fixed prelude: the first committed frame and its transport.
        let first = first_commit(
            &mut webviews,
            &live,
            viewport,
            gpu.as_ref(),
            width,
            height,
            want_dmabuf,
            force_shm,
            timeout,
        );
        let ratio = first.red as f64 / first.total.max(1) as f64;
        println!(
            "frame: {width}x{height} over {}, first commit after {:.2?} ({}), \
             {}/{} red pixels ({:.1}%), {} dma-buf sample(s)",
            first.transport,
            first.elapsed,
            first.first_transport,
            first.red,
            first.total,
            ratio * 100.0,
            first.samples,
        );

        let mut harness = Harness {
            webviews,
            live,
            viewport,
            gpu,
            width,
            height,
            html,
        };
        let steps: [(&str, fn(&mut Harness)); 11] = [
            ("script", Harness::step_script),
            ("click", Harness::step_click),
            ("key", Harness::step_key),
            ("shifted-at", Harness::step_shifted_at),
            ("pointer-leave", Harness::step_pointer_leave),
            ("popups", Harness::step_popups),
            ("menu", Harness::step_menu),
            ("wheel", Harness::step_wheel),
            ("resize", Harness::step_resize),
            ("navigation", Harness::step_navigation),
            ("storage", Harness::step_storage),
        ];
        let only = std::env::var("E2E_ONLY").ok();
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
    /// How many pixels of a BGRA frame are exactly `rgb` (`0xRRGGBB,` byte-reversed).
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
    /// The page's colours. One table for both sides: the page string is
    /// checked against every entry at startup, so an edit cannot drift.
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
    /// A frame's pixels, whichever transport carried them: shm is a straight
    /// read, dma-buf goes through the import + readback.
    fn frame_pixels(frame: &gpui_webview::SurfaceFrame, gpu: Option<&Gpu>) -> Option<Vec<u8>> {
        #[cfg(not(dmabuf))]
        let _ = gpu;
        match &frame.content {
            gpui_webview::SurfaceContent::Image(image) => {
                image.as_bytes(0).map(|bytes| bytes.to_vec())
            }
            #[cfg(dmabuf)]
            gpui_webview::SurfaceContent::Dmabuf(dmabuf) => {
                gpu.and_then(|gpu| import_and_sample(gpu, dmabuf).ok())
            }
        }
    }
    /// Pumps frames until the page shows more than 1000 pixels of a colour, or
    /// `secs` pass; returns the last count. Pixels are read only when the page
    /// commits something new: every repaint must be looked at (the reaction of
    /// interest may be overwritten), but a dma-buf import per poll would be
    /// wasteful.
    fn wait_for_page_colour(
        webviews: &mut WebViews,
        live: &[(u64, Placement)],
        viewport: Bounds<gpui_kit::Pixels>,
        gpu: Option<&Gpu>,
        count: impl Fn(&[u8]) -> usize,
        secs: u64,
    ) -> usize {
        let until = Instant::now() + Duration::from_secs(secs);
        let mut last: Option<gpui_webview::SurfaceFrame> = None;
        let mut seen = 0;
        loop {
            webviews.frame(live, viewport, &[]);
            webviews.pump();
            if let Some(frame) = webviews.frame_image(0) {
                let changed = last.as_ref().is_none_or(|last| !frame.same_as(last));
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
    /// A whole click (motion, press, release) with `button` at page-local
    /// `(x, y)`.
    fn click_at(webviews: &mut WebViews, button: gpui_webview::PointerButton, x: f64, y: f64) {
        for event in [
            gpui_webview::WebInputEvent::PointerMotion { x, y },
            gpui_webview::WebInputEvent::PointerButton {
                x,
                y,
                button,
                pressed: true,
            },
            gpui_webview::WebInputEvent::PointerButton {
                x,
                y,
                button,
                pressed: false,
            },
        ] {
            webviews.input(0, event);
        }
    }
    /// Pumps frames until the popup list is empty (or non-empty, per `want`)
    /// and returns the last state seen. Bounded, so a stuck client fails the
    /// test instead of hanging it.
    fn wait_for_popups(
        webviews: &mut WebViews,
        live: &[(u64, Placement)],
        viewport: Bounds<gpui_kit::Pixels>,
        want: bool,
    ) -> Vec<gpui_webview::PopupFrame> {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            webviews.frame(live, viewport, &[]);
            webviews.pump();
            let popups = webviews.popup_frames(0);
            if popups.is_empty() != want || Instant::now() >= until {
                return popups;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    /// Dump-on-failure: prints the diagnostics (per-band red, top colours, a
    /// coarse map) that tell a partial paint from a layout mix-up from a stale
    /// buffer.
    fn dump_frame(label: &str, bytes: &[u8], width: u32, height: u32) {
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
        eprintln!("dump ({label}):{bands}; top colours: {}", top.join(", "));
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
            eprintln!("dump ({label}): {line}");
        }
    }
}
