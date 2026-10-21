# ADR 0111: `web.respond`, a response of any content type

- Status: Accepted
- Date: 2026-10-21
- Decides: that `std.web` has `respond(status: Int, contentType: String,
  body: String) -> Response`, a response with `content-type: <contentType>`
  and no other header and the body written as given, with nothing appended;
  that `html`, `text` and `json` are written over it and answer the same
  bytes they did; and that there is no JSON-specific variant beside it — no
  `jsonText`, no `jsonPretty`, no `pretty` flag on `json`. Recorded for
  [issue #673](https://github.com/myuon/cove/issues/673)
- Supersedes: nothing
- Refers to, without superseding:
  [ADR 0109](0109-std-web.md), whose constructors this generalises and whose
  §4 rule — a constructor sets only the content type — it keeps

## Context

ADR 0109 gave `std.web` three typed constructors, `html`, `text` and `json`,
and expected that `jsonResponse(status, body)` callers would pass the `Json`
they were rendering. After the minicloud apps moved onto `std.web`
(minicloud #155), two of them answer JSON that `web.json` cannot write:
`examples/ledger` has the document already rendered, as text, and has no
`Json` to pass; `examples/webhooks` renders a page people read with
`json.renderPretty`. Each kept a helper that builds
`web.text(status, "{body}\n")` and then overwrites its content type with
`withHeader("content-type", "application/json")` — a `text/plain` response
relabelled, spelling out the header `std.web` exists to own.

Issue #673 weighed three shapes: `web.jsonText(status, body)` for rendered
JSON, `web.json(status, value, pretty)` or `web.jsonPretty`, and a general
`web.respond(status, contentType, body)`. @myuon decided on 2026-10-21 for the
general one: the smallest surface, and it covers CSV and `text/event-stream`
as well as both JSON cases, at the price of naming the type at the call.

## Decision

```cove
web.respond(status: Int, contentType: String, body: String) -> Response
```

- **One header, `content-type`**, set to `contentType` as given, and no other:
  ADR 0109's §4 holds of it exactly as of the typed constructors, so policy
  headers are still the app's, added with `withHeader` and `withHeaders`.
- **The body as given.** Unlike `json`, no newline is added: a caller that
  writes its own body writes its own last byte, and an event stream or a CSV
  has framing of its own that an appended newline would change.
- **`html`, `text` and `json` are `respond` with their type.** `html` is
  `respond(status, "text/html; charset=utf-8", page.render())`, `text` the
  same with `text/plain; charset=utf-8`, and `json`
  `respond(status, "application/json", "{render(value)}\n")`. Their answers
  are the same values, header map and body byte for byte, as before.
- **No JSON variant.** The two call sites are
  `web.respond(status, "application/json", "{body}\n")` and
  `web.respond(status, "application/json", "{json.renderPretty(v)}\n")`, one
  call each. A named variant would read better there and be one more
  function for each further rendering; it can be added when the calls show
  recurring friction.

`contentType` is not checked. A `Map` value set by `withHeader` is not checked
either, and what an embedder does with a header value that is not one is the
embedder's question, as it was before.

## Consequences

- minicloud's `renderedJson` and `prettyJson` become one `web.respond` call
  each, and no app relabels a `text/plain` response.
- `std.web` stays on the imported list, so a program that does not `use` it
  lowers none of this. `crates/cove-cli/tests/copies.rs` falls by 1: the
  three typed constructors built a `Response` to return apiece and now share
  `respond`'s one, −2, and the cases `tests/e2e/values_std_web` gained are
  +1.
- `values_std_web` now also pins `respond` on the interpreter, the encoded VM
  and the native tier — pretty and already-rendered JSON, CSV, an event
  stream, an empty type — and that each typed constructor equals `respond`
  with its type. Its earlier lines are unchanged byte for byte.
