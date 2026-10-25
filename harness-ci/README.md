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
| `desktop-xvfb/` | Headless Xvfb path for the Claude Desktop stand-in and launch probe. |

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

Do not add a scenario that needs a real account or an external service. If a CLI cannot run
fully offline, document the limitation and test the artifact path with fixtures instead.

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

`.github/workflows/harness-ci.yml` runs:

- unit, lint, format, and Python tool tests;
- the fixture redaction scan;
- the first-read budget;
- pinned and latest CLI scenarios for Claude Code, Codex, Pi, OpenCode, Gemini CLI, and
  GitHub Copilot CLI;
- Cursor CLI installation and offline transcript/status fixtures (its chat path requires
  Cursor's backend, so it is not in the mock/PTY matrix);
- nightly shape-drift checks and fixture capture.

Claude Desktop has a separate offline stand-in and manual launch probe under
[`desktop-xvfb/`](desktop-xvfb/README.md). Its adapter status is documented in
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
