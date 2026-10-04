# Changelog

Security-relevant changes are called out under **Security** (hardening gate
H-38). Versions follow SemVer; in 0.x a minor bump may break the API.

## Unreleased

### Changed
- **One-core and real-port instruction cost.** Fifteen measured wins, none
  changing behaviour (both conformance corpora, the API differential and the
  SMP differential unchanged). On the shipped RISC-V port the seventeen
  `bench/tick-work` rows fall from 1,697 to 1,364 retired instructions
  (`peek_ok` 73 -> 41, `queue_roundtrip` 152 -> 113, `block_cycle` 965 ->
  847); the one-core kernel rows of `bench/kernel-ir` fall 2.3% and the
  two-core shape A 8-10%. The largest pieces: `add_task_to_ready_list_at`,
  so the wake, resume and inheritance paths stop re-resolving a TCB around
  their list edits; `unlock_queue`'s drain loops out of line on the speed
  profile; the tick hook not copied when `TickHook::wants_tick` says it has
  nothing to do (needs `rusty_rtos_core`'s next release); a dead atomic load
  of the port's exit count on every traced path, gated on `T::EMITS`.
- Flash, `bench/kernel-flash`: the `small` profile 17,776 -> 17,646 B; the
  speed profile +294 B from this crate (bodies in line at the paths above).

## 0.3.2 — 2026-10-02

The two-core kernel, made cheaper: fifteen instruction-count wins on the
SMP paths, each measured on its own and none changing behaviour.

### Changed
- **SMP scheduling cost.** Callgrind on the two-core demo corpus (20,000
  ticks) and the two-core differential, the kernel's own instructions:
  semtest -23.8%, BlockQ -20.3%, recmutex -24.5%, differential -31.8%. The
  largest pieces: `prvSelectHighestPriorityTask` walks held tasks by slot and
  resolves only the one it picks, moves the running task with one list
  operation (`rusty_rtos_core` 0.2.4's `move_to_end`) and is in line in the
  switch; `prvYieldForTask` reads the idle mark as a TCB flag bit, as the C
  keeps `taskATTRIBUTE_IS_IDLE`, and tests the running task by slot. Every
  change kept the nine-scenario two-core corpus and both SMP differentials
  identical to FreeRTOS.
- One-core builds are unchanged to the instruction (`bench/kernel-ir`'s
  kernel rows read 219,103,567 before and after).
- Requires `rusty_rtos_core` 0.2.4.

## 0.3.1 — 2026-10-02

SMP is no longer a preview.

### Fixed
- **SMP: priority inheritance on two cores.** `xTaskPriorityInherit`,
  `xTaskPriorityDisinherit` and `vTaskPriorityDisinheritAfterTimeout` each
  have an SMP arm in the C that this kernel lacked: a raised holder that is
  not running is yielded for, and a lowered holder that is running has its
  core yielded. Found by the two-core demo corpus (`recmutex`). One-core
  builds are unchanged.

### Added
- **The two-core demo corpus** (`rusty_rtos_demo`, `--features smp`): nine
  standard demo scenarios identical to FreeRTOS on two cores at 20,000 ticks.
- **Blocking waits in the two-core differential** (`smp_block.trace`).
- **`small`**: the flash profile -- 1.24x the C kernel's flash on rv32
  (1.40x by default), at a priced cost in instructions on the short queue
  paths. See the README.
- `tests/invariants.rs`: seven documented invariants checked after every
  call of 144,000 random kernel calls (hardening gate H-28).

## 0.3.0 — 2026-10-02

**Breaking** (0.x minor): `Kernel::current` is no longer `const fn`, and
`StartHandles` has a new public field. SMP is a **preview**: see the README.

### Added
- **SMP, slices S1, S1b, S2a** (`configNUMBER_OF_CORES = 2`, `docs/plans/smp.md`).
  The scheduler with two cores, read from the pinned `tasks.c`, and the other-core
  delete, suspend, priority-set, resume and wake paths. A 20,000-step two-core
  differential against the C kernel (`tests/smp_differential.rs`) is identical
  on every line. S1 in detail:
- **SMP, slice S1** (`configNUMBER_OF_CORES = 2`, `docs/plans/smp.md`):
  per-core `current` and `xYieldPendings`, one idle task per core,
  `prvSelectHighestPriorityTask`, `prvYieldForTask`, `prvYieldCore`, and
  per-core time slicing on the tick. A port drains cross-core yields with
  `Kernel::take_core_yields()`. `Kernel::current_on(core)` and
  `StartHandles::passive_idle` are new.

### Changed
- `Kernel::current` is no longer `const fn`: on SMP the answer depends on
  the calling core.
- One-core builds are unchanged: conformance-identical (all 25 corpus
  scenarios), and instruction-count neutral on all five `bench/*-ir`
  benches (-0.45 % to +0.13 %).

## 0.2.2 — 2026-10-01

### Security
- `cargo vet` coverage (`supply-chain/`), zero exemptions.
- `fuzz/kernel_api`: every public call with fuzzer-chosen arguments, through
  the same dispatcher as `tests/no_panic.rs`.
- Threat model revision 2 (`docs/threat-model.md`).
- CI: every action pinned to a commit SHA, `permissions: contents: read`,
  `cargo vet --locked`, the unsafe census, the hardening-table check and a
  fuzz regression per push; fuzzing, AddressSanitizer and `cargo careful`
  nightly.

### Changed
- `queue_take` resolves the queue once: rv32 flash 19,726 -> 19,484 B,
  kernel-ir -395,353 Ir; conformance 26/26 unchanged.

### Fixed
- CI was red at 0.2.1 (an unused import, an orphaned doc comment, `cargo fmt`);
  green.
