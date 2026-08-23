# Representative programs

These programs are executable design tests for Cove. The compiler does not
exist yet, so the syntax is provisional.

Together they test the core product hypotheses:

| Program | What it validates |
| --- | --- |
| `hello/` | Familiar syntax and ordinary CLI ergonomics |
| `config/` | `Option`, `Result`, typed configuration, and explicit errors |
| `server/` | A useful HTTP service without framework ceremony |
| `restricted/` | Host-provided capabilities and denied ambient authority |
| `tasks/` | Structured concurrency, cancellation, and trace boundaries |
| `cove.toml` | Host-selected entry functions and granted capabilities |

Each directory is a module; declarations marked `export` form its public API.
The first implementation milestone should make `hello/` run. The MVP is
not complete until all five programs have defined behavior in both diagnostics
and execution.
