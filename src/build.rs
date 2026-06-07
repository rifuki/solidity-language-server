use crate::utils::byte_offset_to_position;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Range};

/// Default error codes that are always suppressed (contract-size and
/// code-size warnings that are noisy for LSP users).
const DEFAULT_IGNORED_CODES: &[&str] = &["5574", "3860"];

/// Check whether a solc error should be suppressed based on its error code.
///
/// Suppresses the hardcoded defaults plus any codes provided in `extra_codes`
/// (from `foundry.toml` `ignored_error_codes`).
pub fn ignored_error_code_warning(value: &serde_json::Value, extra_codes: &[u64]) -> bool {
    let error_code = value
        .get("errorCode")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if DEFAULT_IGNORED_CODES.contains(&error_code) {
        return true;
    }

    // Check user-configured ignored codes from foundry.toml
    if let Ok(code_num) = error_code.parse::<u64>()
        && extra_codes.contains(&code_num)
    {
        return true;
    }

    false
}

pub fn build_output_to_diagnostics(
    solc_output: &serde_json::Value,
    path: impl AsRef<Path>,
    content: &str,
    ignored_error_codes: &[u64],
) -> Vec<Diagnostic> {
    let Some(errors) = solc_output.get("errors").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let path = path.as_ref();
    errors
        .iter()
        .filter_map(|err| parse_diagnostic(err, path, content, ignored_error_codes))
        .collect()
}

/// Check whether the source path from solc's error output refers to the same
/// file the editor has open.
///
/// Solc reports error paths relative to its working directory (wherever the
/// LSP process runs from), e.g. `example/Shop.sol` or just `Shop.sol`.  The
/// editor provides the full absolute path.  We simply check whether the
/// absolute path ends with the relative path solc reported.
fn source_location_matches(source_path: &str, path: &Path) -> bool {
    let source_path = Path::new(source_path);
    if source_path.is_absolute() {
        source_path == path
    } else {
        path.ends_with(source_path)
    }
}

fn parse_diagnostic(
    err: &Value,
    path: &Path,
    content: &str,
    ignored_error_codes: &[u64],
) -> Option<Diagnostic> {
    if ignored_error_code_warning(err, ignored_error_codes) {
        return None;
    }
    let source_file = err
        .get("sourceLocation")
        .and_then(|loc| loc.get("file"))
        .and_then(|f| f.as_str())?;

    if !source_location_matches(source_file, path) {
        return None;
    }

    let start_offset = err
        .get("sourceLocation")
        .and_then(|loc| loc.get("start"))
        .and_then(|s| s.as_u64())
        .unwrap_or(0) as usize;

    let end_offset = err
        .get("sourceLocation")
        .and_then(|loc| loc.get("end"))
        .and_then(|s| s.as_u64())
        .map(|v| v as usize)
        .unwrap_or(start_offset);

    let range = if is_trailing_comma_primary_expression(err, content, start_offset) {
        trailing_comma_range(content, start_offset)
    } else {
        diagnostic_range(content, start_offset, end_offset)
    };

    let message = diagnostic_message(err, content, start_offset);

    let severity = match err.get("severity").and_then(|s| s.as_str()) {
        Some("error") => Some(DiagnosticSeverity::ERROR),
        Some("warning") => Some(DiagnosticSeverity::WARNING),
        Some("note") => Some(DiagnosticSeverity::INFORMATION),
        Some("help") => Some(DiagnosticSeverity::HINT),
        _ => Some(DiagnosticSeverity::INFORMATION),
    };

    let code = err
        .get("errorCode")
        .and_then(|c| c.as_str())
        .map(|s| NumberOrString::String(s.to_string()));

    Some(Diagnostic {
        range,
        severity,
        code,
        code_description: None,
        source: Some("solc".to_string()),
        message,
        related_information: None,
        tags: None,
        data: None,
    })
}

fn diagnostic_message(err: &Value, content: &str, start_offset: usize) -> String {
    let message = err
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("Unknown error")
        .trim();

    let mut diagnostic = if let Some(formatted) = err
        .get("formattedMessage")
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        if formatted.contains(message) {
            formatted.to_string()
        } else {
            format!("{message}\n\n{formatted}")
        }
    } else {
        message.to_string()
    };

    if is_trailing_comma_primary_expression(err, content, start_offset)
        && !diagnostic.contains(TRAILING_COMMA_HINT)
    {
        diagnostic.push_str("\n\n");
        diagnostic.push_str(TRAILING_COMMA_HINT);
    }

    diagnostic
}

const TRAILING_COMMA_HINT: &str =
    "Hint: remove the trailing comma before `)` or add the missing argument.";

fn is_trailing_comma_primary_expression(err: &Value, content: &str, start_offset: usize) -> bool {
    if err.get("errorCode").and_then(|c| c.as_str()) != Some("6933") {
        return false;
    }

    let message = err
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    if !message.contains("Expected primary expression") {
        return false;
    }

    let bytes = content.as_bytes();
    let mut close = start_offset.min(bytes.len());
    while close < bytes.len() && bytes[close].is_ascii_whitespace() {
        close += 1;
    }
    if bytes.get(close).copied() != Some(b')') {
        return false;
    }

    let mut prev = start_offset.min(bytes.len());
    while prev > 0 && bytes[prev - 1].is_ascii_whitespace() {
        prev -= 1;
    }

    bytes.get(prev.saturating_sub(1)).copied() == Some(b',')
}

fn trailing_comma_range(content: &str, close_offset: usize) -> Range {
    let bytes = content.as_bytes();
    let mut comma = close_offset.min(bytes.len());
    while comma > 0 && bytes[comma - 1].is_ascii_whitespace() {
        comma -= 1;
    }

    if comma == 0 || bytes.get(comma - 1).copied() != Some(b',') {
        return diagnostic_range(content, close_offset, close_offset);
    }
    let comma_offset = comma - 1;

    let (comma_line_start, comma_line_end) = line_bounds(content, comma_offset);
    let start = content[comma_line_start..comma_line_end]
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(idx, _)| comma_line_start + idx)
        .unwrap_or(comma_offset);

    let (_, close_line_end) = line_bounds(content, close_offset);

    Range {
        start: byte_offset_to_position(content, start),
        end: byte_offset_to_position(content, close_line_end),
    }
}

fn diagnostic_range(content: &str, start_offset: usize, end_offset: usize) -> Range {
    let len = content.len();
    let start_offset = start_offset.min(len);
    let end_offset = end_offset.min(len);

    if end_offset > start_offset {
        return Range {
            start: byte_offset_to_position(content, start_offset),
            end: byte_offset_to_position(content, end_offset),
        };
    }

    let (line_start, line_end) = line_bounds(content, start_offset);
    let first_non_ws = content[line_start..line_end]
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(idx, _)| line_start + idx)
        .unwrap_or(line_start);
    let expanded_end = if line_end > first_non_ws {
        line_end
    } else {
        next_char_boundary(content, start_offset)
    };

    Range {
        start: byte_offset_to_position(content, first_non_ws),
        end: byte_offset_to_position(content, expanded_end),
    }
}

fn line_bounds(content: &str, offset: usize) -> (usize, usize) {
    let bytes = content.as_bytes();
    let mut start = offset.min(bytes.len());
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }

    let mut end = offset.min(bytes.len());
    while end < bytes.len() && bytes[end] != b'\n' && bytes[end] != b'\r' {
        end += 1;
    }

    (start, end)
}

fn next_char_boundary(content: &str, offset: usize) -> usize {
    if offset >= content.len() {
        return offset;
    }

    content[offset..]
        .chars()
        .next()
        .map(|ch| offset + ch.len_utf8())
        .unwrap_or(offset)
}

/// Extract error-level diagnostics for files OTHER than the one being compiled.
///
/// When compiling `A.sol`, solc may report errors in imported files (e.g.
/// `B.sol` has `import {Test} from "./A.sol"` but `Test` was removed).
/// `build_output_to_diagnostics` filters those out.  This function collects
/// them so the LSP can publish diagnostics to the affected files.
///
/// Returns a map of `absolute_path → Vec<Diagnostic>`.  Only error-severity
/// diagnostics are included.  The `project_root` resolves relative paths.
pub fn cross_file_error_diagnostics(
    solc_output: &Value,
    current_file: &Path,
    project_root: &Path,
    ignored_error_codes: &[u64],
) -> HashMap<PathBuf, Vec<Diagnostic>> {
    let Some(errors) = solc_output.get("errors").and_then(|v| v.as_array()) else {
        return HashMap::new();
    };

    let mut result: HashMap<PathBuf, Vec<Diagnostic>> = HashMap::new();

    for err in errors {
        if ignored_error_code_warning(err, ignored_error_codes) {
            continue;
        }
        if err.get("severity").and_then(|s| s.as_str()) != Some("error") {
            continue;
        }
        let Some(source_file) = err
            .get("sourceLocation")
            .and_then(|loc| loc.get("file"))
            .and_then(|f| f.as_str())
        else {
            continue;
        };
        if source_location_matches(source_file, current_file) {
            continue;
        }

        let source_path = Path::new(source_file);
        let abs_path = if source_path.is_absolute() {
            source_path.to_path_buf()
        } else {
            project_root.join(source_path)
        };
        let Ok(content) = std::fs::read_to_string(&abs_path) else {
            continue;
        };

        let start_offset = err
            .get("sourceLocation")
            .and_then(|loc| loc.get("start"))
            .and_then(|s| s.as_u64())
            .unwrap_or(0) as usize;
        let end_offset = err
            .get("sourceLocation")
            .and_then(|loc| loc.get("end"))
            .and_then(|s| s.as_u64())
            .map(|v| v as usize)
            .unwrap_or(start_offset);

        let code = err
            .get("errorCode")
            .and_then(|c| c.as_str())
            .map(|s| NumberOrString::String(s.to_string()));

        let message = diagnostic_message(err, &content, start_offset);
        let range = if is_trailing_comma_primary_expression(err, &content, start_offset) {
            trailing_comma_range(&content, start_offset)
        } else {
            diagnostic_range(&content, start_offset, end_offset)
        };

        result.entry(abs_path).or_default().push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            code,
            code_description: None,
            source: Some("solc".to_string()),
            message,
            related_information: None,
            tags: None,
            data: None,
        });
    }

    result
}
