# Cove

Cove is an experimental programming language for programs that run inside a
boundary a host controls.

A Cove program can only touch the outside world — the console, files, the
network, the clock — through capabilities the host grants it, and a run can be
bounded by fuel, deadlines and other limits. Inside that boundary it aims to be
an ordinary general-purpose language: familiar to read, quick to compile and
run, and explicit about what it depends on.

It is in the design and MVP stage. Syntax is provisional and may change.

## A taste

```cove
use console.println

/// Returns a greeting for `name`.
export fn greeting(name: String) -> String {
  "Hello, {name}!"
}

export fn main(args: Array<String>) -> Result<Unit, Error> {
  let name = args.get(0).unwrapOr("world")
  console.println(greeting(name))?
  Ok(())
}
```

What a program may use is declared next to how it is run, in `cove.toml`:

```toml
[run.hello]
entry = "hello.main"
allow = ["console"]
```

## Try it

In the browser, with nothing installed: the
[playground](https://myuon.github.io/cove/playground/).

Locally, with a Rust toolchain:

```console
$ cargo build --release -p cove-cli
$ cd examples
$ ../target/release/cove run hello -- Cove
Hello, Cove!
$ ../target/release/cove test
```

The `cove` command also checks (`cove check`), formats (`cove fmt`), tests
(`cove test`), packages a program as a standalone executable (`cove build`),
and records and replays the host calls a run made (`cove trace`,
`cove replay`). `cove help` lists everything.

## What exists

- **A compiler front end** — parser, name resolution and a type checker.
- **A VM** that runs programs by default.
- **A tree-walking interpreter** (`--backend ast`), kept as the reference the
  VM is checked against.
- **A native tier** (`--backend native`, x86-64 only, experimental) that
  compiles the functions it can to machine code and runs the rest on the VM.
- **A standard library written mostly in Cove**, and a Host API whose every
  operation is listed in [docs/BUILTINS.md](docs/BUILTINS.md).
- **Embedding**: a Rust host can run Cove with its own capability
  implementations and limits.

[examples/](examples/README.md) holds the representative programs the
language is measured against — a CSV query tool, a server, a formatter for
Cove written in Cove, and others.

## Documentation

- [Philosophy](docs/PHILOSOPHY.md) — what Cove is trying to be
- [Language Card](docs/LANGUAGE_CARD.md) — the language on one page
- [Language Reference](docs/LANGUAGE_REFERENCE.md) — the rules in full
- [Builtins and the Host API](docs/BUILTINS.md)
- [Architecture decisions](docs/adr/) — every design decision, why it was
  made, and what later changed it. This is where the project's history lives
- [API documentation](https://myuon.github.io/cove/) — rustdoc for the crates

## Name

A cove is a small, sheltered inlet: code that runs inside a boundary the host
provides, without the language feeling limited to sandboxed scripting.
