# Threat model — `rusty_rtos_kernel`

**Unit tier:** critical-path. **Model version:** 1, 2026-09-21.
**Scope:** the scheduler, its IPC objects and its timers — `rusty_rtos_kernel-core`
and the facade over it. The ports, the heaps, the C ABI and the network
libraries are separate units with their own models; where a risk belongs to
one of them this document says so and stops, rather than claiming coverage it
does not have.

Satisfies `use-protection-please` **H-01**. The secrets position is §5
(**H-20**); the residual-risk register is §7 (**H-41**).

---

## 1. What this unit is, in one paragraph

This kernel is a remake of FreeRTOS-Kernel V11.3.1 in Rust. It is **the
trusted computing base of every firmware above it**: if its scheduling
decisions are wrong, every guarantee an application thinks it has is wrong
too, silently. It has no network stack, no filesystem, no dynamic loading and
no notion of a user. It therefore has a small attack surface and an
unusually high blast radius — the opposite shape from most software, and the
reason the evidence below is about *correctness under adversarial input*
rather than about perimeter.

---

## 2. Assets

| asset | why it matters | what failure looks like |
|---|---|---|
| **Scheduling integrity** | the right task runs at the right time, priorities and timeouts hold | a missed deadline in a motor controller; a watchdog task starved and a device that reboots in the field |
| **Memory safety of the firmware above** | the kernel hands out handles to objects it owns | a stale handle read as a live one; one task reading another's queue |
| **Availability** | no panic reachable from an API call or a parsed byte | the whole firmware down, because a panic in `no_std` is a halt |
| **Integrity of the published crates** | downstream builds resolve them by name | a substituted dependency executing at build time or on the device |

---

## 3. Adversaries and what they can do

1. **A buggy or hostile task in the same firmware.** There is no privilege
   boundary between tasks — this is an RTOS, not an OS, and the MPU package
   that would add one ships after 1.0 (§7). Such a task can pass any value to
   any API: a wrong handle, a stale handle, a length that overflows, an
   ISR-only call from a task context.
2. **Crafted bytes on a wire, a store or a bus** reaching a parser. Not in
   this unit — the kernel parses nothing — but its availability is what those
   parsers take down when they panic, so it is named here and owned by the
   library units.
3. **A supply-chain actor** substituting a dependency or a build script.
4. **A field device with no debugger** — not an adversary, but it removes the
   mitigation everyone else relies on: you cannot attach and look. Anything
   this model leaves to "it would be obvious in testing" is a gap.

**Explicitly out of scope:** physical attacks, side channels, glitching, a
compromised toolchain, and anything requiring JTAG. Those are real and they
are not answered here.

---

## 4. The attack paths, and the evidence against each

### 4.1 A wrong scheduling decision — the highest-value path

The path an adversary most wants is not a crash; it is the kernel **quietly
deciding differently** from the kernel the firmware was certified against.

*Mitigation, and it is the unusual one:* every scheduling decision is
compared against the C kernel's own trace, event for event and tick for tick.
`kairos conform --all` runs **26 scenarios byte-identical**, including the
full FreeRTOS demo corpus compiled verbatim from the pinned source. At
100,000 ticks a single scenario is **2,220,518 lines identical**.

*Why this is evidence rather than testing:* the oracle is not our idea of
correct. It is the kernel the industry already ships, so a disagreement is
detectable without anyone having to know which behaviour was intended.

*What it does not cover:* the corpus exercises what the demos exercise. Two
scenarios were added this year precisely because parts of the API had no
scenario reaching them at all (`ApiSweep`, `QueueSet`), and that search is
how two kernel defects were found. **The corpus is a floor, not a ceiling.**

### 4.2 A stale or forged handle

*Mitigation:* handles are indices with a **generation**, never pointers.
Freeing an object bumps its generation, so a handle to a freed object names a
generation that no longer exists and is refused (`Error::Gone`) rather than
followed. An interior offset — a handle that points into the middle of a live
object — is refused too, which the C's `heapVALIDATE_BLOCK_POINTER` does
**not** catch, because it only range-checks.

*Poisoned, not assumed:* removing the generation bump does not fail the
double-free test (a `live` flag catches that on its own); it fails
`a_stale_handle_cannot_read_the_block_that_replaced_it`, which is the
generation's actual job. That is the test that has to fail for the mitigation
to be worth quoting.

### 4.3 A panic reachable from an API

In `no_std` firmware a panic is a halt. The kernel therefore treats
reachability of a panic as a security property, not a quality one.

*Mitigation:* `rusty_rtos_kernel-core` is `#![forbid(unsafe_code)]`, and the
workspace lint denies `unsafe_code` outside audited WRAP crates (`UNSAFE.md`
is the inventory, with the invariant and audit date per block). Indexing,
unwrap, expect, panic and unchecked arithmetic are lint-denied in library
code; every fallible path returns `Result`. A no-panic suite pins the
property.

*Residual:* see §7 — the no-panic suite here is small, and the fuzzing gates
(H-26, H-27) are not met in this unit.

### 4.4 Calling an ISR-safe API from a task, or the reverse

The classic FreeRTOS foot-gun: `xQueueSendFromISR` from a task, or
`xQueueSend` from an ISR. In C both compile.

*Mitigation:* every `FromISR` variant is a separate typed entry, so the wrong
one does not compile rather than failing at run time.

### 4.5 A substituted dependency

*Mitigation:* `Cargo.lock` is committed and a fleet gate (H-07) proves it
resolves in a **fresh clone** rather than only on a developer's box.
`cargo deny` policy denies `*-sys` crates, `ring`, `aws-lc-sys` and
`libc`-linked crates across every repository in the family; `cargo deny
check` runs in the fleet gate.

*Residual:* `cargo vet` coverage (H-10) is not established — see §7.

---

## 5. Secrets — H-20

**No secret enters this unit, by design.** The kernel has no key material, no
credentials and no entropy source; it neither stores nor transports anything
an adversary would want to read. Key material in this family lives behind
PKCS#11 and mID and never crosses into kernel memory.

Consequently there is nothing here to zeroize, and H-20 is satisfied by the
*absence* rather than by a `Zeroize` implementation. **This claim is a
constraint, not an observation**: if a future change gives the kernel a
secret — a random seed for ASLR, a device identity in a TCB — this section is
wrong and H-20 reopens. That is the condition to watch.

The trace subsystem is the one place data leaves the kernel. It emits
scheduling events (task names, handles, tick counts) and never payload bytes;
queue and buffer contents are traced as *lengths*, not contents. A firmware
that considers its task names sensitive should build without the trace
feature.

---

## 6. Assumptions this model depends on

1. **The C kernel is correct enough to be an oracle.** Where FreeRTOS has a
   defect we reproduce it, by construction. Fifteen defects found in the
   pinned C of a sibling unit are recorded rather than silently fixed,
   because a differential that "improves" on its oracle is no longer a
   differential.
2. **The sim's timing contract models the hardware's.** Conformance is proved
   on a deterministic simulator whose clock is critical-section exits. Silicon
   agreement is a separate claim, evidenced separately (the corpus runs
   byte-identical on ESP32-S3 silicon and on a Cortex-M3 under QEMU).
3. **Tasks are not mutually hostile in the memory sense.** Without an MPU
   there is no enforcement; a task that corrupts another's memory is outside
   what this unit can prevent.

---

## 7. Residual risks — H-41

Listed, accepted, and each with the condition that closes it. **A waiver
without a condition is a decision nobody will revisit.**

| # | residual risk | severity | why accepted for now | closes when |
|---|---|---|---|---|
| R-1 | **No privilege separation between tasks.** A hostile task can pass any value to any API and can corrupt any memory it can address. | high | this is what an RTOS is; the mitigation is the MPU package, which is scoped after 1.0 by the mission plan | the MPU unit ships and a firmware runs tasks unprivileged |
| R-2 | **`cargo vet` coverage not established** (H-10). Dependencies are pinned, denied by policy and locked, but not audited row by row. | medium | the dependency set is deliberately tiny and `*-sys`/`libc` crates are denied outright, so the surface is small | a `supply-chain/` config exists and the fleet gate runs `cargo vet` |
| R-3 | **No fuzzing in this unit** (H-26, H-27). The kernel parses nothing, so there is no parser to fuzz — but its APIs take adversarial arguments and are not fuzzed. | medium | the API surface is instead covered by a 26-scenario differential, Kani harnesses and a mutation survey | an API-level fuzz target exists and runs continuously |
| R-4 | **Kani proves the data structures, not the running kernel.** Measured 2026-09-21: harnesses over lists, arenas and names converge in 2–4 s and pass; harnesses that create a task or start the scheduler do **not converge** within 240 s. | medium | the running kernel is covered by the differential and by mutation testing instead; the proofs that do converge cover exactly what a proof is good at | the kernel harnesses converge, or the geometry they run at is shrunk until they do |
| R-5 | **The corpus is a floor.** Trace-identity proves agreement on what the demos do. An API path no scenario reaches is unproven. | medium | the gap is measured rather than assumed — the unreached set was enumerated and reduced to a documented remainder | a coverage instrument replaces the by-hand enumeration |
| R-6 | **No hostile-input testing of the C ABI from this unit.** The shim validates every handle and length; that validation is evidenced in the ABI unit, not here. | low | correct ownership: the boundary belongs to the unit that implements it | the ABI unit's own model covers it |
| R-7 | **Threat model not yet revisited after a major change** (H-02). This is version 1. | low | there has been no major change since it was written | the next change that alters the trust boundary |

---

## 8. How to attack this document

The useful question for a reviewer is not whether the mitigations sound
plausible but **whether each one has a command behind it**. Every claim above
that is evidenced names the gate or the test that produces it, and those run
in the fleet gate rather than by hand. Where a claim has no command it is in
§7 as a residual risk instead — that is the rule this document is written to,
and the places it was uncomfortable to apply are the rows in §7.
