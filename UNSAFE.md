# Unsafe inventory — `rusty_rtos_kernel`

Every `unsafe` block in this repository, its purpose, its invariant and the
date it was last audited (use-protection-please H-16). The workspace lint is
`unsafe_code = "deny"`; the core crate is `forbid(unsafe_code)`; a WRAP crate
that must open a block does so with `#[allow(unsafe_code)]` on the smallest
possible item and a `// SAFETY:` comment beside it, and adds a row here in the
same commit.

| crate | file:line | purpose | invariant | last audit |
|---|---|---|---|---|
| — | — | none: the whole repository is safe Rust | — | 2026-09-09 |

`cargo geiger --all-features` is the measurement; the count in this table may
not rise without a decision-log row in `docs/plans/rusty_rtos_kernel.md`.
