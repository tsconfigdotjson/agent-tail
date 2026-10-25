# Codex rollout format

Survey: read-only, structure only, 2026-10-23. **This Mac only: 1 rollout** (`cli_version`
`0.118.0-alpha.2`, `source: vscode`, written by the VS Code extension). **vibe-os was not
surveyed**: `ssh vibe-os` failed with "Could not resolve hostname" because Tailscale is stopped on
this Mac, and the scope was cut to skip retries. Treat everything below as one sample. tvdash's
`codex-0.160-*.jsonl` fixtures remain the better evidence for 0.160.

## Paths and layout
```
~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl   (.jsonl.zst for compressed history: none here)
~/.codex/session_index.jsonl      {id, thread_name, updated_at}: a thread title source
~/.codex/state_5.sqlite, logs_1.sqlite, sqlite/codex-dev.db   (not surveyed)
```
`CODEX_HOME` overrides `~/.codex`. The tracker reads it when set.

## A4 live CLI check (2026-10-24)

Codex CLI **0.160.1** from npm was run under a PTY against the local mock (no real backend).
An absolute `CODEX_HOME` is the `.codex` home: config, SQLite state, and
`sessions/YYYY/MM/DD/rollout-*.jsonl` all live below it, and `Tracker::from_env`'s
`$CODEX_HOME/sessions` root is correct. The CLI also accepts a relative `CODEX_HOME`, resolved
against its current working directory; the tracker ignores relative values as a B8 safety rule.
CI always gives the CLI an absolute temp path.

A custom `model_providers.<id>` entry with `base_url` pointing at the mock, `env_key` for a
dummy key, and `wire_api = "responses"` works. A separate `/v1/responses` request asks the
model for a one-line title; it does not add a rollout record and 0.160.1 did not write
`session_index.jsonl` (the live state is in SQLite). The tracker's index reader remains for
older/other layouts.

The live rollout uses the same envelope as the fixtures, plus `event_msg/item_completed`
(for `UserMessage` and `AgentMessage`), `token_usage_record`, and `world_state`. The status
records used by this library are the same: `task_started`, `turn_context`, `token_count`,
`task_complete`, and `turn_aborted`.

`task_complete.error` is an **object** in 0.160.1:
`{"message": "…", "codex_error_info": "internal_server_error"}` (older fixtures may show a
string). `turn_aborted.reason` was `interrupted` for Esc.

**No approval event reached the rollout.** Under `on-request`, an escalated `exec_command`
left a visible approval prompt but only wrote a `response_item/function_call` with no
`function_call_output`; no `exec_approval_request`, `apply_patch_approval_request`,
`request_permissions`, `elicitation_request`, `exec_command_begin/end` or `error` record
appeared. `approval_policy = "untrusted"` is rejected by 0.160.1 (`no longer supported`); the
current docs mark it retired, and `-a` accepts only `on-request` and `never`.

`codex resume <id>` appended the new prompt to the same rollout file (one file, two prompts).
The live CLI produced no `.jsonl.zst`; compressed history and resume from an older day folder
remain covered by fixtures rather than this spike.

## Record catalogue (1 rollout; counts)
Envelope: `{timestamp, type, payload}`.

| type / payload.type | n | payload fields |
|---|---|---|
| session_meta | 1 | `id, timestamp, cwd, originator, cli_version, source, model_provider, base_instructions, dynamic_tools, git.{branch,commit_hash,repository_url}` |
| turn_context | 4 | `turn_id, cwd, approval_policy, sandbox_policy.{type,network_access,writable_roots,exclude_tmpdir_env_var,exclude_slash_tmp}, model, effort, summary, personality, collaboration_mode.{mode,settings}, current_date, timezone, realtime_active, truncation_policy, developer_instructions` |
| event_msg/task_started | 4 | `turn_id, model_context_window, collaboration_mode_kind` |
| event_msg/user_message | 4 | `message, images, local_images, text_elements` |
| event_msg/agent_message | 30 | `message, phase, memory_citation` |
| event_msg/token_count | 65 | `info` (null or `{total_token_usage, last_token_usage, model_context_window}`), `rate_limits.{primary,secondary,credits,limit_id,limit_name,plan_type}` |
| event_msg/task_complete | 4 | `turn_id, last_agent_message` |
| response_item/message | 36 | `role` (developer/user/assistant), `content[].type` (input_text/output_text), `phase` |
| response_item/reasoning | 36 | `summary[], content (null), encrypted_content` |
| response_item/function_call | 61 | `name` (exec_command 59, write_stdin 2), `arguments` (JSON string: `cmd, workdir, yield_time_ms, max_output_tokens` / `session_id, chars`), `call_id` |
| response_item/function_call_output | 61 | `call_id, output` |
| response_item/custom_tool_call | 30 | `name` (apply_patch), `input`, `status` (completed), `call_id` |
| response_item/custom_tool_call_output | 30 | `call_id, output` |

`token_count.info.total_token_usage` keys: `input_tokens, cached_input_tokens, output_tokens,
reasoning_output_tokens, total_tokens` (a running total; `last_token_usage` is the per-call delta).

Enums seen: `approval_policy: on-request`; `sandbox_policy.type: workspace-write`; `model: gpt-5.4`;
`effort: medium`; `summary: none`; `personality: friendly`.

## Status signals
- **Turn start:** `event_msg/task_started` (with `turn_id`), then `turn_context`, then
  `user_message`. Firm.
- **Turn end:** `event_msg/task_complete` (same `turn_id`). Firm. The original 0.118 sample
  had neither `turn_aborted` nor `turn_complete`; A4 saw `turn_aborted` with
  `reason: interrupted`, and tvdash's 0.160 fixtures cover `turn_complete`.
- **Approval:** `turn_context.approval_policy` is firm for "can't ask" when it is `never`.
  `approvals_reviewer: auto_review` also means no human; `on-failure` is the older name for
  `on-request`. `sandbox_policy.type` is recorded in `SessionState.sandbox` but does not by
  itself prove no human can be asked (full access is `danger-full-access` with `never`).
  A4 confirmed on 0.160.1 that **approval requests are not written to the rollout**: a pending
  approval is a `function_call` with no output, and no `exec_approval_request` or
  `apply_patch_approval_request` record appears. A pending ask-user tool is the same shape,
  identified by the `request_user_input` function name.
- **Tool pending:** a `function_call`/`custom_tool_call` with no matching `call_id` output.
  `write_stdin` polls a running exec session (`session_id`): it is not a new action.
- **Ask-user tool:** `request_user_input` (tvdash, from 0.160). A4 saw it live in Plan mode;
  it also has no approval/request event, just the `function_call` until its output arrives.
- **Subagents:** none seen.

## Versions seen
`cli_version` 0.118.0-alpha.2 (this Mac). tvdash fixtures: 0.160.1.

## What tvdash gets wrong or misses (`src/sessions/codex.rs`)
- `turn_context.approval_policy` and `sandbox_policy` are read only for `cwd` (codex.rs:51 to 56).
  Under `never`, a long `exec_command` still turns into "may need approval" after 20 s.
- Matches tool output to "the last tool call" rather than by `call_id`. With parallel calls
  (2 to 5 `function_call`s before their outputs are common here), the first output flips the
  session to "thinking" while others are still pending.
- `turn_aborted` maps to `EndTurn` ("done"). Interrupted and aborted are not told apart, and its
  `reason` is unread.
- `session_index.jsonl`'s `thread_name` is ignored. The title is the first prompt line.
  This library reads the index (beside `sessions/`, so `CODEX_HOME` works) and uses
  `thread_name` when present, with the first prompt as the fallback.
- Skips `.jsonl.zst` (by design).

CLIs on this Mac's PATH: none of `codex`, `gemini`, `opencode`, `copilot` or `cursor-agent`
(Codex here comes from the VS Code extension). Not checked on vibe-os.

## Open questions
- Everything on vibe-os (Codex versions, `.zst` presence, Claude Code and Cursor there, and
  installed CLIs). Rerun once Tailscale is up.
- Approval requests are not written to rollouts in 0.160.1. Recheck a future CLI before relying
  on an approval event; the A4 scenarios assert the current negative.
- `CODEX_HOME` is confirmed against 0.160.1 for absolute paths. Older-day-folder resume and
  `.jsonl.zst` history still need a session old enough to exercise them.
