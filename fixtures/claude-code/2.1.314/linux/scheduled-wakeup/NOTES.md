# Claude Code scheduled wakeups

Captured offline from **Claude Code 2.1.314** on Linux through `harness-driver` and the
local mock model server. The transcript is redacted but keeps record order, field order,
tool-use/tool-result linkage, `scheduledFor`, `nextFireAt` and the fire record.

## `/loop` dynamic mode: `ScheduleWakeup`

The tool is not deferred; the model calls it directly.

```json
{"type":"tool_use","name":"ScheduleWakeup","id":"toolu_…","input":{
  "delaySeconds":60,"noop":false,"prompt":"Check the deploy.","reason":"Watching the deploy."}}
```

Its result is a normal user `tool_result` plus a structured `toolUseResult`:

```json
{"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_…",
  "content":"Next wakeup scheduled for 01:42:00 (in 97s). …"}]},
 "toolUseResult":{"scheduledFor":1793324520000,"clampedDelaySeconds":60,"wasClamped":false}}
```

`scheduledFor` is **epoch milliseconds** and is the firm due time; the requested
`delaySeconds` is only the inferred value before the result. The runtime clamps requests to
60–3600 s and rounds the target to the next minute, so the result can be later than the
request (this capture asked for 60 s and was due 97 s later).

A stop call is `{"stop":true}`. Its result has
`{"scheduledFor":0,"clampedDelaySeconds":0,"wasClamped":false,"stopped":true,
"cancelledWakeups":1}` and cancels dynamic-loop wakeups only; a fixed-interval cron is not
cancelled.

## Fixed `/loop 5m` and cron jobs: `CronCreate` / `CronDelete` / `CronList`

The cron tools are deferred. The model first calls `ToolSearch` with
`{"query":"select:CronCreate","max_results":1}`; the result lists a `tool_reference`.
`CronCreate` then takes:

```json
{"type":"tool_use","name":"CronCreate","id":"toolu_…","input":{
  "cron":"*/5 * * * *","prompt":"Check the deploy.","recurring":true}}
```

The captured result is:

```json
{"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_…",
  "content":"Scheduled recurring job 083784d8 (Every 5 minutes). …"}]},
 "toolUseResult":{"id":"083784d8","humanSchedule":"Every 5 minutes","recurring":true,
  "durable":false,"nextFireAt":1793324620207}}
```

`nextFireAt` is epoch milliseconds. 2.1.314 documents `durable:true` writing
`.claude/scheduled_tasks.json`; this job used the default `durable:false`, so it is
session-only and dies with the process. The fixed-interval `/loop 5m …` skill uses this cron
path, not `ScheduleWakeup` (from the skill and the 2.1.314 binary; the captured fixture is a
direct cron call).

`CronDelete` takes `{"id":"083784d8"}`. Its success result is a `tool_result` with content
`Cancelled job 083784d8.` and `toolUseResult:{"id":"083784d8"}`. The id is random per run, so
this fixture does not capture a successful delete; the reducer has a unit test using those
exact result fields. `CronList` returns the jobs and their next run times; it does not create
or clear state by itself.

## What is written when a wakeup fires

Dynamic loop fire, before the resumed prompt:

```json
{"type":"system","subtype":"scheduled_task_fire","timestamp":"2026-10-30T01:42:00.446Z",
 "content":"Claude resuming /loop wakeup (Oct 30 1:42am)","taskId":"…","cron":"42 1 * * *",
 "prompt":"Check the deploy.","taskKind":"loop","cronKind":"loop"}
```

The following user row is `isMeta:true` with `promptSource:"system"`,
`wakeupSource:"loop_wakeup"`, `turnOrigin:"scheduled"`, `scheduledFireId` and
`scheduledTaskId`. It is not a user prompt and does not start a normal turn in the reducer.
The `scheduled_task_fire` record is the clearing evidence.

## Resume and restart

Both the tool call and its result are ordinary transcript rows, so the reducer can derive a
pending wakeup after a restart or `--resume` with no process state. The 2.1.314 binary also
re-arms a still-future pending dynamic wakeup when a session resumes (`re-armed pending
wakeup … after resume`); session-only cron jobs are not re-armed. Durable cron jobs are the
documented exception: 2.1.314 persists them to `.claude/scheduled_tasks.json`, and a missed
one-shot is surfaced for catch-up.

The reducer keeps the `pending_wakeups` entry after an ordinary turn end. It clears it when
the fire record arrives, when `ScheduleWakeup({stop:true})` / a user abort cancels the dynamic
loop, when `CronDelete` names the job, or when the transcript records `agents_killed` at
session exit. It does not use a PID or process registry and does not change `idle_after`.
