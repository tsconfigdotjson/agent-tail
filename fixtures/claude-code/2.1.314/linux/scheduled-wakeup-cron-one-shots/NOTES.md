# Claude Code one-shot cron wakeups

Captured offline from **Claude Code 2.1.314** on Linux through `harness-driver` and the
local mock model server. The model loads the deferred `CronCreate` schema through
`ToolSearch`, then schedules two one-shot jobs whose minute fields alternate:

```json
{"type":"tool_use","name":"CronCreate","id":"toolu_…","input":{
  "cron":"*/2 * * * *","prompt":"Check the deploy.","recurring":false}}
{"type":"tool_use","name":"CronCreate","id":"toolu_…","input":{
  "cron":"1-59/2 * * * *","prompt":"Run the smoke test.","recurring":false}}
```

Their results identify each job and first run time:

```json
{"toolUseResult":{"id":"333f43e3","humanSchedule":"Every 2 minutes","recurring":false,
  "durable":false,"nextFireAt":1793367120000}}
{"toolUseResult":{"id":"ce65a30a","humanSchedule":"1-59/2 * * * *","recurring":false,
  "durable":false,"nextFireAt":1793367180000}}
```

`nextFireAt` is epoch milliseconds. The first job was due at 13:32 and the second at 13:33;
the turn ended `done` with both pending.

## Fire shape in 2.1.314

Each one-shot writes the same `system/scheduled_task_fire` shape as a recurring job. The
first fire was:

```json
{"type":"system","subtype":"scheduled_task_fire","timestamp":"2026-10-30T13:32:00.686Z",
 "content":"Running scheduled task (Oct 30 1:32pm)","taskId":"333f43e3",
 "cron":"*/2 * * * *","prompt":"Check the deploy."}
```

The second, one minute later, named job `ce65a30a`, `cron":"1-59/2 * * * *"` and prompt
`"Run the smoke test."`. Neither fire carries `taskKind` or `cronKind`; the adapter recognizes
a cron from the expression.

## What the checkpoints assert

Both jobs are pending after the turns that armed them. After the first fire (line 50), only
the second job is pending, with its firm `due_at` still 1793367180.0. The first job's fire
therefore consumed one job, not every `recurring:false` cron. The second job stays pending
through the first fired turn, then its own fire clears it; after that the idle checkpoint has
no pending wakeups. `taskId` names the matching job in this capture, but the reducer matches
on `cron` plus `prompt` because 2.1.314 does not guarantee that id equals the `CronCreate`
result id. No PID or process registry is read, and `idle_after` stays 900 s.
