//! Deploying a tenant: compile once, check its authority, prepare once.
//!
//! Every tenant is compiled as a package of its own — its one module and the
//! standard library — so a tenant cannot `use` another tenant's code, and
//! what the checker derives about its entry is about its code alone. What is
//! paid here is paid once per tenant for the life of the server: parsing,
//! checking, lowering, and [`PreparedProgram::new`]'s encoding and
//! verification. A request pays for an [`OwnedVm`] and nothing above it.
//!
//! The capability check is the one thing a deploy can refuse for. The checker
//! derives, per function, the capabilities its call graph requires
//! (`FnEntry::required_capabilities`), and `cove.toml`'s `allow` is what this
//! server grants. A tenant whose entry requires more than it was granted is
//! not deployed, and the server says which capability and why, before any
//! request reaches it. The runtime would refuse the call anyway — the grant
//! is enforced at the boundary — but a refusal at deploy is a refusal that
//! does not wait for the one request that takes the rare branch.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cove_diag::{render, Diagnostic, Severity, SourceMap};
use cove_runtime::{Grants, HostRegistry, Limits, OwnedVm, PreparedProgram, Runtime, Value};
use cove_sema::resolve::Program;
use cove_sema::{Compiler, Config, HostSchemas, RunConfig};

use crate::hosts::{Edge, Kv, Latency, Log, Upstream, SCHEMAS};

/// How the server is asked to deploy its tenants.
#[derive(Clone, Debug)]
pub struct DeployOptions {
    /// The directory holding `cove.toml` and one directory per tenant.
    pub tenants: PathBuf,
    /// How long the simulated `upstream` takes.
    pub latency: Latency,
    /// Whether `log.info` prints nothing.
    pub quiet: bool,
    /// Whether `upstream.get` blocks its worker instead of parking the run.
    pub blocking_upstream: bool,
    /// Which tier a request's isolate runs on.
    pub backend: Backend,
}

/// Which tier the isolates run on: `--backend vm|native`.
///
/// `Native` compiles each tenant's program once, at deploy, with
/// [`PreparedProgram::with_native`], and every isolate of that tenant shares
/// the machine code. A run on it still parks at `upstream.get` and still
/// yields at a safepoint when its slice is up — inside compiled code too
/// (ADR 0085) — so the scheduler is the same one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    /// The encoded VM, which every build and host has.
    #[default]
    Vm,
    /// The native tier, where this build has the code generator (`--features
    /// native`) and this host runs it (Unix x86-64).
    Native,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::Vm => "vm",
            Backend::Native => "native",
        })
    }
}

/// What a request's run is bounded by when `cove.toml` says nothing.
///
/// Fuel because a tenant is somebody else's code and can loop; a deadline
/// because a parked run is still a run, and one whose upstream never answers
/// should not hold its socket forever.
pub fn default_limits() -> Limits {
    Limits {
        fuel: Some(50_000_000),
        deadline: Some(Duration::from_secs(10)),
        max_host_calls: Some(1_000),
        ..Limits::default()
    }
}

/// One tenant, deployed or refused.
pub struct Tenant {
    /// The name that routes to it: `/hello/...` reaches `hello`.
    pub name: String,
    /// `module.function`, from `cove.toml`'s `entry`.
    pub entry: String,
    /// What `cove.toml` grants.
    pub granted: BTreeSet<String>,
    /// The hosts `upstream.fetch` may reach, from `edge.toml`.
    pub fetch: BTreeSet<String>,
    /// What the checker derived the entry requires, when it checked.
    pub required: BTreeSet<String>,
    /// Whether `required` is a lower bound: the entry makes a call the call
    /// graph cannot follow.
    pub open: bool,
    /// What every request's run is bounded by.
    pub limits: Limits,
    /// Running, or why not.
    pub state: State,
}

/// Whether a tenant serves.
pub enum State {
    /// Compiled, checked, granted, and prepared.
    Deployed(Box<Deployed>),
    /// Not deployed, and the reason, which the server prints at startup and
    /// answers every request to the tenant with.
    Refused(String),
}

/// Everything a request's isolate is built from, shared by all of them.
pub struct Deployed {
    pub module: String,
    pub function: String,
    /// The sources, for rendering a runtime error a response carries.
    pub sources: Arc<SourceMap>,
    /// The tenant's host registry — its grants, and the four hosts with the
    /// `kv` store behind them — built once and shared by every run.
    ///
    /// One registry for every request in flight, which a per-run budget makes
    /// sound: a run's budget travels with the run and a host call is charged
    /// to the run that made it, never to the registry (issue #577).
    pub hosts: Arc<HostRegistry>,
    /// The run-wide state over the checked program, shared by every run.
    pub runtime: Arc<Runtime>,
    /// The lowered program, encoded and verified once (#570).
    pub prepared: PreparedProgram,
    /// What deploying cost, once.
    pub cost: DeployCost,
}

/// What deploying one tenant cost.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeployCost {
    /// Reading, parsing and checking the tenant and the standard library.
    pub check: Duration,
    /// Lowering the entry, and [`PreparedProgram::new`] — and, on the native
    /// backend, compiling it.
    pub prepare: Duration,
    /// How many functions the entry reached.
    pub functions: usize,
    /// Building one isolate — its [`OwnedVm`], over the tenant's shared
    /// registry, `Runtime` and prepared program — which is what every request
    /// pays. The mean of a hundred.
    pub isolate: Duration,
}

impl Deployed {
    /// A fresh isolate: a run of the tenant's prepared program, with its own
    /// heap, stack and budget and nothing of any other run's.
    ///
    /// The registry and the `Runtime` are the tenant's, shared with every
    /// other request in flight. They used to be built per isolate, because a
    /// per-invocation budget was installed in the registry and concurrent
    /// runs charged one another's host calls to it (issue #577); a budget is
    /// the run's now, so what an isolate costs is the `OwnedVm` alone.
    pub fn isolate(&self) -> OwnedVm {
        OwnedVm::new(
            Arc::clone(&self.runtime),
            Arc::clone(&self.hosts),
            self.prepared.clone(),
        )
    }
}

impl Tenant {
    /// The one-line account the server prints at startup.
    pub fn describe(&self) -> String {
        let list = |set: &BTreeSet<String>| {
            if set.is_empty() {
                "-".to_string()
            } else {
                set.iter().cloned().collect::<Vec<_>>().join(", ")
            }
        };
        let open = if self.open { " (lower bound)" } else { "" };
        let verdict = match &self.state {
            State::Deployed(deployed) => {
                let unused: Vec<_> = self.granted.difference(&self.required).cloned().collect();
                let unused = if unused.is_empty() || self.open {
                    String::new()
                } else {
                    format!("; granted but unused: {}", unused.join(", "))
                };
                format!(
                    "deployed: {} fn, checked in {:.1} ms, prepared in {:.1} ms, isolate {:.0} us{unused}",
                    deployed.cost.functions,
                    deployed.cost.check.as_secs_f64() * 1e3,
                    deployed.cost.prepare.as_secs_f64() * 1e3,
                    deployed.cost.isolate.as_secs_f64() * 1e6,
                )
            }
            State::Refused(why) => format!("REFUSED: {why}"),
        };
        format!(
            "{:<10} requires [{}]{open}  granted [{}]{}  {verdict}",
            self.name,
            list(&self.required),
            list(&self.granted),
            self.fetch_note(),
        )
    }

    /// `  fetch [hosts]` for a tenant with a fetch allowlist, and nothing for
    /// one without.
    pub fn fetch_note(&self) -> String {
        if self.fetch.is_empty() {
            String::new()
        } else {
            format!(
                "  fetch [{}]",
                self.fetch.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        }
    }

    /// The deployed half, if there is one.
    pub fn deployed(&self) -> Option<&Deployed> {
        match &self.state {
            State::Deployed(deployed) => Some(deployed),
            State::Refused(_) => None,
        }
    }
}

/// Reads `cove.toml` from the tenants directory: one `[run.<tenant>]` table
/// per tenant.
pub fn read_manifest(root: &Path) -> Result<Config, String> {
    let manifest = root.join("cove.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("cannot read `{}`: {e}", manifest.display()))?;
    cove_sema::config::parse(&text).map_err(|e| format!("`{}`: {e}", manifest.display()))
}

/// The server's own policy for each tenant, beyond what `cove.toml` grants:
/// `tenants/edge.toml`.
///
/// A file of its own because `cove.toml` cannot carry it: `cove_sema`'s
/// parser rejects a key it does not know in a `[run.<name>]` table, which is
/// right for `cove run` and leaves an embedder nowhere to put a key of its
/// own (README, "what was awkward").
///
/// ```toml
/// [proxy]
/// fetch = ["127.0.0.1", "localhost"]   # hosts `upstream.fetch` may reach
/// ```
#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// Per tenant, the hosts `upstream.fetch` may reach; a tenant not named
    /// may reach none.
    pub fetch: BTreeMap<String, BTreeSet<String>>,
}

/// Reads `edge.toml` from the tenants directory; no file is no policy.
///
/// A tenant `cove.toml` does not name, or a key this server does not know,
/// is refused rather than skipped: a misspelt allowlist would otherwise be a
/// tenant silently allowed nothing, or a typo that looks like a grant.
pub fn read_policy(root: &Path, config: &Config) -> Result<Policy, String> {
    let path = root.join("edge.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Policy::default());
    };
    let shown = path.display();
    let table: toml::Table = text.parse().map_err(|e| format!("`{shown}`: {e}"))?;
    let mut policy = Policy::default();
    for (tenant, value) in table {
        if !config.runs.contains_key(&tenant) {
            return Err(format!(
                "`{shown}` names `{tenant}`, which cove.toml does not"
            ));
        }
        let toml::Value::Table(keys) = value else {
            return Err(format!("`{shown}`: `{tenant}` is not a table"));
        };
        for (key, value) in keys {
            match (key.as_str(), value) {
                ("fetch", toml::Value::Array(hosts)) => {
                    let hosts = hosts
                        .into_iter()
                        .map(|host| match host {
                            toml::Value::String(host) => Ok(host.to_ascii_lowercase()),
                            other => Err(format!(
                                "`{shown}`: `{tenant}.fetch` holds `{other:?}`, not a host name"
                            )),
                        })
                        .collect::<Result<_, _>>()?;
                    policy.fetch.insert(tenant.clone(), hosts);
                }
                (key, _) => {
                    return Err(format!("`{shown}`: unknown key `{tenant}.{key}`"));
                }
            }
        }
    }
    Ok(policy)
}

/// Deploys every tenant `cove.toml` names, refusing the ones that do not
/// compile or that require more than they are granted.
///
/// A refusal is per tenant: the others deploy. The error is only for a
/// `cove.toml` or an `edge.toml` that cannot be read at all.
pub fn deploy_all(options: &DeployOptions) -> Result<Vec<Tenant>, String> {
    let config = read_manifest(&options.tenants)?;
    let policy = read_policy(&options.tenants, &config)?;
    Ok(config
        .runs
        .iter()
        .map(|(name, run)| deploy(options, describe(name, run, &policy)))
        .collect())
}

/// What a request's run is bounded by: `cove.toml`'s figures, and
/// [`default_limits`] where it says nothing.
pub fn limits_of(run: &RunConfig) -> Limits {
    let defaults = default_limits();
    Limits {
        fuel: run.fuel.or(defaults.fuel),
        deadline: run.deadline.or(defaults.deadline),
        max_host_calls: run.max_host_calls.or(defaults.max_host_calls),
        max_tasks: run.max_tasks.or(Some(8)),
        ..defaults
    }
}

/// A tenant as `cove.toml` and `edge.toml` describe it, before anything is
/// compiled.
pub fn describe(name: &str, run: &RunConfig, policy: &Policy) -> Tenant {
    Tenant {
        name: name.to_string(),
        entry: run.entry.clone(),
        // Exactly what `cove.toml` grants. `edge` is not among it: building
        // an `edge.Response` initializes a type the schema declares, which
        // requires no capability, so a tenant that only answers is pure.
        granted: run.allow.iter().cloned().collect(),
        fetch: policy.fetch.get(name).cloned().unwrap_or_default(),
        required: BTreeSet::new(),
        open: false,
        limits: limits_of(run),
        state: State::Refused(String::new()),
    }
}

/// Deploys one tenant.
fn deploy(options: &DeployOptions, mut tenant: Tenant) -> Tenant {
    tenant.state = match prepare(options, &mut tenant) {
        Ok(deployed) => State::Deployed(Box::new(deployed)),
        Err(why) => State::Refused(why),
    };
    tenant
}

/// A tenant's module, checked against the server's schemas.
pub struct Compiled {
    /// The tenant's files and the standard library's.
    pub sources: SourceMap,
    /// The checked program, its notices included.
    pub program: Program,
    /// Reading, parsing and checking.
    pub check: Duration,
}

/// Loads and checks `module` against the server's own schemas — the values
/// the hosts answer [`cove_runtime::HostApi::module_schema`] with, not a copy
/// of them.
///
/// The module is the `.cove` files directly in `tenants/<module>/`, loaded
/// with the standard library as a package of its own by
/// [`cove_sema::package::load_module`]: nothing beside it is read, so no
/// tenant's package holds another's code.
///
/// The `Err` is rendered, after the stage that refused it: `does not load`
/// or `does not check`, then the diagnostics as `cove check` renders them.
pub fn compile(root: &Path, module: &str) -> Result<Compiled, String> {
    let started = Instant::now();
    let mut sources = SourceMap::new();
    let package = cove_sema::package::load_module(root, module, &mut sources)
        .map_err(|items| format!("does not load:\n{}", report(&sources, &items)))?;
    let program = Compiler::new()
        .with_schemas(HostSchemas::only(SCHEMAS))
        .compile(&package)
        .map_err(|items| format!("does not check:\n{}", report(&sources, &items)))?;
    Ok(Compiled {
        sources,
        program,
        check: started.elapsed(),
    })
}

/// Whether a checked tenant may be deployed: it checks without warnings, it
/// declares its entry, and the entry requires nothing `cove.toml` does not
/// grant. Fills in what the entry requires.
///
/// The one decision the server and `cove-edge check` share, so that the
/// checker cannot pass a tenant the server would refuse.
pub fn admit(tenant: &mut Tenant, compiled: &Compiled) -> Result<(), String> {
    let Some((module, function)) = tenant.entry.split_once('.') else {
        return Err(format!("entry `{}` is not `module.function`", tenant.entry));
    };
    let warnings: Vec<Diagnostic> = compiled
        .program
        .notices
        .iter()
        .filter(|item| item.severity == Severity::Warning)
        .cloned()
        .collect();
    if !warnings.is_empty() {
        return Err(format!(
            "checks with warnings:\n{}",
            report(&compiled.sources, &warnings)
        ));
    }
    let Some(entry) = compiled.program.lookup_fn(module, function) else {
        return Err(format!("`{module}` declares no `{function}`"));
    };
    tenant.required = entry
        .required_capabilities
        .iter()
        .map(|capability| capability.as_str().to_string())
        .collect();
    tenant.open = entry.is_capability_open();
    let missing: Vec<String> = tenant
        .required
        .difference(&tenant.granted)
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "`{}` requires {}, which cove.toml does not grant",
            tenant.entry,
            missing
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(" and ")
        ));
    }
    Ok(())
}

/// The tenant's host registry: its grants, and the four hosts, with a `kv`
/// store of its own that starts empty.
///
/// The server builds one per tenant, at deploy, and shares it with every
/// request; `cove-edge test` builds one per test, so that no test sees what
/// another left in the store.
pub fn registry(tenant: &Tenant, options: &DeployOptions) -> HostRegistry {
    let mut hosts = HostRegistry::new(Grants::new(tenant.granted.iter().cloned()));
    hosts.register(Box::new(Edge));
    hosts.register(Box::new(Kv {
        store: Arc::new(Mutex::new(HashMap::new())),
    }));
    hosts.register(Box::new(Log {
        tenant: tenant.name.clone(),
        quiet: options.quiet,
    }));
    hosts.register(Box::new(Upstream {
        latency: options.latency,
        blocking: options.blocking_upstream,
        tenant: tenant.name.clone(),
        allow: Arc::new(tenant.fetch.clone()),
    }));
    hosts
}

/// Compiles, checks the grant, lowers and prepares; or says why not.
fn prepare(options: &DeployOptions, tenant: &mut Tenant) -> Result<Deployed, String> {
    let Some((module, function)) = tenant.entry.split_once('.') else {
        return Err(format!("entry `{}` is not `module.function`", tenant.entry));
    };
    let (module, function) = (module.to_string(), function.to_string());
    let compiled = compile(&options.tenants, &module)?;
    admit(tenant, &compiled)?;
    let Compiled {
        sources,
        program: checked,
        check,
        ..
    } = compiled;

    let started = Instant::now();
    let schemas = HostSchemas::only(SCHEMAS);
    let lowered = cove_ir::lower_entry(&checked, &sources, &schemas, &module, &function)
        .map_err(|items| format!("does not lower:\n{}", report(&sources, &items)))?;
    let functions = lowered.functions.len();
    let prepared = PreparedProgram::new(Arc::new(lowered));
    let prepared = match options.backend {
        Backend::Vm => prepared,
        Backend::Native => prepared
            .with_native()
            .map_err(|why| format!("`--backend native`: {why}"))?,
    };
    let prepare = started.elapsed();

    let hosts = Arc::new(registry(tenant, options));
    let sources = Arc::new(sources);
    let runtime = Arc::new(Runtime::new(
        Arc::new(checked),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    ));

    let mut deployed = Deployed {
        module,
        function,
        sources,
        hosts,
        runtime,
        prepared,
        cost: DeployCost {
            check,
            prepare,
            functions,
            isolate: Duration::ZERO,
        },
    };
    let started = Instant::now();
    for _ in 0..100 {
        drop(deployed.isolate());
    }
    deployed.cost.isolate = started.elapsed() / 100;
    Ok(deployed)
}

/// Diagnostics, rendered the way `cove check` renders them.
pub fn report(sources: &SourceMap, items: &[Diagnostic]) -> String {
    items.iter().map(|item| render(sources, item)).collect()
}

/// The `edge.Request` a tenant's `handle` is invoked with.
pub fn request_value(method: &str, path: &str, query: &[(String, String)], body: &str) -> Value {
    use cove_runtime::value::MapKey;
    Value::structure(
        "edge.Request",
        vec![
            ("method", Value::string(method)),
            ("path", Value::string(path)),
            (
                "query",
                Value::map(
                    query
                        .iter()
                        .map(|(k, v)| (MapKey::Str(k.clone()), Value::string(v.as_str()))),
                ),
            ),
            ("body", Value::string(body)),
        ],
    )
}
