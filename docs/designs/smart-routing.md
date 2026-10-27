# Smart routing and managed runs for cx

Status: approved scope on 2026-10-27. Implementation plan prepared; no runtime changes yet.

## Outcome

Spread new Codex sessions across eligible accounts without repeatedly changing
the account during work. Support automatic resume, explicit pins, targeted
switching, and inspectable session counts. Extend the same account-selection
policy to the headless QA workflow described in LAM 2m9zf.

## User-facing behavior

```sh
cx run -- -m <model>
cx run -- resume <thread-id>
cx run --account work -- exec -m <model> -c model_reasoning_effort=high "prompt"
cx run --label MON-9839-r9 --usage-file ./run.json -- exec --json "prompt"
cx sessions
cx switch work --session <thread-id>
cx switch work --from personal
cx switch work --session <thread-id> --interrupt
cx usage --live
cx --llm
```

Keep bare `cx` and `cx -- <args>` as interactive shortcuts, using the same new
automatic selection as `cx run`. Installing the update does not move existing
processes. Preserve plain `cx switch` behavior and explain its broadcast scope;
targeted commands do not change shared login or unrelated sessions.

## Routing and concurrency

- Refresh unavailable/stale usage before selection. Check every reported window,
  account-level eligibility, and credential usability. Never assume a reset
  occurred solely because the cached reset timestamp passed.
- Rank eligible accounts using both quota windows, time until reset, and live
  session load. Port the tested cs routing policy where applicable; use reported
  window durations rather than assuming every Codex account exposes 5h and 7d.
- Reserve the account atomically before starting a process. Concurrent launches
  must see pending reservations. Remove dead reservations using process identity,
  not PID alone. Add a configurable per-account headless concurrency cap.
- Store affinity by conversation and account identity, with last-prompt time.
  Recent affinity is a preference, never an exemption from exhausted quota.
  Make its duration configurable; do not claim Claude's cache TTL applies to Codex.
- An explicit account alias or unique email pins the run. Fail clearly rather
  than selecting a different identity. Pins still appear in session counts.

## Interactive sessions and targeted switching

Extend the existing private control socket and app-server controller. Record
thread ID, process identity, alias, label, start time, and pending move/error.
`cx sessions` exposes IDs from another terminal without restarting conversations.
Show counts next to account names in `cx usage`, including zero.

By default, a targeted move waits for tracked turns to complete and then logs
the selected account into that session's app-server. Preserve the conversation,
model and reasoning settings. An explicit interrupt uses Codex turn interruption
and waits for completion before changing identity. Validate the entire selection
before queueing a batch; report runtime failures per session. Older controllers
must reject unsupported targeted requests clearly rather than accept a broadcast.

## Headless QA boundary

Two implementation options need a compatibility check before committing to one:

1. Wrap native `codex exec`: preserves its flags/output, but its public JSONL
   schema does not provide complete model-routing or structured error evidence.
2. Build a headless app-server client: reuses cx authentication and exposes model,
   token, turn-error, and reroute notifications, but requires explicit supported
   exec-argument/output compatibility. Never silently drop unsupported flags.

Recommend keeping the native interactive controller and sharing selection,
reservations and accounting between both frontends. Select the headless frontend
after verifying the installed Codex protocol. Do not claim that the requested
model proves which model served a request. Record requested/resolved model and
observed reroutes separately; fail on an observed mismatch. Missing evidence
must be represented explicitly, not reported as verified.

The headless interface must preserve stdin, JSON stdout and output-file behavior.
cx routing messages go only to stderr. Usage records include account, label,
session ID, requested/resolved model and effort, observed token usage, timestamps,
native exit status and classified outcome. Write completed records atomically;
retain partial/failed outcomes. Distinguish quota, authentication, model mismatch,
task failure and unclassified errors. Do not classify arbitrary task failures as
quota or automatically replay work on that basis.

## Separate follow-up work

Automatic mid-run quota recovery and the restricted QA profile from the LAM are
separate milestones unless explicitly included in the first delivery. Recovery
needs bounded attempts and verified same-thread resume without replaying tool
actions. The QA profile needs enforceable filesystem and network isolation;
workspace-write alone is not evidence of read restrictions.

## Verification and delivery

Test quota/reset-aware selection, exhausted resume, pins, concurrent reservations,
cleanup, targeted isolation, busy/interrupt behavior, refresh-token ownership,
model/effort preservation, clean stdout, records and error classification.
Use temporary state and fake protocol fixtures, then isolated real Codex checks
on Linux/Mac without moving user sessions. Run formatting, lint, tests and review
before syncing. No version bump or release tag requested.

## Open decision

Approved first delivery: interactive parity plus headless QA and records.
Automatic mid-run recovery and the restricted QA profile remain follow-up work.
