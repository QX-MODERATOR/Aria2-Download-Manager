use crate::aria2::{
    emit_event, emit_status, emit_transfer_update, make_event, EventCategory, EventType, UiSession,
    LOG_GROUP,
};
use once_cell::sync::Lazy;
use regex::Regex;
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

fn read_process_output<R: Read + Send + 'static>(
    mut reader: R,
    tx: std::sync::mpsc::Sender<(bool, String)>,
    is_stdout: bool,
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
                            if tx.send((is_stdout, line)).is_err() {
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
        let _ = tx.send((is_stdout, String::from_utf8_lossy(&pending).to_string()));
    }
}

/// User-selected FFmpeg path (validated); checked before bundled/PATH lookup.
static FFMPEG_OVERRIDE: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));
use tauri::{AppHandle, Emitter, Manager};

#[allow(dead_code)]
const USE_BROWSER_COOKIES: bool = false;

struct YtDlpProgressState {
    pct: f64,
    speed: String,
    eta: String,
    total: String,
    downloaded: String,
    last_emit: Instant,
}

impl YtDlpProgressState {
    fn new() -> Self {
        Self {
            pct: 0.0,
            speed: "—".to_string(),
            eta: "—".to_string(),
            total: "—".to_string(),
            downloaded: "—".to_string(),
            last_emit: Instant::now() - Duration::from_secs(10),
        }
    }
}

static PROGRESS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\[download\]\s+(?P<pct>\d+(?:\.\d+)?)%\s+(?:of\s+~?\s*(?P<total>[\d.]+\s*[KMGTP]i?B)?)?(?:\s+at\s+(?P<spd>[\d.]+\s*[KMGTP]i?B/s|Unknown speed))?(?:\s+ETA\s+(?P<eta>[\d:]+|Unknown))?")
        .unwrap()
});

static SIZE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?P<val>[\d.]+)\s*(?P<unit>[kmgt]ib?)").unwrap());

static FILENAME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[download\] Destination: (?P<file>.+)$").unwrap());

static ALREADY_DOWNLOADED_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[download\] (?P<file>.+) has already been downloaded").unwrap());

static MERGER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[Merger\] Merging formats into (?P<file>.+)$").unwrap());

static MERGE_INLINE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)merging formats into").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum YtPhase {
    #[allow(dead_code)]
    Idle,
    ExtractingInfo,
    Downloading,
    PostProcessing,
    Completed,
    Failed,
    Cancelled,
    Paused,
}

impl YtPhase {
    fn as_str(self) -> &'static str {
        match self {
            YtPhase::Idle => "idle",
            YtPhase::ExtractingInfo => "extracting",
            YtPhase::Downloading => "downloading",
            YtPhase::PostProcessing => "post_processing",
            YtPhase::Completed => "completed",
            YtPhase::Failed => "failed",
            YtPhase::Cancelled => "cancelled",
            YtPhase::Paused => "paused",
        }
    }
}

pub fn is_video_platform_url(url: &str) -> bool {
    let url = url.to_lowercase();
    url.contains("youtube.com")
        || url.contains("youtu.be")
        || url.contains("music.youtube.com")
        || url.contains("twitch.tv")
        || url.contains("tiktok.com")
        || url.contains("instagram.com")
        || url.contains("facebook.com/watch")
        || url.contains("facebook.com/share/v/")
        || url.contains("facebook.com/share/r/")
        || url.contains("facebook.com/reel/")
        || url.contains("facebook.com/reels/")
        || url.contains("facebook.com/videos/")
        || url.contains("fb.watch/")
        || url.contains("vimeo.com")
        || url.contains("dailymotion.com")
}

pub fn is_hls_url(url: &str) -> bool {
    let base_url = url.split('?').next().unwrap_or(url);
    base_url.to_lowercase().ends_with(".m3u8")
}

pub fn clean_yt_dlp_error(message: &str) -> String {
    let lower = message.to_lowercase();
    if is_youtube_cookie_required_message(message) {
        return "YouTube requires browser verification for this video.".to_string();
    }

    if lower.contains("failed to decrypt with dpapi")
        || lower.contains("cookies database")
        || (lower.contains("cookies") && lower.contains("from-browser"))
    {
        return "yt-dlp could not read browser cookies directly. Open YouTube in your browser while signed in, then retry with Chrome or Edge cookies.".to_string();
    }

    message.to_string()
}

fn is_youtube_cookie_required_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("sign in to confirm")
        || lower.contains("not a bot")
        || lower.contains("--cookies-from-browser")
        || lower.contains("use --cookies-from-browser")
}

#[derive(serde::Serialize)]
pub struct FfmpegInfo {
    pub available: bool,
    pub path: Option<String>,
    pub source: String,
}

fn validate_ffmpeg_dir(dir: &Path) -> bool {
    let ffmpeg_name = if cfg!(target_os = "windows") {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let ffprobe_name = if cfg!(target_os = "windows") {
        "ffprobe.exe"
    } else {
        "ffprobe"
    };

    let ffmpeg_path = dir.join(ffmpeg_name);
    let ffprobe_path = dir.join(ffprobe_name);

    if !ffmpeg_path.is_file() || !ffprobe_path.is_file() {
        return false;
    }

    // Run ffmpeg -version
    let mut ffmpeg_cmd = Command::new(&ffmpeg_path);
    ffmpeg_cmd
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::process_util::hide_console(&mut ffmpeg_cmd);
    let ffmpeg_ok = ffmpeg_cmd.status().map(|s| s.success()).unwrap_or(false);

    // Run ffprobe -version
    let mut ffprobe_cmd = Command::new(&ffprobe_path);
    ffprobe_cmd
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::process_util::hide_console(&mut ffprobe_cmd);
    let ffprobe_ok = ffprobe_cmd.status().map(|s| s.success()).unwrap_or(false);

    ffmpeg_ok && ffprobe_ok
}

pub fn resolve_ffmpeg_dir(app: &AppHandle) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    // 1. Production resources path (Tauri resolved resource_dir)
    if let Ok(resource_dir) = app.path().resource_dir() {
        candidates.push(resource_dir.join("bin").join("ffmpeg").join("windows"));
        candidates.push(
            resource_dir
                .join("resources")
                .join("bin")
                .join("ffmpeg")
                .join("windows"),
        );
        candidates.push(resource_dir.join("ffmpeg").join("windows"));
        candidates.push(resource_dir.join("bin"));
        candidates.push(resource_dir);
    }

    // 2. Relative to current executable (production fallback)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(
                dir.join("resources")
                    .join("bin")
                    .join("ffmpeg")
                    .join("windows"),
            );
            candidates.push(dir.join("bin").join("ffmpeg").join("windows"));
            candidates.push(dir.join("ffmpeg").join("windows"));
            candidates.push(dir.join("bin"));
            candidates.push(dir.to_path_buf());
        }
    }

    // 3. Development / build tree (Cargo run)
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let root = PathBuf::from(manifest);
        candidates.push(
            root.join("resources")
                .join("bin")
                .join("ffmpeg")
                .join("windows"),
        );
        candidates.push(root.join("bin").join("ffmpeg").join("windows"));
        if let Some(parent) = root.parent() {
            candidates.push(parent.join("target").join("release"));
            candidates.push(parent.join("target").join("debug"));
        }
    }

    for path in candidates {
        if path.is_dir() && validate_ffmpeg_dir(&path) {
            return Some(path);
        }
    }

    None
}

pub fn check_ffmpeg_info(app: &AppHandle) -> FfmpegInfo {
    if let Some(override_path) = FFMPEG_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
        let p = PathBuf::from(&override_path);
        if let Some(parent) = p.parent() {
            if validate_ffmpeg_dir(parent) {
                return FfmpegInfo {
                    available: true,
                    path: Some(override_path),
                    source: "user".into(),
                };
            }
        }
    }

    if let Some(dir) = resolve_ffmpeg_dir(app) {
        let ffmpeg_name = if cfg!(target_os = "windows") {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        let ffmpeg_path = dir.join(ffmpeg_name);
        return FfmpegInfo {
            available: true,
            path: Some(ffmpeg_path.to_string_lossy().to_string()),
            source: "bundled".into(),
        };
    }

    if let Ok(path) = env_path_search(if cfg!(target_os = "windows") {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }) {
        if let Some(parent) = path.parent() {
            if validate_ffmpeg_dir(parent) {
                return FfmpegInfo {
                    available: true,
                    path: Some(path.to_string_lossy().to_string()),
                    source: "path".into(),
                };
            }
        }
    }

    FfmpegInfo {
        available: false,
        path: None,
        source: "missing".into(),
    }
}

pub fn set_ffmpeg_path(path: String) -> Result<FfmpegInfo, String> {
    let p = PathBuf::from(path.trim());
    if !p.is_file() {
        return Err(format!("FFmpeg not found: {}", p.display()));
    }
    if let Some(parent) = p.parent() {
        if !validate_ffmpeg_dir(parent) {
            return Err("Selected folder must contain both working ffmpeg and ffprobe".into());
        }
    } else {
        return Err("Selected file does not have a parent directory".into());
    }
    let s = p.to_string_lossy().to_string();
    if let Ok(mut g) = FFMPEG_OVERRIDE.lock() {
        *g = Some(s.clone());
    }
    Ok(FfmpegInfo {
        available: true,
        path: Some(s),
        source: "user".into(),
    })
}

fn append_js_runtime_args(args: &mut Vec<String>) {
    let binary = if cfg!(target_os = "windows") {
        "node.exe"
    } else {
        "node"
    };
    if env_path_search(binary).is_ok() {
        args.push("--js-runtimes".to_string());
        args.push("node".to_string());
    }
}

#[allow(dead_code)]
pub fn clear_ffmpeg_override() {
    if let Ok(mut g) = FFMPEG_OVERRIDE.lock() {
        *g = None;
    }
}

fn is_youtube_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("youtube.com")
        || lower.contains("youtu.be")
        || lower.contains("music.youtube.com")
}

fn append_format_args(args: &mut Vec<String>, quality: &str, ffmpeg_ok: bool, url: &str) {
    if ffmpeg_ok {
        let youtube = is_youtube_url(url);
        match quality {
            // YouTube: prefer Windows Media Player-compatible codecs up front.
            // This avoids slow post-download re-encoding while steering yt-dlp toward
            // MP4 video, M4A/AAC audio, and an MP4 merge.
            "best" | "best_mp4" if youtube => {
                args.push("-S".to_string());
                args.push("vcodec:h264,acodec:m4a,res,ext:mp4:m4a".to_string());
                args.push("-f".to_string());
                args.push("bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/b".to_string());
                args.push("--merge-output-format".to_string());
                args.push("mp4".to_string());
            }
            "1080p_mp4" => {
                args.push("-f".to_string());
                args.push(
                    "bestvideo[height<=1080][vcodec^=avc1][ext=mp4]+bestaudio[ext=m4a]/best[height<=1080]".to_string(),
                );
                args.push("--merge-output-format".to_string());
                args.push("mp4".to_string());
            }
            "720p_mp4" => {
                args.push("-f".to_string());
                args.push("bestvideo[height<=720][vcodec^=avc1][ext=mp4]+bestaudio[ext=m4a]/best[height<=720]".to_string());
                args.push("--merge-output-format".to_string());
                args.push("mp4".to_string());
            }
            "best_any_mkv" => {
                args.push("-f".to_string());
                args.push("bestvideo+bestaudio/best".to_string());
                args.push("--merge-output-format".to_string());
                args.push("mkv".to_string());
            }
            "mp3" => {
                args.push("-x".to_string());
                args.push("--audio-format".to_string());
                args.push("mp3".to_string());
                args.push("--audio-quality".to_string());
                args.push("0".to_string());
            }
            "m4a" => {
                args.push("-f".to_string());
                args.push("ba[ext=m4a]/ba".to_string());
                args.push("--extract-audio".to_string());
                args.push("--audio-format".to_string());
                args.push("m4a".to_string());
            }
            "best_mp4" => {
                args.push("-f".to_string());
                args.push(
                    "bestvideo[vcodec^=avc1][ext=mp4]+bestaudio[ext=m4a]/best[ext=mp4]/best"
                        .to_string(),
                );
                args.push("--merge-output-format".to_string());
                args.push("mp4".to_string());
            }
            _ => {
                // "best" and legacy aliases
                args.push("-f".to_string());
                args.push("bestvideo+bestaudio/best".to_string());
                args.push("--merge-output-format".to_string());
                args.push("mp4".to_string());
            }
        }
    } else {
        match quality {
            "best_single" => {
                args.push("-f".to_string());
                args.push("best".to_string());
            }
            _ => {
                args.push("-f".to_string());
                args.push("best[ext=mp4]/best".to_string());
            }
        }
    }
}

fn append_platform_network_args(args: &mut Vec<String>, url: &str) {
    let lower = url.to_lowercase();
    if lower.contains("tiktok.com")
        || lower.contains("vm.tiktok.com")
        || lower.contains("vt.tiktok.com")
    {
        args.extend([
            "--socket-timeout".to_string(),
            "60".to_string(),
            "--retries".to_string(),
            "5".to_string(),
            "--fragment-retries".to_string(),
            "5".to_string(),
            "--force-ipv4".to_string(),
        ]);
    }
}

fn emit_yt_phase(
    app: &AppHandle,
    ui_session: &Arc<Mutex<UiSession>>,
    state: &mut YtDlpProgressState,
    phase: YtPhase,
    pct: u32,
    speed: Option<&str>,
    eta: Option<&str>,
) {
    let phase_str = phase.as_str();
    if phase == YtPhase::Completed {
        emit_transfer_update(
            app,
            ui_session,
            100,
            Some(speed.unwrap_or("Completed")),
            Some(eta.unwrap_or("Done")),
            Some(&state.downloaded),
            Some(&state.total),
            phase_str,
        );
    } else if phase == YtPhase::PostProcessing {
        emit_transfer_update(
            app,
            ui_session,
            pct.max(state.pct as u32),
            Some("—"),
            Some("—"),
            Some(&state.downloaded),
            Some(&state.total),
            phase_str,
        );
    } else {
        emit_transfer_update(
            app,
            ui_session,
            pct,
            speed,
            eta,
            Some(&state.downloaded),
            Some(&state.total),
            phase_str,
        );
    }
}

#[allow(dead_code)]
pub fn check_ffmpeg(app: &AppHandle) -> bool {
    check_ffmpeg_info(app).available
}

fn yt_dlp_binary_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "yt-dlp.exe"
    }
    #[cfg(not(target_os = "windows"))]
    {
        "yt-dlp"
    }
}

fn bundled_yt_dlp_names() -> &'static [&'static str] {
    #[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
    {
        &["yt-dlp-x86_64-pc-windows-msvc.exe", "yt-dlp.exe"]
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "gnu"))]
    {
        &["yt-dlp-x86_64-pc-windows-gnu.exe", "yt-dlp.exe"]
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        &["yt-dlp-x86_64-apple-darwin", "yt-dlp"]
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        &["yt-dlp-aarch64-apple-darwin", "yt-dlp"]
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        &["yt-dlp-x86_64-unknown-linux-gnu", "yt-dlp"]
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        &["yt-dlp-aarch64-unknown-linux-gnu", "yt-dlp"]
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
        &[yt_dlp_binary_name()]
    }
}

pub fn resolve_yt_dlp_path(app: &AppHandle) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    // 1. Resources dir (bundled)
    if let Ok(resource_dir) = app.path().resource_dir() {
        let bin_dir = resource_dir.join("bin");
        for name in bundled_yt_dlp_names() {
            candidates.push(bin_dir.join(name));
            candidates.push(resource_dir.join(name));
        }
    }

    // 2. Beside executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bin_dir = dir.join("bin");
            for name in bundled_yt_dlp_names() {
                candidates.push(bin_dir.join(name));
                candidates.push(dir.join(name));
            }
        }
    }

    for path in candidates {
        if path.is_file() {
            return Some(path);
        }
    }

    // 3. Fallback to searching on PATH
    if let Ok(path) = env_path_search(yt_dlp_binary_name()) {
        return Some(path);
    }

    None
}

fn env_path_search(binary_name: &str) -> Result<PathBuf, ()> {
    let path_var = std::env::var_os("PATH").ok_or(())?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary_name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(())
}

fn clean_yt_dlp_path(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim()
        .to_string()
}

fn resolve_yt_dlp_path_from_output(dir: &str, raw: &str) -> PathBuf {
    let clean = clean_yt_dlp_path(raw);
    let path = Path::new(&clean);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(dir).join(path)
    }
}

fn set_detected_output_file(
    ui_session: &Arc<Mutex<UiSession>>,
    dir: &str,
    raw_path: &str,
) -> PathBuf {
    let clean = clean_yt_dlp_path(raw_path);
    let file_name = Path::new(&clean)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&clean)
        .to_string();

    {
        let mut g = ui_session.lock().unwrap();
        g.filename = Some(file_name);
    }

    resolve_yt_dlp_path_from_output(dir, &clean)
}

fn verified_output_metadata(path: Option<&PathBuf>) -> (bool, u64, Option<PathBuf>) {
    let Some(path) = path else {
        return (false, 0, None);
    };

    if let Ok(m) = std::fs::metadata(path) {
        if m.is_file() {
            return (true, m.len(), Some(path.clone()));
        }
    }

    let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
        return (false, 0, None);
    };
    let Some(parent) = path.parent() else {
        return (false, 0, None);
    };

    for marker in [
        ".f137", ".f136", ".f135", ".f134", ".f140", ".f251", ".f250", ".f249",
    ] {
        if let Some((stem, ext)) = file_name.rsplit_once(marker) {
            let candidate = parent.join(format!("{stem}{ext}"));
            if let Ok(m) = std::fs::metadata(&candidate) {
                if m.is_file() {
                    return (true, m.len(), Some(candidate));
                }
            }
        }
    }

    (false, 0, None)
}

fn youtube_video_id(url: &str) -> Option<String> {
    if let Some((_, rest)) = url.split_once("youtu.be/") {
        let id = rest
            .split(|c| c == '?' || c == '&' || c == '/' || c == '#')
            .next()
            .unwrap_or("")
            .trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }

    for key in ["v=", "shorts/"] {
        if let Some((_, rest)) = url.split_once(key) {
            let id = rest
                .split(|c| c == '&' || c == '?' || c == '/' || c == '#')
                .next()
                .unwrap_or("")
                .trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }

    None
}

fn is_final_download_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let lower = name.to_lowercase();
    !(lower.ends_with(".part")
        || lower.ends_with(".ytdl")
        || lower.ends_with(".aria2")
        || lower.ends_with(".tmp")
        || lower.ends_with(".temp")
        || lower.ends_with(".webm.part")
        || lower.ends_with(".mp4.part"))
}

fn find_completed_output(dir: &str, url: &str, started_at: SystemTime) -> Option<(PathBuf, u64)> {
    let id = youtube_video_id(url);
    let mut best: Option<(PathBuf, u64, SystemTime, i32)> = None;
    let entries = std::fs::read_dir(dir).ok()?;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || !is_final_download_file(&path) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) if m.len() > 0 => m,
            _ => continue,
        };
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if modified < started_at {
            continue;
        }

        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let score = if id.as_ref().is_some_and(|needle| file_name.contains(needle)) {
            2
        } else {
            1
        };

        let replace = best
            .as_ref()
            .map(|(_, _, best_modified, best_score)| {
                score > *best_score || (score == *best_score && modified > *best_modified)
            })
            .unwrap_or(true);

        if replace {
            best = Some((path, metadata.len(), modified, score));
        }
    }

    best.map(|(path, size, _, _)| (path, size))
}

pub fn run_yt_dlp_loop(
    app: AppHandle,
    url: String,
    dir: String,
    quality: String,
    download_playlist: bool,
    custom_filename: Option<String>,
    browser_cookie_source: Option<String>,
    abort: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    child_slot: Arc<Mutex<Option<std::process::Child>>>,
    running: Arc<AtomicBool>,
    ui_session: Arc<Mutex<UiSession>>,
) {
    let yt_dlp_path = match resolve_yt_dlp_path(&app) {
        Some(p) => p,
        None => {
            emit_status(&app, "yt-dlp not found", "error");
            emit_event(
                &app,
                &make_event(
                    EventType::DownloadFailed,
                    EventCategory::Error,
                    "yt-dlp missing",
                    Some("Could not find yt-dlp binary in resources or bin folder.".into()),
                    None,
                    Some(LOG_GROUP.into()),
                ),
            );
            running.store(false, Ordering::SeqCst);
            let download_id = ui_session
                .lock()
                .ok()
                .map(|g| g.download_id.clone())
                .unwrap_or_default();
            let payload = serde_json::json!({
                "downloadId": download_id,
                "success": false,
                "status": "failed",
                "engine": "yt-dlp",
                "message": "Could not find yt-dlp binary"
            });
            let _ = app.emit("finished", payload);
            return;
        }
    };

    let ffmpeg_info = check_ffmpeg_info(&app);
    let ffmpeg_ok = ffmpeg_info.available;
    if ffmpeg_ok {
        emit_event(
            &app,
            &make_event(
                EventType::MetadataUpdated,
                EventCategory::Info,
                "FFmpeg detected",
                Some("FFmpeg detected — full quality mode enabled".into()),
                None,
                Some(LOG_GROUP.into()),
            ),
        );
    } else {
        emit_event(&app, &make_event(
            EventType::MetadataUpdated,
            EventCategory::Warn,
            "FFmpeg not found",
            Some(
                "FFmpeg not found. High-quality video+audio merging is disabled; using single-file formats only."
                    .into(),
            ),
            None,
            Some(LOG_GROUP.into()),
        ));
    }

    let mut args = vec![
        url.clone(),
        "--newline".to_string(),
        "--progress".to_string(),
        "--continue".to_string(),
        "--part".to_string(),
        "-P".to_string(),
        dir.clone(),
    ];

    let custom_o = custom_filename.clone().unwrap_or_default();
    if custom_filename.is_some() {
        args.push("-o".to_string());
        args.push(custom_o);
    } else {
        args.push("-o".to_string());
        args.push("%(title).180B [%(id)s].%(ext)s".to_string());
    }

    if !download_playlist {
        args.push("--no-playlist".to_string());
    }

    let ffmpeg_location: Option<String> = ffmpeg_info.path.as_ref().and_then(|ff_path| {
        Path::new(ff_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
    });
    if let Some(ref loc) = ffmpeg_location {
        args.push("--ffmpeg-location".to_string());
        args.push(loc.clone());
    }

    append_js_runtime_args(&mut args);

    if let Some(source) = browser_cookie_source.as_deref() {
        if matches!(source, "chrome" | "edge") {
            args.push("--cookies-from-browser".to_string());
            args.push(source.to_string());
            emit_event(
                &app,
                &make_event(
                    EventType::MetadataUpdated,
                    EventCategory::Info,
                    "Browser cookies enabled for this attempt",
                    Some(format!("Using --cookies-from-browser {source}")),
                    None,
                    Some(LOG_GROUP.into()),
                ),
            );
        }
    }

    emit_event(
        &app,
        &make_event(
            EventType::MetadataUpdated,
            EventCategory::Info,
            "yt-dlp downloader",
            Some("Using yt-dlp native media transfer progress.".into()),
            None,
            Some(LOG_GROUP.into()),
        ),
    );

    append_format_args(&mut args, &quality, ffmpeg_ok, &url);
    append_platform_network_args(&mut args, &url);

    let format_desc = if !ffmpeg_ok {
        "Using single-file format selection (FFmpeg not available)."
    } else if quality.contains("mkv") {
        "Using best-quality format selection with MKV merge output."
    } else if quality.contains("mp3") || quality.contains("m4a") {
        "Using audio extraction mode."
    } else if quality == "best_mp4" {
        "Using H.264 MP4-compatible format selection with merge when needed."
    } else {
        "Using best-quality MP4 merge format selection."
    };

    emit_event(
        &app,
        &make_event(
            EventType::MetadataUpdated,
            EventCategory::Info,
            "Format selection",
            Some(format_desc.into()),
            None,
            Some(LOG_GROUP.into()),
        ),
    );

    emit_status(&app, "Reading video information...", "info");
    emit_transfer_update(&app, &ui_session, 0, None, None, None, None, "extracting");

    let run_started_at = SystemTime::now();
    let mut cmd = Command::new(yt_dlp_path);
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    crate::process_util::hide_console(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            emit_status(&app, "Failed to launch yt-dlp", "error");
            emit_event(
                &app,
                &make_event(
                    EventType::DownloadFailed,
                    EventCategory::Error,
                    "Spawn failed",
                    Some(format!("Error: {}", e)),
                    None,
                    Some(LOG_GROUP.into()),
                ),
            );
            running.store(false, Ordering::SeqCst);
            let download_id = ui_session
                .lock()
                .ok()
                .map(|g| g.download_id.clone())
                .unwrap_or_default();
            let payload = serde_json::json!({
                "downloadId": download_id,
                "success": false,
                "status": "failed",
                "engine": "yt-dlp",
                "message": format!("Failed to spawn yt-dlp: {}", e)
            });
            let _ = app.emit("finished", payload);
            return;
        }
    };
    let stdout = child.stdout.take().expect("Failed to open stdout");
    let stderr = child.stderr.take().expect("Failed to open stderr");

    {
        let mut slot = child_slot.lock().unwrap();
        *slot = Some(child);
    }

    let (tx, rx) = std::sync::mpsc::channel::<(bool, String)>();

    let tx_stdout = tx.clone();
    thread::spawn(move || {
        read_process_output(stdout, tx_stdout, true);
    });

    let tx_stderr = tx.clone();
    thread::spawn(move || {
        read_process_output(stderr, tx_stderr, false);
    });

    // Drop the original sender so `rx.recv()` will return Err when both
    // reader threads finish and no more messages exist. Without this,
    // the receive loop can block forever because the original `tx`
    // remains alive but never sends.
    drop(tx);

    let mut state = YtDlpProgressState::new();
    let app_handle = app.clone();
    let mut phase = YtPhase::ExtractingInfo;
    let mut saw_already_downloaded = false;
    let mut saw_error = false;
    let mut saw_download_progress = false;
    let mut saw_destination = false;
    let mut saw_merger = false;
    let mut final_file_path: Option<PathBuf> = None;
    let mut last_error: Option<String> = None;
    let mut last_stderr: Option<String> = None;
    let mut youtube_cookie_required = false;

    emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 0, None, None);

    loop {
        let (is_stdout, line) = match rx.recv_timeout(std::time::Duration::from_millis(400)) {
            Ok(v) => v,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if abort.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if abort.load(Ordering::SeqCst) {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let line_lower = trimmed.to_lowercase();
        if is_youtube_cookie_required_message(trimmed) {
            youtube_cookie_required = true;
            saw_error = true;
            last_error = Some(clean_yt_dlp_error(trimmed));
        }

        if !is_stdout {
            last_stderr = Some(trimmed.to_string());
        }

        {
            if let Some(caps) = PROGRESS_RE.captures(trimmed) {
                let pct_val = caps["pct"].parse::<f64>().unwrap_or(state.pct);

                if let Some(m) = caps.name("total") {
                    state.total = m.as_str().to_string();
                }
                if let Some(m) = caps.name("spd") {
                    let s = m.as_str();
                    if s != "Unknown speed" && s != "Unknown" && !s.is_empty() {
                        state.speed = s.to_string();
                    }
                }
                if let Some(m) = caps.name("eta") {
                    let e = m.as_str();
                    if e != "Unknown" && e != "0" && e != "00:00" && !e.is_empty() {
                        state.eta = e.to_string();
                    }
                }

                state.pct = pct_val;
                if let Some(total_bytes) = parse_size_to_bytes(&state.total) {
                    state.downloaded = format_bytes((total_bytes * (state.pct / 100.0)) as u64);
                }

                if state.pct > 0.0 {
                    saw_download_progress = true;
                }

                if phase != YtPhase::PostProcessing {
                    phase = YtPhase::Downloading;
                }

                let now = Instant::now();
                if now.duration_since(state.last_emit) >= Duration::from_millis(250)
                    || state.pct >= 100.0
                {
                    state.last_emit = now;
                    let pct_u = state.pct as u32;
                    let spd = state.speed.clone();
                    let eta_s = state.eta.clone();
                    emit_yt_phase(
                        &app_handle,
                        &ui_session,
                        &mut state,
                        phase,
                        pct_u,
                        Some(&spd),
                        Some(&eta_s),
                    );
                }
            } else if let Some(caps) = FILENAME_RE.captures(trimmed) {
                let file_path = caps["file"].trim();
                let resolved = set_detected_output_file(&ui_session, &dir, file_path);
                let file_name = ui_session
                    .lock()
                    .ok()
                    .and_then(|g| g.filename.clone())
                    .unwrap_or_else(|| clean_yt_dlp_path(file_path));
                final_file_path = Some(resolved);
                saw_destination = true;

                emit_event(
                    &app_handle,
                    &make_event(
                        EventType::MetadataUpdated,
                        EventCategory::Info,
                        "File name detected",
                        Some(file_name.to_string()),
                        None,
                        Some(LOG_GROUP.into()),
                    ),
                );
                if phase == YtPhase::ExtractingInfo {
                    phase = YtPhase::Downloading;
                }
            } else if let Some(caps) = ALREADY_DOWNLOADED_RE.captures(trimmed) {
                let file_path = caps["file"].trim();
                saw_already_downloaded = true;
                state.pct = 100.0;
                final_file_path = Some(set_detected_output_file(&ui_session, &dir, file_path));

                emit_status(&app_handle, "Already downloaded", "ok");
                phase = YtPhase::Downloading;
                let spd = state.speed.clone();
                let eta_s = state.eta.clone();
                emit_yt_phase(
                    &app_handle,
                    &ui_session,
                    &mut state,
                    phase,
                    100,
                    Some(&spd),
                    Some(&eta_s),
                );
            } else if MERGER_RE.is_match(trimmed) || MERGE_INLINE_RE.is_match(trimmed) {
                phase = YtPhase::PostProcessing;
                if let Some(caps) = MERGER_RE.captures(trimmed) {
                    let file_path = caps["file"].trim();
                    let resolved = set_detected_output_file(&ui_session, &dir, file_path);
                    let file_name = ui_session
                        .lock()
                        .ok()
                        .and_then(|g| g.filename.clone())
                        .unwrap_or_else(|| clean_yt_dlp_path(file_path));
                    final_file_path = Some(resolved);
                    saw_merger = true;

                    emit_event(
                        &app_handle,
                        &make_event(
                            EventType::MetadataUpdated,
                            EventCategory::Info,
                            "Merging started",
                            Some(file_name.to_string()),
                            None,
                            Some(LOG_GROUP.into()),
                        ),
                    );
                }
                emit_status(&app_handle, "Merging formats…", "info");
                emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 100, None, None);
            } else if line_lower.contains("deleting original file") {
                phase = YtPhase::PostProcessing;
                emit_status(&app_handle, "Post-processing…", "info");
                emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 100, None, None);
            } else if trimmed.contains("[ExtractAudio]") || line_lower.contains("extractaudio") {
                phase = YtPhase::PostProcessing;
                emit_status(&app_handle, "Converting audio…", "info");
                emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 100, None, None);
            } else if trimmed.starts_with("ERROR:") {
                saw_error = true;
                emit_status(&app_handle, "yt-dlp error", "error");
                let detail = clean_yt_dlp_error(trimmed);
                last_error = Some(detail.clone());
                emit_event(
                    &app_handle,
                    &make_event(
                        EventType::DownloadFailed,
                        EventCategory::Error,
                        "yt-dlp reported error",
                        Some(detail),
                        None,
                        Some(LOG_GROUP.into()),
                    ),
                );
            } else if phase == YtPhase::ExtractingInfo
                && (trimmed.starts_with("[info]") || trimmed.starts_with("[youtube]"))
            {
                emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 0, None, None);
            } else if !is_stdout && line_lower.contains("error:") {
                saw_error = true;
                let detail = clean_yt_dlp_error(trimmed);
                last_error = Some(detail.clone());
                emit_event(
                    &app_handle,
                    &make_event(
                        EventType::DownloadFailed,
                        EventCategory::Error,
                        "yt-dlp error",
                        Some(detail),
                        None,
                        Some(LOG_GROUP.into()),
                    ),
                );
            }
        }
    }

    // Wait for the child process to exit and capture its exit status.
    let mut exit_ok = false;
    if let Ok(mut slot) = child_slot.lock() {
        if let Some(mut c) = slot.take() {
            match c.wait() {
                Ok(status) => {
                    exit_ok = status.success();
                }
                Err(_err) => {
                    exit_ok = false;
                }
            }
        }
    }

    let download_id = {
        let g = ui_session.lock().unwrap();
        g.download_id.clone()
    };

    if paused.load(Ordering::SeqCst) {
        phase = YtPhase::Paused;
        emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 0, None, None);
        emit_status(&app_handle, "Download paused", "warn");
        let _ = app_handle.emit(
            "download-paused",
            serde_json::json!({ "downloadId": download_id }),
        );
        running.store(false, Ordering::SeqCst);
        return;
    }

    if abort.load(Ordering::SeqCst) {
        phase = YtPhase::Cancelled;
        emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 0, None, None);
        emit_status(&app_handle, "Download cancelled", "err");
        running.store(false, Ordering::SeqCst);
        let payload = serde_json::json!({
            "downloadId": download_id,
            "success": false,
            "status": "cancelled",
            "engine": "yt-dlp",
            "message": "Download cancelled"
        });
        let _ = app_handle.emit("finished", payload);
        return;
    }

    // Decide final outcome using strict rules
    running.store(false, Ordering::SeqCst);

    // Determine final file existence/size
    let (mut final_exists, mut final_size, verified_path) =
        verified_output_metadata(final_file_path.as_ref());
    if let Some(path) = verified_path {
        if final_file_path.as_ref() != Some(&path) {
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string());
            if let Some(file_name) = file_name {
                if let Ok(mut g) = ui_session.lock() {
                    g.filename = Some(file_name);
                }
            }
            final_file_path = Some(path);
        }
    }
    if !final_exists && exit_ok {
        if let Some((path, size)) = find_completed_output(&dir, &url, run_started_at) {
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string());
            if let Some(file_name) = file_name {
                if let Ok(mut g) = ui_session.lock() {
                    g.filename = Some(file_name);
                }
            }
            final_file_path = Some(path);
            final_size = size;
            final_exists = true;
            saw_destination = true;
        }
    }

    // Build base payload fields
    let engine = "yt-dlp";

    if youtube_cookie_required {
        phase = YtPhase::Failed;
        emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 0, None, None);
        emit_status(&app_handle, "YouTube verification required", "warn");
        let message = "YouTube requires browser verification for this video.";
        let payload = serde_json::json!({
            "downloadId": download_id,
            "success": false,
            "status": "youtube_cookie_required",
            "engine": engine,
            "url": url,
            "message": message
        });
        let _ = app_handle.emit("youtube-cookie-required", payload.clone());
        let _ = app_handle.emit("finished", payload);
        return;
    }

    if saw_already_downloaded {
        // The file already existed prior to this run
        phase = YtPhase::PostProcessing;
        emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 100, None, None);
        emit_status(&app_handle, "File already exists", "warn");
        let file_path_str = final_file_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let file_name = ui_session.lock().ok().and_then(|g| g.filename.clone());
        let payload = serde_json::json!({
            "downloadId": download_id,
            "success": false,
            "status": "already_exists",
            "engine": engine,
            "filePath": file_path_str,
            "fileName": file_name,
            "downloadedBytes": final_size as u64,
            "totalBytes": null,
            "message": "File already exists. No new download was made."
        });
        let _ = app_handle.emit("finished", payload);
        return;
    }

    if abort.load(Ordering::SeqCst) {
        phase = YtPhase::Cancelled;
        emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 0, None, None);
        emit_status(&app_handle, "Download cancelled", "err");
        let payload = serde_json::json!({
            "downloadId": download_id,
            "success": false,
            "status": "cancelled",
            "engine": engine,
            "message": "User cancelled the download"
        });
        let _ = app_handle.emit("finished", payload);
        return;
    }

    if paused.load(Ordering::SeqCst) {
        phase = YtPhase::Paused;
        emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 0, None, None);
        emit_status(&app_handle, "Download paused", "warn");
        let payload = serde_json::json!({
            "downloadId": download_id,
            "success": false,
            "status": "paused",
            "engine": engine,
            "message": "Download paused"
        });
        let _ = app_handle.emit("finished", payload);
        return;
    }

    if !exit_ok || saw_error {
        phase = YtPhase::Failed;
        emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 0, None, None);
        let detail = last_error
            .clone()
            .or_else(|| last_stderr.as_deref().map(clean_yt_dlp_error))
            .unwrap_or_else(|| {
                if saw_error {
                    "yt-dlp reported an error during the run".to_string()
                } else {
                    "yt-dlp exited with a non-zero status".to_string()
                }
            });
        emit_event(
            &app_handle,
            &make_event(
                EventType::DownloadFailed,
                EventCategory::Error,
                "Download failed",
                Some(detail.clone()),
                None,
                Some(LOG_GROUP.into()),
            ),
        );
        emit_status(&app_handle, "Download failed", "error");
        let payload = serde_json::json!({
            "downloadId": download_id,
            "success": false,
            "status": "failed",
            "engine": engine,
            "message": detail
        });
        let _ = app_handle.emit("finished", payload);
        return;
    }

    // exit_ok == true and no saw_error
    if final_exists && final_size > 0 && (saw_download_progress || saw_merger || saw_destination) {
        phase = YtPhase::Completed;
        emit_yt_phase(
            &app_handle,
            &ui_session,
            &mut state,
            phase,
            100,
            Some("Completed"),
            Some("Done"),
        );
        let file_path_str = final_file_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let file_name = ui_session.lock().ok().and_then(|g| g.filename.clone());
        emit_event(
            &app_handle,
            &make_event(
                EventType::DownloadCompleted,
                EventCategory::Success,
                "Download completed successfully",
                file_name.clone().map(|f| format!("File: {f}")),
                None,
                Some(LOG_GROUP.into()),
            ),
        );
        emit_status(&app_handle, "Download complete ✓", "ok");
        let payload = serde_json::json!({
            "downloadId": download_id,
            "success": true,
            "status": "completed",
            "engine": engine,
            "filePath": file_path_str,
            "fileName": file_name,
            "downloadedBytes": final_size as u64,
            "totalBytes": null,
            "message": "Download completed"
        });
        let _ = app_handle.emit("finished", payload);
        return;
    }

    // exit_ok but no verified output
    phase = YtPhase::Failed;
    emit_yt_phase(&app_handle, &ui_session, &mut state, phase, 0, None, None);
    emit_event(
        &app_handle,
        &make_event(
            EventType::DownloadFailed,
            EventCategory::Error,
            "Download failed",
            Some("yt-dlp exited successfully but no verified output file was produced".to_string()),
            None,
            Some(LOG_GROUP.into()),
        ),
    );
    emit_status(&app_handle, "Download failed", "error");
    let payload = serde_json::json!({
        "downloadId": download_id,
        "success": false,
        "status": "failed",
        "engine": engine,
        "message": "yt-dlp exited successfully but no verified output file was produced"
    });
    let _ = app_handle.emit("finished", payload);
}

fn parse_size_to_bytes(s: &str) -> Option<f64> {
    let s = s.to_lowercase();
    let caps = SIZE_RE.captures(&s)?;
    let val = caps["val"].parse::<f64>().ok()?;
    let unit = &caps["unit"];

    let multiplier = match unit {
        "k" | "ki" | "kib" => 1024.0,
        "m" | "mi" | "mib" => 1024.0 * 1024.0,
        "g" | "gi" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "ti" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    Some(val * multiplier)
}

fn format_bytes(bytes: u64) -> String {
    const KI: u64 = 1024;
    const MI: u64 = KI * 1024;
    const GI: u64 = MI * 1024;
    const TI: u64 = GI * 1024;

    if bytes >= TI {
        format!("{:.2}TiB", bytes as f64 / TI as f64)
    } else if bytes >= GI {
        format!("{:.2}GiB", bytes as f64 / GI as f64)
    } else if bytes >= MI {
        format!("{:.2}MiB", bytes as f64 / MI as f64)
    } else if bytes >= KI {
        format!("{:.2}KiB", bytes as f64 / KI as f64)
    } else {
        format!("{}B", bytes)
    }
}
