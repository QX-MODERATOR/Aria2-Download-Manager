//! Builds progressively more permissive sets of extra aria2 CLI flags.
//! Each attempt uses one strategy from the ordered list until one succeeds.

#[derive(Debug, Clone)]
pub struct Strategy {
    pub name: String,
    pub description: String,
    pub args: Vec<String>,
}

// ── User-agent strings ────────────────────────────────────────────────────────

const UA_DESKTOP: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
     AppleWebKit/537.36 (KHTML, like Gecko) Safari/537.36";

const UA_SAFARI: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_4_1) \
     AppleWebKit/605.1.15 (KHTML, like Gecko) \
     Version/17.4.1 Safari/605.1.15";

// ── URL helpers ───────────────────────────────────────────────────────────────

/// Extract `scheme://host` and `scheme://host/` from a URL string.
fn url_origin_referer(url: &str) -> (String, String) {
    // Find "://" to split scheme from the rest
    if let Some(pos) = url.find("://") {
        let scheme = &url[..pos];
        let rest = &url[pos + 3..];
        // host is everything up to the first "/" or end
        let host = rest.split('/').next().unwrap_or(rest);
        let origin = format!("{}://{}", scheme, host);
        let referer = format!("{}/", origin);
        return (origin, referer);
    }
    (String::new(), String::new())
}

// ── Browser header builder ────────────────────────────────────────────────────

fn http_headers(ua: &str, url: &str) -> Vec<String> {
    let (origin, referer) = url_origin_referer(url);
    vec![
        format!("--user-agent={ua}"),
        "--header=Accept: */*".into(),
        "--header=Accept-Language: en-US,en;q=0.9,ar;q=0.8".into(),
        "--header=Accept-Encoding: gzip, deflate, br".into(),
        format!("--header=Referer: {referer}"),
        format!("--header=Origin: {origin}"),
        "--header=Connection: keep-alive".into(),
        "--header=DNT: 1".into(),
        "--header=Sec-Fetch-Dest: video".into(),
        "--header=Sec-Fetch-Mode: no-cors".into(),
        "--header=Sec-Fetch-Site: same-origin".into(),
        "--header=Cache-Control: no-cache".into(),
        "--header=Pragma: no-cache".into(),
    ]
}

// ── Strategy list ─────────────────────────────────────────────────────────────

pub fn build_strategies(url: &str) -> Vec<Strategy> {
    let desktop = http_headers(UA_DESKTOP, url);
    let safari = http_headers(UA_SAFARI, url);

    vec![
        // ── Safe baseline ────────────────────────────────────────────────────
        // Single connection avoids CDN range-request stalls caused by parallel
        // splits (the 80 KiB / freeze pattern seen with multi-connection).
        Strategy {
            name: "Default".into(),
            description: "Single connection — safest starting point; avoids CDN range stalls"
                .into(),
            args: vec!["-x".into(), "1".into(), "-s".into(), "1".into()],
        },
        Strategy {
            name: "Single Connection".into(),
            description: "Single connection + explicit IPv4 — explicit range-stall bypass".into(),
            args: vec![
                "-x".into(),
                "1".into(),
                "-s".into(),
                "1".into(),
                "--disable-ipv6=true".into(),
            ],
        },
        Strategy {
            name: "IPv4 Only".into(),
            description: "Explicit IPv4, default connection count — isolates IPv6 routing issues"
                .into(),
            args: vec!["--disable-ipv6=true".into()],
        },
        Strategy {
            name: "Reduced Split".into(),
            description: "4 connections, 4 splits — standard parallel download attempt".into(),
            args: vec!["-x".into(), "4".into(), "-s".into(), "4".into()],
        },
        // ── Header / UA spoofing ─────────────────────────────────────────────
        Strategy {
            name: "Desktop Headers".into(),
            description: "Desktop UA + Origin/Referer spoof (hotlink bypass)".into(),
            args: desktop.clone(),
        },
        Strategy {
            name: "Desktop / 1-conn".into(),
            description: "Desktop headers + single connection to avoid CDN rate-limits".into(),
            args: {
                let mut a = desktop.clone();
                a.extend(["-x".into(), "1".into(), "-s".into(), "1".into()]);
                a
            },
        },
        Strategy {
            name: "Desktop / Skip-TLS".into(),
            description: "Desktop headers + TLS certificate check disabled".into(),
            args: {
                let mut a = desktop.clone();
                a.push("--check-certificate=false".into());
                a
            },
        },
        Strategy {
            name: "Desktop / HTTP1.1".into(),
            description: "Desktop headers + HTTP/1.1 forced, no keep-alive".into(),
            args: {
                let mut a = desktop.clone();
                a.extend([
                    "--http-no-cache=true".into(),
                    "--header=Connection: close".into(),
                ]);
                a
            },
        },
        Strategy {
            name: "Safari / Relaxed".into(),
            description: "Safari macOS UA + TLS disabled + 4 connections (last resort)".into(),
            args: {
                let mut a = safari;
                a.extend([
                    "--check-certificate=false".into(),
                    "-x".into(),
                    "4".into(),
                    "-s".into(),
                    "4".into(),
                ]);
                a
            },
        },
    ]
}
