//! Process management, retry loop, and Tauri commands.

use crate::classifier::ErrorClassifier;
use crate::strategy::build_strategies;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    env,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
        Arc, Mutex,
    },
    thread,
};
use tauri::{AppHandle, Emitter, Manager};

// ── Constants ─────────────────────────────────────────────────────────────────

const BASE_ARGS: &[&str] = &[
    "-x", "16",
    "-s", "16",
    "-k", "1M",
    "-c",
    "--max-tries=3",
    "--retry-wait=2",
    "--timeout=30",
    "--connect-timeout=15",
    "--summary-interval=1",
    "--console-log-level=notice",
    "--auto-file-renaming=false",
];

const DEBUG_RAW_LOGS: bool = false;
const LOG_GROUP: &str = "Download Session";

static ANSI_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\x1b\[[0-9;]*[mGKHF]|\x1b\][^\x07]*\x07|\r").unwrap()
});

/// aria2 console summary (single line). Must not span multiple Rust string lines — a literal
/// newline in the pattern prevented matching real output (Chrome header paths looked “broken”).
static SUMMARY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"\[#[^\]]+\s+(?P<dl>[\d.]+\s*\w+)/(?P<total>[^\s(]+)\((?P<pct>\d+)%\)(?:.*?CN:(?P<cn>\d+))?(?:.*?DL:(?P<spd>[\d.]+\s*[\w/]+))?(?:.*?ETA:(?P<eta>\S+))?\]",
    )
    .unwrap()
});

/// Sparse summary without DL:/ETA: segments (still has downloaded/total/pct).
static SUMMARY_MIN_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\[#[^\]]+\s+(?P<dl>[\d.]+\s*\w+)/(?P<total>[^\s(]+)\((?P<pct>\d+)%\)(?:.*?CN:(?P<cn>\d+))?")
        .unwrap()
});

static PCT_FALLBACK_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\((\d+)%\)").unwrap());

static SPEED_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)([\d.]+)\s*([kmg])(?:i)?b").unwrap());

/// Final path from aria2 result lines (Windows or POSIX paths).
static DOWNLOAD_COMPLETE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)download complete:\s*(?P<path>.+)$").unwrap()
});

// ── App state (shared across commands) ───────────────────────────────────────

pub struct UiSession {
    pub strategy_name:    String,
    pub strategy_attempt: u32,
    pub filename:         Option<String>,
    /// Suppresses duplicate console lines from noisy aria2 output.
    pub last_log_line:    Option<String>,
    pub event_state:      EventState,
}

impl Default for UiSession {
    fn default() -> Self {
        Self {
            strategy_name:    "Default".into(),
            strategy_attempt: 1,
            filename:         None,
            last_log_line:    None,
            event_state:      EventState::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventType {
    DownloadStarted,
    DownloadProgress,
    DownloadResumed,
    DownloadPaused,
    ConnectionChanged,
    SpeedChanged,
    FileAllocated,
    ChecksumOk,
    DownloadCompleted,
    DownloadFailed,
    Retrying,
    MetadataUpdated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EventCategory {
    Success,
    Warn,
    Info,
    Error,
    Network,
    Progress,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventPayload {
    pub timestamp: String,
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub category: EventCategory,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "updateKey")]
    pub update_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct EventState {
    pub last_filename: Option<String>,
    pub last_phase: Option<String>,
    pub last_percent: Option<u32>,
    pub last_progress_percent: Option<u32>,
    pub last_speed_kib: Option<f64>,
    pub last_connections: Option<u32>,
    pub last_progress_emit_ms: u128,
    pub last_speed_emit_ms: u128,
    pub last_conn_emit_ms: u128,
}

pub struct AppState {
    pub abort:   Arc<AtomicBool>,
    pub running: Arc<AtomicBool>,
    // Holds the child process so stop_download can kill it immediately.
    pub child:   Arc<Mutex<Option<std::process::Child>>>,
    pub ui_session: Arc<Mutex<UiSession>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            abort:   Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            child:   Arc::new(Mutex::new(None)),
            ui_session: Arc::new(Mutex::new(UiSession::default())),
        }
    }
}

// ── aria2c path resolution ────────────────────────────────────────────────────

fn aria2_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "aria2c.exe"
    } else {
        "aria2c"
    }
}

fn bundled_aria2_names() -> &'static [&'static str] {
    #[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
    {
        &["aria2c-x86_64-pc-windows-msvc.exe", "aria2c.exe"]
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "gnu"))]
    {
        &["aria2c-x86_64-pc-windows-gnu.exe", "aria2c.exe"]
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        &["aria2c-x86_64-apple-darwin", "aria2c"]
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        &["aria2c-aarch64-apple-darwin", "aria2c"]
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        &["aria2c-x86_64-unknown-linux-gnu", "aria2c"]
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        &["aria2c-aarch64-unknown-linux-gnu", "aria2c"]
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64", any(target_env = "msvc", target_env = "gnu")),
        all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
        all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))
    )))]
    {
        &[aria2_binary_name()]
    }
}

fn executable_candidates(dir: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    bundled_aria2_names().iter().map(move |name| dir.join(name))
}

fn find_on_path(binary_name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(binary_name);
        if candidate.is_file() {
            return Some(candidate);
        }

        #[cfg(target_os = "windows")]
        {
            let candidate = dir.join("aria2c.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn resolve_aria2_path(app: &AppHandle) -> Option<PathBuf> {
    // 1. Tauri bundled external binary/resource.
    if let Ok(dir) = app.path().resource_dir() {
        if let Some(path) = executable_candidates(&dir).find(|p| p.is_file()) {
            return Some(path);
        }
    }
    // 2. Next to the running executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(path) = executable_candidates(dir).find(|p| p.is_file()) {
                return Some(path);
            }
        }
    }
    // 3. Developer fallback from PATH.
    if let Some(path) = find_on_path(aria2_binary_name()) {
        return Some(path);
    }
    None
}

// ── Emit helpers ──────────────────────────────────────────────────────────────

fn emit_log(app: &AppHandle, text: &str, kind: &str) {
    if !DEBUG_RAW_LOGS {
        return;
    }
    let color = match kind {
        "ok"       => "#28C76F",
        "err"      => "#FF4040",
        "warn"     => "#F0A500",
        "info"     => "#3D7FFF",
        "retry"    => "#A855F7",
        "strategy" => "#22D3EE",
        _          => "#7A9CBF", // base
    };
    let _ = app.emit("log-line", json!({ "text": text, "color": color }));
}

fn emit_event(app: &AppHandle, payload: &EventPayload) {
    let _ = app.emit("event-log", payload);
}

fn emit_status(app: &AppHandle, text: &str, kind: &str) {
    let _ = app.emit("status-msg", json!({ "text": text, "kind": kind }));
}

fn now_ms() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn ts_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn make_event(
    event_type: EventType,
    category: EventCategory,
    title: impl Into<String>,
    details: Option<String>,
    update_key: Option<String>,
    group: Option<String>,
) -> EventPayload {
    EventPayload {
        timestamp: ts_hms(),
        event_type,
        category,
        title: title.into(),
        details,
        update_key,
        group,
    }
}

fn parse_speed_kib(speed: &str) -> Option<f64> {
    let caps = SPEED_RE.captures(speed)?;
    let val: f64 = caps.get(1)?.as_str().parse().ok()?;
    let unit = caps.get(2)?.as_str().to_ascii_lowercase();
    let factor = match unit.as_str() {
        "k" => 1.0,
        "m" => 1024.0,
        "g" => 1024.0 * 1024.0,
        _ => 1.0,
    };
    Some(val * factor)
}

fn filename_hint_from_url(url: &str) -> Option<String> {
    let base = url.split('?').next()?;
    let leaf = base.rsplit('/').next().filter(|s| !s.is_empty())?;
    if leaf.contains(':') && base.contains("://") {
        return None;
    }
    Some(leaf.to_string())
}

fn basename_from_path(p: &str) -> Option<String> {
    PathBuf::from(p.trim())
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

fn total_is_known(total: &str) -> bool {
    let t = total.trim().to_lowercase();
    if t.is_empty() {
        return false;
    }
    !(t.contains('?') || t == "*" || t == "unknown")
}

/// Unified progress payload for the UI (Chrome headers mode uses the same path as default).
fn emit_transfer_update(
    app: &AppHandle,
    ui: &Arc<Mutex<UiSession>>,
    pct: u32,
    speed: Option<&str>,
    eta: Option<&str>,
    downloaded: Option<&str>,
    total: Option<&str>,
    phase: &str,
) {
    let (strategy_name, attempt, fname) = {
        let g = ui.lock().unwrap();
        (
            g.strategy_name.clone(),
            g.strategy_attempt,
            g.filename.clone(),
        )
    };
    let total_s = total.unwrap_or("—");
    let total_known = total.map(total_is_known).unwrap_or(false);
    let _ = app.emit(
        "transfer-update",
        json!({
            "pct": pct,
            "speed": speed.unwrap_or("—"),
            "eta": eta.unwrap_or("—"),
            "downloaded": downloaded.unwrap_or("—"),
            "total": total_s,
            "totalKnown": total_known,
            "phase": phase,
            "filename": fname.unwrap_or_else(|| "—".into()),
            "strategyName": strategy_name,
            "strategyAttempt": attempt,
        }),
    );
    let _ = app.emit("progress", pct);
    if let Some(s) = speed {
        let _ = app.emit("speed", s);
    }
    if let Some(e) = eta {
        let _ = app.emit("eta", e);
    }
    if let Some(d) = downloaded {
        let _ = app.emit("downloaded", d);
    }
}

fn emit_progress_events(
    app: &AppHandle,
    ui: &Arc<Mutex<UiSession>>,
    pct: u32,
    speed: Option<&str>,
    eta: Option<&str>,
    connections: Option<u32>,
) {
    let now = now_ms();
    let speed_kib = speed.and_then(parse_speed_kib);

    let mut g = ui.lock().unwrap();
    let pct_changed = g.event_state.last_percent != Some(pct);
    let speed_changed = match (speed_kib, g.event_state.last_speed_kib) {
        (Some(a), Some(b)) => (a - b).abs() >= 0.01,
        (Some(_), None) => true,
        _ => false,
    };
    let conn_changed = connections.is_some() && g.event_state.last_connections != connections;

    let mut details_parts: Vec<String> = Vec::new();
    if let Some(spd) = speed {
        details_parts.push(format!("Speed {spd}"));
    }
    if let Some(e) = eta {
        details_parts.push(format!("ETA {e}"));
    }
    if let Some(cn) = connections {
        details_parts.push(format!("CN {cn}"));
    }
    let details = if details_parts.is_empty() {
        None
    } else {
        Some(details_parts.join(" • "))
    };

    if pct_changed || speed_changed || conn_changed {
        emit_event(
            app,
            &make_event(
                EventType::DownloadProgress,
                EventCategory::Progress,
                format!("Downloading → {pct}%"),
                details.clone(),
                Some("active-progress".into()),
                Some(LOG_GROUP.into()),
            ),
        );
    }

    let last_progress_pct = g.event_state.last_progress_percent.unwrap_or(0);
    if pct >= last_progress_pct.saturating_add(5) && now.saturating_sub(g.event_state.last_progress_emit_ms) >= 10_000 {
        let mut milestone_parts = vec![format!("{pct}%")];
        if let Some(spd) = speed {
            milestone_parts.push(format!("{spd}"));
        }
        if let Some(cn) = connections {
            milestone_parts.push(format!("CN {cn}"));
        }
        emit_event(
            app,
            &make_event(
                EventType::DownloadProgress,
                EventCategory::Progress,
                "Download progress",
                Some(milestone_parts.join(" • ")),
                None,
                Some(LOG_GROUP.into()),
            ),
        );
        g.event_state.last_progress_emit_ms = now;
        g.event_state.last_progress_percent = Some(pct);
    }

    if let Some(sk) = speed_kib {
        let last = g.event_state.last_speed_kib.unwrap_or(0.0);
        let diff = (sk - last).abs();
        let rel = if last > 0.0 { diff / last } else { 1.0 };
        let abs_ok = diff >= 512.0;
        let rel_ok = rel >= 0.15;
        if (abs_ok || rel_ok) && now.saturating_sub(g.event_state.last_speed_emit_ms) >= 3_000 {
            let detail = speed.map(|s| s.to_string()).or_else(|| {
                Some(format!("{:.2} MiB/s", sk / 1024.0))
            });
            emit_event(
                app,
                &make_event(
                    EventType::SpeedChanged,
                    EventCategory::Network,
                    "Speed changed",
                    detail,
                    None,
                    Some(LOG_GROUP.into()),
                ),
            );
            g.event_state.last_speed_emit_ms = now;
        }
        g.event_state.last_speed_kib = Some(sk);
    }

    if let Some(cn) = connections {
        if g.event_state.last_connections != Some(cn) {
            emit_event(
                app,
                &make_event(
                    EventType::ConnectionChanged,
                    EventCategory::Network,
                    "Connections changed",
                    Some(format!("{cn} connections")),
                    None,
                    Some(LOG_GROUP.into()),
                ),
            );
            g.event_state.last_connections = Some(cn);
            g.event_state.last_conn_emit_ms = now;
        }
    }

    g.event_state.last_percent = Some(pct);
    g.event_state.last_phase = Some("downloading".into());
}

// ── Download loop (runs on a dedicated thread) ────────────────────────────────

fn run_download_loop(
    app:     AppHandle,
    url:     String,
    dir:     String,
    abort:   Arc<AtomicBool>,
    child_slot: Arc<Mutex<Option<std::process::Child>>>,
    running: Arc<AtomicBool>,
    ui_session: Arc<Mutex<UiSession>>,
) {
    let aria2_path = match resolve_aria2_path(&app) {
        Some(path) => path,
        None => {
            emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "error");
            emit_event(
                &app,
                &make_event(
                    EventType::DownloadFailed,
                    EventCategory::Error,
                    format!("{} not found", aria2_binary_name()),
                    Some("No bundled or PATH aria2 executable was found.".into()),
                    None,
                    Some(LOG_GROUP.into()),
                ),
            );
            emit_log(&app, &format!("[FATAL] {} not found", aria2_binary_name()), "err");
            emit_status(&app, "aria2 not found", "err");
            let _ = app.emit("finished", false);
            running.store(false, Ordering::SeqCst);
            return;
        }
    };

    if !aria2_path.exists() {
        emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "error");
        emit_event(
            &app,
            &make_event(
                EventType::DownloadFailed,
                EventCategory::Error,
                format!("{} not found", aria2_binary_name()),
                Some(format!("Path: {}", aria2_path.display())),
                None,
                Some(LOG_GROUP.into()),
            ),
        );
        emit_log(&app, &format!("[FATAL] {} not found at: {}", aria2_binary_name(), aria2_path.display()), "err");
        emit_status(&app, "aria2 not found", "err");
        let _ = app.emit("finished", false);
        running.store(false, Ordering::SeqCst);
        return;
    }

    let strategies   = build_strategies(&url);
    let mut attempt  = 0usize;
    let mut classifier = ErrorClassifier::new();

    loop {
        if abort.load(Ordering::SeqCst) { break; }

        let strat = &strategies[attempt];

        {
            let mut s = ui_session.lock().unwrap();
            s.strategy_name = strat.name.clone();
            s.strategy_attempt = (attempt + 1) as u32;
            s.last_log_line = None;
        }

        // Announce strategy
        let _ = app.emit("strategy-changed", json!({
            "attempt":     attempt + 1,
            "name":        strat.name,
            "description": strat.description,
        }));
        if attempt > 0 {
            emit_event(
                &app,
                &make_event(
                    EventType::Retrying,
                    EventCategory::Warn,
                    "Retrying download",
                    Some(format!("Attempt {} → {}", attempt + 1, strat.name)),
                    None,
                    Some(LOG_GROUP.into()),
                ),
            );
        }
        emit_status(&app, &format!("[Attempt {}]  {} — connecting…", attempt + 1, strat.name), "dl");
        emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "connecting");
        classifier.reset();

        // Build arg list: base + strategy extras + destination + url
        let mut args: Vec<String> = BASE_ARGS.iter().map(|s| s.to_string()).collect();
        args.extend(strat.args.clone());
        args.extend(["-d".into(), dir.clone(), url.clone()]);

        // Launch subprocess
        #[allow(unused_mut)]
        let mut cmd = Command::new(&aria2_path);
        cmd.args(&args)
           .stdout(Stdio::piped())
           .stderr(Stdio::piped());

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "error");
                emit_event(
                    &app,
                    &make_event(
                        EventType::DownloadFailed,
                        EventCategory::Error,
                        "Failed to launch aria2",
                        Some(e.to_string()),
                        None,
                        Some(LOG_GROUP.into()),
                    ),
                );
                emit_log(&app, &format!("[FATAL] Failed to launch aria2: {e}"), "err");
                emit_status(&app, "Launch failed", "err");
                let _ = app.emit("finished", false);
                running.store(false, Ordering::SeqCst);
                return;
            }
        };

        // Store child handle so stop_download() can kill it
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        *child_slot.lock().unwrap() = Some(child);

        // Merge stdout + stderr — aria2 may emit summaries on either stream depending on build/options.
        let (tx, rx) = mpsc::channel::<String>();
        let tx_out = tx.clone();
        thread::spawn(move || {
            let r = BufReader::new(stdout);
            for raw in r.lines().flatten() {
                if tx_out.send(raw).is_err() {
                    break;
                }
            }
        });
        let tx_err = tx.clone();
        thread::spawn(move || {
            let r = BufReader::new(stderr);
            for raw in r.lines().flatten() {
                if tx_err.send(raw).is_err() {
                    break;
                }
            }
        });
        drop(tx);

        for raw in rx {
            if abort.load(Ordering::SeqCst) {
                break;
            }
            let clean = ANSI_RE.replace_all(&raw, "").to_string();
            let clean = clean.trim().to_string();
            if clean.is_empty() {
                continue;
            }

            classifier.feed(&clean);
            dispatch_line(&app, &clean, &ui_session);
        }

        // Wait for process to exit
        let exit_code = {
            let mut guard = child_slot.lock().unwrap();
            if let Some(ref mut c) = *guard {
                c.wait().ok().and_then(|s| s.code()).unwrap_or(1)
            } else {
                1 // was killed
            }
        };
        *child_slot.lock().unwrap() = None;

        // User cancelled
        if abort.load(Ordering::SeqCst) {
            emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "cancelled");
            emit_status(&app, "Download cancelled", "err");
            let _ = app.emit("finished", false);
            running.store(false, Ordering::SeqCst);
            return;
        }

        // Success
        if exit_code == 0 {
            emit_transfer_update(
                &app,
                &ui_session,
                100,
                Some("—"),
                Some("—"),
                None,
                None,
                "complete",
            );
            let fname = ui_session.lock().ok().and_then(|g| g.filename.clone());
            emit_event(
                &app,
                &make_event(
                    EventType::DownloadCompleted,
                    EventCategory::Success,
                    "Download completed",
                    fname.map(|f| format!("File: {f}")),
                    None,
                    Some(LOG_GROUP.into()),
                ),
            );
            emit_status(&app, "Download complete ✓", "ok");
            let _ = app.emit("finished", true);
            running.store(false, Ordering::SeqCst);
            return;
        }

        // Failure — classify and decide next strategy
        let error     = classifier.classify();
        let preferred = error.preferred_strategy();

        emit_log(
            &app,
            &format!("\n[DIAGNOSE] error={} → preferred strategy index={preferred}", error.as_str()),
            "warn",
        );

        let next_attempt = if preferred > attempt { preferred } else { attempt + 1 };

        if next_attempt >= strategies.len() {
            emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "error");
            emit_event(
                &app,
                &make_event(
                    EventType::DownloadFailed,
                    EventCategory::Error,
                    "All strategies failed",
                    Some(format!("error={} (attempts exhausted)", error.as_str())),
                    None,
                    Some(LOG_GROUP.into()),
                ),
            );
            emit_log(&app, "[GIVE UP] All strategies exhausted.", "err");
            emit_status(&app, "All strategies failed", "err");
            let _ = app.emit("finished", false);
            running.store(false, Ordering::SeqCst);
            return;
        }

        attempt = next_attempt;
        let next = &strategies[attempt];
        emit_log(&app, &format!("[RETRY] → {}: {}", next.name, next.description), "retry");
        emit_status(&app, &format!("Retrying with: {}…", next.name), "retry");
        // loop continues with new attempt index
    }

    // Aborted from outer loop check
    emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "cancelled");
    emit_status(&app, "Download cancelled", "err");
    let _ = app.emit("finished", false);
    running.store(false, Ordering::SeqCst);
}

// ── Line dispatcher ───────────────────────────────────────────────────────────

fn dispatch_line(app: &AppHandle, line: &str, ui: &Arc<Mutex<UiSession>>) {
    let clean = line.trim();
    if clean.is_empty() {
        return;
    }

    if let Some(caps) = SUMMARY_RE.captures(clean) {
        let pct = caps
            .name("pct")
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .unwrap_or(0);
        let dl = caps.name("dl").map(|m| m.as_str());
        let total = caps.name("total").map(|m| m.as_str());
        let spd = caps.name("spd").map(|m| m.as_str());
        let eta = caps.name("eta").map(|m| m.as_str());
        let cn = caps
            .name("cn")
            .and_then(|m| m.as_str().parse::<u32>().ok());
        emit_transfer_update(app, ui, pct, spd, eta, dl, total, "downloading");
        emit_status(app, &format!("Downloading… {pct}%"), "dl");
        emit_progress_events(app, ui, pct, spd, eta, cn);
        return;
    }

    if let Some(caps) = SUMMARY_MIN_RE.captures(clean) {
        let pct = caps
            .name("pct")
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .unwrap_or(0);
        let dl = caps.name("dl").map(|m| m.as_str());
        let total = caps.name("total").map(|m| m.as_str());
        let cn = caps
            .name("cn")
            .and_then(|m| m.as_str().parse::<u32>().ok());
        emit_transfer_update(app, ui, pct, None, None, dl, total, "downloading");
        emit_status(app, &format!("Downloading… {pct}%"), "dl");
        emit_progress_events(app, ui, pct, None, None, cn);
        return;
    }

    if let Some(caps) = DOWNLOAD_COMPLETE_RE.captures(clean) {
        if let Some(p) = caps.name("path") {
            if let Some(base) = basename_from_path(p.as_str()) {
                let mut should_emit = None;
                {
                    let mut g = ui.lock().unwrap();
                    g.filename = Some(base);
                    if g.event_state.last_filename != g.filename {
                        g.event_state.last_filename = g.filename.clone();
                        should_emit = g.filename.clone();
                    }
                }
                if let Some(name) = should_emit {
                    emit_event(
                        app,
                        &make_event(
                            EventType::MetadataUpdated,
                            EventCategory::Info,
                            "Metadata updated",
                            Some(format!("File: {name}")),
                            None,
                            Some(LOG_GROUP.into()),
                        ),
                    );
                }
            }
        }
        return;
    }

    if clean.contains("[#") || clean.contains("DL:") {
        if let Some(caps) = PCT_FALLBACK_RE.captures(clean) {
            if let Ok(pct) = caps[1].parse::<u32>() {
                emit_transfer_update(app, ui, pct, None, None, None, None, "downloading");
                emit_status(app, &format!("Downloading… {pct}%"), "dl");
                emit_progress_events(app, ui, pct, None, None, None);
                return;
            }
        }
    }

    let ll = clean.to_lowercase();
    let kind = if ll.contains("error") || ll.contains("failed") || ll.contains("abort") {
        "err"
    } else if ll.contains("download complete") || ll.contains("100%") {
        "ok"
    } else if ll.contains("notice") || ll.contains("warn") {
        "warn"
    } else {
        "base"
    };

    // Drop repetitive progress-looking noise from the log panel
    if clean.starts_with("[#") && clean.contains('%') {
        return;
    }

    if kind == "base" && clean.len() > 240 {
        return;
    }

    {
        let mut g = ui.lock().unwrap();
        if g.last_log_line.as_deref() == Some(clean) {
            return;
        }
        g.last_log_line = Some(clean.to_string());
    }

    emit_log(app, clean, kind);
}

// ── Tauri commands ────────────────────────────────────────────────────────────

#[tauri::command]
pub fn start_download(
    app:   AppHandle,
    state: tauri::State<'_, AppState>,
    url:   String,
    dir:   String,
) -> Result<(), String> {
    if state.running.load(Ordering::SeqCst) {
        return Err("A download is already running".into());
    }

    let fname_hint = filename_hint_from_url(&url);
    {
        let mut g = state.ui_session.lock().unwrap();
        *g = UiSession::default();
        g.filename = fname_hint.clone();
        g.event_state.last_filename = fname_hint.clone();
    }

    emit_event(
        &app,
        &make_event(
            EventType::DownloadStarted,
            EventCategory::Info,
            "Download started",
            Some(format!("URL: {url}")),
            None,
            Some(LOG_GROUP.into()),
        ),
    );
    if let Some(fname) = fname_hint {
        emit_event(
            &app,
            &make_event(
                EventType::MetadataUpdated,
                EventCategory::Info,
                "Metadata updated",
                Some(format!("File: {fname}")),
                None,
                Some(LOG_GROUP.into()),
            ),
        );
    }

    state.abort.store(false, Ordering::SeqCst);
    state.running.store(true, Ordering::SeqCst);

    let abort       = state.abort.clone();
    let child       = state.child.clone();
    let running     = state.running.clone();
    let ui_session  = state.ui_session.clone();

    std::thread::spawn(move || {
        run_download_loop(app, url, dir, abort, child, running, ui_session);
    });

    Ok(())
}

#[tauri::command]
pub fn stop_download(state: tauri::State<'_, AppState>) {
    state.abort.store(true, Ordering::SeqCst);
    if let Ok(mut guard) = state.child.lock() {
        if let Some(ref mut c) = *guard {
            let _ = c.kill();
        }
    }
}

#[tauri::command]
pub fn check_aria2(app: AppHandle) -> bool {
    resolve_aria2_path(&app).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_regex_matches_typical_aria2_console_line() {
        let line = "[#7ef40d 2.0MiB/5.0MiB(40%) CN:1 DL:2.5MiB ETA:2s]";
        let caps = SUMMARY_RE.captures(line).expect("summary line must parse");
        assert_eq!(caps.name("pct").unwrap().as_str(), "40");
        assert_eq!(caps.name("cn").unwrap().as_str(), "1");
        assert_eq!(caps.name("spd").unwrap().as_str(), "2.5MiB");
        assert_eq!(caps.name("eta").unwrap().as_str(), "2s");
    }

    #[test]
    fn summary_min_without_dl_eta_still_yields_pct() {
        let line = "[#b08aa7 0B/0B(100%) CN:1]";
        let caps = SUMMARY_MIN_RE.captures(line).expect("min summary");
        assert_eq!(caps.name("pct").unwrap().as_str(), "100");
        assert_eq!(caps.name("cn").unwrap().as_str(), "1");
    }
}
