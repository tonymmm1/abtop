//! Remote host monitoring over SSH (`~/.config/abtop/hosts.toml`).
//!
//! Each host gets a background thread that runs a small read-only POSIX shell
//! probe (`ssh host sh -s`, script on stdin) every `POLL`. Nothing is installed
//! remotely. Auth is whatever `ssh` would use for that host (default key,
//! agent, `~/.ssh/config`); `BatchMode` makes it fail instead of prompting.

use crate::host_info::{parse_cpu_line, parse_nvidia_smi, GpuInfo, KIB_PER_GIB};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const POLL: Duration = Duration::from_secs(10);
/// A transcript touched this recently counts as "working" (same rule as local).
const WORKING_SECS: i64 = 30;

/// Tab-separated probe output: host/stat/mem/load/gpu/proc records.
const PROBE: &str = r#"export LC_ALL=C
printf 'host\t%s\n' "$(hostname)"
printf 'stat\t%s\n' "$(head -n1 /proc/stat)"
awk '/^MemTotal:/{t=$2} /^MemAvailable:/{a=$2} END{printf "mem\t%s\t%s\n", t, a}' /proc/meminfo
printf 'load\t%s\n' "$(cut -d' ' -f1 /proc/loadavg)"
printf 'cores\t%s\n' "$(grep -c '^cpu[0-9]' /proc/stat)"
if command -v nvidia-smi >/dev/null 2>&1; then
  nvidia-smi --query-gpu=name,utilization.gpu,memory.used,memory.total --format=csv,noheader,nounits 2>/dev/null | sed 's/^/gpu\t/'
fi
now=$(date +%s)
for d in /proc/[0-9]*; do
  comm=$(cat "$d/comm" 2>/dev/null) || continue
  case "$comm" in claude|codex) ;; *) continue ;; esac
  pid=${d#/proc/}
  ppid=$(awk '{print $4}' "$d/stat" 2>/dev/null)
  case "$(cat "/proc/$ppid/comm" 2>/dev/null)" in claude|codex) continue ;; esac
  cwd=$(readlink "$d/cwd" 2>/dev/null)
  f=
  if [ "$comm" = claude ]; then
    sid=$(sed -n 's/.*"sessionId" *: *"\([^"]*\)".*/\1/p' "$HOME/.claude/sessions/$pid.json" 2>/dev/null)
    [ -n "$sid" ] && f=$(ls -t "$HOME"/.claude/projects/*/"$sid".jsonl 2>/dev/null | head -n1)
  else
    f=$(ls -l "$d/fd" 2>/dev/null | sed -n 's/.*-> \(.*rollout-.*\.jsonl\)$/\1/p' | head -n1)
  fi
  age=-
  [ -n "$f" ] && m=$(stat -c %Y "$f" 2>/dev/null) && age=$((now - m))
  br=$(git -C "$cwd" branch --show-current 2>/dev/null)
  dirty=$(git -C "$cwd" status --porcelain 2>/dev/null | wc -l)
  printf 'proc\t%s\t%s\t%s\t%s\t%s\t%s\n' "$comm" "$pid" "$age" "$br" "$dirty" "$cwd"
done
"#;

#[derive(Clone, Debug, PartialEq)]
pub struct HostConfig {
    /// Section name: ssh alias, hostname or IP.
    pub name: String,
    pub user: Option<String>,
    pub port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteSession {
    pub agent: String,
    pub pid: u32,
    /// None when the transcript could not be located.
    pub working: Option<bool>,
    pub project: String,
    pub cwd: String,
    pub branch: String,
    pub dirty: u32,
}

#[derive(Clone, Debug, Default)]
pub struct HostStatus {
    pub name: String,
    pub hostname: String,
    pub error: Option<String>,
    pub updated_at: Option<Instant>,
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub mem_used_gib: f64,
    pub mem_total_gib: f64,
    pub cores: u32,
    pub load1: f64,
    pub gpus: Vec<GpuInfo>,
    pub sessions: Vec<RemoteSession>,
}

pub struct Handle {
    pub hosts: Arc<Mutex<Vec<HostStatus>>>,
    /// One ccusage poller per host, same order as `hosts`.
    pub ccusage: Vec<super::ccusage::Handle>,
}

fn hosts_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("abtop").join("hosts.toml"))
}

pub fn load_hosts() -> Vec<HostConfig> {
    hosts_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse_hosts(&s))
        .unwrap_or_default()
}

/// `[name]` sections with optional `user = "..."` and `port = N`.
pub fn parse_hosts(content: &str) -> Vec<HostConfig> {
    let mut hosts: Vec<HostConfig> = Vec::new();
    // False after an invalid section so its keys don't leak into the previous host.
    let mut in_host = false;
    for line in content.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.trim().trim_matches('"').trim();
            in_host = !name.is_empty();
            if in_host {
                hosts.push(HostConfig {
                    name: name.to_string(),
                    user: None,
                    port: None,
                });
            }
            continue;
        }
        let (true, Some(host), Some((key, val))) =
            (in_host, hosts.last_mut(), line.split_once('='))
        else {
            continue;
        };
        let val = val.trim().trim_matches('"').trim_matches('\'').trim();
        match key.trim() {
            "user" if !val.is_empty() => host.user = Some(val.to_string()),
            "port" => host.port = val.parse().ok(),
            _ => {}
        }
    }
    hosts
}

/// One polling thread per host; results land in `Handle::hosts` (config order).
pub fn start(configs: Vec<HostConfig>) -> Handle {
    let hosts = Arc::new(Mutex::new(
        configs
            .iter()
            .map(|c| HostStatus {
                name: c.name.clone(),
                ..HostStatus::default()
            })
            .collect::<Vec<_>>(),
    ));
    if let Some(dir) = control_dir() {
        let _ = std::fs::create_dir_all(dir);
    }
    let ccusage = configs
        .iter()
        .map(|cfg| {
            let cfg = cfg.clone();
            super::ccusage::start(move || run_ssh(&cfg, super::ccusage::SCRIPT, true))
        })
        .collect();
    for (i, cfg) in configs.into_iter().enumerate() {
        let hosts = hosts.clone();
        std::thread::spawn(move || {
            let mut prev_cpu: Option<(u64, u64)> = None;
            loop {
                let result = run_ssh(&cfg, PROBE, false);
                if let Ok(mut all) = hosts.lock() {
                    let h = &mut all[i];
                    match result {
                        Ok(out) => {
                            let p = parse_probe(&out);
                            h.cpu_pct = match (prev_cpu, p.cpu) {
                                (Some((pb, pi)), Some((b, i))) => {
                                    let (db, di) = (b.saturating_sub(pb), i.saturating_sub(pi));
                                    if db + di > 0 {
                                        db as f64 / (db + di) as f64 * 100.0
                                    } else {
                                        0.0
                                    }
                                }
                                _ => 0.0,
                            };
                            prev_cpu = p.cpu;
                            h.hostname = p.hostname;
                            h.mem_pct = p.mem_pct;
                            h.mem_used_gib = p.mem_used_gib;
                            h.mem_total_gib = p.mem_total_gib;
                            h.cores = p.cores;
                            h.load1 = p.load1;
                            h.gpus = p.gpus;
                            h.sessions = p.sessions;
                            h.error = None;
                            h.updated_at = Some(Instant::now());
                        }
                        Err(e) => h.error = Some(e),
                    }
                }
                std::thread::sleep(POLL);
            }
        });
    }
    Handle { hosts, ccusage }
}

fn control_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("abtop"))
}

/// Run `script` on the host with `sh -s`. With `ccusage`, the exit status is
/// mapped by `ccusage::script_result` (127 = not installed).
fn run_ssh(cfg: &HostConfig, script: &str, ccusage: bool) -> Result<String, String> {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=2",
    ]);
    // Reuse one connection per host between polls.
    if let Some(dir) = control_dir() {
        cmd.args(["-o", "ControlMaster=auto", "-o", "ControlPersist=60"]);
        cmd.arg("-o")
            .arg(format!("ControlPath={}/ssh-%C", dir.display()));
    }
    if let Some(port) = cfg.port {
        cmd.args(["-p", &port.to_string()]);
    }
    if let Some(user) = &cfg.user {
        cmd.args(["-l", user]);
    }
    cmd.args(["--", &cfg.name, "sh", "-s"]);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ssh: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| format!("ssh: {e}"))?;
    // 255 is ssh's own failure; anything else came from the script.
    if ccusage && out.status.code() != Some(255) {
        return super::ccusage::script_result(out.status.code(), &out.stdout, &out.stderr);
    }
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("ssh failed");
        return Err(line.chars().filter(|c| !c.is_control()).take(100).collect());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Debug, Default, PartialEq)]
pub struct Probe {
    pub hostname: String,
    pub cpu: Option<(u64, u64)>,
    pub mem_pct: f64,
    pub mem_used_gib: f64,
    pub mem_total_gib: f64,
    pub cores: u32,
    pub load1: f64,
    pub gpus: Vec<GpuInfo>,
    pub sessions: Vec<RemoteSession>,
}

pub fn parse_probe(out: &str) -> Probe {
    let mut p = Probe::default();
    let mut gpu_lines = String::new();
    for line in out.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["host", name] => p.hostname = name.trim().to_string(),
            ["stat", cpu] => p.cpu = parse_cpu_line(cpu),
            ["mem", total, avail] => {
                let (t, a) = (total.parse::<f64>(), avail.parse::<f64>());
                if let (Ok(t), Ok(a)) = (t, a) {
                    if t > 0.0 {
                        p.mem_pct = (t - a) / t * 100.0;
                        p.mem_used_gib = (t - a) / KIB_PER_GIB;
                        p.mem_total_gib = t / KIB_PER_GIB;
                    }
                }
            }
            ["load", l] => p.load1 = l.trim().parse().unwrap_or(0.0),
            ["cores", n] => p.cores = n.trim().parse().unwrap_or(0),
            ["gpu", rest] => {
                gpu_lines.push_str(rest);
                gpu_lines.push('\n');
            }
            ["proc", agent, pid, age, branch, dirty, cwd @ ..] => {
                let cwd = cwd.join("\t");
                p.sessions.push(RemoteSession {
                    agent: agent.to_string(),
                    pid: pid.parse().unwrap_or(0),
                    working: age.parse::<i64>().ok().map(|a| a <= WORKING_SECS),
                    project: cwd
                        .rsplit('/')
                        .find(|s| !s.is_empty())
                        .unwrap_or("?")
                        .to_string(),
                    cwd,
                    branch: branch.to_string(),
                    dirty: dirty.trim().parse().unwrap_or(0),
                });
            }
            _ => {}
        }
    }
    p.gpus = parse_nvidia_smi(&gpu_lines);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_file() {
        let h = parse_hosts(
            "# my boxes\n[devbox]\n\n[192.0.2.10]\nuser = \"alice\"\nport = 2222 # ssh\n[ ]\nuser = x\n",
        );
        assert_eq!(
            h,
            vec![
                HostConfig {
                    name: "devbox".into(),
                    user: None,
                    port: None
                },
                HostConfig {
                    name: "192.0.2.10".into(),
                    user: Some("alice".into()),
                    port: Some(2222)
                },
            ]
        );
    }

    #[test]
    fn probe_output() {
        let out = "host\tdevbox\n\
stat\tcpu  100 0 50 800 50 0 0 0 0 0\n\
mem\t1000\t250\n\
load\t1.50\n\
cores\t16\n\
gpu\tNVIDIA GeForce RTX 3090, 40, 1024, 24576\n\
proc\tclaude\t42\t5\tmain\t      3\t/home/alice/git/abtop\n\
proc\tcodex\t43\t-\t\t0\t/home/alice/my project\n";
        let p = parse_probe(out);
        assert_eq!(p.hostname, "devbox");
        assert_eq!(p.cpu, Some((150, 850)));
        assert_eq!(p.mem_pct, 75.0);
        assert_eq!(p.load1, 1.5);
        assert_eq!(p.cores, 16);
        assert_eq!(p.gpus.len(), 1);
        assert_eq!(p.sessions.len(), 2);
        assert_eq!(p.sessions[0].working, Some(true));
        assert_eq!(p.sessions[0].project, "abtop");
        assert_eq!(p.sessions[0].dirty, 3);
        assert_eq!(p.sessions[1].working, None);
        assert_eq!(p.sessions[1].project, "my project");
    }
}
