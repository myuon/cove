//! `database`: queries, and the connection this runtime cannot yet hold.
//!
//! The Language Card lists the database among the operations that are typed
//! Host APIs, and `examples/callbacks/main.cove` shows the shape it expects:
//!
//! ```cove
//! let repository = await BookingRepository.connect()?
//! app.repository.create(input, attempt)
//! ```
//!
//! That is a *host resource handle*: `connect` hands back a value, and later
//! calls are made on that value rather than on the module. Nothing in this
//! runtime can produce one, and the gap is not in this module.
//!
//! [`crate::host::HostApi::call`] answers with a [`Value`], and a [`Value`]
//! has no variant that means "a live resource this host still owns". A host
//! could return [`Value::HostModule`], but the interpreter dispatches a
//! method call by the receiver's `type_name`: it looks for a declared type of
//! the package, then for a task scope or task, and then hands the call to the
//! builtins, which know nothing about hosts. There is no branch that sends a
//! method call on a host-returned value back to the registry. Adding one
//! means changing the interpreter and the value representation, and until it
//! is added, `connect` would hand back something no later call could use.
//!
//! `BookingRepository` is a second, separate gap: it is a host *type*, and a
//! `use database` binds only the module, so `BookingRepository.connect()`
//! resolves to nothing at all. Host types have no representation either — the
//! type checker says as much, warning that a host type's values are
//! unchecked because "a Host API's types come from its schema, and there is
//! no schema yet."
//!
//! So this module ships what a connectionless database can honestly do: one
//! `query` that takes SQL and answers with rows. There is no real
//! implementation, and this module does not pretend otherwise. Connecting to
//! a database means speaking a wire protocol to a server, and the runtime
//! depends on nothing but the standard library, which cannot. What exists is
//! the pair the Language Card promises for the ones that cannot be real:
//! [`Database::recorded`], a fake that answers from a table of canned rows,
//! and [`Database::denied`], which refuses every query and says why. The CLI
//! installs the denied one, so a program that asks for `database` is told
//! that this host has none rather than being told that `database` does not
//! exist.

use std::collections::BTreeMap;

use cove_sema::Capability;

use crate::error::RuntimeError;
use crate::host::HostApi;
use crate::schema::{Effect, HostType, OperationSchema};
use crate::value::Value;

/// `database`: querying a database, when the host has one.
pub struct Database {
    source: DatabaseSource,
}

enum DatabaseSource {
    /// Canned rows, keyed by the exact query text.
    Recorded(BTreeMap<String, Vec<String>>),
    /// A host with no database. Every query is refused.
    Denied,
}

/// The operations `database` exposes.
///
/// A row is a `String` because the runtime has no way to describe a row's
/// columns: a typed row would be a host type, and host types have no
/// representation. One `Array<String>` of rows is what a host can honestly
/// hand back today.
///
/// `query` reads. A statement that changes stored data would be a separate
/// operation with a separate [`Effect`], and this module does not ship one:
/// an `execute` whose only implementation is a fake would be a promise that
/// data was written somewhere.
static DATABASE_SCHEMA: &[OperationSchema] = &[OperationSchema {
    name: "query",
    params: &[HostType::String],
    variadic: false,
    result: HostType::Result(&HostType::Array(&HostType::String), &HostType::Error),
    capability: "database",
    effect: Effect::Read,
    cancellable: true,
    recordable: true,
    result_is_task_safe: true,
}];

impl Database {
    /// A fake that answers each query from a table of canned rows, for tests.
    ///
    /// The key is the query text exactly as the program writes it. This is a
    /// recorded answer, not a query engine: a fake that interpreted SQL would
    /// be a database this project did not write and cannot vouch for.
    pub fn recorded(rows: BTreeMap<String, Vec<String>>) -> Self {
        Database {
            source: DatabaseSource::Recorded(rows),
        }
    }

    /// A host with no database, which refuses every query and says so.
    ///
    /// The Language Card lists a denied implementation beside the real, fake,
    /// and filtered ones. Denying here rather than leaving the module out
    /// means the interface is still visible — `query`'s signature is in the
    /// schema — and a run that asks is told what is missing instead of being
    /// told that `database` is not a host module.
    pub fn denied() -> Self {
        Database {
            source: DatabaseSource::Denied,
        }
    }

    fn query(&self, sql: &str) -> Result<Vec<String>, String> {
        match &self.source {
            DatabaseSource::Recorded(rows) => rows
                .get(sql)
                .cloned()
                .ok_or_else(|| format!("database: no recorded answer for `{sql}`")),
            DatabaseSource::Denied => {
                Err("database: this host has no database, so no query can run".to_string())
            }
        }
    }
}

impl HostApi for Database {
    fn name(&self) -> &str {
        "database"
    }

    fn capability(&self) -> Capability {
        Capability::new("database")
    }

    fn schema(&self) -> &[OperationSchema] {
        DATABASE_SCHEMA
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "query" => {
                let [Value::Str(sql)] = args.as_slice() else {
                    return Err(RuntimeError::new(
                        "`database.query` takes one `String` argument",
                    ));
                };
                Ok(match self.query(sql) {
                    Ok(rows) => Value::ok(Value::Array(
                        rows.into_iter().map(|row| Value::Str(row.into())).collect(),
                    )),
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

    fn rows(value: Value) -> Vec<String> {
        match value {
            Value::Enum(result) if &*result.type_name == "Result" && &*result.case == "Ok" => {
                match result.payload.into_iter().next() {
                    Some(Value::Array(items)) => items.iter().map(ToString::to_string).collect(),
                    other => panic!("expected `Ok(Array)`, found {other:?}"),
                }
            }
            other => panic!("expected `Ok(...)`, found {other}"),
        }
    }

    fn err_message(value: Value) -> String {
        match value {
            Value::Enum(result) if &*result.type_name == "Result" && &*result.case == "Err" => {
                result
                    .payload
                    .first()
                    .map(ToString::to_string)
                    .unwrap_or_default()
            }
            other => panic!("expected `Err(...)`, found {other}"),
        }
    }

    fn recorded() -> Database {
        Database::recorded(BTreeMap::from([(
            "select id from bookings".to_string(),
            vec!["b-1".to_string(), "b-2".to_string()],
        )]))
    }

    #[test]
    fn a_recorded_query_answers_its_rows() {
        let database = recorded();

        let answer = database
            .call("query", vec![str_arg("select id from bookings")])
            .unwrap();
        assert_eq!(rows(answer), ["b-1", "b-2"]);
    }

    #[test]
    fn a_query_the_fake_has_no_answer_for_says_so() {
        let database = recorded();

        let answer = database
            .call("query", vec![str_arg("select id from invoices")])
            .unwrap();
        assert_eq!(
            err_message(answer),
            "database: no recorded answer for `select id from invoices`"
        );
    }

    #[test]
    fn a_denied_host_refuses_every_query() {
        let database = Database::denied();

        for sql in ["select 1", "select id from bookings"] {
            let answer = database.call("query", vec![str_arg(sql)]).unwrap();
            assert_eq!(
                err_message(answer),
                "database: this host has no database, so no query can run"
            );
        }
    }

    #[test]
    fn a_run_without_the_database_grant_cannot_query() {
        let mut hosts = HostRegistry::new(Grants::new(["console"]));
        hosts.register(Box::new(Database::denied()));

        let error = hosts
            .call("database", "query", vec![str_arg("select 1")])
            .expect_err("the call should be rejected");
        assert_eq!(
            error.message,
            "`database.query` requires the `database` capability, which this run was not granted"
        );
    }

    /// Denying is an implementation, not an absence: the grant still passes
    /// and the operation still exists, so the run is told what is missing
    /// rather than that `database` is not a host module.
    #[test]
    fn a_granted_denied_host_answers_the_call_with_the_refusal() {
        let mut hosts = HostRegistry::new(Grants::new(["database"]));
        hosts.register(Box::new(Database::denied()));

        let answer = hosts
            .call("database", "query", vec![str_arg("select 1")])
            .expect("the call should be allowed");
        assert_eq!(
            err_message(answer),
            "database: this host has no database, so no query can run"
        );
    }

    #[test]
    fn signatures_read_like_source() {
        let database = Database::denied();
        let rendered: Vec<String> = database.schema().iter().map(|op| op.signature()).collect();
        assert_eq!(rendered, ["query(String) -> Result<Array<String>, Error>"]);
    }
}
