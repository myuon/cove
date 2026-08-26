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
//! with at run time — [`cove_runtime::HostApi::module_schema`] answers with
//! it, and `HostRegistry` enforces it — so the description the checker reads
//! and the one the boundary holds a call to cannot drift apart. Restating
//! one of them is what drift is made of.
//!
//! Nothing is serialized. The in-process embedding API is the case that
//! exists, and a format for describing a host module out of process should
//! be invented when something outside a process needs to read one.

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
    /// Warnings from both halves are carried on the returned program rather
    /// than mixed into its errors, so a caller can report them without
    /// having to decide which of them stopped anything. A failure reports
    /// the errors first and the warnings after, because a reader looking for
    /// what went wrong should not have to read past what merely could.
    pub fn compile(&self, package: &Package) -> Result<Program, Vec<Diagnostic>> {
        let mut program = self.resolve(package)?;
        let (errors, warnings): (Vec<Diagnostic>, Vec<Diagnostic>) = self
            .check(package, &program)
            .into_iter()
            .partition(|d| d.severity == cove_diag::Severity::Error);
        if !errors.is_empty() {
            let mut items = errors;
            items.extend(warnings);
            return Err(items);
        }
        program.warnings.extend(warnings);
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
            program.warnings.is_empty(),
            "a described module warns about nothing: {:?}",
            program.warnings
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
            .warnings
            .iter()
            .map(|item| render(&sources, item))
            .collect();
        assert!(
            rendered.contains("no Host API schema describes the host module `company`"),
            "{rendered}"
        );
        assert!(
            program
                .warnings
                .iter()
                .all(|item| item.severity == Severity::Warning),
            "an undescribed host module warns rather than fails"
        );
    }
}
