# Fixtures

`legacy/` is the original seed corpus (see its README).

The harness/version/os/scenario folders above it follow PLAN.md section 7. Each scenario has:

```
input/session.jsonl      one transcript, as the harness wrote it (redacted)
expected/status.jsonl    status checkpoints
meta.toml               version, OS, capture method, date, description
```

A checkpoint is one JSON object per line:

```json
{"after_line": 4, "now_offset_s": 30.0, "activity": "running_tool", "certainty": "firm", "reason": "tool_call_pending", "permission": "never", "sandbox": "danger_full_access"}
```

`after_line` counts entries read after feeding `input/session.jsonl` from the top. `now` for
that check is the checkpoint line's own timestamp plus `now_offset_s` (so a pending tool is
checked 30 s after its call). Optional `permission`, `sandbox` and `title` fields are
compared when present. `crates/agenttail/tests/status_fixtures.rs` runs every scenario.

Helpers live under `input/subagents/<agent>.jsonl` with optional
`input/subagents/<agent>.meta.json`. A checkpoint can feed them, in order, before it checks:

```json
{"after_line": 4, "helper": "agent-a1", "helper_line": 1, "activity": "background_work"}
{"after_line": 4, "meta": "agent-a1", "activity": "done"}
```

`helper_line` is how many lines of that helper have been fed so far (default: all). The
helper id is `subagents/<agent>.jsonl`, matching a parent `outputFile` of the same name.

`fixtures/cursor/handmade/vscdb-reader/` is a `state.vscdb` fixture, not a transcript
scenario: `input/state-vscdb.jsonl` holds synthetic `cursorDiskKV` key/value rows, and
`crates/agenttail/src/sqlite.rs` builds a real SQLite database from it (with the rows left in
the WAL) to test the snapshot reader.
