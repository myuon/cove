//! Which combinations of an important type and a boundary value a corpus
//! executed (issue 705's matrix; docs/FUZZ_EVALUATION.md, section 11).
//!
//! A **cell** is `family.op.class`: an operation the machine performs, and a
//! class of operand at which a wrong implementation of it is likely to answer
//! differently from a right one — `int.mul.overflow`, `float.cmp.nan`,
//! `string.byte_at.astral.index_len`, `array.load.index_len`. [`MATRIX`] is
//! the whole matrix, as data; [`classify`] says which cells one observation
//! falls in. [`crate::coverage`] observes, on its watched replay, every
//! instruction [`crate::coverage`]'s debugger stops at whose operands decide
//! a cell, and counts per cell how many programs reached it — anywhere, and
//! in a function the native tier compiled other than the entry.
//!
//! The cells are at the **instruction**, not at the source: `array.load.
//! index_len` is a `load-elem` whose index equals its object's header length,
//! which a source-level `xs[xs.length()]` reaches only if the lowering left
//! the bounds check to the instruction. A `String`'s positions are bytes, as
//! `byteAt` and `sliceBytes` count them.

/// The matrix: per family and operations, the classes each operation is
/// measured at. A cell is `family.op.class`; [`cells`] lists them all.
pub const MATRIX: &[(&str, &[&str], &[&str])] = &[
    (
        "int",
        &["add", "sub", "mul"],
        &["zero", "unit", "min", "max", "overflow"],
    ),
    (
        "int",
        &["div", "rem"],
        &["zero", "unit", "min", "overflow", "divisor_zero"],
    ),
    (
        "int",
        &["shl", "shr", "shru"],
        &[
            "negative_value",
            "count_0",
            "count_63",
            "count_out_of_range",
        ],
    ),
    ("int", &["cmp"], &["equal", "adjacent", "extreme"]),
    ("int", &["neg"], &["zero", "min"]),
    (
        "float",
        &["arith", "cmp", "to_int", "math"],
        &["nan", "pos_zero", "neg_zero", "inf", "subnormal"],
    ),
    ("float", &["to_int"], &["out_of_range"]),
    ("float", &["from_int"], &["inexact"]),
    (
        "string",
        &["byte_at"],
        &[
            "empty.index_0",
            "empty.index_len",
            "empty.index_past",
            "ascii.index_0",
            "ascii.index_len",
            "ascii.index_past",
            "multibyte.index_0",
            "multibyte.index_len",
            "multibyte.index_past",
            "astral.index_0",
            "astral.index_len",
            "astral.index_past",
            "combining.index_0",
            "combining.index_len",
            "combining.index_past",
        ],
    ),
    (
        "string",
        &["slice"],
        &[
            "empty.from_0",
            "empty.end_len",
            "empty.end_past",
            "ascii.from_0",
            "ascii.end_len",
            "ascii.end_past",
            "multibyte.from_0",
            "multibyte.end_len",
            "multibyte.end_past",
            "astral.from_0",
            "astral.end_len",
            "astral.end_past",
            "combining.from_0",
            "combining.end_len",
            "combining.end_past",
        ],
    ),
    (
        "array",
        &["load", "store"],
        &["negative", "index_0", "last", "index_len", "past"],
    ),
    ("array", &["len"], &["empty", "one", "large"]),
];

/// What one observed instruction did, with the operands the matrix reads.
#[derive(Clone, Debug, PartialEq)]
pub enum Observed<'a> {
    /// An `Int` operation: `op` is one of [`MATRIX`]'s `int` operations, and
    /// for `neg` only `a` is read.
    Int { op: &'static str, a: i64, b: i64 },
    /// A `Float` operation: `op` is one of `arith`, `cmp`, `to_int`, `math`,
    /// and `values` are its `Float` operands.
    Float { op: &'static str, values: Vec<f64> },
    /// `Int.toFloat` of this value.
    FromInt(i64),
    /// `text.byteAt(index)`.
    ByteAt { text: &'a str, index: i64 },
    /// The `count` bytes of `text` from `from`.
    Slice {
        text: &'a str,
        from: i64,
        count: i64,
    },
    /// A `load-elem` (`store` false) or `store-elem` at `index` of an object
    /// whose header length is `len`.
    Index { store: bool, index: i64, len: u64 },
    /// `len` of anything but a `String`.
    Len(u64),
}

/// At or past this many elements, a length is `large`.
pub const LARGE: u64 = 1024;

/// Every cell of [`MATRIX`], as `family.op.class`, in [`MATRIX`]'s order.
pub fn cells() -> Vec<String> {
    let mut out = Vec::new();
    for (family, ops, classes) in MATRIX {
        for op in *ops {
            for class in *classes {
                out.push(format!("{family}.{op}.{class}"));
            }
        }
    }
    out
}

/// The cells one observation falls in. An observation may fall in several —
/// `0 * i64::MAX` is `int.mul.zero` and `int.mul.max` — or in none.
pub fn classify(observed: &Observed<'_>) -> Vec<String> {
    let mut out = Vec::new();
    let mut put = |family: &str, op: &str, class: &str| out.push(format!("{family}.{op}.{class}"));
    match observed {
        Observed::Int { op, a, b } => {
            let (a, b) = (*a, *b);
            let wide = |x: i64| i128::from(x);
            match *op {
                "add" | "sub" | "mul" => {
                    let exact = match *op {
                        "add" => wide(a) + wide(b),
                        "sub" => wide(a) - wide(b),
                        _ => wide(a) * wide(b),
                    };
                    for (class, hit) in [
                        ("zero", a == 0 || b == 0),
                        ("unit", a.unsigned_abs() == 1 || b.unsigned_abs() == 1),
                        ("min", a == i64::MIN || b == i64::MIN),
                        ("max", a == i64::MAX || b == i64::MAX),
                        ("overflow", i64::try_from(exact).is_err()),
                    ] {
                        if hit {
                            put("int", op, class);
                        }
                    }
                }
                "div" | "rem" => {
                    for (class, hit) in [
                        ("zero", a == 0),
                        ("unit", b == 1 || b == -1),
                        ("min", a == i64::MIN),
                        ("overflow", a == i64::MIN && b == -1),
                        ("divisor_zero", b == 0),
                    ] {
                        if hit {
                            put("int", op, class);
                        }
                    }
                }
                "shl" | "shr" | "shru" => {
                    for (class, hit) in [
                        ("negative_value", a < 0),
                        ("count_0", b == 0),
                        ("count_63", b == 63),
                        ("count_out_of_range", !(0..=63).contains(&b)),
                    ] {
                        if hit {
                            put("int", op, class);
                        }
                    }
                }
                "cmp" => {
                    for (class, hit) in [
                        ("equal", a == b),
                        ("adjacent", (wide(a) - wide(b)).abs() == 1),
                        (
                            "extreme",
                            [a, b].iter().any(|x| *x == i64::MIN || *x == i64::MAX),
                        ),
                    ] {
                        if hit {
                            put("int", op, class);
                        }
                    }
                }
                "neg" => {
                    if a == 0 {
                        put("int", op, "zero");
                    }
                    if a == i64::MIN {
                        put("int", op, "min");
                    }
                }
                _ => {}
            }
        }
        Observed::Float { op, values } => {
            for class in ["nan", "pos_zero", "neg_zero", "inf", "subnormal"] {
                if values.iter().any(|v| float_class(*v) == Some(class)) {
                    put("float", op, class);
                }
            }
            // Finite and outside `[-2^63, 2^63)`: what `toInt` refuses besides
            // a NaN and an infinity.
            if *op == "to_int"
                && values.iter().any(|v| {
                    v.is_finite()
                        && (*v >= 9_223_372_036_854_775_808.0 || *v < -9_223_372_036_854_775_808.0)
                })
            {
                put("float", op, "out_of_range");
            }
        }
        Observed::FromInt(x) => {
            // `i64 as f64` rounds; the cast back is exact for every value it
            // can round to except 2^63, which saturates — and is inexact too.
            let back = *x as f64;
            if back >= 9_223_372_036_854_775_808.0 || back as i64 != *x {
                put("float", "from_int", "inexact");
            }
        }
        Observed::ByteAt { text, index } => {
            let len = text.len() as i64;
            for content in contents(text) {
                for (position, hit) in [
                    ("index_0", *index == 0),
                    ("index_len", *index == len),
                    ("index_past", *index > len),
                ] {
                    if hit {
                        put("string", "byte_at", &format!("{content}.{position}"));
                    }
                }
            }
        }
        Observed::Slice { text, from, count } => {
            let len = i128::from(text.len() as i64);
            let end = i128::from(*from) + i128::from(*count);
            for content in contents(text) {
                for (position, hit) in [
                    ("from_0", *from == 0),
                    ("end_len", end == len),
                    ("end_past", end > len),
                ] {
                    if hit {
                        put("string", "slice", &format!("{content}.{position}"));
                    }
                }
            }
        }
        Observed::Index { store, index, len } => {
            let op = if *store { "store" } else { "load" };
            let len = i128::from(*len);
            let index = i128::from(*index);
            for (class, hit) in [
                ("negative", index < 0),
                ("index_0", index == 0),
                ("last", len > 0 && index == len - 1),
                ("index_len", index == len),
                ("past", index > len),
            ] {
                if hit {
                    put("array", op, class);
                }
            }
        }
        Observed::Len(len) => {
            let class = match *len {
                0 => Some("empty"),
                1 => Some("one"),
                n if n >= LARGE => Some("large"),
                _ => None,
            };
            if let Some(class) = class {
                put("array", "len", class);
            }
        }
    }
    out
}

/// The class of a `Float` the matrix measures at, if it is one of them.
pub fn float_class(v: f64) -> Option<&'static str> {
    if v.is_nan() {
        Some("nan")
    } else if v == 0.0 {
        Some(if v.is_sign_negative() {
            "neg_zero"
        } else {
            "pos_zero"
        })
    } else if v.is_infinite() {
        Some("inf")
    } else if v.is_subnormal() {
        Some("subnormal")
    } else {
        None
    }
}

/// What a `String`'s text holds, as the matrix's content classes: `empty`,
/// or `ascii` for text that is all ASCII; otherwise each of `multibyte` (a
/// character of two or three bytes), `astral` (of four) and `combining` (a
/// combining mark) it holds.
pub fn contents(text: &str) -> Vec<&'static str> {
    if text.is_empty() {
        return vec!["empty"];
    }
    if text.is_ascii() {
        return vec!["ascii"];
    }
    let mut out = Vec::new();
    if text.chars().any(|c| (2..=3).contains(&c.len_utf8())) {
        out.push("multibyte");
    }
    if text.chars().any(|c| c.len_utf8() == 4) {
        out.push("astral");
    }
    if text.chars().any(is_combining) {
        out.push("combining");
    }
    out
}

/// The combining-mark blocks: diacritical marks, their extended and
/// supplement blocks, those for symbols, and the half marks.
fn is_combining(c: char) -> bool {
    matches!(
        u32::from(c),
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn all() -> BTreeSet<String> {
        cells().into_iter().collect()
    }

    fn of(o: Observed<'_>) -> BTreeSet<String> {
        let got: BTreeSet<String> = classify(&o).into_iter().collect();
        let matrix = all();
        for cell in &got {
            assert!(matrix.contains(cell), "{cell} is not a cell of the matrix");
        }
        got
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_matrix_has_no_duplicate_and_its_size_is_written_down() {
        let cells = cells();
        assert_eq!(cells.len(), all().len());
        assert_eq!(
            cells.len(),
            107,
            "docs/FUZZ_EVALUATION.md section 11 says 107"
        );
    }

    #[test]
    fn int_arithmetic_classes() {
        let int = |op, a, b| of(Observed::Int { op, a, b });
        assert_eq!(int("add", 3, 4), set(&[]));
        assert_eq!(
            int("add", i64::MAX, 1),
            set(&["int.add.max", "int.add.unit", "int.add.overflow"])
        );
        assert_eq!(
            int("sub", i64::MIN, 0),
            set(&["int.sub.min", "int.sub.zero"])
        );
        assert_eq!(
            int("sub", i64::MIN, 1),
            set(&["int.sub.min", "int.sub.unit", "int.sub.overflow"])
        );
        assert_eq!(int("mul", 1 << 31, 1 << 31), set(&[]));
        assert_eq!(int("mul", 1 << 32, 1 << 31), set(&["int.mul.overflow"]));
        assert_eq!(
            int("mul", -1, i64::MIN),
            set(&["int.mul.unit", "int.mul.min", "int.mul.overflow"])
        );
        assert_eq!(
            int("div", i64::MIN, -1),
            set(&["int.div.min", "int.div.unit", "int.div.overflow"])
        );
        assert_eq!(
            int("rem", 0, 0),
            set(&["int.rem.zero", "int.rem.divisor_zero"])
        );
        assert_eq!(int("rem", 7, 3), set(&[]));
    }

    #[test]
    fn shift_compare_and_negate_classes() {
        let int = |op, a, b| of(Observed::Int { op, a, b });
        assert_eq!(int("shl", 1, 0), set(&["int.shl.count_0"]));
        assert_eq!(
            int("shr", -8, 63),
            set(&["int.shr.negative_value", "int.shr.count_63"])
        );
        assert_eq!(int("shru", 1, 64), set(&["int.shru.count_out_of_range"]));
        assert_eq!(int("shru", 1, -1), set(&["int.shru.count_out_of_range"]));
        assert_eq!(int("cmp", 5, 5), set(&["int.cmp.equal"]));
        assert_eq!(int("cmp", 5, 6), set(&["int.cmp.adjacent"]));
        assert_eq!(int("cmp", i64::MIN, i64::MAX), set(&["int.cmp.extreme"]));
        assert_eq!(int("neg", i64::MIN, 0), set(&["int.neg.min"]));
        assert_eq!(int("neg", 0, 0), set(&["int.neg.zero"]));
    }

    #[test]
    fn float_classes() {
        let float = |op, values: &[f64]| {
            of(Observed::Float {
                op,
                values: values.to_vec(),
            })
        };
        assert_eq!(float("arith", &[1.5, 2.0]), set(&[]));
        assert_eq!(
            float("arith", &[f64::NAN, -0.0]),
            set(&["float.arith.nan", "float.arith.neg_zero"])
        );
        assert_eq!(
            float("cmp", &[0.0, f64::NEG_INFINITY]),
            set(&["float.cmp.pos_zero", "float.cmp.inf"])
        );
        assert_eq!(
            float("math", &[f64::MIN_POSITIVE / 2.0]),
            set(&["float.math.subnormal"])
        );
        assert_eq!(
            float("to_int", &[1e19]),
            set(&["float.to_int.out_of_range"])
        );
        assert_eq!(float("to_int", &[-9_223_372_036_854_775_808.0]), set(&[]));
        assert_eq!(
            float("to_int", &[-1e19]),
            set(&["float.to_int.out_of_range"])
        );
        assert_eq!(float("to_int", &[9.2e18]), set(&[]));
        assert_eq!(
            float("to_int", &[f64::INFINITY]),
            set(&["float.to_int.inf"])
        );
        assert_eq!(of(Observed::FromInt(1 << 53)), set(&[]));
        assert_eq!(
            of(Observed::FromInt((1 << 53) + 1)),
            set(&["float.from_int.inexact"])
        );
        assert_eq!(
            of(Observed::FromInt(i64::MAX)),
            set(&["float.from_int.inexact"])
        );
        assert_eq!(of(Observed::FromInt(i64::MIN)), set(&[]));
    }

    #[test]
    fn string_contents_and_positions() {
        assert_eq!(contents(""), vec!["empty"]);
        assert_eq!(contents("abc"), vec!["ascii"]);
        assert_eq!(contents("é"), vec!["multibyte"]);
        assert_eq!(contents("a😀"), vec!["astral"]);
        assert_eq!(contents("e\u{301}"), vec!["multibyte", "combining"]);
        assert_eq!(
            of(Observed::ByteAt { text: "", index: 0 }),
            set(&[
                "string.byte_at.empty.index_0",
                "string.byte_at.empty.index_len"
            ])
        );
        assert_eq!(
            of(Observed::ByteAt {
                text: "😀",
                index: 4
            }),
            set(&["string.byte_at.astral.index_len"])
        );
        assert_eq!(
            of(Observed::ByteAt {
                text: "ab",
                index: 3
            }),
            set(&["string.byte_at.ascii.index_past"])
        );
        assert_eq!(
            of(Observed::ByteAt {
                text: "ab",
                index: 1
            }),
            set(&[])
        );
        assert_eq!(
            of(Observed::Slice {
                text: "é!",
                from: 0,
                count: 3
            }),
            set(&[
                "string.slice.multibyte.from_0",
                "string.slice.multibyte.end_len"
            ])
        );
        assert_eq!(
            of(Observed::Slice {
                text: "ab",
                from: 1,
                count: 2
            }),
            set(&["string.slice.ascii.end_past"])
        );
    }

    #[test]
    fn index_and_length_classes() {
        let at = |store, index, len| of(Observed::Index { store, index, len });
        assert_eq!(at(false, -1, 3), set(&["array.load.negative"]));
        assert_eq!(at(false, 0, 3), set(&["array.load.index_0"]));
        assert_eq!(at(true, 2, 3), set(&["array.store.last"]));
        assert_eq!(at(true, 3, 3), set(&["array.store.index_len"]));
        assert_eq!(at(false, 4, 3), set(&["array.load.past"]));
        assert_eq!(
            at(false, 0, 0),
            set(&["array.load.index_0", "array.load.index_len"])
        );
        assert_eq!(of(Observed::Len(0)), set(&["array.len.empty"]));
        assert_eq!(of(Observed::Len(1)), set(&["array.len.one"]));
        assert_eq!(of(Observed::Len(2)), set(&[]));
        assert_eq!(of(Observed::Len(LARGE)), set(&["array.len.large"]));
    }
}
