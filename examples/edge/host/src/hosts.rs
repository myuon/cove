//! The four host modules a tenant may name, and what each of them does.
//!
//! Each module is one [`ModuleSchema`], read twice: [`crate::deploy`] hands
//! every schema to the checker, so a call into one is checked at its call site
//! and a function reaching one requires its capability, and each host below
//! answers [`HostApi::module_schema`] with the same value, so the boundary
//! holds the run to the same table.
//!
//! | module | capability | what it is |
//! | --- | --- | --- |
//! | `edge` | — | the `Request` and `Response` types; no operations |
//! | `kv` | `kv` | a key-value store, one per tenant, answered at once |
//! | `log` | `log` | a line on the server's standard output |
//! | `upstream` | `upstream` | a slow outbound call, answered **pending**: `get` a simulated service, `fetch` a real `http://` URL on the tenant's allowlist |

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::http::{parse_url, Url};
use cove_runtime::{
    Effect, FieldSchema, HostAnswer, HostApi, HostType, ModuleSchema, OperationSchema, Reentry,
    RuntimeError, TypeSchema, Value,
};

// ---------------------------------------------------------------- schemas

/// The handler contract: what a tenant's `handle` takes and answers.
///
/// A module with types and no operations, so naming it requires no
/// capability — a tenant that only builds a `Response` is pure.
pub const EDGE: ModuleSchema = ModuleSchema {
    name: "edge",
    capability: "edge",
    operations: &[],
    types: &[
        TypeSchema {
            name: "Request",
            cases: &[],
            fields: &[
                FieldSchema {
                    name: "method",
                    ty: HostType::String,
                },
                FieldSchema {
                    name: "path",
                    ty: HostType::String,
                },
                FieldSchema {
                    name: "query",
                    ty: HostType::Map(&HostType::String, &HostType::String),
                },
                FieldSchema {
                    name: "body",
                    ty: HostType::String,
                },
            ],
        },
        TypeSchema {
            name: "Response",
            cases: &[],
            fields: &[
                FieldSchema {
                    name: "status",
                    ty: HostType::Int,
                },
                FieldSchema {
                    name: "contentType",
                    ty: HostType::String,
                },
                FieldSchema {
                    name: "body",
                    ty: HostType::String,
                },
            ],
        },
    ],
    resources: &[],
};

/// A tenant's own key-value store.
pub const KV: ModuleSchema = ModuleSchema {
    name: "kv",
    capability: "kv",
    operations: &[
        OperationSchema {
            name: "get",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Option(&HostType::String),
            capability: "kv",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "put",
            params: &[HostType::String, HostType::String],
            variadic: false,
            result: HostType::Unit,
            capability: "kv",
            effect: Effect::ReversibleWrite,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[],
    resources: &[],
};

/// A line on the server's standard output, prefixed with the tenant's name.
pub const LOG: ModuleSchema = ModuleSchema {
    name: "log",
    capability: "log",
    operations: &[OperationSchema {
        name: "info",
        params: &[HostType::String],
        variadic: false,
        result: HostType::Unit,
        capability: "log",
        effect: Effect::IrreversibleWrite,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }],
    types: &[],
    resources: &[],
};

/// Slow outbound calls: `get` asks a simulated service by name, and `fetch`
/// performs a real HTTP `GET` of a URL the tenant's allowlist admits.
pub const UPSTREAM: ModuleSchema = ModuleSchema {
    name: "upstream",
    capability: "upstream",
    operations: &[
        OperationSchema {
            name: "get",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::String, &HostType::Error),
            capability: "upstream",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "fetch",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::String, &HostType::Error),
            capability: "upstream",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[],
    resources: &[],
};

/// Every module a tenant may name, in the order the server registers them.
pub const SCHEMAS: [ModuleSchema; 4] = [EDGE, KV, LOG, UPSTREAM];

// ------------------------------------------------------------------ hosts

/// `edge`: the types, and nothing to call.
pub struct Edge;

impl HostApi for Edge {
    fn module_schema(&self) -> ModuleSchema {
        EDGE
    }

    fn call(&self, op: &str, _args: Vec<Value>) -> Result<Value, RuntimeError> {
        unreachable!("`edge` declares no operation `{op}`")
    }
}

/// `kv`: a map behind a lock, shared by every run of one tenant and by no
/// run of any other.
pub struct Kv {
    pub store: Arc<Mutex<HashMap<String, String>>>,
}

impl HostApi for Kv {
    fn module_schema(&self) -> ModuleSchema {
        KV
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // The boundary held the operation, the arity and each argument to
        // `KV` before this was reached.
        let text = |at: usize| args[at].as_str().expect("checked by the boundary");
        let mut store = self.store.lock().unwrap();
        match op {
            "get" => Ok(match store.get(text(0)) {
                Some(found) => Value::some(Value::string(found.as_str())),
                None => Value::none(),
            }),
            "put" => {
                store.insert(text(0).to_string(), text(1).to_string());
                Ok(Value::unit())
            }
            other => unreachable!("`kv` declares no operation `{other}`"),
        }
    }
}

/// `log`: one line on standard output, unless the server was started quiet.
pub struct Log {
    pub tenant: String,
    pub quiet: bool,
}

impl HostApi for Log {
    fn module_schema(&self) -> ModuleSchema {
        LOG
    }

    fn call(&self, _op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        if !self.quiet {
            let line = args[0].as_str().expect("checked by the boundary");
            println!("[{}] {line}", self.tenant);
        }
        Ok(Value::unit())
    }
}

/// What `upstream` hands the embedder when it answers pending. The scheduler
/// downcasts [`cove_runtime::ParkedVm`]'s request to this.
#[derive(Debug)]
pub enum UpstreamCall {
    /// `upstream.get`: the simulated service asked for.
    Service(String),
    /// `upstream.fetch`: a URL the tenant's allowlist admitted.
    Fetch(Url),
}

/// How long the simulated upstream takes, chosen per call.
#[derive(Clone, Copy, Debug)]
pub struct Latency {
    pub min: Duration,
    pub max: Duration,
}

impl Latency {
    /// A latency in `min..=max`, from `seed`.
    ///
    /// Not random, and not meant to be: a splitmix step over a per-call
    /// counter spreads the calls over the range, and a test that wants a
    /// fixed latency sets `min == max`.
    pub fn pick(&self, seed: u64) -> Duration {
        let span = self.max.saturating_sub(self.min).as_micros() as u64;
        if span == 0 {
            return self.min;
        }
        let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        self.min + Duration::from_micros(z % (span + 1))
    }
}

/// What the simulated upstream service answers, wherever it is computed.
pub fn upstream_answer(service: &str, latency: Duration) -> Result<String, String> {
    let body = match service {
        "weather" => "sunny, 21C",
        "stocks" => "COVE +3.2%",
        "news" => "parked isolates resume on any thread",
        "" => return Err("no service named".to_string()),
        other if other.starts_with("fail") => return Err(format!("`{other}` is down")),
        _ => "ok",
    };
    Ok(format!("{body} [{} ms]", latency.as_millis()))
}

/// A service whose latency is its own rather than the server's `--latency`:
/// `hang` answers after an hour, which is an upstream that never answers as
/// far as any request can tell. It is what a tenant's deadline is for.
pub fn upstream_latency(service: &str) -> Option<Duration> {
    (service == "hang").then(|| Duration::from_secs(3600))
}

/// Blocking calls made, which seeds each one's latency.
static CALLS: AtomicU64 = AtomicU64::new(0);

/// `upstream`: pending wherever the run can park, and a blocking sleep or
/// fetch where it cannot.
pub struct Upstream {
    pub latency: Latency,
    /// Answer every call by sleeping on the worker, as a host that never
    /// heard of ADR 0080 would: the control the parked runs are measured
    /// against.
    pub blocking: bool,
    /// The tenant this registry is for, which a refusal names.
    pub tenant: String,
    /// The hosts `fetch` may reach, from `tenants/edge.toml`. Empty: none.
    ///
    /// This is the filtered implementation PHILOSOPHY's "No ambient
    /// authority" names. The `upstream` capability says a tenant may make
    /// outbound calls at all; the allowlist says to where, and it is held
    /// here, by the host, at the boundary — a URL the tenant builds at run
    /// time cannot get past it, whatever the checker saw.
    pub allow: Arc<BTreeSet<String>>,
}

impl Upstream {
    /// The URL `fetch` was asked for, if it parses and its host is on the
    /// allowlist; otherwise the `Err` the tenant's call answers with.
    fn admit(&self, text: &str) -> Result<Url, String> {
        let url = parse_url(text)?;
        if !self.allow.contains(&url.host) {
            return Err(format!(
                "`{}` is not on tenant `{}`'s fetch allowlist{}",
                url.host,
                self.tenant,
                if self.allow.is_empty() {
                    " (it has none; see tenants/edge.toml)".to_string()
                } else {
                    format!(
                        " ({})",
                        self.allow.iter().cloned().collect::<Vec<_>>().join(", ")
                    )
                }
            ));
        }
        Ok(url)
    }
}

/// What a fetch came to, as the `Result<String, Error>` `upstream.fetch`
/// answers: the body of a 2xx, and an `Err` that says what happened
/// otherwise.
pub fn fetch_answer(url: &str, fetched: Result<(u16, String), String>) -> Result<String, String> {
    match fetched {
        Ok((status, body)) if (200..300).contains(&status) => Ok(body),
        Ok((status, body)) => Err(format!(
            "`{url}` answered {status}: {}",
            body.lines().next().unwrap_or_default()
        )),
        Err(why) => Err(why),
    }
}

/// A `Result<String, Error>` as the value a host answers with.
pub fn result_value(result: Result<String, String>) -> Value {
    match result {
        Ok(text) => Value::ok(Value::string(text)),
        Err(message) => Value::err(Value::error(message)),
    }
}

impl HostApi for Upstream {
    fn module_schema(&self) -> ModuleSchema {
        UPSTREAM
    }

    /// The blocking answer, for a run that is not parkable at this call — a
    /// callback, a spawned task, a `lock` — which in these tenants is never.
    /// It is what the same call costs without ADR 0080: a worker thread
    /// asleep for the whole of the latency.
    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let service = args[0].as_str().expect("checked by the boundary");
        if op == "fetch" {
            return Ok(result_value(self.admit(service).and_then(|url| {
                fetch_answer(&url.to_string(), crate::http::fetch(&url, |_| true))
            })));
        }
        // `hang` sleeps its hour here too: a host that blocks its worker
        // cannot be timed out, which is the control's point.
        let latency = upstream_latency(service)
            .unwrap_or_else(|| self.latency.pick(CALLS.fetch_add(1, Ordering::Relaxed)));
        std::thread::sleep(latency);
        Ok(result_value(upstream_answer(service, latency)))
    }

    fn call_parkable(&self, op: &str, args: Vec<Value>, _back: &mut dyn Reentry) -> HostAnswer {
        if self.blocking {
            return HostAnswer::Ready(self.call(op, args));
        }
        let text = args[0].as_str().expect("checked by the boundary");
        if op == "fetch" {
            // Refused before anything is sent, and answered at once: a
            // refusal is the tenant's `Err` to handle, not a parked run.
            return match self.admit(text) {
                Ok(url) => HostAnswer::Pending(Box::new(UpstreamCall::Fetch(url))),
                Err(why) => HostAnswer::Ready(Ok(result_value(Err(why)))),
            };
        }
        HostAnswer::Pending(Box::new(UpstreamCall::Service(text.to_string())))
    }
}
