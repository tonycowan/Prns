use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum ConfigError {
    Read { path: PathBuf, source: io::Error },
    Parse { line: usize, message: String },
    MissingStoreDirectory { path: PathBuf },
    MissingTransferDirectory { path: PathBuf },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "could not read config {}: {source}",
                    path.display()
                )
            }
            Self::Parse { line, message } => write!(formatter, "config line {line}: {message}"),
            Self::MissingStoreDirectory { path } => write!(
                formatter,
                "config {} has no [object-services] [[object-service]] object-store-directory",
                path.display()
            ),
            Self::MissingTransferDirectory { path } => write!(
                formatter,
                "config {} has no [object-services] [[object-transfer]] object-transfer-directory",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchPolicy {
    pub cdn: bool,
    pub cdn_groups: Vec<String>,
    pub auto_update: bool,
    pub auto_stage: bool,
    pub board: String,
    /// Offers slower than this are held while who-has searches further.
    /// `0` accepts any offer.
    pub minimum_bytes_per_second: u64,
}

pub struct StackPaths {
    pub object_store: PathBuf,
    pub object_transfer: PathBuf,
    pub fetch: FetchPolicy,
}

pub fn load_stack(config_dir: &Path) -> Result<StackPaths, ConfigError> {
    let path = config_dir.join("config");
    let text = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    let parsed = parse_stack(&text).map_err(|error| match error {
        ParseError::Syntax { line, message } => ConfigError::Parse { line, message },
        ParseError::MissingStore => ConfigError::MissingStoreDirectory { path: path.clone() },
        ParseError::MissingTransfer => ConfigError::MissingTransferDirectory { path },
    })?;
    Ok(StackPaths {
        object_store: resolve(config_dir, parsed.store),
        object_transfer: resolve(config_dir, parsed.transfer),
        fetch: parsed.fetch,
    })
}

fn resolve(config_dir: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        config_dir.join(path)
    }
}

#[derive(Debug)]
enum ParseError {
    Syntax { line: usize, message: String },
    MissingStore,
    MissingTransfer,
}

struct ParsedStack {
    store: PathBuf,
    transfer: PathBuf,
    fetch: FetchPolicy,
}

fn parse_stack(text: &str) -> Result<ParsedStack, ParseError> {
    let mut stack = Vec::new();
    let mut store = None;
    let mut transfer = None;
    let mut fetch = FetchPolicy::default();
    for (index, raw) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            let (depth, name) = section_heading(line, line_number)?;
            while stack.len() >= depth {
                stack.pop();
            }
            if stack.len() != depth - 1 {
                return Err(ParseError::Syntax {
                    line: line_number,
                    message: format!("section [{name}] skips a nesting level"),
                });
            }
            stack.push(name);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ParseError::Syntax {
                line: line_number,
                message: "expected key = value".to_string(),
            });
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(ParseError::Syntax {
                line: line_number,
                message: "missing key before '='".to_string(),
            });
        }
        if stack == ["object-services", "object-service"] && key == "object-store-directory" {
            store = Some(required_path(value, "object-store-directory", line_number)?);
        }
        if stack == ["object-services", "object-service"] {
            match key {
                "cdn" => fetch.cdn = yes_value(value, line_number)?,
                "cdn-group" => {
                    let group = unquote(value.trim());
                    if group.is_empty() {
                        return Err(ParseError::Syntax {
                            line: line_number,
                            message: "cdn-group is empty".to_string(),
                        });
                    }
                    fetch.cdn_groups.push(group.to_string());
                }
                "auto-update" => fetch.auto_update = yes_value(value, line_number)?,
                "auto-stage" => fetch.auto_stage = yes_value(value, line_number)?,
                "board" => fetch.board = unquote(value.trim()).to_string(),
                _ => {}
            }
        }
        if stack == ["object-services", "object-transfer"] && key == "object-transfer-directory" {
            transfer = Some(required_path(
                value,
                "object-transfer-directory",
                line_number,
            )?);
        }
        if stack == ["object-services", "object-transfer"] && key == "minimum-bytes-per-second" {
            fetch.minimum_bytes_per_second =
                required_u64(value, "minimum-bytes-per-second", line_number)?;
        }
    }
    Ok(ParsedStack {
        store: store.ok_or(ParseError::MissingStore)?,
        transfer: transfer.ok_or(ParseError::MissingTransfer)?,
        fetch,
    })
}

fn yes_value(value: &str, line_number: usize) -> Result<bool, ParseError> {
    match unquote(value.trim()) {
        "Yes" | "yes" => Ok(true),
        "No" | "no" => Ok(false),
        other => Err(ParseError::Syntax {
            line: line_number,
            message: format!("expected Yes or No, found {other}"),
        }),
    }
}

fn required_u64(value: &str, key: &str, line_number: usize) -> Result<u64, ParseError> {
    let value = unquote(value.trim());
    value.parse().map_err(|_| ParseError::Syntax {
        line: line_number,
        message: format!("{key} must be a non-negative integer"),
    })
}

fn required_path(value: &str, key: &str, line_number: usize) -> Result<PathBuf, ParseError> {
    let value = unquote(value.trim());
    if value.is_empty() {
        return Err(ParseError::Syntax {
            line: line_number,
            message: format!("{key} is empty"),
        });
    }
    Ok(PathBuf::from(value))
}

fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (index, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '#' | ';' if !in_quotes => return &line[..index],
            _ => {}
        }
    }
    line
}

fn section_heading(line: &str, line_number: usize) -> Result<(usize, String), ParseError> {
    let depth = line.chars().take_while(|ch| *ch == '[').count();
    let closing = line.chars().rev().take_while(|ch| *ch == ']').count();
    if depth == 0 || depth != closing || !line.ends_with(']') {
        return Err(ParseError::Syntax {
            line: line_number,
            message: "malformed section heading".to_string(),
        });
    }
    let name = line[depth..line.len() - depth].trim();
    if name.is_empty() {
        return Err(ParseError::Syntax {
            line: line_number,
            message: "section heading has no name".to_string(),
        });
    }
    Ok((depth, name.to_string()))
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::parse_stack;

    #[test]
    fn reads_the_object_service_directory() {
        let text = "\
[reticulum]
  enable_transport = Yes

[object-services]
  [[object-service]]
    object-store-directory = /tmp/stack-a/object-store

  [[object-transfer]]
    object-transfer-directory = /tmp/stack-a/object-transfer
";
        let parsed = parse_stack(text).expect("directories");
        assert_eq!(parsed.store.as_os_str(), "/tmp/stack-a/object-store");
        assert_eq!(parsed.transfer.as_os_str(), "/tmp/stack-a/object-transfer");
        assert!(!parsed.fetch.cdn);
        assert!(parsed.fetch.cdn_groups.is_empty());
    }

    #[test]
    fn reads_cdn_and_firmware_fetch_policy() {
        let text = "\
[object-services]
  [[object-service]]
    object-store-directory = /tmp/store
    cdn = Yes
    cdn-group = site-a
    cdn-group = site-b
    auto-stage = Yes
    auto-update = No
    board = heltec-v4-r8

  [[object-transfer]]
    object-transfer-directory = /tmp/transfer
";
        let parsed = parse_stack(text).expect("policy");
        assert!(parsed.fetch.cdn);
        assert_eq!(parsed.fetch.cdn_groups, ["site-a", "site-b"]);
        assert!(parsed.fetch.auto_stage);
        assert!(!parsed.fetch.auto_update);
        assert_eq!(parsed.fetch.board, "heltec-v4-r8");
        assert_eq!(parsed.fetch.minimum_bytes_per_second, 0);
    }

    #[test]
    fn reads_the_who_has_bandwidth_floor() {
        let text = "\
[object-services]
  [[object-service]]
    object-store-directory = /tmp/store

  [[object-transfer]]
    object-transfer-directory = /tmp/transfer
    minimum-bytes-per-second = 1000
";
        let parsed = parse_stack(text).expect("floor");
        assert_eq!(parsed.fetch.minimum_bytes_per_second, 1000);
    }
}
