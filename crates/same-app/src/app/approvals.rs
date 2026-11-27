//! Approval UI: the per-session approval mode picker, permission cards (with
//! diffs and keyboard answers), question cards, plan cards, and OS
//! notifications when the agent waits in the background.

use super::{NoteKind, SameAppView};
use crate::i18n::t;
use crate::theme::{status_amber, status_green, status_red};
use gpui::{
    div, prelude::FluentBuilder as _, px, AnyElement, AppContext, Context, Entity, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::text::TextView;
use gpui_component::{ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _};
use same_core::{
    ApprovalMode, Language, PermissionOption, PermissionRequest, Session, UserInputRequest,
};

/// Separates a proposed plan from the reply in a stored message. An HTML
/// comment, so it stays invisible if the text is shown as markdown.
pub(super) const PLAN_MARKER: &str = "\n\n<!--same:plan-->\n";

/// Splits a stored reply into its text and proposed plan.
pub(super) fn split_plan(body: &str) -> (&str, Option<&str>) {
    match body.split_once(PLAN_MARKER) {
        Some((text, plan)) => (text.trim_end(), Some(plan.trim())),
        None => (body, None),
    }
}

/// A question card waiting for answers.
pub(super) struct PendingInput {
    pub request: UserInputRequest,
    /// Selected option indices, per question.
    pub selected: Vec<Vec<usize>>,
    /// Free-text answer fields, per question (created when first rendered).
    pub other: Vec<Option<Entity<InputState>>>,
}

impl PendingInput {
    pub fn new(request: UserInputRequest) -> Self {
        let n = request.questions.len();
        Self {
            request,
            selected: vec![Vec::new(); n],
            other: (0..n).map(|_| None).collect(),
        }
    }
}

pub(super) fn mode_label(lang: Language, mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Plan => t(lang, "計画", "Plan"),
        ApprovalMode::Ask => t(lang, "確認", "Ask"),
        ApprovalMode::AcceptEdits => t(lang, "編集は自動", "Accept edits"),
        ApprovalMode::FullAccess => t(lang, "フルアクセス", "Full access"),
    }
}

pub(super) fn mode_hint(lang: Language, mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Plan => t(
            lang,
            "読むだけで変更はせず、計画を提案します",
            "Reads only and proposes a plan",
        ),
        ApprovalMode::Ask => t(
            lang,
            "編集やコマンドの前に確認します（各エージェントの既定）",
            "Asks before edits and commands (each agent's default)",
        ),
        ApprovalMode::AcceptEdits => t(
            lang,
            "ワークスペース内の編集は確認なし、それ以外は確認",
            "Edits in the workspace go through; the rest asks",
        ),
        ApprovalMode::FullAccess => t(
            lang,
            "確認なし・サンドボックスなしで実行します",
            "Runs everything without asking or sandbox",
        ),
    }
}

/// What a key press on a permission card does.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum KeyChoice {
    /// Answer with this option id.
    Option(String),
    /// Deny without an option (the card has no reject option).
    Deny,
}

/// `1`–`9` pick that option; `n` / `escape` the first single-use reject (or
/// a plain deny). No letter or Enter allows: the card takes the focus while
/// the user may be about to type, and a stray key must never grant anything.
pub(super) fn permission_key_choice(key: &str, options: &[PermissionOption]) -> Option<KeyChoice> {
    if let Ok(n) = key.parse::<usize>() {
        return options
            .get(n.checked_sub(1)?)
            .map(|o| KeyChoice::Option(o.id.clone()));
    }
    match key {
        "n" | "escape" => Some(
            options
                .iter()
                .find(|o| o.kind == "reject_once")
                .or_else(|| options.iter().find(|o| o.kind.starts_with("reject")))
                .map(|o| KeyChoice::Option(o.id.clone()))
                .unwrap_or(KeyChoice::Deny),
        ),
        _ => None,
    }
}

/// How a diff line is shown.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum DiffLine {
    Header,
    Hunk,
    Added,
    Removed,
    Context,
}

pub(super) fn classify_diff_line(line: &str) -> DiffLine {
    if line.starts_with("+++") || line.starts_with("---") {
        DiffLine::Header
    } else if line.starts_with("@@") {
        DiffLine::Hunk
    } else if line.starts_with('+') {
        DiffLine::Added
    } else if line.starts_with('-') {
        DiffLine::Removed
    } else {
        DiffLine::Context
    }
}

/// Lines of a diff shown in a card; longer diffs are cut.
const MAX_DIFF_LINES: usize = 400;

impl SameAppView {
    pub(super) fn session_mode(&self, session: &Session) -> ApprovalMode {
        session
            .approval_mode
            .unwrap_or(self.config.default_approval_mode)
    }

    /// Sets a session's approval mode; a running agent picks it up at once
    /// (Codex applies sandbox/policy changes on the next turn).
    pub(super) fn set_mode_for(&mut self, sid: &str, mode: ApprovalMode) {
        let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) else {
            return;
        };
        let previous = s.approval_mode.unwrap_or(self.config.default_approval_mode);
        let provider_id = s.provider_id.clone();
        if mode == ApprovalMode::Plan && previous != ApprovalMode::Plan {
            // "Implement" goes back to this, never to something wider.
            self.pre_plan_mode.insert(sid.to_string(), previous);
        }
        s.approval_mode = Some(mode);
        let _ = self.storage.set_session_approval_mode(sid, Some(mode));
        self.agent_manager.set_session_mode(sid, mode);
        let is_codex = self
            .config
            .providers
            .iter()
            .any(|p| p.id == provider_id && p.kind == same_core::ProviderKind::CodexCli);
        if is_codex && previous != mode && self.is_session_running(sid) {
            let lang = self.config.language;
            self.note(
                NoteKind::Info,
                t(
                    lang,
                    "Codex の承認モード変更は次のターンから適用されます",
                    "Codex approval mode change takes effect on the next turn",
                ),
            );
        }
    }

    /// The mode picker in the composer bar.
    pub(super) fn render_mode_menu(
        &self,
        session: &Session,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let lang = self.config.language;
        let current = self.session_mode(session);
        let view = cx.entity().downgrade();
        let sid = session.id.clone();
        Button::new("composer-mode-menu")
            .xsmall()
            .map(|b| match current {
                ApprovalMode::FullAccess => b.warning(),
                ApprovalMode::Plan => b.info(),
                _ => b.ghost(),
            })
            .tooltip(format!(
                "{} — {}",
                t(lang, "承認モード", "Approval mode"),
                mode_hint(lang, current)
            ))
            .label(format!("{} ▾", mode_label(lang, current)))
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for mode in ApprovalMode::ALL {
                    let v = view.clone();
                    let sid = sid.clone();
                    menu = menu.item(
                        PopupMenuItem::new(format!(
                            "{} — {}",
                            mode_label(lang, mode),
                            mode_hint(lang, mode)
                        ))
                        .checked(mode == current)
                        .on_click(move |_, _, cx| {
                            let sid = sid.clone();
                            let _ = v.update(cx, |this, cx| {
                                this.set_mode_for(&sid, mode);
                                cx.notify();
                            });
                        }),
                    );
                }
                menu
            })
    }

    /// Sends an OS notification while the window is in the background.
    pub(super) fn os_notify(&self, title: String, body: String) {
        if !self.config.notifications || self.window_active {
            return;
        }
        std::thread::spawn(move || {
            let _ = notify_rust::Notification::new()
                .appname("SAME")
                .summary(&title)
                .body(&body)
                .show();
        });
    }

    pub(super) fn notify_waiting(&self, session_id: &str, what: &str) {
        let title = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| self.session_title(s))
            .unwrap_or_default();
        self.os_notify(format!("SAME — {what}"), title);
    }

    /// Answers a permission card and puts the focus back on the prompt.
    pub(super) fn answer_permission(
        &mut self,
        request_id: &str,
        option_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_permissions
            .retain(|p| p.request_id != request_id);
        self.mark_feed_tail_dirty();
        let mgr = self.agent_manager.clone();
        let rid = request_id.to_string();
        self.rt.spawn(async move {
            mgr.respond_permission(&rid, option_id).await;
        });
        self.refocus_after_answer(window, cx);
    }

    /// Answers (or with `None`, dismisses) a question card.
    pub(super) fn answer_input(
        &mut self,
        request_id: &str,
        submit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pos) = self
            .pending_inputs
            .iter()
            .position(|p| p.request.request_id == request_id)
        else {
            return;
        };
        let pending = self.pending_inputs.remove(pos);
        self.mark_feed_tail_dirty();
        let answers = submit.then(|| collect_answers(&pending, cx));
        let mgr = self.agent_manager.clone();
        let rid = request_id.to_string();
        self.rt.spawn(async move {
            mgr.respond_user_input(&rid, answers).await;
        });
        self.refocus_after_answer(window, cx);
    }

    fn refocus_after_answer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sid = self.selected_session_id.clone();
        let more = self
            .pending_permissions
            .iter()
            .any(|p| Some(&p.session_id) == sid.as_ref())
            || self
                .pending_inputs
                .iter()
                .any(|p| Some(&p.request.session_id) == sid.as_ref());
        if more {
            self.focus_approval = true;
        } else {
            self.prompt_input.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    /// Creates the free-text fields of new question cards.
    pub(super) fn ensure_input_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.config.language;
        for pending in self.pending_inputs.iter_mut() {
            for (i, q) in pending.request.questions.iter().enumerate() {
                if q.allow_other && pending.other[i].is_none() {
                    let placeholder = if q.options.is_empty() {
                        t(lang, "回答を入力", "Type your answer")
                    } else {
                        t(lang, "その他（自由入力）", "Other (type your own)")
                    };
                    let secret = q.secret;
                    pending.other[i] = Some(cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder(placeholder.to_string())
                            .masked(secret)
                    }));
                }
            }
        }
    }

    /// Keys on the focused card: see [`permission_key_choice`]; on a question
    /// card `1`–`9` toggle the first question's options, `enter` submits and
    /// `escape` dismisses.
    pub(super) fn on_approval_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(sid) = self.selected_session_id.clone() else {
            return;
        };
        let key = ev.keystroke.key.as_str();
        if ev.keystroke.modifiers.control
            || ev.keystroke.modifiers.alt
            || ev.keystroke.modifiers.platform
        {
            return;
        }
        if let Some(req) = self
            .pending_permissions
            .iter()
            .find(|p| p.session_id == sid)
            .cloned()
        {
            if let Some(choice) = permission_key_choice(key, &req.options) {
                cx.stop_propagation();
                let option = match choice {
                    KeyChoice::Option(id) => Some(id),
                    KeyChoice::Deny => None,
                };
                self.answer_permission(&req.request_id, option, window, cx);
            }
            return;
        }
        let Some(pos) = self
            .pending_inputs
            .iter()
            .position(|p| p.request.session_id == sid)
        else {
            return;
        };
        let rid = self.pending_inputs[pos].request.request_id.clone();
        match key {
            "enter" => {
                cx.stop_propagation();
                if input_complete(&self.pending_inputs[pos], cx) {
                    self.answer_input(&rid, true, window, cx);
                }
            }
            "escape" => {
                cx.stop_propagation();
                self.answer_input(&rid, false, window, cx);
            }
            _ => {
                if let Ok(n) = key.parse::<usize>() {
                    // The first question still without an answer.
                    let pending = &mut self.pending_inputs[pos];
                    let qi = pending.selected.iter().position(Vec::is_empty).unwrap_or(0);
                    if n >= 1 && n <= pending.request.questions[qi].options.len() {
                        cx.stop_propagation();
                        toggle_option(pending, qi, n - 1);
                        cx.notify();
                    }
                }
            }
        }
    }

    pub(super) fn render_permission(
        &self,
        req: PermissionRequest,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let has_reject = req.options.iter().any(|o| o.kind.starts_with("reject"));
        // Without a description, show what the tool is about to do.
        let detail = if req.description.trim().is_empty() {
            req.tool_call_id.as_ref().and_then(|tid| {
                self.live_drafts
                    .get(&req.session_id)
                    .and_then(|d| d.tool_calls.iter().find(|tc| &tc.id == tid))
                    .and_then(|tc| tc.input.clone())
            })
        } else {
            Some(req.description.clone())
        };
        let req_id_cancel = req.request_id.clone();
        div()
            .id(SharedString::from(format!("perm-card-{}", req.request_id)))
            .v_flex()
            .p_3()
            .gap_2()
            .rounded(px(8.0))
            .bg(theme.sidebar)
            .border_1()
            .border_color(gpui::Hsla::from(status_amber()))
            .when(focused, |el| {
                el.track_focus(&self.approval_focus)
                    .focus(|s| s.border_2().border_color(theme.ring))
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        this.on_approval_key(ev, window, cx);
                    }))
            })
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(status_amber())
                            .child(format!(
                                "⚠ {}",
                                t(lang, "ツール実行の許可が必要です", "Permission needed")
                            )),
                    )
                    .when(focused, |el| {
                        el.child(div().text_xs().text_color(theme.muted_foreground).child(t(
                            lang,
                            "数字キーで選択 · N / Esc 拒否",
                            "Number keys pick · N / Esc deny",
                        )))
                    }),
            )
            .child(div().text_sm().child(req.title.clone()))
            .when_some(detail, |el, detail| {
                el.child(
                    div()
                        .id(SharedString::from(format!("perm-desc-{}", req.request_id)))
                        .max_h(px(160.0))
                        .overflow_y_scroll()
                        .text_xs()
                        .p_2()
                        .rounded(px(4.0))
                        .bg(theme.background)
                        .font_family(theme.mono_font_family.clone())
                        .child(detail),
                )
            })
            .when_some(req.diff.clone(), |el, diff: String| {
                el.child(self.render_diff(&req.request_id, &diff, cx))
            })
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .flex_wrap()
                    .children(req.options.into_iter().enumerate().map(|(i, opt)| {
                        let req_id = req.request_id.clone();
                        let opt_id = opt.id.clone();
                        let kind = opt.kind.clone();
                        let label = if i < 9 {
                            format!("{} {}", i + 1, opt.name)
                        } else {
                            opt.name
                        };
                        Button::new(SharedString::from(format!("perm-{}-{}", req_id, opt_id)))
                            .small()
                            .map(|b| match kind.as_str() {
                                "allow_once" => b.primary(),
                                k if k.starts_with("reject") => b.danger(),
                                _ => b.outline(),
                            })
                            .label(label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.answer_permission(&req_id, Some(opt_id.clone()), window, cx);
                            }))
                    }))
                    .when(!has_reject, |el| {
                        el.child(
                            Button::new(SharedString::from(format!(
                                "perm-cancel-{}",
                                req_id_cancel
                            )))
                            .small()
                            .danger()
                            .label(t(lang, "拒否", "Deny"))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.answer_permission(&req_id_cancel, None, window, cx);
                                },
                            )),
                        )
                    }),
            )
            .into_any_element()
    }

    /// A unified diff with added and removed lines colored.
    fn render_diff(&self, key: &str, diff: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let green: gpui::Hsla = status_green().into();
        let red: gpui::Hsla = status_red().into();
        let total = diff.lines().count();
        let lines: Vec<AnyElement> = diff
            .lines()
            .take(MAX_DIFF_LINES)
            .map(|line| {
                let row = div().px_2().whitespace_nowrap().child(if line.is_empty() {
                    " ".to_string()
                } else {
                    line.to_string()
                });
                match classify_diff_line(line) {
                    DiffLine::Header => row.text_color(theme.muted_foreground),
                    DiffLine::Hunk => row.text_color(theme.muted_foreground).bg(theme.secondary),
                    DiffLine::Added => row.text_color(green).bg(green.opacity(0.10)),
                    DiffLine::Removed => row.text_color(red).bg(red.opacity(0.10)),
                    DiffLine::Context => row,
                }
                .into_any_element()
            })
            .collect();
        div()
            .id(SharedString::from(format!("diff-{key}")))
            .v_flex()
            .max_h(px(320.0))
            .overflow_scroll()
            .py_1()
            .rounded(px(4.0))
            .bg(theme.background)
            .text_xs()
            .font_family(theme.mono_font_family.clone())
            .children(lines)
            .when(total > MAX_DIFF_LINES, |el| {
                el.child(
                    div()
                        .px_2()
                        .text_color(theme.muted_foreground)
                        .child(format!("… {} more lines", total - MAX_DIFF_LINES)),
                )
            })
            .into_any_element()
    }

    pub(super) fn render_user_input(
        &self,
        pending: &PendingInput,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let rid = pending.request.request_id.clone();
        let complete = input_complete(pending, cx);
        let questions = pending.request.questions.iter().enumerate().map(|(qi, q)| {
            let options = q.options.iter().enumerate().map(|(oi, opt)| {
                let rid = rid.clone();
                let selected = pending.selected[qi].contains(&oi);
                Button::new(SharedString::from(format!("q-{rid}-{qi}-{oi}")))
                    .small()
                    .map(|b| if selected { b.primary() } else { b.outline() })
                    .label(format!("{} {}", oi + 1, opt.label))
                    .tooltip(opt.description.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(p) = this
                            .pending_inputs
                            .iter_mut()
                            .find(|p| p.request.request_id == rid)
                        {
                            toggle_option(p, qi, oi);
                        }
                        cx.notify();
                    }))
            });
            div()
                .v_flex()
                .gap_1()
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .items_center()
                        .when(!q.header.is_empty(), |el| {
                            el.child(
                                div()
                                    .px_1()
                                    .rounded(px(4.0))
                                    .bg(theme.secondary)
                                    .text_xs()
                                    .child(q.header.clone()),
                            )
                        })
                        .child(div().text_sm().child(q.question.clone()))
                        .when(q.multi_select, |el| {
                            el.child(div().text_xs().text_color(theme.muted_foreground).child(t(
                                lang,
                                "（複数選択可）",
                                "(pick any)",
                            )))
                        }),
                )
                .child(div().h_flex().gap_2().flex_wrap().children(options))
                .when_some(pending.other[qi].clone(), |el, input| {
                    el.child(
                        Input::new(&input)
                            .small()
                            .when(q.secret, |i| i.mask_toggle()),
                    )
                })
        });
        let submit_id = rid.clone();
        let dismiss_id = rid.clone();
        div()
            .id(SharedString::from(format!("input-card-{rid}")))
            .v_flex()
            .p_3()
            .gap_3()
            .rounded(px(8.0))
            .bg(theme.sidebar)
            .border_1()
            .border_color(gpui::Hsla::from(status_amber()))
            .when(focused, |el| {
                el.track_focus(&self.approval_focus)
                    .focus(|s| s.border_2().border_color(theme.ring))
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        this.on_approval_key(ev, window, cx);
                    }))
            })
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(status_amber())
                            .child(format!(
                                "? {}",
                                t(lang, "エージェントからの質問", "The agent asks")
                            )),
                    )
                    .when(focused, |el| {
                        el.child(div().text_xs().text_color(theme.muted_foreground).child(t(
                            lang,
                            "数字キーで選択 · Enter 送信 · Esc 閉じる",
                            "Number keys pick · Enter sends · Esc dismisses",
                        )))
                    }),
            )
            .children(questions)
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("q-submit-{rid}")))
                            .small()
                            .primary()
                            .disabled(!complete)
                            .label(t(lang, "回答する", "Answer"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.answer_input(&submit_id, true, window, cx);
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("q-dismiss-{rid}")))
                            .small()
                            .ghost()
                            .label(t(lang, "答えない", "Skip"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.answer_input(&dismiss_id, false, window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// A proposed plan. `actionable` adds the button that implements it.
    pub(super) fn render_plan_card(
        &self,
        key: &str,
        plan: &str,
        actionable: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        div()
            .v_flex()
            .p_3()
            .gap_2()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.info)
            .bg(theme.info.opacity(0.06))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.info)
                    .child(format!("📋 {}", t(lang, "計画の提案", "Proposed plan"))),
            )
            .child(
                div().text_sm().child(
                    TextView::markdown(SharedString::from(format!("plan-{key}")), plan.to_string())
                        .selectable(true),
                ),
            )
            .when(actionable, |el| {
                el.child(
                    div()
                        .h_flex()
                        .gap_3()
                        .items_center()
                        .child(
                            Button::new(SharedString::from(format!("plan-go-{key}")))
                                .small()
                                .primary()
                                .label(t(lang, "この計画で実装", "Implement this plan"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.implement_plan(window, cx);
                                    cx.notify();
                                })),
                        )
                        .child(div().text_xs().text_color(theme.muted_foreground).child(t(
                            lang,
                            "直したい点があれば、そのまま指示を送ってください",
                            "To change it, just send your feedback",
                        ))),
                )
            })
            .into_any_element()
    }

    /// Leaves plan mode (back to the mode before it) and asks the agent to
    /// carry out its plan.
    pub(super) fn implement_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sid) = self.selected_session_id.clone() else {
            return;
        };
        if self.is_session_running(&sid) {
            return;
        }
        let current_mode = self
            .sessions
            .iter()
            .find(|s| s.id == sid)
            .map(|s| self.session_mode(s));
        let pre_plan = self.pre_plan_mode.remove(&sid);
        let mode = if current_mode == Some(ApprovalMode::Plan) {
            let next = pre_plan
                .filter(|m| *m != ApprovalMode::Plan)
                .unwrap_or(ApprovalMode::Ask);
            self.set_mode_for(&sid, next);
            next
        } else {
            current_mode.unwrap_or(ApprovalMode::Ask)
        };
        // Send the request without disturbing a draft in the prompt.
        let draft = self.prompt_input.read(cx).value().to_string();
        let attachments = std::mem::take(&mut self.attachments);
        let text = t(
            self.config.language,
            "上記の計画どおりに実装してください。",
            "Implement the plan above.",
        );
        self.set_prompt_text(text.to_string(), window, cx);
        self.send_current_prompt(window, cx);
        if !draft.is_empty() {
            self.set_prompt_text(draft, window, cx);
        }
        self.attachments = attachments;
        self.note(
            NoteKind::Info,
            format!(
                "{}: {}",
                t(self.config.language, "承認モード", "Approval mode"),
                mode_label(self.config.language, mode)
            ),
        );
    }
}

fn toggle_option(pending: &mut PendingInput, qi: usize, oi: usize) {
    let multi = pending.request.questions[qi].multi_select;
    let sel = &mut pending.selected[qi];
    if let Some(pos) = sel.iter().position(|&x| x == oi) {
        sel.remove(pos);
    } else if multi {
        sel.push(oi);
    } else {
        *sel = vec![oi];
    }
}

fn other_text(pending: &PendingInput, qi: usize, cx: &gpui::App) -> String {
    pending.other[qi]
        .as_ref()
        .map(|i| i.read(cx).value().trim().to_string())
        .unwrap_or_default()
}

/// Every question has a selected option or typed answer.
fn input_complete(pending: &PendingInput, cx: &gpui::App) -> bool {
    (0..pending.request.questions.len())
        .all(|qi| !pending.selected[qi].is_empty() || !other_text(pending, qi, cx).is_empty())
}

/// The answers per question: selected option labels, then any typed text.
fn collect_answers(pending: &PendingInput, cx: &gpui::App) -> Vec<Vec<String>> {
    pending
        .request
        .questions
        .iter()
        .enumerate()
        .map(|(qi, q)| {
            let typed = other_text(pending, qi, cx);
            if !q.multi_select && !typed.is_empty() {
                return vec![typed];
            }
            let mut a: Vec<String> = pending.selected[qi]
                .iter()
                .filter_map(|&oi| q.options.get(oi).map(|o| o.label.clone()))
                .collect();
            if !typed.is_empty() {
                a.push(typed);
            }
            a
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use same_core::{UserQuestion, UserQuestionOption};

    fn opt(id: &str, kind: &str) -> PermissionOption {
        PermissionOption {
            id: id.into(),
            name: id.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn keys_map_to_options() {
        let opts = vec![
            opt("allow", "allow_once"),
            opt("session", "allow_always"),
            opt("deny", "reject_once"),
            opt("stop", "reject_always"),
        ];
        let pick = |k| permission_key_choice(k, &opts);
        assert_eq!(pick("1"), Some(KeyChoice::Option("allow".into())));
        assert_eq!(pick("4"), Some(KeyChoice::Option("stop".into())));
        assert_eq!(pick("5"), None);
        assert_eq!(pick("0"), None);
        assert_eq!(pick("y"), None);
        assert_eq!(pick("enter"), None);
        assert_eq!(pick("n"), Some(KeyChoice::Option("deny".into())));
        assert_eq!(pick("escape"), Some(KeyChoice::Option("deny".into())));
        assert_eq!(pick("x"), None);
        // No reject option: a plain deny.
        assert_eq!(
            permission_key_choice("n", &[opt("ok", "allow_once")]),
            Some(KeyChoice::Deny)
        );
    }

    #[test]
    fn diff_lines_are_classified() {
        assert_eq!(classify_diff_line("--- a/x"), DiffLine::Header);
        assert_eq!(classify_diff_line("+++ b/x"), DiffLine::Header);
        assert_eq!(classify_diff_line("@@ -1 +1 @@"), DiffLine::Hunk);
        assert_eq!(classify_diff_line("+new"), DiffLine::Added);
        assert_eq!(classify_diff_line("-old"), DiffLine::Removed);
        assert_eq!(classify_diff_line(" same"), DiffLine::Context);
    }

    #[test]
    fn plan_is_split_from_reply() {
        let stored = format!("Here it is.{PLAN_MARKER}1. Do X\n2. Do Y");
        let (text, plan) = split_plan(&stored);
        assert_eq!(text, "Here it is.");
        assert_eq!(plan, Some("1. Do X\n2. Do Y"));
        assert_eq!(split_plan("plain"), ("plain", None));
    }

    #[test]
    fn options_toggle_single_and_multi() {
        let q = |multi| UserQuestion {
            id: "q".into(),
            header: String::new(),
            question: "?".into(),
            options: vec![
                UserQuestionOption {
                    label: "A".into(),
                    description: String::new(),
                },
                UserQuestionOption {
                    label: "B".into(),
                    description: String::new(),
                },
            ],
            multi_select: multi,
            allow_other: false,
            secret: false,
        };
        let mut p = PendingInput::new(UserInputRequest {
            request_id: "r".into(),
            session_id: "s".into(),
            questions: vec![q(false), q(true)],
        });
        toggle_option(&mut p, 0, 0);
        toggle_option(&mut p, 0, 1);
        assert_eq!(p.selected[0], vec![1]);
        toggle_option(&mut p, 1, 0);
        toggle_option(&mut p, 1, 1);
        assert_eq!(p.selected[1], vec![0, 1]);
        toggle_option(&mut p, 1, 0);
        assert_eq!(p.selected[1], vec![1]);
    }

    #[test]
    fn mode_labels_cover_all_modes() {
        for m in ApprovalMode::ALL {
            assert!(!mode_label(Language::Ja, m).is_empty());
            assert!(!mode_hint(Language::En, m).is_empty());
        }
    }
}
