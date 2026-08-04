//! Parses aria2 output lines and classifies the type of failure so the
//! retry engine can choose the correct corrective strategy automatically.

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;

// ── Error codes ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    None,
    Forbidden403,
    NotFound404,
    RedirectErr,
    SslError,
    Timeout,
    Network,
    Checksum,
    Unknown,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::None => "none",
            ErrorCode::Forbidden403 => "403",
            ErrorCode::NotFound404 => "404",
            ErrorCode::RedirectErr => "redirect",
            ErrorCode::SslError => "ssl",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Network => "network",
            ErrorCode::Checksum => "checksum",
            ErrorCode::Unknown => "unknown",
        }
    }
}

// ── Pattern rules (compiled once) ────────────────────────────────────────────

static RULES: Lazy<Vec<(Regex, ErrorCode)>> = Lazy::new(|| {
    vec![
        (
            Regex::new(r"status=403|errorCode=22.*403|HTTP/[\d.]* 403").unwrap(),
            ErrorCode::Forbidden403,
        ),
        (
            Regex::new(r"status=404|HTTP/[\d.]* 404|Not Found").unwrap(),
            ErrorCode::NotFound404,
        ),
        (
            Regex::new(r"(?i)Redirecting to.*\b(403|404|error|login|auth)\b").unwrap(),
            ErrorCode::RedirectErr,
        ),
        (Regex::new(r"errorCode=22").unwrap(), ErrorCode::RedirectErr),
        (
            Regex::new(r"(?i)SSL|TLS|certificate|handshake").unwrap(),
            ErrorCode::SslError,
        ),
        (
            Regex::new(r"(?i)timed?\s?out|ETIMEDOUT|Connection reset").unwrap(),
            ErrorCode::Timeout,
        ),
        (
            Regex::new(r"(?i)ENETUNREACH|ECONNREFUSED|Network is unreachable").unwrap(),
            ErrorCode::Network,
        ),
        (
            Regex::new(r"(?i)checksum|CRC|hash mismatch").unwrap(),
            ErrorCode::Checksum,
        ),
        (
            Regex::new(r"\bERROR\b|\berror\b").unwrap(),
            ErrorCode::Unknown,
        ),
    ]
});

const PRIORITY: &[ErrorCode] = &[
    ErrorCode::Forbidden403,
    ErrorCode::RedirectErr,
    ErrorCode::NotFound404,
    ErrorCode::SslError,
    ErrorCode::Timeout,
    ErrorCode::Network,
    ErrorCode::Checksum,
    ErrorCode::Unknown,
];

// ── Classifier ────────────────────────────────────────────────────────────────

pub struct ErrorClassifier {
    seen: HashSet<&'static str>,
}

impl ErrorClassifier {
    pub fn new() -> Self {
        Self {
            seen: HashSet::new(),
        }
    }

    /// Feed one cleaned (ANSI-stripped) output line.
    pub fn feed(&mut self, line: &str) {
        for (pattern, code) in RULES.iter() {
            if pattern.is_match(line) {
                self.seen.insert(code.as_str());
                return;
            }
        }
    }

    /// Return the highest-priority error seen so far.
    pub fn classify(&self) -> ErrorCode {
        for code in PRIORITY {
            if self.seen.contains(code.as_str()) {
                return code.clone();
            }
        }
        ErrorCode::None
    }
}
