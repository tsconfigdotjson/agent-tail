# Codex usage-limit stop

Captured from Codex CLI **0.160.1** against the harness mock returning HTTP 429 with
`You've hit your 5-hour limit · resets 3:50am (UTC)`.

Codex does not persist the provider body. After its retry budget it writes:

The task-complete error message is `exceeded retry limit, last status: 429 Too Many
Requests`. The error-info field records that the response failed after too many attempts,
with HTTP status 429.

The reducer already reported this as firm `errored`. It now also reports
`stop_reason: "usage_limit"`. Codex records no reset time on this path, so `resets_at` is
`null`; the provider's local-time text cannot be turned into an instant.
