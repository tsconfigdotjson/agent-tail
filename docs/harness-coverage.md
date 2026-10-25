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
| **GitHub Copilot CLI** | CI: plain/`done`, single tool/`running_tool`, unattended permission denial/firm `awaiting_approval`, parallel tools, terminal `session.error`/`errored`. Fixtures also cover approved/user-denied completions, `abort`/`interrupted` followed by routine shutdown, usage totals, and idle. | Live TUI approve/deny capture; `subagent.started/completed` following into background helpers; unobserved ask-user tool. Follow-up: #107. | High → covered core, helper/TUI follow-up |
| **Pi** | CI: plain/`done`, single tool/`running_tool`, parallel tools, failing tool result recovery, terminal API error/`errored`. Fixtures cover abandoned branches, parallel pending-then-thinking transitions, usage/cost totals, `aborted`/`interrupted`, and idle. | Compaction fixture, unknown-entry fixture, `deferred` stop reason. Helpers/approval are unsupported by design. Follow-up: #108. | High → covered core |
| **Claude Code** | CI: plain, approval/pending/bypass, API errors, non-ASCII, long output. Fixtures: approval stall, background helpers, firm turn ends, late prompt, rejected tool/interrupt. Local drivers also cover ask-user, plan, interrupts, resume, accept-edits, compaction and mode/model. | Stable-CI ask-user/plan/interrupt/background drivers; cost/idle fixture checkpoints; dedicated multi-tool fixture. Follow-up: #108. | Medium |
| **Codex** | CI: plain, token usage, on-request inferred approval, never running, ask-user, parallel tools, abort, task error, resume. Fixtures cover approval modes and resumed sessions. | Idle checkpoint and a committed unknown-record fixture. | Low |
| **Gemini CLI** | CI: plain and completed-tool turns. Fixture: `error` exercises the adapter's terminal `type:error` mapping. | No real `type:error` driver path: 0.65.0 rewinds the prompt on an API failure. No interrupted or multi-tool fixture yet. Follow-up: #108. | Medium |
| **OpenCode** | CI: plain and tool turns. Fixtures: live `tool-running`/inferred approval, pending `question`/firm `awaiting_answer`, `step-finish aborted`/interrupted. | Error `step-finish`, multi-tool, and child-session/background fixtures. Follow-up: #108. | Medium |
| **Claude Desktop** | Offline Xvfb stand-in; fixtures cover plain, tool-turn, tool-stall, terminal error and `terminal_reason: aborted_streaming` interruption. | No committed redacted live Desktop capture; adopter still `ported-unverified`. Follow-up: #108. | Medium |
| **Cursor** | Handmade transcript covers thinking, running, done, errored, pending `AskUserQuestion`/awaiting-answer and user-stop interruption. Synthetic `state.vscdb` covers waiting/approved/denied decisions. | Live approval-wait capture remains the Mac smoke path; Cursor CLI stays out of the mock matrix by design. Follow-up: #108. | Low/Medium |

## Known behavior fixed by this audit

The Copilot abort/error fixtures exposed a real reducer-visible bug: 1.0.103 writes
`assistant.turn_end`, then `abort` or `session.error`, then a routine `session.shutdown`.
The adapter previously treated the routine shutdown as another normal turn end, so a final
status read `done` instead of `interrupted`/`errored`. The Copilot parser now marks a
terminal record and keeps it through that shutdown; both sequences have fixtures and a unit
test.

## Adding a case

1. Add a mock scenario and driver scenario, then list the scenario in `harness-ci/ci/`.
2. Or add a fixture with `input/session.jsonl`, `meta.toml`, checkpoints and regenerated
   `expected/events.jsonl`.
3. Run `agent-tail fixture scan`, `cargo test`, and the offline runner.
4. Regenerate the support matrix when fixture metadata or CI lists change.
