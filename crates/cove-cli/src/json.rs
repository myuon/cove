//! A small JSON reader, for reading a recorded trace back.
//!
//! The runtime writes traces as JSONL and depends on nothing to do it, so the
//! side that reads them depends on nothing either. This is a reader only:
//! `cove` writes JSON in exactly one place, [`cove_runtime::trace`], and a
//! second writer here would be a second definition of the trace format.
//!
//! A number keeps the digits it was written with rather than becoming an
//! `f64`, because a trace's nanosecond counts and `Int` values are exact and
//! a round trip through a float is not.

use std::fmt;

/// The maximum nesting depth this reader accepts.
///
/// A trace's values nest a handful of levels deep at most. Refusing more
/// keeps a malformed or hostile file from exhausting the stack, and a reader
/// that would rather reject than misread should reject this too.
const MAX_DEPTH: usize = 64;

/// One JSON value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    /// The number exactly as written, so an integer stays exact.
    Number(String),
    String(String),
    Array(Vec<Json>),
    /// Members in the order they were written.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// The member named `name`, for an object.
    pub(crate) fn get(&self, name: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The string this value holds, if it is one.
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }

    /// The boolean this value holds, if it is one.
    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The elements this value holds, if it is an array.
    pub(crate) fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The number this value holds as a `u64`, if it is one that fits.
    pub(crate) fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Number(digits) => digits.parse().ok(),
            _ => None,
        }
    }

    /// The number this value holds as an `i64`, if it is one that fits.
    pub(crate) fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Number(digits) => digits.parse().ok(),
            _ => None,
        }
    }

    /// The number this value holds as an `f64`, if it is one.
    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(digits) => digits.parse().ok(),
            _ => None,
        }
    }

    /// The name of this value's shape, for a diagnostic.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "a boolean",
            Json::Number(_) => "a number",
            Json::String(_) => "a string",
            Json::Array(_) => "an array",
            Json::Object(_) => "an object",
        }
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(b) => write!(f, "{b}"),
            Json::Number(digits) => f.write_str(digits),
            Json::String(text) => write!(f, "{text:?}"),
            Json::Array(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Json::Object(members) => {
                f.write_str("{")?;
                for (i, (key, value)) in members.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{key:?}:{value}")?;
                }
                f.write_str("}")
            }
        }
    }
}

/// Parses one complete JSON value, rejecting anything after it.
pub(crate) fn parse(text: &str) -> Result<Json, String> {
    let mut reader = Reader {
        chars: text.chars().collect(),
        at: 0,
    };
    reader.skip_space();
    let value = reader.value(0)?;
    reader.skip_space();
    if reader.at < reader.chars.len() {
        return Err(format!(
            "unexpected `{}` after the end of the value",
            reader.chars[reader.at]
        ));
    }
    Ok(value)
}

struct Reader {
    chars: Vec<char>,
    at: usize,
}

impl Reader {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.at += 1;
        }
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        match self.peek() {
            Some(found) if found == c => {
                self.at += 1;
                Ok(())
            }
            Some(found) => Err(format!("expected `{c}`, found `{found}`")),
            None => Err(format!("expected `{c}`, found the end of the line")),
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), String> {
        for c in word.chars() {
            self.expect(c)?;
        }
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err(format!("JSON nested more than {MAX_DEPTH} levels deep"));
        }
        match self.peek() {
            None => Err("expected a value, found the end of the line".to_string()),
            Some('n') => {
                self.literal("null")?;
                Ok(Json::Null)
            }
            Some('t') => {
                self.literal("true")?;
                Ok(Json::Bool(true))
            }
            Some('f') => {
                self.literal("false")?;
                Ok(Json::Bool(false))
            }
            Some('"') => Ok(Json::String(self.string()?)),
            Some('[') => self.array(depth),
            Some('{') => self.object(depth),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            Some(c) => Err(format!("expected a value, found `{c}`")),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, String> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_space();
        if self.peek() == Some(']') {
            self.at += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_space();
            items.push(self.value(depth + 1)?);
            self.skip_space();
            match self.peek() {
                Some(',') => self.at += 1,
                Some(']') => {
                    self.at += 1;
                    return Ok(Json::Array(items));
                }
                Some(c) => return Err(format!("expected `,` or `]`, found `{c}`")),
                None => return Err("expected `,` or `]`, found the end of the line".to_string()),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.expect('{')?;
        let mut members = Vec::new();
        self.skip_space();
        if self.peek() == Some('}') {
            self.at += 1;
            return Ok(Json::Object(members));
        }
        loop {
            self.skip_space();
            let key = self.string()?;
            self.skip_space();
            self.expect(':')?;
            self.skip_space();
            let value = self.value(depth + 1)?;
            members.push((key, value));
            self.skip_space();
            match self.peek() {
                Some(',') => self.at += 1,
                Some('}') => {
                    self.at += 1;
                    return Ok(Json::Object(members));
                }
                Some(c) => return Err(format!("expected `,` or `}}`, found `{c}`")),
                None => return Err("expected `,` or `}`, found the end of the line".to_string()),
            }
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.at;
        if self.peek() == Some('-') {
            self.at += 1;
        }
        let digits_from = self.at;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.at += 1;
        }
        if self.at == digits_from {
            return Err("expected a digit".to_string());
        }
        if self.peek() == Some('.') {
            self.at += 1;
            let from = self.at;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.at += 1;
            }
            if self.at == from {
                return Err("expected a digit after `.`".to_string());
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.at += 1;
            if matches!(self.peek(), Some('+' | '-')) {
                self.at += 1;
            }
            let from = self.at;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.at += 1;
            }
            if self.at == from {
                return Err("expected a digit in the exponent".to_string());
            }
        }
        Ok(Json::Number(self.chars[start..self.at].iter().collect()))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err("unterminated string".to_string());
            };
            self.at += 1;
            match c {
                '"' => return Ok(out),
                '\\' => {
                    let Some(escape) = self.peek() else {
                        return Err("unterminated escape".to_string());
                    };
                    self.at += 1;
                    match escape {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => out.push(self.unicode_escape()?),
                        other => return Err(format!("unknown escape `\\{other}`")),
                    }
                }
                c if (c as u32) < 0x20 => {
                    return Err(format!(
                        "a control character must be escaped, found U+{:04X}",
                        c as u32
                    ))
                }
                c => out.push(c),
            }
        }
    }

    /// Reads the four hexadecimal digits after `\u`, joining a surrogate pair
    /// with the `\u` escape that must follow it.
    fn unicode_escape(&mut self) -> Result<char, String> {
        let high = self.hex4()?;
        if !(0xD800..0xDC00).contains(&high) {
            return char::from_u32(high)
                .ok_or_else(|| format!("`\\u{high:04x}` is not a character"));
        }
        self.expect('\\')?;
        self.expect('u')?;
        let low = self.hex4()?;
        if !(0xDC00..0xE000).contains(&low) {
            return Err(format!(
                "`\\u{high:04x}` is not followed by a low surrogate"
            ));
        }
        let combined = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
        char::from_u32(combined)
            .ok_or_else(|| format!("`\\u{high:04x}\\u{low:04x}` is not a character"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut value = 0;
        for _ in 0..4 {
            let Some(c) = self.peek() else {
                return Err("expected four hexadecimal digits after `\\u`".to_string());
            };
            let digit = c
                .to_digit(16)
                .ok_or_else(|| format!("expected a hexadecimal digit, found `{c}`"))?;
            self.at += 1;
            value = value * 16 + digit;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_shapes_a_trace_line_is_made_of() {
        let value =
            parse(r#"{"event":"host_call","wait_ns":900,"granted":true,"args":[],"outcome":null}"#)
                .expect("a trace line parses");
        assert_eq!(value.get("event").unwrap().as_str(), Some("host_call"));
        assert_eq!(value.get("wait_ns").unwrap().as_u64(), Some(900));
        assert_eq!(value.get("granted").unwrap().as_bool(), Some(true));
        assert_eq!(value.get("args").unwrap().as_array(), Some(&[][..]));
        assert_eq!(value.get("outcome"), Some(&Json::Null));
        assert_eq!(value.get("missing"), None);
    }

    #[test]
    fn an_integer_keeps_the_digits_it_was_written_with() {
        let value = parse("9007199254740993").expect("a large integer parses");
        assert_eq!(value.as_u64(), Some(9_007_199_254_740_993));
        assert_eq!(parse("-7").unwrap().as_i64(), Some(-7));
        assert_eq!(parse("1.5").unwrap().as_f64(), Some(1.5));
        assert_eq!(parse("1e3").unwrap().as_f64(), Some(1000.0));
    }

    #[test]
    fn escapes_are_read_back() {
        assert_eq!(
            parse(r#""a\"b\\c\nd\te\u0007f""#).unwrap().as_str(),
            Some("a\"b\\c\nd\te\u{7}f")
        );
    }

    #[test]
    fn a_surrogate_pair_becomes_one_character() {
        assert_eq!(parse(r#""😀""#).unwrap().as_str(), Some("😀"));
    }

    #[test]
    fn a_lone_surrogate_is_rejected() {
        assert!(parse(r#""\ud83d""#).is_err());
    }

    #[test]
    fn nesting_past_the_limit_is_rejected_rather_than_overflowing_the_stack() {
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        let error = parse(&deep).expect_err("deep nesting is rejected");
        assert!(error.contains("nested more than"), "{error}");
    }

    #[test]
    fn trailing_content_is_rejected() {
        let error = parse("{} {}").expect_err("two values on one line are rejected");
        assert!(error.contains("after the end of the value"), "{error}");
    }

    #[test]
    fn malformed_input_says_what_it_expected() {
        assert!(parse("{\"a\"}").unwrap_err().contains("expected `:`"));
        assert!(parse("[1,]").unwrap_err().contains("expected a value"));
        assert!(parse("tru").unwrap_err().contains("end of the line"));
    }
}
