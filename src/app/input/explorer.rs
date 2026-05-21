use crossterm::event::KeyCode;

use crate::app::{App, Mode};
use crate::pane::state::PaneMode;

impl App {
    pub(crate) fn handle_global_explorer_key(&mut self, key: crate::input::TerminalKey) {
        if self.state.is_prefix_key(key) {
            self.state.mode = Mode::Prefix;
            return;
        }

        let key_event = key.as_key_event();
        let global_explorer_rect_height = self.state.view.global_explorer_rect.height;
        let visible_height = global_explorer_rect_height.saturating_sub(5).max(1) as usize;

        let explorer = &mut self.state.global_explorer;

        if !explorer.files.is_empty() && explorer.selected_index >= explorer.files.len() {
            explorer.selected_index = explorer.files.len() - 1;
        }

        if explorer.search_mode {
            match key_event.code {
                KeyCode::Esc | KeyCode::Enter => {
                    explorer.search_mode = false;
                }
                KeyCode::Backspace => {
                    explorer.search_query.pop();
                    let favorites = crate::config::load_favorites(&explorer.cwd);
                    explorer.files = crate::app::state::build_explorer_entries(
                        &explorer.cwd,
                        explorer.is_tree_view,
                        &explorer.expanded_dirs,
                        &explorer.search_query,
                        explorer.filter_md,
                        explorer.sort_by_mtime,
                        &favorites,
                    );
                    explorer.selected_index = 0;
                    explorer.scroll = 0;
                }
                KeyCode::Char(c) => {
                    explorer.search_query.push(c);
                    let favorites = crate::config::load_favorites(&explorer.cwd);
                    explorer.files = crate::app::state::build_explorer_entries(
                        &explorer.cwd,
                        explorer.is_tree_view,
                        &explorer.expanded_dirs,
                        &explorer.search_query,
                        explorer.filter_md,
                        explorer.sort_by_mtime,
                        &favorites,
                    );
                    explorer.selected_index = 0;
                    explorer.scroll = 0;
                }
                _ => {}
            }
        } else {
            match key_event.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if !explorer.files.is_empty() {
                        explorer.selected_index = explorer.selected_index.saturating_sub(1);
                        explorer.scroll = crate::app::state::calculate_scroll(
                            explorer.selected_index,
                            explorer.scroll,
                            visible_height,
                            explorer.files.len(),
                        );
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !explorer.files.is_empty() {
                        explorer.selected_index =
                            (explorer.selected_index + 1).min(explorer.files.len() - 1);
                        explorer.scroll = crate::app::state::calculate_scroll(
                            explorer.selected_index,
                            explorer.scroll,
                            visible_height,
                            explorer.files.len(),
                        );
                    }
                }
                KeyCode::PageUp => {
                    if !explorer.files.is_empty() {
                        explorer.selected_index =
                            explorer.selected_index.saturating_sub(visible_height);
                        explorer.scroll = crate::app::state::calculate_scroll(
                            explorer.selected_index,
                            explorer.scroll,
                            visible_height,
                            explorer.files.len(),
                        );
                    }
                }
                KeyCode::PageDown => {
                    if !explorer.files.is_empty() {
                        explorer.selected_index = (explorer.selected_index + visible_height)
                            .min(explorer.files.len() - 1);
                        explorer.scroll = crate::app::state::calculate_scroll(
                            explorer.selected_index,
                            explorer.scroll,
                            visible_height,
                            explorer.files.len(),
                        );
                    }
                }
                KeyCode::Char('/') => {
                    explorer.search_mode = true;
                    explorer.search_query = String::new();
                }
                KeyCode::Char('t') => {
                    explorer.is_tree_view = !explorer.is_tree_view;
                    let favorites = crate::config::load_favorites(&explorer.cwd);
                    explorer.files = crate::app::state::build_explorer_entries(
                        &explorer.cwd,
                        explorer.is_tree_view,
                        &explorer.expanded_dirs,
                        &explorer.search_query,
                        explorer.filter_md,
                        explorer.sort_by_mtime,
                        &favorites,
                    );
                    explorer.selected_index = 0;
                    explorer.scroll = 0;
                }
                KeyCode::Char('f') => {
                    explorer.filter_md = !explorer.filter_md;
                    let favorites = crate::config::load_favorites(&explorer.cwd);
                    explorer.files = crate::app::state::build_explorer_entries(
                        &explorer.cwd,
                        explorer.is_tree_view,
                        &explorer.expanded_dirs,
                        &explorer.search_query,
                        explorer.filter_md,
                        explorer.sort_by_mtime,
                        &favorites,
                    );
                    explorer.selected_index = 0;
                    explorer.scroll = 0;
                }
                KeyCode::Char('s') => {
                    explorer.sort_by_mtime = !explorer.sort_by_mtime;
                    let favorites = crate::config::load_favorites(&explorer.cwd);
                    explorer.files = crate::app::state::build_explorer_entries(
                        &explorer.cwd,
                        explorer.is_tree_view,
                        &explorer.expanded_dirs,
                        &explorer.search_query,
                        explorer.filter_md,
                        explorer.sort_by_mtime,
                        &favorites,
                    );
                    explorer.selected_index = 0;
                    explorer.scroll = 0;
                }
                KeyCode::Char('a') => {
                    if let Some(entry) = explorer.files.get(explorer.selected_index) {
                        let path = entry.path.clone();
                        let is_fav = entry.is_favorite;
                        crate::config::save_favorite(&explorer.cwd, &path, !is_fav);
                        let favorites = crate::config::load_favorites(&explorer.cwd);
                        explorer.files = crate::app::state::build_explorer_entries(
                            &explorer.cwd,
                            explorer.is_tree_view,
                            &explorer.expanded_dirs,
                            &explorer.search_query,
                            explorer.filter_md,
                            explorer.sort_by_mtime,
                            &favorites,
                        );
                        if let Some(pos) = explorer.files.iter().position(|f| f.path == path) {
                            explorer.selected_index = pos;
                        }
                        explorer.scroll = crate::app::state::calculate_scroll(
                            explorer.selected_index,
                            explorer.scroll,
                            visible_height,
                            explorer.files.len(),
                        );
                    }
                }
                KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                    self.state.global_explorer_open_selected();
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    let explorer = &mut self.state.global_explorer;
                    if let Some(entry) = explorer.files.get(explorer.selected_index) {
                        if entry.is_dir && entry.is_expanded {
                            explorer.expanded_dirs.remove(&entry.path);
                            let favorites = crate::config::load_favorites(&explorer.cwd);
                            explorer.files = crate::app::state::build_explorer_entries(
                                &explorer.cwd,
                                explorer.is_tree_view,
                                &explorer.expanded_dirs,
                                &explorer.search_query,
                                explorer.filter_md,
                                explorer.sort_by_mtime,
                                &favorites,
                            );
                        } else if let Some(parent) = entry.path.parent() {
                            if parent.starts_with(explorer.cwd.as_path())
                                && parent != explorer.cwd.as_path()
                            {
                                if let Some(pos) =
                                    explorer.files.iter().position(|f| f.path == parent)
                                {
                                    explorer.selected_index = pos;
                                    explorer.scroll = crate::app::state::calculate_scroll(
                                        explorer.selected_index,
                                        explorer.scroll,
                                        visible_height,
                                        explorer.files.len(),
                                    );
                                }
                            }
                        }
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.state.global_explorer.open = false;
                    self.state.mode = Mode::Terminal;
                }
                _ => {}
            }
        }
    }
}

impl crate::app::state::AppState {
    pub(crate) fn global_explorer_open_selected(&mut self) {
        let explorer = &mut self.global_explorer;
        if let Some(entry) = explorer.files.get(explorer.selected_index) {
            if entry.is_dir {
                let path = entry.path.clone();
                if explorer.expanded_dirs.contains(&path) {
                    explorer.expanded_dirs.remove(&path);
                } else {
                    explorer.expanded_dirs.insert(path);
                }
                let favorites = crate::config::load_favorites(&explorer.cwd);
                explorer.files = crate::app::state::build_explorer_entries(
                    &explorer.cwd,
                    explorer.is_tree_view,
                    &explorer.expanded_dirs,
                    &explorer.search_query,
                    explorer.filter_md,
                    explorer.sort_by_mtime,
                    &favorites,
                );
            } else {
                let path = entry.path.clone();
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let lines = content.lines().map(String::from).collect();

                    // Open in the focused pane
                    if let Some(ws_idx) = self.active {
                        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
                            if let Some(pane_id) = ws.focused_pane_id() {
                                if let Some(pane) = ws.pane_state_mut(pane_id) {
                                    pane.mode = PaneMode::MarkdownViewer {
                                        path,
                                        content,
                                        scroll: 0,
                                        lines,
                                    };
                                }
                            }
                        }
                    }

                    // Close the global explorer
                    self.global_explorer.open = false;
                    self.mode = Mode::Terminal;
                }
            }
        }
    }

    pub(crate) fn handle_global_explorer_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
    ) -> bool {
        let rect = self.view.global_explorer_rect;
        let in_explorer = mouse.column >= rect.x
            && mouse.column < rect.x + rect.width
            && mouse.row >= rect.y
            && mouse.row < rect.y + rect.height;

        if !in_explorer {
            if matches!(mouse.kind, crossterm::event::MouseEventKind::Down(_)) {
                self.global_explorer.open = false;
                self.mode = Mode::Terminal;
                return false; // let the click pass through to the pane
            }
            return false;
        }

        let visible_height = rect.height.saturating_sub(5).max(1) as usize;
        let list_y = rect.y + 4;
        let list_bottom = list_y + visible_height as u16;

        match mouse.kind {
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                if mouse.row >= list_y && mouse.row < list_bottom {
                    let clicked_index = (mouse.row - list_y) as usize + self.global_explorer.scroll;
                    if clicked_index < self.global_explorer.files.len() {
                        self.global_explorer.selected_index = clicked_index;
                        self.global_explorer_open_selected();
                    }
                }
            }
            crossterm::event::MouseEventKind::ScrollUp => {
                let explorer = &mut self.global_explorer;
                if !explorer.files.is_empty() {
                    explorer.selected_index = explorer.selected_index.saturating_sub(1);
                    explorer.scroll = crate::app::state::calculate_scroll(
                        explorer.selected_index,
                        explorer.scroll,
                        visible_height,
                        explorer.files.len(),
                    );
                }
            }
            crossterm::event::MouseEventKind::ScrollDown => {
                let explorer = &mut self.global_explorer;
                if !explorer.files.is_empty() {
                    explorer.selected_index =
                        (explorer.selected_index + 1).min(explorer.files.len() - 1);
                    explorer.scroll = crate::app::state::calculate_scroll(
                        explorer.selected_index,
                        explorer.scroll,
                        visible_height,
                        explorer.files.len(),
                    );
                }
            }
            _ => {}
        }
        true
    }
}
