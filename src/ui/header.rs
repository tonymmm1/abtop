use crate::app::App;
use crate::collector::ccusage;
use crate::host_info::GpuInfo;
use crate::locale::t;
use crate::model::SessionStatus;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{grad_at, make_gradient, meter_bar};

pub(crate) fn draw_header(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let session_count = app.sessions.len();
    let active = app.agent_aggregate.active_count;

    let now = chrono::Local::now().format("%m-%d-%Y %H:%M").to_string();
    let version = env!("CARGO_PKG_VERSION");

    // "claude 5/6" = working / live sessions per agent.
    let counters: Vec<(String, bool)> = ["claude", "codex"]
        .iter()
        .map(|cli| {
            let live = app
                .sessions
                .iter()
                .filter(|s| s.agent_cli == *cli && s.status != SessionStatus::Done);
            let working = live.clone().filter(|s| s.status.is_active()).count();
            (format!("{cli} {working}/{}", live.count()), working == 0)
        })
        .collect();
    let agents = counters
        .iter()
        .map(|(c, _)| format!("{c}  "))
        .collect::<String>();

    let title = format!(" abtop v{version} ");
    let right = format!(" {agents}{now}  {active}↑ {session_count}● ");

    let host_str = app.host_metrics.as_ref().map(fmt_host);
    let agent_str = fmt_agent(&app.agent_aggregate);

    // Width budget: prefer host + agents; fall back to agents-only; then to nothing.
    let width = area.width as usize;
    let base = title.len() + right.len() + 4; // 4 = separators / padding
    let (host_render, agent_render) = if width <= 80 {
        (None, None)
    } else {
        pick_metrics(host_str.as_deref(), &agent_str, width, base)
    };

    let mut spans: Vec<Span> = Vec::with_capacity(8);
    spans.push(Span::styled(
        title.clone(),
        Style::default()
            .fg(theme.title)
            .add_modifier(Modifier::BOLD),
    ));

    if let Some(h) = host_render {
        spans.push(Span::styled(
            format!(" {h} "),
            Style::default().fg(theme.graph_text),
        ));
    }

    if host_render.is_some() && agent_render.is_some() {
        spans.push(Span::styled("─", Style::default().fg(theme.div_line)));
    }

    if let Some(a) = agent_render {
        spans.push(Span::styled(
            format!(" {a} "),
            Style::default().fg(theme.graph_text),
        ));
    }

    // Right-align the timestamp+counters block.
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = width.saturating_sub(used + right.chars().count());
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(Span::raw(" "));
    for (text, idle) in &counters {
        // Nothing working → black on bright green: time to start something.
        let style = if *idle {
            Style::default().fg(Color::Black).bg(Color::Rgb(0, 255, 65))
        } else {
            Style::default().fg(theme.main_fg)
        };
        spans.push(Span::styled(
            text.clone(),
            style.add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(
        format!("{now}  "),
        Style::default().fg(theme.graph_text),
    ));
    spans.push(Span::styled(
        format!("{active}↑"),
        Style::default().fg(theme.proc_misc),
    ));
    spans.push(Span::styled(
        format!(" {session_count}●"),
        Style::default().fg(theme.main_fg),
    ));
    spans.push(Span::raw("  "));

    f.render_widget(Paragraph::new(Line::from(spans)), area);

    if area.height >= 2 && header_rows(app) == 2 {
        let row = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };
        // GPU bars on the left, ccusage costs right-aligned.
        let cost = app
            .ccusage
            .as_ref()
            .filter(|h| h.visible())
            .map(|h| usage_spans(h, theme))
            .unwrap_or_default();
        let cost_w = spans_width(&cost);
        let mut spans = if app.gpus.is_empty() {
            Vec::new()
        } else {
            gpu_line(&app.gpus, width.saturating_sub(cost_w), theme).spans
        };
        let pad = width.saturating_sub(spans_width(&spans) + cost_w);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(cost);
        f.render_widget(Paragraph::new(Line::from(spans)), row);
    }
}

/// Header height: a second row appears for GPU bars or ccusage costs.
pub(crate) fn header_rows(app: &App) -> u16 {
    let ccusage = app.ccusage.as_ref().is_some_and(|h| h.visible());
    if app.gpus.is_empty() && !ccusage {
        1
    } else {
        2
    }
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// `5h $125→$1.2k $232/h 4h28m  today $670 2.0B  7d ▂▃▅▂▇▆█ $6.6k`
/// (API list prices from ccusage). Shared with the remote hosts panel.
pub(crate) fn usage_spans(h: &ccusage::Handle, theme: &Theme) -> Vec<Span<'static>> {
    let dim = Style::default().fg(theme.graph_text);
    let val = Style::default()
        .fg(theme.main_fg)
        .add_modifier(Modifier::BOLD);
    let Ok(state) = h.state.lock() else {
        return Vec::new();
    };
    let mut spans = vec![Span::styled("ccusage ", dim)];
    let Some(u) = &state.usage else {
        match &state.error {
            Some(e) => spans.push(Span::styled(format!("{e} "), Style::default().fg(Color::Red))),
            None => spans.push(Span::styled("loading… ", dim)),
        }
        return spans;
    };
    spans.push(Span::styled("5h ", dim));
    match &u.block {
        Some(b) => {
            spans.push(Span::styled(fmt_usd(b.cost), val));
            spans.push(Span::styled(format!("→{} ", fmt_usd(b.projected)), dim));
            spans.push(Span::styled(format!("{}/h ", fmt_usd(b.per_hour)), val));
            spans.push(Span::styled(
                format!("{}h{:02}m  ", b.remaining_min / 60, b.remaining_min % 60),
                dim,
            ));
        }
        None => spans.push(Span::styled("idle  ", dim)),
    }
    spans.push(Span::styled("today ", dim));
    spans.push(Span::styled(fmt_usd(u.today_cost), val));
    spans.push(Span::styled(format!(" {}  ", fmt_count(u.today_tokens)), dim));
    spans.push(Span::styled("7d ", dim));
    spans.push(Span::styled(
        sparkline(&u.week),
        Style::default().fg(theme.proc_misc),
    ));
    spans.push(Span::styled(
        format!(" {}", fmt_usd(u.week.iter().sum())),
        val,
    ));
    let tail = match (&state.error, state.loading) {
        (Some(_), _) => Span::styled(" ! ", Style::default().fg(Color::Red)),
        (None, true) => Span::styled(" … ", dim),
        _ => Span::raw("  "),
    };
    spans.push(tail);
    spans
}

/// "$125", "$1.2k", "$12k".
fn fmt_usd(v: f64) -> String {
    if v >= 9_999.5 {
        format!("${:.0}k", v / 1_000.0)
    } else if v >= 999.5 {
        format!("${:.1}k", v / 1_000.0)
    } else {
        format!("${v:.0}")
    }
}

/// Token counts up to billions: "2.0B", "410M", "12k".
fn fmt_count(n: u64) -> String {
    if n >= 999_950_000 {
        format!("{:.1}B", n as f64 / 1e9)
    } else {
        super::fmt_tokens(n)
    }
}

/// One block character per value, scaled to the largest.
fn sparkline(values: &[f64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = values.iter().cloned().fold(0.0, f64::max);
    values
        .iter()
        .map(|v| {
            if max <= 0.0 {
                BARS[0]
            } else {
                BARS[((v / max) * 7.0).round() as usize]
            }
        })
        .collect()
}

/// `GPU0 RTX 2080 SUPER ■■■■■■  37%  vram ■■■■■■ 0.1/8.0G` per GPU; names are
/// dropped first when the row is too narrow.
fn gpu_line(gpus: &[GpuInfo], width: usize, theme: &Theme) -> Line<'static> {
    let grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);
    let dim = Style::default().fg(theme.graph_text);
    let build = |with_names: bool, bar_w: usize| -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for (i, g) in gpus.iter().enumerate() {
            let vram_pct = if g.mem_total_mb > 0.0 {
                g.mem_used_mb / g.mem_total_mb * 100.0
            } else {
                0.0
            };
            let name = if with_names {
                format!(" {}", g.name)
            } else {
                String::new()
            };
            spans.push(Span::styled(
                format!(" GPU{i}{name} "),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.extend(meter_bar(g.util_pct, bar_w, &grad, theme.meter_bg));
            spans.push(Span::styled(
                format!(" {:>3.0}%", g.util_pct),
                Style::default().fg(grad_at(&grad, g.util_pct)),
            ));
            spans.push(Span::styled("  vram ", dim));
            spans.extend(meter_bar(vram_pct, bar_w, &grad, theme.meter_bg));
            spans.push(Span::styled(
                format!(
                    " {:.1}/{:.1}G  ",
                    g.mem_used_mb / 1024.0,
                    g.mem_total_mb / 1024.0
                ),
                Style::default().fg(grad_at(&grad, vram_pct)),
            ));
        }
        spans
    };
    let fits = |spans: &[Span]| {
        spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>()
            <= width
    };
    let spans = [(true, 8), (false, 8), (false, 4)]
        .into_iter()
        .map(|(names, bar)| build(names, bar))
        .find(|s| fits(s))
        .unwrap_or_else(|| build(false, 2));
    Line::from(spans)
}

fn fmt_host(h: &crate::host_info::HostMetrics) -> String {
    let cpu_label = t("header.cpu");
    let mem_label = t("header.mem");
    let load_label = t("header.load");
    format!(
        "{} {}  {} {}  {} {:>5.1}  NET ↓{} ↑{}",
        cpu_label,
        fmt_cpu(h.cpu_pct, h.cores),
        mem_label,
        fmt_mem(h.mem_used_gib, h.mem_total_gib),
        load_label,
        h.load1,
        fmt_rate(h.net_rx_bps),
        fmt_rate(h.net_tx_bps)
    )
}

/// htop-style CPU where each core is 100%: "340%/1600%" (fixed width per host).
pub(crate) fn fmt_cpu(pct: f64, cores: u32) -> String {
    let cap = cores.max(1) * 100;
    let w = cap.to_string().len();
    format!("{:>w$.0}%/{cap}%", pct * cores.max(1) as f64)
}

/// "12.3/62.7G" (GiB), padded so the used part never changes width.
pub(crate) fn fmt_mem(used_gib: f64, total_gib: f64) -> String {
    let total = format!("{total_gib:.1}");
    let w = total.len();
    format!("{used_gib:>w$.1}/{total}G")
}

/// Bytes/s as a fixed 7-char field (" 1.2M/s", " 340K/s", "  12B/s") so the
/// header doesn't shift as the rate changes.
fn fmt_rate(bps: f64) -> String {
    let m = bps / 1_000_000.0;
    if bps < 999.5 {
        format!("{:>4.0}B/s", bps)
    } else if bps < 999_500.0 {
        format!("{:>4.0}K/s", bps / 1_000.0)
    } else if m < 99.95 {
        format!("{:>4.1}M/s", m)
    } else {
        format!("{:>4.0}M/s", m)
    }
}

fn fmt_agent(a: &crate::host_info::AgentAggregate) -> String {
    let mem = if a.mem_mb >= 1024 {
        format!("{:>4.1}G", a.mem_mb as f64 / 1024.0)
    } else {
        format!("{:>4}M", a.mem_mb)
    };
    let agents_label = t("header.agents");
    let ctx_label = t("header.ctx");
    format!(
        "{} Σ{} {} {:>3.0}%",
        agents_label, mem, ctx_label, a.avg_ctx_pct
    )
}

/// Decide which metrics to render given available width. Drops host first, then
/// agents, returning `(None, None)` when the header is too narrow for either.
fn pick_metrics<'a>(
    host: Option<&'a str>,
    agent: &'a str,
    width: usize,
    base: usize,
) -> (Option<&'a str>, Option<&'a str>) {
    let agent_w = agent.chars().count() + 2;
    let host_w = host.map(|h| h.chars().count() + 3).unwrap_or(0); // +3 for " ─ "

    if width >= base + host_w + agent_w {
        (host, Some(agent))
    } else if width >= base + agent_w {
        (None, Some(agent))
    } else {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rate_is_fixed_width() {
        for bps in [
            0.0, 12.0, 999.4, 999.6, 340_000.0, 999_499.0, 999_600.0, 1.2e6, 99.9e6, 1.2e9,
        ] {
            assert_eq!(super::fmt_rate(bps).chars().count(), 7, "{bps}");
        }
        assert_eq!(super::fmt_rate(1.2e6), " 1.2M/s");
    }

    #[test]
    fn cost_formats() {
        assert_eq!(super::fmt_usd(125.1), "$125");
        assert_eq!(super::fmt_usd(1162.8), "$1.2k");
        assert_eq!(super::fmt_usd(18_400.0), "$18k");
        assert_eq!(super::fmt_count(2_009_191_372), "2.0B");
        assert_eq!(super::sparkline(&[0.0, 50.0, 100.0]), "▁▅█");
        assert_eq!(super::sparkline(&[0.0, 0.0]), "▁▁");
    }

    #[test]
    fn cpu_and_mem_fixed_width() {
        assert_eq!(super::fmt_cpu(21.25, 16), " 340%/1600%");
        assert_eq!(super::fmt_cpu(100.0, 16), "1600%/1600%");
        assert_eq!(super::fmt_mem(1.04, 62.7), " 1.0/62.7G");
        assert_eq!(super::fmt_mem(12.3, 62.7), "12.3/62.7G");
    }
}
