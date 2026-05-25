use serde::Serialize;
use serde_json::Value;

use crate::wave::{BlastRadius, WaveContract, WaveMode, WaveStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WorkroomView {
    pub parent: WorkroomPane,
    pub children: Vec<WorkroomPane>,
    pub selected_pane_id: String,
    pub stats: WorkroomStats,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WorkroomStats {
    pub pane_count: usize,
    pub child_count: usize,
    pub running_children: usize,
    pub needs_attention: usize,
    pub complete_packets: usize,
    pub attached_contracts: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WorkroomPane {
    pub pane_id: String,
    pub terminal_id: Option<String>,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub cwd: Option<String>,
    pub title: String,
    pub role: WorkroomPaneRole,
    pub mode: String,
    pub status: String,
    pub lifecycle_lane: String,
    pub packet: String,
    pub dependency: String,
    pub blast_radius: String,
    pub has_contract: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkroomPaneRole {
    Parent,
    Child,
}

impl WorkroomView {
    pub(crate) fn from_pane_values(panes: &[Value], selected_pane_id: Option<&str>) -> Self {
        let parent_value = panes
            .iter()
            .find(|pane| pane_bool(pane, "is_root_pane"))
            .or_else(|| panes.first());
        let parent = parent_value
            .map(|pane| WorkroomPane::from_value(pane, WorkroomPaneRole::Parent))
            .unwrap_or_else(WorkroomPane::empty_parent);
        let children = panes
            .iter()
            .filter(|pane| !pane_bool(pane, "is_root_pane"))
            .map(|pane| WorkroomPane::from_value(pane, WorkroomPaneRole::Child))
            .collect::<Vec<_>>();
        let selected_pane_id = selected_pane_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|value| {
                parent.pane_id == *value || children.iter().any(|pane| pane.pane_id == *value)
            })
            .map(str::to_string)
            .or_else(|| children.first().map(|pane| pane.pane_id.clone()))
            .unwrap_or_else(|| parent.pane_id.clone());
        let stats = WorkroomStats::from_parent_and_children(&parent, &children);

        Self {
            parent,
            children,
            selected_pane_id,
            stats,
        }
    }
}

impl WorkroomStats {
    fn from_parent_and_children(parent: &WorkroomPane, children: &[WorkroomPane]) -> Self {
        let running_children = children
            .iter()
            .filter(|pane| matches!(pane.status.as_str(), "running" | "queued"))
            .count();
        let needs_attention = children
            .iter()
            .filter(|pane| {
                matches!(
                    pane.lifecycle_lane.as_str(),
                    "needs packet" | "parent review" | "draft contract"
                )
            })
            .count();
        let complete_packets = children
            .iter()
            .filter(|pane| packet_complete(&pane.packet))
            .count();
        let attached_contracts = children.iter().filter(|pane| pane.has_contract).count();

        Self {
            pane_count: children.len() + usize::from(parent.pane_id != "parent"),
            child_count: children.len(),
            running_children,
            needs_attention,
            complete_packets,
            attached_contracts,
        }
    }
}

impl WorkroomPane {
    fn from_value(value: &Value, role: WorkroomPaneRole) -> Self {
        let contract = value
            .get("wave_contract")
            .cloned()
            .and_then(|raw| serde_json::from_value::<WaveContract>(raw).ok())
            .and_then(WaveContract::normalized);
        let pane_id = pane_string(value, "pane_id").unwrap_or_else(|| "pane-unknown".into());
        let terminal_id = pane_string(value, "terminal_id");
        let title = contract
            .as_ref()
            .map(|contract| contract.title.clone())
            .or_else(|| pane_string(value, "title"))
            .or_else(|| pane_string(value, "name"))
            .unwrap_or_else(|| match role {
                WorkroomPaneRole::Parent => "Parent session".into(),
                WorkroomPaneRole::Child => pane_id.clone(),
            });
        let mode = contract
            .as_ref()
            .map(|contract| contract.mode.label().to_string())
            .unwrap_or_else(|| match role {
                WorkroomPaneRole::Parent => WaveMode::Write.label().into(),
                WorkroomPaneRole::Child => WaveMode::DraftOnly.label().into(),
            });
        let status = contract
            .as_ref()
            .and_then(|contract| contract.status)
            .map(status_label)
            .or_else(|| pane_string(value, "custom_status"))
            .or_else(|| pane_string(value, "agent_status"))
            .unwrap_or_else(|| "running".into());
        let lifecycle_lane = contract
            .as_ref()
            .map(|contract| contract.effective_lifecycle_lane().label().to_string())
            .unwrap_or_else(|| match role {
                WorkroomPaneRole::Parent => "parent".into(),
                WorkroomPaneRole::Child => "draft contract".into(),
            });
        let packet = contract
            .as_ref()
            .map(|contract| contract.report.label())
            .unwrap_or_else(|| "0/10".into());
        let dependency = contract
            .as_ref()
            .and_then(|contract| contract.dependency.clone())
            .unwrap_or_else(|| "none".into());
        let blast_radius = contract
            .as_ref()
            .map(|contract| contract.blast_radius.label().to_string())
            .unwrap_or_else(|| BlastRadius::Unknown.label().into());
        let has_contract = contract.is_some();

        Self {
            pane_id,
            terminal_id,
            workspace_id: pane_string(value, "workspace_id"),
            tab_id: pane_string(value, "tab_id"),
            cwd: pane_string(value, "cwd"),
            title,
            role,
            mode,
            status,
            lifecycle_lane,
            packet,
            dependency,
            blast_radius,
            has_contract,
        }
    }

    fn empty_parent() -> Self {
        Self {
            pane_id: "parent".into(),
            terminal_id: None,
            workspace_id: None,
            tab_id: None,
            cwd: None,
            title: "Parent session".into(),
            role: WorkroomPaneRole::Parent,
            mode: WaveMode::Write.label().into(),
            status: "running".into(),
            lifecycle_lane: "parent".into(),
            packet: "0/10".into(),
            dependency: "none".into(),
            blast_radius: BlastRadius::Unknown.label().into(),
            has_contract: false,
        }
    }
}

fn status_label(status: WaveStatus) -> String {
    match status {
        WaveStatus::Queued => "queued",
        WaveStatus::Running => "running",
        WaveStatus::Blocked => "blocked",
        WaveStatus::NeedsReview => "needs review",
        WaveStatus::Accepted => "accepted",
        WaveStatus::Done => "done",
    }
    .into()
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

fn pane_bool(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn pane_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_is_root_and_children_are_nested_under_it() {
        let panes = serde_json::json!([
            {"pane_id": "child-1", "terminal_id": "term-child-1", "workspace_id": "w1", "tab_id": "t1", "cwd": "/repo", "is_root_pane": false, "wave_contract": {"title": "Wave 1: docs", "mode": "write", "status": "running", "report": {"completed_fields": 4, "required_fields": 10}}},
            {"pane_id": "parent-1", "terminal_id": "term-parent-1", "workspace_id": "w1", "tab_id": "t1", "cwd": "/repo", "is_root_pane": true},
            {"pane_id": "child-2", "terminal_id": "term-child-2", "workspace_id": "w1", "tab_id": "t1", "cwd": "/repo", "is_root_pane": false, "wave_contract": {"title": "Wave 2: verifier", "mode": "verifier", "status": "queued", "dependency": "after W1", "report": {"completed_fields": 10, "required_fields": 10}}}
        ]);

        let view = WorkroomView::from_pane_values(panes.as_array().unwrap(), Some("child-2"));

        assert_eq!(view.parent.pane_id, "parent-1");
        assert_eq!(view.parent.title, "Parent session");
        assert_eq!(view.children.len(), 2);
        assert_eq!(view.children[0].title, "Wave 1: docs");
        assert_eq!(view.children[1].dependency, "after W1");
        assert_eq!(view.selected_pane_id, "child-2");
        assert_eq!(view.stats.child_count, 2);
        assert_eq!(view.stats.running_children, 2);
        assert_eq!(view.stats.attached_contracts, 2);
        assert_eq!(view.stats.complete_packets, 1);
    }

    #[test]
    fn selected_pane_falls_back_to_first_child_then_parent() {
        let panes = serde_json::json!([
            {"pane_id": "parent-1", "is_root_pane": true},
            {"pane_id": "child-1", "is_root_pane": false}
        ]);

        let view = WorkroomView::from_pane_values(panes.as_array().unwrap(), Some("missing"));

        assert_eq!(view.selected_pane_id, "child-1");

        let parent_only = serde_json::json!([
            {"pane_id": "parent-1", "is_root_pane": true}
        ]);

        let view = WorkroomView::from_pane_values(parent_only.as_array().unwrap(), None);

        assert_eq!(view.selected_pane_id, "parent-1");
    }

    #[test]
    fn done_incomplete_packet_needs_attention() {
        let panes = serde_json::json!([
            {"pane_id": "parent-1", "is_root_pane": true},
            {"pane_id": "child-1", "is_root_pane": false, "wave_contract": {"title": "done child", "mode": "write", "status": "done", "report": {"completed_fields": 4, "required_fields": 10}}},
            {"pane_id": "child-2", "is_root_pane": false, "wave_contract": {"title": "accepted child", "mode": "write", "status": "accepted", "report": {"completed_fields": 10, "required_fields": 10}}}
        ]);

        let view = WorkroomView::from_pane_values(panes.as_array().unwrap(), None);

        assert_eq!(view.stats.needs_attention, 1);
        assert_eq!(view.stats.complete_packets, 1);
    }
}
