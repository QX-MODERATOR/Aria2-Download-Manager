// Hides the console window in release builds on Windows
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod aria2;
mod classifier;
mod strategy;

pub fn run() {
    tauri::Builder::default()
        .manage(aria2::AppState::new())
        .invoke_handler(tauri::generate_handler![
            aria2::start_download,
            aria2::stop_download,
            aria2::check_aria2,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}
