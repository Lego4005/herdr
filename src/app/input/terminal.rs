use bytes::Bytes;
use crossterm::event::KeyCode;
use tracing::{debug, warn};

use crate::{
    app::{App, Mode},
    input::TerminalKey,
};

struct PreparedPaneInput {
    ws_idx: usize,
    pane_id: crate::layout::PaneId,
    bytes: Bytes,
}

fn is_modifier_only_key(code: &KeyCode) -> bool {
    matches!(code, KeyCode::Modifier(_))
}

impl App {
    pub(crate) fn handle_terminal_key_headless(&mut self, key: TerminalKey) {
        let Some(input) = self.prepare_terminal_key_forward(key) else {
            return;
        };
        if let Some(runtime) = self.lookup_runtime_sender(input.ws_idx, input.pane_id) {
            let _ = runtime.try_send_bytes(input.bytes);
        }
    }

    fn prepare_terminal_key_forward(&mut self, key: TerminalKey) -> Option<PreparedPaneInput> {
        self.state.clear_selection();
        self.selection_autoscroll_deadline = None;
        self.state.update_dismissed = true;

        let key_event = key.as_key_event();

        if let Some(action) = super::terminal_direct_navigation_action(&self.state, key) {
            debug!(
                code = ?key_event.code,
                modifiers = ?key_event.modifiers,
                kind = ?key_event.kind,
                action = ?action,
                "intercepted terminal direct keybinding before forwarding to pane"
            );
            if action == super::navigate::NavigateAction::EditScrollback {
                self.launch_focused_scrollback_editor();
            } else {
                super::navigate::execute_navigate_action_in_context(
                    &mut self.state,
                    action,
                    super::navigate::ActionContext::Direct,
                );
            }
            return None;
        }

        if let Some(binding) = super::navigate::command_for_key(
            &self.state,
            key,
            super::navigate::BindingDispatch::Direct,
        ) {
            debug!(
                code = ?key_event.code,
                modifiers = ?key_event.modifiers,
                kind = ?key_event.kind,
                command = %binding.label,
                "intercepted terminal direct custom command before forwarding to pane"
            );
            self.launch_custom_command(binding, super::navigate::ActionContext::Direct);
            return None;
        }

        if self.state.is_prefix_key(key) {
            self.state.mode = Mode::Prefix;
            return None;
        }

        if is_modifier_only_key(&key_event.code) {
            debug!(
                code = ?key_event.code,
                modifiers = ?key_event.modifiers,
                kind = ?key_event.kind,
                "dropping modifier-only terminal key event instead of forwarding it to pane"
            );
            return None;
        }

        let ws_idx = self.state.active?;
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_id = ws.focused_pane_id()?;
        let rt = self.state.runtime_for_pane_in_workspace(ws_idx, pane_id)?;

        // Intercept plain PageUp/PageDown presses for pane scrollback when the
        // focused pane doesn't handle its own scrolling (e.g., a plain shell
        // with mouse off). Modified page keys are pane shortcuts, and release
        // events should not produce a second host-scroll action.
        // Only intercept when we know the pane state; if input_state is unknown,
        // fail-open and forward the key to the pane.
        if matches!(key_event.code, KeyCode::PageUp | KeyCode::PageDown)
            && key_event.modifiers.is_empty()
        {
            if let Some(input_state) = rt.input_state() {
                if !input_state.alternate_screen && !input_state.mouse_reporting_enabled() {
                    if key_event.kind == crossterm::event::KeyEventKind::Release {
                        return None;
                    }
                    if matches!(
                        key_event.kind,
                        crossterm::event::KeyEventKind::Press
                            | crossterm::event::KeyEventKind::Repeat
                    ) {
                        let lines = self
                            .state
                            .pane_info_by_id(pane_id)
                            .map(|info| info.inner_rect.height as usize)
                            .unwrap_or(10)
                            .max(1);
                        if key_event.code == KeyCode::PageUp {
                            self.state.scroll_pane_up(pane_id, lines);
                        } else {
                            self.state.scroll_pane_down(pane_id, lines);
                        }
                        debug!(
                            code = ?key_event.code,
                            lines,
                            "intercepted page key for pane scrollback"
                        );
                        return None;
                    }
                }
            }
        }

        rt.scroll_reset();
        let protocol = rt.keyboard_protocol();
        let bytes = rt.encode_terminal_key(key);

        if matches!(key_event.code, KeyCode::Esc)
            || key_event
                .modifiers
                .contains(crossterm::event::KeyModifiers::ALT)
        {
            debug!(
                code = ?key_event.code,
                modifiers = ?key_event.modifiers,
                kind = ?key_event.kind,
                protocol = ?protocol,
                encoded = ?bytes,
                "forwarding potentially-ambiguous terminal key to pane"
            );
        }

        if bytes.is_empty() {
            if key.kind != crossterm::event::KeyEventKind::Release
                && !matches!(
                    key.code,
                    KeyCode::CapsLock
                        | KeyCode::ScrollLock
                        | KeyCode::NumLock
                        | KeyCode::PrintScreen
                        | KeyCode::Pause
                        | KeyCode::Menu
                        | KeyCode::KeypadBegin
                        | KeyCode::Media(_)
                        | KeyCode::Modifier(_)
                )
            {
                warn!(code = ?key_event.code, mods = ?key_event.modifiers, state = ?key_event.state, "key produced empty encoding");
            }
            return None;
        }

        Some(PreparedPaneInput {
            ws_idx,
            pane_id,
            bytes: Bytes::from(bytes),
        })
    }

    pub(super) async fn handle_terminal_key(&mut self, key: TerminalKey) {
        let ws_idx = match self.state.active {
            Some(idx) => idx,
            None => return,
        };

        // Obtain required read-only data first to avoid borrow conflicts
        let (pane_id, pane_height, identity_cwd) = {
            let ws = match self.state.workspaces.get(ws_idx) {
                Some(ws) => ws,
                None => return,
            };
            let pane_id = match ws.focused_pane_id() {
                Some(pid) => pid,
                None => return,
            };
            let pane_height = self
                .state
                .pane_info_by_id(pane_id)
                .map(|info| info.inner_rect.height)
                .unwrap_or(24) as usize;
            let identity_cwd = ws.identity_cwd.clone();
            (pane_id, pane_height, identity_cwd)
        };

        let ws = match self.state.workspaces.get_mut(ws_idx) {
            Some(ws) => ws,
            None => return,
        };
        let pane = match ws.pane_state_mut(pane_id) {
            Some(p) => p,
            None => return,
        };

        let is_custom_mode = matches!(
            pane.mode,
            crate::pane::state::PaneMode::FileExplorer { .. }
                | crate::pane::state::PaneMode::MarkdownViewer { .. }
        );

        if is_custom_mode {
            Self::handle_custom_mode_key_internal(key, pane, pane_height, &identity_cwd);
            return;
        }

        let Some(input) = self.prepare_terminal_key_forward(key) else {
            return;
        };
        if let Some(runtime) = self.lookup_runtime_sender(input.ws_idx, input.pane_id) {
            let _ = runtime.send_bytes(input.bytes).await;
        }
    }

    fn handle_custom_mode_key_internal(
        key: TerminalKey,
        pane: &mut crate::pane::state::PaneState,
        pane_height: usize,
        identity_cwd: &std::path::Path,
    ) {
        let key_event = key.as_key_event();
        match &mut pane.mode {
            crate::pane::state::PaneMode::FileExplorer {
                cwd,
                selected_index,
                files,
                scroll,
                search_query,
                search_mode,
                is_tree_view,
                expanded_dirs,
                filter_md,
                sort_by_mtime,
            } => {
                let visible_height = pane_height.saturating_sub(5).max(1);

                if !files.is_empty() && *selected_index >= files.len() {
                    *selected_index = files.len() - 1;
                }

                if *search_mode {
                    match key_event.code {
                        KeyCode::Esc | KeyCode::Enter => {
                            *search_mode = false;
                        }
                        KeyCode::Backspace => {
                            search_query.pop();
                            let favorites = crate::config::load_favorites(identity_cwd);
                            *files = crate::app::state::build_explorer_entries(
                                cwd,
                                *is_tree_view,
                                expanded_dirs,
                                search_query,
                                *filter_md,
                                *sort_by_mtime,
                                &favorites,
                            );
                            *selected_index = 0;
                            *scroll = 0;
                        }
                        KeyCode::Char(c) => {
                            search_query.push(c);
                            let favorites = crate::config::load_favorites(identity_cwd);
                            *files = crate::app::state::build_explorer_entries(
                                cwd,
                                *is_tree_view,
                                expanded_dirs,
                                search_query,
                                *filter_md,
                                *sort_by_mtime,
                                &favorites,
                            );
                            *selected_index = 0;
                            *scroll = 0;
                        }
                        _ => {}
                    }
                } else {
                    match key_event.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            if !files.is_empty() {
                                *selected_index = selected_index.saturating_sub(1);
                                *scroll = crate::app::state::calculate_scroll(
                                    *selected_index,
                                    *scroll,
                                    visible_height,
                                    files.len(),
                                );
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if !files.is_empty() {
                                *selected_index = (*selected_index + 1).min(files.len() - 1);
                                *scroll = crate::app::state::calculate_scroll(
                                    *selected_index,
                                    *scroll,
                                    visible_height,
                                    files.len(),
                                );
                            }
                        }
                        KeyCode::PageUp => {
                            if !files.is_empty() {
                                *selected_index = selected_index.saturating_sub(visible_height);
                                *scroll = crate::app::state::calculate_scroll(
                                    *selected_index,
                                    *scroll,
                                    visible_height,
                                    files.len(),
                                );
                            }
                        }
                        KeyCode::PageDown => {
                            if !files.is_empty() {
                                *selected_index =
                                    (*selected_index + visible_height).min(files.len() - 1);
                                *scroll = crate::app::state::calculate_scroll(
                                    *selected_index,
                                    *scroll,
                                    visible_height,
                                    files.len(),
                                );
                            }
                        }
                        KeyCode::Char('/') => {
                            *search_mode = true;
                            *search_query = String::new();
                        }
                        KeyCode::Char('t') => {
                            *is_tree_view = !*is_tree_view;
                            let favorites = crate::config::load_favorites(identity_cwd);
                            *files = crate::app::state::build_explorer_entries(
                                cwd,
                                *is_tree_view,
                                expanded_dirs,
                                search_query,
                                *filter_md,
                                *sort_by_mtime,
                                &favorites,
                            );
                            *selected_index = 0;
                            *scroll = 0;
                        }
                        KeyCode::Char('f') => {
                            *filter_md = !*filter_md;
                            let favorites = crate::config::load_favorites(identity_cwd);
                            *files = crate::app::state::build_explorer_entries(
                                cwd,
                                *is_tree_view,
                                expanded_dirs,
                                search_query,
                                *filter_md,
                                *sort_by_mtime,
                                &favorites,
                            );
                            *selected_index = 0;
                            *scroll = 0;
                        }
                        KeyCode::Char('s') => {
                            *sort_by_mtime = !*sort_by_mtime;
                            let favorites = crate::config::load_favorites(identity_cwd);
                            *files = crate::app::state::build_explorer_entries(
                                cwd,
                                *is_tree_view,
                                expanded_dirs,
                                search_query,
                                *filter_md,
                                *sort_by_mtime,
                                &favorites,
                            );
                            *selected_index = 0;
                            *scroll = 0;
                        }
                        KeyCode::Char('a') => {
                            if let Some(entry) = files.get(*selected_index) {
                                let path = entry.path.clone();
                                let is_fav = entry.is_favorite;
                                crate::config::save_favorite(identity_cwd, &path, !is_fav);
                                let favorites = crate::config::load_favorites(identity_cwd);
                                *files = crate::app::state::build_explorer_entries(
                                    cwd,
                                    *is_tree_view,
                                    expanded_dirs,
                                    search_query,
                                    *filter_md,
                                    *sort_by_mtime,
                                    &favorites,
                                );
                                if let Some(pos) = files.iter().position(|f| f.path == path) {
                                    *selected_index = pos;
                                }
                                *scroll = crate::app::state::calculate_scroll(
                                    *selected_index,
                                    *scroll,
                                    visible_height,
                                    files.len(),
                                );
                            }
                        }
                        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                            if let Some(entry) = files.get(*selected_index) {
                                if entry.is_dir {
                                    let path = entry.path.clone();
                                    if expanded_dirs.contains(&path) {
                                        expanded_dirs.remove(&path);
                                    } else {
                                        expanded_dirs.insert(path);
                                    }
                                    let favorites = crate::config::load_favorites(identity_cwd);
                                    *files = crate::app::state::build_explorer_entries(
                                        cwd,
                                        *is_tree_view,
                                        expanded_dirs,
                                        search_query,
                                        *filter_md,
                                        *sort_by_mtime,
                                        &favorites,
                                    );
                                } else {
                                    let path = entry.path.clone();
                                    if let Ok(content) = std::fs::read_to_string(&path) {
                                        let lines = content.lines().map(String::from).collect();
                                        pane.mode = crate::pane::state::PaneMode::MarkdownViewer {
                                            path,
                                            content,
                                            scroll: 0,
                                            lines,
                                        };
                                    }
                                }
                            }
                        }
                        KeyCode::Char('h') | KeyCode::Left => {
                            if let Some(entry) = files.get(*selected_index) {
                                if entry.is_dir && entry.is_expanded {
                                    expanded_dirs.remove(&entry.path);
                                    let favorites = crate::config::load_favorites(identity_cwd);
                                    *files = crate::app::state::build_explorer_entries(
                                        cwd,
                                        *is_tree_view,
                                        expanded_dirs,
                                        search_query,
                                        *filter_md,
                                        *sort_by_mtime,
                                        &favorites,
                                    );
                                } else if let Some(parent) = entry.path.parent() {
                                    if parent.starts_with(cwd.as_path()) && parent != cwd.as_path()
                                    {
                                        if let Some(pos) =
                                            files.iter().position(|f| f.path == parent)
                                        {
                                            *selected_index = pos;
                                            *scroll = crate::app::state::calculate_scroll(
                                                *selected_index,
                                                *scroll,
                                                visible_height,
                                                files.len(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Esc | KeyCode::Char('q') => {
                            pane.mode = crate::pane::state::PaneMode::Terminal;
                        }
                        _ => {}
                    }
                }
            }
            crate::pane::state::PaneMode::MarkdownViewer {
                path,
                content: _,
                scroll,
                lines,
            } => match key_event.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if *scroll > 0 {
                        *scroll -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if *scroll < lines.len().saturating_sub(1) {
                        *scroll += 1;
                    }
                }
                KeyCode::PageUp => {
                    *scroll = scroll.saturating_sub(15);
                }
                KeyCode::PageDown => {
                    *scroll = std::cmp::min(lines.len().saturating_sub(1), *scroll + 15);
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    let explorer_cwd = identity_cwd.to_path_buf();
                    let favorites = crate::config::load_favorites(&explorer_cwd);
                    let is_tree_view = true;
                    let mut expanded_dirs = std::collections::HashSet::new();
                    expanded_dirs.insert(explorer_cwd.clone());

                    // Expand all ancestor directories of the file we just closed
                    let mut ancestor = path.parent();
                    while let Some(anc) = ancestor {
                        if anc.starts_with(&explorer_cwd) {
                            expanded_dirs.insert(anc.to_path_buf());
                            ancestor = anc.parent();
                        } else {
                            break;
                        }
                    }

                    let filter_md = false;
                    let sort_by_mtime = false;
                    let files = crate::app::state::build_explorer_entries(
                        &explorer_cwd,
                        is_tree_view,
                        &expanded_dirs,
                        "",
                        filter_md,
                        sort_by_mtime,
                        &favorites,
                    );

                    let selected_index = files.iter().position(|f| f.path == *path).unwrap_or(0);
                    pane.mode = crate::pane::state::PaneMode::FileExplorer {
                        cwd: explorer_cwd,
                        selected_index,
                        files,
                        scroll: 0,
                        search_query: String::new(),
                        search_mode: false,
                        is_tree_view,
                        expanded_dirs,
                        filter_md,
                        sort_by_mtime,
                    };
                }
                KeyCode::Char('w') => {
                    let _ = crate::app::web_launcher::launch_web_viewer(path);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::layout::Rect;

    use super::super::{
        app_for_mouse_test, mouse, numbered_lines_bytes, unique_temp_path, wait_for_file,
    };
    use super::*;
    use crate::{config::Config, pane::state::PaneMode, workspace::Workspace};

    #[tokio::test]
    async fn dragging_selection_above_pane_autoscrolls_and_extends_into_scrollback() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let pane_infos = ws.tabs[0].layout.panes(Rect::new(26, 2, 80, 18));
        let info = pane_infos[0].clone();
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                info.inner_rect.width,
                info.inner_rect.height,
                16 * 1024,
                &numbered_lines_bytes(64),
            ),
        );

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.view.pane_infos = pane_infos;

        let start_metrics = app
            .state
            .runtime_for_pane(pane_id)
            .and_then(crate::terminal::TerminalRuntime::scroll_metrics)
            .expect("initial scroll metrics");
        let start_row = info.inner_rect.y;
        let start_col = info.inner_rect.x + 2;

        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            start_col,
            start_row,
        ));
        app.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            start_col,
            info.inner_rect.y.saturating_sub(1),
        ));

        let end_metrics = app
            .state
            .runtime_for_pane(pane_id)
            .and_then(crate::terminal::TerminalRuntime::scroll_metrics)
            .expect("scroll metrics after drag");
        assert_eq!(
            end_metrics.offset_from_bottom,
            start_metrics.offset_from_bottom + 3
        );

        let selection = app.state.selection.as_ref().expect("selection after drag");
        assert!(selection.is_visible());
        assert_eq!(
            selection.ordered_cells(),
            (
                (
                    (start_metrics.max_offset_from_bottom - end_metrics.offset_from_bottom) as u32,
                    2,
                ),
                (start_metrics.max_offset_from_bottom as u32, 2),
            )
        );
    }

    #[tokio::test]
    async fn releasing_dragged_selection_clears_highlight_after_copy() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let pane_infos = ws.tabs[0].layout.panes(Rect::new(26, 2, 80, 18));
        let info = pane_infos[0].clone();
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                info.inner_rect.width,
                info.inner_rect.height,
                16 * 1024,
                &numbered_lines_bytes(64),
            ),
        );

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.view.pane_infos = pane_infos;

        let row = info.inner_rect.y;
        let start_col = info.inner_rect.x + 1;
        let end_col = info.inner_rect.x + 4;

        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            start_col,
            row,
        ));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), end_col, row));
        assert!(app.state.selection.is_some());

        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), end_col, row));

        assert!(app.state.selection.is_none());
    }

    #[tokio::test]
    async fn wheel_scroll_keeps_in_progress_selection_and_extends_it() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let pane_infos = ws.tabs[0].layout.panes(Rect::new(26, 2, 80, 18));
        let info = pane_infos[0].clone();
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                info.inner_rect.width,
                info.inner_rect.height,
                16 * 1024,
                &numbered_lines_bytes(64),
            ),
        );

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.view.pane_infos = pane_infos;

        let start_metrics = app
            .state
            .runtime_for_pane(pane_id)
            .and_then(crate::terminal::TerminalRuntime::scroll_metrics)
            .expect("initial scroll metrics");
        let top_row = info.inner_rect.y;
        let col = info.inner_rect.x + 2;

        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), col, top_row));
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, col, top_row));

        let end_metrics = app
            .state
            .runtime_for_pane(pane_id)
            .and_then(crate::terminal::TerminalRuntime::scroll_metrics)
            .expect("scroll metrics after wheel");
        assert_eq!(
            end_metrics.offset_from_bottom,
            start_metrics.offset_from_bottom + 3
        );

        let selection = app.state.selection.as_ref().expect("selection after wheel");
        assert!(selection.is_visible());
        assert_eq!(
            selection.ordered_cells(),
            (
                (
                    (start_metrics.max_offset_from_bottom - end_metrics.offset_from_bottom) as u32,
                    2,
                ),
                (start_metrics.max_offset_from_bottom as u32, 2),
            )
        );
    }

    #[tokio::test]
    async fn clicking_unfocused_pane_with_mouse_reporting_focuses_it_via_left_button() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let first_pane = ws.tabs[0].root_pane;
        let second_pane = ws.test_split(ratatui::layout::Direction::Vertical);

        let terminal_area = Rect::new(26, 2, 80, 18);
        let pane_infos = ws.tabs[0].layout.panes(terminal_area);
        let first_info = pane_infos
            .iter()
            .find(|p| p.id == first_pane)
            .unwrap()
            .clone();
        let second_info = pane_infos
            .iter()
            .find(|p| p.id == second_pane)
            .unwrap()
            .clone();

        ws.insert_test_runtime(
            first_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(
                first_info.inner_rect.width.max(1),
                first_info.inner_rect.height.max(1),
                b"",
            ),
        );
        ws.insert_test_runtime(
            second_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(
                second_info.inner_rect.width.max(1),
                second_info.inner_rect.height.max(1),
                b"\x1b[?1002h",
            ),
        );

        ws.tabs[0].layout.focus_pane(first_pane);

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.view.pane_infos = pane_infos;

        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            second_info.inner_rect.x + 2,
            second_info.inner_rect.y + 2,
        ));

        assert_eq!(
            app.state.workspaces[0].tabs[0].layout.focused(),
            second_pane
        );
        assert_eq!(app.state.mode, Mode::Terminal);
    }

    #[tokio::test]
    async fn clicking_unfocused_pane_with_mouse_reporting_focuses_it_via_right_button() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let first_pane = ws.tabs[0].root_pane;
        let second_pane = ws.test_split(ratatui::layout::Direction::Vertical);

        let terminal_area = Rect::new(26, 2, 80, 18);
        let pane_infos = ws.tabs[0].layout.panes(terminal_area);
        let first_info = pane_infos
            .iter()
            .find(|p| p.id == first_pane)
            .unwrap()
            .clone();
        let second_info = pane_infos
            .iter()
            .find(|p| p.id == second_pane)
            .unwrap()
            .clone();

        ws.insert_test_runtime(
            first_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(
                first_info.inner_rect.width.max(1),
                first_info.inner_rect.height.max(1),
                b"",
            ),
        );
        ws.insert_test_runtime(
            second_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(
                second_info.inner_rect.width.max(1),
                second_info.inner_rect.height.max(1),
                b"\x1b[?1002h",
            ),
        );

        ws.tabs[0].layout.focus_pane(first_pane);

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.view.pane_infos = pane_infos;

        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Right),
            second_info.inner_rect.x + 2,
            second_info.inner_rect.y + 2,
        ));

        assert_eq!(
            app.state.workspaces[0].tabs[0].layout.focused(),
            second_pane
        );
        assert_eq!(app.state.mode, Mode::ContextMenu);
        assert!(app.state.context_menu.is_some());
    }

    #[tokio::test]
    async fn terminal_direct_focus_pane_shortcut_switches_focus_without_leaving_terminal_mode() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("test")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.view.pane_infos = app.state.workspaces[0]
            .active_tab()
            .unwrap()
            .layout
            .panes(Rect::new(0, 0, 80, 24));
        let focused_before = app.state.workspaces[0].layout.focused();
        app.state.keybinds.focus_pane_left = crate::config::ActionKeybinds::direct("alt+h");

        app.handle_terminal_key(TerminalKey::new(KeyCode::Char('h'), KeyModifiers::ALT))
            .await;

        assert_ne!(app.state.workspaces[0].layout.focused(), focused_before);
        assert_eq!(app.state.mode, Mode::Terminal);
    }

    #[tokio::test]
    async fn terminal_direct_edit_scrollback_opens_editor_pane() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("test");
        let root_pane = workspace.tabs[0].root_pane;
        workspace.tabs[0].runtimes.insert(
            root_pane,
            crate::pane::PaneRuntime::test_with_scrollback_bytes(20, 5, 4096, b"alpha\nbeta\n"),
        );
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;

        let output_path = unique_temp_path("direct-edit-scrollback");
        let previous_editor = std::env::var_os("EDITOR");
        std::env::set_var(
            "EDITOR",
            format!("sh -c 'cp \"$1\" {}' sh", output_path.display()),
        );
        app.state.keybinds.edit_scrollback = crate::config::ActionKeybinds::direct("ctrl+alt+e");

        app.handle_terminal_key(TerminalKey::new(
            KeyCode::Char('e'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .await;

        match previous_editor {
            Some(value) => std::env::set_var("EDITOR", value),
            None => std::env::remove_var("EDITOR"),
        }

        let content = wait_for_file(&output_path);
        assert!(content.contains("alpha"));
        assert!(content.contains("beta"));
        assert_eq!(app.state.mode, Mode::Terminal);

        let _ = std::fs::remove_file(output_path);
    }

    #[tokio::test]
    async fn direct_custom_command_runs_before_forwarding_to_pane() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("test")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;

        let output_path = unique_temp_path("direct-custom-command");
        let command = format!("printf direct > '{}'", output_path.display());
        app.state.keybinds.custom_commands = vec![crate::config::CustomCommandKeybind {
            bindings: crate::config::ActionKeybinds::direct("ctrl+alt+g"),
            label: "ctrl+alt+g".into(),
            command,
            action: crate::config::CustomCommandAction::Shell,
        }];

        app.handle_terminal_key(TerminalKey::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .await;

        assert_eq!(wait_for_file(&output_path), "direct");
        assert_eq!(app.state.mode, Mode::Terminal);
        let _ = std::fs::remove_file(output_path);
    }

    #[tokio::test]
    async fn direct_custom_pane_command_opens_overlay_pane() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let (workspace, terminal, runtime) = Workspace::new(
            std::env::current_dir().unwrap_or_else(|_| "/".into()),
            24,
            80,
            app.state.pane_scrollback_limit_bytes,
            app.state.host_terminal_theme,
            &app.state.default_shell,
            app.event_tx.clone(),
            app.render_notify.clone(),
            app.render_dirty.clone(),
        )
        .expect("workspace should spawn");
        app.state.workspaces = vec![workspace];
        app.state
            .terminal_runtimes
            .insert(terminal.id.clone(), runtime);
        app.state.terminals.insert(terminal.id.clone(), terminal);
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;

        app.state.keybinds.custom_commands = vec![crate::config::CustomCommandKeybind {
            bindings: crate::config::ActionKeybinds::direct("ctrl+alt+g"),
            label: "ctrl+alt+g".into(),
            command: "printf direct-pane".into(),
            action: crate::config::CustomCommandAction::Pane,
        }];

        app.handle_terminal_key(TerminalKey::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .await;

        assert_eq!(app.state.workspaces[0].tabs[0].layout.pane_count(), 2);
        assert!(app.state.workspaces[0].tabs[0].zoomed);
        assert_eq!(app.state.mode, Mode::Terminal);
    }

    #[tokio::test]
    async fn alt_backspace_is_forwarded_to_focused_pane() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let pane_infos = ws.tabs[0].layout.panes(Rect::new(0, 0, 80, 24));
        let info = pane_infos[0].clone();
        let (runtime, mut rx) = crate::pane::PaneRuntime::test_with_channel(
            info.inner_rect.width,
            info.inner_rect.height,
        );
        ws.tabs[0].runtimes.insert(pane_id, runtime);

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.view.pane_infos = pane_infos;

        let key = crate::input::parse_terminal_key_sequence("\x1b\x7f").unwrap();
        app.handle_terminal_key_headless(key);

        let bytes = rx.try_recv().unwrap();
        assert_eq!(bytes.as_ref(), b"\x1b\x7f");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn page_up_scrolls_plain_shell_pane() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let pane_infos = ws.tabs[0].layout.panes(Rect::new(26, 2, 80, 18));
        let info = pane_infos[0].clone();
        ws.tabs[0].runtimes.insert(
            pane_id,
            crate::pane::PaneRuntime::test_with_scrollback_bytes(
                info.inner_rect.width,
                info.inner_rect.height,
                16 * 1024,
                &numbered_lines_bytes(64),
            ),
        );

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.view.pane_infos = pane_infos;

        let start_metrics = app
            .state
            .runtime_for_pane_in_workspace(0, pane_id)
            .and_then(crate::pane::PaneRuntime::scroll_metrics)
            .expect("initial scroll metrics");
        assert_eq!(start_metrics.offset_from_bottom, 0);

        app.handle_terminal_key_headless(TerminalKey::new(KeyCode::PageUp, KeyModifiers::empty()));

        let end_metrics = app
            .state
            .runtime_for_pane_in_workspace(0, pane_id)
            .and_then(crate::pane::PaneRuntime::scroll_metrics)
            .expect("scroll metrics after PageUp");
        assert_eq!(
            end_metrics.offset_from_bottom,
            info.inner_rect.height as usize
        );
    }

    #[tokio::test]
    async fn page_down_returns_to_bottom_after_page_up() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let pane_infos = ws.tabs[0].layout.panes(Rect::new(26, 2, 80, 18));
        let info = pane_infos[0].clone();
        ws.tabs[0].runtimes.insert(
            pane_id,
            crate::pane::PaneRuntime::test_with_scrollback_bytes(
                info.inner_rect.width,
                info.inner_rect.height,
                16 * 1024,
                &numbered_lines_bytes(64),
            ),
        );

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.view.pane_infos = pane_infos;

        app.handle_terminal_key_headless(TerminalKey::new(KeyCode::PageUp, KeyModifiers::empty()));
        let after_up = app
            .state
            .runtime_for_pane_in_workspace(0, pane_id)
            .and_then(crate::pane::PaneRuntime::scroll_metrics)
            .expect("scroll metrics after PageUp");
        assert!(after_up.offset_from_bottom > 0);

        app.handle_terminal_key_headless(TerminalKey::new(
            KeyCode::PageDown,
            KeyModifiers::empty(),
        ));
        let after_down = app
            .state
            .runtime_for_pane_in_workspace(0, pane_id)
            .and_then(crate::pane::PaneRuntime::scroll_metrics)
            .expect("scroll metrics after PageDown");
        assert_eq!(after_down.offset_from_bottom, 0);
    }

    #[tokio::test]
    async fn page_up_release_does_not_scroll_plain_shell_pane_again() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let pane_infos = ws.tabs[0].layout.panes(Rect::new(26, 2, 80, 18));
        let info = pane_infos[0].clone();
        ws.tabs[0].runtimes.insert(
            pane_id,
            crate::pane::PaneRuntime::test_with_scrollback_bytes(
                info.inner_rect.width,
                info.inner_rect.height,
                16 * 1024,
                &numbered_lines_bytes(64),
            ),
        );

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.view.pane_infos = pane_infos;

        app.handle_terminal_key_headless(TerminalKey::new(KeyCode::PageUp, KeyModifiers::empty()));
        let after_press = app
            .state
            .runtime_for_pane_in_workspace(0, pane_id)
            .and_then(crate::pane::PaneRuntime::scroll_metrics)
            .expect("scroll metrics after PageUp press");
        assert_eq!(
            after_press.offset_from_bottom,
            info.inner_rect.height as usize
        );

        app.handle_terminal_key_headless(
            TerminalKey::new(KeyCode::PageUp, KeyModifiers::empty())
                .with_kind(KeyEventKind::Release),
        );

        let after_release = app
            .state
            .runtime_for_pane_in_workspace(0, pane_id)
            .and_then(crate::pane::PaneRuntime::scroll_metrics)
            .expect("scroll metrics after PageUp release");
        assert_eq!(
            after_release.offset_from_bottom,
            after_press.offset_from_bottom
        );
    }

    #[tokio::test]
    async fn modified_page_up_does_not_host_scroll_plain_shell_pane() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let pane_infos = ws.tabs[0].layout.panes(Rect::new(26, 2, 80, 18));
        let info = pane_infos[0].clone();
        ws.tabs[0].runtimes.insert(
            pane_id,
            crate::pane::PaneRuntime::test_with_scrollback_bytes(
                info.inner_rect.width,
                info.inner_rect.height,
                16 * 1024,
                &numbered_lines_bytes(64),
            ),
        );

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.view.pane_infos = pane_infos;

        app.handle_terminal_key_headless(TerminalKey::new(KeyCode::PageUp, KeyModifiers::CONTROL));

        let metrics = app
            .state
            .runtime_for_pane_in_workspace(0, pane_id)
            .and_then(crate::pane::PaneRuntime::scroll_metrics)
            .expect("scroll metrics after modified PageUp");
        assert_eq!(metrics.offset_from_bottom, 0);
    }

    #[tokio::test]
    async fn page_up_forwarded_to_mouse_reporting_pane() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let pane_infos = ws.tabs[0].layout.panes(Rect::new(26, 2, 80, 18));
        let info = pane_infos[0].clone();
        let mut bytes = b"\x1b[?1002h".to_vec();
        bytes.extend_from_slice(&numbered_lines_bytes(64));
        ws.tabs[0].runtimes.insert(
            pane_id,
            crate::pane::PaneRuntime::test_with_scrollback_bytes(
                info.inner_rect.width,
                info.inner_rect.height,
                16 * 1024,
                &bytes,
            ),
        );

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.view.pane_infos = pane_infos;

        let start_metrics = app
            .state
            .runtime_for_pane_in_workspace(0, pane_id)
            .and_then(crate::pane::PaneRuntime::scroll_metrics)
            .expect("initial scroll metrics");
        assert_eq!(start_metrics.offset_from_bottom, 0);

        app.handle_terminal_key_headless(TerminalKey::new(KeyCode::PageUp, KeyModifiers::empty()));

        let end_metrics = app
            .state
            .runtime_for_pane_in_workspace(0, pane_id)
            .and_then(crate::pane::PaneRuntime::scroll_metrics)
            .expect("scroll metrics after PageUp");
        // Forwarded to pane, so test runtime doesn't process it — scroll stays at bottom.
        assert_eq!(end_metrics.offset_from_bottom, 0);
    }

    #[test]
    fn test_custom_mode_backspace() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;

        let files = vec![
            crate::pane::state::FileEntry {
                name: "file_a.txt".into(),
                path: std::path::PathBuf::from("file_a.txt"),
                is_dir: false,
                depth: 0,
                is_expanded: false,
                is_favorite: false,
            },
            crate::pane::state::FileEntry {
                name: "file_b.txt".into(),
                path: std::path::PathBuf::from("file_b.txt"),
                is_dir: false,
                depth: 0,
                is_expanded: false,
                is_favorite: false,
            },
        ];

        ws.tabs[0].panes.get_mut(&pane_id).unwrap().mode = PaneMode::FileExplorer {
            cwd: std::path::PathBuf::from("."),
            selected_index: 0,
            files,
            scroll: 0,
            search_query: "file_a".to_string(),
            search_mode: true,
            is_tree_view: true,
            expanded_dirs: std::collections::HashSet::new(),
            filter_md: false,
            sort_by_mtime: false,
        };

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);

        let pane = app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap();

        // Handle Backspace
        App::handle_custom_mode_key_internal(
            TerminalKey::new(KeyCode::Backspace, KeyModifiers::empty()),
            pane,
            24,
            &std::path::PathBuf::from("."),
        );

        let updated_pane = &app.state.workspaces[0].tabs[0].panes[&pane_id];
        if let PaneMode::FileExplorer {
            search_query,
            selected_index,
            scroll,
            ..
        } = &updated_pane.mode
        {
            assert_eq!(search_query, "file_");
            assert_eq!(*selected_index, 0);
            assert_eq!(*scroll, 0);
        } else {
            panic!("Expected FileExplorer mode");
        }
    }

    #[test]
    fn test_custom_mode_type_search() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;

        let files = vec![crate::pane::state::FileEntry {
            name: "file_a.txt".into(),
            path: std::path::PathBuf::from("file_a.txt"),
            is_dir: false,
            depth: 0,
            is_expanded: false,
            is_favorite: false,
        }];

        ws.tabs[0].panes.get_mut(&pane_id).unwrap().mode = PaneMode::FileExplorer {
            cwd: std::path::PathBuf::from("."),
            selected_index: 0,
            files,
            scroll: 0,
            search_query: String::new(),
            search_mode: true,
            is_tree_view: true,
            expanded_dirs: std::collections::HashSet::new(),
            filter_md: false,
            sort_by_mtime: false,
        };

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);

        let pane = app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap();

        // Type 'a'
        App::handle_custom_mode_key_internal(
            TerminalKey::new(KeyCode::Char('a'), KeyModifiers::empty()),
            pane,
            24,
            &std::path::PathBuf::from("."),
        );

        let updated_pane = &app.state.workspaces[0].tabs[0].panes[&pane_id];
        if let PaneMode::FileExplorer {
            search_query,
            selected_index,
            scroll,
            ..
        } = &updated_pane.mode
        {
            assert_eq!(search_query, "a");
            assert_eq!(*selected_index, 0);
            assert_eq!(*scroll, 0);
        } else {
            panic!("Expected FileExplorer mode");
        }
    }

    #[test]
    fn test_custom_mode_toggle_search() {
        let mut app = app_for_mouse_test();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;

        let files = vec![crate::pane::state::FileEntry {
            name: "file_a.txt".into(),
            path: std::path::PathBuf::from("file_a.txt"),
            is_dir: false,
            depth: 0,
            is_expanded: false,
            is_favorite: false,
        }];

        ws.tabs[0].panes.get_mut(&pane_id).unwrap().mode = PaneMode::FileExplorer {
            cwd: std::path::PathBuf::from("."),
            selected_index: 0,
            files,
            scroll: 0,
            search_query: String::new(),
            search_mode: false,
            is_tree_view: true,
            expanded_dirs: std::collections::HashSet::new(),
            filter_md: false,
            sort_by_mtime: false,
        };

        app.state.workspaces = vec![ws];
        app.state.active = Some(0);

        let pane = app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap();

        // Press '/' to toggle search mode
        App::handle_custom_mode_key_internal(
            TerminalKey::new(KeyCode::Char('/'), KeyModifiers::empty()),
            pane,
            24,
            &std::path::PathBuf::from("."),
        );

        let updated_pane = &app.state.workspaces[0].tabs[0].panes[&pane_id];
        if let PaneMode::FileExplorer { search_mode, .. } = &updated_pane.mode {
            assert!(*search_mode);
        } else {
            panic!("Expected FileExplorer mode");
        }
    }
}
