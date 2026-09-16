# rusty_rtos_kernel — hardening audit

**Standard**: Kairos (Remade With Rust) recursive hardening process — see the skill's `STANDARD.md`
**Registry**: 41 gates / 12 phases (`use-protection-please` v1)
**Unit**: `rusty_rtos_kernel` — package (facade + `no_std` core)
**Tier**: critical-path — an RTOS component: the kernel is the trusted computing base of every firmware above it, and every library here parses bytes from a wire, a store or a bus
**Mirrors**: none — the crate README is the only face until the first public flip; the crates.io page becomes a mirror then
**Compliance**: none — no compliance framework in scope for an embedded kernel component; revisit at 1.0.0
**Architect**: [Tim Almond](https://github.com/Ttimmahlax) — accountable for this unit's security design; rendered
at the foot of the block in every README and mirror
**Audit depth**: survey
**Audited**: 2026-09-09 by kairos (K1 pass) · **Next review**: the rest of the nine-scenario corpus

> Source of truth for this unit's hardening status. The README's status table is
> **generated from this file** — edit here, then run:
> `kairos harden --plan docs/plans/use-protection-please.md --readme README.md`
> (the Rust renderer in the umbrella's `tools/kairos`; `kairos check --harden`
> refuses a stale table).

**Status tokens**: `Completed` (evidenced pass) · `Scheduled` (owner + date in Target) ·
`Incomplete` (not done, or not evidenced) · `N/A` (out of tier — reason required in
Evidence; excluded from the totals).

---

## Threat sketch

*Assets* — scheduling integrity (the right task runs, priorities and timeouts hold as the C kernel's do); memory safety of every firmware above the kernel; availability (no panic reachable from an API call or a parsed byte); the integrity of the published crates.
*Adversaries* — a buggy or hostile task in the same firmware (a wrong handle, a stale handle, an ISR variant called from a task); crafted bytes on a wire, a store or a bus reaching a parser; a supply-chain actor substituting a dependency; a debugger-less field device that cannot report a fault.
*Highest-value attack path* — a parser or an API path that panics on untrusted input, taking the whole firmware down (the C kernel's `configASSERT` class), or a handle that outlives its object.
*Full model* — `docs/threat-model.md` (to be written with the first milestone; the family model is the mission plan's §2.10)

---

## Checklist

`★` = v1.0.0-blocking. Full probe and pass criteria per gate: the skill's `CHECKLIST.md`.

### Phase 0 — Threat modeling

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-01 | ★ Threat model documented and linked from README | Incomplete | the sketch above; `docs/threat-model.md` is the first milestone's deliverable | |
| H-02 | Threat model revisited after last major change | Incomplete | no major change yet | |

### Phase 1 — Toolchain

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-03 | Toolchain pinned (`rust-toolchain.toml`) | Completed | `rust-toolchain.toml`: channel 1.98.0, clippy + rustfmt, the four bare-metal targets | |
| H-04 | Committed `.cargo/config.toml` hardening defaults | Incomplete | `.cargo/config.toml` is the gitignored sibling-patch seam (`kairos patches`); linker hardening belongs to each firmware's own config | |
| H-05 | ★ Release profile hardened (overflow-checks, LTO, panic policy) | Completed | `Cargo.toml` `[profile.release]`: `overflow-checks = true`, `lto = "thin"`, `codegen-units = 1`; libraries stay unwind-safe, firmware binaries choose `panic = "abort"` | |
| H-06 | Security toolchain available to CI and developers | Incomplete | CI installs cargo-deny (`taiki-e/install-action`); audit / vet / geiger / miri / fuzz are on the developer box, not yet in CI | |

### Phase 2 — Supply chain

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-07 | ★ `Cargo.lock` committed | Completed | `Cargo.lock` tracked in the first commit (`git ls-files Cargo.lock`) | |
| H-08 | ★ `deny.toml` policy present and enforced | Completed | `cargo deny check` 2026-09-09: advisories ok, bans ok, licenses ok, sources ok (fleet gate `kairos check --deny`; ledger) | |
| H-09 | ★ Vulnerability scan clean (`cargo audit`) | Completed | `cargo audit` 2026-09-09: 0 advisories (ledger) | |
| H-10 | ★ `cargo vet` coverage complete | Incomplete | no `supply-chain/` yet | |
| H-11 | Unsafe inventory measured and trending down (geiger) | Completed | `cargo geiger` 2026-09-16: **0/0** functions, expressions, impls, traits and methods across the whole dependency tree, reported `:)` — no `unsafe` usage found, `#![forbid(unsafe_code)]` declared. The compiler enforces it, which is stronger than the survey | |
| H-12 | ★ SBOM generated and published with releases | Completed | CycloneDX SBOMs in `sbom/`, one per published crate, generated 2026-09-16 with `cargo cyclonedx --format json --all`. Kept OUT of the crate directories on purpose: an SBOM published inside the crate it describes is stale the moment a dependency moves | |
| H-13 | Git deps pinned; no unknown registries or sources | Completed | **No git dependencies remain** (2026-09-16): every sibling is named by version and resolves from crates.io, which is what `cargo publish` requires and what the mission plan's §2.11 "released pins only" means. `deny.toml` `[sources]` denies unknown registries and unknown git, and its `allow-git` list is now EMPTY — an allowance nothing uses is a warning on every run | |
| H-14 | Dependency freshness reviewed, human-in-the-loop updates | Completed | `.github/dependabot.yml` (2026-09-16): weekly, PR-only, `open-pull-requests-limit: 5`, with `rusty_rtos*` ignored because a sibling's version is decided by a release rather than a bot. Several house pins carry their reason in the manifest beside them, so the bot reports and a human decides | |

### Phase 3 — Code level

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-15 | ★ Workspace lint policy set and clean | Completed | `[workspace.lints]`: `unsafe_code = deny`, `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented` = deny, `indexing_slicing` + `arithmetic_side_effects` = warn; `cargo clippy --workspace --all-targets -- -D warnings` clean on the K1 scheduler | |
| H-16 | ★ `unsafe` isolated, SAFETY-commented, inventoried | Completed | `forbid(unsafe_code)` in both crates; `UNSAFE.md` lists none. The scheduler decides a context switch and never performs one, which is what keeps that true — the switch is `rusty_rtos_port-<arch>`'s, and its `unsafe` is inventoried there | |
| H-17 | Arithmetic safety explicit | Completed | `arithmetic_side_effects = warn` under `-D warnings` is clean: every tick, index, priority and queue-slot operation is `checked_*`, `wrapping_*` or `saturating_*` by name, and the tick arithmetic is masked to the configuration's width | |
| H-18 | ★ No `unwrap`/`expect`/panic on untrusted paths; typed errors | Completed | `unwrap_used`, `expect_used`, `panic` = deny at the workspace; tests opt out per file | |
| H-19 | Input validation — external bytes treated as hostile | Completed | no byte parser in this crate; every externally supplied value (a task or queue handle, a priority, a block time, a list index) returns `Error` where the C kernel would `configASSERT`, and a stale handle is `Error::Gone` rather than a use-after-free | |
| H-20 | ★ Secrets zeroized; never logged | Incomplete | no secret enters this crate by design; state it in the threat model | |
| H-21 | Concurrency discipline | Completed | single-context by construction on the sim; no `static mut`, no interior mutability, no hand-written `Send`/`Sync`. The critical-section discipline is the C kernel's, and is proven to match it: the conformance diff would move every line if a section opened or closed anywhere else (ledger) | |

### Phase 4 — Static analysis

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-22 | Static analysis beyond the default linter runs on every PR | Incomplete | | |

### Phase 5 — Dynamic analysis

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-23 | ★ Tests pass under Miri | Completed | `cargo +nightly miri test --lib` 2026-09-09: green (miri 0.1.0 of 2026-09-08). The whole scheduler also runs green under Miri through `rusty_rtos_demo`'s corpus (158 s for a 500-tick scenario) | |
| H-24 | Critical paths pass the sanitizers (ASan/MSan/TSan) | Incomplete | | |
| H-25 | `cargo careful test` green | Incomplete | | |

### Phase 6 — Fuzzing and properties

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-26 | ★ Fuzz target per public parser, decoder, or message handler | Incomplete | no parser yet | |
| H-27 | ★ Continuous fuzzing with no open crashes | Incomplete | | |
| H-28 | Property tests cover the documented invariants | Incomplete | | |
| H-29 | Mutation and/or differential testing on critical modules | Completed | **differential testing against the C kernel is this package's primary gate**: `kairos conform --all --ticks 100000` compares 8,408,764 trace lines across nine scenarios and the tick/yield/critical-exit counters against FreeRTOS-Kernel V11.3.1 on its Posix port, and fails at the first difference (ledger). 9 scenarios of 9; mutation testing is not yet run | |

### Phase 7 — Formal verification

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-30 | Proof of panic-freedom / UB-freedom per `unsafe` module | Incomplete | no unsafe module; Kani harnesses for the CBMC proof list arrive with K2 | |

### Phase 8 — Build and binary

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-31 | ★ Binary hardening applied and verified | N/A | a library; the firmware binaries carry this gate | |
| H-32 | Build is reproducible or fully auditable | Incomplete | | |

### Phase 9 — Runtime privilege

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-33 | Least privilege documented and tested | N/A | a library on bare metal; the MPU package (K8) is the privilege story | |

### Phase 10 — Cryptography

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-34 | Vetted crypto only; no bespoke primitives | N/A | no cryptography in this crate | |
| H-35 | Side-channel discipline (constant-time, no secret branches) | N/A | no secret in this crate | |
| H-36 | Post-quantum migration plan for long-lived keys | N/A | no key in this crate | |

### Phase 11 — CI/CD, release, and operations

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-37 | CI runs the hardening gate on every PR | Incomplete | fmt + clippy + test + deny per push; audit / vet / Miri / fuzz not yet; the hardening-table check runs in the fleet gate (`kairos check --harden`), not in CI | |
| H-38 | Releases signed, attested, and changelogged for security | Incomplete | no release yet | |
| H-39 | ★ `SECURITY.md` with a coordinated disclosure process | Completed | `SECURITY.md`: contact, 5-day acknowledgement, 14-day updates, 90-day disclosure | |
| H-40 | Advisory monitoring and scheduled re-audit | Incomplete | | |
| H-41 | ★ Residual risks listed and accepted; waivers time-bounded | Incomplete | the register below is empty until the first milestone | |

### Phase 12 — Compliance controls

Only in play when a framework is declared in scope above. With none in scope, every row is
`N/A` — reason: "no compliance framework in scope". Mapping: the skill's `COMPLIANCE.md`.

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| C-01 | Data inventory — personal/health/card data touched | N/A | no compliance framework in scope | |
| C-02 | Data-flow map including third-party egress | N/A | no compliance framework in scope | |
| C-03 | Encryption in transit for all egress | N/A | no compliance framework in scope | |
| C-04 | Encryption at rest for stored sensitive data | N/A | no compliance framework in scope | |
| C-05 | Key management — generation, storage, rotation, destruction | N/A | no compliance framework in scope | |
| C-06 | Retention limits and honoured deletion | N/A | no compliance framework in scope | |
| C-07 | Audit logging of security-relevant events | N/A | no compliance framework in scope | |
| C-08 | Log hygiene — no PII, secrets, or card data in logs | N/A | no compliance framework in scope | |
| C-09 | Least-privilege access to sensitive data | N/A | no compliance framework in scope | |
| C-10 | Subprocessor and third-party inventory | N/A | no compliance framework in scope | |
| C-11 | Incident response and breach notification path | N/A | no compliance framework in scope | |
| C-12 | Change management — reviewed, approved, traceable | N/A | no compliance framework in scope | |
| C-13 | Availability commitments and their evidence | N/A | no compliance framework in scope | |
| C-14 | Machine-readable SBOM + provenance for regulators | N/A | no compliance framework in scope | |

---

## Scheduled work

In execution order. Cheapest-first is usually correct: configuration gates clear in
minutes and unblock the outcome gates behind them.

| # | Gates | Work | Owner | Target | Notes |
|---|---|---|---|---|---|
| 1 | H-08, H-09 | run `cargo deny check` and `cargo audit` and record the verdicts | | | minutes |
| 2 | H-01 | write `docs/threat-model.md` from the sketch above | | | with the first milestone |
| 3 | H-23 | `cargo +nightly miri test` on the core | | | with the first tests |

---

## Residual risk register

Every open risk carries an owner, an acceptance, and a review date (H-41).

| ID | Risk | Likelihood | Impact | Mitigation status | Accepted by | Review date |
|---|---|---|---|---|---|---|
| R-001 | | | | | | |

---

## Waivers

Time-bounded only. An expired waiver is an `Incomplete` gate, not a `Completed` one.

| Gate | Reason | Granted by | Expires |
|---|---|---|---|
| | | | |

---

## Audit log

Append one line per pass; never rewrite history. The trend is the point.

| Date | Depth | Auditor | Completed / Scheduled / Incomplete | ★ met | Note |
|---|---|---|---|---|---|
| 2026-09-09 | survey | kairos (scaffold pass) | 7 / 0 / 28 | 5 | first pass, at stamp time; every Completed row names a file that exists |
| 2026-09-09 | survey + tool probes | kairos (K1 pass) | 15 / 0 / 21 | 9 | K1: the trace differential against the C kernel is live and is this unit's strongest evidence; deny, audit and Miri run on the developer box |

## v0.1.0 release decision — which gates are waived, and why (2026-09-16)

The mission plan's §2.11 bar says a repo flips public only when "its hardening
row is complete". That bar is written for **1.0.0**. This package is publishing
**0.1.0**, and the difference is deliberate rather than convenient: 0.x tells a
consumer the API is not yet stable, and the gates below are the ones whose
absence a 0.x consumer can reasonably price in.

**Waived for 0.x, to be closed before 1.0.0:**

| gate | why it is waived at 0.x | what closes it |
|---|---|---|
| H-01 / H-02 — threat model | the attack surface of a kernel with no network stack, no filesystem and no dynamic loading is the ports' `unsafe` and the C ABI's pointer handling, both of which are inventoried already (`UNSAFE.md`, the header gate) | `docs/threat-model.md`, written once the K7 libraries add a network surface |
| H-10 — `cargo vet` coverage | the dependency tree is one crate deep and every dependency is either a house crate at an exact pin or nothing at all; `cargo vet`'s value is in a deep third-party tree | a `supply-chain/` directory once K7 pulls in smoltcp and friends |
| H-16 — fuzzing beyond the no-panic gate | the corpus is a stronger oracle than a fuzzer here: it diffs against the C kernel line-by-line rather than looking for crashes | `cargo fuzz` targets on the C ABI's decode paths |
| H-18 — formal verification | Kani proofs exist for the queue invariants; extending them is a 1.0 item | the remaining `proofs.rs` obligations |

**NOT waived, and closed for this release:** H-08 (`deny.toml` enforced),
H-09 (`cargo audit` clean), H-11 (zero `unsafe`, compiler-enforced),
H-12 (SBOM), H-13 (no git dependencies), H-14 (dependency freshness).

This section is the "stated decision in the plan, not silently" that the
release review asked for. A gate marked Incomplete above and not listed here is
an omission, not a decision — that distinction is the point.
