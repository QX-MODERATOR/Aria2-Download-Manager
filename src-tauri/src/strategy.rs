//! Builds progressively more permissive sets of extra aria2 CLI flags.
//! Each attempt uses one strategy from the ordered list until one succeeds.

#[derive(Debug, Clone)]
pub struct Strategy {
    pub name:        String,
    pub description: String,
    pub args:        Vec<String>,
}

// ── User-agent strings ────────────────────────────────────────────────────────

const UA_CHROME: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
     AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/124.0.0.0 Safari/537.36";

const UA_FIREFOX: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:125.0) \
     Gecko/20100101 Firefox/125.0";

const UA_EDGE: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
     AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/124.0.0.0 Safari/537.36 Edg/124.0.0.0";

const UA_SAFARI: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_4_1) \
     AppleWebKit/605.1.15 (KHTML, like Gecko) \
     Version/17.4.1 Safari/605.1.15";

// ── URL helpers ───────────────────────────────────────────────────────────────

/// Extract `scheme://host` and `scheme://host/` from a URL string.
fn url_origin_referer(url: &str) -> (String, String) {
    // Find "://" to split scheme from the rest
    if let Some(pos) = url.find("://") {
        let scheme = &url[..pos];
        let rest   = &url[pos + 3..];
        // host is everything up to the first "/" or end
        let host = rest.split('/').next().unwrap_or(rest);
        let origin  = format!("{}://{}", scheme, host);
        let referer = format!("{}/", origin);
        return (origin, referer);
    }
    (String::new(), String::new())
}

// ── Browser header builder ────────────────────────────────────────────────────

fn browser_headers(ua: &str, url: &str) -> Vec<String> {
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
    let chrome  = browser_headers(UA_CHROME,  url);
    let firefox = browser_headers(UA_FIREFOX, url);
    let edge    = browser_headers(UA_EDGE,    url);
    let safari  = browser_headers(UA_SAFARI,  url);

    vec![
        Strategy {
            name: "Default".into(),
            description: "Standard aria2 — no custom headers".into(),
            args: vec![],
        },
        Strategy {
            name: "Chrome Headers".into(),
            description: "Chrome UA + Origin/Referer spoof (hotlink bypass)".into(),
            args: chrome.clone(),
        },
        Strategy {
            name: "Firefox Headers".into(),
            description: "Firefox UA + browser headers".into(),
            args: firefox,
        },
        Strategy {
            name: "Edge Headers".into(),
            description: "Microsoft Edge UA + browser headers".into(),
            args: edge,
        },
        Strategy {
            name: "Chrome / 1-conn".into(),
            description: "Chrome headers + single connection to avoid CDN rate-limits".into(),
            args: { let mut a = chrome.clone(); a.extend(["-x".into(),"1".into(),"-s".into(),"1".into()]); a },
        },
        Strategy {
            name: "Chrome / Skip-TLS".into(),
            description: "Chrome headers + TLS certificate check disabled".into(),
            args: { let mut a = chrome.clone(); a.push("--check-certificate=false".into()); a },
        },
        Strategy {
            name: "Chrome / HTTP1.1".into(),
            description: "Chrome headers + HTTP/1.1 forced, no keep-alive".into(),
            args: { let mut a = chrome.clone(); a.extend(["--http-no-cache=true".into(),"--header=Connection: close".into()]); a },
        },
        Strategy {
            name: "Safari / Relaxed".into(),
            description: "Safari macOS UA + TLS disabled + 4 connections (last resort)".into(),
            args: {
                let mut a = safari;
                a.extend(["--check-certificate=false".into(),"-x".into(),"4".into(),"-s".into(),"4".into()]);
                a
            },
        },
    ]
}
