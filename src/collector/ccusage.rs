//! Cost summary from [ccusage](https://github.com/ryoppippi/ccusage): today,
//! the active 5-hour block and the last 7 days, at API list prices.
//!
//! ccusage rescans every transcript (several seconds), so each poller runs on
//! its own thread: at startup, every `REFRESH`, and on `refresh()`. The same
//! script runs locally (`sh -c`) and on remote hosts (`ssh host sh -s`).

use serde_json::Value;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

pub const REFRESH: Duration = Duration::from_secs(15 * 60);
/// Error reported when the host has no way to run ccusage (script exit 127).
pub const NOT_FOUND: &str = "ccusage not found";
const SPLIT: &str = "---ccusage-blocks---";

/// Prints today's date, `ccusage daily` JSON, a split line, then
/// `ccusage blocks --active` JSON. Both reports run in parallel.
pub const SCRIPT: &str = r#"export LC_ALL=C
export PATH="$PATH:$HOME/.local/share/mise/shims:$HOME/.local/bin:$HOME/.bun/bin:$HOME/.npm-global/bin"
if command -v ccusage >/dev/null 2>&1; then cc() { ccusage "$@"; }
elif command -v bunx >/dev/null 2>&1; then cc() { bunx ccusage "$@"; }
elif command -v npx >/dev/null 2>&1; then cc() { npx -y ccusage "$@"; }
else echo "ccusage not found" >&2; exit 127; fi
since=$(date -d '-6 days' +%Y%m%d 2>/dev/null || date -v-6d +%Y%m%d)
d=$(mktemp) b=$(mktemp)
cc daily --json --since "$since" >"$d" 2>/dev/null &
cc blocks --active --json >"$b" 2>/dev/null
wait
date +%Y-%m-%d
cat "$d"
echo
echo "---ccusage-blocks---"
cat "$b"
rm -f "$d" "$b"
"#;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub today_cost: f64,
    pub today_tokens: u64,
    /// Daily cost for the last 7 days, oldest first; the last entry is today.
    pub week: Vec<f64>,
    pub block: Option<Block>,
}

/// The active 5-hour billing block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Block {
    pub cost: f64,
    pub projected: f64,
    pub per_hour: f64,
    pub remaining_min: u32,
}

#[derive(Default)]
pub struct State {
    pub usage: Option<Usage>,
    pub error: Option<String>,
    pub loading: bool,
}

pub struct Handle {
    pub state: Arc<Mutex<State>>,
    refresh_tx: mpsc::Sender<()>,
}

impl Handle {
    pub fn refresh(&self) {
        let _ = self.refresh_tx.send(());
    }

    /// Hidden entirely when the host has no way to run ccusage.
    pub fn visible(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|s| s.error.as_deref() != Some(NOT_FOUND))
    }
}

/// Run ccusage on this machine.
pub fn start_local() -> Handle {
    start(|| {
        let out = std::process::Command::new("sh")
            .args(["-c", SCRIPT])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("sh: {e}"))?;
        script_result(out.status.code(), &out.stdout, &out.stderr)
    })
}

/// Map the script's exit status and output to `run`'s result.
pub fn script_result(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Result<String, String> {
    match code {
        Some(0) => Ok(String::from_utf8_lossy(stdout).into_owned()),
        Some(127) => Err(NOT_FOUND.into()),
        _ => {
            let err = String::from_utf8_lossy(stderr);
            Err(err
                .lines()
                .rfind(|l| !l.trim().is_empty())
                .unwrap_or("ccusage failed")
                .chars()
                .filter(|c| !c.is_control())
                .take(100)
                .collect())
        }
    }
}

/// Spawn a poller that calls `run` (which returns the script's stdout) now,
/// every `REFRESH`, and on `refresh()`.
pub fn start(run: impl Fn() -> Result<String, String> + Send + 'static) -> Handle {
    let state = Arc::new(Mutex::new(State {
        loading: true,
        ..State::default()
    }));
    let (tx, rx) = mpsc::channel::<()>();
    let st = state.clone();
    std::thread::spawn(move || loop {
        let started = Instant::now();
        let result = run().and_then(|out| parse(&out));
        if let Ok(mut s) = st.lock() {
            match result {
                // Keep the last good numbers when a refresh fails.
                Ok(u) => (s.usage, s.error) = (Some(u), None),
                Err(e) => s.error = Some(e),
            }
            s.loading = false;
        }
        match rx.recv_timeout(REFRESH.saturating_sub(started.elapsed())) {
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            _ => while rx.try_recv().is_ok() {},
        }
        if let Ok(mut s) = st.lock() {
            s.loading = true;
        }
    });
    Handle {
        state,
        refresh_tx: tx,
    }
}

pub fn parse(out: &str) -> Result<Usage, String> {
    let (head, blocks) = out.split_once(SPLIT).ok_or("unexpected ccusage output")?;
    let (today, daily) = head.trim_start().split_once('\n').unwrap_or((head, ""));
    let today = chrono::NaiveDate::parse_from_str(today.trim(), "%Y-%m-%d")
        .map_err(|_| "unexpected ccusage output")?;
    let daily: Value = serde_json::from_str(daily.trim()).map_err(|_| "ccusage daily: bad JSON")?;
    let blocks: Value = serde_json::from_str(blocks.trim()).unwrap_or(Value::Null);

    let day = |date: chrono::NaiveDate| {
        let key = date.format("%Y-%m-%d").to_string();
        daily["daily"].as_array().and_then(|days| {
            days.iter()
                .find(|d| d["period"].as_str().or(d["date"].as_str()) == Some(key.as_str()))
        })
    };
    let week = (0..7)
        .rev()
        .map(|back| {
            day(today - chrono::Duration::days(back))
                .and_then(|d| d["totalCost"].as_f64())
                .unwrap_or(0.0)
        })
        .collect();
    let today_entry = day(today);
    let block = blocks["blocks"]
        .as_array()
        .and_then(|b| b.iter().find(|b| b["isActive"].as_bool() == Some(true)))
        .map(|b| Block {
            cost: b["costUSD"].as_f64().unwrap_or(0.0),
            projected: b["projection"]["totalCost"].as_f64().unwrap_or(0.0),
            per_hour: b["burnRate"]["costPerHour"].as_f64().unwrap_or(0.0),
            remaining_min: b["projection"]["remainingMinutes"].as_u64().unwrap_or(0) as u32,
        });
    Ok(Usage {
        today_cost: today_entry
            .and_then(|d| d["totalCost"].as_f64())
            .unwrap_or(0.0),
        today_tokens: today_entry
            .and_then(|d| d["totalTokens"].as_u64())
            .unwrap_or(0),
        week,
        block,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_script_output() {
        let out = r#"2026-10-18
{"daily":[{"period":"2026-10-12","totalCost":10.5,"totalTokens":5},
          {"period":"2026-10-18","totalCost":669.9,"totalTokens":2009191372}]}
---ccusage-blocks---
{"blocks":[{"isActive":true,"costUSD":125.1,"burnRate":{"costPerHour":232.3},
            "projection":{"remainingMinutes":268,"totalCost":1162.8}}]}
"#;
        let u = parse(out).unwrap();
        assert_eq!(u.week, vec![10.5, 0.0, 0.0, 0.0, 0.0, 0.0, 669.9]);
        assert_eq!((u.today_cost, u.today_tokens), (669.9, 2009191372));
        assert_eq!(
            u.block,
            Some(Block {
                cost: 125.1,
                projected: 1162.8,
                per_hour: 232.3,
                remaining_min: 268
            })
        );
    }

    #[test]
    fn no_active_block_and_empty_days() {
        let out = "2026-10-18\n{\"daily\":[]}\n---ccusage-blocks---\n{\"blocks\":[]}\n";
        let u = parse(out).unwrap();
        assert_eq!(u.block, None);
        assert_eq!(u.week, vec![0.0; 7]);
        assert!(parse("garbage").is_err());
    }
}
