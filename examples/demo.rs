//! Standalone demo: the reuse proof for `gpui-webview` (EMBEDS.md §15).
//!
//! A GPUI window with a scrolling list of `N` URL surfaces. Click one to make
//! it live; the manager keeps exactly one engine alive, reuses it by
//! navigation when another block is clicked, clips it to the viewport and
//! tears it down when it scrolls out. It links no editor or Notesnook code.
//!
//! ```sh
//! cargo run -p gpui-webview --example demo                       # 6 surfaces
//! cargo run -p gpui-webview --example demo -- 20                 # 20 surfaces
//! cargo run -p gpui-webview --features wry --example demo -- 20  # real WKWebView
//! ```
//!
//! Without a backend feature the demo says so in the status line and draws the
//! posters only: that is the `Error::Unsupported` path every app must handle.

use gpui_kit::*;
use gpui_webview::{
    Config, Placement, Policy, Source, SurfaceSpec, WebViewEvent, WebViews,
};

actions!(gpui_webview_demo, [Quit, Deactivate]);

/// One demo block: a title and the URL it would activate.
struct DemoBlock {
    title: String,
    src: String,
}

/// The scrolling list.
struct Demo {
    blocks: Vec<DemoBlock>,
    scroll: Pixels,
    /// The live key, when `activate` succeeded.
    live: Option<u64>,
    /// The last activation error, shown in the status line.
    error: Option<String>,
    /// `Some` once a backend was created; `None` keeps retrying until then.
    webviews: Option<WebViews>,
    /// Set when `platform_default` failed, so we stop retrying every frame.
    backend_failed: bool,
    focus: FocusHandle,
}

const BLOCK_HEIGHT: f32 = 260.;
const GAP: f32 = 16.;

fn demo_blocks(count: usize) -> Vec<DemoBlock> {
    let seeds = [
        "https://www.youtube.com/embed/dQw4w9WgXcQ",
        "https://player.vimeo.com/video/76979871",
        "https://example.com/",
        "https://www.youtube.com/embed/9bZkp7q19f0",
        "https://player.vimeo.com/video/1084537",
        "https://example.org/",
    ];
    (0..count)
        .map(|index| DemoBlock {
            title: format!("Surface #{index}"),
            src: seeds[index % seeds.len()].to_string(),
        })
        .collect()
}

impl Demo {
    fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let count = std::env::args()
            .nth(2)
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(6)
            .max(1);
        Self {
            blocks: demo_blocks(count),
            scroll: px(0.),
            live: None,
            error: None,
            webviews: None,
            backend_failed: false,
            focus: cx.focus_handle(),
        }
    }

    /// Lazily create the manager on the first paint (the window exists).
    fn webviews(&mut self, window: &mut Window) -> Option<&mut WebViews> {
        if self.webviews.is_none() {
            if self.backend_failed {
                return None;
            }
            match WebViews::platform_default(Config::default()) {
                Ok(webviews) => self.webviews = Some(webviews),
                Err(error) => {
                    self.error = Some(error.to_string());
                    self.backend_failed = true;
                    return None;
                }
            }
        }
        let webviews = self.webviews.as_mut()?;
        let _ = webviews.attach(window);
        Some(webviews)
    }

    fn block_rect(&self, bounds: Bounds<Pixels>, index: usize) -> Bounds<Pixels> {
        let top = bounds.origin.y + px(index as f32 * (BLOCK_HEIGHT + GAP)) - self.scroll;
        Bounds::new(
            point(bounds.origin.x + px(8.), top),
            size(bounds.size.width - px(16.), px(BLOCK_HEIGHT)),
        )
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let key = index as u64;
        if self.live == Some(key) {
            if let Some(webviews) = self.webviews.as_mut() {
                webviews.deactivate(key);
            }
            self.live = None;
            cx.notify();
            return;
        }
        let spec = SurfaceSpec::new(
            Source::Url(self.blocks[index].src.clone()),
            Policy::media(),
            self.blocks[index].title.clone(),
        );
        let Some(webviews) = self.webviews(window) else {
            return;
        };
        match webviews.activate(key, spec) {
            Ok(_) => {
                webviews.set_focus(key, true);
                self.live = Some(key);
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn deactivate(&mut self, cx: &mut Context<Self>) {
        if let (Some(key), Some(webviews)) = (self.live, self.webviews.as_mut()) {
            webviews.deactivate(key);
        }
        self.live = None;
        cx.notify();
    }
}

impl Render for Demo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content_height = px(self.blocks.len() as f32 * (BLOCK_HEIGHT + GAP));
        let entity = cx.entity();
        let element = DemoElement {
            entity,
            blocks: self.blocks.iter().map(|b| b.title.clone()).collect(),
            scroll: self.scroll,
        };
        let status = match (&self.live, &self.error) {
            (Some(key), _) => format!("live: #{key} (Esc deactivates)"),
            (None, Some(error)) => format!("no backend: {error} (posters only)"),
            (None, None) => "click a block to activate".to_string(),
        };
        let scroll = self.scroll;
        let blocks = self.blocks.len();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xdfe3ea))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .border_b_1()
                    .border_color(rgb(0x2a2f39))
                    .child(status),
            )
            .child(
                div()
                    .id("demo-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .key_context("Demo")
                    .track_focus(&self.focus)
                    .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                        let delta = event.delta.pixel_delta(px(20.)).y;
                        let limit = (content_height - px(400.)).max(px(0.));
                        this.scroll = (scroll + delta).clamp(px(0.), limit);
                        cx.notify();
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            // Hit-test in content coordinates.
                            let local_y = event.position.y + this.scroll;
                            let index =
                                (local_y.as_f32() / (BLOCK_HEIGHT + GAP)).floor() as usize;
                            if index < blocks {
                                this.activate(index, window, cx);
                            }
                        }),
                    )
                    .child(element),
            )
            .on_action(cx.listener(|this, _: &Deactivate, _, cx| this.deactivate(cx)))
            .on_action(cx.listener(|_, _: &Quit, _, cx| cx.quit()))
    }
}

/// Paints the posters and drives `WebViews::frame`/`place` with real window
/// coordinates.
struct DemoElement {
    entity: Entity<Demo>,
    blocks: Vec<String>,
    scroll: Pixels,
}

impl IntoElement for DemoElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for DemoElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name("demo-content".into()))
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.position = Position::Absolute;
        style.size.width = relative(1.).into();
        // The scroll extent: taller than the viewport by the block list.
        style.size.height = px(self.blocks.len() as f32 * (BLOCK_HEIGHT + GAP)).into();
        style.overflow = point(Overflow::Hidden, Overflow::Hidden);
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let blocks = self.blocks.clone();
        let scroll = self.scroll;
        self.entity.update(cx, |this, cx| {
            let clip = bounds;
            let mut live: Vec<(u64, Placement)> = Vec::new();
            if let Some(key) = this.live {
                let rect = this.block_rect(bounds, key as usize);
                live.push((key, Placement::clipped_to(rect, clip)));
            }
            for (index, title) in blocks.iter().enumerate() {
                let rect = this.block_rect(bounds, index);
                let visible = rect.origin.y < clip.origin.y + clip.size.height
                    && clip.origin.y < rect.origin.y + rect.size.height;
                if !visible {
                    continue;
                }
                window.paint_quad(PaintQuad {
                    bounds: rect,
                    corner_radii: Corners::all(px(8.)),
                    background: if this.live == Some(index as u64) {
                        rgb(0x1d2430).into()
                    } else {
                        rgb(0x1a1f28).into()
                    },
                    border_widths: Edges::all(px(1.)),
                    border_color: rgb(0x2f3644).into(),
                    border_style: BorderStyle::Solid,
                });
                let font = window.text_style().font();
                let run = TextRun {
                    len: title.len(),
                    font,
                    color: rgb(0x9aa4b2).into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let line = window
                    .text_system()
                    .shape_line(title.clone().into(), px(15.), &[run], None);
                let _ = line.paint(
                    point(rect.origin.x + px(12.), rect.origin.y + px(10.)),
                    px(20.),
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            if let Some(webviews) = this.webviews.as_mut() {
                webviews.frame(&live, clip, &[]);
                for event in webviews.events() {
                    match event {
                        WebViewEvent::OpenExternal { url, .. } => cx.open_url(&url),
                        WebViewEvent::TitleChanged { title, .. } => {
                            eprintln!("surface title: {title}");
                        }
                        WebViewEvent::LoadFailed { error, .. } => {
                            eprintln!("surface load failed: {error}");
                        }
                        WebViewEvent::Closed { key, .. } => {
                            if this.live == Some(key) {
                                this.live = None;
                            }
                        }
                        _ => {}
                    }
                }
            }
            let _ = scroll;
        });
    }
}

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("secondary-q", Quit, None),
            KeyBinding::new("escape", Deactivate, Some("Demo")),
        ]);
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                cx.new(|cx| Demo::new(window, cx))
            })
            .expect("open demo window");
        })
        .detach();
    });
}
