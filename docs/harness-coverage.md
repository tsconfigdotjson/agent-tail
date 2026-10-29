# Harness coverage

This is the adapter-by-adapter audit from #106. It complements the generated support matrix:
the matrix says whether a harness has any CI evidence, while this table says which status
paths the driver and fixture corpus actually exercise.

Statuses are `SessionState.activity` values plus the permission/mode, multi-tool, usage/cost,
title and helper/branch behavior that feed them. "Driver" means a scenario under
`harness-ci/driver/scenarios`; "CI" means it is listed in `harness-ci/ci/scenarios-*.txt`.
Fixtures under `fixtures/` run in `status_fixtures`, whether or not a driver exists.

| Adapter | Now covered | Still missing / deferred | Priority |
|---|---|---|---|
| **GitHub Copilot CLI** | CI: plain/`done`, single tool/`running_tool`, hard-kill mid-tool/`running_tool` then `idle`, unattended permission denial/firm `awaiting_approval`, parallel tools, terminal `session.error`/`errored`. Fixtures cover approved/user-denied completions, `abort`/`interrupted` followed by routine shutdown, usage totals, idle, the SIGKILL silent-death shape (no execution_complete/abort/shutdown), and an interleaved background helper that keeps the parent at `background_work` after its turn ends. | Live TUI approve/deny capture (no safe non-interactive path found; handmade approved/user-denied fixtures remain); unobserved ask-user tool. Follow-up: #107's TUI gap. | High → core and helper covered, TUI gap documented |
| **Pi** | CI: plain/`done`, single tool/`running_tool`, hard-kill mid-tool/`running_tool` then `idle`, parallel tools, failing tool result recovery, terminal API error/`errored`. Fixtures cover abandoned branches, parallel pending-then-thinking transitions, usage/cost totals, `aborted`/`interrupted`, the SIGKILL silent-death shape (no toolResult/aborted row), compaction on the active branch, an unknown entry type that still ends the turn, and idle. | A `deferred` stop reason has no real session, so no fixture claims it. Helpers/approval are unsupported by design. | High → covered core |
| **Claude Code** | CI: plain, approval/pending/bypass, API errors, non-ASCII, long output, ask-user, plan exit, mid-tool interrupt, hard-kill mid-tool, accept-edits, direct background-agent. Fixtures: approval stall, background helpers, firm turn ends, late prompt, rejected tool/interrupt, a hard-kill capture that stays `running_tool` and then falls to `idle` without an interrupt row, cost/idle checkpoints. Local drivers also cover resume, compaction and mode/model. | A dedicated multi-tool fixture is still missing. A nested background helper is not in the driver: 2.1.281 counted it in the parent's pending count while 2.1.310 did not, so the CI scenario uses one direct helper; nesting stays in the fixture/unit corpus. | Medium |
| **Codex** | CI: plain, token usage, on-request inferred approval, never running, hard-kill mid-tool/`running_tool` then `idle`, ask-user, parallel tools, abort, task error, resume. Fixtures cover approval modes, resumed sessions, and a SIGKILL rollout that ends on a pending `function_call` with no output or `turn_aborted`. | Idle checkpoint and a committed unknown-record fixture. | Low |
| **Gemini CLI** | CI: plain, completed-tool and multi-tool turns, and hard-kill mid-tool. Fixtures: `error` exercises the adapter's terminal `type:error` mapping; `multi-tool` is a real 0.65.0 journal with two completed calls in one `toolCalls` array and two function responses; hard-kill pins the pre-tool journal that actually reached disk (`thinking` / `prompt_sent`, no `toolCalls`). | No real `type:error` driver path: 0.65.0 rewinds the prompt on an API failure. No interrupted fixture: no real journal shape was found, and hard-kill is deliberately not turned into an interrupt. | Medium |
| **OpenCode** | CI: plain, tool and hard-kill mid-tool turns. Fixtures: live `tool-running`/inferred approval, the SIGKILL silent-death shape (a running tool part with no `step-finish`), pending `question`/firm `awaiting_answer`, `step-finish aborted`/interrupted, `step-finish error`/errored, parallel tool parts with one pending call, and a child-session/background parent. | A live interactive TUI approval is still uncaptured; only the pending/running tool part is on disk, so it stays inferred after the stall. | Medium |
| **Claude Desktop** | Offline Xvfb stand-in; fixtures cover plain, tool-turn, tool-stall, terminal error and `terminal_reason: aborted_streaming` interruption. | No committed redacted live Desktop capture: none was already on disk or in artifacts in this lane, and the live Jev path needs its Actions secret. The adapter stays `ported-unverified`. | Medium |
| **Cursor** | Handmade transcript covers thinking, running, done, errored, pending `AskUserQuestion`/awaiting-answer and user-stop interruption. Synthetic `state.vscdb` covers waiting/approved/denied decisions. Donated Cursor 3.27.12 Mac snapshots under `fixtures/cursor/3.27.12/macos/` pin `run_terminal_command_v2` waiting with `status: loading`/`additionalStatus: pending` while `composer.status` is `aborted`, then `success`/`completed` after approve and `rejected` after deny. Run Mode was Allowlist (empty). | Cursor CLI stays out of the mock matrix by design; CI keeps the synthetic `state.vscdb` and transcript fixtures and does not add a Cursor Desktop stand-in or live login. | Low/Medium |

## Deferred after #108

These are deliberate, evidence-backed gaps rather than silent skips:

- **Pi `deferred` stop reason:** no real session was found in the local Pi corpus, so the
  adapter keeps it as `Meta` and no fixture claims a terminal state.
- **Gemini interrupted:** 0.65.0 rewinds a failed prompt and does not write a pending tool
  call. No real journal shape exists for an interrupted turn, so no driver expectation was
  invented. The hard-kill scenario pins what a SIGKILL actually leaves (pre-tool
  `thinking` / `prompt_sent`), not an interrupt. Multi-tool is filled from a real 0.65.0
  journal and runs in the matrix.
- **Nested Claude Code background helper in the driver:** the stable scenario uses one direct
  helper. Pinned 2.1.281 includes a nested background helper in the parent's
  `pendingBackgroundAgentCount` (2) while 2.1.310 counts only the launched helper (1), so an
  outer task notification alone cannot clear the nested count. Nested helper linkage remains
  covered by fixtures and observation units rather than a version-gated driver.
- **Claude Desktop live capture:** the offline stand-in and handmade fixtures are committed,
  but no redacted live Jev capture was already available; starting an auth-dependent Desktop
  chase is out of scope for this lane.
- **Cursor live login in CI:** the approval snapshots are filled by the committed Cursor
  3.27.12 Mac dumps, but CI still keeps the synthetic `state.vscdb` and transcript fixtures
  and does not add a Cursor Desktop stand-in or live login. Cursor CLI stays out of the mock
  matrix by design.

## Known behavior fixed by this audit

The Copilot abort/error fixtures exposed a real reducer-visible bug: 1.0.103 writes
`assistant.turn_end`, then `abort` or `session.error`, then a routine `session.shutdown`.
The adapter previously treated the routine shutdown as another normal turn end, so a final
status read `done` instead of `interrupted`/`errored`. The Copilot parser now marks a
terminal record and keeps it through that shutdown; both sequences have fixtures and a unit
test.

Copilot writes helper records into the parent's `events.jsonl` with a top-level `agentId`.
The adapter now keeps those records in a separate helper state and uses
`subagent.started`/`subagent.completed` as the launch/end markers. Previously a helper
prompt could become the parent's title or reset its turn flags, and the parent looked `done`
while its helper was still running. The `subagent-helper` fixture pins both transitions.

## Adding a case

1. Add a mock scenario and driver scenario, then list the scenario in `harness-ci/ci/`.
2. Or add a fixture with `input/session.jsonl`, `meta.toml`, checkpoints and regenerated
   `expected/events.jsonl`.
3. Run `agent-tail fixture scan`, `cargo test`, and the offline runner.
4. Regenerate the support matrix when fixture metadata or CI lists change.
