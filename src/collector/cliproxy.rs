//! Per-account subscription quotas pulled from a CLIProxyAPI management server.
//!
//! Mirrors the management console's `#/quota` page: list auth files, then ask
//! the proxy to call each provider's usage endpoint with that account's token
//! (`POST /v0/management/api-call`, `$TOKEN$` is substituted server-side).
//! HTTP goes through `curl` so abtop keeps zero network dependencies; the
//! management key is passed on stdin so it never shows up in `ps`.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Passive refresh: the local auth-file listing only, no upstream calls.
pub const REFRESH: Duration = Duration::from_secs(60);
/// Provider usage endpoints answer 429 when polled often, so each account is
/// queried at most this often, and only when CLIProxyAPI has no recorded
/// rate-limit headers for the account yet.
const USAGE_EVERY: Duration = Duration::from_secs(60 * 60);
/// Minimum gap for a manual `r` refresh.
const USAGE_FORCED_EVERY: Duration = Duration::from_secs(5 * 60);
const DEFAULT_URL: &str = "http://localhost:8317";

const CLAUDE_USAGE: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_PROFILE: &str = "https://api.anthropic.com/api/oauth/profile";
const CODEX_USAGE: &str = "https://chatgpt.com/backend-api/wham/usage";

/// (usage response key, display label) in display order.
const CLAUDE_WINDOWS: &[(&str, &str)] =
    &[("seven_day", "7-day limit"), ("five_hour", "5-hour limit")];

#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub label: String,
    /// Remaining percentage (100 = untouched). None when the upstream omits it.
    pub remaining: Option<f64>,
    /// Unix seconds. None means "no reset pending".
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct Account {
    pub provider: String,
    pub name: String,
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    pub error: Option<String>,
    /// Disabled in CLIProxyAPI: quota is not fetched and it is left out of summaries.
    pub disabled: bool,
    /// Proxy has parked the account (e.g. after a 429) until this unix time.
    pub cooldown_until: Option<i64>,
    /// Codex subscription period end (`chatgpt_subscription_active_until`).
    pub renews_at: Option<i64>,
}

#[derive(Default)]
pub struct State {
    pub accounts: Vec<Account>,
    pub updated_at: Option<Instant>,
    pub error: Option<String>,
    pub loading: bool,
}

pub struct Handle {
    pub url: String,
    pub state: Arc<Mutex<State>>,
    refresh_tx: mpsc::Sender<()>,
}

impl Handle {
    pub fn refresh(&self) {
        let _ = self.refresh_tx.send(());
    }

    /// A handle with fixed state and no poller thread.
    #[cfg(test)]
    pub(crate) fn with_state(state: State) -> Self {
        Self {
            url: String::new(),
            state: Arc::new(Mutex::new(state)),
            refresh_tx: mpsc::channel().0,
        }
    }
}

/// Env vars win over config so the key can stay out of config.toml.
pub fn resolve(cfg_url: &str, cfg_key: &str) -> Option<(String, String)> {
    let env = |k| {
        std::env::var(k)
            .ok()
            .filter(|v: &String| !v.trim().is_empty())
    };
    let key = env("CLIPROXY_MANAGEMENT_KEY").unwrap_or_else(|| cfg_key.to_string());
    if key.trim().is_empty() {
        return None;
    }
    let url = env("CLIPROXY_URL")
        .or_else(|| (!cfg_url.trim().is_empty()).then(|| cfg_url.to_string()))
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    Some((
        url.trim_end_matches('/').to_string(),
        key.trim().to_string(),
    ))
}

/// Spawn the background poller: fetch now, then every REFRESH or on `refresh()`.
pub fn start(url: String, key: String) -> Handle {
    let state = Arc::new(Mutex::new(State {
        loading: true,
        ..State::default()
    }));
    let (tx, rx) = mpsc::channel::<()>();
    let st = state.clone();
    let base = url.clone();
    std::thread::spawn(move || {
        let mut cache = Cache::default();
        let mut forced = false;
        loop {
            let result = fetch_all(&base, &key, &mut cache, forced);
            if let Ok(mut s) = st.lock() {
                match result {
                    Ok(accounts) => {
                        s.accounts = accounts;
                        s.error = None;
                    }
                    Err(e) => s.error = Some(e),
                }
                s.updated_at = Some(Instant::now());
                s.loading = false;
            }
            forced = match rx.recv_timeout(REFRESH) {
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => false,
                Ok(()) => {
                    while rx.try_recv().is_ok() {}
                    true
                }
            };
            if let Ok(mut s) = st.lock() {
                s.loading = true;
            }
        }
    });
    Handle {
        url,
        state,
        refresh_tx: tx,
    }
}

/// Poller memory across refreshes, keyed by auth_index.
#[derive(Default)]
struct Cache {
    /// Claude plan from the profile endpoint (fetched once per account).
    plans: HashMap<String, String>,
    /// Accounts whose profile was already requested (success or not).
    plan_tried: HashSet<String>,
    usage: HashMap<String, UsageFetch>,
}

/// Last usage-endpoint attempt for one account. Windows survive failed
/// attempts so a 429 keeps showing the last good numbers.
struct UsageFetch {
    attempted: Instant,
    fetched_at: Option<i64>,
    windows: Vec<Window>,
    error: Option<String>,
}

fn fetch_all(
    base: &str,
    key: &str,
    cache: &mut Cache,
    forced: bool,
) -> Result<Vec<Account>, String> {
    let list = mgmt(base, key, "/v0/management/auth-files", None)?;
    let mut files: Vec<&Value> = list["files"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    files.retain(|f| matches!(provider_of(f).as_str(), "claude" | "codex"));

    // Usage endpoints are rate-limited upstream; only call them when due.
    let min_gap = if forced {
        USAGE_FORCED_EVERY
    } else {
        USAGE_EVERY
    };
    let now = now_secs();
    // (file, need_plan, need_usage); usage only when no passive data exists.
    let due: Vec<(&Value, bool, bool)> = files
        .iter()
        .copied()
        .filter(|f| !f["disabled"].as_bool().unwrap_or(false))
        .map(|f| {
            let idx = auth_index(f);
            let need_plan = provider_of(f) == "claude" && !cache.plan_tried.contains(&idx);
            let need_usage = passive_windows(f, now).0.is_empty()
                && cache
                    .usage
                    .get(&idx)
                    .is_none_or(|u| u.attempted.elapsed() >= min_gap);
            (f, need_plan, need_usage)
        })
        .filter(|(_, plan, usage)| *plan || *usage)
        .collect();
    let fetched: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = due
            .iter()
            .map(|&(f, plan, usage)| {
                s.spawn(move || (auth_index(f), fetch_usage(base, key, f, plan, usage)))
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    for (idx, (result, plan)) in fetched {
        cache.plan_tried.insert(idx.clone());
        if let Some(plan) = plan {
            cache.plans.insert(idx.clone(), plan);
        }
        let Some(result) = result else { continue };
        let entry = cache.usage.entry(idx).or_insert(UsageFetch {
            attempted: Instant::now(),
            fetched_at: None,
            windows: Vec::new(),
            error: None,
        });
        entry.attempted = Instant::now();
        match result {
            Ok(w) => {
                entry.windows = w;
                entry.fetched_at = Some(now_secs());
                entry.error = None;
            }
            Err(e) => entry.error = Some(e),
        }
    }

    let mut accounts: Vec<Account> = files.iter().map(|f| build_account(f, cache, now)).collect();
    accounts.sort_by(|a, b| {
        (a.provider != "claude", &a.provider, &a.name).cmp(&(
            b.provider != "claude",
            &b.provider,
            &b.name,
        ))
    });
    Ok(accounts)
}

/// Combine CLIProxyAPI's passive header observations with the cached
/// usage-endpoint result, taking each window from whichever is newer.
fn build_account(file: &Value, cache: &Cache, now: i64) -> Account {
    let idx = auth_index(file);
    let provider = provider_of(file);
    let (passive, observed_at) = passive_windows(file, now);
    let usage = cache.usage.get(&idx);
    let windows = merge_windows(
        usage.map(|u| u.windows.as_slice()).unwrap_or(&[]),
        usage.and_then(|u| u.fetched_at),
        &passive,
        observed_at,
    );
    let codex_plan = str_of(&file["quota"]["signals"]["X-Codex-Plan-Type"])
        .or_else(|| str_of(&file["id_token"]["plan_type"]));
    Account {
        name: str_of(&file["email"])
            .or_else(|| str_of(&file["name"]))
            .unwrap_or_else(|| idx.clone()),
        plan: cache
            .plans
            .get(&idx)
            .cloned()
            .or_else(|| codex_plan.map(|p| plan_label(&p))),
        error: windows
            .is_empty()
            .then(|| usage.and_then(|u| u.error.clone()))
            .flatten(),
        windows,
        disabled: file["disabled"].as_bool().unwrap_or(false),
        cooldown_until: ts(&file["next_retry_after"]).filter(|t| *t > now),
        renews_at: ts(&file["id_token"]["chatgpt_subscription_active_until"]),
        provider,
    }
}

/// Windows from the rate-limit headers CLIProxyAPI recorded on real traffic
/// (zero upstream calls), plus when they were observed.
pub fn passive_windows(file: &Value, now: i64) -> (Vec<Window>, Option<i64>) {
    let signals = file["quota"]["signals"].as_object();
    let get = |name: &str| {
        signals?
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| num(v))
    };
    // A window whose reset already passed has rolled over since the observation.
    let window = |label: String, used: f64, reset: Option<f64>| {
        let reset = reset.map(|r| r as i64);
        match reset {
            Some(r) if r <= now => Window {
                label,
                remaining: Some(100.0),
                resets_at: None,
            },
            _ => Window {
                label,
                remaining: Some(remaining(used)),
                resets_at: reset,
            },
        }
    };
    let mut out = Vec::new();
    if provider_of(file) == "claude" {
        for (label, key) in [("7-day limit", "7d"), ("5-hour limit", "5h")] {
            let prefix = format!("anthropic-ratelimit-unified-{key}");
            if let Some(util) = get(&format!("{prefix}-utilization")) {
                let reset = get(&format!("{prefix}-reset"));
                out.push(window(label.to_string(), util * 100.0, reset));
            }
        }
    } else {
        for (i, slot) in ["primary", "secondary"].iter().enumerate() {
            let prefix = format!("x-codex-{slot}");
            if let Some(used) = get(&format!("{prefix}-used-percent")) {
                let secs = get(&format!("{prefix}-window-minutes")).map(|m| m as i64 * 60);
                let label = format!("{} limit", window_name(secs, i));
                out.push(window(label, used, get(&format!("{prefix}-reset-at"))));
            }
        }
    }
    (out, ts(&file["quota"]["observed_at"]))
}

/// Per label, keep the value from the newer source; labels only one source
/// reports are kept either way.
pub fn merge_windows(
    usage: &[Window],
    usage_at: Option<i64>,
    passive: &[Window],
    passive_at: Option<i64>,
) -> Vec<Window> {
    let passive_newer = passive_at.unwrap_or(i64::MIN) >= usage_at.unwrap_or(i64::MIN);
    let mut out = usage.to_vec();
    for p in passive {
        match out.iter_mut().find(|w| w.label == p.label) {
            Some(w) if passive_newer => *w = p.clone(),
            Some(_) => {}
            None => out.push(p.clone()),
        }
    }
    out
}

/// Usage-endpoint windows (when `need_usage`) and the Claude plan (when
/// `need_plan`) for one account.
fn fetch_usage(
    base: &str,
    key: &str,
    file: &Value,
    need_plan: bool,
    need_usage: bool,
) -> (Option<Result<Vec<Window>, String>>, Option<String>) {
    let idx = auth_index(file);
    if provider_of(file) == "claude" {
        let headers = json!({
            "Authorization": "Bearer $TOKEN$",
            "Content-Type": "application/json",
            "User-Agent": "claude-cli/2.1.280 (external, cli)",
            "anthropic-beta": "oauth-2025-04-20",
        });
        let plan = need_plan
            .then(|| api_call(base, key, &idx, CLAUDE_PROFILE, &headers).ok())
            .flatten()
            .and_then(|p| claude_plan(&p));
        let usage = need_usage
            .then(|| api_call(base, key, &idx, CLAUDE_USAGE, &headers).map(|u| parse_claude(&u)));
        return (usage, plan);
    }
    let mut headers = json!({
        "Authorization": "Bearer $TOKEN$",
        "Content-Type": "application/json",
        "User-Agent": "codex-tui/0.149.1 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.149.1)",
    });
    if let Some(id) = str_of(&file["id_token"]["chatgpt_account_id"]) {
        headers["Chatgpt-Account-Id"] = Value::String(id);
    }
    let usage = need_usage.then(|| {
        api_call(base, key, &idx, CODEX_USAGE, &headers).map(|u| parse_codex(&u, now_secs()))
    });
    (usage, None)
}

fn api_call(base: &str, key: &str, idx: &str, url: &str, header: &Value) -> Result<Value, String> {
    let body = json!({ "auth_index": idx, "method": "GET", "url": url, "header": header });
    let resp = mgmt(base, key, "/v0/management/api-call", Some(&body))?;
    let code = resp["status_code"].as_i64().unwrap_or(0);
    let text = resp["body"].as_str().unwrap_or("");
    let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    if !(200..300).contains(&code) {
        let msg = str_of(&parsed["error"]["message"])
            .or_else(|| str_of(&parsed["error"]))
            .or_else(|| str_of(&parsed["detail"]))
            .unwrap_or_default();
        return Err(clean(&format!("upstream HTTP {code} {msg}")));
    }
    if parsed.is_null() {
        return Err("upstream returned non-JSON body".into());
    }
    Ok(parsed)
}

/// Call the management API through curl. The key goes in via `-H @-` (stdin).
fn mgmt(base: &str, key: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "-m", "30", "-H", "@-", "-w", "\n%{http_code}"]);
    if let Some(b) = body {
        cmd.args(["-H", "Content-Type: application/json", "--data-raw"]);
        cmd.arg(b.to_string());
    }
    cmd.arg(format!("{base}{path}"));
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("curl: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "Authorization: Bearer {key}");
    }
    let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return Err(clean(&String::from_utf8_lossy(&out.stderr)));
    }
    let (payload, code) = stdout.rsplit_once('\n').unwrap_or(("", &stdout));
    let value: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    if code.trim() != "200" {
        let msg = str_of(&value["error"]).unwrap_or_default();
        return Err(clean(&format!("cliproxy HTTP {} {msg}", code.trim())));
    }
    Ok(value)
}

/// Pooled view of one window label across a provider's accounts.
#[derive(Debug, PartialEq)]
pub struct Summary {
    pub label: String,
    /// Sum of remaining percentages (e.g. 43 of 300).
    pub sum: f64,
    /// Number of accounts reporting this window.
    pub count: usize,
    /// Soonest future reset among those accounts.
    pub next_reset: Option<i64>,
}

impl Summary {
    /// Remaining share of the whole pool, 0–100.
    pub fn pooled(&self) -> f64 {
        self.sum / self.count as f64
    }
}

/// Per-label pooled remaining quota for `provider`, in first-seen label order.
pub fn summarize(accounts: &[Account], provider: &str, now: i64) -> Vec<Summary> {
    let mut out: Vec<Summary> = Vec::new();
    let windows = accounts
        .iter()
        .filter(|a| a.provider == provider)
        .flat_map(|a| &a.windows);
    for w in windows {
        let Some(r) = w.remaining else { continue };
        let reset = w.resets_at.filter(|t| *t > now);
        match out.iter_mut().find(|s| s.label == w.label) {
            Some(s) => {
                s.sum += r;
                s.count += 1;
                s.next_reset = s.next_reset.into_iter().chain(reset).min();
            }
            None => out.push(Summary {
                label: w.label.clone(),
                sum: r,
                count: 1,
                next_reset: reset,
            }),
        }
    }
    out
}

// ── response parsing (pure, unit-tested) ─────────────────────────────────────

pub fn parse_claude(v: &Value) -> Vec<Window> {
    CLAUDE_WINDOWS
        .iter()
        .filter(|(key, _)| v[*key].get("utilization").is_some())
        .map(|(key, label)| Window {
            label: label.to_string(),
            remaining: num(&v[*key]["utilization"]).map(remaining),
            resets_at: ts(&v[*key]["resets_at"]),
        })
        .collect()
}

pub fn parse_codex(v: &Value, now: i64) -> Vec<Window> {
    let mut out = Vec::new();
    codex_windows(&v["rate_limit"], "", now, &mut out);
    codex_windows(&v["code_review_rate_limit"], "Code review ", now, &mut out);
    if let Some(extra) = v["additional_rate_limits"].as_array() {
        for (i, e) in extra.iter().enumerate() {
            let name = str_of(&e["limit_name"])
                .or_else(|| str_of(&e["metered_feature"]))
                .unwrap_or_else(|| format!("Additional {}", i + 1));
            codex_windows(&e["rate_limit"], &format!("{name} "), now, &mut out);
        }
    }
    out
}

fn codex_windows(rl: &Value, prefix: &str, now: i64, out: &mut Vec<Window>) {
    if !rl.is_object() {
        return;
    }
    let exhausted =
        rl["limit_reached"].as_bool() == Some(true) || rl["allowed"].as_bool() == Some(false);
    for (i, key) in ["primary_window", "secondary_window"].iter().enumerate() {
        let w = &rl[*key];
        if !w.is_object() {
            continue;
        }
        let secs = num(&w["limit_window_seconds"]).map(|s| s as i64);
        let name = window_name(secs, i);
        let label = if prefix.is_empty() {
            format!("{name} limit")
        } else {
            format!("{prefix}{} limit", name.to_lowercase())
        };
        let used = num(&w["used_percent"]).or(exhausted.then_some(100.0));
        let resets_at = ts(&w["reset_at"]).or_else(|| {
            num(&w["reset_after_seconds"])
                .filter(|s| *s > 0.0)
                .map(|s| now + s as i64)
        });
        out.push(Window {
            label,
            remaining: used.map(remaining),
            resets_at,
        });
    }
}

/// Codex window name from its length; position decides when the length is unknown.
fn window_name(secs: Option<i64>, index: usize) -> &'static str {
    match secs {
        Some(18_000) => "5-hour",
        Some(604_800) => "Weekly",
        Some(s) if (2_419_200..=2_678_400).contains(&s) => "Monthly",
        _ if index == 0 => "5-hour",
        _ => "Weekly",
    }
}

fn claude_plan(profile: &Value) -> Option<String> {
    let org = &profile["organization"];
    if org["organization_type"].as_str() == Some("claude_team")
        && org["subscription_status"].as_str() == Some("active")
    {
        return Some("Team".into());
    }
    let flag = |k: &str| profile["account"][k].as_bool();
    match (flag("has_claude_max"), flag("has_claude_pro")) {
        (Some(true), _) => Some("Max".into()),
        (_, Some(true)) => Some("Pro".into()),
        (Some(false), Some(false)) => Some("Free".into()),
        _ => None,
    }
}

fn remaining(used: f64) -> f64 {
    (100.0 - used).clamp(0.0, 100.0)
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|n: &f64| n.is_finite())
}

/// Unix seconds from epoch secs/ms or an RFC 3339 string.
fn ts(v: &Value) -> Option<i64> {
    if let Some(s) = v.as_str() {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s.trim()) {
            return Some(dt.timestamp());
        }
    }
    let n = num(v).filter(|n| *n > 0.0)?;
    Some(if n < 1e11 {
        n as i64
    } else {
        (n / 1000.0) as i64
    })
}

fn str_of(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn auth_index(f: &Value) -> String {
    str_of(&f["auth_index"]).unwrap_or_else(|| f["auth_index"].to_string())
}

fn provider_of(f: &Value) -> String {
    str_of(&f["provider"])
        .or_else(|| str_of(&f["type"]))
        .unwrap_or_default()
        .to_lowercase()
}

/// Codex plan ids → the names the management console uses.
fn plan_label(p: &str) -> String {
    match p.to_lowercase().as_str() {
        "pro" => "Pro 200".into(),
        "prolite" => "Pro 100".into(),
        "business_premium" => "Business Premium".into(),
        _ => title_case(p),
    }
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Single line, no control chars (upstream text is untrusted), bounded length.
fn clean(s: &str) -> String {
    let one: String = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    one.chars().take(120).collect()
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_usage_windows() {
        let v = json!({
            "five_hour": { "utilization": 40.0, "resets_at": "2026-10-18T16:30:00.123456+00:00" },
            "seven_day": { "utilization": 53, "resets_at": null },
            "seven_day_opus": null,
            "iguana_necktie": { "utilization": 99 }
        });
        let w = parse_claude(&v);
        // Model-scoped windows (Fable, Opus, ...) are intentionally ignored.
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].label, "7-day limit");
        assert_eq!(w[0].remaining, Some(47.0));
        assert_eq!(w[0].resets_at, None);
        assert_eq!(w[1].label, "5-hour limit");
        assert_eq!(w[1].remaining, Some(60.0));
        assert_eq!(w[1].resets_at, Some(1792341000));
    }

    #[test]
    fn codex_usage_windows() {
        let v = json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true, "limit_reached": false,
                "primary_window": { "used_percent": 16, "limit_window_seconds": 604800, "reset_at": 1792800000 },
                "secondary_window": null
            },
            "code_review_rate_limit": {
                "allowed": false,
                "primary_window": { "limit_window_seconds": 18000, "reset_after_seconds": 60 }
            }
        });
        let w = parse_codex(&v, 1000);
        assert_eq!(
            w,
            vec![
                Window {
                    label: "Weekly limit".into(),
                    remaining: Some(84.0),
                    resets_at: Some(1792800000)
                },
                Window {
                    label: "Code review 5-hour limit".into(),
                    remaining: Some(0.0),
                    resets_at: Some(1060)
                },
            ]
        );
    }

    #[test]
    fn summarize_pools_accounts() {
        let acct = |provider: &str, windows: Vec<Window>| Account {
            provider: provider.into(),
            name: String::new(),
            plan: None,
            windows,
            error: None,
            disabled: false,
            cooldown_until: None,
            renews_at: None,
        };
        let win = |label: &str, remaining: Option<f64>, resets_at: Option<i64>| Window {
            label: label.into(),
            remaining,
            resets_at,
        };
        let accounts = vec![
            acct(
                "claude",
                vec![
                    win("7d", Some(43.0), Some(500)),
                    win("5h", Some(100.0), None),
                ],
            ),
            acct(
                "claude",
                vec![win("7d", Some(0.0), Some(200)), win("5h", None, Some(50))],
            ),
            acct("codex", vec![win("7d", Some(80.0), Some(100))]),
        ];
        let s = summarize(&accounts, "claude", 100);
        assert_eq!(
            s,
            vec![
                Summary {
                    label: "7d".into(),
                    sum: 43.0,
                    count: 2,
                    next_reset: Some(200)
                },
                Summary {
                    label: "5h".into(),
                    sum: 100.0,
                    count: 1,
                    next_reset: None
                },
            ]
        );
        assert_eq!(s[0].pooled(), 21.5);
    }

    #[test]
    fn passive_signals_become_windows() {
        let claude = json!({
            "provider": "claude",
            "quota": { "observed_at": "2026-10-19T03:43:14.95+08:00", "signals": {
                "Anthropic-Ratelimit-Unified-7d-Utilization": "0.58",
                "Anthropic-Ratelimit-Unified-7d-Reset": "2000",
                "Anthropic-Ratelimit-Unified-5h-Utilization": "0.63",
                "Anthropic-Ratelimit-Unified-5h-Reset": "500"
            }}
        });
        let (w, at) = passive_windows(&claude, 1000);
        assert_eq!(at, Some(1792352594));
        assert_eq!(w[0].label, "7-day limit");
        assert!((w[0].remaining.unwrap() - 42.0).abs() < 1e-9);
        assert_eq!(w[0].resets_at, Some(2000));
        // 5h reset already passed → rolled over to a fresh window
        assert_eq!(
            w[1],
            Window {
                label: "5-hour limit".into(),
                remaining: Some(100.0),
                resets_at: None
            }
        );

        let codex = json!({ "provider": "codex", "quota": { "signals": {
            "X-Codex-Primary-Used-Percent": "21",
            "X-Codex-Primary-Window-Minutes": "10080",
            "X-Codex-Primary-Reset-At": "5000"
        }}});
        let (w, _) = passive_windows(&codex, 1000);
        assert_eq!(
            w,
            vec![Window {
                label: "Weekly limit".into(),
                remaining: Some(79.0),
                resets_at: Some(5000)
            }]
        );
    }

    #[test]
    fn merge_prefers_newer_source() {
        let win = |label: &str, r: f64| Window {
            label: label.into(),
            remaining: Some(r),
            resets_at: None,
        };
        let usage = vec![win("7d", 10.0), win("other", 100.0)];
        let passive = vec![win("7d", 40.0), win("5h", 90.0)];
        // passive newer: overrides 7d, keeps usage-only window, adds 5h
        assert_eq!(
            merge_windows(&usage, Some(1), &passive, Some(2)),
            vec![win("7d", 40.0), win("other", 100.0), win("5h", 90.0)]
        );
        // usage newer: keeps its 7d
        assert_eq!(
            merge_windows(&usage, Some(3), &passive, Some(2))[0],
            win("7d", 10.0)
        );
        // no usage yet: passive only
        assert_eq!(merge_windows(&[], None, &passive, Some(2)), passive);
    }

    #[test]
    fn plan_and_resolve() {
        assert_eq!(
            claude_plan(&json!({"account": {"has_claude_max": true}})),
            Some("Max".into())
        );
        assert_eq!(claude_plan(&json!({})), None);
        assert_eq!(plan_label("prolite"), "Pro 100");
        assert_eq!(plan_label("plus"), "Plus");
        assert_eq!(ts(&json!(1792800000123u64)), Some(1792800000));
    }
}
