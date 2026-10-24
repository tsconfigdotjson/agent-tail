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
checked 30 s after its call). Optional `permission` and `sandbox` fields are compared when
present. `crates/agenttail/tests/status_fixtures.rs` runs every scenario.
