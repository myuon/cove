//! `cove.toml`: the host's execution configuration.
//!
//! The host chooses the entry function and grants authority at the execution
//! boundary; it never changes the meaning of the language.

use std::collections::BTreeMap;
use std::time::Duration;

/// A parsed `cove.toml`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// `[run.<name>]` tables, keyed by run name.
    pub runs: BTreeMap<String, RunConfig>,
    /// The `[check]` table, controlling `cove check` for the whole package.
    pub check: CheckConfig,
}

/// The `[check]` table.
///
/// Unlike `[run.<name>]`, this is one setting per package, not per run: the
/// Language Card treats denying warnings as something "projects" decide, so
/// it lives at the package's own top-level table rather than being repeated
/// in every `[run.<name>]`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CheckConfig {
    /// Mirrors `cove check --deny-warnings`. When either the config or the
    /// flag asks for denial, `cove check` fails on any warning: a CI
    /// invocation asking for stricter behavior always wins over a project
    /// default that does not.
    pub deny_warnings: bool,
}

/// Parses the text of a `cove.toml`.
///
/// Cove prefers explicit configuration over silently ignored settings, so
/// unknown top-level tables, unknown keys inside a `[run.<name>]` table, and
/// unknown keys inside `[check]` are rejected rather than skipped.
pub fn parse(text: &str) -> Result<Config, String> {
    let table: toml::Table = text.parse().map_err(|e| format!("cove.toml: {e}"))?;

    let mut runs = BTreeMap::new();
    let mut check = CheckConfig::default();
    for (key, value) in &table {
        match key.as_str() {
            "run" => {
                let run_tables = value
                    .as_table()
                    .ok_or_else(|| "cove.toml: `run` must be a table".to_string())?;
                for (name, run_value) in run_tables {
                    runs.insert(name.clone(), parse_run(name, run_value)?);
                }
            }
            "check" => {
                check = parse_check(value)?;
            }
            other => return Err(format!("cove.toml: unknown top-level key `{other}`")),
        }
    }

    Ok(Config { runs, check })
}

fn parse_check(value: &toml::Value) -> Result<CheckConfig, String> {
    let table = value
        .as_table()
        .ok_or_else(|| "cove.toml: `check` must be a table".to_string())?;

    let mut deny_warnings = false;
    for (key, value) in table {
        match key.as_str() {
            "deny_warnings" => {
                deny_warnings = value.as_bool().ok_or_else(|| {
                    "cove.toml: `check.deny_warnings` must be a boolean".to_string()
                })?;
            }
            other => return Err(format!("cove.toml: unknown key `check.{other}`")),
        }
    }
    Ok(CheckConfig { deny_warnings })
}

fn parse_run(name: &str, value: &toml::Value) -> Result<RunConfig, String> {
    let table = value
        .as_table()
        .ok_or_else(|| format!("run `{name}`: must be a table"))?;

    let mut entry = None;
    let mut allow = Vec::new();
    let mut fuel = None;
    let mut deadline = None;
    let mut max_host_calls = None;
    let mut trace = None;
    for (key, value) in table {
        match key.as_str() {
            "entry" => {
                entry = Some(
                    value
                        .as_str()
                        .ok_or_else(|| format!("run `{name}`: `entry` must be a string"))?
                        .to_string(),
                );
            }
            "allow" => {
                let items = value
                    .as_array()
                    .ok_or_else(|| format!("run `{name}`: `allow` must be an array of strings"))?;
                for item in items {
                    let item = item.as_str().ok_or_else(|| {
                        format!("run `{name}`: `allow` must be an array of strings")
                    })?;
                    allow.push(item.to_string());
                }
            }
            "fuel" => {
                fuel = Some(parse_non_negative_integer(name, "fuel", value)?);
            }
            "deadline" => {
                let text = value
                    .as_str()
                    .ok_or_else(|| format!("run `{name}`: `deadline` must be a string"))?;
                deadline = Some(parse_duration(name, text)?);
            }
            "max_host_calls" => {
                max_host_calls = Some(parse_non_negative_integer(name, "max_host_calls", value)?);
            }
            "trace" => {
                trace = Some(
                    value
                        .as_str()
                        .ok_or_else(|| format!("run `{name}`: `trace` must be a string"))?
                        .to_string(),
                );
            }
            other => return Err(format!("run `{name}`: unknown key `{other}`")),
        }
    }

    let entry = entry.ok_or_else(|| format!("run `{name}`: missing `entry`"))?;
    Ok(RunConfig {
        entry,
        allow,
        fuel,
        deadline,
        max_host_calls,
        trace,
    })
}

/// Parses a non-negative integer key, such as `fuel` or `max_host_calls`.
fn parse_non_negative_integer(name: &str, key: &str, value: &toml::Value) -> Result<u64, String> {
    let int = value
        .as_integer()
        .ok_or_else(|| format!("run `{name}`: `{key}` must be an integer"))?;
    u64::try_from(int).map_err(|_| format!("run `{name}`: `{key}` must not be negative"))
}

/// Parses a duration such as `"500ms"` or `"5s"`, using the same unit
/// meanings as the lexer's duration literals: `ns`, `us`, `ms`, `s`, `m`, and
/// `h`.
fn parse_duration(name: &str, text: &str) -> Result<Duration, String> {
    let accepted = "the accepted units are `ns`, `us`, `ms`, `s`, `m`, and `h`";
    let invalid =
        || format!("run `{name}`: `deadline` value `{text}` is not a valid duration; {accepted}");

    let split_at = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(invalid)?;
    let (digits, unit) = text.split_at(split_at);
    if digits.is_empty() {
        return Err(invalid());
    }
    let value: u64 = digits.parse().map_err(|_| invalid())?;

    let nanos_per_unit: u64 = match unit {
        "ns" => 1,
        "us" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60_000_000_000,
        "h" => 3_600_000_000_000,
        _ => return Err(invalid()),
    };
    let nanos = value.checked_mul(nanos_per_unit).ok_or_else(|| {
        format!("run `{name}`: `deadline` value `{text}` overflows a 64-bit nanosecond count")
    })?;
    Ok(Duration::from_nanos(nanos))
}

/// One `[run.<name>]` table.
#[derive(Clone, Debug, PartialEq)]
pub struct RunConfig {
    /// A fully qualified entry function such as `hello.main`.
    pub entry: String,
    /// Coarse capabilities granted to this run.
    pub allow: Vec<String>,
    /// The total fuel this run may spend before the runtime stops it.
    pub fuel: Option<u64>,
    /// The wall-clock deadline this run may take before the runtime stops
    /// it, parsed from a duration string such as `"500ms"`.
    pub deadline: Option<Duration>,
    /// The total number of host calls this run may make before the runtime
    /// stops it.
    pub max_host_calls: Option<u64>,
    /// A path to write a JSONL trace of this run to.
    pub trace: Option<String>,
}

impl RunConfig {
    /// Splits `entry` into its module path and function name.
    pub fn entry_parts(&self) -> Option<(&str, &str)> {
        self.entry.rsplit_once('.')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_example_cove_toml() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/cove.toml");
        let text = std::fs::read_to_string(&path).expect("examples/cove.toml exists");
        let config = parse(&text).expect("examples/cove.toml parses");

        assert_eq!(
            config.runs.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "callbacks",
                "config",
                "hello",
                "restricted",
                "server",
                "tasks",
                "traits",
                "values"
            ]
        );

        let hello = &config.runs["hello"];
        assert_eq!(hello.entry, "hello.main");
        assert_eq!(hello.allow, vec!["console".to_string()]);
        assert_eq!(hello.entry_parts(), Some(("hello", "main")));

        let server = &config.runs["server"];
        assert_eq!(
            server.allow,
            vec!["network".to_string(), "console".to_string()]
        );
    }

    #[test]
    fn rejects_missing_entry() {
        let err = parse("[run.hello]\nallow = [\"console\"]\n").unwrap_err();
        assert_eq!(err, "run `hello`: missing `entry`");
    }

    #[test]
    fn rejects_non_string_allow_item() {
        let err = parse("[run.hello]\nentry = \"hello.main\"\nallow = [1]\n").unwrap_err();
        assert_eq!(err, "run `hello`: `allow` must be an array of strings");
    }

    #[test]
    fn rejects_non_string_entry() {
        let err = parse("[run.hello]\nentry = 1\n").unwrap_err();
        assert_eq!(err, "run `hello`: `entry` must be a string");
    }

    #[test]
    fn rejects_unknown_key_in_run_table() {
        let err =
            parse("[run.hello]\nentry = \"hello.main\"\nallowed = [\"console\"]\n").unwrap_err();
        assert_eq!(err, "run `hello`: unknown key `allowed`");
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        let err = parse("[package]\nname = \"cove\"\n").unwrap_err();
        assert_eq!(err, "cove.toml: unknown top-level key `package`");
    }

    #[test]
    fn a_run_table_with_no_resource_keys_still_parses() {
        let config = parse("[run.hello]\nentry = \"hello.main\"\n").unwrap();
        let hello = &config.runs["hello"];
        assert_eq!(hello.fuel, None);
        assert_eq!(hello.deadline, None);
        assert_eq!(hello.max_host_calls, None);
        assert_eq!(hello.trace, None);
    }

    #[test]
    fn parses_fuel() {
        let config = parse("[run.hello]\nentry = \"hello.main\"\nfuel = 1000\n").unwrap();
        assert_eq!(config.runs["hello"].fuel, Some(1000));
    }

    #[test]
    fn rejects_non_integer_fuel() {
        let err = parse("[run.hello]\nentry = \"hello.main\"\nfuel = \"1000\"\n").unwrap_err();
        assert_eq!(err, "run `hello`: `fuel` must be an integer");
    }

    #[test]
    fn rejects_negative_fuel() {
        let err = parse("[run.hello]\nentry = \"hello.main\"\nfuel = -1\n").unwrap_err();
        assert_eq!(err, "run `hello`: `fuel` must not be negative");
    }

    #[test]
    fn parses_max_host_calls() {
        let config = parse("[run.hello]\nentry = \"hello.main\"\nmax_host_calls = 5\n").unwrap();
        assert_eq!(config.runs["hello"].max_host_calls, Some(5));
    }

    #[test]
    fn rejects_non_integer_max_host_calls() {
        let err = parse("[run.hello]\nentry = \"hello.main\"\nmax_host_calls = 1.5\n").unwrap_err();
        assert_eq!(err, "run `hello`: `max_host_calls` must be an integer");
    }

    #[test]
    fn rejects_negative_max_host_calls() {
        let err = parse("[run.hello]\nentry = \"hello.main\"\nmax_host_calls = -3\n").unwrap_err();
        assert_eq!(err, "run `hello`: `max_host_calls` must not be negative");
    }

    #[test]
    fn parses_every_deadline_unit() {
        let cases = [
            ("1ns", Duration::from_nanos(1)),
            ("1us", Duration::from_micros(1)),
            ("500ms", Duration::from_millis(500)),
            ("5s", Duration::from_secs(5)),
            ("1m", Duration::from_secs(60)),
            ("1h", Duration::from_secs(3600)),
        ];
        for (text, expected) in cases {
            let toml = format!("[run.hello]\nentry = \"hello.main\"\ndeadline = \"{text}\"\n");
            let config = parse(&toml).unwrap_or_else(|e| panic!("`{text}` should parse: {e}"));
            assert_eq!(config.runs["hello"].deadline, Some(expected), "{text}");
        }
    }

    #[test]
    fn rejects_non_string_deadline() {
        let err = parse("[run.hello]\nentry = \"hello.main\"\ndeadline = 500\n").unwrap_err();
        assert_eq!(err, "run `hello`: `deadline` must be a string");
    }

    #[test]
    fn rejects_deadline_with_an_unknown_unit() {
        let err = parse("[run.hello]\nentry = \"hello.main\"\ndeadline = \"5x\"\n").unwrap_err();
        assert_eq!(
            err,
            "run `hello`: `deadline` value `5x` is not a valid duration; the accepted units are `ns`, `us`, `ms`, `s`, `m`, and `h`"
        );
    }

    #[test]
    fn rejects_deadline_with_no_unit() {
        let err = parse("[run.hello]\nentry = \"hello.main\"\ndeadline = \"500\"\n").unwrap_err();
        assert_eq!(
            err,
            "run `hello`: `deadline` value `500` is not a valid duration; the accepted units are `ns`, `us`, `ms`, `s`, `m`, and `h`"
        );
    }

    #[test]
    fn rejects_deadline_with_no_digits() {
        let err = parse("[run.hello]\nentry = \"hello.main\"\ndeadline = \"ms\"\n").unwrap_err();
        assert_eq!(
            err,
            "run `hello`: `deadline` value `ms` is not a valid duration; the accepted units are `ns`, `us`, `ms`, `s`, `m`, and `h`"
        );
    }

    #[test]
    fn parses_trace() {
        let config =
            parse("[run.hello]\nentry = \"hello.main\"\ntrace = \"trace.jsonl\"\n").unwrap();
        assert_eq!(config.runs["hello"].trace, Some("trace.jsonl".to_string()));
    }

    #[test]
    fn rejects_non_string_trace() {
        let err = parse("[run.hello]\nentry = \"hello.main\"\ntrace = 1\n").unwrap_err();
        assert_eq!(err, "run `hello`: `trace` must be a string");
    }

    #[test]
    fn deny_warnings_defaults_to_false() {
        let config = parse("[run.hello]\nentry = \"hello.main\"\n").unwrap();
        assert!(!config.check.deny_warnings);
    }

    #[test]
    fn parses_deny_warnings() {
        let config = parse("[check]\ndeny_warnings = true\n").unwrap();
        assert!(config.check.deny_warnings);
    }

    #[test]
    fn rejects_non_bool_deny_warnings() {
        let err = parse("[check]\ndeny_warnings = \"true\"\n").unwrap_err();
        assert_eq!(err, "cove.toml: `check.deny_warnings` must be a boolean");
    }

    #[test]
    fn rejects_unknown_key_in_check_table() {
        let err = parse("[check]\ndeny_warning = true\n").unwrap_err();
        assert_eq!(err, "cove.toml: unknown key `check.deny_warning`");
    }

    #[test]
    fn rejects_non_table_check() {
        let err = parse("check = true\n").unwrap_err();
        assert_eq!(err, "cove.toml: `check` must be a table");
    }
}
