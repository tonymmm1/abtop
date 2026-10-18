//! Remote hosts panel: one vitals line per host, then one line per agent session.

use crate::app::App;
use crate::collector::remote::{HostStatus, POLL};
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::header::{fmt_cpu, fmt_mem, usage_spans};
use super::{btop_block_active, grad_at, make_gradient, meter_bar};

/// Rows the panel wants (excluding borders); None when no hosts are configured.
pub(crate) fn remote_rows(app: &App) -> Option<u16> {
    let remote = app.remote.as_ref()?;
    let hosts = remote.hosts.lock().ok()?;
    let costs = remote.ccusage.iter().filter(|h| h.visible()).count() as u16;
    Some(hosts.iter().map(|h| 1 + h.sessions.len() as u16).sum::<u16>() + costs)
}

pub(crate) fn draw_remote_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    f.render_widget(
        btop_block_active("remote", "", theme.cpu_box, theme, false),
        area,
    );
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let Some(remote) = app.remote.as_ref() else {
        return;
    };
    let Ok(hosts) = remote.hosts.lock() else {
        return;
    };
    let name_w = hosts
        .iter()
        .map(|h| h.name.chars().count())
        .max()
        .unwrap_or(8)
        .min(20);
    let mut lines = Vec::new();
    for (i, h) in hosts.iter().enumerate() {
        lines.push(host_line(h, name_w, theme));
        if let Some(cost) = remote.ccusage.get(i).filter(|c| c.visible()) {
            let mut spans = vec![Span::raw(" ".repeat(name_w + 2))];
            spans.extend(usage_spans(cost, theme));
            lines.push(Line::from(spans));
        }
        for s in &h.sessions {
            let (dot, style) = match s.working {
                Some(true) => (
                    "●",
                    Style::default()
                        .fg(theme.main_fg)
                        .add_modifier(Modifier::BOLD),
                ),
                Some(false) => ("◌", Style::default().fg(theme.graph_text)),
                None => ("?", Style::default().fg(theme.inactive_fg)),
            };
            let dim = Style::default().fg(theme.graph_text);
            let mut spans = vec![
                Span::styled(format!("   {dot} "), style),
                Span::styled(format!("{:<7}", s.agent), dim),
                Span::styled(
                    format!("{:<18} ", s.project),
                    Style::default()
                        .fg(theme.title)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if !s.branch.is_empty() {
                spans.push(Span::styled(
                    format!("{} ", s.branch),
                    Style::default().fg(theme.proc_misc),
                ));
            }
            if s.dirty > 0 {
                spans.push(Span::styled(
                    format!("~{} ", s.dirty),
                    Style::default().fg(theme.warning_fg),
                ));
            }
            spans.push(Span::styled(
                format!(" {}", s.cwd),
                Style::default().fg(theme.inactive_fg),
            ));
            lines.push(Line::from(spans));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn host_line(h: &HostStatus, name_w: usize, theme: &Theme) -> Line<'static> {
    let grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);
    let dim = Style::default().fg(theme.graph_text);
    let stale = h.updated_at.is_none_or(|t| t.elapsed() > POLL * 3);
    let name_style = Style::default()
        .fg(if stale {
            theme.inactive_fg
        } else {
            theme.title
        })
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled(format!(" {:<name_w$} ", h.name), name_style)];

    if h.updated_at.is_none() {
        let (msg, style) = match &h.error {
            Some(e) => (e.clone(), Style::default().fg(Color::Red)),
            None => ("connecting…".to_string(), dim),
        };
        spans.push(Span::styled(msg, style));
        return Line::from(spans);
    }

    // Same "working/live" counters as the header; green when nothing is working.
    for agent in ["claude", "codex"] {
        let live = h.sessions.iter().filter(|s| s.agent == agent);
        let working = live.clone().filter(|s| s.working == Some(true)).count();
        let style = if working == 0 {
            Style::default().fg(Color::Black).bg(Color::Rgb(0, 255, 65))
        } else {
            Style::default().fg(theme.main_fg)
        };
        spans.push(Span::styled(
            format!("{agent} {working}/{}", live.count()),
            style.add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("  "));
    }

    spans.push(Span::styled("CPU ", dim));
    spans.extend(meter_bar(h.cpu_pct, 6, &grad, theme.meter_bg));
    spans.push(Span::styled(
        format!(" {}  ", fmt_cpu(h.cpu_pct, h.cores)),
        Style::default().fg(grad_at(&grad, h.cpu_pct)),
    ));
    spans.push(Span::styled("MEM ", dim));
    spans.extend(meter_bar(h.mem_pct, 6, &grad, theme.meter_bg));
    spans.push(Span::styled(
        format!(" {}  ", fmt_mem(h.mem_used_gib, h.mem_total_gib)),
        Style::default().fg(grad_at(&grad, h.mem_pct)),
    ));
    spans.push(Span::styled(format!("L {:>5.1}", h.load1), dim));
    for (i, g) in h.gpus.iter().enumerate() {
        let vram = if g.mem_total_mb > 0.0 {
            g.mem_used_mb / g.mem_total_mb * 100.0
        } else {
            0.0
        };
        spans.push(Span::styled(format!("  GPU{i} "), dim));
        spans.push(Span::styled(
            format!("{:>3.0}%", g.util_pct),
            Style::default().fg(grad_at(&grad, g.util_pct)),
        ));
        spans.push(Span::styled(
            format!(
                " {:.1}/{:.1}G",
                g.mem_used_mb / 1024.0,
                g.mem_total_mb / 1024.0
            ),
            Style::default().fg(grad_at(&grad, vram)),
        ));
    }
    if let Some(e) = &h.error {
        spans.push(Span::styled(
            format!("  ! {e}"),
            Style::default().fg(Color::Red),
        ));
    }
    Line::from(spans)
}
