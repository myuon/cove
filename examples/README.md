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
| `places/` | Read-only retention, mutable places, snapshots, and explicit identity |
| `cove.toml` | Host-selected entry functions and granted capabilities |

Each directory is a module; declarations marked `export` form its public API.
The first implementation milestone should make `hello/` run. The MVP is
not complete until all six programs have defined behavior in both diagnostics
and execution.

## Place-model diagnostics

The valid `places/` program also fixes the expected behavior of nearby invalid
programs.

A read-only receiver cannot call a mutating method:

```cove
let booking = Booking(...)
booking.confirm()
// error: `Booking.confirm` requires a mutable receiver
```

A retained mutable place requires an explicit snapshot or identity:

```cove
var booking = Booking(...)
let view = BookingView.new(booking)
// error: `BookingView.new` retains mutable argument `booking`
// help: pass `booking.copy()` or change the API to accept `Ref<Booking>`
```

A temporary parameter cannot escape without a retention mode:

```cove
fn invalid(var self, booking: Booking) {
  self.saved = booking
  // error: parameter `booking` escapes
  // help: store `booking.copy()` or accept `Ref<Booking>`
}
```

These checks deliberately make temporary reads easy while making copies and
mutable aliasing visible.
