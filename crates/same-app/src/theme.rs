use gpui::{px, rgb, App, Pixels, Rgba, SharedString, Window};
use gpui_component::theme::{Theme, ThemeConfig, ThemeMode as GpuiThemeMode, ThemeRegistry};
use same_core::{AppConfig, ColorTheme, ThemeMode};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

/// The current theme's primary colour as 0xRRGGBB. Read by [`accent`] during
/// render, set by [`apply_gpui_theme`].
static ACCENT_HEX: AtomicU32 = AtomicU32::new(0x69C8E8);
/// Foreground for text/icons placed on [`accent`].
static ACCENT_FG_HEX: AtomicU32 = AtomicU32::new(0x142D49);
/// Mode-aware text/link accent as 0xRRGGBB, adjusted for WCAG AA legibility
/// against the active theme's surfaces.
static ACCENT_TEXT_HEX: AtomicU32 = AtomicU32::new(0x69C8E8);
/// Mode-aware accent for standalone indicators (status dots, focus rings,
/// bars) adjusted for >= 3:1 contrast against the active theme's surfaces.
static ACCENT_INDICATOR_HEX: AtomicU32 = AtomicU32::new(0x69C8E8);

/// The theme's primary colour (buttons, highlights, badges).
pub fn accent() -> Rgba {
    rgb(ACCENT_HEX.load(Ordering::Relaxed))
}

/// Mode-aware accent for standalone indicators (status dots, rings, bars).
pub fn accent_indicator() -> Rgba {
    rgb(ACCENT_INDICATOR_HEX.load(Ordering::Relaxed))
}

/// Mode-aware accent for text and links, legible against the active background.
pub fn accent_text() -> Rgba {
    rgb(ACCENT_TEXT_HEX.load(Ordering::Relaxed))
}

/// High-contrast foreground colour for text/icons placed on [`accent`].
pub fn accent_foreground() -> Rgba {
    rgb(ACCENT_FG_HEX.load(Ordering::Relaxed))
}

/// The seed colours of one theme in one mode; every other surface colour is
/// derived from these in [`Palette::colors`].
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Main content background.
    pub background: u32,
    /// Sidebar, cards, title bar.
    pub surface: u32,
    /// Body text.
    pub foreground: u32,
    /// Primary buttons, focus rings, selection.
    pub primary: u32,
    /// Text/icons on `primary`.
    pub primary_foreground: u32,
}

/// Light and dark palettes of `theme`.
pub fn palette(theme: ColorTheme, mode: ThemeMode) -> Palette {
    const fn p(
        background: u32,
        surface: u32,
        foreground: u32,
        primary: u32,
        primary_foreground: u32,
    ) -> Palette {
        Palette {
            background,
            surface,
            foreground,
            primary,
            primary_foreground,
        }
    }
    match (theme, mode) {
        // Deep Ocean #142D49 / Shark Blue #69C8E8 / Sea Foam #E8FAFF.
        (ColorTheme::Ocean, ThemeMode::Dark) => p(0x142D49, 0x10253D, 0xE8FAFF, 0x69C8E8, 0x142D49),
        (ColorTheme::Ocean, ThemeMode::Light) => {
            p(0xF7FDFF, 0xE8FAFF, 0x142D49, 0x69C8E8, 0x142D49)
        }
        (ColorTheme::Classic, ThemeMode::Dark) => {
            p(0x0A0A0A, 0x171717, 0xFAFAFA, 0x2BA6CB, 0x0B0F14)
        }
        (ColorTheme::Classic, ThemeMode::Light) => {
            p(0xFFFFFF, 0xFAFAFA, 0x0A0A0A, 0x2BA6CB, 0x0B0F14)
        }
        (ColorTheme::Twilight, ThemeMode::Dark) => {
            p(0x17142B, 0x1E1A37, 0xECEBFF, 0x9B8CFA, 0x17142B)
        }
        (ColorTheme::Twilight, ThemeMode::Light) => {
            p(0xFBFAFF, 0xF1EFFF, 0x1E1A3D, 0x5B4FD6, 0xFFFFFF)
        }
        (ColorTheme::Sakura, ThemeMode::Dark) => {
            p(0x251820, 0x2E1E29, 0xFFEEF5, 0xF28AB8, 0x251820)
        }
        (ColorTheme::Sakura, ThemeMode::Light) => {
            p(0xFFFAFC, 0xFFEEF5, 0x3A1D2C, 0xC2367A, 0xFFFFFF)
        }
    }
}

/// Every colour the app sets, derived from a [`Palette`].
#[derive(Debug, Clone, Copy)]
struct Derived {
    border: u32,
    input_border: u32,
    muted: u32,
    muted_foreground: u32,
    secondary: u32,
    secondary_hover: u32,
    secondary_active: u32,
    hover: u32,
    primary_hover: u32,
    primary_active: u32,
    accent_text: u32,
    ring: u32,
    scrollbar_thumb: u32,
}

impl Palette {
    /// The surfaces text is drawn on: background, surface, secondary, and a
    /// selected list row over the sidebar.
    fn text_surfaces(&self) -> [u32; 4] {
        [
            self.background,
            self.surface,
            mix(self.background, self.foreground, 0.10),
            mix(self.surface, self.primary, 0.18),
        ]
    }

    fn colors(&self) -> Derived {
        let (bg, fg) = (self.background, self.foreground);
        // Hover/active move the primary away from its foreground so contrast
        // only improves.
        let away = if relative_luminance(self.primary_foreground) < relative_luminance(self.primary)
        {
            0xFFFFFF
        } else {
            0x000000
        };
        let border = mix(bg, fg, 0.14);
        let surfaces = self.text_surfaces();
        let ring_surfaces = [surfaces[0], surfaces[1], surfaces[2], surfaces[3], border];
        Derived {
            border,
            input_border: mix(bg, fg, 0.20),
            muted: mix(bg, fg, 0.08),
            muted_foreground: legible(mix(fg, bg, 0.40), fg, &surfaces),
            secondary: mix(bg, fg, 0.10),
            secondary_hover: mix(bg, fg, 0.14),
            secondary_active: mix(bg, fg, 0.18),
            hover: mix(bg, fg, 0.07),
            primary_hover: mix(self.primary, away, 0.10),
            primary_active: mix(self.primary, away, 0.20),
            accent_text: legible(self.primary, fg, &surfaces),
            ring: ensure_contrast(self.primary, fg, &ring_surfaces, 3.0),
            scrollbar_thumb: mix(bg, fg, 0.30),
        }
    }
}

/// Mixes `amount` (0..=1) of colour `to` into colour `from` (both 0xRRGGBB).
fn mix(from: u32, to: u32, amount: f32) -> u32 {
    let channel = |shift: u32| {
        let a = ((from >> shift) & 0xFF) as f32;
        let b = ((to >> shift) & 0xFF) as f32;
        ((a + (b - a) * amount).round() as u32).min(0xFF) << shift
    };
    channel(16) | channel(8) | channel(0)
}

pub fn status_green() -> Rgba {
    rgb(0x3FB950)
}

pub fn status_amber() -> Rgba {
    rgb(0xD29922)
}

pub fn status_red() -> Rgba {
    rgb(0xF85149)
}

/// WCAG 2.1 relative luminance for a 0xRRGGBB sRGB colour.
fn relative_luminance(hex: u32) -> f32 {
    let linear = |shift: u32| {
        let c = ((hex >> shift) & 0xFF) as f32 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0)
}

fn contrast_ratio(a: u32, b: u32) -> f32 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Moves `hex` toward `toward` until it reaches `min_ratio` contrast against
/// every one of `surfaces`.
fn ensure_contrast(hex: u32, toward: u32, surfaces: &[u32], min_ratio: f32) -> u32 {
    let ok = |c: u32| surfaces.iter().all(|&s| contrast_ratio(c, s) >= min_ratio);
    (0..=20)
        .map(|step| mix(hex, toward, step as f32 / 20.0))
        .find(|&c| ok(c))
        .unwrap_or(toward)
}

/// Moves `hex` toward `toward` until it reaches WCAG AA normal-text contrast
/// (>= 4.5:1) against every one of `surfaces`.
fn legible(hex: u32, toward: u32, surfaces: &[u32]) -> u32 {
    ensure_contrast(hex, toward, surfaces, 4.5)
}

fn hex_str(hex: u32) -> String {
    format!("#{hex:06x}")
}

fn hex_alpha(hex: u32, alpha: u8) -> String {
    format!("#{hex:06x}{alpha:02x}")
}

/// Builds a gpui-component theme for `theme`/`mode` on top of the built-in
/// default for that mode, so keys not set here keep their stock values.
fn theme_config(theme: ColorTheme, mode: ThemeMode, cx: &App) -> Option<Rc<ThemeConfig>> {
    let registry = ThemeRegistry::global(cx);
    let base = match mode {
        ThemeMode::Dark => registry.default_dark_theme(),
        ThemeMode::Light => registry.default_light_theme(),
    };
    theme_config_json(theme, mode, base)
        .and_then(|value| serde_json::from_value::<ThemeConfig>(value).ok())
        .map(Rc::new)
}

/// `base` as JSON with the theme's colours written over it.
fn theme_config_json(
    theme: ColorTheme,
    mode: ThemeMode,
    base: &ThemeConfig,
) -> Option<serde_json::Value> {
    let mut value = serde_json::to_value(base).ok()?;
    let pal = palette(theme, mode);
    let d = pal.colors();
    let (bg, surface, fg, primary) = (pal.background, pal.surface, pal.foreground, pal.primary);
    let list_active_alpha = match mode {
        ThemeMode::Dark => 0x2E,
        ThemeMode::Light => 0x1F,
    };
    let colors = [
        ("background", hex_str(bg)),
        ("foreground", hex_str(fg)),
        ("border", hex_str(d.border)),
        ("input.border", hex_str(d.input_border)),
        ("muted.background", hex_str(d.muted)),
        ("muted.foreground", hex_str(d.muted_foreground)),
        ("accent.background", hex_str(d.secondary)),
        ("accent.foreground", hex_str(fg)),
        ("accordion.background", hex_str(bg)),
        ("group_box.background", hex_str(surface)),
        ("group_box.foreground", hex_str(fg)),
        ("description_list.label.background", hex_str(surface)),
        ("description_list.label.foreground", hex_str(fg)),
        ("caret", hex_str(d.accent_text)),
        ("ring", hex_str(d.ring)),
        ("drag.border", hex_str(primary)),
        ("drop_target.background", hex_alpha(primary, 0x33)),
        ("link", hex_str(d.accent_text)),
        ("link.hover", hex_str(d.accent_text)),
        ("link.active", hex_str(d.accent_text)),
        ("primary.background", hex_str(primary)),
        ("primary.hover.background", hex_str(d.primary_hover)),
        ("primary.active.background", hex_str(d.primary_active)),
        ("primary.foreground", hex_str(pal.primary_foreground)),
        ("secondary.background", hex_str(d.secondary)),
        ("secondary.hover.background", hex_str(d.secondary_hover)),
        ("secondary.active.background", hex_str(d.secondary_active)),
        ("secondary.foreground", hex_str(fg)),
        ("list.background", hex_str(bg)),
        ("list.even.background", hex_str(mix(bg, fg, 0.03))),
        ("list.head.background", hex_str(surface)),
        ("list.hover.background", hex_str(d.hover)),
        (
            "list.active.background",
            hex_alpha(primary, list_active_alpha),
        ),
        ("list.active.border", hex_str(primary)),
        ("list.highlight.foreground", hex_str(d.accent_text)),
        ("popover.background", hex_str(bg)),
        ("popover.foreground", hex_str(fg)),
        ("progress.bar.background", hex_str(primary)),
        ("scrollbar.background", hex_alpha(bg, 0)),
        (
            "scrollbar.thumb.background",
            hex_alpha(d.scrollbar_thumb, 0xE6),
        ),
        (
            "scrollbar.thumb.hover.background",
            hex_str(d.scrollbar_thumb),
        ),
        ("selection.background", hex_alpha(primary, 0x55)),
        ("sidebar.background", hex_str(surface)),
        ("sidebar.foreground", hex_str(fg)),
        ("sidebar.border", hex_str(d.border)),
        ("sidebar.accent.background", hex_str(d.secondary)),
        ("sidebar.accent.foreground", hex_str(fg)),
        ("sidebar.primary.background", hex_str(primary)),
        (
            "sidebar.primary.foreground",
            hex_str(pal.primary_foreground),
        ),
        ("skeleton.background", hex_str(d.muted)),
        ("slider.background", hex_str(primary)),
        ("switch.background", hex_str(d.secondary_active)),
        ("tab.active.background", hex_str(bg)),
        ("tab.active.foreground", hex_str(fg)),
        ("tab.foreground", hex_str(d.muted_foreground)),
        ("tab_bar.background", hex_str(surface)),
        ("tab_bar.segmented.background", hex_str(surface)),
        ("title_bar.background", hex_str(surface)),
        ("title_bar.border", hex_str(d.border)),
    ];
    let extra = [
        ("status_bar.background", hex_str(surface)),
        ("status_bar.border", hex_str(d.border)),
        ("table.background", hex_str(bg)),
        ("table.head.background", hex_str(surface)),
        ("table.head.foreground", hex_str(d.muted_foreground)),
        ("table.hover.background", hex_str(d.hover)),
        ("table.row.border", hex_str(d.border)),
        ("window.border", hex_str(d.border)),
    ];
    let map = value.get_mut("colors")?.as_object_mut()?;
    for (key, color) in colors.into_iter().chain(extra) {
        map.insert(key.to_string(), color.into());
    }
    // Code blocks follow the theme's surfaces too.
    if let Some(highlight) = value.get_mut("highlight").and_then(|h| h.as_object_mut()) {
        highlight.insert("editor.background".into(), hex_str(bg).into());
        highlight.insert("editor.foreground".into(), hex_str(fg).into());
        highlight.insert(
            "editor.active_line.background".into(),
            hex_str(mix(bg, fg, 0.05)).into(),
        );
    }
    let (_, en) = theme.label();
    let suffix = match mode {
        ThemeMode::Dark => "Dark",
        ThemeMode::Light => "Light",
    };
    value["name"] = format!("Same {en} {suffix}").into();
    value["mode"] = serde_json::to_value(match mode {
        ThemeMode::Dark => GpuiThemeMode::Dark,
        ThemeMode::Light => GpuiThemeMode::Light,
    })
    .ok()?;
    Some(value)
}

/// Platform default UI font.
pub fn default_ui_font() -> &'static str {
    if cfg!(target_os = "windows") {
        "Yu Gothic UI"
    } else if cfg!(target_os = "linux") {
        "DejaVu Sans"
    } else {
        ".SystemUIFont"
    }
}

/// Platform default code font.
pub fn default_mono_font() -> &'static str {
    if cfg!(target_os = "windows") {
        "Consolas"
    } else if cfg!(target_os = "linux") {
        "DejaVu Sans Mono"
    } else {
        "Menlo"
    }
}

/// gpui-component's base font size (px) before any user override; captured
/// in [`apply_gpui_theme`] so the settings screen can show it.
static DEFAULT_FONT_SIZE_BITS: AtomicU32 = AtomicU32::new(0x4180_0000); // 16.0
/// (font_size, mono_font_size) as gpui-component first set them.
static DEFAULT_SIZES: OnceLock<(Pixels, Pixels)> = OnceLock::new();

pub fn default_font_size() -> f32 {
    f32::from_bits(DEFAULT_FONT_SIZE_BITS.load(Ordering::Relaxed))
}

pub fn apply_gpui_theme(config: &AppConfig, window: Option<&mut Window>, cx: &mut App) {
    let mode = config.theme;
    let color_theme = config.color_theme;
    let ui_font: SharedString = config
        .ui_font
        .clone()
        .unwrap_or_else(|| default_ui_font().to_string())
        .into();
    let mono_font: SharedString = config
        .mono_font
        .clone()
        .unwrap_or_else(|| default_mono_font().to_string())
        .into();
    let font_size = config.font_size();
    let gpui_mode = match mode {
        ThemeMode::Dark => GpuiThemeMode::Dark,
        ThemeMode::Light => GpuiThemeMode::Light,
    };
    let pal = palette(color_theme, mode);
    let derived = pal.colors();
    // Read by custom-drawn elements (badges, status dots, usage bars, previews).
    ACCENT_HEX.store(pal.primary, Ordering::Relaxed);
    ACCENT_FG_HEX.store(pal.primary_foreground, Ordering::Relaxed);
    ACCENT_TEXT_HEX.store(derived.accent_text, Ordering::Relaxed);
    ACCENT_INDICATOR_HEX.store(derived.ring, Ordering::Relaxed);

    // Install the theme's light and dark configs, then `Theme::change`
    // reloads the one for `mode` (it resets every colour from the config).
    if !cx.has_global::<Theme>() {
        Theme::change(gpui_mode, None, cx);
    }
    let light = theme_config(color_theme, ThemeMode::Light, cx);
    let dark = theme_config(color_theme, ThemeMode::Dark, cx);
    if light.is_none() || dark.is_none() {
        tracing::warn!("could not build the {color_theme:?} theme; using stock colours");
    }
    Theme::update(cx, |theme| {
        if let Some(light) = light {
            theme.light_theme = light;
        }
        if let Some(dark) = dark {
            theme.dark_theme = dark;
        }
    });
    Theme::change(gpui_mode, window, cx);
    tracing::info!("THEMECHECK {} bg={:?}", Theme::global(cx).theme_name(), Rgba::from(Theme::global(cx).background));
    Theme::update(cx, |theme| {
        // Always assigned (not only when overridden) so "reset" takes effect
        // even if `Theme::change` keeps the previous families.
        theme.font_family = ui_font.clone();
        theme.mono_font_family = mono_font.clone();

        // `Root` uses `font_size` as the window's rem size, so every
        // rem-based text size (text_sm, text_xs, …) follows it. Code text
        // scales by the same ratio. The defaults are captured once, before
        // any override, because `Theme::change` may keep the sizes as is.
        let (default_size, default_mono) =
            *DEFAULT_SIZES.get_or_init(|| (theme.font_size, theme.mono_font_size));
        let default_px = f32::from(default_size);
        if default_px > 0.0 {
            DEFAULT_FONT_SIZE_BITS.store(default_px.to_bits(), Ordering::Relaxed);
        }
        match font_size {
            Some(size) if default_px > 0.0 => {
                theme.font_size = px(size);
                theme.mono_font_size = default_mono * (size / default_px);
            }
            _ => {
                theme.font_size = default_size;
                theme.mono_font_size = default_mono;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_palette() -> impl Iterator<Item = (ColorTheme, ThemeMode, Palette)> {
        ColorTheme::ALL.into_iter().flat_map(|t| {
            [ThemeMode::Light, ThemeMode::Dark]
                .into_iter()
                .map(move |m| (t, m, palette(t, m)))
        })
    }

    fn assert_aa(what: &str, fg: u32, bg: u32) {
        let ratio = contrast_ratio(fg, bg);
        assert!(
            ratio >= 4.5,
            "{what}: #{fg:06X} on #{bg:06X} contrast {ratio:.2} < 4.5"
        );
    }

    #[test]
    fn primary_button_states_meet_wcag_aa_contrast() {
        for (theme, mode, pal) in every_palette() {
            let d = pal.colors();
            for (state, bg) in [
                ("base", pal.primary),
                ("hover", d.primary_hover),
                ("active", d.primary_active),
            ] {
                assert_aa(
                    &format!("{theme:?} {mode:?} primary {state}"),
                    pal.primary_foreground,
                    bg,
                );
            }
        }
    }

    #[test]
    fn text_colours_meet_wcag_aa_on_every_surface() {
        for (theme, mode, pal) in every_palette() {
            let d = pal.colors();
            for surface in pal.text_surfaces() {
                assert_aa(
                    &format!("{theme:?} {mode:?} foreground"),
                    pal.foreground,
                    surface,
                );
                assert_aa(
                    &format!("{theme:?} {mode:?} muted foreground"),
                    d.muted_foreground,
                    surface,
                );
                assert_aa(
                    &format!("{theme:?} {mode:?} accent text"),
                    d.accent_text,
                    surface,
                );
                let ring_ratio = contrast_ratio(d.ring, surface);
                assert!(
                    ring_ratio >= 3.0,
                    "{theme:?} {mode:?} focus ring: #{:06x} on #{surface:06x} has contrast {ring_ratio:.2} (< 3.0)",
                    d.ring,
                );
            }
        }
    }

    /// A misspelt key would be dropped silently when the JSON is parsed back
    /// into a `ThemeConfig`, leaving the stock colour in place.
    #[test]
    fn every_theme_key_survives_parsing() {
        for (theme, mode, _) in every_palette() {
            let value = theme_config_json(theme, mode, &ThemeConfig::default()).unwrap();
            let parsed: ThemeConfig = serde_json::from_value(value.clone()).unwrap();
            let back = serde_json::to_value(&parsed).unwrap();
            let written = value["colors"].as_object().unwrap();
            for (key, color) in written {
                if color.is_null() {
                    continue;
                }
                assert_eq!(
                    back["colors"].get(key),
                    Some(color),
                    "{theme:?} {mode:?} key {key}"
                );
            }
            assert_eq!(parsed.mode.is_dark(), mode == ThemeMode::Dark);
        }
    }

    #[test]
    fn ocean_uses_brand_colours() {
        let dark = palette(ColorTheme::Ocean, ThemeMode::Dark);
        assert_eq!(dark.background, 0x142D49);
        assert_eq!(dark.primary, 0x69C8E8);
        assert_eq!(dark.foreground, 0xE8FAFF);
        let light = palette(ColorTheme::Ocean, ThemeMode::Light);
        assert_eq!(light.surface, 0xE8FAFF);
        assert_eq!(light.foreground, 0x142D49);
    }
}
