//! Fixed command-line contract: one config path, then the guest argv verbatim.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// Parsed host arguments. The guest argv is never rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub config: PathBuf,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CliError {
    #[error("usage: circus-sandbox-exec --config /absolute/config.json [--] <guest argv...>")]
    Usage,
    #[error("the configuration path must be absolute")]
    ConfigPath,
    #[error("the guest arguments must be valid UTF-8")]
    Arguments,
}

/// Parses `--config <absolute path> [--] <guest argv...>`.
///
/// A single separator immediately after the configuration path is consumed.
/// Every remaining argument, including later `--` values, belongs to the guest.
/// An empty guest argv and an empty executable are rejected before any backend
/// access.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Cli, CliError> {
    let args: Vec<OsString> = args.into_iter().collect();
    if args.len() < 2 || args[0].as_os_str() != OsStr::new("--config") {
        return Err(CliError::Usage);
    }
    let config = PathBuf::from(&args[1]);
    if !config.is_absolute() {
        return Err(CliError::ConfigPath);
    }
    let mut rest = &args[2..];
    if rest
        .first()
        .is_some_and(|value| value.as_os_str() == OsStr::new("--"))
    {
        rest = &rest[1..];
    }
    if rest.is_empty() {
        return Err(CliError::Usage);
    }
    let argv = rest
        .iter()
        .map(|value| value.clone().into_string())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CliError::Arguments)?;
    if argv.first().is_none_or(String::is_empty) {
        return Err(CliError::Usage);
    }
    Ok(Cli { config, argv })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn accepts_optional_separator_and_preserves_guest_arguments() {
        let windows = PathBuf::from("C:\\work\\config.json");
        let posix = PathBuf::from("/work/config.json");
        let absolute = if windows.is_absolute() {
            windows
        } else {
            posix
        };
        let text = absolute.to_string_lossy().into_owned();
        let with = parse([
            OsString::from("--config"),
            OsString::from(text.clone()),
            OsString::from("--"),
            OsString::from("codex"),
            OsString::from("app-server"),
            OsString::from("--flag"),
        ])
        .unwrap();
        let without = parse([
            OsString::from("--config"),
            OsString::from(text),
            OsString::from("codex"),
            OsString::from("app-server"),
        ])
        .unwrap();
        assert_eq!(with.config, absolute);
        assert_eq!(with.argv, ["codex", "app-server", "--flag"]);
        assert_eq!(without.argv, ["codex", "app-server"]);
    }

    #[test]
    fn rejects_usage_errors_and_empty_argv() {
        let absolute = if PathBuf::from("C:\\work\\config.json").is_absolute() {
            "C:\\work\\config.json"
        } else {
            "/work/config.json"
        };
        assert_eq!(parse(args(&[])), Err(CliError::Usage));
        assert_eq!(parse(args(&["--config"])), Err(CliError::Usage));
        assert_eq!(parse(args(&["--config", absolute])), Err(CliError::Usage));
        assert_eq!(
            parse(args(&["--config", absolute, "--"])),
            Err(CliError::Usage)
        );
        assert_eq!(
            parse(args(&["--config", "relative.json", "codex"])),
            Err(CliError::ConfigPath)
        );
    }
}
