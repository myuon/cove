"""Writes `float_math_reference.txt`, the oracle `float_math.rs` holds
`std.float`'s elementary functions to.

Run with `python3 -I float_math_reference.py SEED PER > float_math_reference.txt`
(the committed file is seed 648, 100 per regime). It needs `mpmath` and
nothing else; every reference value is computed at 300 bits and written as
two doubles, `hi` the exact value correctly rounded and `rest` what is left
of it measured in units in the last place of `hi`, so that a test can measure
an answer's error in those units without a bignum of its own — subnormal
answers included, whose remainder as a double would underflow — and a last column that is `=` when the exact
value is `hi` itself and `~` when it is not. A `NaN` answer is written as
`nan`.

The inputs are seeded random draws from every regime the six functions have —
`exp` across its whole finite range and near nought, `ln` and `log10` over
every finite bit pattern, `sin` and `cos` near nought, through the Cody–Waite
range and past it into the Payne–Hanek one, `pow` near one with huge
exponents, at subnormal answers, and between integers — and the edges: the
signed zeros, the subnormals, the overflow and underflow thresholds, the
reduction boundaries, every normal power of ten, and every power of two
integers below `2^53`.
"""

import math
import random
import struct
import sys

import mpmath

mpmath.mp.prec = 300


def bits(x):
    return struct.unpack('<Q', struct.pack('<d', x))[0]


def frombits(b):
    return struct.unpack('<d', struct.pack('<Q', b))[0]


def finite(r):
    while True:
        x = frombits(r.getrandbits(64))
        if x == x and abs(x) != math.inf:
            return x


def inputs(seed, per):
    r = random.Random(seed)
    rows = []
    edges = [0.0, -0.0, 1.0, -1.0, 2.0, 0.5, 5e-324, -5e-324, 2.2250738585072014e-308,
             1.7976931348623157e308, -1.7976931348623157e308, math.pi, math.pi / 2, math.pi / 4,
             2 * math.pi, 1e22, 1e300, 709.782712893384, 709.7827128933841, -745.1332191019411,
             -745.1332191019412, -708.3964185322641, 1647099.0, 1647098.9999999998,
             6381956970095103 * 2.0 ** 797, 0.7853981633974483, 3.725290298461914e-09,
             1.4901161193847656e-08, 1.0536709282860102e-08]
    for fn in ('exp', 'ln', 'log10', 'sin', 'cos'):
        for e in edges:
            rows.append((fn, e))
    for _ in range(per):
        rows.append(('exp', r.uniform(-746, 710)))
        rows.append(('exp', r.uniform(-1, 1)))
        rows.append(('exp', math.ldexp(r.uniform(-1, 1), -r.randint(0, 60))))
        rows.append(('ln', abs(finite(r))))
        rows.append(('ln', r.uniform(0.5, 2)))
        rows.append(('ln', 1 + math.ldexp(r.uniform(-1, 1), -r.randint(10, 50))))
        rows.append(('log10', abs(finite(r))))
        rows.append(('log10', r.uniform(0.5, 2)))
        for fn in ('sin', 'cos'):
            rows.append((fn, r.uniform(-10, 10)))
            rows.append((fn, math.exp(r.uniform(math.log(1e-10), math.log(1.7e6))) * r.choice((1, -1))))
            rows.append((fn, math.exp(r.uniform(math.log(1.7e6), math.log(1e22))) * r.choice((1, -1))))
            rows.append((fn, finite(r)))
        rows.append(('pow', r.uniform(0, 10), r.uniform(-50, 50)))
        rows.append(('pow', math.exp(r.uniform(-700, 700)), r.uniform(-1, 1)))
        rows.append(('pow', r.uniform(0.999, 1.001), r.uniform(-1e6, 1e6)))
        rows.append(('pow', r.uniform(0.9999995, 1.000001), r.uniform(-3e9, 3e9)))
        rows.append(('pow', float(r.randint(-40, 40)), float(r.randint(-40, 40))))
        rows.append(('pow', -r.uniform(0, 10), float(r.randint(-300, 300))))
        b = r.uniform(0.5, 2.0)
        rows.append(('pow', b, r.uniform(-1074, -1022) / math.log2(b)))
        rows.append(('pow', abs(finite(r)), r.uniform(-2, 2)))
    for n in range(-307, 309):
        rows.append(('log10', float(f'1e{n}')))
    for b in range(2, 30):
        for e in range(0, 54):
            if b ** e < 2 ** 53:
                rows.append(('pow', float(b), float(e)))
    return rows


def exact(fn, args):
    a = [mpmath.mpf(v) for v in args]
    x = a[0]
    if fn == 'exp':
        # Past the range a double answers in, the exact value is only
        # astronomically large or small, and `mpmath` would take minutes to
        # say how much: stand-ins on the right side of every rounding do.
        if x > 710:
            return mpmath.mpf(2) ** 2000
        if x < -746:
            return mpmath.mpf(2) ** -2000
        return mpmath.exp(x)
    if fn in ('ln', 'log10'):
        if x < 0:
            return None
        if x == 0:
            return -mpmath.inf
        return mpmath.log(x) if fn == 'ln' else mpmath.log10(x)
    if fn == 'sin':
        return mpmath.sin(x)
    if fn == 'cos':
        return mpmath.cos(x)
    y = a[1]
    if y == 0:
        return mpmath.mpf(1)
    if x == 0:
        if y < 0:
            odd = y == int(y) and int(y) % 2 == 1
            return -mpmath.inf if odd and math.copysign(1, args[0]) < 0 else mpmath.inf
        return mpmath.mpf(0)
    if x < 0 and y != int(y):
        return None
    return mpmath.power(x, y)


def split(value):
    """`value` as a correctly rounded double and the rest, rounded."""
    if value is None:
        return None
    if mpmath.isinf(value):
        return (math.inf if value > 0 else -math.inf, 0.0, True)
    hi = nearest(value)
    if math.isinf(hi):
        return (hi, 0.0, False)
    rest = float((value - mpmath.mpf(hi)) / unit(hi))
    return (hi, rest, value == mpmath.mpf(hi))


def unit(hi):
    """The gap from `hi` to the next double away from nought."""
    magnitude = abs(hi)
    if magnitude < 2.2250738585072014e-308:
        return mpmath.mpf(2) ** -1074
    return mpmath.mpf(2) ** (math.frexp(magnitude)[1] - 53)


def nearest(value):
    """`value` rounded to a double, ties to even, subnormals rounded once.

    Written out rather than left to `float()`, which rounds to 53 bits first
    and so rounds a subnormal twice.
    """
    if value == 0:
        return 0.0
    magnitude = abs(value)
    e = int(mpmath.floor(mpmath.log(magnitude, 2)))
    # `log` can land a hair either side of a power of two.
    while mpmath.mpf(2) ** e > magnitude:
        e -= 1
    while mpmath.mpf(2) ** (e + 1) <= magnitude:
        e += 1
    unit = mpmath.mpf(2) ** max(e - 52, -1074)
    scaled = magnitude / unit
    n = int(mpmath.floor(scaled))
    rest = scaled - n
    if rest > 0.5 or (rest == 0.5 and n % 2 == 1):
        n += 1
    if n * unit > mpmath.mpf(2) ** 1024 - mpmath.mpf(2) ** 970:
        rounded = math.inf
    else:
        rounded = math.ldexp(n, max(e - 52, -1074))
    return rounded if value > 0 else -rounded


def main():
    seed, per = int(sys.argv[1]), int(sys.argv[2])
    print(f'# float_math_reference.py {seed} {per}: fn, arguments, exact value as hi and rest in ulps (bits), = when it is hi exactly')
    for row in inputs(seed, per):
        fn, args = row[0], row[1:]
        answer = split(exact(fn, args))
        shown = ' '.join('%016x' % bits(v) for v in args)
        if answer is None:
            print(f'{fn} {shown} nan')
        else:
            flag = '=' if answer[2] else '~'
            print(f'{fn} {shown} {bits(answer[0]):016x} {bits(answer[1]):016x} {flag}')


if __name__ == '__main__':
    main()
