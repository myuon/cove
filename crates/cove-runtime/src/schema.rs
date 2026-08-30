//! The Host API schema, and what a value has to be for a declared type to
//! admit it.
//!
//! The schema itself is [`cove_schema`], a crate below both this one and the
//! compiler, because ADR 0001 makes it "shared by the compiler, runtime, and
//! CLI" and the dependency between those two runs one way. Everything it
//! declares is re-exported here, so a host written against the runtime still
//! names one crate: `cove_runtime::schema::HostType` is
//! `cove_schema::HostType`.
//!
//! What is *not* there is [`Admits`], which is the only part of the schema
//! that needs values. A `HostType` is a description and a [`Value`] is the
//! runtime's; this is where the two meet, so it lives on the side of the
//! boundary where values live. The compiler answers the same question against
//! its own `Ty`, at the call site, where a mistake still has a span.

pub use cove_schema::hosts;
pub use cove_schema::{
    module, shipped, Effect, FieldSchema, HostType, ModuleSchema, OperationSchema, ResourceSchema,
    TypeSchema,
};

use cove_schema::builtins::{ERROR, ERR_CASE, NONE_CASE, OK_CASE, OPTION, RESULT, SOME_CASE};

use crate::value::Value;

/// Whether a value is one a declared type admits.
///
/// This is an extension of [`HostType`] rather than an inherent method
/// because the type lives in a crate that has no values to check. Everything
/// it says about *which* values a type admits is the schema's; only the
/// walking of a [`Value`] is this crate's.
pub trait Admits {
    /// Whether `value` is one this type admits, and where it stops being one
    /// when it is not.
    ///
    /// The check follows this type's own recursion rather than looking only
    /// at the outermost constructor, because a shallow check would admit an
    /// `Array<Int>` where an `Array<String>` was declared and the schema says
    /// more than "an array". [`HostType::Any`] admits everything, which is
    /// not a hole: it is the type of an operation whose meaning does not
    /// depend on which value it was given — the work `clock.timeout` bounds,
    /// the body `clock.every` repeats — so there is nothing there to check.
    ///
    /// [`HostType::Named`] is checked by name. Every value carries the
    /// qualified type it was built with and a [`crate::host::ResourceHandle`]
    /// carries the module and kind it was issued for, so the comparison is
    /// one the value itself can answer, with no registry to consult and no
    /// second lookup on a path every host call takes. What that leaves
    /// unchecked is a [`TypeSchema`]'s *fields*: a value calling itself an
    /// `http.Response` is taken at its word about what is inside it. See ADR
    /// 0013's amendment for why that line is drawn there.
    fn admits(&self, value: &Value) -> Result<(), Mismatch>;
}

impl Admits for HostType {
    fn admits(&self, value: &Value) -> Result<(), Mismatch> {
        match (self, value) {
            (HostType::Any, _)
            | (HostType::Unit, Value::Unit)
            | (HostType::Bool, Value::Bool(_))
            | (HostType::Int, Value::Int(_))
            | (HostType::String, Value::Str(_))
            | (HostType::Duration, Value::Duration(_)) => Ok(()),
            // The builtin error struct, which is what `Err` carries
            // everywhere a host declares one.
            (HostType::Error, Value::Struct(fields)) if &*fields.type_name == ERROR.name => Ok(()),
            (HostType::Array(item), Value::Array(items)) => {
                for (index, element) in items.iter().enumerate() {
                    item.admits(element)
                        .map_err(|mismatch| mismatch.inside(&format!("[{index}]")))?;
                }
                Ok(())
            }
            // A `Set` element and a `Map` key are `MapKey`s by construction,
            // so the restriction a schema declares them under is already kept
            // and what is left to check is the type. Each is read back as the
            // value it stands for, which costs an allocation for a `Str` key
            // and nothing for a scalar one; it is what lets one `admits` walk
            // answer for both halves of a map rather than a second walk over a
            // second vocabulary.
            (HostType::Set(item), Value::Set(items)) => {
                for (index, element) in items.iter().enumerate() {
                    item.admits(&element.to_value())
                        .map_err(|mismatch| mismatch.inside(&format!("[{index}]")))?;
                }
                Ok(())
            }
            (HostType::Map(key, value), Value::Map(entries)) => {
                for (index, (found, held)) in entries.iter().enumerate() {
                    key.admits(&found.to_value())
                        .map_err(|mismatch| mismatch.inside(&format!("key[{index}]")))?;
                    value
                        .admits(held)
                        .map_err(|mismatch| mismatch.inside(&format!("[{found}]")))?;
                }
                Ok(())
            }
            // The two builtin enums, whose cases `cove_schema::builtins`
            // declares: a case's name and how much it carries are read off
            // the same entry `Value::some` and `Value::ok` build from.
            (HostType::Option(some), Value::Enum(case)) if &*case.type_name == OPTION.name => {
                match (&*case.case, case.payload.as_slice()) {
                    (name, [inner]) if name == SOME_CASE.name => some
                        .admits(inner)
                        .map_err(|m| m.inside(&SOME_CASE.wildcard_pattern())),
                    (name, []) if name == NONE_CASE.name => Ok(()),
                    _ => Err(mismatched(self, value)),
                }
            }
            (HostType::Result(ok, error), Value::Enum(case)) if &*case.type_name == RESULT.name => {
                match (&*case.case, case.payload.as_slice()) {
                    (name, [inner]) if name == OK_CASE.name => ok
                        .admits(inner)
                        .map_err(|m| m.inside(&OK_CASE.wildcard_pattern())),
                    (name, [inner]) if name == ERR_CASE.name => error
                        .admits(inner)
                        .map_err(|m| m.inside(&ERR_CASE.wildcard_pattern())),
                    _ => Err(mismatched(self, value)),
                }
            }
            // A handle names its module and its kind, which together are the
            // qualified name a signature writes.
            (HostType::Named(name), Value::Resource(handle))
                if handle.qualified_type() == *name =>
            {
                Ok(())
            }
            (HostType::Named(name), Value::Struct(fields)) if &*fields.type_name == *name => Ok(()),
            (HostType::Named(name), Value::Enum(case)) if &*case.type_name == *name => Ok(()),
            _ => Err(mismatched(self, value)),
        }
    }
}

/// A declared type and a value that is none of it, as a mismatch at the point
/// the two part company.
fn mismatched(declared: &HostType, value: &Value) -> Mismatch {
    Mismatch {
        path: String::new(),
        expected: *declared,
        found: value.type_name(),
    }
}

/// Which part of a call a mismatch was found in.
///
/// The two are the same check on the same table read from opposite sides: a
/// wrong result is the host breaking its own word, and a wrong argument is
/// the program breaking it, so the diagnostic says which happened rather than
/// leaving the reader to work it out from the operation's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// The value the host answered with.
    Result,
    /// The argument at this position, counted from one as a reader counts.
    Argument(usize),
}

/// Where a value stopped agreeing with the type declared for it, and what was
/// found there instead.
///
/// The disagreement is reported where it happens rather than at the top of
/// the value: an operation declaring `Result<Array<String>, Error>` that
/// answers `Ok([3])` disagrees at one element, and saying which one is the
/// difference between a diagnostic the host's author can act on and one that
/// says only that the two do not match. Nothing here is built until a value
/// fails, so the path costs a run that never fails nothing at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    /// How the offending part is reached from the whole value, such as
    /// `Ok(_)[0]`. Empty when the value itself, rather than something nested
    /// inside it, is the disagreement.
    pub path: String,
    /// The type declared at that point.
    pub expected: HostType,
    /// What was found there, named the way a diagnostic names a value.
    pub found: String,
}

impl Mismatch {
    /// Re-anchors this mismatch one level out, where the part that disagrees
    /// is reached by `step`.
    fn inside(mut self, step: &str) -> Mismatch {
        self.path.insert_str(0, step);
        self
    }

    /// The disagreement, phrased for a diagnostic about the operation
    /// `shown` names.
    pub fn describe(&self, shown: &str, part: Part) -> String {
        let (verb, whole) = match part {
            Part::Result => ("answered", "its result".to_string()),
            Part::Argument(position) => ("was given", format!("argument {position}")),
        };
        let (found, expected) = (&self.found, &self.expected);
        if self.path.is_empty() {
            match part {
                // A result is the whole of what an operation answers, so
                // naming the place it was found would add nothing.
                Part::Result => {
                    format!("`{shown}` {verb} `{found}`, but its schema declares `{expected}`")
                }
                Part::Argument(_) => format!(
                    "`{shown}` {verb} `{found}` as {whole}, but its schema declares `{expected}` there"
                ),
            }
        } else {
            format!(
                "`{shown}` {verb} `{found}` at `{}` of {whole}, but its schema declares `{expected}` there",
                self.path
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::ResourceHandle;
    use crate::value::{MapKey, StructValue};
    use std::rc::Rc;

    // ------------------------------------------- what a declared type admits
    //
    // ADR 0001 asks each operation to describe its argument, result, and
    // error types, and ADR 0013's amendment makes both the result and the
    // arguments ones the boundary holds a call to. These pin the vocabulary
    // of that check; `host.rs` pins what the boundary does with it.

    /// The one kind of resource a host in these tests can open.
    static CONNECTION: ResourceSchema = ResourceSchema {
        name: "Connection",
        task_safe: true,
        operations: &[],
    };

    /// A struct value named the way a host builds one: qualified by module.
    fn host_struct(type_name: &str, fields: Vec<(&str, Value)>) -> Value {
        Value::Struct(Rc::new(StructValue {
            type_name: type_name.into(),
            fields: fields
                .into_iter()
                .map(|(name, value)| (name.into(), value))
                .collect(),
            opaque: false,
        }))
    }

    #[test]
    fn a_declared_type_admits_the_value_it_names() {
        assert!(HostType::Unit.admits(&Value::Unit).is_ok());
        assert!(HostType::Bool.admits(&Value::Bool(true)).is_ok());
        assert!(HostType::Int.admits(&Value::Int(3)).is_ok());
        assert!(HostType::String.admits(&Value::Str("text".into())).is_ok());
        assert!(HostType::Duration.admits(&Value::Duration(500)).is_ok());
        assert!(HostType::Error.admits(&Value::error("gone")).is_ok());
    }

    #[test]
    fn a_value_of_another_type_is_a_mismatch_where_the_two_part_company() {
        let mismatch = HostType::String
            .admits(&Value::Int(3))
            .expect_err("an `Int` is not a `String`");

        assert_eq!(mismatch.path, "");
        assert_eq!(mismatch.expected, HostType::String);
        assert_eq!(mismatch.found, "Int");
        assert_eq!(
            mismatch.describe("wayward.read", Part::Result),
            "`wayward.read` answered `Int`, but its schema declares `String`"
        );
    }

    /// The same mismatch, read from the other side of the call. A wrong
    /// argument is the program's mistake rather than the host's, so it is
    /// phrased as one.
    #[test]
    fn a_mismatch_names_the_argument_it_was_found_in() {
        let mismatch = HostType::String
            .admits(&Value::Int(3))
            .expect_err("an `Int` is not a `String`");

        assert_eq!(
            mismatch.describe("documents.read", Part::Argument(1)),
            "`documents.read` was given `Int` as argument 1, but its schema declares `String` there"
        );
    }

    #[test]
    fn a_declared_type_is_followed_all_the_way_down() {
        let declared = HostType::Result(&HostType::Array(&HostType::String), &HostType::Error);

        assert!(declared
            .admits(&Value::ok(Value::Array(
                vec![Value::Str("one".into())].into()
            )))
            .is_ok());
        assert!(
            declared.admits(&Value::err(Value::error("gone"))).is_ok(),
            "the declared error type is the one inside `Err`"
        );

        let mismatch = declared
            .admits(&Value::ok(Value::Array(
                vec![Value::Str("one".into()), Value::Int(2)].into(),
            )))
            .expect_err("an `Int` among the declared strings is not admitted");
        assert_eq!(mismatch.path, "Ok(_)[1]");
        assert_eq!(mismatch.expected, HostType::String);
        assert_eq!(
            mismatch.describe("wayward.list", Part::Result),
            "`wayward.list` answered `Int` at `Ok(_)[1]` of its result, but its schema declares `String` there"
        );
        assert_eq!(
            mismatch.describe("wayward.list", Part::Argument(2)),
            "`wayward.list` was given `Int` at `Ok(_)[1]` of argument 2, but its schema declares `String` there"
        );
    }

    /// A `Set` the host built crosses whole, which is what having the variant
    /// is for: the element type is followed exactly as an `Array`'s is.
    #[test]
    fn a_set_is_followed_into_its_elements() {
        let declared = HostType::Set(&HostType::String);
        assert!(declared
            .admits(&Value::set([
                MapKey::Str("docs".to_string()),
                MapKey::Str("migration".to_string()),
            ]))
            .is_ok());
        assert!(
            declared.admits(&Value::set([])).is_ok(),
            "an empty set is a set of anything"
        );

        let mismatch = declared
            .admits(&Value::set([
                MapKey::Str("docs".to_string()),
                MapKey::Int(2),
            ]))
            .expect_err("an `Int` among the declared strings is not admitted");
        // Ascending key order, which is the order a `Set` has: the `Int` sorts
        // before the `Str`.
        assert_eq!(mismatch.path, "[0]");
        assert_eq!(mismatch.expected, HostType::String);
        assert_eq!(
            mismatch.describe("reviews.labels", Part::Result),
            "`reviews.labels` answered `Int` at `[0]` of its result, but its schema declares `String` there"
        );
    }

    /// A `Map` has two halves and the diagnostic says which one disagreed: a
    /// key is named by its position and a value by the key it was held under,
    /// because that is how a reader finds each of them again.
    #[test]
    fn a_map_is_followed_into_both_of_its_halves() {
        let declared = HostType::Map(&HostType::String, &HostType::Int);
        assert!(declared
            .admits(&Value::map([(
                MapKey::Str("breaking-change".to_string()),
                Value::Int(3),
            )]))
            .is_ok());

        let wrong_value = declared
            .admits(&Value::map([(
                MapKey::Str("breaking-change".to_string()),
                Value::Str("three".into()),
            )]))
            .expect_err("a `String` is not the declared `Int`");
        assert_eq!(wrong_value.path, "[breaking-change]");
        assert_eq!(wrong_value.expected, HostType::Int);

        let wrong_key = declared
            .admits(&Value::map([(MapKey::Int(3), Value::Int(3))]))
            .expect_err("an `Int` is not the declared `String`");
        assert_eq!(wrong_key.path, "key[0]");
        assert_eq!(wrong_key.expected, HostType::String);
    }

    /// The two are ordinary compound types everywhere else a compound type is
    /// read, so a `Set` nested in a `Result` is followed through both.
    #[test]
    fn a_set_nested_in_a_result_is_reached_through_it() {
        let declared = HostType::Result(&HostType::Set(&HostType::String), &HostType::Error);
        assert!(declared
            .admits(&Value::ok(Value::set([MapKey::Str("docs".to_string())])))
            .is_ok());
        assert_eq!(
            declared
                .admits(&Value::ok(Value::set([MapKey::Int(1)])))
                .expect_err("the element type is checked through the `Ok`")
                .path,
            "Ok(_)[0]"
        );
        assert_eq!(
            declared
                .admits(&Value::ok(Value::Array(vec![].into())))
                .expect_err("an array is not a set")
                .found,
            "Array"
        );
    }

    #[test]
    fn an_option_is_admitted_by_either_case() {
        let declared = HostType::Option(&HostType::String);

        assert!(declared.admits(&Value::none()).is_ok());
        assert!(declared
            .admits(&Value::some(Value::Str("set".into())))
            .is_ok());
        assert_eq!(
            declared
                .admits(&Value::some(Value::Int(3)))
                .expect_err("`Some(3)` is not an `Option<String>`")
                .path,
            "Some(_)"
        );
    }

    #[test]
    fn any_admits_whatever_it_is_given() {
        assert!(HostType::Any.admits(&Value::Int(3)).is_ok());
        assert!(HostType::Any.admits(&Value::Unit).is_ok());
        assert!(HostType::Any
            .admits(&host_struct("demo.Point", Vec::new()))
            .is_ok());
        assert!(HostType::Array(&HostType::Any)
            .admits(&Value::Array(vec![Value::Int(1), Value::Unit].into()))
            .is_ok());
    }

    #[test]
    fn a_named_type_is_checked_by_the_name_the_value_carries() {
        let declared = HostType::Named("http.Response");
        let response = host_struct(
            "http.Response",
            vec![
                ("status", Value::Int(200)),
                ("body", Value::Str("ok".into())),
            ],
        );
        assert!(declared.admits(&response).is_ok());

        // The name is what is checked; the fields behind it are not. A
        // `Response` with nothing inside it is still a `Response` here.
        assert!(declared
            .admits(&host_struct("http.Response", Vec::new()))
            .is_ok());

        assert_eq!(
            declared
                .admits(&host_struct("demo.Point", Vec::new()))
                .expect_err("another struct is not an `http.Response`")
                .found,
            "demo.Point"
        );
    }

    #[test]
    fn a_named_resource_is_checked_by_the_kind_the_handle_was_issued_for() {
        let declared = HostType::Named("database.Connection");
        let handle = Value::Resource(ResourceHandle::new("database", &CONNECTION, 7));
        assert!(declared.admits(&handle).is_ok());

        let elsewhere = Value::Resource(ResourceHandle::new("http", &CONNECTION, 7));
        assert_eq!(
            declared
                .admits(&elsewhere)
                .expect_err("the same kind of another module is another type")
                .found,
            "http.Connection"
        );
    }
}
