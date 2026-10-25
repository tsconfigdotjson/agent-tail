# Conformance runner

The fixture format is language-neutral. A port in another language implements two commands
over the same `input/` directory and compares canonical JSON, without linking Rust or
understanding `agent-tail`'s internals.

## Commands

```
<impl> events <input-dir>
<impl> status <input-dir> --checkpoints <file>
```

`<input-dir>` is a fixture scenario's `input/` directory (or the scenario directory itself,
which contains `input/`). It holds `session.jsonl` as the main transcript and may hold
`subagents/<agent>.jsonl` plus `subagents/<agent>.meta.json` for helpers that the harness
writes as separate files. GitHub Copilot CLI instead interleaves helper records in
`session.jsonl` with a top-level `agentId`; those are read as part of the main transcript.
A database-only Cursor fixture instead holds `input/state-vscdb.jsonl` and no transcript;
`status` loads the rows, applies the composer status, and checks at `after_line: 0`.

The language-neutral contract is frozen in [`spec/schema/`](../spec/schema/):
`event.schema.json` for events, `session-state.schema.json` for the full state object, and
`status.schema.json` for a checkpoint line. [`spec/status.md`](../spec/status.md) explains
the status fields, checkpoint protocol, canonical output and how a port passes. The
committed fixtures' expected events and statuses are validated against these schemas by
`crates/agent-tail/tests/schema_fixtures.rs`.

- **events** writes one normalized event per line. It reads the main transcript only;
  helper transcripts are reducer inputs, not part of `expected/events.jsonl`.
- **status** replays the main transcript and each checkpoint's helper/meta steps, then writes
  one status object per checkpoint, in checkpoint order. It exits 1 when a checkpoint's
  declared expected fields do not match. For a database-only Cursor fixture it applies the
  composer instead of replaying a transcript.

`agent-tail events` also accepts a single transcript path for compatibility. `--agent
claude_code|codex|cursor|pi` overrides harness detection.

## Canonical JSON

- Object keys are sorted at every depth; arrays keep their order.
- A missing optional field and an explicit `null` are equal. `false` is not `null`.
- Output numbers use the implementation's normal JSON number spelling; `-0.0` and `0.0` are
  equal for comparison.
- Ids, timestamps and free text still appear in `events` output because they are part of the
  event contract. They are checked as values, not as shape.

`status` always emits these fields (with `null` when absent):

```json
{"activity":"done","after_line":8,"background":[],"certainty":"firm","cost_usd":0.0,
 "error":null,"now_offset_s":30.0,"out_tokens":5,"permission":null,"reason":"turn_ended",
 "recent":[{"kind":"prompt","text":"use the tool"}],"sandbox":null,
 "title":"use the tool","usage":{"cache_creation_input_tokens":0,
 "cache_read_input_tokens":0,"input_tokens":10,"output_tokens":5}}
```

A checkpoint file such as `expected/status.jsonl` is both the timing script and the expected
result. A checkpoint may declare only `after_line` and `now_offset_s`; then it is input-only
and always accepted. Optional `permission`, `sandbox`, `title`, `error`, `cost_usd`,
`out_tokens`, `usage`, `recent` and `background` fields are compared when present.

## Shape survey

`agent-tail survey [--drift] [PATH…]` records structure only: record types, field paths,
content-block types, discriminator values and tool names. It never records ids, timestamps or
free text.

- With no paths it surveys every harness root below `HOME`, honouring the discovery spec's
  environment overrides.
- With paths it surveys those transcripts, fixture inputs, or scenario directories.
- `--out FILE` writes the shape as canonical JSON.
- `--drift --baseline spec/baseline` compares against `spec/baseline/<harness>.json` and
  exits non-zero on new shape. New record types, discriminators, content types and tool names
  fail by default.
- `--strict-fields` also fails on new nested field paths. Field-path drift is conservative by
  default because the committed baselines are curated (for example `message.usage` without
  every member); a local survey is not a complete corpus.
- `--strict-removals` also fails when a baseline observation was not seen locally.

Cursor's `state.vscdb` section in the committed baseline remains Phase 0 evidence; the
transcript survey does not query a live database. Its fixture (`state-vscdb.jsonl`) is not
treated as a transcript. OpenCode stores sessions in `opencode.db`; its SQLite row export is
not surveyed yet either, and there is no committed OpenCode baseline.

## Support matrix

`agent-tail support-matrix [--root DIR] [--out FILE] [--check]` regenerates
[`docs/support-matrix.md`](support-matrix.md) from `spec/discovery/*.toml`, fixture
`meta.toml` files and the `harness-ci/ci/scenarios*.txt` lists. `--check` fails when the
committed file is stale.
