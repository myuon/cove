# ADR 0109: `std.web` owns `Request` and `Response`, and the handler plumbing over them

- Status: Accepted
- Date: 2026-10-21
- Decides: that HTTP for a handler is a standard-library module,
  **`std.web`**, that a program has only by importing it; that it **declares
  `Request` and `Response`**, with the fields minicloud's host types have,
  and an embedder's handler contract names `std.web.Request` and
  `std.web.Response` instead of types of its own; that an embedder builds
  and reads them with `Value::structure` and `Value::field` as it would a
  type its package declared, and nothing in `cove-runtime` or `cove-schema`
  changes for it; that a module imported under the name of a host module
  the same module uses is refused, `cove::resolve::import_conflict`; that the
  interface is `html`, `text`, `json`, `redirect`, `Response.withHeader`,
  `Request.header`, `Request.form`, `Request.sameOrigin` and
  `Request.prefix`, with a default set of security headers; that the mount
  prefix is a helper that reads `x-forwarded-prefix`, not a field; and that
  there are no status constants. Recorded for
  [issue #668](https://github.com/myuon/cove/issues/668)
- Supersedes: nothing
- Refers to, without superseding:
  [ADR 0107](0107-std-url.md), whose `parseForm` `Request.form` is, and
  [ADR 0108](0108-std-datetime.md), whose §2 found the collision §2 below
  refuses; [ADR 0097](0097-std-json.md) and
  [ADR 0101](0101-a-page-is-markup-with-an-expression-in-every-hole.md), the
  `Json` and `Html` the constructors take;
  [ADR 0005](0005-module-to-module-imports.md), the order a `use` resolves
  in, and its refusal of a module named after a host module;
  [ADR 0013](0013-host-resource-handles.md), whose amendment takes a host type at its word; and issue #150,
  which gave a host `Vm::invoke` with values it built

## Context

Issue #668 counted what the minicloud apps (minicloud `601dad0`) write
around the host's `web.Request` and `web.Response` — a host module schema
in `crates/minicloud/src/hosts.rs` with two types and no operations:

| helper | copies | where |
|---|---|---|
| `pageHeaders(contentType)` | 4 | `apps/admin/pages.cove`, `examples/{algo,ledger,webhooks}/pages.cove` |
| `htmlResponse` / `textResponse` | 4 | the same four |
| `jsonResponse` | 2 | ledger, webhooks |
| `redirect` (303, `no-store`) | 2 | admin, webhooks |
| `text(status, body)` | 3 | `examples/{hello,crunch,notes}` |
| `sameOrigin` | 3 | `apps/admin/admin.cove`, ledger, webhooks |
| `base()`, reading `x-forwarded-prefix` | 3 + 3 inline | admin, ledger, webhooks; algo, watch ×2 |

**The four `pageHeaders` are not the same policy.** Each sets
`x-content-type-options: nosniff` and `x-frame-options: DENY`. Beyond that:

| | `content-security-policy` | `referrer-policy` | `cache-control` |
|---|---|---|---|
| admin | `default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'` | `same-origin` | `no-store` |
| ledger | the same as admin | `same-origin` | **none** |
| webhooks | `default-src 'none'; style-src 'self'; script-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'` — a script, no images | `same-origin` | `no-store` |
| algo | `default-src 'none'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'` | **`no-referrer`** | `no-store` |

So they are one policy with each app's exceptions written into its copy,
which is the case for a default that an app overrides rather than for four
copies.

The `sameOrigin`s differ by one detail: webhooks compares `origin` against
`{scheme}://{host}` with `host` defaulting to `localhost`, admin and ledger
with `host` defaulting to `""`.

@myuon decided on 2026-10-21 to put this in std, and that **std.web defines
`Request` and `Response`**: the embedder's handler contract refers to std's
types rather than declaring its own. The other choice in the issue — std.web
generic over a protocol the host's types satisfy — needs bounds the language
does not have, and makes the shape an embedder's again.

## Decision

### 1. The types are the library's, and the embedding API already carries them

```cove
use std.web

export struct Request {
  method: String
  path: String
  query: Map<String, String>
  headers: Map<String, String>    // names lowercase
  body: String
}

export struct Response {
  status: Int
  headers: Map<String, String>
  body: String
}
```

The fields, their types and their order are minicloud's, so an app migrates
by changing `use web` to `use std.web`: `web.Request`, `web.Response`,
`request.headers.get(...)` and `web.Response(status: …, headers: …, body: …)`
all read as they did.

**An embedder needs nothing new to hand one over.** What crosses the
boundary into a handler is an argument to an exported function, and since
issue #150 that is `Vm::invoke` (or `Interpreter::invoke`) with a `Value`
the host built. `Value::structure(type_name, fields)` builds a value of any
declared struct by its qualified name, and a standard-library type is
qualified like any other: minicloud's `request_value` becomes

```rust
Value::structure("std.web.Request", [("method", …), ("path", …), ("query", …), ("headers", …), ("body", …)])
```

and its `response_of` is unchanged, because it reads `status`, `headers` and
`body` by name and a `std.web.Response` has those names. A host *schema*
names nothing: a handler is not a host operation, so no `HostType` or
`TypeSchema` is involved, and `cove-schema` does not need a way to name a
library type. That was checked rather than assumed —
`crates/cove-runtime/tests/embedding_web.rs` builds a request in Rust,
lowers the handler with `cove_ir::lower_entry` as an embedder that runs one
entry does, and reads the response, on both backends.

The one thing that changes for the embedder is **how closely the value is
checked**, and it is stricter. A host type was taken at its word about what
it held (ADR 0013's amendment, `crate::schema::Admits`); a declared struct is
followed into its fields, because the lowering reads one at a fixed word
offset (`cove-runtime/src/invoke.rs`, `admits`). So a `std.web.Request`
must carry exactly the five fields **in declaration order**, each of its
declared type, or `invoke` refuses it before the first instruction —
`` `app.handle` was given a `std.web.Request` carrying `method`, `path`,
`headers`, `body` as argument 1, but `std.web.Request` declares `method`,
`path`, `query`, `headers`, `body`, in that order``. minicloud already builds
them in that order. A value still named `web.Request` is refused naming the
type it should have been, which is the mistake a migration makes.

Not decided here: a host *operation* that takes or answers a library type —
a `fetch` host returning a `std.web.Response`, say. `HostType::Named` names a
host's own types; nothing needs more yet.

### 2. A library module and a host module of one name are refused together

ADR 0108 §2 found that `use std.time` beside a host `use time` is accepted
and then misresolves: a qualified call `time.f()` asks the host first, so
the library's functions are unreachable in that module, while a qualified
type `time.X` finds the library. `std.web` beside minicloud's host `web` is
the same case, and it is not avoidable by a name this time — `web` is the
name the module's value has, and the end state is that minicloud has no
host `web` at all.

So it is refused. When a module both imports a module whose last segment
is `q` (`use std.web`, `use app.web`) and uses a host module `q` (`use web`,
or `use web.op`, which also makes `web.` a host qualifier), resolution
reports

```text
error[cove::resolve::import_conflict]: `web` is imported from both the host module `web` and the module `std.web`
```

at the module's `use`, with the host's `use` labelled. It is
`import_conflict` because it is one: a `use` name that would mean two
things. `bind`, which raises that code for two `use`s of one name, never saw
it, because a host module's `use` binds no name of its own; and
`module_shadows_host` does not apply, because the module's name is `std.web`
and not `web`. The check is per module, as `host_uses` and `module_imports`
are — two modules of a package may each have one of them — and costs a walk
of the module's whole-module imports. Importing items of the library by name,
`use std.web.Request`, beside a host `use web` is not refused: those bind
their own names and `web.` is the host's alone.

During a migration, an app that still writes `use web` while the embedder
registers the host module keeps working, and one that writes `use std.web`
works whether or not the host module is still registered.

### 3. The interface

```cove
web.html(status: Int, page: Html) -> Response       // page.render(), the defaults
web.text(status: Int, body: String) -> Response     // text/plain; charset=utf-8, the defaults
web.json(status: Int, value: Json) -> Response      // json.render(value) + "\n", the defaults
web.redirect(location: String) -> Response          // 303, location, cache-control: no-store

response.withHeader(name: String, value: String) -> Response

request.header(name: String) -> Option<String>      // any case
request.form() -> url.Form                          // url.parseForm(request.body)
request.sameOrigin() -> Bool
request.prefix() -> String                          // x-forwarded-prefix, or ""
```

`json` takes a `std.json` value rather than a rendered `String`, as the issue
asked: the two copies' callers rendered just before calling. It writes the
newline after the document both copies wrote. `redirect` sets exactly what
the two copies did and nothing else; a `303` has no body to protect.

`withHeader` lowercases the name (ASCII) and replaces a header of that name,
so `withHeader("Content-Security-Policy", …)` replaces the default and the
map keeps one spelling of each. It is the one way to add or override, and it
reads as a chain on the constructor: admin's `401` is
`web.text(401, …).withHeader("www-authenticate", "Basic realm=…")`.

`header` is the case-insensitive lookup HTTP asks for, comparing ASCII
letters only — a Unicode case fold would match `"\u{212A}ey"`, with a KELVIN
SIGN, to `key`. A name the map holds lowercase is one `get`; any other is a
walk of the headers, so a request a test built with `Content-Type` is read
too.

The four request helpers are **methods**, where the issue's sketch had
`sameOrigin(request)`: they are all questions about one request, and one
spelling is easier to remember than two. `form` reads the body whatever the
method, as the copies did; the query is already `request.query`.

`sameOrigin` is the copies' rule: `sec-fetch-site` decides alone when it is
present (`same-origin` or `none`); otherwise `origin` is compared with
`{x-forwarded-proto or http}://{host}`; with neither header the request is
not from a browser and is admitted. It differs from webhooks' copy in one
case: an `origin` with no `host`, or an empty one, is refused rather than
compared with `localhost` or with `http://`.

### 4. The default headers, and why there is no script in them

`html`, `text` and `json` set the content type and:

```text
content-security-policy: default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'
x-content-type-options:  nosniff
x-frame-options:         DENY
referrer-policy:         same-origin
cache-control:           no-store
```

The policy is admin's and ledger's, the strictest of the four that still
serves a page with a stylesheet and an image of its own: **no script, inline
or from anywhere**. A default should be the policy an app has to loosen, not
one it has to remember to tighten, and two of the four apps run no script.
Webhooks and algo, which do, replace it with `withHeader`. Ledger, which set
no `cache-control`, gets `no-store`; a page that may be cached says so.

They are the same on every constructor, `json` and `text` included, because
the copies did that and a header that costs nothing on a non-page response
is not worth a second policy to describe.

### 5. The mount prefix is a helper that names the header

minicloud tells an app where it is mounted in `x-forwarded-prefix`,
overwriting whatever the client sent. That is a proxy convention — Spring's
`ForwardedHeaderFilter` and Traefik's `StripPrefix` use the same header —
and not HTTP. The two designs were a field the embedder fills, `request.base`
say, or a helper that reads the header.

A **helper**, `request.prefix()`, because a field changes the shape of
`Request`, and the shape is what makes a migration an import change (§1);
because every embedder that mounts apps under a path behind a proxy already
speaks the header; and because the helper's doc comment is where the
convention is written down, including that an embedder must replace the
client's value rather than pass it on. An embedder that mounts nothing sends
no header and the answer is `""`.

### 6. No status constants

The apps write `200`, `303`, `404` as numbers and nothing in the corpus
mistakes one. A table of names would be thirty declarations to save no
reader anything; `redirect` is the one status a constructor fixes.

### 7. All of it is Cove

`std.web` is on `cove_sema::stdlib`'s imported list beside `std.url` and
`std.datetime`, so a program that does not `use` it lowers none of it, and a
handler that answers only `web.text` lowers neither `std.url` nor `json`'s
renderer (`crates/cove-ir/src/lower/tests/imported.rs`). It binds nothing in
`cove_schema::builtins::STANDARD_LIBRARY`, and adds no instruction.

## Known limitations

- **A header name has one value.** `Map<String, String>` cannot hold two
  `set-cookie`s, so a response sets at most one cookie; joining them with a
  comma is not an answer, because a cookie's `Expires` holds one. On the way
  in, minicloud joins a repeated request header with `, `, which is right for
  every list-valued header and wrong for `cookie`, whose separator is `; `.
  The fix is a header type that keeps every value in order —
  `std.url.Form` is the precedent — or a `cookies` field beside `headers`.
  Either changes the shape of `Response`, which is why it is not done here
  with the shapes held to minicloud's; it is future work, and it is the one
  this module's first user with a login will want.
- **The body is a `String`.** A binary request or response body cannot be
  carried; minicloud answers static files outside the run for that reason.
- **The lowercase convention is the embedder's.** Nothing checks that the
  names an embedder hands over are lowercase. `header` reads either, but a
  program reading `request.headers.get(…)` directly relies on it.
- **`query` keeps the last of a repeated name**, as minicloud's host did,
  while `form()` keeps every pair. A request whose query repeats a name is
  read in full only through `url.parseForm` of the raw query, which
  `Request` does not carry.
- **No streaming.** An answer written as it goes, minicloud's `stream` host,
  is not a `Response` and is not covered.

## Consequences

- minicloud can delete its `web` host module. Its `request_value` names
  `std.web.Request` instead of `web.Request`, and `response_of` is
  unchanged. Each app replaces `use web` with `use std.web`; the copies of
  `pageHeaders`, `htmlResponse`, `textResponse`, `jsonResponse`,
  `redirect`, `text`, `sameOrigin` and `base` go, webhooks and algo add a
  `withHeader` for their script policy, and algo one for `no-referrer`.
  `jsonResponse(status, body)` callers pass the `Json` they were rendering.
  An app that writes both `use web` and `use std.web` during the change is
  told so (§2).
- `tests/e2e/values_std_web` pins every answer on the interpreter, the
  encoded VM and the native tier; `fail_std_web_beside_host_web` pins the
  refusal; `crates/cove-runtime/tests/embedding_web.rs` pins the embedding,
  and `cove_sema::resolve`'s tests the refusal with a package module.
- `crates/cove-cli/tests/copies.rs` rises by 52, all of it the one new
  program.
- **Not decided here**: a header type with repeated values, cookies,
  a binary body, and host operations over library types.
