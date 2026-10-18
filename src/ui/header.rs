use crate::app::App;
use crate::locale::t;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub(crate) fn draw_header(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let session_count = app.sessions.len();
    let active = app.agent_aggregate.active_count;

    let now = chrono::Local::now().format("%H:%M").to_string();
    let version = env!("CARGO_PKG_VERSION");

    let title = format!(" abtop v{version} ");
    let right = format!(" {now}  {active}↑ {session_count}● ");

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
    spans.push(Span::styled(
        format!(" {now}  "),
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
    fn cpu_and_mem_fixed_width() {
        assert_eq!(super::fmt_cpu(21.25, 16), " 340%/1600%");
        assert_eq!(super::fmt_cpu(100.0, 16), "1600%/1600%");
        assert_eq!(super::fmt_mem(1.04, 62.7), " 1.0/62.7G");
        assert_eq!(super::fmt_mem(12.3, 62.7), "12.3/62.7G");
    }
}
