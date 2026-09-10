#!/bin/sh
# Sweep the CBMC proof harnesses, one at a time, with a fixed budget.
#
#     wsl -e sh -c 'sh proofs.sh'        # from this package's root
#     BUDGET=300 sh proofs.sh out.txt    # a longer budget, a named file
#
# Kani 0.67 has no Windows build, so this runs under WSL on this host. It
# needs the umbrella checkout: the sibling crates resolve through the
# gitignored `.cargo/config.toml` that `kairos patches` writes.
#
# Method, and why each part of it is there:
#
#   * **One harness at a time.** A blow-up in a combined run says nothing
#     about the harnesses that would have passed, and CBMC gives no partial
#     credit — a single `cargo kani` that runs out of memory reports one
#     failure for the lot.
#
#   * **`--exact` is not optional.** Kani's `--harness` is a *substring*
#     filter. `--harness queue_generic_send` also runs
#     `queue_generic_send_from_isr` and `queue_generic_send_stale_handle`,
#     so three harnesses share one budget and the first is reported as a
#     timeout it did not earn. Every name that is a prefix of another is
#     mis-measured without this, and several here are.
#
#   * **A budget, not a wait.** The harnesses that do not converge do not
#     converge at 700 s either (measured), so a bounded run costs the truth
#     nothing and makes the sweep repeatable.
#
#   * **Its own target directory.** A Windows cargo and a WSL cargo sharing
#     `target/` fight, and the symptom is a build that hangs with nothing
#     running.
#
# `kairos check --kani` is the cheaper gate beside this one: it compiles
# every harness and verifies none, which is seconds. Run it always; run
# this when the kernel's behaviour changed.
set -u

OUT=${1:-proofs.txt}
BUDGET=${BUDGET:-150}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/kani-target}

cd "$(dirname "$0")" || exit 1

harnesses() {
    grep -B1 '^fn ' crates/rusty_rtos_kernel-core/src/proofs.rs \
        | grep -A1 'kani::unwind\|proof' \
        | grep '^fn ' \
        | sed 's/fn \([a-z_0-9]*\).*/\1/' \
        | grep -v '^small_ticks$'
}

: > "$OUT"
for h in $(harnesses); do
    start=$(date +%s)
    out=$(timeout "$BUDGET" cargo kani -p rusty_rtos_kernel-core \
              --harness "proofs::$h" --exact 2>&1)
    rc=$?
    secs=$(( $(date +%s) - start ))
    if [ $rc -eq 124 ]; then
        verdict=TIMEOUT
    elif echo "$out" | grep -q 'VERIFICATION:- SUCCESSFUL'; then
        verdict=PASS
    elif echo "$out" | grep -q 'VERIFICATION:- FAILED'; then
        verdict=FAIL
    else
        verdict=ERROR
    fi
    printf '%-52s %-8s %4ss\n' "$h" "$verdict" "$secs" | tee -a "$OUT"
done

printf '\n%s PASS  %s FAIL  %s TIMEOUT  %s ERROR  of %s\n' \
    "$(grep -c ' PASS ' "$OUT")" "$(grep -c ' FAIL ' "$OUT")" \
    "$(grep -c ' TIMEOUT ' "$OUT")" "$(grep -c ' ERROR ' "$OUT")" \
    "$(grep -c . "$OUT")" | tee -a "$OUT"
