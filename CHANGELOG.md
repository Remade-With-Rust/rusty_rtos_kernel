# Changelog

Security-relevant changes are called out under **Security** (hardening gate
H-38). Versions follow SemVer; in 0.x a minor bump may break the API.

## Unreleased

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
