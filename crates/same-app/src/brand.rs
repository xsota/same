//! SAME's icon and mascot (Shark Egg), embedded from `assets/brand`.
//!
//! In the app the transparent Shark Egg is used; the icon with its background
//! tile is embedded in the executable (see `build.rs`).

use gpui::{Image, ImageFormat};
use std::sync::{Arc, OnceLock};

fn svg(cell: &'static OnceLock<Arc<Image>>, bytes: &'static [u8]) -> Arc<Image> {
    cell.get_or_init(|| Arc::new(Image::from_bytes(ImageFormat::Svg, bytes.to_vec())))
        .clone()
}

pub fn shark_egg() -> Arc<Image> {
    static CELL: OnceLock<Arc<Image>> = OnceLock::new();
    svg(
        &CELL,
        include_bytes!("../../../assets/brand/shark-egg.svg"),
    )
}

/// The window icon for X11 (Windows uses the icon embedded in the exe;
/// Wayland and macOS take it from the desktop entry / bundle).
pub fn window_icon() -> Option<Arc<image::RgbaImage>> {
    let png = include_bytes!("../../../assets/brand/png/same-icon-256.png");
    image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .ok()
        .map(|img| Arc::new(img.into_rgba8()))
}

/// Shark Egg hopping in place: the loading indicator.
///
/// One hop per cycle with a gravity-like arc, a squash on landing and a
/// ground shadow that shrinks while it is in the air. `size` is Shark Egg's
/// width; the element is `size` wide and a little taller, to leave room for
/// the hop. `shadow` is the shadow's color (e.g. the theme's foreground, so
/// it shows on dark and light backgrounds alike).
pub fn shark_egg_loader(
    id: impl Into<gpui::ElementId>,
    size: f32,
    shadow: gpui::Hsla,
) -> gpui::AnyElement {
    use gpui::{
        div, img, px, Animation, AnimationExt as _, InteractiveElement as _, IntoElement,
        ObjectFit, ParentElement as _, Styled as _, StyledImage as _,
    };
    use std::time::Duration;

    let jump = size * 0.32;
    let shadow_h = (size * 0.12).max(2.0);
    let height = size + jump + shadow_h * 0.5;
    let period = Duration::from_millis(900);

    // 0 on the ground .. 1 at the top of the hop.
    let hop = |t: f32| 4.0 * t * (1.0 - t);
    // Squash while touching down (the start and end of each cycle).
    let squash = move |t: f32| (1.0 - hop(t) * 4.0).max(0.0) * 0.14;

    let shark_egg = img(shark_egg()).object_fit(ObjectFit::Fill).with_animation(
        "shark-egg",
        Animation::new(period).repeat_synced(),
        move |el, t| {
            let s = squash(t);
            let w = size * (1.0 + s * 0.6);
            let h = size * (1.0 - s);
            el.absolute()
                .w(px(w))
                .h(px(h))
                .left(px((size - w) / 2.0))
                .bottom(px(shadow_h * 0.5 + hop(t) * jump))
        },
    );

    let shadow_color = shadow;
    let shadow = div()
        .absolute()
        .rounded_full()
        .bg(shadow_color)
        .with_animation(
            "shadow",
            Animation::new(period).repeat_synced(),
            move |el, t| {
                let k = 1.0 - hop(t) * 0.45;
                let w = size * 0.62 * k;
                el.w(px(w))
                    .h(px(shadow_h))
                    .left(px((size - w) / 2.0))
                    .bottom(px(0.0))
                    .opacity(0.08 + 0.10 * k)
            },
        );

    div()
        // The animations' ids are scoped under this one.
        .id(id)
        .relative()
        .flex_shrink_0()
        .w(px(size))
        .h(px(height))
        .child(shadow)
        .child(shark_egg)
        .into_any_element()
}
