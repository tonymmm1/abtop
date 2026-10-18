use crate::app::App;
use crate::collector::cliproxy::{summarize, Account, State, Summary};
use crate::locale::t;
use crate::model::RateLimitInfo;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::accounts::local_date;
use super::{btop_block_active, fmt_tokens, grad_at, make_gradient, remaining_bar, styled_label};

/// Data considered "stale" when its updated_at is older than this many seconds.
const STALE_SECS: u64 = 600;

/// Fixed source order so columns stay stable across runs.
const SOURCES: &[&str] = &["claude", "codex"];

pub(crate) fn draw_quota_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    draw_quota_panel_active(f, app, area, theme, false);
}

pub(crate) fn draw_quota_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    let cpu_grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);

    let block = btop_block_active("quota", "²", theme.cpu_box, theme, active);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    // CLIProxyAPI pools take the whole box; the tokens panel already shows totals.
    if let Some(state) = app.cliproxy.as_ref().and_then(|h| h.state.lock().ok()) {
        draw_cliproxy_rows(f, inner, &state, &cpu_grad, theme);
        return;
    }

    // Bottom summary: total tokens + rate
    let total_tokens: u64 = app.sessions.iter().map(|s| s.total_tokens()).sum();
    let rates = &app.token_rates;
    let ticks_per_min = 30usize;
    let tokens_per_min: f64 = rates.iter().rev().take(ticks_per_min).sum();

    // Split into side-by-side columns: one per known source (CLAUDE | CODEX).
    // Columns are always rendered so the panel layout stays stable even when a
    // source has no data yet.
    let num_sources = SOURCES.len() as u16;
    let col_w = inner.width / num_sources;
    let content_h = inner.height.saturating_sub(1); // reserve last row for totals

    for (i, source) in SOURCES.iter().enumerate() {
        let col_x = inner.x + (i as u16) * col_w;
        let this_w = if i as u16 == num_sources - 1 {
            inner.width - (i as u16) * col_w
        } else {
            col_w
        };
        let col_area = Rect {
            x: col_x,
            y: inner.y,
            width: this_w,
            height: content_h,
        };

        let rl = app
            .rate_limits
            .iter()
            .find(|r| r.source.eq_ignore_ascii_case(source));
        draw_source_column(f, col_area, source, rl, &cpu_grad, theme);
    }

    // Total tokens summary on last row (full width)
    let bottom_area = Rect {
        x: inner.x,
        y: inner.y + content_h,
        width: inner.width,
        height: 1,
    };
    let total_label = t("quota.total");
    f.render_widget(
        Paragraph::new(vec![Line::from(vec![
            Span::styled(
                format!(" {} {}", total_label, fmt_tokens(total_tokens)),
                Style::default().fg(theme.main_fg),
            ),
            Span::styled(
                format!(" {}/min", fmt_tokens(tokens_per_min as u64)),
                Style::default().fg(theme.graph_text),
            ),
        ])]),
        bottom_area,
    );
}

fn draw_source_column(
    f: &mut Frame,
    area: Rect,
    source: &str,
    rl: Option<&RateLimitInfo>,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) {
    let col_w_usize = area.width as usize;
    let bar_w = col_w_usize.saturating_sub(10).clamp(2, 8);

    let Some(rl) = rl else {
        let hint = if source.eq_ignore_ascii_case("claude") {
            t("quota.abtop_setup")
        } else {
            t("quota.run_codex")
        };
        let no_data = t("quota.no_data");
        let lines = vec![
            Line::from(Span::styled(
                format!(" {}", source.to_uppercase()),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("  — {}", no_data),
                Style::default().fg(theme.inactive_fg),
            )),
            Line::from(Span::styled(
                format!(" {}", hint),
                Style::default().fg(theme.graph_text),
            )),
        ];
        f.render_widget(Paragraph::new(lines), area);
        return;
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let is_stale = rl
        .updated_at
        .is_some_and(|ts| now.saturating_sub(ts) > STALE_SECS);

    // Stale data → dim the source name. Drops the unlabeled "Xs ago" row
    // we used to render here; keeping a distinct freshness color but no
    // explicit number is enough to signal "values may be out of date"
    // without competing with the reset countdown for the user's attention.
    let source_color = if is_stale {
        theme.inactive_fg
    } else {
        theme.title
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(" {}", rl.source.to_uppercase()),
        Style::default()
            .fg(source_color)
            .add_modifier(Modifier::BOLD),
    )));

    // Reset countdown is only meaningful when the data is fresh enough
    // that the reported `resets_at` is still in the future. Stale sources
    // get the bar (the % is approximately correct) but no countdown row.
    let show_reset = !is_stale;

    if let Some(used_pct) = rl.five_hour_pct {
        let remaining = (100.0 - used_pct).clamp(0.0, 100.0);
        let reset = if show_reset {
            rl.five_hour_resets_at
                .map(format_reset_time)
                .unwrap_or_default()
        } else {
            String::new()
        };
        let c = grad_at(cpu_grad, used_pct);
        let label_5h = format_window_label(rl.five_hour_window_minutes, t("quota.5h"));
        let mut s = vec![styled_label(
            format!(" {}", label_5h).as_str(),
            theme.graph_text,
        )];
        s.extend(remaining_bar(remaining, bar_w, cpu_grad, theme.meter_bg));
        s.push(Span::styled(
            format!(" {:>3.0}%", remaining),
            Style::default().fg(c),
        ));
        lines.push(Line::from(s));
        // Always reserve the row so both columns line up vertically;
        // when there's nothing meaningful to show (stale source or the
        // cached reset moment is past), render it blank.
        lines.push(Line::from(Span::styled(
            if reset.is_empty() {
                String::new()
            } else {
                format!("  {}", reset)
            },
            Style::default().fg(theme.graph_text),
        )));
    }
    if let Some(used_pct) = rl.seven_day_pct {
        let remaining = (100.0 - used_pct).clamp(0.0, 100.0);
        let reset = if show_reset {
            rl.seven_day_resets_at
                .map(format_reset_time)
                .unwrap_or_default()
        } else {
            String::new()
        };
        let c = grad_at(cpu_grad, used_pct);
        let label_7d = format_window_label(rl.seven_day_window_minutes, t("quota.7d"));
        let mut s = vec![styled_label(
            format!(" {}", label_7d).as_str(),
            theme.graph_text,
        )];
        s.extend(remaining_bar(remaining, bar_w, cpu_grad, theme.meter_bg));
        s.push(Span::styled(
            format!(" {:>3.0}%", remaining),
            Style::default().fg(c),
        ));
        lines.push(Line::from(s));
        // Always reserve the row so both columns line up vertically;
        // when there's nothing meaningful to show (stale source or the
        // cached reset moment is past), render it blank.
        lines.push(Line::from(Span::styled(
            if reset.is_empty() {
                String::new()
            } else {
                format!("  {}", reset)
            },
            Style::default().fg(theme.graph_text),
        )));
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// Pooled CLIProxyAPI quota per provider (`7d  43% of 300% ■■■  in 51m`),
/// followed by every active account with its own bars and reset timers.
fn draw_cliproxy_rows(
    f: &mut Frame,
    area: Rect,
    state: &State,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) {
    let dim = Style::default().fg(theme.graph_text);
    let bold = |c| Style::default().fg(c).add_modifier(Modifier::BOLD);
    let now = chrono::Utc::now().timestamp();
    let width = area.width as usize;
    let mut lines = Vec::new();

    let amount = |s: &Summary| format!("{:.0}% of {}%", s.sum, s.count * 100);
    let per_source: Vec<Vec<Summary>> = SOURCES
        .iter()
        .map(|src| summarize(&state.accounts, src, now))
        .collect();
    // Shared column widths so both providers line up.
    let all = per_source.iter().flatten();
    let label_w = all
        .clone()
        .map(|s| short_label(&s.label).chars().count())
        .max()
        .unwrap_or(2);
    let amount_w = all.map(|s| amount(s).chars().count()).max().unwrap_or(0);
    // "  label amount " then the bar
    let bar_w = width.saturating_sub(label_w + amount_w + 5).min(20);

    // Disabled accounts only appear in the full accounts view (`a`).
    let active: Vec<&Account> = state.accounts.iter().filter(|a| !a.disabled).collect();
    let max_windows = active
        .iter()
        .map(|a| a.windows.len())
        .max()
        .unwrap_or(1)
        .max(1);
    let cell_w = (width.saturating_sub(account_prefix_w(width)) / max_windows)
        .clamp(label_w + 8, label_w + 20);

    for (i, (source, summaries)) in SOURCES.iter().zip(per_source).enumerate() {
        let accounts: Vec<&Account> = active
            .iter()
            .copied()
            .filter(|a| a.provider == *source)
            .collect();
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled(format!(" {}", source.to_uppercase()), bold(theme.title)),
            Span::styled(format!(" ×{}", accounts.len()), dim),
        ]));
        if summaries.is_empty() {
            let msg = match (&state.error, state.loading) {
                (Some(e), _) => e.clone(),
                (None, true) => "loading…".into(),
                _ => format!("— {}", t("quota.no_data")),
            };
            lines.push(Line::from(Span::styled(format!("  {msg}"), dim)));
        }
        for s in &summaries {
            let pooled = s.pooled();
            let (label, amount) = (short_label(&s.label), amount(s));
            let mut spans = vec![
                Span::styled(format!("  {label:<label_w$} "), bold(theme.main_fg)),
                Span::styled(
                    format!("{amount:>amount_w$} "),
                    bold(grad_at(cpu_grad, 100.0 - pooled)),
                ),
            ];
            if bar_w >= 2 {
                spans.extend(remaining_bar(pooled, bar_w, cpu_grad, theme.meter_bg));
            }
            let used = label_w + amount_w + 4 + bar_w;
            if let Some(reset) = s.next_reset.map(|t| format_reset_time(t as u64)) {
                if used + reset.chars().count() + 2 <= width {
                    spans.push(Span::styled(format!("  {reset}"), dim));
                }
            }
            lines.push(Line::from(spans));
        }
        if !accounts.is_empty() {
            lines.push(Line::from(""));
        }
        for a in accounts {
            lines.extend(account_lines(a, width, label_w, cell_w, cpu_grad, theme));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Two lines per account: a bar + percentage per window, then reset timers
/// under each window with cooldown / subscription status under the name.
fn account_lines(
    a: &Account,
    width: usize,
    label_w: usize,
    cell_w: usize,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(theme.graph_text);
    let name_w = account_name_w(width);
    let prefix_w = account_prefix_w(width);
    let ncells = width.saturating_sub(prefix_w) / cell_w;
    // cell: "label ■■■■  42%  "
    let bar_w = cell_w.saturating_sub(label_w + 8).min(12);

    let mut top = vec![
        Span::styled(
            format!("   {}", fit(&a.name, name_w)),
            Style::default().fg(theme.main_fg),
        ),
        Span::styled(
            format!(" {} ", fit(a.plan.as_deref().unwrap_or("—"), 8)),
            dim,
        ),
    ];
    let (status, status_style) = match (a.cooldown_until, a.renews_at) {
        (Some(t), _) => (
            format!("cooldown {}", local_date(t)),
            Style::default().fg(Color::Yellow),
        ),
        (None, Some(t)) => (format!("active until {}", local_date(t)), dim),
        _ => (String::new(), dim),
    };
    let mut bottom = vec![Span::styled(
        fit(&format!("    {status}"), prefix_w),
        status_style,
    )];

    if let Some(err) = &a.error {
        let room = width.saturating_sub(prefix_w);
        top.push(Span::styled(
            fit(err, room).trim_end().to_string(),
            Style::default().fg(Color::Red),
        ));
        return vec![Line::from(top), Line::from(bottom)];
    }
    for w in a.windows.iter().take(ncells) {
        top.push(Span::styled(
            format!("{:<label_w$} ", short_label(&w.label)),
            dim,
        ));
        if bar_w >= 2 {
            let r = w.remaining.unwrap_or(0.0);
            top.extend(remaining_bar(r, bar_w, cpu_grad, theme.meter_bg));
        }
        let pct_w = cell_w - label_w - 1 - bar_w;
        top.push(match w.remaining {
            Some(r) => Span::styled(
                format!("{:<pct_w$}", format!(" {r:>3.0}%")),
                Style::default().fg(grad_at(cpu_grad, 100.0 - r)),
            ),
            None => Span::styled(format!("{:<pct_w$}", "   —"), dim),
        });
        let reset = w
            .resets_at
            .map(|t| format_reset_time(t as u64))
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "—".into());
        bottom.push(Span::styled(fit(&reset, cell_w), dim));
    }
    vec![Line::from(top), Line::from(bottom)]
}

fn account_name_w(width: usize) -> usize {
    (width / 4).clamp(10, 26)
}

/// Columns before the first window cell: indent + name + plan.
fn account_prefix_w(width: usize) -> usize {
    3 + account_name_w(width) + 10
}

/// Rows the CLIProxyAPI quota box wants (excluding borders), for layout sizing.
/// Must mirror `draw_cliproxy_rows`.
pub(crate) fn cliproxy_rows(app: &App) -> Option<u16> {
    let state = app.cliproxy.as_ref()?.state.lock().ok()?;
    let now = chrono::Utc::now().timestamp();
    let rows: usize = SOURCES
        .iter()
        .map(|src| {
            let accounts = state
                .accounts
                .iter()
                .filter(|a| a.provider == *src && !a.disabled)
                .count();
            let pooled = summarize(&state.accounts, src, now).len().max(1);
            // header + pooled rows + blank + 2 per account
            1 + pooled + usize::from(accounts > 0) + 2 * accounts
        })
        .sum();
    // blank line between providers
    Some((rows + SOURCES.len() - 1) as u16)
}

/// Truncate with an ellipsis or right-pad to exactly `w` chars.
fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n > w {
        let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

/// "5-hour limit" → "5h", "7-day Fable 5" → "fable"; keeps the panel narrow.
fn short_label(label: &str) -> String {
    let l = label.trim_end_matches(" limit");
    match l {
        "5-hour" => "5h".into(),
        "7-day" => "7d".into(),
        "Weekly" => "wk".into(),
        "Monthly" => "mo".into(),
        _ => l
            .strip_prefix("7-day ")
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or(l)
            .to_lowercase(),
    }
}

fn format_window_label(window_minutes: Option<u64>, fallback: String) -> String {
    let Some(minutes) = window_minutes else {
        return fallback;
    };

    if minutes == 0 {
        fallback
    } else if minutes % (24 * 60) == 0 {
        format!("{}{}", minutes / (24 * 60), t("time.d"))
    } else if minutes % 60 == 0 {
        format!("{}{}", minutes / 60, t("time.h"))
    } else {
        format!("{}{}", minutes, t("time.m"))
    }
}

/// Format a reset timestamp as a human countdown labeled "in X" so the
/// row reads as a time-until-reset. Returns an empty string when the
/// reset is already in the past — the actual next reset depends on the
/// window length which the caller doesn't track, and showing a "now"
/// sentinel was misleading on stale sources where the window had
/// already rolled over multiple times. Callers skip the row when this
/// returns empty.
pub(crate) fn format_reset_time(reset_ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if reset_ts <= now {
        return String::new();
    }
    let diff = reset_ts - now;
    let prefix = t("quota.in");
    if diff < 60 {
        format!("{} {}{}", prefix, diff, t("time.s"))
    } else if diff < 3600 {
        format!("{} {}{}", prefix, diff / 60, t("time.m"))
    } else if diff < 86400 {
        let h = diff / 3600;
        let m = (diff % 3600) / 60;
        format!("{} {}{} {}{}", prefix, h, t("time.h"), m, t("time.m"))
    } else {
        let d = diff / 86400;
        let h = (diff % 86400) / 3600;
        format!("{} {}{} {}{}", prefix, d, t("time.d"), h, t("time.h"))
    }
}
