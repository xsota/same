//! Token statistics on the usage screen: how many tokens each provider used
//! over the last days (today, 7 or 30), per day and per model, read from the
//! history each CLI keeps on this machine (see `same_agent::stats`).

use super::usage::local_time;
use super::{settings_card, settings_header, SameAppView, UiAsyncMsg};
use crate::i18n::t;
use crate::theme::status_red;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use gpui::{
    div, prelude::FluentBuilder as _, px, relative, rgb, AnyElement, Context, Div, FontWeight,
    Hsla, InteractiveElement as _, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, Sizable as _, StyledExt as _, Theme};
use same_agent::stats::{stats_source, StatsSource};
use same_core::{Language, ProviderConfig, StatsReport, ThemeMode, TokenCounts, TokenStats};
use std::time::Duration;

/// The periods offered, in days (1 = today).
const PERIODS: [u32; 3] = [1, 7, 30];
pub(super) const DEFAULT_STATS_DAYS: u32 = 7;
const CHART_HEIGHT: f32 = 160.0;
/// Models listed by name; the rest fold into one "Other" row.
const TOP_MODELS: usize = 8;

/// The last read of one provider's token history.
pub(super) struct StatsEntry {
    pub days: u32,
    pub context: String,
    pub target: String,
    pub report: Option<StatsReport>,
    pub error: Option<String>,
    pub attempted_at: DateTime<Utc>,
}

/// A fixed colour per provider family (never by rank), validated for colour
/// vision deficiency on both themes.
fn series_color(source: StatsSource, mode: ThemeMode) -> Hsla {
    let hex = match (source, mode) {
        (StatsSource::Claude, ThemeMode::Dark) => 0x338ced,
        (StatsSource::Codex, ThemeMode::Dark) => 0xe76624,
        (StatsSource::OpenCode, ThemeMode::Dark) => 0x00a888,
        (StatsSource::Claude, ThemeMode::Light) => 0x257de0,
        (StatsSource::Codex, ThemeMode::Light) => 0xf47230,
        (StatsSource::OpenCode, ThemeMode::Light) => 0x00b894,
    };
    rgb(hex).into()
}

/// "999", "12.3K", "4.56M", "1.23B".
pub(super) fn fmt_tokens(n: u64) -> String {
    let n = n as f64;
    let (value, unit) = if n >= 1e9 {
        (n / 1e9, "B")
    } else if n >= 1e6 {
        (n / 1e6, "M")
    } else if n >= 1e3 {
        (n / 1e3, "K")
    } else {
        return format!("{n}");
    };
    if value >= 100.0 {
        format!("{value:.0}{unit}")
    } else if value >= 10.0 {
        format!("{value:.1}{unit}")
    } else {
        format!("{value:.2}{unit}")
    }
}

fn period_label(days: u32, lang: Language) -> String {
    match (days, lang) {
        (1, _) => t(lang, "今日", "Today").to_string(),
        (d, Language::Ja) => format!("{d}日間"),
        (d, Language::En) => format!("{d} days"),
    }
}

/// One provider's series in the charts.
struct Series<'a> {
    name: String,
    color: Hsla,
    stats: &'a TokenStats,
}

impl SameAppView {
    /// The counted figure: everything, or without prompt-cache tokens.
    fn stats_metric(&self, c: &TokenCounts) -> u64 {
        if self.stats_include_cache {
            c.total()
        } else {
            c.input + c.output
        }
    }

    pub(super) fn stats_providers(&self) -> Vec<(&ProviderConfig, StatsSource)> {
        let mut seen = std::collections::HashSet::new();
        self.config
            .providers
            .iter()
            .filter(|p| p.enabled)
            .filter_map(|p| {
                let source = stats_source(p)?;
                let store_key = stats_store_key(p, source);
                seen.insert(store_key).then_some((p, source))
            })
            .collect()
    }

    /// Reads the history of every provider whose last read is older than
    /// `max_age`, or was for another period or environment.
    pub(super) fn refresh_stats(&mut self, max_age: Duration) {
        let context = self.exec_label();
        let days = self.stats_days;
        let now = Utc::now();
        let stale: Vec<ProviderConfig> = self
            .stats_providers()
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| !self.stats_fetching.contains(&p.id))
            .filter(|p| match self.stats.get(&p.id) {
                None => true,
                Some(e) => {
                    e.days != days
                        || e.context != context
                        || (now - e.attempted_at).to_std().unwrap_or_default() >= max_age
                }
            })
            .cloned()
            .collect();
        let pref = self.exec_pref();
        let cwd = self.exec_cwd();
        for provider in stale {
            self.stats_fetching.insert(provider.id.clone());
            let manager = self.agent_manager.clone();
            let tx = self.ui_tx.clone();
            let cwd = cwd.clone();
            let context = context.clone();
            self.rt.spawn(async move {
                let (target, res) = manager.fetch_stats(&provider, pref, &cwd, days).await;
                let _ = tx.send(UiAsyncMsg::StatsFetched {
                    provider_id: provider.id,
                    days,
                    context,
                    target,
                    result: res.map_err(|e| format!("{e:#}")),
                });
            });
        }
    }

    pub(super) fn on_stats_fetched(
        &mut self,
        provider_id: String,
        days: u32,
        context: String,
        target: String,
        result: Result<StatsReport, String>,
    ) {
        self.stats_fetching.remove(&provider_id);
        if context != self.exec_label() {
            self.refresh_stats(Duration::ZERO);
            return;
        }
        let previous = self
            .stats
            .remove(&provider_id)
            .filter(|e| e.days == days && e.context == context);
        let (report, error) = match result {
            Ok(report) => (Some(report), None),
            Err(err) => {
                tracing::warn!("Token stats for {provider_id} failed: {err}");
                (previous.and_then(|e| e.report), Some(err))
            }
        };
        self.stats.insert(
            provider_id,
            StatsEntry {
                days,
                context,
                target,
                report,
                error,
                attempted_at: Utc::now(),
            },
        );
        // A read for the period shown may have finished after another
        // period was picked: read again for the one shown.
        if days != self.stats_days {
            self.refresh_stats(Duration::ZERO);
        }
    }

    /// The token statistics section of the usage screen.
    pub(super) fn render_token_stats(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let mode = self.config.theme;

        let mut controls = div().h_flex().items_center().gap_1();
        for days in PERIODS {
            let on = self.stats_days == days;
            controls = controls.child(
                Button::new(SharedString::from(format!("stats-days-{days}")))
                    .small()
                    .label(period_label(days, lang))
                    .when(on, |b| b.primary())
                    .when(!on, |b| b.ghost())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.stats_days = days;
                        this.refresh_stats(Duration::ZERO);
                        cx.notify();
                    })),
            );
        }
        controls = controls
            .child(div().w(px(1.0)).h(px(16.0)).mx_1().bg(theme.border))
            .child(
                Button::new("stats-cache")
                    .small()
                    .label(t(lang, "キャッシュを含む", "Include cache"))
                    .when(self.stats_include_cache, |b| b.primary())
                    .when(!self.stats_include_cache, |b| b.ghost())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stats_include_cache = !this.stats_include_cache;
                        cx.notify();
                    })),
            );

        let providers = self.stats_providers();
        let section = div().v_flex().gap_4().child(settings_header(
            t(lang, "トークン使用量", "Token usage"),
            t(
                lang,
                "このマシンに残っている各 CLI の履歴から集計（SAME 以外での利用も含む）",
                "From the history each CLI keeps on this machine (use outside SAME included)",
            ),
            &theme,
            Some(controls.into_any_element()),
        ));
        if providers.is_empty() {
            return section.child(
                settings_card(&theme).child(
                    div()
                        .px_4()
                        .py_4()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t(
                            lang,
                            "履歴を読めるプロバイダ（Claude Code / Codex / OpenCode）が有効になっていません",
                            "No provider with a readable history (Claude Code / Codex / OpenCode) is enabled",
                        )),
                ),
            );
        }

        let context = self.exec_label();
        let series: Vec<Series> = providers
            .iter()
            .filter_map(|(p, source)| {
                let e = self
                    .stats
                    .get(&p.id)
                    .filter(|e| e.days == self.stats_days && e.context == context)?;
                match &e.report {
                    Some(StatsReport::Available(stats)) => Some(Series {
                        name: p.name.clone(),
                        color: series_color(*source, mode),
                        stats,
                    }),
                    _ => None,
                }
            })
            .collect();

        let mut section = section.child(self.render_stats_summary(&providers, lang, &theme));
        if !series.is_empty() {
            if self.stats_days > 1 {
                section = section.child(self.render_stats_chart(&series, lang, &theme));
            }
            section = section.child(self.render_stats_models(&series, lang, &theme));
        }
        section
    }

    /// Per provider: the total, its split, and how the read went.
    fn render_stats_summary(
        &self,
        providers: &[(&ProviderConfig, StatsSource)],
        lang: Language,
        theme: &Theme,
    ) -> Div {
        let mode = self.config.theme;
        let context = self.exec_label();
        let mut card = settings_card(theme);
        for (i, (p, source)) in providers.iter().enumerate() {
            let entry = self
                .stats
                .get(&p.id)
                .filter(|e| e.days == self.stats_days && e.context == context);
            let fetching = self.stats_fetching.contains(&p.id);
            let mut subtitle = vec![];
            if let Some(e) = entry {
                subtitle.push(e.target.clone());
            }
            let (value, detail): (String, Option<String>) =
                match entry.and_then(|e| e.report.as_ref()) {
                    Some(StatsReport::Available(s)) => {
                        let c = &s.total;
                        let mut parts = match lang {
                            Language::Ja => vec![
                                format!("入力 {}", fmt_tokens(c.input)),
                                format!("出力 {}", fmt_tokens(c.output)),
                                format!("キャッシュ読込 {}", fmt_tokens(c.cache_read)),
                                format!("キャッシュ書込 {}", fmt_tokens(c.cache_write)),
                            ],
                            Language::En => vec![
                                format!("Input {}", fmt_tokens(c.input)),
                                format!("Output {}", fmt_tokens(c.output)),
                                format!("Cache read {}", fmt_tokens(c.cache_read)),
                                format!("Cache write {}", fmt_tokens(c.cache_write)),
                            ],
                        };
                        if let Some(cost) = s.cost_usd {
                            parts.push(match lang {
                                Language::Ja => format!("費用 ${cost:.2}（OpenCode の計算）"),
                                Language::En => format!("Cost ${cost:.2} (as OpenCode counts it)"),
                            });
                        }
                        subtitle.push(match lang {
                            Language::Ja => format!("集計 {}", local_time(s.fetched_at)),
                            Language::En => format!("Read {}", local_time(s.fetched_at)),
                        });
                        (fmt_tokens(self.stats_metric(c)), Some(parts.join(" · ")))
                    }
                    Some(StatsReport::Unavailable(why)) => (
                        "–".to_string(),
                        Some(match lang {
                            Language::Ja => format!("取得できません: {why}"),
                            Language::En => format!("Unavailable: {why}"),
                        }),
                    ),
                    None if fetching => (
                        "…".to_string(),
                        Some(t(lang, "集計中…", "Reading…").to_string()),
                    ),
                    None => ("–".to_string(), None),
                };
            let error = entry.and_then(|e| e.error.clone());

            card = card.child(
                div()
                    .v_flex()
                    .gap_1()
                    .px_4()
                    .py_3()
                    .when(i > 0, |el| el.border_t_1().border_color(theme.border))
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
                                    .gap_2()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .size(px(10.0))
                                            .rounded(px(3.0))
                                            .bg(series_color(*source, mode)),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(p.name.clone()),
                                    )
                                    .when(!subtitle.is_empty(), |el| {
                                        el.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(subtitle.join(" · ")),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(value),
                            ),
                    )
                    .when_some(detail, |el, d| {
                        el.child(div().text_xs().text_color(theme.muted_foreground).child(d))
                    })
                    .when_some(error, |el, err| {
                        el.child(div().text_xs().text_color(status_red()).child(match lang {
                            Language::Ja => format!("集計に失敗しました: {err}"),
                            Language::En => format!("Read failed: {err}"),
                        }))
                    }),
            );
        }
        card
    }

    /// Tokens per day, one stacked bar per day, one segment per provider.
    fn render_stats_chart(&self, series: &[Series], lang: Language, theme: &Theme) -> Div {
        let mut day_set = std::collections::BTreeSet::new();
        for s in series {
            for (d, _) in &s.stats.days {
                day_set.insert(*d);
            }
        }
        let days: Vec<NaiveDate> = day_set.into_iter().collect();
        let n = days.len();
        // values[day][series]
        let values: Vec<Vec<u64>> = days
            .iter()
            .map(|&day| {
                series
                    .iter()
                    .map(|s| {
                        s.stats
                            .days
                            .iter()
                            .find(|(d, _)| *d == day)
                            .map_or(0, |(_, c)| self.stats_metric(c))
                    })
                    .collect()
            })
            .collect();
        let max = values
            .iter()
            .map(|v| v.iter().sum::<u64>())
            .max()
            .unwrap_or(0);

        let legend = div()
            .h_flex()
            .items_center()
            .gap_4()
            .children(series.iter().map(|s| {
                div()
                    .h_flex()
                    .items_center()
                    .gap_1p5()
                    .text_xs()
                    .child(div().size(px(10.0)).rounded(px(3.0)).bg(s.color))
                    .child(s.name.clone())
            }));
        let caption = div()
            .h_flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(t(lang, "日別", "Per day")),
            )
            .child(legend);

        if max == 0 {
            return settings_card(theme).child(
                div().v_flex().gap_3().px_4().py_3().child(caption).child(
                    div().text_sm().text_color(theme.muted_foreground).child(t(
                        lang,
                        "この期間の利用はありません",
                        "No usage in this period",
                    )),
                ),
            );
        }

        let gap = if n > 14 { 2.0 } else { 8.0 };
        let label_every = if n > 14 { 5 } else { 1 };
        let muted = theme.muted_foreground;
        let fg = theme.foreground;

        let bars = div()
            .h_flex()
            .items_end()
            .gap(px(gap))
            .h(px(CHART_HEIGHT))
            .border_b_1()
            .border_color(theme.border)
            .children(days.iter().enumerate().map(|(i, day)| {
                let vals = &values[i];
                // Bottom to top in series order; the top segment gets the
                // rounded end.
                let segs: Vec<(f32, Hsla)> = vals
                    .iter()
                    .zip(series)
                    .filter(|(v, _)| **v > 0)
                    .map(|(v, s)| ((CHART_HEIGHT * (*v as f32 / max as f32)).max(2.0), s.color))
                    .collect();
                let top = segs.len().saturating_sub(1);
                let tip_title = day.format("%Y-%m-%d").to_string();
                let tip_rows: Vec<(String, String, Hsla)> = series
                    .iter()
                    .zip(vals)
                    .map(|(s, v)| (s.name.clone(), fmt_tokens(*v), s.color))
                    .collect();
                let tip_total = fmt_tokens(vals.iter().sum());
                div()
                    .id(SharedString::from(format!("stats-day-{i}")))
                    .flex_1()
                    .h_full()
                    .v_flex()
                    .justify_end()
                    .gap(px(2.0))
                    .rounded_t(px(4.0))
                    .hover(|s| s.bg(theme.list_hover))
                    .children(segs.into_iter().enumerate().rev().map(|(j, (h, color))| {
                        div()
                            .w_full()
                            .h(px(h))
                            .bg(color)
                            .when(j == top, |el| el.rounded_t(px(4.0)))
                    }))
                    .tooltip(move |window, cx| {
                        let title = tip_title.clone();
                        let rows = tip_rows.clone();
                        let total = tip_total.clone();
                        Tooltip::element(move |_, _| {
                            div()
                                .v_flex()
                                .gap_1()
                                .min_w(px(160.0))
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(fg)
                                        .child(title.clone()),
                                )
                                .children(rows.iter().map(|(name, value, color)| {
                                    div()
                                        .h_flex()
                                        .items_center()
                                        .justify_between()
                                        .gap_3()
                                        .text_xs()
                                        .child(
                                            div()
                                                .h_flex()
                                                .items_center()
                                                .gap_1p5()
                                                .text_color(muted)
                                                .child(
                                                    div().size(px(8.0)).rounded(px(2.0)).bg(*color),
                                                )
                                                .child(name.clone()),
                                        )
                                        .child(div().text_color(fg).child(value.clone()))
                                }))
                                .child(
                                    div()
                                        .h_flex()
                                        .justify_between()
                                        .text_xs()
                                        .text_color(fg)
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(t(lang, "合計", "Total"))
                                        .child(total.clone()),
                                )
                        })
                        .build(window, cx)
                    })
            }));

        let labels = div()
            .h_flex()
            .gap(px(gap))
            .children(days.iter().enumerate().map(|(i, day)| {
                let show = (n - 1 - i).is_multiple_of(label_every);
                div()
                    .flex_1()
                    .min_w_0()
                    .h_flex()
                    .justify_center()
                    .text_xs()
                    .text_color(muted)
                    .when(show, |el| {
                        el.child(format!("{}/{}", day.month(), day.day()))
                    })
            }));

        settings_card(theme).child(
            div()
                .v_flex()
                .gap_2()
                .px_4()
                .py_3()
                .child(caption)
                .child(div().text_xs().text_color(muted).child(match lang {
                    Language::Ja => format!("最大 {} / 日", fmt_tokens(max)),
                    Language::En => format!("Peak {} / day", fmt_tokens(max)),
                }))
                .child(bars)
                .child(labels),
        )
    }

    /// The models used most in the period, across providers.
    fn render_stats_models(&self, series: &[Series], lang: Language, theme: &Theme) -> Div {
        let mut rows: Vec<(String, &str, Hsla, u64)> = series
            .iter()
            .flat_map(|s| {
                s.stats
                    .models
                    .iter()
                    .map(move |(m, c)| (m.clone(), s.name.as_str(), s.color, self.stats_metric(c)))
            })
            .filter(|r| r.3 > 0)
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.3));
        let rest: u64 = rows.iter().skip(TOP_MODELS).map(|r| r.3).sum();
        let rest_count = rows.len().saturating_sub(TOP_MODELS);
        rows.truncate(TOP_MODELS);
        let max = rows.first().map_or(0, |r| r.3).max(rest).max(1);

        let mut card = settings_card(theme).child(
            div()
                .px_4()
                .pt_3()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(t(lang, "モデル別", "Per model")),
        );
        if rows.is_empty() {
            return card.child(
                div()
                    .px_4()
                    .py_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t(
                        lang,
                        "この期間の利用はありません",
                        "No usage in this period",
                    )),
            );
        }
        let row = |name: String, provider: String, color: Hsla, value: u64| -> AnyElement {
            div()
                .v_flex()
                .gap_1()
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .text_sm()
                        .child(
                            div()
                                .h_flex()
                                .items_center()
                                .gap_2()
                                .min_w_0()
                                .child(div().truncate().child(name))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(provider),
                                ),
                        )
                        .child(div().flex_shrink_0().child(fmt_tokens(value))),
                )
                .child(
                    div().w_full().h(px(6.0)).child(
                        div()
                            .h_full()
                            .rounded_r(px(4.0))
                            .bg(color)
                            .w(relative(value as f32 / max as f32)),
                    ),
                )
                .into_any_element()
        };
        let mut list = div().v_flex().gap_3().px_4().pt_2().pb_3();
        for (name, provider, color, value) in rows {
            list = list.child(row(name, provider.to_string(), color, value));
        }
        if rest_count > 0 {
            list = list.child(row(
                match lang {
                    Language::Ja => format!("その他（{rest_count} モデル）"),
                    Language::En => format!("Other ({rest_count} models)"),
                },
                String::new(),
                theme.muted_foreground,
                rest,
            ));
        }
        card = card.child(list);
        card
    }
}

/// Key identifying the underlying history store of a provider, so two enabled
/// adapters pointing at the same local history (e.g. `codex` CLI and
/// `codex-acp` with the same home) are not double-counted.
fn stats_store_key(p: &ProviderConfig, source: StatsSource) -> (u8, String, String) {
    let (tag, dir_var) = match source {
        StatsSource::Claude => (0u8, "CLAUDE_CONFIG_DIR"),
        StatsSource::Codex => (1u8, "CODEX_HOME"),
        StatsSource::OpenCode => (2u8, "XDG_DATA_HOME"),
    };
    let dir = p
        .env
        .get(dir_var)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let home = if dir.is_empty() {
        p.env
            .get("HOME")
            .or_else(|| p.env.get("USERPROFILE"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    (tag, dir, home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_numbers() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(12_345), "12.3K");
        assert_eq!(fmt_tokens(4_560_000), "4.56M");
        assert_eq!(fmt_tokens(1_295_966_068), "1.30B");
        assert_eq!(fmt_tokens(219_674), "220K");
    }
}
