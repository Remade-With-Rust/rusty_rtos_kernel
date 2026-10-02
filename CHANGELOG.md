# Changelog

Security-relevant changes are called out under **Security** (hardening gate
H-38). Versions follow SemVer; in 0.x a minor bump may break the API.

## Unreleased

### Added
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
