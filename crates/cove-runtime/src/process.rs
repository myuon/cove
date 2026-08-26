//! `process`: the run's own arguments, its exit status, and filtered
//! subprocesses.
//!
//! The Language Card lists the process among the operations that are typed
//! Host APIs rather than ambient authority, and the reason is sharpest here:
//! a program that can start any other program has every authority the machine
//! has, whatever the rest of its grants say. So `run` is filtered rather than
//! merely granted. [`Process::real`] takes the executables a run may start
//! from the host and refuses everything else, including a bare name that
//! would otherwise be looked up in `PATH` — searching `PATH` is exactly the
//! ambient authority Cove does not have. A host that names no executables has
//! a `process` that cannot start one, which is the default the CLI uses.
//!
//! [`Process::recorded`] is the fake. It answers `run` from a table of canned
//! output rather than starting anything, and it writes the exit code into a
//! [`ProcessLog`] instead of ending the process, so a test can observe what a
//! program asked the host to do without the host doing it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::RuntimeError;
use crate::host::HostApi;
use crate::schema::ModuleSchema;
use crate::value::Value;

/// What a program asked a fake process to do, shared between the host and
/// whoever inspects it.
///
/// Cloning shares the same record, including the clone already given to a
/// [`Process`]. The record is synchronized because a host is reachable from
/// every task of a run.
#[derive(Clone, Debug, Default)]
pub struct ProcessLog(Arc<Mutex<Recorded>>);

#[derive(Debug, Default)]
struct Recorded {
    exit: Option<i64>,
    runs: Vec<(String, Vec<String>)>,
}

impl ProcessLog {
    /// A log with nothing in it yet.
    pub fn new() -> Self {
        ProcessLog::default()
    }

    /// The code the program asked to exit with, if it asked at all.
    ///
    /// A real process would not have come back from `exit`, so only the first
    /// request is recorded: a fake that kept the last one would report an
    /// exit that a real host could never have reached.
    pub fn exit_code(&self) -> Option<i64> {
        self.recorded().exit
    }

    /// Every subprocess the program asked to start, in order, as the program
    /// and its arguments.
    pub fn runs(&self) -> Vec<(String, Vec<String>)> {
        self.recorded().runs.clone()
    }

    /// The record, taken back from a lock a panicking run may have poisoned:
    /// a broken invariant in one task must not turn every later `process`
    /// call in another into a second, unrelated failure.
    fn recorded(&self) -> MutexGuard<'_, Recorded> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// `process`: the arguments a run was given, its exit status, and the
/// executables the host allows it to start.
pub struct Process {
    args: Vec<String>,
    allowed: Vec<PathBuf>,
    control: Control,
}

enum Control {
    /// The real operating-system process: `exit` ends it, and `run` starts a
    /// real subprocess.
    Real,
    /// A process that records what it was asked to do. The map answers `run`
    /// with canned output, and is also the allow-list: a fake can only start
    /// a program it has an answer for.
    Recorded {
        outputs: BTreeMap<String, String>,
        log: ProcessLog,
    },
}

/// What `process` declares about itself.
///
/// The table is [`cove_schema::hosts::PROCESS`], so the description the
/// compiler checks a call against and the one the boundary dispatches through
/// are the same bytes.
const SCHEMA: ModuleSchema = cove_schema::hosts::PROCESS;

impl Process {
    /// The real process.
    ///
    /// `args` are the arguments the host chose to pass on, not the host's own
    /// command line: a run sees what it was given and nothing else. `allowed`
    /// is the list of executables `run` may start, each an absolute path.
    /// An empty list is a `process` that can read its arguments and end
    /// itself but cannot start anything, which is the only safe default a
    /// host that knows nothing about the program can offer.
    pub fn real(args: Vec<String>, allowed: Vec<PathBuf>) -> Self {
        Process {
            args,
            allowed,
            control: Control::Real,
        }
    }

    /// A fake process that records what it was asked to do, for tests.
    ///
    /// `outputs` maps an executable path to the standard output `run` should
    /// answer with, and is also the allow-list: a program the fake has no
    /// answer for is refused exactly as the real host refuses one the host
    /// did not name.
    pub fn recorded(args: Vec<String>, outputs: BTreeMap<String, String>, log: ProcessLog) -> Self {
        Process {
            allowed: outputs.keys().map(PathBuf::from).collect(),
            args,
            control: Control::Recorded { outputs, log },
        }
    }

    /// Ends the run, or records that it was asked to.
    ///
    /// A code the platform cannot express becomes `1`: an exit status is a
    /// small integer everywhere Cove runs, and reporting failure is closer to
    /// what a program asking for an impossible code meant than truncating the
    /// number into an unrelated one.
    fn exit(&self, code: i64) -> Value {
        match &self.control {
            Control::Real => std::process::exit(i32::try_from(code).unwrap_or(1)),
            Control::Recorded { log, .. } => {
                let mut recorded = log.recorded();
                if recorded.exit.is_none() {
                    recorded.exit = Some(code);
                }
                Value::Unit
            }
        }
    }

    /// Starts `program` with `arguments` and waits for it, or reports why
    /// this host will not.
    ///
    /// The allow-list is compared after resolving both sides, so a path that
    /// reaches an allowed executable by another name — through `..`, a
    /// symbolic link, or a directory that is one — is still allowed, and one
    /// that reaches anything else is not.
    fn run(&self, program: &str, arguments: Vec<String>) -> Result<String, String> {
        if !self.is_allowed(program) {
            return Err(format!(
                "process: `{program}` is not an executable this host allows"
            ));
        }
        match &self.control {
            Control::Real => {
                let output = std::process::Command::new(program)
                    .args(&arguments)
                    .output()
                    .map_err(|e| format!("process: cannot start `{program}`: {e}"))?;
                if !output.status.success() {
                    return Err(match output.status.code() {
                        Some(code) => format!("process: `{program}` exited with status {code}"),
                        None => format!("process: `{program}` was ended by a signal"),
                    });
                }
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            Control::Recorded { outputs, log } => {
                log.recorded().runs.push((program.to_string(), arguments));
                Ok(outputs.get(program).cloned().unwrap_or_default())
            }
        }
    }

    /// Whether `program` names one of the executables the host allowed.
    ///
    /// A relative name is refused outright. Resolving one would mean
    /// searching `PATH`, and which program `PATH` finds is decided by the
    /// environment rather than by the host — the ambient authority Cove code
    /// does not have.
    fn is_allowed(&self, program: &str) -> bool {
        let requested = Path::new(program);
        if !requested.is_absolute() {
            return false;
        }
        let resolved = requested.canonicalize().ok();
        self.allowed.iter().any(|allowed| {
            allowed == requested
                || match (&resolved, allowed.canonicalize().ok()) {
                    (Some(a), Some(b)) => a == &b,
                    _ => false,
                }
        })
    }
}

impl HostApi for Process {
    fn module_schema(&self) -> ModuleSchema {
        SCHEMA
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "args" => Ok(Value::Array(
                self.args
                    .iter()
                    .map(|a| Value::Str(a.as_str().into()))
                    .collect(),
            )),
            "exit" => {
                let [Value::Int(code)] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                Ok(self.exit(*code))
            }
            "run" => {
                let [Value::Str(program), Value::Array(arguments)] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                let mut collected = Vec::with_capacity(arguments.len());
                for argument in arguments.iter() {
                    // The boundary followed `Array<String>` all the way down,
                    // so every element is one.
                    let Value::Str(argument) = argument else {
                        unreachable!("checked by HostRegistry::call")
                    };
                    collected.push(argument.to_string());
                }
                let program = program.to_string();
                Ok(match self.run(&program, collected) {
                    Ok(output) => Value::ok(Value::Str(output.into())),
                    Err(message) => Value::err(Value::error(message)),
                })
            }
            _ => unreachable!("checked by HostRegistry::call"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Grants, HostRegistry};

    fn str_arg(text: &str) -> Value {
        Value::Str(text.into())
    }

    fn array_arg(items: &[&str]) -> Value {
        Value::Array(items.iter().map(|s| Value::Str((*s).into())).collect())
    }

    fn strings(value: Value) -> Vec<String> {
        match value {
            Value::Array(items) => items.iter().map(ToString::to_string).collect(),
            other => panic!("expected an `Array`, found {other}"),
        }
    }

    fn ok_value(value: Value) -> Value {
        match value.ok_payload() {
            Some(payload) => payload.first().cloned().unwrap_or(Value::Unit),
            None => panic!("expected `Ok(...)`, found {value}"),
        }
    }

    fn err_message(value: Value) -> String {
        match value.err_payload() {
            Some(payload) => payload.first().map(ToString::to_string).unwrap_or_default(),
            None => panic!("expected `Err(...)`, found {value}"),
        }
    }

    fn fake(outputs: BTreeMap<String, String>) -> (Process, ProcessLog) {
        let log = ProcessLog::new();
        let process = Process::recorded(
            vec!["--name".to_string(), "cove".to_string()],
            outputs,
            log.clone(),
        );
        (process, log)
    }

    #[test]
    fn args_answers_what_the_host_passed_on() {
        let (process, _) = fake(BTreeMap::new());

        let args = process.call("args", Vec::new()).unwrap();
        assert_eq!(strings(args), ["--name", "cove"]);
    }

    #[test]
    fn args_of_a_run_given_nothing_is_empty() {
        let process = Process::real(Vec::new(), Vec::new());

        let args = process.call("args", Vec::new()).unwrap();
        assert!(strings(args).is_empty());
    }

    #[test]
    fn a_fake_records_the_exit_code_instead_of_ending_the_process() {
        let (process, log) = fake(BTreeMap::new());
        assert_eq!(log.exit_code(), None);

        let exited = process.call("exit", vec![Value::Int(3)]).unwrap();
        assert!(matches!(exited, Value::Unit), "{exited}");
        assert_eq!(log.exit_code(), Some(3));
    }

    /// A real process never returns from `exit`, so a fake that let a second
    /// request overwrite the first would report an exit no real host could
    /// have reached.
    #[test]
    fn only_the_first_exit_is_recorded() {
        let (process, log) = fake(BTreeMap::new());

        process.call("exit", vec![Value::Int(3)]).unwrap();
        process.call("exit", vec![Value::Int(0)]).unwrap();
        assert_eq!(log.exit_code(), Some(3));
    }

    #[test]
    fn a_fake_answers_run_from_its_table_and_records_the_call() {
        let (process, log) = fake(BTreeMap::from([(
            "/bin/echo".to_string(),
            "hello\n".to_string(),
        )]));

        let output = process
            .call("run", vec![str_arg("/bin/echo"), array_arg(&["hello"])])
            .unwrap();
        assert_eq!(ok_value(output).to_string(), "hello\n");
        assert_eq!(
            log.runs(),
            [("/bin/echo".to_string(), vec!["hello".to_string()])]
        );
    }

    /// Every program the host did not name, refused by both implementations
    /// before anything is started.
    #[test]
    fn every_program_the_host_did_not_name_is_refused() {
        let allowed = "/bin/echo";
        let refused = [
            // Not on the list at all.
            "/bin/sh",
            // A bare name, which would mean searching `PATH`.
            "echo",
            // A relative path, which would mean the working directory
            // decides what runs.
            "./echo",
            "../bin/echo",
            // An absolute path that does not reach an allowed executable.
            "/usr/bin/env",
        ];

        let (mut fake_process, log) = fake(BTreeMap::from([(
            allowed.to_string(),
            "hello\n".to_string(),
        )]));
        let mut real_process = Process::real(Vec::new(), vec![PathBuf::from(allowed)]);

        for program in refused {
            for process in [&mut fake_process, &mut real_process] {
                let outcome = process
                    .call("run", vec![str_arg(program), array_arg(&[])])
                    .unwrap();
                assert_eq!(
                    err_message(outcome),
                    format!("process: `{program}` is not an executable this host allows"),
                    "`{program}`"
                );
            }
        }
        assert!(log.runs().is_empty());
    }

    /// A host that named no executables cannot start one, which is the
    /// default the CLI installs.
    #[test]
    fn a_host_with_an_empty_allow_list_starts_nothing() {
        let process = Process::real(Vec::new(), Vec::new());

        let outcome = process
            .call("run", vec![str_arg("/bin/echo"), array_arg(&[])])
            .unwrap();
        assert_eq!(
            err_message(outcome),
            "process: `/bin/echo` is not an executable this host allows"
        );
    }

    /// The allow-list names executables, not spellings: a path that reaches
    /// an allowed executable by another route is the same executable.
    #[cfg(unix)]
    #[test]
    fn a_different_spelling_of_an_allowed_executable_is_still_allowed() {
        if !Path::new("/bin/echo").exists() {
            return;
        }
        let process = Process::real(Vec::new(), vec![PathBuf::from("/bin/echo")]);

        let output = process
            .call(
                "run",
                vec![str_arg("/bin/../bin/echo"), array_arg(&["hello"])],
            )
            .unwrap();
        assert_eq!(ok_value(output).to_string(), "hello\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_real_run_answers_with_what_the_subprocess_wrote() {
        if !Path::new("/bin/echo").exists() {
            return;
        }
        let process = Process::real(Vec::new(), vec![PathBuf::from("/bin/echo")]);

        let output = process
            .call(
                "run",
                vec![str_arg("/bin/echo"), array_arg(&["one", "two"])],
            )
            .unwrap();
        assert_eq!(ok_value(output).to_string(), "one two\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_real_run_that_fails_reports_the_status() {
        if !Path::new("/bin/sh").exists() {
            return;
        }
        let process = Process::real(Vec::new(), vec![PathBuf::from("/bin/sh")]);

        let outcome = process
            .call(
                "run",
                vec![str_arg("/bin/sh"), array_arg(&["-c", "exit 7"])],
            )
            .unwrap();
        assert_eq!(
            err_message(outcome),
            "process: `/bin/sh` exited with status 7"
        );
    }

    #[test]
    fn a_run_without_the_process_grant_cannot_read_its_arguments() {
        let mut hosts = HostRegistry::new(Grants::new(["console"]));
        hosts.register(Box::new(Process::real(Vec::new(), Vec::new())));

        let error = hosts
            .call("process", "args", Vec::new())
            .expect_err("the call should be rejected");
        assert_eq!(
            error.message,
            "`process.args` requires the `process` capability, which this run was not granted"
        );
    }

    #[test]
    fn a_granted_process_is_reachable_through_the_registry() {
        let log = ProcessLog::new();
        let mut hosts = HostRegistry::new(Grants::new(["process"]));
        hosts.register(Box::new(Process::recorded(
            vec!["one".to_string()],
            BTreeMap::new(),
            log.clone(),
        )));

        let args = hosts
            .call("process", "args", Vec::new())
            .expect("the call should be allowed");
        assert_eq!(strings(args), ["one"]);

        hosts
            .call("process", "exit", vec![Value::Int(2)])
            .expect("the call should be allowed");
        assert_eq!(log.exit_code(), Some(2));
    }

    #[test]
    fn signatures_read_like_source() {
        let process = Process::real(Vec::new(), Vec::new());
        let rendered: Vec<String> = process.schema().iter().map(|op| op.signature()).collect();
        assert_eq!(
            rendered,
            [
                "args() -> Array<String>",
                "exit(Int) -> Unit",
                "run(String, Array<String>) -> Result<String, Error>",
            ]
        );
    }

    /// Ending a run cannot be replayed by handing back a recorded result, so
    /// `exit` is the one shipped operation that is not recordable.
    #[test]
    fn ending_the_run_is_not_recordable() {
        let process = Process::real(Vec::new(), Vec::new());
        for op in process.schema() {
            assert_eq!(op.recordable, op.name != "exit", "`process.{}`", op.name);
        }
    }
}
