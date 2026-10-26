# harn — design (2026-10-26)

Keeps terminal coding agents (Claude Code, Codex; Gemini CLI, OpenCode as data-only profiles) running:

1. **Auto-continue** — detect from the rendered terminal screen that the agent stopped because of an API
   problem and type `continue` for it, with backoff and safety caps. Reactions are configurable per trigger.
2. **Auto-update** — keep the agent CLIs on their latest version: before launch (`harn run`) and between
   sessions (`harn watch`).

Works purely at the terminal level. Two adapters share one IO-free engine:

- `harn watch` — sidecar for the Orca ADE: polls `orca terminal list/read`, acts with `orca terminal send`.
- `harn run <agent> [args]` — universal PTY wrapper for any terminal (ConPTY on Windows).

## Crates

| crate | kind | deps | role |
|---|---|---|---|
| `harn-core` | lib, no IO, no threads | regex, serde, toml, jiff, thiserror | profiles, classifier, policy, engine |
| `harn-update` | lib | ureq, serde_json, which, dirs, thiserror, tracing | detect install method, check latest, update, cache |
| `harn-orca` | lib | harn-core, serde_json, thiserror, tracing | Orca sidecar adapter |
| `harn-pty` | lib | harn-core, portable-pty, vt100, crossterm, thiserror, tracing | PTY wrapper adapter |
| `harn` | bin | all of the above, clap, anyhow, tracing-subscriber, dirs, toml | CLI, config, logging |

Everything is synchronous (std threads + mpsc channels). No tokio: nothing here needs an async runtime.

## harn-core

`Snapshot { text, last_output_ms }` — rendered screen as plain text (no ANSI), lines joined by `\n`;
`last_output_ms` = epoch ms of the last byte the agent printed. All time is injected (`now_ms`), so the
engine is deterministic in tests.

### Profiles (`crates/harn-core/profiles/<id>.toml`, embedded with `include_str!`)

Per agent: `id`, `name`, `aliases` (command basenames and Orca `agentIdentity` values), `continue_text`
(`"continue"`), `submit` (`"\r"`), `interrupt` (`"\u001b"`), `gap_ms` (pause between input chunks — Ink
treats text+Enter in one write as a paste), `tail_lines` (how many non-blank lines at the bottom count as
"current"), and `[signatures]`: regex lists `working`, `overloaded`, `server_error`, `network`,
`rate_limited`, `usage_limit`, `stopped_early`, plus `reset_hints` (regexes with named groups used to parse
when a usage limit resets). Regexes compile at deserialization — an invalid user profile fails at load
with a clear error. User files in `<config_dir>/harn/profiles/<id>.toml` replace built-ins with the same id.

### Classifier — `classify(profile, text, now_ms, tz) -> Classification`

Real captures (`docs/research/agent-screens.md`) refined the rules:

- Only the rows **below the last echoed user message** (`❯ …` in Claude, `› …` in Codex: the
  `user_echo` signatures) and above the input box count. After `continue` is echoed, the old error sits
  above it and is history by construction. Final-error patterns are anchored on the agents' result
  glyphs (`● API Error:`, `■ `). Login, credit and spend-cap errors never match: they need a human.
- A non-empty input box (anything but the `empty_input` placeholders) is a **draft**: the problem is
  still reported, but the engine never types over it.
- `Working` — any `working` signature in the window ("esc to interrupt", including the agent's *own*
  retry countdown). Working wins over everything: never interrupt an agent that is retrying by itself.
- `Problem(Problem { trigger, line, lines_below, resets_at_ms })` — the LAST line in the tail window
  matching a problem signature. Precedence when one line matches several: usage_limit > rate_limited >
  overloaded > server_error > network. `lines_below` = non-blank lines between that line and the bottom.
  `resets_at_ms` parsed for usage limits ("resets 3pm", "resets 3pm (Europe/Moscow)",
  "try again in 2 hours 5 minutes", "try again at 3:05 PM", "resets Oct 30, 9am") via jiff.
- `Problem(StoppedEarly)` — no error, but the tail ends with a `stopped_early` question
  ("Want me to continue?", "Shall I proceed?", "Продолжить?").
- `Idle` — otherwise.

### Policy (serde, every field defaulted)

```toml
settle_ms = 4000             # screen must be quiet this long before acting
effect_timeout_ms = 90000    # acted but agent never resumed → counts as a failed attempt
recovery_ms = 120000         # working this long → attempt counter resets
stall_after_ms = 900000      # working with an unchanged (normalized) screen this long → Stalled
user_grace_ms = 30000        # pty mode: no automation this long after the user typed
max_actions_per_hour = 40    # global safety cap per session

[reactions.overloaded]       # same shape for every trigger
action = "continue"          # continue | interrupt (Esc, then continue) | notify | ignore
delay_secs = 20              # first wait; doubles per attempt
max_delay_secs = 600
max_attempts = 30
# text = "continue"          # optional override of the profile's continue_text
```

Defaults: overloaded 20s/600s/30, server_error 20s/600s/20, network 10s/300s/30, rate_limited
60s/900s/20 — all `continue`; usage_limit `continue`, delay = margin after the parsed reset time
(60s), fallback backoff 900s→1800s when unparsed, 50 attempts; stalled `notify`; stopped_early `ignore`.

### Engine — one per agent session

`Engine::step(&Snapshot, now_ms) -> Vec<Action>`, `Engine::note_user_input(now_ms)`.

`Action::Input { chunks, gap_ms, trigger, attempt }` (write chunks in order, sleeping `gap_ms` between —
continue = `[text, submit]`, interrupt = `[interrupt, text, submit]`) or `Action::Notify { trigger, message }`.

Rules:
1. A problem is acted on only when the screen is settled (`now - last_output_ms >= settle_ms`).
2. First sighting schedules `due = now + backoff(attempt)` (usage limit: `resets_at + delay`); at `due`, if
   the problem is still shown and the agent is not working → emit the action.
3. If the agent recovers on its own before `due` (Working, or Idle) → cancel.
4. After acting, wait for evidence: Working observed → re-armed. A later problem is a *new occurrence*
   unless `lines_below >= lines_below_at_action + 2` (the agent printed the echoed `continue` and a reply
   below the old error, i.e. the error on screen is the old one) → ignore it.
5. No evidence within `effect_timeout_ms` → failed attempt, schedule the next one with the larger backoff.
6. `attempt > max_attempts` → one `Notify`, then hands off until the agent is Working/Idle again.
7. Attempt counter resets on clean Idle or after `recovery_ms` of continuous Working.
8. Stalled: Working, but the normalized screen (digits, spinner glyphs, whitespace stripped) unchanged for
   `stall_after_ms` → the stalled reaction (default notify, once per stall).
9. Hard caps: `max_actions_per_hour`; nothing within `user_grace_ms` after user keyboard input.
10. Swallowed Enter: if the input box holds exactly the text harn just typed and the agent has not
    worked since, re-send only the submit key (at most twice per action); any other draft blocks typing.

## harn-update

Known agents: claude (`@anthropic-ai/claude-code`, binary `claude`, native updater `claude update`), codex
(`@openai/codex`), gemini (`@google/gemini-cli`), opencode (`opencode-ai`). Install method from the
resolved binary path: npm global prefix → `npm i -g <pkg>@latest` (claude → `claude update`), Claude
native installer → `claude update`, Homebrew → `brew upgrade`, unknown → skip with reason. Installed
version from `<bin> --version`; latest from `https://registry.npmjs.org/<pkg>/latest`, cached
(`check_interval_mins`, default 60) in `<cache_dir>/harn/update-cache.json`. Every failure (offline,
timeout, EBUSY because the binary is running on Windows) is returned as `Outcome::Skipped { reason }` or
an error the caller logs — a launch is never blocked by an update failure.

## harn-orca

`orca terminal list --json` every `interval_ms` (default 2000); terminals whose `agentIdentity` matches a
profile alias get an `Engine`. `orca terminal read` only when `lastOutputAt` moved or the engine has a
pending deadline. Actions → `orca terminal send`. All CLI JSON is validated with serde; a malformed or
failing call is logged and the cycle continues. Emits `Event`s (attached, detached, acted, notice, tick
with the set of agents that currently have live terminals — the binary updates only agents that are not
running). `--dry-run` logs actions without sending. Orca exposes no agent liveness (`agentIdentity`
stays after the agent exits), so a screen whose bottom row is a shell prompt counts as exited and is
never typed into. `--terminal HANDLE=AGENT` pins specific terminals for agents Orca does not identify.
Every call passes `--terminal` explicitly (an omitted handle targets the active terminal).

## harn-pty

`run(RunOptions) -> exit code`. Raw mode, transparent byte passthrough both ways, resize polling,
output bytes also fed into a `vt100::Parser` owned by the engine thread (channels, no shared mutable
state). User keystrokes call `note_user_input`. Injected input goes through the same single pty-writer
thread as user input. Terminal is restored on exit and on panic. Child exit code propagated.

## harn (CLI)

```
harn run [--profile ID] [--no-update] [--dry-run] <program> [args...]
harn watch [--interval-ms N] [--dry-run] [--no-update] [--terminal HANDLE=AGENT]...
harn update [AGENT...] [--check] [--force]
harn check [--profile ID] [FILE|-]      # classify a screen dump; for writing/debugging profiles
harn profiles                           # list built-in + user profiles
```

Config: `<config_dir>/harn/config.toml` with `[policy]` (above), `[update]` (`enabled`,
`check_interval_mins`, `install_timeout_secs`), `[orca]` (`bin`, `interval_ms`). Logs:
`<data_local_dir>/harn/harn.log` (never stdout in `run` mode — it would corrupt the agent's TUI).

## Tray app and notifications (added 2026-10-26)

`harn-tray` is a separate GUI binary (no console window on Windows) for Windows, macOS and Linux, built on
`tao` + `tray-icon`. It replaces a long-running `harn watch` tab:

- watches Orca agent terminals in the background (same engine, same single-instance lock);
- the icon colour shows status (watching / paused / Orca unreachable / an agent needs you);
- menu: status, last action, Auto-continue (pause), Start at login (enabled on first run), Auto-update
  agents, Notifications, Update agents now, Settings… (opens `config.toml`), Open log, Quit;
- toggles are written back to `config.toml` with `toml_edit`, keeping the user's comments.

Desktop notifications (`notify-rust`) come from whichever harn process acts: the tray, `harn watch` or
`harn run`. They fire on the first nudge in a run of failures, on anything that needs the user (gave up,
stalled, unsubmitted `continue`), and on completed agent updates. `[notify] enabled = false` turns them
off. Shared CLI/tray code (config, lock, background updater, notifications) lives in the `harn` lib target.

## Testing

- core: fixture screens in `crates/harn-core/tests/fixtures/<agent>/*.txt` (real captures from the agents
  against a mock API where possible) → classification tests; engine state machine tests with a fake clock.
- update: version parsing, install-method detection, cache, command selection via an injected runner.
- orca: fake `OrcaCli` implementation; JSON parsing against captured CLI output.
- pty: headless integration test spawning a fake agent that prints an overload error and waits for input.

## Out of scope for v0.1

LLM-judged "stopped early" classification, PATH shims that make Orca launch agents through `harn run`,
desktop notifications, release packaging.
