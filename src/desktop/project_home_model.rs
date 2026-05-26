use serde::Serialize;

use super::workroom_model::{WorkroomPane, WorkroomPaneRole, WorkroomView};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProjectHomeView {
    pub project_label: String,
    pub next_action: ProjectNextAction,
    pub board: MissionBoard,
    pub research: ResearchRoomState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProjectNextAction {
    pub title: String,
    pub detail: String,
    pub primary_action: String,
    pub target_room: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MissionBoard {
    pub lanes: Vec<MissionBoardLane>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MissionBoardLane {
    pub id: String,
    pub title: String,
    pub empty: String,
    pub items: Vec<MissionBoardItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MissionBoardItem {
    pub pane_id: String,
    pub title: String,
    pub role: String,
    pub status: String,
    pub packet: String,
    pub detail: String,
    pub action: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ResearchRoomState {
    pub prompt_placeholder: String,
    pub modes: Vec<ResearchMode>,
    pub sources: Vec<ResearchSource>,
    pub foxchat_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ResearchMode {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ResearchSource {
    pub id: String,
    pub label: String,
    pub checked: bool,
    pub required: bool,
}

impl ProjectHomeView {
    pub(crate) fn from_workroom(workroom: &WorkroomView, project_label: impl Into<String>) -> Self {
        let project_label = clean_label(project_label.into(), "Herdr project");
        let board = MissionBoard::from_workroom(workroom);
        let next_action = ProjectNextAction::from_board(&board);
        let research = ResearchRoomState::default();

        Self {
            project_label,
            next_action,
            board,
            research,
        }
    }
}

impl ProjectNextAction {
    fn from_board(board: &MissionBoard) -> Self {
        let needs_packet = board
            .lanes
            .iter()
            .find(|lane| lane.id == "needs_packet")
            .map(|lane| lane.items.len())
            .unwrap_or(0);
        if needs_packet > 0 {
            return Self {
                title: format!("{needs_packet} child session packet needed"),
                detail: "Ask child panes for missing report fields before accepting work.".into(),
                primary_action: "Request packets".into(),
                target_room: "review".into(),
            };
        }

        let parent_review = board
            .lanes
            .iter()
            .find(|lane| lane.id == "parent_review")
            .map(|lane| lane.items.len())
            .unwrap_or(0);
        if parent_review > 0 {
            return Self {
                title: format!("{parent_review} packet ready for parent review"),
                detail: "Review evidence, risks, and changed files before accepting.".into(),
                primary_action: "Open review".into(),
                target_room: "review".into(),
            };
        }

        let running = board
            .lanes
            .iter()
            .find(|lane| lane.id == "running")
            .map(|lane| lane.items.len())
            .unwrap_or(0);
        if running > 0 {
            return Self {
                title: format!("{running} child session running"),
                detail: "Watch live panes only when you need terminal truth.".into(),
                primary_action: "Open workbench".into(),
                target_room: "panes".into(),
            };
        }

        Self {
            title: "Start by scanning or planning this project".into(),
            detail:
                "Use Research when you do not know the project state, or import a plan when you already have one."
                    .into(),
            primary_action: "Open research".into(),
            target_room: "research".into(),
        }
    }
}

impl MissionBoard {
    fn from_workroom(workroom: &WorkroomView) -> Self {
        let mut lanes = vec![
            MissionBoardLane::new("inbox", "Inbox / Ideas", "No draft ideas yet."),
            MissionBoardLane::new("planned", "Planned", "No planned child sessions."),
            MissionBoardLane::new("running", "Running", "No child sessions running."),
            MissionBoardLane::new("needs_packet", "Needs Packet", "No packets missing."),
            MissionBoardLane::new(
                "parent_review",
                "Parent Review",
                "No packets waiting for review.",
            ),
            MissionBoardLane::new("accepted", "Accepted", "No accepted packets yet."),
        ];

        for pane in &workroom.children {
            let lane_id = lane_for_pane(pane);
            let item = MissionBoardItem::from_pane(pane);
            if let Some(lane) = lanes.iter_mut().find(|lane| lane.id == lane_id) {
                lane.items.push(item);
            }
        }

        if workroom.children.is_empty() {
            lanes[0].items.push(MissionBoardItem {
                pane_id: workroom.parent.pane_id.clone(),
                title: "No child sessions yet".into(),
                role: "parent".into(),
                status: "ready".into(),
                packet: "0/10".into(),
                detail:
                    "Open Research to scan the project, or import a session plan to launch child panes."
                        .into(),
                action: "research".into(),
            });
        }

        Self { lanes }
    }
}

impl MissionBoardLane {
    fn new(id: &str, title: &str, empty: &str) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            empty: empty.into(),
            items: Vec::new(),
        }
    }
}

impl MissionBoardItem {
    fn from_pane(pane: &WorkroomPane) -> Self {
        Self {
            pane_id: pane.pane_id.clone(),
            title: pane.title.clone(),
            role: match pane.role {
                WorkroomPaneRole::Parent => "parent".into(),
                WorkroomPaneRole::Child => "child".into(),
            },
            status: pane.status.clone(),
            packet: pane.packet.clone(),
            detail: format!("{}; {}; {}", pane.mode, pane.dependency, pane.blast_radius),
            action: action_for_pane(pane).into(),
        }
    }
}

impl Default for ResearchRoomState {
    fn default() -> Self {
        Self {
            prompt_placeholder:
                "Ask what this project is, what changed, what is risky, or what to do next.".into(),
            modes: vec![
                ResearchMode::new("summary", "Summary"),
                ResearchMode::new("code", "Code"),
                ResearchMode::new("design", "Design"),
                ResearchMode::new("research", "Research"),
                ResearchMode::new("inspired", "Get Inspired"),
                ResearchMode::new("deep", "Think Deeply"),
            ],
            sources: vec![
                ResearchSource::new("px", "px project scan", true, false),
                ResearchSource::new("git", "git status", true, true),
                ResearchSource::new("recorder", "mission recorder", true, true),
                ResearchSource::new("sessions", ".sessions files", true, false),
                ResearchSource::new("terminal", "terminal output fallback", false, false),
                ResearchSource::new("foxchat", "open FoxChat", false, false),
            ],
            foxchat_url: "http://localhost:5100/chat?theme=dark".into(),
        }
    }
}

impl ResearchMode {
    fn new(id: &str, label: &str) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

impl ResearchSource {
    fn new(id: &str, label: &str, checked: bool, required: bool) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            checked,
            required,
        }
    }
}

fn lane_for_pane(pane: &WorkroomPane) -> &'static str {
    if pane.status == "accepted" {
        return "accepted";
    }
    if pane.lifecycle_lane == "parent review" || packet_complete(&pane.packet) {
        return "parent_review";
    }
    if pane.lifecycle_lane == "needs packet" || !packet_complete(&pane.packet) {
        return "needs_packet";
    }
    if matches!(pane.status.as_str(), "running" | "queued") {
        return "running";
    }
    if pane.has_contract {
        return "planned";
    }
    "inbox"
}

fn action_for_pane(pane: &WorkroomPane) -> &'static str {
    if pane.status == "accepted" {
        "open evidence"
    } else if pane.lifecycle_lane == "parent review" || packet_complete(&pane.packet) {
        "review packet"
    } else if pane.lifecycle_lane == "needs packet" || !packet_complete(&pane.packet) {
        "request packet"
    } else if matches!(pane.status.as_str(), "running" | "queued") {
        "watch pane"
    } else {
        "inspect"
    }
}

fn packet_complete(packet: &str) -> bool {
    let Some((done, required)) = packet.split_once('/') else {
        return false;
    };
    let Ok(done) = done.parse::<u16>() else {
        return false;
    };
    let Ok(required) = required.parse::<u16>() else {
        return false;
    };
    required > 0 && done >= required
}

fn clean_label(value: String, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.into()
    } else {
        trimmed.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::workroom_model::WorkroomView;

    #[test]
    fn empty_project_home_points_to_research() {
        let panes = serde_json::json!([
            {"pane_id": "parent-1", "is_root_pane": true, "cwd": "/repo"}
        ]);
        let workroom = WorkroomView::from_pane_values(panes.as_array().unwrap(), None);
        let home = ProjectHomeView::from_workroom(&workroom, "repo");

        assert_eq!(home.project_label, "repo");
        assert_eq!(home.next_action.target_room, "research");
        assert_eq!(home.board.lanes[0].items[0].title, "No child sessions yet");
        assert!(home
            .research
            .sources
            .iter()
            .any(|source| source.id == "px" && source.checked));
        assert!(home
            .research
            .sources
            .iter()
            .any(|source| source.id == "foxchat" && !source.checked));
    }

    #[test]
    fn incomplete_child_goes_to_needs_packet() {
        let panes = serde_json::json!([
            {"pane_id": "parent-1", "is_root_pane": true},
            {"pane_id": "child-1", "is_root_pane": false, "wave_contract": {"title": "Docs child", "mode": "write", "status": "done", "report": {"completed_fields": 4, "required_fields": 10}}}
        ]);
        let workroom = WorkroomView::from_pane_values(panes.as_array().unwrap(), None);
        let home = ProjectHomeView::from_workroom(&workroom, "repo");
        let lane = home
            .board
            .lanes
            .iter()
            .find(|lane| lane.id == "needs_packet")
            .unwrap();

        assert_eq!(lane.items.len(), 1);
        assert_eq!(lane.items[0].action, "request packet");
        assert_eq!(home.next_action.target_room, "review");
    }

    #[test]
    fn complete_child_goes_to_parent_review() {
        let panes = serde_json::json!([
            {"pane_id": "parent-1", "is_root_pane": true},
            {"pane_id": "child-1", "is_root_pane": false, "wave_contract": {"title": "Verifier", "mode": "verifier", "status": "done", "report": {"completed_fields": 10, "required_fields": 10}}}
        ]);
        let workroom = WorkroomView::from_pane_values(panes.as_array().unwrap(), None);
        let home = ProjectHomeView::from_workroom(&workroom, "repo");
        let lane = home
            .board
            .lanes
            .iter()
            .find(|lane| lane.id == "parent_review")
            .unwrap();

        assert_eq!(lane.items.len(), 1);
        assert_eq!(lane.items[0].action, "review packet");
        assert_eq!(home.next_action.primary_action, "Open review");
    }
}
