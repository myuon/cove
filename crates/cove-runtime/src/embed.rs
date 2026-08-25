//! Running a Cove program that a native executable carries inside itself.
//!
//! `cove build` writes a small Rust crate whose `main` hands the package's
//! sources and its `[run.<name>]` table to [`Embedded::main`]. That crate
//! links this one, so the executable it produces embeds the interpreter
//! rather than compiling Cove to machine code; see ADR 0009.
//!
//! [`register_hosts`] is the one place the host implementations a run gets
//! are chosen. `cove run` and a built binary both call it, so a built binary
//! cannot drift into registering a different boundary than the run it was
//! built from.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use cove_diag::{render, Severity, SourceMap};
use cove_sema::config::{Config, RunConfig};
use cove_sema::package::{Module, Package, Unit};

use crate::clock::Clock;
use crate::database::Database;
use crate::files::Files;
use crate::host::{Console, Documents, Env, GrantSource, Grants, HostRegistry};
use crate::interp::Interpreter;
use crate::process::Process;
use crate::runtime::Runtime;
use crate::trace::create_trace_file;
use crate::{
    Budget, Cancellation, JsonlSink, Limits, NullSink, TraceHeader, TraceSink, Value, ValueCapture,
};

/// The hosts a run is given, and the directories they are confined to.
pub struct HostSetup {
    /// The capabilities `[run.<name>] allow` granted.
    pub grants: Vec<String>,
    /// The one directory the `documents` host may read.
    pub documents_root: PathBuf,
    /// The one directory the `files` host may reach.
    pub files_root: PathBuf,
    /// The arguments the program itself receives, which `process.args`
    /// reports and nothing else sees.
    pub program_args: Vec<String>,
    /// The executables `process.run` may start. Empty allows none.
    pub allow_exec: Vec<PathBuf>,
}

/// Registers every host implementation a run may reach, granting exactly
/// `setup.grants`.
///
/// Registering a module does not grant it: `HostRegistry::call` rejects every
/// call whose capability is missing from the grant set, so registering the
/// full set here and granting a subset is what makes an ungranted call a
/// reported refusal rather than an unknown module.
pub fn register_hosts(setup: HostSetup) -> HostRegistry {
    let mut hosts = HostRegistry::new(Grants::new(setup.grants));
    hosts.register(Box::new(Console::new(std::io::stdout())));
    hosts.register(Box::new(Env::from_process()));
    hosts.register(Box::new(Documents::rooted(setup.documents_root)));
    hosts.register(Box::new(Clock::real()));
    // Granting `files` must not hand over the machine, so this host picks one
    // directory and the runtime refuses every path outside it.
    hosts.register(Box::new(Files::rooted(setup.files_root)));
    // A program that can start any other program has every authority the
    // machine has, so `process.run` is filtered, not merely granted. This
    // host knows nothing about what a package is entitled to start, so it
    // allows nothing until the caller names an executable. `process.args`
    // passes on exactly the arguments the entry function receives, and
    // nothing of the launching command line.
    hosts.register(Box::new(Process::real(
        setup.program_args,
        setup.allow_exec,
    )));
    // There is no real `database`: connecting to one means speaking a wire
    // protocol, and this toolchain depends on nothing but the standard
    // library. A denied implementation is one of the four the Language Card
    // names, and it tells a run what is missing instead of telling it that
    // `database` is not a host module.
    hosts.register(Box::new(Database::denied()));
    hosts
}

/// One `.cove` file a built binary carries.
pub struct EmbeddedSource {
    /// The file's path relative to the package root it was built from, such
    /// as `hello/main.cove`. It names the module the file belongs to and is
    /// what a diagnostic reports, so a built binary's errors carry no path
    /// from the machine that built it.
    pub path: &'static str,
    /// The file's text, exactly as it was checked at build time.
    pub text: &'static str,
}

/// The `[run.<name>]` table a built binary carries.
///
/// Every field was fixed when the binary was built. Nothing reads a
/// `cove.toml` at run time, so a file placed beside the binary can neither
/// widen its grants nor raise its limits.
pub struct EmbeddedRun {
    /// The `[run.<name>]` table this binary was built from.
    pub name: &'static str,
    /// The fully qualified entry function, such as `hello.main`.
    pub entry: &'static str,
    /// The capabilities this binary was granted.
    pub allow: &'static [&'static str],
    /// The total fuel this binary may spend.
    pub fuel: Option<u64>,
    /// The wall-clock deadline this binary may take, in nanoseconds.
    pub deadline_nanos: Option<u64>,
    /// The total number of host calls this binary may make.
    pub max_host_calls: Option<u64>,
    /// A path to write a JSONL trace to, or `-` for stderr.
    pub trace: Option<&'static str>,
    /// The one directory the `files` host may reach, as an absolute path
    /// chosen at build time. Without one, the binary uses `files/` in its
    /// working directory.
    pub files_root: Option<&'static str>,
    /// The executables `process.run` may start.
    pub allow_exec: &'static [&'static str],
}

/// A whole program: the sources a built binary carries and the run it
/// carries them for.
pub struct Embedded {
    /// Every `.cove` file of the package, which together are the whole
    /// program: the binary reads no source from disk.
    pub sources: &'static [EmbeddedSource],
    /// The run the sources were built for.
    pub run: EmbeddedRun,
}

impl Embedded {
    /// Runs the embedded program, and is the whole of a built binary's
    /// `main`.
    ///
    /// Every process argument is the program's own: a built binary parses no
    /// flags of its own, because a flag it honoured would be a way to ask it
    /// for something its `[run.<name>]` table did not.
    pub fn main(&self) -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match self.run(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(Failure::Message(message)) => {
                eprintln!("error: {message}");
                ExitCode::FAILURE
            }
            Err(Failure::Diagnostics { sources, items }) => {
                for item in &items {
                    eprint!("{}", render(&sources, item));
                }
                let errors = items
                    .iter()
                    .filter(|d| d.severity == Severity::Error)
                    .count();
                if errors > 0 {
                    eprintln!("{errors} error(s)");
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
        }
    }

    fn run(&self, program_args: Vec<String>) -> Result<(), Failure> {
        let mut sources = SourceMap::new();
        let parsed = self.package(&mut sources);
        // Shared, because a task running on another thread points a
        // diagnostic into the same source map this thread reports from.
        let sources = Arc::new(sources);
        let package = parsed.map_err(|items| Failure::Diagnostics {
            sources: sources.clone(),
            items,
        })?;
        // The program was checked when it was built, so this is resolution
        // only: the type checker's answer cannot have changed for sources
        // that cannot have changed.
        let program =
            cove_sema::resolve::resolve(&package).map_err(|items| Failure::Diagnostics {
                sources: sources.clone(),
                items,
            })?;

        let (module, entry) = self.run.entry.rsplit_once('.').ok_or_else(|| {
            Failure::Message(format!("`{}` is not a qualified entry", self.run.entry))
        })?;

        let working_dir = std::env::current_dir()
            .map_err(|e| Failure::Message(format!("cannot read the current directory: {e}")))?;
        let mut hosts = register_hosts(HostSetup {
            grants: self.run.allow.iter().map(|s| (*s).to_string()).collect(),
            // A built binary has no package root, so the data a host may
            // reach is named relative to where the binary is run. That is
            // what lets an executable and its `documents/` and `files/`
            // directories be copied somewhere else together.
            documents_root: working_dir.join("documents"),
            files_root: match self.run.files_root {
                Some(root) => PathBuf::from(root),
                None => working_dir.join("files"),
            },
            program_args: program_args.clone(),
            allow_exec: self.run.allow_exec.iter().map(PathBuf::from).collect(),
        });
        // So that a refused call does not send the reader to a `cove.toml`
        // this binary will never read.
        hosts.set_grant_source(GrantSource::Sealed);

        let limits = Limits {
            fuel: self.run.fuel,
            deadline: self.run.deadline_nanos.map(Duration::from_nanos),
            max_host_calls: self.run.max_host_calls,
            max_call_depth: None,
        };
        hosts.set_budget(Budget::with_cancellation(limits, Cancellation::new()));

        // `HostRegistry::call` and the task and entry events the interpreter
        // traces reach the one destination the run named, from whichever
        // thread produced them. A binary built for a run that asked for no
        // trace installs `NullSink`, which is what tells the registry that
        // nothing will read a description of the values a call carried.
        let sink = self.sink(&program_args)?;
        hosts.set_trace(sink.clone());

        let args: Vec<Rc<str>> = program_args.iter().map(|a| a.as_str().into()).collect();
        let runtime =
            Runtime::new(Arc::new(program), sources.clone(), Arc::new(hosts)).with_trace(sink);
        match Interpreter::new(&runtime).run_entry(module, entry, args) {
            Ok(value) => report_exit(&value).map_err(Failure::Message),
            Err(error) => Err(Failure::Diagnostics {
                sources,
                items: vec![error.to_diagnostic()],
            }),
        }
    }

    /// Rebuilds the package the binary was built from, in memory.
    ///
    /// A directory is a module and its name follows its path, so the module
    /// each embedded file belongs to is derived from that file's recorded
    /// relative path exactly as `cove_sema::package::load` derives it from
    /// the directory on disk.
    fn package(&self, sources: &mut SourceMap) -> Result<Package, Vec<cove_diag::Diagnostic>> {
        let mut modules: BTreeMap<String, Module> = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for source in self.sources {
            let path = PathBuf::from(source.path);
            let Some(dir) = path.parent() else {
                continue;
            };
            let name = dir
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(".");
            let file = sources.add(path.clone(), source.text);
            match cove_syntax::parse_file(sources, file) {
                Ok(ast) => {
                    modules
                        .entry(name.clone())
                        .or_insert_with(|| Module {
                            name,
                            dir: dir.to_path_buf(),
                            units: Vec::new(),
                        })
                        .units
                        .push(Unit { file, path, ast });
                }
                Err(items) => diagnostics.extend(items),
            }
        }
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }

        let mut runs = BTreeMap::new();
        runs.insert(
            self.run.name.to_string(),
            RunConfig {
                entry: self.run.entry.to_string(),
                allow: self.run.allow.iter().map(|s| (*s).to_string()).collect(),
                fuel: self.run.fuel,
                deadline: self.run.deadline_nanos.map(Duration::from_nanos),
                max_host_calls: self.run.max_host_calls,
                trace: self.run.trace.map(str::to_string),
            },
        );
        Ok(Package {
            root: PathBuf::new(),
            config: Config {
                runs,
                ..Config::default()
            },
            modules,
        })
    }

    /// Opens the trace destination the `[run.<name>]` table named.
    fn sink(&self, program_args: &[String]) -> Result<Arc<dyn TraceSink>, Failure> {
        let header = TraceHeader {
            values: ValueCapture::Full,
            entry: self.run.entry.to_string(),
            args: program_args.to_vec(),
        };
        match self.run.trace {
            None => Ok(Arc::new(NullSink)),
            Some("-") => Ok(Arc::new(JsonlSink::new(std::io::stderr(), header))),
            Some(path) => {
                let file = create_trace_file(std::path::Path::new(path)).map_err(|e| {
                    Failure::Message(format!("cannot create trace file `{path}`: {e}"))
                })?;
                // A trace a program can be asked to share should not surprise
                // the person sharing it, so the run says so once, here.
                eprintln!(
                    "note: `{path}` will record the arguments and result of every host call, which may include secrets"
                );
                Ok(Arc::new(JsonlSink::new(file, header)))
            }
        }
    }
}

/// An entry returning `Err(...)` fails the run and prints the error, exactly
/// as it does under `cove run`.
fn report_exit(value: &Value) -> Result<(), String> {
    if let Value::Enum(result) = value {
        if &*result.type_name == "Result" && &*result.case == "Err" {
            return Err(result
                .payload
                .first()
                .map(ToString::to_string)
                .unwrap_or_default());
        }
    }
    Ok(())
}

/// Why a built binary stopped.
enum Failure {
    Message(String),
    Diagnostics {
        sources: Arc<SourceMap>,
        items: Vec<cove_diag::Diagnostic>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program that reaches for a capability its run was not granted.
    static REFUSED: &[EmbeddedSource] = &[EmbeddedSource {
        path: "app/main.cove",
        text: "\
use files

/// Reads a file this run was never granted.
export fn main() -> Result<Unit, Error> {
  let notes = files.read(\"notes.txt\")?
  Ok(())
}
",
    }];

    /// A program in a nested module, which only its recorded path names.
    static NESTED: &[EmbeddedSource] = &[EmbeddedSource {
        path: "a/b/main.cove",
        text: "\
/// Answers without needing any capability.
export fn main() -> Result<Unit, Error> {
  Ok(())
}
",
    }];

    fn embedded(sources: &'static [EmbeddedSource], entry: &'static str) -> Embedded {
        Embedded {
            sources,
            run: EmbeddedRun {
                name: "app",
                entry,
                allow: &[],
                fuel: None,
                deadline_nanos: None,
                max_host_calls: None,
                trace: None,
                files_root: None,
                allow_exec: &[],
            },
        }
    }

    #[test]
    fn a_module_name_follows_the_path_the_binary_recorded() {
        let embedded = embedded(NESTED, "a.b.main");
        assert!(
            embedded.run(Vec::new()).is_ok(),
            "a directory is a module and its name follows its path, embedded or not"
        );
    }

    #[test]
    fn a_trace_the_run_table_asked_for_is_written_where_it_named() {
        let path: &'static str = Box::leak(
            std::env::temp_dir()
                .join(format!("cove-embed-trace-{}.jsonl", std::process::id()))
                .display()
                .to_string()
                .into_boxed_str(),
        );
        let mut embedded = embedded(NESTED, "a.b.main");
        embedded.run.trace = Some(path);
        assert!(embedded.run(Vec::new()).is_ok());

        let trace = std::fs::read_to_string(path).expect("the trace file was created");
        let _ = std::fs::remove_file(path);
        assert!(
            trace
                .lines()
                .next()
                .is_some_and(|line| line.contains("\"event\":\"trace_header\"")
                    && line.contains("\"entry\":\"a.b.main\"")),
            "{trace}"
        );
    }

    #[test]
    fn a_capability_the_binary_was_not_built_with_is_refused() {
        let embedded = embedded(REFUSED, "app.main");
        let Err(Failure::Diagnostics { sources, items }) = embedded.run(Vec::new()) else {
            panic!("an ungranted call must be refused");
        };
        let rendered = render(&sources, &items[0]);
        assert!(
            rendered.contains(
                "`files.read` requires the `files` capability, which this run was not granted"
            ),
            "{rendered}"
        );
        // The reported path is the one the package had, not one from the
        // machine that built the binary.
        assert!(rendered.contains("--> app/main.cove:5:15"), "{rendered}");
        // Editing a `cove.toml` beside the binary would do nothing, so the
        // help does not suggest it on its own.
        assert!(
            rendered.contains("help: this binary carries the capabilities it was built with"),
            "{rendered}"
        );
    }
}
