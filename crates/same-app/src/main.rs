#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod i18n;
mod theme;
mod workspace_fs;

use app::SameAppView;
use gpui::{px, size, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
use gpui_component::Root;
use gpui_kit_assets::Assets;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let initial_dir = std::env::args().nth(1).map(PathBuf::from);

    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            app::bind_keys(cx);

            let bounds = Bounds::centered(None, size(px(1360.0), px(860.0)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("SAME — Shared Agentic Multi-provider Environment".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };

            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| {
                    SameAppView::new(initial_dir, window, cx).expect("Failed to initialize SAME")
                });
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("Failed to open SAME window");
            // Closing the last window quits the app (and stops its agents).
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}
