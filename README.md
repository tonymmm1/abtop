# abtop

**Like [btop](https://github.com/aristocratos/btop), but for your AI coding agents.**

See every Claude Code, Codex CLI, and OpenCode session at a glance — token usage, context window %, rate limits, child processes, open ports, and more.

> [!NOTE]
> This is a fork of [graykode/abtop](https://github.com/graykode/abtop). It adds a [CLIProxyAPI](#cliproxyapi-accounts) accounts and pooled-quota view, [remote hosts](#remote-hosts) over SSH, [ccusage](#ccusage-costs) costs, GPU and network metrics in the header, per-host project tags, faster startup, and a `matrix` theme. See [Fork additions](#fork-additions).
Claude Code, Codex CLI, and OpenCode sessions are discovered from local process/file state, so multiple active profiles are supported across macOS, Linux, and Windows.

![demo](https://raw.githubusercontent.com/graykode/abtop/main/assets/demo.gif)

## Why

- Running 3+ agents across projects? See them all in one screen.
- Hitting rate limits? Watch your quota in real-time.
- Agent spawned a server and forgot to kill it? Orphan port detection.
- Context window filling up? Per-session % bars with warnings.
- Agents spread over several machines or proxy accounts? See their hosts, quotas and costs in the same window.

Read-only. The core view needs no API keys or auth; the optional fork features use only what you configure (a CLIProxyAPI key, your SSH setup, a local `ccusage`).

## Install

### This fork

```bash
cargo install --git https://github.com/tonymmm1/abtop
```

Needs Rust 1.88 or newer. The installers and release binaries below are upstream builds and don't include the fork additions.

### macOS / Linux

> [!IMPORTANT]
> On Linux, ensure `sqlite3` is installed to enable monitoring for OpenCode sessions.

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/graykode/abtop/releases/latest/download/abtop-installer.sh | sh
```

### Cargo

```bash
cargo install abtop
```

### Windows

Native support — no WSL required. Uses `sysinfo` for process info and host CPU/MEM metrics, and `netstat -ano` for listening ports. Windows has no load average, so LOAD is reported as 0. OpenCode session discovery additionally requires the `sqlite3` CLI (`winget install SQLite.SQLite`); without it abtop prints a one-time warning to stderr.

```powershell
powershell -c "irm https://github.com/graykode/abtop/releases/latest/download/abtop-installer.ps1 | iex"
```

Or `cargo install abtop` from any terminal with Git in PATH. Claude Code config is resolved automatically from `%USERPROFILE%\.claude`.

### Other

Pre-built binaries for all platforms are available on the [GitHub Releases](https://github.com/graykode/abtop/releases) page.

## Usage

```bash
abtop                    # Launch TUI
abtop --once             # Print snapshot and exit
abtop --json             # Print one JSON snapshot and exit (for scripts/tools)
abtop --setup            # Install rate limit collection hook
abtop --theme dracula    # Launch with a specific theme
abtop --mouse            # Enable mouse click/scroll navigation
abtop --accounts         # Open the CLIProxyAPI accounts view at startup
abtop --demo             # Launch with demo data
```

Recommended terminal size: **120x40** or larger. Minimum 80x24 — panels hide gracefully when small.
Mouse capture is off by default so terminal drag selection and copy keep working. Launch with `--mouse` if you prefer click targets and wheel navigation.

### Terminal Jump

Press `Enter` to focus the terminal running the selected agent. abtop supports cmux, tmux, and iTerm2 on macOS.

```bash
tmux new -s work
# pane 0: abtop
# pane 1: claude (project A)
# pane 2: claude (project B)
# → Enter on a session in abtop jumps to its pane
```

## Supported Agents

| Feature           | Claude Code | Codex CLI | OpenCode |
| ----------------- | :---------: | :-------: | :------: |
| Session Discovery |     ✅      |    ✅     |    ✅    |
| Token Tracking    |     ✅      |    ✅     |    ✅    |
| Context Window %  |     ✅      |    ✅     |    ❌    |
| Status Detection  |     ✅      |    ✅     |    ✅    |
| Current Task      |     ✅      |    ✅     |    ❌    |
| Rate Limit        |     ✅      |    ✅     |    ❌    |
| Git Status        |     ✅      |    ✅     |    ✅    |
| Children / Ports  |     ✅      |    ✅     |    ✅    |
| Subagents         |     ✅      |    ❌     |    ❌    |
| Memory Status     |     ✅      |    ❌     |    ❌    |

OpenCode support reads the local SQLite database at `~/.local/share/opencode/opencode.db` (also the default location on Windows; `%LOCALAPPDATA%\opencode` and `%APPDATA%\opencode` are probed as fallbacks) and requires `sqlite3` in `PATH` (on Windows: `winget install SQLite.SQLite`).

## Themes

13 built-in themes, including 4 colorblind-friendly options (`high-contrast`, `protanopia`, `deuteranopia`, `tritanopia`). Press `t` to cycle at runtime, or launch with `--theme <name>`. Your choice is saved to `~/.config/abtop/config.toml`.

| btop (default) | dracula | catppuccin |
|:-:|:-:|:-:|
| ![btop](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/btop.png) | ![dracula](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/dracula.png) | ![catppuccin](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/catppuccin.png) |

| tokyo-night | gruvbox | nord |
|:-:|:-:|:-:|
| ![tokyo-night](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/tokyo-night.png) | ![gruvbox](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/gruvbox.png) | ![nord](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/nord.png) |

Colorblind-friendly themes:

| high-contrast | protanopia |
|:-:|:-:|
| ![high-contrast](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/high-contrast.png) | ![protanopia](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/protanopia.png) |

| deuteranopia | tritanopia |
|:-:|:-:|
| ![deuteranopia](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/deuteranopia.png) | ![tritanopia](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/tritanopia.png) |

Light themes (`light` — Solarized cream, `white` — GitHub-style pure white) for bright terminals.

`matrix` (fork) is phosphor green on black, keeping amber and red for warnings.



| light | white |
|:-:|:-:|
| ![light](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/light.png) | ![white](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/white.png) |

## Configuration

`~/.config/abtop/config.toml` supports:

```toml
theme = "btop"
# Hide specific agent CLIs from the TUI (case-insensitive).
# Useful if you only use one agent and want a cleaner view.
hidden_agents = ["codex"]
# Additional Claude Code profile roots to scan.
# abtop also auto-discovers ~/.claude and ~/.claude-* roots that contain
# both sessions/ and projects/.
claude_config_dirs = ["~/.claude-personal", "~/.claude-work-team"]
# UI language. Omit or leave empty to auto-detect from LANG.
language = "zh"
# CLIProxyAPI management server (fork). Env CLIPROXY_URL and
# CLIPROXY_MANAGEMENT_KEY take precedence.
cliproxy_url = "http://localhost:8317"
cliproxy_key_file = "~/.config/abtop/cliproxy.key"   # default
```

Remote hosts live in their own file; see [Remote hosts](#remote-hosts).

### Supported Languages

| Code | Language            |
| ---- | ------------------- |
| `en` | English (default)   |
| `zh` | Simplified Chinese  |

When `language` is unset, abtop auto-detects from `LANG` — any value starting with `zh` switches to Simplified Chinese, otherwise English.

## Key Bindings

| Key                | Action                                                        |
| ------------------ | ------------------------------------------------------------- |
| `↑`/`↓` or `k`/`j` | Select session                                                |
| `Enter`            | Jump to session terminal                                      |
| `/`                | Filter sessions (`Esc` clears)                                |
| `x`                | Kill selected session                                         |
| `X`                | Kill all orphan ports                                         |
| `r`                | Force refresh                                                 |
| `a`                | CLIProxyAPI accounts view (`r` inside refreshes quotas)       |
| `U`                | Refresh ccusage costs                                         |
| `v`                | View menu                                                     |
| `c`                | Config page                                                   |
| `t`                | Cycle theme                                                   |
| `T`                | Toggle subagent tree view                                     |
| `l`                | Toggle timeline                                               |
| `f`                | Toggle file audit                                             |
| `M`                | Toggle MCP session suppression                                |
| `1`–`7`            | Toggle panels: context, quota, tokens, projects, ports, sessions, MCP |
| `?`                | Help                                                          |
| `q`                | Quit                                                          |

In narrow terminals, `w`/`u`/`s` switch between the Work, Usage and System tabs, and `+`/`-` maximize or restore a section.

## Fork additions

### Header

- htop-style CPU (`340%/1600%`), memory as used/total GiB, and network ↓/↑ rates from `/proc/net/dev` (loopback and virtual bridges skipped). Fields are fixed-width so the header doesn't jitter.
- A GPU row with utilization and VRAM bars for NVIDIA (`nvidia-smi`) and AMD (amdgpu sysfs). It appears only when a GPU is detected.
- Working/live session counts per agent; an agent with nothing running is highlighted. When an agent's last working session goes idle, abtop plays the freedesktop warning sound (terminal bell as a fallback) and shows a status message once.
- The clock includes the date.

### CLIProxyAPI accounts

If you route agents through [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI), abtop polls its management API for Claude and Codex auth files and shows per-account quota windows, plans, cooldowns, subscription end dates and reset timers.

- The quota panel shows pooled remaining quota per provider plus every active account. `a` (or `--accounts`) opens a full-screen accounts view.
- The 60 s refresh only lists auth files on your local proxy. Upstream usage endpoints are called at most once an hour per account (every 5 minutes on a manual `r`), and only when the proxy has no recorded rate-limit headers for that account. On an error the last good numbers stay on screen.
- The management key comes from `CLIPROXY_MANAGEMENT_KEY`, `cliproxy_key` in the config, or `~/.config/abtop/cliproxy.key`. It is passed to `curl` on stdin, so it never appears in `ps`.

### Remote hosts

List hosts in `~/.config/abtop/hosts.toml`:

```toml
[devbox]            # ssh alias, hostname or IP

[192.0.2.10]
user = "alice"
port = 2222
```

Every 10 s, abtop runs a small read-only shell probe on each host (`ssh host sh -s`, script on stdin, one reused connection per host). Nothing is installed remotely. Auth is whatever `ssh` uses for that host, and `BatchMode` makes it fail rather than prompt. Each host shows CPU, memory, load, GPUs and its running agent sessions. The panel appears only when hosts are configured and takes at most a third of the screen.

The projects panel tags each project with session counts per host (`local×2 devbox×1`) and also lists projects that run only on a remote host.

### ccusage costs

When [ccusage](https://github.com/ryoppippi/ccusage) can run (as `ccusage`, `bunx ccusage` or `npx ccusage`), abtop shows its numbers in the header and under each remote host:

```
ccusage 5h $145→$1.1k $232/h 4h28m  today $670 2.0B  7d ▂▃▅▂▇▆█ $6.6k
```

- `5h $145→$1.1k $232/h 4h28m`: the active 5-hour block, as spent so far → projected by the block's end at the current burn rate, then the burn rate and the time left.
- `today $670 2.0B`: today's cost and tokens.
- `7d ▂▃▅▂▇▆█ $6.6k`: daily cost over the last 7 days (today on the right) and the total.

Costs are at API list prices; on a subscription they measure usage, not your bill. ccusage rescans all transcripts, so it runs in the background at startup, every 15 minutes, and on `U`. A trailing `…` means a refresh is running, `!` means the last one failed (the previous numbers are kept). Hosts where ccusage can't run show nothing.

### Startup

The window draws immediately with a "loading sessions…" status, and `git status` for each session directory runs in parallel instead of one after another.

## Library / JSON snapshot

abtop is also a library crate, so local tools can reuse its data-collection
layer in-process — no re-scanning, no subprocesses — and serialize the same
state the TUI renders.

```bash
abtop --json    # one-shot JSON snapshot for scripts
```

For long-running consumers, build an `App`, refresh it with
`App::tick_no_summaries()` (which never spawns `claude --print`, so it doesn't
touch your Claude quota), and call `App::to_snapshot(interval_ms)` to get a
JSON-serializable [`Snapshot`]:

```rust,no_run
use abtop::app::App;
use abtop::{config, theme::Theme};

let cfg = config::load_config();
let mut app = App::new_with_config_and_claude_dirs(
    Theme::default(), &cfg.hidden_agents, cfg.panels, &cfg.claude_config_dirs,
);
app.tick_no_summaries();
let json = serde_json::to_string(&app.to_snapshot(2_000)).unwrap();
```

`App` is not `Send` (it owns the collectors), so keep it on one thread and pass
the serialized JSON elsewhere. [abtop-web-ui](https://github.com/XKHoshizora/abtop-web-ui)
is a reference consumer: a local-first web dashboard built on exactly this API.

## Privacy

abtop reads local files and local process/open-file metadata only. No API keys, no auth. In the TUI and `--once` output, tool names and file paths are shown, but file contents and prompt text are never displayed. Session summaries are generated via `claude --print`, which makes its own API call. The fork's optional features add network access only when configured: `curl` to your CLIProxyAPI server (and, through it, the providers' usage endpoints), `ssh` to the hosts in `hosts.toml`, and `ccusage`, which may be fetched by `bunx`/`npx` if it isn't installed.

The JSON snapshot includes richer local dashboard data, including `summary`, `chat_messages`, working directories, config roots, tool-call previews, child process commands, token counts, and port metadata. Chat text is bounded and redacted by the collectors, but it is still derived from local transcripts and may contain sensitive project context. Treat JSON snapshots as local/private data and avoid writing them to shared logs or exposing them on a network without your own access controls.

## Acknowledgements

Huge thanks to [@tbouquet](https://github.com/tbouquet) for driving much of abtop's recent shape — themes, config overlay and panel toggles, session filtering, subagent tree view, the context window gauge with compaction detection, plus a steady stream of fixes and security hardening along the way.

## License

MIT
