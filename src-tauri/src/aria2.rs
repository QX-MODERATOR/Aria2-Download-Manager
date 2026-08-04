//! Direct aria2 process runner and Tauri commands.
//!
//! This module intentionally keeps the aria2 path simple: spawn aria2c once per
//! strategy, parse its console progress, and verify that the final file exists
//! with non-zero size before reporting success.

use crate::classifier::{ErrorClassifier, ErrorCode};
use crate::strategy::{build_strategies, Strategy};
use crate::yt_dlp;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    env,
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Manager};

const BASE_ARGS: &[&str] = &[
    "--continue=true",
    "--max-tries=5",
    "--retry-wait=2",
    "--timeout=60",
    "--connect-timeout=30",
    "--summary-interval=1",
    "--console-log-level=notice",
    "--show-console-readout=true",
    "--file-allocation=none",
    "--auto-file-renaming=false",
    "--allow-overwrite=true",
];

const MAX_DIAGNOSTIC_CHARS: usize = 1_200;
pub const LOG_GROUP: &str = "Download Session";

static ANSI_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\x1b\[[0-9;]*[mGKHF]|\x1b\][^\x07]*\x07").unwrap());
static PROGRESS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[#(?P<gid>\S+)\s+(?P<body>[^\]]+)\]").unwrap());
static SIZE_PAIR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?P<done>[\d.]+\s*(?:[KMGTP]?i?B|[KMGTP]?B|B))(?:/(?P<total>[\d.]+\s*(?:[KMGTP]?i?B|[KMGTP]?B|B)|\?+))?(?:\((?P<pct>[\d.]+)%\))?",
    )
    .unwrap()
});
static SPEED_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bDL:(?P<speed>[\d.]+\s*(?:[KMGTP]?i?B|[KMGTP]?B|B))").unwrap());
static ETA_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bETA:(?P<eta>[^\s\]]+)").unwrap());
static COMPLETE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)download complete:\s*(?P<path>.+)$").unwrap());

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
    #[serde(skip_serializing_if = "Option::is_none", rename = "downloadId")]
    pub download_id: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Default, Clone)]
pub struct EventState {
    pub last_filename: Option<String>,
    pub last_phase: Option<String>,
    pub last_percent: Option<u32>,
}

#[allow(dead_code)]
pub struct UiSession {
    pub engine: String,
    pub strategy_name: String,
    pub strategy_attempt: u32,
    pub filename: Option<String>,
    pub last_log_line: Option<String>,
    pub event_state: EventState,
    pub download_id: String,
}

impl Default for UiSession {
    fn default() -> Self {
        Self {
            engine: "aria2".into(),
            strategy_name: "Default".into(),
            strategy_attempt: 1,
            filename: None,
            last_log_line: None,
            event_state: EventState::default(),
            download_id: String::new(),
        }
    }
}

pub struct AppState {
    pub abort: Arc<AtomicBool>,
    pub running: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    pub child: Arc<Mutex<Option<std::process::Child>>>,
    pub ui_session: Arc<Mutex<UiSession>>,
    pub paused_session: Arc<Mutex<Option<PausedSession>>>,
}

#[derive(Clone)]
pub struct PausedSession {
    pub url: String,
    pub dir: String,
    pub engine: String,
    pub quality: Option<String>,
    pub playlist: Option<bool>,
    pub custom_filename: Option<String>,
    pub browser_cookie_source: Option<String>,
    pub download_id: String,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            abort: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            child: Arc::new(Mutex::new(None)),
            ui_session: Arc::new(Mutex::new(UiSession::default())),
            paused_session: Arc::new(Mutex::new(None)),
        }
    }
}

#[derive(Debug, Clone)]
struct Aria2Runtime {
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct Aria2ResolutionError {
    message: String,
    diagnostics: Vec<String>,
}

#[derive(Debug, Clone)]
struct FileSnapshot {
    len: u64,
    modified: SystemTime,
}

#[derive(Debug, Clone)]
struct ProgressSnapshot {
    pct: u32,
    downloaded: String,
    total: String,
    speed: String,
    eta: String,
}

impl Default for ProgressSnapshot {
    fn default() -> Self {
        Self {
            pct: 0,
            downloaded: "-".into(),
            total: "-".into(),
            speed: "-".into(),
            eta: "-".into(),
        }
    }
}

#[derive(Debug)]
enum AttemptResult {
    Completed {
        path: PathBuf,
        file_name: String,
        bytes: u64,
    },
    Failed {
        detail: String,
    },
    Paused,
    Cancelled,
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn ts_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}

pub fn make_event(
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
        download_id: None,
    }
}

fn current_download_id(app: &AppHandle) -> String {
    app.state::<AppState>()
        .ui_session
        .lock()
        .ok()
        .map(|g| g.download_id.clone())
        .unwrap_or_default()
}

pub fn emit_event(app: &AppHandle, payload: &EventPayload) {
    let mut payload = payload.clone();
    if payload.download_id.is_none() {
        payload.download_id = Some(current_download_id(app));
    }
    let _ = app.emit("event-log", payload);
}

pub fn emit_status(app: &AppHandle, text: &str, kind: &str) {
    let _ = app.emit(
        "status-msg",
        json!({
            "text": text,
            "kind": kind,
            "downloadId": current_download_id(app)
        }),
    );
}

pub fn emit_transfer_update(
    app: &AppHandle,
    ui: &Arc<Mutex<UiSession>>,
    pct: u32,
    speed: Option<&str>,
    eta: Option<&str>,
    downloaded: Option<&str>,
    total: Option<&str>,
    phase: &str,
) {
    let (engine, strategy_name, attempt, filename, download_id) = {
        let g = ui.lock().unwrap();
        (
            g.engine.clone(),
            g.strategy_name.clone(),
            g.strategy_attempt,
            g.filename.clone(),
            g.download_id.clone(),
        )
    };
    let total_s = total.unwrap_or("-");
    let total_known = total_is_known(total_s);
    let _ = app.emit(
        "transfer-update",
        json!({
            "downloadId": download_id,
            "pct": pct.min(100),
            "speed": speed.unwrap_or("-"),
            "eta": eta.unwrap_or("-"),
            "downloaded": downloaded.unwrap_or("-"),
            "total": total_s,
            "totalKnown": total_known,
            "phase": phase,
            "strategyName": strategy_name,
            "strategyAttempt": attempt,
            "engine": engine,
            "filename": filename,
        }),
    );
}

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
        all(
            target_os = "windows",
            target_arch = "x86_64",
            any(target_env = "msvc", target_env = "gnu")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
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

fn bundled_aria2_candidates(app: &AppHandle) -> Vec<PathBuf> {
    let mut roots = Vec::<PathBuf>::new();

    if let Ok(dir) = app.path().resource_dir() {
        roots.push(dir);
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
            roots.push(dir.join("bin"));
        }
    }
    if let Ok(cwd) = env::current_dir() {
        roots.push(cwd.clone());
        roots.push(cwd.join("bin"));
        roots.push(cwd.join("src-tauri").join("bin"));
        roots.push(cwd.join("..").join("src-tauri").join("bin"));
    }

    let mut out = Vec::new();
    for root in roots {
        out.extend(executable_candidates(&root).filter(|p| p.is_file()));
    }
    out.sort();
    out.dedup();
    out
}

fn trim_diagnostic(text: &str) -> String {
    let clean = text.trim();
    if clean.len() <= MAX_DIAGNOSTIC_CHARS {
        return clean.to_string();
    }
    let boundary = clean
        .char_indices()
        .take_while(|(i, _)| *i < MAX_DIAGNOSTIC_CHARS)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}...", &clean[..boundary])
}

fn output_text(output: &Output) -> String {
    trim_diagnostic(&format!(
        "exit={}; stdout={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn validate_aria2_candidate(path: PathBuf, source: &'static str) -> Result<Aria2Runtime, String> {
    if !path.is_file() {
        return Err(format!(
            "{source} candidate is not a file: {}",
            path.display()
        ));
    }
    let mut cmd = Command::new(&path);
    cmd.arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process_util::hide_console(&mut cmd);

    match cmd.output() {
        Ok(output) if output.status.success() => Ok(Aria2Runtime { path }),
        Ok(output) => Err(format!(
            "aria2c validation failed at {}: {}",
            path.display(),
            output_text(&output)
        )),
        Err(err) => Err(format!(
            "aria2c failed to start at {}: {err}",
            path.display()
        )),
    }
}

fn resolve_aria2(app: &AppHandle) -> Result<Aria2Runtime, Aria2ResolutionError> {
    let mut diagnostics = Vec::new();

    for path in bundled_aria2_candidates(app) {
        match validate_aria2_candidate(path, "bundled sidecar") {
            Ok(runtime) => return Ok(runtime),
            Err(err) => diagnostics.push(err),
        }
    }

    if let Some(path) = find_on_path(aria2_binary_name()) {
        match validate_aria2_candidate(path, "system PATH") {
            Ok(runtime) => return Ok(runtime),
            Err(err) => diagnostics.push(err),
        }
    } else {
        diagnostics.push(format!("{} not found on PATH", aria2_binary_name()));
    }

    Err(Aria2ResolutionError {
        message: format!("{} not found or not usable.", aria2_binary_name()),
        diagnostics,
    })
}

pub(crate) fn resolve_aria2_path(app: &AppHandle) -> Option<PathBuf> {
    resolve_aria2(app).ok().map(|runtime| runtime.path)
}

fn validate_output_dir(dir: &str) -> Result<(), String> {
    let path = Path::new(dir);
    if !path.is_dir() {
        return Err(format!(
            "Download folder does not exist: {}",
            path.display()
        ));
    }
    let test_path = path.join(format!(".aria2-manager-write-test-{}", now_ms()));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&test_path)
    {
        Ok(_) => {
            let _ = fs::remove_file(&test_path);
            Ok(())
        }
        Err(err) => Err(format!("Download folder is not writable: {err}")),
    }
}

fn filename_hint_from_url(url: &str) -> Option<String> {
    let path = url.split('?').next()?.trim_end_matches('/');
    let leaf = path.rsplit('/').next()?.trim();
    if leaf.is_empty() || leaf.contains("://") || leaf.contains(':') {
        return None;
    }
    Some(leaf.to_string())
}

fn file_name_from_path(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("download")
        .to_string()
}

fn known_target_path(dir: &str, url: &str, custom_filename: Option<&str>) -> Option<PathBuf> {
    let name = custom_filename
        .map(str::to_string)
        .or_else(|| filename_hint_from_url(url))?;
    Some(Path::new(dir).join(name))
}

fn remove_stale_zero_target(path: &Path) {
    if path
        .metadata()
        .map(|m| m.is_file() && m.len() == 0)
        .unwrap_or(false)
    {
        let _ = fs::remove_file(path);
    }
    let aria2 = PathBuf::from(format!("{}.aria2", path.to_string_lossy()));
    if aria2
        .metadata()
        .map(|m| m.is_file() && m.len() == 0)
        .unwrap_or(false)
    {
        let _ = fs::remove_file(aria2);
    }
}

fn snapshot_dir(dir: &Path) -> HashMap<PathBuf, FileSnapshot> {
    let mut map = HashMap::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return map;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || is_control_file(&path) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            map.insert(
                path,
                FileSnapshot {
                    len: meta.len(),
                    modified: meta.modified().unwrap_or(UNIX_EPOCH),
                },
            );
        }
    }
    map
}

fn is_control_file(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("aria2"))
        .unwrap_or(false)
}

fn newest_changed_file(
    dir: &Path,
    before: &HashMap<PathBuf, FileSnapshot>,
    started_at: SystemTime,
) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || is_control_file(&path) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let len = meta.len();
        if len == 0 {
            continue;
        }
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        let changed = before
            .get(&path)
            .map(|old| old.len != len || old.modified < modified)
            .unwrap_or(true);
        if changed || modified >= started_at {
            candidates.push((path, modified, len));
        }
    }
    candidates.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)));
    candidates.pop().map(|(path, _, _)| path)
}

fn parse_bytes(value: &str) -> Option<u64> {
    let compact = value.trim().replace(' ', "");
    let caps = Regex::new(r"(?i)^([\d.]+)([KMGTP]?i?B|[KMGTP]?B|B)$")
        .ok()?
        .captures(&compact)?;
    let n: f64 = caps.get(1)?.as_str().parse().ok()?;
    let unit = caps.get(2)?.as_str().to_ascii_lowercase();
    let factor = match unit.as_str() {
        "b" => 1.0,
        "kb" => 1_000.0,
        "kib" => 1_024.0,
        "mb" => 1_000_000.0,
        "mib" => 1_048_576.0,
        "gb" => 1_000_000_000.0,
        "gib" => 1_073_741_824.0,
        "tb" => 1_000_000_000_000.0,
        "tib" => 1_099_511_627_776.0,
        "pb" => 1_000_000_000_000_000.0,
        "pib" => 1_125_899_906_842_624.0,
        _ => 1.0,
    };
    Some((n * factor).round() as u64)
}

fn format_bytes(n: u64) -> String {
    const KI: u64 = 1_024;
    const MI: u64 = KI * 1_024;
    const GI: u64 = MI * 1_024;
    const TI: u64 = GI * 1_024;
    if n >= TI {
        format!("{:.2}TiB", n as f64 / TI as f64)
    } else if n >= GI {
        format!("{:.2}GiB", n as f64 / GI as f64)
    } else if n >= MI {
        format!("{:.2}MiB", n as f64 / MI as f64)
    } else if n >= KI {
        format!("{:.2}KiB", n as f64 / KI as f64)
    } else {
        format!("{n}B")
    }
}

fn total_is_known(total: &str) -> bool {
    let t = total.trim();
    !t.is_empty() && t != "-" && t != "?" && !t.contains('?')
}

fn parse_progress_line(line: &str, last: &ProgressSnapshot) -> Option<ProgressSnapshot> {
    let progress = PROGRESS_RE.captures(line)?;
    let body = progress.name("body")?.as_str();
    let sizes = SIZE_PAIR_RE.captures(body)?;
    let downloaded = sizes.name("done")?.as_str().replace(' ', "");
    let total = sizes
        .name("total")
        .map(|m| m.as_str().replace(' ', ""))
        .unwrap_or_else(|| last.total.clone());
    let pct = sizes
        .name("pct")
        .and_then(|m| m.as_str().parse::<f64>().ok())
        .map(|n| n.round() as u32)
        .or_else(|| {
            let done = parse_bytes(&downloaded)?;
            let total = parse_bytes(&total)?;
            if total == 0 {
                return None;
            }
            Some(((done as f64 / total as f64) * 100.0).round() as u32)
        })
        .unwrap_or(last.pct)
        .min(100);
    let speed = SPEED_RE
        .captures(body)
        .and_then(|c| c.name("speed"))
        .map(|m| {
            let s = m.as_str().replace(' ', "");
            if s.ends_with("/s") {
                s
            } else {
                format!("{s}/s")
            }
        })
        .unwrap_or_else(|| last.speed.clone());
    let eta = ETA_RE
        .captures(body)
        .and_then(|c| c.name("eta"))
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| last.eta.clone());

    Some(ProgressSnapshot {
        pct,
        downloaded,
        total,
        speed,
        eta,
    })
}

fn clean_output_line(raw: &str) -> String {
    ANSI_RE.replace_all(raw, "").trim().to_string()
}

fn parse_completed_path(line: &str) -> Option<PathBuf> {
    if let Some(caps) = COMPLETE_RE.captures(line) {
        let p = caps.name("path")?.as_str().trim().trim_matches('"');
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }

    if line.contains("|OK|") || line.contains("|OK ") {
        let parts = line.split('|').map(str::trim).collect::<Vec<_>>();
        if let Some(path) = parts.last().filter(|p| !p.is_empty()) {
            return Some(PathBuf::from(path));
        }
    }

    None
}

fn failure_summary(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    if lower.contains("status=403") || lower.contains(" 403") {
        Some("HTTP 403 forbidden".into())
    } else if lower.contains("status=404") || lower.contains(" 404") || lower.contains("not found")
    {
        Some("HTTP 404 not found".into())
    } else if lower.contains("certificate") || lower.contains("tls") || lower.contains("ssl") {
        Some("TLS certificate error".into())
    } else if lower.contains("timed out") || lower.contains("timeout") {
        Some("network timeout".into())
    } else if lower.contains("error") || lower.contains("failed") {
        Some(trim_diagnostic(line))
    } else {
        None
    }
}

fn build_failure_detail(
    code: &ErrorCode,
    exit_code: Option<i32>,
    lines: &VecDeque<String>,
) -> String {
    for line in lines.iter().rev() {
        if let Some(summary) = failure_summary(line) {
            return match exit_code {
                Some(code) => format!("{summary}; exit code {code}"),
                None => summary,
            };
        }
    }
    let recent = lines
        .iter()
        .rev()
        .take(4)
        .cloned()
        .collect::<Vec<_>>()
        .join(" | ");
    match (exit_code, recent.is_empty()) {
        (Some(exit), true) => format!("aria2 failed; error={}; exit code {exit}", code.as_str()),
        (Some(exit), false) => format!(
            "aria2 failed; error={}; exit code {exit}; recent output: {}",
            code.as_str(),
            trim_diagnostic(&recent)
        ),
        (None, true) => format!("aria2 failed; error={}", code.as_str()),
        (None, false) => format!(
            "aria2 failed; error={}; recent output: {}",
            code.as_str(),
            trim_diagnostic(&recent)
        ),
    }
}

fn remember_line(lines: &mut VecDeque<String>, line: String) {
    if line.is_empty() {
        return;
    }
    lines.push_back(line);
    while lines.len() > 60 {
        lines.pop_front();
    }
}

fn read_process_output<R: Read + Send + 'static>(
    mut reader: R,
    tx: mpsc::Sender<(bool, String)>,
    is_stderr: bool,
) {
    let mut pending = Vec::<u8>::new();
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for &byte in &buf[..n] {
                    if byte == b'\n' || byte == b'\r' {
                        if !pending.is_empty() {
                            let line = String::from_utf8_lossy(&pending).to_string();
                            pending.clear();
                            if tx.send((is_stderr, line)).is_err() {
                                return;
                            }
                        }
                    } else {
                        pending.push(byte);
                    }
                }
            }
            Err(_) => break,
        }
    }
    if !pending.is_empty() {
        let _ = tx.send((is_stderr, String::from_utf8_lossy(&pending).to_string()));
    }
}

fn kill_child_tree(child: &mut std::process::Child) {
    let pid = child.id();
    #[cfg(target_os = "windows")]
    {
        let mut kill_cmd = Command::new("taskkill");
        kill_cmd
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::process_util::hide_console(&mut kill_cmd);
        let _ = kill_cmd.status();
    }
    let _ = child.kill();
}

pub fn shutdown_active_download(state: &AppState) {
    state.abort.store(true, Ordering::SeqCst);
    state.paused.store(false, Ordering::SeqCst);
    if let Ok(mut ps) = state.paused_session.lock() {
        *ps = None;
    }
    if let Ok(mut guard) = state.child.lock() {
        if let Some(child) = guard.as_mut() {
            kill_child_tree(child);
        }
    }
}

fn clear_child_slot(child_slot: &Arc<Mutex<Option<std::process::Child>>>) {
    if let Ok(mut guard) = child_slot.lock() {
        *guard = None;
    }
}

fn wait_for_child_exit(
    child_slot: &Arc<Mutex<Option<std::process::Child>>>,
) -> Option<std::process::ExitStatus> {
    loop {
        let maybe_status = {
            let mut guard = child_slot.lock().ok()?;
            let child = guard.as_mut()?;
            match child.try_wait() {
                Ok(Some(status)) => Some(status),
                Ok(None) => None,
                Err(_) => return None,
            }
        };
        if maybe_status.is_some() {
            return maybe_status;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn emit_strategy(app: &AppHandle, ui: &Arc<Mutex<UiSession>>, attempt: usize, strategy: &Strategy) {
    {
        let mut g = ui.lock().unwrap();
        g.strategy_name = strategy.name.clone();
        g.strategy_attempt = (attempt + 1) as u32;
    }
    let _ = app.emit(
        "strategy-changed",
        json!({
            "attempt": attempt + 1,
            "name": strategy.name,
            "description": strategy.description,
            "engine": "aria2",
        }),
    );
    emit_event(
        app,
        &make_event(
            EventType::MetadataUpdated,
            EventCategory::Info,
            format!("Using {}", strategy.name),
            Some(strategy.description.clone()),
            Some("aria2-strategy".into()),
            Some(LOG_GROUP.into()),
        ),
    );
}

fn aria2_command_args(
    url: &str,
    dir: &str,
    custom_filename: Option<&str>,
    strategy: &Strategy,
) -> Vec<String> {
    let mut args = BASE_ARGS.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    args.extend(strategy.args.clone());
    args.push(format!("--dir={dir}"));
    if let Some(name) = custom_filename.filter(|s| !s.trim().is_empty()) {
        args.push(format!("--out={}", name.trim()));
    }
    args.push(url.to_string());
    args
}

fn run_aria2_attempt(
    app: &AppHandle,
    aria2_path: &Path,
    url: &str,
    dir: &str,
    custom_filename: Option<&str>,
    strategy: &Strategy,
    abort: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
    child_slot: &Arc<Mutex<Option<std::process::Child>>>,
    ui_session: &Arc<Mutex<UiSession>>,
    before: &HashMap<PathBuf, FileSnapshot>,
    started_at: SystemTime,
) -> AttemptResult {
    let args = aria2_command_args(url, dir, custom_filename, strategy);

    emit_status(app, "Connecting...", "dl");
    emit_transfer_update(app, ui_session, 0, None, None, None, None, "connecting");

    let mut cmd = Command::new(aria2_path);
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process_util::hide_console(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return AttemptResult::Failed {
                detail: format!("Failed to launch aria2: {err}"),
            };
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    {
        let mut guard = child_slot.lock().unwrap();
        *guard = Some(child);
    }

    let (tx, rx) = mpsc::channel::<(bool, String)>();
    if let Some(stdout) = stdout {
        let tx = tx.clone();
        thread::spawn(move || read_process_output(stdout, tx, false));
    }
    if let Some(stderr) = stderr {
        let tx = tx.clone();
        thread::spawn(move || read_process_output(stderr, tx, true));
    }
    drop(tx);

    let mut lines = VecDeque::new();
    let mut classifier = ErrorClassifier::new();
    let mut progress = ProgressSnapshot::default();
    let mut last_emit = Instant::now() - Duration::from_secs(5);
    let mut parsed_complete_path: Option<PathBuf> = None;
    let exit_status;

    loop {
        if abort.load(Ordering::SeqCst) {
            if let Ok(mut guard) = child_slot.lock() {
                if let Some(child) = guard.as_mut() {
                    kill_child_tree(child);
                }
            }
            let _ = wait_for_child_exit(child_slot);
            clear_child_slot(child_slot);
            return if paused.load(Ordering::SeqCst) {
                AttemptResult::Paused
            } else {
                AttemptResult::Cancelled
            };
        }

        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok((_is_stderr, raw)) => {
                let line = clean_output_line(&raw);
                if line.is_empty() {
                    continue;
                }
                classifier.feed(&line);
                remember_line(&mut lines, line.clone());

                if let Some(path) = parse_completed_path(&line) {
                    parsed_complete_path = Some(path);
                }

                if let Some(next) = parse_progress_line(&line, &progress) {
                    progress = next;
                    if last_emit.elapsed() >= Duration::from_millis(250) {
                        emit_transfer_update(
                            app,
                            ui_session,
                            progress.pct,
                            Some(&progress.speed),
                            Some(&progress.eta),
                            Some(&progress.downloaded),
                            Some(&progress.total),
                            "downloading",
                        );
                        emit_status(app, &format!("Downloading... {}%", progress.pct), "dl");
                        last_emit = Instant::now();
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }

        let status = {
            let mut guard = match child_slot.lock() {
                Ok(guard) => guard,
                Err(_) => return AttemptResult::Cancelled,
            };
            let Some(child) = guard.as_mut() else {
                return AttemptResult::Cancelled;
            };
            match child.try_wait() {
                Ok(Some(status)) => Some(status),
                Ok(None) => None,
                Err(err) => {
                    return AttemptResult::Failed {
                        detail: format!("Could not read aria2 process status: {err}"),
                    };
                }
            }
        };

        if let Some(status) = status {
            exit_status = Some(status);
            break;
        }
    }

    while let Ok((_is_stderr, raw)) = rx.try_recv() {
        let line = clean_output_line(&raw);
        if line.is_empty() {
            continue;
        }
        classifier.feed(&line);
        remember_line(&mut lines, line.clone());
        if let Some(path) = parse_completed_path(&line) {
            parsed_complete_path = Some(path);
        }
        if let Some(next) = parse_progress_line(&line, &progress) {
            progress = next;
        }
    }

    clear_child_slot(child_slot);
    let status = exit_status;
    let success = status.map(|s| s.success()).unwrap_or(false);

    if success {
        emit_transfer_update(
            app,
            ui_session,
            100,
            Some("Completed"),
            Some("Done"),
            Some(&progress.downloaded),
            Some(&progress.total),
            "completed",
        );

        let dir_path = Path::new(dir);
        let expected = known_target_path(dir, url, custom_filename);
        let path = parsed_complete_path
            .filter(|p| p.is_file())
            .or_else(|| expected.filter(|p| p.is_file()))
            .or_else(|| newest_changed_file(dir_path, before, started_at));

        let Some(path) = path else {
            return AttemptResult::Failed {
                detail: "aria2 exited successfully but no output file was found".into(),
            };
        };

        let bytes = path.metadata().map(|m| m.len()).unwrap_or(0);
        if bytes == 0 {
            return AttemptResult::Failed {
                detail: format!("aria2 created a 0B file: {}", path.display()),
            };
        }
        return AttemptResult::Completed {
            file_name: file_name_from_path(&path),
            path,
            bytes,
        };
    }

    let code = classifier.classify();
    let exit_code = status.and_then(|s| s.code());
    AttemptResult::Failed {
        detail: build_failure_detail(&code, exit_code, &lines),
    }
}

fn emit_finished(
    app: &AppHandle,
    download_id: &str,
    success: bool,
    status: &str,
    message: &str,
    file_path: Option<&Path>,
    file_name: Option<&str>,
    bytes: Option<u64>,
) {
    let _ = app.emit(
        "finished",
        json!({
            "downloadId": download_id,
            "success": success,
            "status": status,
            "engine": "aria2",
            "message": message,
            "filePath": file_path.map(|p| p.to_string_lossy().to_string()),
            "fileName": file_name,
            "downloadedBytes": bytes,
            "totalBytes": bytes,
        }),
    );
}

fn clear_paused_session_if_current(app: &AppHandle, download_id: &str) {
    let state = app.state::<AppState>();
    let paused_session = state.paused_session.clone();
    if let Ok(mut guard) = paused_session.lock() {
        if guard
            .as_ref()
            .map(|s| s.download_id == download_id)
            .unwrap_or(false)
        {
            *guard = None;
        }
    };
}

#[allow(clippy::too_many_arguments)]
fn run_download_loop(
    app: AppHandle,
    url: String,
    dir: String,
    custom_filename: Option<String>,
    resume_existing: bool,
    abort: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    child_slot: Arc<Mutex<Option<std::process::Child>>>,
    running: Arc<AtomicBool>,
    ui_session: Arc<Mutex<UiSession>>,
) {
    let download_id = ui_session.lock().unwrap().download_id.clone();
    let finish = |_app: &AppHandle, running: &Arc<AtomicBool>| {
        running.store(false, Ordering::SeqCst);
    };

    let runtime = match resolve_aria2(&app) {
        Ok(runtime) => runtime,
        Err(err) => {
            let detail = if err.diagnostics.is_empty() {
                err.message
            } else {
                format!(
                    "{} Diagnostics: {}",
                    err.message,
                    err.diagnostics.join(" | ")
                )
            };
            emit_status(&app, "aria2 not found", "err");
            emit_event(
                &app,
                &make_event(
                    EventType::DownloadFailed,
                    EventCategory::Error,
                    "aria2 missing",
                    Some(detail.clone()),
                    None,
                    Some(LOG_GROUP.into()),
                ),
            );
            emit_finished(
                &app,
                &download_id,
                false,
                "failed",
                &detail,
                None,
                None,
                None,
            );
            clear_paused_session_if_current(&app, &download_id);
            finish(&app, &running);
            return;
        }
    };

    if let Err(err) = validate_output_dir(&dir) {
        emit_status(&app, "Download folder unavailable", "err");
        emit_finished(&app, &download_id, false, "failed", &err, None, None, None);
        clear_paused_session_if_current(&app, &download_id);
        finish(&app, &running);
        return;
    }

    if let Some(target) = known_target_path(&dir, &url, custom_filename.as_deref()) {
        if !resume_existing {
            match target.metadata() {
                Ok(meta) if meta.is_file() && meta.len() > 0 => {
                    let file_name = file_name_from_path(&target);
                    emit_status(&app, "File already exists", "warn");
                    emit_finished(
                        &app,
                        &download_id,
                        false,
                        "already_exists",
                        "File already exists. No new download was made.",
                        Some(&target),
                        Some(&file_name),
                        Some(meta.len()),
                    );
                    clear_paused_session_if_current(&app, &download_id);
                    finish(&app, &running);
                    return;
                }
                Ok(_) => remove_stale_zero_target(&target),
                Err(_) => {}
            }
        }
        if let Ok(mut g) = ui_session.lock() {
            g.filename = Some(file_name_from_path(&target));
            g.event_state.last_filename = g.filename.clone();
        }
    }

    let dir_path = Path::new(&dir).to_path_buf();
    let before = snapshot_dir(&dir_path);
    let started_at = SystemTime::now()
        .checked_sub(Duration::from_secs(2))
        .unwrap_or(SystemTime::now());
    let mut strategies = build_strategies(&url);
    if strategies.is_empty() {
        strategies.push(Strategy {
            name: "Default".into(),
            description: "Default aria2 settings".into(),
            args: Vec::new(),
        });
    }

    let mut final_error = String::from("aria2 did not complete the download");
    let mut attempt = 0usize;
    while attempt < strategies.len() {
        if abort.load(Ordering::SeqCst) {
            break;
        }
        let strategy = &strategies[attempt];
        emit_strategy(&app, &ui_session, attempt, strategy);
        emit_status(
            &app,
            &format!("Starting aria2 attempt {}...", attempt + 1),
            "dl",
        );

        match run_aria2_attempt(
            &app,
            &runtime.path,
            &url,
            &dir,
            custom_filename.as_deref(),
            strategy,
            &abort,
            &paused,
            &child_slot,
            &ui_session,
            &before,
            started_at,
        ) {
            AttemptResult::Completed {
                path,
                file_name,
                bytes,
            } => {
                if let Ok(mut g) = ui_session.lock() {
                    g.filename = Some(file_name.clone());
                }
                emit_event(
                    &app,
                    &make_event(
                        EventType::DownloadCompleted,
                        EventCategory::Success,
                        "Download completed",
                        Some(format!(
                            "Saved: {} ({})",
                            path.display(),
                            format_bytes(bytes)
                        )),
                        None,
                        Some(LOG_GROUP.into()),
                    ),
                );
                emit_status(&app, "Download complete", "ok");
                emit_finished(
                    &app,
                    &download_id,
                    true,
                    "completed",
                    "Download completed",
                    Some(&path),
                    Some(&file_name),
                    Some(bytes),
                );
                clear_paused_session_if_current(&app, &download_id);
                finish(&app, &running);
                return;
            }
            AttemptResult::Paused => {
                emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "paused");
                emit_status(&app, "Download paused", "warn");
                let _ = app.emit("download-paused", json!({ "downloadId": download_id }));
                finish(&app, &running);
                return;
            }
            AttemptResult::Cancelled => {
                emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "cancelled");
                emit_status(&app, "Download cancelled", "err");
                emit_finished(
                    &app,
                    &download_id,
                    false,
                    "cancelled",
                    "Download cancelled",
                    None,
                    None,
                    None,
                );
                clear_paused_session_if_current(&app, &download_id);
                finish(&app, &running);
                return;
            }
            AttemptResult::Failed { detail } => {
                final_error = detail;
                attempt += 1;
                if attempt < strategies.len() {
                    let next = &strategies[attempt];
                    emit_event(
                        &app,
                        &make_event(
                            EventType::Retrying,
                            EventCategory::Warn,
                            format!("Retrying with {}", next.name),
                            Some(final_error.clone()),
                            Some("aria2-retry".into()),
                            Some(LOG_GROUP.into()),
                        ),
                    );
                    emit_status(&app, &format!("Retrying with {}...", next.name), "retry");
                }
            }
        }
    }

    if paused.load(Ordering::SeqCst) {
        emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "paused");
        emit_status(&app, "Download paused", "warn");
        let _ = app.emit("download-paused", json!({ "downloadId": download_id }));
        finish(&app, &running);
        return;
    }

    if abort.load(Ordering::SeqCst) {
        emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "cancelled");
        emit_status(&app, "Download cancelled", "err");
        emit_finished(
            &app,
            &download_id,
            false,
            "cancelled",
            "Download cancelled",
            None,
            None,
            None,
        );
        clear_paused_session_if_current(&app, &download_id);
        finish(&app, &running);
        return;
    }

    emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "error");
    emit_event(
        &app,
        &make_event(
            EventType::DownloadFailed,
            EventCategory::Error,
            "Download failed",
            Some(final_error.clone()),
            None,
            Some(LOG_GROUP.into()),
        ),
    );
    emit_status(&app, "Download failed", "err");
    emit_finished(
        &app,
        &download_id,
        false,
        "failed",
        &final_error,
        None,
        None,
        None,
    );
    clear_paused_session_if_current(&app, &download_id);
    finish(&app, &running);
}

#[tauri::command]
pub fn start_download(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    url: String,
    dir: String,
    quality: Option<String>,
    playlist: Option<bool>,
    custom_filename: Option<String>,
    browser_cookie_source: Option<String>,
    download_id: String,
) -> Result<(), String> {
    if state.running.load(Ordering::SeqCst) {
        return Err("A download is already running".into());
    }

    let url_lower = url.to_lowercase();
    if !(url_lower.starts_with("http://")
        || url_lower.starts_with("https://")
        || url_lower.starts_with("ftp://"))
    {
        return Err("Please enter a valid http, https, or ftp URL.".into());
    }
    validate_output_dir(&dir)?;

    let resume_existing = state
        .paused_session
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .map(|s| s.download_id == download_id && s.url == url && s.dir == dir)
        .unwrap_or(false);

    let engine = if yt_dlp::is_video_platform_url(&url) || yt_dlp::is_hls_url(&url) {
        "yt-dlp"
    } else {
        "aria2"
    };
    let is_video = engine == "yt-dlp";
    let filename_hint = if is_video {
        custom_filename.clone()
    } else {
        custom_filename
            .clone()
            .or_else(|| filename_hint_from_url(&url))
    };

    {
        let mut g = state.ui_session.lock().unwrap();
        *g = UiSession::default();
        g.engine = engine.to_string();
        g.filename = filename_hint.clone();
        g.event_state.last_filename = filename_hint.clone();
        g.strategy_name = engine.to_string();
        g.download_id = download_id.clone();
    }

    {
        let mut ps = state.paused_session.lock().unwrap();
        *ps = Some(PausedSession {
            url: url.clone(),
            dir: dir.clone(),
            engine: engine.to_string(),
            quality: quality.clone(),
            playlist,
            custom_filename: custom_filename.clone(),
            browser_cookie_source: browser_cookie_source.clone(),
            download_id: download_id.clone(),
        });
    }

    emit_event(
        &app,
        &make_event(
            EventType::DownloadStarted,
            EventCategory::Info,
            format!("Download started (Engine: {engine})"),
            Some(format!("URL: {url}")),
            None,
            Some(LOG_GROUP.into()),
        ),
    );
    if let Some(name) = filename_hint {
        emit_event(
            &app,
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

    state.abort.store(false, Ordering::SeqCst);
    state.paused.store(false, Ordering::SeqCst);
    state.running.store(true, Ordering::SeqCst);

    let abort = state.abort.clone();
    let paused = state.paused.clone();
    let child = state.child.clone();
    let running = state.running.clone();
    let ui_session = state.ui_session.clone();
    let q = quality.unwrap_or_else(|| "best".to_string());
    let pl = playlist.unwrap_or(false);
    let cf = custom_filename.clone();
    let browser_cookies = browser_cookie_source.clone();

    thread::spawn(move || {
        if is_video {
            yt_dlp::run_yt_dlp_loop(
                app,
                url,
                dir,
                q,
                pl,
                cf,
                browser_cookies,
                abort,
                paused,
                child,
                running,
                ui_session,
            );
        } else {
            run_download_loop(
                app,
                url,
                dir,
                cf,
                resume_existing,
                abort,
                paused,
                child,
                running,
                ui_session,
            );
        }
    });

    Ok(())
}

#[tauri::command]
pub fn stop_download(state: tauri::State<'_, AppState>) -> Result<(), String> {
    shutdown_active_download(&state);
    Ok(())
}

#[tauri::command]
pub fn pause_download(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    if !state.running.load(Ordering::SeqCst) {
        return Err("No active download to pause".into());
    }
    state.paused.store(true, Ordering::SeqCst);
    state.abort.store(true, Ordering::SeqCst);
    if let Ok(mut guard) = state.child.lock() {
        if let Some(child) = guard.as_mut() {
            kill_child_tree(child);
        }
    }
    emit_status(&app, "Download paused", "warn");
    emit_event(
        &app,
        &make_event(
            EventType::DownloadPaused,
            EventCategory::Warn,
            "Download paused",
            None,
            None,
            Some(LOG_GROUP.into()),
        ),
    );
    Ok(())
}

#[tauri::command]
pub async fn resume_download(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    for _ in 0..30 {
        if !state.running.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if state.running.load(Ordering::SeqCst) {
        return Err("Previous download is still shutting down, try again in a moment".into());
    }

    let session = state.paused_session.lock().unwrap().clone();
    if let Some(s) = session {
        state.abort.store(false, Ordering::SeqCst);
        state.paused.store(false, Ordering::SeqCst);
        emit_status(&app, &format!("Resuming {} download...", s.engine), "info");
        emit_event(
            &app,
            &make_event(
                EventType::DownloadResumed,
                EventCategory::Info,
                "Download resumed",
                Some(format!("Engine: {}", s.engine)),
                None,
                Some(LOG_GROUP.into()),
            ),
        );
        let app_for_state = app.clone();
        let state_clone = app_for_state.state::<AppState>();
        start_download(
            app,
            state_clone,
            s.url,
            s.dir,
            s.quality,
            s.playlist,
            s.custom_filename,
            s.browser_cookie_source,
            s.download_id,
        )
    } else {
        Err("No paused session found to resume".into())
    }
}

#[tauri::command]
pub async fn check_aria2(app: AppHandle) -> bool {
    tokio::task::spawn_blocking(move || resolve_aria2_path(&app).is_some())
        .await
        .unwrap_or(false)
}

#[tauri::command]
pub async fn check_ytdlp(app: AppHandle) -> bool {
    tokio::task::spawn_blocking(move || crate::yt_dlp::resolve_yt_dlp_path(&app).is_some())
        .await
        .unwrap_or(false)
}

#[tauri::command]
pub async fn get_ffmpeg_info(app: AppHandle) -> crate::yt_dlp::FfmpegInfo {
    tokio::task::spawn_blocking(move || crate::yt_dlp::check_ffmpeg_info(&app))
        .await
        .unwrap_or_else(|_| crate::yt_dlp::FfmpegInfo {
            available: false,
            path: None,
            source: "check failed".into(),
        })
}

#[tauri::command]
pub fn set_ffmpeg_path(path: String) -> Result<crate::yt_dlp::FfmpegInfo, String> {
    crate::yt_dlp::set_ffmpeg_path(path)
}

#[tauri::command]
pub fn open_download_folder(path: String) -> Result<(), String> {
    let p = Path::new(path.trim());
    let folder = if p.is_dir() {
        p.to_path_buf()
    } else if p.is_file() {
        p.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("Could not resolve parent folder: {}", p.display()))?
    } else {
        return Err(format!("Folder does not exist: {}", p.display()));
    };
    let path_str = folder.to_string_lossy().to_string();

    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("explorer");
        cmd.arg(&path_str);
        crate::process_util::hide_console(&mut cmd);
        cmd.spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed to open folder: {e}"))?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(&path_str)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed to open folder: {e}"))?;
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(&path_str)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed to open folder: {e}"))?;
        return Ok(());
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        Err("Open folder is not supported on this platform".into())
    }
}

#[tauri::command]
pub fn open_youtube() -> Result<(), String> {
    let url = "https://www.youtube.com";

    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("explorer");
        cmd.arg(url);
        crate::process_util::hide_console(&mut cmd);
        cmd.spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed to open YouTube: {e}"))?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed to open YouTube: {e}"))?;
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed to open YouTube: {e}"))?;
        return Ok(());
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        Err("Opening YouTube is not supported on this platform".into())
    }
}

#[tauri::command]
pub fn get_default_download_dir() -> Result<String, String> {
    dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|p| p.join("Downloads")))
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| "Could not resolve Downloads directory".to_string())
}
