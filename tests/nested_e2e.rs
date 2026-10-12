//! Headless end-to-end check of the Linux nested compositor.
//!
//! No host display is involved: GTK/WebKitGTK connect to the private Wayland
//! socket this crate serves, render the page, and commit `wl_shm` buffers that
//! come back as frame images the app would paint. That makes the risky parts of
//! the Linux path (GTK startup against our compositor, `xdg_toplevel` mapping
//! and configure, WebKit's first paint, buffer copy, teardown) testable on any
//! Linux box — including a headless VM.
//!
//! ```sh
//! WEBKIT_DISABLE_DMABUF_RENDERER=1 WEBKIT_DISABLE_COMPOSITING_MODE=1 \
//!   cargo test -p gpui-webview --features nested --test nested_e2e -- --nocapture
//! ```
//!
//! It is `harness = false` so everything runs on the process's main thread,
//! which GTK requires. Exit code 0 = pass. `E2E_TIMEOUT_SECS` overrides the
//! 45-second budget for the first commit (a slow, GPU-less VM may need it).

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("nested_e2e is Linux-only; nothing to do.");
}

#[cfg(target_os = "linux")]
fn main() {
    use std::time::{Duration, Instant};

    use gpui_kit::{Bounds, point, px, size};
    use gpui_webview::backend::nested::NestedBackend;
    use gpui_webview::{Config, Placement, Policy, Source, SurfaceSpec, WebViews};

    let (width, height) = (320u32, 200u32);
    // No network involved: a red page, so a committed frame is self-evidently
    // the page and not an empty surface.
    let html = "<!doctype html><html><body style=\"margin:0;background:#c81e1e\">\
                <h1 style=\"margin:0;color:#fff;font-family:sans-serif\">nested e2e</h1>\
                </body></html>";
    let timeout = std::env::var("E2E_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(45));

    let fail = |what: &str| -> ! {
        eprintln!("FAIL: {what}");
        std::process::exit(1);
    };

    // The Wayland socket lives in `XDG_RUNTIME_DIR`; make one when the
    // environment has none (CI, containers) so the test is self-contained.
    let owned_runtime = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => None,
        _ => {
            let dir = std::env::temp_dir().join(format!("nested-e2e-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create XDG_RUNTIME_DIR");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                    .expect("chmod 0700 XDG_RUNTIME_DIR");
            }
            unsafe {
                std::env::set_var("XDG_RUNTIME_DIR", &dir);
            }
            Some(dir)
        }
    };

    let backend = NestedBackend::new()
        .unwrap_or_else(|error| fail(&format!("could not start the compositor: {error}")));
    let mut webviews = WebViews::new(backend, Config::default());

    let spec = SurfaceSpec::new(
        Source::Html {
            html: html.to_string(),
            base: None,
        },
        Policy::default(),
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

    // Pump the compositor/GTK until the page commits a frame *with the page
    // in it*. The first commits are GTK's pre-paint frames (an empty window),
    // so waiting for the red background is the real end-to-end signal.
    let started = Instant::now();
    let mut latest = None;
    let mut first_commit = None;
    let mut red = false;
    while started.elapsed() < timeout {
        webviews.frame(&live, viewport, &[]);
        if let Some(candidate) = webviews.frame_image(0) {
            if first_commit.is_none() {
                first_commit = Some(started.elapsed());
            }
            if let Some(bytes) = candidate.image.as_bytes(0) {
                red = bytes
                    .chunks_exact(4)
                    .any(|pixel| pixel[0] == 0x1e && pixel[1] == 0x1e && pixel[2] == 0xc8);
            }
            latest = Some(candidate);
            if red {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let Some(frame) = latest else {
        fail(&format!(
            "no frame committed within {:?} (WebKit never painted)",
            timeout
        ));
    };
    let elapsed = started.elapsed();

    let (frame_width, frame_height) = frame.size;
    if (frame_width, frame_height) != (width, height) {
        fail(&format!(
            "committed size {frame_width}x{frame_height} != configured {width}x{height}"
        ));
    }
    let bytes = frame
        .image
        .as_bytes(0)
        .unwrap_or_else(|| fail("the committed frame has no pixel data"));
    let expected = width as usize * height as usize * 4;
    if bytes.len() != expected {
        fail(&format!(
            "frame has {} bytes, expected {expected} ({}x{}x4)",
            bytes.len(),
            width,
            height
        ));
    }
    // ARGB8888 = BGRA in little-endian memory; `#c81e1e` is B=0x1e, G=0x1e,
    // R=0xc8. The page is mostly that red, with white text on top.
    let opaque = bytes.chunks_exact(4).filter(|pixel| pixel[3] != 0).count();
    let red = bytes
        .chunks_exact(4)
        .filter(|pixel| pixel[0] == 0x1e && pixel[1] == 0x1e && pixel[2] == 0xc8)
        .count();
    if opaque == 0 {
        fail("the committed frame is fully transparent");
    }
    if red == 0 {
        // Diagnose rather than just fail: a few sampled pixels plus the most
        // common colours say whether the page is blank, white or garbled.
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
            .take(5)
            .map(|(pixel, count)| format!("#{:02x}{:02x}{:02x}{:02x} x{count}", pixel[2], pixel[1], pixel[0], pixel[3]))
            .collect();
        eprintln!(
            "diagnostics: first commit after {:?}, size {}x{}, top colours: {}",
            first_commit.unwrap_or(elapsed),
            frame_width,
            frame_height,
            top.join(", ")
        );
        fail("no red page pixels in the committed frame (wrong content?)");
    }
    println!(
        "frame: {frame_width}x{frame_height}, first commit after {:.2?}, \
         {opaque}/{} opaque, {red} red pixels",
        elapsed,
        width as usize * height as usize
    );

    // Re-activating with the same spec must reuse the engine (no recreate).
    webviews
        .activate(0, SurfaceSpec::new(
            Source::Html {
                html: html.to_string(),
                base: None,
            },
            Policy::default(),
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

    if let Some(dir) = owned_runtime {
        let _ = std::fs::remove_dir_all(dir);
    }
    println!("PASS: nested compositor committed a correctly sized frame and tore down");
}
