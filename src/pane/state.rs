use crate::terminal::TerminalId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: std::path::PathBuf,
    pub is_dir: bool,
    pub name: String,
    pub depth: usize,
    pub is_expanded: bool,
    pub is_favorite: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneMode {
    Terminal,

    MarkdownViewer {
        path: std::path::PathBuf,
        content: String,
        scroll: usize,
        lines: Vec<String>,
    },
}

/// Viewport state for a pane.
///
/// Terminal identity, cwd, labels, and agent metadata live in TerminalState.
pub struct PaneState {
    pub attached_terminal_id: TerminalId,
    /// Whether the user has seen this pane since its last state change to Idle.
    /// False = "Done" (agent finished while user was in another workspace).
    pub seen: bool,
    pub mode: PaneMode,
}

impl PaneState {
    pub fn new(attached_terminal_id: TerminalId) -> Self {
        Self {
            attached_terminal_id,
            seen: true,
            mode: PaneMode::Terminal,
        }
    }
}
