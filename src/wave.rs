use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaveMode {
    ReadOnly,
    DraftOnly,
    Write,
    Reviewer,
    Verifier,
    Monitor,
}

impl WaveMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::DraftOnly => "draft-only",
            Self::Write => "write",
            Self::Reviewer => "reviewer",
            Self::Verifier => "verifier",
            Self::Monitor => "monitor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaveStatus {
    Queued,
    Running,
    Blocked,
    NeedsReview,
    Accepted,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaveLifecycleLane {
    DraftContract,
    Running,
    NeedsPacket,
    ParentReview,
    Accepted,
}

impl WaveLifecycleLane {
    pub fn label(self) -> &'static str {
        match self {
            Self::DraftContract => "draft contract",
            Self::Running => "running",
            Self::NeedsPacket => "needs packet",
            Self::ParentReview => "parent review",
            Self::Accepted => "accepted",
        }
    }

    pub fn for_status_and_report(status: WaveStatus, report: &WaveReportGate) -> Self {
        match status {
            WaveStatus::Accepted => Self::Accepted,
            WaveStatus::Queued | WaveStatus::Running => Self::Running,
            WaveStatus::Done if report.completed_fields < report.required_fields => {
                Self::NeedsPacket
            }
            WaveStatus::Blocked | WaveStatus::NeedsReview | WaveStatus::Done => Self::ParentReview,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WavePromptDelivery {
    Agent,
    ShellCard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BlastRadius {
    None,
    Minor,
    Danger,
    #[default]
    Unknown,
}

impl BlastRadius {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "no overlap",
            Self::Minor => "minor overlap",
            Self::Danger => "danger overlap",
            Self::Unknown => "overlap unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaveArc {
    pub id: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<WaveStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaveReportGate {
    #[serde(default)]
    pub completed_fields: u8,
    #[serde(default = "default_required_report_fields")]
    pub required_fields: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_items: Vec<String>,
}

impl Default for WaveReportGate {
    fn default() -> Self {
        Self {
            completed_fields: 0,
            required_fields: default_required_report_fields(),
            completed_items: Vec::new(),
        }
    }
}

impl WaveReportGate {
    pub fn normalized(mut self) -> Self {
        if self.required_fields == 0 {
            self.required_fields = default_required_report_fields();
        }
        let mut completed_items = Vec::new();
        for item in self.completed_items {
            let item = item.trim().to_string();
            if item.is_empty() {
                continue;
            }
            let already_seen = completed_items
                .iter()
                .any(|seen: &String| seen.eq_ignore_ascii_case(&item));
            if !already_seen {
                completed_items.push(item);
            }
        }
        completed_items.truncate(self.required_fields as usize);
        self.completed_items = completed_items;
        if self.completed_items.is_empty() {
            self.completed_fields = self.completed_fields.min(self.required_fields);
        } else {
            self.completed_fields = self.completed_items.len().min(u8::MAX as usize) as u8;
        }
        self
    }

    pub fn label(&self) -> String {
        format!("{}/{}", self.completed_fields, self.required_fields)
    }
}

fn default_required_report_fields() -> u8 {
    10
}

const DEFAULT_REPORT_PACKET_ITEMS: [&str; 10] = [
    "What I did",
    "What I found",
    "Evidence / receipts",
    "Files read",
    "Files changed",
    "Commands run",
    "Risks / unknowns",
    "Good / bad / ugly",
    "Recommendation",
    "Next wave suggestion",
];

pub fn default_report_packet_items() -> &'static [&'static str] {
    &DEFAULT_REPORT_PACKET_ITEMS
}

pub fn derive_default_report_gate(text: &str) -> WaveReportGate {
    let completed_items = DEFAULT_REPORT_PACKET_ITEMS
        .iter()
        .filter(|item| report_item_marker_present(text, item))
        .map(|item| (*item).to_string())
        .collect();

    WaveReportGate {
        completed_fields: 0,
        required_fields: DEFAULT_REPORT_PACKET_ITEMS.len().min(u8::MAX as usize) as u8,
        completed_items,
    }
    .normalized()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaveContract {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub mode: WaveMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<WaveStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_lane: Option<WaveLifecycleLane>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency: Option<String>,
    #[serde(default)]
    pub report: WaveReportGate,
    #[serde(default)]
    pub blast_radius: BlastRadius,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_delivery: Option<WavePromptDelivery>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arcs: Vec<WaveArc>,
}

impl WaveContract {
    pub fn normalized(mut self) -> Option<Self> {
        self.title = self.title.trim().to_string();
        if self.title.is_empty() {
            return None;
        }
        self.dependency = self
            .dependency
            .map(|dependency| dependency.trim().to_string())
            .filter(|dependency| !dependency.is_empty());
        self.pane_id = self
            .pane_id
            .map(|pane_id| pane_id.trim().to_string())
            .filter(|pane_id| !pane_id.is_empty());
        self.arcs = self
            .arcs
            .into_iter()
            .filter_map(|mut arc| {
                arc.id = arc.id.trim().to_string();
                arc.summary = arc.summary.trim().to_string();
                (!arc.id.is_empty() || !arc.summary.is_empty()).then_some(arc)
            })
            .collect();
        self.report = self.report.normalized();
        Some(self)
    }

    pub fn effective_lifecycle_lane(&self) -> WaveLifecycleLane {
        self.lifecycle_lane
            .unwrap_or_else(|| self.derived_lifecycle_lane())
    }

    pub fn lifecycle_lane_for_status(&self, status: WaveStatus) -> WaveLifecycleLane {
        WaveLifecycleLane::for_status_and_report(status, &self.report)
    }

    fn derived_lifecycle_lane(&self) -> WaveLifecycleLane {
        if let Some(status) = self.status {
            return self.lifecycle_lane_for_status(status);
        }
        if self.report.completed_fields < self.report.required_fields {
            WaveLifecycleLane::NeedsPacket
        } else {
            WaveLifecycleLane::ParentReview
        }
    }

    pub fn border_label(&self) -> String {
        let mut parts = vec![
            self.title.clone(),
            self.mode.label().to_string(),
            format!("lane {}", self.effective_lifecycle_lane().label()),
            format!("packet {}", self.report.label()),
        ];
        if self.blast_radius != BlastRadius::Unknown {
            parts.push(self.blast_radius.label().to_string());
        }
        if let Some(dependency) = self.dependency.as_deref() {
            parts.push(dependency.to_string());
        }
        parts.join(" | ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaveContractParseError {
    Empty,
    InvalidJson(String),
    MissingWaveId,
    WaveNotFound(String),
    InvalidDerivedContract,
}

impl fmt::Display for WaveContractParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "wave contract document is empty"),
            Self::InvalidJson(err) => write!(f, "invalid wave contract JSON: {err}"),
            Self::MissingWaveId => write!(
                f,
                "markdown session files need --wave <id> unless they contain a herdr-wave-contract JSON fence"
            ),
            Self::WaveNotFound(wave_id) => write!(f, "wave {wave_id} not found in session file"),
            Self::InvalidDerivedContract => write!(f, "could not derive a valid wave contract"),
        }
    }
}

impl std::error::Error for WaveContractParseError {}

pub fn parse_wave_contract_document(
    text: &str,
    wave_id: Option<&str>,
) -> Result<WaveContract, WaveContractParseError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(WaveContractParseError::Empty);
    }

    if trimmed.starts_with('{') {
        return parse_contract_json(trimmed);
    }

    if let Some(block) = extract_fenced_block(trimmed, &["herdr-wave-contract", "wave-contract"]) {
        return parse_contract_json(block.trim());
    }

    let Some(wave_id) = wave_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return Err(WaveContractParseError::MissingWaveId);
    };

    derive_session_wave_contract(trimmed, wave_id)
}

pub fn parse_session_wave_contracts(
    text: &str,
) -> Result<Vec<WaveContract>, WaveContractParseError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(WaveContractParseError::Empty);
    }

    if trimmed.starts_with('[') {
        let contracts = serde_json::from_str::<Vec<WaveContract>>(trimmed)
            .map_err(|err| WaveContractParseError::InvalidJson(err.to_string()))?;
        return normalize_contracts(contracts);
    }

    if let Some(block) = extract_fenced_block(trimmed, &["herdr-wave-contracts", "wave-contracts"])
    {
        let contracts = serde_json::from_str::<Vec<WaveContract>>(block.trim())
            .map_err(|err| WaveContractParseError::InvalidJson(err.to_string()))?;
        return normalize_contracts(contracts);
    }

    let wave_ids = session_wave_ids(trimmed);
    if wave_ids.is_empty() {
        let contracts = recommended_dispatch_contracts(trimmed);
        if contracts.is_empty() {
            return Err(WaveContractParseError::MissingWaveId);
        }
        return normalize_contracts(contracts);
    }

    let mut contracts = Vec::new();
    for wave_id in wave_ids {
        contracts.push(derive_session_wave_contract(trimmed, &wave_id)?);
    }
    normalize_contracts(contracts)
}

fn parse_contract_json(text: &str) -> Result<WaveContract, WaveContractParseError> {
    let contract = serde_json::from_str::<WaveContract>(text)
        .map_err(|err| WaveContractParseError::InvalidJson(err.to_string()))?;
    contract
        .normalized()
        .ok_or(WaveContractParseError::InvalidDerivedContract)
}

fn normalize_contracts(
    contracts: Vec<WaveContract>,
) -> Result<Vec<WaveContract>, WaveContractParseError> {
    let contracts: Vec<WaveContract> = contracts
        .into_iter()
        .filter_map(WaveContract::normalized)
        .collect();
    if contracts.is_empty() {
        return Err(WaveContractParseError::InvalidDerivedContract);
    }
    Ok(contracts)
}

fn session_wave_ids(text: &str) -> Vec<String> {
    let ids = session_contract_actor_ids(text);
    if !ids.is_empty() {
        return ids;
    }
    worker_heading_wave_ids(text)
}

fn recommended_dispatch_contracts(text: &str) -> Vec<WaveContract> {
    let mut contracts = Vec::new();
    let mut current: Option<DispatchWaveDraft> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some((label, descriptor)) = parse_dispatch_wave_heading(trimmed) {
            if let Some(draft) = current.take() {
                if let Some(contract) = draft.into_contract() {
                    contracts.push(contract);
                }
            }
            current = Some(DispatchWaveDraft {
                label,
                descriptor,
                body: Vec::new(),
            });
            continue;
        }

        if let Some(draft) = current.as_mut() {
            draft.body.push(line.to_string());
        }
    }

    if let Some(draft) = current {
        if let Some(contract) = draft.into_contract() {
            contracts.push(contract);
        }
    }

    contracts
}

#[derive(Debug)]
struct DispatchWaveDraft {
    label: String,
    descriptor: String,
    body: Vec<String>,
}

impl DispatchWaveDraft {
    fn into_contract(self) -> Option<WaveContract> {
        let body = self.body.join("\n");
        let arcs = dispatch_arcs(&body);
        let body_with_descriptor = format!("{}\n{}", self.descriptor, body);
        let mut contract = WaveContract {
            title: dispatch_title(&self.label, &self.descriptor),
            pane_id: None,
            mode: infer_wave_mode("", &body_with_descriptor),
            status: Some(WaveStatus::Queued),
            lifecycle_lane: Some(WaveLifecycleLane::Running),
            dependency: dispatch_dependency(&self.descriptor),
            report: WaveReportGate::default(),
            blast_radius: infer_blast_radius(&self.descriptor, &body),
            prompt_delivery: None,
            arcs,
        };
        if contract.dependency.is_none() && contract.title.to_ascii_lowercase().contains("parallel")
        {
            contract.dependency = Some("parallel".into());
        }
        contract.normalized()
    }
}

fn parse_dispatch_wave_heading(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim().trim_end_matches(':').trim();
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("wave ") {
        return None;
    }

    let rest = trimmed.get(5..)?.trim_start();
    let digit_len = rest
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .map(|(index, ch)| index + ch.len_utf8())
        .last()?;
    let number = rest.get(..digit_len)?.trim();
    if number.is_empty() {
        return None;
    }
    let after_number = rest.get(digit_len..)?.trim_start();
    let descriptor = after_number
        .strip_prefix('—')
        .or_else(|| after_number.strip_prefix('-'))
        .or_else(|| after_number.strip_prefix(':'))
        .map(str::trim)
        .unwrap_or(after_number);
    let descriptor = strip_trailing_parenthetical(descriptor)
        .trim()
        .trim_end_matches(':')
        .trim()
        .to_string();
    Some((format!("Wave {number}"), descriptor))
}

fn dispatch_title(label: &str, descriptor: &str) -> String {
    let descriptor = descriptor.trim();
    if descriptor.is_empty() {
        label.to_string()
    } else {
        format!("{label}: {descriptor}")
    }
}

fn dispatch_dependency(descriptor: &str) -> Option<String> {
    let descriptor = strip_trailing_parenthetical(descriptor).trim();
    let lower = descriptor.to_ascii_lowercase();
    if lower.contains("parallel") {
        return Some("parallel".into());
    }
    let after_index = lower.find("after ")?;
    let after = descriptor.get(after_index + "after ".len()..)?.trim();
    let after = after
        .split([',', ';', '('])
        .next()
        .unwrap_or(after)
        .trim()
        .trim_end_matches(':')
        .trim();
    (!after.is_empty()).then(|| format!("after {after}"))
}

fn dispatch_arcs(body: &str) -> Vec<WaveArc> {
    body.lines()
        .filter_map(|line| {
            let mut value = line.trim();
            value = value
                .strip_prefix("- ")
                .or_else(|| value.strip_prefix("* "))
                .or_else(|| value.strip_prefix("+ "))
                .unwrap_or(value)
                .trim();
            let (id, summary) = value.split_once(':')?;
            let id = id.trim();
            let summary = summary.trim();
            let id_is_arc = !id.is_empty()
                && id.len() <= 12
                && id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'));
            (id_is_arc && !summary.is_empty()).then(|| WaveArc {
                id: id.to_string(),
                summary: summary.to_string(),
                status: Some(WaveStatus::Queued),
            })
        })
        .collect()
}

fn session_contract_actor_ids(text: &str) -> Vec<String> {
    let Some(block) = extract_fenced_block(text, &["px-session-contract"]) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(block) else {
        return Vec::new();
    };
    let Some(actors) = value.get("actors").and_then(Value::as_object) else {
        return Vec::new();
    };
    actors
        .keys()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .collect()
}

fn worker_heading_wave_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with("## ") {
                return None;
            }
            let heading = trimmed.trim_start_matches('#').trim();
            let (_, title) = heading.split_once(':')?;
            let title = title.trim();
            let wave_id = title
                .split(|ch: char| ch == '-' || ch.is_whitespace() || ch == '(')
                .next()?
                .trim();
            (!wave_id.is_empty()).then(|| wave_id.to_string())
        })
        .collect()
}

fn derive_session_wave_contract(
    text: &str,
    wave_id: &str,
) -> Result<WaveContract, WaveContractParseError> {
    let Some(section) = find_worker_section(text, wave_id) else {
        return Err(WaveContractParseError::WaveNotFound(wave_id.to_string()));
    };
    let required_sections = required_sections_for_wave(text, wave_id);
    let report = derive_report_gate(&section.body, &required_sections);
    let mut contract = WaveContract {
        title: section.title,
        pane_id: None,
        mode: infer_wave_mode(&section.role, &section.body),
        status: infer_wave_status(text, &section.body),
        lifecycle_lane: None,
        dependency: None,
        report,
        blast_radius: infer_blast_radius(text, &section.body),
        prompt_delivery: None,
        arcs: Vec::new(),
    };
    if contract.title.is_empty() {
        contract.title = wave_id.to_string();
    }
    contract
        .normalized()
        .ok_or(WaveContractParseError::InvalidDerivedContract)
}

fn extract_fenced_block<'a>(text: &'a str, names: &[&str]) -> Option<&'a str> {
    let mut search_from = 0;
    while let Some(start) = text[search_from..].find("```") {
        let fence_start = search_from + start;
        let after_ticks = fence_start + 3;
        let line_end = text[after_ticks..]
            .find('\n')
            .map(|offset| after_ticks + offset)?;
        let info = text[after_ticks..line_end].trim();
        let matches_name = names.iter().any(|name| {
            info.eq_ignore_ascii_case(name)
                || info
                    .split_whitespace()
                    .next()
                    .is_some_and(|first| first.eq_ignore_ascii_case(name))
        });
        let content_start = line_end + 1;
        let end_offset = text[content_start..].find("```")?;
        if matches_name {
            return Some(&text[content_start..content_start + end_offset]);
        }
        search_from = content_start + end_offset + 3;
    }
    None
}

#[derive(Debug)]
struct WorkerSection {
    title: String,
    role: String,
    body: String,
}

fn find_worker_section(text: &str, wave_id: &str) -> Option<WorkerSection> {
    let lower_wave_id = wave_id.to_ascii_lowercase();
    let mut lines = Vec::new();
    let mut byte = 0;
    for line in text.lines() {
        lines.push((byte, line));
        byte += line.len() + 1;
    }

    for (index, (start, line)) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !trimmed.starts_with("## ") {
            continue;
        }
        if !trimmed.to_ascii_lowercase().contains(&lower_wave_id) {
            continue;
        }
        let end = lines[index + 1..]
            .iter()
            .find(|(_, candidate)| {
                let trimmed = candidate.trim();
                let trimmed_lower = trimmed.to_ascii_lowercase();
                trimmed.starts_with("## ")
                    && trimmed_lower.contains("worker")
                    && !trimmed_lower.contains(&lower_wave_id)
            })
            .map(|(next_start, _)| *next_start)
            .unwrap_or(text.len());
        let body = text[*start..end].to_string();
        let heading = trimmed.trim_start_matches('#').trim();
        return Some(WorkerSection {
            title: worker_heading_title(heading),
            role: worker_heading_role(heading).unwrap_or_default(),
            body,
        });
    }

    None
}

fn worker_heading_title(heading: &str) -> String {
    let after_colon = heading
        .split_once(':')
        .map(|(_, title)| title.trim())
        .unwrap_or(heading);
    strip_trailing_parenthetical(after_colon).trim().to_string()
}

fn worker_heading_role(heading: &str) -> Option<String> {
    let open = heading.rfind('(')?;
    let close = heading[open..].find(')')? + open;
    (close > open + 1).then(|| heading[open + 1..close].trim().to_string())
}

fn strip_trailing_parenthetical(value: &str) -> &str {
    let trimmed = value.trim();
    let Some(open) = trimmed.rfind('(') else {
        return trimmed;
    };
    if trimmed.ends_with(')') {
        trimmed[..open].trim_end()
    } else {
        trimmed
    }
}

fn required_sections_for_wave(text: &str, wave_id: &str) -> Vec<String> {
    let Some(block) = extract_fenced_block(text, &["px-session-contract"]) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(block) else {
        return Vec::new();
    };
    let Some(actors) = value.get("actors").and_then(Value::as_object) else {
        return Vec::new();
    };
    actors
        .iter()
        .find(|(actor_id, _)| actor_id.eq_ignore_ascii_case(wave_id))
        .map(|(_, actor)| actor)
        .or_else(|| actors.get(wave_id))
        .and_then(|actor| actor.get("required_sections"))
        .and_then(Value::as_array)
        .map(|sections| {
            sections
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|section| !section.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn derive_report_gate(body: &str, required_sections: &[String]) -> WaveReportGate {
    if required_sections.is_empty() {
        return WaveReportGate::default();
    }
    let body_lower = body.to_ascii_lowercase();
    let completed = required_sections
        .iter()
        .filter(|section| {
            let section_lower = section.to_ascii_lowercase();
            body_lower.contains(&format!("### {section_lower}"))
                || body_lower.contains(&format!("**{section_lower}"))
        })
        .count();
    WaveReportGate {
        completed_fields: completed.min(u8::MAX as usize) as u8,
        required_fields: required_sections.len().min(u8::MAX as usize) as u8,
        completed_items: required_sections
            .iter()
            .filter(|section| {
                let section_lower = section.to_ascii_lowercase();
                body_lower.contains(&format!("### {section_lower}"))
                    || body_lower.contains(&format!("**{section_lower}"))
            })
            .cloned()
            .collect(),
    }
    .normalized()
}

fn report_item_marker_present(text: &str, item: &str) -> bool {
    let expected = report_item_key(item);
    text.lines()
        .filter_map(report_line_marker)
        .any(|marker| report_item_key(&marker) == expected)
}

fn report_line_marker(line: &str) -> Option<String> {
    let mut value = line.trim();
    if value.is_empty() {
        return None;
    }

    loop {
        let next = strip_report_line_prefix(value);
        if next == value {
            break;
        }
        value = next.trim_start();
    }

    if let Some(stripped) = value.strip_prefix("**") {
        if let Some((marker, _)) = stripped.split_once("**") {
            return non_empty_marker(marker);
        }
    }

    let marker = value
        .split_once(':')
        .map(|(marker, _)| marker)
        .unwrap_or(value);
    non_empty_marker(marker)
}

fn strip_report_line_prefix(value: &str) -> &str {
    let trimmed = value.trim_start();
    if let Some(rest) = trimmed.strip_prefix('>') {
        return rest;
    }
    if let Some(rest) = trimmed.strip_prefix('#') {
        return rest.trim_start_matches('#');
    }
    for prefix in ["- [x] ", "- [X] ", "- [ ] ", "- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest;
        }
    }

    let mut chars = trimmed.char_indices().peekable();
    let mut saw_digit = false;
    while let Some((_, ch)) = chars.peek().copied() {
        if ch.is_ascii_digit() {
            saw_digit = true;
            let _ = chars.next();
        } else {
            break;
        }
    }
    if saw_digit {
        if let Some((mark_index, mark)) = chars.peek().copied() {
            if matches!(mark, '.' | ')') {
                let after_mark = mark_index + mark.len_utf8();
                return trimmed[after_mark..].trim_start();
            }
        }
    }

    value
}

fn non_empty_marker(marker: &str) -> Option<String> {
    let marker = marker
        .trim()
        .trim_matches('*')
        .trim_matches('_')
        .trim_matches('`')
        .trim();
    (!marker.is_empty()).then(|| marker.to_string())
}

fn report_item_key(value: &str) -> String {
    let normalized = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();

    match normalized.as_str() {
        "nextchildsuggestion" => "nextwavesuggestion".into(),
        _ => normalized,
    }
}

fn infer_wave_mode(role: &str, body: &str) -> WaveMode {
    let role = role.to_ascii_lowercase();
    if role.contains("coder") || role.contains("writer") {
        return WaveMode::Write;
    }
    if role.contains("researcher") || role.contains("read-only") || role.contains("readonly") {
        return WaveMode::ReadOnly;
    }
    if role.contains("reviewer") || role.contains("auditor") {
        return WaveMode::Reviewer;
    }
    if role.contains("verifier") || role.contains("tester") {
        return WaveMode::Verifier;
    }
    if role.contains("monitor") {
        return WaveMode::Monitor;
    }

    let text = body.to_ascii_lowercase();
    if text.contains("read-only") || text.contains("readonly") || text.contains("researcher") {
        WaveMode::ReadOnly
    } else if text.contains("reviewer") || text.contains("auditor") {
        WaveMode::Reviewer
    } else if text.contains("verifier") || text.contains("tester") || text.contains("regression") {
        WaveMode::Verifier
    } else if text.contains("monitor") {
        WaveMode::Monitor
    } else if text.contains("draft-only") || text.contains("propose_patch") {
        WaveMode::DraftOnly
    } else if text.contains("coder") || text.contains("modify") || text.contains("write") {
        WaveMode::Write
    } else {
        WaveMode::ReadOnly
    }
}

fn infer_wave_status(session_text: &str, body: &str) -> Option<WaveStatus> {
    let body_lower = body.to_ascii_lowercase();
    if body_lower.contains("**verdict:** ship") || body_lower.contains("verdict: ship") {
        return Some(WaveStatus::Done);
    }
    if body_lower.contains("needs_fix") || body_lower.contains("needs-fix") {
        return Some(WaveStatus::NeedsReview);
    }
    if body_lower.contains("blocker") || body_lower.contains("blocked") {
        return Some(WaveStatus::Blocked);
    }

    let session_status = frontmatter_value(session_text, "status")?.to_ascii_uppercase();
    match session_status.as_str() {
        "IN_PROGRESS" => Some(WaveStatus::Running),
        "COMPLETED" => Some(WaveStatus::Done),
        "PARTIAL" => Some(WaveStatus::NeedsReview),
        "FAILED" => Some(WaveStatus::Blocked),
        _ => None,
    }
}

fn infer_blast_radius(session_text: &str, body: &str) -> BlastRadius {
    let text = format!("{session_text}\n{body}").to_ascii_lowercase();
    if text.contains("danger overlap") || text.contains("same-file conflict") {
        BlastRadius::Danger
    } else if text.contains("minor overlap") {
        BlastRadius::Minor
    } else if text.contains("no blast-radius overlap") || text.contains("no overlap") {
        BlastRadius::None
    } else {
        BlastRadius::Unknown
    }
}

fn frontmatter_value(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        if line.trim() == "---" {
            return None;
        }
        let Some((candidate_key, value)) = line.split_once(':') else {
            continue;
        };
        if candidate_key.trim() == key {
            return Some(value.split('#').next().unwrap_or("").trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_empty_optional_fields_and_caps_report_completion() {
        let contract = WaveContract {
            title: "  Wave 2  ".into(),
            pane_id: Some("  ".into()),
            mode: WaveMode::ReadOnly,
            status: None,
            lifecycle_lane: None,
            dependency: Some("  ".into()),
            report: WaveReportGate {
                completed_fields: 12,
                required_fields: 10,
                completed_items: Vec::new(),
            },
            blast_radius: BlastRadius::None,
            prompt_delivery: None,
            arcs: vec![
                WaveArc {
                    id: " B ".into(),
                    summary: " diagnose ".into(),
                    status: None,
                },
                WaveArc {
                    id: " ".into(),
                    summary: " ".into(),
                    status: None,
                },
            ],
        }
        .normalized()
        .expect("title is present");

        assert_eq!(contract.title, "Wave 2");
        assert_eq!(contract.pane_id, None);
        assert_eq!(contract.dependency, None);
        assert_eq!(contract.report.completed_fields, 10);
        assert_eq!(contract.arcs.len(), 1);
        assert_eq!(contract.arcs[0].id, "B");
    }

    #[test]
    fn border_label_includes_mode_packet_blast_radius_and_dependency() {
        let contract = WaveContract {
            title: "Wave 1: proof docs".into(),
            pane_id: None,
            mode: WaveMode::Write,
            status: Some(WaveStatus::Running),
            lifecycle_lane: None,
            dependency: Some("parallel OK".into()),
            report: WaveReportGate {
                completed_fields: 4,
                required_fields: 10,
                completed_items: Vec::new(),
            },
            blast_radius: BlastRadius::None,
            prompt_delivery: None,
            arcs: Vec::new(),
        };

        assert_eq!(
            contract.border_label(),
            "Wave 1: proof docs | write | lane running | packet 4/10 | no overlap | parallel OK"
        );
    }

    #[test]
    fn lifecycle_lane_persists_and_falls_back_to_contract_state() {
        let explicit = WaveContract {
            title: "Wave 3: verifier".into(),
            pane_id: None,
            mode: WaveMode::Verifier,
            status: Some(WaveStatus::Done),
            lifecycle_lane: Some(WaveLifecycleLane::ParentReview),
            dependency: None,
            report: WaveReportGate {
                completed_fields: 2,
                required_fields: 2,
                completed_items: Vec::new(),
            },
            blast_radius: BlastRadius::Unknown,
            prompt_delivery: None,
            arcs: Vec::new(),
        };

        assert_eq!(
            explicit.effective_lifecycle_lane(),
            WaveLifecycleLane::ParentReview
        );
        assert_eq!(
            serde_json::to_value(&explicit).unwrap()["lifecycle_lane"],
            "parent_review"
        );

        let missing_packet = WaveContract {
            lifecycle_lane: None,
            status: Some(WaveStatus::Done),
            report: WaveReportGate {
                completed_fields: 1,
                required_fields: 2,
                completed_items: Vec::new(),
            },
            ..explicit
        };

        assert_eq!(
            missing_packet.effective_lifecycle_lane(),
            WaveLifecycleLane::NeedsPacket
        );
        assert_eq!(
            missing_packet.lifecycle_lane_for_status(WaveStatus::Accepted),
            WaveLifecycleLane::Accepted
        );
    }

    #[test]
    fn parses_raw_json_contract_document() {
        let contract = parse_wave_contract_document(
            r#"{
                "title": " Wave 1: proof docs ",
                "mode": "write",
                "dependency": " parallel OK ",
                "report": {"completed_fields": 4, "required_fields": 10},
                "blast_radius": "none"
            }"#,
            None,
        )
        .expect("json contract parses");

        assert_eq!(contract.title, "Wave 1: proof docs");
        assert_eq!(contract.mode, WaveMode::Write);
        assert_eq!(contract.report.label(), "4/10");
        assert_eq!(contract.dependency.as_deref(), Some("parallel OK"));
    }

    #[test]
    fn parses_fenced_json_contract_document() {
        let markdown = r#"
# Mission note

```herdr-wave-contract
{"title":"Wave 2: stale-binary research","mode":"read_only","report":{"completed_fields":1,"required_fields":10}}
```
"#;

        let contract =
            parse_wave_contract_document(markdown, None).expect("fenced contract parses");

        assert_eq!(contract.title, "Wave 2: stale-binary research");
        assert_eq!(contract.mode, WaveMode::ReadOnly);
        assert_eq!(contract.report.label(), "1/10");
    }

    #[test]
    fn derives_contract_from_foxflow_session_worker_section() {
        let markdown = r#"---
status: IN_PROGRESS
authorized_files:
  - crates/example/src/lib.rs
---

```px-session-contract
{
  "actors": {
    "W1": {
      "role": "worker",
      "required_sections": [
        "Deliverables",
        "Things I Noticed",
        "Brief Critique",
        "Self-TM"
      ]
    }
  }
}
```

# WAVE-TEST

## Worker 1: W1-proof-docs (coder)

### Deliverables
Done.

### Things I Noticed
- useful thing

### Brief Critique
88/100

### Self-TM
**Verdict:** SHIP
"#;

        let contract =
            parse_wave_contract_document(markdown, Some("W1")).expect("session wave derives");

        assert_eq!(contract.title, "W1-proof-docs");
        assert_eq!(contract.mode, WaveMode::Write);
        assert_eq!(contract.status, Some(WaveStatus::Done));
        assert_eq!(contract.report.label(), "4/4");
        assert_eq!(
            contract.report.completed_items,
            vec![
                "Deliverables".to_string(),
                "Things I Noticed".to_string(),
                "Brief Critique".to_string(),
                "Self-TM".to_string()
            ]
        );
        assert_eq!(contract.blast_radius, BlastRadius::Unknown);
    }

    #[test]
    fn derives_all_contracts_from_px_session_contract_actors() {
        let markdown = r#"
```px-session-contract
{
  "actors": {
    "W1": {"role": "worker", "required_sections": ["Deliverables", "Self-TM"]},
    "W2": {"role": "worker", "required_sections": ["Deliverables", "Self-TM"]}
  }
}
```

## Worker 1: W1-proof-docs (coder)

### Deliverables
Done.

### Self-TM
Verdict: SHIP

## Worker 2: W2-stale-binary (researcher)

### Deliverables
Found stale build path.
"#;

        let contracts = parse_session_wave_contracts(markdown).expect("contracts derive");

        assert_eq!(contracts.len(), 2);
        assert_eq!(contracts[0].title, "W1-proof-docs");
        assert_eq!(contracts[0].mode, WaveMode::Write);
        assert_eq!(contracts[0].report.label(), "2/2");
        assert_eq!(contracts[1].title, "W2-stale-binary");
        assert_eq!(contracts[1].mode, WaveMode::ReadOnly);
        assert_eq!(contracts[1].report.label(), "1/2");
    }

    #[test]
    fn derives_contracts_from_recommended_dispatch_shape() {
        let markdown = r#"
Recommended dispatch shape (if you greenlight)

Wave 1 — parallel, no blast-radius overlap (~30 min total):
- A: docs/customers/propelis/proof-packs/tier-v-2026-05-22.md (NEW file, single coder, redacted artifact + per-question proof rows + receipt IDs)
- D: docs/customers/propelis/demo-script-2026.md (NEW file, single coder, sales-safe claim language + product boundary callout)

Wave 2 — sequential after A+D (~20 min):
- B: researcher arc (read-only) — diagnose stale-binary. If finding is "cargo cache didn't rebuild" -> ship a defensive cargo clean step in restart script.
"#;

        let contracts = parse_session_wave_contracts(markdown).expect("dispatch shape derives");

        assert_eq!(contracts.len(), 2);
        assert_eq!(
            contracts[0].title,
            "Wave 1: parallel, no blast-radius overlap"
        );
        assert_eq!(contracts[0].mode, WaveMode::Write);
        assert_eq!(contracts[0].blast_radius, BlastRadius::None);
        assert_eq!(contracts[0].status, Some(WaveStatus::Queued));
        assert_eq!(contracts[0].dependency.as_deref(), Some("parallel"));
        assert_eq!(contracts[0].arcs.len(), 2);
        assert_eq!(contracts[0].arcs[0].id, "A");
        assert!(contracts[0].arcs[0]
            .summary
            .contains("tier-v-2026-05-22.md"));
        assert_eq!(contracts[0].arcs[1].id, "D");

        assert_eq!(contracts[1].title, "Wave 2: sequential after A+D");
        assert_eq!(contracts[1].mode, WaveMode::ReadOnly);
        assert_eq!(contracts[1].dependency.as_deref(), Some("after A+D"));
        assert_eq!(contracts[1].arcs.len(), 1);
        assert_eq!(contracts[1].arcs[0].id, "B");
    }

    #[test]
    fn report_gate_dedupes_items_and_counts_them() {
        let report = WaveReportGate {
            completed_fields: 9,
            required_fields: 3,
            completed_items: vec![
                " Evidence / receipts ".into(),
                "evidence / receipts".into(),
                "Commands run".into(),
                "".into(),
                "Risks / unknowns".into(),
                "Recommendation".into(),
            ],
        }
        .normalized();

        assert_eq!(report.completed_fields, 3);
        assert_eq!(
            report.completed_items,
            vec![
                "Evidence / receipts".to_string(),
                "Commands run".to_string(),
                "Risks / unknowns".to_string()
            ]
        );
    }

    #[test]
    fn default_report_packet_items_use_wave_language() {
        assert!(default_report_packet_items().contains(&"Next wave suggestion"));
        assert!(!default_report_packet_items().contains(&"Next child suggestion"));
    }

    #[test]
    fn report_gate_accepts_legacy_next_child_suggestion_marker() {
        let report = derive_default_report_gate(
            "What I did: checked the pane\n\
             Next child suggestion: start verifier after packet acceptance\n",
        );

        assert_eq!(report.label(), "2/10");
        assert!(report
            .completed_items
            .iter()
            .any(|item| item == "Next wave suggestion"));
    }

    #[test]
    fn derives_default_report_gate_from_child_output_headings() {
        let report = derive_default_report_gate(
            r#"
Report packet

### What I did
Read the deployment logs and narrowed the crash path.

**Evidence / receipts**
- railway log receipt 123

Files changed:
none

Good / bad / ugly:
Good: scope stayed narrow.
"#,
        );

        assert_eq!(report.label(), "4/10");
        assert_eq!(
            report.completed_items,
            vec![
                "What I did".to_string(),
                "Evidence / receipts".to_string(),
                "Files changed".to_string(),
                "Good / bad / ugly".to_string()
            ]
        );
    }

    #[test]
    fn default_report_gate_ignores_prompt_lists_without_field_markers() {
        let report = derive_default_report_gate(
            "Required report fields: What I did, Evidence / receipts, Commands run",
        );

        assert_eq!(report.label(), "0/10");
        assert!(report.completed_items.is_empty());
    }
}
