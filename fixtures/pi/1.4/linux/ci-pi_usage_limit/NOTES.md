# Pi usage-limit stop

Captured from Pi **1.4.0** against the harness mock returning HTTP 429 with a
`rate_limit_error` body. Pi writes one assistant message with `stopReason: "error"` and:

```text
429 {"error":{"message":"You've hit your 5-hour limit · resets 3:50am (UTC)",
"type":"rate_limit_error"},"type":"error"}
```

The reducer already reported this as firm `errored`. It now also reports
`stop_reason: "usage_limit"` from the error text. No reset time is derivable from a local
clock time without a date, so `resets_at` is `null`.
