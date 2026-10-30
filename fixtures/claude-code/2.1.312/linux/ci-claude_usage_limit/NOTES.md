# Claude Code usage-limit stop

Source: a real Claude Code **2.1.312** transcript on this machine (redacted by
`agent-tail fixture capture`/`fixture redact`). A separate 2.1.314 run against the mock's
HTTP 429 usage-limit error produced the same synthetic assistant row, but without the
structured quota fields, so this fixture keeps the richer real 2.1.312 row.

The transcript records a synthetic assistant row with:

- `isApiErrorMessage: true`, `error: "rate_limit"`, `apiStatus: 429`;
- `apiError: "usage_limit_reached"`;
- `apiErrorParams` → `rate_limit_info` → `resetsAt`, and `quotaLimits` → `resetsAt`;
- a text message: `You've hit your 5-hour limit · resets 3:50am (UTC)`.

The reducer already reported this as firm `errored`. It now also reports
`stop_reason: "usage_limit"` and `resets_at: 1793159400`, and keeps both through the
following `system/informational` and `system/turn_duration` rows. The next user prompt
clears them (unit coverage in `crates/agent-tail/tests/usage_limit.rs`).
