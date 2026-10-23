# Legacy fixtures

The seed corpus, copied unchanged from StatusWall's engine (`tvdash/src/sessions/fixtures/`),
where they were already scrubbed: prompts, tool inputs and outputs replaced, paths made
placeholders (`/home/dev/code/demo-app`, `/Users/sam/Work/demo-app`), structure and
bookkeeping entries kept. The crate's adapter tests and the chunking property tests read
them.

| file | harness | capture | what it is |
|---|---|---|---|
| `claude-code-2.1.jsonl` | Claude Code 2.1 | donated | a real session: a prompt, Bash / Read / Write calls, a generated title, a cost entry |
| `codex-0.160-exec.jsonl` | Codex CLI 0.160.1 | donated | a real `codex exec` rollout, run against a stand-in model, long instructions trimmed |
| `codex-0.160-resume.jsonl` | Codex CLI 0.160.1 | donated | the same session after `codex exec resume`: a patch, a command, a `request_user_input` |
| `cursor-synthetic.jsonl` | Cursor agent | handmade | written by hand in the shape seen on a real Mac (October 2026); none of anyone's content |

They predate the fixture layout in PLAN.md section 7 (`fixtures/<harness>/<version>/<os>/<scenario>/`
with `meta.toml` and expected checkpoints). They'll move into it once `fixture redact` and the
status checkpoints exist; until then they stay here, byte for byte as tvdash has them.
