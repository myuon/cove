//! The checking pipeline, and the one thing an embedder configures about it.
//!
//! `cove check` reads the Host API schemas of the modules the toolchain
//! ships, so a call into `http.fetch` is checked at the call site: its
//! arity, its argument types, the type it produces, the fields of the types
//! it names, and the capability it costs. A module an embedder registers
//! used to get none of that. It exists only at run time, so the checker had
//! nothing to read, every call into it produced an unknown type, and the
//! Host API boundary was the first thing that looked at the call at all.
//!
//! Embedding is a primary use of Cove, so that was a gap rather than a
//! design: an embedder can already write a perfectly precise
//! [`ModuleSchema`], and what was missing was somewhere to hand it. A
//! [`Compiler`] is that somewhere.
//!
//! ```no_run
//! # use cove_schema::ModuleSchema;
//! # use cove_sema::{package, Compiler};
//! # fn main() -> Result<(), Vec<cove_diag::Diagnostic>> {
//! # const COMPANY: ModuleSchema = ModuleSchema {
//! #     name: "company", capability: "company",
//! #     operations: &[], types: &[], resources: &[],
//! # };
//! # let mut sources = cove_diag::SourceMap::new();
//! # let package = package::load(std::path::Path::new("."), &mut sources)?;
//! let program = Compiler::new().with_host_schema(COMPANY).compile(&package)?;
//! # Ok(())
//! # }
//! ```
//!
//! The value handed over is the same `ModuleSchema` the module registers
//! with at run time — `cove_runtime::HostApi::module_schema` answers with
//! it, and `HostRegistry` enforces it — so the description the checker reads
//! and the one the boundary holds a call to cannot drift apart. Restating
//! one of them is what drift is made of.
//!
//! # No `cove` command can be handed one, and none should be
//!
//! This used to end "nothing is serialized: a format for describing a host
//! module out of process should be invented when something outside a process
//! needs to read one".
//! [Issue #151](https://github.com/myuon/cove/issues/151) is a candidate for
//! that something, and the answer it settled on is no. It is worth stating
//! here rather than in an issue, because this is where an embedder meets the
//! question.
//!
//! The complaint is real. A rule package written against an embedder's module
//! can be checked by the embedder, in Rust, and cannot be checked by the
//! toolchain the person who wrote it has: `cove check` reports
//! `cove::resolve::unchecked_host` and stops at the boundary, and `cove test`
//! cannot run a test that touches the module at all. The rule author's whole
//! toolchain is `cove fmt`, `cove check` and `cove test`, and two of the three
//! stop where the embedder begins.
//!
//! What would not fix it is a `[hosts]` key in `cove.toml` naming a serialized
//! schema. The whole of what makes a [`ModuleSchema`] worth anything is that
//! the value the checker reads and the value the boundary enforces are one
//! value; a description in a config file is a second one, written by hand in
//! another vocabulary, kept true by whoever remembered. A checker reading the
//! second while the run enforces the first reports exactly the failure ADR
//! 0017 exists to prevent, with the authority of having checked. Generating
//! the file from the `const` removes the drift and leaves the rest: the copy
//! is stale the moment the embedder rebuilds, and nothing in the package can
//! tell.
//!
//! `cove test` is what settles it, though, and it settles it for any format.
//! A schema lets the checker *check* a call into `reviews`; it lets nothing
//! *run* one, because what answers a call is an implementation, an
//! implementation is Rust, and no description carries one. A `cove` that had
//! been handed a schema would check a rule package it still could not test —
//! one of the two commands the issue is about, and the one a rule author uses
//! most.
//!
//! So the toolchain for a package written against an embedder's module is the
//! embedder's to provide, and providing it is one line, because the value that
//! describes the module is the value the registry was registered with:
//!
//! ```ignore
//! let package = cove_sema::package::load(root, &mut sources)?;
//! let program = Compiler::new()
//!     .with_host_schemas(hosts.module_schemas())
//!     .compile(&package)?;
//! for notice in &program.notices {
//!     eprint!("{}", cove_diag::render(&sources, notice));
//! }
//! ```
//!
//! `examples/rules/host/src/bin/check.rs` is that, whole: it is the `cove
//! check` of an application that embeds Cove, and it reads the same `REVIEWS`
//! its `HostApi` answers with, so the two cannot drift. An embedder that wants
//! the test runner too registers its hosts beside the schemas and runs the
//! package's `test fn` declarations against them — which needs the
//! implementation, and is the same conclusion reached from the other end.
//!
//! The `unchecked_host` warning stays, and it is accurate: a `cove check` that
//! was handed no description has not checked those calls, and saying so is
//! better than a silence that reads like a proof. Its `help` already names the
//! API to hand the schema to; what it cannot say in one line, and what this
//! section is, is why there is no flag to hand it to instead.

use cove_diag::Diagnostic;
use cove_schema::{HostSchemas, ModuleSchema};

use crate::package::Package;
use crate::resolve::Program;
use crate::{resolve, typeck};

/// A checking pipeline, and the Host API schemas it reads.
///
/// The default reads the shipped schemas and nothing else, which is what
/// every `cove` command does. An embedder adds the modules it registers.
#[derive(Clone, Debug, Default)]
pub struct Compiler {
    schemas: HostSchemas,
}

impl Compiler {
    /// A pipeline that reads the shipped Host API schemas.
    pub fn new() -> Compiler {
        Compiler::default()
    }

    /// Adds one host module's description.
    ///
    /// The schema is checked against exactly as a shipped module's is: the
    /// module may be named by a `use`, no package module may shadow it,
    /// calls into it are checked at the call site, its types may be written
    /// and initialized, its resources answer the operations it declares, and
    /// a function reaching it requires the capability the schema names.
    pub fn with_host_schema(mut self, schema: ModuleSchema) -> Compiler {
        self.schemas.insert(schema);
        self
    }

    /// Adds every host module in `schemas`.
    ///
    /// This is what pairs a checker with a set of registered hosts in one
    /// line: `cove_runtime::HostRegistry::module_schemas` hands back the
    /// table every registered module declared itself with, and passing it
    /// here checks the program against the same descriptions the run will
    /// enforce.
    pub fn with_host_schemas(
        mut self,
        schemas: impl IntoIterator<Item = ModuleSchema>,
    ) -> Compiler {
        self.schemas.extend(schemas);
        self
    }

    /// Reads `schemas` and nothing else, replacing whatever this pipeline
    /// was reading.
    ///
    /// This is how an embedding whose registry is its own says so.
    /// [`with_host_schema`](Compiler::with_host_schema) and
    /// [`with_host_schemas`](Compiler::with_host_schemas) *add* to the
    /// shipped tables, which is right for a run that registers the shipped
    /// hosts and some of its own. A run that registers neither wants
    /// `cove_schema::HostSchemas::only`, so that a `use files` in a program
    /// it is about to run is reported by the checker rather than by the
    /// boundary:
    ///
    /// ```ignore
    /// let program = Compiler::new()
    ///     .with_schemas(HostSchemas::only(hosts.module_schemas()))
    ///     .compile(&package)?;
    /// ```
    pub fn with_schemas(mut self, schemas: HostSchemas) -> Compiler {
        self.schemas = schemas;
        self
    }

    /// The host modules this pipeline can see.
    pub fn host_schemas(&self) -> &HostSchemas {
        &self.schemas
    }

    /// Resolves `package`: names, imports, capabilities, and the call graph.
    pub fn resolve(&self, package: &Package) -> Result<Program, Vec<Diagnostic>> {
        resolve::resolve_with(package, &self.schemas)
    }

    /// Type-checks an already resolved `package`, reporting errors and
    /// warnings together.
    pub fn check(&self, package: &Package, program: &Program) -> Vec<Diagnostic> {
        typeck::check_with(package, program, &self.schemas)
    }

    /// Resolves and type-checks `package`, which is what `cove check` does.
    ///
    /// The returned program carries [`Program::facts`]: the type the checker
    /// settled for every expression, and the declaration each resolved
    /// method call reaches.
    ///
    /// Warnings and notes from both halves are carried on the returned
    /// program's [`Program::notices`] rather than mixed into its errors, so
    /// a caller can report them without having to decide which of them
    /// stopped anything. A failure reports the errors first and the warnings
    /// after, because a reader looking for what went wrong should not have
    /// to read past what merely could.
    pub fn compile(&self, package: &Package) -> Result<Program, Vec<Diagnostic>> {
        let mut program = self.resolve(package)?;
        let (diagnostics, facts) = typeck::check_facts(package, &program, &self.schemas);
        let (errors, warnings): (Vec<Diagnostic>, Vec<Diagnostic>) = diagnostics
            .into_iter()
            .partition(|d| d.severity == cove_diag::Severity::Error);
        if !errors.is_empty() {
            let mut items = errors;
            items.extend(warnings);
            return Err(items);
        }
        program.notices.extend(warnings);
        // A program that checked carries what the check worked out. Nothing
        // downstream has to walk the tree again to learn a type, which ADR
        // 0019 makes the rule for the lowering and which holds for anything
        // else reading a checked package.
        program.facts = facts;
        Ok(program)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use cove_diag::{render, Severity, SourceMap};
    use cove_schema::{Effect, FieldSchema, HostType, OperationSchema, ResourceSchema, TypeSchema};

    use super::*;
    use crate::capability::Capability;
    use crate::config::Config;
    use crate::package::{Module, Unit};

    /// A host module no toolchain ships: one operation, one resource, one
    /// type of its own, and a capability that is not its own name.
    const COMPANY: ModuleSchema = ModuleSchema {
        name: "company",
        capability: "directory",
        operations: &[
            OperationSchema {
                name: "employee",
                params: &[HostType::String],
                variadic: false,
                result: HostType::Result(&HostType::Named("company.Employee"), &HostType::Error),
                capability: "directory",
                effect: Effect::Read,
                cancellable: false,
                recordable: true,
                result_is_task_safe: true,
            },
            OperationSchema {
                name: "roster",
                params: &[],
                variadic: false,
                result: HostType::Result(&HostType::Named("company.Roster"), &HostType::Error),
                capability: "directory",
                effect: Effect::Read,
                cancellable: false,
                recordable: true,
                result_is_task_safe: true,
            },
        ],
        types: &[TypeSchema {
            name: "Employee",
            cases: &[],
            fields: &[
                FieldSchema {
                    name: "name",
                    ty: HostType::String,
                },
                FieldSchema {
                    name: "seniority",
                    ty: HostType::Int,
                },
            ],
        }],
        resources: &[ResourceSchema {
            name: "Roster",
            task_safe: true,
            operations: &[OperationSchema {
                name: "count",
                params: &[],
                variadic: false,
                result: HostType::Int,
                capability: "directory",
                effect: Effect::Read,
                cancellable: false,
                recordable: true,
                result_is_task_safe: true,
            }],
        }],
    };

    /// Builds a one-module package out of `text`, without touching disk.
    fn package_of(text: &str) -> (SourceMap, Package) {
        package_of_modules(&[("app", text)])
    }

    /// Builds a package of one file per named module, without touching disk.
    fn package_of_modules(modules: &[(&str, &str)]) -> (SourceMap, Package) {
        let mut sources = SourceMap::new();
        let mut loaded = BTreeMap::new();
        for (name, text) in modules {
            let path = PathBuf::from(format!("{name}/main.cove"));
            let file = sources.add(path.clone(), *text);
            let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
            loaded.insert(
                (*name).to_string(),
                Module {
                    name: (*name).to_string(),
                    dir: PathBuf::from(name),
                    units: vec![Unit { file, path, ast }],
                },
            );
        }
        (
            sources,
            Package {
                root: PathBuf::new(),
                config: Config::default(),
                modules: loaded,
            },
        )
    }

    const WELL_TYPED: &str = "\
use company

/// Reports how senior one employee is.
export fn seniority(id: String) -> Result<Int, Error> {
  let found = company.employee(id)?
  Ok(found.seniority)
}
";

    #[test]
    fn a_supplied_schema_checks_a_call_into_a_module_nothing_ships() {
        let (_, package) = package_of(WELL_TYPED);
        let program = Compiler::new()
            .with_host_schema(COMPANY)
            .compile(&package)
            .expect("a well-typed program against a supplied schema checks");
        assert!(
            program.notices.is_empty(),
            "a described module warns about nothing: {:?}",
            program.notices
        );
    }

    /// The capability a call requires is the one the schema declares, not the
    /// module's name: `company` is gated on `directory`.
    ///
    /// This is also what `cove outline` prints as a function's `requires`
    /// line, which reads `required_capabilities` and nothing else, so a
    /// custom module appears there for the same reason a shipped one does.
    #[test]
    fn a_supplied_schema_declares_the_capability_a_call_requires() {
        let (_, package) = package_of(WELL_TYPED);
        let program = Compiler::new()
            .with_host_schema(COMPANY)
            .compile(&package)
            .expect("checks");
        assert_eq!(
            program.modules["app"].functions["seniority"].required_capabilities,
            [Capability::new("directory")].into_iter().collect()
        );
    }

    /// The whole point: a mistake in a call into a custom module is an error
    /// at its call site, exactly as it is for a shipped one.
    #[test]
    fn a_supplied_schema_reports_a_mistake_at_the_call_site() {
        let (sources, package) = package_of(
            "\
use company

/// Passes an `Int` where the schema declares a `String`.
export fn seniority(id: Int) -> Result<Int, Error> {
  let found = company.employee(id)?
  Ok(found.tenure)
}
",
        );
        let items = Compiler::new()
            .with_host_schema(COMPANY)
            .compile(&package)
            .expect_err("an argument the schema does not declare is an error");
        let rendered: String = items.iter().map(|item| render(&sources, item)).collect();
        assert!(
            rendered.contains("expected `String`, found `Int`")
                && rendered.contains("argument `#1` is `String`"),
            "{rendered}"
        );
        assert!(
            rendered.contains("`company.Employee` has no field `tenure`"),
            "{rendered}"
        );
    }

    /// A supplied module owns its name as completely as a shipped one does.
    /// Modules resolve before hosts, so a package module named `company`
    /// would make the embedder's module unreachable for the whole package,
    /// silently -- which is the reason shipped names are refused too.
    #[test]
    fn a_package_module_may_not_shadow_a_supplied_host_module() {
        let (sources, package) = package_of_modules(&[
            ("company", "/// Does something.\nexport fn thing() {\n}\n"),
            ("app", "use company.thing\n"),
        ]);
        let items = Compiler::new()
            .with_host_schema(COMPANY)
            .compile(&package)
            .expect_err("a package module named after a host module is refused");
        let rendered: String = items.iter().map(|item| render(&sources, item)).collect();
        assert!(
            rendered.contains("module `company` has the same name as the host module `company`"),
            "{rendered}"
        );
    }

    /// A handle a supplied schema declares answers the operations that
    /// schema gives it, and the checker knows what each of them produces.
    #[test]
    fn a_supplied_schema_checks_an_operation_on_a_resource_it_declares() {
        let (_, package) = package_of(
            "\
use company

/// Counts the whole directory through a handle the host keeps.
export fn size() -> Result<Int, Error> {
  let roster = company.roster()?
  Ok(roster.count())
}
",
        );
        Compiler::new()
            .with_host_schema(COMPANY)
            .compile(&package)
            .expect("a resource operation the schema declares checks");
    }

    #[test]
    fn a_supplied_schema_reports_an_operation_its_resource_does_not_answer() {
        let (sources, package) = package_of(
            "\
use company

/// Calls an operation `company.Roster` does not answer.
export fn size() -> Result<Int, Error> {
  let roster = company.roster()?
  Ok(roster.total())
}
",
        );
        let items = Compiler::new()
            .with_host_schema(COMPANY)
            .compile(&package)
            .expect_err("an operation the resource does not answer is an error");
        let rendered: String = items.iter().map(|item| render(&sources, item)).collect();
        assert!(
            rendered.contains("`company.Roster` has no operation `total`")
                && rendered.contains("answers `count`"),
            "{rendered}"
        );
    }

    /// Without the schema the same program still compiles, because a module
    /// the checker cannot see is left to the boundary. It says so, though.
    #[test]
    fn a_module_no_schema_describes_is_left_to_the_boundary_with_a_warning() {
        let (sources, package) = package_of(WELL_TYPED);
        let program = Compiler::new()
            .compile(&package)
            .expect("an unknown host module is not an error");
        let rendered: String = program
            .notices
            .iter()
            .map(|item| render(&sources, item))
            .collect();
        assert!(
            rendered.contains("no Host API schema describes the host module `company`"),
            "{rendered}"
        );
        assert!(
            program
                .notices
                .iter()
                .all(|item| item.severity == Severity::Warning),
            "an undescribed host module warns rather than fails"
        );
    }
}
