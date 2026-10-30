# Claude Code recurring cron wakeups

Captured offline from **Claude Code 2.1.314** on Linux through `harness-driver` and the
local mock model server. The model loads the deferred `CronCreate` schema through
`ToolSearch`, then schedules a job every minute:

```json
{"type":"tool_use","name":"CronCreate","id":"toolu_…","input":{
  "cron":"* * * * *","prompt":"Check the deploy.","recurring":true}}
```

The result carries the job id and first run time:

```json
{"toolUseResult":{"id":"<id-0032>","humanSchedule":"Every minute","recurring":true,
  "durable":false,"nextFireAt":1793362637970.7637}}
```

`nextFireAt` is epoch milliseconds. The turn ends `done` with the recurring job pending.

## Fire shape in 2.1.314

Each cron run writes `system/scheduled_task_fire` and then its `isMeta:true` scheduled
prompt. This capture's two fire records were:

```json
{"type":"system","subtype":"scheduled_task_fire","timestamp":"2026-10-30T12:17:18.329Z",
 "content":"Running scheduled task (Oct 30 12:17pm)","taskId":"<id-0032>",
 "cron":"* * * * *","prompt":"Check the deploy."}
```

The second was one minute later. Unlike the dynamic-loop fire shape, these cron records
carry no `taskKind`/`cronKind`; `taskId` is the job id. The adapter therefore falls back to
the recorded cron expression, which every fire record carries.

## What the checkpoints assert

The job is pending before the first fire (`due_at` firm from `nextFireAt`). After each fire
it is still pending, but `due_at` is `null` and certainty is `inferred`, because neither the
fire record nor a durable state file gives the scheduler's next run reliably. The final
checkpoint is the same pending job after 901 s of no writes, so `activity` is `idle` rather
than `done` while the wakeup remains armed. A second fire does not duplicate or drop it.

A one-shot (`recurring:false`) cron and a dynamic `ScheduleWakeup` loop are consumed by
their fire; `CronDelete` with the job id, `agents_killed`, and session end still clear a
recurring job. Those paths are covered by unit tests; this capture has no delete because the
job id is random and the session ends with the job still armed. No PID or process registry
is read, and `idle_after` stays 900 s.
