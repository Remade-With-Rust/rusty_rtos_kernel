#!/bin/sh
# Fresh seeds for the API differential (plan decision D2): write N new
# scripts, both builds, into fresh/ (git ignores it), for
# `cargo test --release -p rusty_rtos_kernel-core --test api_differential -- --ignored`.
# A seed that fails there is a finding; pin it by making it the run.sh default.
#
#   wsl -e sh -c 'cd /mnt/f/coding/rusty_RTOS/rusty_rtos_kernel/oracle/api && sh run.sh && N=8 sh fresh.sh'
#
# run.sh builds api1/api2 first; FROM picks the first seed (default: the
# epoch second, so two runs do not repeat each other).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
N=${N:-8}
STEPS=${STEPS:-28000}
FROM=${FROM:-$(date +%s)}
mkdir -p "$here/fresh"
i=0
while [ "$i" -lt "$N" ]; do
    seed=$(printf '0x%08x' $(( (FROM + i * 2654435761) & 0xffffffff )))
    for cores in 1 2; do
        "$here/api$cores" "$seed" "$STEPS" > "$here/fresh/api$cores-$seed.trace"
        if grep -q '^ASSERT' "$here/fresh/api$cores-$seed.trace"; then
            echo "seed $seed, $cores core(s): the C asserted -- the generator asked for something undefined" >&2
            exit 1
        fi
    done
    i=$((i + 1))
done
echo "wrote $N seeds x 2 builds to $here/fresh"
