//! `database`: connections, and the queries made on them.
//!
//! The Language Card lists the database among the operations that are typed
//! Host APIs, and `examples/callbacks/main.cove` shows the shape it expects:
//!
//! ```cove
//! let repository = database.connect("bookings")?
//! repository.query("insert into bookings ...")?
//! ```
//!
//! That is a *host resource handle*: `connect` hands back a name, and later
//! calls are made on that name rather than on the module. ADR 0013 is what
//! makes one possible — [`crate::host::ResourceHandle`] is the value, and
//! `Connection` in this module's [`crate::schema::ResourceSchema`] is what
//! says which operations it answers and that it may cross a task boundary.
//!
//! What a connection *is* stays here, on the host's side. A handle carries a
//! number and nothing else, so the only way to learn anything through one is
//! to call an operation the schema declares, and a handle whose connection
//! has been closed finds nothing to call: that is a reported error, not a
//! call on whatever occupies the slot now.
//!
//! There is still no real implementation, and this module does not pretend
//! otherwise. Connecting to a database means speaking a wire protocol to a
//! server, and the runtime depends on nothing but the standard library, which
//! cannot. What exists is the pair the Language Card promises for the ones
//! that cannot be real: [`Database::recorded`], a fake whose connections
//! answer from a table of canned rows, and [`Database::denied`], which
//! refuses to connect and says why. The CLI installs the denied one, so a
//! program that asks for `database` is told that this host has none rather
//! than being told that `database` does not exist.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::error::RuntimeError;
use crate::host::{HostApi, Reentry, ResourceHandle};
use crate::schema::ModuleSchema;
use crate::value::{Repr, Value};

/// `database`: querying a database, when the host has one.
pub struct Database {
    source: DatabaseSource,
    /// Which connections this host still has open, by the identity it
    /// issued.
    ///
    /// A handle addresses an entry here and nothing else. What is stored is
    /// the name the program connected to, because a fake has nothing else to
    /// keep; a real host would store the socket in the same place, and
    /// nothing above this line would change.
    open: Mutex<BTreeMap<u64, String>>,
    /// The identity the next connection gets.
    next_id: AtomicU64,
}

enum DatabaseSource {
    /// Canned rows, keyed by the exact query text.
    Recorded(BTreeMap<String, Vec<String>>),
    /// A host with no database. Every query is refused.
    Denied,
}

/// What `database` declares about itself.
///
/// The table is [`cove_schema::hosts::DATABASE`], so the description the
/// compiler checks a call against and the one the boundary dispatches through
/// are the same bytes.
const SCHEMA: ModuleSchema = cove_schema::hosts::DATABASE;

impl Database {
    /// A fake that answers each query from a table of canned rows, for tests.
    ///
    /// The key is the query text exactly as the program writes it. This is a
    /// recorded answer, not a query engine: a fake that interpreted SQL would
    /// be a database this project did not write and cannot vouch for.
    pub fn recorded(rows: BTreeMap<String, Vec<String>>) -> Self {
        Database::with_source(DatabaseSource::Recorded(rows))
    }

    /// A host with no database, which refuses every query and says so.
    ///
    /// The Language Card lists a denied implementation beside the real, fake,
    /// and filtered ones. Denying here rather than leaving the module out
    /// means the interface is still visible — `query`'s signature is in the
    /// schema — and a run that asks is told what is missing instead of being
    /// told that `database` is not a host module.
    pub fn denied() -> Self {
        Database::with_source(DatabaseSource::Denied)
    }

    fn with_source(source: DatabaseSource) -> Self {
        Database {
            source,
            open: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Opens a connection to `name` and issues the handle that names it.
    fn connect(&self, name: &str) -> Value {
        match &self.source {
            DatabaseSource::Recorded(_) => {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                self.locked().insert(id, name.to_string());
                Value::ok(Value(Repr::Resource(ResourceHandle::new(
                    "database",
                    &SCHEMA.resources[0],
                    id,
                ))))
            }
            DatabaseSource::Denied => Value::err(Value::error(
                "database: this host has no database, so nothing can connect",
            )),
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, String>> {
        self.open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    fn module_schema(&self) -> ModuleSchema {
        SCHEMA
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "query" => {
                let [Value(Repr::Str(sql))] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                Ok(rows_of(self.query(sql)))
            }
            "connect" => {
                let [Value(Repr::Str(name))] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                Ok(self.connect(name))
            }
            _ => unreachable!("checked by HostRegistry::call"),
        }
    }

    fn call_resource(
        &self,
        handle: &ResourceHandle,
        op: &str,
        args: Vec<Value>,
        _back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        match op {
            "query" => {
                let [Value(Repr::Str(sql))] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                if !self.locked().contains_key(&handle.id) {
                    return Err(closed(handle, "query"));
                }
                Ok(rows_of(self.query(sql)))
            }
            "close" => match self.locked().remove(&handle.id) {
                Some(_) => Ok(Value::ok(Value(Repr::Unit))),
                None => Err(closed(handle, "close")),
            },
            _ => unreachable!("checked by HostRegistry::call_resource"),
        }
    }
}

/// `Ok(rows)` or `Err(Error(message))`, as Cove reads it.
fn rows_of(answer: Result<Vec<String>, String>) -> Value {
    match answer {
        Ok(rows) => Value::ok(Value(Repr::Array(
            rows.into_iter()
                .map(|row| Value(Repr::Str(row.into())))
                .collect(),
        ))),
        Err(message) => Value::err(Value::error(message)),
    }
}

/// A call on a handle whose connection this host no longer has.
///
/// This is a [`RuntimeError`] rather than a Cove `Err`, and deliberately: a
/// query against a connection that was closed is not an expected failure the
/// program should handle, it is the program having kept a name past the thing
/// it named.
fn closed(handle: &ResourceHandle, op: &str) -> RuntimeError {
    RuntimeError::new(format!(
        "`{handle}` is closed, so `{op}` has nothing to act on"
    ))
    .with_rule(
        "A host resource handle names a resource the host owns. Closing the resource ends the handle; the name outlives it and addresses nothing.",
    )
    .with_help("open a new one, or move the `close` after the last use")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Grants, HostRegistry, NoReentry};

    fn str_arg(text: &str) -> Value {
        Value(Repr::Str(text.into()))
    }

    fn rows(value: Value) -> Vec<String> {
        match value.ok_payload() {
            Some(payload) => match payload.first() {
                Some(Value(Repr::Array(items))) => items.iter().map(ToString::to_string).collect(),
                other => panic!("expected `Ok(Array)`, found {other:?}"),
            },
            None => panic!("expected `Ok(...)`, found {value}"),
        }
    }

    fn err_message(value: Value) -> String {
        match value.err_payload() {
            Some(payload) => payload.first().map(ToString::to_string).unwrap_or_default(),
            None => panic!("expected `Err(...)`, found {value}"),
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
        let rendered: Vec<String> = database
            .module_schema()
            .operations
            .iter()
            .map(|op| op.signature())
            .collect();
        assert_eq!(
            rendered,
            [
                "query(String) -> Result<Array<String>, Error>",
                "connect(String) -> Result<database.Connection, Error>",
            ]
        );
        let rendered: Vec<String> = SCHEMA.resources[0]
            .operations
            .iter()
            .map(|op| op.signature())
            .collect();
        assert_eq!(
            rendered,
            [
                "query(String) -> Result<Array<String>, Error>",
                "close() -> Result<Unit, Error>",
            ]
        );
    }

    /// A handle is a name, and closing the resource ends what it named.
    #[test]
    fn a_closed_connection_reports_that_its_handle_addresses_nothing() {
        let mut hosts = HostRegistry::new(Grants::new(["database"]));
        hosts.register(Box::new(recorded()));

        let opened = hosts
            .call("database", "connect", vec![str_arg("bookings")])
            .expect("the call should be allowed");
        let Value(Repr::Enum(result)) = opened else {
            panic!("expected `Ok(...)`");
        };
        let Some(Value(Repr::Resource(handle))) = result.payload.into_vec().into_iter().next()
        else {
            panic!("`connect` answers with a handle");
        };
        assert_eq!(handle.qualified_type(), "database.Connection");
        assert!(handle.task_safe);

        let rows = hosts
            .call_resource(
                &handle,
                "query",
                vec![str_arg("select id from bookings")],
                &mut NoReentry,
            )
            .expect("a query on an open connection is allowed");
        assert_eq!(super::tests::rows(rows), ["b-1", "b-2"]);

        hosts
            .call_resource(&handle, "close", Vec::new(), &mut NoReentry)
            .expect("closing an open connection is allowed");

        let error = hosts
            .call_resource(
                &handle,
                "query",
                vec![str_arg("select id from bookings")],
                &mut NoReentry,
            )
            .expect_err("a query on a closed connection is refused");
        assert_eq!(
            error.message,
            "`database.Connection#1` is closed, so `query` has nothing to act on"
        );
    }

    /// The grant gates a handle's operations exactly as it gates the
    /// module's: the boundary is one choke point, not two.
    #[test]
    fn a_run_without_the_database_grant_cannot_use_a_handle() {
        let mut hosts = HostRegistry::new(Grants::new(["console"]));
        hosts.register(Box::new(Database::denied()));
        let handle = ResourceHandle::new("database", &SCHEMA.resources[0], 1);

        let error = hosts
            .call_resource(&handle, "query", vec![str_arg("select 1")], &mut NoReentry)
            .expect_err("the call should be rejected");
        assert_eq!(
            error.message,
            "`database.Connection.query` requires the `database` capability, which this run was not granted"
        );
    }
}
