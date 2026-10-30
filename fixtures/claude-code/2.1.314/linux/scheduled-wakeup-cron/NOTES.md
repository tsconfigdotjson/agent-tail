# Claude Code cron wakeups

Captured offline from **Claude Code 2.1.314** on Linux through `harness-driver` and the
local mock model server. The model loads the deferred `CronCreate` schema through
`ToolSearch`, then calls:

```json
{"type":"tool_use","name":"CronCreate","id":"toolu_…","input":{
  "cron":"*/5 * * * *","prompt":"Check the deploy.","recurring":true}}
```

The result is a normal `tool_result` with the human schedule text, plus structured data:

```json
{"toolUseResult":{"id":"083784d8","humanSchedule":"Every 5 minutes","recurring":true,
  "durable":false,"nextFireAt":1793324620207}}
```

`nextFireAt` is the firm due time in epoch milliseconds. The turn ends `done` with the cron
still pending. The `durable` field is false here, so the job is session-only; 2.1.314 can
persist a durable job to `.claude/scheduled_tasks.json`. See the sibling dynamic-wakeup fixture's NOTES for `CronDelete`, `scheduled_task_fire`,
resume behaviour and the shared clearing rules.
