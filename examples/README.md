# Representative programs

These programs are executable design tests for Cove. The compiler does not
exist yet, so the syntax is provisional.

Together they test the core product hypotheses:

| Program | What it validates |
| --- | --- |
| `hello.cove` | Familiar syntax and ordinary CLI ergonomics |
| `config.cove` | `Option`, `Result`, typed configuration, and explicit errors |
| `server.cove` | A useful HTTP service without framework ceremony |
| `restricted.cove` | Host-provided capabilities and denied ambient authority |
| `tasks.cove` | Structured concurrency, cancellation, and trace boundaries |

The first implementation milestone should make `hello.cove` run. The MVP is
not complete until all five programs have defined behavior in both diagnostics
and execution.

