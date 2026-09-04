//! Writing JSON by hand.
//!
//! This workspace has one third-party dependency in total, and it is not a
//! JSON library: the CLI's `--json` output and
//! [`cove_runtime::value_to_json`] are both written this way. So is this. The
//! escaping rule below is the same one `cove-runtime`'s `json_string` uses,
//! deliberately — two spellings of "what a JSON string may not contain" would
//! be two answers that could drift.

/// One JSON string literal, quotes included.
pub(crate) fn string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A JSON array of already-rendered members.
pub(crate) fn array(members: impl IntoIterator<Item = String>) -> String {
    let members: Vec<String> = members.into_iter().collect();
    format!("[{}]", members.join(","))
}

/// A JSON object from `(key, already-rendered value)` pairs.
pub(crate) fn object<'a>(fields: impl IntoIterator<Item = (&'a str, String)>) -> String {
    let fields: Vec<String> = fields
        .into_iter()
        .map(|(key, value)| format!("{}:{value}", string(key)))
        .collect();
    format!("{{{}}}", fields.join(","))
}

/// A value that may be absent, as JSON: `null` rather than an empty string,
/// for the reason `cove-runtime`'s `json_measure` gives — an empty answer is
/// an answer and the absence of one is not.
pub(crate) fn or_null(held: Option<String>) -> String {
    held.unwrap_or_else(|| "null".to_string())
}
