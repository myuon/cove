# ADR 0096: The formatter keeps the parentheses the source wrote

- Status: Accepted
- Date: 2026-10-14
- Supersedes nothing. No ADR decided that the formatter removes redundant
  parentheses; `cove_syntax::format` did so as an implementation choice,
  pinned only by its unit test `parenthesises_only_where_precedence_requires_it`.
  Refers to [ADR 0077](0077-cove-fmt-is-covefmt.md), which made covefmt
  `cove fmt` and kept the Rust formatter as its oracle
- Refers to: [issue #641](https://github.com/myuon/cove/issues/641), the
  diagnosis and the decision; [PR #640](https://github.com/myuon/cove/pull/640),
  where the disagreement was met
- Decides: that `cove fmt` keeps every pair of parentheses the source wrote,
  needed or not; that the oracle, `cove_syntax::format`, does the same, and
  still writes a pair precedence requires when the tree it prints has none;
  and that the parser records the written pairs as a count on the expression
  that only the formatter reads

## Context

Since ADR 0077 two formatters must agree on every file in the repository:
covefmt, which is `cove fmt`, and the Rust formatter it is judged against.
They did not agree about parentheses, and nothing had shown it.

covefmt works on tokens. It keeps every `(` and `)` it reads, and its own
correctness guarantee — the meaning check of #557, which refuses any output
whose significant tokens differ from its input's — holds it to that: dropping
a parenthesis is a token difference and is refused.

The Rust formatter works on the tree, and the parser threw parentheses away:
`(e)` became `e` with a wider span. So the printer wrote only the pairs
precedence required, and `(a && b) || (c && d)` came out as
`a && b || c && d`, `1 + (2 * 3)` as `1 + 2 * 3`.

The repository passed the agreement test only because every file in it had
been formatted by the Rust formatter, which had already taken the redundant
pairs out. PR #640 wrote a condition of the shape
`(c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || …` in
`tools/covefmt/covefmt/scan.cove`; covefmt kept it, the oracle rewrote it, and
the test failed. #640 rewrote the line around the disagreement.

Issue #641 named the two ways out:

- **(a)** teach covefmt to remove redundant parentheses: a token-level
  precedence analysis — with left associativity, so `1 - (2 - 3)` keeps its
  pair, and the rule that parenthesises a header ending in `}` — and an
  exemption in the meaning check, which today requires the significant tokens
  to match exactly;
- **(b)** make keeping them the intended behaviour and change the oracle.

@myuon decided (b) on 2026-10-14.

## Decision

1. **`cove fmt` keeps every pair of parentheses the source wrote.** Redundant
   or required, one pair or `((x))`, they come back. What a formatter does to
   the space *inside* them is layout and stays the formatter's: `( x )` is
   written `(x)`.
2. **The oracle keeps them too.** The parser records on each expression how
   many pairs were written around it (`Expr::parens`), and the Rust formatter
   writes them back, laid out exactly as it lays out a pair precedence
   requires, so that the two formatters break a long line in the same place.
3. **A pair precedence requires is still written when the tree has none**, and
   so is the pair around a `while`, `if`, `for` or `match` header that would
   otherwise end in a `}`. A tree built by hand, or with its pairs erased,
   prints as it did before. No pair is ever written that is neither written
   nor required.
4. **Only the formatter reads the count.** Parentheses carry no meaning the
   tree's shape does not already hold, so to name resolution, the type
   checker, the lowering and the interpreter a parenthesised expression is the
   expression; none of them looks at the field.

## Why

Removing redundant parentheses is arguably the more natural thing for a
formatter to do, and it is what the Rust formatter did. But a pair that
precedence does not need is not noise when an author wrote it on purpose:
`(a && b) || (c && d)` says how it is meant to be read, and a reader who does
not carry the precedence table in their head is helped by it. A formatter's
job is to make layout a non-question; whether to spell out a grouping is a
question about meaning for the reader, and it is the author's.

The cost of (a) is the other half of the argument:

- covefmt would need a precedence analysis over tokens it does not today
  parse as expressions, with associativity and the header rule, all of it
  agreeing with the Rust parser's tree in every case;
- its meaning check would have to stop asking for exactly the input's
  significant tokens and start allowing parentheses to vanish where that
  analysis says they may — the check that refuses a formatter bug would be
  taught to accept one class of change on the say-so of the code it checks.
  Keeping written parentheses leaves that check as strict as it was.

(b) costs a field on `Expr` and a few lines in the Rust printer.

## Consequences

- `tools/covefmt/covefmt/scan.cove` is back to the condition #640 meant to
  write, `!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'z') || …)`, so the
  repository holds the shape the agreement test failed on.
- covefmt's `written_parentheses_are_kept` holds the cases — redundant,
  nested, `(x)`, a parenthesised link of a method chain, `(await x)?` — and
  its body writes several as code, so the oracle reads them on every run.
  The Rust formatter's `keeps_the_parentheses_the_source_wrote`,
  `tightens_written_parentheses_and_invents_none` and
  `parenthesises_a_bare_tree_where_precedence_requires_it` replace the test
  that pinned the old behaviour.
- A parenthesised link ends a method chain for layout: `(a.b()).c().d()`
  breaks before the dots after the parentheses, not inside them. A
  parenthesised `await` keeps its pair: `(await x)?` is not rewritten to
  `await x?`, though the two parse to one tree.
- A parameter's `Display`, which renders its default in a signature (API
  snapshots, the outline), now shows the parentheses the source wrote around
  that default. No default in the repository has any.
- Existing files are unchanged: the Rust formatter had removed every
  redundant pair from them already, and nothing removes or adds one now.
