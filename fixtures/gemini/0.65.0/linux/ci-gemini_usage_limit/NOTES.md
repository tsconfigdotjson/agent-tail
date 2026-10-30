# Gemini CLI usage-limit stop

Captured from Gemini CLI **0.65.0** against the harness mock returning HTTP 429 with
`You've hit your 5-hour limit · resets 3:50am (UTC)`. The capture sets
`general.maxAttempts: 1` so the terminal failure lands promptly; the normal default is 10.

Gemini does **not** write an error row to the session journal for this failure. The journal
contains the user prompt and then:

```json
{"$rewindTo":"<the just-sent prompt id>"}
```

The error (`RetryableQuotaError`, status 429) only reaches stdout/JSON output and stderr, not
the journal. Before this change the adapter treated `$rewindTo` as metadata, so the session
stayed firm `thinking` / `prompt_sent` forever. A rewind of the just-sent prompt with no model
answer is now firm `errored`. The journal does not record why it rewound, so `stop_reason`
and `resets_at` stay `null`; a normal pre-prompt context rewind remains metadata.
