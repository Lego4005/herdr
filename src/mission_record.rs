use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug)]
pub enum MissionRecordError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Json(serde_json::Error),
    MissingMission,
    InvalidInput(String),
}

impl fmt::Display for MissionRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Sql(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::MissingMission => write!(f, "no mission record found"),
            Self::InvalidInput(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for MissionRecordError {}

impl From<std::io::Error> for MissionRecordError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<rusqlite::Error> for MissionRecordError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sql(err)
    }
}

impl From<serde_json::Error> for MissionRecordError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

pub type Result<T> = std::result::Result<T, MissionRecordError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionSnapshot {
    pub id: i64,
    pub project_root: String,
    pub title: String,
    pub markdown_path: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionChild {
    pub mission_id: i64,
    pub pane_id: String,
    pub terminal_id: Option<String>,
    pub provider: String,
    pub contract_json: Option<String>,
    pub status: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionEvent {
    pub id: i64,
    pub mission_id: i64,
    pub pane_id: Option<String>,
    pub provider: String,
    pub kind: String,
    pub text: Option<String>,
    pub payload: Value,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionPacket {
    pub mission_id: i64,
    pub pane_id: String,
    pub fields: Map<String, Value>,
    pub completed_fields: usize,
    pub ready: bool,
    pub audit_score: Option<i64>,
    pub audit_verdict: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionRecordView {
    pub mission: MissionSnapshot,
    pub children: Vec<MissionChild>,
    pub packets: Vec<MissionPacket>,
    pub events: Vec<MissionEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionEventInput {
    pub mission_id: Option<i64>,
    pub pane_id: Option<String>,
    pub provider: String,
    pub kind: String,
    pub text: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionEventQuery {
    pub mission_id: Option<i64>,
    pub pane_id: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionAdapterEvent {
    pub provider: String,
    pub kind: String,
    pub text: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct MissionStore {
    db_path: PathBuf,
}

impl MissionStore {
    pub fn default_path() -> PathBuf {
        crate::session::data_dir().join("mission-records.sqlite3")
    }

    pub fn default_store() -> Result<Self> {
        Self::open(Self::default_path())
    }

    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let db_path = path.into();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let store = Self { db_path };
        store.with_connection(|connection| {
            init_schema(connection)?;
            Ok(())
        })?;
        Ok(store)
    }

    fn with_connection<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let connection = Connection::open(&self.db_path)?;
        f(&connection)
    }

    #[cfg(test)]
    pub fn table_names(&self) -> Result<Vec<String>> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(MissionRecordError::from)
        })
    }

    pub fn init_mission(&self, project_root: &Path, title: &str) -> Result<MissionSnapshot> {
        let markdown_path = default_markdown_path(project_root, title)?;
        self.init_mission_with_path(project_root, title, &markdown_path)
    }

    pub fn init_mission_with_path(
        &self,
        project_root: &Path,
        title: &str,
        markdown_path: &Path,
    ) -> Result<MissionSnapshot> {
        let title = clean_required(title, "mission title")?;
        let project_root = project_root.display().to_string();
        let markdown_path = markdown_path.display().to_string();
        let now = now_string();
        let mission = self.with_connection(|connection| {
            if let Some(existing) = mission_by_markdown_path(connection, &markdown_path)? {
                return Ok(existing);
            }
            connection.execute(
                "INSERT INTO missions (project_root, title, markdown_path, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'active', ?4, ?4)",
                params![project_root, title, markdown_path, now],
            )?;
            let id = connection.last_insert_rowid();
            mission_by_id(connection, id)?.ok_or(MissionRecordError::MissingMission)
        })?;
        self.render_mission_markdown(mission.id)?;
        Ok(mission)
    }

    pub fn upsert_child(
        &self,
        mission_id: i64,
        pane_id: &str,
        terminal_id: Option<&str>,
        provider: &str,
        contract_json: Option<&str>,
        status: &str,
    ) -> Result<MissionChild> {
        let pane_id = clean_required(pane_id, "pane id")?;
        let provider = normalize_provider(provider);
        let status = clean_required(status, "child status")?;
        let now = now_string();
        let child = self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO mission_children
                 (mission_id, pane_id, terminal_id, provider, contract_json, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                 ON CONFLICT(mission_id, pane_id) DO UPDATE SET
                   terminal_id=excluded.terminal_id,
                   provider=excluded.provider,
                   contract_json=excluded.contract_json,
                   status=excluded.status,
                   updated_at=excluded.updated_at",
                params![mission_id, pane_id, terminal_id, provider, contract_json, status, now],
            )?;
            child_by_pane(connection, mission_id, &pane_id)?
                .ok_or(MissionRecordError::MissingMission)
        })?;
        self.render_mission_markdown(mission_id)?;
        Ok(child)
    }

    pub fn record_event(&self, input: MissionEventInput) -> Result<MissionEvent> {
        let mission_id = self.resolve_mission_id(input.mission_id, input.pane_id.as_deref())?;
        let provider = normalize_provider(&input.provider);
        let kind = clean_required(&input.kind, "event kind")?;
        let payload_json = serde_json::to_string(&input.payload)?;
        let now = now_string();
        let event = self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO mission_events
                 (mission_id, pane_id, provider, kind, text, payload_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    mission_id,
                    input.pane_id,
                    provider,
                    kind,
                    input.text,
                    payload_json,
                    now
                ],
            )?;
            let id = connection.last_insert_rowid();
            event_by_id(connection, id)?.ok_or(MissionRecordError::MissingMission)
        })?;
        self.render_mission_markdown(mission_id)?;
        Ok(event)
    }

    pub fn update_packet_field(
        &self,
        mission_id: i64,
        pane_id: &str,
        field: &str,
        text: &str,
    ) -> Result<MissionPacket> {
        let pane_id = clean_required(pane_id, "pane id")?;
        let field = clean_required(field, "packet field")?;
        let now = now_string();
        let packet = self.with_connection(|connection| {
            let mut fields = packet_fields(connection, mission_id, &pane_id)?;
            fields.insert(field.clone(), Value::String(text.to_string()));
            let fields_json = serde_json::to_string(&fields)?;
            connection.execute(
                "INSERT INTO mission_packets
                 (mission_id, pane_id, fields_json, ready, updated_at)
                 VALUES (?1, ?2, ?3, 0, ?4)
                 ON CONFLICT(mission_id, pane_id) DO UPDATE SET
                   fields_json=excluded.fields_json,
                   updated_at=excluded.updated_at",
                params![mission_id, pane_id, fields_json, now],
            )?;
            packet_by_pane(connection, mission_id, &pane_id)?
                .ok_or(MissionRecordError::MissingMission)
        })?;
        self.record_event(MissionEventInput {
            mission_id: Some(mission_id),
            pane_id: Some(pane_id),
            provider: "generic".into(),
            kind: "report_field".into(),
            text: Some(format!("{field}: {text}")),
            payload: serde_json::json!({ "field": field, "text": text }),
        })?;
        Ok(packet)
    }

    pub fn mark_packet_ready(&self, mission_id: i64, pane_id: &str) -> Result<MissionPacket> {
        let pane_id = clean_required(pane_id, "pane id")?;
        let now = now_string();
        let packet = self.with_connection(|connection| {
            let fields = packet_fields(connection, mission_id, &pane_id)?;
            let fields_json = serde_json::to_string(&fields)?;
            connection.execute(
                "INSERT INTO mission_packets
                 (mission_id, pane_id, fields_json, ready, updated_at)
                 VALUES (?1, ?2, ?3, 1, ?4)
                 ON CONFLICT(mission_id, pane_id) DO UPDATE SET
                   ready=1,
                   updated_at=excluded.updated_at",
                params![mission_id, pane_id, fields_json, now],
            )?;
            packet_by_pane(connection, mission_id, &pane_id)?
                .ok_or(MissionRecordError::MissingMission)
        })?;
        self.record_event(MissionEventInput {
            mission_id: Some(mission_id),
            pane_id: Some(pane_id),
            provider: "generic".into(),
            kind: "packet_ready".into(),
            text: Some("packet marked ready".into()),
            payload: serde_json::json!({ "ready": true }),
        })?;
        Ok(packet)
    }

    pub fn record_audit(
        &self,
        mission_id: i64,
        pane_id: &str,
        score: i64,
        verdict: &str,
    ) -> Result<MissionPacket> {
        let pane_id = clean_required(pane_id, "pane id")?;
        let verdict = clean_required(verdict, "audit verdict")?;
        let now = now_string();
        let packet = self.with_connection(|connection| {
            let fields = packet_fields(connection, mission_id, &pane_id)?;
            let fields_json = serde_json::to_string(&fields)?;
            connection.execute(
                "INSERT INTO mission_packets
                 (mission_id, pane_id, fields_json, ready, audit_score, audit_verdict, updated_at)
                 VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)
                 ON CONFLICT(mission_id, pane_id) DO UPDATE SET
                   audit_score=excluded.audit_score,
                   audit_verdict=excluded.audit_verdict,
                   updated_at=excluded.updated_at",
                params![mission_id, pane_id, fields_json, score, verdict, now],
            )?;
            packet_by_pane(connection, mission_id, &pane_id)?
                .ok_or(MissionRecordError::MissingMission)
        })?;
        self.record_event(MissionEventInput {
            mission_id: Some(mission_id),
            pane_id: Some(pane_id),
            provider: "generic".into(),
            kind: "audit".into(),
            text: Some(format!("{score}: {verdict}")),
            payload: serde_json::json!({ "score": score, "verdict": verdict }),
        })?;
        Ok(packet)
    }

    pub fn list_events(&self, query: MissionEventQuery) -> Result<Vec<MissionEvent>> {
        let mission_id = match query.mission_id {
            Some(id) => Some(id),
            None if query.pane_id.is_some() => {
                self.resolve_mission_id(None, query.pane_id.as_deref()).ok()
            }
            None => None,
        };
        let limit = query.limit.unwrap_or(100).min(500) as i64;
        self.with_connection(|connection| match (mission_id, query.pane_id.as_deref()) {
            (Some(mission_id), Some(pane_id)) => query_events(
                connection,
                "WHERE mission_id = ?1 AND pane_id = ?2",
                params![mission_id, pane_id],
                limit,
            ),
            (Some(mission_id), None) => query_events(
                connection,
                "WHERE mission_id = ?1",
                params![mission_id],
                limit,
            ),
            (None, Some(pane_id)) => {
                query_events(connection, "WHERE pane_id = ?1", params![pane_id], limit)
            }
            (None, None) => query_events(connection, "", rusqlite::params![], limit),
        })
    }

    pub fn packet_for_pane(&self, pane_id: &str) -> Result<Option<MissionPacket>> {
        self.with_connection(|connection| {
            let Some(mission_id) = mission_id_for_pane(connection, pane_id)? else {
                return Ok(None);
            };
            packet_by_pane(connection, mission_id, pane_id)
        })
    }

    pub fn get_mission(&self, mission_id: Option<i64>) -> Result<MissionRecordView> {
        let mission = self.with_connection(|connection| {
            let mission = match mission_id {
                Some(id) => mission_by_id(connection, id)?,
                None => latest_mission(connection)?,
            };
            mission.ok_or(MissionRecordError::MissingMission)
        })?;
        self.get_mission_by_id(mission.id)
    }

    pub fn get_mission_by_id(&self, mission_id: i64) -> Result<MissionRecordView> {
        self.with_connection(|connection| {
            let mission =
                mission_by_id(connection, mission_id)?.ok_or(MissionRecordError::MissingMission)?;
            Ok(MissionRecordView {
                children: children_for_mission(connection, mission_id)?,
                packets: packets_for_mission(connection, mission_id)?,
                events: events_for_mission(connection, mission_id, 200)?,
                mission,
            })
        })
    }

    pub fn resolve_mission_id(
        &self,
        mission_id: Option<i64>,
        pane_id: Option<&str>,
    ) -> Result<i64> {
        if let Some(mission_id) = mission_id {
            return Ok(mission_id);
        }
        self.with_connection(|connection| {
            if let Some(pane_id) = pane_id {
                if let Some(id) = mission_id_for_pane(connection, pane_id)? {
                    return Ok(id);
                }
            }
            latest_mission(connection)?
                .map(|mission| mission.id)
                .ok_or(MissionRecordError::MissingMission)
        })
    }

    fn render_mission_markdown(&self, mission_id: i64) -> Result<()> {
        let view = self.get_mission_by_id(mission_id)?;
        write_markdown(&view)
    }
}

pub fn adapter_event_from_input(
    provider: &str,
    kind: &str,
    payload: Value,
) -> Result<MissionAdapterEvent> {
    let provider = normalize_provider(provider);
    let kind = clean_required(kind, "adapter event kind")?;
    let text = adapter_text(&provider, &kind, &payload);
    Ok(MissionAdapterEvent {
        provider,
        kind,
        text,
        payload,
    })
}

fn init_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS missions (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          project_root TEXT NOT NULL,
          title TEXT NOT NULL,
          markdown_path TEXT NOT NULL UNIQUE,
          status TEXT NOT NULL,
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS mission_children (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          mission_id INTEGER NOT NULL REFERENCES missions(id) ON DELETE CASCADE,
          pane_id TEXT NOT NULL,
          terminal_id TEXT,
          provider TEXT NOT NULL,
          contract_json TEXT,
          status TEXT NOT NULL,
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL,
          UNIQUE(mission_id, pane_id)
        );
        CREATE TABLE IF NOT EXISTS mission_events (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          mission_id INTEGER NOT NULL REFERENCES missions(id) ON DELETE CASCADE,
          pane_id TEXT,
          provider TEXT NOT NULL,
          kind TEXT NOT NULL,
          text TEXT,
          payload_json TEXT NOT NULL,
          created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS mission_packets (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          mission_id INTEGER NOT NULL REFERENCES missions(id) ON DELETE CASCADE,
          pane_id TEXT NOT NULL,
          fields_json TEXT NOT NULL,
          ready INTEGER NOT NULL DEFAULT 0,
          audit_score INTEGER,
          audit_verdict TEXT,
          updated_at TEXT NOT NULL,
          UNIQUE(mission_id, pane_id)
        );
        ",
    )?;
    Ok(())
}

fn default_markdown_path(project_root: &Path, title: &str) -> Result<PathBuf> {
    let (year, month, day, hour) = utc_parts(SystemTime::now());
    let slug = slugify(title);
    Ok(project_root
        .join(".sessions")
        .join(format!("{year:04}"))
        .join(format!("{month:02}"))
        .join(format!("{year:04}-{month:02}-{day:02}-{hour:02}-{slug}.md")))
}

fn write_markdown(view: &MissionRecordView) -> Result<()> {
    let path = PathBuf::from(&view.mission.markdown_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let markdown = render_markdown(view);
    std::fs::write(path, markdown)?;
    Ok(())
}

fn render_markdown(view: &MissionRecordView) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", view.mission.title));
    out.push_str("## Mission\n\n");
    out.push_str(&format!("- id: {}\n", view.mission.id));
    out.push_str(&format!("- project: {}\n", view.mission.project_root));
    out.push_str(&format!("- status: {}\n", view.mission.status));
    out.push_str(&format!("- created: {}\n", view.mission.created_at));
    out.push_str(&format!("- updated: {}\n\n", view.mission.updated_at));

    out.push_str("## Child sessions\n\n");
    if view.children.is_empty() {
        out.push_str("- none yet\n\n");
    } else {
        for child in &view.children {
            out.push_str(&format!(
                "- `{}` provider={} status={} terminal={}\n",
                child.pane_id,
                child.provider,
                child.status,
                child.terminal_id.as_deref().unwrap_or("none")
            ));
        }
        out.push('\n');
    }

    out.push_str("## Assignments\n\n");
    let assignment_events: Vec<_> = view
        .events
        .iter()
        .filter(|event| event.kind == "assignment")
        .collect();
    if assignment_events.is_empty() {
        out.push_str("- none yet\n\n");
    } else {
        for event in assignment_events {
            out.push_str(&format!(
                "- [{}] `{}` {}\n",
                event.created_at,
                event.pane_id.as_deref().unwrap_or("mission"),
                event.text.as_deref().unwrap_or("")
            ));
        }
        out.push('\n');
    }

    out.push_str("## Report packets\n\n");
    if view.packets.is_empty() {
        out.push_str("- none yet\n\n");
    } else {
        for packet in &view.packets {
            out.push_str(&format!(
                "### `{}`\n\n- ready: {}\n- completed_fields: {}\n",
                packet.pane_id, packet.ready, packet.completed_fields
            ));
            if let Some(score) = packet.audit_score {
                out.push_str(&format!("- audit_score: {score}\n"));
            }
            if let Some(verdict) = packet.audit_verdict.as_deref() {
                out.push_str(&format!("- audit_verdict: {verdict}\n"));
            }
            for (field, value) in &packet.fields {
                out.push_str(&format!(
                    "- {}: {}\n",
                    field,
                    value.as_str().unwrap_or(&value.to_string())
                ));
            }
            out.push('\n');
        }
    }

    out.push_str("## Audit\n\n");
    let audit_events: Vec<_> = view
        .events
        .iter()
        .filter(|event| event.kind == "audit")
        .collect();
    if audit_events.is_empty() {
        out.push_str("- none yet\n\n");
    } else {
        for event in audit_events {
            out.push_str(&format!(
                "- [{}] `{}` {}\n",
                event.created_at,
                event.pane_id.as_deref().unwrap_or("mission"),
                event.text.as_deref().unwrap_or("")
            ));
        }
        out.push('\n');
    }

    out.push_str("## Timeline\n\n");
    if view.events.is_empty() {
        out.push_str("- none yet\n\n");
    } else {
        for event in &view.events {
            out.push_str(&format!(
                "- [{}] {}/{} `{}` {}\n",
                event.created_at,
                event.provider,
                event.kind,
                event.pane_id.as_deref().unwrap_or("mission"),
                event.text.as_deref().unwrap_or("")
            ));
        }
        out.push('\n');
    }

    out.push_str("## Adapter events\n\n");
    let adapter_events: Vec<_> = view
        .events
        .iter()
        .filter(|event| event.kind.starts_with("subagent_") || event.kind.starts_with("adapter_"))
        .collect();
    if adapter_events.is_empty() {
        out.push_str("- none yet\n");
    } else {
        for event in adapter_events {
            out.push_str(&format!(
                "- [{}] {} `{}` {}\n",
                event.created_at,
                event.provider,
                event.pane_id.as_deref().unwrap_or("mission"),
                event.text.as_deref().unwrap_or("")
            ));
        }
    }

    out
}

fn mission_by_id(connection: &Connection, id: i64) -> Result<Option<MissionSnapshot>> {
    connection
        .query_row(
            "SELECT id, project_root, title, markdown_path, status, created_at, updated_at
             FROM missions WHERE id = ?1",
            params![id],
            row_to_mission,
        )
        .optional()
        .map_err(MissionRecordError::from)
}

fn mission_by_markdown_path(
    connection: &Connection,
    markdown_path: &str,
) -> Result<Option<MissionSnapshot>> {
    connection
        .query_row(
            "SELECT id, project_root, title, markdown_path, status, created_at, updated_at
             FROM missions WHERE markdown_path = ?1",
            params![markdown_path],
            row_to_mission,
        )
        .optional()
        .map_err(MissionRecordError::from)
}

fn latest_mission(connection: &Connection) -> Result<Option<MissionSnapshot>> {
    connection
        .query_row(
            "SELECT id, project_root, title, markdown_path, status, created_at, updated_at
             FROM missions ORDER BY id DESC LIMIT 1",
            [],
            row_to_mission,
        )
        .optional()
        .map_err(MissionRecordError::from)
}

fn mission_id_for_pane(connection: &Connection, pane_id: &str) -> Result<Option<i64>> {
    connection
        .query_row(
            "SELECT mission_id FROM (
               SELECT mission_id, updated_at FROM mission_children WHERE pane_id = ?1
               UNION ALL
               SELECT mission_id, updated_at FROM mission_packets WHERE pane_id = ?1
             ) ORDER BY updated_at DESC LIMIT 1",
            params![pane_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(MissionRecordError::from)
}

fn child_by_pane(
    connection: &Connection,
    mission_id: i64,
    pane_id: &str,
) -> Result<Option<MissionChild>> {
    connection
        .query_row(
            "SELECT mission_id, pane_id, terminal_id, provider, contract_json, status, updated_at
             FROM mission_children WHERE mission_id = ?1 AND pane_id = ?2",
            params![mission_id, pane_id],
            row_to_child,
        )
        .optional()
        .map_err(MissionRecordError::from)
}

fn children_for_mission(connection: &Connection, mission_id: i64) -> Result<Vec<MissionChild>> {
    let mut statement = connection.prepare(
        "SELECT mission_id, pane_id, terminal_id, provider, contract_json, status, updated_at
         FROM mission_children WHERE mission_id = ?1 ORDER BY id",
    )?;
    let rows = statement.query_map(params![mission_id], row_to_child)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(MissionRecordError::from)
}

fn event_by_id(connection: &Connection, id: i64) -> Result<Option<MissionEvent>> {
    connection
        .query_row(
            "SELECT id, mission_id, pane_id, provider, kind, text, payload_json, created_at
             FROM mission_events WHERE id = ?1",
            params![id],
            row_to_event,
        )
        .optional()
        .map_err(MissionRecordError::from)
}

fn events_for_mission(
    connection: &Connection,
    mission_id: i64,
    limit: i64,
) -> Result<Vec<MissionEvent>> {
    query_events(
        connection,
        "WHERE mission_id = ?1",
        params![mission_id],
        limit,
    )
}

fn query_events<P>(
    connection: &Connection,
    where_clause: &str,
    params: P,
    limit: i64,
) -> Result<Vec<MissionEvent>>
where
    P: rusqlite::Params,
{
    let sql = format!(
        "SELECT id, mission_id, pane_id, provider, kind, text, payload_json, created_at
         FROM mission_events {where_clause} ORDER BY id DESC LIMIT {limit}"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params, row_to_event)?;
    let mut events = rows
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(MissionRecordError::from)?;
    events.reverse();
    Ok(events)
}

fn packet_fields(
    connection: &Connection,
    mission_id: i64,
    pane_id: &str,
) -> Result<Map<String, Value>> {
    let raw = connection
        .query_row(
            "SELECT fields_json FROM mission_packets WHERE mission_id = ?1 AND pane_id = ?2",
            params![mission_id, pane_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match raw {
        Some(raw) => {
            serde_json::from_str::<Map<String, Value>>(&raw).map_err(MissionRecordError::from)
        }
        None => Ok(Map::new()),
    }
}

fn packet_by_pane(
    connection: &Connection,
    mission_id: i64,
    pane_id: &str,
) -> Result<Option<MissionPacket>> {
    connection
        .query_row(
            "SELECT mission_id, pane_id, fields_json, ready, audit_score, audit_verdict, updated_at
             FROM mission_packets WHERE mission_id = ?1 AND pane_id = ?2",
            params![mission_id, pane_id],
            row_to_packet,
        )
        .optional()
        .map_err(MissionRecordError::from)
}

fn packets_for_mission(connection: &Connection, mission_id: i64) -> Result<Vec<MissionPacket>> {
    let mut statement = connection.prepare(
        "SELECT mission_id, pane_id, fields_json, ready, audit_score, audit_verdict, updated_at
         FROM mission_packets WHERE mission_id = ?1 ORDER BY id",
    )?;
    let rows = statement.query_map(params![mission_id], row_to_packet)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(MissionRecordError::from)
}

fn row_to_mission(row: &rusqlite::Row<'_>) -> rusqlite::Result<MissionSnapshot> {
    Ok(MissionSnapshot {
        id: row.get(0)?,
        project_root: row.get(1)?,
        title: row.get(2)?,
        markdown_path: row.get(3)?,
        status: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn row_to_child(row: &rusqlite::Row<'_>) -> rusqlite::Result<MissionChild> {
    Ok(MissionChild {
        mission_id: row.get(0)?,
        pane_id: row.get(1)?,
        terminal_id: row.get(2)?,
        provider: row.get(3)?,
        contract_json: row.get(4)?,
        status: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<MissionEvent> {
    let payload_json: String = row.get(6)?;
    let payload = serde_json::from_str(&payload_json).unwrap_or(Value::Null);
    Ok(MissionEvent {
        id: row.get(0)?,
        mission_id: row.get(1)?,
        pane_id: row.get(2)?,
        provider: row.get(3)?,
        kind: row.get(4)?,
        text: row.get(5)?,
        payload,
        created_at: row.get(7)?,
    })
}

fn row_to_packet(row: &rusqlite::Row<'_>) -> rusqlite::Result<MissionPacket> {
    let fields_json: String = row.get(2)?;
    let fields = serde_json::from_str::<Map<String, Value>>(&fields_json).unwrap_or_default();
    let completed_fields = fields.len();
    Ok(MissionPacket {
        mission_id: row.get(0)?,
        pane_id: row.get(1)?,
        fields,
        completed_fields,
        ready: row.get::<_, i64>(3)? != 0,
        audit_score: row.get(4)?,
        audit_verdict: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn clean_required(value: &str, label: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(MissionRecordError::InvalidInput(format!(
            "{label} is required"
        )));
    }
    Ok(value.to_string())
}

fn normalize_provider(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "claude" => "claude".into(),
        "codex" => "codex".into(),
        "fdag" | "px" => "fdag".into(),
        "generic" | "" => "generic".into(),
        _ => "generic".into(),
    }
}

fn adapter_text(provider: &str, kind: &str, payload: &Value) -> Option<String> {
    let role = payload.get("role").and_then(Value::as_str);
    let agent_id = payload
        .get("agent_id")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_str);
    let summary = payload
        .get("summary")
        .or_else(|| payload.get("text"))
        .and_then(Value::as_str);

    match (provider, kind, role, agent_id, summary) {
        ("claude", kind, Some(role), Some(agent_id), _) if kind.starts_with("subagent_") => {
            Some(format!("{role} {agent_id}"))
        }
        (_, _, _, _, Some(summary)) => Some(summary.to_string()),
        ("codex", kind, _, _, _) => Some(format!("codex {kind}")),
        ("fdag", kind, _, _, _) => Some(format!("fdag {kind}")),
        _ => None,
    }
}

fn now_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "mission".into()
    } else {
        slug.to_string()
    }
}

fn utc_parts(time: SystemTime) -> (i32, u32, u32, u32) {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let hour = seconds.rem_euclid(86_400) / 3_600;
    let (year, month, day) = civil_from_days(days);
    (year, month, day, hour as u32)
}

// Howard Hinnant's civil-from-days algorithm. It keeps Herdr dependency-light.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "herdr-mission-record-{name}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn migration_creates_recorder_tables() {
        let root = temp_dir("migration");
        let store = MissionStore::open(root.join("mission.sqlite3")).expect("store");

        let tables = store.table_names().expect("tables");

        assert!(tables.contains(&"missions".to_string()));
        assert!(tables.contains(&"mission_children".to_string()));
        assert!(tables.contains(&"mission_events".to_string()));
        assert!(tables.contains(&"mission_packets".to_string()));
    }

    #[test]
    fn init_mission_creates_db_row_and_project_markdown() {
        let root = temp_dir("init");
        let project = root.join("project");
        fs::create_dir_all(&project).expect("project");
        let store = MissionStore::open(root.join("mission.sqlite3")).expect("store");

        let mission = store
            .init_mission(&project, "Resolve Migration Failure")
            .expect("mission");

        assert_eq!(mission.project_root, project.display().to_string());
        assert!(mission.markdown_path.contains("/.sessions/"));
        assert!(mission
            .markdown_path
            .ends_with("resolve-migration-failure.md"));

        let markdown = fs::read_to_string(&mission.markdown_path).expect("markdown");
        assert!(markdown.contains("# Resolve Migration Failure"));
        assert!(markdown.contains("## Timeline"));
    }

    #[test]
    fn appending_event_updates_sqlite_and_markdown_timeline() {
        let root = temp_dir("event");
        let project = root.join("project");
        fs::create_dir_all(&project).expect("project");
        let store = MissionStore::open(root.join("mission.sqlite3")).expect("store");
        let mission = store
            .init_mission(&project, "Event Mission")
            .expect("mission");

        let event = store
            .record_event(MissionEventInput {
                mission_id: Some(mission.id),
                pane_id: Some("pane-1".into()),
                provider: "generic".into(),
                kind: "note".into(),
                text: Some("found stale binary clue".into()),
                payload: json!({"source": "test"}),
            })
            .expect("event");

        assert_eq!(event.kind, "note");
        let events = store
            .list_events(MissionEventQuery {
                mission_id: Some(mission.id),
                pane_id: None,
                limit: Some(10),
            })
            .expect("events");
        assert_eq!(events.len(), 1);

        let markdown = fs::read_to_string(&mission.markdown_path).expect("markdown");
        assert!(markdown.contains("found stale binary clue"));
        assert!(markdown.contains("generic/note"));
    }

    #[test]
    fn packet_field_updates_merge_without_losing_prior_fields() {
        let root = temp_dir("packet");
        let project = root.join("project");
        fs::create_dir_all(&project).expect("project");
        let store = MissionStore::open(root.join("mission.sqlite3")).expect("store");
        let mission = store
            .init_mission(&project, "Packet Mission")
            .expect("mission");

        store
            .update_packet_field(mission.id, "pane-1", "What I did", "inspected logs")
            .expect("field one");
        let packet = store
            .update_packet_field(
                mission.id,
                "pane-1",
                "Evidence / receipts",
                "railway log receipt",
            )
            .expect("field two");

        assert_eq!(packet.completed_fields, 2);
        assert_eq!(packet.fields["What I did"], "inspected logs");
        assert_eq!(packet.fields["Evidence / receipts"], "railway log receipt");

        let ready = store
            .mark_packet_ready(mission.id, "pane-1")
            .expect("ready");
        assert!(ready.ready);

        let markdown = fs::read_to_string(&mission.markdown_path).expect("markdown");
        assert!(markdown.contains("inspected logs"));
        assert!(markdown.contains("railway log receipt"));
    }

    #[test]
    fn packet_lookup_finds_cli_only_packet_without_child_row() {
        let root = temp_dir("packet-cli-only");
        let project = root.join("project");
        fs::create_dir_all(&project).expect("project");
        let store = MissionStore::open(root.join("mission.sqlite3")).expect("store");
        let mission = store.init_mission(&project, "CLI Packet").expect("mission");

        store
            .update_packet_field(
                mission.id,
                "pane-cli-only",
                "What I did",
                "recorded without mission_children",
            )
            .expect("field");

        let packet = store
            .packet_for_pane("pane-cli-only")
            .expect("packet lookup")
            .expect("cli packet should be discoverable by pane id");
        assert_eq!(
            packet
                .fields
                .get("What I did")
                .and_then(serde_json::Value::as_str),
            Some("recorded without mission_children")
        );
    }

    #[test]
    fn adapter_event_from_input_normalizes_known_providers() {
        let event = adapter_event_from_input(
            "claude",
            "subagent_started",
            json!({"agent_id": "a1", "role": "researcher"}),
        )
        .expect("adapter event");

        assert_eq!(event.provider, "claude");
        assert_eq!(event.kind, "subagent_started");
        assert_eq!(event.text.as_deref(), Some("researcher a1"));
    }
}
