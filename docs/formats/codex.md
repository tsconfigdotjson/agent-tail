# Codex rollout format

This note is based on a read-only survey of one rollout (`cli_version`
`0.118.0-alpha.2`, `source: vscode`, written by the VS Code extension), plus live harness runs
and fixtures for **0.160.1**. Treat the 0.118 details as one sample; the 0.160.1 evidence is
stronger.

## Paths and layout
```
~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl   (.jsonl.zst for compressed history: none here)
~/.codex/session_index.jsonl      {id, thread_name, updated_at}: a thread title source
~/.codex/state_5.sqlite, logs_1.sqlite, sqlite/codex-dev.db   (not surveyed)
```
`CODEX_HOME` overrides `~/.codex`. The tracker reads it when set.

## Live CLI check: 0.160.1

Codex CLI **0.160.1** from npm was run under a PTY against the local mock (no real backend).
An absolute `CODEX_HOME` is the `.codex` home: config, SQLite state, and
`sessions/YYYY/MM/DD/rollout-*.jsonl` all live below it, and `Tracker::from_env`'s
`$CODEX_HOME/sessions` root is correct. The CLI also accepts a relative `CODEX_HOME`, resolved
against its current working directory; the tracker ignores relative values as a safety rule.
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
remain covered by fixtures rather than this survey.

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
  had neither `turn_aborted` nor `turn_complete`; the live 0.160.1 run saw `turn_aborted` with
  `reason: interrupted`, and the 0.160.1 fixtures cover `turn_complete`.
- **Usage limit:** a 429 that exhausts Codex's retries ends in `task_complete.error`, e.g.
  `{"message": "exceeded retry limit, last status: 429 Too Many Requests",
  "codex_error_info": {"response_too_many_failed_attempts": {"http_status_code": 429}}}`.
  The provider body is not persisted. `agent-tail` keeps firm `errored` and adds
  `stop_reason: "usage_limit"`; no reset time is recorded on this path.
- **Approval:** `turn_context.approval_policy` is firm for "can't ask" when it is `never`.
  `approvals_reviewer: auto_review` also means no human; `on-failure` is the older name for
  `on-request`. `sandbox_policy.type` is recorded in `SessionState.sandbox` but does not by
  itself prove no human can be asked (full access is `danger-full-access` with `never`).
  The live 0.160.1 run confirmed that **approval requests are not written to the rollout**: a pending
  approval is a `function_call` with no output, and no `exec_approval_request` or
  `apply_patch_approval_request` record appears. A pending ask-user tool is the same shape,
  identified by the `request_user_input` function name.
- **Tool pending:** a `function_call`/`custom_tool_call` with no matching `call_id` output.
  `write_stdin` polls a running exec session (`session_id`): it is not a new action. A
  SIGKILL after the call but before its output leaves only that pending row; no
  `turn_aborted` is written, so the honest state is `running_tool`, not `interrupted`.
  `ci-codex_hard_kill_mid_tool` pins this silent-death shape.
- **Ask-user tool:** `request_user_input` (first observed in 0.160). It was seen live in Plan mode;
  it also has no approval/request event, just the `function_call` until its output arrives.
- **Subagents:** none seen.

## Versions seen
`cli_version` 0.118.0-alpha.2 in the original one-rollout survey. CI and committed fixtures use
0.160.1.

## How agent-tail handles these records

- Reads `turn_context.approval_policy`, `approvals_reviewer`, and `sandbox_policy`. Under
  `never`, a long `exec_command` remains firmly `running_tool`.
- Matches tool output by `call_id`, so parallel `function_call`s stay pending until their own
  outputs arrive.
- Maps `turn_aborted` to `interrupted` and keeps its `reason`; `task_complete` and
  `turn_complete` are firm normal ends.
- Reads `session_index.jsonl` beside `sessions/` (so `CODEX_HOME` works) and uses
  `thread_name` as the title when present, with the first prompt as the fallback.
- Skips `.jsonl.zst` compressed history by design; live sessions are not compressed.

## Open questions
- Approval requests are not written to rollouts in 0.160.1. Recheck a future CLI before relying
  on an approval event; the current scenarios assert the negative.
- `CODEX_HOME` is confirmed against 0.160.1 for absolute paths. Older-day-folder resume and
  `.jsonl.zst` history still need a session old enough to exercise them.
