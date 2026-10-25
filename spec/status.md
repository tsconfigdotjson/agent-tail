# Status conformance (v0.1)

This file freezes the status contract a port implements without linking Rust or knowing
`agenttail`'s internals. It sits beside the machine-readable schemas in
[`spec/schema/`](schema/):

- [`event.schema.json`](schema/event.schema.json): one normalized event, as printed by
  `agenttail events`.
- [`session-state.schema.json`](schema/session-state.schema.json): the full
  `SessionReducer::state(now)` object.
- [`status.schema.json`](schema/status.schema.json): one line of a fixture
  `expected/status.jsonl`, which is both a timing instruction and an optional expected
  status.

The fixture runner and the commands are described in
[`docs/conformance.md`](../docs/conformance.md). This document explains the status meaning
and the checkpoint protocol.

## Events

An event carries common provenance (`id`, `harness`, `source`, `offset`, `length`, `line`,
`record_ids`, `at`, optional `sidechain` and `off_chain`) plus a `type` and its payload. `source` is the
caller's file index (`0` for a single transcript), offsets and lengths are byte positions in
the original input, and `line` is one-based. `at` is seconds since the Unix epoch, or `null`
when the record has no time of its own. Ids are deterministic for the same harness, source
and bytes. A port should produce the same values, not merely the same shape.

The v0.1 event types are `user_prompt`, `assistant_text`, `reasoning`, `tool_call`,
`tool_result`, `approval_requested`, `approval_completed`, `turn_start`, `turn_end`, `usage`,
`permission_mode`, `sandbox_policy`, `model_change`, `title`, `cost`, `helper_spawn`,
`helper_end`, `compaction`, `session_info`, `meta` and `unknown`. Known records with no richer
form are `meta`; records or lines this version does not understand are `unknown`, never
dropped. Required fields are the deterministic id, provenance and the fields that make the
event meaningful. An optional field may be absent or `null`; `agenttail` emits explicit
`null`s, and canonical comparison treats the two as equal. `sidechain` and `off_chain` are
omitted unless true; the latter marks a record on an abandoned Pi branch that the reducer
ignores. A title's origin is `title_source`; a tool call's short display line is
`activity_line`, keeping both distinct from provenance `source` and `line`.

The event contract is the **Full** parse. `ParseMode::Lean` is a local first-read
optimization that may shorten long strings before parsing; it is not a different event shape
and there is no per-event `truncated` flag in v0.1. Lean's count of shortened lines is
`SessionState.diagnostics.cut`. Use Full when producing or checking event dumps.

## Session state

`SessionState` is the headline status plus the evidence behind it:

| field | meaning |
|---|---|
| `activity` | what the agent is doing: `awaiting_answer`, `awaiting_approval`, `running_tool`, `thinking`, `background_work`, `done`, `interrupted`, `errored`, `idle`, `unknown` |
| `certainty` | `firm` when the transcript says so, `inferred` for a timing/shape guess |
| `reason` | the one rule that decided the activity |
| `since`, `last_at`, `last_write` | seconds since the epoch, or `null`; the activity's start, the last state-changing entry, and the newest transcript/helper write |
| `background` | helpers (subagents/tasks) still running, each with its own activity, certainty, reason and times |
| `permission`, `sandbox` | the harness's last recorded mode/policy, or `null` |
| `error` | the harness's error text when the turn ended with one, or `null` |
| `meta` | `project`, `folder`, `branch`, `title` |
| `cost_usd`, `usage`, `out_tokens` | recorded spend and token counts, never estimated |
| `recent` | the last few activity lines (`prompt`, `command`, or `tool` with optional detail) |
| `diagnostics` | `lines`, `unparsed`, `cut` |

Optional state fields may be absent or `null`, as with events. `Activity` values are
machine-readable; the coarse four-class grouping (`needs-you`, `working`, `done`, `idle`) is
`Activity::class()` in the Rust library and is deliberately not part of this JSON shape. A
port may use it or its own wording.

## Checkpoints

Each fixture's `expected/status.jsonl` has one JSON object per line. Required fields:

```json
{"after_line": 24, "now_offset_s": 30.0}
```

`after_line` is the number of entries read from the main transcript before checking;
`after_line: 0` is before the first entry. `now` is the checkpoint line's own record time
plus `now_offset_s` seconds, falling back to the most recent record time seen for an entry
with no time of its own.

A checkpoint may add the expected status and fixture steps:

```json
{"after_line": 8, "now_offset_s": 0.0, "activity": "done", "certainty": "firm",
 "reason": "turn_ended", "title": "use the tool"}
```

The assertable fields are `activity`, `certainty`, `reason`, `permission`, `sandbox`,
`title`, `error`, `cost_usd`, `out_tokens`, `usage` and `recent`. They are optional in a
checkpoint: an input-only checkpoint (only the two required timing fields) is always
accepted. When present, optional fields are compared; a missing optional field and an
explicit `null` are equal. `false` is not `null`.

Helpers are fed before a checkpoint with `helper` (its key under `input/subagents/`) and
optionally `helper_line` (how many lines have been fed so far; default all). `meta` feeds
that helper's `.meta.json`. A checkpoint can therefore check the parent after helper activity
or metadata ended it.

## How a port passes

1. Implement `events <input-dir>` and `status <input-dir> --checkpoints <file>` from
   [`docs/conformance.md`](../docs/conformance.md).
2. Write canonical JSON: object keys sorted at every depth, arrays in order, a missing
   optional field equal to `null`, and all status fields (`activity`, `certainty`, `reason`,
   `permission`, `sandbox`, `title`, `error`, `cost_usd`, `out_tokens`, `usage`, `recent`)
   emitted even when null.
3. For each checkpoint, feed the main transcript up to `after_line`, apply any helper/meta
   steps, compute the state at `now`, and compare the declared fields. Exit non-zero on a
   mismatch; an unreached checkpoint is also a failure.
4. Validate your event and status objects against the schemas above. The committed fixture
   corpus is the reference; `crates/agenttail/tests/schema_fixtures.rs` validates every
   committed `expected/events.jsonl` and `expected/status.jsonl` line against these schemas.

## Time and remote frames

`Timestamp` is `f64` seconds since the Unix epoch in v0.1, matching remote frames. A record
with no time of its own is dated by the transcript's mtime as of the chunk (the library
cannot read the clock). Remote callers must substitute the arrival time before `feed` when a
frame reports `mtime = 0` because the remote `stat` failed; Rust callers can use
`ChunkCtx::mtime_or(arrival)`.
