# ADR 0107: Percent-encoding and forms are `std.url`, and a form keeps every pair

- Status: Accepted
- Date: 2026-10-21
- Decides: that percent-encoding and the `application/x-www-form-urlencoded`
  form are a standard-library module, `std.url`, a program has only by
  importing it; that its interface is `percentEncode`, `percentDecode`,
  `parseForm` and `encodeQuery`, over two structs, `Pair` and `Form`; that
  encoding writes RFC 3986's unreserved set as itself and every other byte as
  `%` and two **uppercase** digits, so a space is `%20`; that `percentDecode`
  is strict and has no lossy twin, and `parseForm` is the WHATWG urlencoded
  parser and never fails; that **a form keeps every pair, in order**, with
  `get` answering the first value of a name and `all` every one; and that all
  of it is Cove, with no instruction of its own. Recorded for
  [issue #666](https://github.com/myuon/cove/issues/666)
- Supersedes: nothing
- Refers to, without superseding:
  [issue #646](https://github.com/myuon/cove/issues/646), items 3 and 4, the
  survey that found this code in five minicloud apps;
  [ADR 0106](0106-utf-8-over-bytes-and-int-to-text-in-a-radix-are-cove.md),
  whose `String.fromUtf8` and `fromUtf8Lossy` this reads bytes with;
  [ADR 0097](0097-std-json.md), the precedent for a library module a program
  imports and for refusing to drop a repeated name silently;
  [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s Decision 2
  and [ADR 0073](0073-a-primitive-below-the-library-is-an-instruction.md)'s
  Decision 2, which say where a new primitive would go; and
  [ADR 0062](0062-an-append-is-ensure-store-commit.md), the append the
  encoders write through

## Context

Issue #646 counted what five minicloud apps copy between them. Items 3 and 4
are this ADR.

**`urlDecode` and `parseForm` are in all five.**
`apps/admin/text/text.cove:5-39` and `:123-136` (minicloud `601dad0`), and
the same in algo, ledger, watch and webhooks. The host parses a query string before the app
sees it, but a POST body arrives as a `String`, so every app that reads a form
does the `+`-is-a-space, `%XX`-is-a-byte, read-as-UTF-8 pass itself — and
with it the `decodeUtf8` ADR 0106 replaced, which accepted overlong forms.

**`parseForm` loses a value without saying so.** It folds the pairs into a
`Map` with `inserted`, so for a name written twice the last value wins:
`tag=a&tag=b` reads as `tag=b`. That is the shape of the duplicate-key
problem ADR 0097 settled for JSON objects, and here, unlike in JSON, a
repeated name is not malformed — it is how a form sends a set of checkboxes
or a multiple `<select>`.

**`urlEncode` is in three, in two spellings.** Admin factors out
`unreserved(byte)` and `hexDigit(value)`; ledger and algo inline the range
test and share a `hexByte`. Both build a `Vector` of one-byte strings and join
it. Its callers — 17 of them — build query strings for links by hand around
it, `examples/ledger/comparing.cove:205` being two pairs written out as
`?a={text.urlEncode(a.id)}&amp;b={text.urlEncode(summary.id)}`.

What the callers do with a form, read from the code: `get(name)` and
`unwrapOr`, `contains(name)` for a checkbox (admin's `cap.*`, `confirm`,
`force`; webhooks' `masking`), and, in algo, a walk over every pair to lay
the form over the query. None of them wants a decode that can fail, and none
calls `urlDecode` except through `parseForm` and its tests.

@myuon decided on 2026-10-21 to add URL encoding and form decoding to std,
as a module `std.url` rather than methods on `String`, with uppercase hex, and
with a form that keeps every pair.

## Decision

### 1. The interface

```cove
use std.url

export struct Pair { name: String, value: String }
export struct Form { pairs: Array<Pair> }

form.get(name)       -> Option<String>   // the first value of `name`
form.all(name)       -> Array<String>    // every value of `name`, in order
form.contains(name)  -> Bool
form.pairs           -> Array<Pair>      // every pair, in order

url.percentEncode(text: String)        -> String
url.percentDecode(text: String)        -> Result<String, Error>
url.parseForm(body: String)            -> Form
url.encodeQuery(pairs: Array<Pair>)    -> String
```

A module and not methods on `String`, as the owner decided, and the
repository's conventions agree: `String`'s methods are what every program has
and every program lowers, while a URL is a format one program in several
reads, the way `std.json` is (ADR 0097). It is on
`cove_sema::stdlib`'s imported list, so a program that does not `use std.url`
lowers none of it, and the corpus surveys that count every program's
functions do not move for programs that never import it.

### 2. Encoding is RFC 3986's, with uppercase digits

`percentEncode` leaves exactly the unreserved set, `A-Z a-z 0-9 - . _ ~`, and
writes every other byte of the UTF-8 as `%XX`. Uppercase, because RFC 3986
§2.1 says producers *should* use it and it is what the copies wrote. One
encoder for every place a URL holds text: what it leaves alone is safe in a
path segment, a query name or value, and a form field, because `/`, `?`,
`&`, `=`, `+`, `#` and a space are all escaped.

The digits are written by a two-line `hexDigit` in `std.url` rather than
through ADR 0106's `Int.toHex`, which is lowercase — what JSON and every
language's own formatting write, and the right default there. Uppercasing its
answer would cost a `String` a byte and a second pass; the encoder instead
counts its answer's length first and writes every byte once into one buffer.
Text with nothing to escape is answered as itself.

### 3. `encodeQuery` writes `%20`, and is a form body too

`encodeQuery(pairs)` is `name=value` joined by `&`, each through
`percentEncode`: `a=1&b=x%20y`. The WHATWG form serializer
(`application/x-www-form-urlencoded`, URL Standard §5.2) writes a space as
`+` and leaves `*` unescaped. This deviates on both, on purpose: the callers
build links, and in a URL `%20` is a space to every reader while `+` is one
only to a form reader. Every form reader decodes `%20` and `%2A` the same as
the serializer's `+` and `*`, so the answer is a valid form body as well, and
`parseForm(encodeQuery(pairs)).pairs` is `pairs` again. There is one encoder
and not an `encodeForm` beside it, because the two would differ in one byte
and every reader accepts both.

### 4. `percentDecode` is strict, and has no lossy twin

`percentDecode` reads each `%XX` — either case — as the byte it names and the
bytes as UTF-8 through `String.fromUtf8`'s decoder. It answers `Err` for a
`%` not followed by two hexadecimal digits (`%`, `%4` at the end, `%G1`) and
for escapes whose bytes are not well-formed UTF-8 (`%C3` alone, the overlong
`%C0%80`), and each message names the byte offset in the *input*: the `%`
that is malformed, or the escape the ill-formed sequence starts at. The
decoder reports an offset into the decoded bytes; the refusal walks the input
again to translate it, which only the failing path pays. A `+` is a `+`:
reading it as a space is the form's rule, not percent-encoding's.

A lossy variant is not exported. The code this replaces never decodes a
percent-encoded value outside a form — `urlDecode` is called only by
`parseForm` and its tests — and a form is read by `parseForm`, which is lossy
already. A `percentDecodeLossy` would be a function with no caller; it is
easy to add when one appears, and "Earn complexity through use" says to wait.

### 5. `parseForm` is the WHATWG urlencoded parser, and cannot fail

`parseForm` transcribes the URL Standard's urlencoded parser (§5.1): split on
`&`, skip an empty piece, split each piece at its first `=` (none means an
empty value), read `+` as a space and `%XX` as a byte, and decode the bytes as
UTF-8 with replacement — `String.fromUtf8Lossy`, which replaces each maximal
subpart with one U+FFFD as the WHATWG decoder does. A `%` that starts no
escape is kept literally, the WHATWG percent-decoder's rule. So it never
fails, which is the parser's own rule and what a server wants: a browser's
form is read however it was written, and a field that came out wrong is a
value the program checks like any other.

The one deviation is the input's type: the standard parses bytes, and this
takes a `String`, which is already UTF-8, because that is what the host hands
a program as a body. A name or value with no `+` and no `%` is a slice of the
body rather than a copy.

### 6. A form keeps every pair, in order

`Form` is an ordered `Array<Pair>` and not a `Map`. `tag=a&tag=b` is two
pairs; `get("tag")` is `Some("a")`, the **first**, as `URLSearchParams.get`
answers; `all("tag")` is `["a", "b"]`; and `pairs` is every pair in the order
the text held them, which is the way to walk a form.

A `Map<String, Array<String>>`, which #646 suggested, keeps every value too,
but loses the order between names, and makes the common question — the one
value of a field — a `get` and then a `get(0)`. A list of pairs answers both,
is what the standard defines a form as, and is cheap at the sizes forms come
in: `get` is a linear scan over a few dozen pairs. Cove has no tuples, so a
pair is a struct, `Pair { name, value }`, named as the URL Standard names
them.

`Form` is not `opaque`. Any array of pairs is a form, so there is no
invariant for opacity to protect, and a test or a caller building a query can
write `Form(pairs: …)` or pass the array straight to `encodeQuery`.

`get` answering the first rather than the last changes what the copies did
for a name written twice. The copies' answer was an accident of
`Map.inserted`, nothing in the apps depends on it, and the first is what
every other reader of a form answers.

### 7. All of it is Cove

ADR 0073 gives a new primitive two homes: an instruction, for an operation of
the machine, or Cove in the standard library. Everything here is a rule about
text over what the substrate already reaches: `byteAt`, ADR 0062's ensure,
store and commit into `core.bytesAllocate` and `core.vectorWithCapacity`,
`sliceBytes`, and ADR 0106's decoder, which `percentDecode` calls directly
(`std.string.decodeInto`, a private function one library module may name in
another) so that it learns *where* the bytes went wrong and not only that
they did. No instruction is added, and the native tier compiles every
function of the module.

A rough measure, to say which class this is in and nothing finer: twenty
rounds of `parseForm`, `encodeQuery` and `percentDecode` over a 638,000-byte
body of 10,000 pairs, multibyte text and escapes in every value, run whole in
**5.7 s on the VM and 1.2 s on the native tier** on the machine this was
written on — about 150 and 31 ns a byte processed, setup included. Linear,
one pass over the input for each answer, which is the class ADR 0106's
decoder is in.

## Consequences

- The five minicloud copies of `urlDecode` and `parseForm` and the three of
  `urlEncode` can be replaced by `std.url`. A caller of `parseForm` that
  passes the result on as a `Map<String, String>` changes its parameter type
  to `url.Form`; `get` and `contains` read as they did. `algo`'s walk over
  `entry.key` and `entry.value` becomes one over `form.pairs`, `pair.name`
  and `pair.value`. A hand-built query string becomes `url.encodeQuery`.
- A form with a name written twice now answers the **first** value from
  `get`, where the copies answered the last; `all` answers both.
- `tests/e2e/values_std_url` pins every answer on the interpreter, the
  encoded VM and the native tier: round trips over a stride of the code
  space, multibyte text, `+` against `%2B`, repeated names, every malformed
  escape and ill-formed sequence with its offset, an empty body, and `=`
  inside a value.
- `crates/cove-cli/tests/copies.rs` rises by 26, all of it that program: the
  module adds nothing to a program that does not import it.
- **Not decided here**: parsing a whole URL (scheme, host, path), and
  `application/x-www-form-urlencoded` over a non-UTF-8 encoding. Neither has
  a caller.
