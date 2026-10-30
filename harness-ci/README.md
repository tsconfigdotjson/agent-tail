# Harness CI

Real coding-agent CLIs, a mock model server, and `agent-tail`'s status checked against what
the CLI actually shows. This is the integration-test rig; the main library tests do not
depend on it.

There are four pieces:

| Directory | Purpose |
|---|---|
| `mock-server/` | Local Anthropic Messages, OpenAI-compatible, and Gemini protocol mock. Scenario files control replies, tool calls, delays, and errors. |
| `driver/` | Starts a CLI under a PTY, parses its screen, tails the session artifact it writes, and scores `agent-tail`'s state against the screen. |
| `ci/` | Scenario lists, pinned/latest matrix generation, offline runner, shape survey, drift checks, and the first-read budget. |
| `desktop-xvfb/` | Headless Xvfb path for the Desktop stand-in, live Jev lane, and launch probe. |

This directory is a separate Cargo workspace so the library's dependencies stay small. The
root `cargo test` does not build it; CI does.

## Run it locally

From the repository root:

```sh
cargo test --all-features

cd harness-ci
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p harness-driver -p mock-server --locked

python3 -m unittest discover -s ci -p 'test_*.py' -v
python3 -m unittest discover -s desktop-xvfb -p 'test_*.py' -v
```

A single scenario needs a built driver and a CLI binary:

```sh
cd harness-ci
cargo build -p harness-driver --release --locked

# The offline runner enforces the no-network policy around the command.
ci/offline-run.sh \
  "$PWD/target/release/harness-driver" \
  --scenario "$PWD/driver/scenarios/claude_plain_qa.json" \
  --out /tmp/claude-plain-qa
```

The output directory contains the screen/timeline evidence, the transcript snapshot, and a
score file.

## Offline by default

No live model, API key, or network access is needed. The driver supplies dummy credentials
that only the local mock sees, points the CLI at `localhost`, and uses a temporary `HOME` and
config directory. CI additionally sets `HARNESS_CI_REQUIRE_OFFLINE=1` and enforces isolation
with a network namespace on Linux or `sandbox-exec` on macOS.

The one deliberate exception is the live Jev Desktop lane under
[`desktop-xvfb/`](desktop-xvfb/README.md). It runs on the nightly schedule only when the
repository `OPENROUTER_API_KEY` Actions secret is present, is skipped cleanly when absent, and
is never part of the push/PR matrix. It uses Jev to choose UI actions and scores the Desktop
audit log on disk; there is no writer LLM in that path. Do not add another scenario that needs
a real account or an external service. If a CLI cannot run fully offline, document the
limitation and test the artifact path with fixtures instead.

## Scenarios and matrix

Each driver/mock pair uses the same scenario name. For example:

```text
driver/scenarios/claude_bash_approval.json
mock-server/scenarios/bash_approval.json
```

The scenario lists under `ci/` define the matrix for each harness:

- `scenarios.txt` for Claude Code
- `scenarios-codex.txt`
- `scenarios-pi.txt`
- `scenarios-opencode.txt`
- `scenarios-gemini.txt`
- `scenarios-copilot.txt`

The pinned versions are the ones the committed fixtures were captured against. The nightly
matrix also resolves and runs npm `latest` for each harness, so a release that changes the
transcript shape fails loudly.

Recent coverage additions include a hard-kill mid-tool scenario for every CLI in the matrix
(Claude Code, Codex, Copilot CLI, Pi, OpenCode and Gemini CLI), Copilot parallel tool calls and
a terminal `session.error`, Pi parallel tool calls plus failing-tool recovery, Pi
compaction/unknown-entry fixtures, OpenCode error-step-finish, parallel-part and child-session
fixtures, and a Gemini multi-tool turn captured from a real 0.65.0 journal. Every CLI also has
a usage-limit scenario from the mock's HTTP 429 response; Gemini's journal only rewinds the
prompt, so its scenario pins firm `errored` without an inferred cause. The hard-kill
scenario kills the process group without Esc/Ctrl-C/exit and pins the rows the harness
actually flushed: `running_tool` for five harnesses, and the pre-tool
`thinking`/`prompt_sent` journal for Gemini 0.65.0, which writes tool calls only after
completion. The Claude Code stable list now includes ask-user, plan exit, mid-tool interrupt,
hard-kill mid-tool, accept-edits and a direct background-agent turn. The background driver uses one direct helper because pinned 2.1.281
also counted a nested background helper in the parent's pending count, while 2.1.310 counted
only the launched helper; the single-helper turn is stable on both. The fixtures also pin
Copilot `abort`/`session.error` followed by a routine `session.shutdown` (which must not turn
those terminal states back into `done`). The nightly Claude Code shape baseline was refreshed
from the expanded stable list, so the new scenarios are approved shape rather than first-night
drift. The adapter-by-adapter table, including deferred cases, is
[`docs/harness-coverage.md`](../docs/harness-coverage.md).

The SIGSTOP/SIGCONT wake scenarios stop the CLI's whole process group at the requested state,
keep sampling while it is stopped, then continue it and record the next turn. `freeze` in a
driver scenario names the screen/activity to wait for, the stopped duration, optional
`drop_connections` and `advance_now_ms` for a VM-restore stand-in, and optional
`resume_send`/`resume_delay_ms` bytes for the live composer. The long-gap variants drop the
mock's open streams and move the reducer's clock (and writes after the wake) forward an hour;
the fixtures shift post-wake record times by the same hour so the disk capture models the
clock jump without waiting. Claude Code and Codex cover done, awaiting answer and awaiting
approval; Copilot covers done and a real TUI approval; OpenCode covers done and the question
tool; Pi and Gemini cover done. Unreachable states are recorded in each fixture's `NOTES.md`
rather than invented. See `fixtures/*/*/linux/ci-*_sigstop_wake_*/NOTES.md`.

Two Codex scenarios avoid host-dependent false failures. `codex_token_count` sends its second
prompt on the first idle after a delay because a fast turn can finish without leaving the TUI
idle screen long enough to count a second idle occurrence. `codex_approval_never` runs with
`danger-full-access` so a nested CI host cannot fail Codex's bubblewrap sandbox before the
long command starts; the policy under test is still `approval_policy = never`.

`.github/workflows/harness-ci.yml` runs:

- unit, lint, format, and Python tool tests;
- the fixture redaction scan;
- the first-read budget;
- pinned and latest CLI scenarios for Claude Code, Codex, Pi, OpenCode, Gemini CLI, and
  GitHub Copilot CLI;
- Cursor CLI installation and offline transcript/status fixtures (its chat path requires
  Cursor's backend, so it is not in the mock/PTY matrix); the manual macOS/Linux
  [`cursor/`](cursor/README.md) snapshot tool captures a live `state.vscdb` approval wait
  without opening the live database, and [`cursor/mac-smoke.md`](cursor/mac-smoke.md) is the
  periodic already-signed-in Mac confirmation path;
- nightly shape-drift checks and fixture capture.

Claude Desktop has a separate offline stand-in, a live Jev lane, and a manual launch probe
under [`desktop-xvfb/`](desktop-xvfb/README.md). The stand-in runs every night. The live lane
also runs on the `43 3 * * *` schedule when the `OPENROUTER_API_KEY` Actions secret is set,
and can be dispatched manually; it is absent from the PR matrix. It uses `typesafe/jev-1.14`
through OpenRouter to choose UI actions and scores the on-disk Desktop audit log. Secret setup
is documented in the [desktop-xvfb README](desktop-xvfb/README.md#configure-the-live-jev-secret).
Its adapter status is documented in
[`docs/formats/claude-desktop.md`](../docs/formats/claude-desktop.md).

## What the driver checks

The driver reads the CLI screen through a vt100 parser and polls the session artifact on disk.
It fails when the firm activity or reason differs, when a needed event is missing, or when a
scenario does not reach its expected end state. Timing- or shape-inferred states are allowed
only where the scenario declares them.

This catches the bugs that unit fixtures cannot:

- permission mode changing whether a pending tool is `running_tool` or `awaiting_approval`;
- a long tool call stalling into the wrong state;
- asks-user, interrupts, aborts, API errors, and turn ends;
- background helpers keeping a finished turn in `background_work`;
- transcript ordering differences caused by fast model responses;
- a CLI release changing record types or field paths.

## First-read budget

The budget gate builds the `first_read` benchmark and runs the executable under GNU
`/usr/bin/time`:

```sh
PATH="$HOME/.cargo/bin:$PATH" harness-ci/ci/first-read-budget.sh
```

It reads a synthetic 500 MB Claude Code transcript and fails when the `Lean` first read takes
3.0 seconds or more, or reaches 50 MB peak RSS. `Full` is reported for comparison but is not
budgeted. No machine-specific measurement is claimed here; the current numbers are recorded
by CI logs.

## Shape drift and fixture capture

`harness-ci/ci/survey.py` records structure only: record types, field paths, content-block
types, discriminator values, and tool names. It never records ids, timestamps, prose, or
secrets.

The nightly job compares the newest observations with `harness-ci/drift/baseline/`. New shape
opens a pull request with a report under `harness-ci/drift/reports/`. Captured sessions are
redacted and scanned with the same offline tool people use locally:

```sh
target/release/agent-tail fixture scan fixtures
```

No repository secret is used for capture. Set `AGENT_TAIL_BIN` to the built CLI when running
the nightly capture helper locally.

## CI artifacts and drift branches

Artifacts are evidence for a failed run, not a build archive, with two deliberate exceptions.
`build-driver` compiles the native `harness-driver` once per OS and uploads a short-lived
`harness-driver-*` input (1-day retention) for the matrix lanes, which no longer install Rust
or compile it themselves. The artifact is OS-specific because the driver is a native
executable. The nightly Linux Claude Code lane uploads a slim `harness-drift-*` artifact
containing only the shape, transcripts, subagents, and scores that the drift job reads. It does
not upload screens, timelines, or mock request logs, and artifact names stay version/OS-scoped
rather than adding a per-run identifier. Every other harness lane uploads its run directory
only when the job fails, and Desktop uploads its `runs/` evidence only on failure; those
evidence artifacts keep a 3-day retention.

The drift PR uses one stable scratch branch, `harness-drift/claude-code`. Each nightly run
force-pushes that branch and updates the existing open PR when there is one, so repeated
drift cannot leave a `harness-drift/*` branch per run behind. The branch is safe to delete
after merge; the next drift recreates it. Do not add `$GITHUB_RUN_ID` or another per-run
suffix to the branch name.

## Adding a harness or scenario

1. Add the mock scenario and driver scenario.
2. Add the scenario name to the matching `ci/scenarios-*.txt` list.
3. Add a CI job or extend the matrix for the harness, using a temp home and an offline mock
   endpoint.
4. Capture or hand-make a fixture, add status checkpoints, and update the support matrix.
5. Update `spec/discovery/<harness>.toml` and the format note under `docs/formats/`.
6. Run the unit tests and the offline runner before opening a pull request.

A harness is not CI-verified until its scenario runs in the matrix and the support matrix
says so.
