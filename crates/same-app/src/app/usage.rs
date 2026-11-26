//! Usage dashboard: the plan limits of every enabled provider in one place,
//! with bars, the share used and left, and the time until each limit resets.
//!
//! The numbers come from the provider's own CLI (see `same_agent::quota`);
//! anything a provider cannot report is shown as unavailable, never guessed.

use super::{settings_card, settings_header, MainView, SameAppView, UiAsyncMsg};
use crate::i18n::t;
use crate::theme::{status_amber, status_red};
use chrono::{DateTime, Local, Utc};
use gpui::{
    canvas, div, point, prelude::FluentBuilder as _, px, relative, AnyElement, Context, Div,
    FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement, PathBuilder, Pixels,
    Point, SharedString, StatefulInteractiveElement as _, Styled,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, Theme};
use same_core::{
    Language, ProviderConfig, ProviderKind, QuotaReport, QuotaSnapshot, QuotaWindow,
    QuotaWindowKind,
};
use std::time::Duration;

/// While the dashboard is open, limits older than this are fetched again.
const QUOTA_REFRESH: Duration = Duration::from_secs(5 * 60);
/// Otherwise (for the title bar rings), limits older than this.
const QUOTA_REFRESH_BACKGROUND: Duration = Duration::from_secs(10 * 60);
/// Opening the dashboard, or a finished turn, refetches limits older than this.
const QUOTA_REFRESH_SOON: Duration = Duration::from_secs(60);
/// How often limits are checked for age and countdowns re-rendered.
pub(super) const QUOTA_TICK: Duration = Duration::from_secs(60);

/// Share used from which a limit is shown as nearly used up.
const WARN_PERCENT: f64 = 80.0;
const DANGER_PERCENT: f64 = 95.0;

/// The last quota fetch of one provider.
pub(super) struct QuotaEntry {
    /// Where SAME asked for it to run (the exec label of the project then),
    /// so a project in another environment (another login) refetches.
    pub context: String,
    /// Where it actually ran (e.g. "WSL: Ubuntu").
    pub target: String,
    /// The last answer the provider gave; kept when a later fetch fails.
    pub report: Option<QuotaReport>,
    /// Why the latest fetch failed, if it did.
    pub error: Option<String>,
    pub attempted_at: DateTime<Utc>,
}

/// Providers whose CLI can be asked for its limits.
fn reports_quota(provider: &ProviderConfig) -> bool {
    matches!(
        provider.kind,
        ProviderKind::ClaudeCli | ProviderKind::CodexCli
    )
}

impl SameAppView {
    pub(super) fn open_usage(&mut self) {
        self.active_view = MainView::Usage;
        self.refresh_quotas(QUOTA_REFRESH_SOON, None);
        self.refresh_stats(QUOTA_REFRESH);
    }

    /// Periodic refresh: often while the dashboard is open, rarely otherwise.
    pub(super) fn tick_quotas(&mut self) {
        if self.active_view == MainView::Usage {
            self.refresh_stats(QUOTA_REFRESH);
        }
        let max_age = if self.active_view == MainView::Usage {
            QUOTA_REFRESH
        } else {
            QUOTA_REFRESH_BACKGROUND
        };
        self.refresh_quotas(max_age, None);
    }

    /// A turn of `session_id` ended: its provider's usage just changed.
    pub(super) fn refresh_quota_after_turn(&mut self, session_id: &str) {
        let Some(provider_id) = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| s.provider_id.clone())
        else {
            return;
        };
        self.refresh_quotas(QUOTA_REFRESH_SOON, Some(&provider_id));
    }

    /// Fetches the limits of every enabled provider (or only `only`) whose
    /// last fetch is older than `max_age` (or was for another environment).
    pub(super) fn refresh_quotas(&mut self, max_age: Duration, only: Option<&str>) {
        let context = self.exec_label();
        let now = Utc::now();
        let stale: Vec<ProviderConfig> = self
            .config
            .providers
            .iter()
            .filter(|p| p.enabled && reports_quota(p))
            .filter(|p| only.is_none_or(|id| p.id == id))
            .filter(|p| !self.quotas_fetching.contains(&p.id))
            .filter(|p| match self.quotas.get(&p.id) {
                None => true,
                Some(e) => {
                    e.context != context
                        || (now - e.attempted_at).to_std().unwrap_or_default() >= max_age
                }
            })
            .cloned()
            .collect();
        let pref = self.exec_pref();
        let cwd = self.exec_cwd();
        for provider in stale {
            self.quotas_fetching.insert(provider.id.clone());
            let manager = self.agent_manager.clone();
            let tx = self.ui_tx.clone();
            let cwd = cwd.clone();
            let context = context.clone();
            self.rt.spawn(async move {
                let (target, res) = manager.fetch_quota(&provider, pref, &cwd).await;
                let _ = tx.send(UiAsyncMsg::QuotaFetched {
                    provider_id: provider.id,
                    context,
                    target,
                    result: res.map_err(|e| format!("{e:#}")),
                });
            });
        }
    }

    pub(super) fn on_quota_fetched(
        &mut self,
        provider_id: String,
        context: String,
        target: String,
        result: Result<QuotaReport, String>,
    ) {
        self.quotas_fetching.remove(&provider_id);
        if context != self.exec_label() {
            // The user switched execution context while this fetch was in
            // flight; discard the stale result and fetch for the current one.
            self.refresh_quotas(Duration::ZERO, Some(&provider_id));
            return;
        }
        let previous = self
            .quotas
            .remove(&provider_id)
            // Another environment's numbers are not this one's.
            .filter(|e| e.context == context);
        let (report, error) = match result {
            Ok(report) => (Some(report), None),
            Err(err) => {
                tracing::warn!("Quota fetch for {provider_id} failed: {err}");
                (previous.and_then(|e| e.report), Some(err))
            }
        };
        self.quotas.insert(
            provider_id,
            QuotaEntry {
                context,
                target,
                report,
                error,
                attempted_at: Utc::now(),
            },
        );
    }

    pub(super) fn render_usage_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let fetching = !self.quotas_fetching.is_empty();

        let refresh = Button::new("usage-refresh")
            .small()
            .outline()
            .icon(Icon::new(IconName::RefreshCw).small())
            .label(t(lang, "更新", "Refresh"))
            .loading(fetching)
            .on_click(cx.listener(|this, _, _, cx| {
                this.refresh_quotas(Duration::ZERO, None);
                this.refresh_stats(Duration::ZERO);
                cx.notify();
            }))
            .into_any_element();

        let providers: Vec<&ProviderConfig> =
            self.config.providers.iter().filter(|p| p.enabled).collect();

        let body = div()
            .v_flex()
            .gap_4()
            .child(settings_header(
                t(lang, "使用量", "Usage"),
                t(
                    lang,
                    "各プロバイダのプランの利用制限（プロバイダが報告した値のみ）",
                    "Plan limits of each provider (only what the provider reports)",
                ),
                &theme,
                Some(refresh),
            ))
            .when(providers.is_empty(), |el| {
                el.child(
                    settings_card(&theme).child(
                        div()
                            .px_4()
                            .py_4()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t(
                                lang,
                                "有効なプロバイダがありません",
                                "No providers are enabled",
                            )),
                    ),
                )
            })
            .children(
                providers
                    .into_iter()
                    .map(|p| self.render_quota_card(p, lang, &theme)),
            )
            .child(div().pt_4().child(self.render_token_stats(cx)));

        div()
            .id("usage-page-scroll")
            .flex_1()
            .h_full()
            .min_w_0()
            .bg(theme.background)
            .overflow_y_scroll()
            .child(div().p_6().max_w(px(880.0)).child(body))
    }

    fn render_quota_card(&self, provider: &ProviderConfig, lang: Language, theme: &Theme) -> Div {
        let context = self.exec_label();
        let entry = self
            .quotas
            .get(&provider.id)
            .filter(|e| e.context == context);
        let fetching = self.quotas_fetching.contains(&provider.id);
        let snapshot = match entry.and_then(|e| e.report.as_ref()) {
            Some(QuotaReport::Available(s)) => Some(s),
            _ => None,
        };

        let mut subtitle = vec![provider.kind.label().to_string()];
        if let Some(e) = entry {
            subtitle.push(e.target.clone());
        }
        let plan = snapshot.and_then(|s| s.plan.as_deref()).map(plan_label);

        let status: String = if fetching {
            t(lang, "取得中…", "Fetching…").to_string()
        } else if let Some(s) = snapshot {
            match lang {
                Language::Ja => format!("最終取得 {}", local_time(s.fetched_at)),
                Language::En => format!("Updated {}", local_time(s.fetched_at)),
            }
        } else {
            String::new()
        };

        let header = div()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_4()
            .py_3()
            .child(
                div()
                    .v_flex()
                    .gap_0p5()
                    .min_w_0()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(provider.name.clone()),
                            )
                            .when_some(plan, |el, plan| {
                                el.child(
                                    div()
                                        .px_1p5()
                                        .rounded(px(4.0))
                                        .border_1()
                                        .border_color(theme.border)
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(plan),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(subtitle.join(" · ")),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(status),
            );

        let muted_line = |text: String| {
            div()
                .px_4()
                .py_3()
                .border_t_1()
                .border_color(theme.border)
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(text)
        };

        let mut card = settings_card(theme).child(header);
        if !reports_quota(provider) {
            card = card.child(muted_line(
                t(
                    lang,
                    "このプロバイダは利用制限を取得できません（ACP には取得方法がありません）",
                    "This provider cannot report its limits (ACP has no way to ask)",
                )
                .to_string(),
            ));
        } else {
            match entry.and_then(|e| e.report.as_ref()) {
                // Not asked yet, or the first fetch is still running.
                None => {}
                Some(QuotaReport::Unavailable(why)) => {
                    card = card.child(muted_line(match lang {
                        Language::Ja => format!("取得できません: {why}"),
                        Language::En => format!("Unavailable: {why}"),
                    }));
                }
                Some(QuotaReport::Available(snap)) => {
                    card = card.child(self.render_quota_windows(snap, lang, theme));
                }
            }
            if let Some(err) = entry.and_then(|e| e.error.as_ref()) {
                card = card.child(
                    div()
                        .px_4()
                        .py_2()
                        .border_t_1()
                        .border_color(theme.border)
                        .text_xs()
                        .text_color(status_red())
                        .child(match lang {
                            Language::Ja => format!("更新に失敗しました: {err}"),
                            Language::En => format!("Refresh failed: {err}"),
                        }),
                );
            }
        }
        card
    }

    fn render_quota_windows(&self, snap: &QuotaSnapshot, lang: Language, theme: &Theme) -> Div {
        let list = div().v_flex().border_t_1().border_color(theme.border);
        if snap.windows.is_empty() {
            return list.child(
                div()
                    .px_4()
                    .py_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t(
                        lang,
                        "利用制限の情報はありません",
                        "The provider reported no limits",
                    )),
            );
        }
        let now = Utc::now();
        list.children(
            snap.windows
                .iter()
                .enumerate()
                .map(|(i, w)| quota_window_row(i, w, now, lang, theme)),
        )
    }
}

/// One line of a ring's hover card.
struct RingLine {
    label: String,
    detail: String,
    color: Hsla,
}

impl SameAppView {
    /// Title bar: a ring per provider that reports its limits, filled to its
    /// most-used limit. Hover for the details, click for the dashboard.
    pub(super) fn render_quota_rings(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let now = Utc::now();
        let context = self.exec_label();
        let providers: Vec<&ProviderConfig> = self
            .config
            .providers
            .iter()
            .filter(|p| p.enabled && reports_quota(p))
            .collect();
        let on_usage = self.active_view == MainView::Usage;
        if providers.is_empty() {
            if self.stats_providers().is_empty() {
                return None;
            }
            return Some(
                div()
                    .h_flex()
                    .items_center()
                    .child(
                        Button::new("tb-usage-open")
                            .xsmall()
                            .map(|b| if on_usage { b.primary() } else { b.ghost() })
                            .label(t(lang, "使用量", "Usage"))
                            .tooltip(if on_usage {
                                t(lang, "クリックでワークスペースに戻る", "Click to go back")
                            } else {
                                t(lang, "クリックで使用量を表示", "Click for usage details")
                            })
                            .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.active_view == MainView::Usage {
                                    this.active_view = MainView::Chat;
                                } else {
                                    this.open_usage();
                                }
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }

        let rings = providers.into_iter().map(|provider| {
            let entry = self
                .quotas
                .get(&provider.id)
                .filter(|e| e.context == context);
            let fetching = self.quotas_fetching.contains(&provider.id);
            let report = entry.and_then(|e| e.report.as_ref());
            let snapshot = match report {
                Some(QuotaReport::Available(s)) => Some(s),
                _ => None,
            };
            // The limit closest to running out decides the ring.
            let worst = snapshot.and_then(|s| {
                s.windows
                    .iter()
                    .filter(|w| !w.resets_at.is_some_and(|r| r <= now))
                    .map(|w| w.used_percent.clamp(0.0, 100.0))
                    .fold(None, |acc: Option<f64>, u| {
                        Some(acc.map_or(u, |a| a.max(u)))
                    })
            });
            let color: Hsla = match worst {
                Some(u) => usage_color(u, false, &theme),
                None => theme.muted_foreground,
            };

            let mut title = provider.name.clone();
            if let Some(plan) = snapshot.and_then(|s| s.plan.as_deref()) {
                title = format!("{title} · {}", plan_label(plan));
            }
            let mut lines: Vec<RingLine> = Vec::new();
            match report {
                Some(QuotaReport::Available(snap)) => {
                    for w in &snap.windows {
                        let used = w.used_percent.clamp(0.0, 100.0);
                        let expired = w.resets_at.is_some_and(|r| r <= now);
                        let mut detail = if expired {
                            t(lang, "リセット済み", "Reset since").to_string()
                        } else {
                            match lang {
                                Language::Ja => format!("{}% 使用", fmt_pct(used)),
                                Language::En => format!("{}% used", fmt_pct(used)),
                            }
                        };
                        if let Some(r) = w.resets_at.filter(|_| !expired) {
                            let left = fmt_duration((r - now).num_seconds(), lang);
                            detail.push_str(&match lang {
                                Language::Ja => format!(" · リセットまで {left}"),
                                Language::En => format!(" · resets in {left}"),
                            });
                        }
                        lines.push(RingLine {
                            label: window_label(&w.kind, lang),
                            detail,
                            color: usage_color(used, expired, &theme),
                        });
                    }
                    if snap.windows.is_empty() {
                        lines.push(RingLine {
                            label: t(lang, "利用制限の情報はありません", "No limits reported")
                                .to_string(),
                            detail: String::new(),
                            color: theme.muted_foreground,
                        });
                    }
                }
                Some(QuotaReport::Unavailable(why)) => lines.push(RingLine {
                    label: t(lang, "取得できません", "Unavailable").to_string(),
                    detail: why.clone(),
                    color: theme.muted_foreground,
                }),
                None => lines.push(RingLine {
                    label: if fetching {
                        t(lang, "取得中…", "Fetching…").to_string()
                    } else {
                        t(lang, "未取得", "Not fetched yet").to_string()
                    },
                    detail: String::new(),
                    color: theme.muted_foreground,
                }),
            }
            let footer = match (entry.and_then(|e| e.error.as_ref()), snapshot) {
                (Some(err), _) => match lang {
                    Language::Ja => format!("更新に失敗しました: {err}"),
                    Language::En => format!("Refresh failed: {err}"),
                },
                (None, Some(s)) => match lang {
                    Language::Ja => format!("最終取得 {}", local_time(s.fetched_at)),
                    Language::En => format!("Updated {}", local_time(s.fetched_at)),
                },
                _ => String::new(),
            };
            let hint = if on_usage {
                t(lang, "クリックでワークスペースに戻る", "Click to go back")
            } else {
                t(lang, "クリックで使用量を表示", "Click for usage details")
            };

            let center = match worst {
                Some(u) => fmt_pct_short(u),
                None if fetching && report.is_none() => "…".to_string(),
                None => "–".to_string(),
            };
            let fraction = worst.map_or(0.0, |u| (u / 100.0) as f32);
            let track = theme.border;
            let muted = theme.muted_foreground;
            let fg = theme.foreground;

            div()
                .id(SharedString::from(format!("tb-quota-{}", provider.id)))
                .relative()
                .size(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(|s| s.bg(theme.list_hover))
                // Keep the press for the ring, not a window drag.
                .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            paint_ring(bounds.center(), bounds.size.width, 1.0, track, window);
                            if fraction > 0.0 {
                                paint_ring(
                                    bounds.center(),
                                    bounds.size.width,
                                    fraction,
                                    color,
                                    window,
                                );
                            }
                        },
                    )
                    .absolute()
                    .top(px(3.0))
                    .left(px(3.0))
                    .size(px(22.0)),
                )
                .child(
                    div()
                        .text_size(px(8.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(if worst.is_some() { fg } else { muted })
                        .child(center),
                )
                .tooltip(move |window, cx| {
                    let title = title.clone();
                    let lines: Vec<(String, String, Hsla)> = lines
                        .iter()
                        .map(|l| (l.label.clone(), l.detail.clone(), l.color))
                        .collect();
                    let footer = footer.clone();
                    Tooltip::element(move |_, _| {
                        div()
                            .v_flex()
                            .gap_1()
                            .min_w(px(220.0))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(fg)
                                    .child(title.clone()),
                            )
                            .children(lines.iter().map(|(label, detail, color)| {
                                div()
                                    .h_flex()
                                    .items_center()
                                    .gap_2()
                                    .text_xs()
                                    .child(div().size(px(6.0)).rounded_full().bg(*color))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(fg)
                                            .child(label.clone()),
                                    )
                                    .when(!detail.is_empty(), |el| {
                                        el.child(div().text_color(muted).child(detail.clone()))
                                    })
                            }))
                            .when(!footer.is_empty(), |el| {
                                el.child(
                                    div()
                                        .pt_1()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(footer.clone()),
                                )
                            })
                            .child(div().text_xs().text_color(muted).child(hint))
                    })
                    .build(window, cx)
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.active_view == MainView::Usage {
                        this.active_view = MainView::Chat;
                    } else {
                        this.open_usage();
                    }
                    cx.notify();
                }))
                .into_any_element()
        });

        Some(
            div()
                .h_flex()
                .items_center()
                .gap_0p5()
                .children(rings)
                .into_any_element(),
        )
    }
}

/// Strokes the first `fraction` of a circle filling a `size`-wide square
/// around `center`, clockwise from the top.
fn paint_ring(
    center: Point<Pixels>,
    size: Pixels,
    fraction: f32,
    color: Hsla,
    window: &mut gpui::Window,
) {
    const STROKE: f32 = 2.5;
    const SEGMENTS: usize = 64;
    let radius = f32::from(size) / 2.0 - STROKE / 2.0;
    let steps = ((SEGMENTS as f32 * fraction).ceil() as usize).max(1);
    let at = |i: usize| {
        let angle = -std::f32::consts::FRAC_PI_2
            + std::f32::consts::TAU * fraction * (i as f32 / steps as f32);
        point(
            center.x + px(radius * angle.cos()),
            center.y + px(radius * angle.sin()),
        )
    };
    let mut path = PathBuilder::stroke(px(STROKE));
    path.move_to(at(0));
    for i in 1..=steps {
        path.line_to(at(i));
    }
    if fraction >= 1.0 {
        path.close();
    }
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

/// The colour for a limit `used` percent used up.
fn usage_color(used: f64, expired: bool, theme: &Theme) -> Hsla {
    if expired {
        theme.muted_foreground
    } else if used >= DANGER_PERCENT {
        status_red().into()
    } else if used >= WARN_PERCENT {
        status_amber().into()
    } else {
        theme.ring
    }
}

/// The percent inside a ring: whole numbers only, it is 22 px wide.
fn fmt_pct_short(value: f64) -> String {
    format!("{}", value.round() as i64)
}

fn quota_window_row(
    index: usize,
    w: &QuotaWindow,
    now: DateTime<Utc>,
    lang: Language,
    theme: &Theme,
) -> AnyElement {
    let used = w.used_percent.clamp(0.0, 100.0);
    // Past its reset, the old number no longer says anything.
    let expired = w.resets_at.is_some_and(|r| r <= now);
    let color = usage_color(used, expired, theme);

    let figures = if expired {
        t(
            lang,
            "リセット済み（更新すると最新の値を取得します）",
            "Reset since (refresh for the current value)",
        )
        .to_string()
    } else {
        match lang {
            Language::Ja => format!("{}% 使用 · 残り {}%", fmt_pct(used), fmt_pct(100.0 - used)),
            Language::En => format!("{}% used · {}% left", fmt_pct(used), fmt_pct(100.0 - used)),
        }
    };
    let reset = w.resets_at.filter(|_| !expired).map(|r| {
        let left = fmt_duration((r - now).num_seconds(), lang);
        match lang {
            Language::Ja => format!("リセットまで {left}（{}）", local_time(r)),
            Language::En => format!("Resets in {left} ({})", local_time(r)),
        }
    });

    div()
        .v_flex()
        .gap_1p5()
        .px_4()
        .py_3()
        .when(index > 0, |el| el.border_t_1().border_color(theme.border))
        .child(
            div()
                .h_flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_1p5()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .when(!expired && used >= WARN_PERCENT, |el| {
                            el.child(
                                Icon::new(IconName::TriangleAlert)
                                    .xsmall()
                                    .text_color(color),
                            )
                        })
                        .child(window_label(&w.kind, lang)),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(if expired {
                            theme.muted_foreground
                        } else {
                            theme.foreground
                        })
                        .child(figures),
                ),
        )
        .child(
            div()
                .w_full()
                .h(px(8.0))
                .rounded(px(4.0))
                .bg(theme.border)
                .child(
                    div()
                        .h_full()
                        .rounded(px(4.0))
                        .bg(color)
                        .w(relative(if expired { 0.0 } else { (used / 100.0) as f32 })),
                ),
        )
        .when_some(reset, |el, reset| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(reset),
            )
        })
        .into_any_element()
}

/// "5時間" / "5-hour", "週間" / "Weekly", plus the model for a per-model limit.
fn window_label(kind: &QuotaWindowKind, lang: Language) -> String {
    let span = |minutes: u32| -> String {
        match (minutes, lang) {
            (0, _) => t(lang, "利用制限", "Limit").to_string(),
            (10080, _) => t(lang, "週間", "Weekly").to_string(),
            (m, Language::Ja) if m % 1440 == 0 => format!("{}日間", m / 1440),
            (m, Language::En) if m % 1440 == 0 => format!("{}-day", m / 1440),
            (m, Language::Ja) if m % 60 == 0 => format!("{}時間", m / 60),
            (m, Language::En) if m % 60 == 0 => format!("{}-hour", m / 60),
            (m, Language::Ja) => format!("{m}分間"),
            (m, Language::En) => format!("{m}-minute"),
        }
    };
    match kind {
        QuotaWindowKind::Rolling { minutes } => span(*minutes),
        QuotaWindowKind::Model { minutes, model } => format!("{} ({model})", span(*minutes)),
    }
}

/// "pro" → "Pro"; plan names are shown as the provider spells them otherwise.
fn plan_label(plan: &str) -> String {
    let mut chars = plan.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn fmt_pct(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.1}")
    }
}

/// A coarse countdown: "2日3時間" / "2d 3h", "3時間12分" / "3h 12m".
fn fmt_duration(secs: i64, lang: Language) -> String {
    let mins = secs.max(0) / 60;
    let (d, h, m) = (mins / 1440, (mins / 60) % 24, mins % 60);
    match lang {
        Language::Ja if d > 0 => format!("{d}日{h}時間"),
        Language::Ja if h > 0 => format!("{h}時間{m}分"),
        Language::Ja if m > 0 => format!("{m}分"),
        Language::Ja => "1分未満".to_string(),
        Language::En if d > 0 => format!("{d}d {h}h"),
        Language::En if h > 0 => format!("{h}h {m}m"),
        Language::En if m > 0 => format!("{m}m"),
        Language::En => "<1m".to_string(),
    }
}

pub(super) fn local_time(at: DateTime<Utc>) -> String {
    let local = at.with_timezone(&Local);
    if local.date_naive() == Local::now().date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%m/%d %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_labels() {
        let rolling = |minutes| QuotaWindowKind::Rolling { minutes };
        assert_eq!(window_label(&rolling(300), Language::Ja), "5時間");
        assert_eq!(window_label(&rolling(300), Language::En), "5-hour");
        assert_eq!(window_label(&rolling(10080), Language::Ja), "週間");
        assert_eq!(window_label(&rolling(1440), Language::En), "1-day");
        assert_eq!(window_label(&rolling(90), Language::En), "90-minute");
        assert_eq!(window_label(&rolling(0), Language::En), "Limit");
        let opus = QuotaWindowKind::Model {
            minutes: 10080,
            model: "Opus".into(),
        };
        assert_eq!(window_label(&opus, Language::En), "Weekly (Opus)");
    }

    #[test]
    fn countdowns_and_percent() {
        assert_eq!(
            fmt_duration(2 * 86400 + 3 * 3600 + 59, Language::Ja),
            "2日3時間"
        );
        assert_eq!(fmt_duration(3 * 3600 + 12 * 60, Language::En), "3h 12m");
        assert_eq!(fmt_duration(30, Language::En), "<1m");
        assert_eq!(fmt_duration(-5, Language::Ja), "1分未満");
        assert_eq!(fmt_pct(54.0), "54");
        assert_eq!(fmt_pct(54.25), "54.2");
        assert_eq!(plan_label("pro"), "Pro");
    }
}
