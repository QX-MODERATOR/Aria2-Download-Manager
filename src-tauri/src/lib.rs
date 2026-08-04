// Hide the app's own console on Windows (dev + release).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod aria2;
mod classifier;
mod process_util;
mod strategy;
mod yt_dlp;

use tauri::Manager;

pub fn run() {
    tauri::Builder::default()
        .manage(aria2::AppState::new())
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::CloseRequested { .. }) {
                let state = window.state::<aria2::AppState>();
                aria2::shutdown_active_download(&state);
            }
        })
        .invoke_handler(tauri::generate_handler![
            aria2::start_download,
            aria2::stop_download,
            aria2::check_aria2,
            aria2::check_ytdlp,
            aria2::get_ffmpeg_info,
            aria2::set_ffmpeg_path,
            aria2::open_download_folder,
            aria2::open_youtube,
            aria2::pause_download,
            aria2::resume_download,
            aria2::get_default_download_dir,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}
