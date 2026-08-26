# Cove

## Architecture Decision Records

ADRs live in `docs/adr/`, numbered sequentially.

**An accepted ADR is immutable.** Once an ADR's status is `Accepted`, its
decision does not change. If the decision needs to change, write a new ADR
that supersedes it. Do not amend, reword, or extend the decision in place —
not to correct it, not to narrow it, not to record what a later change made
true.

The reason is that an ADR is a record of what was decided and why it was
decided *at the time*. Editing it destroys exactly the thing it exists to
preserve: a reader can no longer tell what the project believed when it
committed to a course, or what it learned that made it change course. A
superseding ADR keeps both, and the pair reads as a history.

This overrides the "Amendment (date): ..." sections found in older ADRs. That
convention is retired. Leave the existing ones where they are — removing them
would be the same mistake in the other direction — but do not add more.

### Superseding

A new ADR that replaces an older one:

- states `Supersedes: [ADR NNNN](NNNN-slug.md)` in its header;
- explains what changed and why the earlier decision no longer holds, not just
  what the new decision is.

The superseded ADR gets exactly one edit, to its header, and nothing else:

- its status becomes `Superseded by [ADR NNNN](NNNN-slug.md)`.

That pointer is the only permitted change to an accepted ADR, because without
it the new decision is unfindable from the old one. The body stays as written,
including the parts the new ADR contradicts.

Most supersession is partial: a broad ADR such as
[0001](docs/adr/0001-mvp-language-design.md) decides many things at once, and a
later ADR usually replaces one of them. Then the new ADR says
`Supersedes: [ADR NNNN](NNNN-slug.md)'s <named decision>`, and the older ADR's
header gains `Superseded in part by [ADR NNNN](NNNN-slug.md)`, naming which
decision. Its prose still stays untouched — including the sentence that is now
wrong. The pointer is what tells a reader to go find out how.

A new ADR that does not contradict an earlier one supersedes nothing. It
refers to the earlier ADR from its own Context, one way, and does not edit it.

### Numbering

Take the next free number. When two branches in flight both claim one, the
second to merge renumbers — which means moving the file *and* updating every
link to it, including back-links from other ADRs and from `README.md`.
