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
`CODEX_HOME` overrides `~/.codex`. That is not verified here (open question).

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
- **Turn end:** `event_msg/task_complete` (same `turn_id`). Firm. `turn_aborted` and
  `turn_complete` were not in this sample. tvdash handles them from its 0.160 fixtures.
- **Approval:** `turn_context.approval_policy` is firm for "can't ask" when it is `never`.
  This sample is `on-request` with a `workspace-write` sandbox, but **no approval event appeared**
  (no `exec_approval_request` or `apply_patch_approval_request`), so whether rollouts record approval
  requests at all is **unverified**. *Inference:* approvals travel over the app-server
  protocol and may never reach the rollout.
- **Tool pending:** a `function_call`/`custom_tool_call` with no matching `call_id` output.
  `write_stdin` polls a running exec session (`session_id`): it is not a new action.
- **Ask-user tool:** `request_user_input` (tvdash, from 0.160). None here.
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
- Skips `.jsonl.zst` (by design).

CLIs on this Mac's PATH: none of `codex`, `gemini`, `opencode`, `copilot` or `cursor-agent`
(Codex here comes from the VS Code extension). Not checked on vibe-os.

## Open questions
- Everything on vibe-os (Codex versions, `.zst` presence, approval events, `turn_aborted` shape,
  Claude Code and Cursor there, and installed CLIs). Rerun once Tailscale is up.
- Are approval requests ever written to rollouts in 0.160+? (Phase 4 harness CI.)
- `CODEX_HOME` semantics: not researched.
