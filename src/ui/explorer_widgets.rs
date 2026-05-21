use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::app::AppState;
use crate::pane::state::FileEntry;

fn file_icon(is_dir: bool, name: &str) -> &'static str {
    if is_dir {
        "📁"
    } else if name.ends_with(".md") {
        "📝"
    } else if name.ends_with(".toml") || name == "justfile" || name.starts_with('.') {
        "⚙️"
    } else if name.ends_with(".rs") {
        "🦀"
    } else {
        "📄"
    }
}

// Allow clippy::too_many_arguments since we need to pass the individual state fields of the explorer mode for rendering
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_explorer(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    cwd: &std::path::Path,
    files: &[FileEntry],
    selected_index: usize,
    scroll: usize,
    search_query: &str,
    search_mode: bool,
    is_tree_view: bool,
    filter_md: bool,
    sort_by_mtime: bool,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // CWD Title
            Constraint::Length(1), // Search Input
            Constraint::Min(3),    // File List
            Constraint::Length(1), // Help footer
        ])
        .split(area);

    // 1. CWD Title
    let view_mode_str = if is_tree_view { " [Tree]" } else { " [List]" };
    let filter_str = if filter_md { " [MD Only]" } else { "" };
    let sort_str = if sort_by_mtime {
        " [Sorted: mtime]"
    } else {
        ""
    };

    let title_line = Line::from(vec![
        Span::styled(
            " HERDR EXPLORER: ",
            Style::default()
                .fg(Color::Black)
                .bg(app.palette.accent)
                .bold(),
        ),
        Span::styled(
            format!(" {} ", cwd.display()),
            Style::default().fg(app.palette.accent).bold(),
        ),
        Span::styled(
            format!("{}{}{} ", view_mode_str, filter_str, sort_str),
            Style::default().fg(app.palette.accent).italic(),
        ),
    ]);
    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    // 2. Search Input
    let search_label = Span::styled(
        " 🔍 Search: ",
        Style::default().fg(app.palette.accent).bold(),
    );
    let search_val = if search_query.is_empty() {
        if search_mode {
            Span::styled(
                "type to filter...",
                Style::default().fg(app.palette.overlay0).italic(),
            )
        } else {
            Span::styled(
                "press / to search",
                Style::default().fg(app.palette.overlay0).italic(),
            )
        }
    } else {
        Span::styled(search_query, Style::default().fg(app.palette.text).bold())
    };
    let search_bg = if search_mode {
        Style::default().bg(app.palette.surface0)
    } else {
        Style::default()
    };
    let search_line = Line::from(vec![search_label, search_val]).patch_style(search_bg);
    frame.render_widget(Paragraph::new(search_line), chunks[1]);

    let selected_index = if files.is_empty() {
        0
    } else {
        selected_index.min(files.len() - 1)
    };

    // 3. File List
    let items: Vec<ListItem> = files
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let icon = file_icon(entry.is_dir, &entry.name);
            let name = &entry.name;

            let is_selected = i == selected_index;
            let (prefix, prefix_style) = if is_selected {
                (" ➜ ", Style::default().fg(app.palette.accent).bold())
            } else {
                ("   ", Style::default())
            };

            let name_style = if entry.is_dir {
                if is_selected {
                    Style::default().fg(app.palette.accent).bold()
                } else {
                    Style::default().fg(app.palette.accent)
                }
            } else if is_selected {
                Style::default().fg(app.palette.text).bold()
            } else {
                Style::default().fg(app.palette.text)
            };

            let bg_style = if is_selected {
                Style::default().bg(app.palette.surface0)
            } else {
                Style::default()
            };

            let indent = if is_tree_view {
                "  ".repeat(entry.depth)
            } else {
                "".to_string()
            };

            let dir_indicator = if entry.is_dir {
                if entry.is_expanded {
                    "▼ "
                } else {
                    "▶ "
                }
            } else {
                "  "
            };

            let fav_star = if entry.is_favorite { "⭐ " } else { "" };

            let line = Line::from(vec![
                Span::styled(prefix, prefix_style),
                Span::styled(indent, Style::default()),
                Span::styled(dir_indicator, Style::default().fg(app.palette.overlay0)),
                Span::styled(format!("{}  ", icon), Style::default()),
                Span::styled(fav_star, Style::default()),
                Span::styled(name, name_style),
            ])
            .patch_style(bg_style);

            ListItem::new(line)
        })
        .collect();

    let mut list_state = ListState::default().with_selected(Some(selected_index));
    *list_state.offset_mut() = scroll;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.palette.overlay0));

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(app.palette.surface0));

    frame.render_stateful_widget(list, chunks[2], &mut list_state);

    // 4. Help Footer
    let help_line = Line::from(vec![
        Span::styled(" [/]", Style::default().fg(app.palette.accent).bold()),
        Span::styled(" Search ", Style::default().fg(app.palette.overlay0)),
        Span::styled(" [t]", Style::default().fg(app.palette.accent).bold()),
        Span::styled(" View ", Style::default().fg(app.palette.overlay0)),
        Span::styled(" [f]", Style::default().fg(app.palette.accent).bold()),
        Span::styled(" MD ", Style::default().fg(app.palette.overlay0)),
        Span::styled(" [s]", Style::default().fg(app.palette.accent).bold()),
        Span::styled(" Sort ", Style::default().fg(app.palette.overlay0)),
        Span::styled(" [a]", Style::default().fg(app.palette.accent).bold()),
        Span::styled(" Fav ", Style::default().fg(app.palette.overlay0)),
        Span::styled(" [Enter]", Style::default().fg(app.palette.accent).bold()),
        Span::styled(" Open/Tgl ", Style::default().fg(app.palette.overlay0)),
        Span::styled(" [Esc]", Style::default().fg(app.palette.accent).bold()),
        Span::styled(" Close", Style::default().fg(app.palette.overlay0)),
    ]);
    frame.render_widget(
        Paragraph::new(help_line).bg(app.palette.surface_dim),
        chunks[3],
    );
}

pub(crate) fn render_viewer(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    path: &std::path::Path,
    lines: &[String],
    scroll: usize,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Title
            Constraint::Min(3),    // Content
            Constraint::Length(1), // Footer
        ])
        .split(area);

    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Unknown".to_string());

    // 1. Title
    let title_line = Line::from(vec![
        Span::styled(
            " HERDR VIEWER: ",
            Style::default()
                .fg(Color::Black)
                .bg(app.palette.accent)
                .bold(),
        ),
        Span::styled(
            format!(" {} ", filename),
            Style::default().fg(app.palette.accent).bold(),
        ),
    ]);
    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    // 2. Content
    let content_height = chunks[1].height.saturating_sub(2) as usize; // account for border
    let visible_lines: Vec<Line> = lines
        .iter()
        .skip(scroll)
        .take(content_height)
        .map(|line| parse_markdown_line(line, app))
        .collect();

    let visible_count = visible_lines.len();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.palette.overlay0));

    let paragraph = Paragraph::new(visible_lines).block(block);
    frame.render_widget(paragraph, chunks[1]);

    // 3. Footer
    let progress = if lines.is_empty() {
        0
    } else {
        let current_pos = scroll + visible_count;
        ((current_pos as f64 / lines.len() as f64) * 100.0) as usize
    };

    let help_line = Line::from(vec![
        Span::styled(" [↑/↓/j/k]", Style::default().fg(app.palette.accent).bold()),
        Span::styled(" Scroll  ", Style::default().fg(app.palette.overlay0)),
        Span::styled("[w]", Style::default().fg(app.palette.accent).bold()),
        Span::styled(" Browser  ", Style::default().fg(app.palette.overlay0)),
        Span::styled("[Esc/q]", Style::default().fg(app.palette.accent).bold()),
        Span::styled(" Back  ", Style::default().fg(app.palette.overlay0)),
        Span::styled(
            format!(" {:>3}%", progress),
            Style::default().fg(app.palette.accent).bold(),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(help_line).bg(app.palette.surface_dim),
        chunks[2],
    );
}

fn parse_markdown_line(line: &str, app: &AppState) -> Line<'static> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("# ") {
        Line::from(vec![
            Span::styled("█ ", Style::default().fg(app.palette.accent)),
            Span::styled(
                rest.to_string(),
                Style::default().fg(app.palette.accent).bold().underlined(),
            ),
        ])
    } else if let Some(rest) = trimmed.strip_prefix("## ") {
        Line::from(vec![
            Span::styled("■ ", Style::default().fg(app.palette.accent)),
            Span::styled(rest.to_string(), Style::default().bold()),
        ])
    } else if let Some(rest) = trimmed.strip_prefix("### ") {
        Line::from(vec![
            Span::styled("○ ", Style::default().fg(app.palette.accent)),
            Span::styled(rest.to_string(), Style::default().bold()),
        ])
    } else if let Some(rest) = trimmed.strip_prefix("> ") {
        Line::from(vec![
            Span::styled(" ▍ ", Style::default().fg(app.palette.accent)),
            Span::styled(
                rest.to_string(),
                Style::default().fg(app.palette.overlay0).italic(),
            ),
        ])
    } else if let Some(rest) = trimmed.strip_prefix("- ") {
        Line::from(vec![
            Span::styled(" • ", Style::default().fg(app.palette.accent).bold()),
            Span::styled(rest.to_string(), Style::default()),
        ])
    } else if let Some(rest) = trimmed.strip_prefix("* ") {
        Line::from(vec![
            Span::styled(" • ", Style::default().fg(app.palette.accent).bold()),
            Span::styled(rest.to_string(), Style::default()),
        ])
    } else if trimmed == "---" || trimmed == "***" {
        Line::from(vec![Span::styled(
            "─".repeat(40),
            Style::default().fg(app.palette.overlay0),
        )])
    } else {
        let mut spans = Vec::new();
        let mut current = String::new();
        let mut in_code = false;

        for ch in line.chars() {
            if ch == '`' {
                if !current.is_empty() {
                    if in_code {
                        spans.push(Span::styled(
                            current.clone(),
                            Style::default().fg(Color::Yellow),
                        ));
                    } else {
                        spans.push(Span::styled(current.clone(), Style::default()));
                    }
                    current.clear();
                }
                in_code = !in_code;
            } else {
                current.push(ch);
            }
        }
        if !current.is_empty() {
            if in_code {
                spans.push(Span::styled(current, Style::default().fg(Color::Yellow)));
            } else {
                spans.push(Span::styled(current, Style::default()));
            }
        }

        if spans.is_empty() {
            Line::from(vec![Span::raw("")])
        } else {
            Line::from(spans)
        }
    }
}
