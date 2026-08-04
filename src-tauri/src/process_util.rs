//! Hide console windows for child processes on Windows (no CMD flash).

use std::process::Command;

/// Prevent `cmd.exe` / console flashes when spawning CLI tools (aria2c, yt-dlp, taskkill).
pub fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}
