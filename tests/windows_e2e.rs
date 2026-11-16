//! Windows end to end (EMBEDS_WINDOWS.md W5, gate WG2).
//!
//! A real GPUI window hosts a WebView2 surface, in the logged-on session (the
//! suite is driven by `scripts/windows-run.sh`, which schedules it there).
//! What is checked:
//!
//! * the engine paints the page colour — WebView2 renders its own snapshot
//!   (`CapturePreview`), so this holds even where the desktop readback cannot
//!   see GPU-composited content;
//! * the page is on screen inside the clip and absent outside it —
//!   `Windows.Graphics.Capture` reads the composited window (including
//!   WebView2), unlike `PrintWindow` (which leaves the child area dark) and
//!   GDI `BitBlt` (blind to flip-model/DWM content). Only when WGC is
//!   unavailable does the test fall back to GDI and mark the on-screen claim
//!   unverified instead of guessing;
//! * a placement sweep across the clip's top, bottom and left edges keeps the
//!   page in the intersection and the poster outside it (WGC);
//! * a wheel scroll through the webview itself moves the page's content while
//!   the clip stays put (WGC + `SendInput`, absolute coordinates with a move
//!   event — T46);
//! * focus round-trips through the editor's own calls: `set_focus(true)` moves
//!   Win32 focus off GPUI's HWND, `set_focus(false)` hands it back (asserted
//!   via `GetFocus`; the foreground gate is T47);
//! * a strict clip's scripts do not run (the guarded strip stays its inline
//!   colour) and its links: `https:` becomes `WebViewEvent::OpenExternal`,
//!   `mailto:` produces no event at all;
//! * a second `navigate` reuses the engine (no new `msedgewebview2` process);
//! * teardown destroys the clip container and the WebView2 working set falls
//!   back towards its pre-activation baseline.
//!
//! `harness = false`: WebView2 and GPUI both need the main thread.

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("windows_e2e is Windows-only; nothing to do.");
}

#[cfg(target_os = "windows")]
fn main() {
    windows_e2e::main();
}

#[cfg(target_os = "windows")]
mod windows_e2e {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use gpui_kit::*;
    use gpui_webview::backend::native::NativeBackend;
    use gpui_webview::{Config, OverlayBehavior, Placement, Policy, Source, SurfaceSpec, WebViews};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::core::{Interface as _, PCWSTR, factory, w};
    use windows::Foundation::TypedEventHandler;
    use windows::Graphics::Capture::{
        Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
    };
    use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
    use windows::Graphics::DirectX::DirectXPixelFormat;
    use windows::Graphics::SizeInt32;
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
        DISPLAYCONFIG_DEVICE_INFO_TYPE, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo,
        GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
    };
    use windows::Win32::Foundation::{COLORREF, ERROR_SUCCESS, HMODULE, HWND, LUID, POINT, RECT};
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_10_0,
        D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CAPTUREBLT, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC,
        DeleteObject, GetDC, GetMonitorInfoW, GetPixel, HBITMAP, HDC, HGDIOBJ, MONITORINFOEXW,
        MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint, ReleaseDC, SRCCOPY, SelectObject,
    };
    use windows::Win32::System::WinRT::Direct3D11::{
        CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
    };
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
    use windows::Win32::System::Power::{
        ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT,
        MOUSE_EVENT_FLAGS, SendInput, SetActiveWindow, VIRTUAL_KEY, VK_MENU,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        ASFW_ANY, AllowSetForegroundWindow, BringWindowToTop, FindWindowExW, FindWindowW, GW_CHILD,
        GetClassNameW, GetClientRect, GetCursorPos, GetForegroundWindow, GetSystemMetrics,
        GetWindow, GetWindowRect, GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST, IsWindow,
        IsWindowVisible, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_RESTORE, SetCursorPos,
        SetForegroundWindow, SetWindowPos, ShowWindow, SwitchToThisWindow, WHEEL_DELTA,
        WindowFromPoint,
    };

    const WINDOW_WIDTH: f32 = 900.;
    const WINDOW_HEIGHT: f32 = 600.;
    /// The block/clip rect in logical window coordinates; the whole rect is in
    /// the viewport, so the page fills it exactly.
    const CLIP_X: f32 = 200.;
    const CLIP_Y: f32 = 150.;
    const CLIP_WIDTH: f32 = 400.;
    const CLIP_HEIGHT: f32 = 300.;
    /// The page colours and GPUI's own poster colour (never confusable).
    const PAGE: (u8, u8, u8) = (0xC8, 0x32, 0x32);
    const GREEN: (u8, u8, u8) = (0x2E, 0xC8, 0x64);
    const PURPLE: (u8, u8, u8) = (0x80, 0x00, 0xFF);
    const BLUE: (u8, u8, u8) = (0x00, 0x00, 0xFF);
    const POSTER: (u8, u8, u8) = (0x10, 0x20, 0x30);
    const MEGABYTE: u64 = 1024 * 1024;

    /// The window root: a poster-coloured surface, so the pixels outside the
    /// clip are GPUI's own render.
    struct Root;

    impl Render for Root {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().bg(rgb(0x102030))
        }
    }

    /// What the on-screen part of the test could establish.
    #[derive(Clone, Copy)]
    enum Visibility {
        /// `Windows.Graphics.Capture` proved the pixels: the assertions are
        /// authoritative.
        Proven,
        /// GDI readback was used instead; `blind` means it could not see the
        /// webview's composition and nothing inside the clip was asserted.
        Gdi { blind: bool },
    }

    pub(super) fn main() {
        // T44: must run before GPUI's platform init or the surface would be
        // hidden under the DirectComposition target.
        gpui_webview::prepare_process();
        // The input checks need an unlocked, active desktop; an idle display
        // sleep or lock would kill them mid-run.
        unsafe {
            let _ =
                SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
        }

        // `GPUI_E2E_DPI_PERCENT=150` runs the whole suite at that display
        // scale (the runner sets it); the scale is restored before exit.
        // `GPUI_E2E_DPI_RESTORE_REL` overrides the value restored, for
        // recovery after a run that was interrupted before restoring.
        let original_scale: Option<i32> = match std::env::var("GPUI_E2E_DPI_PERCENT")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
        {
            Some(percent) => match set_dpi_scale(percent) {
                Some(previous) => {
                    println!("==> display scale set to {percent}% (previous rel {previous})");
                    Some(previous)
                }
                None => {
                    eprintln!("FAIL: could not set the display scale to {percent}%");
                    std::process::exit(1);
                }
            },
            None => None,
        };
        let restore_scale = std::env::var("GPUI_E2E_DPI_RESTORE_REL")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
            .or(original_scale);

        let app = application();
        app.run(move |cx| {
            init(cx);
            cx.spawn(async move |cx| {
                let failures = run_checks(cx).await;
                for failure in &failures {
                    eprintln!("FAIL: {failure}");
                }
                if let Some(previous) = restore_scale {
                    restore_dpi_scale(previous);
                }
                if failures.is_empty() {
                    println!("windows_e2e: all checks passed");
                }
                std::process::exit(if failures.is_empty() { 0 } else { 1 });
            })
            .detach();
        });
    }

    async fn run_checks(cx: &mut AsyncApp) -> Vec<String> {
        let mut failures = Vec::new();
        // Before any engine exists; other apps' WebView2 processes are part of
        // the sum, so only the delta means anything.
        let baseline_working_set = webview2_working_set();

        let backend = match NativeBackend::new() {
            Ok(backend) => backend,
            Err(error) => return vec![format!("NativeBackend::new: {error}")],
        };
        let webviews = Rc::new(RefCell::new(WebViews::new(
            backend,
            Config {
                capacity: 1,
                on_overlay: OverlayBehavior::Freeze,
            },
        )));
        // The parent HWND cannot be read off a `Window` before it exists; the
        // build closure stores it.
        let parent = Rc::new(Cell::new(0_isize));
        let open = {
            let webviews = webviews.clone();
            let parent = parent.clone();
            cx.open_window(window_options(), move |window, cx| {
                if let Err(error) = webviews.borrow_mut().attach(window) {
                    eprintln!("attach failed: {error}");
                }
                parent.set(parent_hwnd(window));
                cx.new(|_cx| Root)
            })
        };
        if let Err(error) = open {
            return vec![format!("open_window: {error}")];
        }
        let raw_parent = parent.get();
        if raw_parent == 0 {
            return vec!["the GPUI window has no HWND".into()];
        }
        let hwnd = HWND(raw_parent as *mut _);
        // The session checks (WGC, SendInput focus) need the window in front.
        // It stays topmost for the run: a per-window WGC capture sees it
        // regardless, but synthetic input does not — without this, clicks and
        // wheel events land on whatever covers the window.
        let foreground = ensure_foreground(cx, hwnd, 5_000).await;
        println!(
            "==> window foreground: {foreground} (foreground hwnd is {:?})",
            unsafe { GetForegroundWindow() }
        );
        let scale = dpi_scale(hwnd);

        let viewport = Bounds::new(point(px(0.), px(0.)), size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)));
        let rect = Bounds::new(
            point(px(CLIP_X), px(CLIP_Y)),
            size(px(CLIP_WIDTH), px(CLIP_HEIGHT)),
        );
        let placement = Placement::clipped_to(rect, viewport);
        let spec = |colour: &str| {
            SurfaceSpec::new(
                Source::Html {
                    html: format!("<html><body style=\"margin:0;background:{colour}\"></body></html>"),
                    base: None,
                },
                Policy::strict(),
                "windows e2e",
            )
            .with_size(CLIP_WIDTH as u32, CLIP_HEIGHT as u32)
        };

        if let Err(error) = webviews.borrow_mut().activate(0, spec("#c83232")) {
            return vec![format!("activate: {error}")];
        }
        webviews.borrow_mut().frame(&[(0, placement)], viewport, &[]);

        // 1. The engine paints the page. `CapturePreview` is rendered by
        //    WebView2 itself, so this does not depend on the desktop.
        println!("==> waiting for the engine's page snapshot");
        match wait_for_page(cx, &webviews, PAGE, 10_000).await {
            Some(seen) if seen == PAGE => {
                println!("PASS the engine paints the page colour {seen:?}");
            }
            other => failures.push(format!(
                "the engine never painted the page colour (snapshot centre {other:?})"
            )),
        }

        // 2. On-screen pixels. Windows.Graphics.Capture is the proof (it reads
        //    the composited window including WebView2); GDI is a fallback that
        //    may be blind, in which case the on-screen claim is left
        //    unverified rather than guessed.
        let inside = physical(CLIP_X + CLIP_WIDTH / 2., CLIP_Y + CLIP_HEIGHT / 2., scale);
        let outside_right = physical(CLIP_X + CLIP_WIDTH + 80., CLIP_Y + CLIP_HEIGHT / 2., scale);
        let outside_top = physical(CLIP_X + CLIP_WIDTH / 2., 60., scale);
        let offset = client_offset(hwnd);
        println!("==> waiting for the page colour on screen");
        let mut wgc = WgcSession::new(hwnd);
        let visibility = match wgc.as_mut() {
            Some(session) => {
                match wait_for_wgc(cx, session, offset, inside, PAGE, 10_000).await {
                    Some(seen) if seen == PAGE => {
                        println!("PASS inside the clip shows the page colour on screen (WGC)");
                        let outside = session.frame(Duration::from_millis(500));
                        let (right, top) = match &outside {
                            Some(pixels) => (
                                pixels.colour(
                                    offset.0 + outside_right.0,
                                    offset.1 + outside_right.1,
                                ),
                                pixels.colour(offset.0 + outside_top.0, offset.1 + outside_top.1),
                            ),
                            None => (POSTER, POSTER),
                        };
                        if right == PAGE || top == PAGE {
                            failures.push(format!(
                                "the page colour leaked outside the clip (right {right:?}, top {top:?})"
                            ));
                        } else {
                            println!("PASS outside the clip shows the poster ({right:?}, {top:?})");
                        }
                        Visibility::Proven
                    }
                    Some(seen) => {
                        failures.push(format!(
                            "Windows.Graphics.Capture shows {seen:?} inside the clip, not the \
                             page {PAGE:?}: the surface is not visible on screen (T44 stop condition)"
                        ));
                        Visibility::Proven
                    }
                    None => {
                        println!(
                            "SKIP the on-screen proof: Windows.Graphics.Capture is unavailable; \
                             the engine snapshot above is the page proof, visibility is unverified"
                        );
                        gdi_visibility(hwnd, inside, outside_right, outside_top, &mut failures)
                    }
                }
            }
            None => {
                println!(
                    "SKIP the on-screen proof: Windows.Graphics.Capture is unavailable; the \
                     engine snapshot above is the page proof, visibility is unverified"
                );
                gdi_visibility(hwnd, inside, outside_right, outside_top, &mut failures)
            }
        };

        // 3. A placement sweep across both edges of the clip: the page must
        //    stay in the intersection and the poster outside it, at the top,
        //    bottom and left clips. This is the scroll-clipping maths with a
        //    real webview; the wheel-driven scroll itself is step 6.
        if let Some(session) = wgc.as_mut() {
            println!("==> scroll sweep across the clip edges");
            let sweeps = [
                // (label, viewport, page point, poster point)
                (
                    "top clipped",
                    Bounds::new(point(px(0.), px(250.)), size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT))),
                    (400, 300),
                    (400, 200),
                ),
                (
                    "bottom clipped",
                    Bounds::new(point(px(0.), px(0.)), size(px(WINDOW_WIDTH), px(400.))),
                    (400, 300),
                    (400, 420),
                ),
                (
                    "left clipped",
                    Bounds::new(point(px(300.), px(0.)), size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT))),
                    (450, 300),
                    (250, 300),
                ),
                (
                    "restored",
                    viewport,
                    (400, 300),
                    (400, 550),
                ),
            ];
            for (label, sweep_viewport, page_point, poster_point) in sweeps {
                let sweep = Placement::clipped_to(rect, sweep_viewport);
                webviews
                    .borrow_mut()
                    .frame(&[(0, sweep)], sweep_viewport, &[]);
                let page_point = physical(page_point.0 as f32, page_point.1 as f32, scale);
                let poster_point = physical(poster_point.0 as f32, poster_point.1 as f32, scale);
                match wait_for_wgc_pair(
                    cx,
                    session,
                    offset,
                    page_point,
                    poster_point,
                    (PAGE, POSTER),
                    5_000,
                )
                .await
                {
                    Some((page_seen, poster_seen))
                        if page_seen == PAGE && poster_seen == POSTER =>
                    {
                        println!("PASS {label}: page inside the clip, poster outside");
                    }
                    other => failures.push(format!(
                        "{label}: expected page {PAGE:?} inside and poster {POSTER:?} outside, \
                         got {other:?}"
                    )),
                }
            }
            // Back to the full placement for the rest.
            webviews.borrow_mut().frame(&[(0, placement)], viewport, &[]);
        } else {
            println!("SKIP the scroll sweep: Windows.Graphics.Capture is unavailable");
        }

        // 4. Focus round-trip through `SendInput`: clicking the webview moves
        //    Win32 focus off GPUI's HWND; `set_focus(false)` (what the editor
        //    does when the pointer clicks back into it) hands it back.
        println!("==> focus round-trip");
        if !ensure_foreground(cx, hwnd, 5_000).await {
            failures.push(
                "the test window never became foreground; the focus round-trip needs it".into(),
            );
        } else if session_locked() {
            println!("SKIP the focus round-trip: the session locked during the run");
        } else {
            // `set_focus(true)` is exactly what the editor calls when a live
            // surface is activated: wry moves focus to the webview.
            webviews.borrow_mut().set_focus(0, true);
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let after_focus = unsafe { GetFocus() };
            if after_focus == hwnd {
                failures.push(
                    "set_focus(true) left Win32 focus on GPUI's window".into(),
                );
            } else {
                println!("PASS set_focus(true) moved Win32 focus off GPUI ({after_focus:?})");
            }
            // ... and `set_focus(false)` hands it back (the editor calls it
            // when the pointer clicks back into the text).
            webviews.borrow_mut().set_focus(0, false);
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let after_return = unsafe { GetFocus() };
            if after_return == hwnd {
                println!("PASS set_focus(false) returned Win32 focus to GPUI");
            } else {
                failures.push(format!(
                    "set_focus(false) did not return Win32 focus to GPUI ({after_return:?})"
                ));
            }
        }

        // 5. An intersecting overlay freezes (hides) the container.
        let frozen = Placement {
            overlay: true,
            ..placement
        };
        webviews
            .borrow_mut()
            .frame(&[(0, frozen)], viewport, &[rect]);
        match container_visible(hwnd) {
            Some(false) => println!("PASS the overlay hid the clip container (freeze)"),
            Some(true) => failures.push("the overlay did not hide the clip container".into()),
            None => failures.push("no gpui_webview_clip container to freeze".into()),
        }
        if let Some(session) = wgc.as_mut() {
            match wait_for_wgc(cx, session, offset, inside, POSTER, 2_000).await {
                Some(seen) if seen == POSTER => println!("PASS the poster shows while frozen (WGC)"),
                other => failures.push(format!(
                    "the poster did not show while frozen (WGC shows {other:?})"
                )),
            }
        } else if let Visibility::Gdi { blind: false } = visibility {
            match wait_for_screen(cx, hwnd, inside, POSTER, 1_500).await {
                Some(seen) if seen == POSTER => println!("PASS the poster shows while frozen"),
                other => println!(
                    "SKIP the frozen screen sample: the readback shows {other:?} (the container \
                     is hidden, checked above)"
                ),
            }
        }

        // 6. Removing the overlay brings the same engine back.
        webviews.borrow_mut().frame(&[(0, placement)], viewport, &[]);
        match container_visible(hwnd) {
            Some(true) => println!("PASS the surface unfroze"),
            Some(false) => {
                failures.push("the clip container stayed hidden after the overlay left".into())
            }
            None => failures.push("no gpui_webview_clip container after unfreeze".into()),
        }

        // 7. Strict interaction page: scripts off, links out. This is also the
        //    engine-reuse navigate (the process tree must not grow).
        println!("==> strict interaction page");
        let interaction = SurfaceSpec::new(
            Source::Html {
                html: interaction_html(),
                base: None,
            },
            Policy::strict(),
            "windows e2e interaction",
        )
        .with_size(CLIP_WIDTH as u32, CLIP_HEIGHT as u32);
        let before = webview2_processes();
        let navigated = webviews.borrow_mut().activate(0, interaction);
        match navigated {
            Err(error) => failures.push(format!("navigate: {error}")),
            Ok(_) => {
                webviews
                    .borrow_mut()
                    .frame(&[(0, placement)], viewport, &[]);
                // The centre of the page is the red section.
                match wait_for_page(cx, &webviews, PAGE, 10_000).await {
                    Some(seen) if seen == PAGE => {
                        println!("PASS the reused engine paints the interaction page")
                    }
                    other => failures.push(format!(
                        "the reused engine never painted the interaction page (snapshot centre \
                         {other:?})"
                    )),
                }
                let after = webview2_processes();
                let _ = std::process::Command::new("powershell")
                    .args(["-NoProfile", "-Command", "Get-CimInstance Win32_Process -Filter \"Name='msedgewebview2.exe'\" | ForEach-Object { '{0} parent={1} start={2} {3}' -f $_.ProcessId, $_.ParentProcessId, $_.CreationDate.ToString('HH:mm:ss.fff'), (($_.CommandLine -split ' ' | Where-Object { $_ -like '--type=*' -or $_ -like '--utility-sub-type=*' }) -join ' ') }"])
                    .status();
                if after > before {
                    failures.push(format!(
                        "navigate grew the WebView2 process tree: {before} -> {after}"
                    ));
                } else {
                    println!("PASS engine reused: {before} -> {after} msedgewebview2 process(es)");
                }

                // Scripts off: the script would paint the guard strip blue.
                if let Some(session) = wgc.as_mut() {
                    let guard = physical(CLIP_X + CLIP_WIDTH / 2., CLIP_Y + 30., scale);
                    match wait_for_wgc(cx, session, offset, guard, PAGE, 5_000).await {
                        Some(seen) if seen == PAGE => {
                            println!("PASS the clip's script did not run (guard stays red)")
                        }
                        Some(BLUE) => failures.push(
                            "the clip's script ran: the guard strip turned blue despite \
                             scripts off"
                                .into(),
                        ),
                        other => failures.push(format!(
                            "the guard strip showed {other:?}, neither the inline red {PAGE:?} \
                             nor the script's blue {BLUE:?}"
                        )),
                    }

                    // A wheel scroll through the webview itself: the content
                    // moves to the green section while the clip stays put, and
                    // scrolling back restores the top.
                    ensure_foreground(cx, hwnd, 3_000).await;
                    if session_locked() {
                        println!("SKIP the wheel scroll: the session locked during the run");
                    } else {
                        // A real user clicks the page (or it has focus) before
                        // scrolling; give it focus, and retry the wheel — the
                        // first tick after the cursor teleport can still be
                        // lost even with the move event.
                        webviews.borrow_mut().set_focus(0, true);
                        let mut reached = None;
                        for _ in 0..4 {
                            send_wheel(hwnd, inside, -4);
                            let seen = wait_for_wgc(cx, session, offset, inside, GREEN, 2_500).await;
                            match seen {
                                Some(seen) if seen == GREEN => {
                                    reached = Some(seen);
                                    break;
                                }
                                other => reached = other,
                            }
                        }
                        match reached {
                            Some(seen) if seen == GREEN => {
                                println!("PASS a wheel scroll moved the page content (bottom edge)")
                            }
                            other => failures.push(format!(
                                "the wheel scroll did not reach the green section (WGC centre {other:?})"
                            )),
                        }
                        let mut back = None;
                        for _ in 0..4 {
                            send_wheel(hwnd, inside, 4);
                            let seen = wait_for_wgc(cx, session, offset, inside, PAGE, 2_500).await;
                            match seen {
                                Some(seen) if seen == PAGE => {
                                    back = Some(seen);
                                    break;
                                }
                                other => back = other,
                            }
                        }
                        match back {
                            Some(seen) if seen == PAGE => {
                                println!("PASS scrolling back restored the top of the page")
                            }
                            other => failures.push(format!(
                                "scrolling back did not restore the top (WGC centre {other:?})"
                            )),
                        }
                        webviews.borrow_mut().set_focus(0, false);
                    }
                }

                // The link strips are purple; checking them keeps the clicks
                // deterministic (and catches a missed layout).
                ensure_foreground(cx, hwnd, 3_000).await;
                let https_point = physical(CLIP_X + CLIP_WIDTH / 2., CLIP_Y + 80., scale);
                let mailto_point = physical(CLIP_X + CLIP_WIDTH / 2., CLIP_Y + 120., scale);
                if let Some(session) = wgc.as_mut() {
                    match wait_for_wgc_pair(
                        cx,
                        session,
                        offset,
                        https_point,
                        mailto_point,
                        (PURPLE, PURPLE),
                        5_000,
                    )
                    .await
                    {
                        Some(seen) if seen == (PURPLE, PURPLE) => {
                            println!("PASS the link strips are on screen where they are clicked");
                        }
                        other => failures.push(format!(
                            "the link strips are not on screen (https, mailto: {other:?})"
                        )),
                    }
                }

                // Links: `https:` leaves through OpenExternal; `mailto:` is
                // refused and must produce no event at all.
                if session_locked() {
                    println!("SKIP the link checks: the session locked during the run");
                } else {
                    send_click(hwnd, https_point);
                    let https_event = wait_for_event(cx, &webviews, |event| {
                        matches!(event, gpui_webview::WebViewEvent::OpenExternal { url, .. }
                            if url == "https://example.com/e2e")
                    })
                    .await;
                    if https_event {
                        println!("PASS an https link produced OpenExternal");
                    } else {
                        failures.push("an https link produced no OpenExternal event".into());
                    }
                    send_click(hwnd, mailto_point);
                    cx.background_executor()
                        .timer(Duration::from_millis(800))
                        .await;
                    let mailto_event = webviews.borrow_mut().events().iter().any(|event| {
                        matches!(event, gpui_webview::WebViewEvent::OpenExternal { url, .. }
                            if url.starts_with("mailto:"))
                    });
                    if mailto_event {
                        failures
                            .push("a mailto link reached OpenExternal; it must be refused".into());
                    } else {
                        println!("PASS a mailto link produced no event");
                    }
                }
            }
        }

        // 8. Teardown: the container goes, and the engine's working set falls
        //    back towards the baseline. The window stops being topmost first.
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE,
            );
        }
        let peak_working_set = webview2_working_set();
        match find_clip(hwnd) {
            Ok(container) => {
                webviews.borrow_mut().deactivate(0);
                let alive = unsafe { IsWindow(Some(container)).as_bool() };
                if alive {
                    failures.push("teardown left the clip container alive".into());
                } else {
                    println!("PASS teardown destroyed the clip container");
                }
            }
            Err(_) => failures.push("no gpui_webview_clip container was found".into()),
        }
        if webviews.borrow_mut().live_count() != 0 {
            failures.push("the manager still reports a live surface after deactivate".into());
        }
        match (baseline_working_set, peak_working_set) {
            (Some(baseline), Some(peak)) => {
                let mut settled = peak;
                for _ in 0..30 {
                    cx.background_executor()
                        .timer(Duration::from_millis(500))
                        .await;
                    settled = webview2_working_set().unwrap_or(settled);
                    if settled <= baseline + 64 * MEGABYTE {
                        break;
                    }
                }
                println!(
                    "working set: baseline {} MB, peak {} MB, after teardown {} MB",
                    baseline / MEGABYTE,
                    peak / MEGABYTE,
                    settled / MEGABYTE
                );
                if settled + 32 * MEGABYTE >= peak {
                    failures.push(format!(
                        "the WebView2 working set did not fall after teardown (baseline {} MB, \
                         peak {} MB, settled {} MB)",
                        baseline / MEGABYTE,
                        peak / MEGABYTE,
                        settled / MEGABYTE
                    ));
                } else {
                    println!("PASS the WebView2 working set fell after teardown");
                }
            }
            _ => println!("SKIP the working-set check: PowerShell is unavailable"),
        }

        let _ = wgc;
        failures
    }

    /// The strict clip: a guarded strip a script would repaint, an `https:`
    /// link, a `mailto:` link, and a second screenful so a wheel scroll moves
    /// the content.
    fn interaction_html() -> String {
        "<html><body style=\"margin:0\">\
         <div id=\"guard\" style=\"height:60px;background:#c83232\"></div>\
         <a id=\"https\" href=\"https://example.com/e2e\" style=\"display:block;height:40px;background:#8000ff\"></a>\
         <a id=\"mailto\" href=\"mailto:e2e@example.com\" style=\"display:block;height:40px;background:#8000ff\"></a>\
         <div style=\"height:160px;background:#c83232\"></div>\
         <div style=\"height:300px;background:#2ec864\"></div>\
         <script>document.getElementById('guard').style.background='#0000ff';</script>\
         </body></html>"
            .to_owned()
    }

    /// Waits until both the page and the poster probe hold.
    async fn wait_for_wgc_pair(
        cx: &mut AsyncApp,
        session: &mut WgcSession,
        offset: (i32, i32),
        page_point: (i32, i32),
        poster_point: (i32, i32),
        expected: ((u8, u8, u8), (u8, u8, u8)),
        timeout_ms: u64,
    ) -> Option<((u8, u8, u8), (u8, u8, u8))> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut last = None;
        loop {
            if let Some(pixels) = session.frame(Duration::from_millis(250)) {
                let seen = (
                    pixels.colour(offset.0 + page_point.0, offset.1 + page_point.1),
                    pixels.colour(offset.0 + poster_point.0, offset.1 + poster_point.1),
                );
                if seen == expected || Instant::now() >= deadline {
                    return Some(seen);
                }
                last = Some(seen);
            }
            if Instant::now() >= deadline {
                return last;
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        }
    }

    /// Drains events until `matches` holds; `true` when it did within 3 s.
    async fn wait_for_event(
        cx: &mut AsyncApp,
        webviews: &Rc<RefCell<WebViews>>,
        matches: impl Fn(&gpui_webview::WebViewEvent) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if webviews.borrow_mut().events().iter().any(&matches) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
        }
    }

    /// The GDI fallback: outside probes are trustworthy; the inside sample is
    /// only a failure when it reads exactly the poster (the T44 case), because
    /// GDI cannot see the webview's composition.
    fn gdi_visibility(
        hwnd: HWND,
        inside: (i32, i32),
        outside_right: (i32, i32),
        outside_top: (i32, i32),
        failures: &mut Vec<String>,
    ) -> Visibility {
        let screen = capture(hwnd);
        let right = screen.colour(outside_right.0, outside_right.1);
        let top = screen.colour(outside_top.0, outside_top.1);
        drop(screen);
        let blind = right != POSTER && top != POSTER;
        if blind {
            println!(
                "SKIP the GDI on-screen checks too: the readback is blind to this desktop's GPU \
                 composition (outside samples {right:?}, {top:?}; the poster is {POSTER:?})"
            );
            eprintln!("  {}", container_state(hwnd));
            return Visibility::Gdi { blind: true };
        }
        if right == PAGE || top == PAGE {
            failures.push(format!(
                "the page colour leaked outside the clip (right {right:?}, top {top:?})"
            ));
        } else {
            println!("PASS outside the clip shows the poster ({right:?}, {top:?})");
        }
        let seen = capture(hwnd).colour(inside.0, inside.1);
        if seen == PAGE {
            println!("PASS inside the clip shows the page colour on screen (GDI)");
        } else if seen == POSTER {
            failures.push(
                "the screen still shows the poster inside the clip: the surface is hidden \
                 under GPUI (T44 stop condition)"
                    .into(),
            );
        } else {
            println!(
                "SKIP the inside-clip GDI sample: the readback is blind to WebView2's \
                 composition ({seen:?})"
            );
        }
        Visibility::Gdi { blind: false }
    }

    fn window_options() -> WindowOptions {
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(0.), px(0.)),
                size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("gpui-webview e2e".into()),
                ..Default::default()
            }),
            app_id: Some("com.notesnook.gpui-webview-e2e".into()),
            ..Default::default()
        }
    }

    /// GPUI's top-level HWND (the surface's ultimate parent).
    fn parent_hwnd(window: &Window) -> isize {
        let Ok(handle) = HasWindowHandle::window_handle(window) else {
            return 0;
        };
        match handle.as_raw() {
            RawWindowHandle::Win32(handle) => handle.hwnd.get(),
            _ => 0,
        }
    }

    /// The window's current DPI scale (96 dpi = 1.0).
    fn dpi_scale(hwnd: HWND) -> f64 {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi == 0 { 1.0 } else { dpi as f64 / 96.0 }
    }

    /// A logical window point in the client's physical pixels.
    fn physical(x: f32, y: f32, scale: f64) -> (i32, i32) {
        (
            (x as f64 * scale).round() as i32,
            (y as f64 * scale).round() as i32,
        )
    }

    /// The client origin inside the window: WGC frames are window-relative,
    /// the test's points are client-relative.
    fn client_offset(hwnd: HWND) -> (i32, i32) {
        let mut window_rect = RECT::default();
        let mut client_origin = POINT::default();
        unsafe {
            let _ = GetWindowRect(hwnd, &mut window_rect);
            let _ = ClientToScreen(hwnd, &mut client_origin);
        }
        (
            client_origin.x - window_rect.left,
            client_origin.y - window_rect.top,
        )
    }

    /// A client point in screen coordinates.
    fn screen_point(hwnd: HWND, local: (i32, i32)) -> POINT {
        let mut point = POINT {
            x: local.0,
            y: local.1,
        };
        unsafe {
            let _ = ClientToScreen(hwnd, &mut point);
        }
        point
    }

    /// One synthesised left click at a client-relative point, in this session
    /// (the Windows equivalent of the Linux suite's xdotool click). The move
    /// comes first in the same batch: the window under the cursor must see the
    /// motion before the button event.
    fn send_click(hwnd: HWND, local: (i32, i32)) {
        let point = screen_point(hwnd, local);
        unsafe {
            let inputs = [
                absolute_mouse(MOUSEEVENTF_MOVE, point, 0),
                absolute_mouse(MOUSEEVENTF_LEFTDOWN, point, 0),
                absolute_mouse(MOUSEEVENTF_LEFTUP, point, 0),
            ];
            let inserted = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
            eprintln!("e2e input: click {local:?} at {point:?} {inserted}");
            log_input_targets(point);
        }
    }

    /// `notches` wheel steps (negative scrolls down) at a client-relative
    /// point, after a move event (the first wheel tick after a cursor teleport
    /// is otherwise lost — the Windows sibling of Linux trap T7).
    fn send_wheel(hwnd: HWND, local: (i32, i32), notches: i32) {
        let point = screen_point(hwnd, local);
        let delta = notches * WHEEL_DELTA as i32;
        unsafe {
            let inputs = [
                absolute_mouse(MOUSEEVENTF_MOVE, point, 0),
                absolute_mouse(MOUSEEVENTF_WHEEL, point, delta),
            ];
            let inserted = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
            eprintln!("e2e input: wheel {notches} at {point:?} {inserted}");
            log_input_targets(point);
        }
    }

    /// A mouse input at absolute virtual-desktop coordinates: `SetCursorPos`
    /// and relative events are refused on some desktops, absolute ones are
    /// not.
    fn absolute_mouse(flags: MOUSE_EVENT_FLAGS, point: POINT, data: i32) -> INPUT {
        unsafe {
            let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let width = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let height = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
            let x = ((point.x - left) as i64 * 65535 / width as i64) as i32;
            let y = ((point.y - top) as i64 * 65535 / height as i64) as i32;
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: x,
                        dy: y,
                        mouseData: data as u32,
                        dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }
        }
    }

    /// Diagnostic for a failed synthetic input: where the cursor is, what
    /// window is under it, and the class of the container's child (wry's
    /// webview host).
    fn log_input_targets(expected: POINT) {
        unsafe {
            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            let under = WindowFromPoint(cursor);
            let mut class = [0_u16; 64];
            let length = GetClassNameW(under, &mut class);
            let class = String::from_utf16_lossy(&class[..length.max(0) as usize]);
            eprintln!(
                "e2e input: cursor {cursor:?} under {under:?} class {class:?} (expected {expected:?})"
            );
        }
    }

    /// A key-down/up tap of Alt: the classic way a background process is
    /// allowed to take the foreground (the foreground lock ignores
    /// `SetForegroundWindow` from a freshly launched task otherwise).
    fn send_alt_tap() {
        unsafe {
            let inputs = [
                key_input(VK_MENU, KEYBD_EVENT_FLAGS::default()),
                key_input(VK_MENU, KEYEVENTF_KEYUP),
            ];
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }

    /// Keeps the window topmost and takes the foreground the supported way:
    /// allow the foreground change, attach to the current foreground thread,
    /// `SetForegroundWindow`, detach. Synthetic input needs a topmost window
    /// (it already is by then); the focus assertions need it foreground too.
    fn bring_to_front(hwnd: HWND) {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            let _ = AllowSetForegroundWindow(ASFW_ANY);
            send_alt_tap();
            let foreground = GetForegroundWindow();
            let foreground_thread = GetWindowThreadProcessId(foreground, None);
            let this_thread = GetCurrentThreadId();
            let attached = foreground_thread != 0
                && foreground_thread != this_thread
                && AttachThreadInput(this_thread, foreground_thread, true).as_bool();
            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);
            let _ = SetActiveWindow(hwnd);
            SwitchToThisWindow(hwnd, true);
            if attached {
                let _ = AttachThreadInput(this_thread, foreground_thread, false);
            }
        }
    }

    /// Retries [`bring_to_front`] until the window is the foreground window
    /// (the scale-change and lock transitions can steal it after the first
    /// attempt) or `timeout_ms` passes.
    async fn ensure_foreground(cx: &mut AsyncApp, hwnd: HWND, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if unsafe { GetForegroundWindow() } == hwnd {
                return true;
            }
            bring_to_front(hwnd);
            if unsafe { GetForegroundWindow() } == hwnd {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
        }
    }

    fn key_input(key: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: key,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// Waits until WebView2's own page snapshot shows `colour` at its centre,
    /// sampling at 50 ms; returns the last centre sample.
    async fn wait_for_page(
        cx: &mut AsyncApp,
        webviews: &Rc<RefCell<WebViews>>,
        colour: (u8, u8, u8),
        timeout_ms: u64,
    ) -> Option<(u8, u8, u8)> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let seen = webviews
                .borrow_mut()
                .page_snapshot(0)
                .and_then(|png| centre_colour(&png));
            if seen == Some(colour) || Instant::now() >= deadline {
                return seen;
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        }
    }

    /// Waits until the WGC frame shows `colour` at `local` (window-relative
    /// after `offset`); `None` when no frame arrives at all.
    async fn wait_for_wgc(
        cx: &mut AsyncApp,
        session: &mut WgcSession,
        offset: (i32, i32),
        local: (i32, i32),
        colour: (u8, u8, u8),
        timeout_ms: u64,
    ) -> Option<(u8, u8, u8)> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut last = None;
        loop {
            if let Some(pixels) = session.frame(Duration::from_millis(250)) {
                let seen = pixels.colour(offset.0 + local.0, offset.1 + local.1);
                if seen == colour {
                    return Some(seen);
                }
                last = Some(seen);
            }
            if Instant::now() >= deadline {
                return last;
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        }
    }

    /// The centre pixel of a PNG, RGBA.
    fn centre_colour(png: &[u8]) -> Option<(u8, u8, u8)> {
        let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?;
        let image = image.to_rgba8();
        let (width, height) = image.dimensions();
        let pixel = image.get_pixel(width / 2, height / 2).0;
        Some((pixel[0], pixel[1], pixel[2]))
    }

    /// Waits until the screen at `point` shows `colour`, sampling at 50 ms;
    /// returns the last sample (DWM may need a moment to recompose after a
    /// hide/show).
    async fn wait_for_screen(
        cx: &mut AsyncApp,
        hwnd: HWND,
        point: (i32, i32),
        colour: (u8, u8, u8),
        timeout_ms: u64,
    ) -> Option<(u8, u8, u8)> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let seen = capture(hwnd).colour(point.0, point.1);
            if seen == colour || Instant::now() >= deadline {
                return Some(seen);
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        }
    }

    /// Whether the clip container exists and is visible.
    fn container_visible(parent: HWND) -> Option<bool> {
        let container = find_clip(parent).ok()?;
        Some(unsafe { IsWindowVisible(container).as_bool() })
    }

    /// What the container looks like right now: found or not, visible or not,
    /// and its screen rect. Evidence when the screen readback is blind.
    fn container_state(parent: HWND) -> String {
        match find_clip(parent) {
            Err(_) => "no gpui_webview_clip container exists".into(),
            Ok(container) => {
                let visible = unsafe { IsWindowVisible(container).as_bool() };
                let mut rect = RECT::default();
                let _ = unsafe { GetWindowRect(container, &mut rect) };
                format!(
                    "container visible={visible} screen rect=({},{},{},{})",
                    rect.left, rect.top, rect.right, rect.bottom
                )
            }
        }
    }

    /// The live clip-container HWND under GPUI's window.
    fn find_clip(parent: HWND) -> windows::core::Result<HWND> {
        unsafe { FindWindowExW(Some(parent), None, w!("gpui_webview_clip"), PCWSTR::null()) }
    }

    /// Whether the interactive session is locked: input then goes to the
    /// secure desktop, where `SetCursorPos`/`SendInput` are refused (they
    /// return zero and the cursor reads (0,0)). WGC still captures windows.
    fn session_locked() -> bool {
        unsafe { FindWindowW(w!("LockScreenBackstopFrame"), PCWSTR::null()).is_ok() }
    }

    /// `msedgewebview2.exe` processes on the machine; the tree for our
    /// environment counts too, but a reuse must not grow the total.
    fn webview2_processes() -> usize {
        let output = std::process::Command::new("tasklist")
            .args([
                "/FI",
                "IMAGENAME eq msedgewebview2.exe",
                "/FO",
                "CSV",
                "/NH",
            ])
            .output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| line.contains("msedgewebview2.exe"))
                .count(),
            // tasklist unavailable: make both counts equal so the check
            // cannot fail spuriously.
            Err(_) => usize::MAX,
        }
    }

    /// The combined working set of every `msedgewebview2.exe` process, in
    /// bytes. Other apps contribute a roughly constant offset; the activation
    /// delta is what the checks use.
    fn webview2_working_set() -> Option<u64> {
        // The scheduled task's PATH may not include PowerShell; resolve it.
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_owned());
        let powershell = format!("{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
        let output = std::process::Command::new(&powershell)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "(Get-Process msedgewebview2 -ErrorAction SilentlyContinue | \
                 Measure-Object WorkingSet64 -Sum).Sum",
            ])
            .output();
        match output {
            Ok(output) => {
                let text = String::from_utf8_lossy(&output.stdout);
                let trimmed = text.trim();
                // No processes at all (before activation, after teardown):
                // `Measure-Object` prints nothing.
                if trimmed.is_empty() {
                    Some(0)
                } else {
                    match trimmed.parse::<u64>() {
                        Ok(bytes) => Some(bytes),
                        Err(_) => {
                            static REPORTED: std::sync::Once = std::sync::Once::new();
                            REPORTED.call_once(|| {
                                eprintln!(
                                    "working-set: cannot parse {text:?} (stderr {:?})",
                                    String::from_utf8_lossy(&output.stderr)
                                )
                            });
                            None
                        }
                    }
                }
            }
            Err(error) => {
                static REPORTED: std::sync::Once = std::sync::Once::new();
                REPORTED.call_once(|| eprintln!("working-set: {powershell} failed: {error}"));
                None
            }
        }
    }

    /// A CPU copy of a captured frame, BGRA rows top-down.
    struct Pixels {
        width: i32,
        height: i32,
        bgra: Vec<u8>,
    }

    impl Pixels {
        fn colour(&self, x: i32, y: i32) -> (u8, u8, u8) {
            let x = x.clamp(0, (self.width - 1).max(0));
            let y = y.clamp(0, (self.height - 1).max(0));
            let index = ((y * self.width + x) * 4) as usize;
            if index + 2 >= self.bgra.len() {
                return (0, 0, 0);
            }
            // BGRA.
            (
                self.bgra[index + 2],
                self.bgra[index + 1],
                self.bgra[index],
            )
        }
    }

    /// A live `Windows.Graphics.Capture` session on the window. It reads the
    /// composited window (DWM/DirectComposition, WebView2 included), which
    /// `PrintWindow` and screen `BitBlt` both fail to see for WebView2.
    struct WgcSession {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        pool: Direct3D11CaptureFramePool,
        session: GraphicsCaptureSession,
        frames: std::sync::mpsc::Receiver<()>,
    }

    impl WgcSession {
        fn new(hwnd: HWND) -> Option<Self> {
            unsafe {
                let levels = [
                    D3D_FEATURE_LEVEL_11_0,
                    D3D_FEATURE_LEVEL_10_1,
                    D3D_FEATURE_LEVEL_10_0,
                ];
                let mut device = None;
                let mut context = None;
                let mut created = D3D11CreateDevice(
                    None::<&IDXGIAdapter>,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
                .is_ok();
                if !created {
                    created = D3D11CreateDevice(
                        None::<&IDXGIAdapter>,
                        D3D_DRIVER_TYPE_WARP,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        Some(&levels),
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )
                    .is_ok();
                }
                if !created {
                    return None;
                }
                let device = device?;
                let context = context?;

                let dxgi: IDXGIDevice = device.cast().ok()?;
                let winrt_device: IDirect3DDevice =
                    CreateDirect3D11DeviceFromDXGIDevice(&dxgi).ok()?.cast().ok()?;
                let interop: IGraphicsCaptureItemInterop =
                    factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().ok()?;
                let item: GraphicsCaptureItem = interop.CreateForWindow(hwnd).ok()?;
                let size: SizeInt32 = item.Size().ok()?;
                let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                    &winrt_device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    size,
                )
                .ok()?;
                let session = pool.CreateCaptureSession(&item).ok()?;
                // No yellow capture border while the test runs.
                let _ = session.SetIsBorderRequired(false);
                let (tx, frames) = std::sync::mpsc::channel();
                let handler = TypedEventHandler::<
                    Direct3D11CaptureFramePool,
                    windows::core::IInspectable,
                >::new(move |_sender, _args| {
                    let _ = tx.send(());
                    Ok(())
                });
                pool.FrameArrived(&handler).ok()?;
                session.StartCapture().ok()?;
                Some(Self {
                    device,
                    context,
                    pool,
                    session,
                    frames,
                })
            }
        }

        /// Waits up to `wait` for the next frame (content change; the first
        /// frame after `StartCapture` always arrives) and copies it to CPU
        /// pixels.
        fn frame(&mut self, wait: Duration) -> Option<Pixels> {
            unsafe {
                let _ = self.frames.recv_timeout(wait);
                let frame = self.pool.TryGetNextFrame().ok()?;
                let surface = frame.Surface().ok()?;
                let access: IDirect3DDxgiInterfaceAccess = surface.cast().ok()?;
                let texture: ID3D11Texture2D = access.GetInterface().ok()?;

                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Width: desc.Width,
                    Height: desc.Height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: desc.Format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut staging = None;
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                    .ok()?;
                let staging = staging?;
                self.context.CopyResource(&staging, &texture);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .ok()?;
                let width = desc.Width as i32;
                let height = desc.Height as i32;
                let mut bgra = vec![0_u8; (width * height * 4) as usize];
                for row in 0..height {
                    let source =
                        (mapped.pData as *const u8).add((row * mapped.RowPitch as i32) as usize);
                    let target = bgra.as_mut_ptr().add((row * width * 4) as usize);
                    std::ptr::copy_nonoverlapping(source, target, (width * 4) as usize);
                }
                self.context.Unmap(&staging, 0);
                Some(Pixels {
                    width,
                    height,
                    bgra,
                })
            }
        }
    }

    impl Drop for WgcSession {
        fn drop(&mut self) {
            // Both `Close`s are safe COM calls in the generated bindings.
            let _ = self.session.Close();
            let _ = self.pool.Close();
        }
    }

    /// A client-area bitmap filled by a screen `BitBlt`, sampled with
    /// `GetPixel`. The WGC session is the proof; this is the fallback.
    struct Capture {
        hdc: HDC,
        bitmap: HBITMAP,
        previous: HGDIOBJ,
        width: i32,
        height: i32,
    }

    impl Capture {
        fn colour(&self, x: i32, y: i32) -> (u8, u8, u8) {
            let x = x.clamp(0, (self.width - 1).max(0));
            let y = y.clamp(0, (self.height - 1).max(0));
            let value: COLORREF = unsafe { GetPixel(self.hdc, x, y) };
            if value.0 == u32::MAX {
                // CLR_INVALID.
                return (0xFF, 0xFF, 0xFF);
            }
            (value.0 as u8, (value.0 >> 8) as u8, (value.0 >> 16) as u8)
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            unsafe {
                SelectObject(self.hdc, self.previous);
                let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
                let _ = DeleteDC(self.hdc);
            }
        }
    }

    fn capture(hwnd: HWND) -> Capture {
        unsafe {
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let width = (rect.right - rect.left).max(1);
            let height = (rect.bottom - rect.top).max(1);
            // Client origin in screen coordinates; `BitBlt` from the screen is
            // the readback that includes WebView2's content. It can still come
            // back blind on a flip-model/DWM desktop; callers handle that.
            let mut origin = POINT::default();
            let _ = ClientToScreen(hwnd, &mut origin);
            let screen = GetDC(None);
            let hdc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, width, height);
            let previous = SelectObject(hdc, HGDIOBJ(bitmap.0));
            let _ = BitBlt(
                hdc,
                0,
                0,
                width,
                height,
                Some(screen),
                origin.x,
                origin.y,
                SRCCOPY | CAPTUREBLT,
            );
            let _ = ReleaseDC(None, screen);
            Capture {
                hdc,
                bitmap,
                previous,
                width,
                height,
            }
        }
    }

    // ---- display scale (the 150% run) -------------------------------------

    #[repr(C)]
    struct SourceDpiScaleGet {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        min_scale_rel: i32,
        cur_scale_rel: i32,
        max_scale_rel: i32,
    }

    #[repr(C)]
    struct SourceDpiScaleSet {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        scale_rel: i32,
    }

    /// `DISPLAYCONFIG_DEVICE_INFO_GET_DPI_SCALE` / `SET_DPI_SCALE` were
    /// undocumented when the working reference (lihas/windows-DPI-scaling-
    /// sample) was written; the values the API accepts are `-3` and `-4`, and
    /// the `*ScaleRel` fields are *steps in the supported DPI list*, not
    /// percentage points. DPI scaling is a property of the source.
    const DEVICE_INFO_GET_DPI_SCALE: i32 = -3;
    const DEVICE_INFO_SET_DPI_SCALE: i32 = -4;

    /// The scales Windows offers (Settings' list).
    const DPI_SCALE_PERCENTS: [i32; 12] =
        [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

    fn dpi_scale_index(percent: i32) -> Option<i32> {
        DPI_SCALE_PERCENTS
            .iter()
            .position(|value| *value == percent)
            .map(|index| index as i32)
    }

    fn dpi_header(kind: i32, adapter: LUID, id: u32, size: u32) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
        DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(kind),
            size,
            adapterId: adapter,
            id,
        }
    }

    /// The active display path behind the primary monitor.
    fn primary_display_source() -> Option<(LUID, u32)> {
        unsafe {
            let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if !GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool() {
                return None;
            }
            let device = utf16_z(&info.szDevice);

            let mut path_count = 0_u32;
            let mut mode_count = 0_u32;
            if GetDisplayConfigBufferSizes(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                &mut mode_count,
            ) != ERROR_SUCCESS
            {
                return None;
            }
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            if QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            ) != ERROR_SUCCESS
            {
                return None;
            }
            paths.truncate(path_count as usize);
            for path in &paths {
                let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                    header: dpi_header(
                        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME.0,
                        path.sourceInfo.adapterId,
                        path.sourceInfo.id,
                        std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    ),
                    viewGdiDeviceName: [0; 32],
                };
                if DisplayConfigGetDeviceInfo(&mut name.header) == 0
                    && utf16_z(&name.viewGdiDeviceName) == device
                {
                    return Some((path.sourceInfo.adapterId, path.sourceInfo.id));
                }
            }
            None
        }
    }

    /// Sets the primary display's DPI scale (e.g. 150) and waits until the
    /// compositor reports it. Returns the previous relative scale for
    /// [`restore_dpi_scale`].
    fn set_dpi_scale(percent: i32) -> Option<i32> {
        unsafe {
            let (adapter, id) = primary_display_source()?;
            let mut get = SourceDpiScaleGet {
                header: dpi_header(
                    DEVICE_INFO_GET_DPI_SCALE,
                    adapter,
                    id,
                    std::mem::size_of::<SourceDpiScaleGet>() as u32,
                ),
                min_scale_rel: 0,
                cur_scale_rel: 0,
                max_scale_rel: 0,
            };
            if DisplayConfigGetDeviceInfo(&mut get.header) != 0 {
                eprintln!("DisplayConfigGetDeviceInfo(DPI_SCALE) failed");
                return None;
            }
            // Steps in the supported list, relative to the recommended scale.
            let target_index = dpi_scale_index(percent)?;
            let current_index = dpi_scale_index(effective_dpi_percent())?;
            let recommended_index = current_index - get.cur_scale_rel;
            let target_rel =
                (target_index - recommended_index).clamp(get.min_scale_rel, get.max_scale_rel);
            let set = SourceDpiScaleSet {
                header: dpi_header(
                    DEVICE_INFO_SET_DPI_SCALE,
                    adapter,
                    id,
                    std::mem::size_of::<SourceDpiScaleSet>() as u32,
                ),
                scale_rel: target_rel,
            };
            if DisplayConfigSetDeviceInfo(&set.header) != 0 {
                eprintln!("DisplayConfigSetDeviceInfo(DPI_SCALE) failed");
                return None;
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while effective_dpi_percent() != percent && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            if effective_dpi_percent() != percent {
                eprintln!(
                    "the display scale did not become {percent}% (reads {}%)",
                    effective_dpi_percent()
                );
            }
            Some(get.cur_scale_rel)
        }
    }

    /// Restores a scale returned by [`set_dpi_scale`].
    fn restore_dpi_scale(previous_rel: i32) {
        unsafe {
            let Some((adapter, id)) = primary_display_source() else {
                return;
            };
            let set = SourceDpiScaleSet {
                header: dpi_header(
                    DEVICE_INFO_SET_DPI_SCALE,
                    adapter,
                    id,
                    std::mem::size_of::<SourceDpiScaleSet>() as u32,
                ),
                scale_rel: previous_rel,
            };
            let _ = DisplayConfigSetDeviceInfo(&set.header);
        }
    }

    /// The primary monitor's effective DPI as a percentage (100, 125, 150, …).
    fn effective_dpi_percent() -> i32 {
        unsafe {
            let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut dpi_x = 96_u32;
            let mut dpi_y = 96_u32;
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            (dpi_x as i32 * 100) / 96
        }
    }

    fn utf16_z(value: &[u16]) -> String {
        let end = value.iter().position(|unit| *unit == 0).unwrap_or(value.len());
        String::from_utf16_lossy(&value[..end])
    }
}
