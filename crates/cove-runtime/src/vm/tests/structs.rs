//! Building a struct, reading and writing a field, calling a method on one,
//! and what an opaque struct renders as.

use super::*;

#[test]
fn a_struct_is_built_read_and_written() {
    assert_eq!(
        agree(&format!(
            "{CURSOR}export fn main() -> Int {{\n  let cursor = Cursor(at: 3, step: 2)\n  cursor.at\n}}\n"
        ))
        .value(),
        "Int(3)"
    );
    assert_eq!(
        agree(&format!(
            "{CURSOR}export fn main() -> Int {{\n  var cursor = Cursor(at: 3, step: 2)\n  cursor.at = 9\n  cursor.at\n}}\n"
        ))
        .value(),
        "Int(9)"
    );
    assert_eq!(
        agree(&format!(
            "{CURSOR}export fn main() -> Int {{\n  var cursor = Cursor(at: 3, step: 2)\n  cursor.at += cursor.step\n  cursor.at\n}}\n"
        ))
        .value(),
        "Int(5)"
    );
}

/// A struct is a value: writing a copy's field leaves the original alone.
#[test]
fn writing_a_copys_field_leaves_the_original_alone() {
    assert_eq!(
        agree(&format!(
            "{CURSOR}export fn main() -> Int {{\n  let first = Cursor(at: 1, step: 1)\n  var second = first\n  second.at = 99\n  first.at\n}}\n"
        ))
        .value(),
        "Int(1)"
    );
}

#[test]
fn a_method_takes_its_receiver_and_answers() {
    let source = format!(
        "{CURSOR}impl Cursor {{\n  fn position(self) -> Int {{\n    self.at\n  }}\n\n  fn ahead(self, by: Int) -> Int {{\n    self.at + by * self.step\n  }}\n}}\n\nexport fn main() -> Int {{\n  let cursor = Cursor(at: 4, step: 3)\n  cursor.position() + cursor.ahead(by: 2)\n}}\n"
    );
    assert_eq!(agree(&source).value(), "Int(14)");
}

/// An opaque struct renders as its name alone, on both backends.
///
/// The IR carries the type's name and not whether it is opaque, so the VM
/// asks the checker the same question `Interpreter::init_struct` asks.
#[test]
fn an_opaque_struct_renders_as_its_name() {
    let source = "export opaque struct Token {\n  secret: Int\n}\n\nexport fn main() -> String {\n  let token = Token(secret: 7)\n  \"{token}\"\n}\n";
    assert_eq!(agree(source).value(), "Str(\"Token\")");
}
