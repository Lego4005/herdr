//! Experimental semantic-frame desktop workroom bridge.
//!
//! This is intentionally a small proof client: it exposes Herdr's existing
//! `SemanticFrame` stream as browser-friendly Server-Sent Events and can host
//! that page either in a browser or a lightweight native WebView window.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::api::schema::{
    AgentStartParams, EmptyParams, Method, PaneListParams, PaneReadParams, PaneRenameParams,
    PaneSendTextParams, PaneSetWaveContractParams, PaneSplitParams, PaneTarget, ReadFormat,
    ReadSource, Request, SplitDirection,
};
use crate::server::headless::client_socket_path;
use crate::server::protocol::{
    self, CellData, ClientMessage, CursorState, RenderEncoding, ServerMessage, MAX_FRAME_SIZE,
    MAX_GRAPHICS_FRAME_SIZE, PROTOCOL_VERSION,
};
use crate::wave::{
    default_report_packet_items, derive_default_report_gate, parse_session_wave_contracts,
    BlastRadius, WaveArc, WaveContract, WaveLifecycleLane, WaveMode, WavePromptDelivery,
    WaveReportGate, WaveStatus,
};

mod workroom_model;

const DEFAULT_BIND_ADDR: &str = "127.0.0.1:0";
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;
const MAX_PREVIEW_COLS: u16 = 300;
const MAX_PREVIEW_ROWS: u16 = 120;
const PREFERRED_CHILD_AGENT_COMMANDS: [&str; 5] = ["claude", "codex", "pi", "opencode", "hermes"];
const DESKTOP_SERVER_READY_TIMEOUT: Duration = Duration::from_secs(5);
const DESKTOP_STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MISSION_RADAR_SCAN_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_DISPATCH_EVENTS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopLaunchMode {
    NativeApp,
    WebPreview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopCommandConfig {
    bind_addr: String,
    launch_mode: DesktopLaunchMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DesktopCommandPlan {
    Run(DesktopCommandConfig),
    Help,
    UsageError(String),
}

pub(crate) fn run_desktop_command(args: &[String]) -> io::Result<i32> {
    let config = match parse_desktop_command(args) {
        DesktopCommandPlan::Run(config) => config,
        DesktopCommandPlan::Help => {
            print_desktop_help();
            return Ok(0);
        }
        DesktopCommandPlan::UsageError(message) => {
            eprintln!("{message}");
            print_desktop_help();
            return Ok(2);
        }
    };

    let addr = config
        .bind_addr
        .parse::<SocketAddr>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    match config.launch_mode {
        DesktopLaunchMode::NativeApp => run_native_app(addr),
        DesktopLaunchMode::WebPreview => run_preview_server(addr),
    }
}

fn default_desktop_launch_mode() -> DesktopLaunchMode {
    #[cfg(target_os = "macos")]
    {
        DesktopLaunchMode::NativeApp
    }

    #[cfg(not(target_os = "macos"))]
    {
        DesktopLaunchMode::WebPreview
    }
}

fn parse_desktop_command(args: &[String]) -> DesktopCommandPlan {
    let mut config = DesktopCommandConfig {
        bind_addr: DEFAULT_BIND_ADDR.to_string(),
        launch_mode: default_desktop_launch_mode(),
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--app" => {
                config.launch_mode = DesktopLaunchMode::NativeApp;
                index += 1;
            }
            "--web" | "--preview" => {
                config.launch_mode = DesktopLaunchMode::WebPreview;
                index += 1;
            }
            "--bind" => {
                let Some(value) = args.get(index + 1) else {
                    return DesktopCommandPlan::UsageError("missing value for --bind".into());
                };
                config.bind_addr = value.clone();
                index += 2;
            }
            "--port" => {
                let Some(value) = args.get(index + 1) else {
                    return DesktopCommandPlan::UsageError("missing value for --port".into());
                };
                let Ok(port) = value.parse::<u16>() else {
                    return DesktopCommandPlan::UsageError(format!(
                        "invalid --port value: {value}"
                    ));
                };
                config.bind_addr = format!("127.0.0.1:{port}");
                index += 2;
            }
            "help" | "--help" | "-h" => {
                return DesktopCommandPlan::Help;
            }
            other => {
                return DesktopCommandPlan::UsageError(format!("unknown option: {other}"));
            }
        }
    }

    DesktopCommandPlan::Run(config)
}

fn run_preview_server(addr: SocketAddr) -> io::Result<i32> {
    ensure_desktop_server_ready()?;
    let listener = TcpListener::bind(addr)?;
    let local_addr = listener.local_addr()?;

    println!("Herdr Workroom web preview listening on http://{local_addr}/");
    println!("Open that URL; Herdr server startup is handled by the workroom shell.");

    serve_preview_listener(listener)
}

fn serve_preview_listener(listener: TcpListener) -> io::Result<i32> {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(err) = handle_http_connection(stream) {
                        eprintln!("desktop preview connection failed: {err}");
                    }
                });
            }
            Err(err) => eprintln!("desktop preview accept failed: {err}"),
        }
    }

    Ok(0)
}

#[cfg(target_os = "macos")]
fn run_native_app(addr: SocketAddr) -> io::Result<i32> {
    ensure_desktop_server_ready()?;
    let listener = TcpListener::bind(addr)?;
    let local_addr = listener.local_addr()?;
    let url = format!("http://{local_addr}/");

    println!("Herdr Workroom app listening on {url}");
    println!("Close the app window to stop the desktop shell.");

    thread::spawn(move || {
        if let Err(err) = serve_preview_listener(listener) {
            eprintln!("desktop app preview server failed: {err}");
        }
    });

    launch_native_webview(&url)
}

#[cfg(not(target_os = "macos"))]
fn run_native_app(_addr: SocketAddr) -> io::Result<i32> {
    eprintln!("herdr desktop --app is currently implemented for macOS only. Use `herdr desktop --web` on this platform.");
    Ok(2)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopServerReadiness {
    AlreadyRunning,
    Spawned,
}

fn ensure_desktop_server_ready() -> io::Result<DesktopServerReadiness> {
    if crate::server::autodetect::is_server_listening() {
        validate_desktop_server_compatibility()?;
        return Ok(DesktopServerReadiness::AlreadyRunning);
    }

    crate::server::autodetect::spawn_server_daemon()?;
    crate::server::autodetect::wait_for_server_socket(
        &client_socket_path(),
        DESKTOP_SERVER_READY_TIMEOUT,
    )?;
    validate_desktop_server_compatibility()?;
    Ok(DesktopServerReadiness::Spawned)
}

fn validate_desktop_server_compatibility() -> io::Result<()> {
    validate_desktop_runtime_status(crate::api::read_runtime_status_at(
        &crate::api::socket_path(),
        DESKTOP_STATUS_REQUEST_TIMEOUT,
    )?)
}

fn validate_desktop_runtime_status(status: Option<crate::api::RuntimeStatus>) -> io::Result<()> {
    let Some(status) = status else {
        return Err(io::Error::other(
            "Herdr server is listening, but its status API is unavailable. Try `herdr server stop`, then run `herdr desktop` again.",
        ));
    };

    if status.protocol == Some(PROTOCOL_VERSION) {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "Herdr server is running from v{} / protocol {}, but this desktop shell needs v{} / protocol {}. Stop the old server with `herdr server stop`, then run `herdr desktop` again.",
        status.version.as_deref().unwrap_or("unknown"),
        status
            .protocol
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        env!("CARGO_PKG_VERSION"),
        PROTOCOL_VERSION
    )))
}

#[cfg(target_os = "macos")]
fn launch_native_webview(url: &str) -> io::Result<i32> {
    use tao::dpi::LogicalSize;
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoop};
    use tao::window::WindowBuilder;
    use wry::WebViewBuilder;

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("Herdr Workroom")
        .with_inner_size(LogicalSize::new(1440.0, 920.0))
        .with_min_inner_size(LogicalSize::new(1120.0, 720.0))
        .build(&event_loop)
        .map_err(|err| io::Error::other(err.to_string()))?;

    let _webview = WebViewBuilder::new()
        .with_url(url)
        .build(&window)
        .map_err(|err| io::Error::other(err.to_string()))?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    });
}

fn handle_http_connection(mut stream: TcpStream) -> io::Result<()> {
    let request_line = read_request_line(&mut stream)?;
    let Some(target) = request_line.split_whitespace().nth(1) else {
        return write_text_response(&mut stream, 400, "Bad Request", "bad request");
    };
    let (path, query) = split_path_query(target);

    match path {
        "/" | "/index.html" => write_html_response(&mut stream, INDEX_HTML),
        "/favicon.ico" => write_text_response(&mut stream, 204, "No Content", ""),
        "/health" => write_text_response(&mut stream, 200, "OK", "ok\n"),
        "/workspaces" => handle_workspaces(stream),
        "/panes" => handle_panes(stream),
        "/integrations" => handle_integrations(stream),
        "/mission/import" => handle_mission_import(stream, query),
        "/mission/radar" => handle_mission_radar(stream, query),
        "/mission/workroom" => handle_mission_workroom(stream, query),
        "/events" => handle_events(stream, query),
        "/input" => handle_input(stream, query),
        "/pane/input" => handle_pane_input(stream, query),
        "/pane/dispatch" => handle_pane_dispatch(stream, query),
        "/dispatches" => handle_dispatches(stream),
        "/pane/output" => handle_pane_output(stream, query),
        "/mission/sweep" => handle_mission_sweep(stream, query),
        "/pane/split" => handle_pane_split(stream, query),
        "/agent/start" => handle_agent_start(stream, query),
        "/pane/rename" => handle_pane_rename(stream, query),
        "/pane/contract" => handle_pane_contract(stream, query),
        "/pane/report" => handle_pane_report(stream, query),
        "/pane/ingest-report" => handle_pane_ingest_report(stream, query),
        "/pane/status" => handle_pane_status(stream, query),
        "/mission/unlock" => handle_mission_unlock(stream, query),
        "/pane/close" => handle_pane_close(stream, query),
        "/git/status" => handle_git_status(stream, query),
        "/terminal/events" => handle_terminal_events(stream, query),
        _ => write_text_response(&mut stream, 404, "Not Found", "not found\n"),
    }
}

fn read_request_line(stream: &mut TcpStream) -> io::Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut bytes = Vec::new();
    let mut buf = [0u8; 512];
    while bytes.len() < 8192 {
        let read = stream.read(&mut buf)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    stream.set_read_timeout(None)?;

    let request = String::from_utf8_lossy(&bytes);
    Ok(request.lines().next().unwrap_or("").to_string())
}

fn split_path_query(target: &str) -> (&str, Option<&str>) {
    target
        .split_once('?')
        .map_or((target, None), |(path, query)| (path, Some(query)))
}

fn handle_events(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let cols = query_u16(query, "cols")
        .unwrap_or(DEFAULT_COLS)
        .clamp(1, MAX_PREVIEW_COLS);
    let rows = query_u16(query, "rows")
        .unwrap_or(DEFAULT_ROWS)
        .clamp(1, MAX_PREVIEW_ROWS);

    write!(
        stream,
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: keep-alive\r\n\
         Access-Control-Allow-Origin: *\r\n\
         \r\n"
    )?;
    stream.flush()?;

    match stream_semantic_frames(&mut stream, cols, rows) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = write_sse_event(
                &mut stream,
                "error",
                &serde_json::json!({"message": err.to_string()}),
            );
            Err(err)
        }
    }
}

fn handle_input(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_data) = query_value(query, "data") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing input data\n");
    };
    let Some(data) = percent_decode(raw_data) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid input data\n");
    };
    if data.is_empty() {
        return write_text_response(&mut stream, 204, "No Content", "");
    }

    let cols = query_u16(query, "cols")
        .unwrap_or(DEFAULT_COLS)
        .clamp(1, MAX_PREVIEW_COLS);
    let rows = query_u16(query, "rows")
        .unwrap_or(DEFAULT_ROWS)
        .clamp(1, MAX_PREVIEW_ROWS);

    send_client_input(data, cols, rows)?;
    write_text_response(&mut stream, 200, "OK", "ok\n")
}

fn handle_workspaces(mut stream: TcpStream) -> io::Result<()> {
    let response = send_api_request(&Request {
        id: "desktop:workspace:list".into(),
        method: Method::WorkspaceList(EmptyParams {}),
    })?;
    write_json_response(&mut stream, &response)
}

fn handle_panes(mut stream: TcpStream) -> io::Result<()> {
    let response = send_api_request(&Request {
        id: "desktop:pane:list".into(),
        method: Method::PaneList(PaneListParams { workspace_id: None }),
    })?;
    write_json_response(&mut stream, &response)
}

#[derive(Debug, Clone, Copy)]
struct DesktopAgentCandidate<'a> {
    command: &'a str,
    available: bool,
}

fn fallback_child_agent_argv() -> Vec<String> {
    vec!["/bin/zsh".into(), "-l".into()]
}

fn preferred_child_agent_argv(candidates: &[DesktopAgentCandidate<'_>]) -> Vec<String> {
    for preferred in PREFERRED_CHILD_AGENT_COMMANDS {
        if candidates
            .iter()
            .any(|candidate| candidate.command == preferred && candidate.available)
        {
            return vec![preferred.into()];
        }
    }

    fallback_child_agent_argv()
}

fn resolve_desktop_agent_argv(argv: &[String]) -> Vec<String> {
    let paths = std::env::var_os("PATH");
    resolve_desktop_agent_argv_from_paths(argv, paths.as_deref(), &desktop_user_agent_search_dirs())
}

fn resolve_desktop_agent_argv_from_paths(
    argv: &[String],
    path_env: Option<&OsStr>,
    extra_dirs: &[PathBuf],
) -> Vec<String> {
    let Some((program, args)) = argv.split_first() else {
        return Vec::new();
    };
    let mut resolved = Vec::with_capacity(argv.len());
    let command_path = Path::new(program);
    if command_path.components().count() > 1 {
        resolved.push(program.clone());
    } else if let Some(path) = desktop_resolve_command_from_paths(program, path_env, extra_dirs) {
        resolved.push(path.to_string_lossy().into_owned());
    } else {
        resolved.push(program.clone());
    }
    resolved.extend(args.iter().cloned());
    resolved
}

fn desktop_integration_state_label(
    state: crate::integration::IntegrationStatusKind,
) -> &'static str {
    match state {
        crate::integration::IntegrationStatusKind::NotInstalled => "not_installed",
        crate::integration::IntegrationStatusKind::Current => "current",
        crate::integration::IntegrationStatusKind::Outdated => "outdated",
    }
}

fn desktop_integration_status_label(
    state: crate::integration::IntegrationStatusKind,
    command_available: bool,
) -> &'static str {
    match (command_available, state) {
        (_, crate::integration::IntegrationStatusKind::Current) => "installed",
        (_, crate::integration::IntegrationStatusKind::Outdated) => "update available",
        (true, crate::integration::IntegrationStatusKind::NotInstalled) => "available",
        (false, crate::integration::IntegrationStatusKind::NotInstalled) => "not found",
    }
}

fn desktop_integration_needs_install(
    state: crate::integration::IntegrationStatusKind,
    command_available: bool,
) -> bool {
    state == crate::integration::IntegrationStatusKind::Outdated
        || (command_available && state == crate::integration::IntegrationStatusKind::NotInstalled)
}

fn desktop_command_available(command: &str) -> bool {
    desktop_resolve_command(command).is_some()
}

fn desktop_resolve_command(command: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH");
    desktop_resolve_command_from_paths(command, paths.as_deref(), &desktop_user_agent_search_dirs())
}

fn desktop_resolve_command_from_paths(
    command: &str,
    path_env: Option<&OsStr>,
    extra_dirs: &[PathBuf],
) -> Option<PathBuf> {
    let command_path = Path::new(command);
    if command_path.components().count() > 1 {
        return desktop_executable_file_exists(command_path).then(|| command_path.to_path_buf());
    }

    path_env
        .into_iter()
        .flat_map(std::env::split_paths)
        .chain(extra_dirs.iter().cloned())
        .map(|dir| dir.join(command))
        .find(|path| desktop_executable_file_exists(path))
}

#[cfg(test)]
fn desktop_command_available_from_paths(
    command: &str,
    path_env: Option<&OsStr>,
    extra_dirs: &[PathBuf],
) -> bool {
    desktop_resolve_command_from_paths(command, path_env, extra_dirs).is_some()
}

fn desktop_user_agent_search_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    vec![home.join(".local/bin"), home.join("bin")]
}

fn desktop_executable_file_exists(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn handle_integrations(mut stream: TcpStream) -> io::Result<()> {
    let recommendations = crate::integration::integration_recommendations();
    let command_availability = recommendations
        .iter()
        .map(|recommendation| DesktopAgentCandidate {
            command: recommendation.command,
            available: desktop_command_available(recommendation.command),
        })
        .collect::<Vec<_>>();
    let preferred_argv = preferred_child_agent_argv(&command_availability);
    let preferred_label = preferred_argv
        .first()
        .map(|command| command.as_str())
        .unwrap_or("/bin/zsh");
    let recommendations_json = recommendations
        .into_iter()
        .map(|recommendation| {
            let command_available = command_availability
                .iter()
                .find(|candidate| candidate.command == recommendation.command)
                .is_some_and(|candidate| candidate.available);
            serde_json::json!({
                "target": crate::integration::integration_target_label(recommendation.target),
                "label": recommendation.label,
                "command": recommendation.command,
                "command_available": command_available,
                "status": desktop_integration_state_label(recommendation.state),
                "status_label": desktop_integration_status_label(recommendation.state, command_available),
                "needs_install": desktop_integration_needs_install(recommendation.state, command_available),
                "path": recommendation.path.display().to_string(),
            })
        })
        .collect::<Vec<_>>();

    write_json_response(
        &mut stream,
        &serde_json::json!({
            "id": "desktop:integrations",
            "result": {
                "preferred_argv": preferred_argv,
                "preferred_label": preferred_label,
                "fallback_argv": fallback_child_agent_argv(),
                "recommendations": recommendations_json,
            }
        }),
    )
}

fn handle_mission_import(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_path) = query_value(query, "path") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing path\n");
    };
    let Some(path) = percent_decode_string(raw_path) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid path\n");
    };
    let path = path.trim();
    if path.is_empty() {
        return write_text_response(&mut stream, 400, "Bad Request", "empty path\n");
    }
    let text = fs::read_to_string(path)?;
    let contracts = match parse_session_wave_contracts(&text) {
        Ok(contracts) => contracts,
        Err(err) => {
            return write_text_response(&mut stream, 400, "Bad Request", &format!("{err}\n"))
        }
    };
    let apply =
        !query_value(query, "apply").is_some_and(|value| matches!(value, "0" | "false" | "no"));

    let panes_response = send_api_request(&Request {
        id: "desktop:mission-import:panes".into(),
        method: Method::PaneList(PaneListParams { workspace_id: None }),
    })?;
    if panes_response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &panes_response);
    }
    let create_missing = query_bool(query, "create_missing");
    let child_targets = child_pane_targets_from_panes_response(&panes_response);
    let reuse_mode = if create_missing {
        MissionImportReuseMode::TitleOnly
    } else {
        MissionImportReuseMode::Positional
    };
    let mut assignments =
        mission_import_plan_for_targets_with_reuse(&contracts, &child_targets, reuse_mode);
    let launch_target_pane_id = query_value(query, "target_pane_id")
        .and_then(percent_decode_string)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| root_pane_id_from_panes_response(&panes_response));
    let launch_argv = match mission_import_launch_argv(query) {
        Ok(argv) => argv,
        Err(message) => return write_text_response(&mut stream, 400, "Bad Request", message),
    };
    let prompt_scope = parse_mission_import_prompt_scope(
        query_value(query, "prompt_scope").or_else(|| query_value(query, "send_prompt")),
    );
    let gate_dependencies = !query_value(query, "gate_dependencies")
        .is_some_and(|value| matches!(value, "0" | "false" | "no"));
    mission_import_mark_planned_actions(&mut assignments, create_missing);
    mission_import_apply_dependency_gates(&mut assignments, &contracts, gate_dependencies);
    let planned_reused = mission_import_planned_reuse_count(&assignments);
    let planned_created = mission_import_planned_create_count(&assignments, create_missing);
    let planned_prompted =
        mission_import_planned_prompt_count(&assignments, create_missing, prompt_scope);

    if apply {
        for (index, assignment) in assignments.iter_mut().enumerate() {
            let mut created_pane = false;
            if assignment.pane_id.is_none() && create_missing {
                let Some(target_pane_id) = launch_target_pane_id.clone() else {
                    assignment.status = MissionImportAssignmentStatus::Failed;
                    assignment.error = Some("no parent pane available for child creation".into());
                    continue;
                };
                match create_mission_import_child_pane(
                    &target_pane_id,
                    &contracts[index],
                    &launch_argv,
                )? {
                    Ok(identity) => {
                        assignment.pane_id = Some(identity.pane_id);
                        assignment.terminal_id = identity.terminal_id;
                        created_pane = true;
                    }
                    Err(error) => {
                        assignment.status = MissionImportAssignmentStatus::Failed;
                        assignment.error = Some(error);
                        continue;
                    }
                }
            }

            let Some(pane_id) = assignment.pane_id.clone() else {
                continue;
            };
            let mut contract = contracts[index].clone();
            contract.pane_id = Some(pane_id.clone());
            contract
                .prompt_delivery
                .get_or_insert_with(|| mission_import_prompt_delivery(&launch_argv));
            let prompt_this_pane = prompt_scope.should_prompt(created_pane)
                && assignment.gate_status == MissionDependencyGateStatus::Ready;
            contract.status = Some(mission_import_contract_status(
                contract.status,
                prompt_this_pane,
                assignment.gate_status,
            ));
            let response = send_api_request(&Request {
                id: format!("desktop:mission-import:{pane_id}"),
                method: Method::PaneSetWaveContract(PaneSetWaveContractParams {
                    pane_id: pane_id.clone(),
                    contract: Some(contract.clone()),
                }),
            })?;
            if response.get("error").is_some() {
                assignment.status = MissionImportAssignmentStatus::Failed;
                assignment.error = response
                    .pointer("/error/message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some("contract apply failed".into()));
            } else {
                assignment.status = if created_pane {
                    MissionImportAssignmentStatus::Created
                } else {
                    MissionImportAssignmentStatus::Applied
                };
                if prompt_this_pane {
                    match send_mission_import_contract_prompt(path, &pane_id, &contract)? {
                        Ok(()) => assignment.prompt_sent = true,
                        Err(error) => {
                            assignment.status = MissionImportAssignmentStatus::Failed;
                            assignment.error = Some(error);
                        }
                    }
                }
            }
        }
    }

    let applied = mission_import_applied_count(&assignments);
    let created = mission_import_created_count(&assignments);
    let missing_panes = mission_import_missing_panes_count(&assignments);
    write_json_response(
        &mut stream,
        &serde_json::json!({
            "id": "desktop:mission-import",
            "result": {
                "type": "mission_import",
                "path": path,
                "contracts": contracts,
                "assignments": assignments,
                "applied": applied,
                "created": created,
                "missing_panes": missing_panes,
                "planned_reused": planned_reused,
                "planned_created": planned_created,
                "planned_prompted": planned_prompted,
                "apply": apply,
                "create_missing": create_missing,
                "gate_dependencies": gate_dependencies,
                "prompt_scope": prompt_scope
            }
        }),
    )
}

#[derive(Debug, Clone, Serialize)]
struct MissionRadarCandidate {
    id: String,
    title: String,
    family: String,
    #[serde(rename = "type")]
    kind: String,
    mode: String,
    tone: String,
    signal: String,
    dependency: String,
    source: String,
    evidence: Vec<String>,
    allowed_paths: Vec<String>,
    required_report: Vec<String>,
    suggested_commands: Vec<String>,
    brief: String,
}

#[derive(Debug, Clone)]
struct DesktopCommandCapture {
    success: bool,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

fn handle_mission_radar(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let panes_response = send_api_request(&Request {
        id: "desktop:mission-radar:panes".into(),
        method: Method::PaneList(PaneListParams { workspace_id: None }),
    })?;
    if panes_response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &panes_response);
    }

    let cwd = mission_radar_cwd(query, &panes_response)?;
    let px_path = desktop_resolve_command("px");
    let mut px_state = serde_json::json!({
        "available": px_path.is_some(),
        "ran": false,
        "ok": false,
    });
    let mut candidates = Vec::new();

    if let Some(px_path) = px_path {
        match run_command_capture(
            &px_path,
            &["audit", "--repo", &cwd, "--json"],
            Path::new(&cwd),
            MISSION_RADAR_SCAN_TIMEOUT,
        ) {
            Ok(output) => {
                let parsed_audit = parse_px_audit_stdout(&output.stdout);
                match parsed_audit {
                    Ok(audit) if output.success && !output.timed_out => {
                        candidates = mission_radar_candidates_from_px_audit(&audit);
                        px_state = serde_json::json!({
                            "available": true,
                            "ran": true,
                            "ok": true,
                            "timed_out": false,
                            "space": audit.get("space").and_then(serde_json::Value::as_str),
                            "built_at": audit.get("built_at").and_then(serde_json::Value::as_str),
                            "health_score": audit.get("health_score").and_then(serde_json::Value::as_i64),
                            "status": audit.get("status").and_then(serde_json::Value::as_str),
                            "stderr": truncate_for_json(&output.stderr, 600),
                        });
                    }
                    Ok(audit) => {
                        candidates = mission_radar_candidates_from_px_audit(&audit);
                        px_state = serde_json::json!({
                            "available": true,
                            "ran": true,
                            "ok": false,
                            "timed_out": output.timed_out,
                            "error": "px audit exited non-zero",
                            "stderr": truncate_for_json(&output.stderr, 600),
                        });
                    }
                    Err(error) => {
                        px_state = serde_json::json!({
                            "available": true,
                            "ran": true,
                            "ok": false,
                            "timed_out": output.timed_out,
                            "error": error,
                            "stderr": truncate_for_json(&output.stderr, 600),
                        });
                    }
                }
            }
            Err(error) => {
                px_state = serde_json::json!({
                    "available": true,
                    "ran": true,
                    "ok": false,
                    "error": error.to_string(),
                });
            }
        }
    }

    write_json_response(
        &mut stream,
        &serde_json::json!({
            "id": "desktop:mission-radar",
            "result": {
                "type": "mission_radar",
                "cwd": cwd,
                "px": px_state,
                "candidates": candidates,
            }
        }),
    )
}

fn mission_radar_cwd(
    query: Option<&str>,
    panes_response: &serde_json::Value,
) -> io::Result<String> {
    if let Some(cwd) = query_value(query, "cwd")
        .and_then(percent_decode_string)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Ok(cwd);
    }

    if let Some(cwd) = root_pane_cwd_from_panes_response(panes_response) {
        return Ok(cwd);
    }

    std::env::current_dir().map(|path| path.display().to_string())
}

fn root_pane_cwd_from_panes_response(response: &serde_json::Value) -> Option<String> {
    let panes = response
        .pointer("/result/panes")
        .and_then(serde_json::Value::as_array)?;
    panes
        .iter()
        .find(|pane| {
            pane.get("is_root_pane")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .or_else(|| panes.first())?
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_string)
}

fn run_command_capture(
    command: &Path,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> io::Result<DesktopCommandCapture> {
    let capture_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let stdout_path = std::env::temp_dir().join(format!(
        "herdr-desktop-command-{}-{capture_id}.out",
        std::process::id()
    ));
    let stderr_path = std::env::temp_dir().join(format!(
        "herdr-desktop-command-{}-{capture_id}.err",
        std::process::id()
    ));
    let stdout_file = fs::File::create(&stdout_path)?;
    let stderr_file = fs::File::create(&stderr_path)?;
    let mut child = Command::new(command)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()?;
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(status) = child.try_wait()? {
            let capture = DesktopCommandCapture {
                success: status.success(),
                timed_out: false,
                stdout: read_capture_path(&stdout_path),
                stderr: read_capture_path(&stderr_path),
            };
            let _ = fs::remove_file(&stdout_path);
            let _ = fs::remove_file(&stderr_path);
            return Ok(capture);
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let capture = DesktopCommandCapture {
                success: false,
                timed_out: true,
                stdout: read_capture_path(&stdout_path),
                stderr: read_capture_path(&stderr_path),
            };
            let _ = fs::remove_file(&stdout_path);
            let _ = fs::remove_file(&stderr_path);
            return Ok(capture);
        }

        thread::sleep(Duration::from_millis(25));
    }
}

fn read_capture_path(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .unwrap_or_else(|error| format!("failed to read {}: {error}", path.display()))
}

fn parse_px_audit_stdout(stdout: &str) -> Result<serde_json::Value, String> {
    let Some(start) = stdout.find('{') else {
        return Err("px audit did not return JSON".into());
    };
    serde_json::from_str(&stdout[start..]).map_err(|err| err.to_string())
}

fn mission_radar_candidates_from_px_audit(audit: &serde_json::Value) -> Vec<MissionRadarCandidate> {
    let mut candidates = Vec::new();
    let health_score = audit
        .get("health_score")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(100);
    let health_notes = audit
        .get("health_notes")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|note| note.get("message").and_then(serde_json::Value::as_str))
        .take(4)
        .map(str::to_string)
        .collect::<Vec<_>>();

    if health_score < 80 || !health_notes.is_empty() {
        candidates.push(MissionRadarCandidate {
            id: "px-code-health-scout".into(),
            title: "PX code-health scout".into(),
            family: "insight_task".into(),
            kind: "project research".into(),
            mode: "read_only".into(),
            tone: if health_score < 60 { "warn" } else { "" }.into(),
            signal: format!(
                "health score {health_score}; {}",
                health_notes
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "review PX health notes".into())
            ),
            dependency: String::new(),
            source: "px audit".into(),
            evidence: health_notes.clone(),
            allowed_paths: vec!["read-only repo inspection".into(), ".px/.fox health evidence".into()],
            required_report: vec![
                "what I found".into(),
                "evidence / receipts".into(),
                "recommended next wave".into(),
                "good / bad / ugly".into(),
            ],
            suggested_commands: vec![
                "px doctor --lite".into(),
                "px audit --repo <repo> --json".into(),
            ],
            brief: radar_brief(
                "PX code-health scout",
                "Use PX audit evidence to propose the next highest-leverage Herdr waves.",
                &[
                    format!("Current PX health score: {health_score}"),
                    format!(
                        "Health notes: {}",
                        health_notes.join("; ").trim_matches(';')
                    ),
                    "Return 3 candidate wave contracts with evidence, risk, mode, and expected report packet.".into(),
                ],
            ),
        });
    }

    let hotspots = audit_array_objects(audit, "hotspot_files")
        .into_iter()
        .filter_map(|item| {
            let path = item.get("path")?.as_str()?.trim();
            (!path.is_empty()).then(|| {
                format!(
                    "{} ({} refs)",
                    path,
                    item.get("total_incoming_refs")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or_default()
                )
            })
        })
        .take(5)
        .collect::<Vec<_>>();
    if !hotspots.is_empty() {
        candidates.push(MissionRadarCandidate {
            id: "px-hotspot-refactor-scout".into(),
            title: "PX hotspot refactor scout".into(),
            family: "ideation_wave".into(),
            kind: "code quality".into(),
            mode: "read_only".into(),
            tone: "warn".into(),
            signal: hotspots.first().cloned().unwrap_or_default(),
            dependency: String::new(),
            source: "px audit".into(),
            evidence: hotspots.clone(),
            allowed_paths: hotspots.iter().map(|item| item.split_whitespace().next().unwrap_or(item).to_string()).collect(),
            required_report: vec![
                "existing pattern".into(),
                "blast-radius risk".into(),
                "test guard recommendation".into(),
                "next wave contract".into(),
            ],
            suggested_commands: vec![
                "px context file <hotspot>".into(),
                "px impact <symbol> --depth 2".into(),
            ],
            brief: radar_brief(
                "PX hotspot refactor scout",
                "Inspect high-reference files and propose safe refactor or test-protection waves.",
                &[
                    format!("PX hotspot files: {}", hotspots.join("; ")),
                    "Do not edit files in this wave.".into(),
                    "Return one conservative wave, one test-coverage wave, and one do-not-touch warning if risk is too high.".into(),
                ],
            ),
        });
    }

    let dead_ratio = audit
        .get("dead_ratio")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    let dead_count = audit
        .get("dead_count")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default();
    if dead_ratio >= 0.15 && dead_count > 0 {
        let dead_examples = audit_array_objects(audit, "dead_symbols")
            .into_iter()
            .filter_map(|item| {
                Some(format!(
                    "{} in {}:{}",
                    item.get("name")?.as_str()?,
                    item.get("file")?.as_str()?,
                    item.get("line")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or_default()
                ))
            })
            .take(5)
            .collect::<Vec<_>>();
        candidates.push(MissionRadarCandidate {
            id: "px-dead-code-scout".into(),
            title: "PX dead-code cleanup scout".into(),
            family: "ideation_wave".into(),
            kind: "code quality".into(),
            mode: "read_only".into(),
            tone: "warn".into(),
            signal: format!("{dead_count} dead symbols ({:.0}%)", dead_ratio * 100.0),
            dependency: String::new(),
            source: "px audit".into(),
            evidence: dead_examples.clone(),
            allowed_paths: vec!["read-only symbol validation".into()],
            required_report: vec![
                "candidate cleanup".into(),
                "proof of non-use".into(),
                "false-positive risk".into(),
                "safe deletion order".into(),
            ],
            suggested_commands: vec![
                "px explain-miss <symbol> --space herdr --json".into(),
                "px query literal <symbol> --space herdr --json".into(),
            ],
            brief: radar_brief(
                "PX dead-code cleanup scout",
                "Validate whether PX dead-code findings are real cleanup opportunities or index false positives.",
                &[
                    format!("{dead_count} dead symbols reported; ratio {:.1}%.", dead_ratio * 100.0),
                    format!("Examples: {}", dead_examples.join("; ")),
                    "Return only safe, low-blast-radius cleanup candidates with proof of non-use.".into(),
                ],
            ),
        });
    }

    let coupled = audit_array_objects(audit, "high_coupling_symbols")
        .into_iter()
        .filter_map(|item| {
            Some(format!(
                "{} in {} ({} refs)",
                item.get("name")?.as_str()?,
                item.get("file")?.as_str()?,
                item.get("use_count")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or_default()
            ))
        })
        .take(5)
        .collect::<Vec<_>>();
    if !coupled.is_empty() {
        candidates.push(MissionRadarCandidate {
            id: "px-coupling-reviewer".into(),
            title: "PX coupling risk reviewer".into(),
            family: "ideation_wave".into(),
            kind: "architecture risk".into(),
            mode: "reviewer".into(),
            tone: "warn".into(),
            signal: coupled.first().cloned().unwrap_or_default(),
            dependency: String::new(),
            source: "px audit".into(),
            evidence: coupled.clone(),
            allowed_paths: vec!["read-only high-coupling symbols".into()],
            required_report: vec![
                "risk map".into(),
                "protected files".into(),
                "test guard recommendation".into(),
                "reviewer verdict".into(),
            ],
            suggested_commands: vec![
                "px callers <symbol> --space herdr".into(),
                "px callees <symbol> --space herdr".into(),
            ],
            brief: radar_brief(
                "PX coupling risk reviewer",
                "Inspect high-coupling symbols and recommend guardrails before any write wave touches them.",
                &[
                    format!("High-coupling symbols: {}", coupled.join("; ")),
                    "Map what should be protected by tests before edits.".into(),
                    "Return reviewer/verifier wave contracts, not implementation changes.".into(),
                ],
            ),
        });
    }

    let diagnostics_missing = audit
        .pointer("/phase_coverage/p5_diagnostics")
        .and_then(serde_json::Value::as_bool)
        == Some(false);
    if diagnostics_missing {
        candidates.push(MissionRadarCandidate {
            id: "px-diagnostics-refresh-scout".into(),
            title: "PX diagnostics refresh scout".into(),
            family: "insight_task".into(),
            kind: "index health".into(),
            mode: "read_only".into(),
            tone: String::new(),
            signal: "PX diagnostics phase is not populated".into(),
            dependency: String::new(),
            source: "px audit".into(),
            evidence: vec!["phase_coverage.p5_diagnostics = false".into()],
            allowed_paths: vec!["PX index metadata".into()],
            required_report: vec![
                "diagnostics need".into(),
                "cost / benefit".into(),
                "recommended command".into(),
                "defer reason if not now".into(),
            ],
            suggested_commands: vec![
                "px doctor --lite".into(),
                "px rebuild --space herdr --repo <repo> --with-diagnostics".into(),
            ],
            brief: radar_brief(
                "PX diagnostics refresh scout",
                "Decide whether a diagnostics rebuild is worth the time before planning risky waves.",
                &[
                    "PX audit reports p5 diagnostics missing.".into(),
                    "Inspect current need for diagnostics versus cheap targeted tests.".into(),
                    "Recommend whether to run px rebuild --with-diagnostics now or defer.".into(),
                ],
            ),
        });
    }

    candidates.truncate(5);
    candidates
}

fn audit_array_objects<'a>(
    audit: &'a serde_json::Value,
    key: &str,
) -> Vec<&'a serde_json::Map<String, serde_json::Value>> {
    audit
        .get(key)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_object)
        .collect()
}

fn radar_brief(title: &str, goal: &str, focus: &[String]) -> String {
    let mut lines = vec![
        format!("Goal: {goal}"),
        String::new(),
        "Research policy:".into(),
        "- Use px first if it is available; otherwise inspect with normal shell tools and say px was unavailable.".into(),
        "- Do not edit files unless the parent explicitly changes this wave mode.".into(),
        "- Return a report packet with evidence, files read, risks, good/bad/ugly, and next-wave recommendation.".into(),
        String::new(),
        format!("Focus for {title}:"),
    ];
    lines.extend(focus.iter().map(|item| format!("- {item}")));
    lines.join("\n")
}

fn truncate_for_json(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for ch in value.chars().take(max_chars) {
        output.push(ch);
    }
    output
}

fn handle_git_status(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_cwd) = query_value(query, "cwd") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing cwd\n");
    };
    let Some(cwd) = percent_decode_string(raw_cwd) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid cwd\n");
    };
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return write_text_response(&mut stream, 400, "Bad Request", "empty cwd\n");
    }

    let response = git_status_for_cwd(cwd);
    write_json_response(
        &mut stream,
        &serde_json::json!({
            "id": "desktop:git:status",
            "result": response,
        }),
    )
}

fn handle_pane_input(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_pane_id) = query_value(query, "pane_id") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing pane_id\n");
    };
    let Some(raw_data) = query_value(query, "data") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing input data\n");
    };
    let Some(pane_id) = percent_decode_string(raw_pane_id) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid pane_id\n");
    };
    let Some(data) = percent_decode_string(raw_data) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid input data\n");
    };
    if data.is_empty() {
        return write_text_response(&mut stream, 204, "No Content", "");
    }
    let delivery = query_value(query, "delivery").and_then(parse_prompt_delivery);
    let text = pane_input_payload(&data, delivery);

    let response = send_api_request(&Request {
        id: "desktop:pane:input".into(),
        method: Method::PaneSendText(PaneSendTextParams { pane_id, text }),
    })?;
    if response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &response);
    }
    write_json_response(&mut stream, &response)
}

fn handle_pane_dispatch(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_pane_ids) = query_value(query, "pane_ids") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing pane_ids\n");
    };
    let Some(raw_data) = query_value(query, "data") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing input data\n");
    };
    let Some(pane_ids_json) = percent_decode_string(raw_pane_ids) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid pane_ids\n");
    };
    let Some(data) = percent_decode_string(raw_data) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid input data\n");
    };
    if data.is_empty() {
        return write_text_response(&mut stream, 400, "Bad Request", "empty input data\n");
    }
    let pane_ids = match parse_dispatch_pane_ids(&pane_ids_json) {
        Ok(pane_ids) => pane_ids,
        Err(err) => {
            return write_text_response(&mut stream, 400, "Bad Request", &format!("{err}\n"))
        }
    };
    let target_label = query_value(query, "target")
        .and_then(percent_decode_string)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "selected panes".into());
    let delivery = query_value(query, "delivery").and_then(parse_prompt_delivery);
    let text = pane_input_payload(&data, delivery);

    let receipts = pane_ids
        .into_iter()
        .map(|pane_id| {
            let response = send_api_request(&Request {
                id: format!("desktop:pane:dispatch:{pane_id}"),
                method: Method::PaneSendText(PaneSendTextParams {
                    pane_id: pane_id.clone(),
                    text: text.clone(),
                }),
            });
            match response {
                Ok(value) if value.get("error").is_none() => PaneDispatchReceipt::ok(pane_id),
                Ok(value) => PaneDispatchReceipt::failed(
                    pane_id,
                    value
                        .pointer("/error/message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("send failed")
                        .to_string(),
                ),
                Err(err) => PaneDispatchReceipt::failed(pane_id, err.to_string()),
            }
        })
        .collect();

    let summary = PaneDispatchSummary::from_receipts(target_label, receipts);
    let event = record_dispatch_event(summary)?;
    write_json_response(
        &mut stream,
        &serde_json::json!({
            "id": "desktop:pane:dispatch",
            "result": {
                "type": "pane_dispatch",
                "dispatch": event,
            }
        }),
    )
}

fn handle_dispatches(mut stream: TcpStream) -> io::Result<()> {
    let dispatches = dispatch_ledger_events()?;
    write_json_response(
        &mut stream,
        &serde_json::json!({
            "id": "desktop:dispatches",
            "result": {
                "type": "dispatch_events",
                "dispatches": dispatches,
            }
        }),
    )
}

fn handle_pane_output(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_pane_id) = query_value(query, "pane_id") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing pane_id\n");
    };
    let Some(pane_id) = percent_decode_string(raw_pane_id) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid pane_id\n");
    };
    let lines = query_u16(query, "lines")
        .map(u32::from)
        .unwrap_or(80)
        .min(1000);
    let snapshot = match read_pane_output_snapshot(&pane_id, lines, 12)? {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return write_json_response_with_status(
                &mut stream,
                409,
                "Conflict",
                &serde_json::json!({
                    "id": "desktop:pane:output",
                    "error": {
                        "code": "pane_read_failed",
                        "message": error
                    }
                }),
            );
        }
    };
    write_json_response(
        &mut stream,
        &serde_json::json!({
            "id": "desktop:pane:output",
            "result": {
                "type": "pane_output",
                "output": snapshot,
            }
        }),
    )
}

fn handle_mission_workroom(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let selected_pane_id = query_value(query, "selected")
        .and_then(percent_decode_string)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let panes_response = send_api_request(&Request {
        id: "desktop:mission-workroom:panes".into(),
        method: Method::PaneList(PaneListParams { workspace_id: None }),
    })?;

    if panes_response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &panes_response);
    }

    let panes = panes_response
        .pointer("/result/panes")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let workroom =
        workroom_model::WorkroomView::from_pane_values(panes, selected_pane_id.as_deref());

    write_json_response(
        &mut stream,
        &serde_json::json!({
            "id": "desktop:mission-workroom",
            "result": {
                "type": "mission_workroom",
                "workroom": workroom,
            }
        }),
    )
}

fn handle_mission_sweep(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let lines = query_u16(query, "lines")
        .map(u32::from)
        .unwrap_or(500)
        .min(2000);
    let ingest =
        !query_value(query, "ingest").is_some_and(|value| matches!(value, "0" | "false" | "no"));

    let panes_response = send_api_request(&Request {
        id: "desktop:mission-sweep:panes".into(),
        method: Method::PaneList(PaneListParams { workspace_id: None }),
    })?;
    if panes_response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &panes_response);
    }

    let panes = panes_response
        .pointer("/result/panes")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let sweep_panes = mission_sweep_panes_from_pane_values(panes, lines, ingest)?;
    let summary = MissionSweepSummary::from_panes(&sweep_panes);
    let attention = mission_attention_items(&sweep_panes);
    let dependency_gates = mission_dependency_gates(&sweep_panes);

    write_json_response(
        &mut stream,
        &serde_json::json!({
            "id": "desktop:mission-sweep",
            "result": {
                "type": "mission_sweep",
                "ingest": ingest,
                "summary": summary,
                "attention": attention,
                "dependency_gates": dependency_gates,
                "panes": sweep_panes,
            }
        }),
    )
}

fn mission_sweep_panes_from_pane_values(
    panes: &[serde_json::Value],
    lines: u32,
    ingest: bool,
) -> io::Result<Vec<MissionSweepPane>> {
    panes
        .iter()
        .filter(|pane| {
            !pane
                .get("is_root_pane")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .map(|pane| sweep_child_pane(pane, lines, ingest))
        .collect()
}

fn handle_pane_split(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_pane_id) = query_value(query, "pane_id") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing pane_id\n");
    };
    let Some(raw_direction) = query_value(query, "direction") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing direction\n");
    };
    let Some(pane_id) = percent_decode_string(raw_pane_id) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid pane_id\n");
    };
    let direction = match raw_direction {
        "right" | "horizontal" => SplitDirection::Right,
        "down" | "vertical" => SplitDirection::Down,
        _ => {
            return write_text_response(&mut stream, 400, "Bad Request", "invalid direction\n");
        }
    };
    let cwd = query_value(query, "cwd").and_then(percent_decode_string);
    let focus =
        !query_value(query, "focus").is_some_and(|value| matches!(value, "0" | "false" | "no"));

    let response = send_api_request(&Request {
        id: "desktop:pane:split".into(),
        method: Method::PaneSplit(PaneSplitParams {
            workspace_id: None,
            target_pane_id: pane_id,
            direction,
            cwd,
            focus,
        }),
    })?;
    if response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &response);
    }
    write_json_response(&mut stream, &response)
}

fn handle_agent_start(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_target_pane_id) = query_value(query, "target_pane_id") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing target_pane_id\n");
    };
    let Some(raw_name) = query_value(query, "name") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing name\n");
    };
    let Some(raw_argv) = query_value(query, "argv") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing argv\n");
    };
    let Some(target_pane_id) = percent_decode_string(raw_target_pane_id) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid target_pane_id\n");
    };
    let Some(name) = percent_decode_string(raw_name) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid name\n");
    };
    let Some(argv_json) = percent_decode_string(raw_argv) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid argv\n");
    };
    let argv = match serde_json::from_str::<Vec<String>>(&argv_json) {
        Ok(argv) if !argv.is_empty() => argv,
        _ => return write_text_response(&mut stream, 400, "Bad Request", "invalid argv JSON\n"),
    };
    let resolved_argv = resolve_desktop_agent_argv(&argv);
    let name = name.trim().to_string();
    if name.is_empty() {
        return write_text_response(&mut stream, 400, "Bad Request", "empty name\n");
    }

    let split = match query_value(query, "direction") {
        Some("right" | "horizontal") | None => SplitDirection::Right,
        Some("down" | "vertical") => SplitDirection::Down,
        Some(_) => {
            return write_text_response(&mut stream, 400, "Bad Request", "invalid direction\n");
        }
    };
    let focus =
        !query_value(query, "focus").is_some_and(|value| matches!(value, "0" | "false" | "no"));
    let cwd = query_value(query, "cwd").and_then(percent_decode_string);
    let workspace_id = query_value(query, "workspace_id").and_then(percent_decode_string);
    let tab_id = query_value(query, "tab_id").and_then(percent_decode_string);
    let prompt_delivery = mission_import_prompt_delivery(&argv);

    let start_response = send_api_request(&Request {
        id: "desktop:agent:start".into(),
        method: Method::AgentStart(AgentStartParams {
            name: name.clone(),
            cwd,
            workspace_id,
            tab_id,
            target_pane_id: Some(target_pane_id),
            split: Some(split),
            focus,
            argv: resolved_argv.clone(),
        }),
    })?;
    if start_response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &start_response);
    }
    let Some(agent_pane_id) = start_response
        .pointer("/result/agent/pane_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
    else {
        return write_json_response_with_status(
            &mut stream,
            409,
            "Conflict",
            &serde_json::json!({
                "id": "desktop:agent:start",
                "error": {
                    "code": "missing_agent_pane",
                    "message": "agent.start did not return a pane id"
                }
            }),
        );
    };

    let title = query_value(query, "title")
        .and_then(percent_decode_string)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| name.clone());
    let mode = query_value(query, "mode")
        .and_then(parse_wave_mode)
        .unwrap_or(WaveMode::DraftOnly);
    let dependency = query_value(query, "dependency")
        .and_then(percent_decode_string)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let brief = query_value(query, "brief")
        .and_then(percent_decode_string)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let arcs = brief
        .clone()
        .map(|summary| {
            vec![WaveArc {
                id: "brief".into(),
                summary,
                status: Some(WaveStatus::Running),
            }]
        })
        .unwrap_or_default();
    let status = query_value(query, "status")
        .and_then(parse_wave_status)
        .unwrap_or(WaveStatus::Running);
    let contract = WaveContract {
        title,
        pane_id: Some(agent_pane_id.clone()),
        mode,
        status: Some(status),
        lifecycle_lane: Some(crate::wave::WaveLifecycleLane::for_status_and_report(
            status,
            &WaveReportGate::default(),
        )),
        dependency,
        report: WaveReportGate::default(),
        blast_radius: BlastRadius::Unknown,
        prompt_delivery: Some(prompt_delivery),
        arcs,
    };
    let contract_response = send_api_request(&Request {
        id: "desktop:agent:contract".into(),
        method: Method::PaneSetWaveContract(PaneSetWaveContractParams {
            pane_id: agent_pane_id.clone(),
            contract: Some(contract),
        }),
    })?;
    if contract_response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &contract_response);
    }

    let prompt_sent = match query_value(query, "prompt")
        .and_then(percent_decode_string)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        Some(prompt) => {
            let response = send_api_request(&Request {
                id: "desktop:agent:dispatch".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    pane_id: agent_pane_id,
                    text: format!("{prompt}\n"),
                }),
            })?;
            response.get("error").is_none()
        }
        None => false,
    };

    write_json_response(
        &mut stream,
        &serde_json::json!({
            "id": "desktop:agent:start",
            "result": {
                "type": "agent_child_started",
                "start": start_response.get("result").cloned(),
                "contract": contract_response.get("result").cloned(),
                "argv": argv,
                "resolved_argv": resolved_argv,
                "prompt_sent": prompt_sent
            }
        }),
    )
}

fn handle_pane_rename(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_pane_id) = query_value(query, "pane_id") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing pane_id\n");
    };
    let Some(pane_id) = percent_decode_string(raw_pane_id) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid pane_id\n");
    };
    let label = query_value(query, "label").and_then(percent_decode_string);

    let response = send_api_request(&Request {
        id: "desktop:pane:rename".into(),
        method: Method::PaneRename(PaneRenameParams { pane_id, label }),
    })?;
    if response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &response);
    }
    write_json_response(&mut stream, &response)
}

fn handle_pane_contract(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_pane_id) = query_value(query, "pane_id") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing pane_id\n");
    };
    let Some(pane_id) = percent_decode_string(raw_pane_id) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid pane_id\n");
    };
    let clear = query_bool(query, "clear");
    let contract = if clear {
        None
    } else {
        let Some(raw_title) = query_value(query, "title") else {
            return write_text_response(&mut stream, 400, "Bad Request", "missing title\n");
        };
        let Some(title) = percent_decode_string(raw_title) else {
            return write_text_response(&mut stream, 400, "Bad Request", "invalid title\n");
        };
        let title = title.trim().to_string();
        if title.is_empty() {
            return write_text_response(&mut stream, 400, "Bad Request", "empty title\n");
        }
        let mode = query_value(query, "mode")
            .and_then(parse_wave_mode)
            .unwrap_or(WaveMode::DraftOnly);
        let status = query_value(query, "status")
            .and_then(parse_wave_status)
            .or(Some(WaveStatus::Running));
        let dependency = query_value(query, "dependency")
            .and_then(percent_decode_string)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let prompt_delivery = query_value(query, "delivery").and_then(parse_prompt_delivery);
        let brief = query_value(query, "brief")
            .and_then(percent_decode_string)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let arcs = brief
            .map(|summary| {
                vec![WaveArc {
                    id: "brief".into(),
                    summary,
                    status,
                }]
            })
            .unwrap_or_default();
        Some(WaveContract {
            title,
            pane_id: Some(pane_id.clone()),
            mode,
            status,
            lifecycle_lane: status.map(|status| {
                WaveLifecycleLane::for_status_and_report(status, &WaveReportGate::default())
            }),
            dependency,
            report: WaveReportGate::default(),
            blast_radius: BlastRadius::Unknown,
            prompt_delivery,
            arcs,
        })
    };

    let response = send_api_request(&Request {
        id: "desktop:pane:contract".into(),
        method: Method::PaneSetWaveContract(PaneSetWaveContractParams { pane_id, contract }),
    })?;
    if response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &response);
    }
    write_json_response(&mut stream, &response)
}

fn handle_pane_report(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_pane_id) = query_value(query, "pane_id") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing pane_id\n");
    };
    let Some(raw_items) = query_value(query, "items") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing items\n");
    };
    let Some(pane_id) = percent_decode_string(raw_pane_id) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid pane_id\n");
    };
    let Some(items_json) = percent_decode_string(raw_items) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid items\n");
    };
    let items = match serde_json::from_str::<Vec<String>>(&items_json) {
        Ok(items) => items,
        Err(_) => {
            return write_text_response(&mut stream, 400, "Bad Request", "invalid items JSON\n")
        }
    };

    let current = send_api_request(&Request {
        id: "desktop:pane:get-report".into(),
        method: Method::PaneGet(PaneTarget {
            pane_id: pane_id.clone(),
        }),
    })?;
    if current.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &current);
    }
    let Some(contract_value) = current.pointer("/result/pane/wave_contract").cloned() else {
        return write_json_response_with_status(
            &mut stream,
            409,
            "Conflict",
            &serde_json::json!({
                "id": "desktop:pane:report",
                "error": {
                    "code": "missing_contract",
                    "message": "selected pane has no contract"
                }
            }),
        );
    };
    let mut contract =
        serde_json::from_value::<WaveContract>(contract_value).map_err(io::Error::other)?;
    contract.pane_id = Some(pane_id.clone());
    contract.report.completed_items = items;
    contract.report = contract.report.normalized();

    let response = send_api_request(&Request {
        id: "desktop:pane:report".into(),
        method: Method::PaneSetWaveContract(PaneSetWaveContractParams {
            pane_id,
            contract: Some(contract),
        }),
    })?;
    if response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &response);
    }
    write_json_response(&mut stream, &response)
}

fn handle_pane_ingest_report(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_pane_id) = query_value(query, "pane_id") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing pane_id\n");
    };
    let Some(pane_id) = percent_decode_string(raw_pane_id) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid pane_id\n");
    };
    let lines = query_u16(query, "lines")
        .map(u32::from)
        .unwrap_or(500)
        .min(2000);

    let snapshot = match read_pane_output_snapshot(&pane_id, lines, 12)? {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return write_json_response_with_status(
                &mut stream,
                409,
                "Conflict",
                &serde_json::json!({
                    "id": "desktop:pane:ingest-report",
                    "error": {
                        "code": "pane_read_failed",
                        "message": error
                    }
                }),
            );
        }
    };
    let contract = match get_pane_contract(&pane_id)? {
        Ok(contract) => contract,
        Err(error) => {
            return write_json_response_with_status(
                &mut stream,
                409,
                "Conflict",
                &serde_json::json!({
                    "id": "desktop:pane:ingest-report",
                    "error": {
                        "code": "missing_contract",
                        "message": error
                    }
                }),
            );
        }
    };
    let report = match ingest_report_text_for_contract(&pane_id, contract, &snapshot.text)? {
        Ok(report) => report,
        Err(error) => {
            return write_json_response_with_status(
                &mut stream,
                409,
                "Conflict",
                &serde_json::json!({
                    "id": "desktop:pane:ingest-report",
                    "error": {
                        "code": "report_ingest_failed",
                        "message": error
                    }
                }),
            );
        }
    };

    write_json_response(
        &mut stream,
        &serde_json::json!({
            "id": "desktop:pane:ingest-report",
            "result": {
                "type": "report_ingested",
                "detected_items": report.detected_items,
                "completed_items": report.completed_items,
                "missing_items": report.missing_items,
                "completed_fields": report.completed_fields,
                "required_fields": report.required_fields,
                "output": snapshot,
            }
        }),
    )
}

fn handle_pane_status(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_pane_id) = query_value(query, "pane_id") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing pane_id\n");
    };
    let Some(raw_status) = query_value(query, "status") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing status\n");
    };
    let Some(pane_id) = percent_decode_string(raw_pane_id) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid pane_id\n");
    };
    let Some(status) = parse_wave_status(raw_status) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid status\n");
    };

    let current = send_api_request(&Request {
        id: "desktop:pane:get-status".into(),
        method: Method::PaneGet(PaneTarget {
            pane_id: pane_id.clone(),
        }),
    })?;
    if current.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &current);
    }
    let Some(contract_value) = current.pointer("/result/pane/wave_contract").cloned() else {
        return write_json_response_with_status(
            &mut stream,
            409,
            "Conflict",
            &serde_json::json!({
                "id": "desktop:pane:status",
                "error": {
                    "code": "missing_contract",
                    "message": "selected pane has no contract"
                }
            }),
        );
    };
    let mut contract =
        serde_json::from_value::<WaveContract>(contract_value).map_err(io::Error::other)?;
    contract.pane_id = Some(pane_id.clone());
    contract.status = Some(status);
    contract.lifecycle_lane = Some(contract.lifecycle_lane_for_status(status));

    let response = send_api_request(&Request {
        id: "desktop:pane:status".into(),
        method: Method::PaneSetWaveContract(PaneSetWaveContractParams {
            pane_id,
            contract: Some(contract),
        }),
    })?;
    if response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &response);
    }
    write_json_response(&mut stream, &response)
}

fn handle_mission_unlock(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let apply =
        !query_value(query, "apply").is_some_and(|value| matches!(value, "0" | "false" | "no"));
    let panes_response = send_api_request(&Request {
        id: "desktop:mission-unlock:panes".into(),
        method: Method::PaneList(PaneListParams { workspace_id: None }),
    })?;
    if panes_response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &panes_response);
    }
    let panes = panes_response
        .pointer("/result/panes")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let sweep_panes = mission_sweep_panes_from_pane_values(panes, 80, false)?;
    let candidates = mission_unlock_candidates(&sweep_panes);
    let mut results = Vec::new();

    if apply {
        for candidate in &candidates {
            results.push(unlock_mission_pane(candidate)?);
        }
    }

    let unlocked = results.iter().filter(|result| result.prompt_sent).count();
    write_json_response(
        &mut stream,
        &serde_json::json!({
            "id": "desktop:mission-unlock",
            "result": {
                "type": "mission_unlock",
                "apply": apply,
                "candidates": candidates,
                "results": results,
                "unlocked": unlocked
            }
        }),
    )
}

fn handle_pane_close(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_pane_id) = query_value(query, "pane_id") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing pane_id\n");
    };
    let Some(pane_id) = percent_decode_string(raw_pane_id) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid pane_id\n");
    };

    let response = send_api_request(&Request {
        id: "desktop:pane:close".into(),
        method: Method::PaneClose(PaneTarget { pane_id }),
    })?;
    if response.get("error").is_some() {
        return write_json_response_with_status(&mut stream, 409, "Conflict", &response);
    }
    write_json_response(&mut stream, &response)
}

fn handle_terminal_events(mut stream: TcpStream, query: Option<&str>) -> io::Result<()> {
    let Some(raw_terminal_id) = query_value(query, "terminal_id") else {
        return write_text_response(&mut stream, 400, "Bad Request", "missing terminal_id\n");
    };
    let Some(terminal_id) = percent_decode_string(raw_terminal_id) else {
        return write_text_response(&mut stream, 400, "Bad Request", "invalid terminal_id\n");
    };
    let cols = query_u16(query, "cols")
        .unwrap_or(DEFAULT_COLS)
        .clamp(1, MAX_PREVIEW_COLS);
    let rows = query_u16(query, "rows")
        .unwrap_or(DEFAULT_ROWS)
        .clamp(1, MAX_PREVIEW_ROWS);
    let takeover = query_bool(query, "takeover");

    write!(
        stream,
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: keep-alive\r\n\
         Access-Control-Allow-Origin: *\r\n\
         \r\n"
    )?;
    stream.flush()?;

    match stream_terminal_frames(&mut stream, &terminal_id, cols, rows, takeover) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = write_sse_event(
                &mut stream,
                "terminal-error",
                &serde_json::json!({"message": err.to_string()}),
            );
            Ok(())
        }
    }
}

fn send_client_input(data: Vec<u8>, cols: u16, rows: u16) -> io::Result<()> {
    let mut client = UnixStream::connect(client_socket_path())?;
    protocol::write_message(
        &mut client,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols,
            rows,
            cell_width_px: 0,
            cell_height_px: 0,
            requested_encoding: RenderEncoding::SemanticFrame,
        },
    )
    .map_err(protocol_io_error)?;

    let welcome: ServerMessage =
        protocol::read_message(&mut client, MAX_FRAME_SIZE).map_err(protocol_io_error)?;
    match welcome {
        ServerMessage::Welcome {
            error: Some(error), ..
        } => {
            return Err(io::Error::other(format!(
                "server rejected input client: {error}"
            )));
        }
        ServerMessage::Welcome { .. } => {}
        _ => return Err(io::Error::other("expected Welcome message")),
    }

    protocol::write_message(&mut client, &ClientMessage::Input { data })
        .map_err(protocol_io_error)?;
    let _ = protocol::write_message(&mut client, &ClientMessage::Detach);
    Ok(())
}

fn send_api_request(request: &Request) -> io::Result<serde_json::Value> {
    let mut stream = UnixStream::connect(crate::api::socket_path())?;
    stream.write_all(serde_json::to_string(request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    serde_json::from_str(&line).map_err(io::Error::other)
}

#[derive(Serialize)]
struct GitStatus {
    cwd: String,
    is_repository: bool,
    branch: Option<String>,
    entries: Vec<GitStatusEntry>,
    error: Option<String>,
}

#[derive(Serialize)]
struct GitStatusEntry {
    code: String,
    path: String,
    old_path: Option<String>,
    staged: bool,
    unstaged: bool,
    untracked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PaneDispatchReceipt {
    pane_id: String,
    ok: bool,
    error: Option<String>,
}

impl PaneDispatchReceipt {
    fn ok(pane_id: String) -> Self {
        Self {
            pane_id,
            ok: true,
            error: None,
        }
    }

    fn failed(pane_id: String, error: String) -> Self {
        Self {
            pane_id,
            ok: false,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PaneDispatchSummary {
    target: String,
    requested: usize,
    sent: usize,
    failed: usize,
    receipts: Vec<PaneDispatchReceipt>,
}

impl PaneDispatchSummary {
    fn from_receipts(target: String, receipts: Vec<PaneDispatchReceipt>) -> Self {
        let requested = receipts.len();
        let sent = receipts.iter().filter(|receipt| receipt.ok).count();
        let failed = requested.saturating_sub(sent);
        Self {
            target,
            requested,
            sent,
            failed,
            receipts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PaneDispatchEvent {
    at: String,
    #[serde(flatten)]
    summary: PaneDispatchSummary,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PaneDispatchPaneReceipt {
    at: String,
    summary: PaneDispatchSummary,
    receipt: PaneDispatchReceipt,
}

#[derive(Debug, Clone)]
struct PaneDispatchLedger {
    capacity: usize,
    events: Vec<PaneDispatchEvent>,
}

impl PaneDispatchLedger {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            events: Vec::new(),
        }
    }

    fn record(&mut self, at: String, summary: PaneDispatchSummary) -> PaneDispatchEvent {
        let event = PaneDispatchEvent { at, summary };
        self.events.insert(0, event.clone());
        if self.capacity == 0 {
            self.events.clear();
        } else if self.events.len() > self.capacity {
            self.events.truncate(self.capacity);
        }
        event
    }

    fn events(&self) -> Vec<PaneDispatchEvent> {
        self.events.clone()
    }

    #[cfg(test)]
    fn latest_for_pane(&self, pane_id: &str) -> Option<PaneDispatchPaneReceipt> {
        self.events.iter().find_map(|event| {
            event
                .summary
                .receipts
                .iter()
                .find(|receipt| receipt.pane_id == pane_id)
                .map(|receipt| PaneDispatchPaneReceipt {
                    at: event.at.clone(),
                    summary: event.summary.clone(),
                    receipt: receipt.clone(),
                })
        })
    }
}

static DISPATCH_LEDGER: OnceLock<Mutex<PaneDispatchLedger>> = OnceLock::new();

fn dispatch_ledger() -> &'static Mutex<PaneDispatchLedger> {
    DISPATCH_LEDGER.get_or_init(|| Mutex::new(PaneDispatchLedger::new(MAX_DISPATCH_EVENTS)))
}

fn record_dispatch_event(summary: PaneDispatchSummary) -> io::Result<PaneDispatchEvent> {
    let mut ledger = dispatch_ledger()
        .lock()
        .map_err(|_| io::Error::other("dispatch ledger lock poisoned"))?;
    Ok(ledger.record(dispatch_timestamp(), summary))
}

fn dispatch_ledger_events() -> io::Result<Vec<PaneDispatchEvent>> {
    let ledger = dispatch_ledger()
        .lock()
        .map_err(|_| io::Error::other("dispatch ledger lock poisoned"))?;
    Ok(ledger.events())
}

fn dispatch_timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs().to_string(),
        Err(_) => "0".into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PaneOutputSnapshot {
    pane_id: String,
    text: String,
    line_count: usize,
    nonempty_line_count: usize,
    last_nonempty_line: Option<String>,
    tail_lines: Vec<String>,
}

impl PaneOutputSnapshot {
    fn from_text(pane_id: String, text: String, max_tail_lines: usize) -> Self {
        let line_count = text.lines().count();
        let nonempty_lines: Vec<String> = text
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        let tail_start = nonempty_lines.len().saturating_sub(max_tail_lines);
        let tail_lines = nonempty_lines[tail_start..].to_vec();
        let last_nonempty_line = nonempty_lines.last().cloned();
        Self {
            pane_id,
            text,
            line_count,
            nonempty_line_count: nonempty_lines.len(),
            last_nonempty_line,
            tail_lines,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReportIngestSnapshot {
    detected_items: Vec<String>,
    completed_items: Vec<String>,
    missing_items: Vec<String>,
    completed_fields: u8,
    required_fields: u8,
}

impl ReportIngestSnapshot {
    fn from_gates(detected: &WaveReportGate, report: &WaveReportGate) -> Self {
        Self {
            detected_items: detected.completed_items.clone(),
            completed_items: report.completed_items.clone(),
            missing_items: report_missing_items(report),
            completed_fields: report.completed_fields,
            required_fields: report.required_fields,
        }
    }

    fn ready(&self) -> bool {
        self.completed_fields >= self.required_fields && self.required_fields > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionSweepPane {
    pane_id: String,
    title: String,
    status: Option<WaveStatus>,
    dependency: Option<String>,
    arc_ids: Vec<String>,
    output: Option<PaneOutputSnapshot>,
    report: Option<ReportIngestSnapshot>,
    error: Option<String>,
}

#[cfg(test)]
impl MissionSweepPane {
    fn with_error(mut self, error: Option<&str>) -> Self {
        self.error = error.map(str::to_string);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionSweepSummary {
    children: usize,
    read: usize,
    ingested: usize,
    ready_packets: usize,
    needs_attention: usize,
    failed: usize,
}

impl MissionSweepSummary {
    fn from_panes(panes: &[MissionSweepPane]) -> Self {
        let children = panes.len();
        let read = panes.iter().filter(|pane| pane.output.is_some()).count();
        let ingested = panes.iter().filter(|pane| pane.report.is_some()).count();
        let ready_packets = panes
            .iter()
            .filter(|pane| {
                pane.report
                    .as_ref()
                    .is_some_and(ReportIngestSnapshot::ready)
            })
            .count();
        let failed = panes.iter().filter(|pane| pane.error.is_some()).count();
        let needs_attention = children.saturating_sub(ready_packets);
        Self {
            children,
            read,
            ingested,
            ready_packets,
            needs_attention,
            failed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MissionAttentionKind {
    Error,
    MissingPacket,
    MissingContract,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MissionDependencyGateStatus {
    Ready,
    WaitingPacket,
    NeedsAcceptance,
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionDependencyGate {
    pane_id: String,
    title: String,
    dependency: String,
    status: MissionDependencyGateStatus,
    reason: String,
    upstream_pane_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionUnlockCandidate {
    pane_id: String,
    title: String,
    dependency: String,
    reason: String,
    upstream_pane_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionUnlockResult {
    pane_id: String,
    title: String,
    prompt_sent: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionAttentionItem {
    pane_id: String,
    title: String,
    kind: MissionAttentionKind,
    message: String,
    missing_items: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MissionImportAssignmentStatus {
    Ready,
    Applied,
    Created,
    NoPane,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MissionImportPlannedAction {
    ReuseExisting,
    CreateNew,
    NeedsPane,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MissionImportPromptScope {
    None,
    Created,
    All,
}

impl MissionImportPromptScope {
    fn should_prompt(self, created_pane: bool) -> bool {
        match self {
            Self::None => false,
            Self::Created => created_pane,
            Self::All => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionImportAssignment {
    contract_title: String,
    pane_id: Option<String>,
    terminal_id: Option<String>,
    status: MissionImportAssignmentStatus,
    planned_action: MissionImportPlannedAction,
    gate_status: MissionDependencyGateStatus,
    gate_reason: Option<String>,
    gate_upstream_titles: Vec<String>,
    error: Option<String>,
    prompt_sent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MissionImportPaneTarget {
    pane_id: String,
    terminal_id: Option<String>,
    contract_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MissionImportChildIdentity {
    pane_id: String,
    terminal_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissionImportReuseMode {
    Positional,
    TitleOnly,
}

#[cfg(test)]
fn mission_import_plan(
    contracts: &[WaveContract],
    child_pane_ids: &[String],
) -> Vec<MissionImportAssignment> {
    let targets: Vec<MissionImportPaneTarget> = child_pane_ids
        .iter()
        .map(|pane_id| MissionImportPaneTarget {
            pane_id: pane_id.clone(),
            terminal_id: None,
            contract_title: None,
        })
        .collect();
    mission_import_plan_for_targets(contracts, &targets)
}

#[cfg(test)]
fn mission_import_plan_for_targets(
    contracts: &[WaveContract],
    targets: &[MissionImportPaneTarget],
) -> Vec<MissionImportAssignment> {
    mission_import_plan_for_targets_with_reuse(
        contracts,
        targets,
        MissionImportReuseMode::Positional,
    )
}

fn mission_import_plan_for_targets_with_reuse(
    contracts: &[WaveContract],
    targets: &[MissionImportPaneTarget],
    reuse_mode: MissionImportReuseMode,
) -> Vec<MissionImportAssignment> {
    let mut used_targets = vec![false; targets.len()];
    contracts
        .iter()
        .enumerate()
        .map(|(index, contract)| {
            let title_match = targets
                .iter()
                .enumerate()
                .find(|(target_index, target)| {
                    !used_targets[*target_index]
                        && target
                            .contract_title
                            .as_deref()
                            .is_some_and(|title| title.eq_ignore_ascii_case(&contract.title))
                })
                .map(|(target_index, _)| target_index);
            let positional_match = || {
                if reuse_mode != MissionImportReuseMode::Positional {
                    return None;
                }
                targets
                    .iter()
                    .enumerate()
                    .filter(|(target_index, _)| !used_targets[*target_index])
                    .nth(
                        index.saturating_sub(
                            used_targets
                                .iter()
                                .take(index)
                                .filter(|used| **used)
                                .count(),
                        ),
                    )
                    .map(|(target_index, _)| target_index)
                    .or_else(|| {
                        targets
                            .iter()
                            .enumerate()
                            .find(|(target_index, _)| !used_targets[*target_index])
                            .map(|(target_index, _)| target_index)
                    })
            };
            let matched_index = title_match.or_else(positional_match);
            let matched_target = matched_index.map(|target_index| {
                used_targets[target_index] = true;
                targets[target_index].clone()
            });
            let pane_id = matched_target.as_ref().map(|target| target.pane_id.clone());
            let terminal_id = matched_target.and_then(|target| target.terminal_id);
            let status = if pane_id.is_some() {
                MissionImportAssignmentStatus::Ready
            } else {
                MissionImportAssignmentStatus::NoPane
            };
            MissionImportAssignment {
                contract_title: contract.title.clone(),
                pane_id,
                terminal_id,
                status,
                planned_action: MissionImportPlannedAction::NeedsPane,
                gate_status: MissionDependencyGateStatus::Ready,
                gate_reason: None,
                gate_upstream_titles: Vec::new(),
                error: None,
                prompt_sent: false,
            }
        })
        .collect()
}

fn mission_import_mark_planned_actions(
    assignments: &mut [MissionImportAssignment],
    create_missing: bool,
) {
    for assignment in assignments {
        assignment.planned_action = if assignment.pane_id.is_some() {
            MissionImportPlannedAction::ReuseExisting
        } else if create_missing && assignment.status == MissionImportAssignmentStatus::NoPane {
            MissionImportPlannedAction::CreateNew
        } else {
            MissionImportPlannedAction::NeedsPane
        };
    }
}

fn mission_import_apply_dependency_gates(
    assignments: &mut [MissionImportAssignment],
    contracts: &[WaveContract],
    gate_dependencies: bool,
) {
    let gates = mission_import_dependency_gates_for_contracts(contracts);
    for (assignment, gate) in assignments.iter_mut().zip(gates) {
        assignment.gate_status = if gate_dependencies {
            gate.status
        } else {
            MissionDependencyGateStatus::Ready
        };
        assignment.gate_reason = Some(gate.reason).filter(|_| gate_dependencies);
        assignment.gate_upstream_titles = gate.upstream_pane_ids;
    }
}

fn mission_import_dependency_gates_for_contracts(
    contracts: &[WaveContract],
) -> Vec<MissionDependencyGate> {
    let sweep_panes: Vec<MissionSweepPane> = contracts
        .iter()
        .map(|contract| MissionSweepPane {
            pane_id: contract.title.clone(),
            title: contract.title.clone(),
            status: contract.status,
            dependency: contract.dependency.clone(),
            arc_ids: contract
                .arcs
                .iter()
                .map(|arc| arc.id.trim().to_string())
                .filter(|id| !id.is_empty())
                .collect(),
            output: None,
            report: Some(ReportIngestSnapshot::from_gates(
                &contract.report,
                &contract.report,
            )),
            error: None,
        })
        .collect();
    mission_dependency_gates(&sweep_panes)
}

fn mission_import_contract_status(
    existing: Option<WaveStatus>,
    prompt_sent: bool,
    gate_status: MissionDependencyGateStatus,
) -> WaveStatus {
    if existing == Some(WaveStatus::Accepted) {
        return WaveStatus::Accepted;
    }
    if prompt_sent {
        return WaveStatus::Running;
    }
    if gate_status != MissionDependencyGateStatus::Ready {
        return WaveStatus::Queued;
    }
    existing.unwrap_or(WaveStatus::Queued)
}

fn mission_import_applied_count(assignments: &[MissionImportAssignment]) -> usize {
    assignments
        .iter()
        .filter(|assignment| {
            matches!(
                assignment.status,
                MissionImportAssignmentStatus::Applied | MissionImportAssignmentStatus::Created
            )
        })
        .count()
}

fn mission_import_created_count(assignments: &[MissionImportAssignment]) -> usize {
    assignments
        .iter()
        .filter(|assignment| assignment.status == MissionImportAssignmentStatus::Created)
        .count()
}

fn mission_import_missing_panes_count(assignments: &[MissionImportAssignment]) -> usize {
    assignments
        .iter()
        .filter(|assignment| assignment.status == MissionImportAssignmentStatus::NoPane)
        .count()
}

fn mission_import_planned_reuse_count(assignments: &[MissionImportAssignment]) -> usize {
    assignments
        .iter()
        .filter(|assignment| {
            assignment.pane_id.is_some()
                && assignment.status == MissionImportAssignmentStatus::Ready
        })
        .count()
}

fn mission_import_planned_create_count(
    assignments: &[MissionImportAssignment],
    create_missing: bool,
) -> usize {
    if !create_missing {
        return 0;
    }
    assignments
        .iter()
        .filter(|assignment| assignment.status == MissionImportAssignmentStatus::NoPane)
        .count()
}

fn mission_import_planned_prompt_count(
    assignments: &[MissionImportAssignment],
    create_missing: bool,
    prompt_scope: MissionImportPromptScope,
) -> usize {
    match prompt_scope {
        MissionImportPromptScope::None => 0,
        MissionImportPromptScope::Created => {
            if !create_missing {
                return 0;
            }
            assignments
                .iter()
                .filter(|assignment| {
                    assignment.status == MissionImportAssignmentStatus::NoPane
                        && assignment.gate_status == MissionDependencyGateStatus::Ready
                })
                .count()
        }
        MissionImportPromptScope::All => assignments
            .iter()
            .filter(|assignment| {
                assignment.gate_status == MissionDependencyGateStatus::Ready
                    && (assignment.pane_id.is_some()
                        || (create_missing
                            && assignment.status == MissionImportAssignmentStatus::NoPane))
            })
            .count(),
    }
}

fn parse_mission_import_prompt_scope(value: Option<&str>) -> MissionImportPromptScope {
    match value {
        Some("created" | "create" | "new" | "true" | "1" | "yes") => {
            MissionImportPromptScope::Created
        }
        Some("all" | "every") => MissionImportPromptScope::All,
        _ => MissionImportPromptScope::None,
    }
}

fn child_pane_targets_from_panes_response(
    response: &serde_json::Value,
) -> Vec<MissionImportPaneTarget> {
    response
        .pointer("/result/panes")
        .and_then(serde_json::Value::as_array)
        .map(|panes| {
            panes
                .iter()
                .filter(|pane| {
                    !pane
                        .get("is_root_pane")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                })
                .filter_map(|pane| {
                    Some(MissionImportPaneTarget {
                        pane_id: pane.get("pane_id")?.as_str()?.to_string(),
                        terminal_id: pane
                            .get("terminal_id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                        contract_title: pane
                            .pointer("/wave_contract/title")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn root_pane_id_from_panes_response(response: &serde_json::Value) -> Option<String> {
    let panes = response
        .pointer("/result/panes")
        .and_then(serde_json::Value::as_array)?;
    panes
        .iter()
        .find(|pane| {
            pane.get("is_root_pane")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .or_else(|| panes.first())
        .and_then(|pane| pane.get("pane_id").and_then(serde_json::Value::as_str))
        .map(str::to_string)
}

fn mission_import_launch_argv(query: Option<&str>) -> Result<Vec<String>, &'static str> {
    let Some(raw_argv) = query_value(query, "argv") else {
        return Ok(default_mission_import_launch_argv());
    };
    let Some(argv_json) = percent_decode_string(raw_argv) else {
        return Err("invalid argv\n");
    };
    let argv =
        serde_json::from_str::<Vec<String>>(&argv_json).map_err(|_| "invalid argv JSON\n")?;
    let argv: Vec<String> = argv
        .into_iter()
        .map(|arg| arg.trim().to_string())
        .filter(|arg| !arg.is_empty())
        .collect();
    if argv.is_empty() {
        return Err("empty argv\n");
    }
    Ok(argv)
}

fn default_mission_import_launch_argv() -> Vec<String> {
    vec!["/bin/zsh".into(), "-l".into()]
}

fn create_mission_import_child_pane(
    target_pane_id: &str,
    contract: &WaveContract,
    argv: &[String],
) -> io::Result<Result<MissionImportChildIdentity, String>> {
    let response = send_api_request(&Request {
        id: format!("desktop:mission-import:create:{target_pane_id}"),
        method: Method::AgentStart(AgentStartParams {
            name: contract.title.clone(),
            cwd: None,
            workspace_id: None,
            tab_id: None,
            target_pane_id: Some(target_pane_id.to_string()),
            split: Some(SplitDirection::Right),
            focus: false,
            argv: argv.to_vec(),
        }),
    })?;

    if response.get("error").is_some() {
        let message = response
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("child pane creation failed")
            .to_string();
        return Ok(Err(message));
    }

    Ok(mission_import_child_identity_from_start_response(&response))
}

fn mission_import_child_identity_from_start_response(
    response: &serde_json::Value,
) -> Result<MissionImportChildIdentity, String> {
    let Some(pane_id) = response
        .pointer("/result/agent/pane_id")
        .and_then(serde_json::Value::as_str)
    else {
        return Err("agent.start did not return a pane id".into());
    };
    let terminal_id = response
        .pointer("/result/agent/terminal_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    Ok(MissionImportChildIdentity {
        pane_id: pane_id.to_string(),
        terminal_id,
    })
}

fn send_mission_import_contract_prompt(
    session_path: &str,
    pane_id: &str,
    contract: &WaveContract,
) -> io::Result<Result<(), String>> {
    let report_fields = mission_import_report_prompt_fields(contract);
    let prompt = mission_import_contract_payload(session_path, pane_id, contract, &report_fields);
    let response = send_api_request(&Request {
        id: format!("desktop:mission-import:prompt:{pane_id}"),
        method: Method::PaneSendText(PaneSendTextParams {
            pane_id: pane_id.to_string(),
            text: prompt,
        }),
    })?;

    if response.get("error").is_some() {
        let message = response
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("contract prompt dispatch failed")
            .to_string();
        return Ok(Err(message));
    }

    Ok(Ok(()))
}

fn mission_import_prompt_delivery(argv: &[String]) -> WavePromptDelivery {
    let Some(command) = argv.first() else {
        return WavePromptDelivery::ShellCard;
    };
    let command = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    match command.as_str() {
        "sh" | "bash" | "zsh" | "fish" | "dash" => WavePromptDelivery::ShellCard,
        _ => WavePromptDelivery::Agent,
    }
}

fn mission_import_contract_payload(
    session_path: &str,
    pane_id: &str,
    contract: &WaveContract,
    report_fields: &[String],
) -> String {
    let prompt = mission_import_contract_prompt(session_path, pane_id, contract, report_fields);
    match contract
        .prompt_delivery
        .unwrap_or(WavePromptDelivery::Agent)
    {
        WavePromptDelivery::Agent => prompt,
        WavePromptDelivery::ShellCard => shell_contract_card_payload(&prompt),
    }
}

fn shell_contract_card_payload(prompt: &str) -> String {
    let normalized = prompt.replace('\r', "\n");
    let mut lines: Vec<String> = normalized
        .trim_end_matches('\n')
        .lines()
        .map(shell_single_quote)
        .collect();
    if lines.is_empty() {
        lines.push(shell_single_quote(""));
    }
    format!("printf '%s\\n' {}\n", lines.join(" "))
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn unlock_mission_pane(candidate: &MissionUnlockCandidate) -> io::Result<MissionUnlockResult> {
    let current = send_api_request(&Request {
        id: format!("desktop:mission-unlock:get:{}", candidate.pane_id),
        method: Method::PaneGet(PaneTarget {
            pane_id: candidate.pane_id.clone(),
        }),
    })?;
    if current.get("error").is_some() {
        return Ok(MissionUnlockResult {
            pane_id: candidate.pane_id.clone(),
            title: candidate.title.clone(),
            prompt_sent: false,
            error: Some(response_error_message(&current, "pane lookup failed")),
        });
    }
    let Some(contract_value) = current.pointer("/result/pane/wave_contract").cloned() else {
        return Ok(MissionUnlockResult {
            pane_id: candidate.pane_id.clone(),
            title: candidate.title.clone(),
            prompt_sent: false,
            error: Some("selected pane has no contract".into()),
        });
    };
    let mut contract =
        serde_json::from_value::<WaveContract>(contract_value).map_err(io::Error::other)?;
    contract.pane_id = Some(candidate.pane_id.clone());
    contract.status = Some(WaveStatus::Running);

    let update = send_api_request(&Request {
        id: format!("desktop:mission-unlock:status:{}", candidate.pane_id),
        method: Method::PaneSetWaveContract(PaneSetWaveContractParams {
            pane_id: candidate.pane_id.clone(),
            contract: Some(contract.clone()),
        }),
    })?;
    if update.get("error").is_some() {
        return Ok(MissionUnlockResult {
            pane_id: candidate.pane_id.clone(),
            title: candidate.title.clone(),
            prompt_sent: false,
            error: Some(response_error_message(&update, "contract update failed")),
        });
    }

    match send_mission_import_contract_prompt("mission unlock", &candidate.pane_id, &contract)? {
        Ok(()) => Ok(MissionUnlockResult {
            pane_id: candidate.pane_id.clone(),
            title: candidate.title.clone(),
            prompt_sent: true,
            error: None,
        }),
        Err(error) => Ok(MissionUnlockResult {
            pane_id: candidate.pane_id.clone(),
            title: candidate.title.clone(),
            prompt_sent: false,
            error: Some(error),
        }),
    }
}

fn mission_import_report_prompt_fields(contract: &WaveContract) -> Vec<String> {
    let required = usize::from(contract.report.required_fields.max(1));
    let mut fields = Vec::new();
    for item in &contract.report.completed_items {
        let item = item.trim();
        if item.is_empty()
            || fields
                .iter()
                .any(|seen: &String| seen.eq_ignore_ascii_case(item))
        {
            continue;
        }
        fields.push(item.to_string());
    }
    let defaults = default_report_packet_items();
    for item in defaults {
        if fields.len() >= required {
            break;
        }
        if fields.iter().any(|seen| seen.eq_ignore_ascii_case(item)) {
            continue;
        }
        fields.push((*item).to_string());
    }
    while fields.len() < required {
        fields.push(format!("Required field {}", fields.len() + 1));
    }
    fields
}

fn mission_import_contract_prompt(
    session_path: &str,
    pane_id: &str,
    contract: &WaveContract,
    report_fields: &[String],
) -> String {
    let arcs = if contract.arcs.is_empty() {
        "- no arcs declared".to_string()
    } else {
        contract
            .arcs
            .iter()
            .map(|arc| {
                let id = if arc.id.is_empty() {
                    "arc"
                } else {
                    arc.id.as_str()
                };
                let summary = if arc.summary.is_empty() {
                    "no summary"
                } else {
                    arc.summary.as_str()
                };
                format!("- {id}: {summary}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let report_rows = report_fields
        .iter()
        .enumerate()
        .map(|(index, field)| format!("{}. {}", index + 1, field))
        .collect::<Vec<_>>()
        .join("\n");
    let dependency = contract.dependency.as_deref().unwrap_or("parallel OK");
    let status = contract
        .status
        .map(|status| format!("{status:?}"))
        .unwrap_or_else(|| "Queued".into());

    format!(
        "Mission import source: {session_path}\n\
         Mission contract: {title}\n\
         Pane id: {pane_id}\n\
         Mode: {mode}\n\
         Status: {status}\n\
         Dependency: {dependency}\n\
         Blast radius: {blast}\n\
         \n\
         Contract arcs:\n{arcs}\n\
         \n\
         Required report packet:\n{report_rows}\n\
         \n\
         Start inside this child pane. Use px if it is available on this system, stay inside the contract scope, and report blockers before widening scope.\n",
        title = contract.title,
        mode = contract.mode.label(),
        blast = contract.blast_radius.label()
    )
}

fn read_pane_output_snapshot(
    pane_id: &str,
    lines: u32,
    max_tail_lines: usize,
) -> io::Result<Result<PaneOutputSnapshot, String>> {
    let response = send_api_request(&Request {
        id: format!("desktop:pane:read-output:{pane_id}"),
        method: Method::PaneRead(PaneReadParams {
            pane_id: pane_id.to_string(),
            source: ReadSource::RecentUnwrapped,
            lines: Some(lines),
            format: ReadFormat::Text,
            strip_ansi: true,
        }),
    })?;
    if response.get("error").is_some() {
        return Ok(Err(response_error_message(&response, "pane read failed")));
    }

    let text = response
        .pointer("/result/read/text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(Ok(PaneOutputSnapshot::from_text(
        pane_id.to_string(),
        text,
        max_tail_lines,
    )))
}

fn get_pane_contract(pane_id: &str) -> io::Result<Result<WaveContract, String>> {
    let current = send_api_request(&Request {
        id: format!("desktop:pane:get-contract:{pane_id}"),
        method: Method::PaneGet(PaneTarget {
            pane_id: pane_id.to_string(),
        }),
    })?;
    if current.get("error").is_some() {
        return Ok(Err(response_error_message(&current, "pane get failed")));
    }
    let Some(contract_value) = current.pointer("/result/pane/wave_contract").cloned() else {
        return Ok(Err("selected pane has no contract".into()));
    };
    serde_json::from_value::<WaveContract>(contract_value)
        .map(Ok)
        .map_err(io::Error::other)
}

fn ingest_report_text_for_contract(
    pane_id: &str,
    mut contract: WaveContract,
    text: &str,
) -> io::Result<Result<ReportIngestSnapshot, String>> {
    let detected = derive_default_report_gate(text);
    contract.pane_id = Some(pane_id.to_string());
    contract.report = merge_report_gates(&contract.report, &detected);
    let report = contract.report.clone();

    let response = send_api_request(&Request {
        id: format!("desktop:pane:ingest-report:{pane_id}"),
        method: Method::PaneSetWaveContract(PaneSetWaveContractParams {
            pane_id: pane_id.to_string(),
            contract: Some(contract),
        }),
    })?;
    if response.get("error").is_some() {
        return Ok(Err(response_error_message(
            &response,
            "contract update failed",
        )));
    }

    Ok(Ok(ReportIngestSnapshot::from_gates(&detected, &report)))
}

fn sweep_child_pane(
    pane: &serde_json::Value,
    lines: u32,
    ingest: bool,
) -> io::Result<MissionSweepPane> {
    let pane_id = pane
        .get("pane_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let title = pane
        .pointer("/wave_contract/title")
        .and_then(serde_json::Value::as_str)
        .or_else(|| pane.get("label").and_then(serde_json::Value::as_str))
        .or_else(|| pane.get("agent").and_then(serde_json::Value::as_str))
        .unwrap_or(&pane_id)
        .to_string();
    let parsed_contract = pane
        .get("wave_contract")
        .cloned()
        .map(serde_json::from_value::<WaveContract>);
    let status = parsed_contract
        .as_ref()
        .and_then(|contract| contract.as_ref().ok())
        .and_then(|contract| contract.status);
    let dependency = parsed_contract
        .as_ref()
        .and_then(|contract| contract.as_ref().ok())
        .and_then(|contract| contract.dependency.clone());
    let arc_ids = parsed_contract
        .as_ref()
        .and_then(|contract| contract.as_ref().ok())
        .map(|contract| {
            contract
                .arcs
                .iter()
                .map(|arc| arc.id.trim().to_string())
                .filter(|id| !id.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if pane_id.is_empty() {
        return Ok(MissionSweepPane {
            pane_id,
            title,
            status,
            dependency,
            arc_ids,
            output: None,
            report: None,
            error: Some("pane is missing pane_id".into()),
        });
    }

    let output = match read_pane_output_snapshot(&pane_id, lines, 12)? {
        Ok(output) => output,
        Err(error) => {
            return Ok(MissionSweepPane {
                pane_id,
                title,
                status,
                dependency,
                arc_ids,
                output: None,
                report: None,
                error: Some(error),
            });
        }
    };
    let report = if ingest {
        match parsed_contract {
            Some(Ok(contract)) => {
                match ingest_report_text_for_contract(&pane_id, contract, &output.text)? {
                    Ok(report) => Some(report),
                    Err(error) => {
                        return Ok(MissionSweepPane {
                            pane_id,
                            title,
                            status,
                            dependency,
                            arc_ids,
                            output: Some(output),
                            report: None,
                            error: Some(error),
                        });
                    }
                }
            }
            Some(Err(err)) => {
                return Ok(MissionSweepPane {
                    pane_id,
                    title,
                    status,
                    dependency,
                    arc_ids,
                    output: Some(output),
                    report: None,
                    error: Some(err.to_string()),
                });
            }
            None => None,
        }
    } else {
        None
    };

    Ok(MissionSweepPane {
        pane_id,
        title,
        status,
        dependency,
        arc_ids,
        output: Some(output),
        report,
        error: None,
    })
}

fn report_missing_items(report: &WaveReportGate) -> Vec<String> {
    if report.completed_fields >= report.required_fields {
        return Vec::new();
    }
    let missing_count = report
        .required_fields
        .saturating_sub(report.completed_fields)
        .into();
    default_report_packet_items()
        .iter()
        .filter(|item| {
            !report
                .completed_items
                .iter()
                .any(|completed| completed.eq_ignore_ascii_case(item))
        })
        .take(missing_count)
        .map(|item| (*item).to_string())
        .collect()
}

fn response_error_message(response: &serde_json::Value, fallback: &str) -> String {
    response
        .pointer("/error/message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

fn mission_attention_items(panes: &[MissionSweepPane]) -> Vec<MissionAttentionItem> {
    panes
        .iter()
        .filter_map(|pane| {
            if let Some(error) = pane.error.as_deref() {
                return Some(MissionAttentionItem {
                    pane_id: pane.pane_id.clone(),
                    title: pane.title.clone(),
                    kind: MissionAttentionKind::Error,
                    message: error.to_string(),
                    missing_items: Vec::new(),
                });
            }
            let Some(report) = pane.report.as_ref() else {
                return Some(MissionAttentionItem {
                    pane_id: pane.pane_id.clone(),
                    title: pane.title.clone(),
                    kind: MissionAttentionKind::MissingContract,
                    message: "child pane has no report contract".into(),
                    missing_items: Vec::new(),
                });
            };
            if report.ready() {
                return None;
            }
            let missing = report.missing_items.clone();
            let missing_count = usize::from(
                report
                    .required_fields
                    .saturating_sub(report.completed_fields),
            );
            Some(MissionAttentionItem {
                pane_id: pane.pane_id.clone(),
                title: pane.title.clone(),
                kind: MissionAttentionKind::MissingPacket,
                message: format!(
                    "{} packet field{} missing",
                    missing_count,
                    if missing_count == 1 { "" } else { "s" }
                ),
                missing_items: missing,
            })
        })
        .collect()
}

fn mission_dependency_gates(panes: &[MissionSweepPane]) -> Vec<MissionDependencyGate> {
    panes
        .iter()
        .enumerate()
        .map(|(index, pane)| mission_dependency_gate_for_pane(index, pane, panes))
        .collect()
}

fn mission_unlock_candidates(panes: &[MissionSweepPane]) -> Vec<MissionUnlockCandidate> {
    mission_dependency_gates(panes)
        .into_iter()
        .filter(|gate| gate.status == MissionDependencyGateStatus::Ready)
        .filter(|gate| !dependency_is_parallel(&gate.dependency))
        .filter(|gate| {
            panes
                .iter()
                .any(|pane| pane.pane_id == gate.pane_id && pane.status == Some(WaveStatus::Queued))
        })
        .map(|gate| MissionUnlockCandidate {
            pane_id: gate.pane_id,
            title: gate.title,
            dependency: gate.dependency,
            reason: gate.reason,
            upstream_pane_ids: gate.upstream_pane_ids,
        })
        .collect()
}

fn mission_dependency_gate_for_pane(
    index: usize,
    pane: &MissionSweepPane,
    panes: &[MissionSweepPane],
) -> MissionDependencyGate {
    let dependency = pane
        .dependency
        .as_deref()
        .map(str::trim)
        .filter(|dependency| !dependency.is_empty())
        .unwrap_or("parallel");
    if dependency_is_parallel(dependency) {
        return MissionDependencyGate {
            pane_id: pane.pane_id.clone(),
            title: pane.title.clone(),
            dependency: dependency.to_string(),
            status: MissionDependencyGateStatus::Ready,
            reason: "parallel wave can run now".into(),
            upstream_pane_ids: Vec::new(),
        };
    }

    let requirements = dependency_requirements(dependency);
    let matching_upstreams: Vec<&MissionSweepPane> = panes
        .iter()
        .enumerate()
        .filter(|(candidate_index, _)| *candidate_index != index)
        .map(|(_, candidate)| candidate)
        .filter(|candidate| dependency_matches_pane(&requirements, candidate))
        .collect();
    let upstreams = select_dependency_upstreams(pane, matching_upstreams);
    if upstreams.is_empty() {
        return MissionDependencyGate {
            pane_id: pane.pane_id.clone(),
            title: pane.title.clone(),
            dependency: dependency.to_string(),
            status: MissionDependencyGateStatus::Unresolved,
            reason: format!("no upstream pane matched {dependency}"),
            upstream_pane_ids: Vec::new(),
        };
    }

    let upstream_pane_ids = upstreams
        .iter()
        .map(|upstream| upstream.pane_id.clone())
        .collect();
    if upstreams
        .iter()
        .all(|upstream| upstream.status == Some(WaveStatus::Accepted))
    {
        return MissionDependencyGate {
            pane_id: pane.pane_id.clone(),
            title: pane.title.clone(),
            dependency: dependency.to_string(),
            status: MissionDependencyGateStatus::Ready,
            reason: "upstream packet accepted".into(),
            upstream_pane_ids,
        };
    }
    if upstreams.iter().any(|upstream| {
        upstream
            .report
            .as_ref()
            .is_none_or(|report| !report.ready())
    }) {
        return MissionDependencyGate {
            pane_id: pane.pane_id.clone(),
            title: pane.title.clone(),
            dependency: dependency.to_string(),
            status: MissionDependencyGateStatus::WaitingPacket,
            reason: "waiting for upstream report packet".into(),
            upstream_pane_ids,
        };
    }

    MissionDependencyGate {
        pane_id: pane.pane_id.clone(),
        title: pane.title.clone(),
        dependency: dependency.to_string(),
        status: MissionDependencyGateStatus::NeedsAcceptance,
        reason: "upstream packet is complete but not accepted".into(),
        upstream_pane_ids,
    }
}

fn dependency_is_parallel(dependency: &str) -> bool {
    let dependency = dependency.trim();
    dependency.eq_ignore_ascii_case("parallel")
        || dependency.eq_ignore_ascii_case("parallel ok")
        || dependency.eq_ignore_ascii_case("none")
        || dependency.eq_ignore_ascii_case("no dependency")
}

fn dependency_requirements(dependency: &str) -> Vec<String> {
    let lower = dependency.to_ascii_lowercase();
    let mut normalized = dependency
        .get(
            lower
                .find("after ")
                .map(|index| index + "after ".len())
                .unwrap_or(0)..,
        )
        .unwrap_or(dependency)
        .to_string();
    for word in ["packet", "accepted", "approval", "parent", "then"] {
        normalized = normalized.replace(word, " ");
    }
    normalized
        .split(|ch: char| ch == '+' || ch == ',' || ch == ';' || ch == '/' || ch.is_whitespace())
        .map(|part| part.trim().trim_matches(|ch: char| !ch.is_alphanumeric()))
        .filter(|part| !part.is_empty() && !part.eq_ignore_ascii_case("and"))
        .map(|part| part.to_ascii_lowercase())
        .collect()
}

fn dependency_matches_pane(requirements: &[String], pane: &MissionSweepPane) -> bool {
    if requirements.is_empty() {
        return false;
    }
    let title = pane.title.to_ascii_lowercase();
    let arc_ids: Vec<String> = pane
        .arc_ids
        .iter()
        .map(|arc| arc.to_ascii_lowercase())
        .collect();
    requirements.iter().all(|requirement| {
        title.contains(requirement)
            || arc_ids.iter().any(|arc| arc == requirement)
            || wave_number_requirement_matches_title(requirement, &title)
    })
}

fn select_dependency_upstreams<'a>(
    pane: &MissionSweepPane,
    candidates: Vec<&'a MissionSweepPane>,
) -> Vec<&'a MissionSweepPane> {
    let Some(current_number) = wave_number_from_title(&pane.title) else {
        return candidates;
    };
    let nearest = candidates
        .iter()
        .filter_map(|candidate| {
            let number = wave_number_from_title(&candidate.title)?;
            (number < current_number).then_some(number)
        })
        .max();
    let Some(nearest) = nearest else {
        return candidates;
    };
    candidates
        .into_iter()
        .filter(|candidate| wave_number_from_title(&candidate.title) == Some(nearest))
        .collect()
}

fn wave_number_requirement_matches_title(requirement: &str, title: &str) -> bool {
    let Some(number) = requirement.strip_prefix('w') else {
        return false;
    };
    !number.is_empty()
        && number.chars().all(|ch| ch.is_ascii_digit())
        && title.contains(&format!("wave {number}"))
}

fn wave_number_from_title(title: &str) -> Option<u32> {
    let lower = title.to_ascii_lowercase();
    let index = lower.find("wave")?;
    let after = lower.get(index + "wave".len()..)?.trim_start();
    let digits: String = after.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    (!digits.is_empty())
        .then(|| digits.parse::<u32>().ok())
        .flatten()
}

fn git_status_for_cwd(cwd: &str) -> GitStatus {
    let output = Command::new("git")
        .args([
            "-C",
            cwd,
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
        ])
        .output();

    let Ok(output) = output else {
        return GitStatus {
            cwd: cwd.into(),
            is_repository: false,
            branch: None,
            entries: Vec::new(),
            error: Some("git executable unavailable".into()),
        };
    };

    if !output.status.success() {
        return GitStatus {
            cwd: cwd.into(),
            is_repository: false,
            branch: None,
            entries: Vec::new(),
            error: Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        };
    }

    let branch = Command::new("git")
        .args(["-C", cwd, "branch", "--show-current"])
        .output()
        .ok()
        .and_then(|output| {
            output.status.success().then(|| {
                let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
                (!branch.is_empty()).then_some(branch)
            })?
        });

    let stdout = String::from_utf8_lossy(&output.stdout);
    let entries = stdout.lines().filter_map(parse_git_status_line).collect();

    GitStatus {
        cwd: cwd.into(),
        is_repository: true,
        branch,
        entries,
        error: None,
    }
}

fn parse_git_status_line(line: &str) -> Option<GitStatusEntry> {
    if line.len() < 4 {
        return None;
    }
    let code = line.get(0..2)?.to_string();
    let raw_path = line.get(3..)?.trim();
    if raw_path.is_empty() {
        return None;
    }

    let (old_path, path) = raw_path
        .split_once(" -> ")
        .map_or((None, raw_path), |(old, new)| (Some(old.to_string()), new));
    let mut chars = code.chars();
    let staged_char = chars.next().unwrap_or(' ');
    let unstaged_char = chars.next().unwrap_or(' ');
    let untracked = code == "??";
    Some(GitStatusEntry {
        code,
        path: path.to_string(),
        old_path,
        staged: !untracked && staged_char != ' ',
        unstaged: !untracked && unstaged_char != ' ',
        untracked,
    })
}

fn stream_semantic_frames(out: &mut impl Write, cols: u16, rows: u16) -> io::Result<()> {
    let mut client = UnixStream::connect(client_socket_path())?;
    protocol::write_message(
        &mut client,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols,
            rows,
            cell_width_px: 0,
            cell_height_px: 0,
            requested_encoding: RenderEncoding::SemanticFrame,
        },
    )
    .map_err(protocol_io_error)?;

    let welcome: ServerMessage =
        protocol::read_message(&mut client, MAX_FRAME_SIZE).map_err(protocol_io_error)?;
    match welcome {
        ServerMessage::Welcome {
            error: Some(error), ..
        } => {
            return Err(io::Error::other(format!(
                "server rejected preview client: {error}"
            )));
        }
        ServerMessage::Welcome {
            encoding: RenderEncoding::SemanticFrame,
            ..
        } => {}
        ServerMessage::Welcome { encoding, .. } => {
            return Err(io::Error::other(format!(
                "server selected unsupported render encoding: {encoding:?}"
            )));
        }
        _ => return Err(io::Error::other("expected Welcome message")),
    }

    loop {
        let message: ServerMessage = protocol::read_message(&mut client, MAX_GRAPHICS_FRAME_SIZE)
            .map_err(protocol_io_error)?;
        match message {
            ServerMessage::Frame(frame) => {
                write_sse_event(out, "frame", &CanvasFrame::from(&frame))?
            }
            ServerMessage::ServerShutdown { reason } => {
                write_sse_event(out, "shutdown", &serde_json::json!({ "reason": reason }))?;
                return Ok(());
            }
            ServerMessage::Notify { kind, message } => {
                write_sse_event(
                    out,
                    "notify",
                    &serde_json::json!({ "kind": format!("{kind:?}"), "message": message }),
                )?;
            }
            ServerMessage::ReloadSoundConfig
            | ServerMessage::Clipboard { .. }
            | ServerMessage::Graphics { .. }
            | ServerMessage::MouseCapture { .. }
            | ServerMessage::Terminal(_) => {}
            ServerMessage::Welcome { .. } => {}
        }
    }
}

fn stream_terminal_frames(
    out: &mut impl Write,
    terminal_id: &str,
    cols: u16,
    rows: u16,
    takeover: bool,
) -> io::Result<()> {
    let mut client = UnixStream::connect(client_socket_path())?;
    protocol::write_message(
        &mut client,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols,
            rows,
            cell_width_px: 0,
            cell_height_px: 0,
            requested_encoding: RenderEncoding::SemanticFrame,
        },
    )
    .map_err(protocol_io_error)?;

    let welcome: ServerMessage =
        protocol::read_message(&mut client, MAX_FRAME_SIZE).map_err(protocol_io_error)?;
    match welcome {
        ServerMessage::Welcome {
            error: Some(error), ..
        } => {
            return Err(io::Error::other(format!(
                "server rejected terminal client: {error}"
            )));
        }
        ServerMessage::Welcome {
            encoding: RenderEncoding::SemanticFrame,
            ..
        } => {}
        ServerMessage::Welcome { encoding, .. } => {
            return Err(io::Error::other(format!(
                "server selected unsupported render encoding: {encoding:?}"
            )));
        }
        _ => return Err(io::Error::other("expected Welcome message")),
    }

    protocol::write_message(
        &mut client,
        &ClientMessage::AttachTerminal {
            terminal_id: terminal_id.to_owned(),
            takeover,
        },
    )
    .map_err(protocol_io_error)?;

    loop {
        let message: ServerMessage = protocol::read_message(&mut client, MAX_GRAPHICS_FRAME_SIZE)
            .map_err(protocol_io_error)?;
        match message {
            ServerMessage::Frame(frame) => {
                write_sse_event(out, "frame", &CanvasFrame::from(&frame))?
            }
            ServerMessage::ServerShutdown { reason } => {
                write_sse_event(out, "shutdown", &serde_json::json!({ "reason": reason }))?;
                return Ok(());
            }
            ServerMessage::Notify { kind, message } => {
                write_sse_event(
                    out,
                    "notify",
                    &serde_json::json!({ "kind": format!("{kind:?}"), "message": message }),
                )?;
            }
            ServerMessage::ReloadSoundConfig
            | ServerMessage::Clipboard { .. }
            | ServerMessage::Graphics { .. }
            | ServerMessage::MouseCapture { .. }
            | ServerMessage::Terminal(_) => {}
            ServerMessage::Welcome { .. } => {}
        }
    }
}

#[derive(Serialize)]
struct CanvasFrame<'a> {
    cells: &'a [CellData],
    width: u16,
    height: u16,
    cursor: &'a Option<CursorState>,
    hyperlinks: &'a [String],
}

impl<'a> From<&'a protocol::FrameData> for CanvasFrame<'a> {
    fn from(frame: &'a protocol::FrameData) -> Self {
        Self {
            cells: &frame.cells,
            width: frame.width,
            height: frame.height,
            cursor: &frame.cursor,
            hyperlinks: &frame.hyperlinks,
        }
    }
}

fn write_sse_event(out: &mut impl Write, event: &str, value: &impl Serialize) -> io::Result<()> {
    let json = serde_json::to_string(value).map_err(io::Error::other)?;
    out.write_all(b"event: ")?;
    out.write_all(event.as_bytes())?;
    out.write_all(b"\n")?;
    out.write_all(b"data: ")?;
    out.write_all(json.as_bytes())?;
    out.write_all(b"\n\n")?;
    out.flush()
}

fn write_html_response(stream: &mut TcpStream, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Cache-Control: no-cache\r\n\
         Content-Length: {}\r\n\
         \r\n{}",
        body.len(),
        body
    )
}

fn write_text_response(
    stream: &mut TcpStream,
    status: u16,
    label: &str,
    body: &str,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {label}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Cache-Control: no-cache\r\n\
         Content-Length: {}\r\n\
         \r\n{}",
        body.len(),
        body
    )
}

fn write_json_response(stream: &mut TcpStream, value: &serde_json::Value) -> io::Result<()> {
    write_json_response_with_status(stream, 200, "OK", value)
}

fn write_json_response_with_status(
    stream: &mut TcpStream,
    status: u16,
    label: &str,
    value: &serde_json::Value,
) -> io::Result<()> {
    let body = serde_json::to_string(value).map_err(io::Error::other)?;
    write!(
        stream,
        "HTTP/1.1 {status} {label}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Cache-Control: no-cache\r\n\
         Content-Length: {}\r\n\
         \r\n{}",
        body.len(),
        body
    )
}

fn query_u16(query: Option<&str>, key: &str) -> Option<u16> {
    query_value(query, key).and_then(|value| value.parse::<u16>().ok())
}

fn query_bool(query: Option<&str>, key: &str) -> bool {
    query_value(query, key).is_some_and(|value| matches!(value, "1" | "true" | "yes"))
}

fn query_value<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?.split('&').find_map(|part| {
        let (candidate_key, value) = part.split_once('=')?;
        (candidate_key == key).then_some(value)
    })
}

fn parse_dispatch_pane_ids(value: &str) -> Result<Vec<String>, &'static str> {
    let pane_ids =
        serde_json::from_str::<Vec<String>>(value).map_err(|_| "invalid pane_ids JSON")?;
    let mut deduped = Vec::new();
    for pane_id in pane_ids {
        let pane_id = pane_id.trim().to_string();
        if pane_id.is_empty() || deduped.iter().any(|seen: &String| seen == &pane_id) {
            continue;
        }
        deduped.push(pane_id);
    }
    if deduped.is_empty() {
        return Err("pane_ids is empty");
    }
    Ok(deduped)
}

fn percent_decode(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hi = *bytes.get(index + 1)?;
                let lo = *bytes.get(index + 2)?;
                decoded.push(hex_value(hi)? << 4 | hex_value(lo)?);
                index += 3;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    Some(decoded)
}

fn percent_decode_string(value: &str) -> Option<String> {
    String::from_utf8(percent_decode(value)?).ok()
}

fn parse_wave_mode(value: &str) -> Option<WaveMode> {
    match value {
        "read_only" | "read-only" | "readonly" => Some(WaveMode::ReadOnly),
        "draft_only" | "draft-only" | "draft" => Some(WaveMode::DraftOnly),
        "write" => Some(WaveMode::Write),
        "reviewer" | "review" => Some(WaveMode::Reviewer),
        "verifier" | "verify" => Some(WaveMode::Verifier),
        "monitor" => Some(WaveMode::Monitor),
        _ => None,
    }
}

fn parse_wave_status(value: &str) -> Option<WaveStatus> {
    match value {
        "queued" | "queue" => Some(WaveStatus::Queued),
        "running" | "run" => Some(WaveStatus::Running),
        "blocked" | "block" => Some(WaveStatus::Blocked),
        "needs_review" | "needs-review" | "review" => Some(WaveStatus::NeedsReview),
        "accepted" => Some(WaveStatus::Accepted),
        "done" => Some(WaveStatus::Done),
        _ => None,
    }
}

fn parse_prompt_delivery(value: &str) -> Option<WavePromptDelivery> {
    match value {
        "agent" => Some(WavePromptDelivery::Agent),
        "shell_card" | "shell-card" | "card" => Some(WavePromptDelivery::ShellCard),
        _ => None,
    }
}

fn pane_input_payload(data: &str, delivery: Option<WavePromptDelivery>) -> String {
    match delivery {
        Some(WavePromptDelivery::ShellCard) => shell_contract_card_payload(data),
        _ => data.to_string(),
    }
}

fn merge_report_gates(existing: &WaveReportGate, detected: &WaveReportGate) -> WaveReportGate {
    let mut completed = Vec::new();
    if existing.completed_items.is_empty() {
        let count = existing.completed_fields.min(existing.required_fields) as usize;
        completed.extend(
            default_report_packet_items()
                .iter()
                .take(count)
                .map(|item| (*item).to_string()),
        );
    } else {
        completed.extend(existing.completed_items.iter().cloned());
    }
    completed.extend(detected.completed_items.iter().cloned());

    let mut ordered = Vec::new();
    for item in default_report_packet_items() {
        if completed
            .iter()
            .any(|completed_item| completed_item.eq_ignore_ascii_case(item))
        {
            ordered.push((*item).to_string());
        }
    }
    for item in completed {
        let item = item.trim();
        if item.is_empty()
            || ordered
                .iter()
                .any(|ordered_item| ordered_item.eq_ignore_ascii_case(item))
        {
            continue;
        }
        ordered.push(item.to_string());
    }

    WaveReportGate {
        completed_fields: 0,
        required_fields: if existing.required_fields == 0 {
            default_report_packet_items().len().min(u8::MAX as usize) as u8
        } else {
            existing.required_fields
        },
        completed_items: ordered,
    }
    .normalized()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn protocol_io_error(err: protocol::FramingError) -> io::Error {
    io::Error::other(err.to_string())
}

fn print_desktop_help() {
    eprintln!("herdr desktop commands:");
    eprintln!("  herdr desktop [--port N]          open the native workroom app on macOS");
    eprintln!("  herdr desktop --app [--port N]    force the native workroom app");
    eprintln!("  herdr desktop --web [--port N]    run the browser preview server");
    eprintln!("  herdr desktop [--bind 127.0.0.1:0]");
    eprintln!("  (starts or attaches to the Herdr server automatically)");
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Herdr Workroom Preview</title>
  <style>
    :root {
      color-scheme: dark;
      --bg: #090b0f;
      --panel: #121821;
      --panel-2: #171f2b;
      --panel-3: #202938;
      --line: #2c3648;
      --line-strong: #3f5f8d;
      --text: #edf2f8;
      --muted: #a8b3c2;
      --faint: #758195;
      --blue: #6fb3ff;
      --green: #7ee2a8;
      --yellow: #f3c969;
      --red: #ff7a90;
      --violet: #b9a2ff;
      --terminal: #090b0f;
    }
    * { box-sizing: border-box; }
    html, body {
      margin: 0;
      width: 100%;
      height: 100%;
      overflow: hidden;
      background: var(--bg);
      color: var(--text);
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }
    body.terminal-expanded .topbar,
    body.terminal-expanded .tree,
    body.terminal-expanded .inspector,
    body.terminal-expanded .mission-strip,
    body.terminal-expanded .review-tabs,
    body.terminal-expanded .tabs {
      display: none;
    }
    body.terminal-expanded .wall-hud {
      display: none;
    }
    body.terminal-expanded .layout {
      grid-template-columns: 1fr;
      height: 100vh;
    }
    body.terminal-expanded .surface {
      padding: 0;
    }
    body.terminal-expanded .terminal-panel {
      display: grid;
      position: fixed;
      inset: 0;
      z-index: 10;
      border: 0;
      border-radius: 0;
    }
    body.pane-wall .layout {
      grid-template-columns: minmax(0, 1fr);
    }
    body.pane-wall .mission-strip,
    body.pane-wall .review-tabs,
    body.pane-wall .pane-command-deck,
    body.pane-wall .pane-roster {
      display: none;
    }
    body.pane-wall .surface {
      padding: 8px 8px 68px;
      grid-template-rows: minmax(0, 1fr);
      gap: 0;
    }
    body.pane-wall .tree {
      position: fixed;
      left: 8px;
      top: 54px;
      bottom: 64px;
      z-index: 9;
      width: min(286px, calc(100vw - 48px));
      border: 1px solid var(--line);
      border-radius: 8px;
      box-shadow: 18px 0 42px rgba(0, 0, 0, 0.38);
    }
    body.pane-wall .inspector {
      position: fixed;
      right: 8px;
      top: 54px;
      bottom: 64px;
      z-index: 9;
      width: min(360px, calc(100vw - 48px));
      border: 1px solid var(--line);
      border-radius: 8px;
      box-shadow: -18px 0 42px rgba(0, 0, 0, 0.38);
    }
    body.pane-wall.wall-hud-expanded .surface {
      padding-bottom: 240px;
    }
    body.terminal-expanded.pane-wall .surface {
      padding: 0;
    }
    body[data-active-tab="project"] .mission-strip,
    body[data-active-tab="panes"]:not(.pane-wall) .mission-strip,
    body[data-active-group="review"] .mission-strip {
      display: none;
    }
    body[data-active-tab="project"] .tree,
    body[data-active-tab="project"] .inspector,
    body[data-active-group="review"] .tree,
    body[data-active-group="review"] .inspector {
      display: none;
    }
    body[data-active-group="review"].hide-inspector:not(.pane-wall):not(.hide-tree) .layout,
    body[data-active-group="review"].hide-tree:not(.pane-wall) .layout,
    body[data-active-group="review"]:not(.pane-wall) .layout {
      grid-template-columns: minmax(0, 1fr);
    }
    body[data-active-tab="panes"]:not(.pane-wall) .surface {
      grid-template-rows: minmax(0, 1fr);
    }
    body.pane-wall .tab-page[data-page="panes"].active {
      grid-template-rows: minmax(0, 1fr);
      overflow: hidden;
    }
    body.pane-wall .terminal-panel {
      display: none;
    }
    body.terminal-expanded.pane-wall .terminal-panel {
      display: grid;
    }
    body.pane-wall .wave-grid {
      grid-template-columns: repeat(auto-fit, minmax(360px, 1fr));
      grid-auto-rows: minmax(300px, 1fr);
      align-content: stretch;
      gap: 8px;
    }
    body.pane-wall[data-pane-count="1"] .wave-grid {
      grid-template-columns: minmax(0, 1fr);
    }
    body.pane-wall[data-pane-count="2"] .wave-grid,
    body.pane-wall[data-pane-count="3"] .wave-grid,
    body.pane-wall[data-pane-count="4"] .wave-grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
    body.pane-wall[data-pane-count="5"] .wave-grid,
    body.pane-wall[data-pane-count="6"] .wave-grid {
      grid-template-columns: repeat(3, minmax(0, 1fr));
    }
    body.pane-wall[data-pane-count="7"] .wave-grid,
    body.pane-wall[data-pane-count="8"] .wave-grid {
      grid-template-columns: repeat(3, minmax(0, 1fr));
    }
    body.pane-wall[data-density="roomy"][data-pane-count="5"] .wave-grid,
    body.pane-wall[data-density="roomy"][data-pane-count="6"] .wave-grid,
    body.pane-wall[data-density="roomy"][data-pane-count="7"] .wave-grid,
    body.pane-wall[data-density="roomy"][data-pane-count="8"] .wave-grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
    body.pane-wall .wave-card {
      grid-column: auto;
      min-height: 300px;
      grid-template-rows: auto minmax(0, 1fr);
    }
    body.pane-wall .wave-title {
      padding: 6px 8px;
    }
    body.pane-wall .pane-progress {
      bottom: 28px;
    }
    body.pane-wall .pane-chips {
      bottom: 5px;
      max-width: calc(100% - 96px);
    }
    body.pane-wall .tile-controls {
      display: none;
    }
    body.pane-wall .wave-card:hover .tile-controls,
    body.pane-wall .wave-card:focus-visible .tile-controls,
    body.pane-wall .tile-controls:focus-within {
      display: flex;
    }
    body.pane-wall .pane-footer {
      display: none;
    }
    body.pane-wall .wave-card:hover .pane-footer,
    body.pane-wall .wave-card:focus-visible .pane-footer {
      display: grid;
    }
    body[data-density="roomy"] .wave-grid {
      grid-template-columns: repeat(auto-fit, minmax(380px, 1fr));
      grid-auto-rows: minmax(380px, 1fr);
    }
    body[data-density="roomy"] .wave-card {
      min-height: 380px;
    }
    body[data-density="dense"] .wave-grid {
      grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
      grid-auto-rows: minmax(320px, 1fr);
    }
    body[data-density="dense"] .wave-card {
      min-height: 320px;
    }
    body[data-density="tight"] .wave-grid {
      grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
      grid-auto-rows: minmax(250px, 1fr);
    }
    body[data-density="tight"] .wave-card {
      min-height: 250px;
    }
    body:not(.pane-wall) .tab-page[data-page="panes"].active {
      grid-template-rows: minmax(0, 1fr);
    }
    body:not(.pane-wall) .wave-grid {
      display: none;
    }
    body:not(.pane-wall) .wave-card {
      min-height: 52px;
      grid-template-rows: auto minmax(0, 1fr);
      border-radius: 7px;
      cursor: pointer;
    }
    body:not(.pane-wall) .wave-card:not(.active) {
      background: rgba(14, 20, 30, 0.82);
    }
    body:not(.pane-wall) .mini-terminal {
      display: none;
    }
    body:not(.pane-wall) .tile-controls {
      display: none;
    }
    body:not(.pane-wall) .wave-title {
      grid-template-columns: minmax(0, 1fr);
      min-height: 50px;
      padding: 7px 8px;
      border-bottom: 0;
    }
    body:not(.pane-wall) .terminal-role {
      display: none;
    }
    body:not(.pane-wall) .pane-footer {
      display: none;
    }
    body:not(.pane-wall) .pane-rail-summary {
      display: none;
    }
    body:not(.pane-wall) .terminal-panel {
      display: grid;
    }
    body.pane-wall[data-density="roomy"] .wave-grid {
      grid-auto-rows: minmax(330px, 1fr);
    }
    body.pane-wall[data-density="dense"] .wave-grid {
      grid-auto-rows: minmax(300px, 1fr);
    }
    body.pane-wall[data-density="tight"] .wave-grid {
      grid-auto-rows: minmax(250px, 1fr);
    }
    body.pane-wall[data-density="roomy"] .wave-card {
      min-height: 330px;
    }
    body.pane-wall[data-density="dense"] .wave-card {
      min-height: 300px;
    }
    body.pane-wall[data-density="tight"] .wave-card {
      min-height: 250px;
    }
    .wall-hud {
      display: none;
    }
    body.pane-wall .wall-hud {
      position: fixed;
      left: 8px;
      right: 8px;
      bottom: 8px;
      z-index: 8;
      display: grid;
      grid-template-columns: minmax(220px, 1fr) auto;
      gap: 10px;
      align-items: center;
      min-height: 48px;
      padding: 8px 10px;
      border: 1px solid rgba(111, 179, 255, 0.28);
      border-radius: 8px;
      background: rgba(13, 17, 24, 0.94);
      box-shadow: 0 -12px 28px rgba(0, 0, 0, 0.28);
      backdrop-filter: blur(8px);
    }
    body.pane-wall.wall-hud-expanded .wall-hud {
      grid-template-columns: minmax(230px, 0.9fr) minmax(360px, 1.15fr) minmax(250px, 0.72fr) auto;
      min-height: 136px;
    }
    body.pane-wall:not(.wall-hud-expanded) .wall-hud-pulse,
    body.pane-wall:not(.wall-hud-expanded) .wall-hud-dag,
    body.pane-wall:not(.wall-hud-expanded) .wall-hud-command,
    body.pane-wall:not(.wall-hud-expanded) .wall-hud-meter,
    body.pane-wall:not(.wall-hud-expanded) .wall-hud-presets {
      display: none;
    }
    body.pane-wall:not(.wall-hud-expanded) .wall-hud-main {
      display: flex;
      align-items: center;
      gap: 8px;
    }
    body.pane-wall:not(.wall-hud-expanded) .wall-hud-kicker,
    body.pane-wall:not(.wall-hud-expanded) .wall-hud-meta {
      display: none;
    }
    body.pane-wall:not(.wall-hud-expanded) .wall-hud-context {
      flex-wrap: nowrap;
    }
    body.pane-wall:not(.wall-hud-expanded) .wall-context-pill {
      max-width: 150px;
    }
    body.pane-wall:not(.wall-hud-expanded) #wallHudMessage,
    body.pane-wall:not(.wall-hud-expanded) #wallHudSweep,
    body.pane-wall:not(.wall-hud-expanded) #wallHudNewChild,
    body.pane-wall:not(.wall-hud-expanded) #wallHudPacketsAll,
    body.pane-wall:not(.wall-hud-expanded) #wallHudPacketAction {
      display: none;
    }
    body.terminal-expanded.pane-wall .wall-hud {
      display: none;
    }
    .wall-hud-main,
    .wall-hud-command,
    .wall-hud-meter {
      min-width: 0;
      display: grid;
      gap: 3px;
    }
    .wall-hud-command {
      grid-template-columns: minmax(0, 1fr) 120px auto auto;
      align-items: stretch;
      gap: 6px;
    }
    .wall-hud-input {
      min-width: 0;
      min-height: 48px;
      max-height: 72px;
      resize: vertical;
      border: 1px solid var(--line);
      border-radius: 7px;
      padding: 7px 8px;
      background: #090d14;
      color: var(--text);
      outline: none;
      font: 11px/1.35 ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .wall-hud-input:focus {
      border-color: var(--line-strong);
      box-shadow: inset 0 0 0 1px rgba(111, 179, 255, 0.16);
    }
    .wall-hud-scope {
      min-width: 0;
      border: 1px solid var(--line);
      border-radius: 7px;
      padding: 0 7px;
      background: #101722;
      color: var(--text);
      outline: none;
      font: 800 10px Inter, ui-sans-serif, system-ui, sans-serif;
    }
    .wall-hud-status {
      grid-column: 1 / -1;
      min-width: 0;
      overflow: hidden;
      color: var(--faint);
      text-overflow: ellipsis;
      white-space: nowrap;
      font: 10px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .wall-hud-presets {
      grid-column: 1 / -1;
      display: flex;
      flex-wrap: wrap;
      gap: 5px;
      min-width: 0;
    }
    .wall-preset {
      min-height: 22px;
      border: 1px solid rgba(117, 129, 149, 0.24);
      border-radius: 999px;
      padding: 0 8px;
      background: #0d1118;
      color: var(--muted);
      font: 800 10px Inter, ui-sans-serif, system-ui, sans-serif;
      cursor: pointer;
      white-space: nowrap;
    }
    .wall-preset:hover {
      border-color: var(--line-strong);
      color: var(--text);
    }
    .wall-hud-kicker {
      color: var(--blue);
      font-size: 9px;
      font-weight: 900;
      letter-spacing: 0.1em;
      text-transform: uppercase;
    }
    .wall-hud-title,
    .wall-hud-meta,
    .wall-hud-meter span {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .wall-hud-title {
      color: var(--text);
      font-size: 13px;
      font-weight: 850;
    }
    .wall-hud-context {
      display: flex;
      flex-wrap: wrap;
      gap: 5px;
      min-width: 0;
      overflow: hidden;
    }
    .wall-context-pill {
      min-width: 0;
      max-width: 170px;
      padding: 2px 7px;
      border: 1px solid rgba(117, 129, 149, 0.26);
      border-radius: 999px;
      background: rgba(16, 23, 34, 0.84);
      color: var(--muted);
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
      font: 800 10px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .wall-context-pill.good { border-color: rgba(126, 226, 168, 0.42); color: var(--green); }
    .wall-context-pill.warn { border-color: rgba(243, 201, 105, 0.45); color: var(--yellow); }
    .wall-context-pill.bad { border-color: rgba(255, 122, 144, 0.45); color: var(--red); }
    .wall-hud-pulse {
      min-width: 0;
      display: grid;
      grid-template-columns: repeat(4, minmax(0, 1fr));
      gap: 4px;
      margin-top: 3px;
    }
    .wall-hud-dag {
      min-width: 0;
      display: flex;
      gap: 4px;
      margin-top: 5px;
      overflow: auto hidden;
      padding-bottom: 1px;
    }
    .wall-dag-node {
      min-width: 48px;
      max-width: 96px;
      min-height: 26px;
      display: grid;
      gap: 1px;
      align-content: center;
      border: 1px solid rgba(117, 129, 149, 0.22);
      border-radius: 6px;
      padding: 3px 5px;
      background: #0b1018;
      color: var(--muted);
      text-align: left;
      cursor: pointer;
    }
    .wall-dag-node.active {
      border-color: var(--line-strong);
      background: #142033;
      color: var(--text);
    }
    .wall-dag-node.good {
      border-color: rgba(126, 226, 168, 0.32);
    }
    .wall-dag-node.warn {
      border-color: rgba(243, 201, 105, 0.4);
    }
    .wall-dag-node.bad {
      border-color: rgba(255, 122, 144, 0.4);
    }
    .wall-dag-node strong,
    .wall-dag-node small {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .wall-dag-node strong {
      color: inherit;
      font: 850 10px Inter, ui-sans-serif, system-ui, sans-serif;
    }
    .wall-dag-node small {
      color: var(--faint);
      font: 8px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .wall-pulse-item {
      min-width: 0;
      overflow: hidden;
      border: 1px solid rgba(117, 129, 149, 0.22);
      border-radius: 6px;
      padding: 4px 5px;
      background: #0b1018;
    }
    .wall-pulse-value,
    .wall-pulse-label {
      display: block;
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .wall-pulse-value {
      color: var(--text);
      font: 850 12px Inter, ui-sans-serif, system-ui, sans-serif;
    }
    .wall-pulse-label {
      color: var(--faint);
      font: 8px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      text-transform: uppercase;
    }
    .wall-hud-meta,
    .wall-hud-meter span {
      color: var(--muted);
      font: 10px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .wall-hud-readout {
      min-width: 0;
      max-height: 48px;
      margin: 2px 0 0;
      overflow: auto;
      border: 1px solid rgba(117, 129, 149, 0.22);
      border-radius: 7px;
      padding: 5px 6px;
      background: #090d14;
      color: #b9c7d9;
      white-space: pre-wrap;
      font: 10px/1.35 ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .wall-hud-actions {
      display: flex;
      flex-wrap: wrap;
      justify-content: flex-end;
      gap: 6px;
    }
    .wall-hud-button {
      min-height: 26px;
      padding: 0 9px;
      border: 1px solid var(--line);
      border-radius: 7px;
      background: #101722;
      color: var(--muted);
      font: 800 10px Inter, ui-sans-serif, system-ui, sans-serif;
      cursor: pointer;
      white-space: nowrap;
    }
    .wall-hud-button.primary {
      border-color: rgba(126, 226, 168, 0.55);
      color: var(--text);
      background: #14261d;
    }
    .wall-hud-button:hover {
      border-color: var(--line-strong);
      color: var(--text);
    }
    .app-shell {
      height: 100vh;
      display: grid;
      grid-template-rows: 46px 1fr;
    }
    .topbar {
      display: grid;
      grid-template-columns: 280px 1fr auto;
      align-items: center;
      gap: 16px;
      padding: 0 18px;
      border-bottom: 1px solid var(--line);
      background: #0d1118;
    }
    .brand {
      min-width: 0;
      font-size: 13px;
      font-weight: 700;
      letter-spacing: 0;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    .tabs {
      display: flex;
      align-items: center;
      justify-content: center;
      gap: 6px;
      min-width: 0;
    }
    .tab {
      height: 30px;
      padding: 0 12px;
      border: 1px solid transparent;
      border-radius: 7px;
      background: transparent;
      color: var(--muted);
      font: 600 12px inherit;
      cursor: pointer;
    }
    .tab.active {
      border-color: var(--line-strong);
      background: #15243a;
      color: var(--text);
    }
    .review-tabs {
      display: flex;
      align-items: center;
      gap: 6px;
      min-width: 0;
      padding: 4px 2px 0;
      overflow-x: auto;
    }
	    .review-tabs[hidden] {
	      display: none;
	    }
    .review-tab {
      height: 28px;
      padding: 0 10px;
      border: 1px solid var(--line);
      border-radius: 7px;
      background: rgba(18, 24, 33, 0.86);
      color: var(--muted);
      font: 700 11px inherit;
      cursor: pointer;
      white-space: nowrap;
    }
	    .review-tab.active {
	      border-color: var(--line-strong);
	      background: #172234;
	      color: var(--text);
	    }
	    .review-decision-grid {
	      display: grid;
	      grid-template-columns: repeat(4, minmax(130px, 1fr));
	      gap: 10px;
	      margin-top: 14px;
	    }
	    .review-decision-card {
	      min-width: 0;
	      padding: 12px;
	      border: 1px solid var(--line);
	      border-radius: 8px;
	      background: var(--panel-2);
	    }
	    .review-decision-card.good {
	      border-color: rgba(126, 226, 168, 0.34);
	      background: rgba(18, 38, 28, 0.72);
	    }
	    .review-decision-card.warn {
	      border-color: rgba(243, 201, 105, 0.38);
	      background: rgba(46, 35, 7, 0.28);
	    }
	    .review-decision-card.bad {
	      border-color: rgba(255, 122, 144, 0.38);
	      background: rgba(52, 8, 19, 0.28);
	    }
	    .review-decision-value {
	      color: var(--text);
	      font-size: 22px;
	      font-weight: 850;
	      line-height: 1.1;
	    }
	    .review-decision-label {
	      margin-top: 4px;
	      color: var(--faint);
	      font-size: 11px;
	    }
	    .review-docket-primary {
	      display: grid;
	      grid-template-columns: minmax(0, 1fr) auto;
	      gap: 10px;
	      align-items: center;
	      padding: 10px 0 2px;
	      border-top: 1px solid rgba(255, 255, 255, 0.06);
	    }
	    .review-docket-primary strong {
	      display: block;
	      min-width: 0;
	      overflow: hidden;
	      color: var(--text);
	      text-overflow: ellipsis;
	      white-space: nowrap;
	    }
	    .review-docket-primary .ops-sub {
	      max-width: 840px;
	    }
	    .review-toolbar {
	      display: flex;
	      align-items: center;
	      justify-content: space-between;
	      gap: 10px;
	      min-width: 0;
	      margin-top: 12px;
	      padding: 8px 10px;
	      border: 1px solid rgba(117, 129, 149, 0.22);
	      border-radius: 8px;
	      background: #0d1118;
	      color: var(--muted);
	      font-size: 11px;
	    }
	    .review-toolbar span {
	      min-width: 0;
	      overflow: hidden;
	      text-overflow: ellipsis;
	      white-space: nowrap;
	    }
    .mission-room,
    .review-room,
    .lane-room {
      min-width: 0;
      min-height: 0;
      overflow: auto;
      padding: 16px;
    }
    .mission-room-head,
    .review-room-head,
    .lane-room-head {
      display: grid;
      grid-template-columns: minmax(0, 1fr) auto;
      align-items: end;
      gap: 14px;
      min-width: 0;
      padding-bottom: 12px;
      border-bottom: 1px solid var(--line);
    }
    .mission-room-head h1,
    .review-room-head h1,
    .lane-room-head h1 {
      max-width: 780px;
    }
    .mission-room-head .review-toolbar,
    .review-room-head .review-toolbar,
    .lane-room-head .review-toolbar {
      min-width: min(420px, 42vw);
      margin-top: 0;
    }
    .mission-room-body,
    .review-room-body,
    .lane-room-body {
      min-width: 0;
      display: grid;
      grid-template-columns: minmax(0, 1fr) minmax(260px, 320px);
      gap: 12px;
      margin-top: 12px;
    }
    body.hide-room-context .mission-room-body,
    body.hide-room-context .review-room-body,
    body.hide-room-context .lane-room-body {
      grid-template-columns: minmax(0, 1fr);
    }
    .mission-main,
    .review-main,
    .lane-main {
      min-width: 0;
      display: grid;
      align-content: start;
      gap: 12px;
    }
    .mission-main .mission-brief,
    .review-main .mission-brief {
      margin: 0;
    }
    .mission-state-room,
    .review-decision-room {
      min-width: 0;
    }
    .mission-state-room.ops-grid,
    .review-decision-room .ops-grid {
      grid-template-columns: minmax(0, 1fr);
      margin-top: 0;
    }
    .mission-lifecycle-board {
      min-width: 0;
      display: grid;
      grid-template-columns: repeat(5, minmax(160px, 1fr));
      gap: 8px;
      align-items: stretch;
    }
    .mission-board-lane {
      min-width: 0;
      min-height: 220px;
      display: grid;
      grid-template-rows: auto minmax(0, 1fr);
      border: 1px solid var(--line);
      border-radius: 8px;
      background: rgba(16, 23, 34, 0.82);
      overflow: hidden;
    }
    .mission-board-lane[data-mission-lane-collapsed="true"] {
      min-height: 0;
      grid-template-rows: auto;
    }
    .mission-board-lane-head {
      padding: 0;
      border-bottom: 1px solid rgba(117, 129, 149, 0.2);
      background: rgba(13, 17, 24, 0.78);
    }
    .mission-board-lane-toggle {
      width: 100%;
      min-width: 0;
      display: grid;
      grid-template-columns: minmax(0, 1fr) auto;
      gap: 8px;
      align-items: center;
      border: 0;
      border-radius: 0;
      padding: 9px 10px;
      color: inherit;
      background: transparent;
      text-align: left;
      cursor: pointer;
    }
    .mission-board-lane-toggle:hover {
      background: rgba(38, 52, 73, 0.58);
    }
    .mission-board-lane-head strong,
    .mission-board-lane-head small {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .mission-board-lane-head strong {
      display: block;
      color: var(--text);
      font-size: 12px;
      font-weight: 850;
    }
    .mission-board-lane-head small {
      display: block;
      color: var(--faint);
      font-size: 10px;
      line-height: 1.3;
    }
    .mission-board-count {
      min-width: 24px;
      padding: 2px 7px;
      border: 1px solid rgba(117, 129, 149, 0.24);
      border-radius: 999px;
      color: var(--muted);
      text-align: center;
      font: 850 10px Inter, ui-sans-serif, system-ui, sans-serif;
    }
    .mission-board-stack {
      min-width: 0;
      display: grid;
      align-content: start;
      gap: 7px;
      padding: 8px;
    }
    .mission-board-lane[data-mission-lane-collapsed="true"] .mission-board-stack {
      display: none;
    }
    .mission-board-card {
      min-width: 0;
      display: grid;
      gap: 6px;
      border: 1px solid rgba(117, 129, 149, 0.22);
      border-radius: 7px;
      padding: 8px;
      background: rgba(9, 13, 20, 0.7);
      color: inherit;
      text-align: left;
      cursor: pointer;
    }
    .mission-board-card:hover,
    .mission-board-card.active {
      border-color: var(--line-strong);
      background: #142033;
    }
    .mission-board-card strong,
    .mission-board-card span {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .mission-board-card strong {
      color: var(--text);
      font-size: 12px;
    }
    .mission-board-card span {
      color: var(--muted);
      font-size: 10px;
    }
    .mission-board-actions {
      display: flex;
      flex-wrap: wrap;
      gap: 5px;
      min-width: 0;
    }
    .mission-sidecar,
    .review-sidecar,
    .lane-sidecar {
      min-width: 0;
      align-self: start;
      position: sticky;
      top: 0;
      display: grid;
      gap: 10px;
      padding: 12px;
      border: 1px solid var(--line);
      border-radius: 8px;
      background: rgba(16, 23, 34, 0.96);
    }
    body.hide-room-context .mission-sidecar,
    body.hide-room-context .review-sidecar,
    body.hide-room-context .lane-sidecar {
      display: none;
    }
    .mission-sidecar h2,
    .review-sidecar h2,
    .lane-sidecar h2 {
      margin: 4px 0 0;
      font-size: 15px;
    }
    .mission-sidecar p,
    .review-sidecar p,
    .lane-sidecar p {
      color: var(--muted);
      font-size: 12px;
      line-height: 1.45;
    }
    .lane-main .doc-grid,
    .lane-main .evidence-grid,
    .lane-main .audit-grid,
    .lane-main .ops-grid {
      margin-top: 0;
    }
    .room-lane-actions {
      display: grid;
      gap: 6px;
      min-width: 0;
    }
    .room-lane-button {
      min-height: 30px;
      border: 1px solid var(--line);
      border-radius: 7px;
      background: #0d1118;
      color: var(--text);
      font: 800 11px Inter, ui-sans-serif, system-ui, sans-serif;
      cursor: pointer;
      text-align: left;
      padding: 0 10px;
    }
    .room-lane-button:hover {
      border-color: var(--line-strong);
      background: #142033;
    }
	    .mission-brief {
	      display: grid;
	      grid-template-columns: minmax(280px, 1.15fr) repeat(3, minmax(180px, 0.72fr));
	      gap: 10px;
	      margin: 12px 0;
	      min-width: 0;
	    }
	    .mission-brief-card {
	      min-width: 0;
	      display: grid;
	      align-content: start;
	      gap: 6px;
	      min-height: 118px;
	      padding: 12px;
	      border: 1px solid var(--line);
	      border-radius: 8px;
	      background: #101722;
	    }
	    .mission-brief-card.primary {
	      background: linear-gradient(135deg, rgba(20, 38, 61, 0.82), rgba(16, 23, 34, 0.96));
	      border-color: rgba(111, 179, 255, 0.42);
	    }
	    .mission-brief-kicker {
	      color: var(--blue);
	      font-size: 10px;
	      font-weight: 900;
	      letter-spacing: 0.11em;
	      text-transform: uppercase;
	    }
	    .mission-brief-card strong,
	    .mission-brief-card p {
	      min-width: 0;
	      overflow: hidden;
	      text-overflow: ellipsis;
	    }
	    .mission-brief-card strong {
	      color: var(--text);
	      font-size: 15px;
	      line-height: 1.2;
	    }
	    .mission-brief-card p {
	      color: var(--muted);
	      font-size: 12px;
	      line-height: 1.45;
	    }
	    .mission-brief-actions,
	    .mission-brief-chips {
	      display: flex;
	      flex-wrap: wrap;
	      gap: 5px;
	      min-width: 0;
	    }
    .runtime {
      justify-self: end;
      position: relative;
      display: flex;
      align-items: center;
      gap: 8px;
      min-width: 0;
      color: var(--muted);
      font: 12px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
	    .view-menu {
	      position: relative;
	      display: inline-flex;
	      align-items: center;
	    }
    body:not([data-active-tab="panes"]) .view-menu {
      display: none;
    }
	    .view-menu-panel {
	      display: none;
	      position: absolute;
      top: calc(100% + 8px);
      right: 0;
      z-index: 18;
      width: min(320px, 78vw);
      gap: 9px;
      padding: 10px;
      border: 1px solid var(--line);
      border-radius: 8px;
      background: rgba(13, 17, 24, 0.98);
	      box-shadow: 0 18px 42px rgba(0, 0, 0, 0.35);
	    }
	    .drawer-switches {
	      display: inline-flex;
	      align-items: center;
	      min-width: 0;
	      overflow: hidden;
	      border: 1px solid var(--line);
	      border-radius: 7px;
	      background: #0d1118;
	    }
	    body:not([data-active-tab="panes"]) .drawer-switches {
	      display: none;
	    }
	    .drawer-switch {
	      height: 28px;
	      min-width: 64px;
	      padding: 0 10px;
	      border: 0;
	      border-right: 1px solid var(--line);
	      background: transparent;
	      color: var(--muted);
	      font: 850 11px Inter, ui-sans-serif, system-ui, sans-serif;
	      cursor: pointer;
	      white-space: nowrap;
	    }
	    .drawer-switch:last-child {
	      border-right: 0;
	    }
	    .drawer-switch:hover,
	    .drawer-switch[aria-pressed="true"] {
	      background: #142033;
	      color: var(--text);
	    }
	    body.pane-wall #commandQuickToggle {
	      display: none;
	    }
	    body.show-view-menu .view-menu-panel {
	      display: grid;
	    }
    .view-menu-title,
    .view-menu-label {
      color: var(--faint);
      font: 900 10px Inter, ui-sans-serif, system-ui, sans-serif;
      letter-spacing: 0.1em;
      text-transform: uppercase;
    }
    .view-menu-section {
      min-width: 0;
      display: grid;
      gap: 6px;
    }
    .view-menu-actions {
      display: grid;
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 6px;
    }
    .wall-toggle {
      height: 28px;
      padding: 0 10px;
      border: 1px solid var(--line);
      border-radius: 7px;
      background: #142033;
      color: var(--text);
      font: 800 11px Inter, ui-sans-serif, system-ui, sans-serif;
      cursor: pointer;
      white-space: nowrap;
      box-shadow: inset 0 0 0 1px rgba(111, 179, 255, 0.14);
    }
    .density-control {
      display: inline-flex;
      align-items: center;
      min-width: 0;
      overflow: hidden;
      border: 1px solid var(--line);
      border-radius: 7px;
      background: #0d1118;
    }
    .density-button {
      height: 26px;
      padding: 0 8px;
      border: 0;
      border-right: 1px solid var(--line);
      background: transparent;
      color: var(--muted);
      font: 800 10px Inter, ui-sans-serif, system-ui, sans-serif;
      cursor: pointer;
      white-space: nowrap;
    }
    .density-button:last-child {
      border-right: 0;
    }
    .density-button:hover,
    .density-button[aria-pressed="true"] {
      background: #142033;
      color: var(--text);
    }
    .wall-toggle:hover,
    .wall-toggle[aria-pressed="true"] {
      border-color: var(--line-strong);
      background: #142033;
      color: var(--text);
    }
    .wall-toggle.subtle {
      background: #101722;
      color: var(--muted);
      box-shadow: none;
    }
    .live-mode-switch {
      display: inline-flex;
      align-items: center;
      min-width: 0;
      overflow: hidden;
      border: 1px solid var(--line);
      border-radius: 7px;
      background: #0d1118;
      box-shadow: inset 0 0 0 1px rgba(111, 179, 255, 0.08);
    }
    body:not([data-active-tab="panes"]) .live-mode-switch {
      display: none;
    }
    .live-mode-button {
      height: 28px;
      min-width: 64px;
      padding: 0 10px;
      border: 0;
      border-right: 1px solid var(--line);
      background: transparent;
      color: var(--muted);
      font: 850 11px Inter, ui-sans-serif, system-ui, sans-serif;
      cursor: pointer;
      white-space: nowrap;
    }
    .live-mode-button:last-child {
      border-right: 0;
    }
    .live-mode-button:hover,
    .live-mode-button[aria-pressed="true"] {
      background: #142033;
      color: var(--text);
    }
    body.pane-wall #controlsToggle,
    body.pane-wall #rosterToggle {
      display: none;
    }
    body:not(.pane-wall) [data-view-section="pane-density"],
    body:not(.pane-wall) [data-view-section="pane-wall"] {
      display: none;
    }
    .dot {
      width: 8px;
      height: 8px;
      border-radius: 999px;
      background: var(--yellow);
      box-shadow: 0 0 14px rgba(243, 201, 105, 0.5);
      flex: 0 0 auto;
    }
    .dot.live {
      background: var(--green);
      box-shadow: 0 0 14px rgba(126, 226, 168, 0.5);
    }
    .layout {
      min-height: 0;
      position: relative;
      display: grid;
      grid-template-columns: 286px minmax(560px, 1fr);
    }
    body.hide-tree .tree,
    body.hide-inspector .inspector {
      display: none;
    }
    body.hide-inspector:not(.pane-wall):not(.hide-tree) .layout {
      grid-template-columns: 286px minmax(560px, 1fr);
    }
	    body.hide-tree:not(.pane-wall) .layout {
	      grid-template-columns: minmax(0, 1fr);
	    }
    body[data-active-tab="project"].hide-inspector:not(.pane-wall):not(.hide-tree) .layout,
    body[data-active-tab="project"].hide-tree:not(.pane-wall) .layout,
    body[data-active-tab="project"]:not(.pane-wall) .layout,
    body[data-active-group="review"].hide-inspector:not(.pane-wall):not(.hide-tree) .layout,
    body[data-active-group="review"].hide-tree:not(.pane-wall) .layout,
    body[data-active-group="review"]:not(.pane-wall) .layout {
      grid-template-columns: minmax(0, 1fr);
    }
		    .edge-reopen {
		      display: none;
		      position: fixed;
		      left: 0;
		      top: 50%;
		      z-index: 17;
		      width: 22px;
		      min-height: 58px;
		      height: auto;
		      padding: 8px 0;
		      transform: translateY(-50%);
		      writing-mode: vertical-rl;
		      text-orientation: mixed;
		      letter-spacing: 0.02em;
		      border: 1px solid var(--line-strong);
		      border-radius: 0 8px 8px 0;
		      background: #142033;
		      color: var(--text);
		      font: 800 11px Inter, ui-sans-serif, system-ui, sans-serif;
		      cursor: pointer;
		      opacity: 0.66;
		      box-shadow: 0 12px 34px rgba(0, 0, 0, 0.38);
		    }
		    .edge-reopen:hover,
		    .edge-reopen:focus-visible {
		      opacity: 1;
		    }
		    .edge-reopen.details-edge {
		      left: auto;
		      right: 0;
		      border-radius: 8px 0 0 8px;
		    }
    body.pane-wall .edge-reopen {
      top: 50%;
      width: 22px;
      min-height: 58px;
      height: auto;
      padding: 8px 0;
      transform: translateY(-50%);
      writing-mode: vertical-rl;
      text-orientation: mixed;
      letter-spacing: 0.02em;
      opacity: 0.58;
      box-shadow: 0 10px 28px rgba(0, 0, 0, 0.28);
    }
    body.pane-wall .edge-reopen:hover,
    body.pane-wall .edge-reopen:focus-visible {
      opacity: 1;
    }
    body.pane-wall .edge-reopen.tree-edge {
      left: 0;
      right: auto;
      border-radius: 0 8px 8px 0;
    }
    body.pane-wall .edge-reopen.details-edge {
      right: 0;
      left: auto;
      border-radius: 8px 0 0 8px;
    }
    .drawer-scrim {
      display: none;
      position: fixed;
      inset: 54px 0 64px;
      z-index: 8;
      border: 0;
      padding: 0;
      background: rgba(0, 0, 0, 0.14);
      cursor: default;
    }
    body.pane-wall:not(.hide-tree) .drawer-scrim,
    body.pane-wall:not(.hide-inspector) .drawer-scrim {
      display: block;
    }
    body.terminal-expanded .drawer-scrim {
      display: none;
    }
		    body[data-active-tab="panes"].hide-tree:not(.terminal-expanded) .edge-reopen.tree-edge,
		    body[data-active-tab="panes"].hide-inspector:not(.terminal-expanded) .edge-reopen.details-edge {
		      display: inline-flex;
		      align-items: center;
		      justify-content: center;
		    }
	    body:not(.hide-inspector):not(.pane-wall) .inspector {
	      position: absolute;
      top: 0;
      right: 0;
      bottom: 0;
      z-index: 16;
      width: min(340px, calc(100vw - 40px));
      border-left: 1px solid var(--line);
      box-shadow: -18px 0 42px rgba(0, 0, 0, 0.38);
    }
    .tree,
    .inspector {
      min-height: 0;
      overflow: auto;
      background: #0f141d;
    }
    .tree {
      border-right: 1px solid var(--line);
      padding: 14px 12px;
    }
    .tree-caption {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      margin: 4px 4px 10px;
      color: var(--faint);
      font-size: 10px;
      font-weight: 800;
      letter-spacing: 0.12em;
      text-transform: uppercase;
    }
    .tree-caption strong {
      min-width: 0;
      overflow: hidden;
      color: var(--muted);
      text-overflow: ellipsis;
      white-space: nowrap;
      font: inherit;
      letter-spacing: inherit;
    }
    .tree-caption-actions {
      display: flex;
      min-width: 0;
      align-items: center;
      gap: 6px;
    }
    .tree-close {
      width: 24px;
      height: 24px;
      padding: 0;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: #101722;
      color: var(--muted);
      font: 900 12px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      cursor: pointer;
    }
    .tree-close:hover {
      border-color: var(--line-strong);
      background: #142033;
      color: var(--text);
    }
    .inspector {
      border-left: 1px solid var(--line);
      display: grid;
      grid-template-rows: auto auto minmax(0, 1fr);
      overflow: hidden;
      padding: 16px 14px;
    }
    .file-tree {
      display: grid;
      gap: 2px;
    }
    .project-anchor,
    .mission-node,
    .wave-node,
    .arc-node {
      display: grid;
      grid-template-columns: auto 1fr auto;
      align-items: center;
      gap: 8px;
      width: 100%;
      min-height: 32px;
      padding: 5px 7px;
      border-radius: 6px;
      color: var(--muted);
      font-size: 12px;
    }
    .project-anchor span:nth-child(2),
    .mission-node span:nth-child(2),
    .wave-node span:nth-child(2),
    .arc-node span:nth-child(2) {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .mission-node,
    .wave-node {
      appearance: none;
      -webkit-appearance: none;
      font-family: inherit;
      line-height: 1.2;
      text-align: left;
      cursor: pointer;
    }
    .mission-node {
      border: 1px solid var(--line-strong);
      background: #15243a;
      color: var(--text);
      font-weight: 700;
    }
    .mission-node.active {
      box-shadow: inset 3px 0 0 var(--green);
      background: #18304b;
    }
    .tree-root-icon {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      width: 18px;
      height: 22px;
      border: 1px solid rgba(126, 226, 168, 0.34);
      border-radius: 5px;
      color: var(--green);
      font: 900 9px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .tree-parent-actions {
      display: inline-flex;
      align-items: center;
      justify-content: flex-end;
      gap: 5px;
      min-width: 0;
    }
    .tree-icon-button {
      min-width: 24px;
      min-height: 22px;
      border: 1px solid rgba(126, 226, 168, 0.32);
      border-radius: 6px;
      padding: 0 7px;
      background: #102019;
      color: var(--green);
      font: 900 12px Inter, ui-sans-serif, system-ui, sans-serif;
      cursor: pointer;
      line-height: 1;
    }
    .tree-icon-button:hover {
      border-color: var(--line-strong);
      color: var(--text);
    }
    .project-anchor {
      min-height: 28px;
      margin-bottom: 6px;
      padding-inline: 3px 5px;
      border: 0;
      border-radius: 0;
      background: transparent;
      color: var(--muted);
    }
    .project-anchor .tree-main strong {
      color: var(--text);
    }
    .project-anchor .badge {
      background: #182333;
      border-color: rgba(111, 179, 255, 0.34);
      color: var(--blue);
    }
    .tree-children {
      display: grid;
      gap: 1px;
      margin-left: 13px;
      padding-left: 9px;
      border-left: 1px solid rgba(117, 129, 149, 0.28);
    }
    .tree-children.root-tree {
      margin-left: 0;
      padding-left: 0;
      border-left: 0;
    }
    .tree-children[hidden] {
      display: none;
    }
    .wave-node {
      border: 1px solid transparent;
      background: transparent;
      font-size: 12px;
    }
    .wave-node.active {
      border-color: var(--line-strong);
      background: #172235;
      color: var(--text);
    }
    .tree-main {
      min-width: 0;
      display: grid;
      gap: 2px;
    }
    .tree-main strong,
    .tree-main small {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .tree-main strong {
      font-weight: 850;
    }
    .tree-main small {
      color: var(--faint);
      font: 9px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .arc-node {
      min-height: 24px;
      padding-block: 4px;
      color: var(--faint);
    }
    .chev {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      width: 18px;
      height: 22px;
      border: 1px solid rgba(117, 129, 149, 0.2);
      border-radius: 5px;
      color: var(--faint);
      font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .mission-node:hover .chev,
    .wave-node:hover .chev {
      border-color: var(--line-strong);
      color: var(--text);
      background: #101722;
    }
    .badge {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      min-width: 18px;
      height: 18px;
      padding: 0 6px;
      border-radius: 999px;
      border: 1px solid var(--line);
      background: var(--panel-2);
      color: var(--muted);
      font-size: 10px;
      font-weight: 800;
      white-space: nowrap;
    }
    .badge.green { color: #11251a; background: var(--green); border-color: var(--green); }
    .badge.yellow { color: #2e2307; background: var(--yellow); border-color: var(--yellow); }
    .badge.blue { color: #061b31; background: var(--blue); border-color: var(--blue); }
    .badge.red { color: #340813; background: var(--red); border-color: var(--red); }
	    .surface {
	      min-width: 0;
	      min-height: 0;
	      position: relative;
	      overflow: hidden;
	      display: grid;
	      grid-template-rows: auto 1fr;
	      gap: 12px;
	      padding: 14px;
	      background: var(--bg);
	    }
    .mission-strip {
      display: grid;
      grid-template-columns: minmax(320px, 1fr) auto;
      gap: 12px;
      align-items: stretch;
    }
    .mission-card,
    .metric-card,
    .panel,
    .wave-card,
    .inspector-card {
      border: 1px solid var(--line);
      border-radius: 8px;
      background: rgba(18, 24, 33, 0.94);
    }
    .mission-card {
      padding: 14px 16px;
    }
    .mission-kicker {
      color: var(--blue);
      font-size: 11px;
      font-weight: 800;
      letter-spacing: 0.12em;
      text-transform: uppercase;
    }
    h1, h2, h3, p {
      margin: 0;
    }
    h1 {
      margin-top: 5px;
      font-size: 19px;
      line-height: 1.2;
    }
    .mission-copy {
      margin-top: 7px;
      color: var(--muted);
      font-size: 13px;
      line-height: 1.45;
      max-width: 850px;
    }
    .metrics {
      display: grid;
      grid-template-columns: repeat(3, 104px);
      gap: 8px;
    }
    .metric-card {
      padding: 10px;
    }
    .metric-value {
      font-size: 18px;
      font-weight: 800;
    }
    .metric-label {
      margin-top: 3px;
      color: var(--faint);
      font-size: 11px;
    }
    .tab-page {
      min-height: 0;
      display: none;
    }
	    .tab-page.active {
	      display: grid;
	      grid-template-rows: minmax(0, 1fr);
	      gap: 12px;
	    }
	    .tab-page[data-page="panes"].active {
	      position: relative;
	      grid-template-rows: minmax(0, 1fr);
	      overflow: hidden;
	    }
	    .live-command-strip {
	      display: none;
	      min-width: 0;
	      align-items: center;
	      justify-content: space-between;
	      gap: 12px;
	      padding: 8px 10px;
	      border: 1px solid var(--line);
	      border-radius: 8px;
	      background: rgba(16, 23, 34, 0.96);
	    }
	    body:not(.pane-wall) .live-command-strip {
	      display: none;
	    }
	    body.pane-wall .live-command-strip {
	      display: none;
	    }
	    .live-command-copy {
	      min-width: 0;
	      display: flex;
	      align-items: baseline;
	      gap: 8px;
	      overflow: hidden;
	    }
	    .live-command-kicker {
	      flex: 0 0 auto;
	      color: var(--green);
	      font: 900 10px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
	      letter-spacing: 0.08em;
	      text-transform: uppercase;
	    }
	    .live-command-copy strong,
	    .live-command-copy span:last-child {
	      min-width: 0;
	      overflow: hidden;
	      text-overflow: ellipsis;
	      white-space: nowrap;
	    }
	    .live-command-copy strong {
	      color: var(--text);
	      font-size: 12px;
	    }
	    .live-command-copy span:last-child {
	      color: var(--muted);
	      font-size: 11px;
	    }
	    .live-command-actions {
	      display: flex;
	      align-items: center;
	      justify-content: flex-end;
	      gap: 6px;
	      flex-wrap: wrap;
	      min-width: 0;
	    }
	    .pane-command-deck {
	      display: none;
	      position: absolute;
	      left: 8px;
	      right: 8px;
	      bottom: 8px;
	      z-index: 15;
	      min-width: 0;
	      gap: 8px;
	      max-height: min(360px, 42vh);
	      overflow: auto;
	      padding: 9px 10px;
	      border: 1px solid var(--line);
	      border-radius: 8px;
	      background: rgba(18, 24, 33, 0.94);
	      box-shadow: 0 -18px 42px rgba(0, 0, 0, 0.38);
	    }
    body.show-command-deck .tab-page[data-page="panes"].active .pane-command-deck {
      display: grid;
    }
	    body.show-command-deck.show-command-advanced .tab-page[data-page="panes"].active .pane-command-deck {
	      max-height: min(620px, 72vh);
	    }
    body.pane-wall .tab-page[data-page="panes"].active .pane-command-deck {
      display: none;
    }
    .deck-top {
      display: grid;
      grid-template-columns: minmax(220px, 1fr) auto;
      align-items: center;
      gap: 10px;
      min-width: 0;
    }
    .deck-title {
      display: flex;
      align-items: baseline;
      gap: 8px;
      min-width: 0;
    }
    .deck-title strong {
      color: var(--text);
      font-size: 12px;
      white-space: nowrap;
    }
    .deck-title span {
      min-width: 0;
      overflow: hidden;
      color: var(--muted);
      text-overflow: ellipsis;
      white-space: nowrap;
      font-size: 11px;
    }
    .deck-actions {
      display: flex;
      flex-wrap: wrap;
      justify-content: flex-end;
      gap: 6px;
    }
    .deck-advanced-action {
      display: none;
    }
    body.show-command-advanced .deck-advanced-action {
      display: inline-block;
    }
    .deck-advanced {
      display: none;
      min-width: 0;
      gap: 8px;
    }
    body.show-command-advanced .deck-advanced {
      display: grid;
    }
    .deck-button,
    .pane-action {
      min-height: 24px;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: #101722;
      color: var(--muted);
      font: 800 10px inherit;
      cursor: pointer;
    }
    .deck-button {
      padding: 0 9px;
    }
    .deck-button.primary {
      border-color: rgba(126, 226, 168, 0.55);
      color: var(--text);
      background: #14261d;
    }
    .deck-button.danger {
      border-color: rgba(255, 122, 144, 0.45);
      color: #ffc2cc;
      background: #241018;
    }
    .deck-button:hover,
    .pane-action:hover {
      border-color: var(--line-strong);
      color: var(--text);
    }
    .deck-composer {
      display: grid;
      grid-template-columns: minmax(0, 1fr) auto;
      gap: 8px;
      min-width: 0;
    }
    .deck-command {
      width: 100%;
      min-height: 56px;
      max-height: 120px;
      resize: vertical;
      border: 1px solid var(--line);
      border-radius: 7px;
      padding: 8px 9px;
      background: #090d14;
      color: var(--text);
      outline: none;
      font: 12px/1.35 ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .deck-command:focus {
      border-color: var(--line-strong);
      box-shadow: inset 0 0 0 1px rgba(111, 179, 255, 0.18);
    }
    .deck-send-stack {
      display: grid;
      grid-template-rows: 1fr 1fr;
      gap: 6px;
      min-width: 118px;
    }
    .deck-scope-row {
      display: grid;
      grid-template-columns: auto 190px minmax(0, 1fr);
      align-items: center;
      gap: 8px;
      min-width: 0;
      color: var(--faint);
      font-size: 10px;
      font-weight: 800;
      letter-spacing: 0.06em;
      text-transform: uppercase;
    }
    .deck-scope {
      width: 100%;
      height: 28px;
      border: 1px solid var(--line);
      border-radius: 7px;
      padding: 0 8px;
      background: #0d1118;
      color: var(--text);
      font: 700 11px inherit;
      outline: none;
    }
    .deck-scope:focus {
      border-color: var(--line-strong);
    }
    .deck-scope-summary {
      min-width: 0;
      overflow: hidden;
      color: var(--muted);
      text-overflow: ellipsis;
      white-space: nowrap;
      letter-spacing: 0;
      text-transform: none;
      font-weight: 600;
    }
    .deck-status {
      min-height: 16px;
      color: var(--faint);
      font: 10px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .deck-receipts {
      display: none;
      grid-template-columns: repeat(auto-fit, minmax(210px, 1fr));
      gap: 6px;
      min-width: 0;
      min-height: 54px;
      max-height: 74px;
      overflow: auto;
    }
    body.show-command-advanced .deck-receipts {
      display: grid;
    }
    .deck-receipt {
      min-width: 0;
      display: grid;
      grid-template-columns: auto minmax(0, 1fr);
      gap: 7px;
      align-items: start;
      padding: 6px 7px;
      border: 1px solid rgba(117, 129, 149, 0.22);
      border-radius: 7px;
      background: #0d1118;
      color: var(--muted);
      font-size: 10px;
      line-height: 1.3;
    }
    .deck-receipt strong {
      display: block;
      min-width: 0;
      overflow: hidden;
      color: var(--text);
      text-overflow: ellipsis;
      white-space: nowrap;
      font-size: 11px;
    }
    .deck-receipt .ops-sub {
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .mission-launch {
      min-width: 0;
      display: grid;
      gap: 7px;
      padding: 8px;
      border: 1px solid rgba(111, 179, 255, 0.24);
      border-radius: 8px;
      background: #0d141f;
    }
    .launch-head {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 10px;
      min-width: 0;
      color: var(--faint);
      font-size: 10px;
      font-weight: 900;
      letter-spacing: 0.08em;
      text-transform: uppercase;
    }
    .launch-head span {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .launch-row {
      display: grid;
      grid-template-columns: minmax(260px, 1.4fr) minmax(150px, 0.75fr) auto auto;
      gap: 6px;
      min-width: 0;
    }
    .launch-input {
      min-width: 0;
      height: 28px;
      border: 1px solid var(--line);
      border-radius: 7px;
      padding: 0 8px;
      background: #090d14;
      color: var(--text);
      outline: none;
      font: 700 11px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .launch-input:focus {
      border-color: var(--line-strong);
    }
    .child-dispatch .launch-row {
      grid-template-columns: minmax(120px, 0.9fr) minmax(104px, 0.4fr) minmax(150px, 0.75fr) minmax(120px, 0.5fr) auto auto;
    }
    .child-dispatch .deck-scope {
      height: 28px;
      min-width: 0;
    }
    .child-brief {
      min-height: 52px;
      margin-top: 1px;
    }
    .launch-plan {
      display: grid;
      gap: 5px;
      max-height: 150px;
      overflow: auto;
    }
    .launch-plan:empty {
      display: none;
    }
    .launch-plan-row {
      min-width: 0;
      display: grid;
      grid-template-columns: minmax(120px, 0.8fr) minmax(150px, 0.9fr) minmax(190px, 1fr);
      gap: 6px;
      align-items: center;
      padding: 6px 7px;
      border: 1px solid rgba(117, 129, 149, 0.22);
      border-radius: 7px;
      background: #0a1018;
      color: var(--muted);
      font-size: 11px;
    }
    .launch-plan-row strong,
    .launch-plan-row span {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .launch-plan-row strong {
      color: var(--text);
    }
    .launch-plan-action {
      justify-self: start;
      padding: 2px 6px;
      border: 1px solid rgba(117, 129, 149, 0.24);
      border-radius: 999px;
      color: var(--muted);
      font: 900 10px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      text-transform: uppercase;
    }
    .launch-plan-action.create {
      border-color: rgba(111, 179, 255, 0.42);
      color: var(--blue);
    }
    .launch-plan-action.reuse {
      border-color: rgba(126, 226, 168, 0.38);
      color: var(--green);
    }
    .launch-plan-action.missing {
      border-color: rgba(241, 196, 95, 0.42);
      color: var(--yellow);
    }
    .launch-status {
      min-height: 15px;
      color: var(--faint);
      font: 10px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .mission-pulse {
      display: grid;
      grid-template-columns: repeat(4, minmax(0, 1fr));
      gap: 6px;
    }
    .pulse-item {
      min-width: 0;
      padding: 6px 7px;
      border: 1px solid rgba(117, 129, 149, 0.22);
      border-radius: 7px;
      background: #0d1118;
    }
    .pulse-value {
      color: var(--text);
      font-size: 13px;
      font-weight: 800;
      line-height: 1.1;
    }
    .pulse-label {
      margin-top: 3px;
      color: var(--faint);
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
      font-size: 10px;
    }
    .attention-inbox,
    .packet-review,
    .change-radar {
      min-width: 0;
      display: grid;
      gap: 6px;
      padding: 7px;
      border: 1px solid rgba(117, 129, 149, 0.22);
      border-radius: 8px;
      background: #0d1118;
    }
    .attention-head,
    .review-head,
    .change-head {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      min-width: 0;
      color: var(--faint);
      font-size: 10px;
      font-weight: 900;
      letter-spacing: 0.08em;
      text-transform: uppercase;
    }
    .attention-head span,
    .review-head span,
    .change-head span {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .attention-list,
    .review-list,
    .change-list {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
      gap: 6px;
      min-width: 0;
      max-height: 88px;
      overflow: auto;
    }
    .attention-item,
    .review-item,
    .change-item {
      min-width: 0;
      display: grid;
      grid-template-columns: minmax(0, 1fr) auto;
      align-items: start;
      gap: 8px;
      padding: 7px;
      border: 1px solid rgba(117, 129, 149, 0.2);
      border-radius: 7px;
      background: #101722;
      color: var(--muted);
      font-size: 11px;
      line-height: 1.35;
    }
    .attention-item strong,
    .attention-item span,
    .review-item strong,
    .review-item span,
    .change-item strong,
    .change-item span {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .attention-item strong,
    .review-item strong,
    .change-item strong {
      display: block;
      color: var(--text);
      font-size: 11px;
    }
    .attention-item.empty,
    .review-item.empty,
    .change-item.empty {
      grid-template-columns: 1fr;
      color: var(--faint);
    }
    .attention-actions,
    .review-actions,
    .change-actions {
      display: flex;
      flex-wrap: wrap;
      justify-content: flex-end;
      gap: 5px;
    }
	    .pane-roster {
	      min-width: 0;
	      position: absolute;
	      left: 8px;
	      right: 8px;
	      bottom: 8px;
	      z-index: 14;
	      max-height: 154px;
	      display: none;
	      grid-template-rows: auto minmax(0, 1fr);
	      overflow: hidden;
	      border: 1px solid var(--line);
	      border-radius: 8px;
	      background: rgba(18, 24, 33, 0.88);
	      box-shadow: 0 -18px 42px rgba(0, 0, 0, 0.34);
	    }
	    body.show-roster .pane-roster {
	      display: grid;
	    }
    .pane-roster-head,
    .pane-roster-row {
      display: grid;
      grid-template-columns: minmax(180px, 1.15fr) minmax(190px, 0.9fr) 96px 102px 112px minmax(176px, auto);
      align-items: center;
      gap: 8px;
      min-width: 0;
      padding: 7px 10px;
    }
    .pane-roster-head {
      border-bottom: 1px solid #222a36;
      background: #10151e;
      color: var(--faint);
      font-size: 10px;
      font-weight: 800;
      letter-spacing: 0.08em;
      text-transform: uppercase;
    }
    .pane-roster-body {
      min-height: 0;
      overflow: auto;
    }
    .pane-roster-row {
      min-height: 42px;
      border-bottom: 1px solid rgba(255, 255, 255, 0.05);
      color: var(--muted);
      font-size: 11px;
      cursor: pointer;
    }
    .pane-roster-row:last-child {
      border-bottom: 0;
    }
    .pane-roster-row:hover,
    .pane-roster-row.active {
      background: #142033;
    }
    .pane-roster-row.active {
      box-shadow: inset 2px 0 0 var(--blue);
    }
    .roster-main,
    .roster-terminal,
    .roster-signal {
      min-width: 0;
      display: grid;
      gap: 2px;
    }
    .roster-main strong,
    .roster-terminal code,
    .roster-signal strong {
      min-width: 0;
      overflow: hidden;
      color: var(--text);
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .roster-terminal code {
      font: 700 11px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .roster-sub {
      min-width: 0;
      overflow: hidden;
      color: var(--faint);
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .roster-actions {
      display: flex;
      flex-wrap: wrap;
      justify-content: flex-end;
      gap: 5px;
      min-width: 0;
    }
    .roster-progress {
      display: grid;
      gap: 4px;
      min-width: 0;
    }
    .role-chip {
      width: fit-content;
      max-width: 100%;
      padding: 2px 6px;
      border: 1px solid rgba(117, 129, 149, 0.28);
      border-radius: 999px;
      background: #0d1118;
      color: var(--muted);
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
      font-size: 9px;
      font-weight: 900;
      letter-spacing: 0.08em;
      text-transform: uppercase;
    }
    .role-chip.parent {
      border-color: rgba(126, 226, 168, 0.38);
      color: var(--green);
    }
    .wave-grid {
      min-width: 0;
      min-height: 0;
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
      grid-auto-rows: minmax(300px, 1fr);
      align-content: stretch;
      gap: 10px;
      overflow: auto;
    }
    .wave-card {
	      min-width: 0;
      min-height: 340px;
	      display: grid;
	      grid-template-rows: auto minmax(0, 1fr) auto;
	      overflow: hidden;
	      cursor: text;
	      outline: none;
	    }
    .wave-card.parent-pane {
      min-height: 340px;
      border-color: rgba(126, 226, 168, 0.56);
      background: #13211d;
    }
    .wave-card.child-pane {
      background: rgba(18, 24, 33, 0.96);
    }
    .wave-card.active {
      border-color: var(--line-strong);
      background: #142033;
      box-shadow: inset 0 0 0 1px rgba(111, 179, 255, 0.2);
    }
    .wave-card:focus-visible {
      border-color: var(--blue);
      box-shadow: 0 0 0 2px rgba(111, 179, 255, 0.25);
    }
    .wave-title {
      display: grid;
      grid-template-columns: auto minmax(0, 1fr) auto;
      align-items: center;
      gap: 8px;
      min-width: 0;
      padding: 9px 10px;
      border-bottom: 1px solid #222a36;
      background: #10151e;
      font-size: 12px;
      font-weight: 800;
    }
    .wave-title > span {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .wave-heading {
      display: grid;
      gap: 2px;
      min-width: 0;
    }
    .wave-heading strong,
    .wave-heading small {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .wave-heading strong {
      color: var(--text);
      font-weight: 850;
    }
    .wave-heading small {
      color: var(--faint);
      font: 700 10px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .pane-id {
      color: var(--faint);
      font: 700 11px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .terminal-role {
      color: var(--green);
      font: 800 10px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      letter-spacing: 0.04em;
      text-transform: uppercase;
    }
    .tile-action {
      height: 22px;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: #101722;
      color: var(--muted);
      font: 800 10px inherit;
      cursor: pointer;
    }
    .tile-action:hover {
      border-color: var(--line-strong);
      color: var(--text);
    }
    .tile-controls {
      min-width: 0;
      display: flex;
      flex-wrap: wrap;
      justify-content: flex-end;
      gap: 5px;
      opacity: 0;
      pointer-events: none;
      transform: translateY(-2px);
      transition: opacity 120ms ease, transform 120ms ease;
    }
    body.pane-wall .wave-card:hover .tile-controls,
    body.pane-wall .wave-card:focus-visible .tile-controls,
    body.pane-wall .tile-controls:focus-within {
      opacity: 1;
      pointer-events: auto;
      transform: translateY(0);
    }
    .tile-controls .pane-action {
      min-height: 22px;
      padding: 0 7px;
      background: rgba(16, 23, 34, 0.92);
    }
    .tile-controls .pane-action.secondary {
      display: none;
    }
    .wave-card:hover .tile-controls .pane-action.secondary,
    .wave-card:focus-visible .tile-controls .pane-action.secondary,
    .tile-controls:focus-within .pane-action.secondary {
      display: inline-flex;
      align-items: center;
    }
    .wave-meta {
      display: flex;
      flex-wrap: wrap;
      gap: 5px;
    }
    .wave-purpose {
      color: var(--muted);
      font-size: 12px;
      line-height: 1.4;
    }
    .tail {
      min-height: 66px;
      padding: 8px;
      border: 1px solid #252d3a;
      border-radius: 7px;
      background: var(--terminal);
      color: #b9c7d9;
      font: 11px/1.35 ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      overflow: hidden;
    }
    .tail::before {
      content: attr(data-label);
      display: block;
      margin: -2px -2px 6px;
      padding-bottom: 5px;
      border-bottom: 1px solid rgba(117, 129, 149, 0.24);
      color: var(--faint);
      font-size: 10px;
      font-weight: 800;
      letter-spacing: 0.04em;
      text-transform: uppercase;
    }
    .tail strong {
      color: var(--green);
      font-weight: 700;
    }
    .mini-terminal {
      position: relative;
      min-height: 0;
      overflow: hidden;
      border: 1px solid rgba(117, 129, 149, 0.18);
      border-width: 1px 0;
      border-radius: 0;
      background: var(--terminal);
    }
    .wave-card.parent-pane .mini-terminal {
      border-color: rgba(126, 226, 168, 0.36);
    }
    .mini-terminal::before {
      display: block;
      content: attr(data-label);
      position: absolute;
      top: 6px;
      right: 8px;
      z-index: 1;
      max-width: min(66%, calc(100% - 16px));
      padding: 3px 6px;
      border: 1px solid rgba(117, 129, 149, 0.24);
      border-radius: 6px;
      background: rgba(9, 11, 15, 0.78);
      color: var(--muted);
      font: 800 9px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      letter-spacing: 0.05em;
      text-transform: uppercase;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
      text-align: right;
      pointer-events: none;
    }
	    .mini-terminal canvas {
	      display: block;
	      width: 100%;
	      height: 100%;
	      background: var(--terminal);
	    }
    .tile-status {
      position: absolute;
      right: 8px;
      bottom: 7px;
      max-width: calc(100% - 14px);
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
      color: var(--faint);
      font: 10px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      pointer-events: none;
    }
    .pane-chips {
      position: absolute;
      left: 8px;
      bottom: 7px;
      display: flex;
      gap: 5px;
      max-width: calc(100% - 120px);
      overflow: hidden;
      pointer-events: none;
    }
    .pane-progress {
      position: absolute;
      left: 8px;
      right: 8px;
      bottom: 32px;
      height: 4px;
      overflow: hidden;
      border-radius: 999px;
      background: rgba(117, 129, 149, 0.18);
      pointer-events: none;
    }
    .pane-progress span {
      display: block;
      height: 100%;
      width: 0%;
      border-radius: inherit;
      background: var(--faint);
    }
    .pane-progress span.good { background: var(--green); }
    .pane-progress span.warn { background: var(--yellow); }
    .pane-progress span.bad { background: var(--red); }
    .pane-footer {
      min-width: 0;
      display: grid;
      grid-template-columns: minmax(0, 1fr) auto;
      gap: 5px 8px;
      align-items: center;
      padding: 5px 8px 6px;
      border-top: 1px solid #222a36;
      background: #0c111a;
    }
    .pane-footer .pane-progress {
      position: static;
      grid-column: 1 / -1;
      height: 4px;
    }
    .pane-footer .pane-chips {
      position: static;
      display: flex;
      gap: 5px;
      max-width: 100%;
      overflow: hidden;
      pointer-events: auto;
    }
    .pane-footer .tile-status {
      position: static;
      max-width: none;
      pointer-events: auto;
    }
    .pane-rail-summary {
      display: none;
      grid-template-columns: minmax(0, 1fr) auto;
      gap: 6px;
      align-items: end;
      min-width: 0;
      padding: 0 8px 8px;
    }
    .rail-status,
    .rail-signal {
      min-width: 0;
      overflow: hidden;
      color: var(--muted);
      text-overflow: ellipsis;
      white-space: nowrap;
      font: 700 10px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .rail-status strong {
      color: var(--text);
      font: 800 10px Inter, ui-sans-serif, system-ui, sans-serif;
      text-transform: uppercase;
    }
    .rail-packet {
      min-width: 52px;
      padding: 3px 7px;
      border: 1px solid rgba(117, 129, 149, 0.24);
      border-radius: 999px;
      background: rgba(9, 13, 20, 0.78);
      color: var(--muted);
      text-align: center;
      font: 800 10px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      white-space: nowrap;
    }
    .rail-packet.good { border-color: rgba(126, 226, 168, 0.4); color: var(--green); }
    .rail-packet.warn { border-color: rgba(243, 201, 105, 0.42); color: var(--yellow); }
    .rail-packet.bad { border-color: rgba(255, 109, 141, 0.42); color: var(--red); }
    .rail-signal {
      grid-column: 1 / -1;
      color: var(--faint);
    }
    .pane-actions {
      position: absolute;
      right: 7px;
      top: 7px;
      display: flex;
      gap: 5px;
      opacity: 0;
      transform: translateY(-2px);
      transition: opacity 120ms ease, transform 120ms ease;
    }
    .wave-card:hover .pane-actions,
    .wave-card:focus-within .pane-actions,
    .wave-card.active .pane-actions {
      opacity: 1;
      transform: translateY(0);
    }
    .pane-action {
      padding: 0 7px;
      background: rgba(16, 23, 34, 0.92);
    }
    .pane-action.good {
      border-color: rgba(126, 226, 168, 0.58);
      color: var(--green);
    }
    .pane-action.danger {
      border-color: rgba(255, 122, 144, 0.5);
      color: var(--red);
    }
    .pane-chip {
      min-width: 0;
      max-width: 180px;
      padding: 2px 6px;
      border: 1px solid rgba(117, 129, 149, 0.28);
      border-radius: 999px;
      background: rgba(18, 24, 33, 0.86);
      color: var(--muted);
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
      font: 800 9px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    body.pane-wall .pane-chip.context-detail {
      display: none;
    }
    .pane-chip.good { color: #11251a; background: var(--green); border-color: var(--green); }
    .pane-chip.warn { color: #2e2307; background: var(--yellow); border-color: var(--yellow); }
    .pane-chip.bad { color: #340813; background: var(--red); border-color: var(--red); }
    .empty-state {
      min-height: 170px;
      padding: 16px;
      border: 1px dashed var(--line);
      border-radius: 8px;
      color: var(--muted);
      background: rgba(18, 24, 33, 0.72);
      font-size: 13px;
      line-height: 1.45;
    }
    .focus-note {
      color: var(--faint);
      font-size: 11px;
      white-space: nowrap;
    }
    .terminal-panel {
      min-width: 0;
      min-height: 0;
      display: none;
      grid-template-rows: 34px 1fr;
      overflow: hidden;
      border: 1px solid var(--line);
      border-radius: 8px;
      background: var(--terminal);
    }
    .terminal-head {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 10px;
      min-width: 0;
      padding: 0 10px;
      border-bottom: 1px solid #222a36;
      background: #10151e;
      color: var(--muted);
      font: 12px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .terminal-title {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
      color: var(--text);
      font-weight: 700;
    }
	    .terminal-actions {
	      display: flex;
	      align-items: center;
	      gap: 6px;
	      min-width: 0;
	      overflow: hidden;
	      white-space: nowrap;
	    }
    .icon-button {
      width: 24px;
      height: 24px;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: var(--panel-2);
      color: var(--muted);
      cursor: pointer;
      font: 700 12px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
	    .terminal-drawer-actions {
	      display: inline-flex;
	      align-items: center;
	      flex: 0 1 auto;
	      min-width: 0;
	      overflow: hidden;
	      border: 1px solid var(--line);
	      border-radius: 6px;
	      background: #0d1118;
	    }
	    .terminal-drawer-button {
	      width: auto;
	      min-width: 52px;
	      padding: 0 8px;
	      border: 0;
	      border-right: 1px solid var(--line);
	      border-radius: 0;
	      background: transparent;
	      color: var(--muted);
	      font-family: Inter, ui-sans-serif, system-ui, sans-serif;
	      font-size: 11px;
	      font-weight: 800;
	    }
	    .terminal-expand-button {
	      width: auto;
	      min-width: 42px;
	      padding: 0 8px;
	      font-family: Inter, ui-sans-serif, system-ui, sans-serif;
	      font-size: 11px;
	      font-weight: 800;
	    }
	    .terminal-drawer-button:last-child {
	      border-right: 0;
	    }
	    .terminal-drawer-button:hover,
	    .terminal-expand-button:hover {
	      background: #142033;
	      color: var(--text);
	    }
    .terminal-wrap {
      position: relative;
      min-height: 0;
      overflow: hidden;
      outline: none;
    }
    #screen {
      display: block;
      width: 100%;
      height: 100%;
      background: var(--terminal);
      outline: none;
    }
    .empty-page {
      min-height: 0;
      padding: 18px;
      border: 1px solid var(--line);
      border-radius: 8px;
      background: rgba(18, 24, 33, 0.94);
      overflow: auto;
    }
    .doc-grid,
    .evidence-grid,
    .audit-grid {
      display: grid;
      grid-template-columns: repeat(2, minmax(220px, 1fr));
      gap: 10px;
      margin-top: 14px;
    }
    .ops-grid {
      display: grid;
      grid-template-columns: repeat(2, minmax(260px, 1fr));
      gap: 10px;
      margin-top: 14px;
    }
    .ops-card {
      min-width: 0;
      padding: 12px;
      border: 1px solid var(--line);
      border-radius: 8px;
      background: var(--panel-2);
    }
    .ops-card.wide {
      grid-column: 1 / -1;
    }
    .ops-card h3 {
      margin-bottom: 8px;
      font-size: 13px;
    }
    .ops-kicker {
      margin-bottom: 4px;
      color: var(--faint);
      font-size: 10px;
      font-weight: 800;
      letter-spacing: 0.08em;
      text-transform: uppercase;
    }
    .ops-row {
      display: grid;
      grid-template-columns: minmax(120px, 1fr) auto auto;
      gap: 8px;
      align-items: center;
      min-height: 34px;
      padding: 8px 0;
      border-top: 1px solid rgba(255, 255, 255, 0.06);
      color: var(--muted);
      font-size: 12px;
    }
    .ops-row:first-of-type {
      border-top: 0;
    }
    .ops-row strong {
      display: block;
      min-width: 0;
      overflow: hidden;
      color: var(--text);
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .ops-row span {
      min-width: 0;
    }
    .ops-sub {
      margin-top: 2px;
      color: var(--faint);
      font-size: 11px;
      line-height: 1.35;
    }
    .ops-pill {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      min-height: 20px;
      padding: 2px 7px;
      border: 1px solid var(--line);
      border-radius: 999px;
      background: #101722;
      color: var(--muted);
      font-size: 10px;
      font-weight: 800;
      white-space: nowrap;
    }
    .ops-pill.good {
      color: #102319;
      background: var(--green);
      border-color: var(--green);
    }
    .ops-pill.warn {
      color: #2e2307;
      background: var(--yellow);
      border-color: var(--yellow);
    }
    .ops-pill.bad {
      color: #340813;
      background: var(--red);
      border-color: var(--red);
    }
    .mini-action {
      min-height: 24px;
      border: 1px solid var(--line);
      border-radius: 6px;
      padding: 2px 8px;
      background: #101722;
      color: var(--text);
      font: 800 10px/1.2 inherit;
      cursor: pointer;
      white-space: nowrap;
    }
    .mini-action:hover {
      border-color: var(--line-strong);
      background: #172235;
    }
    .mini-action.primary {
      border-color: rgba(126, 226, 168, 0.42);
      color: #c6f8dc;
      background: #10241a;
    }
    .mini-action.primary:hover {
      border-color: rgba(126, 226, 168, 0.76);
      background: #163423;
    }
    .mini-action.danger {
      border-color: rgba(255, 122, 144, 0.36);
      color: #ffc2cc;
      background: #241018;
    }
    .mini-action.danger:hover {
      border-color: rgba(255, 122, 144, 0.7);
      background: #351421;
    }
    .radar-actions {
      display: inline-flex;
      justify-content: flex-end;
      gap: 6px;
      white-space: nowrap;
    }
    .radar-picks {
      display: grid;
      grid-template-columns: repeat(3, minmax(170px, 1fr));
      gap: 8px;
      margin: 10px 0 6px;
    }
    .radar-pick {
      min-width: 0;
      display: grid;
      gap: 8px;
      align-content: start;
      border: 1px solid rgba(117, 129, 149, 0.22);
      border-radius: 8px;
      padding: 10px;
      background: rgba(9, 13, 20, 0.52);
    }
    .radar-pick strong,
    .radar-pick span {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .radar-pick-label {
      color: var(--blue);
      font-size: 9px;
      font-weight: 900;
      letter-spacing: 0.1em;
      text-transform: uppercase;
    }
    .radar-pick strong {
      color: var(--text);
      font-size: 12px;
    }
    .radar-pick-empty {
      color: var(--muted);
      font-size: 11px;
    }
    .radar-lanes {
      display: grid;
      gap: 7px;
      margin-top: 10px;
    }
    .radar-lane {
      display: grid;
      border: 1px solid var(--line);
      border-radius: 8px;
      background: rgba(11, 16, 24, 0.52);
      overflow: hidden;
    }
    .radar-lane[open] {
      background: rgba(16, 23, 34, 0.72);
    }
    .radar-lane-head {
      display: grid;
      grid-template-columns: minmax(0, 1fr) auto;
      gap: 10px;
      align-items: start;
      padding: 9px;
      cursor: pointer;
      list-style: none;
    }
    .radar-lane-head::-webkit-details-marker {
      display: none;
    }
    .radar-lane-head:hover {
      background: rgba(38, 52, 73, 0.44);
    }
    .radar-lane-body {
      display: grid;
      gap: 0;
      padding: 0 9px 6px;
      border-top: 1px solid rgba(117, 129, 149, 0.18);
    }
    .radar-lane-title {
      margin: 0;
      font: 900 11px/1.25 inherit;
      color: var(--text);
    }
    .radar-lane-desc {
      margin-top: 2px;
      color: var(--muted);
      font-size: 10px;
      line-height: 1.35;
    }
    .radar-lane-count {
      min-width: 28px;
      padding: 2px 7px;
      border: 1px solid var(--line);
      border-radius: 999px;
      color: var(--muted);
      text-align: center;
      font: 900 10px/1.2 inherit;
      background: #0f1723;
    }
    .radar-lane-empty {
      color: var(--muted);
      font-size: 10px;
      padding: 5px 0 2px;
    }
    .radar-row {
      display: grid;
      grid-template-columns: minmax(0, 1fr) auto auto;
      gap: 8px;
      align-items: center;
      padding: 7px 0;
      border-top: 1px solid rgba(255, 255, 255, 0.06);
    }
    .radar-row strong {
      display: block;
      min-width: 0;
      overflow: hidden;
      color: var(--text);
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .radar-row .ops-sub {
      margin-top: 2px;
    }
    @media (max-width: 1100px) {
      .radar-picks {
        grid-template-columns: minmax(0, 1fr);
      }
      .radar-row {
        grid-template-columns: minmax(0, 1fr) auto;
      }
      .radar-row .radar-actions {
        grid-column: 1 / -1;
        justify-content: flex-start;
      }
    }
    .progress {
      height: 6px;
      overflow: hidden;
      border-radius: 999px;
      background: #0d1118;
      border: 1px solid rgba(255, 255, 255, 0.06);
    }
    .progress span {
      display: block;
      height: 100%;
      border-radius: inherit;
      background: var(--blue);
    }
    .progress span.good { background: var(--green); }
    .progress span.warn { background: var(--yellow); }
    .progress span.bad { background: var(--red); }
    .dispatch-receipts {
      display: grid;
      gap: 6px;
      max-height: 104px;
      margin-top: 8px;
      overflow: auto;
      color: var(--muted);
      font-size: 11px;
    }
    .dispatch-receipt {
      display: grid;
      grid-template-columns: auto minmax(0, 1fr);
      gap: 7px;
      align-items: start;
      padding: 7px;
      border: 1px solid rgba(255, 255, 255, 0.06);
      border-radius: 7px;
      background: #0d1118;
    }
    .dispatch-receipt strong {
      display: block;
      color: var(--text);
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .doc-row {
      padding: 12px;
      border: 1px solid var(--line);
      border-radius: 8px;
      background: var(--panel-2);
    }
    .doc-row h3 {
      font-size: 13px;
      margin-bottom: 8px;
    }
    .doc-row p,
    .doc-row li {
      color: var(--muted);
      font-size: 12px;
      line-height: 1.45;
    }
    .doc-row ul {
      margin: 0;
      padding-left: 18px;
    }
    .inspector-card {
      margin-bottom: 12px;
      padding: 12px;
    }
    .inspector-body {
      min-height: 0;
      overflow: auto;
      padding-right: 2px;
    }
    .inspector-tabs {
      display: grid;
      grid-template-columns: repeat(4, minmax(0, 1fr));
      gap: 6px;
      margin-bottom: 10px;
    }
    .inspector-tab {
      min-width: 0;
      height: 28px;
      padding: 0 6px;
      border: 1px solid var(--line);
      border-radius: 7px;
      background: #101722;
      color: var(--muted);
      font: 800 10px inherit;
      cursor: pointer;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    .inspector-tab:hover,
    .inspector-tab.active {
      border-color: var(--line-strong);
      background: #142033;
      color: var(--text);
    }
    .inspector-panel {
      display: none;
    }
    .inspector-panel.active {
      display: block;
    }
    .inspector-advanced-panels {
      display: none;
      gap: 12px;
      margin-top: 12px;
    }
    body.show-inspector-advanced .inspector-advanced-panels {
      display: grid;
    }
    .inspector-top {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      margin-bottom: 10px;
    }
    .inspector-top .section-title {
      margin: 0;
    }
    .inspector-top-actions {
      display: flex;
      align-items: center;
      gap: 6px;
    }
    .inspector-toggle {
      min-height: 24px;
      padding: 0 8px;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: #101722;
      color: var(--muted);
      font: 800 10px inherit;
      cursor: pointer;
    }
    .inspector-close {
      width: 24px;
      padding: 0;
      font: 900 12px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    .inspector-toggle:hover,
    .inspector-toggle[aria-pressed="true"] {
      border-color: var(--line-strong);
      color: var(--text);
      background: #142033;
    }
    .inspector-card.secondary {
      display: none;
    }
    body.show-inspector-advanced .inspector-card.secondary {
      display: block;
    }
    .inspector-card h2 {
      font-size: 13px;
      margin-bottom: 8px;
    }
    .field {
      display: grid;
      grid-template-columns: 96px 1fr;
      gap: 8px;
      padding: 6px 0;
      border-top: 1px solid rgba(255, 255, 255, 0.06);
      color: var(--muted);
      font-size: 12px;
      line-height: 1.35;
    }
    .field:first-of-type {
      border-top: 0;
    }
    .field label {
      color: var(--faint);
      font-weight: 700;
    }
    .packet-list {
      display: grid;
      gap: 6px;
    }
	    .packet-item {
	      display: grid;
	      grid-template-columns: 14px 1fr;
	      gap: 7px;
	      align-items: start;
	      color: var(--muted);
	      font-size: 12px;
	      cursor: pointer;
	    }
	    .packet-item:hover {
	      color: var(--text);
	    }
    .check {
      width: 12px;
      height: 12px;
      margin-top: 2px;
      border-radius: 3px;
      border: 1px solid var(--line);
      background: var(--panel-3);
    }
	    .check.done {
	      background: var(--green);
	      border-color: var(--green);
	    }
	    .command-box {
	      width: 100%;
	      min-height: 84px;
	      resize: vertical;
	      border: 1px solid var(--line);
	      border-radius: 7px;
	      padding: 9px;
	      background: #0d1118;
	      color: var(--text);
	      font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
	      outline: none;
	    }
	    .command-box:focus {
	      border-color: var(--line-strong);
	    }
	    .output-snapshot {
	      min-height: 126px;
	      max-height: 220px;
	      overflow: auto;
	      border: 1px solid var(--line);
	      border-radius: 7px;
	      padding: 9px;
	      background: #0d1118;
	      color: #b9c7d9;
	      font: 11px/1.42 ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
	      white-space: pre-wrap;
	    }
	    .output-meta {
	      margin-bottom: 8px;
	      color: var(--faint);
	      font-size: 11px;
	    }
	    .input-row {
	      display: grid;
	      grid-template-columns: minmax(0, 1fr) 112px;
	      gap: 8px;
	      margin-bottom: 8px;
	    }
	    .text-input,
	    .select-input {
	      width: 100%;
	      height: 30px;
	      border: 1px solid var(--line);
	      border-radius: 7px;
	      padding: 0 8px;
	      background: #0d1118;
	      color: var(--text);
	      font: 12px inherit;
	      outline: none;
	    }
	    .text-input:focus,
	    .select-input:focus {
	      border-color: var(--line-strong);
	    }
	    .control-row {
	      display: grid;
	      grid-template-columns: 1fr 1fr;
	      gap: 8px;
		      margin: 9px 0 6px;
		    }
		    .control-row.triple {
		      grid-template-columns: repeat(3, minmax(0, 1fr));
		    }
	    .action-button {
	      min-height: 30px;
	      border: 1px solid var(--line);
	      border-radius: 7px;
	      padding: 4px 8px;
	      background: var(--panel-2);
	      color: var(--text);
	      font: 700 12px/1.15 inherit;
	      cursor: pointer;
	    }
	    .control-row.triple .action-button {
	      min-height: 36px;
	    }
	    .action-button:hover {
	      border-color: var(--line-strong);
	      background: #172235;
	    }
    @media (max-width: 1400px) {
      .wave-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); }
    }
	    @media (max-width: 1300px) {
	      .layout { grid-template-columns: 260px minmax(400px, 1fr) 280px; }
	      .tree { padding: 12px 10px; }
	      .inspector { padding: 12px 10px; }
	      .topbar { grid-template-columns: 240px 1fr auto; gap: 10px; padding-inline: 12px; }
	      .wave-grid { grid-template-columns: repeat(2, minmax(280px, 1fr)); }
	      .metrics { grid-template-columns: repeat(3, 82px); }
	      .density-button { padding-inline: 6px; }
	      body.pane-wall.wall-hud-expanded .wall-hud {
	        grid-template-columns: minmax(210px, 0.8fr) minmax(320px, 1.05fr) minmax(220px, 0.7fr) auto;
	      }
	      .wall-hud-command {
	        grid-template-columns: minmax(0, 1fr) minmax(120px, 0.35fr) auto auto;
	      }
	      .wall-hud-actions {
	        justify-content: flex-start;
	      }
	    }
	    @media (max-width: 980px) {
	      .review-decision-grid {
	        grid-template-columns: repeat(2, minmax(0, 1fr));
	      }
	      .focus-note,
	      #terminalStatus {
	        display: none;
	      }
	      .terminal-drawer-button {
	        min-width: 38px;
	        padding: 0 6px;
	      }
	      .terminal-expand-button {
	        min-width: 38px;
	      }
	      body.pane-wall .wall-hud {
	        grid-template-columns: minmax(0, 1fr);
	      }
	      .mission-brief {
	        grid-template-columns: minmax(0, 1fr);
	      }
	    }
  </style>
</head>
<body data-density="dense" data-active-tab="panes" data-active-group="panes" data-active-inspector-tab="pane">
  <div class="app-shell">
    <header class="topbar">
      <div class="brand" id="brandLabel">Herdr Workroom</div>
	      <nav class="tabs" aria-label="Mission tabs">
	        <button class="tab" data-tab="project">Mission</button>
	        <button class="tab active" data-tab="panes">Workbench</button>
	        <button class="tab" data-tab="review">Review</button>
	      </nav>
	      <div class="runtime">
		        <div class="live-mode-switch" aria-label="Live pane view mode">
		          <button class="live-mode-button" id="focusModeToggle" data-live-mode="one" aria-pressed="true" title="Show one watched pane">One pane</button>
		          <button class="live-mode-button" id="paneWallToggle" data-live-mode="all" aria-pressed="false" title="Show every live parent/child terminal pane">All panes</button>
		        </div>
		        <div class="drawer-switches" aria-label="Workbench drawers">
		          <button class="drawer-switch" id="treeQuickToggle" aria-pressed="true" title="Show or hide the parent/child session tree">Tree</button>
		          <button class="drawer-switch" id="detailsQuickToggle" aria-pressed="false" title="Show or hide the watched-pane details drawer">Details</button>
		          <button class="drawer-switch" id="commandQuickToggle" aria-pressed="false" title="Show or hide the parent command drawer">Command</button>
		        </div>
		        <div class="view-menu">
		          <button class="wall-toggle subtle" id="viewMenuToggle" aria-pressed="false" title="Show view and space controls">View</button>
	          <div class="view-menu-panel" id="viewMenuPanel" aria-label="View options">
	            <div class="view-menu-title">Pane space</div>
	            <div class="view-menu-section" data-view-section="pane-density">
	              <span class="view-menu-label">All-pane density</span>
	              <div class="density-control" aria-label="Pane density">
	                <button class="density-button" data-density="roomy" aria-pressed="false" title="Roomier terminal tiles">Roomy</button>
	                <button class="density-button" data-density="dense" aria-pressed="true" title="Balanced terminal wall density">Dense</button>
	                <button class="density-button" data-density="tight" aria-pressed="false" title="Fit more live panes">Tight</button>
	              </div>
	            </div>
	            <div class="view-menu-section" data-view-section="focus-drawers">
	              <span class="view-menu-label">Workbench rails</span>
	              <div class="view-menu-actions">
	                <button class="wall-toggle subtle" id="treeToggle" aria-pressed="true" title="Show or hide the session tree">Session tree</button>
	                <button class="wall-toggle subtle" id="detailsToggle" aria-pressed="false" title="Show or hide watched-pane details">Details drawer</button>
	                <button class="wall-toggle subtle" id="controlsToggle" aria-pressed="false" title="Show or hide the parent intervention tray">Parent tray</button>
	                <button class="wall-toggle subtle" id="rosterToggle" aria-pressed="false" title="Show or hide the pane table">Pane table</button>
	              </div>
	            </div>
	            <div class="view-menu-section" data-view-section="pane-wall">
	              <span class="view-menu-label">All-pane wall</span>
	              <div class="view-menu-actions">
	                <button class="wall-toggle subtle" id="wallHudDetailsToggle" aria-pressed="false" title="Open the all-pane intervention rail">Intervention rail</button>
	                <button class="wall-toggle subtle" id="wallHudFocusMode" title="Return to workbench">Workbench</button>
	              </div>
	            </div>
	          </div>
	        </div>
	        <span class="dot" id="liveDot"></span><span id="streamStatus">connecting</span>
	      </div>
	      </header>
	      <button class="edge-reopen tree-edge" id="treeReopen" title="Reopen the session tree">Tree</button>
	      <button class="edge-reopen details-edge" id="detailsReopen" title="Reopen watched-pane details">Details</button>
	      <button type="button" class="drawer-scrim" id="drawerScrim" aria-label="Close open drawers" title="Close open drawers"></button>
	      <div class="layout">
      <aside class="tree">
        <div class="tree-caption"><span>Session tree</span><div class="tree-caption-actions"><strong id="projectNodeLabel">Loading project</strong><button class="tree-close" id="closeTreeDrawer" aria-label="Close session tree drawer" title="Close session tree drawer">x</button></div></div>
        <div class="file-tree">
          <div class="tree-children root-tree" id="missionChildren">
            <div class="arc-node"><span></span><span>Loading panes</span><span></span></div>
          </div>
        </div>
      </aside>
      <main class="surface">
        <section class="mission-strip">
          <div class="mission-card">
            <div class="mission-kicker">Parent mission</div>
            <h1 id="parentTitle">Loading mission</h1>
            <p class="mission-copy" id="parentSummary">Loading child pane state.</p>
          </div>
          <div class="metrics">
            <div class="metric-card"><div class="metric-value" id="metricWaves">0</div><div class="metric-label">child panes</div></div>
            <div class="metric-card"><div class="metric-value" id="metricPacket">0/10</div><div class="metric-label">packets</div></div>
            <div class="metric-card"><div class="metric-value" id="metricOverlap">0</div><div class="metric-label">contracts</div></div>
          </div>
	        </section>
	        <div class="review-tabs" id="reviewTabs" hidden aria-label="Review lane navigation lives in the room sidecar.">
	          <button class="review-tab active" data-review-tab="review">Overview</button>
	          <button class="review-tab" data-review-tab="evidence">Evidence</button>
	          <button class="review-tab" data-review-tab="changes">Changes</button>
	          <button class="review-tab" data-review-tab="audit">Audit</button>
	          <button class="review-tab" data-review-tab="timeline">Timeline</button>
	        </div>
        <section class="tab-page active" data-page="panes">
          <div class="live-command-strip" id="liveCommandStrip">
            <div class="live-command-copy">
              <span class="live-command-kicker">Parent command</span>
              <strong id="liveCommandSummary">Loading parent decision state.</strong>
              <span id="liveCommandMeta">Packets and sweep state will appear here.</span>
            </div>
            <div class="live-command-actions" id="liveCommandActions"></div>
          </div>
          <div class="wave-grid" id="waveGrid">
            <div class="empty-state">Loading Herdr panes.</div>
          </div>
	          <div class="pane-command-deck">
	            <div class="deck-top">
	              <div class="deck-title">
	                <strong>Parent command tray</strong>
                <span id="deckSummary">Watch a child pane, intervene, read it, or broadcast to all children.</span>
              </div>
              <div class="deck-actions">
                <button class="deck-button" id="deckMessageSelected">Intervene watched</button>
                <button class="deck-button" id="deckReadSelected">Read watched</button>
                <button class="deck-button" id="deckRequestPackets">Request packets</button>
                <button class="deck-button" id="deckAdvancedToggle" aria-pressed="false" title="Show child launch, inbox, packet queue, and change radar">Mission tools</button>
                <button class="deck-button deck-advanced-action" id="deckStartChild">New wave pane</button>
                <button class="deck-button deck-advanced-action" id="deckSweep">Sweep packets</button>
                <button class="deck-button deck-advanced-action danger" id="deckStopSelected">Stop watched child</button>
                <button class="deck-button primary deck-advanced-action" id="deckUnlockReady">Unlock ready</button>
              </div>
            </div>
            <div class="deck-composer">
              <textarea class="deck-command" id="deckCommand" spellcheck="false" placeholder="Type an intervention, then send it to the watched pane or all child panes."></textarea>
              <div class="deck-send-stack">
                <button class="deck-button primary" id="deckSendSelected">Send watched</button>
                <button class="deck-button" id="deckSendAll">Broadcast scope</button>
              </div>
            </div>
            <div class="deck-scope-row">
              <label for="deckScope">Scope</label>
              <select id="deckScope" class="deck-scope">
                <option value="all">All children</option>
                <option value="attention">Needs attention</option>
                <option value="missing_packet">Missing packets</option>
                <option value="write">Write-capable</option>
                <option value="read_only">Read-only</option>
                <option value="review">Review / verify</option>
              </select>
              <span id="deckScopeSummary" class="deck-scope-summary">Broadcast targets all child panes.</span>
            </div>
            <div class="deck-status" id="deckStatus">No parent dispatch yet.</div>
            <div class="deck-receipts" id="deckDispatchReceipts">
              <div class="deck-receipt"><span class="dot"></span><span><strong>No parent messages yet</strong><span class="ops-sub">Receipts appear here when the parent talks to a child pane.</span></span></div>
            </div>
            <div class="deck-advanced" id="deckAdvancedPanel">
            <div class="mission-launch child-dispatch" id="childDispatch">
              <div class="launch-head"><span>Wave dispatch</span><span id="childDispatchSummary">parent -> new pane -> contract -> start brief</span></div>
              <div class="launch-row">
                <input id="deckChildTitle" class="launch-input" value="Researcher" aria-label="Child pane title">
                <select id="deckChildMode" class="deck-scope" aria-label="Child pane mode">
                  <option value="read_only">read-only</option>
                  <option value="draft_only" selected>draft-only</option>
                  <option value="write">write</option>
                  <option value="reviewer">reviewer</option>
                  <option value="verifier">verifier</option>
                  <option value="monitor">monitor</option>
                </select>
                <input id="deckChildArgv" class="launch-input" value="/bin/zsh -l" aria-label="Child pane command">
                <select id="deckChildDependency" class="deck-scope" aria-label="Child pane dependency">
                  <option value="">parallel</option>
                  <option value="after parent approval">after approval</option>
                  <option value="after packet accepted">after packet</option>
                </select>
                <button class="deck-button primary" id="deckStartChildRight">Create right</button>
                <button class="deck-button" id="deckStartChildDown">Create down</button>
              </div>
              <textarea id="deckChildBrief" class="deck-command child-brief" spellcheck="false" placeholder="Goal, allowed paths/tools, required report packet"></textarea>
              <div class="launch-status" id="childDispatchStatus">Ready.</div>
            </div>
            <div class="mission-launch" id="missionLaunch">
              <div class="launch-head"><span>Mission launch</span><span id="missionLaunchSummary">session file -> real child panes</span></div>
              <div class="launch-row">
                <input id="deckSessionPath" class="launch-input" aria-label="Mission session file path" placeholder="/path/to/session.md">
                <input id="deckAgentArgv" class="launch-input" value="/bin/zsh -l" aria-label="Launch argv">
                <button class="deck-button" id="deckPreviewImport">Preview launch</button>
                <button class="deck-button primary" id="deckImportSession">Launch child panes</button>
                <button class="deck-button" id="deckImportWall">Launch + pane wall</button>
              </div>
              <div class="launch-plan" id="missionLaunchPlan"></div>
              <div class="launch-status" id="missionLaunchStatus">Ready.</div>
            </div>
            <div class="mission-pulse" id="missionPulse">
              <div class="pulse-item"><div class="pulse-value">0</div><div class="pulse-label">child panes</div></div>
              <div class="pulse-item"><div class="pulse-value">0/0</div><div class="pulse-label">packets ready</div></div>
              <div class="pulse-item"><div class="pulse-value">0</div><div class="pulse-label">parent decisions</div></div>
              <div class="pulse-item"><div class="pulse-value">0</div><div class="pulse-label">source changes</div></div>
            </div>
            <div class="attention-inbox" id="attentionInbox">
              <div class="attention-head"><span>Needs-input inbox</span><span id="attentionInboxCount">0</span></div>
              <div class="attention-list" id="attentionInboxList">
                <div class="attention-item empty">No child panes need parent attention.</div>
              </div>
            </div>
            <div class="packet-review" id="packetReview">
              <div class="review-head"><span>Packet review queue</span><span id="packetReviewCount">0</span></div>
              <div class="review-list" id="packetReviewList">
                <div class="review-item empty">No completed child packets are waiting for review.</div>
              </div>
            </div>
            <div class="change-radar" id="changeRadar">
              <div class="change-head"><span>Change radar</span><span id="changeRadarCount">0</span></div>
              <div class="change-list" id="changeRadarList">
                <div class="change-item empty">No git-visible source changes detected.</div>
              </div>
            </div>
            </div>
          </div>
          <div class="pane-roster" id="paneRoster">
            <div class="pane-roster-head">
              <span>Session pane</span>
              <span>Terminal</span>
              <span>Mode</span>
              <span>Packet</span>
              <span>Signal</span>
              <span>Actions</span>
            </div>
            <div class="pane-roster-body"><div class="pane-roster-row"><span>Loading pane roster</span><span></span><span></span><span></span><span></span><span></span></div></div>
          </div>
	          <section class="terminal-panel">
	            <div class="terminal-head">
	              <div class="terminal-title" id="terminalTitle">Selected pane</div>
              <div class="terminal-actions"><span class="focus-note">watching one pane</span><span id="terminalStatus">interactive</span><span class="terminal-drawer-actions" aria-label="Watched pane drawer shortcuts"><button class="icon-button terminal-drawer-button" data-open-inspector="pane" aria-label="Inspect watched pane" title="Inspect watched pane">Inspect</button><button class="icon-button terminal-drawer-button" data-open-inspector="command" aria-label="Intervene in watched pane" title="Intervene in watched pane">Intervene</button><button class="icon-button terminal-drawer-button" data-open-inspector="output" aria-label="Read watched pane output" title="Read watched pane output">Output</button><button class="icon-button terminal-drawer-button" data-open-inspector="packet" aria-label="Review watched pane report packet" title="Review watched pane report packet">Packet</button></span><button class="icon-button terminal-expand-button" id="expandTerminal" aria-label="Expand watched pane" title="Expand watched pane">Expand pane</button></div>
	            </div>
            <div class="terminal-wrap" id="terminalWrap" tabindex="0">
              <canvas id="screen" tabindex="0"></canvas>
            </div>
          </section>
		        </section>
		        <section class="tab-page" data-page="project">
		          <div class="mission-room">
		            <div class="mission-room-head">
		              <div>
		                <div class="mission-kicker">Mission contract</div>
		                <h1>Parent scope and dispatch contract</h1>
		              </div>
		              <div class="review-toolbar">
		                <span id="missionSweepStatus">Sweep has not run in this view yet.</span>
		                <button class="mini-action primary" data-radar-scan>Research next work</button>
		                <button class="mini-action" id="missionRefreshSweep">Refresh sweep</button>
		                <button class="mini-action" data-room-context-toggle aria-pressed="true" title="Show or hide the mission context rail">Context</button>
		              </div>
		            </div>
		            <div class="mission-room-body">
		              <div class="mission-main">
		                <div class="mission-brief" id="missionBrief"></div>
		                <div class="mission-state-room ops-grid" id="projectBoard"></div>
		              </div>
		              <aside class="mission-sidecar">
		                <div>
		                  <div class="mission-kicker">Mission lanes</div>
		                  <h2>Parent contract lanes</h2>
		                  <p>Mission is the contract surface. Keep goals, scope, child dispatch, and review gates here; jump to live panes only when terminal truth matters.</p>
		                </div>
		                <div class="room-lane-actions">
		                  <button class="room-lane-button" data-radar-scan>Research next work</button>
		                  <button class="room-lane-button" data-room-jump="panes">Workbench</button>
		                  <button class="room-lane-button" data-open-command-tray>Launch children</button>
		                  <button class="room-lane-button" data-room-jump="review">Review</button>
		                  <button class="room-lane-button" data-room-jump="evidence">Evidence</button>
		                </div>
		              </aside>
		            </div>
		          </div>
		        </section>
		        <section class="tab-page" data-page="review">
		          <div class="review-room">
		            <div class="review-room-head">
		              <div>
		                <div class="mission-kicker">Review room</div>
		                <h1>Mission decisions before anything gets crowned done</h1>
		              </div>
		              <div class="review-toolbar">
		                <span id="reviewSweepStatus">Sweep has not run in this view yet.</span>
		                <button class="mini-action" id="reviewRefreshSweep">Refresh sweep</button>
		                <button class="mini-action" data-room-context-toggle aria-pressed="true" title="Show or hide the review context rail">Context</button>
		              </div>
		            </div>
		            <div class="review-room-body">
		              <div class="review-main">
		                <div class="mission-brief review-brief" id="reviewBrief"></div>
		                <div class="review-decision-room" id="reviewDecisionRoom"></div>
		              </div>
		              <aside class="review-sidecar">
		                <div>
		                  <div class="mission-kicker">Review lanes</div>
		                  <h2>Context lanes</h2>
		                  <p>Review does not need every control visible at once. Jump into the lane you need, then come back to the decision queue.</p>
		                </div>
		                <div class="room-lane-actions">
		                  <button class="room-lane-button" data-room-jump="evidence">Evidence</button>
		                  <button class="room-lane-button" data-room-jump="changes">Changes</button>
		                  <button class="room-lane-button" data-room-jump="audit">Audit</button>
		                  <button class="room-lane-button" data-room-jump="timeline">Timeline</button>
		                </div>
		              </aside>
		            </div>
		          </div>
		        </section>
		        <section class="tab-page" data-page="evidence">
		          <div class="lane-room">
		            <div class="lane-room-head">
		              <div>
		                <div class="mission-kicker">Evidence board</div>
		                <h1>Claims and receipts from child panes</h1>
		              </div>
		              <div class="review-toolbar">
		                <span>Evidence lane context</span>
		                <button class="mini-action" data-room-context-toggle aria-pressed="true" title="Show or hide the evidence context rail">Context</button>
		              </div>
		            </div>
		            <div class="lane-room-body">
		              <div class="lane-main">
		                <div class="evidence-grid" id="evidenceBoard"></div>
		              </div>
		              <aside class="lane-sidecar">
		                <div>
		                  <div class="mission-kicker">Review lane</div>
		                  <h2>Evidence lanes</h2>
		                  <p>Receipts live here so the parent can judge claims without opening every pane. Return to Overview when the lane has enough signal for a decision.</p>
		                </div>
		                <div class="room-lane-actions">
		                  <button class="room-lane-button" data-room-jump="review">Overview</button>
		                  <button class="room-lane-button" data-room-jump="changes">Changes</button>
		                  <button class="room-lane-button" data-room-jump="audit">Audit</button>
		                  <button class="room-lane-button" data-room-jump="timeline">Timeline</button>
		                </div>
		              </aside>
		            </div>
		          </div>
	        </section>
	        <section class="tab-page" data-page="changes">
	          <div class="lane-room">
	            <div class="lane-room-head">
	              <div>
	                <div class="mission-kicker">Changes</div>
	                <h1>Blast radius radar</h1>
	              </div>
	              <div class="review-toolbar">
	                <span>Change lane context</span>
	                <button class="mini-action" data-room-context-toggle aria-pressed="true" title="Show or hide the change context rail">Context</button>
	              </div>
	            </div>
	            <div class="lane-room-body">
	              <div class="lane-main">
	                <div class="doc-grid" id="changesBoard"></div>
	              </div>
	              <aside class="lane-sidecar">
	                <div>
	                  <div class="mission-kicker">Review lane</div>
	                  <h2>Change lanes</h2>
	                  <p>Predicted and actual file touchpoints stay together here. Return to Overview when the lane has enough signal for a decision.</p>
	                </div>
	                <div class="room-lane-actions">
	                  <button class="room-lane-button" data-room-jump="review">Overview</button>
	                  <button class="room-lane-button" data-room-jump="evidence">Evidence</button>
	                  <button class="room-lane-button" data-room-jump="audit">Audit</button>
	                  <button class="room-lane-button" data-room-jump="timeline">Timeline</button>
	                </div>
	              </aside>
	            </div>
	          </div>
	        </section>
	        <section class="tab-page" data-page="audit">
	          <div class="lane-room">
	            <div class="lane-room-head">
	              <div>
	                <div class="mission-kicker">Done court</div>
	                <h1>Acceptance gates</h1>
	              </div>
	              <div class="review-toolbar">
	                <span>Audit lane context</span>
	                <button class="mini-action" data-room-context-toggle aria-pressed="true" title="Show or hide the audit context rail">Context</button>
	              </div>
	            </div>
	            <div class="lane-room-body">
	              <div class="lane-main">
	                <div class="audit-grid" id="auditBoard"></div>
	              </div>
	              <aside class="lane-sidecar">
	                <div>
	                  <div class="mission-kicker">Review lane</div>
	                  <h2>Audit lanes</h2>
	                  <p>Done means accepted packet, evidence, risk call, and a clear parent verdict. Return to Overview when the lane has enough signal for a decision.</p>
	                </div>
	                <div class="room-lane-actions">
	                  <button class="room-lane-button" data-room-jump="review">Overview</button>
	                  <button class="room-lane-button" data-room-jump="evidence">Evidence</button>
	                  <button class="room-lane-button" data-room-jump="changes">Changes</button>
	                  <button class="room-lane-button" data-room-jump="timeline">Timeline</button>
	                </div>
	              </aside>
	            </div>
	          </div>
	        </section>
	        <section class="tab-page" data-page="timeline">
	          <div class="lane-room">
	            <div class="lane-room-head">
	              <div>
	                <div class="mission-kicker">Timeline</div>
	                <h1>Mission events</h1>
	              </div>
	              <div class="review-toolbar">
	                <span>Timeline lane context</span>
	                <button class="mini-action" data-room-context-toggle aria-pressed="true" title="Show or hide the timeline context rail">Context</button>
	              </div>
	            </div>
	            <div class="lane-room-body">
	              <div class="lane-main">
	                <div class="doc-grid" id="timelineBoard"></div>
	              </div>
	              <aside class="lane-sidecar">
	                <div>
	                  <div class="mission-kicker">Review lane</div>
	                  <h2>Timeline lanes</h2>
	                  <p>The sequence of starts, stops, packets, and parent decisions stays readable here. Return to Overview when the lane has enough signal for a decision.</p>
	                </div>
	                <div class="room-lane-actions">
	                  <button class="room-lane-button" data-room-jump="review">Overview</button>
	                  <button class="room-lane-button" data-room-jump="evidence">Evidence</button>
	                  <button class="room-lane-button" data-room-jump="changes">Changes</button>
	                  <button class="room-lane-button" data-room-jump="audit">Audit</button>
	                </div>
	              </aside>
	            </div>
	          </div>
	        </section>
      </main>
	      <aside class="inspector">
	        <div class="inspector-top">
	          <h2 class="section-title">Watched pane</h2>
	          <div class="inspector-top-actions">
	            <button class="inspector-toggle" id="inspectorAdvancedToggle" aria-pressed="false" title="Show report, launch, and mission utilities">Pane tools</button>
	            <button class="inspector-toggle inspector-close" id="closeInspector" title="Close watched-pane drawer">x</button>
	          </div>
	        </div>
        <nav class="inspector-tabs" aria-label="Watched pane drawer">
          <button class="inspector-tab active" data-inspector-tab="pane">Inspect</button>
          <button class="inspector-tab" data-inspector-tab="command">Intervene</button>
          <button class="inspector-tab" data-inspector-tab="output">Output</button>
          <button class="inspector-tab" data-inspector-tab="packet">Packet</button>
        </nav>
        <div class="inspector-body">
        <section class="inspector-card inspector-panel active" data-inspector-panel="pane">
          <h2 id="selectedTitle">No pane watched</h2>
          <div class="field"><label>Pane</label><span id="selectedPane">none</span></div>
          <div class="field"><label>Terminal</label><span id="selectedTerminal">none</span></div>
          <div class="field"><label>Workspace</label><span id="selectedWorkspace">none</span></div>
          <div class="field"><label>Tab</label><span id="selectedTab">none</span></div>
          <div class="field"><label>Mode</label><span id="selectedMode">write</span></div>
          <div class="field"><label>Status</label><span id="selectedStatus">running</span></div>
          <div class="field"><label>Depends</label><span id="selectedDepends">none</span></div>
          <div class="field"><label>Blast</label><span id="selectedBlast">none</span></div>
          <div class="field"><label>Arcs</label><span id="selectedArcs">A: schema, D: packet</span></div>
	        </section>
	        <section class="inspector-card inspector-panel" data-inspector-panel="command">
	          <h2>Intervention rail</h2>
	          <textarea id="parentCommand" class="command-box" rows="4" placeholder="Intervention to selected child pane, or broadcast to all child panes."></textarea>
	          <div class="control-row">
	            <button class="action-button" id="sendSelected">Send watched</button>
	            <button class="action-button" id="sendAll">Send all children</button>
	          </div>
	          <div class="control-row triple">
	            <button class="action-button" id="commandReadOutput">Read</button>
	            <button class="action-button" id="commandRequestPacket">Packet</button>
	            <button class="action-button" id="closeSelected">Stop child</button>
	          </div>
	          <div class="field"><label>Target</label><span id="controlTarget">watched pane</span></div>
	          <div class="field"><label>Last dispatch</label><span id="selectedDispatch">no dispatch yet</span></div>
	          <div class="field"><label>Policy</label><span>px optional, pane contracts required when present</span></div>
	          <div class="dispatch-receipts" id="dispatchReceipts">
	            <div class="dispatch-receipt"><span class="dot"></span><span><strong>No dispatches yet</strong>Messages sent from the parent will show per-pane receipts here.</span></div>
	          </div>
	        </section>
	        <section class="inspector-card inspector-panel" data-inspector-panel="output">
	          <h2>Latest output</h2>
	          <div class="output-meta" id="outputMeta">No output snapshot yet.</div>
	          <pre class="output-snapshot" id="selectedOutput">Select a pane, then read its output.</pre>
	          <div class="control-row">
	            <button class="action-button" id="readOutput">Read output</button>
	            <button class="action-button" id="copyOutputPrompt">Use as prompt</button>
	          </div>
	        </section>
	        <section class="inspector-card inspector-panel" data-inspector-panel="packet">
	          <h2>Report packet</h2>
	          <div class="packet-list" id="packetList"></div>
	          <div class="control-row">
	            <button class="action-button" id="ingestPacket">Ingest output</button>
	            <button class="action-button" id="requestPacket">Request packet</button>
	          </div>
	        </section>
        <div class="inspector-advanced-panels" id="inspectorAdvancedPanels">
	        <section class="inspector-card secondary">
	          <h2>New child session</h2>
	          <div class="input-row">
	            <input id="childTitle" class="text-input" value="Researcher" aria-label="Child title">
	            <select id="childMode" class="select-input" aria-label="Child mode">
	              <option value="read_only">read-only</option>
	              <option value="draft_only" selected>draft-only</option>
	              <option value="write">write</option>
	              <option value="reviewer">reviewer</option>
	              <option value="verifier">verifier</option>
	              <option value="monitor">monitor</option>
	            </select>
	          </div>
	          <div class="input-row">
	            <input id="agentArgv" class="text-input" value="claude" aria-label="Agent argv" placeholder="claude, codex, zsh, ...">
	            <select id="childDependency" class="select-input" aria-label="Dependency">
	              <option value="">parallel</option>
	              <option value="after parent approval">after approval</option>
	              <option value="after packet accepted">after packet</option>
	            </select>
	          </div>
	          <textarea id="childBrief" class="command-box" rows="3" placeholder="Goal, allowed files/tools, required report packet"></textarea>
	          <div class="control-row">
	            <button class="action-button" id="startChildRight">Create right</button>
	            <button class="action-button" id="startChildDown">Create down</button>
	          </div>
	        </section>
	        <section class="inspector-card secondary">
	          <h2>Mission utilities</h2>
	          <div class="input-row">
	            <input id="sessionPath" class="text-input" aria-label="Session file path" placeholder="/path/to/FoxFlow/.sessions/...md">
	            <button class="action-button" id="importSession">Import</button>
	          </div>
	          <div class="control-row triple">
	            <button class="action-button" id="loadContract">Load contract</button>
	            <button class="action-button" id="loadMissing">Load missing</button>
	            <button class="action-button" id="loadFiles">Load files</button>
	          </div>
	          <div class="control-row triple">
	            <button class="action-button" id="markAccepted">Accept</button>
	            <button class="action-button" id="markReview">Needs review</button>
	            <button class="action-button" id="refreshEvidence">Refresh</button>
	          </div>
	          <div class="control-row">
	            <button class="action-button" id="splitRight">Split right</button>
	            <button class="action-button" id="splitDown">Split down</button>
	          </div>
        </section>
        </div>
        </div>
      </aside>
    </div>
  </div>
  <div class="wall-hud" id="wallHud" aria-live="polite">
    <div class="wall-hud-main">
      <span class="wall-hud-kicker">Watching pane</span>
      <strong class="wall-hud-title" id="wallHudTitle">No pane watched</strong>
      <span class="wall-hud-meta" id="wallHudMeta">Choose a live pane to watch or intervene.</span>
      <div class="wall-hud-context" id="wallHudContext"></div>
      <div class="wall-hud-pulse" id="wallHudPulse">
        <span class="wall-pulse-item"><strong class="wall-pulse-value">0</strong><span class="wall-pulse-label">children</span></span>
        <span class="wall-pulse-item"><strong class="wall-pulse-value">0/0</strong><span class="wall-pulse-label">packets</span></span>
        <span class="wall-pulse-item"><strong class="wall-pulse-value">0</strong><span class="wall-pulse-label">decisions</span></span>
        <span class="wall-pulse-item"><strong class="wall-pulse-value">0</strong><span class="wall-pulse-label">failed</span></span>
      </div>
      <div class="wall-hud-dag" id="wallHudDag" aria-label="Mission pane rail"></div>
    </div>
    <div class="wall-hud-command">
      <textarea class="wall-hud-input" id="wallHudCommand" spellcheck="false" placeholder="Intervention instruction to watched pane or scope"></textarea>
      <select class="wall-hud-scope" id="wallHudScope" aria-label="Wall command scope">
        <option value="all">All children</option>
        <option value="attention">Needs attention</option>
        <option value="missing_packet">Missing packets</option>
        <option value="write">Write-capable</option>
        <option value="read_only">Read-only</option>
        <option value="review">Review / verify</option>
      </select>
      <button class="wall-hud-button primary" id="wallHudSendSelected">Send watched</button>
      <button class="wall-hud-button" id="wallHudSendScope">Broadcast scope</button>
      <span class="wall-hud-status" id="wallHudStatus">Intervention rail ready.</span>
      <div class="wall-hud-presets" aria-label="Parent quick prompts">
        <button class="wall-preset" data-wall-preset="status">status ping</button>
        <button class="wall-preset" data-wall-preset="packet">report packet</button>
        <button class="wall-preset" data-wall-preset="freeze">freeze edits</button>
        <button class="wall-preset" data-wall-preset="finish">finish handoff</button>
      </div>
    </div>
    <div class="wall-hud-meter">
      <span id="wallHudPacket">packet n/a</span>
      <span id="wallHudTerminal">terminal n/a</span>
      <pre class="wall-hud-readout" id="wallHudReadout">No read snapshot yet.</pre>
    </div>
    <div class="wall-hud-actions">
      <button class="wall-hud-button" id="wallHudMessage">Intervene</button>
      <button class="wall-hud-button" id="wallHudRead">Read output</button>
      <button class="wall-hud-button" id="wallHudSweep">Sweep mission</button>
      <button class="wall-hud-button" id="wallHudNewChild">New child</button>
      <button class="wall-hud-button" id="wallHudPacketsAll">Request packets</button>
      <button class="wall-hud-button" id="wallHudPacketAction">Request packet</button>
      <button class="wall-hud-button" id="wallHudMore" aria-pressed="false">Intervene</button>
      <button class="wall-hud-button primary" id="wallHudFull">Expand pane</button>
      <button class="wall-hud-button" id="wallHudExit">Workbench</button>
    </div>
  </div>
  <script>
    const canvas = document.getElementById('screen');
    const terminalWrap = document.getElementById('terminalWrap');
    const streamStatus = document.getElementById('streamStatus');
    const terminalStatus = document.getElementById('terminalStatus');
    const terminalTitle = document.getElementById('terminalTitle');
    const liveDot = document.getElementById('liveDot');
    const ctx = canvas.getContext('2d', { alpha: false });
    const fontSize = 13;
    const fontFamily = 'ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace';
    const cellHeight = 17;
    let cellWidth = 8;
    let source = null;
	    let currentCols = 120;
	    let currentRows = 40;

	    let waves = {};
	    let workspaces = [];
	    let gitStatuses = {};
	    let dispatchEvents = [];
	    let outputSnapshots = {};
	    let outputEvents = [];
	    let missionImportEvents = [];
	    let lastMissionSweep = null;
	    let workroomProjection = null;
	    let evidenceLedger = [];
	    let missionRefreshInFlight = false;
		    let reviewRefreshInFlight = false;
		    let parentPaneId = null;
		    let selectedWaveId = null;
		    let missionRadarItems = [];
		    let missionRadarScan = null;
		    let missionRadarScanInFlight = false;
	    let focusSource = null;
    const tileSources = new Map();
    const latestFrames = new Map();
    const tileCanvases = new Map();
	    const tileCols = 72;
	    const tileRows = 18;
	    let integrationState = {
	      preferredArgv: ['/bin/zsh', '-l'],
	      preferredLabel: 'shell',
	      recommendations: []
	    };
	    let missionBoardCollapsedLanes = new Set();
    const viewPreferenceKeys = {
      paneWall: 'herdr.desktop.paneWall',
      density: 'herdr.desktop.paneDensity',
      viewMenu: 'herdr.desktop.viewMenu',
      treeVisible: 'herdr.desktop.treeVisible',
      detailsVisible: 'herdr.desktop.detailsVisible',
      commandDeck: 'herdr.desktop.commandDeck',
      commandAdvanced: 'herdr.desktop.commandAdvanced',
      roster: 'herdr.desktop.roster',
      inspectorAdvanced: 'herdr.desktop.inspectorAdvanced',
      roomContextVisible: 'herdr.desktop.roomContextVisible.v2',
      missionBoardCollapsed: 'herdr.desktop.missionBoardCollapsed',
      missionRadarScan: 'herdr.desktop.missionRadarScan',
      evidenceLedger: 'herdr.desktop.evidenceLedger.v1'
    };
    const validDensities = new Set(['roomy', 'dense', 'tight']);
    const packetFields = [
      'What I did',
      'What I found',
      'Evidence / receipts',
      'Files read',
      'Files changed',
      'Commands run',
      'Risks / unknowns',
      'Good / bad / ugly',
	      'Recommendation',
	      'Next wave suggestion'
	    ];

    function readPreference(key, fallback = '') {
      try {
        return window.localStorage.getItem(key) || fallback;
      } catch (_) {
        return fallback;
      }
    }

    function writePreference(key, value) {
      try {
        window.localStorage.setItem(key, value);
      } catch (_) {}
    }

    function readJsonPreference(key, fallback = null) {
      const raw = readPreference(key, '');
      if (!raw) return fallback;
      try {
        return JSON.parse(raw);
      } catch (_) {
        return fallback;
      }
    }

    function writeJsonPreference(key, value) {
      try {
        window.localStorage.setItem(key, JSON.stringify(value));
      } catch (_) {}
    }

    function resizeCanvas() {
      const scale = window.devicePixelRatio || 1;
      const rect = terminalWrap.getBoundingClientRect();
      const width = Math.max(320, Math.floor(rect.width));
      const height = Math.max(180, Math.floor(rect.height));
      canvas.width = Math.floor(width * scale);
      canvas.height = Math.floor(height * scale);
      canvas.style.width = width + 'px';
      canvas.style.height = height + 'px';
      ctx.setTransform(scale, 0, 0, scale, 0, 0);
      ctx.font = fontSize + 'px ' + fontFamily;
      cellWidth = Math.max(7, Math.ceil(ctx.measureText('W').width));
      currentCols = Math.max(1, Math.floor(width / cellWidth));
      currentRows = Math.max(1, Math.floor(height / cellHeight));
    }

    function color(value, fallback) {
      const tag = (value >>> 24) & 0xff;
      const named = [
        fallback, '#090b0f', '#ff7a90', '#7ee2a8', '#f3c969', '#6fb3ff',
        '#b9a2ff', '#56d4dd', '#a8b3c2', '#758195', '#ff9aa2', '#a5d6a7',
        '#ffe6a7', '#a5d6ff', '#e2c5ff', '#9be9f5', '#edf2f8'
      ];
      if (tag === 0) return named[value & 0xff] || fallback;
      if (tag === 1) return fallback;
      if (tag === 2) {
        const r = (value >>> 16) & 0xff;
        const g = (value >>> 8) & 0xff;
        const b = value & 0xff;
        return `rgb(${r}, ${g}, ${b})`;
      }
      return fallback;
    }

    function drawFrame(targetCanvas, frame, options = {}) {
      const width = frame.width || 1;
      const targetCtx = targetCanvas.getContext('2d', { alpha: false });
      const scale = window.devicePixelRatio || 1;
      const rect = targetCanvas.getBoundingClientRect();
      const drawWidth = Math.max(120, Math.floor(rect.width || targetCanvas.clientWidth || 320));
      const drawHeight = Math.max(60, Math.floor(rect.height || targetCanvas.clientHeight || 120));
      const localFontSize = options.fontSize || fontSize;
      const localCellHeight = options.cellHeight || cellHeight;
      targetCanvas.width = Math.floor(drawWidth * scale);
      targetCanvas.height = Math.floor(drawHeight * scale);
      targetCtx.setTransform(scale, 0, 0, scale, 0, 0);
      targetCtx.font = localFontSize + 'px ' + fontFamily;
      targetCtx.textBaseline = 'top';
      const localCellWidth = Math.max(6, Math.floor(drawWidth / width));
      targetCtx.fillStyle = '#090b0f';
      targetCtx.fillRect(0, 0, drawWidth, drawHeight);

      for (let index = 0; index < frame.cells.length; index++) {
        const cell = frame.cells[index];
        const x = (index % width) * localCellWidth;
        const y = Math.floor(index / width) * localCellHeight;
        targetCtx.fillStyle = color(cell.bg, '#090b0f');
        targetCtx.fillRect(x, y, localCellWidth, localCellHeight);
        if (!cell.skip && cell.symbol && cell.symbol !== ' ') {
          targetCtx.fillStyle = color(cell.fg, '#d7dde8');
          targetCtx.fillText(cell.symbol, x, y + 1);
        }
      }

      if (frame.cursor && frame.cursor.visible) {
        targetCtx.strokeStyle = '#f3c969';
        targetCtx.lineWidth = 1;
        targetCtx.strokeRect(frame.cursor.x * localCellWidth + 0.5, frame.cursor.y * localCellHeight + 0.5, localCellWidth - 1, localCellHeight - 1);
      }
    }

    function livePaneStatus() {
      const count = Object.keys(waves).length;
      return count ? `${count} live pane${count === 1 ? '' : 's'}` : 'no panes';
    }

    function runtimeStatusForWave(wave) {
      if (document.body.classList.contains('pane-wall')) return livePaneStatus();
      if (!wave) return 'No pane watched';
      return `Watching ${wave.role === 'parent' ? 'Parent session' : wave.title}`;
    }

    function setRuntimeStatus(message, live = true) {
      streamStatus.textContent = message;
      liveDot.classList.toggle('live', live);
    }

    function terminalAttachmentLabel(wave) {
      if (!wave?.terminal) return 'no terminal attached';
      return wave.role === 'parent' ? 'parent terminal attached' : 'child terminal attached';
    }

	    function renderFocus(frame) {
	      drawFrame(canvas, frame);
	      setRuntimeStatus(runtimeStatusForWave(waves[selectedWaveId]));
	      terminalStatus.textContent = 'interactive';
    }

    function renderTile(id, frame) {
      latestFrames.set(id, frame);
      const tileCanvas = tileCanvases.get(id);
      if (tileCanvas) {
        drawFrame(tileCanvas, frame, { fontSize: 11, cellHeight: 14 });
      }
      const status = document.getElementById(`tile-status-${cssId(id)}`);
      if (status) status.textContent = `${frame.width || 1}x${frame.height || 1} live`;
    }

    function updateWallHud() {
      const title = document.getElementById('wallHudTitle');
      const meta = document.getElementById('wallHudMeta');
      const context = document.getElementById('wallHudContext');
      const packetNode = document.getElementById('wallHudPacket');
      const terminalNode = document.getElementById('wallHudTerminal');
      if (!title || !meta || !context || !packetNode || !terminalNode) return;
      const wave = waves[selectedWaveId];
      if (!wave) {
        title.textContent = 'No pane watched';
        meta.textContent = 'Choose a live pane to watch or intervene.';
        context.innerHTML = '<span class="wall-context-pill">select pane</span>';
        packetNode.textContent = 'packet n/a';
        terminalNode.textContent = 'no terminal attached';
        updateWallPulse();
        updateWallDag();
        return;
      }
      const packet = packetParts(wave);
      const missing = wave.role === 'parent' ? 0 : missingPacketFields(wave).length;
      const attention = attentionChip(wave);
      title.textContent = `Watching ${wave.title}`;
      meta.textContent = wave.role === 'parent'
        ? `parent lane watching ${childWaves().length} child pane${childWaves().length === 1 ? '' : 's'}`
        : `intervention lane - ${wave.mode} - ${wave.status} - ${attention.label}${missing ? ` - ${missing} packet field${missing === 1 ? '' : 's'} missing` : ''}`;
      context.innerHTML = renderWallContext(wave);
      packetNode.textContent = wave.role === 'parent'
        ? `mission parent - ${childWaves().length} child panes`
        : `packet ${packet.done}/${packet.required} - blast ${wave.blast}`;
      terminalNode.textContent = terminalAttachmentLabel(wave);
      updateWallPulse();
      updateWallDag();
      renderWallReadout(wave.id);
    }

    function renderWallContext(wave) {
      const packet = packetParts(wave);
      const gate = dependencyGateForWave(wave);
      const gateText = dependencyGateLabel(gate).replace(/^gate\s+/i, '');
      const attention = attentionChip(wave);
      const roleTone = wave.role === 'parent' ? 'good' : toneForStatus(wave.status);
      if (wave.role === 'parent') {
        return [
          `<span class="wall-context-pill good">parent session</span>`,
          `<span class="wall-context-pill">${childWaves().length} child pane${childWaves().length === 1 ? '' : 's'}</span>`,
          `<span class="wall-context-pill">mission broadcaster</span>`,
          `<span class="wall-context-pill">${escapeHtml(terminalAttachmentLabel(wave))}</span>`
        ].join('');
      }
      return [
        `<span class="wall-context-pill ${escapeHtml(roleTone)}">${escapeHtml(wave.mode)} / ${escapeHtml(wave.status)}</span>`,
        `<span class="wall-context-pill ${escapeHtml(packetTone(wave))}">packet ${packet.done}/${packet.required}</span>`,
        `<span class="wall-context-pill ${escapeHtml(dependencyGateTone(gate.status))}">gate ${gateText}</span>`,
        `<span class="wall-context-pill">blast ${wave.blast}</span>`,
        `<span class="wall-context-pill ${escapeHtml(attention.tone)}">${escapeHtml(attention.label)}</span>`
      ].join('');
    }

    function updateWallPulse(summary = null) {
      const container = document.getElementById('wallHudPulse');
      if (!container) return;
      const children = childWaves();
      const contracted = children.filter(wave => wave.pane.wave_contract);
      const ready = contracted.filter(wave => !missingPacketFields(wave).length);
      const attentionCount = attentionItems().length || children.filter(wave => {
        if (!wave.pane.wave_contract) return true;
        return wave.status.includes('blocked') || wave.status.includes('needs') || missingPacketFields(wave).length > 0;
      }).length;
      const failed = children.filter(wave => wave.status.includes('failed')).length;
      const values = {
        children: summary?.children ?? children.length,
        packets: `${summary?.ready_packets ?? ready.length}/${summary?.ingested ?? contracted.length}`,
        attention: summary?.needs_attention ?? attentionCount,
        failed: summary?.failed ?? failed
      };
      container.innerHTML = [
        `<span class="wall-pulse-item"><strong class="wall-pulse-value">${escapeHtml(values.children)}</strong><span class="wall-pulse-label">children</span></span>`,
        `<span class="wall-pulse-item"><strong class="wall-pulse-value">${escapeHtml(values.packets)}</strong><span class="wall-pulse-label">packets</span></span>`,
        `<span class="wall-pulse-item"><strong class="wall-pulse-value">${escapeHtml(values.attention)}</strong><span class="wall-pulse-label">decisions</span></span>`,
        `<span class="wall-pulse-item"><strong class="wall-pulse-value">${escapeHtml(values.failed)}</strong><span class="wall-pulse-label">failed</span></span>`
      ].join('');
    }

    function dependencyLabel(wave, index) {
      if (wave.role === 'parent') return 'parent';
      const dependency = String(wave.depends || 'none');
      if (!dependency || dependency === 'none') return 'parallel';
      if (/w\d+/i.test(dependency)) return dependency;
      if (dependency.length <= 12) return dependency;
      return `after ${index}`;
    }

    function updateWallDag() {
      const container = document.getElementById('wallHudDag');
      if (!container) return;
      const list = allPaneWaves();
      if (!list.length) {
        container.innerHTML = '';
        return;
      }
      container.innerHTML = list.map((wave, index) => {
        const label = wave.role === 'parent' ? 'P' : `C${index}`;
        const tone = wave.role === 'parent' ? 'good' : toneForStatus(wave.status);
        const dependency = dependencyLabel(wave, index);
        const packet = wave.role === 'parent' ? `${childWaves().length} child` : wave.packetLabel;
        const title = `${wave.title} - ${wave.role} - ${wave.status} - ${dependency} - ${paneIdentity(wave)}`;
        return `
          <button class="wall-dag-node ${escapeHtml(tone)} ${wave.id === selectedWaveId ? 'active' : ''}" data-wall-dag-wave="${escapeHtml(wave.id)}" title="${escapeHtml(title)}">
            <strong>${escapeHtml(label)} ${escapeHtml(wave.status)}</strong>
            <small>${escapeHtml(dependency)} / ${escapeHtml(packet)}</small>
          </button>
        `;
      }).join('');
    }

    function renderWallReadout(waveId = selectedWaveId) {
      const readout = document.getElementById('wallHudReadout');
      if (!readout) return;
      const output = outputSnapshots[waveId] || outputSnapshots[waves[waveId]?.paneId || ''];
      if (!output) {
        readout.textContent = 'No read snapshot yet.';
        return;
      }
      const tail = Array.isArray(output.tail_lines) ? output.tail_lines : [];
      const rows = tail.length ? tail.slice(-4) : [output.last_nonempty_line || output.text || 'No readable output.'];
      readout.textContent = [
        `${output.at || 'now'} - ${output.nonempty_line_count || 0} lines`,
        ...rows
      ].join('\n');
    }

    function connect() {
      if (focusSource) focusSource.close();
      resizeCanvas();
      const wave = waves[selectedWaveId];
      if (!wave || !wave.terminal) {
        setRuntimeStatus('No pane watched', false);
        terminalStatus.textContent = 'no pane watched';
        return;
      }
      focusSource = new EventSource(`/terminal/events?terminal_id=${encodeURIComponent(wave.terminal)}&cols=${currentCols}&rows=${currentRows}`);
      focusSource.addEventListener('open', () => {
        setRuntimeStatus(runtimeStatusForWave(wave));
      });
      focusSource.addEventListener('frame', event => {
        const frame = JSON.parse(event.data);
        latestFrames.set(wave.id, frame);
        renderFocus(frame);
      });
      focusSource.addEventListener('notify', event => {
        const data = JSON.parse(event.data);
        setRuntimeStatus(data.message || 'notification');
      });
      focusSource.addEventListener('terminal-error', event => {
        const data = JSON.parse(event.data);
        setRuntimeStatus(data.message || 'terminal attach failed', false);
        terminalStatus.textContent = 'offline';
      });
      focusSource.addEventListener('shutdown', event => {
        let reason = 'terminal stream shut down';
        try {
          const data = JSON.parse(event.data);
          reason = data.reason || reason;
        } catch (_) {}
        setRuntimeStatus(reason, false);
        terminalStatus.textContent = 'offline';
        focusSource.close();
      });
      focusSource.addEventListener('error', () => {
        setRuntimeStatus('Pane stream disconnected', false);
        terminalStatus.textContent = 'offline';
      });
    }

    function connectTileStream(wave, attempt = 0) {
      const existing = tileSources.get(wave.id);
      if (existing) existing.close();
      tileSources.delete(wave.id);
	      if (!wave.terminal) {
	        const frame = latestFrames.get(wave.id);
	        if (frame) renderTile(wave.id, frame);
	        const status = document.getElementById(`tile-status-${cssId(wave.id)}`);
	        if (status) status.textContent = 'no terminal';
	        return;
      }
      const status = document.getElementById(`tile-status-${cssId(wave.id)}`);
      if (status) status.textContent = 'connecting';
      const tileCanvas = tileCanvases.get(wave.id);
      const rect = tileCanvas?.getBoundingClientRect();
      const cols = Math.max(40, Math.min(140, Math.floor((rect?.width || 560) / 8)));
      const rows = Math.max(8, Math.min(48, Math.floor((rect?.height || 240) / 14)));
      const stream = new EventSource(`/terminal/events?terminal_id=${encodeURIComponent(wave.terminal)}&cols=${cols || tileCols}&rows=${rows || tileRows}`);
      tileSources.set(wave.id, stream);
      stream.addEventListener('frame', event => renderTile(wave.id, JSON.parse(event.data)));
      stream.addEventListener('open', () => {
        const status = document.getElementById(`tile-status-${cssId(wave.id)}`);
        if (status) status.textContent = 'live';
        if (!document.body.classList.contains('terminal-expanded')) {
          setRuntimeStatus(livePaneStatus());
        }
      });
      stream.addEventListener('shutdown', event => {
        let reason = 'detached';
        try {
          const data = JSON.parse(event.data);
          reason = data.reason || reason;
        } catch (_) {}
        const status = document.getElementById(`tile-status-${cssId(wave.id)}`);
        stream.close();
        tileSources.delete(wave.id);
        if (reason.includes('terminal attach failed') && attempt < 4) {
          if (status) status.textContent = 'reattaching';
          setTimeout(() => {
            connectTileStream(wave, attempt + 1);
          }, 350 * (attempt + 1));
          return;
        }
        if (status) status.textContent = reason;
      });
      stream.addEventListener('error', () => {
        const status = document.getElementById(`tile-status-${cssId(wave.id)}`);
        stream.close();
        tileSources.delete(wave.id);
        if (attempt < 4) {
          if (status) status.textContent = 'reattaching';
          setTimeout(() => {
            connectTileStream(wave, attempt + 1);
          }, 350 * (attempt + 1));
        } else if (status) {
          status.textContent = 'stream unavailable';
        }
      });
    }

    function shouldUseTileStreams() {
      return document.body.classList.contains('pane-wall');
    }

    function reconnectStreamsForSelection(previousId = null) {
      if (shouldUseTileStreams() && previousId && waves[previousId] && !tileSources.has(previousId)) {
        setTimeout(() => connectTileStream(waves[previousId]), 120);
      }
      if (document.body.classList.contains('terminal-expanded') || !shouldUseTileStreams()) {
        connect();
      } else if (focusSource) {
        focusSource.close();
        focusSource = null;
      }
      if (shouldUseTileStreams()) {
        Object.values(waves).forEach(wave => {
          if (!tileSources.has(wave.id)) connectTileStream(wave);
        });
      } else {
        tileSources.forEach(stream => stream.close());
        tileSources.clear();
        document.querySelectorAll('.tile-status').forEach(node => {
          node.textContent = 'focus mode';
        });
      }
    }

    function pauseTerminalStreams(reason = 'parent action') {
      if (focusSource) {
        focusSource.close();
        focusSource = null;
      }
      tileSources.forEach(stream => stream.close());
      tileSources.clear();
      document.querySelectorAll('.tile-status').forEach(node => {
        node.textContent = reason;
      });
      setRuntimeStatus(reason, false);
    }

    function keyPayload(event) {
      if (event.defaultPrevented || event.isComposing) return null;
      if (event.metaKey || event.altKey) return null;
      if (event.ctrlKey && event.key.length === 1) {
        const upper = event.key.toUpperCase();
        if (upper >= 'A' && upper <= 'Z') {
          return String.fromCharCode(upper.charCodeAt(0) - 64);
        }
      }
      const special = {
        Enter: '\r',
        Backspace: '\x7f',
        Tab: '\t',
        Escape: '\x1b',
        ArrowUp: '\x1b[A',
        ArrowDown: '\x1b[B',
        ArrowRight: '\x1b[C',
        ArrowLeft: '\x1b[D',
        Home: '\x1b[H',
        End: '\x1b[F',
        PageUp: '\x1b[5~',
        PageDown: '\x1b[6~',
        Delete: '\x1b[3~'
      };
      if (special[event.key]) return special[event.key];
      if (event.key.length === 1 && !event.ctrlKey) return event.key;
      return null;
    }

	    function sendInput(data) {
	      if (!data) return;
	      const wave = waves[selectedWaveId];
	      if (!wave || !wave.paneId) {
	        terminalStatus.textContent = 'no pane watched';
	        return;
	      }
	      sendTextToPane(wave.paneId, data).catch(() => {
	        terminalStatus.textContent = 'input failed';
	      });
	    }

	    async function sendTextToPane(paneId, data, options = {}) {
	      const params = new URLSearchParams({
	        pane_id: paneId,
	        data
	      });
	      if (options.delivery) params.set('delivery', options.delivery);
	      const url = `/pane/input?${params.toString()}`;
	      const response = await fetch(url, { cache: 'no-store' });
	      if (!response.ok) throw new Error(`pane ${paneId} rejected input`);
	      return response;
	    }

		    function commandBoxForSource(source = 'drawer') {
		      if (source === 'deck') return document.getElementById('deckCommand');
		      if (source === 'wall') return document.getElementById('wallHudCommand');
		      return document.getElementById('parentCommand');
		    }

		    function commandScopeForSource(source = 'drawer') {
		      if (source === 'deck') return document.getElementById('deckScope')?.value || 'all';
		      if (source === 'wall') return document.getElementById('wallHudScope')?.value || 'all';
		      return 'all';
		    }

		    function setCommandStatus(source, message) {
		      if (source === 'deck') {
		        const deckStatus = document.getElementById('deckStatus');
		        if (deckStatus) deckStatus.textContent = message;
		      } else if (source === 'wall') {
		        const wallStatus = document.getElementById('wallHudStatus');
		        if (wallStatus) wallStatus.textContent = message;
		      }
		    }

		    function commandPayload(source = 'drawer') {
		      const box = commandBoxForSource(source);
		      const text = box.value;
		      if (!text.trim()) return '';
		      return text.replaceAll('\n', '\r') + (text.endsWith('\n') ? '' : '\r');
		    }

	    function contractPrompt(wave) {
	      if (!wave) return '';
	      const project = document.getElementById('parentTitle').textContent || 'Herdr mission';
	      const reportDone = donePacketFields(wave);
	      const reportMissing = missingPacketFields(wave);
	      const arcs = wave.arcList.length
	        ? wave.arcList.map(arc => `- ${arc.id || 'arc'}: ${cleanContractText(arc.summary || 'no summary')}`).join('\n')
	        : '- no arcs declared';
	      return [
	        `Parent mission: ${project}`,
	        `Child pane: ${wave.title}`,
	        `Pane id: ${wave.paneId}`,
	        `Terminal id: ${wave.terminal}`,
	        `Mode: ${wave.mode}`,
	        `Status: ${wave.status}`,
	        `Depends: ${wave.depends}`,
	        `Blast radius: ${wave.blast}`,
	        '',
	        'Contract arcs:',
	        arcs,
	        '',
	        `Report packet: ${wave.packetLabel}`,
	        `Complete: ${reportDone.length ? reportDone.join(', ') : 'none'}`,
	        `Missing: ${reportMissing.length ? reportMissing.join(', ') : 'none'}`,
	        '',
	        'Please work within this contract, report evidence/receipts, and call out blockers before changing scope.'
	      ].join('\n');
	    }

    function missingPacketPrompt(wave) {
	      if (!wave) return '';
	      const project = document.getElementById('parentTitle').textContent || 'Herdr mission';
	      const missing = missingPacketFields(wave);
	      const done = donePacketFields(wave);
	      const gitEntries = gitEntriesForWave(wave).filter(entry => !localOnlyPath(entry.path));
	      const gitRows = gitEntries.length
	        ? gitEntries.map(entry => `- ${gitCodeLabel(entry)}: ${entry.path}`).join('\n')
	        : '- no git-visible source files from this pane cwd yet';
	      return [
	        `Parent mission: ${project}`,
	        `Child pane: ${wave.title}`,
	        `Pane id: ${wave.paneId}`,
	        `Terminal id: ${wave.terminal}`,
	        `Current packet: ${wave.packetLabel}`,
	        '',
	        `Already complete: ${done.length ? done.join(', ') : 'none'}`,
	        `Missing fields: ${missing.length ? missing.join(', ') : 'none'}`,
	        '',
	        'Git-visible source files from this pane workspace:',
	        gitRows,
	        '',
	        'Please send a structured report packet that fills the missing fields above. Include exact commands, evidence/receipts, files read, files changed, risks/unknowns, good/bad/ugly, recommendation, and next wave suggestion. If a field is not applicable, say why instead of leaving it silent.'
	      ].join('\n');
	    }

	    function promptDeliveryForWave(wave) {
	      const delivery = wave?.promptDelivery || wave?.pane?.wave_contract?.prompt_delivery || '';
	      if (delivery === 'agent') return '';
	      if (delivery === 'shell_card') return 'shell_card';
	      if (!wave?.pane?.agent && (!wave?.pane?.agent_status || wave.pane.agent_status === 'unknown')) {
	        return 'shell_card';
	      }
	      return '';
	    }

	    function loadContractPrompt() {
	      const wave = waves[selectedWaveId];
	      if (!wave) {
	        document.getElementById('controlTarget').textContent = 'no pane selected';
	        return;
	      }
	      const box = document.getElementById('parentCommand');
	      box.value = contractPrompt(wave);
	      box.focus();
	      document.getElementById('controlTarget').textContent = `loaded contract for ${wave.paneId}`;
	    }

	    function loadMissingPacketPrompt(destination = 'drawer') {
	      const wave = waves[selectedWaveId];
	      if (!wave) {
	        document.getElementById('controlTarget').textContent = 'no pane selected';
	        return;
	      }
		      const box = commandBoxForSource(destination);
		      box.value = missingPacketPrompt(wave);
		      box.focus();
		      setCommandStatus(destination, `Missing-packet request staged for ${wave.title}.`);
		      document.getElementById('controlTarget').textContent = `loaded missing packet request for ${wave.paneId}`;
		    }

		    function messagePanePrompt(wave) {
		      const project = document.getElementById('parentTitle').textContent || 'Herdr mission';
		      const target = wave.role === 'parent' ? 'parent pane' : 'child pane';
	      return [
	        `Parent mission: ${project}`,
	        `Message target: ${wave.title}`,
	        `Target pane: ${wave.paneId}`,
	        `Terminal: ${wave.terminal}`,
	        `Role: ${target}`,
	        `Mode: ${wave.mode}`,
	        `Current packet: ${wave.packetLabel}`,
	        '',
	        'Parent message:',
		        ''
		      ].join('\n');
		    }

		    function wallPresetPrompt(kind, wave) {
		      if (!wave) return '';
		      if (kind === 'packet') return missingPacketPrompt(wave);
		      const project = document.getElementById('parentTitle').textContent || 'Herdr mission';
		      const header = [
		        `Parent mission: ${project}`,
		        `Target pane: ${wave.title}`,
		        `Pane id: ${wave.paneId}`,
		        `Terminal id: ${wave.terminal}`,
		        `Role: ${wave.role}`,
		        `Mode: ${wave.mode}`,
		        ''
		      ];
		      if (kind === 'freeze') {
		        return header.concat([
		          'Parent instruction: freeze edits now.',
		          '',
		          'Stop write actions, installs, cleanup, deploys, and broad file changes. Do not expand scope. Reply with:',
		          '1. current state',
		          '2. files touched or planned',
		          '3. safest next action',
		          '4. what permission you need from the parent'
		        ]).join('\n');
		      }
		      if (kind === 'finish') {
		        return header.concat([
		          'Parent instruction: prepare a finish handoff.',
		          '',
		          'If this pane is done, return a concise report packet with evidence/receipts, files read, files changed, commands run, risks, good/bad/ugly, recommendation, and next wave suggestion.',
		          'If this pane is not done, say exactly what remains and whether the parent should continue, split, or stop this pane.'
		        ]).join('\n');
		      }
		      return header.concat([
		        'Parent check-in: send a short status ping.',
		        '',
		        'Reply with:',
		        '1. what you are doing right now',
		        '2. last useful evidence or receipt',
		        '3. blocker, if any',
		        '4. changed files, if any',
		        '5. recommended next parent action'
		      ]).join('\n');
		    }

		    function loadWallPreset(kind) {
		      const wave = waves[selectedWaveId];
		      if (!wave) {
		        setCommandStatus('wall', 'Select a pane before loading a quick prompt.');
		        return;
		      }
		      const box = commandBoxForSource('wall');
		      box.value = wallPresetPrompt(kind, wave);
		      box.focus();
		      box.setSelectionRange(box.value.length, box.value.length);
		      const labels = {
		        status: 'Status ping',
		        packet: 'Report packet',
		        freeze: 'Freeze-edits instruction',
		        finish: 'Finish handoff'
		      };
		      setCommandStatus('wall', `${labels[kind] || 'Quick prompt'} staged for ${wave.title}.`);
		    }

		    function loadPaneMessagePrompt(waveId = selectedWaveId, destination = 'drawer') {
		      if (waveId && waves[waveId]) selectWave(waveId);
		      const wave = waves[selectedWaveId];
	      if (!wave) {
	        document.getElementById('controlTarget').textContent = 'no pane selected';
	        return;
	      }
		      const box = commandBoxForSource(destination);
		      box.value = messagePanePrompt(wave);
		      box.focus();
		      box.setSelectionRange(box.value.length, box.value.length);
		      setCommandStatus(destination, `Message staged for ${wave.title}.`);
		      document.getElementById('controlTarget').textContent = `message ready for ${wave.paneId}`;
		    }

		    function fileReceiptPrompt() {
		      const project = document.getElementById('parentTitle').textContent || 'Herdr mission';
	      const gitEntries = allGitEntries();
	      const sourceFiles = gitEntries.filter(entry => !localOnlyPath(entry.path));
	      const localFiles = gitEntries.filter(entry => localOnlyPath(entry.path));
	      const branches = [...new Set(Object.values(gitStatuses).map(status => status.branch).filter(Boolean))];
	      const sourceRows = sourceFiles.length
	        ? sourceFiles.map(entry => `- ${gitCodeLabel(entry)}: ${entry.path}${entry.old_path ? ` (from ${entry.old_path})` : ''}`).join('\n')
	        : '- none';
	      const localRows = localFiles.length
	        ? localFiles.map(entry => `- ${gitCodeLabel(entry)}: ${entry.path}`).join('\n')
	        : '- none';
	      return [
	        `Parent mission: ${project}`,
	        `Repository branch: ${branches.join(', ') || 'unknown'}`,
	        `Changed source files: ${sourceFiles.length}`,
	        `Local/session artifacts: ${localFiles.length}`,
	        '',
	        'Source file receipt:',
	        sourceRows,
	        '',
	        'Local/session artifact receipt:',
	        localRows,
	        '',
	        'Attribution note: these files are visible in the shared checkout. Please state which changes you own, which are inherited from another child, and whether the Files changed report-packet field can be marked complete.'
	      ].join('\n');
	    }

		    function loadFileReceiptPrompt(destination = 'drawer') {
			      const box = commandBoxForSource(destination);
			      box.value = fileReceiptPrompt();
			      box.focus();
			      setCommandStatus(destination, 'File receipt request staged.');
			      document.getElementById('controlTarget').textContent = 'loaded file receipt';
			    }

			    function missionImportActionMeta(action) {
			      if (action === 'reuse_existing') return { label: 'reuse pane', tone: 'reuse' };
			      if (action === 'create_new') return { label: 'create child pane', tone: 'create' };
			      return { label: 'missing pane', tone: 'missing' };
			    }

			    function missionImportPaneIdentity(item) {
			      const pane = item.pane_id || 'new child pane';
			      const terminal = item.terminal_id || (item.pane_id ? 'terminal after refresh' : 'new terminal');
			      return `${pane} / ${terminal}`;
			    }

			    function renderMissionLaunchPlan(result = {}, preview = false) {
			      const plan = document.getElementById('missionLaunchPlan');
			      if (!plan) return;
			      const assignments = Array.isArray(result.assignments) ? result.assignments : [];
			      if (!assignments.length) {
			        plan.innerHTML = '';
			        return;
			      }
			      plan.innerHTML = assignments.map((item, index) => {
			        const action = missionImportActionMeta(item.planned_action || '');
			        const title = item.contract_title || `Wave ${index + 1}`;
			        const gateHeld = item.gate_status && item.gate_status !== 'ready';
			        const gateLabel = gateHeld ? dependencyGateLabel(item) : '';
			        const status = preview
			          ? (gateHeld ? `planned, ${gateLabel}` : 'planned')
			          : (item.prompt_sent ? 'prompt sent' : (gateHeld ? `held: ${gateLabel}` : item.status || 'applied'));
			        return `
			          <div class="launch-plan-row">
			            <strong>${escapeHtml(title)}</strong>
			            <span class="launch-plan-action ${escapeHtml(action.tone)}">${escapeHtml(action.label)}</span>
			            <span title="${escapeHtml(missionImportPaneIdentity(item))}">${escapeHtml(missionImportPaneIdentity(item))} - ${escapeHtml(status)}</span>
			          </div>
			        `;
			      }).join('');
			    }

			    function setMissionImportStatus(message, source = 'drawer') {
			      document.getElementById('controlTarget').textContent = message;
			      const launchStatus = document.getElementById('missionLaunchStatus');
		      if (launchStatus) launchStatus.textContent = message;
		      if (source === 'deck') {
		        const deckStatus = document.getElementById('deckStatus');
		        if (deckStatus) deckStatus.textContent = message;
		      }
		    }

		    async function importMissionSession(source = 'drawer', options = {}) {
		      if (typeof source !== 'string') source = 'drawer';
		      const preview = Boolean(options.preview);
		      const input = document.getElementById(source === 'deck' ? 'deckSessionPath' : 'sessionPath');
		      const fallbackInput = document.getElementById(source === 'deck' ? 'sessionPath' : 'deckSessionPath');
		      const path = input.value.trim();
		      if (!path) {
		        setMissionImportStatus('missing session file path', source);
		        input.focus();
		        return;
		      }
		      if (fallbackInput && !fallbackInput.value.trim()) fallbackInput.value = path;
			      setMissionImportStatus(preview ? 'previewing pane launch' : 'launching child panes', source);
			      pauseTerminalStreams(preview ? 'previewing pane launch' : 'launching child panes');
		      try {
		        const parent = parentWave();
		        const argvInput = document.getElementById(source === 'deck' ? 'deckAgentArgv' : 'agentArgv');
		        const fallbackArgvInput = document.getElementById(source === 'deck' ? 'agentArgv' : 'deckAgentArgv');
		        const argv = splitArgv(argvInput?.value || fallbackArgvInput?.value || '');
			        const params = new URLSearchParams({
			          path,
			          apply: preview ? 'false' : 'true',
			          create_missing: 'true',
			          gate_dependencies: 'true',
			          prompt_scope: 'created'
			        });
		        if (parent?.paneId) params.set('target_pane_id', parent.paneId);
		        if (argv.length) params.set('argv', JSON.stringify(argv));
		        const response = await fetch(`/mission/import?${params.toString()}`, { cache: 'no-store' });
		        const payload = await response.json();
		        if (!response.ok || payload.error) throw new Error(payload.error?.message || 'mission import failed');
		        const result = payload.result || {};
		        missionImportEvents.unshift({
		          at: new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }),
		          path: result.path || path,
		          contracts: Array.isArray(result.contracts) ? result.contracts.length : 0,
		          applied: Number(result.applied || 0),
		          created: Number(result.created || 0),
		          missing: Number(result.missing_panes || 0),
		          plannedCreated: Number(result.planned_created || 0),
		          plannedReused: Number(result.planned_reused || 0),
		          plannedPrompted: Number(result.planned_prompted || 0),
		          gated: Boolean(result.gate_dependencies),
			          preview,
			          assignments: result.assignments || []
			        });
		        missionImportEvents = missionImportEvents.slice(0, 6);
		        renderMissionLaunchPlan(result, preview);
		        const contractCount = Array.isArray(result.contracts) ? result.contracts.length : 0;
		        const held = Array.isArray(result.assignments)
		          ? result.assignments.filter(item => item.gate_status && item.gate_status !== 'ready').length
		          : 0;
		        const importedMessage = preview
		          ? `preview: create ${result.planned_created || 0}, reuse ${result.planned_reused || 0}, prompt ${result.planned_prompted || 0}`
		          : `launched ${result.applied || 0}/${contractCount}; created ${result.created || 0}; reused ${result.planned_reused || 0}; held ${held}`;
		        setMissionImportStatus(importedMessage, source);
		        if (!preview) await loadPanes(selectedWaveId);
		        if (preview) setTimeout(() => reconnectStreamsForSelection(), 120);
		        renderDerivedBoards();
		        if (!preview && options.openWall) setPaneWall(true);
		      } catch (error) {
		        setMissionImportStatus(error.message || 'mission import failed', source);
		        setTimeout(() => reconnectStreamsForSelection(), 120);
		      }
		    }

	    async function sweepMissionChildren(options = {}) {
	      pauseTerminalStreams('sweeping');
	      const ingest = options.ingest === false ? 'false' : 'true';
	      const lines = options.ingest === false ? '80' : '500';
	      const response = await fetch(`/mission/sweep?ingest=${ingest}&lines=${lines}`, { cache: 'no-store' });
	      const payload = await response.json();
	      if (!response.ok || payload.error) throw new Error(payload.error?.message || 'mission sweep failed');
	      const result = payload.result || {};
	      lastMissionSweep = result;
	      const now = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
	      (result.panes || []).forEach(pane => {
	        if (!pane.output) return;
	        outputSnapshots[pane.pane_id] = {
	          ...pane.output,
	          title: pane.title || pane.pane_id,
	          at: now
	        };
	        outputEvents.unshift(outputSnapshots[pane.pane_id]);
	      });
	      outputEvents = outputEvents.slice(0, 12);
	      const summary = result.summary || { children: 0, read: 0, ingested: 0, ready_packets: 0, needs_attention: 0, failed: 0 };
	      recordEvidenceReceipt({
	        kind: 'sweep',
	        title: 'Mission sweep',
	        paneId: parentPaneId || '',
	        detail: `${summary.children || 0} children; ${summary.read || 0} read; packets ${summary.ready_packets || 0}/${summary.ingested || 0}; attention ${summary.needs_attention || 0}; failed ${summary.failed || 0}`,
	        payload: summary
	      });
	      return summary;
	    }

	    async function refreshEvidence(options = {}) {
	      const list = Object.values(waves);
	      if (!list.length) return;
	      if (!options.quiet) document.getElementById('controlTarget').textContent = 'sweeping child panes';
	      if (options.source === 'mission') setMissionSweepStatus('Refreshing mission sweep...');
	      if (options.source === 'review') setReviewSweepStatus('Refreshing mission sweep...');
	      try {
	        const summary = await sweepMissionChildren({ ingest: options.ingest });
	        await loadPanes(selectedWaveId);
	        updateWallPulse(summary);
	        if (options.source === 'mission') {
	          const now = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
	          setMissionSweepStatus(`Last sweep ${now}: ${summary.ready_packets || 0}/${summary.ingested || 0} ready, ${summary.needs_attention || 0} sweep blocker${Number(summary.needs_attention || 0) === 1 ? '' : 's'}.`);
	        }
	        if (options.source === 'review') {
	          const now = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
	          setReviewSweepStatus(`Last sweep ${now}: ${summary.ready_packets || 0}/${summary.ingested || 0} ready, ${summary.needs_attention || 0} sweep blocker${Number(summary.needs_attention || 0) === 1 ? '' : 's'}.`);
	        }
	        if (!options.quiet) {
	          document.getElementById('controlTarget').textContent =
	            `swept ${summary.children || 0}; read ${summary.read || 0}; packets ${summary.ready_packets || 0}/${summary.ingested || 0}; sweep blockers ${summary.needs_attention || 0}`;
	          setCommandStatus('wall', `Swept ${summary.children || 0} child pane${Number(summary.children || 0) === 1 ? '' : 's'}; read ${summary.read || 0}; packets ${summary.ready_packets || 0}/${summary.ingested || 0}; sweep blockers ${summary.needs_attention || 0}.`);
	        }
	        return;
	      } catch (error) {
	        if (options.source === 'mission') setMissionSweepStatus(error.message || 'mission sweep failed');
	        if (options.source === 'review') setReviewSweepStatus(error.message || 'mission sweep failed');
	        if (!options.quiet) {
	          document.getElementById('controlTarget').textContent = error.message || 'mission sweep failed';
	          setCommandStatus('wall', error.message || 'mission sweep failed');
	        }
	      }
	      await loadGitStatuses(list);
	      renderMissionPulse();
	      renderPaneRoster();
	      renderAttentionInbox();
	      renderPacketReviewQueue();
	      renderChangeRadar();
	      updateDeckScopeSummary();
	      renderDerivedBoards();
	      bindWaveInteractions();
	      const sourceCount = allGitEntries().filter(entry => !localOnlyPath(entry.path)).length;
	      if (!options.quiet) document.getElementById('controlTarget').textContent = `${sourceCount} source file${sourceCount === 1 ? '' : 's'} visible`;
	    }

	    function setMissionSweepStatus(message) {
	      const status = document.getElementById('missionSweepStatus');
	      if (status) status.textContent = message;
	    }

	    function setReviewSweepStatus(message) {
	      const status = document.getElementById('reviewSweepStatus');
	      if (status) status.textContent = message;
	    }

	    async function refreshMissionRoom(options = {}) {
	      if (missionRefreshInFlight) return;
	      missionRefreshInFlight = true;
	      try {
	        await refreshEvidence({ quiet: true, ingest: true, source: 'mission', ...options });
	      } finally {
	        missionRefreshInFlight = false;
	      }
	    }

	    async function refreshReviewRoom(options = {}) {
	      if (reviewRefreshInFlight) return;
	      reviewRefreshInFlight = true;
	      try {
	        await refreshEvidence({ quiet: true, ingest: true, source: 'review', ...options });
	      } finally {
	        reviewRefreshInFlight = false;
	      }
	    }

		    async function sendParentCommand(target, source = 'drawer') {
		      const data = commandPayload(source);
		      if (!data) {
		        document.getElementById('controlTarget').textContent = 'empty command';
		        setCommandStatus(source, 'Type a command before sending.');
		        return;
		      }
		      const scopedSource = source === 'deck' || source === 'wall';
		      const scope = scopedSource ? commandScopeForSource(source) : 'all';
		      const list = target === 'all'
		        ? (scopedSource ? targetWavesForScope(scope) : childWaves())
		        : [waves[selectedWaveId]].filter(Boolean);
		      const targetLabel = target === 'all' ? scopeLabel(scope) : list[0]?.title || 'watched pane';
		      if (!list.length) {
		        document.getElementById('controlTarget').textContent = 'no pane target';
		        setCommandStatus(source, `No ${scopeLabel(scope)} targets available.`);
		        return;
		      }
		      document.getElementById('controlTarget').textContent =
		        target === 'all' ? `sending to ${list.length} ${scopeLabel(scope)} panes` : `sending to ${list[0].paneId}`;
		      setCommandStatus(
		        source,
		        target === 'all' ? `Sending to ${list.length} ${scopeLabel(scope)} pane${list.length === 1 ? '' : 's'}...` : `Sending to ${list[0].title}...`
		      );
		      pauseTerminalStreams('sending');
		      try {
	        const summary = await dispatchTextToWaves(targetLabel, list, data);
	        recordDispatch(summary);
	        document.getElementById('controlTarget').textContent =
	          summary.failed
	            ? `sent ${summary.sent}/${summary.requested}; ${summary.failed} failed`
	            : `sent ${summary.sent}/${summary.requested}`;
		        setCommandStatus(
		          source,
		          summary.failed
		            ? `Sent ${summary.sent}/${summary.requested}; ${summary.failed} failed.`
		            : `Sent ${summary.sent}/${summary.requested} to ${summary.target}.`
		        );
		        updateDeckScopeSummary();
		      } catch (error) {
		        document.getElementById('controlTarget').textContent = error.message || 'send failed';
		        setCommandStatus(source, error.message || 'send failed');
		      } finally {
		        setTimeout(() => reconnectStreamsForSelection(), 120);
		      }
		    }

		    async function dispatchTextToWaves(targetLabel, targetWaves, data) {
		      const groups = new Map();
		      targetWaves.forEach(wave => {
		        const delivery = promptDeliveryForWave(wave) || 'agent';
		        const group = groups.get(delivery) || [];
		        group.push(wave);
		        groups.set(delivery, group);
		      });
		      const summaries = [];
		      for (const [delivery, group] of groups.entries()) {
		        summaries.push(await dispatchTextToPanes(
		          targetLabel,
		          group.map(wave => wave.paneId),
		          data,
		          { delivery: delivery === 'shell_card' ? 'shell_card' : '' }
		        ));
		      }
		      return combineDispatchSummaries(targetLabel, summaries);
		    }

		    function combineDispatchSummaries(targetLabel, summaries) {
		      const receipts = summaries.flatMap(summary => Array.isArray(summary.receipts) ? summary.receipts : []);
		      const requested = summaries.reduce((total, summary) => total + Number(summary.requested || 0), 0);
		      const sent = summaries.reduce((total, summary) => total + Number(summary.sent || 0), 0);
		      const failed = summaries.reduce((total, summary) => total + Number(summary.failed || 0), 0);
		      return { target: targetLabel, requested, sent, failed, receipts };
		    }

		    async function dispatchTextToPanes(targetLabel, paneIds, data, options = {}) {
		      const params = new URLSearchParams({
		        target: targetLabel,
		        pane_ids: JSON.stringify(paneIds),
		        data
		      });
		      if (options.delivery) params.set('delivery', options.delivery);
		      const response = await fetch(`/pane/dispatch?${params.toString()}`, { cache: 'no-store' });
		      const payload = await response.json();
		      if (!response.ok || payload.error) throw new Error(payload.error?.message || 'dispatch failed');
		      return payload.result?.dispatch || { target: targetLabel, requested: paneIds.length, sent: 0, failed: paneIds.length, receipts: [] };
		    }

		    async function loadDispatches() {
		      const response = await fetch('/dispatches', { cache: 'no-store' });
		      const payload = await response.json();
		      if (!response.ok || payload.error) throw new Error(payload.error?.message || 'dispatch ledger failed');
		      dispatchEvents = Array.isArray(payload.result?.dispatches) ? payload.result.dispatches : [];
		      renderDispatchReceipts();
		    }

		    async function requestMissingPackets(source = 'deck') {
		      const targets = targetWavesForScope('missing_packet');
		      if (!targets.length) {
		        document.getElementById('controlTarget').textContent = 'no missing packets';
		        setCommandStatus(source, 'No child panes have missing report packet fields.');
		        return;
		      }
		      document.getElementById('controlTarget').textContent = `requesting packets from ${targets.length} child pane${targets.length === 1 ? '' : 's'}`;
		      setCommandStatus(source, `Requesting report packets from ${targets.length} child pane${targets.length === 1 ? '' : 's'}...`);
		      pauseTerminalStreams('requesting packets');
		      const receipts = [];
		      for (const wave of targets) {
		        try {
		          const delivery = promptDeliveryForWave(wave);
		          const rawPrompt = missingPacketPrompt(wave);
		          const prompt = delivery === 'shell_card'
		            ? rawPrompt
		            : rawPrompt.replaceAll('\n', '\r') + '\r';
		          await sendTextToPane(wave.paneId, prompt, { delivery });
		          receipts.push({ pane_id: wave.paneId, ok: true });
		        } catch (error) {
		          receipts.push({ pane_id: wave.paneId, ok: false, error: error.message || 'request failed' });
		        }
		      }
		      const sent = receipts.filter(receipt => receipt.ok).length;
		      const summary = {
		        target: 'missing packet request',
		        requested: targets.length,
		        sent,
		        failed: targets.length - sent,
		        receipts
		      };
		      recordDispatch(summary);
		      document.getElementById('controlTarget').textContent =
		        summary.failed
		          ? `requested ${summary.sent}/${summary.requested}; ${summary.failed} failed`
		          : `requested ${summary.sent}/${summary.requested} missing packets`;
		      setCommandStatus(
		        source,
		        summary.failed
		          ? `Requested ${summary.sent}/${summary.requested} packet${summary.requested === 1 ? '' : 's'}; ${summary.failed} failed.`
		          : `Requested ${summary.sent}/${summary.requested} missing packet${summary.requested === 1 ? '' : 's'}.`
		      );
		      updateDeckScopeSummary();
		      setTimeout(() => reconnectStreamsForSelection(), 120);
		    }

		    async function unlockReadyWaves(source = 'deck') {
		      document.getElementById('controlTarget').textContent = 'unlocking ready child panes';
		      setCommandStatus(source, 'Checking dependency gates for queued child panes...');
		      pauseTerminalStreams('unlocking ready waves');
		      try {
		        const response = await fetch('/mission/unlock?apply=true', { cache: 'no-store' });
		        const payload = await response.json();
		        if (!response.ok || payload.error) throw new Error(payload.error?.message || 'mission unlock failed');
		        const result = payload.result || {};
		        const candidates = Array.isArray(result.candidates) ? result.candidates : [];
		        const results = Array.isArray(result.results) ? result.results : [];
		        const unlocked = Number(result.unlocked || 0);
		        const failed = results.filter(item => item.error).length;
		        document.getElementById('controlTarget').textContent = `unlocked ${unlocked}/${candidates.length} ready child panes`;
		        setCommandStatus(
		          source,
		          candidates.length
		            ? `Unlocked ${unlocked}/${candidates.length} ready queued child pane${candidates.length === 1 ? '' : 's'}${failed ? `; ${failed} failed` : ''}.`
		            : 'No queued child panes are ready to unlock yet.'
		        );
		        if (results.length) {
		          recordDispatch({
		            target: 'dependency unlock',
		            requested: candidates.length,
		            sent: unlocked,
		            failed,
		            receipts: results.map(item => ({
		              pane_id: item.pane_id,
		              ok: Boolean(item.prompt_sent) && !item.error,
		              error: item.error || null
		            }))
		          });
		        }
		        await refreshEvidence({ quiet: true, ingest: false });
		      } catch (error) {
		        document.getElementById('controlTarget').textContent = error.message || 'mission unlock failed';
		        setCommandStatus(source, error.message || 'mission unlock failed');
		        setTimeout(() => reconnectStreamsForSelection(), 120);
		      }
		    }

		    function recordDispatch(summary) {
		      dispatchEvents.unshift({
		        at: summary.at || new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }),
		        ...summary
		      });
		      dispatchEvents = dispatchEvents.slice(0, 8);
		      renderWaveGrid();
		      renderDispatchReceipts();
		      renderPaneRoster();
		      renderAttentionInbox();
		      renderPacketReviewQueue();
		      renderChangeRadar();
		      renderDerivedBoards();
	      bindWaveInteractions();
	    }

		    function dispatchEventTime(value) {
		      const raw = String(value || '').trim();
		      if (/^\d+$/.test(raw)) {
		        const date = new Date(Number(raw) * 1000);
		        if (!Number.isNaN(date.getTime())) {
		          return date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
		        }
		      }
		      return raw || 'now';
		    }

		    function hydrateEvidenceLedger() {
		      const stored = readJsonPreference(viewPreferenceKeys.evidenceLedger, []);
		      evidenceLedger = Array.isArray(stored) ? stored.slice(0, 50) : [];
		    }

		    function persistEvidenceLedger() {
		      evidenceLedger = evidenceLedger.slice(0, 50);
		      writeJsonPreference(viewPreferenceKeys.evidenceLedger, evidenceLedger);
		    }

		    function recordEvidenceReceipt(entry) {
		      const now = new Date();
		      const receipt = {
		        id: entry.id || `${now.getTime()}-${Math.random().toString(36).slice(2, 8)}`,
		        at: entry.at || now.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }),
		        kind: entry.kind || 'proof',
		        title: entry.title || 'Proof receipt',
		        paneId: entry.paneId || '',
		        detail: entry.detail || '',
		        payload: entry.payload || null
		      };
		      evidenceLedger = [
		        receipt,
		        ...evidenceLedger.filter(item => item.id !== receipt.id)
		      ].slice(0, 50);
		      persistEvidenceLedger();
		      return receipt;
		    }

		    function evidenceLedgerRows(limit = 12) {
		      const rows = evidenceLedger.slice(0, limit);
		      if (!rows.length) return emptyRow('No proof receipts recorded yet.');
		      return rows.map(receipt => {
		        const wave = receipt.paneId ? waves[receipt.paneId] : null;
		        const action = wave ? selectButton(wave, 'inspect') : '';
		        return `<div class="ops-row"><span><strong>${escapeHtml(receipt.at)} ${escapeHtml(receipt.title)}</strong><div class="ops-sub">${escapeHtml(receipt.detail || receipt.paneId || receipt.kind)}</div></span><span class="ops-pill ${escapeHtml(receipt.kind === 'error' ? 'warn' : 'good')}">${escapeHtml(receipt.kind)}</span><span>${action}</span></div>`;
		      }).join('');
		    }

	    async function sweepWallMission() {
	      setCommandStatus('wall', 'Sweeping parent -> child terminal panes...');
	      try {
	        const summary = await sweepMissionChildren();
	        await loadPanes(selectedWaveId);
	        updateWallPulse(summary);
	        setCommandStatus('wall', `Swept ${summary.children || 0} child pane${Number(summary.children || 0) === 1 ? '' : 's'}; read ${summary.read || 0}; packets ${summary.ready_packets || 0}/${summary.ingested || 0}; attention ${summary.needs_attention || 0}; failed ${summary.failed || 0}.`);
	      } catch (error) {
	        setCommandStatus('wall', error.message || 'wall sweep failed');
	        setTimeout(() => reconnectStreamsForSelection(), 120);
	      }
	    }

		    function renderDispatchReceipts() {
		      const container = document.getElementById('dispatchReceipts');
		      const deckContainer = document.getElementById('deckDispatchReceipts');
		      if (!dispatchEvents.length) {
		        const emptyDrawer = '<div class="dispatch-receipt"><span class="dot"></span><span><strong>No dispatches yet</strong>Messages sent from the parent will show per-pane receipts here.</span></div>';
		        const emptyDeck = '<div class="deck-receipt"><span class="dot"></span><span><strong>No parent messages yet</strong><span class="ops-sub">Receipts appear here when the parent talks to a child pane.</span></span></div>';
		        if (container) container.innerHTML = emptyDrawer;
		        if (deckContainer) deckContainer.innerHTML = emptyDeck;
		        updateSelectedDispatchStatus();
		        return;
		      }
		      const drawerMarkup = dispatchEvents.map(event => {
		        const tone = event.failed ? 'yellow' : 'green';
		        const at = dispatchEventTime(event.at);
		        const receipts = (event.receipts || [])
		          .map(receipt => `${receipt.ok ? 'ok' : 'fail'} ${receipt.pane_id}${receipt.error ? `: ${receipt.error}` : ''}`)
		          .join('; ');
		        return `<div class="dispatch-receipt"><span class="dot ${tone === 'green' ? 'live' : ''}"></span><span><strong>${escapeHtml(at)} ${escapeHtml(event.target)}</strong>${escapeHtml(event.sent)}/${escapeHtml(event.requested)} delivered${event.failed ? `, ${escapeHtml(event.failed)} failed` : ''}<div class="ops-sub">${escapeHtml(receipts || 'no receipts')}</div></span></div>`;
		      }).join('');
		      if (container) container.innerHTML = drawerMarkup;
		      if (deckContainer) {
		        deckContainer.innerHTML = dispatchEvents.slice(0, 3).map(event => {
		          const tone = event.failed ? 'yellow' : 'green';
		          const at = dispatchEventTime(event.at);
		          const receipts = (event.receipts || [])
		            .map(receipt => `${receipt.ok ? 'ok' : 'fail'} ${receipt.pane_id}`)
		            .join('; ');
		          return `<div class="deck-receipt"><span class="dot ${tone === 'green' ? 'live' : ''}"></span><span><strong>${escapeHtml(at)} ${escapeHtml(event.target)}</strong>${escapeHtml(event.sent)}/${escapeHtml(event.requested)} delivered<div class="ops-sub">${escapeHtml(receipts || 'no receipts')}</div></span></div>`;
		        }).join('');
		      }
		      updateSelectedDispatchStatus();
		    }

		    function updateSelectedDispatchStatus() {
		      const node = document.getElementById('selectedDispatch');
		      if (!node) return;
		      const wave = waves[selectedWaveId];
		      const dispatch = dispatchReceiptForWave(wave);
		      if (!wave) {
		        node.textContent = 'no pane selected';
		        return;
		      }
		      if (!dispatch) {
		        node.textContent = 'no dispatch yet';
		        return;
		      }
		      const label = dispatch.receipt.ok ? 'delivered' : 'failed';
		      const suffix = dispatch.receipt.error ? `: ${dispatch.receipt.error}` : '';
		      node.textContent = `${dispatchEventTime(dispatch.event.at)} ${label} via ${dispatch.event.target}${suffix}`;
		    }

		    async function splitSelectedPane(direction) {
		      const wave = waves[selectedWaveId];
		      if (!wave || !wave.paneId) {
		        document.getElementById('controlTarget').textContent = 'no pane selected';
		        return;
		      }
		      document.getElementById('controlTarget').textContent = `splitting ${wave.paneId}`;
		      pauseTerminalStreams('splitting');
		      try {
		        const response = await fetch(`/pane/split?pane_id=${encodeURIComponent(wave.paneId)}&direction=${encodeURIComponent(direction)}&focus=true`, { cache: 'no-store' });
		        const payload = await response.json();
		        if (!response.ok || payload.error) throw new Error(payload.error?.message || 'split failed');
		        const newPaneId = payload.result?.pane?.pane_id;
		        document.getElementById('controlTarget').textContent = newPaneId ? `created ${newPaneId}` : 'pane created';
		        await loadPanes(newPaneId);
		      } catch (error) {
		        document.getElementById('controlTarget').textContent = error.message || 'split failed';
		        setTimeout(() => reconnectStreamsForSelection(), 120);
		      }
		    }

		    async function attachPaneContract(paneId, title, mode, brief, status = 'running') {
		      const params = new URLSearchParams({
		        pane_id: paneId,
		        title,
		        mode,
		        status
		      });
		      if (brief.trim()) params.set('brief', brief.trim());
		      params.set('delivery', 'shell_card');
		      const response = await fetch(`/pane/contract?${params.toString()}`, { cache: 'no-store' });
		      const payload = await response.json();
		      if (!response.ok || payload.error) throw new Error(payload.error?.message || 'contract failed');
		      return payload;
		    }

		    function setChildDispatchStatus(message, source = 'drawer') {
		      document.getElementById('controlTarget').textContent = message;
		      const childStatus = document.getElementById('childDispatchStatus');
		      if (childStatus) childStatus.textContent = message;
		      const childSummary = document.getElementById('childDispatchSummary');
		      if (childSummary) childSummary.textContent = message;
		      setCommandStatus(source, message);
		    }

		    function mirroredValue(primaryId, fallbackId, fallback = '') {
		      const primary = document.getElementById(primaryId);
		      const fallbackNode = document.getElementById(fallbackId);
		      const value = String(primary?.value || '').trim() || String(fallbackNode?.value || '').trim() || fallback;
		      if (primary && !String(primary.value || '').trim() && value) primary.value = value;
		      if (fallbackNode && !String(fallbackNode.value || '').trim() && value) fallbackNode.value = value;
		      return value;
		    }

		    function dependencyIsParallel(dependency) {
		      const normalized = String(dependency || '').trim().toLowerCase();
		      return !normalized || normalized === 'parallel' || normalized === 'parallel ok' || normalized === 'none' || normalized === 'no dependency';
		    }

		    function dispatchStatusForDependency(dependency) {
		      return dependencyIsParallel(dependency) ? 'running' : 'queued';
		    }

		    function childDispatchConfig(source = 'drawer') {
		      const deckFirst = source === 'deck' || source === 'wall';
		      const title = deckFirst
		        ? mirroredValue('deckChildTitle', 'childTitle', `Child pane ${childWaves().length + 1}`)
		        : mirroredValue('childTitle', 'deckChildTitle', `Child pane ${childWaves().length + 1}`);
		      const mode = deckFirst
		        ? mirroredValue('deckChildMode', 'childMode', 'draft_only')
		        : mirroredValue('childMode', 'deckChildMode', 'draft_only');
		      const brief = deckFirst
		        ? mirroredValue('deckChildBrief', 'childBrief', '')
		        : mirroredValue('childBrief', 'deckChildBrief', '');
		      const argvText = deckFirst
		        ? mirroredValue('deckChildArgv', 'agentArgv', '/bin/zsh -l')
		        : mirroredValue('agentArgv', 'deckChildArgv', '/bin/zsh -l');
		      const dependency = deckFirst
		        ? mirroredValue('deckChildDependency', 'childDependency', '')
		        : mirroredValue('childDependency', 'deckChildDependency', '');
		      const status = dispatchStatusForDependency(dependency);
		      return {
		        title: title || `Child pane ${childWaves().length + 1}`,
		        mode: mode || 'draft_only',
		        brief,
		        argv: splitArgv(argvText),
		        argvText,
		        dependency,
		        status
		      };
		    }

		    function shellSafeHeredocPrompt(prompt) {
		      const delimiter = prompt.includes('HERDR_CHILD_CONTRACT') ? 'HERDR_CHILD_BRIEF' : 'HERDR_CHILD_CONTRACT';
		      return `cat <<'${delimiter}'\n${prompt}\n${delimiter}`;
		    }

		    function dispatchPromptForArgv(argv, prompt) {
		      const executable = basename(argv[0] || '').toLowerCase();
		      return ['sh', 'bash', 'zsh', 'fish'].includes(executable)
		        ? shellSafeHeredocPrompt(prompt)
		        : prompt;
		    }

		    function recordChildDispatch({ title, paneId, promptSent, argvText }) {
		      recordEvidenceReceipt({
		        kind: promptSent ? 'launch' : 'error',
		        title: `Child launch: ${title}`,
		        paneId,
		        detail: `${promptSent ? 'brief sent' : 'brief not sent'}; ${argvText || 'split pane'}`,
		        payload: { promptSent: Boolean(promptSent), argvText: argvText || 'split pane' }
		      });
		      recordDispatch({
		        target: `child dispatch: ${title}`,
		        requested: 1,
		        sent: promptSent ? 1 : 0,
		        failed: promptSent ? 0 : 1,
		        receipts: [{
		          pane_id: paneId,
		          ok: Boolean(promptSent),
		          error: promptSent ? undefined : 'start brief was not delivered'
		        }]
		      });
		      const summary = `${title} -> ${paneId}; contract attached; ${promptSent ? 'brief sent' : 'brief not sent'}; ${argvText || 'split pane'}`;
		      const childStatus = document.getElementById('childDispatchStatus');
		      if (childStatus) childStatus.textContent = summary;
		      const childSummary = document.getElementById('childDispatchSummary');
		      if (childSummary) childSummary.textContent = summary;
		    }

		    async function startChildSession(direction, source = 'drawer') {
		      const wave = parentWave() || waves[selectedWaveId];
		      if (!wave || !wave.paneId) {
		        setChildDispatchStatus('no parent pane selected', source);
		        return;
		      }
		      const { title, mode, brief, argv, argvText, dependency, status } = childDispatchConfig(source);
		      const startPrompt = newChildDispatchPrompt(title, mode, brief, dependency, status);
		      const actionLabel = status === 'queued' ? 'queueing' : 'starting';
		      setChildDispatchStatus(`${actionLabel} ${title} from parent ${wave.paneId}`, source);
		      pauseTerminalStreams('starting');
		      try {
		        if (argv.length) {
		          const params = new URLSearchParams({
		            target_pane_id: wave.paneId,
		            workspace_id: wave.workspaceId,
		            tab_id: wave.tabId,
		            cwd: wave.cwd,
		            direction,
		            name: title,
		            title,
		            mode,
		            status,
		            argv: JSON.stringify(argv),
		            prompt: dispatchPromptForArgv(argv, startPrompt),
		            focus: 'true'
		          });
		          if (brief) params.set('brief', brief);
		          if (dependency) params.set('dependency', dependency);
		          const agentResponse = await fetch(`/agent/start?${params.toString()}`, { cache: 'no-store' });
		          const agentPayload = await agentResponse.json();
		          if (!agentResponse.ok || agentPayload.error) throw new Error(agentPayload.error?.message || 'agent start failed');
		          const newPaneId = agentPayload.result?.start?.agent?.pane_id;
		          if (!newPaneId) throw new Error('agent start did not return a pane id');
		          const promptSent = Boolean(agentPayload.result?.prompt_sent);
		          await loadPanes(newPaneId);
		          recordChildDispatch({ title, paneId: newPaneId, promptSent, argvText });
		          setChildDispatchStatus(`${status === 'queued' ? 'queued' : 'started'} ${title} in ${newPaneId}; contract attached; ${promptSent ? 'brief sent' : 'brief not sent'}`, source);
		          return;
		        }
		        const splitResponse = await fetch(`/pane/split?pane_id=${encodeURIComponent(wave.paneId)}&direction=${encodeURIComponent(direction)}&focus=true`, { cache: 'no-store' });
		        const splitPayload = await splitResponse.json();
		        if (!splitResponse.ok || splitPayload.error) throw new Error(splitPayload.error?.message || 'split failed');
		        const newPaneId = splitPayload.result?.pane?.pane_id;
		        if (!newPaneId) throw new Error('split did not return a pane id');
		        await attachPaneContract(newPaneId, title, mode, brief, status);
		        await sendTextToPane(newPaneId, `${shellSafeHeredocPrompt(startPrompt)}\r`);
		        await loadPanes(newPaneId);
		        recordChildDispatch({ title, paneId: newPaneId, promptSent: true, argvText: 'split pane' });
		        setChildDispatchStatus(`${status === 'queued' ? 'queued' : 'created'} ${title} in ${newPaneId}; contract attached; brief sent`, source);
		      } catch (error) {
		        setChildDispatchStatus(error.message || 'start failed', source);
		        setTimeout(() => reconnectStreamsForSelection(), 120);
		      }
		    }

		    async function saveReportPacket(field, complete) {
		      const wave = waves[selectedWaveId];
		      if (!wave || !wave.pane?.wave_contract) {
		        document.getElementById('controlTarget').textContent = 'selected pane has no packet contract';
		        return;
		      }
		      const done = new Set(wave.packet.map(item => item.toLowerCase()));
		      const key = field.toLowerCase();
		      if (complete) {
		        done.add(key);
		      } else {
		        done.delete(key);
		      }
		      const items = packetFields.filter(candidate => done.has(candidate.toLowerCase()));
		      document.getElementById('controlTarget').textContent = `saving packet ${items.length}/${packetFields.length}`;
		      try {
		        const params = new URLSearchParams({
		          pane_id: wave.paneId,
		          items: JSON.stringify(items)
		        });
		        const response = await fetch(`/pane/report?${params.toString()}`, { cache: 'no-store' });
		        const payload = await response.json();
		        if (!response.ok || payload.error) throw new Error(payload.error?.message || 'packet save failed');
		        document.getElementById('controlTarget').textContent = `packet ${items.length}/${packetFields.length}`;
		        await loadPanes(wave.id);
		      } catch (error) {
		        document.getElementById('controlTarget').textContent = error.message || 'packet save failed';
		      }
		    }

		    async function ingestSelectedReportPacket(waveId = selectedWaveId) {
		      const wave = waves[waveId];
		      if (!wave || !wave.paneId) {
		        document.getElementById('controlTarget').textContent = 'no pane selected';
		        return;
		      }
		      if (!wave.pane?.wave_contract) {
		        document.getElementById('controlTarget').textContent = 'selected pane has no contract to ingest into';
		        return;
		      }
		      document.getElementById('controlTarget').textContent = `ingesting ${wave.paneId}`;
		      try {
		        const response = await fetch(`/pane/ingest-report?pane_id=${encodeURIComponent(wave.paneId)}`, { cache: 'no-store' });
		        const payload = await response.json();
		        if (!response.ok || payload.error) throw new Error(payload.error?.message || 'ingest failed');
		        const result = payload.result || {};
		        const detected = Array.isArray(result.detected_items) ? result.detected_items.length : 0;
		        const done = Number(result.completed_fields || 0);
		        const required = Number(result.required_fields || packetFields.length);
		        document.getElementById('controlTarget').textContent = `ingested ${detected}; packet ${done}/${required}`;
		        recordEvidenceReceipt({
		          kind: 'packet',
		          title: `Packet ingest: ${wave.title}`,
		          paneId: wave.id,
		          detail: `${detected} detected; packet ${done}/${required}`,
		          payload: result
		        });
		        await loadPanes(wave.id);
		      } catch (error) {
		        document.getElementById('controlTarget').textContent = error.message || 'ingest failed';
		      }
		    }

		    async function readSelectedPaneOutput(waveId = selectedWaveId) {
		      const wave = waves[waveId];
		      if (!wave || !wave.paneId) {
		        document.getElementById('controlTarget').textContent = 'no pane selected';
		        return;
		      }
		      document.getElementById('controlTarget').textContent = `reading ${wave.paneId}`;
		      const deckStatus = document.getElementById('deckStatus');
		      if (deckStatus) deckStatus.textContent = `Reading ${wave.title}...`;
		      pauseTerminalStreams('reading');
		      try {
		        const response = await fetch(`/pane/output?pane_id=${encodeURIComponent(wave.paneId)}&lines=120`, { cache: 'no-store' });
		        const payload = await response.json();
		        if (!response.ok || payload.error) throw new Error(payload.error?.message || 'read failed');
		        const output = payload.result?.output;
		        if (!output) throw new Error('read returned no output');
		        outputSnapshots[wave.id] = {
		          ...output,
		          title: wave.title,
		          at: new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' })
		        };
		        outputEvents.unshift(outputSnapshots[wave.id]);
		        outputEvents = outputEvents.slice(0, 8);
		        recordEvidenceReceipt({
		          kind: 'read',
		          title: `Read output: ${wave.title}`,
		          paneId: wave.id,
		          detail: `${output.nonempty_line_count || 0} non-empty lines; last: ${output.last_nonempty_line || 'none'}`,
		          payload: outputSnapshots[wave.id]
		        });
		        renderWaveGrid();
		        renderSelectedOutput(wave.id);
		        renderWallReadout(wave.id);
		        renderPaneRoster();
		        renderAttentionInbox();
		        renderPacketReviewQueue();
		        renderChangeRadar();
		        renderDerivedBoards();
		        bindWaveInteractions();
		        document.getElementById('controlTarget').textContent = `read ${output.nonempty_line_count || 0} lines from ${wave.paneId}`;
		        if (deckStatus) deckStatus.textContent = `Read ${output.nonempty_line_count || 0} lines from ${wave.title}.`;
		        setCommandStatus('wall', `Read ${output.nonempty_line_count || 0} lines from ${wave.title}.`);
		      } catch (error) {
		        document.getElementById('controlTarget').textContent = error.message || 'read failed';
		        if (deckStatus) deckStatus.textContent = error.message || 'read failed';
		        setCommandStatus('wall', error.message || 'read failed');
		      } finally {
		        setTimeout(() => reconnectStreamsForSelection(), 120);
		      }
		    }

		    function renderSelectedOutput(waveId = selectedWaveId) {
		      const output = outputSnapshots[waveId];
		      const meta = document.getElementById('outputMeta');
		      const body = document.getElementById('selectedOutput');
		      if (!meta || !body) return;
		      if (!output) {
		        meta.textContent = 'No output snapshot yet.';
		        body.textContent = 'Select a pane, then read its output.';
		        return;
		      }
		      const tail = Array.isArray(output.tail_lines) ? output.tail_lines : [];
	      meta.textContent = `${output.at || 'now'} ${output.pane_id}; ${output.nonempty_line_count || 0} non-empty lines; last: ${output.last_nonempty_line || 'none'}`;
	      body.textContent = tail.length ? tail.join('\n') : (output.text || 'No readable output.');
	      renderWallReadout(waveId);
	    }

		    function copyOutputToPrompt() {
		      const output = outputSnapshots[selectedWaveId];
		      const wave = waves[selectedWaveId];
		      if (!output || !wave) {
		        document.getElementById('controlTarget').textContent = 'no output snapshot';
		        return;
		      }
		      const box = document.getElementById('parentCommand');
		      box.value = [
		        `Parent read snapshot from ${wave.title} (${wave.paneId})`,
		        `Last line: ${output.last_nonempty_line || 'none'}`,
		        '',
		        'Recent output:',
		        (output.tail_lines || []).join('\n') || output.text || 'No readable output.',
		        '',
		        'Please respond with a structured report packet or blocker update.'
		      ].join('\n');
		      box.focus();
		      document.getElementById('controlTarget').textContent = `loaded output from ${wave.paneId}`;
		    }

		    async function setSelectedPaneStatus(status) {
		      const wave = waves[selectedWaveId];
		      if (!wave || !wave.paneId) {
		        document.getElementById('controlTarget').textContent = 'no pane selected';
		        return;
		      }
		      if (!wave.pane?.wave_contract) {
		        document.getElementById('controlTarget').textContent = 'selected pane has no contract';
		        return;
		      }
		      document.getElementById('controlTarget').textContent = `marking ${wave.paneId} ${status}`;
		      pauseTerminalStreams('updating status');
		      try {
		        const params = new URLSearchParams({
		          pane_id: wave.paneId,
		          status
		        });
		        const response = await fetch(`/pane/status?${params.toString()}`, { cache: 'no-store' });
		        const payload = await response.json();
		        if (!response.ok || payload.error) throw new Error(payload.error?.message || 'status update failed');
		        const shouldUnlock = status === 'accepted';
		        document.getElementById('controlTarget').textContent = shouldUnlock
		          ? `${wave.paneId} accepted; checking held waves`
		          : `${wave.paneId} marked ${status}`;
		        const deckStatus = document.getElementById('deckStatus');
		        if (deckStatus) deckStatus.textContent = shouldUnlock
		          ? `${wave.title} accepted. Checking dependency gates.`
		          : `${wave.title} marked ${labelFromSnake(status, status)}.`;
		        recordEvidenceReceipt({
		          kind: status === 'accepted' ? 'accept' : 'review',
		          title: `Parent verdict: ${wave.title}`,
		          paneId: wave.id,
		          detail: `${wave.title} marked ${labelFromSnake(status, status)}`,
		          payload: payload.result || {}
		        });
		        await loadPanes(wave.id);
		        if (shouldUnlock) await unlockReadyWaves('deck');
		      } catch (error) {
		        document.getElementById('controlTarget').textContent = error.message || 'status update failed';
		        setTimeout(() => reconnectStreamsForSelection(), 120);
		      }
		    }

		    async function closePane(waveId = selectedWaveId, options = {}) {
		      const wave = waves[waveId];
		      const paneList = Object.values(waves);
		      if (!wave || !wave.paneId) {
		        document.getElementById('controlTarget').textContent = 'no pane selected';
		        return;
		      }
		      if (wave.role === 'parent' && !options.allowParent) {
		        document.getElementById('controlTarget').textContent = 'parent pane stays open';
		        const deckStatus = document.getElementById('deckStatus');
		        if (deckStatus) deckStatus.textContent = 'Select a child pane before stopping a session.';
		        return;
		      }
		      if (paneList.length <= 1) {
		        document.getElementById('controlTarget').textContent = 'last pane stays open';
		        return;
		      }
		      if (!window.confirm(`Stop child pane ${wave.title}?`)) return;
		      const fallback = paneList.find(item => item.id !== wave.id)?.id || null;
		      document.getElementById('controlTarget').textContent = `closing ${wave.paneId}`;
		      const deckStatus = document.getElementById('deckStatus');
		      if (deckStatus) deckStatus.textContent = `Stopping ${wave.title}...`;
		      pauseTerminalStreams('stopping');
		      try {
		        const response = await fetch(`/pane/close?pane_id=${encodeURIComponent(wave.paneId)}`, { cache: 'no-store' });
		        const payload = await response.json();
		        if (!response.ok || payload.error) throw new Error(payload.error?.message || 'close failed');
		        selectedWaveId = fallback;
		        document.getElementById('controlTarget').textContent = 'pane closed';
		        if (deckStatus) deckStatus.textContent = `Stopped ${wave.title}.`;
		        await loadPanes(fallback);
		      } catch (error) {
		        document.getElementById('controlTarget').textContent = error.message || 'close failed';
		        if (deckStatus) deckStatus.textContent = error.message || 'stop failed';
		        setTimeout(() => reconnectStreamsForSelection(), 120);
		      }
		    }

		    async function closeSelectedPane() {
		      return closePane(selectedWaveId);
		    }

		    function cssId(value) {
	      return String(value).replace(/[^a-zA-Z0-9_-]/g, '_');
	    }

	    function compactId(value) {
	      const text = String(value || 'none');
	      if (text.length <= 20) return text;
	      return `${text.slice(0, 9)}...${text.slice(-6)}`;
	    }

	    function paneIdentity(wave) {
	      if (!wave) return 'no pane';
	      return `${compactId(wave.paneId)} / ${compactId(wave.terminal || 'no terminal')}`;
	    }

	    function paneDebugTitle(wave) {
	      if (!wave) return 'no pane';
	      return `${wave.paneId || 'no pane'} / ${wave.terminal || 'no terminal'}`;
	    }

	    function visiblePaneLabel(wave, childIndex = 0) {
	      if (!wave) return 'no pane selected';
	      if (wave.role === 'parent') {
	        return `parent root / ${childWaves().length} child pane${childWaves().length === 1 ? '' : 's'}`;
	      }
	      const packet = packetParts(wave);
	      const packetText = packet.required ? `packet ${packet.done}/${packet.required}` : 'packet n/a';
	      const gate = dependencyGateLabel(dependencyGateForWave(wave));
	      return `child pane ${childIndex || '?'} / ${packetText} / ${gate}`;
	    }

	    function focusRailLabel(wave, childIndex = 0) {
	      if (!wave) return '';
	      if (wave.role === 'parent') {
	        return `${childWaves().length} child${childWaves().length === 1 ? '' : 'ren'}`;
	      }
	      const packet = packetParts(wave);
	      const packetText = packet.required ? `${packet.done}/${packet.required}` : 'n/a';
	      return `child ${childIndex || '?'} / ${packetText} / ${wave.status}`;
	    }

	    function focusedTerminalTitle(wave) {
	      if (!wave) return 'No Herdr pane selected';
	      return `Watching pane: ${wave.role === 'parent' ? 'Parent session' : wave.title}`;
	    }

    function escapeHtml(value) {
      return String(value ?? '')
        .replaceAll('&', '&amp;')
        .replaceAll('<', '&lt;')
        .replaceAll('>', '&gt;')
        .replaceAll('"', '&quot;')
        .replaceAll("'", '&#39;');
    }

    function labelFromSnake(value, fallback = 'unknown') {
      if (!value) return fallback;
      return String(value).replaceAll('_', '-');
    }

    function localOnlyPath(path) {
      return ['.fastembed_cache/', '.fox/', '.superpowers/', '.tmp/'].some(prefix => String(path || '').startsWith(prefix));
    }

    function gitStatusForCwd(cwd) {
      return gitStatuses[cwd] || { is_repository: false, entries: [] };
    }

    function gitEntriesForWave(wave) {
      return gitStatusForCwd(wave.cwd).entries || [];
    }

	    function parentWave() {
	      return (parentPaneId && waves[parentPaneId])
	        || Object.values(waves).find(wave => wave.role === 'parent')
	        || Object.values(waves).find(wave => wave.isRoot)
	        || Object.values(waves)[0]
	        || null;
	    }

	    function childWaves() {
	      const parent = parentWave();
	      return Object.values(waves).filter(wave => !parent || wave.id !== parent.id);
	    }

	    function allPaneWaves() {
	      const parent = parentWave();
	      const children = childWaves();
	      return parent ? [parent, ...children] : children;
	    }

    function allGitEntries() {
      const seen = new Set();
      return Object.values(gitStatuses).flatMap(status => (status.entries || []).map(entry => ({ ...entry, cwd: status.cwd })))
        .filter(entry => {
          const key = `${entry.cwd}:${entry.path}:${entry.code}`;
          if (seen.has(key)) return false;
          seen.add(key);
          return true;
        });
    }

    function gitCodeLabel(entry) {
      if (entry.untracked) return 'new';
      if (entry.code.includes('D')) return 'deleted';
      if (entry.code.includes('R')) return 'renamed';
      if (entry.code.includes('A')) return 'added';
      if (entry.code.includes('M')) return 'modified';
      return entry.code.trim() || 'changed';
    }

    async function loadGitStatuses(waveList) {
      const uniqueCwds = [...new Set(waveList.map(wave => wave.cwd).filter(Boolean))];
      const pairs = await Promise.all(uniqueCwds.map(async cwd => {
        try {
          const response = await fetch(`/git/status?cwd=${encodeURIComponent(cwd)}`, { cache: 'no-store' });
          const payload = await response.json();
          return [cwd, payload.result || { cwd, is_repository: false, entries: [], error: 'git status unavailable' }];
        } catch (error) {
          return [cwd, { cwd, is_repository: false, entries: [], error: error.message || 'git status unavailable' }];
        }
      }));
      gitStatuses = Object.fromEntries(pairs);
    }

		    function packetFor(contract) {
		      const report = contract?.report || {};
		      if (Array.isArray(report.completed_items) && report.completed_items.length) {
		        return report.completed_items.map(field => String(field).toLowerCase());
		      }
		      const completed = Math.max(0, Math.min(packetFields.length, Number(report.completed_fields || 0)));
		      return packetFields.slice(0, completed).map(field => field.toLowerCase());
		    }

		    function attentionItems() {
		      return Array.isArray(lastMissionSweep?.attention) ? lastMissionSweep.attention : [];
		    }

		    function attentionForPane(wave) {
		      return attentionItems().find(item => item.pane_id === wave.paneId) || null;
		    }

		    function sweepPaneFor(wave) {
		      const panes = Array.isArray(lastMissionSweep?.panes) ? lastMissionSweep.panes : [];
		      return panes.find(pane => pane.pane_id === wave.paneId) || null;
		    }

		    function dependencyGateForWave(wave) {
		      if (!wave || wave.role === 'parent') {
		        return { status: 'ready', reason: 'parent owns child gates', dependency: 'parent', upstream_pane_ids: [] };
		      }
		      const gates = Array.isArray(lastMissionSweep?.dependency_gates) ? lastMissionSweep.dependency_gates : [];
		      const gate = gates.find(item => item.pane_id === wave.paneId);
		      if (gate) return gate;
		      const dependency = wave.depends && wave.depends !== 'none' ? wave.depends : 'parallel';
		      if (/^(parallel|parallel ok|none|no dependency)$/i.test(dependency)) {
		        return { status: 'ready', reason: 'parallel wave can run now', dependency, upstream_pane_ids: [] };
		      }
		      return { status: 'unresolved', reason: `sweep needed to resolve ${dependency}`, dependency, upstream_pane_ids: [] };
		    }

		    function dependencyGateTone(status) {
		      if (status === 'ready') return 'good';
		      if (status === 'needs_acceptance' || status === 'waiting_packet') return 'warn';
		      return 'warn';
		    }

		    function dependencyGateLabel(gate) {
		      if (!gate) return 'gate unknown';
		      if (gate.status === 'ready') return 'gate ready';
		      if (gate.status === 'waiting_packet') return 'wait packet';
		      if (gate.status === 'needs_acceptance') return 'needs accept';
		      return 'gate unresolved';
		    }

		    function outputSnapshotForWave(wave) {
		      if (!wave) return null;
		      return outputSnapshots[wave.id] || outputSnapshots[wave.paneId] || null;
		    }

		    function dispatchForWave(wave) {
		      if (!wave) return null;
		      return dispatchEvents.find(event =>
		        (event.receipts || []).some(receipt => receipt.pane_id === wave.paneId)
		      ) || null;
		    }

		    function dispatchReceiptForWave(wave) {
		      const event = dispatchForWave(wave);
		      if (!event) return null;
		      const receipt = (event.receipts || []).find(item => item.pane_id === wave.paneId);
		      return receipt ? { event, receipt } : null;
		    }

		    function packetPercent(wave) {
		      const packet = packetParts(wave);
		      return packet.required ? Math.round((packet.done / packet.required) * 100) : 0;
		    }

		    function attentionChip(wave) {
		      const attention = attentionForPane(wave);
		      if (attention) {
		        return {
		          label: labelFromSnake(attention.kind, 'attention'),
		          tone: attentionTone(attention.kind)
		        };
		      }
		      if (wave.role === 'parent') {
		        return { label: 'parent', tone: 'good' };
		      }
		      const missing = missingPacketFields(wave);
		      if (missing.length) return { label: `${missing.length} missing`, tone: 'warn' };
		      return { label: 'clear', tone: 'good' };
		    }

		    function attentionReasonForWave(wave) {
		      const attention = attentionForPane(wave);
		      if (attention) {
		        const missing = Array.isArray(attention.missing_items) ? attention.missing_items : [];
		        const suffix = missing.length ? `: ${missing.slice(0, 3).join(', ')}${missing.length > 3 ? ', ...' : ''}` : '';
		        return {
		          label: labelFromSnake(attention.kind, 'attention'),
		          detail: `${attention.message || 'needs parent attention'}${suffix}`,
		          tone: attentionTone(attention.kind)
		        };
		      }
		      if (!wave.pane.wave_contract) {
		        return { label: 'no contract', detail: 'Child has no wave contract yet.', tone: 'warn' };
		      }
		      if (wave.status.includes('blocked') || wave.status.includes('needs')) {
		        return { label: wave.status, detail: 'Child status needs a parent decision.', tone: 'warn' };
		      }
		      const missing = missingPacketFields(wave);
		      if (missing.length) {
		        return {
		          label: 'missing packet',
		          detail: `${missing.length} packet field${missing.length === 1 ? '' : 's'} missing: ${missing.slice(0, 3).join(', ')}${missing.length > 3 ? ', ...' : ''}`,
		          tone: 'warn'
		        };
		      }
		      return null;
		    }

	    function attentionInboxItems() {
	      return childWaves()
	        .map(wave => ({ wave, reason: attentionReasonForWave(wave) }))
	        .filter(item => item.reason);
	    }

	    function packetReviewGateReason(wave) {
	      const reason = attentionReasonForWave(wave);
	      if (!reason) return null;
	      return {
	        ...reason,
	        detail: `${reason.detail} - blocked from done court until cleared.`
	      };
	    }

	    function packetReviewItems() {
	      return childWaves().filter(wave =>
	        wave.pane.wave_contract
	        && missingPacketFields(wave).length === 0
	        && !packetReviewGateReason(wave)
	        && !['accepted'].includes(wave.status)
	      );
	    }

		    function renderMissionPulse() {
		      const container = document.getElementById('missionPulse');
		      if (!container) return;
		      const children = childWaves();
		      const contracted = children.filter(wave => wave.pane.wave_contract);
		      const ready = contracted.filter(wave => !missingPacketFields(wave).length);
		      const attentionCount = attentionItems().length || children.filter(wave => {
		        if (!wave.pane.wave_contract) return true;
		        return wave.status.includes('blocked') || wave.status.includes('needs') || missingPacketFields(wave).length > 0;
		      }).length;
		      const sourceChanges = allGitEntries().filter(entry => !localOnlyPath(entry.path)).length;
		      container.innerHTML = [
		        `<div class="pulse-item"><div class="pulse-value">${escapeHtml(children.length)}</div><div class="pulse-label">child panes</div></div>`,
		        `<div class="pulse-item"><div class="pulse-value">${escapeHtml(ready.length)}/${escapeHtml(contracted.length)}</div><div class="pulse-label">packets ready</div></div>`,
		        `<div class="pulse-item"><div class="pulse-value">${escapeHtml(attentionCount)}</div><div class="pulse-label">parent decisions</div></div>`,
		        `<div class="pulse-item"><div class="pulse-value">${escapeHtml(sourceChanges)}</div><div class="pulse-label">source changes</div></div>`
		      ].join('');
		    }

		    function renderAttentionInbox() {
		      const count = document.getElementById('attentionInboxCount');
		      const list = document.getElementById('attentionInboxList');
		      if (!count || !list) return;
		      const items = attentionInboxItems();
		      count.textContent = String(items.length);
		      if (!items.length) {
		        list.innerHTML = '<div class="attention-item empty">No child panes need parent attention.</div>';
		        return;
		      }
		      list.innerHTML = items.map(({ wave, reason }) => `
		        <div class="attention-item">
		          <span>
		            <strong>${escapeHtml(wave.title)}</strong>
		            <span>${escapeHtml(reason.detail)}</span>
		          </span>
		          <span class="attention-actions">
		            <span class="ops-pill ${escapeHtml(reason.tone)}">${escapeHtml(reason.label)}</span>
		            ${selectButton(wave)}
		            ${readButton(wave)}
		            ${missingButton(wave, 'deck')}
		          </span>
		        </div>
		      `).join('');
		    }

		    function renderPacketReviewQueue() {
		      const count = document.getElementById('packetReviewCount');
		      const list = document.getElementById('packetReviewList');
		      if (!count || !list) return;
		      const items = packetReviewItems();
		      count.textContent = String(items.length);
		      if (!items.length) {
		        list.innerHTML = '<div class="review-item empty">No completed child packets are waiting for review.</div>';
		        return;
		      }
		      list.innerHTML = items.map(wave => {
		        const packet = packetParts(wave);
		        const detail = wave.status === 'done'
		          ? 'Packet complete; parent acceptance required before dependents should trust it.'
		          : `Packet complete; status is ${wave.status}.`;
		        return `
		          <div class="review-item">
		            <span>
		              <strong>${escapeHtml(wave.title)}</strong>
		              <span>${escapeHtml(detail)}</span>
		            </span>
		            <span class="review-actions">
		              <span class="ops-pill ${escapeHtml(packetTone(wave))}">${escapeHtml(packet.done)}/${escapeHtml(packet.required)}</span>
		              ${selectButton(wave)}
		              ${readButton(wave)}
              ${statusButton(wave, 'accepted', 'accept packet')}
              ${statusButton(wave, 'needs_review', 'send back')}
		            </span>
		          </div>
		        `;
		      }).join('');
		    }

		    function renderChangeRadar() {
		      const count = document.getElementById('changeRadarCount');
		      const list = document.getElementById('changeRadarList');
		      if (!count || !list) return;
		      const entries = allGitEntries();
		      const sourceEntries = entries.filter(entry => !localOnlyPath(entry.path));
		      const localEntries = entries.filter(entry => localOnlyPath(entry.path));
		      count.textContent = `${sourceEntries.length}`;
		      if (!sourceEntries.length && !localEntries.length) {
		        list.innerHTML = '<div class="change-item empty">No git-visible source changes detected.</div>';
		        return;
		      }
		      const rows = sourceEntries.slice(0, 5).map(entry => `
		        <div class="change-item">
		          <span>
		            <strong>${escapeHtml(entry.path)}</strong>
		            <span>${escapeHtml(entry.cwd || '')}${entry.old_path ? ` - from ${escapeHtml(entry.old_path)}` : ''}</span>
		          </span>
		          <span class="change-actions">
		            <span class="ops-pill ${entry.untracked ? 'warn' : ''}">${escapeHtml(gitCodeLabel(entry))}</span>
		            <button class="mini-action" data-file-receipt="deck">receipt</button>
		          </span>
		        </div>
		      `);
		      if (sourceEntries.length > rows.length) {
		        rows.push(`<div class="change-item"><span><strong>${escapeHtml(sourceEntries.length - rows.length)} more source changes</strong><span>Open Changes for the full list.</span></span><span class="change-actions"><button class="mini-action" data-file-receipt="deck">receipt</button></span></div>`);
		      }
		      if (localEntries.length) {
		        rows.push(`<div class="change-item"><span><strong>${escapeHtml(localEntries.length)} local/session artifact${localEntries.length === 1 ? '' : 's'}</strong><span>Ignored cache, session, or local-only paths are separated from source changes.</span></span><span class="change-actions"><span class="ops-pill warn">local</span><button class="mini-action" data-file-receipt="deck">receipt</button></span></div>`);
		      }
		      list.innerHTML = rows.join('');
		    }

		    function scopeLabel(scope) {
		      const labels = {
		        all: 'all children',
		        attention: 'needs attention',
		        missing_packet: 'missing packets',
		        write: 'write-capable',
		        read_only: 'read-only',
		        review: 'review / verify'
		      };
		      return labels[scope] || 'all children';
		    }

		    function targetWavesForScope(scope) {
		      const children = childWaves();
		      if (scope === 'attention') {
		        return children.filter(wave =>
		          attentionForPane(wave)
		          || !wave.pane.wave_contract
		          || wave.status.includes('blocked')
		          || wave.status.includes('needs')
		          || missingPacketFields(wave).length > 0
		        );
		      }
		      if (scope === 'missing_packet') {
		        return children.filter(wave => missingPacketFields(wave).length > 0);
		      }
		      if (scope === 'write') {
		        return children.filter(wave => ['write', 'draft-only'].includes(wave.mode));
		      }
		      if (scope === 'read_only') {
		        return children.filter(wave => wave.mode === 'read-only');
		      }
		      if (scope === 'review') {
		        return children.filter(wave =>
		          ['reviewer', 'verifier'].includes(wave.mode)
		          || wave.status.includes('review')
		          || wave.status.includes('verify')
		        );
		      }
		      return children;
		    }

		    function currentDeckScope() {
		      return commandScopeForSource('deck');
		    }

		    function updateDeckScopeSummary() {
		      const summary = document.getElementById('deckScopeSummary');
		      if (!summary) return;
		      const scope = currentDeckScope();
		      const targets = targetWavesForScope(scope);
		      const names = targets.slice(0, 3).map(wave => wave.title).join(', ');
		      const suffix = targets.length > 3 ? `, +${targets.length - 3} more` : '';
		      summary.textContent = targets.length
		        ? `${scopeLabel(scope)}: ${targets.length} target${targets.length === 1 ? '' : 's'} (${names}${suffix})`
		        : `${scopeLabel(scope)}: no matching child panes`;
		    }

		    function renderPaneRoster() {
		      const roster = document.getElementById('paneRoster');
		      if (!roster) return;
		      const body = roster.querySelector('.pane-roster-body');
		      if (!body) return;
		      const list = allPaneWaves();
		      if (!list.length) {
		        body.innerHTML = '<div class="pane-roster-row"><span>No panes in this project session yet.</span><span></span><span></span><span></span><span></span><span></span></div>';
		        return;
		      }
		      body.innerHTML = list.map(wave => {
		        const roleText = wave.role === 'parent' ? 'parent' : 'child';
		        const dependency = wave.role === 'parent'
		          ? 'owns child panes'
		          : wave.depends === 'none'
		            ? 'parallel OK'
		            : wave.depends;
		        const packet = packetParts(wave);
		        const packetLabel = wave.role === 'parent'
		          ? 'mission'
		          : `${packet.done}/${packet.required}`;
		        const packetClass = wave.role === 'parent' ? 'good' : packetTone(wave);
		        const gate = dependencyGateForWave(wave);
		        const gateClass = dependencyGateTone(gate.status);
		        const attention = attentionChip(wave);
		        const sweep = sweepPaneFor(wave);
		        const snapshot = outputSnapshotForWave(wave);
		        const dispatch = dispatchReceiptForWave(wave);
		        const outputLabel = snapshot
		          ? `read ${snapshot.nonempty_line_count || 0} lines ${snapshot.at || ''}`.trim()
		          : sweep?.output
		            ? `${sweep.output.nonempty_line_count || 0} lines`
		            : wave.status;
		        const commsLabel = dispatch
		          ? `${dispatch.receipt.ok ? 'parent msg ok' : 'parent msg failed'} ${dispatch.event.at}`
		          : wave.role === 'parent'
		            ? 'parent broadcaster'
		            : 'no parent msg yet';
		        const terminal = wave.terminal || 'no terminal';
		        const cwdLabel = wave.cwd ? basename(wave.cwd) : 'unknown cwd';
		        const actions = [
		          selectButton(wave),
		          readButton(wave),
		          wave.role === 'parent' ? '' : `<button class="mini-action" data-message-wave="${escapeHtml(wave.id)}">intervene</button>`,
		          wave.role === 'parent' ? '' : missingButton(wave),
		          wave.role === 'parent' ? '' : `<button class="mini-action danger" data-close-wave="${escapeHtml(wave.id)}">stop</button>`,
		          `<button class="mini-action" data-full="${escapeHtml(wave.id)}">expand</button>`
		        ].filter(Boolean).join(' ');
		        return `
		          <div class="pane-roster-row ${wave.id === selectedWaveId ? 'active' : ''}" data-wave="${escapeHtml(wave.id)}" title="${escapeHtml(terminal)}">
		            <div class="roster-main">
		              <span class="role-chip ${escapeHtml(roleText)}">${escapeHtml(roleText)}</span>
		              <strong>${escapeHtml(wave.title)}</strong>
		              <span class="roster-sub">${escapeHtml(wave.paneId)} - ${escapeHtml(dependency)} - ${escapeHtml(dependencyGateLabel(gate))}</span>
		            </div>
		            <div class="roster-terminal">
		              <code>${escapeHtml(terminal)}</code>
		              <span class="roster-sub">${escapeHtml(cwdLabel)}</span>
		            </div>
		            <span class="ops-pill ${toneForStatus(wave.mode)}">${escapeHtml(wave.mode)}</span>
		            <div class="roster-progress">
		              <span class="ops-pill ${escapeHtml(packetClass)}">${escapeHtml(packetLabel)}</span>
		              ${wave.role === 'parent' ? '' : `<span class="ops-pill ${escapeHtml(gateClass)}">${escapeHtml(dependencyGateLabel(gate))}</span>`}
		              ${wave.role === 'parent' ? '' : progressMarkup(wave)}
		            </div>
		            <div class="roster-signal">
		              <strong>${escapeHtml(attention.label)}</strong>
		              <span class="roster-sub">${escapeHtml(outputLabel)} - ${escapeHtml(commsLabel)}</span>
		            </div>
		            <div class="roster-actions">${actions}</div>
		          </div>
		        `;
		      }).join('');
		    }

	    function basename(path) {
	      if (!path) return '';
	      const clean = String(path).replace(/\/+$/, '');
	      return clean.split('/').pop() || clean;
	    }

	    function looksLikeOldDemoLabel(value) {
	      const normalized = String(value || '').trim();
	      return /^wave\s+(1:\s*contract surface|2:\s*desktop shell|3:\s*verifier)$/i.test(normalized) ||
	        /mission control mvp/i.test(normalized);
	    }

	    function cleanContractText(value) {
	      return String(value || '').replace(/\bwave\b/gi, 'pane');
	    }

	    function titleCaseSessionName(value) {
	      return String(value || '')
	        .split(' ')
	        .filter(Boolean)
	        .map(word => {
	          if (/^[A-Z0-9]{2,}$/.test(word)) return word;
	          return word.charAt(0).toUpperCase() + word.slice(1);
	        })
	        .join(' ');
	    }

	    function sessionDisplayTitle(value, fallback = 'Child session pane') {
	      const raw = String(value || '').trim();
	      if (!raw) return fallback;
	      let title = raw;
	      for (let i = 0; i < 2; i += 1) {
	        title = title
	          .replace(/^w\d+[-_:\s]+/i, '')
	          .replace(/^wave[-_\s]*\d+[a-z]?[-_:\s]*/i, '')
	          .replace(/^wave\s+\d+[a-z]?[-_:\s]*/i, '');
	      }
	      title = title
	        .replace(/[-_]+/g, ' ')
	        .replace(/\s+/g, ' ')
	        .trim();
	      if (!title || looksLikeOldDemoLabel(title)) return fallback;
	      return titleCaseSessionName(title);
	    }

		    function splitArgv(value) {
		      const input = String(value || '').trim();
		      if (!input) return [];
		      const parts = [];
		      input.replace(/"([^"]*)"|'([^']*)'|[^\s]+/g, (match, doubleQuoted, singleQuoted) => {
		        parts.push(doubleQuoted ?? singleQuoted ?? match);
		        return match;
		      });
		      return parts;
		    }

		    function argvLabel(argv) {
		      return Array.isArray(argv) && argv.length ? argv.join(' ') : '/bin/zsh -l';
		    }

		    function isAutoAgentValue(value) {
		      const normalized = String(value || '').trim();
		      return !normalized
		        || normalized === '/bin/zsh -l'
		        || normalized === 'claude'
		        || normalized === 'codex'
		        || normalized === 'pi'
		        || normalized === 'opencode'
		        || normalized === 'hermes';
		    }

		    function applyPreferredAgentDefaults() {
		      const preferred = integrationState.preferredArgv || ['/bin/zsh', '-l'];
		      const label = argvLabel(preferred);
		      ['deckChildArgv', 'agentArgv', 'deckAgentArgv'].forEach(id => {
		        const input = document.getElementById(id);
		        if (input && isAutoAgentValue(input.value)) input.value = label;
		      });
		      const available = integrationState.recommendations
		        .filter(item => item.command_available)
		        .map(item => `${item.command}${item.needs_install ? ' hook needed' : ''}`);
		      const status = available.length
		        ? `agent default: ${label}; available: ${available.join(', ')}`
		        : 'agent default: /bin/zsh -l; no agent CLI found on PATH';
		      const childStatus = document.getElementById('childDispatchStatus');
		      if (childStatus) childStatus.textContent = status;
		      const launchSummary = document.getElementById('missionLaunchSummary');
		      if (launchSummary) launchSummary.textContent = `${label} -> child panes`;
		    }

		    async function loadIntegrations() {
		      try {
		        const response = await fetch('/integrations', { cache: 'no-store' });
		        const payload = await response.json();
		        if (!response.ok || payload.error) throw new Error(payload.error?.message || 'integration probe failed');
		        const result = payload.result || {};
		        integrationState = {
		          preferredArgv: Array.isArray(result.preferred_argv) && result.preferred_argv.length ? result.preferred_argv : ['/bin/zsh', '-l'],
		          preferredLabel: result.preferred_label || 'shell',
		          recommendations: Array.isArray(result.recommendations) ? result.recommendations : []
		        };
		      } catch (_) {
		        integrationState = {
		          preferredArgv: ['/bin/zsh', '-l'],
		          preferredLabel: 'shell',
		          recommendations: []
		        };
		      }
		      applyPreferredAgentDefaults();
		    }

		    function newChildDispatchPrompt(title, mode, brief, dependency, status = 'running') {
	      const project = document.getElementById('parentTitle').textContent || 'Herdr mission';
	      const gated = status === 'queued' && !dependencyIsParallel(dependency);
	      const gateInstruction = gated
	        ? [
	          'Dependency gate:',
	          `This pane is queued behind "${dependency}". Do not begin implementation or make edits until the parent explicitly unlocks or accepts the upstream packet.`,
	          'You may read this brief, confirm you are waiting, and then stay idle for parent instruction.'
	        ]
	        : [
	          'Dependency gate:',
	          'This pane is parallel-ready; begin only inside the approved scope.'
	        ];
	      return [
	        `Parent mission: ${project}`,
	        `Child session: ${title}`,
	        `Mode: ${labelFromSnake(mode, 'draft-only')}`,
	        `Status: ${status}`,
	        `Dependency: ${dependency || 'parallel OK'}`,
	        '',
	        ...gateInstruction,
	        '',
	        'Mission contract:',
	        brief || 'Work only inside the parent-approved scope for this child pane.',
	        '',
	        'Tool policy:',
	        'Use px for repo/context checks if it is available on this system. If px is missing, continue with normal shell inspection and mention that in the report.',
	        '',
	        'Required report packet:',
	        packetFields.map((field, index) => `${index + 1}. ${field}`).join('\n'),
	        '',
	        'Before claiming done, report evidence/receipts, files read, files changed, commands run, risks, good/bad/ugly, recommendation, and next wave suggestion.'
	      ].join('\n');
	    }

	    function paneTitle(pane, contract, index) {
	      const contractTitle = contract.title || '';
	      const contractMatchesPane = !contract.pane_id || contract.pane_id === pane.pane_id;
	      const fallback = pane.is_root_pane ? 'Parent session' : `Child session pane ${index + 1}`;
	      if (!pane.is_root_pane && contractTitle && contractMatchesPane) {
	        return sessionDisplayTitle(contractTitle, fallback);
	      }
	      const raw = pane.label || pane.agent || contract.title || '';
	      return sessionDisplayTitle(raw, contractTitle ? sessionDisplayTitle(contractTitle, fallback) : fallback);
	    }

	    function currentProjectLabel(panes) {
	      const focused = panes.find(pane => pane.focused) || panes[0];
	      const workspace = workspaces.find(item => item.workspace_id === focused?.workspace_id) || workspaces[0];
	      return basename(focused?.cwd) || (looksLikeOldDemoLabel(workspace?.label) ? '' : workspace?.label) || focused?.workspace_id || 'Herdr project';
	    }

	    function workroomPaneEntries(workroom = workroomProjection) {
	      if (!workroom) return [];
	      return [workroom.parent, ...(Array.isArray(workroom.children) ? workroom.children : [])].filter(Boolean);
	    }

	    function workroomPaneMap(workroom = workroomProjection) {
	      return new Map(workroomPaneEntries(workroom).map(pane => [pane.pane_id, pane]));
	    }

	    function applyWorkroomProjection(wave, model) {
	      if (!wave || !model) return wave;
	      wave.workroom = model;
	      wave.role = model.role || wave.role;
	      wave.mode = model.mode ? labelFromSnake(model.mode, wave.mode) : wave.mode;
	      wave.status = model.status ? labelFromSnake(model.status, wave.status) : wave.status;
	      wave.lifecycleLane = model.lifecycle_lane || wave.lifecycleLane;
	      wave.depends = model.dependency || wave.depends;
	      wave.blast = model.blast_radius ? labelFromSnake(model.blast_radius, wave.blast) : wave.blast;
	      wave.packetLabel = model.packet || wave.packetLabel;
	      wave.hasContract = Boolean(model.has_contract);
	      if (model.terminal_id) wave.terminal = model.terminal_id;
	      if (model.workspace_id) wave.workspaceId = model.workspace_id;
	      if (model.tab_id) wave.tabId = model.tab_id;
	      if (model.cwd) wave.cwd = model.cwd;
	      if (model.role === 'parent') {
	        wave.title = 'Parent session';
	      } else if (model.title) {
	        wave.title = sessionDisplayTitle(model.title, wave.title);
	      }
	      return wave;
	    }

	    function paneToWave(pane, index) {
	      const contract = pane.wave_contract || {};
	      const arcs = Array.isArray(contract.arcs) ? contract.arcs : [];
	      const title = paneTitle(pane, contract, index);
	      const contractTitle = contract.title && contract.title !== title ? contract.title : '';
	      const contractDrift = Boolean(contract.pane_id && contract.pane_id !== pane.pane_id);
	      const status = labelFromSnake(contract.status || pane.custom_status || pane.agent_status, 'idle');
	      return {
	        id: pane.pane_id,
	        paneId: pane.pane_id,
	        terminal: pane.terminal_id,
	        workspaceId: pane.workspace_id,
	        tabId: pane.tab_id,
	        isRoot: Boolean(pane.is_root_pane),
	        role: pane.is_root_pane ? 'parent' : 'child',
	        cwd: pane.cwd || '',
	        title,
	        contractTitle,
	        contractDrift,
	        lifecycleLane: contract.lifecycle_lane || '',
	        mode: labelFromSnake(contract.mode, 'terminal'),
	        status,
        depends: contract.dependency || 'none',
        promptDelivery: contract.prompt_delivery || '',
	        blast: labelFromSnake(contract.blast_radius, 'unknown'),
	        arcs: arcs.length ? arcs.map(arc => `${arc.id || 'arc'}: ${cleanContractText(arc.summary || '')}`).join(', ') : 'none declared',
        arcList: arcs,
        packet: packetFor(contract),
        packetLabel: contract.report ? `${(contract.report.completed_items?.length || contract.report.completed_fields || 0)}/${contract.report.required_fields || 10}` : '0/10',
        pane
      };
    }

	    function renderMissionTree() {
	      const container = document.getElementById('missionChildren');
	      const parent = parentWave();
	      const children = childWaves();
	      if (!parent) {
	        container.innerHTML = '<div class="arc-node"><span></span><span>No panes found</span><span></span></div>';
	        return;
	      }
	      const parentCollapsed = document.querySelector('[data-children="parent-pane"]')?.hidden || false;
      const childRows = children.length
        ? children.map(wave => {
          const childIndex = children.findIndex(child => child.id === wave.id) + 1;
          const packet = packetParts(wave);
          const packetLabel = packet.required ? `packet ${packet.done}/${packet.required}` : 'packet n/a';
          const childDetailLabel = `${wave.mode} / ${wave.status} / ${packetLabel} / ${dependencyGateLabel(dependencyGateForWave(wave))}`;
          const arcs = wave.arcList.length
            ? `<div class="tree-children" data-children="${escapeHtml(wave.id)}" hidden>${wave.arcList.map(arc => `<div class="arc-node"><span></span><span class="tree-main"><strong>${escapeHtml(arc.id || 'arc')}</strong><small>${escapeHtml(cleanContractText(arc.summary || ''))}</small></span><span></span></div>`).join('')}</div>`
            : `<div class="tree-children" data-children="${escapeHtml(wave.id)}" hidden><div class="arc-node"><span></span><span class="tree-main"><strong>${escapeHtml(wave.title)}</strong><small title="${escapeHtml(paneDebugTitle(wave))}">${escapeHtml(childDetailLabel)}</small></span><span></span></div></div>`;
          return `
	            <button class="wave-node" data-wave="${escapeHtml(wave.id)}" data-toggle="${escapeHtml(wave.id)}" data-tree-role="child" aria-expanded="false" title="Click row to select. Click disclosure to show contract details.">
	              <span class="chev">></span><span class="tree-main"><strong>${escapeHtml(wave.title)}</strong><small title="${escapeHtml(paneDebugTitle(wave))}">${escapeHtml(visiblePaneLabel(wave, childIndex))}</small></span><span class="badge">${escapeHtml(wave.status)}</span>
	            </button>
	            ${arcs}
	          `;
	        }).join('')
	        : '<div class="arc-node"><span></span><span>No child panes yet</span><span></span></div>';
	      container.innerHTML = `
	        <div class="mission-node" data-wave="${escapeHtml(parent.id)}" data-toggle="parent-pane" data-tree-role="parent" role="button" tabindex="0" aria-expanded="${parentCollapsed ? 'false' : 'true'}" title="Parent session. Click to select the root pane; click the disclosure to collapse child panes.">
	          <span class="chev">${parentCollapsed ? '>' : 'v'}</span><span class="tree-main"><strong>Parent session</strong><small>parent root / ${escapeHtml(children.length)} child pane${children.length === 1 ? '' : 's'}</small></span><span class="tree-parent-actions"><button class="tree-icon-button" data-tree-create-child aria-label="Create child pane under parent session" title="Create child pane under parent session">+</button><span class="badge green">${escapeHtml(children.length)} child${children.length === 1 ? '' : 'ren'}</span></span>
	        </div>
	        <div class="tree-children" data-children="parent-pane"${parentCollapsed ? ' hidden' : ''}>${childRows}</div>
	      `;
		    }

	    function renderWaveGrid() {
	      const grid = document.getElementById('waveGrid');
	      const list = allPaneWaves();
	      const children = childWaves();
	      document.body.dataset.paneCount = String(list.length);
	      if (!list.length) {
	        grid.innerHTML = '<div class="empty-state">No panes found yet. Create or split Herdr panes in this project session and they will appear here.</div>';
	        return;
	      }
	      grid.innerHTML = list.map(wave => {
	        const isParent = wave.role === 'parent';
		        const childIndex = isParent ? 0 : children.findIndex(child => child.id === wave.id) + 1;
		        const roleLabel = isParent ? 'Parent session terminal' : `Child session terminal ${childIndex || ''}`.trim();
                const roleBadge = isParent ? 'PARENT' : `CHILD ${childIndex || ''}`.trim();
		        const statusTone = isParent ? 'green' : toneForStatus(wave.status);
		        const contractLabel = isParent ? `${childWaves().length} child panes` : (wave.contractDrift ? 'contract id drift' : (wave.pane.wave_contract ? 'contract attached' : 'no contract'));
		        const terminalLabel = wave.terminal || 'no terminal';
		        const tileTitle = isParent ? 'Parent session terminal' : wave.title;
	        const visibleLabel = visiblePaneLabel(wave, childIndex);
	        const railLabel = focusRailLabel(wave, childIndex);
	        const packet = packetParts(wave);
	        const packetToneClass = isParent ? 'good' : packetTone(wave);
	        const packetLabel = isParent ? 'parent control' : (packet.required ? `packet ${packet.done}/${packet.required}` : 'packet n/a');
	        const canAccept = !isParent && packet.required > 0 && packet.done >= packet.required && wave.status !== 'accepted';
	        const attention = attentionChip(wave);
	        const gate = dependencyGateForWave(wave);
	        const gateClass = isParent ? 'good' : dependencyGateTone(gate.status);
	        const sweep = sweepPaneFor(wave);
	        const snapshot = outputSnapshotForWave(wave);
	        const dispatch = dispatchReceiptForWave(wave);
		        const outputLabel = isParent
		          ? 'parent control'
		          : (snapshot
		            ? `read ${snapshot.nonempty_line_count || 0} ${snapshot.at || ''}`.trim()
		            : (sweep?.output ? `${sweep.output.nonempty_line_count || 0} lines read` : 'not read yet'));
		        const dispatchLabel = isParent
		          ? 'parent broadcaster'
		          : (dispatch
		            ? `${dispatch.receipt.ok ? 'msg ok' : 'msg failed'} ${dispatchEventTime(dispatch.event.at)}`
		            : 'no parent msg');
		        const dispatchClass = isParent || dispatch?.receipt?.ok ? 'good' : dispatch ? 'bad' : '';
	        const progressPercent = isParent ? 100 : packetPercent(wave);
	        const railStatus = isParent
	          ? `${childWaves().length} children`
	          : wave.status;
	        const railPacket = isParent ? `${childWaves().length} child` : (packet.required ? `${packet.done}/${packet.required}` : 'n/a');
	        const actions = [
	          `<button class="pane-action" data-tile-action="read" data-read-wave="${escapeHtml(wave.id)}" title="Read this pane output into the parent drawer">read</button>`,
	          isParent ? '' : `<button class="pane-action" data-tile-action="message" data-message-wave="${escapeHtml(wave.id)}" title="Intervene in this child pane">intervene</button>`,
	          isParent ? '' : `<button class="pane-action secondary" data-tile-action="packet" data-missing-wave="${escapeHtml(wave.id)}" data-prompt-target="deck" title="Draft a report-packet request for this child pane">packet</button>`,
	          canAccept ? `<button class="pane-action secondary good" data-tile-action="accept" data-status-wave="${escapeHtml(wave.id)}" data-status="accepted" title="Accept this child pane report packet">accept</button>` : '',
	          isParent ? '' : `<button class="pane-action secondary danger" data-tile-action="stop" data-close-wave="${escapeHtml(wave.id)}" title="Stop and close this child pane">stop</button>`,
	          `<button class="pane-action" data-tile-action="full" data-full="${escapeHtml(wave.id)}" title="Expand this pane">expand</button>`
	        ].filter(Boolean).join('');
	        return `
		          <article class="wave-card ${wave.role === 'parent' ? 'parent-pane' : 'child-pane'}" data-wave="${escapeHtml(wave.id)}" data-rail-selected="${wave.id === selectedWaveId ? 'true' : 'false'}" tabindex="0" aria-label="${escapeHtml(tileTitle)} ${escapeHtml(roleLabel)} live terminal pane">
	            <div class="wave-title">
	              <span class="terminal-role">${escapeHtml(roleBadge)}</span>
	              <span class="wave-heading"><strong>${escapeHtml(tileTitle)}</strong><small class="pane-id" title="${escapeHtml(visibleLabel)}">${escapeHtml(railLabel)}</small></span>
	              <span class="tile-controls">${actions}</span>
	            </div>
	            <div class="mini-terminal" data-label="${escapeHtml(roleBadge)} - ${escapeHtml(terminalLabel)} - ${escapeHtml(basename(wave.cwd) || 'cwd')}">
	              <canvas id="tile-${cssId(wave.id)}"></canvas>
	            </div>
	            <div class="pane-rail-summary">
	              <div class="rail-status"><strong>${escapeHtml(isParent ? 'parent' : `child ${childIndex || ''}`.trim())}</strong> ${escapeHtml(railStatus)}</div>
	              <div class="rail-packet ${escapeHtml(packetToneClass)}" title="${escapeHtml(packetLabel)}">${escapeHtml(railPacket)}</div>
	            </div>
	            <div class="pane-footer">
	              <div class="pane-progress" title="${escapeHtml(packetLabel)}"><span class="${escapeHtml(packetToneClass)}" style="width: ${escapeHtml(progressPercent)}%"></span></div>
	              <div class="pane-chips">
	                <span class="pane-chip">${escapeHtml(isParent ? 'parent root' : wave.mode)}</span>
	                <span class="pane-chip context-detail">${escapeHtml(isParent ? 'parent can read/message' : contractLabel)}</span>
	                <span class="pane-chip context-detail">${escapeHtml(compactId(terminalLabel))}</span>
	                <span class="pane-chip ${escapeHtml(packetToneClass)}">${escapeHtml(packetLabel)}</span>
	                <span class="pane-chip ${escapeHtml(gateClass)}">${escapeHtml(isParent ? 'parent gate' : dependencyGateLabel(gate))}</span>
	                <span class="pane-chip ${escapeHtml(attention.tone)}">${escapeHtml(attention.label)}</span>
	                <span class="pane-chip context-detail" data-tile-signal="parent-read">${escapeHtml(outputLabel)}</span>
	                <span class="pane-chip ${escapeHtml(dispatchClass)} context-detail" data-tile-signal="parent-dispatch">${escapeHtml(dispatchLabel)}</span>
	              </div>
	              <div class="tile-status" id="tile-status-${cssId(wave.id)}">${escapeHtml(terminalLabel === 'no terminal' ? 'no terminal' : `stream ${compactId(terminalLabel)}`)}</div>
	            </div>
	          </article>
	        `;
	      }).join('');
	      tileCanvases.clear();
	      list.forEach(wave => {
	        const tileCanvas = document.getElementById(`tile-${cssId(wave.id)}`);
	        if (tileCanvas) {
	          tileCanvases.set(wave.id, tileCanvas);
	          const frame = latestFrames.get(wave.id);
	          if (frame) renderTile(wave.id, frame);
	        }
	      });
	    }

		    function updateMetrics() {
		      const list = childWaves();
		      document.getElementById('metricWaves').textContent = String(list.length);
		      const contracts = list.filter(wave => wave.pane.wave_contract).length;
		      const packetTotals = list.reduce((totals, wave) => {
		        const report = wave.pane.wave_contract?.report;
		        if (!report) return totals;
		        totals.done += wave.packet.length || Number(report.completed_fields || 0);
		        totals.required += Number(report.required_fields || 10);
		        return totals;
		      }, { done: 0, required: 0 });
		      document.getElementById('metricPacket').textContent =
		        packetTotals.required ? `${packetTotals.done}/${packetTotals.required}` : '0/0';
		      document.getElementById('metricOverlap').textContent = String(contracts);
		    }

		    function donePacketFields(wave) {
		      const report = wave.pane.wave_contract?.report || {};
		      if (Array.isArray(report.completed_items) && report.completed_items.length) {
		        return report.completed_items.map(item => String(item));
		      }
		      const done = new Set(wave.packet.map(item => item.toLowerCase()));
		      return packetFields.filter(field => done.has(field.toLowerCase()));
		    }

		    function missingPacketFields(wave) {
		      const report = wave.pane.wave_contract?.report || {};
		      const required = Number(report.required_fields || 0);
		      const completed = Number(report.completed_fields || wave.packet.length || 0);
		      if (required > 0 && completed >= required) return [];
		      const missingCount = required > 0 ? Math.max(0, required - completed) : packetFields.length;
		      const done = new Set(wave.packet.map(item => item.toLowerCase()));
		      return packetFields
		        .filter(field => !done.has(field.toLowerCase()))
		        .slice(0, missingCount);
		    }

		    function card(title, body) {
		      return `<div class="doc-row"><h3>${escapeHtml(title)}</h3><p>${escapeHtml(body)}</p></div>`;
		    }

		    function toneForStatus(value) {
		      const status = String(value || '').toLowerCase();
		      if (status.includes('blocked') || status.includes('failed')) return 'bad';
		      if (status.includes('needs') || status.includes('queued') || status.includes('review')) return 'warn';
		      if (status.includes('done') || status.includes('accepted') || status.includes('running')) return 'good';
		      return '';
		    }

		    function packetParts(wave) {
		      const report = wave.pane.wave_contract?.report || {};
		      const required = Number(report.required_fields || 10);
		      const done = Math.min(required, wave.packet.length || Number(report.completed_fields || 0));
		      return { done, required, missing: Math.max(0, required - done) };
		    }

		    function packetTone(wave) {
		      const packet = packetParts(wave);
		      if (!packet.required) return '';
		      if (packet.done >= packet.required) return 'good';
		      if (packet.done > 0) return 'warn';
		      return 'bad';
		    }

		    function progressMarkup(wave) {
		      const packet = packetParts(wave);
		      const percent = packet.required ? Math.round((packet.done / packet.required) * 100) : 0;
		      return `<div class="progress" aria-label="${packet.done} of ${packet.required} packet fields complete"><span class="${packetTone(wave)}" style="width: ${percent}%"></span></div>`;
		    }

		    function opsCard(kicker, title, rows, wide = false) {
		      return `<section class="ops-card${wide ? ' wide' : ''}"><div class="ops-kicker">${escapeHtml(kicker)}</div><h3>${escapeHtml(title)}</h3>${rows}</section>`;
		    }

		    function reviewDecisionCard(label, value, detail, tone = '') {
		      return `
		        <div class="review-decision-card ${escapeHtml(tone)}">
		          <div class="review-decision-value">${escapeHtml(value)}</div>
		          <div class="review-decision-label">${escapeHtml(label)}</div>
		          <div class="ops-sub">${escapeHtml(detail)}</div>
		        </div>
		      `;
		    }

		    function reviewDocketCard({
		      nextAction,
		      nextActionControls,
		      attention,
		      readyForAcceptance,
		      blockedGates,
		      contracts,
		      readyPackets,
		      primaryChanges
		    }) {
		      const missingPackets = Math.max(0, contracts.length - readyPackets.length);
		      const docketTone = attention.length || readyForAcceptance.length || blockedGates.length
		        ? 'warn'
		        : 'good';
		      const cards = [
		        reviewDecisionCard(
		          'parent decisions',
		          String(attention.length),
		          attention.length ? 'needs a human verdict before acceptance' : 'no sweep blockers',
		          attention.length ? 'warn' : 'good'
		        ),
		        reviewDecisionCard(
		          'ready packets',
		          String(readyForAcceptance.length),
		          'can be accepted or sent back from Done court',
		          readyForAcceptance.length ? 'warn' : 'good'
		        ),
		        reviewDecisionCard(
		          'blocked gates',
		          String(blockedGates.length),
		          'dependent panes waiting on upstream packet acceptance',
		          blockedGates.length ? 'warn' : 'good'
		        ),
		        reviewDecisionCard(
		          'missing packets',
		          String(missingPackets),
		          `${readyPackets.length}/${contracts.length || 0} contracted panes complete`,
		          missingPackets ? 'bad' : 'good'
		        )
		      ].join('');
		      return `
		        <section class="ops-card wide review-docket">
		          <div class="ops-kicker">Parent decision docket</div>
		          <h3>What gets judged now</h3>
		          <div class="review-docket-primary">
		            <span><strong>${escapeHtml(nextAction)}</strong><div class="ops-sub">Decide from receipts first. Evidence, changes, audit, and timeline stay one click away instead of living as permanent clutter.</div></span>
		            <span class="radar-actions">${nextActionControls} ${roomJumpButton('evidence', 'evidence')} ${roomJumpButton('audit', 'audit')}</span>
		          </div>
		          <div class="review-decision-grid">${cards}</div>
		          <div class="ops-row"><span><strong>Evidence footprint</strong><div class="ops-sub">${escapeHtml(primaryChanges.length)} shared checkout source change${primaryChanges.length === 1 ? '' : 's'} visible to the parent.</div></span><span class="ops-pill ${escapeHtml(docketTone)}">${escapeHtml(docketTone === 'good' ? 'clear' : 'judge')}</span><span>${roomJumpButton('changes', 'changes')} ${roomJumpButton('timeline', 'timeline')}</span></div>
		        </section>
		      `;
		    }

		    function emptyRow(message) {
		      return `<div class="ops-row"><span><strong>${escapeHtml(message)}</strong></span><span></span><span></span></div>`;
		    }

		    function selectButton(wave, label = 'focus pane') {
		      return `<button class="mini-action" data-select-wave="${escapeHtml(wave.id)}">${escapeHtml(label)}</button>`;
		    }

		    function contractButton(wave) {
		      return `<button class="mini-action" data-contract-wave="${escapeHtml(wave.id)}">contract</button>`;
		    }

		    function missingButton(wave, destination = 'drawer', label = 'request packet') {
		      return `<button class="mini-action" data-missing-wave="${escapeHtml(wave.id)}" data-prompt-target="${escapeHtml(destination)}">${escapeHtml(label)}</button>`;
		    }

		    function ingestButton(wave, label = 'ingest output') {
		      return `<button class="mini-action" data-ingest-wave="${escapeHtml(wave.id)}">${escapeHtml(label)}</button>`;
		    }

		    function readButton(wave, label = 'read output') {
		      return `<button class="mini-action" data-read-wave="${escapeHtml(wave.id)}">${escapeHtml(label)}</button>`;
		    }

	    function statusButton(wave, status, label) {
	      return `<button class="mini-action" data-status-wave="${escapeHtml(wave.id)}" data-status="${escapeHtml(status)}">${escapeHtml(label)}</button>`;
	    }

	    function roomJumpButton(tab, label) {
	      return `<button class="mini-action" data-room-jump="${escapeHtml(tab)}">${escapeHtml(label)}</button>`;
	    }

	    function humanizeDecisionText(value) {
	      return String(value || 'attention')
	        .replaceAll('_', ' ')
	        .replaceAll('-', ' ')
	        .replace(/\s+/g, ' ')
	        .trim()
	        .toLowerCase();
	    }

	    function liveDecisionLabel(reason) {
	      const label = humanizeDecisionText(reason?.label);
	      if (label.includes('missing contract') || label === 'no contract') {
	        return 'contract needs repair';
	      }
	      if (label.includes('missing packet')) {
	        return 'report packet incomplete';
	      }
	      if (label.includes('needs') || label.includes('blocked')) {
	        return 'parent review needed';
	      }
	      return label || 'parent attention needed';
	    }

	    function renderLiveCommandStrip({
	      attention,
	      readyForAcceptance,
	      contracts,
	      readyPackets,
	      primaryChanges,
	      nextAction,
	      actionControls
	    }) {
	      const summary = document.getElementById('liveCommandSummary');
	      const meta = document.getElementById('liveCommandMeta');
	      const actions = document.getElementById('liveCommandActions');
	      if (!summary || !meta || !actions) return;
	      const decisionCount = attention.length + readyForAcceptance.length;
	      const packetText = contracts.length
	        ? `${readyPackets.length}/${contracts.length} packet${contracts.length === 1 ? '' : 's'} complete`
	        : 'no child contracts yet';
	      const blockerText = attention.length
	        ? `${attention.length} sweep blocker${attention.length === 1 ? '' : 's'}`
	        : 'no sweep blockers';
	      const acceptanceText = readyForAcceptance.length
	        ? `${readyForAcceptance.length} acceptance review${readyForAcceptance.length === 1 ? '' : 's'}`
	        : 'no acceptance reviews';
	      const changeText = `${primaryChanges.length} shared checkout changed file${primaryChanges.length === 1 ? '' : 's'}`;
	      summary.textContent = nextAction;
	      meta.textContent = `${packetText}; ${blockerText}; ${acceptanceText}; ${changeText}.`;
	      actions.innerHTML = `${actionControls} ${roomJumpButton('review', 'open review')} ${roomJumpButton('audit', 'audit gates')}`;
	    }

		    function paneOpsRow(wave, detail, action = '') {
		      return `
		        <div class="ops-row">
		          <span><strong>${escapeHtml(wave.title)}</strong><div class="ops-sub">${escapeHtml(detail)}</div></span>
		          <span class="ops-pill ${toneForStatus(wave.status)}">${escapeHtml(wave.status)}</span>
		          <span>${action || selectButton(wave)}</span>
		        </div>`;
		    }

		    const missionLifecycleLanes = [
		      {
		        id: 'draft_contract',
		        attr: 'data-mission-lane="draft_contract"',
		        title: 'Draft contract',
		        hint: 'child panes without approved scope'
		      },
		      {
		        id: 'running',
		        attr: 'data-mission-lane="running"',
		        title: 'Running',
		        hint: 'contracted panes still doing work'
		      },
		      {
		        id: 'needs_packet',
		        attr: 'data-mission-lane="needs_packet"',
		        title: 'Needs packet',
		        hint: 'missing required report fields'
		      },
		      {
		        id: 'parent_review',
		        attr: 'data-mission-lane="parent_review"',
		        title: 'Parent review',
		        hint: 'ready, blocked, or waiting for verdict'
		      },
		      {
		        id: 'accepted',
		        attr: 'data-mission-lane="accepted"',
		        title: 'Accepted',
		        hint: 'packet accepted by parent'
		      }
		    ];

		    function missionLifecycleLaneIdSet() {
		      return new Set(missionLifecycleLanes.map(lane => lane.id));
		    }

		    function loadMissionBoardCollapsedLanes() {
		      const valid = missionLifecycleLaneIdSet();
		      try {
		        const parsed = JSON.parse(readPreference(viewPreferenceKeys.missionBoardCollapsed, '[]'));
		        if (!Array.isArray(parsed)) return new Set();
		        return new Set(parsed.filter(laneId => valid.has(laneId)));
		      } catch (_) {
		        return new Set();
		      }
		    }

		    function persistMissionBoardCollapsedLanes() {
		      writePreference(viewPreferenceKeys.missionBoardCollapsed, JSON.stringify([...missionBoardCollapsedLanes]));
		    }

		    function setMissionBoardLaneCollapsed(laneId, collapsed) {
		      if (!missionLifecycleLaneIdSet().has(laneId)) return;
		      if (collapsed) {
		        missionBoardCollapsedLanes.add(laneId);
		      } else {
		        missionBoardCollapsedLanes.delete(laneId);
		      }
		      persistMissionBoardCollapsedLanes();
		      renderDerivedBoards();
		      bindWaveInteractions();
		    }

		    function normalizedWaveStatus(wave) {
		      return String(wave?.status || '')
		        .toLowerCase()
		        .replaceAll('-', '_')
		        .replace(/\s+/g, '_');
		    }

		    function normalizedLifecycleLane(value) {
		      const lane = String(value || '')
		        .toLowerCase()
		        .replaceAll('-', '_')
		        .replace(/\s+/g, '_');
		      return missionLifecycleLaneIdSet().has(lane) ? lane : '';
		    }

		    function missionLifecycleLaneForWave(wave) {
		      const status = normalizedWaveStatus(wave);
		      if (!wave.pane.wave_contract) return 'draft_contract';
		      const explicitLane = normalizedLifecycleLane(wave.lifecycleLane);
		      if (explicitLane) return explicitLane;
		      if (status === 'accepted') return 'accepted';
		      if (status === 'queued' || status === 'running') return 'running';
		      if (missingPacketFields(wave).length) return 'needs_packet';
		      if (status === 'blocked' || status === 'needs_review' || status === 'done') {
		        return 'parent_review';
		      }
		      return packetReviewGateReason(wave) ? 'needs_packet' : 'parent_review';
		    }

		    function missionBoardActionsForWave(wave, laneId) {
		      if (laneId === 'draft_contract') return `${selectButton(wave, 'inspect')} ${contractButton(wave)}`;
		      if (laneId === 'needs_packet') return `${readButton(wave, 'read')} ${missingButton(wave, 'deck', 'request')}`;
		      if (laneId === 'parent_review') return `${selectButton(wave, 'inspect')} ${statusButton(wave, 'accepted', 'accept')}`;
		      if (laneId === 'accepted') return `${selectButton(wave, 'inspect')} ${roomJumpButton('timeline', 'timeline')}`;
		      return `${selectButton(wave, 'inspect')} ${readButton(wave, 'read')}`;
		    }

			    function renderMissionLifecycleBoard(list) {
			      const byLane = Object.fromEntries(missionLifecycleLanes.map(lane => [lane.id, []]));
			      list.forEach(wave => {
		        const laneId = missionLifecycleLaneForWave(wave);
		        const lane = byLane[laneId] ? laneId : 'running';
		        byLane[lane].push(wave);
		      });
		      const lanes = missionLifecycleLanes.map(lane => {
		        const rows = byLane[lane.id] || [];
		        const collapsed = missionBoardCollapsedLanes.has(lane.id);
		        const stackId = `mission-board-stack-${cssId(lane.id)}`;
		        const cards = rows.length
		          ? rows.map(wave => {
		            const packet = packetParts(wave);
		            const gate = dependencyGateForWave(wave);
		            const detail = `${wave.mode}; ${wave.status}; packet ${packet.done}/${packet.required}; ${dependencyGateLabel(gate)}`;
		            return `
		              <article class="mission-board-card${wave.id === selectedWaveId ? ' active' : ''}" data-board-wave="${escapeHtml(wave.id)}" data-lifecycle-lane="${escapeHtml(lane.id)}" data-wave="${escapeHtml(wave.id)}" tabindex="0" role="button" title="${escapeHtml(paneDebugTitle(wave))}">
		                <strong>${escapeHtml(wave.title)}</strong>
		                <span>${escapeHtml(detail)}</span>
		                <div class="mission-board-actions">${missionBoardActionsForWave(wave, lane.id)}</div>
		              </article>
		            `;
		          }).join('')
		          : `<div class="ops-row"><span><strong>No panes</strong><div class="ops-sub">${escapeHtml(lane.hint)}</div></span><span></span><span></span></div>`;
		        return `
		          <section class="mission-board-lane" ${lane.attr} data-mission-lane-collapsed="${collapsed ? 'true' : 'false'}">
		            <div class="mission-board-lane-head">
		              <button class="mission-board-lane-toggle" data-mission-lane-toggle="${escapeHtml(lane.id)}" aria-expanded="${String(!collapsed)}" aria-controls="${escapeHtml(stackId)}" title="${collapsed ? 'Expand' : 'Collapse'} ${escapeHtml(lane.title)}">
		                <span><strong>${escapeHtml(lane.title)}</strong><small>${escapeHtml(lane.hint)}</small></span>
		                <span class="mission-board-count">${escapeHtml(rows.length)}</span>
		              </button>
		            </div>
		            <div class="mission-board-stack" id="${escapeHtml(stackId)}"${collapsed ? ' hidden' : ''}>${cards}</div>
		          </section>
		        `;
			      }).join('');
			      return `<section class="ops-card wide"><div class="ops-kicker">Mission Board</div><h3>Pane lifecycle lanes</h3><div class="mission-lifecycle-board" aria-label="Mission Board">${lanes}</div></section>`;
			    }

			    function radarBrief(title, goal, bullets) {
			      return [
			        `Goal: ${goal}`,
			        '',
			        'Research policy:',
			        '- Use px first if it is available; otherwise inspect the repo with normal shell tools and say px was unavailable.',
			        '- Do not edit files unless this candidate explicitly allows it.',
			        '- Return a report packet with evidence, files read, recommended next wave, and good/bad/ugly.',
			        '',
			        'Focus:',
			        ...bullets.map(item => `- ${item}`)
			      ].join('\n');
			    }

			    function buildMissionRadarItems({
			      list,
			      contracts,
			      readyPackets,
			      attention,
			      readyForAcceptance,
			      blockedGates,
			      primaryChanges,
			      localOnlyChanges
			    }) {
			      const candidates = [];
			      const childCount = list.length;
			      const missingContracts = list.filter(wave => !wave.pane.wave_contract);
			      const incompletePackets = contracts.filter(wave => missingPacketFields(wave).length);
			      const acceptedPackets = contracts.filter(wave => normalizedWaveStatus(wave) === 'accepted');
			      const add = item => candidates.push({
			        family: 'insight_task',
			        dependency: '',
			        mode: 'read_only',
			        tone: '',
			        source: 'mission state',
			        evidence: [],
			        allowedPaths: ['current Herdr mission panes'],
			        requiredReport: ['what I found', 'evidence / receipts', 'recommended next wave', 'good / bad / ugly'],
			        suggestedCommands: [],
			        ...item
			      });

			      add({
			        id: 'project-discovery-scout',
			        title: 'Project discovery scout',
			        family: 'roadmap_wave',
			        type: 'roadmap discovery',
			        mode: 'read_only',
			        tone: childCount ? 'good' : 'warn',
			        signal: childCount
			          ? `${childCount} child pane${childCount === 1 ? '' : 's'} to synthesize`
			          : 'no child panes yet',
			        allowedPaths: ['README/docs', 'project configuration', 'current pane contracts', 'PX audit evidence when available'],
			        requiredReport: ['project purpose', 'current maturity', 'top gaps', '3-5 wave contracts', 'blast-radius risk'],
			        suggestedCommands: ['px init --repo <repo> --space herdr --json', 'px audit --repo <repo> --json'],
			        brief: radarBrief(
			          'Project discovery scout',
			          'Research this repository and propose the next useful Herdr waves.',
			          [
			            'Summarize project purpose, current maturity, and highest-value gaps.',
			            'Generate 3-5 candidate wave contracts across code quality, UX, docs, security, performance, and product roadmap.',
			            'For each candidate, include mode, allowed paths, expected report packet, dependencies, and blast-radius risk.'
			          ]
			        )
			      });

			      if (missingContracts.length) {
			        add({
			          id: 'contract-sweeper',
			          title: 'Contract sweeper',
			          type: 'governance cleanup',
			          mode: 'draft_only',
			          tone: 'warn',
			          signal: `${missingContracts.length} pane${missingContracts.length === 1 ? '' : 's'} missing contracts`,
			          evidence: missingContracts.map(wave => `${wave.title} has no wave contract`),
			          allowedPaths: ['pane list', 'pane output tails', 'mission state only'],
			          requiredReport: ['proposed contract title', 'mode', 'dependency', 'allowed paths', 'packet expectations'],
			          brief: radarBrief(
			            'Contract sweeper',
			            'Inspect panes without contracts and draft safe wave contracts for parent approval.',
			            [
			              `Panes needing contracts: ${missingContracts.map(wave => wave.title).join(', ')}`,
			              'Do not change source files.',
			              'Produce one suggested contract per pane with title, mode, dependency, allowed paths, and packet expectations.'
			            ]
			          )
			        });
			      }

			      if (incompletePackets.length) {
			        add({
			          id: 'packet-auditor',
			          title: 'Packet auditor',
			          type: 'report packet',
			          mode: 'reviewer',
			          tone: 'warn',
			          signal: `${incompletePackets.length} packet${incompletePackets.length === 1 ? '' : 's'} incomplete`,
			          evidence: incompletePackets.map(wave => `${wave.title}: missing ${missingPacketFields(wave).slice(0, 3).join(', ')}`),
			          allowedPaths: ['child pane output', 'wave contracts', 'report packet state'],
			          requiredReport: ['missing fields', 'evidence requests', 'dependency impact', 'send-back recommendation'],
			          brief: radarBrief(
			            'Packet auditor',
			            'Read child outputs and turn incomplete report packets into parent-reviewable receipts.',
			            [
			              `Incomplete packets: ${incompletePackets.map(wave => `${wave.title} (${missingPacketFields(wave).slice(0, 3).join(', ')})`).join('; ')}`,
			              'Ask children for missing evidence instead of inventing it.',
			              'Report which dependencies are still blocked by packet gaps.'
			            ]
			          )
			        });
			      }

			      if (readyForAcceptance.length || blockedGates.length) {
			        add({
			          id: 'review-gate-judge',
			          title: 'Review gate judge',
			          type: 'done court',
			          mode: 'reviewer',
			          tone: 'warn',
			          signal: `${readyForAcceptance.length} ready, ${blockedGates.length} blocked`,
			          evidence: [
			            ...readyForAcceptance.map(wave => `${wave.title}: packet ready`),
			            ...blockedGates.map(item => `${item.wave.title}: ${item.gate.reason}`)
			          ],
			          allowedPaths: ['accepted packet candidates', 'blocked dependency gates', 'pane outputs'],
			          requiredReport: ['accept/send-back verdict', 'evidence used', 'missing proof', 'downstream unblock decision'],
			          brief: radarBrief(
			            'Review gate judge',
			            'Evaluate ready packets and blocked gates before the parent accepts downstream work.',
			            [
			              `Ready packets: ${readyForAcceptance.map(wave => wave.title).join(', ') || 'none'}`,
			              `Blocked gates: ${blockedGates.map(item => item.wave.title).join(', ') || 'none'}`,
			              'Return accept/send-back recommendations with evidence and missing-proof reasons.'
			            ]
			          )
			        });
			      }

			      if (primaryChanges.length || localOnlyChanges.length) {
			        add({
			          id: 'blast-radius-reviewer',
			          title: 'Blast-radius reviewer',
			          family: 'ideation_wave',
			          type: 'change radar',
			          mode: 'reviewer',
			          tone: primaryChanges.length ? 'warn' : '',
			          signal: `${primaryChanges.length} source, ${localOnlyChanges.length} local artifact`,
			          evidence: primaryChanges.slice(0, 8).map(entry => entry.path),
			          allowedPaths: primaryChanges.slice(0, 12).map(entry => entry.path),
			          requiredReport: ['owner map', 'overlap risk', 'test recommendation', 'exclude-from-landing list'],
			          suggestedCommands: ['git status --short', 'git diff --check'],
			          brief: radarBrief(
			            'Blast-radius reviewer',
			            'Map changed files to wave ownership and flag overlap before the mission gets messy.',
			            [
			              `Source changes visible: ${primaryChanges.slice(0, 8).map(entry => entry.path).join(', ') || 'none'}`,
			              `Local/session artifacts: ${localOnlyChanges.slice(0, 6).map(entry => entry.path).join(', ') || 'none'}`,
			              'Return owner, risk, test recommendation, and whether any change should be excluded from landing.'
			            ]
			          )
			        });
			      }

			      if (acceptedPackets.length) {
			        add({
			          id: 'next-wave-compiler',
			          title: 'Next-wave compiler',
			          family: 'roadmap_wave',
			          type: 'roadmap synthesis',
			          mode: 'read_only',
			          tone: 'good',
			          signal: `${acceptedPackets.length} accepted packet${acceptedPackets.length === 1 ? '' : 's'} to learn from`,
			          evidence: acceptedPackets.map(wave => `${wave.title}: accepted packet`),
			          allowedPaths: ['accepted report packets', 'mission timeline', 'current project radar'],
			          requiredReport: ['lesson summary', 'conservative next wave', 'ambitious next wave', 'dependencies', 'risk'],
			          brief: radarBrief(
			            'Next-wave compiler',
			            'Use accepted packets and current repo state to propose the next mission wave sequence.',
			            [
			              `Accepted packets: ${acceptedPackets.map(wave => wave.title).join(', ')}`,
			              'Preserve what worked, call out bad/ugly lessons, and propose one conservative next wave plus one ambitious option.',
			              'Return wave contracts instead of broad advice.'
			            ]
			          )
			        });
			      }

			      const unique = [];
			      const seen = new Set();
			      for (const item of candidates) {
			        if (seen.has(item.id)) continue;
			        seen.add(item.id);
			        unique.push(item);
			      }
			      return unique.slice(0, 5);
			    }

			    function mergeMissionRadarItems(scannedItems, localItems) {
			      const merged = [];
			      const seen = new Set();
			      [...(scannedItems || []), ...(localItems || [])].forEach(item => {
			        if (!item || !item.id || seen.has(item.id)) return;
			        seen.add(item.id);
			        merged.push(item);
			      });
			      return merged.slice(0, 7);
			    }

			    function scannedMissionRadarItems() {
			      const items = missionRadarScan?.candidates;
			      if (!Array.isArray(items)) return [];
			      return items.map(item => ({
			        id: item.id,
			        title: item.title,
			        family: item.family || 'insight_task',
			        type: item.type || item.kind || item.source || 'project research',
			        mode: item.mode || 'read_only',
			        tone: item.tone || '',
			        signal: item.signal || item.source || 'px scan',
			        dependency: item.dependency || '',
			        source: item.source || 'px scan',
			        evidence: Array.isArray(item.evidence) ? item.evidence : [],
			        allowedPaths: Array.isArray(item.allowed_paths) ? item.allowed_paths : [],
			        requiredReport: Array.isArray(item.required_report) ? item.required_report : [],
			        suggestedCommands: Array.isArray(item.suggested_commands) ? item.suggested_commands : [],
			        brief: item.brief || ''
			      }));
			    }

			    function radarFamilyLabel(value) {
			      const normalized = String(value || 'insight_task').trim();
			      if (normalized === 'roadmap_wave') return 'roadmap wave';
			      if (normalized === 'ideation_wave') return 'ideation wave';
			      return 'insight task';
			    }

			    function radarFamilyMeta(value) {
			      const normalized = String(value || 'insight_task').trim();
			      if (normalized === 'roadmap_wave') {
			        return {
			          key: 'roadmap_wave',
			          title: 'Roadmap waves',
			          description: 'Mission-scale discovery, sequencing, and next-wave synthesis.'
			        };
			      }
			      if (normalized === 'ideation_wave') {
			        return {
			          key: 'ideation_wave',
			          title: 'Ideation waves',
			          description: 'Focused research waves that turn code signals into scoped work.'
			        };
			      }
			      return {
			        key: 'insight_task',
			        title: 'Insight tasks',
			        description: 'Small research jobs that explain what is worth doing next.'
			      };
			    }

			    function radarCandidateRow(item) {
			      const family = radarFamilyMeta(item.family);
			      const scopeCount = item.allowedPaths?.length || 0;
			      const reportCount = item.requiredReport?.length || 0;
			      const evidenceCount = item.evidence?.length || 0;
			      const detail = [
			        item.type || family.title,
			        item.signal || item.source || '',
			        scopeCount ? `${scopeCount} scope` : '',
			        reportCount ? `${reportCount} report` : '',
			        evidenceCount ? `${evidenceCount} receipt${evidenceCount === 1 ? '' : 's'}` : ''
			      ].filter(Boolean).join(' - ');
			      return `
			        <div class="radar-row">
			          <span><strong>${escapeHtml(item.title)}</strong><div class="ops-sub">${escapeHtml(detail)}</div></span>
			          <span class="ops-pill ${escapeHtml(item.tone || '')}">${escapeHtml(labelFromSnake(item.mode, 'read-only'))}</span>
			          <span class="radar-actions"><button class="mini-action" data-radar-stage="${escapeHtml(item.id)}">stage</button><button class="mini-action primary" data-radar-launch="${escapeHtml(item.id)}">launch</button></span>
			        </div>
			      `;
			    }

			    function radarPickCard(meta, item) {
			      if (!item) {
			        return `
			          <div class="radar-pick">
			            <span class="radar-pick-label">${escapeHtml(meta.title)}</span>
			            <span class="radar-pick-empty">No candidate yet.</span>
			          </div>
			        `;
			      }
			      return `
			        <div class="radar-pick">
			          <span class="radar-pick-label">${escapeHtml(meta.title)}</span>
			          <strong>${escapeHtml(item.title)}</strong>
			          <div class="ops-sub">${escapeHtml(item.signal || item.type || item.source || 'ready to stage')}</div>
			          <span class="radar-actions"><button class="mini-action" data-radar-stage="${escapeHtml(item.id)}">stage</button><button class="mini-action primary" data-radar-launch="${escapeHtml(item.id)}">launch</button></span>
			        </div>
			      `;
			    }

			    function radarLane(meta, laneItems) {
			      return `
			        <details class="radar-lane" data-radar-family="${escapeHtml(meta.key)}">
			          <summary class="radar-lane-head">
			            <span>
			              <h3 class="radar-lane-title">${escapeHtml(meta.title)}</h3>
			              <div class="radar-lane-desc">${escapeHtml(meta.description)}</div>
			            </span>
			            <span class="radar-lane-count">${escapeHtml(String(laneItems.length))}</span>
			          </summary>
			          <div class="radar-lane-body">
			            ${laneItems.length ? laneItems.map(radarCandidateRow).join('') : '<div class="radar-lane-empty">No candidates in this lane yet.</div>'}
			          </div>
			        </details>
			      `;
			    }

			    function missionDraftStageMeta(family) {
			      const normalized = radarFamilyMeta(family).key;
			      if (normalized === 'ideation_wave') {
			        return {
			          family: 'ideation_wave',
			          order: 2,
			          title: 'Shape wave contracts',
			          dependency: 'after scout receipts',
			          purpose: 'Turn repo signals into scoped child panes with mode, paths, and packet gates.'
			        };
			      }
			      if (normalized === 'roadmap_wave') {
			        return {
			          family: 'roadmap_wave',
			          order: 3,
			          title: 'Sequence the mission',
			          dependency: 'after accepted packets',
			          purpose: 'Convert accepted findings into the next parent mission sequence.'
			        };
			      }
			      return {
			        family: 'insight_task',
			        order: 1,
			        title: 'Scout the repo',
			        dependency: 'parallel / read-only',
			        purpose: 'Research what matters before the parent spends real panes on work.'
			      };
			    }

			    function compileMissionDraft(items) {
			      const groups = new Map();
			      (items || []).forEach(item => {
			        const meta = missionDraftStageMeta(item.family);
			        if (!groups.has(meta.family)) groups.set(meta.family, { ...meta, items: [] });
			        groups.get(meta.family).items.push(item);
			      });
			      return [...groups.values()].sort((left, right) => left.order - right.order);
			    }

			    function primaryMissionDraftCandidates(items = missionRadarItems) {
			      return compileMissionDraft(items)
			        .map(stage => ({ stage, candidate: stage.items[0] }))
			        .filter(item => item.candidate);
			    }

			    function missionDraftDependencyForPick(picks, index) {
			      if (index <= 0) return '';
			      const previous = picks[index - 1]?.candidate?.title || 'previous stage';
			      return `after ${previous}`;
			    }

			    function missionDraftRow(stage, displayOrder = stage.order) {
			      const primary = stage.items[0];
			      const extras = Math.max(0, stage.items.length - 1);
			      const candidateText = primary
			        ? `${primary.title}${extras ? `, +${extras} more` : ''}`
			        : 'No candidate yet';
			      const modeText = primary?.mode ? labelFromSnake(primary.mode, 'read-only') : 'read-only';
			      const actions = primary
			        ? `<button class="mini-action" data-radar-stage="${escapeHtml(primary.id)}">stage</button><button class="mini-action primary" data-radar-launch="${escapeHtml(primary.id)}">launch pane</button>`
			        : '';
			      return `
			        <div class="ops-row mission-draft-row">
			          <span><strong>${escapeHtml(displayOrder)}. ${escapeHtml(stage.title)}</strong><div class="ops-sub">${escapeHtml(stage.purpose)} ${escapeHtml(candidateText)}</div></span>
			          <span class="ops-pill">${escapeHtml(modeText)}</span>
			          <span class="radar-actions">${actions}</span>
			        </div>
			      `;
			    }

			    function renderMissionDraftCard(items) {
			      const stages = compileMissionDraft(items);
			      const summary = stages.length
			        ? `${stages.length} stage${stages.length === 1 ? '' : 's'} compiled from ${items.length} candidate${items.length === 1 ? '' : 's'}`
			        : 'Run Radar to compile candidate waves into a parent mission draft.';
			      const header = `
			        <div class="ops-row">
			          <span><strong>Mission draft</strong><div class="ops-sub">${escapeHtml(summary)} Compiled lanes launch as real child panes; missing lanes stay out of the draft.</div></span>
			          <span class="ops-pill ${stages.length ? 'good' : 'warn'}">${stages.length ? 'compiled' : 'empty'}</span>
			          <span class="radar-actions"><button class="mini-action" data-radar-scan>Research next work</button><button class="mini-action" data-radar-stage-draft>stage draft</button><button class="mini-action primary" data-radar-launch-draft>launch draft</button></span>
			        </div>
			      `;
			      const rows = stages.length
			        ? stages.map((stage, index) => missionDraftRow(stage, index + 1)).join('')
			        : emptyRow('No mission draft yet. Run Research next work to ask PX what this repo should do next.');
			      return opsCard('Mission compiler draft', 'Radar candidates sequenced into launchable panes', header + rows, true);
			    }

			    function candidateContractBrief(candidate) {
			      const lines = [];
			      lines.push(`Radar candidate kind: ${radarFamilyLabel(candidate.family)}`);
			      if (candidate.type) lines.push(`Category: ${candidate.type}`);
			      if (candidate.source) lines.push(`Source: ${candidate.source}`);
			      if (candidate.signal) lines.push(`Signal: ${candidate.signal}`);
			      if (candidate.allowedPaths?.length) {
			        lines.push('');
			        lines.push('Allowed paths / scope:');
			        candidate.allowedPaths.forEach(item => lines.push(`- ${item}`));
			      }
			      if (candidate.requiredReport?.length) {
			        lines.push('');
			        lines.push('Required report packet additions:');
			        candidate.requiredReport.forEach(item => lines.push(`- ${item}`));
			      }
			      if (candidate.suggestedCommands?.length) {
			        lines.push('');
			        lines.push('Suggested commands:');
			        candidate.suggestedCommands.forEach(item => lines.push(`- ${item}`));
			      }
			      if (candidate.evidence?.length) {
			        lines.push('');
			        lines.push('Evidence to inspect first:');
			        candidate.evidence.slice(0, 8).forEach(item => lines.push(`- ${item}`));
			      }
			      if (candidate.brief) {
			        lines.push('');
			        lines.push(candidate.brief);
			      }
			      return lines.join('\n').trim();
			    }

			    function renderProjectRadarCard(items) {
			      const px = missionRadarScan?.px || {};
			      const scanStatus = missionRadarScanInFlight
			        ? 'scanning with px...'
			        : missionRadarScan
			          ? px.available
			            ? `${px.ok ? 'px scan loaded' : 'px scan had warnings'}${missionRadarScan.cwd ? ` - ${missionRadarScan.cwd}` : ''}`
			            : 'px unavailable; using live mission state'
			          : 'not scanned yet';
			      const scanTone = missionRadarScanInFlight
			        ? 'warn'
			        : missionRadarScan
			          ? px.ok
			            ? 'good'
			            : px.available
			              ? 'warn'
			              : ''
			          : '';
			      const scanRow = `
			        <div class="ops-row">
			          <span><strong>PX project scan</strong><div class="ops-sub" id="missionRadarScanStatus">${escapeHtml(scanStatus)}</div></span>
			          <span class="ops-pill ${escapeHtml(scanTone)}">${escapeHtml(scannedMissionRadarItems().length)} px</span>
			          <span><button class="mini-action" data-radar-scan>${missionRadarScanInFlight ? 'scanning' : 'scan'}</button></span>
			        </div>
			      `;
			      const families = ['insight_task', 'ideation_wave', 'roadmap_wave'].map(radarFamilyMeta);
			      const topPicks = families.map(meta => {
			        const laneItems = items.filter(item => radarFamilyMeta(item.family).key === meta.key);
			        return radarPickCard(meta, laneItems[0]);
			      }).join('');
			      const lanes = families.map(meta => {
			        const laneItems = items.filter(item => radarFamilyMeta(item.family).key === meta.key);
			        return radarLane(meta, laneItems);
			      }).join('');
			      const empty = items.length ? '' : emptyRow('Radar has no candidate waves yet.');
			      return opsCard('Project radar', 'Expandable research receipts from PX and live panes', scanRow + empty + `<div class="radar-picks">${topPicks}</div><div class="radar-lanes">${lanes}</div>`, true);
			    }

			    function stageRadarCandidate(candidateId, options = {}) {
			      const candidate = missionRadarItems.find(item => item.id === candidateId);
			      if (!candidate) return;
			      applyRadarCandidateToDeck(candidate);
			      setActiveTab('panes');
			      setCommandDeckVisible(true);
			      setChromeToggle('show-command-advanced', 'deckAdvancedToggle', viewPreferenceKeys.commandAdvanced, true, { persist: false });
			      const status = document.getElementById('childDispatchStatus');
			      if (status) status.textContent = `${candidate.title} staged from Project Radar. Review scope, then create right or down.`;
			      setCommandStatus('deck', `${candidate.title} staged. Review scope before launch.`);
			      const launch = document.getElementById('childDispatch');
			      if (launch && options.scroll !== false) launch.scrollIntoView({ block: 'nearest' });
			    }

			    function setSelectValue(select, value, label = value) {
			      if (!select) return;
			      const normalized = String(value || '').trim();
			      if (!normalized) {
			        select.value = '';
			        return;
			      }
			      const exists = Array.from(select.options).some(option => option.value === normalized);
			      if (!exists) {
			        const option = document.createElement('option');
			        option.value = normalized;
			        option.textContent = labelFromSnake(label || normalized, normalized);
			        option.dataset.dynamic = 'mission-draft';
			        select.append(option);
			      }
			      select.value = normalized;
			    }

			    function applyRadarCandidateToDeck(candidate, stage = null, options = {}) {
			      const title = document.getElementById('deckChildTitle');
			      const mode = document.getElementById('deckChildMode');
			      const dependency = document.getElementById('deckChildDependency');
			      const brief = document.getElementById('deckChildBrief');
			      const dependencyValue = options.dependency ?? (candidate.dependency || stage?.dependency || '');
			      const dependencyLabel = options.dependencyLabel || dependencyValue || stage?.dependency || 'parallel';
			      if (title) title.value = candidate.title;
			      setSelectValue(mode, candidate.mode, labelFromSnake(candidate.mode, 'draft-only'));
			      setSelectValue(dependency, dependencyValue, dependencyLabel);
			      if (brief) {
			        const stageLines = stage
			          ? [`Mission compiler stage: ${stage.order}. ${stage.title}`, `Stage dependency: ${dependencyLabel}`, `Stage purpose: ${stage.purpose}`, '']
			          : [];
			        brief.value = [...stageLines, candidateContractBrief(candidate)].join('\n').trim();
			      }
			    }

			    function stageMissionDraft() {
			      const picks = primaryMissionDraftCandidates();
			      if (!picks.length) {
			        setCommandStatus('deck', 'Run Research next work before staging a mission draft.');
			        return;
			      }
			      const first = picks[0];
			      const dependency = missionDraftDependencyForPick(picks, 0);
			      applyRadarCandidateToDeck(first.candidate, first.stage, {
			        dependency,
			        dependencyLabel: dependency || 'parallel'
			      });
			      setActiveTab('panes');
			      setCommandDeckVisible(true);
			      setChromeToggle('show-command-advanced', 'deckAdvancedToggle', viewPreferenceKeys.commandAdvanced, true, { persist: false });
			      const status = document.getElementById('childDispatchStatus');
			      if (status) status.textContent = `${picks.length} mission draft stage${picks.length === 1 ? '' : 's'} ready. First stage is staged; launch draft creates all primary stage panes.`;
			      setCommandStatus('deck', `Mission draft staged: ${picks.map(item => item.candidate.title).join(' -> ')}`);
			      const launch = document.getElementById('childDispatch');
			      if (launch) launch.scrollIntoView({ block: 'nearest' });
			    }

			    async function launchRadarCandidate(candidateId, direction = 'right') {
			      const candidate = missionRadarItems.find(item => item.id === candidateId);
			      if (!candidate) return;
			      stageRadarCandidate(candidateId, { scroll: false });
			      setCommandStatus('deck', `Launching ${candidate.title} as a real child pane...`);
			      await startChildSession(direction, 'deck');
			    }

			    async function launchMissionDraft(direction = 'right') {
			      const picks = primaryMissionDraftCandidates();
			      if (!picks.length) {
			        setCommandStatus('deck', 'Run Research next work before launching a mission draft.');
			        return;
			      }
			      setActiveTab('panes');
			      setCommandDeckVisible(true);
			      setChromeToggle('show-command-advanced', 'deckAdvancedToggle', viewPreferenceKeys.commandAdvanced, true, { persist: false });
			      for (let index = 0; index < picks.length; index++) {
			        const { stage, candidate } = picks[index];
			        const dependency = missionDraftDependencyForPick(picks, index);
			        applyRadarCandidateToDeck(candidate, stage, {
			          dependency,
			          dependencyLabel: dependency || 'parallel'
			        });
			        setCommandStatus('deck', `Launching mission draft ${index + 1}/${picks.length}: ${candidate.title}`);
			        await startChildSession(direction, 'deck');
			      }
			      setPaneWall(true);
			      setCommandStatus('deck', `Mission draft launched: ${picks.length} real child pane${picks.length === 1 ? '' : 's'} created from Radar.`);
			    }

			    async function scanMissionRadar() {
			      if (missionRadarScanInFlight) return;
			      missionRadarScanInFlight = true;
			      renderDerivedBoards();
			      bindWaveInteractions();
			      const parent = parentWave() || waves[selectedWaveId] || Object.values(waves)[0];
			      const params = new URLSearchParams();
			      if (parent?.cwd) params.set('cwd', parent.cwd);
			      try {
			        const response = await fetch(`/mission/radar?${params.toString()}`, { cache: 'no-store' });
			        const payload = await response.json();
			        if (!response.ok || payload.error) throw new Error(payload.error?.message || 'radar scan failed');
			        missionRadarScan = payload.result || null;
			        writeJsonPreference(viewPreferenceKeys.missionRadarScan, missionRadarScan);
			      } catch (error) {
			        missionRadarScan = {
			          px: {
			            available: false,
			            ran: false,
			            ok: false,
			            error: error.message || 'radar scan failed'
			          },
			          candidates: []
			        };
			        writeJsonPreference(viewPreferenceKeys.missionRadarScan, missionRadarScan);
			      } finally {
			        missionRadarScanInFlight = false;
			        renderDerivedBoards();
			        bindWaveInteractions();
			      }
			    }

				    function renderMissionBrief({
				      parent,
			      children,
			      contracts,
		      readyPackets,
		      attention,
		      readyForAcceptance,
		      blockedGates,
		      primaryChanges,
		      nextAction,
		      nextActionControls
		    }) {
		      const container = document.getElementById('missionBrief');
		      if (!container) return;
		      const parentTitle = document.getElementById('parentTitle')?.textContent || parent?.title || 'Herdr mission';
		      const sweep = lastMissionSweep?.summary || {};
		      const sweepText = lastMissionSweep
		        ? `${sweep.ready_packets || 0}/${sweep.ingested || 0} packets ready; ${sweep.needs_attention || 0} sweep blocker${Number(sweep.needs_attention || 0) === 1 ? '' : 's'}; ${sweep.failed || 0} failed reads.`
		        : 'No live sweep has run yet.';
		      const decisionTone = attention.length || readyForAcceptance.length || blockedGates.length ? 'warn' : 'good';
		      const packetText = contracts.length
		        ? `${readyPackets.length}/${contracts.length} child packets complete.`
		        : 'No child contracts attached yet.';
		      container.innerHTML = `
		        <section class="mission-brief-card primary">
		          <div class="mission-brief-kicker">Mission command brief</div>
		          <strong>${escapeHtml(parentTitle)}</strong>
		          <p>${escapeHtml(children.length)} child pane${children.length === 1 ? '' : 's'} live; ${escapeHtml(contracts.length)} contract${contracts.length === 1 ? '' : 's'} attached. ${escapeHtml(packetText)}</p>
		          <div class="mission-brief-chips">
		            <span class="ops-pill good">${escapeHtml(children.length)} live</span>
		            <span class="ops-pill ${contracts.length === children.length && children.length ? 'good' : 'warn'}">${escapeHtml(contracts.length)}/${escapeHtml(children.length)} contracts</span>
		          </div>
		          <div class="mission-brief-actions"><button class="mini-action primary" data-radar-scan>research next work</button><button class="mini-action" data-room-jump="panes">open panes</button></div>
		        </section>
		        <section class="mission-brief-card">
		          <div class="mission-brief-kicker">Next parent decision</div>
		          <strong>${escapeHtml(nextAction)}</strong>
		          <p>Start here before reading tables. Clear decisions, accept packets, or jump to the evidence that proves the mission.</p>
		          <div class="mission-brief-actions"><span class="ops-pill ${decisionTone}">${attention.length || readyForAcceptance.length || blockedGates.length ? 'decision' : 'clear'}</span>${nextActionControls}</div>
		        </section>
		        <section class="mission-brief-card">
		          <div class="mission-brief-kicker">Sweep truth</div>
		          <strong>${escapeHtml(sweepText)}</strong>
		          <p>The sweep reads child panes and report packets; parent decisions stay separate so stale terminal reads do not masquerade as governance.</p>
		        </section>
		        <section class="mission-brief-card">
		          <div class="mission-brief-kicker">Operational policy</div>
		          <strong>Contracts first, receipts required.</strong>
		          <p>PX is optional when present; wave contracts and packets are required for review gates. Dependencies wait for accepted packets.</p>
		          <div class="mission-brief-chips"><span class="ops-pill">${escapeHtml(primaryChanges.length)} changed</span><span class="ops-pill good">px optional</span></div>
			        </section>`;
			    }

		    function renderReviewBrief({
		      attention,
		      readyForAcceptance,
		      blockedPacketCount,
		      contracts,
		      readyPackets,
		      blockedGates,
		      primaryChanges,
		      localOnlyChanges,
		      nextAction,
		      nextActionControls
		    }) {
		      const container = document.getElementById('reviewBrief');
		      if (!container) return;
		      const decisionCount = attention.length;
		      const verdictTone = decisionCount || readyForAcceptance.length || blockedGates.length ? 'warn' : 'good';
		      const verdict = decisionCount
		        ? `${decisionCount} parent decision${decisionCount === 1 ? '' : 's'} waiting`
		        : readyForAcceptance.length
		          ? `${readyForAcceptance.length} packet${readyForAcceptance.length === 1 ? '' : 's'} ready for acceptance`
		          : blockedGates.length
		            ? `${blockedGates.length} dependency gate${blockedGates.length === 1 ? '' : 's'} blocked`
		            : 'Review room clear';
		      const packetMissing = Math.max(0, contracts.length - readyPackets.length);
		      container.innerHTML = `
		        <section class="mission-brief-card primary">
		          <div class="mission-brief-kicker">Review command brief</div>
		          <strong>${escapeHtml(verdict)}</strong>
		          <p>Primary verdict: ${escapeHtml(nextAction)}. Make the parent decision, then audit evidence before accepting the mission.</p>
		          <div class="mission-brief-actions"><span class="ops-pill ${verdictTone}">verdict</span>${nextActionControls}</div>
		        </section>
		        <section class="mission-brief-card">
		          <div class="mission-brief-kicker">Done court</div>
		          <strong>${escapeHtml(readyForAcceptance.length)} ready; ${escapeHtml(blockedPacketCount)} blocked.</strong>
		          <p>${escapeHtml(packetMissing)} packet${packetMissing === 1 ? '' : 's'} still missing required fields. Parent decisions block acceptance separately from sweep blockers.</p>
		          <div class="mission-brief-actions">${roomJumpButton('audit', 'audit')}</div>
		        </section>
		        <section class="mission-brief-card">
		          <div class="mission-brief-kicker">Evidence readiness</div>
		          <strong>${escapeHtml(primaryChanges.length)} source change${primaryChanges.length === 1 ? '' : 's'} visible.</strong>
		          <p>${escapeHtml(contracts.length)} contract${contracts.length === 1 ? '' : 's'} tracked; ${escapeHtml(localOnlyChanges.length)} local/session artifact${localOnlyChanges.length === 1 ? '' : 's'} separated.</p>
		          <div class="mission-brief-actions">${roomJumpButton('evidence', 'evidence')} ${roomJumpButton('changes', 'changes')}</div>
		        </section>
		        <section class="mission-brief-card">
		          <div class="mission-brief-kicker">Gate status</div>
		          <strong>${escapeHtml(blockedGates.length)} dependency gate${blockedGates.length === 1 ? '' : 's'} blocked.</strong>
		          <p>Dependencies wait for accepted upstream packets. Timeline keeps the receipt trail when a decision changes.</p>
		          <div class="mission-brief-actions">${roomJumpButton('timeline', 'timeline')}</div>
		        </section>`;
		    }

		    function attentionTone(kind) {
		      if (kind === 'error') return 'bad';
		      if (kind === 'missing_packet' || kind === 'missing_contract') return 'warn';
		      return '';
		    }

		    function attentionRowsMarkup(fallbackRows) {
		      const items = Array.isArray(lastMissionSweep?.attention) ? lastMissionSweep.attention : [];
	      if (!items.length) return fallbackRows || emptyRow('No sweep blockers from latest child read.');
		      return items.map(item => {
		        const wave = waves[item.pane_id];
		        const missing = Array.isArray(item.missing_items) ? item.missing_items : [];
		        const detail = missing.length
		          ? `${item.message}: ${missing.slice(0, 4).join(', ')}${missing.length > 4 ? ', ...' : ''}`
		          : item.message || 'needs parent attention';
		        const actions = wave
		          ? `${selectButton(wave)} ${readButton(wave)} ${missingButton(wave)}`
		          : '';
		        return `<div class="ops-row"><span><strong>${escapeHtml(item.title || item.pane_id)}</strong><div class="ops-sub">${escapeHtml(detail)}</div></span><span class="ops-pill ${attentionTone(item.kind)}">${escapeHtml(labelFromSnake(item.kind, 'attention'))}</span><span>${actions}</span></div>`;
		      }).join('');
		    }

		    function renderDerivedBoards() {
		      const list = childWaves();
		      const allPanes = allPaneWaves();
		      const parent = parentWave();
	      const projectBoard = document.getElementById('projectBoard');
	      const reviewDecisionRoom = document.getElementById('reviewDecisionRoom');
	      const evidenceBoard = document.getElementById('evidenceBoard');
	      const changesBoard = document.getElementById('changesBoard');
	      const auditBoard = document.getElementById('auditBoard');
	      const timelineBoard = document.getElementById('timelineBoard');
	      const contracts = list.filter(wave => wave.pane.wave_contract);
	      const blocked = list.filter(wave => wave.status.includes('blocked') || wave.status.includes('needs'));
	      const writeCapable = list.filter(wave => ['write', 'draft-only'].includes(wave.mode));
	      const readyPackets = contracts.filter(wave => !missingPacketFields(wave).length);
	      const readyForAcceptance = packetReviewItems();
	      const blockedPacketCount = readyPackets.filter(wave => packetReviewGateReason(wave)).length;
	      const attention = attentionInboxItems();
	      const gates = contracts.map(wave => ({ wave, gate: dependencyGateForWave(wave) }));
	      const blockedGates = gates.filter(item => item.gate.status !== 'ready');
	      const modes = list.reduce((counts, wave) => {
	        counts[wave.mode] = (counts[wave.mode] || 0) + 1;
	        return counts;
		      }, {});
			      const modeSummary = Object.entries(modes).map(([mode, count]) => `${mode}: ${count}`).join('; ');
			      const gitEntries = allGitEntries();
			      const primaryChanges = gitEntries.filter(entry => !localOnlyPath(entry.path));
			      const localOnlyChanges = gitEntries.filter(entry => localOnlyPath(entry.path));
			      const gitBranches = [...new Set(Object.values(gitStatuses).map(status => status.branch).filter(Boolean))];
			      const localMissionRadarItems = buildMissionRadarItems({
			        list,
			        contracts,
			        readyPackets,
			        attention,
			        readyForAcceptance,
			        blockedGates,
			        primaryChanges,
			        localOnlyChanges
			      });
			      missionRadarItems = mergeMissionRadarItems(
			        scannedMissionRadarItems(),
			        localMissionRadarItems
			      );

	      projectBoard.className = 'mission-state-room ops-grid';
	      evidenceBoard.className = 'ops-grid';
	      changesBoard.className = 'ops-grid';
	      auditBoard.className = 'ops-grid';
	      timelineBoard.className = 'ops-grid';

	      const nextAction = attention.length
	        ? `Check ${attention[0].wave.title}: ${liveDecisionLabel(attention[0].reason)}`
	        : readyForAcceptance.length
	          ? `Accept or reject ${readyForAcceptance[0].title}`
	          : blockedGates.length
	            ? `Resolve ${blockedGates[0].wave.title} gate`
	            : primaryChanges.length
	              ? 'Review changed files'
	              : list.length
	                ? 'Mission review is clear'
	                : 'Create child panes';
	      const nextActionControls = attention.length
	        ? `${selectButton(attention[0].wave)} ${readButton(attention[0].wave)} ${missingButton(attention[0].wave, 'deck')}`
	        : readyForAcceptance.length
	          ? `${statusButton(readyForAcceptance[0], 'accepted', 'accept packet')} ${statusButton(readyForAcceptance[0], 'needs_review', 'send back')}`
	          : blockedGates.length
	            ? `${selectButton(blockedGates[0].wave)} ${roomJumpButton('audit', 'open audit')}`
	            : primaryChanges.length
	              ? roomJumpButton('changes', 'open changes')
	              : list.length
	                ? roomJumpButton('timeline', 'open timeline')
	                : roomJumpButton('panes', 'open focus');
	      const liveActionControls = attention.length
	        ? `${selectButton(attention[0].wave, 'inspect child')} ${readButton(attention[0].wave, 'read output')} ${missingButton(attention[0].wave, 'deck', 'request packet')}`
	        : readyForAcceptance.length
	          ? `${statusButton(readyForAcceptance[0], 'accepted', 'accept packet')} ${statusButton(readyForAcceptance[0], 'needs_review', 'send back')}`
	          : blockedGates.length
	            ? `${selectButton(blockedGates[0].wave, 'inspect gate')} ${roomJumpButton('audit', 'audit gates')}`
	            : primaryChanges.length
	              ? roomJumpButton('changes', 'review changes')
	              : list.length
	                ? roomJumpButton('timeline', 'open timeline')
	                : roomJumpButton('panes', 'open focus');
	      renderLiveCommandStrip({
	        attention,
	        readyForAcceptance,
	        contracts,
	        readyPackets,
	        primaryChanges,
	        nextAction,
	        actionControls: liveActionControls
	      });
	      renderMissionBrief({
	        parent,
	        children: list,
	        contracts,
	        readyPackets,
	        attention,
	        readyForAcceptance,
	        blockedGates,
	        primaryChanges,
	        nextAction,
	        nextActionControls
	      });
	      renderReviewBrief({
	        attention,
	        readyForAcceptance,
	        blockedPacketCount,
	        contracts,
	        readyPackets,
	        blockedGates,
	        primaryChanges,
	        localOnlyChanges,
	        nextAction,
	        nextActionControls
	      });
	      const decisionRows = [
	        `<div class="ops-row"><span><strong>Next parent action</strong><div class="ops-sub">${escapeHtml(nextAction)}</div></span><span class="ops-pill ${attention.length || readyForAcceptance.length || blockedGates.length ? 'warn' : 'good'}">${attention.length || readyForAcceptance.length || blockedGates.length ? 'decide' : 'clear'}</span><span>${nextActionControls}</span></div>`,
	        `<div class="ops-row"><span><strong>Packet court</strong><div class="ops-sub">${readyForAcceptance.length} ready for acceptance; ${blockedPacketCount} blocked by parent decision; ${contracts.length - readyPackets.length} still missing required fields.</div></span><span class="ops-pill ${readyForAcceptance.length ? 'warn' : blockedPacketCount ? 'warn' : 'good'}">${readyForAcceptance.length}/${contracts.length}</span><span>${roomJumpButton('audit', 'audit')}</span></div>`,
	        `<div class="ops-row"><span><strong>Evidence coverage</strong><div class="ops-sub">${contracts.length} contract${contracts.length === 1 ? '' : 's'}; ${primaryChanges.length} source file${primaryChanges.length === 1 ? '' : 's'} changed; ${localOnlyChanges.length} local artifact${localOnlyChanges.length === 1 ? '' : 's'} separated.</div></span><span class="ops-pill ${primaryChanges.length ? 'warn' : 'good'}">${primaryChanges.length}</span><span>${roomJumpButton('evidence', 'evidence')} ${roomJumpButton('changes', 'changes')}</span></div>`
	      ].join('');
	      const reviewReadyRows = readyForAcceptance.length
	        ? readyForAcceptance.map(wave => paneOpsRow(wave, `packet ${wave.packetLabel}; ready for parent accept/review`, `${statusButton(wave, 'accepted', 'accept packet')} ${statusButton(wave, 'needs_review', 'send back')} ${selectButton(wave)}`)).join('')
	        : emptyRow('No child packets are ready for acceptance.');
	      const reviewBlockedRows = readyPackets
	        .map(wave => ({ wave, reason: packetReviewGateReason(wave) }))
	        .filter(item => item.reason)
	        .map(item => paneOpsRow(item.wave, item.reason.detail, `${selectButton(item.wave)} ${readButton(item.wave)} ${missingButton(item.wave, 'deck')}`))
	        .join('');
		      const reviewGateRows = blockedGates.length
		        ? blockedGates.map(item => paneOpsRow(item.wave, `${item.gate.dependency || item.wave.depends}: ${item.gate.reason || dependencyGateLabel(item.gate)}`, `<span class="ops-pill ${dependencyGateTone(item.gate.status)}">${escapeHtml(dependencyGateLabel(item.gate))}</span> ${selectButton(item.wave)}`)).join('')
		        : emptyRow('No blocked dependency gates.');
		      const attentionDetailRows = attentionRowsMarkup(emptyRow('No sweep blockers from latest child read.'));
		      reviewDecisionRoom.innerHTML = `<div class="ops-grid">` + [
		        reviewDocketCard({
		          nextAction,
		          nextActionControls,
		          attention,
		          readyForAcceptance,
		          blockedGates,
		          contracts,
		          readyPackets,
		          primaryChanges
		        }),
		        opsCard('Decision queue', 'What needs the parent now', decisionRows + attentionDetailRows, true),
		        opsCard('Done court', 'Packets ready for acceptance', reviewReadyRows + (reviewBlockedRows || ''), true),
		        opsCard('Dependency gates', 'What is holding downstream waves', reviewGateRows),
	        opsCard('Evidence map', 'Where to inspect receipts and changes', `<div class="ops-row"><span><strong>Review tabs</strong><div class="ops-sub">Use Evidence for claims, Changes for blast radius, Audit for gates, Timeline for receipts.</div></span><span class="ops-pill good">mapped</span><span>${roomJumpButton('evidence', 'evidence')} ${roomJumpButton('changes', 'changes')} ${roomJumpButton('timeline', 'timeline')}</span></div>`)
	      ].join('') + `</div>`;

		      const parentRow = parent
		        ? paneOpsRow(parent, `parent terminal; ${parent.terminal}; ${parent.cwd || 'unknown cwd'}`, selectButton(parent, 'focus parent'))
		        : emptyRow('No parent pane visible yet.');
		      const dispatchRows = list.length
		        ? parentRow + list.map(wave => paneOpsRow(wave, `${wave.mode}; ${wave.depends === 'none' ? 'no dependency' : wave.depends}; ${wave.terminal}`, `${selectButton(wave)} ${readButton(wave)} ${contractButton(wave)}`)).join('')
		        : parentRow + emptyRow('No child panes yet.');
		      const blockedRows = blocked.length
		        ? blocked.map(wave => paneOpsRow(wave, `needs parent review; packet ${wave.packetLabel}; ${wave.depends}`, `${selectButton(wave)} ${readButton(wave)} ${ingestButton(wave)} ${missingButton(wave)}`)).join('')
		        : emptyRow('No child needs parent attention.');
		      const attentionRows = attentionRowsMarkup(blockedRows);
		      const governanceRows = [
		        `<div class="ops-row"><span><strong>Contracts</strong><div class="ops-sub">${contracts.length} of ${list.length} children have mission contracts.</div></span><span class="ops-pill ${contracts.length === list.length && list.length ? 'good' : 'warn'}">${contracts.length}/${list.length}</span><span></span></div>`,
		        `<div class="ops-row"><span><strong>Packets</strong><div class="ops-sub">${readyPackets.length} complete; dependencies should wait for accepted packets.</div></span><span class="ops-pill ${readyPackets.length === contracts.length && contracts.length ? 'good' : 'warn'}">${readyPackets.length}/${contracts.length}</span><span></span></div>`,
		        `<div class="ops-row"><span><strong>Repo evidence</strong><div class="ops-sub">${primaryChanges.length} changed file${primaryChanges.length === 1 ? '' : 's'} visible from git${gitBranches.length ? ` on ${gitBranches.join(', ')}` : ''}.</div></span><span class="ops-pill ${primaryChanges.length ? 'warn' : 'good'}">${primaryChanges.length}</span><span></span></div>`,
		        `<div class="ops-row"><span><strong>Modes</strong><div class="ops-sub">${modeSummary || 'No child modes available yet.'}</div></span><span class="ops-pill">${writeCapable.length} write-capable</span><span></span></div>`
		      ].join('');
			      projectBoard.innerHTML = [
			        renderMissionDraftCard(missionRadarItems),
			        renderProjectRadarCard(missionRadarItems),
			        renderMissionLifecycleBoard(list),
			        opsCard('Mission state', 'Parent and child pane queue', dispatchRows, true),
			        opsCard('Parent decisions', 'Local review queue', attentionRows),
			        opsCard('Governor', 'Scope and packet gates', governanceRows)
			      ].join('');

		      const proofReceiptRows = evidenceLedgerRows(12);
		      evidenceBoard.innerHTML = [
		        opsCard('Proof receipts', 'Launch, read, packet, sweep, and parent verdict receipts', proofReceiptRows, true),
		        list.length ? opsCard('Claims', 'Child evidence ledger', list.map(wave => {
		          const done = donePacketFields(wave);
		          const claim = wave.pane.wave_contract ? wave.arcs : 'No contract attached yet';
		          const receipts = done.length ? done.join(', ') : 'No receipts yet';
		          const files = gitEntriesForWave(wave).filter(entry => !localOnlyPath(entry.path)).length;
		          return paneOpsRow(wave, `Claim: ${claim}. Receipts: ${receipts}. Git files visible: ${files}.`, `<span class="ops-pill ${packetTone(wave)}">${escapeHtml(wave.packetLabel)}</span> ${readButton(wave)} ${ingestButton(wave)} ${selectButton(wave)}`);
		        }).join(''), true) : opsCard('Claims', 'Child evidence ledger', emptyRow('Create a child pane to start collecting evidence.'), true)
		      ].join('');

		      const changedRows = primaryChanges.length
		        ? primaryChanges.slice(0, 16).map(entry => `<div class="ops-row"><span><strong>${escapeHtml(entry.path)}</strong><div class="ops-sub">${escapeHtml(entry.cwd || '')}${entry.old_path ? `; from ${escapeHtml(entry.old_path)}` : ''}</div></span><span class="ops-pill ${entry.untracked ? 'warn' : ''}">${escapeHtml(gitCodeLabel(entry))}</span><span></span></div>`).join('')
		        : emptyRow('No git-visible source changes in pane workspaces.');
		      const localOnlyRows = localOnlyChanges.length
		        ? localOnlyChanges.slice(0, 8).map(entry => `<div class="ops-row"><span><strong>${escapeHtml(entry.path)}</strong><div class="ops-sub">local/session artifact; keep out of landing diff unless intentional</div></span><span class="ops-pill warn">${escapeHtml(gitCodeLabel(entry))}</span><span></span></div>`).join('')
		        : emptyRow('No local/session artifact changes detected.');
		      changesBoard.innerHTML = list.length ? [
		        opsCard('Git truth', 'Changed files visible to panes', changedRows, true),
		        opsCard('Blast radius', 'Predicted change lanes', list.map(wave => {
		          const blast = wave.blast === 'unknown' ? 'not declared' : wave.blast;
		          const tone = blast === 'none' ? 'good' : blast === 'minor' ? 'warn' : '';
		          const files = gitEntriesForWave(wave).filter(entry => !localOnlyPath(entry.path)).length;
		          return `<div class="ops-row"><span><strong>${escapeHtml(wave.title)}</strong><div class="ops-sub">${escapeHtml(wave.cwd || 'unknown cwd')}; ${files} git-visible files</div></span><span class="ops-pill ${tone}">${escapeHtml(blast)}</span><span>${selectButton(wave)}</span></div>`;
		        }).join(''), true),
		        opsCard('Conflict radar', 'Overlap watch', list.some(wave => wave.blast !== 'none')
		          ? list.filter(wave => wave.blast !== 'none').map(wave => paneOpsRow(wave, `${wave.mode}; blast radius ${wave.blast}; shared checkout has ${primaryChanges.length} git-visible file${primaryChanges.length === 1 ? '' : 's'}`, selectButton(wave))).join('')
		          : emptyRow('No declared blast-radius overlap.'), true)
		        ,
		        opsCard('Local artifacts', 'Ignored or session-side drift', localOnlyRows, true)
		      ].join('') : opsCard('Blast radius', 'Predicted change lanes', emptyRow('No change radar data until panes exist.'), true);

		      auditBoard.innerHTML = contracts.length ? [
		        opsCard('Sweep blockers', 'Parent inbox from pane sweep', attentionRows, true),
		        opsCard('Done court', 'Packet gates', contracts.map(wave => {
		          const missing = missingPacketFields(wave);
		          const packet = packetParts(wave);
		          const filesChangedMissing = missing.some(field => field.toLowerCase() === 'files changed');
		          const filesObserved = gitEntriesForWave(wave).filter(entry => !localOnlyPath(entry.path)).length;
		          const evidenceNote = filesObserved && filesChangedMissing ? `; shared checkout has ${filesObserved} files but packet lacks Files changed` : '';
		          const verdict = missing.length ? `missing ${missing.length}: ${missing.slice(0, 3).join(', ')}${missing.length > 3 ? ', ...' : ''}${evidenceNote}` : 'packet complete';
		          const action = missing.length
		            ? `${readButton(wave)} ${ingestButton(wave)} ${missingButton(wave)} ${statusButton(wave, 'needs_review', 'send back')}`
		            : `${statusButton(wave, 'accepted', 'accept packet')} ${statusButton(wave, 'needs_review', 'send back')}`;
		          return `<div class="ops-row"><span><strong>${escapeHtml(wave.title)}</strong><div class="ops-sub">${escapeHtml(verdict)}${progressMarkup(wave)}</div></span><span class="ops-pill ${packetTone(wave)}">${packet.done}/${packet.required}</span><span>${action}</span></div>`;
		        }).join(''), true),
		        opsCard('Dependency gates', 'What can run next', contracts.map(wave => {
		          const gate = dependencyGateForWave(wave);
		          const upstream = Array.isArray(gate.upstream_pane_ids) && gate.upstream_pane_ids.length
		            ? `; upstream ${gate.upstream_pane_ids.join(', ')}`
		            : '';
		          const detail = `${gate.dependency || wave.depends || 'parallel'}: ${gate.reason || dependencyGateLabel(gate)}${upstream}`;
		          return paneOpsRow(wave, detail, `<span class="ops-pill ${dependencyGateTone(gate.status)}">${escapeHtml(dependencyGateLabel(gate))}</span>`);
		        }).join(''), true)
		      ].join('') : [
		        opsCard('Sweep blockers', 'Parent inbox from pane sweep', attentionRows, true),
		        opsCard('Done court', 'Packet gates', emptyRow('Attach contracts to child panes before audit can decide anything useful.'), true)
		      ].join('');

			      const dispatchReceiptRows = dispatchEvents.length
			        ? dispatchEvents.map(event => `<div class="ops-row"><span><strong>${escapeHtml(event.at)} ${escapeHtml(event.target)}</strong><div class="ops-sub">${escapeHtml((event.receipts || []).map(receipt => `${receipt.ok ? 'ok' : 'fail'} ${receipt.pane_id}${receipt.error ? `: ${receipt.error}` : ''}`).join('; ') || 'no receipts')}</div></span><span class="ops-pill ${event.failed ? 'warn' : 'good'}">${escapeHtml(event.sent)}/${escapeHtml(event.requested)}</span><span></span></div>`).join('')
			        : emptyRow('No parent dispatch receipts yet.');
			      const outputRows = outputEvents.length
			        ? outputEvents.map(event => `<div class="ops-row"><span><strong>${escapeHtml(event.at)} ${escapeHtml(event.title || event.pane_id)}</strong><div class="ops-sub">${escapeHtml(event.last_nonempty_line || 'no output')}</div></span><span class="ops-pill">${escapeHtml(event.nonempty_line_count || 0)} lines</span><span></span></div>`).join('')
			        : emptyRow('No child output snapshots yet.');
			      const importRows = missionImportEvents.length
			        ? missionImportEvents.map(event => {
			          const assignmentSummary = (event.assignments || []).map(item => {
			            const action = missionImportActionMeta(item.planned_action || '').label;
			            const terminal = item.terminal_id ? ` / ${item.terminal_id}` : '';
			            return `${item.contract_title}: ${action} -> ${item.pane_id || 'new child pane'}${terminal} (${item.status})`;
			          }).join('; ') || event.path;
			          const pill = event.preview
			            ? `preview +${event.plannedCreated || 0} / reuse ${event.plannedReused || 0}`
			            : `${event.applied}/${event.contracts}${event.created ? `, +${event.created} panes` : ''}`;
			          return `<div class="ops-row"><span><strong>${escapeHtml(event.at)} ${escapeHtml(basename(event.path) || 'session file')}</strong><div class="ops-sub">${escapeHtml(assignmentSummary)}</div></span><span class="ops-pill ${event.missing ? 'warn' : 'good'}">${escapeHtml(pill)}</span><span></span></div>`;
			        }).join('')
			        : emptyRow('No mission session imports yet.');
			      timelineBoard.innerHTML = allPanes.length ? [
			        opsCard('Timeline', 'Current pane lifecycle', allPanes.map((wave, index) => {
			        const focus = wave.pane.focused ? 'focused now' : 'background';
			        return paneOpsRow(wave, `${index + 1}. ${focus}; ${wave.terminal}; packet ${wave.packetLabel}`, selectButton(wave));
			      }).join(''), true),
			        opsCard('Mission imports', 'Session files applied to panes', importRows, true),
			        opsCard('Proof receipts', 'Runtime proof trail', proofReceiptRows, true),
			        opsCard('Dispatch receipts', 'Parent-to-child messages', dispatchReceiptRows, true),
			        opsCard('Read snapshots', 'Parent-read child output', outputRows, true)
			      ].join('') : [
			        opsCard('Timeline', 'Current pane lifecycle', emptyRow('Pane lifecycle events will appear once the mission has panes.'), true),
			        opsCard('Proof receipts', 'Runtime proof trail', proofReceiptRows, true)
			      ].join('');
			    }

    function bindWaveInteractions() {
      document.querySelectorAll('[data-tree-create-child]').forEach(button => {
        if (button.dataset.bound === '1') return;
        button.dataset.bound = '1';
        button.addEventListener('click', event => {
          event.stopPropagation();
          startChildSession('right', 'tree');
        });
      });
      document.querySelectorAll('.mission-node[data-wave], .wave-node[data-wave]').forEach(node => {
        if (node.dataset.bound === '1') return;
	        node.dataset.bound = '1';
	        node.addEventListener('click', event => {
	          const disclosureClick = event.target.closest('.chev');
	          if (disclosureClick && node.dataset.toggle) {
	            event.preventDefault();
	            if (node.dataset.wave && waves[node.dataset.wave]) selectWave(node.dataset.wave, { reconnect: false });
	            toggleTree(node.dataset.toggle);
	            return;
	          }
          if (node.dataset.wave !== 'monitor') selectWave(node.dataset.wave);
        });
        node.addEventListener('dblclick', event => {
          event.preventDefault();
          openExpandedPane(node.dataset.wave);
        });
      });
      document.querySelectorAll('.wave-card[data-wave]').forEach(node => {
        if (node.dataset.bound === '1') return;
        node.dataset.bound = '1';
        node.addEventListener('click', () => {
          selectWave(node.dataset.wave);
          node.focus();
        });
        node.addEventListener('dblclick', () => {
          openExpandedPane(node.dataset.wave);
        });
      });
      document.querySelectorAll('[data-mission-lane-toggle]').forEach(button => {
        if (button.dataset.bound === '1') return;
        button.dataset.bound = '1';
        button.addEventListener('click', event => {
          event.stopPropagation();
          setMissionBoardLaneCollapsed(
            button.dataset.missionLaneToggle,
            button.getAttribute('aria-expanded') === 'true'
          );
        });
      });
	      document.querySelectorAll('.mission-board-card[data-wave]').forEach(node => {
	        if (node.dataset.bound === '1') return;
	        node.dataset.bound = '1';
	        node.addEventListener('click', event => {
          if (event.target.closest('button')) return;
          selectWave(node.dataset.wave);
        });
        node.addEventListener('keydown', event => {
          if (event.target.closest('button')) return;
          if (event.key !== 'Enter' && event.key !== ' ') return;
          event.preventDefault();
          selectWave(node.dataset.wave);
        });
        node.addEventListener('dblclick', event => {
	          if (event.target.closest('button')) return;
	          openExpandedPane(node.dataset.wave);
	        });
	      });
	      document.querySelectorAll('[data-radar-stage]').forEach(button => {
	        if (button.dataset.bound === '1') return;
	        button.dataset.bound = '1';
	        button.addEventListener('click', event => {
	          event.stopPropagation();
	          stageRadarCandidate(button.dataset.radarStage);
	        });
	      });
	      document.querySelectorAll('[data-radar-launch]').forEach(button => {
	        if (button.dataset.bound === '1') return;
	        button.dataset.bound = '1';
	        button.addEventListener('click', event => {
	          event.stopPropagation();
	          launchRadarCandidate(button.dataset.radarLaunch, 'right');
	        });
	      });
	      document.querySelectorAll('[data-radar-stage-draft]').forEach(button => {
	        if (button.dataset.bound === '1') return;
	        button.dataset.bound = '1';
	        button.addEventListener('click', event => {
	          event.stopPropagation();
	          stageMissionDraft();
	        });
	      });
	      document.querySelectorAll('[data-radar-launch-draft]').forEach(button => {
	        if (button.dataset.bound === '1') return;
	        button.dataset.bound = '1';
	        button.addEventListener('click', event => {
	          event.stopPropagation();
	          launchMissionDraft('right');
	        });
	      });
	      document.querySelectorAll('[data-radar-scan]').forEach(button => {
	        if (button.dataset.bound === '1') return;
	        button.dataset.bound = '1';
	        button.addEventListener('click', event => {
	          event.stopPropagation();
	          scanMissionRadar();
	        });
	      });
	      document.querySelectorAll('.pane-roster-row[data-wave]').forEach(node => {
	        if (node.dataset.bound === '1') return;
	        node.dataset.bound = '1';
        node.addEventListener('click', event => {
          if (event.target.closest('button')) return;
          selectWave(node.dataset.wave);
        });
        node.addEventListener('dblclick', () => {
          openExpandedPane(node.dataset.wave);
        });
      });
      document.querySelectorAll('[data-full]').forEach(button => {
        if (button.dataset.bound === '1') return;
        button.dataset.bound = '1';
        button.addEventListener('click', event => {
          event.stopPropagation();
          openExpandedPane(button.dataset.full);
        });
      });
      document.querySelectorAll('[data-select-wave]').forEach(button => {
        if (button.dataset.bound === '1') return;
        button.dataset.bound = '1';
        button.addEventListener('click', event => {
          event.stopPropagation();
          selectWave(button.dataset.selectWave);
        });
      });
      document.querySelectorAll('[data-contract-wave]').forEach(button => {
        if (button.dataset.bound === '1') return;
        button.dataset.bound = '1';
        button.addEventListener('click', event => {
          event.stopPropagation();
          selectWave(button.dataset.contractWave);
          loadContractPrompt();
        });
      });
      document.querySelectorAll('[data-missing-wave]').forEach(button => {
        if (button.dataset.bound === '1') return;
        button.dataset.bound = '1';
        button.addEventListener('click', event => {
          event.stopPropagation();
          selectWave(button.dataset.missingWave);
          loadMissingPacketPrompt(button.dataset.promptTarget || 'drawer');
        });
      });
      document.querySelectorAll('[data-message-wave]').forEach(button => {
        if (button.dataset.bound === '1') return;
        button.dataset.bound = '1';
        button.addEventListener('click', event => {
          event.stopPropagation();
          loadPaneMessagePrompt(button.dataset.messageWave, 'drawer');
        });
      });
      document.querySelectorAll('[data-ingest-wave]').forEach(button => {
        if (button.dataset.bound === '1') return;
        button.dataset.bound = '1';
        button.addEventListener('click', event => {
          event.stopPropagation();
          selectWave(button.dataset.ingestWave);
          ingestSelectedReportPacket(button.dataset.ingestWave);
        });
      });
      document.querySelectorAll('[data-read-wave]').forEach(button => {
        if (button.dataset.bound === '1') return;
        button.dataset.bound = '1';
        button.addEventListener('click', event => {
          event.stopPropagation();
          selectWave(button.dataset.readWave);
          readSelectedPaneOutput(button.dataset.readWave);
        });
      });
      document.querySelectorAll('[data-status-wave]').forEach(button => {
        if (button.dataset.bound === '1') return;
        button.dataset.bound = '1';
        button.addEventListener('click', event => {
          event.stopPropagation();
          selectWave(button.dataset.statusWave);
          setSelectedPaneStatus(button.dataset.status);
        });
      });
      document.querySelectorAll('[data-close-wave]').forEach(button => {
        if (button.dataset.bound === '1') return;
        button.dataset.bound = '1';
        button.addEventListener('click', event => {
          event.stopPropagation();
          closePane(button.dataset.closeWave);
        });
      });
	      document.querySelectorAll('[data-file-receipt]').forEach(button => {
	        if (button.dataset.bound === '1') return;
	        button.dataset.bound = '1';
	        button.addEventListener('click', event => {
	          event.stopPropagation();
	          loadFileReceiptPrompt(button.dataset.fileReceipt || 'drawer');
	        });
	      });
	      document.querySelectorAll('[data-room-jump]').forEach(button => {
	        if (button.dataset.bound === '1') return;
	        button.dataset.bound = '1';
	        button.addEventListener('click', event => {
	          event.stopPropagation();
	          setActiveTab(button.dataset.roomJump);
	        });
	      });
      document.querySelectorAll('[data-open-command-tray]').forEach(button => {
        if (button.dataset.bound === '1') return;
        button.dataset.bound = '1';
        button.addEventListener('click', event => {
          event.stopPropagation();
          setActiveTab('panes');
          setCommandDeckVisible(true);
        });
      });
	    }

    function setExpanded(expanded) {
      document.body.classList.toggle('terminal-expanded', expanded);
      if (expanded) {
        setTimeout(connect, 80);
        terminalWrap.focus();
      } else {
        if (focusSource) {
          focusSource.close();
          focusSource = null;
        }
        if (selectedWaveId && tileSources.has(selectedWaveId)) {
          tileSources.get(selectedWaveId).close();
          tileSources.delete(selectedWaveId);
        }
        reconnectStreamsForSelection();
        setRuntimeStatus(livePaneStatus());
      }
    }

	    const reviewTabNames = new Set(['review', 'evidence', 'changes', 'audit', 'timeline']);

	    function resolvePrimaryTab(tab) {
	      return tab;
	    }

    function tabGroup(tab) {
      return reviewTabNames.has(tab) ? 'review' : tab;
    }

    function updateReviewTabs(tab) {
      const reviewTabsNode = document.getElementById('reviewTabs');
      if (reviewTabsNode) reviewTabsNode.hidden = true;
      document.querySelectorAll('[data-review-tab]').forEach(node => {
        node.classList.toggle('active', node.dataset.reviewTab === tab);
      });
    }

    function setInspectorTab(tab) {
      const validTabs = new Set(['pane', 'command', 'output', 'packet']);
      const next = validTabs.has(tab) ? tab : 'pane';
      document.body.dataset.activeInspectorTab = next;
      document.querySelectorAll('[data-inspector-tab]').forEach(node => {
        node.classList.toggle('active', node.dataset.inspectorTab === next);
      });
      document.querySelectorAll('[data-inspector-panel]').forEach(node => {
        node.classList.toggle('active', node.dataset.inspectorPanel === next);
      });
    }

    function setActiveTab(tab) {
      const targetTab = resolvePrimaryTab(tab);
      const targetGroup = tabGroup(targetTab);
      if (targetTab !== 'panes' && document.body.classList.contains('pane-wall')) {
        setPaneWall(false, { reconnect: false });
      }
      if (targetTab !== 'panes') closeViewMenu();
      document.body.dataset.activeTab = targetTab;
      document.body.dataset.activeGroup = targetGroup;
      document.querySelectorAll('.tab').forEach(node => {
        const nodeTab = resolvePrimaryTab(node.dataset.tab);
        node.classList.toggle('active', tabGroup(nodeTab) === targetGroup);
      });
      document.querySelectorAll('.tab-page').forEach(node => node.classList.toggle('active', node.dataset.page === targetTab));
      updateReviewTabs(targetTab);
      if (targetTab === 'project') refreshMissionRoom({ quiet: true });
      if (targetTab === 'review') refreshReviewRoom({ quiet: true });
      if (targetTab === 'panes' && selectedWaveId) setTimeout(() => reconnectStreamsForSelection(), 80);
    }

    function setDensity(density, options = {}) {
      const next = validDensities.has(density) ? density : 'dense';
      document.body.dataset.density = next;
      document.querySelectorAll('.density-button[data-density]').forEach(button => {
        button.setAttribute('aria-pressed', String(button.dataset.density === next));
      });
      if (options.persist !== false) writePreference(viewPreferenceKeys.density, next);
      if (options.reconnect !== false && Object.keys(waves).length) {
        pauseTerminalStreams(`density ${next}`);
        setTimeout(() => reconnectStreamsForSelection(), 160);
      }
    }

	    function setPaneWall(enabled, options = {}) {
	      document.body.classList.toggle('pane-wall', enabled);
	      if (enabled) {
	        setChromeToggle('show-view-menu', 'viewMenuToggle', viewPreferenceKeys.viewMenu, false, { persist: false });
        setRegionVisible('hide-tree', 'treeToggle', viewPreferenceKeys.treeVisible, false, { persist: false });
        setRegionVisible('hide-inspector', 'detailsToggle', viewPreferenceKeys.detailsVisible, false, { persist: false });
	        setWallHudExpanded(false);
	      }
	      updateLiveModeSwitch(enabled);
	      syncWorkbenchDrawerButtons();
	      if (options.persist !== false) writePreference(viewPreferenceKeys.paneWall, enabled ? '1' : '0');
      if (enabled) setActiveTab('panes');
      updateWallHud();
      if (options.reconnect !== false && Object.keys(waves).length) {
        pauseTerminalStreams(enabled ? 'opening pane wall' : 'closing pane wall');
        setTimeout(() => reconnectStreamsForSelection(), 160);
      }
    }

    function setWallHudExpanded(enabled) {
      const button = document.getElementById('wallHudMore');
      const menuButton = document.getElementById('wallHudDetailsToggle');
      document.body.classList.toggle('wall-hud-expanded', enabled);
      if (button) {
        button.setAttribute('aria-pressed', String(enabled));
        button.textContent = enabled ? 'Hide intervention' : 'Intervene';
        button.title = enabled ? 'Hide intervention rail' : 'Open intervention rail';
      }
      if (menuButton) {
        menuButton.setAttribute('aria-pressed', String(enabled));
        menuButton.textContent = enabled ? 'Hide intervention' : 'Intervention rail';
        menuButton.title = enabled ? 'Hide intervention rail' : 'Open the all-pane intervention rail';
      }
    }

    function setRoomContextVisible(visible, options = {}) {
      document.body.classList.toggle('hide-room-context', !visible);
      document.querySelectorAll('[data-room-context-toggle]').forEach(button => {
        button.setAttribute('aria-pressed', String(visible));
        button.title = visible ? 'Hide the room context rail' : 'Show the room context rail';
      });
      if (options.persist !== false) {
        writePreference(viewPreferenceKeys.roomContextVisible, visible ? '1' : '0');
      }
    }

    function initializeViewPreferences() {
      setDensity(readPreference(viewPreferenceKeys.density, 'dense'), { persist: false, reconnect: false });
      missionRadarScan = readJsonPreference(viewPreferenceKeys.missionRadarScan, null);
      hydrateEvidenceLedger();
      missionBoardCollapsedLanes = loadMissionBoardCollapsedLanes();
      setWallHudExpanded(false);
      setRoomContextVisible(readPreference(viewPreferenceKeys.roomContextVisible, '0') === '1', { persist: false });
      setChromeToggle('show-view-menu', 'viewMenuToggle', viewPreferenceKeys.viewMenu, false, { persist: false });
      setRegionVisible('hide-tree', 'treeToggle', viewPreferenceKeys.treeVisible, readPreference(viewPreferenceKeys.treeVisible, '1') === '1', { persist: false });
      setRegionVisible('hide-inspector', 'detailsToggle', viewPreferenceKeys.detailsVisible, false, { persist: false });
      setChromeToggle('show-command-deck', 'controlsToggle', viewPreferenceKeys.commandDeck, false, { persist: false });
      setChromeToggle('show-command-advanced', 'deckAdvancedToggle', viewPreferenceKeys.commandAdvanced, false, { persist: false });
      setChromeToggle('show-roster', 'rosterToggle', viewPreferenceKeys.roster, false, { persist: false });
      setChromeToggle('show-inspector-advanced', 'inspectorAdvancedToggle', viewPreferenceKeys.inspectorAdvanced, false, { persist: false });
      setPaneWall(readPreference(viewPreferenceKeys.paneWall, '0') === '1', { persist: false, reconnect: false });
    }

	    function setRegionVisible(className, buttonId, preferenceKey, visible, options = {}) {
	      document.body.classList.toggle(className, !visible);
	      const button = document.getElementById(buttonId);
	      if (button) button.setAttribute('aria-pressed', String(visible));
	      if (options.persist !== false) writePreference(preferenceKey, visible ? '1' : '0');
	      syncWorkbenchDrawerButtons();
	    }

    function toggleRegionVisible(className, buttonId, preferenceKey) {
      setRegionVisible(className, buttonId, preferenceKey, document.body.classList.contains(className));
    }

    function openInspectorDrawer(tab = 'pane') {
      setRegionVisible('hide-inspector', 'detailsToggle', viewPreferenceKeys.detailsVisible, true, { persist: false });
      setInspectorTab(tab);
      closeViewMenu();
    }

    function closeTreeDrawer() {
      setRegionVisible('hide-tree', 'treeToggle', viewPreferenceKeys.treeVisible, false, { persist: false });
      closeViewMenu();
    }

    function closeInspectorDrawer() {
      setRegionVisible('hide-inspector', 'detailsToggle', viewPreferenceKeys.detailsVisible, false, { persist: false });
      setChromeToggle('show-inspector-advanced', 'inspectorAdvancedToggle', viewPreferenceKeys.inspectorAdvanced, false, { persist: false });
    }

	    function closeOpenPaneDrawers() {
	      let closed = false;
	      if (!document.body.classList.contains('hide-tree')) {
	        closeTreeDrawer();
        closed = true;
      }
      if (!document.body.classList.contains('hide-inspector')) {
        closeInspectorDrawer();
        closed = true;
      }
	      if (closed) closeViewMenu();
	      return closed;
	    }

	    function closeOpenPaneWallDrawers() {
	      if (!document.body.classList.contains('pane-wall')) return false;
	      return closeOpenPaneDrawers();
	    }

    function closeOpenWallHud() {
      if (!document.body.classList.contains('wall-hud-expanded')) return false;
      setWallHudExpanded(false);
      closeViewMenu();
      return true;
    }

    function closeOpenViewMenu() {
      if (!document.body.classList.contains('show-view-menu')) return false;
      closeViewMenu();
      return true;
    }

	    function setChromeToggle(className, buttonId, preferenceKey, enabled, options = {}) {
	      document.body.classList.toggle(className, enabled);
	      const button = document.getElementById(buttonId);
	      if (button) button.setAttribute('aria-pressed', String(enabled));
	      if (options.persist !== false) writePreference(preferenceKey, enabled ? '1' : '0');
	      syncWorkbenchDrawerButtons();
	    }

    function toggleChrome(className, buttonId, preferenceKey, options = {}) {
      setChromeToggle(className, buttonId, preferenceKey, !document.body.classList.contains(className), options);
    }

	    function closeViewMenu() {
	      setChromeToggle('show-view-menu', 'viewMenuToggle', viewPreferenceKeys.viewMenu, false, { persist: false });
	    }

	    function syncWorkbenchDrawerButtons() {
	      const sync = (id, pressed) => {
	        const button = document.getElementById(id);
	        if (button) button.setAttribute('aria-pressed', String(pressed));
	      };
	      sync('treeQuickToggle', !document.body.classList.contains('hide-tree'));
	      sync('detailsQuickToggle', !document.body.classList.contains('hide-inspector'));
	      sync('commandQuickToggle', document.body.classList.contains('show-command-deck'));
	    }

    function openExpandedPane(id) {
      if (!id || !waves[id]) return;
      selectWave(id);
      setExpanded(true);
    }

    function setCommandDeckVisible(enabled) {
      setChromeToggle('show-command-deck', 'controlsToggle', viewPreferenceKeys.commandDeck, enabled, { persist: false });
      if (!enabled) {
        setChromeToggle('show-command-advanced', 'deckAdvancedToggle', viewPreferenceKeys.commandAdvanced, false, { persist: false });
      }
    }

	    function allPanesButtonLabel() {
	      const count = Object.keys(waves).length;
	      return count ? `All panes (${count})` : 'All panes';
	    }

    function updateLiveModeSwitch(enabled = document.body.classList.contains('pane-wall')) {
      const focusButton = document.getElementById('focusModeToggle');
      const gridButton = document.getElementById('paneWallToggle');
      if (focusButton) {
        focusButton.setAttribute('aria-pressed', String(!enabled));
        focusButton.title = enabled
          ? 'Return to one watched pane'
          : 'The workbench is showing one watched pane';
      }
      if (gridButton) {
        gridButton.setAttribute('aria-pressed', String(enabled));
        gridButton.textContent = allPanesButtonLabel();
        gridButton.title = enabled
          ? 'All parent/child terminal panes are visible'
          : 'Show every live parent/child terminal pane';
      }
    }

    function updatePaneWallButton() {
      updateLiveModeSwitch();
    }

	    function updateDeckSummary(childCount = childWaves().length) {
	      const deckSummary = document.getElementById('deckSummary');
	      if (!deckSummary) return;
	      const selected = selectedWaveId && waves[selectedWaveId] ? waves[selectedWaveId] : null;
	      const selectedText = selected
	        ? `Watching ${selected.title} (${selected.role})`
	        : 'No pane watched';
	      deckSummary.textContent = `${selectedText}; ${childCount} child pane${childCount === 1 ? '' : 's'} live; parent can read, intervene, broadcast, or stop children.`;
	    }

		    async function loadPanes(preferredPaneId = null) {
	      setRuntimeStatus('loading panes', false);
	      const selectedParam = preferredPaneId || selectedWaveId || '';
	      const workroomUrl = selectedParam
	        ? `/mission/workroom?selected=${encodeURIComponent(selectedParam)}`
	        : '/mission/workroom';
	      const [workspaceResponse, paneResponse, workroomResponse] = await Promise.all([
	        fetch('/workspaces', { cache: 'no-store' }),
	        fetch('/panes', { cache: 'no-store' }),
	        fetch(workroomUrl, { cache: 'no-store' })
	      ]);
	      const workspacePayload = await workspaceResponse.json();
	      const payload = await paneResponse.json();
	      const workroomPayload = await workroomResponse.json();
	      if (workspacePayload.error) throw new Error(workspacePayload.error.message || 'workspace list failed');
	      if (payload.error) throw new Error(payload.error.message || 'pane list failed');
	      if (workroomPayload.error) throw new Error(workroomPayload.error.message || 'workroom projection failed');
	      workspaces = workspacePayload.result?.workspaces || [];
	      workroomProjection = workroomPayload.result?.workroom || null;
	      const panes = payload.result?.panes || [];
	      const projectLabel = currentProjectLabel(panes);
	      document.getElementById('brandLabel').textContent = `Herdr Workroom - ${projectLabel}`;
	      document.getElementById('projectNodeLabel').textContent = projectLabel;
	      const workroomById = workroomPaneMap(workroomProjection);
	      const childPaneIds = new Set((workroomProjection?.children || []).map(pane => pane.pane_id));
	      const waveList = panes.map((pane, index) => applyWorkroomProjection(
	        paneToWave(pane, index),
	        workroomById.get(pane.pane_id)
	      ));
	      const parentCandidate =
	        (workroomProjection?.parent?.pane_id
	          ? waveList.find(wave => wave.id === workroomProjection.parent.pane_id)
	          : null)
	        || waveList.find(wave => wave.isRoot && wave.pane.focused)
	        || waveList.find(wave => wave.isRoot)
	        || waveList[0]
	        || null;
	      parentPaneId = parentCandidate?.id || null;
	      waveList.forEach((wave, index) => {
	        wave.role = parentPaneId && wave.id === parentPaneId
	          ? 'parent'
	          : childPaneIds.has(wave.id)
	            ? 'child'
	            : 'child';
	        if (wave.role === 'parent') {
	          wave.title = 'Parent session';
	        } else if (wave.role === 'child' && wave.title === 'Parent session') {
	          wave.title = `Child pane ${index + 1}`;
	        }
	      });
	      const stats = workroomProjection?.stats;
	      const childCount = Number(stats?.child_count ?? Math.max(0, waveList.length - (parentPaneId ? 1 : 0)));
	      document.getElementById('parentTitle').textContent = projectLabel;
	      const contractCount = Number(stats?.attached_contracts ?? waveList.filter(wave => wave.role === 'child' && wave.pane.wave_contract).length);
	      const needsReview = waveList.filter(wave => {
	        if (wave.role !== 'child') return false;
	        const pane = wave.pane;
	        const status = labelFromSnake(pane.wave_contract?.status || pane.custom_status || pane.agent_status, '');
	        return status.includes('needs') || status.includes('blocked');
	      }).length;
	      const attentionCount = Number(stats?.needs_attention ?? needsReview);
	      const parentDecisionWord = attentionCount === 1 ? '' : 's';
	      document.getElementById('parentSummary').textContent = waveList.length
	        ? `Parent pane ${parentPaneId || 'unknown'} with ${childCount} child pane${childCount === 1 ? '' : 's'} live; ${contractCount} child contract${contractCount === 1 ? '' : 's'}; ${attentionCount} parent decision${parentDecisionWord}.`
	        : 'No panes are currently visible in this project session.';
	      waves = Object.fromEntries(waveList.map(wave => [wave.id, wave]));
	      await loadGitStatuses(Object.values(waves));
	      selectedWaveId = preferredPaneId && waves[preferredPaneId]
	        ? preferredPaneId
	        : workroomProjection?.selected_pane_id && waves[workroomProjection.selected_pane_id]
	          ? workroomProjection.selected_pane_id
	        : selectedWaveId && waves[selectedWaveId]
	          ? selectedWaveId
	          : parentPaneId || Object.keys(waves)[0] || null;
	      updateDeckSummary(childCount);
	      updatePaneWallButton();
	      updateDeckScopeSummary();
	      renderMissionTree();
	      renderWaveGrid();
	      updateMetrics();
	      renderMissionPulse();
	      updateWallPulse();
	      updateWallDag();
	      renderPaneRoster();
	      renderAttentionInbox();
	      renderPacketReviewQueue();
	      renderChangeRadar();
	      renderDerivedBoards();
	      bindWaveInteractions();
      if (selectedWaveId) {
        selectWave(selectedWaveId, { reconnect: false });
        reconnectStreamsForSelection();
        setRuntimeStatus(runtimeStatusForWave(waves[selectedWaveId]));
      } else {
        terminalTitle.textContent = 'No Herdr pane selected';
        terminalTitle.title = '';
        terminalStatus.textContent = 'offline';
        setRuntimeStatus('no panes', false);
      }
    }

    function selectWave(id, options = {}) {
      const wave = waves[id];
      if (!wave) return;
      const previousId = selectedWaveId;
      selectedWaveId = id;
      document.querySelectorAll('[data-wave]').forEach(node => {
        node.classList.toggle('active', node.dataset.wave === id);
      });
	      document.getElementById('selectedTitle').textContent = `Watching ${wave.title} (${wave.role})`;
	      document.getElementById('selectedPane').textContent = wave.paneId;
	      document.getElementById('selectedTerminal').textContent = wave.terminal;
	      document.getElementById('selectedWorkspace').textContent = wave.workspaceId;
	      document.getElementById('selectedTab').textContent = wave.tabId;
	      document.getElementById('selectedMode').textContent = `${wave.role}; ${wave.mode}`;
	      document.getElementById('selectedStatus').textContent = wave.status;
	      document.getElementById('selectedDepends').textContent = wave.depends;
	      document.getElementById('selectedBlast').textContent = wave.blast;
	      document.getElementById('selectedArcs').textContent = wave.arcs;
	      document.getElementById('controlTarget').textContent = `watching ${wave.paneId}`;
	      updateSelectedDispatchStatus();
	      updateDeckSummary(childWaves().length);
	      updateDeckScopeSummary();
	      updateWallHud();
	      terminalTitle.textContent = focusedTerminalTitle(wave);
	      terminalTitle.title = paneDebugTitle(wave);
	      renderSelectedOutput(id);
      const done = new Set(wave.packet.map(item => item.toLowerCase()));
      const list = document.getElementById('packetList');
      list.innerHTML = '';
	      packetFields.forEach(field => {
	        const item = document.createElement('div');
	        item.className = 'packet-item';
	        item.dataset.field = field;
	        const box = document.createElement('span');
	        const isDone = done.has(field.toLowerCase());
	        box.className = 'check' + (isDone ? ' done' : '');
	        const text = document.createElement('span');
	        text.textContent = field;
	        item.append(box, text);
	        item.addEventListener('click', () => saveReportPacket(field, !isDone));
	        list.append(item);
	      });
      if (options.reconnect !== false && previousId !== id) {
        reconnectStreamsForSelection(previousId);
      }
    }

    function setCollapsed(key, collapsed) {
      const children = document.querySelector(`[data-children="${key}"]`);
      if (!children) return;
      children.hidden = collapsed;
      document.querySelectorAll(`[data-toggle="${key}"]`).forEach(toggle => {
        toggle.setAttribute('aria-expanded', String(!collapsed));
        const chev = toggle.querySelector('.chev');
        if (chev) chev.textContent = collapsed ? '>' : 'v';
      });
    }

    function toggleTree(key) {
      const children = document.querySelector(`[data-children="${key}"]`);
      if (!children) return;
      setCollapsed(key, !children.hidden);
    }

	    initializeViewPreferences();
	    document.querySelectorAll('.tab').forEach(button => {
	      button.addEventListener('click', () => setActiveTab(button.dataset.tab));
	    });
	    document.querySelectorAll('[data-review-tab]').forEach(button => {
	      button.addEventListener('click', () => setActiveTab(button.dataset.reviewTab));
	    });
	    document.querySelectorAll('[data-room-context-toggle]').forEach(button => {
	      button.addEventListener('click', () => {
	        setRoomContextVisible(document.body.classList.contains('hide-room-context'));
	      });
	    });
	    document.querySelectorAll('[data-inspector-tab]').forEach(button => {
	      button.addEventListener('click', () => setInspectorTab(button.dataset.inspectorTab));
	    });
	    document.querySelectorAll('.density-button[data-density]').forEach(button => {
	      button.addEventListener('click', () => setDensity(button.dataset.density));
	    });
		    document.getElementById('viewMenuToggle').addEventListener('click', () => {
		      toggleChrome('show-view-menu', 'viewMenuToggle', viewPreferenceKeys.viewMenu, { persist: false });
		    });
		    document.querySelectorAll('[data-open-inspector]').forEach(button => {
		      button.addEventListener('click', () => {
		        openInspectorDrawer(button.dataset.openInspector || 'pane');
		      });
		    });
		    document.getElementById('closeInspector').addEventListener('click', () => {
		      closeInspectorDrawer();
		    });
		    document.getElementById('treeToggle').addEventListener('click', () => {
		      toggleRegionVisible('hide-tree', 'treeToggle', viewPreferenceKeys.treeVisible);
		      closeViewMenu();
		    });
		    document.getElementById('treeQuickToggle').addEventListener('click', () => {
		      toggleRegionVisible('hide-tree', 'treeToggle', viewPreferenceKeys.treeVisible);
		      closeViewMenu();
		    });
		    document.getElementById('closeTreeDrawer').addEventListener('click', event => {
		      event.stopPropagation();
		      closeTreeDrawer();
		    });
		    document.getElementById('drawerScrim').addEventListener('click', () => {
		      closeOpenPaneWallDrawers();
		    });
		    document.getElementById('treeReopen').addEventListener('click', () => {
		      setRegionVisible('hide-tree', 'treeToggle', viewPreferenceKeys.treeVisible, true, { persist: false });
		    });
			    document.getElementById('detailsReopen').addEventListener('click', () => {
			      openInspectorDrawer('pane');
			    });
			    document.getElementById('detailsQuickToggle').addEventListener('click', () => {
			      if (document.body.classList.contains('hide-inspector')) {
			        openInspectorDrawer('pane');
			      } else {
			        closeInspectorDrawer();
			      }
			    });
			    document.getElementById('detailsToggle').addEventListener('click', () => {
			      toggleRegionVisible('hide-inspector', 'detailsToggle', viewPreferenceKeys.detailsVisible);
			      closeViewMenu();
			    });
			    document.getElementById('commandQuickToggle').addEventListener('click', () => {
			      setCommandDeckVisible(!document.body.classList.contains('show-command-deck'));
			      closeViewMenu();
			    });
		    document.getElementById('controlsToggle').addEventListener('click', () => {
		      setCommandDeckVisible(!document.body.classList.contains('show-command-deck'));
		      closeViewMenu();
		    });
		    document.getElementById('rosterToggle').addEventListener('click', () => {
		      toggleChrome('show-roster', 'rosterToggle', viewPreferenceKeys.roster, { persist: false });
		      closeViewMenu();
		    });
		    document.getElementById('wallHudDetailsToggle').addEventListener('click', () => {
		      setWallHudExpanded(!document.body.classList.contains('wall-hud-expanded'));
		      closeViewMenu();
		    });
		    document.getElementById('wallHudFocusMode').addEventListener('click', () => {
		      setPaneWall(false);
		      closeViewMenu();
		    });
	    document.getElementById('inspectorAdvancedToggle').addEventListener('click', () => {
	      toggleChrome('show-inspector-advanced', 'inspectorAdvancedToggle', viewPreferenceKeys.inspectorAdvanced, { persist: false });
	    });
	    document.getElementById('deckAdvancedToggle').addEventListener('click', () => {
	      toggleChrome('show-command-advanced', 'deckAdvancedToggle', viewPreferenceKeys.commandAdvanced, { persist: false });
	    });
	    document.querySelectorAll('.mission-node[data-toggle]').forEach(node => {
	      node.addEventListener('click', () => toggleTree(node.dataset.toggle));
		    });
		    document.getElementById('paneWallToggle').addEventListener('click', () => {
		      setPaneWall(!document.body.classList.contains('pane-wall'));
		    });
		    document.getElementById('focusModeToggle').addEventListener('click', () => {
		      setPaneWall(false);
		    });
		    document.getElementById('wallHudMessage').addEventListener('click', event => {
		      event.stopPropagation();
		      loadPaneMessagePrompt(selectedWaveId, 'wall');
		    });
		    document.getElementById('wallHudRead').addEventListener('click', event => {
		      event.stopPropagation();
		      readSelectedPaneOutput();
		    });
		    document.getElementById('wallHudSweep').addEventListener('click', event => {
		      event.stopPropagation();
		      sweepWallMission();
		    });
		    document.getElementById('wallHudNewChild').addEventListener('click', event => {
		      event.stopPropagation();
		      startChildSession('right', 'wall');
		    });
		    document.getElementById('wallHudPacketsAll').addEventListener('click', event => {
		      event.stopPropagation();
		      requestMissingPackets('wall');
		    });
		    document.getElementById('wallHudPacketAction').addEventListener('click', event => {
		      event.stopPropagation();
		      loadMissingPacketPrompt('wall');
		    });
		    document.getElementById('wallHudMore').addEventListener('click', event => {
		      event.stopPropagation();
		      setWallHudExpanded(!document.body.classList.contains('wall-hud-expanded'));
		    });
		    document.getElementById('wallHudSendSelected').addEventListener('click', event => {
		      event.stopPropagation();
		      sendParentCommand('selected', 'wall');
		    });
		    document.getElementById('wallHudSendScope').addEventListener('click', event => {
		      event.stopPropagation();
		      sendParentCommand('all', 'wall');
		    });
		    document.querySelectorAll('[data-wall-preset]').forEach(button => {
		      button.addEventListener('click', event => {
		        event.stopPropagation();
		        loadWallPreset(button.dataset.wallPreset);
		      });
		    });
		    document.getElementById('wallHudDag').addEventListener('mousedown', event => {
		      const node = event.target.closest('[data-wall-dag-wave]');
		      if (!node || event.detail < 2) return;
		      event.stopPropagation();
		      openExpandedPane(node.dataset.wallDagWave);
		    });
		    document.getElementById('wallHudDag').addEventListener('click', event => {
		      const node = event.target.closest('[data-wall-dag-wave]');
		      if (!node) return;
		      event.stopPropagation();
		      selectWave(node.dataset.wallDagWave);
		    });
		    document.getElementById('wallHudDag').addEventListener('dblclick', event => {
		      const node = event.target.closest('[data-wall-dag-wave]');
		      if (!node) return;
		      event.stopPropagation();
		      openExpandedPane(node.dataset.wallDagWave);
		    });
		    document.getElementById('wallHudFull').addEventListener('click', event => {
		      event.stopPropagation();
		      setExpanded(true);
		    });
		    document.getElementById('wallHudExit').addEventListener('click', event => {
		      event.stopPropagation();
		      setPaneWall(false);
		    });
		    document.getElementById('expandTerminal').addEventListener('click', () => {
		      setExpanded(!document.body.classList.contains('terminal-expanded'));
		    });
		    document.getElementById('importSession').addEventListener('click', () => importMissionSession('drawer'));
		    document.getElementById('deckPreviewImport').addEventListener('click', () => importMissionSession('deck', { preview: true }));
		    document.getElementById('deckImportSession').addEventListener('click', () => importMissionSession('deck'));
		    document.getElementById('deckImportWall').addEventListener('click', () => importMissionSession('deck', { openWall: true }));
		    document.getElementById('deckSessionPath').addEventListener('keydown', event => {
		      if (event.key === 'Enter') {
		        event.preventDefault();
		        importMissionSession('deck');
		      }
		    });
		    document.getElementById('loadContract').addEventListener('click', loadContractPrompt);
		    document.getElementById('loadMissing').addEventListener('click', loadMissingPacketPrompt);
		    document.getElementById('loadFiles').addEventListener('click', loadFileReceiptPrompt);
		    document.getElementById('ingestPacket').addEventListener('click', () => ingestSelectedReportPacket());
		    document.getElementById('requestPacket').addEventListener('click', loadMissingPacketPrompt);
		    document.getElementById('commandReadOutput').addEventListener('click', () => readSelectedPaneOutput());
		    document.getElementById('commandRequestPacket').addEventListener('click', loadMissingPacketPrompt);
		    document.getElementById('readOutput').addEventListener('click', () => readSelectedPaneOutput());
		    document.getElementById('copyOutputPrompt').addEventListener('click', copyOutputToPrompt);
		    document.getElementById('markAccepted').addEventListener('click', () => setSelectedPaneStatus('accepted'));
		    document.getElementById('markReview').addEventListener('click', () => setSelectedPaneStatus('needs_review'));
		    document.getElementById('refreshEvidence').addEventListener('click', () => refreshEvidence());
		    document.getElementById('missionRefreshSweep').addEventListener('click', () => refreshMissionRoom({ quiet: false }));
		    document.getElementById('reviewRefreshSweep').addEventListener('click', () => refreshReviewRoom({ quiet: false }));
		    document.getElementById('sendSelected').addEventListener('click', () => sendParentCommand('selected'));
		    document.getElementById('sendAll').addEventListener('click', () => sendParentCommand('all'));
		    document.getElementById('deckMessageSelected').addEventListener('click', () => loadPaneMessagePrompt(selectedWaveId, 'deck'));
		    document.getElementById('deckReadSelected').addEventListener('click', () => readSelectedPaneOutput());
		    document.getElementById('deckSendSelected').addEventListener('click', () => sendParentCommand('selected', 'deck'));
		    document.getElementById('deckSendAll').addEventListener('click', () => sendParentCommand('all', 'deck'));
		    document.getElementById('deckStartChild').addEventListener('click', () => startChildSession('right', 'deck'));
		    document.getElementById('deckStartChildRight').addEventListener('click', () => startChildSession('right', 'deck'));
		    document.getElementById('deckStartChildDown').addEventListener('click', () => startChildSession('down', 'deck'));
		    document.getElementById('deckStopSelected').addEventListener('click', () => closeSelectedPane());
		    document.getElementById('deckSweep').addEventListener('click', () => refreshEvidence());
		    document.getElementById('deckUnlockReady').addEventListener('click', () => unlockReadyWaves('deck'));
		    document.getElementById('deckRequestPackets').addEventListener('click', () => requestMissingPackets('deck'));
		    document.getElementById('deckScope').addEventListener('change', updateDeckScopeSummary);
		    document.getElementById('wallHudScope').addEventListener('change', event => {
		      setCommandStatus('wall', `${scopeLabel(event.target.value)} selected.`);
		    });
		    document.getElementById('splitRight').addEventListener('click', () => splitSelectedPane('right'));
		    document.getElementById('splitDown').addEventListener('click', () => splitSelectedPane('down'));
		    document.getElementById('closeSelected').addEventListener('click', closeSelectedPane);
		    document.getElementById('startChildRight').addEventListener('click', () => startChildSession('right', 'drawer'));
		    document.getElementById('startChildDown').addEventListener('click', () => startChildSession('down', 'drawer'));
		    terminalWrap.addEventListener('click', () => terminalWrap.focus());
    terminalWrap.addEventListener('dblclick', () => {
      setExpanded(false);
    });
	    document.addEventListener('keydown', event => {
	      if (event.key === 'Escape' && closeOpenViewMenu()) {
	        event.preventDefault();
	        return;
	      }
		      if (event.key === 'Escape' && closeOpenPaneDrawers()) {
		        event.preventDefault();
		        return;
		      }
	      if (event.key === 'Escape' && closeOpenWallHud()) {
	        event.preventDefault();
	        return;
	      }
	      if (event.key === 'Escape' && document.body.classList.contains('terminal-expanded')) {
	        event.preventDefault();
	        setExpanded(false);
	        return;
	      }
      if (event.target === document.getElementById('deckCommand')) {
        if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') {
          event.preventDefault();
          sendParentCommand(event.shiftKey ? 'all' : 'selected', 'deck');
        }
        return;
      }
      if (event.target === document.getElementById('wallHudCommand')) {
        if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') {
          event.preventDefault();
          sendParentCommand(event.shiftKey ? 'all' : 'selected', 'wall');
        }
        return;
      }
      const activeTile = document.activeElement?.classList?.contains('wave-card');
      const activeFocusTerminal = document.activeElement === terminalWrap || document.activeElement === canvas;
      if (!activeTile && !activeFocusTerminal) return;
      const payload = keyPayload(event);
      if (!payload) return;
      event.preventDefault();
      sendInput(payload);
    });
	    window.addEventListener('resize', () => {
	      clearTimeout(window.__herdrResize);
	      window.__herdrResize = setTimeout(() => {
	        if (document.body.classList.contains('terminal-expanded')) connect();
	      }, 180);
	    });
	    window.setInterval(() => {
	      refreshEvidence({ quiet: true }).catch(() => {});
	    }, 8000);
		    Promise.all([
		      loadIntegrations().catch(() => {}),
		      loadDispatches().catch(() => {})
		    ])
		      .finally(() => {
		        loadPanes()
		          .then(() => refreshEvidence({ quiet: true, ingest: false }).catch(() => {}))
		          .catch(error => {
		          setRuntimeStatus(error.message || 'pane load failed', false);
		          terminalStatus.textContent = 'offline';
			      document.getElementById('waveGrid').innerHTML =
			        `<div class="empty-state">Could not load Herdr panes: ${escapeHtml(error.message || error)}. Start a Herdr server, then reopen this workroom.</div>`;
		          document.getElementById('missionChildren').innerHTML =
		            '<div class="arc-node"><span></span><span>No pane data available</span><span></span></div>';
		        });
		      });
	  </script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_path_query_separates_query_string() {
        assert_eq!(
            split_path_query("/events?cols=80"),
            ("/events", Some("cols=80"))
        );
        assert_eq!(split_path_query("/"), ("/", None));
    }

    #[test]
    fn desktop_shell_groups_review_surfaces_under_three_primary_tabs() {
        assert_eq!(INDEX_HTML.matches("data-tab=\"").count(), 3);
        assert!(INDEX_HTML.contains("data-tab=\"project\">Mission"));
        assert!(INDEX_HTML.contains("data-tab=\"panes\">Workbench"));
        assert!(!INDEX_HTML.contains("data-tab=\"panes\">Panes"));
        assert!(!INDEX_HTML.contains("data-tab=\"panes\">Live Panes"));
        assert!(INDEX_HTML.contains("data-tab=\"review\">Review"));
        assert!(INDEX_HTML.contains("id=\"reviewTabs\" hidden"));
        assert!(INDEX_HTML.contains("data-review-tab=\"review\">Overview"));
        assert!(INDEX_HTML.contains("data-page=\"review\""));
        assert!(INDEX_HTML.contains("id=\"reviewDecisionRoom\""));
        assert!(INDEX_HTML.contains("const reviewTabNames = new Set(['review', 'evidence', 'changes', 'audit', 'timeline']);"));
        assert!(INDEX_HTML.contains("return tab;"));
        assert!(!INDEX_HTML.contains("return tab === 'review' ? 'evidence' : tab;"));
        assert!(!INDEX_HTML.contains("data-tab=\"evidence\">Evidence"));
        assert!(!INDEX_HTML.contains("data-tab=\"changes\">Changes"));
        assert!(!INDEX_HTML.contains("data-tab=\"audit\">Audit"));
        assert!(!INDEX_HTML.contains("data-tab=\"timeline\">Timeline"));
    }

    #[test]
    fn desktop_shell_review_overview_has_actionable_navigation() {
        assert!(INDEX_HTML.contains("function roomJumpButton(tab, label)"));
        assert!(INDEX_HTML.contains("data-room-jump=\"${escapeHtml(tab)}\""));
        assert!(INDEX_HTML.contains("roomJumpButton('changes'"));
        assert!(INDEX_HTML.contains("roomJumpButton('audit'"));
        assert!(INDEX_HTML.contains("roomJumpButton('timeline'"));
        assert!(INDEX_HTML.contains("const nextActionControls ="));
        assert!(INDEX_HTML.contains("document.querySelectorAll('[data-room-jump]').forEach"));
        assert!(INDEX_HTML.contains("setActiveTab(button.dataset.roomJump);"));
        assert!(!INDEX_HTML.contains("data-review-jump"));
        assert!(!INDEX_HTML.contains("reviewJumpButton"));
    }

    #[test]
    fn desktop_shell_review_page_starts_with_decision_brief() {
        assert!(INDEX_HTML.contains("id=\"reviewBrief\""));
        assert!(INDEX_HTML.contains("function renderReviewBrief"));
        assert!(INDEX_HTML.contains("Review command brief"));
        assert!(INDEX_HTML.contains("Primary verdict"));
        assert!(INDEX_HTML.contains("Done court"));
        assert!(INDEX_HTML.contains("Evidence readiness"));
        assert!(INDEX_HTML.contains("Gate status"));
        assert!(INDEX_HTML.contains("renderReviewBrief({"));
        assert!(!INDEX_HTML.contains("reviewDecisionRoom.innerHTML = reviewDecisionMetrics +"));
    }

    #[test]
    fn desktop_shell_review_room_starts_with_parent_decision_docket() {
        assert!(INDEX_HTML.contains("function reviewDecisionCard(label, value, detail, tone = '')"));
        assert!(INDEX_HTML.contains("function reviewDocketCard({"));
        assert!(INDEX_HTML.contains("Parent decision docket"));
        assert!(INDEX_HTML.contains("What gets judged now"));
        assert!(INDEX_HTML.contains("review-decision-grid"));
        assert!(INDEX_HTML.contains("parent decisions"));
        assert!(INDEX_HTML.contains("ready packets"));
        assert!(INDEX_HTML.contains("blocked gates"));
        assert!(INDEX_HTML.contains("missing packets"));
        assert!(INDEX_HTML.contains("Evidence footprint"));
        assert!(INDEX_HTML.contains("reviewDocketCard({"));
        assert!(
            INDEX_HTML.find("reviewDocketCard({").unwrap()
                < INDEX_HTML.find("opsCard('Decision queue'").unwrap()
        );
    }

    #[test]
    fn desktop_shell_review_mode_is_a_full_width_decision_room() {
        assert!(INDEX_HTML.contains("body[data-active-group=\"review\"] .tree"));
        assert!(INDEX_HTML.contains("body[data-active-group=\"review\"] .inspector"));
        assert!(INDEX_HTML.contains(
            "body[data-active-group=\"review\"].hide-inspector:not(.pane-wall):not(.hide-tree) .layout"
        ));
        assert!(
            INDEX_HTML
                .rfind("body[data-active-group=\"review\"].hide-inspector")
                .unwrap()
                > INDEX_HTML.rfind("\n    .layout {\n").unwrap()
        );
        assert!(
            INDEX_HTML
                .rfind("body[data-active-group=\"review\"].hide-inspector")
                .unwrap()
                > INDEX_HTML
                    .rfind("body.hide-inspector:not(.pane-wall):not(.hide-tree) .layout")
                    .unwrap()
        );
        assert!(INDEX_HTML.contains("class=\"review-room\""));
        assert!(INDEX_HTML.contains("class=\"review-room-body\""));
        assert!(INDEX_HTML.contains("class=\"review-main\""));
        assert!(INDEX_HTML.contains("class=\"review-sidecar\""));
        assert!(INDEX_HTML.contains("class=\"review-decision-room\" id=\"reviewDecisionRoom\""));
        assert!(!INDEX_HTML.contains(
            "<section class=\"tab-page\" data-page=\"review\">\n\t\t          <div class=\"empty-page\">"
        ));
    }

    #[test]
    fn desktop_shell_review_sidecar_holds_context_lanes() {
        assert!(INDEX_HTML.contains("<h2>Context lanes</h2>"));
        assert!(INDEX_HTML.contains("data-room-jump=\"evidence\">Evidence</button>"));
        assert!(INDEX_HTML.contains("data-room-jump=\"changes\">Changes</button>"));
        assert!(INDEX_HTML.contains("data-room-jump=\"audit\">Audit</button>"));
        assert!(INDEX_HTML.contains("data-room-jump=\"timeline\">Timeline</button>"));
        assert!(INDEX_HTML.contains("Review does not need every control visible at once."));
    }

    #[test]
    fn desktop_shell_review_sub_lanes_use_room_shells() {
        for page in ["evidence", "changes", "audit", "timeline"] {
            assert!(INDEX_HTML.contains(&format!("data-page=\"{page}\"")));
        }
        assert!(INDEX_HTML.contains("class=\"lane-room\""));
        assert!(INDEX_HTML.contains("class=\"lane-room-head\""));
        assert!(INDEX_HTML.contains("class=\"lane-room-body\""));
        assert!(INDEX_HTML.contains("class=\"lane-main\""));
        assert!(INDEX_HTML.contains("class=\"lane-sidecar\""));
        assert!(!INDEX_HTML.contains(
            "<section class=\"tab-page\" data-page=\"evidence\">\n\t\t          <div class=\"empty-page\">"
        ));
        assert!(!INDEX_HTML.contains(
            "<section class=\"tab-page\" data-page=\"changes\">\n\t          <div class=\"empty-page\">"
        ));
        assert!(!INDEX_HTML.contains(
            "<section class=\"tab-page\" data-page=\"audit\">\n\t          <div class=\"empty-page\">"
        ));
        assert!(!INDEX_HTML.contains(
            "<section class=\"tab-page\" data-page=\"timeline\">\n\t          <div class=\"empty-page\">"
        ));
    }

    #[test]
    fn desktop_shell_review_sub_lanes_keep_context_navigation_close() {
        assert!(INDEX_HTML.contains("data-room-jump=\"review\">Overview</button>"));
        assert!(INDEX_HTML.contains("<h2>Evidence lanes</h2>"));
        assert!(INDEX_HTML.contains("<h2>Change lanes</h2>"));
        assert!(INDEX_HTML.contains("<h2>Audit lanes</h2>"));
        assert!(INDEX_HTML.contains("<h2>Timeline lanes</h2>"));
        assert!(INDEX_HTML
            .contains("Return to Overview when the lane has enough signal for a decision."));
    }

    #[test]
    fn desktop_shell_review_lanes_are_sidecar_navigation_not_midpage_tabs() {
        assert!(INDEX_HTML.contains("id=\"reviewTabs\" hidden"));
        assert!(INDEX_HTML.contains("if (reviewTabsNode) reviewTabsNode.hidden = true;"));
        assert!(!INDEX_HTML.contains("reviewTabsNode.hidden = group !== 'review';"));
        assert!(INDEX_HTML.contains("Review lane navigation lives in the room sidecar."));
    }

    #[test]
    fn desktop_shell_room_context_can_collapse() {
        assert!(INDEX_HTML.contains("data-room-context-toggle"));
        assert!(INDEX_HTML.contains("function setRoomContextVisible"));
        assert!(INDEX_HTML.contains("body.hide-room-context .mission-sidecar"));
        assert!(INDEX_HTML.contains("body.hide-room-context .review-sidecar"));
        assert!(INDEX_HTML.contains("body.hide-room-context .lane-sidecar"));
        assert!(INDEX_HTML.contains(
            "body.hide-room-context .mission-room-body,\n    body.hide-room-context .review-room-body,\n    body.hide-room-context .lane-room-body"
        ));
        assert!(INDEX_HTML.contains("Context</button>"));
        assert!(
            INDEX_HTML.contains("document.querySelectorAll('[data-room-context-toggle]').forEach")
        );
        assert!(INDEX_HTML.contains("roomContextVisible: 'herdr.desktop.roomContextVisible.v2'"));
        assert!(INDEX_HTML
            .contains("setRoomContextVisible(readPreference(viewPreferenceKeys.roomContextVisible, '0') === '1'"));
    }

    #[test]
    fn desktop_shell_packet_review_respects_attention_gates() {
        assert!(INDEX_HTML.contains("function packetReviewGateReason(wave)"));
        assert!(INDEX_HTML.contains("!packetReviewGateReason(wave)"));
        assert!(INDEX_HTML.contains("blockedPacketCount"));
        assert!(INDEX_HTML.contains("blocked by parent decision"));
        assert!(INDEX_HTML.contains("blocked from done court"));
    }

    #[test]
    fn desktop_shell_review_room_has_freshness_controls() {
        assert!(INDEX_HTML.contains("id=\"reviewRefreshSweep\""));
        assert!(INDEX_HTML.contains("id=\"reviewSweepStatus\""));
        assert!(INDEX_HTML.contains("function refreshReviewRoom(options = {})"));
        assert!(INDEX_HTML.contains("refreshEvidence({ quiet: true, ingest: true"));
        assert!(INDEX_HTML.contains("setReviewSweepStatus"));
        assert!(
            INDEX_HTML.contains("document.getElementById('reviewRefreshSweep').addEventListener")
        );
    }

    #[test]
    fn desktop_shell_mission_room_has_freshness_controls() {
        assert!(INDEX_HTML.contains("id=\"missionRefreshSweep\""));
        assert!(INDEX_HTML.contains("id=\"missionSweepStatus\""));
        assert!(INDEX_HTML.contains("function refreshMissionRoom(options = {})"));
        assert!(INDEX_HTML.contains("source: 'mission'"));
        assert!(INDEX_HTML
            .contains("if (targetTab === 'project') refreshMissionRoom({ quiet: true });"));
        assert!(
            INDEX_HTML.contains("document.getElementById('missionRefreshSweep').addEventListener")
        );
    }

    #[test]
    fn desktop_shell_mission_page_starts_with_command_brief() {
        assert!(INDEX_HTML.contains("id=\"missionBrief\""));
        assert!(INDEX_HTML.contains("class=\"mission-brief\""));
        assert!(INDEX_HTML.contains("function renderMissionBrief"));
        assert!(INDEX_HTML.contains("Mission command brief"));
        assert!(INDEX_HTML.contains("Next parent decision"));
        assert!(INDEX_HTML.contains("Sweep truth"));
        assert!(INDEX_HTML.contains("Operational policy"));
        assert!(INDEX_HTML.contains("renderMissionBrief({"));
    }

    #[test]
    fn desktop_shell_mission_page_has_lifecycle_board_backed_by_panes() {
        assert!(INDEX_HTML.contains("function missionLifecycleLaneForWave(wave)"));
        assert!(INDEX_HTML.contains("function renderMissionLifecycleBoard(list)"));
        assert!(INDEX_HTML.contains("class=\"mission-lifecycle-board\""));
        assert!(INDEX_HTML.contains("aria-label=\"Mission Board\""));
        assert!(INDEX_HTML.contains("data-mission-lane=\"draft_contract\""));
        assert!(INDEX_HTML.contains("data-mission-lane=\"running\""));
        assert!(INDEX_HTML.contains("data-mission-lane=\"needs_packet\""));
        assert!(INDEX_HTML.contains("data-mission-lane=\"parent_review\""));
        assert!(INDEX_HTML.contains("data-mission-lane=\"accepted\""));
        assert!(INDEX_HTML.contains("data-board-wave"));
        assert!(INDEX_HTML.contains(".mission-board-card[data-wave]"));
        assert!(INDEX_HTML.contains("if (event.target.closest('button')) return;"));
        assert!(INDEX_HTML.contains("missionLifecycleLaneForWave(wave)"));
        assert!(INDEX_HTML.contains("renderMissionLifecycleBoard(list)"));
    }

    #[test]
    fn desktop_shell_mission_board_lanes_are_collapsible_and_persisted() {
        assert!(INDEX_HTML.contains("missionBoardCollapsed: 'herdr.desktop.missionBoardCollapsed'"));
        assert!(INDEX_HTML.contains("let missionBoardCollapsedLanes = new Set();"));
        assert!(INDEX_HTML.contains("function setMissionBoardLaneCollapsed(laneId, collapsed)"));
        assert!(INDEX_HTML.contains("data-mission-lane-toggle"));
        assert!(INDEX_HTML.contains("data-mission-lane-collapsed"));
        assert!(INDEX_HTML.contains("data-lifecycle-lane"));
        assert!(INDEX_HTML.contains("writePreference(viewPreferenceKeys.missionBoardCollapsed"));
        assert!(INDEX_HTML.contains("document.querySelectorAll('[data-mission-lane-toggle]')"));
        assert!(INDEX_HTML.contains("function normalizedWaveStatus(wave)"));
    }

    #[test]
    fn desktop_shell_mission_board_prefers_contract_lifecycle_lane() {
        assert!(INDEX_HTML.contains("lifecycleLane: contract.lifecycle_lane || ''"));
        assert!(INDEX_HTML.contains("function normalizedLifecycleLane(value)"));
        assert!(INDEX_HTML
            .contains("const explicitLane = normalizedLifecycleLane(wave.lifecycleLane);"));
        assert!(INDEX_HTML.contains("if (explicitLane) return explicitLane;"));
    }

    #[test]
    fn desktop_shell_project_radar_stages_research_candidates() {
        assert!(INDEX_HTML.contains("let missionRadarItems = [];"));
        assert!(INDEX_HTML.contains("let missionRadarScan = null;"));
        assert!(INDEX_HTML.contains("function buildMissionRadarItems({"));
        assert!(INDEX_HTML.contains("function mergeMissionRadarItems(scannedItems, localItems)"));
        assert!(INDEX_HTML.contains("function scanMissionRadar()"));
        assert!(INDEX_HTML.contains("function radarFamilyLabel(value)"));
        assert!(INDEX_HTML.contains("function radarFamilyMeta(value)"));
        assert!(INDEX_HTML.contains("function radarCandidateRow(item)"));
        assert!(INDEX_HTML.contains("function radarLane(meta, laneItems)"));
        assert!(INDEX_HTML.contains("function compileMissionDraft(items)"));
        assert!(INDEX_HTML
            .contains("function primaryMissionDraftCandidates(items = missionRadarItems)"));
        assert!(INDEX_HTML.contains("function missionDraftDependencyForPick(picks, index)"));
        assert!(INDEX_HTML.contains("function renderMissionDraftCard(items)"));
        assert!(INDEX_HTML.contains("function missionDraftRow(stage, displayOrder = stage.order)"));
        assert!(INDEX_HTML.contains("function stageMissionDraft()"));
        assert!(INDEX_HTML.contains("function launchMissionDraft(direction = 'right')"));
        assert!(INDEX_HTML.contains("function setSelectValue(select, value, label = value)"));
        assert!(INDEX_HTML
            .contains("function applyRadarCandidateToDeck(candidate, stage = null, options = {})"));
        assert!(INDEX_HTML.contains("option.dataset.dynamic = 'mission-draft';"));
        assert!(INDEX_HTML.contains("return `after ${previous}`;"));
        assert!(INDEX_HTML.contains("function candidateContractBrief(candidate)"));
        assert!(INDEX_HTML.contains("Project radar"));
        assert!(INDEX_HTML.contains("Mission compiler draft"));
        assert!(INDEX_HTML.contains("Radar candidates sequenced into launchable panes"));
        assert!(INDEX_HTML.contains("data-radar-stage-draft"));
        assert!(INDEX_HTML.contains("data-radar-launch-draft"));
        assert!(INDEX_HTML.contains(">stage draft</button>"));
        assert!(INDEX_HTML.contains(">launch draft</button>"));
        assert!(INDEX_HTML.contains("Mission compiler stage: ${stage.order}. ${stage.title}"));
        assert!(INDEX_HTML.contains(
            "Compiled lanes launch as real child panes; missing lanes stay out of the draft."
        ));
        assert!(INDEX_HTML.contains("Scout the repo"));
        assert!(INDEX_HTML.contains("Shape wave contracts"));
        assert!(INDEX_HTML.contains("Sequence the mission"));
        assert!(INDEX_HTML.contains("Expandable research receipts from PX and live panes"));
        assert!(INDEX_HTML.contains("Research next work"));
        assert!(INDEX_HTML.contains(">research next work</button>"));
        assert!(INDEX_HTML.contains("PX project scan"));
        assert!(INDEX_HTML.contains("function radarPickCard(meta, item)"));
        assert!(INDEX_HTML.contains("class=\"radar-picks\""));
        assert!(INDEX_HTML.contains("class=\"radar-lanes\""));
        assert!(INDEX_HTML.contains("<details class=\"radar-lane\""));
        assert!(INDEX_HTML.contains("<summary class=\"radar-lane-head\">"));
        assert!(INDEX_HTML.contains("class=\"radar-lane-body\""));
        assert!(INDEX_HTML.contains("data-radar-family"));
        assert!(INDEX_HTML.contains("Insight tasks"));
        assert!(INDEX_HTML.contains("Ideation waves"));
        assert!(INDEX_HTML.contains("Roadmap waves"));
        assert!(INDEX_HTML.contains("Project discovery scout"));
        assert!(INDEX_HTML.contains("insight_task"));
        assert!(INDEX_HTML.contains("ideation_wave"));
        assert!(INDEX_HTML.contains("roadmap_wave"));
        assert!(INDEX_HTML.contains("Allowed paths / scope:"));
        assert!(INDEX_HTML.contains("Required report packet additions:"));
        assert!(INDEX_HTML.contains("viewPreferenceKeys.missionRadarScan"));
        assert!(INDEX_HTML.contains("writeJsonPreference(viewPreferenceKeys.missionRadarScan"));
        assert!(INDEX_HTML
            .contains("code quality, UX, docs, security, performance, and product roadmap"));
        assert!(INDEX_HTML.contains("data-radar-stage"));
        assert!(INDEX_HTML.contains("data-radar-launch"));
        assert!(INDEX_HTML.contains("data-radar-scan"));
        assert!(INDEX_HTML.contains("renderMissionDraftCard(missionRadarItems)"));
        assert!(INDEX_HTML.contains("fetch(`/mission/radar?${params.toString()}`"));
        assert!(INDEX_HTML.contains("function stageRadarCandidate(candidateId, options = {})"));
        assert!(
            INDEX_HTML.contains("function launchRadarCandidate(candidateId, direction = 'right')")
        );
        assert!(INDEX_HTML.contains("stageRadarCandidate(button.dataset.radarStage);"));
        assert!(INDEX_HTML.contains("launchRadarCandidate(button.dataset.radarLaunch, 'right');"));
        assert!(INDEX_HTML.contains("stageMissionDraft();"));
        assert!(INDEX_HTML.contains("launchMissionDraft('right');"));
        assert!(
            INDEX_HTML.contains("const dependency = missionDraftDependencyForPick(picks, index);")
        );
        assert!(
            INDEX_HTML
                .find("renderMissionDraftCard(missionRadarItems)")
                .unwrap()
                < INDEX_HTML
                    .find("renderProjectRadarCard(missionRadarItems)")
                    .unwrap()
        );
        assert!(INDEX_HTML.contains("setPaneWall(true);"));
        assert!(INDEX_HTML.contains("setActiveTab('panes');"));
        assert!(INDEX_HTML.contains("await startChildSession(direction, 'deck');"));
    }

    #[test]
    fn desktop_shell_mission_draft_queues_dependency_gated_child_panes() {
        assert!(INDEX_HTML.contains("function dependencyIsParallel(dependency)"));
        assert!(INDEX_HTML.contains("function dispatchStatusForDependency(dependency)"));
        assert!(
            INDEX_HTML.contains("return dependencyIsParallel(dependency) ? 'running' : 'queued';")
        );
        assert!(INDEX_HTML.contains("const status = dispatchStatusForDependency(dependency);"));
        assert!(INDEX_HTML.contains("const { title, mode, brief, argv, argvText, dependency, status } = childDispatchConfig(source);"));
        assert!(INDEX_HTML.contains(
            "const startPrompt = newChildDispatchPrompt(title, mode, brief, dependency, status);"
        ));
        assert!(INDEX_HTML.contains("status === 'queued' ? 'queueing' : 'starting'"));
        assert!(INDEX_HTML.contains("status === 'queued' ? 'queued' : 'started'"));
        assert!(
            INDEX_HTML.contains("await attachPaneContract(newPaneId, title, mode, brief, status);")
        );
        assert!(INDEX_HTML.contains(
            "function newChildDispatchPrompt(title, mode, brief, dependency, status = 'running')"
        ));
        assert!(INDEX_HTML.contains("`Status: ${status}`"));
        assert!(INDEX_HTML.contains("This pane is queued behind"));
        assert!(INDEX_HTML.contains(
            "Do not begin implementation or make edits until the parent explicitly unlocks"
        ));
        assert!(INDEX_HTML
            .contains("This pane is parallel-ready; begin only inside the approved scope."));
    }

    #[test]
    fn desktop_shell_agent_start_accepts_queued_status_contracts() {
        let route_source = include_str!("desktop.rs");
        assert!(route_source.contains("let status = query_value(query, \"status\")"));
        assert!(route_source.contains(".and_then(parse_wave_status)"));
        assert!(route_source.contains("status: Some(status)"));
        assert!(route_source.contains(
            "WaveLifecycleLane::for_status_and_report(status, &WaveReportGate::default())"
        ));
        assert!(route_source.contains("status.map(|status|"));
    }

    #[test]
    fn mission_radar_px_audit_generates_candidate_waves() {
        let audit = serde_json::json!({
            "space": "herdr",
            "health_score": 50,
            "dead_count": 12,
            "dead_ratio": 0.25,
            "health_notes": [
                {"severity": "Problem", "message": "High dead code (Rust): 25%"}
            ],
            "hotspot_files": [
                {"path": "src/app/state.rs", "total_incoming_refs": 2438}
            ],
            "dead_symbols": [
                {"name": "old_fn", "file": "src/old.rs", "line": 9}
            ],
            "high_coupling_symbols": [
                {"name": "Mode", "file": "src/app/state.rs", "use_count": 673}
            ],
            "phase_coverage": {
                "p5_diagnostics": false
            }
        });

        let candidates = mission_radar_candidates_from_px_audit(&audit);
        let ids = candidates
            .iter()
            .map(|candidate| candidate.id.as_str())
            .collect::<Vec<_>>();

        assert!(ids.contains(&"px-code-health-scout"));
        assert!(ids.contains(&"px-hotspot-refactor-scout"));
        assert!(ids.contains(&"px-dead-code-scout"));
        assert!(ids.contains(&"px-coupling-reviewer"));
        assert!(ids.contains(&"px-diagnostics-refresh-scout"));
        assert!(candidates
            .iter()
            .all(|candidate| candidate.brief.contains("Use px first")));
        assert!(candidates
            .iter()
            .any(|candidate| candidate.family == "insight_task"));
        assert!(candidates
            .iter()
            .any(|candidate| candidate.family == "ideation_wave"));
        assert!(candidates
            .iter()
            .all(|candidate| !candidate.required_report.is_empty()));
    }

    #[test]
    fn desktop_shell_routes_mission_radar_endpoint() {
        assert!(INDEX_HTML.contains("/mission/radar?"));
        let route_source = include_str!("desktop.rs");
        assert!(route_source.contains("\"/mission/radar\" => handle_mission_radar(stream, query)"));
        assert!(route_source.contains("px"));
        assert!(route_source.contains("audit"));
        assert!(route_source.contains("MISSION_RADAR_SCAN_TIMEOUT"));
    }

    #[test]
    fn desktop_shell_routes_mission_workroom_endpoint() {
        let route_source = include_str!("desktop.rs");

        assert!(route_source
            .contains("\"/mission/workroom\" => handle_mission_workroom(stream, query)"));
        assert!(route_source.contains("fn handle_mission_workroom"));
        assert!(route_source.contains("workroom_model::WorkroomView::from_pane_values"));
    }

    #[test]
    fn desktop_shell_keeps_existing_workbench_and_review_sidecar_systems() {
        assert!(INDEX_HTML.contains("data-tab=\"project\">Mission</button>"));
        assert!(INDEX_HTML.contains("data-tab=\"panes\">Workbench</button>"));
        assert!(INDEX_HTML.contains("data-tab=\"review\">Review</button>"));
        assert!(INDEX_HTML.contains("data-review-tab=\"evidence\""));
        assert!(INDEX_HTML.contains("data-review-tab=\"changes\""));
        assert!(INDEX_HTML.contains("data-review-tab=\"audit\""));
        assert!(INDEX_HTML.contains("data-review-tab=\"timeline\""));
        assert!(!INDEX_HTML.contains("data-inspector-tab=\"evidence\""));
        assert!(!INDEX_HTML.contains("data-inspector-tab=\"changes\""));
        assert!(!INDEX_HTML.contains("data-inspector-tab=\"audit\""));
        assert!(!INDEX_HTML.contains("data-inspector-tab=\"timeline\""));
    }

    #[test]
    fn desktop_shell_does_not_duplicate_tree_or_child_launch_functions() {
        assert!(INDEX_HTML.contains("function renderMissionTree()"));
        assert!(
            INDEX_HTML.contains("async function startChildSession(direction, source = 'drawer')")
        );
        assert!(INDEX_HTML.contains("id=\"deckStartChildRight\""));
        assert!(INDEX_HTML.contains("id=\"deckStartChildDown\""));
        assert!(!INDEX_HTML.contains("function renderWorkroomTree("));
        assert!(!INDEX_HTML.contains("function launchChildPaneFromDeck("));
        assert!(!INDEX_HTML.contains("id=\"deckCreateRight\""));
        assert!(!INDEX_HTML.contains("id=\"deckCreateDown\""));
    }

    #[test]
    fn desktop_shell_mission_mode_is_a_full_width_parent_contract_room() {
        assert!(INDEX_HTML.contains("body[data-active-tab=\"project\"] .tree"));
        assert!(INDEX_HTML.contains("body[data-active-tab=\"project\"] .inspector"));
        assert!(INDEX_HTML.contains(
            "body[data-active-tab=\"project\"].hide-inspector:not(.pane-wall):not(.hide-tree) .layout"
        ));
        assert!(
            INDEX_HTML
                .rfind("body[data-active-tab=\"project\"].hide-inspector")
                .unwrap()
                > INDEX_HTML.rfind("\n    .layout {\n").unwrap()
        );
        assert!(INDEX_HTML.contains("class=\"mission-room\""));
        assert!(INDEX_HTML.contains("class=\"mission-room-body\""));
        assert!(INDEX_HTML.contains("class=\"mission-main\""));
        assert!(INDEX_HTML.contains("class=\"mission-sidecar\""));
        assert!(INDEX_HTML.contains("class=\"mission-state-room ops-grid\" id=\"projectBoard\""));
        assert!(!INDEX_HTML.contains(
            "<section class=\"tab-page\" data-page=\"project\">\n\t\t          <div class=\"empty-page\">"
        ));
    }

    #[test]
    fn desktop_shell_mission_sidecar_holds_parent_contract_lanes() {
        assert!(INDEX_HTML.contains("<h2>Parent contract lanes</h2>"));
        assert!(INDEX_HTML.contains("Mission is the contract surface."));
        assert!(INDEX_HTML.contains("data-room-jump=\"panes\">Workbench</button>"));
        assert!(!INDEX_HTML.contains("data-room-jump=\"panes\">Panes</button>"));
        assert!(!INDEX_HTML.contains("data-room-jump=\"panes\">Live Panes</button>"));
        assert!(INDEX_HTML.contains("data-open-command-tray"));
        assert!(INDEX_HTML.contains("data-room-jump=\"review\">Review</button>"));
        assert!(INDEX_HTML.contains("data-room-jump=\"evidence\">Evidence</button>"));
        assert!(INDEX_HTML.contains("class=\"room-lane-button\" data-room-jump"));
        assert!(!INDEX_HTML.contains("class=\"review-lane-button\""));
    }

    #[test]
    fn desktop_shell_labels_parent_decisions_separately_from_sweep_blockers() {
        assert!(INDEX_HTML.contains("parent decisions</div>"));
        assert!(INDEX_HTML.contains("sweep blocker"));
        assert!(INDEX_HTML.contains("No sweep blockers from latest child read."));
        assert!(INDEX_HTML.contains("parentDecisionWord"));
        assert!(INDEX_HTML
            .contains("const attentionCount = Number(stats?.needs_attention ?? needsReview);"));
        assert!(INDEX_HTML.contains("${attentionCount} parent decision${parentDecisionWord}"));
        assert!(!INDEX_HTML.contains("${needsReview} ${attentionWord} attention."));
        assert!(!INDEX_HTML.contains("needs-input items"));
        assert!(!INDEX_HTML.contains("0 attention."));
    }

    #[test]
    fn desktop_shell_can_collapse_regions_and_starts_cockpit_closed() {
        assert!(INDEX_HTML.contains("id=\"treeToggle\""));
        assert!(INDEX_HTML.contains("id=\"detailsToggle\""));
        assert!(INDEX_HTML.contains("body.hide-tree .tree"));
        assert!(INDEX_HTML.contains("id=\"treeReopen\""));
        assert!(INDEX_HTML.contains(
            "body[data-active-tab=\"panes\"].hide-tree:not(.terminal-expanded) .edge-reopen.tree-edge"
        ));
        assert!(INDEX_HTML.contains("setRegionVisible('hide-tree', 'treeToggle', viewPreferenceKeys.treeVisible, true, { persist: false });"));
        assert!(INDEX_HTML.contains("body.hide-inspector .inspector"));
        assert!(INDEX_HTML
            .contains("setChromeToggle('show-command-deck', 'controlsToggle', viewPreferenceKeys.commandDeck, false"));
        assert!(INDEX_HTML.contains(
            "setChromeToggle('show-roster', 'rosterToggle', viewPreferenceKeys.roster, false"
        ));
        assert!(INDEX_HTML.contains(
            "setChromeToggle('show-inspector-advanced', 'inspectorAdvancedToggle', viewPreferenceKeys.inspectorAdvanced, false"
        ));
    }

    #[test]
    fn desktop_shell_details_drawer_starts_closed_and_overlays_surface() {
        assert!(INDEX_HTML.contains("id=\"detailsToggle\" aria-pressed=\"false\""));
        assert!(INDEX_HTML.contains(
            "setRegionVisible('hide-inspector', 'detailsToggle', viewPreferenceKeys.detailsVisible, false, { persist: false })"
        ));
        assert!(INDEX_HTML.contains("grid-template-columns: 286px minmax(560px, 1fr);"));
        assert!(INDEX_HTML.contains("body:not(.hide-inspector):not(.pane-wall) .inspector"));
        assert!(INDEX_HTML.contains(
            "body[data-active-tab=\"panes\"].hide-inspector:not(.terminal-expanded) .edge-reopen.details-edge"
        ));
        assert!(INDEX_HTML.contains("box-shadow: -18px 0 42px"));
    }

    #[test]
    fn desktop_shell_workbench_has_direct_drawer_switches() {
        assert!(INDEX_HTML.contains("class=\"drawer-switches\" aria-label=\"Workbench drawers\""));
        assert!(INDEX_HTML.contains("id=\"treeQuickToggle\""));
        assert!(INDEX_HTML.contains("id=\"detailsQuickToggle\""));
        assert!(INDEX_HTML.contains("id=\"commandQuickToggle\""));
        assert!(INDEX_HTML.contains("function syncWorkbenchDrawerButtons()"));
        assert!(INDEX_HTML
            .contains("sync('treeQuickToggle', !document.body.classList.contains('hide-tree'));"));
        assert!(INDEX_HTML.contains(
            "sync('detailsQuickToggle', !document.body.classList.contains('hide-inspector'));"
        ));
        assert!(INDEX_HTML.contains(
            "sync('commandQuickToggle', document.body.classList.contains('show-command-deck'));"
        ));
        assert!(INDEX_HTML
            .contains("document.getElementById('treeQuickToggle').addEventListener('click'"));
        assert!(INDEX_HTML
            .contains("document.getElementById('detailsQuickToggle').addEventListener('click'"));
        assert!(INDEX_HTML
            .contains("document.getElementById('commandQuickToggle').addEventListener('click'"));
        assert!(INDEX_HTML.contains("body:not([data-active-tab=\"panes\"]) .drawer-switches"));
    }

    #[test]
    fn desktop_shell_inspector_is_contextual_from_focused_terminal() {
        assert!(INDEX_HTML.contains("class=\"terminal-drawer-actions\""));
        assert!(INDEX_HTML.contains("data-open-inspector=\"pane\""));
        assert!(INDEX_HTML.contains("data-open-inspector=\"command\""));
        assert!(INDEX_HTML.contains("data-open-inspector=\"output\""));
        assert!(INDEX_HTML.contains("data-open-inspector=\"packet\""));
        assert!(INDEX_HTML.contains("aria-label=\"Inspect watched pane\""));
        assert!(INDEX_HTML.contains("aria-label=\"Intervene in watched pane\""));
        assert!(INDEX_HTML.contains("aria-label=\"Read watched pane output\""));
        assert!(INDEX_HTML.contains("aria-label=\"Review watched pane report packet\""));
        assert!(INDEX_HTML.contains(">Inspect</button>"));
        assert!(INDEX_HTML.contains(">Intervene</button>"));
        assert!(INDEX_HTML.contains(">Output</button>"));
        assert!(INDEX_HTML.contains(">Packet</button>"));
        assert!(INDEX_HTML.contains(">Expand pane</button>"));
        assert!(INDEX_HTML.contains("watching one pane"));
        assert!(INDEX_HTML.contains(
            "placeholder=\"Intervention to selected child pane, or broadcast to all child panes.\""
        ));
        assert!(!INDEX_HTML.contains(">D</button>"));
        assert!(!INDEX_HTML.contains(">M</button>"));
        assert!(!INDEX_HTML.contains(">R</button>"));
        assert!(!INDEX_HTML.contains(">P</button>"));
        assert!(!INDEX_HTML.contains(">F</button>"));
        assert!(!INDEX_HTML.contains(">Info</button>"));
        assert!(!INDEX_HTML.contains(">Expand</button>"));
        assert!(!INDEX_HTML.contains("aria-label=\"Message selected pane\""));
        assert!(!INDEX_HTML.contains("focused terminal"));
        assert!(!INDEX_HTML.contains(
            "placeholder=\"Message the selected child pane, or broadcast to all child panes.\""
        ));
        assert!(!INDEX_HTML.contains("title=\"Inspect selected pane details\">Details</button>"));
        assert!(!INDEX_HTML.contains(
            "title=\"Message or broadcast from the selected pane drawer\">Command</button>"
        ));
        assert!(!INDEX_HTML.contains("title=\"Read latest selected pane output\">Output</button>"));
        assert!(INDEX_HTML.contains("aria-label=\"Expand watched pane\""));
        assert!(!INDEX_HTML.contains("aria-label=\"Toggle focused terminal fullscreen\""));
        assert!(!INDEX_HTML.contains(">Fullscreen</button>"));
        assert!(!INDEX_HTML.contains(">[]</button>"));
        assert!(INDEX_HTML.contains("id=\"closeInspector\""));
        assert!(INDEX_HTML.contains("function openInspectorDrawer(tab = 'pane')"));
        assert!(INDEX_HTML.contains("function closeInspectorDrawer()"));
        assert!(INDEX_HTML.contains("document.querySelectorAll('[data-open-inspector]').forEach"));
        assert!(INDEX_HTML
            .contains("document.getElementById('closeInspector').addEventListener('click'"));
        assert!(INDEX_HTML.contains("openInspectorDrawer(button.dataset.openInspector || 'pane');"));
        assert!(INDEX_HTML.contains("closeInspectorDrawer();"));
    }

    #[test]
    fn desktop_shell_command_deck_starts_compact_with_advanced_controls_hidden() {
        assert!(INDEX_HTML.contains("id=\"deckAdvancedToggle\""));
        assert!(INDEX_HTML
            .contains("class=\"deck-button deck-advanced-action danger\" id=\"deckStopSelected\""));
        assert!(INDEX_HTML.contains("class=\"deck-advanced\" id=\"deckAdvancedPanel\""));
        assert!(INDEX_HTML.contains(".deck-advanced {\n      display: none;"));
        assert!(INDEX_HTML.contains("body.show-command-advanced .deck-advanced"));
        assert!(INDEX_HTML.contains(".deck-receipts {\n      display: none;"));
        assert!(INDEX_HTML.contains("body.show-command-advanced .deck-receipts"));
        assert!(INDEX_HTML
            .contains("setChromeToggle('show-command-advanced', 'deckAdvancedToggle', viewPreferenceKeys.commandAdvanced, false"));
        assert!(INDEX_HTML.contains("setCommandDeckVisible"));
    }

    #[test]
    fn desktop_shell_parent_command_trays_use_watched_intervention_language() {
        assert!(INDEX_HTML
            .contains("Watch a child pane, intervene, read it, or broadcast to all children."));
        assert!(INDEX_HTML.contains("id=\"deckMessageSelected\">Intervene watched</button>"));
        assert!(INDEX_HTML.contains("id=\"deckReadSelected\">Read watched</button>"));
        assert!(INDEX_HTML.contains("id=\"deckStopSelected\">Stop watched child</button>"));
        assert!(INDEX_HTML.contains("placeholder=\"Type an intervention, then send it to the watched pane or all child panes.\""));
        assert!(INDEX_HTML.contains("id=\"deckSendSelected\">Send watched</button>"));
        assert!(INDEX_HTML.contains("id=\"wallHudMessage\">Intervene</button>"));
        assert!(INDEX_HTML.contains("id=\"wallHudSendSelected\">Send watched</button>"));
        assert!(INDEX_HTML
            .contains("placeholder=\"Intervention instruction to watched pane or scope\""));
        assert!(INDEX_HTML.contains("list[0]?.title || 'watched pane'"));
        assert!(INDEX_HTML.contains("The workbench is showing one watched pane"));
        assert!(!INDEX_HTML.contains(">Message selected</button>"));
        assert!(!INDEX_HTML.contains(">Read selected</button>"));
        assert!(!INDEX_HTML.contains(">Send selected</button>"));
        assert!(!INDEX_HTML.contains(">Stop selected child</button>"));
        assert!(!INDEX_HTML.contains(
            "Type a parent instruction, then send it to the selected pane or all child panes."
        ));
        assert!(!INDEX_HTML.contains("Intervention instruction to selected pane or scope"));
        assert!(!INDEX_HTML.contains("The workbench is showing the selected pane"));
    }

    #[test]
    fn desktop_shell_advanced_toggles_use_specific_labels() {
        assert!(INDEX_HTML.contains(
            "id=\"deckAdvancedToggle\" aria-pressed=\"false\" title=\"Show child launch, inbox, packet queue, and change radar\">Mission tools</button>"
        ));
        assert!(INDEX_HTML.contains(
            "id=\"inspectorAdvancedToggle\" aria-pressed=\"false\" title=\"Show report, launch, and mission utilities\">Pane tools</button>"
        ));
        assert!(!INDEX_HTML.contains("id=\"deckAdvancedToggle\" aria-pressed=\"false\" title=\"Show child launch, inbox, packet queue, and change radar\">More</button>"));
        assert!(!INDEX_HTML.contains("id=\"inspectorAdvancedToggle\" aria-pressed=\"false\" title=\"Show report, launch, and mission utilities\">More</button>"));
    }

    #[test]
    fn desktop_shell_secondary_chrome_lives_in_view_menu() {
        assert!(INDEX_HTML.contains("id=\"viewMenuToggle\""));
        assert!(INDEX_HTML.contains("class=\"view-menu-panel\" id=\"viewMenuPanel\""));
        assert!(INDEX_HTML.contains(".view-menu-panel {"));
        assert!(INDEX_HTML.contains("display: none;"));
        assert!(INDEX_HTML.contains("body.show-view-menu .view-menu-panel"));
        assert!(!INDEX_HTML.contains("body.pane-wall .view-menu-panel {\n      display: none;"));
        assert!(INDEX_HTML.contains(
            "setChromeToggle('show-view-menu', 'viewMenuToggle', viewPreferenceKeys.viewMenu, false"
        ));
        assert!(INDEX_HTML.contains("<div class=\"view-menu-title\">Pane space</div>"));
        assert!(INDEX_HTML.contains("data-density=\"roomy\""));
        assert!(INDEX_HTML.contains("id=\"treeToggle\""));
        assert!(INDEX_HTML.contains("id=\"detailsToggle\""));
        assert!(INDEX_HTML.contains("id=\"controlsToggle\""));
        assert!(INDEX_HTML.contains("id=\"rosterToggle\""));
    }

    #[test]
    fn desktop_shell_view_menu_uses_workroom_space_language() {
        assert!(INDEX_HTML.contains(
            "id=\"viewMenuToggle\" aria-pressed=\"false\" title=\"Show view and space controls\">View</button>"
        ));
        assert!(INDEX_HTML.contains("<div class=\"view-menu-title\">Pane space</div>"));
        assert!(INDEX_HTML.contains("<span class=\"view-menu-label\">Workbench rails</span>"));
        assert!(INDEX_HTML.contains("id=\"treeToggle\" aria-pressed=\"true\" title=\"Show or hide the session tree\">Session tree</button>"));
        assert!(INDEX_HTML.contains("id=\"detailsToggle\" aria-pressed=\"false\" title=\"Show or hide watched-pane details\">Details drawer</button>"));
        assert!(INDEX_HTML.contains("id=\"controlsToggle\" aria-pressed=\"false\" title=\"Show or hide the parent intervention tray\">Parent tray</button>"));
        assert!(INDEX_HTML.contains("id=\"rosterToggle\" aria-pressed=\"false\" title=\"Show or hide the pane table\">Pane table</button>"));
        assert!(INDEX_HTML.contains("<span class=\"view-menu-label\">All-pane wall</span>"));
        assert!(!INDEX_HTML.contains("<div class=\"view-menu-title\">Layout</div>"));
        assert!(!INDEX_HTML.contains("<span class=\"view-menu-label\">Drawers</span>"));
        assert!(!INDEX_HTML.contains("id=\"controlsToggle\" aria-pressed=\"false\" title=\"Show or hide the parent command tray\">Command</button>"));
        assert!(!INDEX_HTML.contains(
            "id=\"rosterToggle\" aria-pressed=\"false\" title=\"Show or hide the pane table\">Table</button>"
        ));
    }

    #[test]
    fn desktop_shell_layout_menu_is_mode_aware() {
        assert!(INDEX_HTML.contains("data-view-section=\"pane-density\""));
        assert!(INDEX_HTML.contains("data-view-section=\"focus-drawers\""));
        assert!(INDEX_HTML.contains("data-view-section=\"pane-wall\""));
        assert!(INDEX_HTML.contains("body:not(.pane-wall) [data-view-section=\"pane-density\"]"));
        assert!(INDEX_HTML.contains("body.pane-wall #controlsToggle"));
        assert!(INDEX_HTML.contains("body.pane-wall #rosterToggle"));
        assert!(!INDEX_HTML.contains("body.pane-wall [data-view-section=\"focus-drawers\"]"));
        assert!(INDEX_HTML.contains("body:not(.pane-wall) [data-view-section=\"pane-wall\"]"));
        assert!(INDEX_HTML.contains("id=\"wallHudDetailsToggle\""));
        assert!(INDEX_HTML.contains("id=\"wallHudFocusMode\""));
        assert!(INDEX_HTML.contains(
            "setWallHudExpanded(!document.body.classList.contains('wall-hud-expanded'));"
        ));
        assert!(INDEX_HTML.contains("document.getElementById('wallHudFocusMode')"));
    }

    #[test]
    fn desktop_shell_pane_wall_keeps_tree_and_details_as_drawers() {
        assert!(INDEX_HTML.contains("body.pane-wall .tree {\n      position: fixed;"));
        assert!(INDEX_HTML.contains("body.pane-wall .inspector {\n      position: fixed;"));
        assert!(INDEX_HTML.contains("class=\"edge-reopen tree-edge\" id=\"treeReopen\""));
        assert!(INDEX_HTML.contains("class=\"edge-reopen details-edge\" id=\"detailsReopen\""));
        assert!(INDEX_HTML.contains(
            "body[data-active-tab=\"panes\"].hide-tree:not(.terminal-expanded) .edge-reopen.tree-edge"
        ));
        assert!(INDEX_HTML.contains(
            "body[data-active-tab=\"panes\"].hide-inspector:not(.terminal-expanded) .edge-reopen.details-edge"
        ));
        assert!(!INDEX_HTML.contains(
            "body.pane-wall .tree,\n    body.pane-wall .inspector,\n    body.pane-wall .mission-strip"
        ));
        assert!(!INDEX_HTML.contains("body.pane-wall [data-view-section=\"focus-drawers\"]"));
    }

    #[test]
    fn desktop_shell_pane_wall_uses_side_edge_drawer_tabs() {
        assert!(INDEX_HTML.contains("body.pane-wall .edge-reopen {\n      top: 50%;"));
        assert!(INDEX_HTML.contains("transform: translateY(-50%);"));
        assert!(INDEX_HTML.contains("writing-mode: vertical-rl;"));
        assert!(INDEX_HTML.contains("width: 22px;"));
        assert!(INDEX_HTML.contains("opacity: 0.58;"));
        assert!(INDEX_HTML.contains("body.pane-wall .edge-reopen:hover"));
        assert!(INDEX_HTML.contains("body.pane-wall .edge-reopen.tree-edge"));
        assert!(INDEX_HTML.contains("body.pane-wall .edge-reopen.details-edge"));
        assert!(!INDEX_HTML.contains("body.pane-wall .edge-reopen {\n      top: 58px;"));
    }

    #[test]
    fn desktop_shell_pane_wall_drawers_close_from_outside_scrim() {
        assert!(INDEX_HTML.contains("class=\"drawer-scrim\" id=\"drawerScrim\""));
        assert!(INDEX_HTML.contains("aria-label=\"Close open drawers\""));
        assert!(INDEX_HTML.contains(".drawer-scrim {\n      display: none;"));
        assert!(INDEX_HTML.contains("body.pane-wall:not(.hide-tree) .drawer-scrim"));
        assert!(INDEX_HTML.contains("body.pane-wall:not(.hide-inspector) .drawer-scrim"));
        assert!(
            INDEX_HTML.contains("document.getElementById('drawerScrim').addEventListener('click'")
        );
        assert!(INDEX_HTML.contains("closeOpenPaneWallDrawers();"));
    }

    #[test]
    fn desktop_shell_left_drawer_can_close_itself_like_the_right_drawer() {
        assert!(INDEX_HTML.contains("id=\"closeTreeDrawer\""));
        assert!(INDEX_HTML.contains("aria-label=\"Close session tree drawer\""));
        assert!(INDEX_HTML.contains("function closeTreeDrawer()"));
        assert!(INDEX_HTML.contains(
            "setRegionVisible('hide-tree', 'treeToggle', viewPreferenceKeys.treeVisible, false"
        ));
        assert!(INDEX_HTML
            .contains("document.getElementById('closeTreeDrawer').addEventListener('click'"));
        assert!(INDEX_HTML.contains("closeTreeDrawer();"));
        assert!(INDEX_HTML.contains("id=\"treeReopen\""));
    }

    #[test]
    fn desktop_shell_escape_closes_open_drawers_first() {
        assert!(INDEX_HTML.contains("function closeOpenPaneDrawers()"));
        assert!(INDEX_HTML.contains("function closeOpenPaneWallDrawers()"));
        assert!(INDEX_HTML.contains("document.body.classList.contains('pane-wall')"));
        assert!(INDEX_HTML.contains("closeTreeDrawer();"));
        assert!(INDEX_HTML.contains("closeInspectorDrawer();"));
        assert!(INDEX_HTML.contains("if (event.key === 'Escape' && closeOpenPaneDrawers())"));
        assert!(
            INDEX_HTML
                .find("if (event.key === 'Escape' && closeOpenPaneDrawers())")
                .unwrap()
                < INDEX_HTML
                    .find(
                        "if (event.key === 'Escape' && document.body.classList.contains('terminal-expanded'))",
                    )
                .unwrap()
        );
    }

    #[test]
    fn desktop_shell_escape_closes_view_menu_before_drawers_or_terminal() {
        assert!(INDEX_HTML.contains("function closeOpenViewMenu()"));
        assert!(INDEX_HTML.contains("document.body.classList.contains('show-view-menu')"));
        assert!(INDEX_HTML.contains("if (event.key === 'Escape' && closeOpenViewMenu())"));
        assert!(
            INDEX_HTML
                .find("if (event.key === 'Escape' && closeOpenViewMenu())")
                .unwrap()
                < INDEX_HTML
                    .find("if (event.key === 'Escape' && closeOpenPaneDrawers())")
                    .unwrap()
        );
        assert!(
            INDEX_HTML
                .find("if (event.key === 'Escape' && closeOpenViewMenu())")
                .unwrap()
                < INDEX_HTML
                    .find(
                        "if (event.key === 'Escape' && document.body.classList.contains('terminal-expanded'))",
                    )
                    .unwrap()
        );
    }

    #[test]
    fn desktop_shell_escape_closes_intervention_rail_before_terminal() {
        assert!(INDEX_HTML.contains("function closeOpenWallHud()"));
        assert!(INDEX_HTML.contains("document.body.classList.contains('wall-hud-expanded')"));
        assert!(INDEX_HTML.contains("setWallHudExpanded(false);"));
        assert!(INDEX_HTML.contains("if (event.key === 'Escape' && closeOpenWallHud())"));
        assert!(
            INDEX_HTML
                .find("if (event.key === 'Escape' && closeOpenPaneDrawers())")
                .unwrap()
                < INDEX_HTML
                    .find("if (event.key === 'Escape' && closeOpenWallHud())")
                    .unwrap()
        );
        assert!(
            INDEX_HTML
                .find("if (event.key === 'Escape' && closeOpenWallHud())")
                .unwrap()
                < INDEX_HTML
                    .find(
                        "if (event.key === 'Escape' && document.body.classList.contains('terminal-expanded'))",
                    )
                    .unwrap()
        );
    }

    #[test]
    fn desktop_shell_restores_wall_mode_after_drawer_preferences() {
        let tree_restore = INDEX_HTML
            .find(
                "setRegionVisible('hide-tree', 'treeToggle', viewPreferenceKeys.treeVisible, readPreference",
            )
            .expect("tree drawer preference should be restored");
        let details_restore = INDEX_HTML
            .find(
                "setRegionVisible('hide-inspector', 'detailsToggle', viewPreferenceKeys.detailsVisible, false",
            )
            .expect("details drawer preference should be restored");
        let wall_restore = INDEX_HTML
            .find("setPaneWall(readPreference(viewPreferenceKeys.paneWall")
            .expect("pane wall preference should be restored");

        assert!(tree_restore < wall_restore);
        assert!(details_restore < wall_restore);
    }

    #[test]
    fn desktop_shell_layout_menu_only_appears_on_live_tab() {
        assert!(INDEX_HTML.contains(
            "body:not([data-active-tab=\"panes\"]) .view-menu {\n      display: none;\n    }"
        ));
        assert!(INDEX_HTML.contains("if (targetTab !== 'panes') closeViewMenu();"));
    }

    #[test]
    fn desktop_shell_view_menu_closes_after_region_toggle() {
        assert!(INDEX_HTML.contains("function closeViewMenu()"));
        assert!(INDEX_HTML.contains(
            "toggleRegionVisible('hide-tree', 'treeToggle', viewPreferenceKeys.treeVisible);"
        ));
        assert!(INDEX_HTML.contains(
            "toggleRegionVisible('hide-inspector', 'detailsToggle', viewPreferenceKeys.detailsVisible);"
        ));
        assert!(INDEX_HTML.contains(
            "toggleChrome('show-roster', 'rosterToggle', viewPreferenceKeys.roster, { persist: false });"
        ));
        assert!(INDEX_HTML.matches("closeViewMenu();").count() >= 4);
    }

    #[test]
    fn desktop_shell_loads_workroom_projection_as_parent_child_truth() {
        assert!(INDEX_HTML.contains("let workroomProjection = null;"));
        assert!(INDEX_HTML.contains("const workroomUrl ="));
        assert!(INDEX_HTML.contains("/mission/workroom"));
        assert!(INDEX_HTML.contains("fetch(workroomUrl, { cache: 'no-store' })"));
        assert!(INDEX_HTML.contains("function applyWorkroomProjection(wave, model)"));
        assert!(INDEX_HTML.contains("workroomProjection?.selected_pane_id"));
        assert!(INDEX_HTML.contains("workroomProjection?.stats"));
    }

    #[test]
    fn desktop_shell_records_proof_receipts_as_evidence_ledger() {
        assert!(INDEX_HTML.contains("let evidenceLedger = [];"));
        assert!(INDEX_HTML.contains("evidenceLedger: 'herdr.desktop.evidenceLedger.v1'"));
        assert!(INDEX_HTML.contains("function recordEvidenceReceipt(entry)"));
        assert!(INDEX_HTML.contains("persistEvidenceLedger();"));
        assert!(INDEX_HTML.contains("Proof receipts"));
        assert!(INDEX_HTML.contains("recordEvidenceReceipt({"));
    }

    #[test]
    fn desktop_shell_left_rail_is_explicit_parent_child_tree() {
        assert!(INDEX_HTML.contains("<span>Session tree</span>"));
        assert!(!INDEX_HTML.contains("Project session"));
        assert!(!INDEX_HTML.contains("id=\"projectNodeBadge\""));
        assert!(!INDEX_HTML.contains("document.getElementById('projectNodeBadge').textContent"));
        assert!(INDEX_HTML.contains("class=\"mission-node\" data-wave"));
        assert!(INDEX_HTML.contains("data-tree-role=\"parent\""));
        assert!(INDEX_HTML.contains("parent root / ${escapeHtml(children.length)} child"));
        assert!(!INDEX_HTML.contains("root terminal pane - owns"));
        assert!(INDEX_HTML.contains("data-tree-create-child"));
        assert!(INDEX_HTML.contains("class=\"tree-parent-actions\""));
        assert!(INDEX_HTML.contains("aria-label=\"Create child pane under parent session\""));
        assert!(INDEX_HTML.contains("data-toggle=\"parent-pane\""));
        assert!(INDEX_HTML.contains("data-tree-role=\"child\""));
        assert!(INDEX_HTML.contains(".mission-node.active"));
        assert!(INDEX_HTML.contains("document.querySelectorAll('[data-tree-create-child]')"));
    }

    #[test]
    fn desktop_shell_parent_row_owns_child_tree_controls() {
        assert!(INDEX_HTML.contains("class=\"mission-node\" data-wave"));
        assert!(INDEX_HTML.contains("data-toggle=\"parent-pane\""));
        assert!(INDEX_HTML.contains("aria-label=\"Create child pane under parent session\""));
        assert!(INDEX_HTML.contains("class=\"tree-parent-actions\""));
        assert!(INDEX_HTML.contains("parent root / ${escapeHtml(children.length)} child"));
        assert!(!INDEX_HTML.contains("class=\"tree-group-label\""));
        assert!(!INDEX_HTML.contains("<span>Children</span>"));
        assert!(!INDEX_HTML.contains("data-tree-collapse=\"parent-pane\""));
        assert!(!INDEX_HTML.contains("toggle.textContent = collapsed ? 'Show' : 'Hide'"));
    }

    #[test]
    fn desktop_shell_tree_expansion_uses_child_pane_identity() {
        assert!(INDEX_HTML.contains("const childDetailLabel ="));
        assert!(INDEX_HTML.contains("<strong>${escapeHtml(wave.title)}</strong>"));
        assert!(!INDEX_HTML.contains("<strong>Live terminal</strong>"));
    }

    #[test]
    fn desktop_shell_uses_human_visible_pane_labels_not_raw_ids() {
        assert!(INDEX_HTML.contains("function visiblePaneLabel(wave"));
        assert!(INDEX_HTML.contains("function paneDebugTitle(wave"));
        assert!(INDEX_HTML.contains("title=\"${escapeHtml(paneDebugTitle(wave))}\""));
        assert!(!INDEX_HTML.contains("child session pane - ${escapeHtml(paneIdentity(wave))}"));
        assert!(!INDEX_HTML.contains(">real pane ${escapeHtml(paneIdentity(wave))}<"));
        assert!(INDEX_HTML.contains("visiblePaneLabel(wave, childIndex"));
    }

    #[test]
    fn desktop_shell_focus_terminal_title_is_human_readable() {
        assert!(INDEX_HTML.contains("function focusedTerminalTitle(wave"));
        assert!(INDEX_HTML.contains(
            "return `Watching pane: ${wave.role === 'parent' ? 'Parent session' : wave.title}`;"
        ));
        assert!(INDEX_HTML.contains("terminalTitle.title = paneDebugTitle(wave)"));
        assert!(INDEX_HTML.contains("terminalTitle.textContent = focusedTerminalTitle(wave"));
        assert!(!INDEX_HTML.contains(
            "return `Focused terminal: ${wave.role === 'parent' ? 'Parent session' : wave.title}`;"
        ));
        assert!(!INDEX_HTML.contains("Selected pane: ${wave.title} (${wave.terminal})"));
    }

    #[test]
    fn desktop_shell_right_drawer_uses_context_tabs() {
        assert!(INDEX_HTML.contains("<h2 class=\"section-title\">Watched pane</h2>"));
        assert!(INDEX_HTML.contains("title=\"Close watched-pane drawer\""));
        assert!(INDEX_HTML.contains("class=\"inspector-tabs\""));
        assert!(INDEX_HTML.contains("aria-label=\"Watched pane drawer\""));
        assert!(INDEX_HTML.contains("data-inspector-tab=\"pane\""));
        assert!(INDEX_HTML.contains("data-inspector-tab=\"command\""));
        assert!(INDEX_HTML.contains("data-inspector-tab=\"output\""));
        assert!(INDEX_HTML.contains("data-inspector-tab=\"packet\""));
        assert!(INDEX_HTML.contains("data-inspector-tab=\"pane\">Inspect</button>"));
        assert!(INDEX_HTML.contains("data-inspector-tab=\"command\">Intervene</button>"));
        assert!(INDEX_HTML.contains("data-inspector-tab=\"output\">Output</button>"));
        assert!(INDEX_HTML.contains("data-inspector-tab=\"packet\">Packet</button>"));
        assert!(INDEX_HTML.contains("data-inspector-panel=\"pane\""));
        assert!(INDEX_HTML.contains("data-inspector-panel=\"command\""));
        assert!(INDEX_HTML.contains("data-inspector-panel=\"output\""));
        assert!(INDEX_HTML.contains("data-inspector-panel=\"packet\""));
        assert!(INDEX_HTML.contains("<h2 id=\"selectedTitle\">No pane watched</h2>"));
        assert!(INDEX_HTML.contains("<h2>Intervention rail</h2>"));
        assert!(INDEX_HTML.contains("id=\"sendSelected\">Send watched</button>"));
        assert!(INDEX_HTML.contains("<span id=\"controlTarget\">watched pane</span>"));
        assert!(INDEX_HTML.contains("document.getElementById('selectedTitle').textContent = `Watching ${wave.title} (${wave.role})`;"));
        assert!(INDEX_HTML.contains(
            "document.getElementById('controlTarget').textContent = `watching ${wave.paneId}`;"
        ));
        assert!(INDEX_HTML.contains("function setInspectorTab(tab)"));
        assert!(INDEX_HTML.contains("document.body.dataset.activeInspectorTab = next"));
        assert!(INDEX_HTML.contains("inspector-advanced-panels"));
        assert!(!INDEX_HTML.contains("<h2 class=\"section-title\">Selected pane</h2>"));
        assert!(!INDEX_HTML.contains("aria-label=\"Selected pane drawer\""));
        assert!(!INDEX_HTML.contains("data-inspector-tab=\"pane\">Pane</button>"));
        assert!(!INDEX_HTML.contains("data-inspector-tab=\"command\">Command</button>"));
        assert!(!INDEX_HTML.contains("<h2 id=\"selectedTitle\">No pane selected</h2>"));
        assert!(!INDEX_HTML.contains("<h2>Pane command</h2>"));
        assert!(!INDEX_HTML.contains("id=\"sendSelected\">Send selected</button>"));
        assert!(!INDEX_HTML.contains("<span id=\"controlTarget\">selected pane</span>"));
    }

    #[test]
    fn desktop_shell_default_workroom_is_focus_mode_not_full_tile_wall() {
        assert!(INDEX_HTML.contains("id=\"paneWallToggle\""));
        assert!(INDEX_HTML.contains("return count ? `All panes (${count})` : 'All panes';"));
        assert!(INDEX_HTML.contains("function allPanesButtonLabel()"));
        assert!(INDEX_HTML.contains("function updateLiveModeSwitch(enabled"));
        assert!(INDEX_HTML.contains("gridButton.textContent = allPanesButtonLabel();"));
        assert!(!INDEX_HTML.contains("return count ? `Command wall (${count})` : 'Command wall';"));
        assert!(INDEX_HTML.contains("body:not(.pane-wall) .terminal-panel"));
        assert!(INDEX_HTML.contains("body:not(.pane-wall) .mini-terminal"));
        assert!(INDEX_HTML.contains("function shouldUseTileStreams()"));
        assert!(INDEX_HTML.contains("if (shouldUseTileStreams())"));
        assert!(INDEX_HTML.contains("body.pane-wall .terminal-panel"));
    }

    #[test]
    fn desktop_shell_live_modes_are_explicit_segmented_controls() {
        assert!(INDEX_HTML.contains("class=\"live-mode-switch\""));
        assert!(INDEX_HTML.contains("id=\"focusModeToggle\""));
        assert!(INDEX_HTML.contains("data-live-mode=\"one\""));
        assert!(INDEX_HTML.contains("id=\"paneWallToggle\""));
        assert!(INDEX_HTML.contains("data-live-mode=\"all\""));
        assert!(INDEX_HTML.contains("function updateLiveModeSwitch(enabled"));
        assert!(INDEX_HTML.contains("focusButton.setAttribute('aria-pressed', String(!enabled));"));
        assert!(INDEX_HTML.contains("gridButton.setAttribute('aria-pressed', String(enabled));"));
        assert!(INDEX_HTML.contains("gridButton.textContent = allPanesButtonLabel();"));
        assert!(INDEX_HTML.contains("document.getElementById('focusModeToggle').addEventListener"));
        assert!(
            !INDEX_HTML.contains("button.textContent = enabled ? 'Focus' : paneWallButtonLabel();")
        );
    }

    #[test]
    fn desktop_shell_live_modes_use_workroom_language() {
        assert!(INDEX_HTML.contains("id=\"focusModeToggle\" data-live-mode=\"one\""));
        assert!(INDEX_HTML.contains(">One pane</button>"));
        assert!(INDEX_HTML.contains("id=\"paneWallToggle\" data-live-mode=\"all\""));
        assert!(INDEX_HTML.contains(">All panes</button>"));
        assert!(INDEX_HTML.contains("Return to one watched pane"));
        assert!(INDEX_HTML.contains("All parent/child terminal panes are visible"));
        assert!(INDEX_HTML.contains("The workbench is showing one watched pane"));
        assert!(!INDEX_HTML.contains("Command wall"));
        assert!(!INDEX_HTML.contains(">Focus</button>"));
        assert!(!INDEX_HTML.contains("Focus mode</button>"));
    }

    #[test]
    fn desktop_shell_workbench_mode_switch_precedes_secondary_view_menu() {
        let mode_switch = INDEX_HTML
            .find("<div class=\"live-mode-switch\"")
            .expect("mode switch exists");
        let view_menu = INDEX_HTML
            .find("<div class=\"view-menu\">")
            .expect("view menu exists");
        assert!(mode_switch < view_menu);
    }

    #[test]
    fn desktop_shell_live_mode_switch_only_appears_on_live_tab() {
        assert!(INDEX_HTML.contains(
            "body:not([data-active-tab=\"panes\"]) .live-mode-switch {\n      display: none;\n    }"
        ));
    }

    #[test]
    fn desktop_shell_topbar_status_uses_human_runtime_language() {
        assert!(INDEX_HTML.contains("function runtimeStatusForWave(wave)"));
        assert!(INDEX_HTML.contains("function setRuntimeStatus(message, live = true)"));
        assert!(
            INDEX_HTML.contains("setRuntimeStatus(runtimeStatusForWave(waves[selectedWaveId]));")
        );
        assert!(INDEX_HTML.contains("setRuntimeStatus('loading panes', false);"));
        assert!(!INDEX_HTML.contains("focus frame"));
        assert!(!INDEX_HTML.contains("connected to ${wave.terminal}"));
        assert!(!INDEX_HTML
            .contains("streamStatus.textContent = `${frame.width || 1}x${frame.height || 1}"));
    }

    #[test]
    fn desktop_shell_watched_pane_status_avoids_attach_and_raw_id_language() {
        assert!(INDEX_HTML.contains("<span id=\"terminalStatus\">interactive</span>"));
        assert!(INDEX_HTML.contains("terminalStatus.textContent = 'interactive';"));
        assert!(INDEX_HTML.contains("terminalStatus.textContent = 'offline';"));
        assert!(INDEX_HTML.contains("terminalStatus.textContent = 'no pane watched';"));
        assert!(INDEX_HTML.contains("parent lane watching ${childWaves().length} child pane"));
        assert!(INDEX_HTML.contains("parent terminal attached"));
        assert!(INDEX_HTML.contains("child terminal attached"));
        assert!(!INDEX_HTML.contains("attached read/write"));
        assert!(!INDEX_HTML.contains(
            "terminalNode.textContent = `${wave.terminal || 'no terminal'} - ${wave.paneId}`;"
        ));
        assert!(!INDEX_HTML
            .contains("parent command pane watching ${childWaves().length} child terminal pane"));
    }

    #[test]
    fn desktop_shell_all_panes_mode_names_the_actual_terminal_wall() {
        assert!(INDEX_HTML.contains("function allPanesButtonLabel()"));
        assert!(INDEX_HTML.contains("return count ? `All panes (${count})` : 'All panes';"));
        assert!(INDEX_HTML.contains("title=\"Show every live parent/child terminal pane\""));
        assert!(INDEX_HTML.contains("All parent/child terminal panes are visible"));
        assert!(!INDEX_HTML.contains("return count ? `Command wall (${count})` : 'Command wall';"));
        assert!(!INDEX_HTML.contains("return count ? `Grid (${count})` : 'Grid';"));
        assert!(!INDEX_HTML.contains(">Grid</button>"));
        assert!(!INDEX_HTML.contains("Show every live parent/child terminal pane in a grid"));
    }

    #[test]
    fn desktop_shell_all_panes_mode_is_scoped_to_live_tab() {
        assert!(INDEX_HTML.contains(
            "if (targetTab !== 'panes' && document.body.classList.contains('pane-wall'))"
        ));
        assert!(INDEX_HTML.contains("setPaneWall(false, { reconnect: false });"));
        assert!(INDEX_HTML.contains(
            "if (targetTab === 'panes' && selectedWaveId) setTimeout(() => reconnectStreamsForSelection(), 80);"
        ));
    }

    #[test]
    fn desktop_shell_pane_wall_tile_actions_use_readable_labels() {
        assert!(INDEX_HTML.contains(
            "node.addEventListener('click', () => {\n          selectWave(node.dataset.wave);"
        ));
        assert!(!INDEX_HTML.contains("data-tile-action=\"select\""));
        assert!(!INDEX_HTML.contains("title=\"Focus this Herdr pane\">focus</button>"));
        assert!(INDEX_HTML.contains(
            "data-tile-action=\"message\" data-message-wave=\"${escapeHtml(wave.id)}\" title=\"Intervene in this child pane\">intervene</button>"
        ));
        assert!(INDEX_HTML.contains(
            "data-tile-action=\"full\" data-full=\"${escapeHtml(wave.id)}\" title=\"Expand this pane\">expand</button>"
        ));
        assert!(INDEX_HTML.contains(
            "data-tile-action=\"read\" data-read-wave=\"${escapeHtml(wave.id)}\" title=\"Read this pane output into the parent drawer\">read</button>"
        ));
        assert!(!INDEX_HTML.contains(">select</button>"));
        assert!(!INDEX_HTML.contains(">msg</button>"));
        assert!(!INDEX_HTML.contains(">message</button>"));
        assert!(!INDEX_HTML.contains(">full</button>"));
        assert!(!INDEX_HTML.contains("Open this terminal pane fullscreen"));
    }

    #[test]
    fn desktop_shell_live_focus_has_parent_command_strip() {
        assert!(INDEX_HTML.contains("class=\"live-command-strip\" id=\"liveCommandStrip\""));
        assert!(INDEX_HTML.contains("id=\"liveCommandSummary\""));
        assert!(INDEX_HTML.contains("id=\"liveCommandActions\""));
        assert!(INDEX_HTML.contains("function renderLiveCommandStrip"));
        assert!(INDEX_HTML.contains("Parent command"));
        assert!(INDEX_HTML.contains("body:not(.pane-wall) .live-command-strip"));
        assert!(INDEX_HTML.contains("body.pane-wall .live-command-strip"));
        assert!(INDEX_HTML.contains("renderLiveCommandStrip({"));
        assert!(INDEX_HTML.contains("roomJumpButton('review', 'open review')"));
        assert!(INDEX_HTML.contains("roomJumpButton('audit', 'audit gates')"));
    }

    #[test]
    fn desktop_shell_live_focus_keeps_center_terminal_first() {
        assert!(INDEX_HTML.contains(
            "body:not(.pane-wall) .tab-page[data-page=\"panes\"].active {\n      grid-template-rows: minmax(0, 1fr);"
        ));
        assert!(INDEX_HTML
            .contains("body:not(.pane-wall) .live-command-strip {\n\t      display: none;"));
        assert!(INDEX_HTML.contains("id=\"deckSendAll\">Broadcast scope</button>"));
        assert!(INDEX_HTML.contains("id=\"wallHudSendScope\">Broadcast scope</button>"));
        assert!(!INDEX_HTML.contains("id=\"deckSendAll\">Send scope</button>"));
        assert!(!INDEX_HTML.contains("id=\"wallHudSendScope\">Send scope</button>"));
        assert!(!INDEX_HTML.contains(
            "body:not(.pane-wall) .tab-page[data-page=\"panes\"].active {\n      grid-template-rows: auto minmax(0, 1fr);"
        ));
    }

    #[test]
    fn desktop_shell_live_command_strip_uses_human_decision_language() {
        assert!(INDEX_HTML.contains("function liveDecisionLabel(reason"));
        assert!(INDEX_HTML.contains("return 'contract needs repair';"));
        assert!(INDEX_HTML.contains("return 'report packet incomplete';"));
        assert!(INDEX_HTML.contains(
            "Check ${attention[0].wave.title}: ${liveDecisionLabel(attention[0].reason)}"
        ));
        assert!(INDEX_HTML.contains("const blockerText = attention.length"));
        assert!(INDEX_HTML.contains("const acceptanceText = readyForAcceptance.length"));
        assert!(INDEX_HTML.contains("shared checkout"));
        assert!(!INDEX_HTML.contains("${attention[0].wave.title}: ${attention[0].reason.label}"));
    }

    #[test]
    fn desktop_shell_live_command_strip_uses_parent_action_labels() {
        assert!(INDEX_HTML.contains("const liveActionControls ="));
        assert!(INDEX_HTML.contains("selectButton(attention[0].wave, 'inspect child')"));
        assert!(INDEX_HTML.contains("readButton(attention[0].wave, 'read output')"));
        assert!(INDEX_HTML.contains("missingButton(attention[0].wave, 'deck', 'request packet')"));
        assert!(INDEX_HTML.contains("roomJumpButton('review', 'open review')"));
        assert!(INDEX_HTML.contains("roomJumpButton('audit', 'audit gates')"));
        assert!(INDEX_HTML.contains("actions.innerHTML = `${actionControls}"));
        assert!(!INDEX_HTML.contains("actions.innerHTML = `${nextActionControls}"));
    }

    #[test]
    fn desktop_shell_ops_actions_use_parent_facing_labels() {
        assert!(INDEX_HTML.contains("function selectButton(wave, label = 'focus pane')"));
        assert!(INDEX_HTML.contains("function readButton(wave, label = 'read output')"));
        assert!(INDEX_HTML.contains(
            "function missingButton(wave, destination = 'drawer', label = 'request packet')"
        ));
        assert!(INDEX_HTML.contains("function ingestButton(wave, label = 'ingest output')"));
        assert!(!INDEX_HTML.contains("function selectButton(wave, label = 'select')"));
        assert!(!INDEX_HTML.contains("function readButton(wave, label = 'read')"));
        assert!(!INDEX_HTML
            .contains("function missingButton(wave, destination = 'drawer', label = 'request')"));
        assert!(!INDEX_HTML.contains("function ingestButton(wave)"));
        assert!(!INDEX_HTML.contains("selectButton(wave, 'select')"));
        assert!(!INDEX_HTML.contains("statusButton(wave, 'needs_review', 'review')"));
    }

    #[test]
    fn desktop_shell_direct_pane_interactions_expand_panes() {
        assert!(INDEX_HTML.contains("function openExpandedPane(id)"));
        assert!(INDEX_HTML.contains("body.terminal-expanded.pane-wall .terminal-panel"));
        assert!(INDEX_HTML.contains("openExpandedPane(node.dataset.wave);"));
        assert!(INDEX_HTML.contains("openExpandedPane(button.dataset.full);"));
        assert!(INDEX_HTML.contains("openExpandedPane(node.dataset.wallDagWave);"));
        assert!(!INDEX_HTML.contains("openWaveFullscreen"));
        assert!(!INDEX_HTML.contains("Open this terminal pane fullscreen"));
        assert!(!INDEX_HTML.contains("node.addEventListener('dblclick', event => {\n          if (!node.dataset.toggle) return;\n          event.preventDefault();\n          toggleTree(node.dataset.toggle);\n        });"));
    }

    #[test]
    fn desktop_shell_workroom_uses_full_surface_height() {
        assert!(INDEX_HTML.contains("body[data-active-tab=\"panes\"]:not(.pane-wall) .surface"));
        assert!(INDEX_HTML.contains("grid-template-rows: minmax(0, 1fr);"));
    }

    #[test]
    fn desktop_shell_command_and_roster_are_bottom_trays() {
        assert!(INDEX_HTML.contains(".pane-command-deck {"));
        assert!(INDEX_HTML.contains(".pane-roster {"));
        assert!(INDEX_HTML.matches("position: absolute;").count() >= 3);
        assert!(INDEX_HTML.contains("Parent command tray"));
        assert!(!INDEX_HTML.contains(
	            "body.show-command-deck .tab-page[data-page=\"panes\"].active {\n      grid-template-rows: minmax(300px, 1fr) auto;"
	        ));
        assert!(!INDEX_HTML.contains(
	            "body.show-roster .tab-page[data-page=\"panes\"].active {\n      grid-template-rows: minmax(300px, 1fr) auto;"
	        ));
    }

    #[test]
    fn desktop_shell_focus_mode_uses_left_tree_instead_of_duplicate_pane_rail() {
        assert!(INDEX_HTML.contains("class=\"pane-rail-summary\""));
        assert!(INDEX_HTML.contains("body:not(.pane-wall) .wave-grid {\n      display: none;"));
        assert!(INDEX_HTML.contains(
            "body:not(.pane-wall) .tab-page[data-page=\"panes\"].active {\n      grid-template-rows: minmax(0, 1fr);"
        ));
        assert!(INDEX_HTML.contains("function focusRailLabel(wave"));
        assert!(!INDEX_HTML.contains("grid-auto-columns: minmax(132px, 168px);"));
        assert!(!INDEX_HTML.contains("max-height: 60px;"));
        assert!(!INDEX_HTML.contains("selected pane' : 'click to focus'"));
        assert!(!INDEX_HTML.contains("const railSignal ="));
    }

    #[test]
    fn desktop_shell_grid_mode_uses_live_terminal_tiles() {
        assert!(INDEX_HTML.contains("body.pane-wall .wave-grid"));
        assert!(INDEX_HTML.contains("grid-template-columns: repeat(auto-fit, minmax(360px, 1fr));"));
        assert!(INDEX_HTML.contains(
            "body.pane-wall[data-pane-count=\"7\"] .wave-grid,\n    body.pane-wall[data-pane-count=\"8\"] .wave-grid {\n      grid-template-columns: repeat(3, minmax(0, 1fr));"
        ));
        assert!(INDEX_HTML.contains("body:not(.pane-wall) .terminal-role"));
        assert!(INDEX_HTML.contains("display: none;"));
        assert!(INDEX_HTML.contains(
            "const roleBadge = isParent ? 'PARENT' : `CHILD ${childIndex || ''}`.trim();"
        ));
    }

    #[test]
    fn desktop_shell_grid_tiles_mark_selected_pane_without_extra_signal_text() {
        assert!(INDEX_HTML
            .contains("data-rail-selected=\"${wave.id === selectedWaveId ? 'true' : 'false'}\""));
        assert!(INDEX_HTML.contains("node.classList.toggle('active', node.dataset.wave === id);"));
        assert!(!INDEX_HTML.contains("data-rail-signal=\"selected\""));
        assert!(!INDEX_HTML.contains("body:not(.pane-wall) .wave-card.active .rail-signal"));
        assert!(!INDEX_HTML.contains("click to focus"));
    }

    #[test]
    fn desktop_shell_all_panes_hides_tile_chrome_until_requested() {
        assert!(INDEX_HTML.contains("body.pane-wall .wave-card {\n      grid-column: auto;\n      min-height: 300px;\n      grid-template-rows: auto minmax(0, 1fr);"));
        assert!(INDEX_HTML.contains("body.pane-wall .tile-controls {\n      display: none;"));
        assert!(INDEX_HTML.contains("body.pane-wall .wave-card:hover .tile-controls,\n    body.pane-wall .wave-card:focus-visible .tile-controls,\n    body.pane-wall .tile-controls:focus-within {\n      display: flex;"));
        assert!(INDEX_HTML.contains("body.pane-wall .pane-footer {\n      display: none;"));
        assert!(INDEX_HTML.contains("body.pane-wall .wave-card:hover .pane-footer,\n    body.pane-wall .wave-card:focus-visible .pane-footer {\n      display: grid;"));
        assert!(!INDEX_HTML.contains("body.pane-wall .wave-card:focus-within .pane-footer"));
        assert!(!INDEX_HTML.contains("body.pane-wall .wave-card.active .pane-footer"));
    }

    #[test]
    fn desktop_shell_pane_wall_hud_is_compact_by_default() {
        assert!(INDEX_HTML.contains("id=\"wallHudMore\""));
        assert!(INDEX_HTML.contains(
            "class=\"wall-hud-button\" id=\"wallHudMore\" aria-pressed=\"false\">Intervene</button>"
        ));
        assert!(INDEX_HTML.contains("function setWallHudExpanded(enabled"));
        let set_pane_wall = INDEX_HTML
            .find("function setPaneWall(enabled")
            .expect("setPaneWall should exist");
        let compact_reset = INDEX_HTML[set_pane_wall..]
            .find("setWallHudExpanded(false);")
            .expect("pane wall should compact the HUD");
        let live_mode_sync = INDEX_HTML[set_pane_wall..]
            .find("updateLiveModeSwitch(enabled);")
            .expect("pane wall should update the live mode switch");
        assert!(compact_reset < live_mode_sync);
        assert!(INDEX_HTML.contains("setWallHudExpanded(false);"));
        assert!(!INDEX_HTML.contains("wallHudExpanded:"));
        assert!(!INDEX_HTML.contains("viewPreferenceKeys.wallHudExpanded"));
        assert!(INDEX_HTML.contains("body.pane-wall .surface {\n      padding: 8px 8px 68px;"));
        assert!(INDEX_HTML.contains("body.pane-wall:not(.wall-hud-expanded) .wall-hud-command"));
        assert!(INDEX_HTML.contains("body.pane-wall:not(.wall-hud-expanded) .wall-hud-meter"));
        assert!(INDEX_HTML.contains("body.pane-wall:not(.wall-hud-expanded) #wallHudMessage"));
        assert!(INDEX_HTML.contains("body.pane-wall:not(.wall-hud-expanded) #wallHudSweep"));
        assert!(INDEX_HTML.contains("body.pane-wall.wall-hud-expanded .wall-hud"));
    }

    #[test]
    fn desktop_shell_pane_wall_hud_carries_selected_pane_context() {
        assert!(INDEX_HTML.contains("class=\"wall-hud-context\" id=\"wallHudContext\""));
        assert!(INDEX_HTML.contains("const context = document.getElementById('wallHudContext');"));
        assert!(INDEX_HTML.contains("function renderWallContext(wave"));
        assert!(INDEX_HTML.contains("class=\"wall-context-pill"));
        assert!(INDEX_HTML.contains("packet ${packet.done}/${packet.required}"));
        assert!(INDEX_HTML
            .contains("const gateText = dependencyGateLabel(gate).replace(/^gate\\s+/i, '');"));
        assert!(INDEX_HTML.contains("gate ${gateText}"));
        assert!(INDEX_HTML.contains("blast ${wave.blast}"));
        assert!(INDEX_HTML.contains("context.innerHTML = renderWallContext(wave);"));
        assert!(!INDEX_HTML.contains(
            "body.pane-wall:not(.wall-hud-expanded) .wall-hud-context {\n      display: none;"
        ));
    }

    #[test]
    fn desktop_shell_pane_wall_media_keeps_compact_hud_as_one_rail() {
        assert!(INDEX_HTML
            .contains("body.pane-wall .wall-hud {\n      position: fixed;\n      left: 8px;"));
        assert!(INDEX_HTML.contains(
            "body.pane-wall.wall-hud-expanded .wall-hud {\n      grid-template-columns: minmax(230px, 0.9fr)"
        ));
        assert!(INDEX_HTML.contains(
            "body.pane-wall.wall-hud-expanded .wall-hud {\n\t        grid-template-columns: minmax(210px, 0.8fr)"
        ));
        assert!(!INDEX_HTML.contains(
            "body.pane-wall .wall-hud {\n\t        grid-template-columns: minmax(210px, 0.8fr)"
        ));
    }

    #[test]
    fn desktop_shell_pane_wall_compact_hud_is_single_row_context_rail() {
        assert!(INDEX_HTML.contains(
            "body.pane-wall:not(.wall-hud-expanded) .wall-hud-main {\n      display: flex;"
        ));
        assert!(INDEX_HTML.contains(
            "body.pane-wall:not(.wall-hud-expanded) .wall-hud-kicker,\n    body.pane-wall:not(.wall-hud-expanded) .wall-hud-meta {\n      display: none;"
        ));
        assert!(INDEX_HTML.contains(
            "body.pane-wall:not(.wall-hud-expanded) .wall-hud-context {\n      flex-wrap: nowrap;"
        ));
        assert!(INDEX_HTML.contains(
            "body.pane-wall:not(.wall-hud-expanded) .wall-context-pill {\n      max-width: 150px;"
        ));
    }

    #[test]
    fn desktop_shell_pane_wall_hud_uses_actionable_labels() {
        assert!(INDEX_HTML.contains("id=\"wallHudRead\">Read output</button>"));
        assert!(INDEX_HTML.contains("id=\"wallHudSweep\">Sweep mission</button>"));
        assert!(INDEX_HTML.contains("id=\"wallHudNewChild\">New child</button>"));
        assert!(INDEX_HTML.contains("id=\"wallHudPacketsAll\">Request packets</button>"));
        assert!(INDEX_HTML.contains("id=\"wallHudPacketAction\">Request packet</button>"));
        assert!(INDEX_HTML.contains("id=\"wallHudMore\" aria-pressed=\"false\">Intervene</button>"));
        assert!(INDEX_HTML
            .contains("button.textContent = enabled ? 'Hide intervention' : 'Intervene';"));
        assert!(INDEX_HTML.contains(
            "menuButton.textContent = enabled ? 'Hide intervention' : 'Intervention rail';"
        ));
        assert!(INDEX_HTML.contains("id=\"wallHudFull\">Expand pane</button>"));
        assert!(INDEX_HTML.contains("id=\"wallHudExit\">Workbench</button>"));
        assert!(!INDEX_HTML.contains("id=\"wallHudNewChild\">New</button>"));
        assert!(!INDEX_HTML.contains("id=\"wallHudPacketsAll\">Packets all</button>"));
        assert!(!INDEX_HTML.contains("id=\"wallHudPacketAction\">Packet</button>"));
        assert!(!INDEX_HTML.contains("id=\"wallHudMore\" aria-pressed=\"false\">More</button>"));
        assert!(!INDEX_HTML.contains("id=\"wallHudMore\" aria-pressed=\"false\">Details</button>"));
        assert!(
            !INDEX_HTML.contains("id=\"wallHudMore\" aria-pressed=\"false\">Command rail</button>")
        );
        assert!(!INDEX_HTML.contains("id=\"wallHudExit\">Focus mode</button>"));
        assert!(!INDEX_HTML.contains("id=\"wallHudExit\">Exit wall</button>"));
    }

    #[test]
    fn desktop_shell_command_wall_compact_bar_is_watch_first() {
        assert!(INDEX_HTML.contains("id=\"wallHudRead\">Read output</button>"));
        assert!(INDEX_HTML.contains("id=\"wallHudMore\" aria-pressed=\"false\">Intervene</button>"));
        assert!(INDEX_HTML.contains("id=\"wallHudFull\">Expand pane</button>"));
        assert!(INDEX_HTML.contains("id=\"wallHudExit\">Workbench</button>"));
        assert!(INDEX_HTML.contains("body.pane-wall:not(.wall-hud-expanded) #wallHudMessage"));
        assert!(INDEX_HTML.contains("body.pane-wall:not(.wall-hud-expanded) .wall-hud-command"));
        assert!(INDEX_HTML.contains("Intervention rail ready."));
        assert!(!INDEX_HTML.contains("Parent command lane ready."));
        assert!(!INDEX_HTML.contains("Command rail ready."));
    }

    #[test]
    fn desktop_shell_all_panes_separates_watching_from_intervention() {
        assert!(INDEX_HTML.contains("<span class=\"wall-hud-kicker\">Watching pane</span>"));
        assert!(INDEX_HTML.contains("Choose a live pane to watch or intervene."));
        assert!(INDEX_HTML
            .contains("placeholder=\"Intervention instruction to watched pane or scope\""));
        assert!(INDEX_HTML.contains("title=\"Open the all-pane intervention rail\""));
        assert!(INDEX_HTML
            .contains("title = enabled ? 'Hide intervention rail' : 'Open intervention rail';"));
        assert!(INDEX_HTML.contains(
            "title = enabled ? 'Hide intervention rail' : 'Open the all-pane intervention rail';"
        ));
        assert!(!INDEX_HTML.contains("Open the all-pane command rail"));
        assert!(!INDEX_HTML.contains("Parent instruction to selected pane or scope"));
    }

    #[test]
    fn desktop_shell_command_wall_hud_uses_watching_language() {
        assert!(INDEX_HTML.contains("<span class=\"wall-hud-kicker\">Watching pane</span>"));
        assert!(INDEX_HTML.contains(
            "<strong class=\"wall-hud-title\" id=\"wallHudTitle\">No pane watched</strong>"
        ));
        assert!(INDEX_HTML.contains("Choose a live pane to watch or intervene."));
        assert!(INDEX_HTML.contains("title.textContent = `Watching ${wave.title}`;"));
        assert!(INDEX_HTML.contains("parent lane watching ${childWaves().length} child pane"));
        assert!(INDEX_HTML.contains("intervention lane - ${wave.mode} - ${wave.status}"));
        assert!(!INDEX_HTML.contains("<span class=\"wall-hud-kicker\">Selected pane</span>"));
        assert!(!INDEX_HTML.contains("title.textContent = `${wave.title} (${wave.role})`;"));
    }

    #[test]
    fn desktop_shell_pane_wall_tile_actions_are_contextual() {
        assert!(INDEX_HTML.contains("pointer-events: none;"));
        assert!(!INDEX_HTML.contains("body.pane-wall .wave-card.active .tile-controls"));
        assert!(INDEX_HTML.contains("body.pane-wall .wave-card:hover .tile-controls"));
        assert!(INDEX_HTML.contains("body.pane-wall .wave-card:focus-visible .tile-controls"));
        assert!(!INDEX_HTML.contains("body.pane-wall .wave-card:focus-within .tile-controls"));
        assert!(!INDEX_HTML.contains("body.pane-wall .tile-controls {\n      opacity: 1;"));
    }

    #[test]
    fn desktop_shell_pane_wall_keeps_tile_signals_scannable() {
        assert!(INDEX_HTML.contains("class=\"pane-chip context-detail\""));
        assert!(
            INDEX_HTML.contains("class=\"pane-chip ${escapeHtml(dispatchClass)} context-detail\"")
        );
        assert!(INDEX_HTML.contains("body.pane-wall .pane-chip.context-detail"));
        assert!(INDEX_HTML.contains("display: none;"));
    }

    #[test]
    fn query_u16_reads_simple_values() {
        assert_eq!(query_u16(Some("cols=90&rows=31"), "cols"), Some(90));
        assert_eq!(query_u16(Some("cols=90&rows=31"), "rows"), Some(31));
        assert_eq!(query_u16(Some("cols=nope"), "cols"), None);
    }

    #[test]
    fn query_bool_reads_truthy_values() {
        assert!(query_bool(Some("takeover=1"), "takeover"));
        assert!(query_bool(Some("takeover=true"), "takeover"));
        assert!(query_bool(Some("takeover=yes"), "takeover"));
        assert!(!query_bool(Some("takeover=0"), "takeover"));
        assert!(!query_bool(Some("other=1"), "takeover"));
    }

    #[test]
    fn query_value_reads_raw_value() {
        assert_eq!(
            query_value(Some("data=%1B%5BA&rows=31"), "data"),
            Some("%1B%5BA")
        );
        assert_eq!(query_value(Some("data=%1B%5BA&rows=31"), "missing"), None);
    }

    #[test]
    fn percent_decode_decodes_terminal_bytes() {
        assert_eq!(percent_decode("%1B%5BA").unwrap(), b"\x1b[A");
        assert_eq!(percent_decode("hello%20there").unwrap(), b"hello there");
        assert!(percent_decode("%no").is_none());
    }

    #[test]
    fn contract_query_parsers_accept_ui_values() {
        assert_eq!(parse_wave_mode("read_only"), Some(WaveMode::ReadOnly));
        assert_eq!(parse_wave_mode("draft-only"), Some(WaveMode::DraftOnly));
        assert_eq!(parse_wave_mode("verifier"), Some(WaveMode::Verifier));
        assert_eq!(parse_wave_mode("???"), None);

        assert_eq!(parse_wave_status("running"), Some(WaveStatus::Running));
        assert_eq!(
            parse_wave_status("needs-review"),
            Some(WaveStatus::NeedsReview)
        );
        assert_eq!(parse_wave_status("accepted"), Some(WaveStatus::Accepted));
        assert_eq!(parse_wave_status("done"), Some(WaveStatus::Done));
        assert_eq!(parse_wave_status("???"), None);
    }

    #[test]
    fn merge_report_gates_preserves_manual_items_and_adds_detected_fields() {
        let existing = WaveReportGate {
            completed_fields: 1,
            required_fields: 10,
            completed_items: vec!["Commands run".into()],
        };
        let detected = WaveReportGate {
            completed_fields: 2,
            required_fields: 10,
            completed_items: vec!["What I did".into(), "Evidence / receipts".into()],
        };

        let merged = merge_report_gates(&existing, &detected);

        assert_eq!(merged.label(), "3/10");
        assert_eq!(
            merged.completed_items,
            vec![
                "What I did".to_string(),
                "Evidence / receipts".to_string(),
                "Commands run".to_string()
            ]
        );
    }

    #[test]
    fn merge_report_gates_preserves_custom_required_field_count() {
        let existing = WaveReportGate {
            completed_fields: 2,
            required_fields: 2,
            completed_items: vec!["Deliverables".into(), "Self-TM".into()],
        };
        let detected = WaveReportGate::default();

        let merged = merge_report_gates(&existing, &detected);

        assert_eq!(merged.label(), "2/2");
        assert_eq!(
            merged.completed_items,
            vec!["Deliverables".to_string(), "Self-TM".to_string()]
        );
    }

    #[test]
    fn dispatch_pane_ids_dedupes_and_rejects_empty_lists() {
        let ids = parse_dispatch_pane_ids(r#"[" p1 ","","p2","p1"]"#).unwrap();
        assert_eq!(ids, vec!["p1".to_string(), "p2".to_string()]);

        assert!(parse_dispatch_pane_ids("[]").is_err());
        assert!(parse_dispatch_pane_ids(r#"[" ",""]"#).is_err());
        assert!(parse_dispatch_pane_ids("not-json").is_err());
    }

    #[test]
    fn dispatch_summary_counts_success_and_failure_receipts() {
        let summary = PaneDispatchSummary::from_receipts(
            "all children".into(),
            vec![
                PaneDispatchReceipt::ok("p1".into()),
                PaneDispatchReceipt::failed("p2".into(), "pane not found".into()),
            ],
        );

        assert_eq!(summary.requested, 2);
        assert_eq!(summary.sent, 1);
        assert_eq!(summary.failed, 1);
    }

    #[test]
    fn dispatch_ledger_keeps_recent_events_and_finds_selected_pane_receipt() {
        let mut ledger = PaneDispatchLedger::new(2);
        let first = PaneDispatchSummary::from_receipts(
            "first child".into(),
            vec![PaneDispatchReceipt::ok("p1".into())],
        );
        let second = PaneDispatchSummary::from_receipts(
            "second child".into(),
            vec![PaneDispatchReceipt::ok("p2".into())],
        );
        let third = PaneDispatchSummary::from_receipts(
            "third child".into(),
            vec![PaneDispatchReceipt::failed("p3".into(), "gone".into())],
        );

        ledger.record("10:00:01".into(), first);
        ledger.record("10:00:02".into(), second);
        ledger.record("10:00:03".into(), third);

        let events = ledger.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].summary.target, "third child");
        assert_eq!(events[1].summary.target, "second child");

        let selected = ledger
            .latest_for_pane("p3")
            .expect("latest receipt for selected pane");
        assert_eq!(selected.at, "10:00:03");
        assert_eq!(selected.summary.failed, 1);
        assert_eq!(selected.receipt.error.as_deref(), Some("gone"));
        assert!(ledger.latest_for_pane("p1").is_none());

        let encoded = serde_json::to_value(&events[0]).expect("dispatch event json");
        assert_eq!(encoded["at"], "10:00:03");
        assert_eq!(encoded["target"], "third child");
        assert_eq!(encoded["failed"], 1);
        assert!(encoded.get("summary").is_none());
    }

    fn desktop_args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn desktop_command_defaults_to_native_app_on_macos() {
        let DesktopCommandPlan::Run(config) = parse_desktop_command(&[]) else {
            panic!("expected run plan");
        };

        assert_eq!(config.launch_mode, DesktopLaunchMode::NativeApp);
        assert_eq!(config.bind_addr, DEFAULT_BIND_ADDR);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn desktop_command_defaults_to_web_preview_off_macos() {
        let DesktopCommandPlan::Run(config) = parse_desktop_command(&[]) else {
            panic!("expected run plan");
        };

        assert_eq!(config.launch_mode, DesktopLaunchMode::WebPreview);
        assert_eq!(config.bind_addr, DEFAULT_BIND_ADDR);
    }

    #[test]
    fn desktop_command_web_flag_forces_preview_server() {
        let DesktopCommandPlan::Run(config) =
            parse_desktop_command(&desktop_args(&["--web", "--port", "62662"]))
        else {
            panic!("expected run plan");
        };

        assert_eq!(config.launch_mode, DesktopLaunchMode::WebPreview);
        assert_eq!(config.bind_addr, "127.0.0.1:62662");
    }

    #[test]
    fn desktop_command_app_flag_forces_native_app() {
        let DesktopCommandPlan::Run(config) =
            parse_desktop_command(&desktop_args(&["--app", "--bind", "127.0.0.1:62662"]))
        else {
            panic!("expected run plan");
        };

        assert_eq!(config.launch_mode, DesktopLaunchMode::NativeApp);
        assert_eq!(config.bind_addr, "127.0.0.1:62662");
    }

    #[test]
    fn desktop_command_parser_reports_usage_errors() {
        assert!(matches!(
            parse_desktop_command(&desktop_args(&["--port", "nope"])),
            DesktopCommandPlan::UsageError(_)
        ));
        assert!(matches!(
            parse_desktop_command(&desktop_args(&["--wat"])),
            DesktopCommandPlan::UsageError(_)
        ));
        assert_eq!(
            parse_desktop_command(&desktop_args(&["help"])),
            DesktopCommandPlan::Help
        );
    }

    #[test]
    fn desktop_runtime_status_accepts_matching_protocol() {
        let status = crate::api::RuntimeStatus {
            version: Some(env!("CARGO_PKG_VERSION").into()),
            protocol: Some(PROTOCOL_VERSION),
        };

        assert!(validate_desktop_runtime_status(Some(status)).is_ok());
    }

    #[test]
    fn desktop_runtime_status_rejects_missing_status() {
        let err = validate_desktop_runtime_status(None).unwrap_err();

        assert!(err.to_string().contains("status API is unavailable"));
    }

    #[test]
    fn desktop_runtime_status_rejects_protocol_mismatch() {
        let status = crate::api::RuntimeStatus {
            version: Some("0.0.1".into()),
            protocol: Some(PROTOCOL_VERSION.saturating_sub(1)),
        };

        let err = validate_desktop_runtime_status(Some(status)).unwrap_err();

        assert!(err.to_string().contains("protocol"));
        assert!(err.to_string().contains("herdr server stop"));
    }

    #[test]
    fn preferred_child_agent_argv_prefers_agent_cli_over_shell() {
        let candidates = vec![
            DesktopAgentCandidate {
                command: "codex",
                available: true,
            },
            DesktopAgentCandidate {
                command: "claude",
                available: true,
            },
            DesktopAgentCandidate {
                command: "pi",
                available: true,
            },
        ];

        assert_eq!(preferred_child_agent_argv(&candidates), vec!["claude"]);
    }

    #[test]
    fn preferred_child_agent_argv_falls_back_to_shell_when_no_agent_cli_exists() {
        let candidates = vec![
            DesktopAgentCandidate {
                command: "claude",
                available: false,
            },
            DesktopAgentCandidate {
                command: "codex",
                available: false,
            },
        ];

        assert_eq!(
            preferred_child_agent_argv(&candidates),
            vec!["/bin/zsh", "-l"]
        );
    }

    #[test]
    fn desktop_command_lookup_checks_user_local_agent_dirs_beyond_path() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-desktop-agent-path-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("create temp agent dir");
        let executable = dir.join("claude");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("write fake agent");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&executable)
                .expect("fake agent metadata")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&executable, permissions).expect("chmod fake agent");
        }

        assert!(desktop_command_available_from_paths(
            "claude",
            Some(OsStr::new("")),
            std::slice::from_ref(&dir)
        ));
        assert!(!desktop_command_available_from_paths(
            "not-claude",
            Some(OsStr::new("")),
            std::slice::from_ref(&dir)
        ));

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn desktop_agent_argv_resolves_commands_from_extra_agent_dirs() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-desktop-agent-argv-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("create temp agent dir");
        let executable = dir.join("claude");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("write fake agent");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&executable)
                .expect("fake agent metadata")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&executable, permissions).expect("chmod fake agent");
        }

        let resolved = resolve_desktop_agent_argv_from_paths(
            &["claude".into(), "--print".into()],
            Some(OsStr::new("")),
            std::slice::from_ref(&dir),
        );

        assert_eq!(
            resolved,
            vec![
                executable.to_string_lossy().into_owned(),
                "--print".to_string()
            ]
        );

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn desktop_integration_labels_use_desktop_command_availability() {
        assert_eq!(
            desktop_integration_status_label(
                crate::integration::IntegrationStatusKind::NotInstalled,
                true
            ),
            "available"
        );
        assert!(desktop_integration_needs_install(
            crate::integration::IntegrationStatusKind::NotInstalled,
            true
        ));
        assert_eq!(
            desktop_integration_status_label(
                crate::integration::IntegrationStatusKind::NotInstalled,
                false
            ),
            "not found"
        );
        assert!(!desktop_integration_needs_install(
            crate::integration::IntegrationStatusKind::NotInstalled,
            false
        ));
    }

    #[test]
    fn pane_output_snapshot_keeps_recent_nonempty_lines() {
        let snapshot = PaneOutputSnapshot::from_text(
            "p1".into(),
            "one\n\n  two  \nthree\nfour\nfive\n".into(),
            3,
        );

        assert_eq!(snapshot.pane_id, "p1");
        assert_eq!(snapshot.line_count, 6);
        assert_eq!(snapshot.nonempty_line_count, 5);
        assert_eq!(snapshot.tail_lines, vec!["three", "four", "five"]);
        assert_eq!(snapshot.last_nonempty_line.as_deref(), Some("five"));
    }

    #[test]
    fn report_ingest_snapshot_has_no_missing_items_when_packet_is_complete() {
        let detected = WaveReportGate::default();
        let report = WaveReportGate {
            completed_fields: 2,
            required_fields: 2,
            completed_items: vec!["Deliverables".into(), "Self-TM".into()],
        }
        .normalized();

        let snapshot = ReportIngestSnapshot::from_gates(&detected, &report);

        assert!(snapshot.ready());
        assert!(snapshot.missing_items.is_empty());
    }

    #[test]
    fn mission_sweep_summary_counts_ready_missing_and_failed_children() {
        let ready_report = ReportIngestSnapshot {
            detected_items: Vec::new(),
            completed_items: vec!["What I did".into(), "What I found".into()],
            missing_items: Vec::new(),
            completed_fields: 2,
            required_fields: 2,
        };
        let missing_report = ReportIngestSnapshot {
            detected_items: Vec::new(),
            completed_items: vec!["What I did".into()],
            missing_items: vec!["What I found".into()],
            completed_fields: 1,
            required_fields: 2,
        };
        let panes = vec![
            test_sweep_pane("p1", Some(ready_report), None),
            test_sweep_pane("p2", Some(missing_report), None),
            test_sweep_pane("p3", None, Some("read failed")),
        ];

        let summary = MissionSweepSummary::from_panes(&panes);

        assert_eq!(summary.children, 3);
        assert_eq!(summary.read, 2);
        assert_eq!(summary.ingested, 2);
        assert_eq!(summary.ready_packets, 1);
        assert_eq!(summary.needs_attention, 2);
        assert_eq!(summary.failed, 1);
    }

    #[test]
    fn mission_attention_items_name_missing_packets_and_errors() {
        let missing_report = ReportIngestSnapshot {
            detected_items: Vec::new(),
            completed_items: vec!["What I did".into()],
            missing_items: vec!["What I found".into(), "Evidence / receipts".into()],
            completed_fields: 1,
            required_fields: 3,
        };
        let panes = vec![
            test_sweep_pane("p1", Some(missing_report), None),
            test_sweep_pane("p2", None, Some("pane read failed")),
        ];

        let attention = mission_attention_items(&panes);

        assert_eq!(attention.len(), 2);
        assert_eq!(attention[0].pane_id, "p1");
        assert_eq!(attention[0].kind, MissionAttentionKind::MissingPacket);
        assert_eq!(
            attention[0].missing_items,
            vec!["What I found", "Evidence / receipts"]
        );
        assert!(attention[0].message.contains("2 packet fields missing"));
        assert_eq!(attention[1].pane_id, "p2");
        assert_eq!(attention[1].kind, MissionAttentionKind::Error);
        assert!(attention[1].message.contains("pane read failed"));
    }

    #[test]
    fn dependency_gates_wait_for_upstream_packet_acceptance() {
        let ready_report = ReportIngestSnapshot {
            detected_items: Vec::new(),
            completed_items: vec!["What I did".into(), "What I found".into()],
            missing_items: Vec::new(),
            completed_fields: 2,
            required_fields: 2,
        };
        let upstream_done = test_sweep_pane_with_contract(
            "p1",
            "Wave 1: proof docs",
            Some(ready_report.clone()),
            Some(WaveStatus::Done),
            None,
            &["A", "D"],
        );
        let downstream = test_sweep_pane_with_contract(
            "p2",
            "Wave 2: stale-binary research",
            None,
            Some(WaveStatus::Queued),
            Some("after A+D"),
            &["B"],
        );

        let gates = mission_dependency_gates(&[upstream_done, downstream.clone()]);

        assert_eq!(
            gates[1].status,
            MissionDependencyGateStatus::NeedsAcceptance
        );
        assert_eq!(gates[1].upstream_pane_ids, vec!["p1"]);

        let upstream_accepted = test_sweep_pane_with_contract(
            "p1",
            "Wave 1: proof docs",
            Some(ready_report),
            Some(WaveStatus::Accepted),
            None,
            &["A", "D"],
        );

        let gates = mission_dependency_gates(&[upstream_accepted, downstream]);

        assert_eq!(gates[1].status, MissionDependencyGateStatus::Ready);
    }

    #[test]
    fn dependency_gates_match_upstream_when_layout_order_is_reversed() {
        let upstream = test_sweep_pane_with_contract(
            "p1",
            "Wave 1: proof docs",
            None,
            Some(WaveStatus::Queued),
            None,
            &["A", "D"],
        );
        let downstream = test_sweep_pane_with_contract(
            "p2",
            "Wave 2: stale-binary research",
            None,
            Some(WaveStatus::Queued),
            Some("after A+D"),
            &["B"],
        );

        let gates = mission_dependency_gates(&[downstream, upstream]);

        assert_eq!(gates[0].status, MissionDependencyGateStatus::WaitingPacket);
        assert_eq!(gates[0].upstream_pane_ids, vec!["p1"]);
    }

    #[test]
    fn dependency_gates_prefer_nearest_numbered_upstream_wave() {
        let stale_upstream = test_sweep_pane_with_contract(
            "old",
            "Wave 1: older proof docs",
            None,
            Some(WaveStatus::Queued),
            None,
            &["A", "D"],
        );
        let nearest_upstream = test_sweep_pane_with_contract(
            "new",
            "Wave 96: current proof docs",
            None,
            Some(WaveStatus::Queued),
            None,
            &["A", "D"],
        );
        let downstream = test_sweep_pane_with_contract(
            "downstream",
            "Wave 97: stale-binary research",
            None,
            Some(WaveStatus::Queued),
            Some("after A+D"),
            &["B"],
        );

        let gates = mission_dependency_gates(&[downstream, stale_upstream, nearest_upstream]);

        assert_eq!(gates[0].upstream_pane_ids, vec!["new"]);
    }

    #[test]
    fn mission_unlock_candidates_include_queued_ready_dependent_waves_only() {
        let accepted_upstream = test_sweep_pane_with_contract(
            "upstream",
            "Wave 1: proof docs",
            None,
            Some(WaveStatus::Accepted),
            Some("parallel"),
            &["A", "D"],
        );
        let queued_downstream = test_sweep_pane_with_contract(
            "downstream",
            "Wave 2: stale-binary research",
            None,
            Some(WaveStatus::Queued),
            Some("after A+D"),
            &["B"],
        );
        let running_downstream = test_sweep_pane_with_contract(
            "running",
            "Wave 3: verifier",
            None,
            Some(WaveStatus::Running),
            Some("after A+D"),
            &["V"],
        );
        let parallel_queued = test_sweep_pane_with_contract(
            "parallel",
            "Wave 4: docs",
            None,
            Some(WaveStatus::Queued),
            Some("parallel"),
            &["P"],
        );

        let candidates = mission_unlock_candidates(&[
            accepted_upstream,
            queued_downstream,
            running_downstream,
            parallel_queued,
        ]);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].pane_id, "downstream");
        assert_eq!(candidates[0].dependency, "after A+D");
        assert_eq!(candidates[0].upstream_pane_ids, vec!["upstream"]);
    }

    #[test]
    fn mission_unlock_candidates_wait_for_parent_acceptance() {
        let ready_report = ReportIngestSnapshot {
            detected_items: Vec::new(),
            completed_items: default_report_packet_items()
                .iter()
                .map(|item| (*item).to_string())
                .collect(),
            missing_items: Vec::new(),
            completed_fields: 10,
            required_fields: 10,
        };
        let upstream_done = test_sweep_pane_with_contract(
            "upstream",
            "Wave 1: proof docs",
            Some(ready_report),
            Some(WaveStatus::Done),
            Some("parallel"),
            &["A", "D"],
        );
        let downstream = test_sweep_pane_with_contract(
            "downstream",
            "Wave 2: stale-binary research",
            None,
            Some(WaveStatus::Queued),
            Some("after A+D"),
            &["B"],
        );

        assert!(mission_unlock_candidates(&[upstream_done, downstream]).is_empty());
    }

    #[test]
    fn mission_import_plan_maps_contracts_to_child_panes_in_order() {
        let contracts = vec![
            test_contract("W1"),
            test_contract("W2"),
            test_contract("W3"),
        ];
        let plan = mission_import_plan(&contracts, &["pane-1".into(), "pane-2".into()]);

        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].pane_id.as_deref(), Some("pane-1"));
        assert_eq!(plan[0].status, MissionImportAssignmentStatus::Ready);
        assert_eq!(plan[1].pane_id.as_deref(), Some("pane-2"));
        assert_eq!(plan[2].pane_id, None);
        assert_eq!(plan[2].status, MissionImportAssignmentStatus::NoPane);
    }

    #[test]
    fn mission_import_plan_prefers_existing_contract_title_matches() {
        let contracts = vec![
            test_contract("W1"),
            test_contract("W2"),
            test_contract("W3"),
        ];
        let targets = vec![
            test_pane_target("pane-3", Some("W3")),
            test_pane_target("pane-1", Some("W1")),
            test_pane_target("pane-2", Some("W2")),
        ];

        let plan = mission_import_plan_for_targets(&contracts, &targets);

        assert_eq!(plan[0].pane_id.as_deref(), Some("pane-1"));
        assert_eq!(plan[1].pane_id.as_deref(), Some("pane-2"));
        assert_eq!(plan[2].pane_id.as_deref(), Some("pane-3"));
    }

    #[test]
    fn mission_import_targets_carry_terminal_ids_for_reuse_preview() {
        let response = serde_json::json!({
            "result": {
                "panes": [
                    {"pane_id": "parent", "terminal_id": "term-parent", "is_root_pane": true},
                    {"pane_id": "child", "terminal_id": "term-child", "is_root_pane": false, "wave_contract": {"title": "W1"}}
                ]
            }
        });

        let targets = child_pane_targets_from_panes_response(&response);
        let plan = mission_import_plan_for_targets(&[test_contract("W1")], &targets);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].terminal_id.as_deref(), Some("term-child"));
        assert_eq!(plan[0].terminal_id.as_deref(), Some("term-child"));
    }

    #[test]
    fn mission_import_child_identity_reads_pane_and_terminal_from_agent_start() {
        let response = serde_json::json!({
            "result": {
                "agent": {
                    "pane_id": "pane-created",
                    "terminal_id": "term-created"
                }
            }
        });

        let identity = mission_import_child_identity_from_start_response(&response).unwrap();

        assert_eq!(identity.pane_id, "pane-created");
        assert_eq!(identity.terminal_id.as_deref(), Some("term-created"));
    }

    #[test]
    fn mission_import_launch_mode_creates_new_panes_for_unmatched_contracts() {
        let contracts = vec![
            test_contract("Wave 1: proof docs"),
            test_contract("Wave 2: stale-binary research"),
        ];
        let targets = vec![
            test_pane_target("pane-old-1", Some("Old investigation")),
            test_pane_target("pane-old-2", Some("Unrelated verifier")),
        ];

        let plan = mission_import_plan_for_targets_with_reuse(
            &contracts,
            &targets,
            MissionImportReuseMode::TitleOnly,
        );

        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].pane_id, None);
        assert_eq!(plan[0].status, MissionImportAssignmentStatus::NoPane);
        assert_eq!(plan[1].pane_id, None);
        assert_eq!(plan[1].status, MissionImportAssignmentStatus::NoPane);
    }

    #[test]
    fn mission_import_plan_marks_real_pane_actions_for_preview() {
        let mut assignments = vec![
            test_assignment(
                "W1",
                Some("pane-existing"),
                MissionImportAssignmentStatus::Ready,
            ),
            test_assignment("W2", None, MissionImportAssignmentStatus::NoPane),
        ];

        mission_import_mark_planned_actions(&mut assignments, true);

        assert_eq!(
            assignments[0].planned_action,
            MissionImportPlannedAction::ReuseExisting
        );
        assert_eq!(
            assignments[1].planned_action,
            MissionImportPlannedAction::CreateNew
        );

        mission_import_mark_planned_actions(&mut assignments, false);

        assert_eq!(
            assignments[1].planned_action,
            MissionImportPlannedAction::NeedsPane
        );
    }

    #[test]
    fn mission_import_preview_counts_create_reuse_and_prompt_plan() {
        let assignments = vec![
            test_assignment(
                "W1",
                Some("pane-existing"),
                MissionImportAssignmentStatus::Ready,
            ),
            test_assignment("W2", None, MissionImportAssignmentStatus::NoPane),
            test_assignment("W3", None, MissionImportAssignmentStatus::NoPane),
        ];

        assert_eq!(mission_import_planned_reuse_count(&assignments), 1);
        assert_eq!(mission_import_planned_create_count(&assignments, true), 2);
        assert_eq!(
            mission_import_planned_prompt_count(
                &assignments,
                true,
                MissionImportPromptScope::Created
            ),
            2
        );
        assert_eq!(
            mission_import_planned_prompt_count(&assignments, true, MissionImportPromptScope::All),
            3
        );
        assert_eq!(mission_import_planned_create_count(&assignments, false), 0);
    }

    #[test]
    fn mission_import_prompt_plan_holds_dependent_created_waves_until_upstream_accepted() {
        let mut upstream = test_contract("Wave 1: proof docs");
        upstream.arcs = vec![
            WaveArc {
                id: "A".into(),
                summary: "proof pack".into(),
                status: Some(WaveStatus::Queued),
            },
            WaveArc {
                id: "D".into(),
                summary: "demo script".into(),
                status: Some(WaveStatus::Queued),
            },
        ];
        let mut downstream = test_contract("Wave 2: stale-binary research");
        downstream.dependency = Some("after A+D".into());
        let contracts = vec![upstream, downstream];
        let mut assignments = mission_import_plan_for_targets_with_reuse(
            &contracts,
            &[],
            MissionImportReuseMode::TitleOnly,
        );

        mission_import_mark_planned_actions(&mut assignments, true);
        mission_import_apply_dependency_gates(&mut assignments, &contracts, true);

        assert_eq!(
            assignments[0].gate_status,
            MissionDependencyGateStatus::Ready
        );
        assert_eq!(
            assignments[1].gate_status,
            MissionDependencyGateStatus::WaitingPacket
        );
        assert_eq!(
            assignments[1].gate_reason.as_deref(),
            Some("waiting for upstream report packet")
        );
        assert_eq!(
            mission_import_planned_prompt_count(
                &assignments,
                true,
                MissionImportPromptScope::Created
            ),
            1
        );
    }

    #[test]
    fn mission_import_contract_status_tracks_running_prompted_and_held_waves() {
        assert_eq!(
            mission_import_contract_status(None, true, MissionDependencyGateStatus::Ready),
            WaveStatus::Running
        );
        assert_eq!(
            mission_import_contract_status(None, false, MissionDependencyGateStatus::WaitingPacket),
            WaveStatus::Queued
        );
        assert_eq!(
            mission_import_contract_status(
                Some(WaveStatus::Accepted),
                false,
                MissionDependencyGateStatus::Ready
            ),
            WaveStatus::Accepted
        );
    }

    #[test]
    fn mission_import_counts_created_child_panes_as_applied_contracts() {
        let assignments = vec![
            test_assignment("W1", Some("pane-1"), MissionImportAssignmentStatus::Applied),
            test_assignment("W2", Some("pane-2"), MissionImportAssignmentStatus::Created),
            test_assignment("W3", None, MissionImportAssignmentStatus::NoPane),
            test_assignment("W4", None, MissionImportAssignmentStatus::Failed),
        ];

        assert_eq!(mission_import_applied_count(&assignments), 2);
        assert_eq!(mission_import_created_count(&assignments), 1);
        assert_eq!(mission_import_missing_panes_count(&assignments), 1);
    }

    #[test]
    fn mission_import_prompt_scope_parser_defaults_to_none() {
        assert_eq!(
            parse_mission_import_prompt_scope(None),
            MissionImportPromptScope::None
        );
        assert_eq!(
            parse_mission_import_prompt_scope(Some("created")),
            MissionImportPromptScope::Created
        );
        assert_eq!(
            parse_mission_import_prompt_scope(Some("all")),
            MissionImportPromptScope::All
        );
        assert_eq!(
            parse_mission_import_prompt_scope(Some("true")),
            MissionImportPromptScope::Created
        );
        assert_eq!(
            parse_mission_import_prompt_scope(Some("bogus")),
            MissionImportPromptScope::None
        );
    }

    #[test]
    fn mission_import_contract_prompt_gives_child_enough_to_start_work() {
        let mut contract = test_contract("W4-docs-verifier");
        contract.mode = WaveMode::Verifier;
        contract.dependency = Some("after W3 packet accepted".into());
        contract.arcs = vec![WaveArc {
            id: "V".into(),
            summary: "verify report receipts".into(),
            status: Some(WaveStatus::Queued),
        }];

        let prompt = mission_import_contract_prompt(
            "/tmp/session.md",
            "pane-4",
            &contract,
            &["Deliverables".into(), "Self-TM".into()],
        );

        assert!(prompt.contains("Mission contract: W4-docs-verifier"));
        assert!(prompt.contains("Pane id: pane-4"));
        assert!(prompt.contains("Mode: verifier"));
        assert!(prompt.contains("Dependency: after W3 packet accepted"));
        assert!(prompt.contains("- V: verify report receipts"));
        assert!(prompt.contains("1. Deliverables"));
        assert!(prompt.contains("2. Self-TM"));
        assert!(prompt.ends_with('\n'));
    }

    #[test]
    fn mission_import_prompt_delivery_detects_plain_shell_launches() {
        assert_eq!(
            mission_import_prompt_delivery(&["/bin/zsh".into(), "-l".into()]),
            WavePromptDelivery::ShellCard
        );
        assert_eq!(
            mission_import_prompt_delivery(&["bash".into()]),
            WavePromptDelivery::ShellCard
        );
        assert_eq!(
            mission_import_prompt_delivery(&["claude".into()]),
            WavePromptDelivery::Agent
        );
    }

    #[test]
    fn mission_import_contract_payload_uses_shell_card_for_shell_launches() {
        let mut contract = test_contract("Wave 9: shell fallback");
        contract.prompt_delivery = Some(WavePromptDelivery::ShellCard);
        let payload = mission_import_contract_payload(
            "/tmp/session.md",
            "pane-shell",
            &contract,
            &["What I did".into(), "Evidence / receipts".into()],
        );

        assert!(payload.starts_with("printf '%s\\n' "));
        assert!(payload.contains("'Mission contract: Wave 9: shell fallback'"));
        assert!(payload.contains("'1. What I did'"));
        assert!(!payload.contains("\n1. What I did"));
        assert!(payload.ends_with('\n'));
    }

    #[test]
    fn mission_import_contract_payload_keeps_raw_prompt_for_agent_launches() {
        let mut contract = test_contract("Wave 10: agent");
        contract.prompt_delivery = Some(WavePromptDelivery::Agent);
        let payload =
            mission_import_contract_payload("/tmp/session.md", "pane-agent", &contract, &[]);

        assert!(payload.starts_with("Mission import source: /tmp/session.md"));
        assert!(payload.contains("Mission contract: Wave 10: agent"));
        assert!(!payload.starts_with("printf"));
    }

    #[test]
    fn pane_input_payload_uses_shell_card_delivery_when_requested() {
        let payload = pane_input_payload(
            "Missing fields:\n1. Evidence / receipts\n2. Commands run\n",
            Some(WavePromptDelivery::ShellCard),
        );

        assert!(payload.starts_with("printf '%s\\n' "));
        assert!(payload.contains("'1. Evidence / receipts'"));
        assert!(!payload.contains("\n1. Evidence / receipts"));
        assert!(payload.ends_with('\n'));
    }

    #[test]
    fn pane_input_payload_keeps_agent_delivery_raw() {
        let payload = pane_input_payload(
            "Missing fields:\n1. Evidence / receipts\n",
            Some(WavePromptDelivery::Agent),
        );

        assert_eq!(payload, "Missing fields:\n1. Evidence / receipts\n");
    }

    #[test]
    fn pane_input_payload_normalizes_shell_dispatch_carriage_returns() {
        let payload = pane_input_payload(
            "Parent message:\rstatus ping from parent\r",
            Some(WavePromptDelivery::ShellCard),
        );

        assert!(payload.starts_with("printf '%s\\n' "));
        assert!(payload.contains("'Parent message:'"));
        assert!(payload.contains("'status ping from parent'"));
        assert!(!payload.contains("Parent message:\\rstatus"));
    }

    #[test]
    fn root_pane_id_from_panes_response_prefers_declared_root() {
        let response = serde_json::json!({
            "result": {
                "panes": [
                    {"pane_id": "child-first", "is_root_pane": false},
                    {"pane_id": "root-pane", "is_root_pane": true}
                ]
            }
        });

        assert_eq!(
            root_pane_id_from_panes_response(&response).as_deref(),
            Some("root-pane")
        );
    }

    #[test]
    fn git_status_line_parses_changed_and_untracked_files() {
        let modified = parse_git_status_line(" M src/desktop.rs").unwrap();
        assert_eq!(modified.code, " M");
        assert_eq!(modified.path, "src/desktop.rs");
        assert!(!modified.staged);
        assert!(modified.unstaged);
        assert!(!modified.untracked);

        let untracked = parse_git_status_line("?? src/wave.rs").unwrap();
        assert_eq!(untracked.code, "??");
        assert_eq!(untracked.path, "src/wave.rs");
        assert!(untracked.untracked);
    }

    #[test]
    fn git_status_line_parses_renamed_files() {
        let renamed = parse_git_status_line("R  old.rs -> new.rs").unwrap();
        assert_eq!(renamed.code, "R ");
        assert_eq!(renamed.old_path.as_deref(), Some("old.rs"));
        assert_eq!(renamed.path, "new.rs");
        assert!(renamed.staged);
        assert!(!renamed.unstaged);
    }

    fn test_contract(title: &str) -> WaveContract {
        WaveContract {
            title: title.into(),
            pane_id: None,
            mode: WaveMode::ReadOnly,
            status: Some(WaveStatus::Queued),
            lifecycle_lane: Some(crate::wave::WaveLifecycleLane::Running),
            dependency: None,
            report: WaveReportGate::default(),
            blast_radius: BlastRadius::Unknown,
            prompt_delivery: None,
            arcs: Vec::new(),
        }
    }

    fn test_pane_target(pane_id: &str, contract_title: Option<&str>) -> MissionImportPaneTarget {
        MissionImportPaneTarget {
            pane_id: pane_id.into(),
            terminal_id: Some(format!("term-{pane_id}")),
            contract_title: contract_title.map(str::to_string),
        }
    }

    fn test_assignment(
        title: &str,
        pane_id: Option<&str>,
        status: MissionImportAssignmentStatus,
    ) -> MissionImportAssignment {
        MissionImportAssignment {
            contract_title: title.into(),
            pane_id: pane_id.map(str::to_string),
            terminal_id: pane_id.map(|id| format!("term-{id}")),
            status,
            planned_action: MissionImportPlannedAction::NeedsPane,
            gate_status: MissionDependencyGateStatus::Ready,
            gate_reason: None,
            gate_upstream_titles: Vec::new(),
            error: None,
            prompt_sent: false,
        }
    }

    fn test_sweep_pane(
        pane_id: &str,
        report: Option<ReportIngestSnapshot>,
        error: Option<&str>,
    ) -> MissionSweepPane {
        test_sweep_pane_with_contract(pane_id, pane_id, report, None, None, &[]).with_error(error)
    }

    fn test_sweep_pane_with_contract(
        pane_id: &str,
        title: &str,
        report: Option<ReportIngestSnapshot>,
        status: Option<WaveStatus>,
        dependency: Option<&str>,
        arc_ids: &[&str],
    ) -> MissionSweepPane {
        MissionSweepPane {
            pane_id: pane_id.into(),
            title: title.into(),
            status,
            dependency: dependency.map(str::to_string),
            arc_ids: arc_ids.iter().map(|arc| (*arc).to_string()).collect(),
            output: report
                .as_ref()
                .map(|_| PaneOutputSnapshot::from_text(pane_id.into(), "done".into(), 12)),
            report,
            error: None,
        }
    }
}
