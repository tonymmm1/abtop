use crate::app::App;
use crate::collector::remote::RemoteSession;
use crate::locale::t;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{btop_block_active, grad_at, make_gradient, truncate_str};

pub(crate) fn draw_projects_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    draw_projects_panel_active(f, app, area, theme, false);
}

pub(crate) fn draw_projects_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    let mut lines = Vec::new();
    let no_git = t("projects.no_git");
    let clean = t("projects.clean");
    let no_projects = t("projects.no_projects");
    let used_grad = make_gradient(
        theme.used_grad.start,
        theme.used_grad.mid,
        theme.used_grad.end,
    );

    // One row pair per project: local projects first (session order), then
    // projects that only run on remote hosts. Hosts are matched by dir name.
    let remote_hosts = app.remote.as_ref().and_then(|r| r.hosts.lock().ok());
    let remote: Vec<(&str, &RemoteSession)> = remote_hosts
        .iter()
        .flat_map(|hosts| hosts.iter())
        .flat_map(|h| h.sessions.iter().map(move |s| (h.name.as_str(), s)))
        .collect();
    let mut projects: Vec<Project> = Vec::new();
    for session in &app.sessions {
        if projects.iter().any(|p| p.name == session.project_name) {
            continue;
        }
        projects.push(Project {
            name: session.project_name.clone(),
            branch: session.git_branch.clone(),
            added: session.git_added,
            modified: session.git_modified,
            hosts: Vec::new(),
        });
    }
    for (host, s) in &remote {
        if !projects.iter().any(|p| p.name == s.project) {
            projects.push(Project {
                name: s.project.clone(),
                branch: s.branch.clone(),
                added: 0,
                modified: s.dirty,
                hosts: Vec::new(),
            });
        }
        bump(&mut projects, &s.project, host);
    }
    if !remote_hosts.as_ref().is_none_or(|h| h.is_empty()) {
        for s in &app.sessions {
            bump(&mut projects, &s.project_name, "local");
        }
        // `bump` appends, so move "local" ahead of the remote hosts.
        for p in &mut projects {
            p.hosts.sort_by_key(|(h, _)| *h != "local");
        }
    }

    for p in &projects {
        let mut name_spans = vec![Span::styled(
            format!(" {}", truncate_str(&p.name, 14)),
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        )];
        let hosts: Vec<String> = p.hosts.iter().map(|(h, n)| format!("{h}×{n}")).collect();
        if !hosts.is_empty() {
            name_spans.push(Span::styled(
                format!("  {}", hosts.join(" ")),
                Style::default().fg(theme.graph_text),
            ));
        }
        lines.push(Line::from(name_spans));
        let branch = if p.branch.is_empty() {
            no_git.clone()
        } else {
            p.branch.clone()
        };
        let branch_color = if p.branch.is_empty() {
            theme.inactive_fg
        } else {
            theme.main_fg
        };
        let mut branch_spans = vec![
            Span::styled("   ", Style::default()),
            Span::styled(branch, Style::default().fg(branch_color)),
        ];
        if p.added > 0 || p.modified > 0 {
            branch_spans.push(Span::styled(" ", Style::default()));
            if p.added > 0 {
                branch_spans.push(Span::styled(
                    format!("+{}", p.added),
                    Style::default().fg(theme.proc_misc),
                ));
            }
            if p.modified > 0 {
                if p.added > 0 {
                    branch_spans.push(Span::styled(" ", Style::default()));
                }
                branch_spans.push(Span::styled(
                    format!("~{}", p.modified),
                    Style::default().fg(grad_at(&used_grad, 70.0)),
                ));
            }
        } else {
            branch_spans.push(Span::styled(
                format!(" {}", clean),
                Style::default().fg(theme.proc_misc),
            ));
        }
        lines.push(Line::from(branch_spans));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {}", no_projects),
            Style::default().fg(theme.inactive_fg),
        )));
    }

    let block = btop_block_active("projects", "⁴", theme.mem_box, theme, active);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

struct Project {
    name: String,
    branch: String,
    added: u32,
    modified: u32,
    /// (host, session count); empty when no remote hosts are configured.
    hosts: Vec<(String, usize)>,
}

/// Count one more session of `project` on `host`.
fn bump(projects: &mut [Project], project: &str, host: &str) {
    let Some(p) = projects.iter_mut().find(|p| p.name == project) else {
        return;
    };
    match p.hosts.iter_mut().find(|(h, _)| h == host) {
        Some((_, n)) => *n += 1,
        None => p.hosts.push((host.to_string(), 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::remote::{Handle, HostStatus};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn projects_show_hosts_and_remote_only_projects() {
        let mut app = App::new_with_config(Theme::default(), &[], Default::default());
        crate::demo::populate_demo(&mut app);
        let local = app.sessions[0].project_name.clone();
        app.sessions.truncate(1);
        let session = |project: &str| RemoteSession {
            agent: "claude".into(),
            pid: 1,
            working: Some(true),
            project: project.into(),
            cwd: format!("/srv/{project}"),
            branch: "main".into(),
            dirty: 2,
        };
        app.remote = Some(Handle {
            hosts: std::sync::Arc::new(std::sync::Mutex::new(vec![HostStatus {
                name: "devbox".into(),
                sessions: vec![session(&local), session(&local), session("api")],
                ..Default::default()
            }])),
            ccusage: Vec::new(),
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|f| draw_projects_panel(f, &app, f.area(), &Theme::default()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..8)
            .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        assert!(rows[1].contains("local×1 devbox×2"), "{rows:#?}");
        assert!(rows[3].contains("api") && rows[3].contains("devbox×1"), "{rows:#?}");
        assert!(rows[4].contains("main ~2"), "{rows:#?}");
    }
}
