# Architecture

Aria2 Download Manager uses a lightweight HTML/JS frontend and a Rust backend bridged through Tauri commands and events.

## High-Level Flow

```
Frontend (HTML/JS)
  invoke("start_download", { url, dir })
  invoke("stop_download")
  invoke("check_aria2")
      |
      v
Tauri IPC
      |
      v
Rust Backend
  - classifier.rs: parses aria2 output for known error patterns
  - strategy.rs: builds retry header strategies
  - aria2.rs: manages aria2 process and emits progress/events
      |
      v
Frontend event listeners update UI state and logs
```

## Key Components

- src/index.html: UI and event handling
- src-tauri/src/aria2.rs: aria2 process lifecycle and retry logic
- src-tauri/src/classifier.rs: error classification
- src-tauri/src/strategy.rs: strategy builder
