# rusty_rtos_kernel — package plan

**One sentence:** FreeRTOS-Kernel remade in Rust: the fixed-priority preemptive scheduler, task notifications, queues, semaphores, mutexes with priority inheritance, queue sets, software timers, event groups, stream and message buffers — a pure state machine over the Port seam, forbid(unsafe), traced against the C kernel.

Family plan: Kairos `docs/plans/rtos-mission.md` (umbrella repo) — its §2.1
names what this package remakes, wraps and never touches; its §6 carries the
phase this package's kill test belongs to. This file obeys that one.

Written 2026-09-09. Status: **K1 — the scheduler agrees with the C kernel on
the first scenario.** `dynamic` produces a trace identical to the C kernel's
for 100,000 ticks, counters included. Eight scenarios of the corpus remain,
and IPC beyond the queue is K2's.

---

## 1. What it is, what it is not

**Is:** `tasks.c` and `queue.c` as a state machine over indices. The ready
lists, the two delayed lists, the pending-ready and suspended lists, the
tick, the context-switch *decision*, and the queue with its two event
lists. It owns no CPU (that is `Port`), no memory (that is `Heap`), no
stack, and no allocator: every object lives in a `const`-sized arena from
`rusty_rtos_core`.

**Is not:** a context switch. A switch is a stack swap and a stack swap is
`unsafe`, which the family allows only in `rusty_rtos_port-<arch>`. This
crate decides *which task should run* and fires the same
`TASK_SWITCHED_OUT` / `_IN` pair the C kernel fires; on silicon the port
acts on that with its fenced assembly, and on the sim the runner acts on it
by stepping the next task's body.

## 2. The laws this package encodes

1. **`forbid(unsafe)`, and it is not a slogan.** There is no pointer in a
   TCB, no stack, no allocator. A stale handle is `Error::Gone`; the C
   kernel's use-after-delete is not representable.
2. **The C control flow, statement for statement** — including where the
   critical sections open and close. On the sim an outermost
   critical-section exit *is* the clock (`ORACLES.md`, sim contract v1, rule
   3), so a section moved by one line moves every tick after it. The
   subtlety that costs a diff: FreeRTOS yields from *inside* the section in
   `vTaskResume`, `vTaskPrioritySet`, `xTaskResumeAll` and the queue calls,
   and from *outside* it in `vTaskSuspend` and `vTaskDelay`.
3. **Where the C asserts, this returns.** `configASSERT` stops the world;
   an `Error` does not. Every public function documents which errors it can
   return and why.
4. **Geometry is const, not heap.** `Kernel<C, P, T, TASKS, ITEMS, LISTS,
   QUEUES, SLOTS>`; `Kernel::new` refuses a geometry that does not add up
   rather than indexing out of range later. `items_for` and `lists_for`
   compute the derived numbers.
5. **The trace is the gate.** Every claim in §3 is backed by
   `kairos conform`, which compares this kernel's trace with the C kernel's
   line for line, counters included. No claim is made from reading.

## 3. The surface as built (2026-09-09)

| C function | here | state |
|---|---|---|
| `xTaskCreate` | `create_task` | conformant |
| `vTaskStartScheduler` | `start_scheduler` (creates IDLE, the timer queue and `Tmr Svc`) | conformant |
| `vTaskSwitchContext` | `switch_context` | conformant |
| `xTaskIncrementTick` | `increment_tick`, `tick_from_isr` | conformant, pended-tick replay included |
| `vTaskDelay` | `delay` | conformant |
| `vTaskSuspend` / `vTaskResume` | `suspend` / `resume` | conformant |
| `vTaskPrioritySet` / `uxTaskPriorityGet` | `set_priority` / `task_priority_get` | conformant |
| `eTaskGetState` | `task_state_get` / `state_of` | conformant |
| `vTaskSuspendAll` / `xTaskResumeAll` | `suspend_all` / `resume_all` | conformant |
| `xQueueCreate` / `xQueueSend` / `xQueueReceive` | `queue_create` / `queue_send` / `queue_receive` | conformant for a zero block time, which is all the corpus uses so far; a blocking send or receive returns `Error::Unsupported` rather than pretending |
| `vQueueWaitForMessageRestricted` | `wait_for_message_restricted` | conformant (the timer task's indefinite block) |
| `xTaskRemoveFromEventList` | `remove_from_event_list` | conformant |
| `vTaskDelete`, notifications, mutexes, timers, event groups, stream buffers | — | K2 |

**What a stackless kernel has to say out loud.** Three mechanisms exist
here that a C port gets for free from having stacks, each found by a trace
diff and each named in the code:

- `started` / the first switch-in resets the nesting count, because a real
  port hands a new task a fresh stack (`prvWaitForStart`).
- `owed_exits` / nesting is per task, saved across a switch and unwound when
  the task runs again (`prvSwitchThread`'s `uxSavedCriticalNesting`), so the
  exits of an abandoned frame are deferred rather than lost or taken early.
- `owes_yield` / a call preempted before its trailing `portYIELD()` owes
  that yield until the task runs again — `vTaskDelay` and `vTaskSuspend`
  both have one.

`resume_pending` pays both debts, in that order, before the task's next
statement.

## 4. Roadmap

| Milestone | Adds | Driven by | Kill test |
|---|---|---|---|
| **K1a** (done 2026-09-09) | tasks, the tick, the switch, the zero-block queue | K1 | `dynamic` trace-identical for 100,000 ticks |
| K1b | whatever the remaining eight scenarios need — blocking queue sends and receives, `vTaskDelayUntil`, `vTaskDelete` | K1 | all nine trace-identical |
| K2 | notifications, mutexes with priority inheritance, counting semaphores, queue sets, software timers, event groups, stream and message buffers | K2 | the K2 corpus trace-identical; Kani harnesses for the CBMC proof list |
| K3 | whatever a real port needs from the switch decision (a `Port` that swaps stacks rather than a runner that steps bodies) | K3 | the corpus on QEMU and on a C6 |

## 5. Deliberately absent

- **Co-routines.** Deprecated upstream; mission plan §2.1 says never.
- **`configUSE_TRACE_FACILITY` tables and run-time stats.** Tracing is the
  `Trace` seam; stats are K6.
- **A heap.** `Heap` is a seam; the kernel's own objects are arenas.
- **Blocking queue operations**, for now: `Error::Unsupported` is honest
  where a silent busy-wait would not be.

## 6. Risks

| Risk | Mitigation |
|---|---|
| The remaining eight scenarios each expose a new C corner and the diff turns into a long grind | each divergence is localised by `kairos conform --exits` in one run; the three mechanisms in §3 were the structural ones, and the rest are expected to be single functions |
| The state-machine model diverges from a real stack-switching port at K3, and the corpus has to be re-proved | the kernel's decisions are the same either way; K3's kill test runs the same corpus on QEMU, which is what would catch it |
| A blocking queue path is written to satisfy a scenario rather than to match C | every path lands with its scenario's trace diff, never before |

## 7. Decision log

| Date | Decision |
|---|---|
| 2026-09-09 | Stamped from the Kairos template; obeys the family plan. |
| 2026-09-09 | The kernel decides the switch and never performs it: `forbid(unsafe)` holds, and a port or a runner acts on the decision. |
| 2026-09-09 | Critical nesting is per task, saved across a switch, its exits deferred; the tick handler's bump is not a critical section. Found by trace diff, not by reading — the events agreed for 1,598 lines after the accounting had drifted. |
| 2026-09-09 | A blocking queue send or receive returns `Error::Unsupported` until a scenario needs it, rather than shipping an untested path that the corpus would not exercise. |
