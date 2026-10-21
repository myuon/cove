//! An embedder's handler contract over `std.web`'s types (ADR 0109).
//!
//! An HTTP embedder used to declare its own `Request` and `Response` in a
//! host module schema — minicloud's `web`, types and no operations — and hand
//! a handler a `web.Request`. Since ADR 0109 the types are the standard
//! library's: a handler is `export fn handle(request: web.Request) ->
//! web.Response` under `use std.web`, and the embedder builds a
//! `std.web.Request` and reads a `std.web.Response` with nothing but
//! [`Value::structure`], [`Value::map`] and [`Value::field`], as it would a
//! type its own package declared. No schema names them and none is needed:
//! `Vm::invoke` checks a declared struct against its declaration, field by
//! field, wherever the declaration lives.
//!
//! What this holds, on both backends and through `cove_ir::lower_entry` —
//! the sliced lowering an embedder that runs one entry uses:
//!
//! - a request the host built reaches the handler and its headers are read
//!   in any case; the response comes back as a `std.web.Response` whose
//!   fields, including the default security headers, are read by name;
//! - a request built under the old host type's name, `web.Request`, is
//!   refused before anything runs, naming the type it should have been —
//!   the one mistake an embedder migrating from a host type will make;
//! - so is one whose fields are not `std.web.Request`'s, in its order.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_runtime::interp::Interpreter;
use cove_runtime::value::MapKey;
use cove_runtime::{Grants, HostRegistry, Runtime, RuntimeError, Value, Vm};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program;
use cove_sema::{Compiler, Config, HostSchemas};

/// A handler as an app writes one: a form that changes something is checked
/// for its origin, and every answer is one of `std.web`'s constructors.
const HANDLER: &str = "\
use std.web
use std.html
use std.url

/// The handler contract: a `std.web.Request` in, a `std.web.Response` out.
export fn handle(request: web.Request) -> web.Response {
  if request.method == \"POST\" {
    if !request.sameOrigin() {
      return web.text(403, \"not from this site\\n\")
    }
    let name = request.form().get(\"name\").unwrapOr(\"nobody\")
    return web.redirect(\"{request.prefix()}/hello/{url.percentEncode(name)}\")
  }
  let agent = request.header(\"User-Agent\").unwrapOr(\"-\")
  web.html(200, html\"<p>{request.path} for {agent}</p>\")
    .withHeader(\"X-Handled-By\", \"cove\")
}
";

#[derive(Clone, Copy, Debug)]
enum Backend {
    Ast,
    Vm,
}

const BOTH: [Backend; 2] = [Backend::Ast, Backend::Vm];

/// [`HANDLER`] as a one-module package `app`, checked.
fn checked() -> (Arc<SourceMap>, Arc<Program>) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("app/app.cove");
    let file = sources.add(path.clone(), HANDLER);
    let ast = cove_syntax::parse_file(&sources, file).expect("the handler parses");
    let mut modules = BTreeMap::from([(
        "app".to_string(),
        Module {
            name: "app".to_string(),
            dir: PathBuf::from("app"),
            units: vec![Unit { file, path, ast }],
        },
    )]);
    for (name, module) in cove_sema::stdlib::attach(&mut sources).expect("stdlib parses") {
        modules.insert(name, module);
    }
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules,
    };
    match Compiler::new().compile(&package) {
        Ok(program) => (Arc::new(sources), Arc::new(program)),
        Err(items) => panic!(
            "the handler checks:\n{}",
            items
                .iter()
                .map(|item| cove_diag::render(&sources, item))
                .collect::<String>()
        ),
    }
}

/// Calls `app.handle` with `request` on `backend`, the VM's program lowered
/// for that one entry.
fn handle(backend: Backend, request: Value) -> Result<Value, RuntimeError> {
    let (sources, program) = checked();
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(program.clone(), sources.clone(), hosts.clone());
    match backend {
        Backend::Ast => Interpreter::new(&runtime).invoke("app", "handle", vec![request]),
        Backend::Vm => {
            let lowered =
                cove_ir::lower_entry(&program, &sources, &HostSchemas::new(), "app", "handle")
                    .unwrap_or_else(|items| {
                        panic!(
                            "the handler lowers:\n{}",
                            items
                                .iter()
                                .map(|item| cove_diag::render(&sources, item))
                                .collect::<String>()
                        )
                    });
            Vm::new(&runtime, &hosts, &Arc::new(lowered)).invoke("app", "handle", vec![request])
        }
    }
}

/// A `Map<String, String>` value.
fn strings(pairs: &[(&str, &str)]) -> Value {
    Value::map(
        pairs
            .iter()
            .map(|(key, value)| (MapKey::Str((*key).into()), Value::string(*value))),
    )
}

/// The request an HTTP embedder builds: `std.web.Request`'s five fields, in
/// the order the struct declares them, header names lowercase.
fn request(type_name: &str, method: &str, headers: &[(&str, &str)], body: &str) -> Value {
    Value::structure(
        type_name,
        [
            ("method", Value::string(method)),
            ("path", Value::string("/a/b")),
            ("query", strings(&[("q", "1")])),
            ("headers", strings(headers)),
            ("body", Value::string(body)),
        ],
    )
}

/// What an embedder reads out of a response: the status, every header, the
/// body — by name, as minicloud's `response_of` reads them.
fn read(response: &Value) -> (i64, BTreeMap<String, String>, String) {
    assert_eq!(response.declared_type(), Some("std.web.Response"));
    let status = response
        .field("status")
        .and_then(Value::as_int)
        .expect("an Int status");
    let headers = response
        .field("headers")
        .and_then(Value::entries)
        .expect("a Map of headers")
        .map(|(key, value)| {
            let MapKey::Str(name) = key else {
                panic!("a header name that is not a String: {key:?}");
            };
            (
                name.to_string(),
                value.as_str().expect("a String header value").to_string(),
            )
        })
        .collect();
    let body = response
        .field("body")
        .and_then(Value::as_str)
        .expect("a String body")
        .to_string();
    (status, headers, body)
}

#[test]
fn an_embedder_builds_a_std_web_request_and_reads_the_response() {
    for backend in BOTH {
        let answer = handle(
            backend,
            request(
                "std.web.Request",
                "GET",
                &[("user-agent", "curl/8"), ("host", "example.test")],
                "",
            ),
        )
        .unwrap_or_else(|error| panic!("{backend:?}: {}", error.message));
        let (status, headers, body) = read(&answer);
        assert_eq!(status, 200, "{backend:?}");
        assert_eq!(body, "<p>/a/b for curl/8</p>", "{backend:?}");
        assert_eq!(
            headers.get("content-type").map(String::as_str),
            Some("text/html; charset=utf-8"),
            "{backend:?}"
        );
        assert_eq!(
            headers.get("x-handled-by").map(String::as_str),
            Some("cove"),
            "{backend:?}: `withHeader` lowercases the name it sets"
        );
        for (name, value) in [
            ("x-content-type-options", "nosniff"),
            ("x-frame-options", "DENY"),
            ("referrer-policy", "same-origin"),
            ("cache-control", "no-store"),
        ] {
            assert_eq!(
                headers.get(name).map(String::as_str),
                Some(value),
                "{backend:?}: {name}"
            );
        }
        assert!(
            headers
                .get("content-security-policy")
                .is_some_and(|policy| policy.starts_with("default-src 'none';")),
            "{backend:?}: {headers:?}"
        );
    }
}

#[test]
fn a_posted_form_is_read_and_answered_with_a_redirect_under_the_prefix() {
    for backend in BOTH {
        let posted = |headers: &[(&str, &str)]| {
            let answer = handle(
                backend,
                request("std.web.Request", "POST", headers, "name=J%C3%BCrgen"),
            )
            .unwrap_or_else(|error| panic!("{backend:?}: {}", error.message));
            read(&answer)
        };
        let (status, headers, body) = posted(&[
            ("sec-fetch-site", "same-origin"),
            ("x-forwarded-prefix", "/hello-app"),
        ]);
        assert_eq!(status, 303, "{backend:?}");
        assert_eq!(
            headers.get("location").map(String::as_str),
            Some("/hello-app/hello/J%C3%BCrgen"),
            "{backend:?}"
        );
        assert_eq!(body, "", "{backend:?}");

        let (status, _, body) = posted(&[("sec-fetch-site", "cross-site")]);
        assert_eq!(
            (status, body.as_str()),
            (403, "not from this site\n"),
            "{backend:?}"
        );
    }
}

/// The mistake an embedder migrating from a host `web` module makes: the
/// value still carries the host type's name. It is refused before anything
/// runs, and the refusal names the type it should have carried.
#[test]
fn a_request_under_the_old_host_name_is_refused_naming_std_web() {
    for backend in BOTH {
        let error = handle(backend, request("web.Request", "GET", &[], ""))
            .expect_err("a `web.Request` is not a `std.web.Request`");
        assert_eq!(
            error.message,
            "`app.handle` was given `web.Request` as argument 1, but it declares \
             `std.web.Request` there",
            "{backend:?}"
        );
    }
}

/// `std.web.Request` is a declared struct, so it is followed into its fields:
/// a value carrying them in another order, or missing one, is refused — a
/// host type was taken at its word about what it held.
#[test]
fn a_request_with_the_wrong_fields_is_refused_by_shape() {
    for backend in BOTH {
        let missing = Value::structure(
            "std.web.Request",
            [
                ("method", Value::string("GET")),
                ("path", Value::string("/")),
                ("headers", strings(&[])),
                ("body", Value::string("")),
            ],
        );
        let error = handle(backend, missing).expect_err("a request with no `query`");
        assert_eq!(
            error.message,
            "`app.handle` was given a `std.web.Request` carrying `method`, `path`, \
             `headers`, `body` as argument 1, but `std.web.Request` declares `method`, \
             `path`, `query`, `headers`, `body`, in that order",
            "{backend:?}"
        );

        let mistyped = Value::structure(
            "std.web.Request",
            [
                ("method", Value::string("GET")),
                ("path", Value::string("/")),
                ("query", strings(&[])),
                (
                    "headers",
                    Value::map([(MapKey::Str("x".into()), Value::int(1))]),
                ),
                ("body", Value::string("")),
            ],
        );
        let error = handle(backend, mistyped).expect_err("a header value that is an Int");
        assert_eq!(
            error.message,
            "`app.handle` was given `Int` at `.headers{value}` of argument 1, but it \
             declares `String` there",
            "{backend:?}"
        );
    }
}
