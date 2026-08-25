//! What the host modules the toolchain ships declare about themselves.
//!
//! Each `HostApi` implementation in `cove-runtime` answers with the table
//! named here rather than one of its own, so the description a run enforces,
//! the one `cove check` checks a call against, and the one `cove trace` reads
//! out of a recorded file are the same bytes. That is what ADR 0001's
//! "shared by the compiler, runtime, and CLI" means when it is taken
//! literally.
//!
//! A host outside this workspace declares itself the same way, in its own
//! crate: nothing here is privileged, and [`SHIPPED`] is only the list of the
//! modules `cove run` wires up. The compiler cannot see an embedder's tables,
//! which is why the boundary checks a call as well as the checker.

use crate::{
    Effect, FieldSchema, HostType, ModuleSchema, OperationSchema, ResourceSchema, TypeSchema,
};

/// Every host module `cove run` registers, in the order it registers them.
///
/// `cove trace` and `cove replay` read a trace without a host to ask, and
/// both need what the schema says: which calls the trace recorded are
/// irreversible, which capability each one needs, and whether a result was
/// recordable. `cove-sema` needs the same table for the other end of the
/// call, where an argument still has a span to point at.
pub static SHIPPED: &[ModuleSchema] = &[
    CONSOLE, ENV, DOCUMENTS, CLOCK, FILES, PROCESS, DATABASE, HTTP,
];

/// Every host module the toolchain ships.
pub fn shipped() -> &'static [ModuleSchema] {
    SHIPPED
}

/// The shipped module `name` describes itself with, if there is one.
///
/// A name that is not here is not an error: a host may register any module it
/// likes, and the compiler says only that it cannot check what it cannot see.
pub fn module(name: &str) -> Option<&'static ModuleSchema> {
    SHIPPED.iter().find(|module| module.name == name)
}

// ------------------------------------------------------------------ console

/// `console`: line-oriented output.
///
/// Both operations take a variadic `String`, which is why
/// `console.println("a", "b")` prints one line of two space-separated parts.
/// Bytes already handed to the terminal cannot be taken back, so both are
/// irreversible writes.
pub const CONSOLE: ModuleSchema = ModuleSchema {
    name: "console",
    capability: "console",
    operations: &[
        OperationSchema {
            name: "println",
            params: &[HostType::String],
            variadic: true,
            result: HostType::Result(&HostType::Unit, &HostType::Error),
            capability: "console",
            effect: Effect::IrreversibleWrite,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "print",
            params: &[HostType::String],
            variadic: true,
            result: HostType::Result(&HostType::Unit, &HostType::Error),
            capability: "console",
            effect: Effect::IrreversibleWrite,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[],
    resources: &[],
};

// ---------------------------------------------------------------------- env

/// `env`: read-only access to the environment the host supplies.
pub const ENV: ModuleSchema = ModuleSchema {
    name: "env",
    capability: "env",
    operations: &[OperationSchema {
        name: "get",
        params: &[HostType::String],
        variadic: false,
        result: HostType::Option(&HostType::String),
        capability: "env",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }],
    types: &[],
    resources: &[],
};

// ---------------------------------------------------------------- documents

/// `documents`: a filtered, read-only view over a fixed set of named text
/// documents.
pub const DOCUMENTS: ModuleSchema = ModuleSchema {
    name: "documents",
    capability: "documents",
    operations: &[OperationSchema {
        name: "read",
        params: &[HostType::String],
        variadic: false,
        result: HostType::Result(&HostType::String, &HostType::Error),
        capability: "documents",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }],
    types: &[],
    resources: &[],
};

// -------------------------------------------------------------------- clock

/// `clock`: monotonic time, waiting, and work bounded or repeated in time.
///
/// `timeout` and `every` are both given work rather than data: the first
/// takes the block it bounds as a trailing closure, and the second takes the
/// body it repeats. Neither could be written before ADR 0013 added a way back
/// into Cove, because a host call receives values and had no way to run one.
/// Both declare that work [`HostType::Any`]: what it produces is the
/// program's business, not the clock's.
///
/// Both are reads. Waiting leaves nothing outside the run different, and
/// whatever the body does is charged where the body does it.
pub const CLOCK: ModuleSchema = ModuleSchema {
    name: "clock",
    capability: "clock",
    operations: &[
        OperationSchema {
            name: "now",
            params: &[],
            variadic: false,
            result: HostType::Duration,
            capability: "clock",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "sleep",
            params: &[HostType::Duration],
            variadic: false,
            result: HostType::Result(&HostType::Unit, &HostType::Error),
            capability: "clock",
            // Waiting leaves nothing outside the run different, so it reads
            // the clock rather than writing anything.
            effect: Effect::Read,
            // Nothing has happened yet while a wait is in flight, so
            // abandoning one is safe. A cancelled task stops at its next
            // safepoint, which is after the wait it is already inside
            // returns.
            cancellable: true,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "timeout",
            params: &[HostType::Duration, HostType::Any],
            variadic: false,
            result: HostType::Result(&HostType::Any, &HostType::Error),
            capability: "clock",
            effect: Effect::Read,
            cancellable: true,
            // What the body did is the body's own business and is recorded
            // where it happened; what this call answers is whether the bound
            // held.
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "every",
            params: &[HostType::Duration, HostType::Any],
            variadic: false,
            result: HostType::Result(&HostType::Unit, &HostType::Error),
            capability: "clock",
            effect: Effect::Read,
            cancellable: true,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[],
    resources: &[],
};

// -------------------------------------------------------------------- files

/// `files`: reading and writing a rooted directory.
///
/// This is the first host whose operations disagree about [`Effect`], and the
/// disagreement is real: `read`, `exists`, and `list` leave the filesystem
/// exactly as they found it, while `write` and `delete` destroy whatever was
/// there before and no host can put it back. Nothing in the language consults
/// `effect` — `cove impact` is its eventual consumer — but the runtime does:
/// it counts the calls that changed the world, and `cove run --stats` reports
/// the count, so a run says how much of what it did cannot be undone.
///
/// `read`, `exists`, and `list` are cancellable for the same reason
/// `clock.sleep` is: abandoning one leaves nothing outside the run different.
/// `write` and `delete` are not, because a call already in flight may already
/// have reached the disk.
pub const FILES: ModuleSchema = ModuleSchema {
    name: "files",
    capability: "files",
    operations: &[
        OperationSchema {
            name: "read",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::String, &HostType::Error),
            capability: "files",
            effect: Effect::Read,
            cancellable: true,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "write",
            params: &[HostType::String, HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::Unit, &HostType::Error),
            capability: "files",
            effect: Effect::IrreversibleWrite,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "exists",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Bool,
            capability: "files",
            effect: Effect::Read,
            cancellable: true,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "list",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::Array(&HostType::String), &HostType::Error),
            capability: "files",
            effect: Effect::Read,
            cancellable: true,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "delete",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::Unit, &HostType::Error),
            capability: "files",
            effect: Effect::IrreversibleWrite,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[],
    resources: &[],
};

// ------------------------------------------------------------------ process

/// `process`: the run's own arguments, its end, and subprocesses.
///
/// `exit` and `run` are irreversible writes: a process that has ended cannot
/// be brought back, and a subprocess that has run has already done whatever
/// it does. Neither is cancellable for the same reason.
///
/// `exit` is the one shipped operation that is not recordable. Recordability
/// means the result can be handed back later without calling the host again,
/// and `exit` has no result worth handing back — a replay that returned
/// `Unit` in its place would continue running a program that had ended.
///
/// `run` is deliberately not a spawn: it starts the subprocess, waits for it,
/// and answers with what it wrote to standard output.
pub const PROCESS: ModuleSchema = ModuleSchema {
    name: "process",
    capability: "process",
    operations: &[
        OperationSchema {
            name: "args",
            params: &[],
            variadic: false,
            result: HostType::Array(&HostType::String),
            capability: "process",
            effect: Effect::Read,
            cancellable: true,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "exit",
            params: &[HostType::Int],
            variadic: false,
            result: HostType::Unit,
            capability: "process",
            effect: Effect::IrreversibleWrite,
            cancellable: false,
            recordable: false,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "run",
            params: &[HostType::String, HostType::Array(&HostType::String)],
            variadic: false,
            result: HostType::Result(&HostType::String, &HostType::Error),
            capability: "process",
            effect: Effect::IrreversibleWrite,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[],
    resources: &[],
};

// ----------------------------------------------------------------- database

/// `database`: querying, and connections a host keeps.
///
/// A row is a `String` because the runtime has no way to describe a row's
/// columns: a typed row would need a host type with fields the boundary
/// checks, and it checks a declared type's name only. One `Array<String>` of
/// rows is what a host can honestly hand back today.
///
/// `query` reads. A statement that changes stored data would be a separate
/// operation with a separate [`Effect`], and this module does not ship one:
/// an `execute` whose only implementation is a fake would be a promise that
/// data was written somewhere.
///
/// The connection is task-safe. What a handle names lives behind the host's
/// own lock, so two tasks holding the same handle take turns rather than
/// racing — which is exactly the condition the Language Card puts on a host
/// resource crossing a task boundary. `examples/callbacks/main.cove` depends
/// on it: the repository is captured by handlers that run in request tasks.
pub const DATABASE: ModuleSchema = ModuleSchema {
    name: "database",
    capability: "database",
    operations: &[
        OperationSchema {
            name: "query",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::Array(&HostType::String), &HostType::Error),
            capability: "database",
            effect: Effect::Read,
            cancellable: true,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "connect",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::Named("database.Connection"), &HostType::Error),
            capability: "database",
            // Taking a connection is a change the same host can put back,
            // which is what `close` does.
            effect: Effect::ReversibleWrite,
            cancellable: false,
            // A handle is a name, so a trace records the name and a replay
            // hands the same one back.
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[],
    resources: &[ResourceSchema {
        name: "Connection",
        task_safe: true,
        operations: &[
            OperationSchema {
                name: "query",
                params: &[HostType::String],
                variadic: false,
                result: HostType::Result(&HostType::Array(&HostType::String), &HostType::Error),
                capability: "database",
                effect: Effect::Read,
                cancellable: true,
                recordable: true,
                result_is_task_safe: true,
            },
            OperationSchema {
                name: "close",
                params: &[],
                variadic: false,
                result: HostType::Result(&HostType::Unit, &HostType::Error),
                capability: "database",
                effect: Effect::ReversibleWrite,
                cancellable: false,
                recordable: true,
                result_is_task_safe: true,
            },
        ],
    }],
};

// --------------------------------------------------------------------- http

/// `http`: fetching over the network, and listening on a port.
///
/// The four declared types are all ordinary data: a request and a response
/// are what crossed the wire, a method is one of two names, and a route pairs
/// them with the callback that answers it. A `handler` is [`HostType::Any`]
/// because the host never looks inside it — it stores the value and calls it.
///
/// `listen` is a reversible write: it takes a port from the machine, and
/// `close` gives it back. `fetch` reads, since a `GET` is what it sends and
/// nothing outside the run is different afterwards. `json` touches nothing at
/// all — it is a constructor the host owns because the host owns the
/// encoding.
///
/// The listener lives behind a lock the host owns, so two tasks may both hold
/// the handle and take turns accepting: the resource is task-safe, and the
/// schema is where it says so.
pub const HTTP: ModuleSchema = ModuleSchema {
    name: "http",
    capability: "http",
    operations: &[
        OperationSchema {
            name: "fetch",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::String, &HostType::Error),
            capability: "http",
            effect: Effect::Read,
            cancellable: true,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "json",
            params: &[HostType::Int, HostType::Any],
            variadic: false,
            result: HostType::Named("http.Response"),
            capability: "http",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "listen",
            params: &[HostType::Int],
            variadic: false,
            result: HostType::Result(&HostType::Named("http.Server"), &HostType::Error),
            capability: "http",
            effect: Effect::ReversibleWrite,
            cancellable: false,
            // A handle is a name, so recording one records the name. A replay
            // hands the same name back and answers the calls made on it from
            // the trace as well.
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[
        TypeSchema {
            name: "Method",
            cases: &["Get", "Post"],
            fields: &[],
        },
        TypeSchema {
            name: "Request",
            cases: &[],
            fields: &[
                FieldSchema {
                    name: "method",
                    ty: HostType::Named("http.Method"),
                },
                FieldSchema {
                    name: "path",
                    ty: HostType::String,
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
                    name: "body",
                    ty: HostType::String,
                },
            ],
        },
        TypeSchema {
            name: "Route",
            cases: &[],
            fields: &[
                FieldSchema {
                    name: "method",
                    ty: HostType::Named("http.Method"),
                },
                FieldSchema {
                    name: "path",
                    ty: HostType::String,
                },
                FieldSchema {
                    name: "handler",
                    ty: HostType::Any,
                },
            ],
        },
    ],
    resources: &[ResourceSchema {
        name: "Server",
        task_safe: true,
        operations: &[
            OperationSchema {
                name: "port",
                params: &[],
                variadic: false,
                result: HostType::Int,
                capability: "http",
                effect: Effect::Read,
                cancellable: false,
                recordable: true,
                result_is_task_safe: true,
            },
            OperationSchema {
                name: "handle",
                params: &[HostType::Array(&HostType::Named("http.Route"))],
                variadic: false,
                result: HostType::Result(&HostType::Bool, &HostType::Error),
                capability: "http",
                // A response that has reached a client cannot be taken back.
                effect: Effect::IrreversibleWrite,
                cancellable: true,
                // The answer is whether a request arrived, which is a fact
                // about the run and not about the handler that ran inside it.
                // Replaying it reproduces the shape of the loop; the handler
                // runs for real either way, because it is the program's own
                // code.
                recordable: true,
                result_is_task_safe: true,
            },
            OperationSchema {
                name: "close",
                params: &[],
                variadic: false,
                result: HostType::Result(&HostType::Unit, &HostType::Error),
                capability: "http",
                effect: Effect::ReversibleWrite,
                cancellable: false,
                recordable: true,
                result_is_task_safe: true,
            },
        ],
    }],
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry gates on the module's capability, and each operation
    /// declares the capability it needs. Nothing today mixes capabilities
    /// inside one module, and a module whose operations disagreed with it
    /// would make the grant check and the schema tell different stories.
    #[test]
    fn every_operation_declares_its_module_capability() {
        for module in SHIPPED {
            for entry in module.operations {
                assert_eq!(entry.capability, module.capability, "`{}`", module.name);
            }
            for resource in module.resources {
                for entry in resource.operations {
                    assert_eq!(entry.capability, module.capability, "`{}`", module.name);
                }
            }
        }
    }

    /// Every `Named` type a shipped operation mentions is one a shipped
    /// module declares. A name nothing declares would be a signature naming a
    /// type that does not exist, which neither end could check a value
    /// against.
    #[test]
    fn every_declared_type_a_shipped_operation_names_exists() {
        fn named(ty: &HostType, found: &mut Vec<&'static str>) {
            match ty {
                HostType::Named(name) => found.push(name),
                HostType::Array(inner) | HostType::Option(inner) => named(inner, found),
                HostType::Result(ok, error) => {
                    named(ok, found);
                    named(error, found);
                }
                _ => {}
            }
        }

        let mut names = Vec::new();
        for module in SHIPPED {
            let operations = module
                .operations
                .iter()
                .chain(module.resources.iter().flat_map(|r| r.operations));
            for entry in operations {
                for param in entry.params {
                    named(param, &mut names);
                }
                named(&entry.result, &mut names);
            }
            for declared in module.types {
                for field in declared.fields {
                    named(&field.ty, &mut names);
                }
            }
        }

        for name in names {
            let (owner, type_name) = name
                .split_once('.')
                .unwrap_or_else(|| panic!("`{name}` is not written qualified"));
            let owner = module(owner).unwrap_or_else(|| panic!("`{name}` names no shipped module"));
            assert!(
                owner.declares_type(type_name),
                "`{name}` names a type `{}` does not declare",
                owner.name
            );
        }
    }

    /// What `cove trace`, `cove replay`, and `cove check` read instead of a
    /// live host.
    #[test]
    fn the_shipped_schema_names_every_module_a_run_registers() {
        let names: Vec<&str> = SHIPPED.iter().map(|module| module.name).collect();
        assert_eq!(
            names,
            [
                "console",
                "env",
                "documents",
                "clock",
                "files",
                "process",
                "database",
                "http"
            ]
        );
    }
}
