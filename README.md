# harn

Keeps terminal coding agents working while you are away.

- **Auto-continue.** Claude Code, Codex and similar agents stop dead when the API is overloaded
  (`529 Overloaded`), returns 5xx, drops the stream or rate-limits you. Someone then has to type `continue`.
  harn reads the agent's screen, recognises these stops and types `continue` for you. It uses backoff,
  waits until a usage limit resets, and has hard safety caps.
- **Auto-update.** Agent CLIs ship almost daily. harn updates them to the latest version right before
  launch (`harn run`) or between sessions (`harn watch`).

Everything happens at the terminal level, so there are no API keys, proxies or patches to the agents.
It works with [Orca](https://github.com/stablyai/orca) and any other terminal on Windows, macOS and Linux.

| agent | auto-continue | auto-update |
|---|---|---|
| Claude Code | ✅ (screens verified against the real CLI) | ✅ `claude update` / npm |
| Codex CLI | ✅ (screens verified against the real CLI) | ✅ npm / Homebrew |
| Gemini CLI | notify only (no verified screens yet) | ✅ npm |
| OpenCode | notify only (no verified screens yet) | ✅ npm |

## Install

```sh
git clone <this repo> && cargo install --path harn/crates/harn
```

## Use

### Inside Orca (or any ADE that exposes its terminals)

Open a terminal tab and leave this running:

```sh
harn watch
```

harn discovers every Claude Code / Codex terminal Orca knows about. When one stops on an API error it
types `continue` there. It also updates agents that currently have no live session. Add `--dry-run` to
see what it would do without typing anything. If Orca doesn't recognise an agent, point harn at its
terminal with `--terminal <handle>=<agent>` (handles come from `orca terminal list`). Run one
`harn watch` per machine (a second one refuses to start). Inside Orca, don't also alias the agents to
`harn run`, or both would react to the same error.

### Any terminal

Run the agent through harn:

```sh
harn run claude
harn run codex --model o3
```

harn first updates the agent, then runs it in a pseudo-terminal that is passed through untouched. You
use the agent exactly as before; harn only steps in when the agent has stopped on an error. To make
this the default:

```sh
# bash / zsh
alias claude='harn run claude'
alias codex='harn run codex'
```

```powershell
# PowerShell profile
function claude { harn run claude @args }
function codex { harn run codex @args }
```

### Other commands

```sh
harn update            # update every installed agent now
harn update --check    # just report what is outdated
harn profiles          # list known agents and where their profiles come from
harn check screen.txt --profile codex   # classify a saved screen (for writing profiles)
```

## How it decides

The agent's terminal is rendered into plain text, and the bottom of the screen is matched against the
agent's **profile**:

| trigger | example | default reaction |
|---|---|---|
| `overloaded` | `API Error: 529 … overloaded_error` | continue, 20s → 10min backoff, 30 attempts |
| `server_error` | `API Error: 500`, `exceeded retry limit, last status: 503` | continue, 20s → 10min, 20 attempts |
| `network` | `stream disconnected before completion`, `Connection error` | continue, 10s → 5min, 30 attempts |
| `rate_limited` | `429 Too Many Requests` | continue, 60s → 15min, 20 attempts |
| `usage_limit` | `You've hit your limit · resets 3pm` | wait until the reset time + 60s, then continue |
| `stalled` | spinner running, nothing changing for 15 min | notify |
| `stopped_early` | "Want me to continue?" | ignore (opt-in) |

Safety rails:

- harn never types while the agent is working or retrying on its own.
- It types only when the agent's own empty input box is on screen. If you have a draft there, a
  dialog or menu is open, or the agent has exited to a shell, it does nothing.
- In `harn watch` it re-reads the screen before pressing Enter and aborts unless the input box holds
  exactly what it typed.
- It acts only after the screen has been quiet for a few seconds.
- It does not act on the same error twice. Errors above your last message are history.
- It stops after `max_attempts` per trigger and after `max_actions_per_hour`, and then notifies you.
- Login, billing and spend-cap errors are never auto-continued; they need you.
- In `harn run` it backs off for 30s whenever you type.

## Configure

`~/.config/harn/config.toml` (Linux), `~/Library/Application Support/harn/config.toml` (macOS),
`%APPDATA%\harn\config.toml` (Windows). Every key is optional.

```toml
[policy]
settle_ms = 4000            # screen must be quiet this long before harn acts
max_actions_per_hour = 40

[policy.reactions.overloaded]
action = "continue"         # continue | interrupt (Esc, then continue) | notify | ignore
delay_secs = 20             # doubles on each attempt…
max_delay_secs = 600        # …up to this
max_attempts = 30
text = "continue"           # what to type

[policy.reactions.stopped_early]
action = "continue"         # let harn answer "Want me to continue?" for you

[update]
enabled = true
check_interval_mins = 60

[orca]
bin = "orca"
interval_ms = 2000
```

Logs go to `<local data dir>/harn/harn.log`, with verbosity set by `HARN_LOG=debug`.

### Profiles

Each agent is a TOML file with regexes for each trigger. See
[`crates/harn-core/profiles`](crates/harn-core/profiles). To fix a pattern or add an agent, drop a
file with the same `id` into `<config dir>/harn/profiles/`. Test it with `harn check`.

## Embedding

The crates are usable on their own:

- [`harn-core`](crates/harn-core): an IO-free classifier and state machine. You feed it screen text
  and get back actions; time is injected.
- [`harn-update`](crates/harn-update): detects how an agent was installed and updates it.
- [`harn-pty`](crates/harn-pty) and [`harn-orca`](crates/harn-orca): the two adapters.

## Contributing

Real screens are the most valuable contribution. When an agent shows a new kind of error, save the
screen text and add it under `crates/harn-core/tests/fixtures/<agent>/<trigger>-<what>.txt`. The
fixture test picks it up automatically. `docs/research/agent-screens.md` explains how the existing
fixtures were captured against a mock API.

## License

MIT or Apache-2.0, at your option.
