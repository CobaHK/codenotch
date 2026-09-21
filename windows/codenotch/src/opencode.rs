//! OpenCode Go plan usage adapter, ported from the Mac app's `OpenCodeProvider` / `OpenCodeUsage` /
//! `OpenCodeCredentials`.
//!
//! Data path (the same bargain the other borrowed-key providers strike):
//!   1. Credential: OpenCode's own sign-in (`opencode auth login`) writes
//!      `~/.local/share/opencode/auth.json` (or `%APPDATA%\opencode\auth.json` on Windows) — an
//!      object keyed by provider id. The `opencode-go` entry is the Zen Go plan's own API key; any
//!      other entry (`openai`, `google`, …) is that vendor's key and must not be claimed as OpenCode's.
//!      The entry is either the key string itself or an object carrying it under one of a few field
//!      names — both shapes have shipped across OpenCode versions.
//!   2. Endpoint: `GET https://opencode.ai/zen/go/v1/usage` (`Authorization: Bearer …`), the same
//!      one the OpenCode dashboard reads. Reply:
//!      ```text
//!      {"usage":{
//!        "rolling":{"status":"ok","percent":0,"resetsAt":"2026-09-06T12:31:06.611Z"},
//!        "weekly": {"status":"ok","percent":0,"resetsAt":"2026-09-07T00:00:00.611Z"},
//!        "monthly":{"status":"ok","percent":0,"resetsAt":"2026-10-03T13:09:45.611Z"}}}
//!      ```
//!      `percent` is *used*, matching the dashboard's "X% used" — no inversion needed.
//!
//! Two upstream quirks, both from the Mac app's comments: a valid key with no Go plan answers 401,
//! the same as a bad key — that reads as needsAuth here too, since nothing tells the two apart; and
//! Zen pay-as-you-go credit balance has no API at all, so this covers the Go windows only.
//!
//! Read only, never written; the token never reaches logs, events or the UI.

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::AppState;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const ENDPOINT: &str = "https://opencode.ai/zen/go/v1/usage";
const POLL_SECS: u64 = 300;

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `~/.local/share/opencode/auth.json`, then `%APPDATA%\opencode\auth.json` — the same two
/// candidates `glm.rs`'s `opencode_key` borrows a Z.ai key from.
fn auth_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = dirs::home_dir() {
        v.push(home.join(".local").join("share").join("opencode").join("auth.json"));
    }
    if let Some(appdata) = dirs::config_dir() {
        v.push(appdata.join("opencode").join("auth.json"));
    }
    v
}

fn store_path() -> PathBuf {
    crate::config::config_path().with_file_name("opencode.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .map(|mut s| {
            if !s.windows.is_empty() {
                s.status = "stale".into();
            }
            s
        })
        .unwrap_or_default()
}

fn persist(s: &UsageSnapshot) {
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(store_path(), t);
    }
}

pub fn present() -> bool {
    auth_paths().iter().any(|p| p.is_file())
}

// ---------------- Credentials ----------------

/// Non-empty strings only: an empty key is worse than a missing one, it is a request that cannot
/// succeed being sent all the same.
fn non_empty(v: Option<&serde_json::Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// The `opencode-go` entry is either the key itself or an object carrying it — both shapes have
/// shipped across OpenCode versions, matching the Mac app's `OpenCodeCredentials.load`.
fn token_from(entry: &serde_json::Value) -> Option<String> {
    if let Some(token) = non_empty(Some(entry)) {
        return Some(token);
    }
    let object = entry.as_object()?;
    ["key", "apiKey", "api_key", "token", "accessToken"].iter().find_map(|f| non_empty(object.get(*f)))
}

fn read_credentials() -> Option<String> {
    for p in auth_paths() {
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let Some(entry) = root.get("opencode-go") else { continue };
        if let Some(token) = token_from(entry) {
            return Some(token);
        }
    }
    None
}

/// For doctor: contains no secret values
pub fn probe() -> String {
    let paths = auth_paths();
    let Some(found) = paths.iter().find(|p| p.is_file()) else {
        return format!(
            "OpenCode: not found ({}) — run `opencode auth login` to connect the Go plan",
            paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(" or ")
        );
    };
    match read_credentials() {
        Some(t) => format!("OpenCode: opencode-go key found ({} chars) in {}", t.len(), found.display()),
        None => format!("OpenCode: {} exists but holds no opencode-go entry — connect Go inside OpenCode", found.display()),
    }
}

// ---------------- Parsing ----------------

/// Window ids in headline order: the ring means the rolling window, the current one, the same
/// subject Claude's session and Codex's primary are.
const WINDOWS: [(&str, &str); 3] = [("rolling", "5h limit"), ("weekly", "Weekly limit"), ("monthly", "Monthly limit")];

fn iso_ms(v: Option<&serde_json::Value>) -> Option<u64> {
    v.and_then(|x| x.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis().max(0) as u64)
}

pub fn parse_windows(v: &serde_json::Value) -> Vec<LimitWindow> {
    let Some(usage) = v.get("usage") else { return Vec::new() };
    WINDOWS
        .iter()
        .filter_map(|(id, label)| {
            let entry = usage.get(id)?;
            let used = entry.get("percent")?.as_f64()? / 100.0;
            Some(LimitWindow {
                id: (*id).into(),
                label: (*label).into(),
                used: used.clamp(0.0, 1.0),
                resets_at: iso_ms(entry.get("resetsAt")),
                ..Default::default()
            })
        })
        .collect()
}

enum FetchErr {
    NeedsAuth,
    /// A valid key not entitled to the Go plan: readable, but metering nothing — not an error.
    NothingMetered,
    RateLimited,
    Other(String),
}

fn fetch_once(token: &str) -> Result<serde_json::Value, FetchErr> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
    match agent.get(ENDPOINT).set("Authorization", &format!("Bearer {token}")).set("Accept", "application/json").call() {
        Ok(r) => r.into_json::<serde_json::Value>().map_err(|e| FetchErr::Other(format!("parse: {e}"))),
        Err(ureq::Error::Status(401, _)) => Err(FetchErr::NeedsAuth),
        Err(ureq::Error::Status(403, _)) => Err(FetchErr::NothingMetered),
        Err(ureq::Error::Status(429, _)) => Err(FetchErr::RateLimited),
        Err(ureq::Error::Status(code, _)) => Err(FetchErr::Other(format!("HTTP {code}"))),
        Err(e) => Err(FetchErr::Other(format!("{e}"))),
    }
}

fn read_once(prev: &UsageSnapshot) -> UsageSnapshot {
    let mut snap = prev.clone();
    let Some(token) = read_credentials() else {
        snap.status = "needsAuth".into();
        snap.note = "Connect Go inside OpenCode (opencode auth login) — the notch reads its key.".into();
        return snap;
    };
    match fetch_once(&token) {
        Ok(v) => {
            let windows = parse_windows(&v);
            snap.fetched_at = now_ms();
            if windows.is_empty() {
                snap.status = "none".into();
                snap.windows.clear();
                snap.note = "OpenCode returned no usable Go plan windows".into();
            } else {
                snap.status = "ok".into();
                snap.windows = windows;
                snap.note = "via OpenCode".into();
            }
        }
        Err(FetchErr::NeedsAuth) => {
            snap.status = "needsAuth".into();
            snap.note = "OpenCode key was rejected — run opencode auth login again".into();
        }
        Err(FetchErr::NothingMetered) => {
            snap.status = "none".into();
            snap.windows.clear();
            snap.note = "No OpenCode Go subscription on this key".into();
        }
        Err(FetchErr::RateLimited) => {
            snap.status = if snap.windows.is_empty() { "error" } else { "stale" }.into();
            snap.note = "OpenCode is rate limiting; the last reading stands".into();
        }
        Err(FetchErr::Other(msg)) => {
            snap.status = if snap.windows.is_empty() { "error" } else { "stale" }.into();
            snap.note = msg;
        }
    }
    snap
}

fn broadcast(app: &AppHandle, snap: UsageSnapshot) {
    let st = app.state::<AppState>();
    *st.opencode.lock().unwrap() = snap.clone();
    persist(&snap);
    let _ = app.emit("opencode", &snap);
}

fn sleep_interruptible(secs: u64) {
    for _ in 0..secs {
        if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        {
            let st = app.state::<AppState>();
            let snap = st.opencode.lock().unwrap().clone();
            let _ = app.emit("opencode", &snap);
        }
        if !present() {
            broadcast(&app, UsageSnapshot { status: "absent".into(), ..Default::default() });
            loop {
                sleep_interruptible(600); // OpenCode is not installed/signed in anywhere: look again every 10 minutes
                if present() {
                    break;
                }
            }
        }
        loop {
            let prev = {
                let st = app.state::<AppState>();
                let s = st.opencode.lock().unwrap().clone();
                s
            };
            let snap = read_once(&prev);
            if snap.status == "error" || snap.status == "stale" {
                crate::applog(&format!("opencode: {}", snap.note));
            }
            broadcast(&app, snap);
            sleep_interruptible(POLL_SECS);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_read_from_a_plain_string_entry() {
        let v: serde_json::Value = serde_json::from_str(r#"{"opencode-go":"sk-abc"}"#).unwrap();
        assert_eq!(token_from(v.get("opencode-go").unwrap()).as_deref(), Some("sk-abc"));
    }

    #[test]
    fn token_read_from_an_object_entry() {
        let v: serde_json::Value = serde_json::from_str(r#"{"opencode-go":{"type":"api","key":"sk-def"}}"#).unwrap();
        assert_eq!(token_from(v.get("opencode-go").unwrap()).as_deref(), Some("sk-def"));
    }

    #[test]
    fn an_empty_token_is_treated_as_missing() {
        let v: serde_json::Value = serde_json::from_str(r#"{"opencode-go":""}"#).unwrap();
        assert!(token_from(v.get("opencode-go").unwrap()).is_none());
    }

    #[test]
    fn usage_windows_become_limit_windows() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"usage":{
                "rolling":{"status":"ok","percent":12.5,"resetsAt":"2026-09-06T12:31:06.611Z"},
                "weekly": {"status":"ok","percent":40,"resetsAt":"2026-09-07T00:00:00Z"},
                "monthly":{"status":"ok","percent":5,"resetsAt":"2026-10-03T13:09:45.611Z"}}}"#,
        )
        .unwrap();
        let ws = parse_windows(&v);
        assert_eq!(ws.len(), 3);
        assert_eq!(ws[0].id, "rolling");
        assert!((ws[0].used - 0.125).abs() < 1e-9);
        assert!(ws[0].resets_at.is_some());
        assert_eq!(ws[1].id, "weekly");
        assert_eq!(ws[2].id, "monthly");
    }

    #[test]
    fn a_missing_usage_object_yields_no_windows() {
        let v: serde_json::Value = serde_json::from_str(r#"{}"#).unwrap();
        assert!(parse_windows(&v).is_empty());
    }

    /// Hits the real endpoint with whatever opencode-go key is on this machine. Opt-in, like
    /// `codex::tests::live_native_quota` — not run by CI, prints no secret, just the status and
    /// window ids/percentages that come back.
    #[test]
    #[ignore]
    fn live_fetch_prints_the_real_account_windows() {
        let snap = read_once(&UsageSnapshot::default());
        println!("status={} note={:?}", snap.status, snap.note);
        for w in &snap.windows {
            println!("  {} ({}): {:.1}% resets_at={:?}", w.id, w.label, w.used * 100.0, w.resets_at);
        }
    }
}
