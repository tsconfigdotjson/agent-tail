# OpenCode usage-limit stop

Captured from OpenCode **1.19.4** against the harness mock returning HTTP 429. The assistant
`message.data` row keeps the provider failure under `error`:

```json
{"name":"APIError","data":{"message":"You've hit your 5-hour limit · resets 3:50am (UTC)",
 "statusCode":429,"responseBody":"...rate_limit_error..."}}
```

The reducer already reported this as firm `errored`, but its error text was the generic
`OpenCode error`. It now keeps the nested provider message and reports
`stop_reason: "usage_limit"`. OpenCode records no reset timestamp on this path, so
`resets_at` is `null`.
