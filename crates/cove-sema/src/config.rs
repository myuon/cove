//! `cove.toml`: the host's execution configuration.
//!
//! The host chooses the entry function and grants authority at the execution
//! boundary; it never changes the meaning of the language.

use std::collections::BTreeMap;

/// A parsed `cove.toml`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// `[run.<name>]` tables, keyed by run name.
    pub runs: BTreeMap<String, RunConfig>,
}

/// Parses the text of a `cove.toml`.
///
/// Cove prefers explicit configuration over silently ignored settings, so
/// unknown top-level tables and unknown keys inside a `[run.<name>]` table
/// are rejected rather than skipped.
pub fn parse(text: &str) -> Result<Config, String> {
    let table: toml::Table = text.parse().map_err(|e| format!("cove.toml: {e}"))?;

    let mut runs = BTreeMap::new();
    for (key, value) in &table {
        if key != "run" {
            return Err(format!("cove.toml: unknown top-level key `{key}`"));
        }
        let run_tables = value
            .as_table()
            .ok_or_else(|| "cove.toml: `run` must be a table".to_string())?;
        for (name, run_value) in run_tables {
            runs.insert(name.clone(), parse_run(name, run_value)?);
        }
    }

    Ok(Config { runs })
}

fn parse_run(name: &str, value: &toml::Value) -> Result<RunConfig, String> {
    let table = value
        .as_table()
        .ok_or_else(|| format!("run `{name}`: must be a table"))?;

    let mut entry = None;
    let mut allow = Vec::new();
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
            other => return Err(format!("run `{name}`: unknown key `{other}`")),
        }
    }

    let entry = entry.ok_or_else(|| format!("run `{name}`: missing `entry`"))?;
    Ok(RunConfig { entry, allow })
}

/// One `[run.<name>]` table.
#[derive(Clone, Debug, PartialEq)]
pub struct RunConfig {
    /// A fully qualified entry function such as `hello.main`.
    pub entry: String,
    /// Coarse capabilities granted to this run.
    pub allow: Vec<String>,
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
}
