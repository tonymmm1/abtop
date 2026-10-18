//! Full-screen per-account quota view (`a`), fed by the CLIProxyAPI poller.

use crate::app::App;
use crate::collector::cliproxy::{summarize, Account, State, REFRESH};
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::quota::format_reset_time;
use super::{btop_block_active, grad_at, make_gradient, remaining_bar};

const COL_W: usize = 28;
const PROVIDERS: &[&str] = &["claude", "codex"];

pub(crate) fn draw_accounts(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    f.render_widget(
        btop_block_active("accounts", "", theme.cpu_box, theme, false),
        area,
    );
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let w = inner.width as usize;
    let dim = Style::default().fg(theme.graph_text);
    let grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);

    let Some(handle) = app.cliproxy.as_ref() else {
        let lines = vec![
            Line::from(Span::styled(
                " CLIProxyAPI is not configured.",
                Style::default().fg(theme.title),
            )),
            Line::from(Span::styled(
                " Set CLIPROXY_MANAGEMENT_KEY (and optionally CLIPROXY_URL), or cliproxy_key /",
                dim,
            )),
            Line::from(Span::styled(
                " cliproxy_url in ~/.config/abtop/config.toml, then restart.",
                dim,
            )),
            Line::from(Span::styled(" a/Esc close", dim)),
        ];
        f.render_widget(Paragraph::new(lines), inner);
        return;
    };
    let Ok(state) = handle.state.lock() else {
        return;
    };

    let mut lines = vec![status_line(&handle.url, &state, theme), Line::from("")];
    let name_w = (w / 4).clamp(16, 36);
    let ncols = (w.saturating_sub(name_w) / COL_W).max(1);
    let now = chrono::Utc::now().timestamp();

    for provider in PROVIDERS {
        let accounts: Vec<&Account> = state
            .accounts
            .iter()
            .filter(|a| a.provider == *provider)
            .collect();
        if accounts.is_empty() {
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}", provider.to_uppercase()),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {}", accounts.len()), dim),
        ]));
        summary_lines(&state.accounts, provider, now, &grad, theme, &mut lines);
        lines.push(divider(w, theme));
        for a in accounts {
            account_lines(a, name_w, ncols, now, &grad, theme, &mut lines);
            lines.push(divider(w, theme));
        }
        lines.push(Line::from(""));
    }

    let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
    f.render_widget(
        Paragraph::new(lines).scroll((app.accounts_scroll.min(max_scroll), 0)),
        inner,
    );
}

fn status_line(url: &str, s: &State, theme: &Theme) -> Line<'static> {
    let dim = Style::default().fg(theme.graph_text);
    let mut spans = vec![Span::styled(
        format!(" cliproxy {url}"),
        Style::default().fg(theme.main_fg),
    )];
    match (s.loading, s.updated_at) {
        (true, _) => spans.push(Span::styled(" · refreshing…", dim)),
        (false, Some(t)) => {
            let age = t.elapsed().as_secs();
            let next = REFRESH.as_secs().saturating_sub(age);
            spans.push(Span::styled(
                format!(" · updated {age}s ago · next in {next}s"),
                dim,
            ));
        }
        _ => {}
    }
    if let Some(e) = &s.error {
        spans.push(Span::styled(
            format!(" · {e}"),
            Style::default().fg(Color::Red),
        ));
    }
    spans.push(Span::styled("   r refresh · j/k scroll · a/Esc close", dim));
    Line::from(spans)
}

/// One line per window label: summed remaining across accounts + soonest reset.
fn summary_lines(
    accounts: &[Account],
    provider: &str,
    now: i64,
    grad: &[Color; 101],
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
) {
    let dim = Style::default().fg(theme.graph_text);
    for s in summarize(accounts, provider, now) {
        let mut spans = vec![
            Span::styled(format!("   {:<20}", s.label), dim),
            Span::styled(
                format!("{:>4.0}%", s.sum),
                Style::default()
                    .fg(grad_at(grad, 100.0 - s.pooled()))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" of {:<5} ", format!("{}%", s.count * 100)), dim),
        ];
        spans.extend(remaining_bar(s.pooled(), 16, grad, theme.meter_bg));
        if let Some(t) = s.next_reset {
            spans.push(Span::styled(
                format!("  next reset {}", reset_text(Some(t), now)),
                dim,
            ));
        }
        out.push(Line::from(spans));
    }
}

fn account_lines(
    a: &Account,
    name_w: usize,
    ncols: usize,
    now: i64,
    grad: &[Color; 101],
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
) {
    let dim = Style::default().fg(theme.graph_text);
    let name = Span::styled(
        pad(&format!(" {}", a.name), name_w),
        Style::default()
            .fg(theme.main_fg)
            .add_modifier(Modifier::BOLD),
    );
    let plan = Span::styled(
        pad(&format!("  {}", a.plan.as_deref().unwrap_or("")), name_w),
        dim,
    );

    let note = match (a.cooldown_until, a.renews_at) {
        (Some(t), _) => Span::styled(
            pad(&format!("  cooldown until {}", local_date(t)), name_w),
            Style::default().fg(Color::Yellow),
        ),
        (None, Some(t)) => Span::styled(
            pad(&format!("  active until {}", local_date(t)), name_w),
            dim,
        ),
        _ => Span::raw(" ".repeat(name_w)),
    };

    if a.disabled {
        out.push(Line::from(vec![
            name,
            Span::styled(
                "disabled in CLIProxyAPI",
                Style::default().fg(theme.inactive_fg),
            ),
        ]));
        out.push(Line::from(plan));
        return;
    }
    if let Some(err) = &a.error {
        out.push(Line::from(vec![
            name,
            Span::styled(err.clone(), Style::default().fg(Color::Red)),
        ]));
        out.push(Line::from(plan));
        out.push(Line::from(note));
        return;
    }

    let blank = || Span::raw(" ".repeat(name_w));
    for (i, chunk) in a.windows.chunks(ncols).enumerate() {
        let (mut l1, mut l2, mut l3) = if i == 0 {
            (vec![name.clone()], vec![plan.clone()], vec![note.clone()])
        } else {
            (vec![blank()], vec![blank()], vec![blank()])
        };
        for w in chunk {
            let label_w = COL_W - 8;
            l1.push(Span::styled(pad(&w.label, label_w), dim));
            match w.remaining {
                Some(r) => {
                    l1.push(Span::styled(
                        format!("{:>4.0}%  ", r),
                        Style::default()
                            .fg(grad_at(grad, 100.0 - r))
                            .add_modifier(Modifier::BOLD),
                    ));
                    l2.extend(remaining_bar(r, COL_W - 4, grad, theme.meter_bg));
                }
                None => {
                    l1.push(Span::styled(format!("{:>5}  ", "—"), dim));
                    l2.push(Span::raw(" ".repeat(COL_W - 4)));
                }
            }
            l2.push(Span::raw("    "));
            l3.push(Span::styled(pad(&reset_text(w.resets_at, now), COL_W), dim));
        }
        out.extend([Line::from(l1), Line::from(l2), Line::from(l3)]);
    }
}

pub(crate) fn local_date(t: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(t, 0)
        .single()
        .map(|d| d.format("%m/%d %H:%M").to_string())
        .unwrap_or_default()
}

/// "in 2d 3h · 10/21 02:00" in local time, or "No reset pending".
fn reset_text(resets_at: Option<i64>, now: i64) -> String {
    match resets_at.filter(|t| *t > now) {
        Some(t) => format!("{} · {}", format_reset_time(t as u64), local_date(t)),
        None => "No reset pending".into(),
    }
}

fn divider(w: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(w),
        Style::default().fg(theme.div_line),
    ))
}

/// Truncate with an ellipsis or right-pad to exactly `w` chars.
fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n > w {
        let mut t: String = s.chars().take(w.saturating_sub(2)).collect();
        t.push_str("… ");
        t
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}
