#!/usr/bin/env python3
"""Disassemble a raw x86-64 dump (COVE_NATIVE_DUMP) with clang + objdump."""
import os, subprocess, sys, tempfile

path = sys.argv[1]
data = open(path, "rb").read()
with tempfile.TemporaryDirectory() as t:
    s = os.path.join(t, "a.s")
    with open(s, "w") as f:
        f.write(".text\n.globl _f\n_f:\n")
        for i in range(0, len(data), 16):
            f.write(".byte " + ",".join(str(x) for x in data[i : i + 16]) + "\n")
    o = os.path.join(t, "a.o")
    subprocess.run(["clang", "-c", "-target", "x86_64-apple-macos", s, "-o", o], check=True)
    out = subprocess.run(["objdump", "-d", "--no-show-raw-insn", o], check=True, capture_output=True, text=True).stdout
    print("\n".join(out.splitlines()[6:]))
