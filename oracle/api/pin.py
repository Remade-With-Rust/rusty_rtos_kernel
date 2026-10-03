#!/usr/bin/env python3
"""Turn an API-differential trace into a PIN: every step kept, every
observation replaced by the FNV-1a/64 digest of its text.

A pin is a third of a trace's size and judges exactly as much -- the replay
compares Kairos's observation's digest -- but a failure shows only Kairos's
line. Regenerate the C's with the seed in the pin's header:
`SEED=<seed> STEPS=<n> sh run.sh` (or the api1 / api2 binaries directly).

    python3 pin.py fresh/api2-0x3c6ef74a.trace pins/api2-0x3c6ef74a.pin
"""
import sys


def fnv1a64(data: bytes) -> int:
    h = 0xCBF29CE484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def main() -> None:
    src, dst = sys.argv[1], sys.argv[2]
    lines = open(src, encoding="utf-8").read().splitlines()
    out = [lines[0]]
    for line in lines[1:]:
        step, want = line.split(" | ", 1)
        out.append(f"{step} | #{fnv1a64(want.encode()):016x}")
    with open(dst, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(out) + "\n")


if __name__ == "__main__":
    main()
