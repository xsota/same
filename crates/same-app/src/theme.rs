use gpui::{rgb, App, Rgba, Window};
use gpui_component::theme::{Theme, ThemeMode as GpuiThemeMode};
use same_core::ThemeMode;

pub fn shark_teal() -> Rgba {
    rgb(0x2BA6CB)
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

pub fn apply_gpui_theme(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    let gpui_mode = match mode {
        ThemeMode::Dark => GpuiThemeMode::Dark,
        ThemeMode::Light => GpuiThemeMode::Light,
    };
    Theme::change(gpui_mode, window, cx);
    #[cfg(target_os = "linux")]
    {
        let theme = Theme::global_mut(cx);
        theme.font_family = "DejaVu Sans".into();
        theme.mono_font_family = "DejaVu Sans Mono".into();
    }
    #[cfg(target_os = "windows")]
    {
        let theme = Theme::global_mut(cx);
        theme.font_family = "Yu Gothic UI".into();
        theme.mono_font_family = "Consolas".into();
    }
}
