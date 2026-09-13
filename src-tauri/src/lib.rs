use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    Emitter, Manager,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

mod codex_sessions;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const BLINK_INTERVAL: Duration = Duration::from_millis(500);
const SESSION_STALE_SECS: u64 = 300;
const WORKING_STALE_SECS: u64 = 60;
const COMPLETED_DISPLAY_SECS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
enum State {
    Idle = 0,
    Completed = 1,
    Working = 2,
    Waiting = 3,
    Error = 4,
}

impl State {
    fn from_str(s: &str) -> Self {
        match s {
            "working" => State::Working,
            "waiting" => State::Waiting,
            "error" => State::Error,
            "completed" => State::Completed,
            _ => State::Idle,
        }
    }

    fn key(self) -> &'static str {
        match self {
            State::Working => "working",
            State::Waiting => "waiting",
            State::Error => "error",
            State::Completed => "completed",
            State::Idle => "idle",
        }
    }
}

#[derive(Deserialize)]
struct SessionData {
    state: String,
    #[allow(dead_code)]
    message: Option<String>,
    timestamp: Option<u64>,
    session_id: Option<String>,
    agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionSnapshot {
    session_id: String,
    agent: String,
    state: State,
    message: String,
    timestamp: u64,
    modified_at: u64,
}

fn merge_scanned_codex_sessions(
    mut hook_sessions: Vec<SessionSnapshot>,
    scanned_sessions: Vec<codex_sessions::CodexSession>,
) -> Vec<SessionSnapshot> {
    for scanned in scanned_sessions {
        let waiting_hook_exists = hook_sessions.iter().any(|session| {
            session.agent == "codex"
                && session.session_id == scanned.session_id
                && session.state == State::Waiting
        });
        if waiting_hook_exists {
            continue;
        }
        hook_sessions.retain(|session| {
            !(session.agent == "codex" && session.session_id == scanned.session_id)
        });
        hook_sessions.push(SessionSnapshot {
            session_id: scanned.session_id,
            agent: "codex".to_string(),
            state: match scanned.state {
                codex_sessions::SessionState::Idle => State::Idle,
                codex_sessions::SessionState::Working => State::Working,
                codex_sessions::SessionState::Completed => State::Completed,
            },
            message: scanned.message,
            timestamp: scanned.timestamp,
            modified_at: scanned.timestamp,
        });
    }
    hook_sessions
}

struct AppState {
    state: State,
    message: String,
    timestamp: u64,
    active_count: usize,
    blink_on: bool,
    completed_since: Option<u64>,
    setup_error: Option<String>,
    autostart_error: Option<String>,
}

fn startup_failure_message(
    autostart_error: Option<&str>,
    codex_setup_error: Option<&str>,
) -> Option<String> {
    let mut errors = Vec::new();
    if let Some(error) = autostart_error {
        errors.push(format!("autostart setup failed: {error}"));
    }
    if let Some(error) = codex_setup_error {
        errors.push(format!("Codex setup failed: {error}"));
    }
    (!errors.is_empty()).then(|| errors.join("; "))
}

fn sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".code-light")
        .join("sessions")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn make_empty_icon() -> Image<'static> {
    let size = 64u32;
    let rgba = vec![0u8; (size * size * 4) as usize];
    Image::new_owned(rgba, size, size)
}

fn build_icons() -> HashMap<String, Image<'static>> {
    let color_map: &[(&str, &[u8])] = &[
        ("idle", include_bytes!("../icons/status/gray.png")),
        ("working", include_bytes!("../icons/status/green.png")),
        ("waiting", include_bytes!("../icons/status/yellow.png")),
        ("error", include_bytes!("../icons/status/red.png")),
        ("completed", include_bytes!("../icons/status/blue.png")),
    ];
    let mut map = HashMap::new();
    for (key, data) in color_map {
        let img = image::load_from_memory(data)
            .unwrap_or_else(|e| panic!("Failed to decode embedded icon '{}': {}", key, e));
        let rgba = img.to_rgba8();
        let (w, h) = (rgba.width(), rgba.height());
        map.insert(key.to_string(), Image::new_owned(rgba.into_raw(), w, h));
    }
    map.insert("off".to_string(), make_empty_icon());
    map
}

fn read_all_sessions() -> (State, String, u64, usize) {
    let now = now_secs();
    let dir = sessions_dir();
    let _ = fs::create_dir_all(&dir);
    let mut hook_sessions = Vec::new();

    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            let mtime = modified
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            if now.saturating_sub(mtime) > SESSION_STALE_SECS {
                continue;
            }

            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(data) = serde_json::from_str::<SessionData>(&content) else {
                continue;
            };

            let fallback_id = path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown");
            hook_sessions.push(SessionSnapshot {
                session_id: data.session_id.unwrap_or_else(|| fallback_id.to_string()),
                agent: data.agent.unwrap_or_else(|| "unknown".to_string()),
                state: State::from_str(&data.state),
                message: data.message.unwrap_or_default(),
                timestamp: data.timestamp.unwrap_or(mtime),
                modified_at: mtime,
            });
        }
    }

    let codex_root = dirs::home_dir()
        .unwrap_or_default()
        .join(".codex")
        .join("sessions");
    let sessions = merge_scanned_codex_sessions(
        hook_sessions,
        codex_sessions::scan_codex_sessions(&codex_root),
    );
    let mut best_state = State::Idle;
    let mut best_message = String::new();
    let mut best_ts = 0;
    let active_count = sessions.len();
    for session in sessions {
        let mut state = session.state;
        if state == State::Working && now.saturating_sub(session.modified_at) > WORKING_STALE_SECS {
            state = State::Completed;
        }
        if state > best_state {
            best_state = state;
            best_message = session.message;
            best_ts = session.timestamp;
        }
    }

    (best_state, best_message, best_ts, active_count)
}

fn format_time(ts: u64) -> String {
    if ts == 0 {
        return String::new();
    }
    let delta = now_secs().saturating_sub(ts);
    if delta < 60 {
        format!("{}s ago", delta)
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else {
        format!("{}h ago", delta / 3600)
    }
}

fn build_status_text(state: State, message: &str, ts: u64, count: usize) -> String {
    let label = match state {
        State::Working => "Working",
        State::Waiting => "Waiting for confirmation",
        State::Error => "Error",
        State::Completed => "Task completed",
        State::Idle => "Idle",
    };
    let mut parts = vec![format!("code-light: {}", label)];
    if count > 1 {
        parts.push(format!("({} sessions)", count));
    }
    if !message.is_empty() {
        parts.push(message.to_string());
    }
    let time_str = format_time(ts);
    if !time_str.is_empty() {
        parts.push(time_str);
    }
    parts.join(" | ")
}

fn cleanup_completed_sessions() {
    let dir = sessions_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(data) = serde_json::from_str::<SessionData>(&content) {
                if data.state == "completed" {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

fn get_hooks_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let bundled = parent.join("hooks");
            if bundled.is_dir() {
                return bundled;
            }

            let search_paths: &[&str] = if cfg!(target_os = "macos") {
                &["../Resources/hooks", "../Resources/_up_/hooks"]
            } else if cfg!(target_os = "linux") {
                &["../lib/code-light/hooks", "../resources/hooks"]
            } else {
                &["../resources/hooks"]
            };

            for rel in search_paths {
                if let Some(resolved) = parent.join(rel).canonicalize().ok() {
                    if resolved.is_dir() {
                        return resolved;
                    }
                }
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("hooks")
}

fn shell_command(hooks_dir: &std::path::Path, script: &str) -> String {
    let script_path = hooks_dir.join(script);
    let path = script_path.display().to_string();
    if cfg!(target_os = "windows") {
        let path = path.replace('\\', "/");
        format!("bash '{}'", path)
    } else {
        format!("bash '{}'", path)
    }
}

fn base_hooks_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".code-light")
        .join("hooks")
}

fn claude_local_hooks_dir() -> PathBuf {
    base_hooks_dir().join("claude")
}

fn codex_local_hooks_dir() -> PathBuf {
    base_hooks_dir().join("codex")
}

fn copy_hooks_to_local(src: &std::path::Path, dest: &std::path::Path) {
    let _ = fs::create_dir_all(dest);
    if let Ok(entries) = fs::read_dir(src) {
        for entry in entries.flatten() {
            let src_path = entry.path();
            if src_path.is_file() {
                let dest_path = dest.join(entry.file_name());
                let _ = fs::copy(&src_path, &dest_path);
            }
        }
    }
}

fn setup_hooks() {
    let settings_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".claude")
        .join("settings.json");

    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::Value::Object(Default::default()))
    } else {
        serde_json::Value::Object(Default::default())
    };

    let bundled_claude = get_hooks_dir().join("claude");
    let claude_dest = claude_local_hooks_dir();
    copy_hooks_to_local(&bundled_claude, &claude_dest);

    let hook_defs = serde_json::json!({
        "PreToolUse": [{ "matcher": "", "hooks": [{ "type": "command", "command": shell_command(&claude_dest, "pre-tool-use.sh") }] }],
        "PostToolUse": [{ "matcher": "", "hooks": [{ "type": "command", "command": shell_command(&claude_dest, "post-tool-use.sh") }] }],
        "PostToolUseFailure": [{ "matcher": "", "hooks": [{ "type": "command", "command": shell_command(&claude_dest, "post-tool-use-failure.sh") }] }],
        "Notification": [{ "matcher": "", "hooks": [{ "type": "command", "command": shell_command(&claude_dest, "notification.sh") }] }],
        "Stop": [{ "matcher": "", "hooks": [{ "type": "command", "command": shell_command(&claude_dest, "stop.sh") }] }],
    });

    if let Some(hooks) = hook_defs.as_object() {
        let settings_hooks = settings
            .as_object_mut()
            .unwrap()
            .entry("hooks")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));

        for (event, defs) in hooks {
            settings_hooks
                .as_object_mut()
                .unwrap()
                .insert(event.clone(), defs.clone());
        }
    }

    if let Ok(content) = serde_json::to_string_pretty(&settings) {
        let _ = fs::write(&settings_path, content);
    }

    let _ = fs::create_dir_all(sessions_dir());
}

fn setup_codex_hooks() -> Result<(), String> {
    let codex_dir = dirs::home_dir().unwrap_or_default().join(".codex");
    let hooks_json_path = codex_dir.join("hooks.json");

    let mut codex_config: serde_json::Value = if hooks_json_path.exists() {
        let content = fs::read_to_string(&hooks_json_path)
            .map_err(|error| format!("failed to read hooks.json: {error}"))?;
        parse_codex_hooks_config(&content)?
    } else {
        serde_json::Value::Object(Default::default())
    };
    let config_root = codex_config
        .as_object_mut()
        .ok_or_else(|| "invalid hooks.json root; existing file preserved".to_string())?;

    let codex_dest = codex_local_hooks_dir();
    let command = install_codex_hook_wrapper(&codex_dest)?;

    let hook_defs = serde_json::json!({
        "SessionStart": [{ "matcher": "", "hooks": [{ "type": "command", "command": command, "timeout": 5, "statusMessage": "Code Light: Session tracking" }] }],
        "PreToolUse": [{ "matcher": "", "hooks": [{ "type": "command", "command": command, "timeout": 5, "statusMessage": "Code Light: Tracking" }] }],
        "PermissionRequest": [{ "matcher": "", "hooks": [{ "type": "command", "command": command, "timeout": 5, "statusMessage": "Code Light: Waiting for approval" }] }],
        "AskUserQuestion": [{ "matcher": "", "hooks": [{ "type": "command", "command": command, "timeout": 5, "statusMessage": "Code Light: Waiting for answer" }] }],
        "PostToolUse": [{ "matcher": "", "hooks": [{ "type": "command", "command": command, "timeout": 5 }] }],
        "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": command, "timeout": 5 }] }],
        "Stop": [{ "hooks": [{ "type": "command", "command": command, "timeout": 5 }] }],
    });

    let config_hooks = config_root
        .entry("hooks")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    if !config_hooks.is_object() {
        return Err("invalid hooks.json hooks section; existing file preserved".to_string());
    }

    for (event, defs) in hook_defs.as_object().unwrap() {
        let entries = config_hooks
            .as_object_mut()
            .unwrap()
            .entry(event.clone())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        let entries = entries
            .as_array_mut()
            .ok_or_else(|| format!("invalid {event} hook list; existing file preserved"))?;
        let new_entries = defs
            .as_array()
            .ok_or_else(|| format!("invalid generated {event} hook list"))?;
        entries.retain(|entry| {
            let legacy_command = entry
                .pointer("/hooks/0/command")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .replace('\\', "/")
                .to_ascii_lowercase();
            !(legacy_command.contains("bash")
                && legacy_command.contains(".code-light/hooks/codex/"))
        });
        let command = new_entries.iter().find_map(|entry| {
            entry
                .pointer("/hooks/0/command")
                .and_then(|value| value.as_str())
        });
        let already_installed = command.is_some_and(|command| {
            entries.iter().any(|entry| {
                entry
                    .pointer("/hooks/0/command")
                    .and_then(|value| value.as_str())
                    == Some(command)
            })
        });
        if !already_installed {
            entries.extend(new_entries.iter().cloned());
        }
    }

    fs::create_dir_all(&codex_dir)
        .map_err(|error| format!("failed to create Codex directory: {error}"))?;
    ensure_codex_hooks_enabled(&codex_dir)?;
    let content = serde_json::to_string_pretty(&codex_config)
        .map_err(|error| format!("failed to serialize hooks.json: {error}"))?;
    replace_file_contents(&hooks_json_path, content.as_bytes())
        .map_err(|error| format!("failed to write hooks.json: {error}"))?;
    fs::create_dir_all(sessions_dir())
        .map_err(|error| format!("failed to create session directory: {error}"))?;
    Ok(())
}

fn parse_codex_hooks_config(content: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(content.trim_start_matches('\u{feff}'))
        .map_err(|error| format!("invalid hooks.json; existing file preserved: {error}"))
}

fn install_codex_hook_wrapper(dest: &std::path::Path) -> Result<String, String> {
    fs::create_dir_all(dest)
        .map_err(|error| format!("failed to create hook directory: {error}"))?;
    let executable = std::env::current_exe()
        .map_err(|error| format!("failed to locate Code Light executable: {error}"))?;

    if cfg!(target_os = "windows") {
        let wrapper = dest.join("code-light-codex-hook.cmd");
        let contents = format!(
            "@echo off\r\n\"{}\" --codex-hook >nul 2>nul\r\nexit /b 0\r\n",
            executable.display()
        );
        fs::write(&wrapper, contents)
            .map_err(|error| format!("failed to write hook wrapper: {error}"))?;
        Ok(format!(
            "cmd /d /s /c \"\"{}\"\"",
            wrapper.display().to_string().replace('\\', "/")
        ))
    } else {
        let wrapper = dest.join("code-light-codex-hook.sh");
        let contents = format!(
            "#!/bin/sh\n\"{}\" --codex-hook >/dev/null 2>/dev/null\nexit 0\n",
            executable.display()
        );
        fs::write(&wrapper, contents)
            .map_err(|error| format!("failed to write hook wrapper: {error}"))?;
        Ok(format!("sh \"{}\"", wrapper.display()))
    }
}

fn ensure_codex_hooks_enabled(codex_dir: &std::path::Path) -> Result<(), String> {
    let config_path = codex_dir.join("config.toml");
    let contents = if config_path.exists() {
        fs::read_to_string(&config_path)
            .map_err(|error| format!("failed to read Codex config.toml: {error}"))?
    } else {
        String::new()
    };
    let next_contents = enable_hooks_feature(contents.trim_start_matches('\u{feff}'));
    if next_contents != contents {
        replace_file_contents(&config_path, next_contents.as_bytes())
            .map_err(|error| format!("failed to enable Codex hooks: {error}"))?;
    }
    Ok(())
}

fn enable_hooks_feature(contents: &str) -> String {
    let newline = if contents.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();
    if let Some(features_index) = lines
        .iter()
        .position(|line| toml_section_header(line) == Some("features"))
    {
        let section_end = lines
            .iter()
            .enumerate()
            .skip(features_index + 1)
            .find(|(_, line)| toml_section_header(line).is_some())
            .map(|(index, _)| index)
            .unwrap_or(lines.len());
        if let Some(hooks_index) = (features_index + 1..section_end).find(|index| {
            lines[*index]
                .split_once('=')
                .is_some_and(|(name, _)| name.trim() == "hooks")
        }) {
            if lines[hooks_index]
                .split_once('=')
                .is_some_and(|(_, value)| value.trim() == "true")
            {
                return contents.to_string();
            }
            lines[hooks_index] = "hooks = true".to_string();
        } else {
            lines.insert(features_index + 1, "hooks = true".to_string());
        }
    } else {
        if !lines.is_empty() && !lines.last().is_some_and(|line| line.is_empty()) {
            lines.push(String::new());
        }
        lines.push("[features]".to_string());
        lines.push("hooks = true".to_string());
    }
    let mut result = lines.join(newline);
    result.push_str(newline);
    result
}

fn toml_section_header(line: &str) -> Option<&str> {
    let header = line.split('#').next()?.trim();
    header.strip_prefix('[')?.strip_suffix(']')
}

fn replace_file_contents(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&temp, contents)?;
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(_) if path.exists() => {
            fs::remove_file(path)?;
            fs::rename(&temp, path)
        }
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
        }
    }
}

#[cfg(target_os = "macos")]
fn make_window_transparent(window: &tauri::WebviewWindow) {
    use objc::runtime::{Class, Object, NO};
    use objc::{msg_send, sel, sel_impl};
    if let Ok(ptr) = window.ns_window() {
        unsafe {
            let ns_window = ptr as *mut Object;
            let clear: *mut Object = msg_send![Class::get("NSColor").unwrap(), clearColor];
            let _: () = msg_send![ns_window, setOpaque: NO];
            let _: () = msg_send![ns_window, setBackgroundColor: clear];
        }
    }
}

#[tauri::command]
fn get_current_state(state: tauri::State<'_, Arc<Mutex<AppState>>>) -> serde_json::Value {
    let s = state.lock().unwrap();
    serde_json::json!({
        "state": s.state.key(),
        "message": s.message,
        "timestamp": s.timestamp,
        "activeCount": s.active_count,
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let icons = Arc::new(Mutex::new(build_icons()));

    let app_state = Arc::new(Mutex::new(AppState {
        state: State::Idle,
        message: String::new(),
        timestamp: 0,
        active_count: 0,
        blink_on: true,
        completed_since: None,
        setup_error: None,
        autostart_error: None,
    }));

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None::<Vec<&str>>,
        ))
        .plugin(tauri_plugin_opener::init())
        .manage(app_state.clone())
        .invoke_handler(tauri::generate_handler![get_current_state])
        .setup(move |app| {
            let initial_icon = icons.lock().unwrap().get("idle").unwrap().clone();

            let setup_claude_item =
                MenuItemBuilder::with_id("setup_claude", "Setup Claude Hooks").build(app)?;
            let setup_codex_item =
                MenuItemBuilder::with_id("setup_codex", "Setup Codex Hooks").build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "Quit Code Light").build(app)?;
            let menu = MenuBuilder::new(app)
                .item(&setup_claude_item)
                .item(&setup_codex_item)
                .separator()
                .item(&quit_item)
                .build()?;

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let autostart_error = app
                .autolaunch()
                .enable()
                .err()
                .map(|error| error.to_string());
            let codex_setup_error = setup_codex_hooks().err();
            let startup_failure =
                startup_failure_message(autostart_error.as_deref(), codex_setup_error.as_deref());
            let startup_tooltip = startup_failure
                .as_deref()
                .map(|error| format!("code-light: {error}"))
                .unwrap_or_else(|| "code-light: Idle".to_string());
            if let Some(error) = autostart_error {
                let mut state = app_state.lock().unwrap();
                state.state = State::Error;
                state.message = error.clone();
                state.autostart_error = Some(error);
            }
            if let Some(error) = codex_setup_error {
                let mut state = app_state.lock().unwrap();
                state.state = State::Error;
                state.message = error.clone();
                state.setup_error = Some(error);
            }

            let tray = TrayIconBuilder::with_id("main")
                .icon(initial_icon)
                .menu(&menu)
                .tooltip(startup_tooltip)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "setup_claude" => {
                        setup_hooks();
                        if let Some(tray) = app.tray_by_id("main") {
                            let _ = tray.set_tooltip(Some("code-light: Claude hooks configured!"));
                        }
                    }
                    "setup_codex" => {
                        let result = setup_codex_hooks();
                        let state = app.state::<Arc<Mutex<AppState>>>();
                        let mut state = state.lock().unwrap();
                        match &result {
                            Ok(()) => state.setup_error = None,
                            Err(error) => {
                                state.state = State::Error;
                                state.message = error.clone();
                                state.setup_error = Some(error.clone());
                            }
                        }
                        drop(state);
                        if let Some(tray) = app.tray_by_id("main") {
                            let tooltip = match result {
                                Ok(()) => "code-light: Codex hooks configured!".to_string(),
                                Err(_) => "code-light: Codex hook setup failed".to_string(),
                            };
                            let _ = tray.set_tooltip(Some(tooltip));
                        }
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|_tray, event| {
                    let _ = event;
                })
                .build(app)?;
            app.manage(tray);

            // Poll thread
            let poll_app = app.handle().clone();
            let poll_state = app_state.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(POLL_INTERVAL);
                let (state, message, ts, count) = read_all_sessions();
                let mut s = poll_state.lock().unwrap();

                if let Some(error) =
                    startup_failure_message(s.autostart_error.as_deref(), s.setup_error.as_deref())
                {
                    if let Some(tray) = poll_app.tray_by_id("main") {
                        let text: String = format!("code-light setup failed: {error}")
                            .chars()
                            .take(120)
                            .collect();
                        let _ = tray.set_tooltip(Some(text));
                    }
                    continue;
                }

                if s.state == State::Completed {
                    if let Some(since) = s.completed_since {
                        if now_secs() - since > COMPLETED_DISPLAY_SECS {
                            s.state = State::Idle;
                            s.completed_since = None;
                            drop(s);
                            cleanup_completed_sessions();
                            s = poll_state.lock().unwrap();
                        }
                    }
                }

                if state != s.state {
                    s.state = state;
                    s.message = message;
                    s.timestamp = ts;
                    s.active_count = count;
                    s.blink_on = true;
                    s.completed_since = if state == State::Completed {
                        Some(now_secs())
                    } else {
                        None
                    };

                    let payload = serde_json::json!({
                        "state": s.state.key(),
                        "message": s.message,
                        "timestamp": s.timestamp,
                        "activeCount": s.active_count,
                    });
                    let _ = poll_app.emit("state-changed", payload);
                } else if ts != s.timestamp || count != s.active_count {
                    s.message = message;
                    s.timestamp = ts;
                    s.active_count = count;
                }

                let text = build_status_text(s.state, &s.message, s.timestamp, s.active_count);
                if let Some(tray) = poll_app.tray_by_id("main") {
                    let _ = tray.set_tooltip(Some(&text));
                }
            });

            // Blink thread
            let blink_app = app.handle().clone();
            let blink_state = app_state.clone();
            let blink_icons = icons.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(BLINK_INTERVAL);
                let mut s = blink_state.lock().unwrap();

                let key = match s.state {
                    State::Working | State::Waiting | State::Error => {
                        s.blink_on = !s.blink_on;
                        if s.blink_on {
                            s.state.key().to_string()
                        } else {
                            "off".to_string()
                        }
                    }
                    _ => s.state.key().to_string(),
                };

                let icon = blink_icons.lock().unwrap().get(&key).unwrap().clone();
                if let Some(tray) = blink_app.tray_by_id("main") {
                    let _ = tray.set_icon(Some(icon));
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::{
        codex_hook_state, enable_hooks_feature, merge_scanned_codex_sessions,
        parse_codex_hooks_config, startup_failure_message, write_codex_session, SessionSnapshot,
        State,
    };
    use crate::codex_sessions::{CodexSession, SessionState};

    fn hook_session(id: &str, agent: &str, state: State) -> SessionSnapshot {
        SessionSnapshot {
            session_id: id.to_string(),
            agent: agent.to_string(),
            state,
            message: String::new(),
            timestamp: 1,
            modified_at: 1,
        }
    }

    #[test]
    fn merge_scanned_codex_replaces_matching_hook_session_and_keeps_claude() {
        let merged = merge_scanned_codex_sessions(
            vec![
                hook_session("codex-1", "codex", State::Completed),
                hook_session("claude-1", "claude", State::Waiting),
            ],
            vec![CodexSession {
                session_id: "codex-1".to_string(),
                state: SessionState::Working,
                message: "Task working".to_string(),
                timestamp: 2,
            }],
        );

        assert_eq!(merged.len(), 2);
        assert!(merged
            .iter()
            .any(|session| { session.session_id == "codex-1" && session.state == State::Working }));
        assert!(merged.iter().any(|session| {
            session.session_id == "claude-1" && session.state == State::Waiting
        }));
    }

    #[test]
    fn codex_hook_events_map_permission_to_waiting() {
        assert_eq!(codex_hook_state("PermissionRequest"), State::Waiting);
        assert_eq!(codex_hook_state("PreToolUse"), State::Working);
        assert_eq!(codex_hook_state("Stop"), State::Completed);
    }

    #[test]
    fn merge_scanned_codex_preserves_waiting_hook_state() {
        let merged = merge_scanned_codex_sessions(
            vec![hook_session("codex-approval", "codex", State::Waiting)],
            vec![CodexSession {
                session_id: "codex-approval".to_string(),
                state: SessionState::Working,
                message: "Scanner still sees work".to_string(),
                timestamp: 2,
            }],
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].state, State::Waiting);
    }

    #[test]
    fn codex_session_writer_replaces_existing_state() {
        let dir = std::env::temp_dir().join(format!(
            "code-light-session-test-{}-{}",
            std::process::id(),
            super::now_secs()
        ));

        write_codex_session(&dir, "replace-me", State::Working, "Working").unwrap();
        write_codex_session(&dir, "replace-me", State::Waiting, "Waiting").unwrap();

        let raw = std::fs::read_to_string(dir.join("replace-me.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["state"], "waiting");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_hook_config_accepts_utf8_bom_without_losing_entries() {
        let value = parse_codex_hooks_config(
            "\u{feff}{\"hooks\":{\"PermissionRequest\":[{\"hooks\":[]}]}}",
        )
        .unwrap();

        assert!(value["hooks"]["PermissionRequest"].is_array());
        assert!(parse_codex_hooks_config("{broken").is_err());
    }

    #[test]
    fn codex_hooks_feature_is_enabled_inside_existing_section() {
        let config = "model = \"gpt\"\r\n\r\n[features] # existing\r\nhooks = false\r\nfoo = true\r\n\r\n[mcp]\r\nbar = true\r\n";
        let enabled = enable_hooks_feature(config);

        assert!(enabled.contains("[features] # existing\r\nhooks = true\r\nfoo = true"));
        assert!(enabled.contains("[mcp]\r\nbar = true"));
    }

    #[test]
    fn startup_failure_message_keeps_autostart_failure_visible() {
        assert_eq!(
            startup_failure_message(Some("registry access denied"), None),
            Some("autostart setup failed: registry access denied".to_string())
        );
    }
}

fn codex_hook_state(event: &str) -> State {
    match event {
        "PermissionRequest" | "AskUserQuestion" => State::Waiting,
        "Stop" | "SessionEnd" => State::Completed,
        "SessionStart" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => State::Working,
        _ => State::Idle,
    }
}

pub fn run_codex_hook_bridge() -> bool {
    if !std::env::args().any(|arg| arg == "--codex-hook") {
        return false;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return true;
    }
    let Ok(event) = serde_json::from_str::<serde_json::Value>(&input) else {
        return true;
    };
    let Some(raw_id) = event.get("session_id").and_then(|value| value.as_str()) else {
        return true;
    };
    let session_id: String = raw_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        .take(160)
        .collect();
    if session_id.is_empty() {
        return true;
    }
    let event_name = event
        .get("hook_event_name")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let state = codex_hook_state(event_name);
    let message = match state {
        State::Waiting => "Waiting for approval",
        State::Completed => "Task finished",
        State::Working => "Codex working",
        State::Idle | State::Error => "Idle",
    };
    let _ = write_codex_session(&sessions_dir(), &session_id, state, message);
    true
}

fn write_codex_session(
    dir: &std::path::Path,
    session_id: &str,
    state: State,
    message: &str,
) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let target = dir.join(format!("{session_id}.json"));
    let temp = dir.join(format!("{session_id}.json.tmp.{}", std::process::id()));
    let payload = serde_json::json!({
        "state": state.key(),
        "message": message,
        "timestamp": now_secs(),
        "session_id": session_id,
        "agent": "codex"
    });
    fs::write(&temp, payload.to_string())?;

    match fs::rename(&temp, &target) {
        Ok(()) => Ok(()),
        Err(first_error) if target.exists() => {
            fs::remove_file(&target)?;
            fs::rename(&temp, &target)
        }
        Err(first_error) => {
            let _ = fs::remove_file(&temp);
            Err(first_error)
        }
    }
}
