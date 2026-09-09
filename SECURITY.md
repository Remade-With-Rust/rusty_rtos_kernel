# Security policy

`rusty_rtos_kernel` is part of Kairos (FreeRTOS remade in memory-safe Rust, by
Remade With Rust). The kernel and everything that parses bytes from a wire, a
store or a bus is treated as critical-path code: `forbid(unsafe)` in the core,
a no-panic gate on every parser, `cargo deny` on every push, and a hardening
audit rendered at the foot of the README from
`docs/plans/use-protection-please.md`.

## Reporting a vulnerability

Please do not open a public issue for a security problem.

- Email **security@mata.network** with the crate name, the version, a
  description and, where possible, a reproducer.
- You will receive an acknowledgement within **5 working days** and a
  status update at least every **14 days** until the report is resolved.
- We ask for **90 days** of coordinated disclosure from the acknowledgement;
  we will credit you in the advisory unless you prefer otherwise.

## Scope

In scope: memory safety, panics reachable from untrusted input, scheduling
invariants (priority inversion, starvation the C kernel does not exhibit),
timing side channels in any cryptographic path, and supply-chain integrity of
the published crates.

Out of scope: the C FreeRTOS oracle checked out under `oracle/` in the
umbrella (report those to FreeRTOS), and vendor radio blobs a firmware links.

## Supported versions

Until 1.0.0, only the latest published version receives fixes. The v1.0.0
gate (the 17 starred rows of the hardening audit) is what turns that into a
support window.
