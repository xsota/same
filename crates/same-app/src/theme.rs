use gpui::{rgb, App, Hsla, Rgba, Window};
use gpui_component::theme::{Theme, ThemeMode as GpuiThemeMode};
use same_core::{AccentColor, ThemeMode};
use std::sync::atomic::{AtomicU32, Ordering};

/// The current accent as 0xRRGGBB. Read by [`accent`] during render, set by
/// [`apply_gpui_theme`].
static ACCENT_HEX: AtomicU32 = AtomicU32::new(0x2BA6CB);
/// Mode-aware text/link accent as 0xRRGGBB, adjusted for WCAG AA legibility
/// against the active theme background.
static ACCENT_TEXT_HEX: AtomicU32 = AtomicU32::new(0x2BA6CB);

/// The user's accent ("theme colour"); defaults to the brand teal.
pub fn accent() -> Rgba {
    rgb(ACCENT_HEX.load(Ordering::Relaxed))
}

/// Mode-aware accent for text and links, legible against the active background.
pub fn accent_text() -> Rgba {
    rgb(ACCENT_TEXT_HEX.load(Ordering::Relaxed))
}

/// High-contrast foreground colour for text/icons placed on [`accent`].
pub fn accent_foreground() -> Rgba {
    let (_, _, _, fg) = primary_palette(ACCENT_HEX.load(Ordering::Relaxed));
    rgb(fg)
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

/// Picks white or near-black foreground, whichever gives higher contrast
/// against `bg`, along with hover/active backgrounds that preserve legibility.
fn primary_palette(hex: u32) -> (u32, u32, u32, u32) {
    const LIGHT_FG: u32 = 0xFFFFFF;
    const DARK_FG: u32 = 0x0B0F14;
    let use_light_fg = contrast_ratio(hex, LIGHT_FG) >= contrast_ratio(hex, DARK_FG);
    let fg = if use_light_fg { LIGHT_FG } else { DARK_FG };
    // Shift hover/active away from the foreground so contrast stays strong.
    let hover = if use_light_fg {
        mix(hex, 0x000000, 0.10)
    } else {
        mix(hex, 0xFFFFFF, 0.10)
    };
    let active = if use_light_fg {
        mix(hex, 0x000000, 0.20)
    } else {
        mix(hex, 0xFFFFFF, 0.20)
    };
    (hex, hover, active, fg)
}

/// Adjusts `hex` toward black (light mode) or white (dark mode) until it
/// reaches WCAG AA normal-text contrast (>= 4.5:1) against the least favorable
/// surface used in that mode (including sidebar/card backgrounds).
fn accent_text_for_mode(hex: u32, mode: ThemeMode) -> u32 {
    let (surface, target) = match mode {
        ThemeMode::Light => (0xF4F4F5, 0x000000),
        ThemeMode::Dark => (0x27272A, 0xFFFFFF),
    };
    if contrast_ratio(hex, surface) >= 4.5 {
        return hex;
    }
    for step in 1..=20 {
        let candidate = mix(hex, target, step as f32 / 20.0);
        if contrast_ratio(candidate, surface) >= 4.5 {
            return candidate;
        }
    }
    target
}

pub fn apply_gpui_theme(
    mode: ThemeMode,
    accent_color: AccentColor,
    window: Option<&mut Window>,
    cx: &mut App,
) {
    let gpui_mode = match mode {
        ThemeMode::Dark => GpuiThemeMode::Dark,
        ThemeMode::Light => GpuiThemeMode::Light,
    };
    let text_hex = accent_text_for_mode(accent_color.hex(), mode);
    ACCENT_HEX.store(accent_color.hex(), Ordering::Relaxed);
    ACCENT_TEXT_HEX.store(text_hex, Ordering::Relaxed);
    // `Theme::change` resets every colour, so the accent is re-applied after
    // through `Theme::update` (which reconciles button tokens and syncs
    // Markdown/text defaults).
    Theme::change(gpui_mode, window, cx);
    Theme::update(cx, |theme| {
        let (primary, hover, active, fg) = primary_palette(accent_color.hex());
        let base: Hsla = rgb(primary).into();
        let hover_hsla: Hsla = rgb(hover).into();
        let active_hsla: Hsla = rgb(active).into();
        let fg_hsla: Hsla = rgb(fg).into();
        let link_hsla: Hsla = rgb(text_hex).into();

        theme.primary = base;
        theme.primary_hover = hover_hsla;
        theme.primary_active = active_hsla;
        theme.primary_foreground = fg_hsla;
        theme.button_primary = base;
        theme.button_primary_hover = hover_hsla;
        theme.button_primary_active = active_hsla;
        theme.button_primary_foreground = fg_hsla;
        theme.ring = base;
        theme.link = link_hsla;
        theme.caret = base;
        theme.list_active = base.opacity(match mode {
            ThemeMode::Dark => 0.18,
            ThemeMode::Light => 0.12,
        });
        theme.list_active_border = base;

        #[cfg(target_os = "linux")]
        {
            theme.font_family = "DejaVu Sans".into();
            theme.mono_font_family = "DejaVu Sans Mono".into();
        }
        #[cfg(target_os = "windows")]
        {
            theme.font_family = "Yu Gothic UI".into();
            theme.mono_font_family = "Consolas".into();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_button_states_meet_wcag_aa_contrast() {
        for preset in AccentColor::ALL {
            let (base, hover, active, fg) = primary_palette(preset.hex());
            for (state, bg) in [("base", base), ("hover", hover), ("active", active)] {
                let ratio = contrast_ratio(bg, fg);
                assert!(
                    ratio >= 4.5,
                    "{preset:?} {state} (#{bg:06X} on #{fg:06X}) contrast {ratio:.2} < 4.5"
                );
            }
        }
    }

    #[test]
    fn accent_text_meets_wcag_aa_in_both_modes() {
        for preset in AccentColor::ALL {
            for (mode, surfaces) in [
                (ThemeMode::Light, &[0xFFFFFF_u32, 0xFAFAFA, 0xF4F4F5][..]),
                (ThemeMode::Dark, &[0x09090B_u32, 0x18181B, 0x27272A][..]),
            ] {
                let text = accent_text_for_mode(preset.hex(), mode);
                for &surface in surfaces {
                    let ratio = contrast_ratio(text, surface);
                    assert!(
                        ratio >= 4.5,
                        "{preset:?} {mode:?} (#{text:06X} on #{surface:06X}) contrast {ratio:.2} < 4.5"
                    );
                }
            }
        }
    }
}
