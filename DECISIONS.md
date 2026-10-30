# Design decisions

This is the public decision log for `agent-tail`. It records the choices that shape the
library, CLI, fixture corpus, and release process. It is intentionally shorter than a design
document: the code, [spec/](spec/), and [docs/](docs/) are the source of truth for behavior.

## Product and release

- **Name: `agent-tail`.** Prose, repository, binary, and documentation use `agent-tail`.
  Rust package names use the same spelling with hyphens: `agent-tail` for the library and
  `agent-tail-cli` for the CLI package. The installed binary is `agent-tail`.
- **Licence: MIT.** The workspace and both packages declare `license = "MIT"`; the repository
  contains the standard [MIT licence](LICENSE).
- **Go public: yes.** The repository is prepared as a public open-source project, with no
  private service integration, customer-specific vocabulary, or internal operations notes in
  public paths.
- **Repository URL:** <https://github.com/tsconfigdotjson/agent-tail>.
- **No crates.io publication yet.** `publish = false` remains set for both packages until a
  separate release decision. Users build or install from the repository.
- **Downstream display mapping is deferred.** This repository exposes machine-readable
  activities, certainty, and reasons. Mapping them to a particular dashboard, TV, label set,
  colour set, or product vocabulary belongs to that consumer and is not part of this project.
- **Environment variable rename is a clean break.** The pre-release environment-variable
  prefix was replaced by `AGENT_TAIL_*` as part of the full rename. There is no compatibility
  alias; the old names were only used by repository tooling.

## Scope

- **Read-only and local.** No network, no subprocesses, no hooks, and no writes to harness
  directories. The only writes are private, read-only database snapshots in the system temp
  folder, removed before a read returns.
- **Transcripts are the source of truth.** `agent-tail` does not ask agents to instrument
  themselves. It reads the files and databases they already write.
- **Status first, events underneath.** The headline API is a reduced `SessionState`. The event
  layer is a general-purpose base, but v0.1 models only what status, title, usage, and recent
  activity need.
- **Support is evidence-based.** A harness is never claimed as supported without a real
  artifact or a real harness run. Evidence levels and versions are generated into
  [docs/support-matrix.md](docs/support-matrix.md).
- **No pricing guesses.** Cost is passed through only where the harness records it. Token
  counts are reported as the harness reports them and are never estimated.

## Architecture

- **Sans-IO core.** `SessionReducer` takes bytes plus provenance (`offset`, file `mtime`,
  path) and never reads the clock or filesystem. Filesystem discovery and tailing live behind
  the optional `fs` feature, on by default.
- **Injected time.** Every status call takes `now`. State is a pure function of the bytes fed
  and that time, which keeps timelines deterministic and testable.
- **Bounded memory.** The reducer keeps a small state per session, never the raw transcript,
  plus the start of an incomplete line. The `Lean` parse mode shortens oversized strings
  before parsing and reports the cut count; `Full` is lossless and is what event dumps use.
- **Layouts are data.** Harness roots, depth, helper folders, ask-user tool names, permission
  vocabulary, timestamp rules, and observed quirks live in `spec/discovery/*.toml` and are
  embedded at build time. A local walk and a remote loop can use the same layout.
- **One event pipeline.** Parsing emits normalized events; the reducer folds those events.
  A status and an event dump therefore cannot disagree. Unknown or malformed records are kept
  as `unknown`, never silently dropped.
- **Helpers are tracked.** Subagent and background transcripts feed `background`, and a
  finished turn with helpers still running is `background_work`, not `done`. A harness's own
  count or metadata can make that firm; a helper transcript alone is inferred.
- **Only the active branch moves Pi status.** Pi session files form a tree. Events from
  abandoned branches are emitted with `off_chain` and ignored by the reducer.
- **Time is `f64` seconds since the Unix epoch** for v0.1. It matches the canonical JSON and
  remote-frame format. Entries without their own timestamp are dated by transcript write time,
  never read time.
- **Database reads use snapshots.** Cursor and OpenCode databases are copied with their
  WAL/shared-memory sidecars, opened read-only, and deleted. They are never opened in place.

## Status model

- **Approval is only possible when the mode allows it.** A pending tool in bypass/never-ask
  mode, an SDK entry point, or an AI-approval reviewer is firm `running_tool`, regardless of
  timing. An unknown mode keeps the stall inference and marks it `inferred`.
- **Firm turn ends beat timing.** Harness-specific records for normal completion, interruption,
  errors, and helper termination move status before the idle-age rule applies.
- **An unread session is `unknown`, not `idle`.** `idle` means the session has recognizable
  state and no writes for the idle window.
- **Coarse classes are not wording.** The library exposes `Activity::class()` (`needs-you`,
  `working`, `done`, `idle`), but labels and display choices belong to the consumer.
- **A pending wakeup is state, not a new activity.** A turn that armed a scheduled wakeup is
  still `done`; `SessionState.pending_wakeups` records the `loop`/`cron` kind, due time,
  optional reason and certainty. It is derived only from transcript tool calls/results and
  clearing records, never from a process registry.
- **Thresholds are configurable.** The approval stall and idle window are options; defaults
  are 20 seconds and 15 minutes, respectively.

## Fixtures and conformance

- **Fixtures are versioned per harness, version, OS, and scenario.** Each scenario has an
  `input/`, expected events and statuses, `meta.toml`, and optionally a redaction report.
- **Redaction is deterministic and structure-preserving.** The same secret gets the same
  placeholder. Reports name placeholders and categories, never original secrets or home paths.
- **Fixtures are never captured unredacted.** The pre-commit hook and CI scan committed
  fixtures for secrets and real home paths.
- **The conformance contract is language-neutral.** `events` and `status` use canonical JSON
  and the schemas in [spec/schema/](spec/schema/), so a port does not need Rust or access to
  library internals.
- **Shape drift is visible.** `agent-tail survey --drift` compares observed transcripts with
  the committed baselines and fails on new record types, discriminators, content types, or
  tool names.
