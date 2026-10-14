//! Child-view placement maths, shared by the Windows backend and unit-tested
//! on every platform.
//!
//! Every edge is rounded independently, and both rectangles are derived from
//! the *same* rounded values: the container gets the clip's rounded edges, the
//! webview gets the block's rounded edges relative to the clip. Rounding an
//! origin + size instead lets the two drift apart by a pixel at fractional
//! scales (125%, 150%, 200%), which shows up as the page wobbling against its
//! clip while scrolling.

/// Physical rectangles for one placement at `scale`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PhysicalPlacement {
    /// Container position (`x`, `y`) and size (`width`, `height`).
    pub(super) container: (i32, i32, i32, i32),
    /// Webview position relative to the container, and size.
    pub(super) webview: (i32, i32, i32, i32),
}

/// `rect`/`clip` are logical `(x, y, width, height)` window coordinates.
pub(super) fn physical_placement(
    rect: (f64, f64, f64, f64),
    clip: (f64, f64, f64, f64),
    scale: f64,
) -> PhysicalPlacement {
    let edge = |value: f64| (value * scale).round() as i32;
    let (rect_x, rect_y, rect_width, rect_height) = rect;
    let (clip_x, clip_y, clip_width, clip_height) = clip;
    let clip_left = edge(clip_x);
    let clip_top = edge(clip_y);
    let clip_right = edge(clip_x + clip_width);
    let clip_bottom = edge(clip_y + clip_height);
    let rect_left = edge(rect_x);
    let rect_top = edge(rect_y);
    let rect_right = edge(rect_x + rect_width);
    let rect_bottom = edge(rect_y + rect_height);
    PhysicalPlacement {
        container: (
            clip_left,
            clip_top,
            clip_right - clip_left,
            clip_bottom - clip_top,
        ),
        webview: (
            rect_left - clip_left,
            rect_top - clip_top,
            rect_right - rect_left,
            rect_bottom - rect_top,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(value: f64, scale: f64) -> i32 {
        (value * scale).round() as i32
    }

    /// The container's edges are exactly the clip's, at every common scale.
    #[test]
    fn container_edges_are_the_clip_edges() {
        let rect = (200.0, 150.0, 400.0, 300.0);
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let clip = (203.3, 152.7, 396.7, 297.3);
            let placement = physical_placement(rect, clip, scale);
            let (x, y, width, height) = placement.container;
            assert_eq!(x, edge(clip.0, scale), "scale {scale}");
            assert_eq!(y, edge(clip.1, scale), "scale {scale}");
            assert_eq!(x + width, edge(clip.0 + clip.2, scale), "scale {scale}");
            assert_eq!(y + height, edge(clip.1 + clip.3, scale), "scale {scale}");
        }
    }

    /// The webview and the container derive from the same rounded edges: the
    /// block's rounded edges are exactly the container's plus the webview's
    /// offset, and the webview's width is a difference of rounded edges.
    #[test]
    fn webview_shares_the_rounded_edges_with_the_container() {
        let rect = (0.3, 0.3, 400.4, 300.4);
        let clip = (1.0, 1.0, 399.4, 299.4);
        for scale in [1.25, 1.5, 2.0] {
            let placement = physical_placement(rect, clip, scale);
            let (container_x, container_y, ..) = placement.container;
            let (webview_x, webview_y, webview_width, webview_height) = placement.webview;
            assert_eq!(
                container_x + webview_x,
                edge(rect.0, scale),
                "block left edge, scale {scale}"
            );
            assert_eq!(
                container_y + webview_y,
                edge(rect.1, scale),
                "block top edge, scale {scale}"
            );
            assert_eq!(
                webview_width,
                edge(rect.0 + rect.2, scale) - edge(rect.0, scale),
                "block width, scale {scale}"
            );
            assert_eq!(
                webview_height,
                edge(rect.1 + rect.3, scale) - edge(rect.1, scale),
                "block height, scale {scale}"
            );
        }
    }

    /// The case origin+size rounding gets wrong: a fractional origin at 150%
    /// rounds to different pixel counts than the difference of rounded edges.
    #[test]
    fn fractional_origins_do_not_drift() {
        let rect = (0.3, 0.0, 10.3, 10.0);
        let clip = (1.0, 0.0, 9.6, 10.0);
        let placement = physical_placement(rect, clip, 1.5);
        // Old shape: round((rect.x - clip.x) * s) and round(rect.w * s).
        let old_offset = ((rect.0 - clip.0) * 1.5).round() as i32;
        let old_width = (rect.2 * 1.5).round() as i32;
        assert_ne!(
            placement.webview.0, old_offset,
            "the fixture must exercise the drift the fix removes"
        );
        assert_eq!(placement.webview.0, edge(rect.0, 1.5) - edge(clip.0, 1.5));
        assert_eq!(
            placement.webview.2,
            edge(rect.0 + rect.2, 1.5) - edge(rect.0, 1.5)
        );
        assert_eq!(old_width, 15);
        assert_eq!(placement.webview.2, 16);
    }

    /// A fully visible block has a zero offset and the container is the block.
    #[test]
    fn a_fully_visible_block_is_exact() {
        let block = (200.0, 150.0, 400.0, 300.0);
        let placement = physical_placement(block, block, 2.0);
        assert_eq!(placement.webview, (0, 0, 800, 600));
        assert_eq!(placement.container, (400, 300, 800, 600));
    }
}
