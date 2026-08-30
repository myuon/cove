# ADR 0025: A client is told what a server answered

- Status: Accepted
- Date: 2026-08-30
- Implemented by: [PR #169](https://github.com/myuon/cove/pull/169), closing
  [issue #145](https://github.com/myuon/cove/issues/145)
- Implementation status: complete — `http.fetch` is declared
  `fetch(String) -> Result<http.Response, Error>` in `cove_schema::hosts::HTTP`,
  the real host reads the status off the wire and the recorded fake answers a
  status per URL, the response a client will hold is bounded, and
  `examples/covecheck` names an expected status in its manifest.

## Context

`http.fetch` was declared `fetch(String) -> Result<String, Error>`, so the
whole of what a Cove client learned from a request was the body it answered
with, or a message. The real host *had* the status: `fetch_over_tcp` parsed
the status line, compared it against `200..300`, and turned anything else into
`Err("http: {url} answered {status}")` before the body reached the program.

`examples/covecheck` is a health and link checker written against that, and it
is the evidence. An expected status is the first thing such a manifest wants
to write down, and the program could not offer the field, so its `Outcome` has
an `Unchecked` case whose doc comment had to explain that it covers two
unrelated things: a connection that could not be made, and a `404` that was
made and refused. A run against `examples/server`, which routes only
`/health`, reported four endpoints identically to a run against nothing at
all:

```console
  unchecked   bookings                http: http://127.0.0.1:8080/bookings answered 404
  unchecked   metrics                 http: cannot connect to 127.0.0.1:8080: Connection refused
```

Both lines are prose the host wrote. A program that wanted to tell them apart
would have had to match on that prose, and a message is not an interface: it
has no type, nothing checks it, and the host may reword it.

There was a second, quieter asymmetry. `http`'s listener holds itself to fixed
bounds — 8 KiB of request line, 32 KiB of headers, 1 MiB of body — precisely so
a peer cannot make the host allocate on demand, and
[ADR 0018](0018-streaming-file-io.md) cites those bounds as the model when it
bounds `files.Reader.readLine`. The client had none: `fetch_over_tcp` called
`read_to_end` and held whatever arrived.

## Decision

### `fetch` answers the type a server sends

```text
http.fetch(String) -> Result<http.Response, Error>   requires http
```

`http.Response` already existed, with `status: Int` and `body: String`, as
what a route's handler returns and what `http.json` builds. It is now the type
of both halves of the module. A client and a server learn the same two facts
about a response, so declaring the client's separately would have been two
names for one shape, and a program that proxied one to the other would have
had to copy it field by field.

### A status is data, so an `Err` means no response arrived

Whatever status the peer sent is an `Ok`. `Err` is reserved for a run that
learned nothing: a URL this host will not send, a connection it could not
make, a read that ran out of time, or a response larger than it will hold.

This is the whole of what the issue asked for, and it is a distinction in the
*shape* of the answer rather than in its wording. A `404` and a refused
connection are different news — the first is a fact about a service that is
running, the second is the absence of any fact at all — and a program now
tells them apart by pattern-matching a `Result` it was already
pattern-matching.

The host is the wrong place to decide which statuses are failures. It was
deciding on behalf of every program that would ever call it, and `200..300` is
a policy rather than a protocol fact: a checker for an endpoint that is
*supposed* to answer `403` was, under the old rule, unable to observe success.

### A plain struct, not a resource handle

`http` issues resource handles already — `http.Server` answers `port` and
`close` — so "a response is a handle you ask questions of" was available, and
it is the shape a streaming body would need. It is rejected here on
[ADR 0013](0013-host-resource-handles.md)'s own line: a handle is for
something the host owns and can run out of, and something a program opens,
holds, and closes. A response that has already arrived in full is none of
those.

Making it one would have added a `close` nothing should call and a lifetime
nothing has, which is the argument
[ADR 0020](0020-a-diagnostic-stream-for-console.md) made against a
`console.Stream`. It would also have made the client's answer a different
shape from the server's, undoing the one type this decision is built on. A
body that is read once is a value; the day `http` can answer a body it has not
finished reading, that operation will be a different one, and a handle will be
right for *it*.

### The response the client holds is bounded, by a constant

A response over 1 MiB is an error rather than an allocation. It is the same
number as the listener's body bound, because it is the same promise read from
the other side: which end opened the connection does not change how much of
this process the other end may occupy. A URL in a manifest reaches whatever is
answering on that port today, so a server a program chose to fetch from is not
more trustworthy than a peer that connected to the listener.

The bound is counted over the bytes as they arrive, not checked against a
`Content-Length`, so what a peer claims never decides what this host
allocates.

It is a constant and not a parameter, for the reason ADR 0018 gives about
`readLine`: a caller has no way to say how large an answer it has not seen yet
should be, so a per-request bound would make every caller answer a question
about a response that has not arrived. A program that wants to reject a large
answer can measure the one it was given, which is what `covecheck`'s
`maxLength` does and what its README says that does and does not mean.

### One capability, and no new operation

`fetch` still requires `http` and is still the only client operation. Nothing
here is a new authority: a program that could reach a server could always
reach it, and learning what the server said back is less than it already had —
it had the body.

[ADR 0020](0020-a-diagnostic-stream-for-console.md)'s question, whether two
ways of doing one thing deserve two capabilities, does not arise, because
there is one way.

## What is not decided here

Issue #145 named three things worth deciding together, and the third is
answered by leaving it alone. **A per-request timeout is `clock.timeout`.**
Cove already has one way to bound any expression, `examples/covecheck` already
writes one around each fetch, and a `timeout` field on a request would be a
second bound with different semantics for one operation. The real host still
clamps its own wait by `READ_TIMEOUT` and by what the run's deadline leaves,
which is what makes a fetch unable to outlive its run.

That is a decision about the interface and not a claim about enforcement. A
`clock.timeout` on a real clock raises a cancellation flag that a *blocking*
socket read does not look at, so such a bound is reported when the read
returns rather than cutting it short — which is
[ADR 0024](0024-a-stop-is-a-bound-not-a-point.md)'s rule applied to a bound of
up to thirty seconds. Making `fetch` poll the way `http.Server.handle` already
polls for a connection would close that, and it is a separate change to a
separate concern.

**A method, headers, and a request body are not here either.** Each of them
needs a type describing what a *client* sends, and `http.Request` is the
server's: it names a path where a client names a URL. Deciding what a client's
request is, is a decision of its own size, and the status is what the program
that found this gap needed.

## Consequences

`http.fetch` changes what an existing program sees, and both callers in
`examples/` moved.

`examples/covecheck` gains an `expectStatus` field. A check that names one
expects that status exactly, which is how an endpoint that is supposed to
refuse gets checked at all; a check that names none expects any status from
200 to 299, which is what keeps every manifest written before this field
meaning what it meant, since such a check was only ever reached by a `2xx`.
The status is judged before the body, because the body of a wrong status is an
error page and every text condition would then report something true about
that page and nothing about the service. Its `Unchecked` outcome still exists
and still means what its name says; it no longer means two things at once.

`examples/tasks` takes each panel's body and refuses a panel whose endpoint
did not answer successfully, rather than rendering an error page as content.
It had no such choice before, because a non-2xx never reached it.

`Http::recorded` takes a status and a body per URL instead of a body, so a
test can drive a program's handling of a server that answered badly — which a
table of bodies could not express at all, since the only failure it could
produce was "no recorded answer", and that is the shape of a connection that
was never made.
