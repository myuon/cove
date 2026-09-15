"""Self time per component from a `/usr/bin/sample` call graph.

`scripts/covefmt-profile.sh` is what runs this, and its header is where the
method, its 10% cost and its noise floor are written down. This is the reader:
`sample` prints one indented tree per thread -- `<count> <symbol> (in <image>) +
<offset> [<address>]`, two columns of prefix per level -- and a node's *self*
count is its own count less its children's. `sample`'s own "Sort by top of
stack" table is the same quantity collapsed across every thread, which is no use
here: the main thread of a `cove run` is asleep in `pthread_join` for the whole
run and would be most of the profile.

Two rules beyond "bucket the leaf", and both were arrived at by getting the
answer wrong first:

- **a `bzero` or a `memmove` is charged by who called it.** `_platform_bzero` is
  the frame zero fill under `push_frame` and the allocator's own clearing under
  `malloc`, and charging it whole to either is wrong -- done by the leaf alone it
  put 2.3% of the run in whichever bucket happened to win. The ancestor chain
  decides.
- **an allocator entry point is always allocation**, wherever it was provoked
  from. A `free` under a string builtin is a cost the builtin caused and the
  allocator paid, and the bucket issue #369 asks for is "allocation and
  collection" rather than "allocation the collector asked for".

The patterns are matched against *mangled* names, and the second rule above has
a trap in it that is worth not stepping in twice: a mangled generic name spells
out its type arguments, so `Machine::drive`'s symbol contains the characters
`5alloc3vec3VecyE` and a substring test for "alloc" finds an allocator in the
ancestry of the entire run. `ANCESTOR_ALLOCATOR` is the narrow list that cannot.
"""

import collections
import re
import sys

LINE = re.compile(r"^(?P<prefix>[\s+!:|]*?)(?P<count>\d+) (?P<rest>.*)$")

ALLOCATOR = (
    "malloc",
    "free",
    "calloc",
    "realloc",
    "should_clear",
    "szone",
    "nanov2",
    "madvise",
    "mutex",
    "6Memory5alloc",
    "8allocate",
    "5chunk",
    "rust_alloc",
    "rdl_alloc",
    "rust_dealloc",
    "rdl_dealloc",
    "drop_glue",
)

# The ancestor test, and it is narrower than the bucket above on purpose: a
# mangled generic name holds the words of its type arguments, so `Machine::drive`
# reads `...5alloc3vec3VecyE...` and a substring test for "alloc" finds an
# allocator in the ancestry of the whole run. Every pattern here is one that
# cannot occur inside a mangled name.
ANCESTOR_ALLOCATOR = (
    "malloc",
    "szone",
    "nanov2",
    "should_clear",
    "free_tiny",
    "free_small",
    "free_medium",
    "6Memory5alloc",
    "8allocate",
    "madvise",
)

BUCKETS = [
    ("generated native code", ["<generated native code>"]),
    (
        "tier lookup and transition",
        ["NativeProgram", "Tiered5entry", "7tier_of", "12from_encoded"],
    ),
    (
        "open/close/call helpers",
        [
            "native4open",
            "native5close",
            "native4call",
            "native9call_body",
            "native5enter",
            "native9republish",
            "10open_frame",
            "9words_of",
        ],
    ),
    ("safepoints", ["9safepoint", "6budget", "12cancellation", "10bulk_work"]),
    (
        "frame growth and zeroing",
        ["10push_frame", "9pop_frame", "11clear_words", "Space4fill", "bzero", "memset"],
    ),
    (
        "slot copies: arguments, returns and `copy`",
        [
            "10copy_slots",
            "10copy_words",
            "10read_words",
            "11write_words",
            "9read_into",
            "10write_from",
            "17copy_string_bytes",
            "memmove",
            "memcpy",
        ],
    ),
    ("allocation and collection", list(ALLOCATOR) + ["7collect", "4Heap"]),
    ("runtime intrinsics", ["10intrinsics", "14call_intrinsic"]),
    (
        "encoded dispatch",
        [
            "encoded8dispatch",
            "exec9int_arith",
            "12append_bytes",
            "12string_bytes",
            "10bytes_word",
            "15compare_strings",
            "6buffer",
            "13finish_buffer",
            "11payload_run",
            "15set_payload_run",
            "7checked",
            "Machine4span",
            "DYLD-STUB",
        ],
    ),
    (
        "linear-memory slot access",
        ["6Memory4read", "6Memory5write", "7element", "5blend", "13object_layout",
         "10object_len", "13payload_words", "19fixed_payload_words", "17try_payload_words"],
    ),
]

# Charged by the caller rather than by itself.
BY_CALLER = ("bzero", "memset", "memmove", "memcpy", "_platform_", "DYLD-STUB")
IDLE = ("__ulock_wait", "__psynch_cvwait", "kevent", "mach_msg")


def bucket_of(symbol):
    for name, patterns in BUCKETS:
        if any(pattern in symbol for pattern in patterns):
            return name
    return None


def rows_of(path):
    rows, inside = [], False
    for line in open(path):
        if line.startswith("Call graph:"):
            inside = True
            continue
        if line.startswith("Total number in stack"):
            break
        if not inside:
            continue
        found = LINE.match(line.rstrip("\n"))
        if found is None:
            continue
        rest = found.group("rest")
        symbol = rest.split(" (in ")[0]
        if "<unknown binary>" in rest:
            symbol = "<generated native code>"
        rows.append((len(found.group("prefix")) // 2, int(found.group("count")), symbol))
    return rows


def cove_thread(rows):
    out, taking, base = [], False, 0
    for depth, count, symbol in rows:
        if symbol.startswith("Thread_"):
            taking = "cove entry" in symbol
            base = depth
            continue
        if taking:
            out.append((depth - base, count, symbol))
    return out


def tally(rows):
    held = collections.Counter()
    chain = []
    for at, (depth, count, symbol) in enumerate(rows):
        while chain and chain[-1][0] >= depth:
            chain.pop()
        children = 0
        for deeper, more, _ in rows[at + 1 :]:
            if deeper <= depth:
                break
            if deeper == depth + 1:
                children += more
        own = count - children
        if own > 0 and not any(idle in symbol for idle in IDLE):
            above = [name for _, name in chain]
            # An allocator anywhere above means this is allocator work whatever
            # the leaf says; that is what tells the frame zero fill from the
            # allocator's own clearing.
            under_allocator = any(
                any(pattern in name for pattern in ANCESTOR_ALLOCATOR) for name in above
            )
            if any(k in symbol for k in BY_CALLER) and under_allocator:
                name = "allocation and collection"
            else:
                name = bucket_of(symbol)
                if name is None:
                    for parent in reversed(above):
                        found = bucket_of(parent)
                        if found is not None:
                            name = found
                            break
            held[(name, symbol)] += own
        chain.append((depth, symbol))
    return held


def main():
    held = collections.Counter()
    for path in sys.argv[1:]:
        held.update(tally(cove_thread(rows_of(path))))
    grand = sum(held.values())
    buckets = collections.Counter()
    for (name, _), count in held.items():
        buckets[name or "unattributed"] += count

    print(f"{grand} samples in the Cove thread, over {len(sys.argv) - 1} run(s)")
    print(f"  {'component':<44} {'samples':>9} {'share':>8}")
    for name, _ in BUCKETS:
        print(f"  {name:<44} {buckets[name]:>9} {100 * buckets[name] / grand:>7.2f}%")
    rest = buckets["unattributed"]
    print(f"  {'unattributed':<44} {rest:>9} {100 * rest / grand:>7.2f}%")

    host = sum(
        count
        for (name, symbol), count in held.items()
        if name == "allocation and collection"
        and not symbol.startswith("_RNvM")
        and "cove_runtime" not in symbol
    )
    print(
        f"  {'-- of which the host allocator (malloc/free/mutex)':<44} "
        f"{host:>9} {100 * host / grand:>7.2f}%"
    )

    print("\n  unattributed, over 0.05% each")
    for (name, symbol), count in held.most_common():
        if name is None and count * 2000 > grand:
            print(f"    {100 * count / grand:>6.2f}%  {count:>7}  {symbol[:100]}")
    print("\n  the twenty-five dearest symbols")
    for (name, symbol), count in held.most_common(25):
        print(f"    {100 * count / grand:>6.2f}%  {count:>7}  {(name or '-'):<42}  {symbol[:74]}")


main()
