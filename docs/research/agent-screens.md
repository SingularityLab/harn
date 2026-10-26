# Agent screens: ground truth for the harn detector

Captured 2026-10-26 on Windows 11 inside Orca, using Claude Code **2.1.310** and Codex CLI **0.164.0**.
Each CLI was run against a local mock API that fails on purpose (529, 500, 503, 429, socket reset,
mid-stream drop, usage limit). Config was isolated in a temporary `CLAUDE_CONFIG_DIR` / `CODEX_HOME`.
Strings were pulled from the shipped binaries: the Claude Code Bun bundle (`bin/claude.exe`) and the Codex
Rust binary (`vendor/x86_64-pc-windows-msvc/bin/codex.exe`).

Fixtures are in `crates/harn-core/tests/fixtures/{claude,codex}/`. Every fixture is the `tail` array from
`orca terminal read --screen --json`, one row per line. The only edit is the working directory, which was
replaced with `~\projects\demo-project`. Fixtures marked **synthetic** reuse a real captured layout with
the error line swapped for wording taken from the binary, because these states cannot be triggered against
a mock with an API key.

## 0. Findings that change detector design

1. **Agent retry screens contain error text.** During its own retry, Codex shows
   `• Reconnecting... 2/3 (3s • esc to interrupt)` with `└ Stream disconnected before completion: …`
   underneath. Claude shows `✻ Connection dropped (ECONNRESET) · Retrying in 1s · attempt 3/4`.
   Matching only `stream disconnected` or `Connection dropped` would fire in the middle of the agent's own
   retry. Treat **`esc to interrupt` anywhere on screen as "busy, do nothing"**, and anchor final errors
   to their prefix: `● API Error:` for Claude, `■ ` for Codex.
2. **Codex reports HTTP 500 as "high demand".** A plain 500 renders as
   `■ We’re currently experiencing high demand, which may cause temporary errors.`
   A 503 with `server_is_overloaded` renders as `■ Selected model is at capacity. Please try a different model.`
   Both mean "retry later", so they belong in one category.
3. **Claude gives up after 3 consecutive 529s**, even when `CLAUDE_CODE_MAX_RETRIES` allows more. It then
   prints `API Error: Repeated 529 Overloaded errors…`. Other errors use the full retry budget.
4. **Old errors stay on screen after "continue".** Both agents keep the history: old error, then the echoed
   `continue`, then new output. Only consider error lines **below the last user-echo line**. In Claude,
   user-echo lines start with `❯ ` and the input row is a bare `❯` between two `─` rules. In Codex,
   user-echo lines start with `› `, and the composer is the **last** `› ` row, so exclude it.
5. **Orca drops blank rows.** `tail` holds non-blank rendered rows only: 18 rows for a full-height Claude
   pane. Count lines in non-blank rows, and never assume a fixed screen height.
6. **Apostrophes differ by agent.** Claude uses an ASCII `'` (`You've hit your …`). Codex uses U+2019
   (`You’ve`, `We’re`). Match `[’']`.
7. Claude **wraps long error lines** onto a second row with a 2-space indent (`  (127.0.0.1:47529).`), and
   `tailRows[i].wrapped` is `false` for that row. Join continuation rows before matching end-anchored
   patterns.
8. With API-key auth, Claude **never shows the subscription usage-limit text**. Even with the
   `anthropic-ratelimit-unified-*` headers, the final screen says `API Error: Request rejected (429) · Rate limited`.
   The retry banner does use the unified headers: `✻ Session limit reached · Retrying in 1s (11:47pm) · attempt 1/4`.

## 1. Claude Code 2.1.310

### Layout

The final error screen looks like this (`overloaded.txt`):

```
 ▐▛███▛█   Claude Code v2.1.310            ← header (logo, 3 rows) + notices
❯ say hi                                    ← user echo
● API Error: Repeated 529 Overloaded errors. … check your inference gateway
  (127.0.0.1:47529).                        ← wrapped continuation (2-space indent)
✻ Cogitated for 10s · done 8:25 PM          ← turn-finished line ("<Verb>ed for Ns · done h:mm AM/PM")
──────────────────────────────────────      ← input box top rule
❯                                           ← input row
──────────────────────────────────────      ← input box bottom rule
  ⏵⏵ auto mode on (shift+tab to cycle)      ← footer: permission-mode line (+ statusline, tasks, agents if configured)
```

Below the error there are **5 non-blank rows** in this setup: done-line, rule, input, rule, footer. A real
user setup adds more footer rows (status line, `· 1 shell`, a background-agents list), so scan the last
~40 rows rather than a fixed offset.

- **Working** (`working.txt`): the spinner row is a glyph from `· ✢ * ✶ ✻ ✽` followed by a random verb and
  `…` (for example `✶ Cascading…`). The footer gets ` · esc to interrupt` appended:
  `  ⏵⏵ auto mode on (shift+tab to cycle) · esc to interrupt`. When the turn ends, that suffix disappears.
- **Own retry** (`retrying*.txt`): the spinner row turns into
  `✻ <msg> · Retrying in <dur>[ (<reset>)] · attempt <n>/<max>`. The footer still shows `esc to interrupt`.
  `<msg>` is `API error` for attempts 1–2. From attempt 3 onward, or immediately for network-down, SSL
  or rate-limit cases, it is the formatted error: `500 Internal server error`,
  `Connection dropped (ECONNRESET)`, `429 <message>`, or `Session limit reached`.
  Source: `` ` · Retrying in ${_}${f} · attempt ${t.attempt}/${t.maxRetries}` ``.
- Other retry and stall banners in the same component:
  - `Waiting for API response · will retry in <dur> · check your network`
  - `No response from the API after <dur> · retrying, waiting up to <dur> · attempt n/m`
  - `… · retrying once, waiting up to <dur>`
  - `… · next try in <dur> · attempt n · esc to interrupt` (low-priority wait)
  - `Auto mode check unavailable · next try in …`

### Final error strings

Captured live:

| Fixture | Line |
|---|---|
| `overloaded.txt` | `● API Error: Repeated 529 Overloaded errors. The API is at capacity — this is usually temporary. Try again in a moment. If it persists, check your inference gateway (127.0.0.1:47529).` |
| `server_error.txt` | `● API Error: 500 Internal server error. This is a server-side issue, usually temporary — try again in a moment. If it persists, check …` |
| `network.txt` | `● API Error: Connection dropped (ECONNRESET)` |
| `rate_limited.txt` | `● API Error: Request rejected (429) · Number of request tokens has exceeded your per-minute rate limit` |
| `rate_limited_unified_headers.txt` | `● API Error: Request rejected (429) · Rate limited` |

With a real endpoint, the `If it persists` suffix reads ` If it persists, check https://status.claude.com.`

`server_error.txt` also holds history: an older 529 error, a successful `continue`, and a stray
`❯ C:/Program Files/Git/clear` row. That row is an artifact: Git Bash rewrote `/clear` while sending it.
The file is useful as a "several errors on screen, only the last one matters" test.

Extracted from the binary, not captured:

- **Network:** `Connection error.` · `Connection closed before the response finished` · `Connection lost` ·
  `Connection lost while your computer was asleep` · `No response from API` · `Request timed out` ·
  `Request timed out. Check your internet connection and proxy settings` ·
  `Unable to connect to API: SSL certificate verification failed …` · `SSL certificate error (<code>)…`
- **Overload:** `Opus is experiencing high load, please use /model to switch to Sonnet` (also `Fable …`) ·
  `Fast mode overloaded and is temporarily unavailable · resets in …`
- **Rate limit (not quota):** `Server is temporarily limiting requests (not your usage limit)`
- **Auth (not retryable, never auto-continue):** `API Error: 401 Invalid API key · Please run /login` ·
  `OAuth token revoked · Please run /login` · `Login expired · Please run /login`
- **Aborted:** `API Error: Request was aborted.`
- **Stuck turn:** ` The last attempt reached the limit while still thinking.`

### Usage limit (subscription)

The text is built by `` `You've hit your ${limitName}${suffix}${progressSaved}` ``.

- `limitName` comes from `{five_hour: "session limit", seven_day: "weekly limit", seven_day_opus: "Opus limit",
  seven_day_sonnet: "Sonnet limit", seven_day_overage_included: "Fable limit", overage: "usage credit limit"}`,
  and falls back to `usage limit` or `limit`.
- `suffix` is `` ` · resets ${time}` ``. `progressSaved` is ` · progress saved`.
- Other variants:
  - `You're out of usage credits · resets …`
  - `You're out of extra usage`
  - `You've hit your team's shared budget …`
  - `You've hit your monthly spend limit …` and `You've hit your org's monthly spend limit …`
  - `Your org is out of usage · add funds to continue`

  These spend-cap and out-of-credits variants need a human, so harn should not keep re-sending for them.
- Warnings, which do **not** stop the turn: `You've used 90% of your session limit · resets 5pm (Europe/Moscow)`
  and `Approaching weekly limit · resets …`
- **Reset-time format** (`yR`, en-US):
  - Within 24 h: `5pm`, `11:47pm`. The time is lower-case, with no space before am/pm, and minutes are
    omitted when `:00`.
  - Further out: `Nov 2, 5pm` or `Nov 2, 5:30pm`.
  - A different year adds it: `Nov 2, 2027, 5pm`.
  - The stop message appends ` (<IANA tz>)`, for example `resets 11:47pm (Europe/Moscow)`. The retry
    banner shows only `(11:47pm)`.
- Synthetic fixtures: `usage_limit_session.txt` (`● You've hit your session limit · resets 11:47pm (Europe/Moscow)`)
  and `usage_limit_weekly.txt` (`… weekly limit · resets Nov 2, 5pm (Europe/Moscow)`).
  The `● ` bullet placement is inferred: these messages skip the `API Error:` prefix.

### Retry knobs

- `CLAUDE_CODE_MAX_RETRIES=<n>` (verified; the UI showed `attempt n/4` with `4`). A 529 still gives up after
  3 in a row.
- Related settings:
  - `CLAUDE_STREAM_FIRST_BYTE_TIMEOUT_MS`
  - `--fallback-model` / `fallbackModel`, which switches model on repeated 529s
  - `StopFailure` hook, which fires when an API error ends the turn. This could be an alternative signal
    to screen scraping.

## 2. Codex CLI 0.164.0

### Layout

```
  >_ OpenAI Codex (v0.164.0)                ← header (big braille logo only before the first prompt)
     ~\projects\demo-project
› say hi                                    ← user echo
■ <error text>                              ← final error (red square)
› Ask Codex to do anything                  ← composer (placeholder; holds typed text when non-empty)
  gpt-5.1-codex default · ~\projects\demo-project
  ? for shortcuts                       ⚠ 1 warning · f2 to view
```

There are **3 non-blank rows** below the error: composer, model/cwd, shortcuts.

- **Working:** `• Working (0s • esc to interrupt)` sits right above the composer. The header word can change
  to a reasoning-summary title, so match `esc to interrupt` and not `Working`.
- **Own retry** (`retrying.txt`, needs `stream_max_retries > 0`):
  `• Reconnecting... 2/3 (3s • esc to interrupt)`, followed by
  `  └ Stream disconnected before completion: <detail>`. The capital `S` is the only difference from the
  final error.
- **Turn done:** `  Worked for 1s • 20:32`.
- **User interrupt (not an API error):** `■ Conversation interrupted - use /feedback if something went wrong`.

### Final error strings

Captured live (`■ ` prefix):

| Fixture | Cause | Line |
|---|---|---|
| `server_error.txt` | 500 | `■ We’re currently experiencing high demand, which may cause temporary errors.` |
| `overloaded.txt` | 503 `server_is_overloaded` | `■ Selected model is at capacity. Please try a different model.` |
| `rate_limited.txt` | 429 | `■ exceeded retry limit, last status: 429 Too Many Requests` |
| `network.txt` | connection reset | `■ stream disconnected before completion: error sending request` |
| `network_midstream.txt` | drop mid-SSE | `■ stream disconnected before completion: Transport error: network error: error decoding response body` |
| `usage_limit.txt` | 429 `usage_limit_reached`, plan `plus`; **real** | `■ You’ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/settings/usage to purchase more credits or try again at 11:49 PM.` |

Synthetic:
- `usage_limit_other_day.txt`: `■ You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Nov 3rd, 2026 9:05 AM.`
- `usage_limit_free_plan.txt`: `■ You’ve hit your usage limit. Upgrade to Plus to continue using Codex (https://chatgpt.com/explore/plus), or try again at Nov 3rd, 2026 9:05 AM.`

Extracted from the binary:

- **Retry and rate limit:** `exceeded retry limit, last status: <code> <reason>` (any status) · `rate limit exceeded: …` ·
  `(?i)try again in\s*(\d+(?:\.\d+)?)\s*(s|ms|seconds?)` (Codex's own parser for 429 bodies)
- **Network:** `stream disconnected before completion: …` · `Reconnecting... waiting for network`
- **Capacity:** `Flex capacity unavailable.`
- **Quota and credits (human action needed, do not loop):** `Quota exceeded. Check your plan and billing details.` ·
  `Your workspace is out of credits. …` · `You hit your spend cap …` ·
  `You’ve hit your usage limit for <model>. Switch to another model now, …`
- **Usage-limit tails:** ` or try again at <date>.` · ` or try again later.` · ` Try again at <date>.` · ` Try again later.`
- **Reset-time format** (chrono): `%-I:%M %p` for the same day, for example `11:49 PM`. Otherwise
  `%b %-d<st|nd|rd|th>, %Y %-I:%M %p`, for example `Nov 3rd, 2026 9:05 AM`. Local time, no time zone shown.

### Retry knobs

Per provider in `config.toml`: `[model_providers.<id>]` takes `request_max_retries`,
`stream_max_retries` and `stream_idle_timeout_ms`. All three keys are in the binary, and the
`stream_max_retries = 3` behaviour was verified (`Reconnecting... n/3`). Upstream defaults are 4 / 5 /
300000 ms (not re-verified).

## 3. Recommended regexes

All patterns should run on the rows **below the last user-echo row**, after joining Claude continuation rows.
They use Rust `regex` syntax with `(?m)`, so `^` and `$` anchor to lines. Inside the table, `\|` stands for
the regex alternation `|`.

| Category | Claude | Codex |
|---|---|---|
| **busy (veto)** | `esc to interrupt` | `esc to interrupt` |
| own_retry (busy) | `·\s*Retrying in \S+.*·\s*attempt \d+/\d+` | `^•\s*Reconnecting\.\.\.\s*\d+/\d+` |
| overloaded | `^● API Error: (?:Repeated 529\|529\b\|.*(?i:overloaded))\|^● .*is experiencing high load` | `^■ (?:Selected model is at capacity\|We[’']re currently experiencing high demand\|Flex capacity unavailable)` |
| server_error | `^● API Error: 5\d\d\b\|server-side issue` | `^■ exceeded retry limit, last status: 5\d\d\|^■ We[’']re currently experiencing high demand` |
| network | `^● API Error: (?:Connection (?:dropped\|error\|closed\|lost)\|Request timed out\|No response from API\|Unable to connect)` | `^■ stream disconnected before completion\|^■ .*(?:error sending request\|Transport error\|network error)` |
| rate_limited | `^● API Error: (?:Request rejected \(429\)\|429\b)\|Server is temporarily limiting requests` | `^■ exceeded retry limit, last status: 429\|^■ rate limit exceeded` |
| usage_limit | `^● You[’']ve hit your (?:session \|weekly \|Opus \|Sonnet \|Fable \|usage credit \|usage )?limit(?: · resets (?P<reset>[^·\n]+?))?(?: · .*)?$` | `^■ You[’']ve hit your usage limit\..*?(?:try again at (?P<reset>[^.\n]+?)\.\|try again later\.)` |
| needs_human (no auto-continue) | `API Error: 401\|Please run /login\|out of usage credits\|spend limit\|shared budget\|out of extra usage` | `Quota exceeded\|out of credits\|spend cap\|Conversation interrupted` |

The Claude own-retry pattern applies to the spinner row, which starts with `✻`.

To parse the reset time:

- Claude: `^(?:(?P<mon>[A-Z][a-z]{2}) (?P<day>\d{1,2}), (?:(?P<year>\d{4}), )?)?(?P<h>\d{1,2})(?::(?P<m>\d{2}))?(?P<ampm>am|pm)(?: \((?P<tz>[^)]+)\))?$`
- Codex: `^(?:(?P<mon>[A-Z][a-z]{2}) (?P<day>\d{1,2})(?:st|nd|rd|th), (?P<year>\d{4}) )?(?P<h>\d{1,2}):(?P<m>\d{2}) (?P<ampm>AM|PM)$`

If no date is given, use the next occurrence of that time. Codex uses local time; Claude uses the time zone
named in parentheses.

For **stopped-early questions** there is no wire signal. A heuristic is a last assistant row (`● ` for
Claude, `• ` for Codex) that ends in `?`, plus
`(?i)\b(?:should i|shall i|do you want me to|would you like me to|want me to)\b.*\b(?:continue|proceed|go ahead|keep going)\b`.

## 4. Gemini CLI and OpenCode (source text only, no captures)

These strings come from the GitHub sources, not from captures. They are not verified on a real screen.

### Gemini CLI (`google-gemini/gemini-cli`, main)

- **Working:** `(esc to cancel, 12s)` or `(esc to cancel, 1m 5s)` after a braille spinner and a label
  (`Thinking...`, the model's thought subject, or a tip).
  - Source: `packages/cli/src/ui/components/LoadingIndicator.tsx`.
  - Hooks show `Executing Hook: <name>`. Confirmation waits show `Waiting for user confirmation...`
    without `esc to cancel`.
- **Own retry** (`hooks/useLoadingIndicator.ts`):
  - At the default `errorVerbosity: low`, attempt 1 shows nothing. From attempt 2 the status line shows
    `This is taking a bit longer, we're still on it.`
  - With `full` it shows `Trying to reach <model> (Attempt n/max)`.
  - There are 10 attempts, with a 5 s initial and 30 s maximum backoff (`core/src/utils/retry.ts`).
- **Stop:** `✕ [API Error: <message>]` or `✕ [API Error: <message> (Status: UNAVAILABLE)]`
  (`core/src/utils/errorParsing.ts`).
  - On 429 one of these follows:
    - `Possible quota limitations in place or slow response times detected. Switching to the <model> model for the rest of this session.`
    - `Please wait and try again later. To increase your limits, request a quota increase through AI Studio, or switch to another /auth method`
  - At low verbosity an INFO line follows: `This request failed. Press F12 for diagnostics, …`
  - Bodies seen in GitHub issues: `The model is overloaded. Please try again later.`,
    `Resource has been exhausted (e.g. check quota).`, `You have exhausted your capacity on this model.`
  - `Request cancelled.` is a user Esc, not an API problem.
- **Quota dialog** (`hooks/useQuotaAndFallback.ts`, `ProQuotaDialog.tsx`):
  - Text: `Usage limit reached for <model>.` then `Access resets at <time>.`
  - Capacity variants: `We are currently experiencing high demand for <model>.` and
    `We are currently experiencing high demand.`
  - Options: `Keep trying`, `Switch to <model>`, `Stop`, `Upgrade for higher limits`.
  - Reset time uses `Intl.DateTimeFormat('en-US',{hour:'numeric',minute:'2-digit',timeZoneName:'short'})`,
    for example `3:45 PM PST` or `3:45 PM GMT+3`. Newer ICU puts U+202F before AM/PM.
- **Network:** the SDK text appears inside `[API Error: …]`, for example `fetch failed`. Retry triggers
  include `fetch failed`, `ECONNRESET`, `ETIMEDOUT`, `ENOTFOUND`, `EAI_AGAIN` and `ECONNREFUSED`.
- **Anchors:** `✕ \[API Error:` for stopped, and `esc to cancel` for busy.

### OpenCode (`anomalyco/opencode`, formerly `sst/opencode`)

The latest release (v1.19.4) and `dev` render retries differently, so match both.

- **Working:** a block spinner, `■`/`⬝` or `[⋯]` with animations off, followed by `esc interrupt`. After
  one Esc it shows `esc again to interrupt`. It stays up during retries.
- **Own retry:**
  - v1.19.4: `<message> [retrying in 4s attempt #2]` or `… [retrying attempt #1]`. The message is cut at
    80 characters.
  - Subagent rows: `↳ Retrying (attempt n) · <message>`.
  - dev: `⚠ Retrying in 4s · attempt 2 · <message>` or `⚠ Retry due · attempt 2 · <message>`.
- **Retry messages** (`packages/opencode/src/session/retry.ts`, at most 5 retries):
  - `Provider is overloaded`, `Too Many Requests`, `Free usage exceeded, subscribe to Go`.
  - `<Limit> usage limit reached. It will reset in <N hours M minutes | less than a minute>. To continue using this model now, …`
- **Stop:**
  - v1.19.4 shows a red-bordered panel with the raw provider message and **no prefix**. Examples:
    `Too Many Requests`, `Service Unavailable`, `Provider response headers timed out after <ms>ms`,
    `Connection reset by server`.
  - dev shows `Error: <message>`.
  - Limits:
    - `You've used today's free messages. They reset at <time>.`
    - `Usage limit reached` / `<model> usage limit reached` with `Resets 3:45 PM today` or `Resets Oct 27, 3:45 PM`
    - `Free usage limit reached. …`
    - `Workspace usage limit reached. …`
    - `ChatGPT usage limit reached. …`
- **Regexes (subagent proposal, sanity-tested in Python, not on real screens):**
  - working: `(?i)\(esc to cancel, [0-9hms ]+\)|\besc (?:again to )?interrupt\b`
  - own_retry: `(?i)\[retrying(?: in [^\]]+?)? attempt #\d+\]|Retrying in \d+s · attempt \d+|Retry due · attempt \d+|Retrying \(attempt \d+\) ·|we're still on it|Trying to reach .* \(Attempt \d+/\d+\)`
  - usage_limit: `(?i)usage limit reached|exhausted your (?:daily )?(?:quota|capacity)|used today's free messages|free usage exceeded|out of credits`
  - Because OpenCode v1.19.4 has no stop prefix, "stopped" means no working marker on screen plus
    category text in the last few rows.

## 5. Orca CLI notes

- **Read:** `orca terminal read --terminal <h> --screen --json` returns
  `{id, ok, result:{terminal:{handle, status, tail:[string], tailRows:[{wrapped, trailingSpaces}], truncated, limited, oldestCursor, nextCursor, latestCursor, returnedLineCount, source:"screen"}}, _meta:{runtimeId}}`.
  - The text is **rendered**: no ANSI, and wide glyphs (`─ ❯ ● ✻ ■ •`) are preserved.
  - **Blank rows are omitted.**
  - A `--screen` read is the current frame only. There is no scrollback, and `--cursor` is not allowed with it.
  - Without `--screen`, the help says the read returns accumulated stream output (`--cursor` / `--limit`
    for paging). For these TUI agents it still reported `source:"screen"`, so always pass `--screen` and
    check `source`.
  - Each CLI call took about 0.6–1.3 s, so polling faster than about 1 Hz per terminal is unrealistic.
- **Always pass `--terminal`.** Without it, `read` and `send` target the *active* terminal of the current
  worktree, which may be another agent's session.
- **Send:** `orca terminal send --terminal <h> --text "continue" --enter --json`.
  - The text is delivered as a bracketed paste (`ESC[200~…ESC[201~`) followed by CR. `say hi` wrote 19
    bytes. Both agents submitted it correctly.
  - The JSON receipt can warn "no turn start was observed, so the Enter may have been swallowed" even
    when the turn did start. Do not treat that warning as failure; confirm by re-reading the screen.
  - `--wait-submit <s>` observes submission. `--retry-request <id>` makes a retry idempotent.
  - `--interrupt` sends Ctrl-C as one byte, `\x03`.
  - With `--enter`, Orca refuses the send when the agent shows an approval dialog. The receipt is
    `ok:false, error.code:"agent_prompt_blocked"`, plus a `--retry-request` id. It does **not** refuse
    for Codex's rate-limit model picker (see §6).
  - **Without** `--enter`, `--text` writes raw bytes with no paste wrapping. Escape sequences pass
    through, for example `$'\x1b[B'` (3 bytes) or `$'\r'`, so this path presses keys in any dialog.
- **Discovery:** `orca terminal list --json` gives `handle`, `worktreePath`, `title` (Codex sets `Codex ready`),
  `agentIdentity` (`"claude"` / `"codex"`), `connected`, `writable`, `lastOutputAt` and a squashed one-line
  `preview`. `agentIdentity` is a cheap way to pick the detector.
- **Create and close:**
  - `orca terminal create --worktree path:<dir> --title <t> --command "<typed into the default shell>" --json`
    returns `result.terminal.handle`.
  - `orca terminal close --terminal <h>` kills the pty.
- **Git Bash gotcha:** MSYS rewrites `--text "/clear"` to `C:/Program Files/Git/clear`. Use
  `MSYS_NO_PATHCONV=1` when scripting from Git Bash. Spawning `orca` from Rust is unaffected.

## 6. Exit and menu screens

These were captured live, with the same isolation as above. All of these fixtures are real captures.
Paths were neutralised to `C:\work` and `~\projects`.

### Exit

- **Claude** (`claude/exited_slash_exit.txt` via `/exit`, `claude/exited_ctrl_c.txt` via Ctrl+C twice):
  - Claude renders in the alternate screen. On exit **the whole TUI disappears**: the input box, rules,
    footer and the API error that was on screen are all gone.
  - What is left: the earlier shell rows, then `Resume this session with:` /
    `claude --resume <uuid>` / `PS C:\work\demo-project>`.
  - After the first Ctrl+C the input box is still there and the footer row becomes
    `  Press Ctrl-C again to exit` (`claude/ctrl_c_pending.txt`). The agent is still alive.
- **Codex** (`codex/exited_quit.txt` via `/quit`):
  - Codex also clears the TUI, so no `› ` row remains.
  - What is left: `To continue this session, run:` / `  codex resume <uuid>` / shell prompt.
  - The `WARNING: … PATH aliases …` rows come from my `CODEX_HOME` living under `%TEMP%`. Real users don't see them.
  - **Ctrl+C could not exit Codex 0.164 through Orca/ConPTY on Windows.** I tried `--text $'\x03'`,
    `--interrupt` (both 1 byte) and `$'\x03\x03'`. The key is received: the first press blanks the
    `? for shortcuts` hint. Codex never quit, so there is no `exited_ctrl_c` fixture for Codex. The
    binary has `<key> again to quit`.
- **Orca metadata after exit:**
  - `title` reverts to the shell (`C:\Program Files\PowerShell\7\pwsh.exe`). Claude sets
    `Claude Code` while running.
  - `agentIdentity` **stays `"claude"`** after exit. It is stale, so it is not a liveness signal.

### Menus and dialogs

When a dialog is open, Claude does not render its input box and Codex does not render its composer.

| Fixture | Key rows |
|---|---|
| `claude/menu_permission.txt` | `Bash command` … ` Do you want to proceed?` / ` ❯ 1. Yes` / `   2. Yes, and always allow access to …` / `   3. Yes, and switch to auto mode · …` / `   4. No` / ` Esc to cancel · Tab to amend` |
| `claude/menu_model.txt` (`/model`) | `▔▔▔…` / `   Select model` / `   ❯ 1. Default (recommended) ✔ …` … `   Enter to set as default · s to use this session only · Esc to cancel` |
| `claude/menu_startup_auto_mode.txt` | ` Make auto mode your default permission mode?` / `   ❯ Yes, set auto mode as my default permission mode` / `     No, keep manual mode` (**no hint footer**). It appears on launch when `defaultMode` is not auto. |
| `codex/menu_approval.txt` | `  Would you like to run the following command?` / `  $ touch harn-test.txt` / `› 1. Yes, proceed (y)` / `  2. Yes, and don't ask again … (p)` / `  3. No, and tell Codex what to do differently (esc)` / `  Press enter to confirm or esc to cancel` |
| `codex/menu_rate_limit_switch.txt` | `⚠ Heads up, you have less than 5% of your 5h limit left. …` / `  Approaching rate limits` / `  Switch to gpt-6-luna for lower credit usage?` / `› 1. Switch to gpt-6-luna   …` / `  2. Keep current model` / `  3. Keep current model (never show again) …` / `  enter select · esc back`. The mock triggered it with a normal reply carrying `x-codex-primary-used-percent: 96`. |
| `codex/menu_rate_limit_switch_after_continue.txt` | The screen after `continue`+Enter: the picker took option 1 (footer is now `GPT-6-Luna medium`). No prompt was sent. |

**What typing "continue" into a dialog actually does:**

| Dialog | `orca terminal send --text continue --enter` | Raw bytes `continue\r` |
|---|---|---|
| Claude permission | refused: `ok:false`, `error.code:"agent_prompt_blocked"` | **chose `1. Yes` and ran the command** |
| Codex approval | refused: `agent_prompt_blocked` | not tested |
| Codex rate-limit picker | **`ok:true`: Enter chose `1. Switch to gpt-6-luna`** | n/a |
| Claude startup dialog | no visible effect, sent twice (receipt not inspected) | raw `\x1b[B` then `\r` moved the selection and chose the option |

Orca's guard covers approval prompts but not every picker. harn needs its own menu veto.

**Dialog strings from the binaries**

- **Claude key hints** render as `<Key> to <action>` joined with ` · `. Keys are capitalised (`Enter`,
  `Esc`, `Tab`, `Space`, `←/→`, or a letter). Actions found include cancel, confirm, select, close, go back,
  continue, save, toggle, dismiss, view, amend, open, choose, add, skip and expand.
  - Literal strings: `Press Esc to cancel`, `Press Esc to go back`, `Esc to close`, `Enter to submit`,
    `Space to toggle, Enter to confirm, …` and `n/N to navigate`.
  - Questions: `Do you want to proceed?`, `Do you want to use this API key?`,
    `Do you want to allow this connection?`, `Do you want to allow Claude to fetch this content?`,
    `Would you like to:`, `Would you like to install it?`
  - The busy footer is **lower-case** `esc to interrupt`.
- **Codex** strings:
  - Footers: `Press <key> to confirm or <key> to cancel` and `… or <key> to go back`, rendered as
    `Press enter to confirm or esc to cancel`. Also `<key> select · <key> back`, rendered as
    `enter select · esc back`, and `<key> again to quit`.
  - Questions: `Would you like to run the following command?`, `… make the following edits?`,
    `… grant these permissions?`, `… send input to the existing terminal?`,
    `Do you want to approve network access to "<host>"?`
  - Options: `Yes, proceed (y)`, `No, and tell Codex what to do differently (esc)`,
    `No, continue without running it`, `Keep current model (never show again)`.

### Regexes (`(?m)`; apply to the last ~40 rows)

**(a) Agent input box present at the bottom.** This is the positive "alive and at prompt" signal.

- **Claude:**
  1. Find the last row `^❯(?: .*)?$` at column 0 that has a rule row `^─{10,}` both directly above and
     directly below it. Use a prefix match, because the rule can carry a label.
  2. Accept only if no row below the lower rule is a shell prompt:
     `^PS [A-Za-z]:\.*>|^[A-Za-z]:\[^>]*>|^\S*[$#%] ?$`.
  3. Accept only if at most ~15 rows sit below the lower rule.
  - Do **not** whitelist footer rows. Statuslines, `⏵⏵ …` / `⏸ manual mode on …`, `● main`,
    `◯ <agent> …` and background-task rows are all arbitrary.
  - User-echo rows (`❯ say hi`) never have rules on both sides.
- **Codex:**
  1. Find the last row `^› `. It must not match `^› \d+\. `, which is a highlighted menu option.
  2. It must be followed within 3 rows by the footer, `^  \S.* · .+` (model · cwd) or `^  \? for shortcuts`.
  3. There must be no shell prompt below it.
- Exiting removes these structures entirely, so if they are missing, either the agent has exited or a
  dialog is open. Either way, never send.

**(b) Menu or dialog open** (veto any send):

- **Claude:**
  - `^\s+❯ \S` matches an indented selector. The input-box `❯` is at column 0.
  - `^\s*(?:Esc|Enter|Tab|Space|[←↑]\S*) to [a-z]` matches a capitalised hint row.
  - `Do you want to [^?\n]+\?` and `^\s*Would you like to` match the questions.
- **Codex:**
  - `^› \d+\. ` matches a highlighted numbered option.
  - `Press enter to confirm or esc to (?:cancel|go back)` and `^\s*enter select · esc back` match the footers.
  - `Would you like to [^?\n]+\?` and `Do you want to approve network access` match the questions.
