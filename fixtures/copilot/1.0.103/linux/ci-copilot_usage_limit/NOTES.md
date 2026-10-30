# Copilot CLI usage-limit stop

Captured from GitHub Copilot CLI **1.0.103** against the harness mock returning HTTP 429.
After its retries Copilot writes:

```json
{"type":"session.error","data":{"errorType":"rate_limit","statusCode":429,
 "message":"Failed to get response from the AI model; retried 5 times ... Last error: 429
 You've hit your 5-hour limit · resets 3:50am (UTC)"}}
```

A routine `session.shutdown` follows. The adapter already kept the terminal error through
that shutdown and reported firm `errored`. It now also reports
`stop_reason: "usage_limit"`. Copilot records no reset timestamp, so `resets_at` is `null`.
