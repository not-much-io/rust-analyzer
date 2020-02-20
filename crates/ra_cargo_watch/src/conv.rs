//! This module provides the functionality needed to convert diagnostics from
//! `cargo check` json format to the LSP diagnostic format.
use cargo_metadata::diagnostic::{
    Diagnostic as RustDiagnostic, DiagnosticLevel, DiagnosticSpan, DiagnosticSpanMacroExpansion,
};
use lsp_types::{
    CodeAction, Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticTag,
    Location, NumberOrString, Position, Range, TextEdit, Url, WorkspaceEdit,
};
use std::{
    fmt::Write,
    path::{Component, Path, PathBuf, Prefix},
    str::FromStr,
};

#[cfg(test)]
mod test;

/// Converts a Rust level string to a LSP severity
fn map_level_to_severity(val: DiagnosticLevel) -> Option<DiagnosticSeverity> {
    match val {
        DiagnosticLevel::Ice => Some(DiagnosticSeverity::Error),
        DiagnosticLevel::Error => Some(DiagnosticSeverity::Error),
        DiagnosticLevel::Warning => Some(DiagnosticSeverity::Warning),
        DiagnosticLevel::Note => Some(DiagnosticSeverity::Information),
        DiagnosticLevel::Help => Some(DiagnosticSeverity::Hint),
        DiagnosticLevel::Unknown => None,
    }
}

/// Check whether a file name is from macro invocation
fn is_from_macro(file_name: &str) -> bool {
    file_name.starts_with('<') && file_name.ends_with('>')
}

/// Converts a Rust macro span to a LSP location recursively
fn map_macro_span_to_location(
    span_macro: &DiagnosticSpanMacroExpansion,
    workspace_root: &PathBuf,
) -> Option<Location> {
    if !is_from_macro(&span_macro.span.file_name) {
        return Some(map_span_to_location(&span_macro.span, workspace_root));
    }

    if let Some(expansion) = &span_macro.span.expansion {
        return map_macro_span_to_location(&expansion, workspace_root);
    }

    None
}

/// Converts a Rust span to a LSP location, resolving macro expansion site if neccesary
fn map_span_to_location(span: &DiagnosticSpan, workspace_root: &PathBuf) -> Location {
    if span.expansion.is_some() {
        let expansion = span.expansion.as_ref().unwrap();
        if let Some(macro_range) = map_macro_span_to_location(&expansion, workspace_root) {
            return macro_range;
        }
    }

    map_span_to_location_naive(span, workspace_root)
}

/// Converts a Rust span to a LSP location
fn map_span_to_location_naive(span: &DiagnosticSpan, workspace_root: &PathBuf) -> Location {
    let mut file_name = workspace_root.clone();
    file_name.push(&span.file_name);
    let uri = url_from_path_with_drive_lowercasing(file_name).unwrap();

    let range = Range::new(
        Position::new(span.line_start as u64 - 1, span.column_start as u64 - 1),
        Position::new(span.line_end as u64 - 1, span.column_end as u64 - 1),
    );

    Location { uri, range }
}

/// Converts a secondary Rust span to a LSP related information
///
/// If the span is unlabelled this will return `None`.
fn map_secondary_span_to_related(
    span: &DiagnosticSpan,
    workspace_root: &PathBuf,
) -> Option<DiagnosticRelatedInformation> {
    if let Some(label) = &span.label {
        let location = map_span_to_location(span, workspace_root);
        Some(DiagnosticRelatedInformation { location, message: label.clone() })
    } else {
        // Nothing to label this with
        None
    }
}

/// Determines if diagnostic is related to unused code
fn is_unused_or_unnecessary(rd: &RustDiagnostic) -> bool {
    if let Some(code) = &rd.code {
        match code.code.as_str() {
            "dead_code" | "unknown_lints" | "unreachable_code" | "unused_attributes"
            | "unused_imports" | "unused_macros" | "unused_variables" => true,
            _ => false,
        }
    } else {
        false
    }
}

/// Determines if diagnostic is related to deprecated code
fn is_deprecated(rd: &RustDiagnostic) -> bool {
    if let Some(code) = &rd.code {
        match code.code.as_str() {
            "deprecated" => true,
            _ => false,
        }
    } else {
        false
    }
}

enum MappedRustChildDiagnostic {
    Related(DiagnosticRelatedInformation),
    SuggestedFix(CodeAction),
    MessageLine(String),
}

fn map_rust_child_diagnostic(
    rd: &RustDiagnostic,
    workspace_root: &PathBuf,
) -> MappedRustChildDiagnostic {
    let span: &DiagnosticSpan = match rd.spans.iter().find(|s| s.is_primary) {
        Some(span) => span,
        None => {
            // `rustc` uses these spanless children as a way to print multi-line
            // messages
            return MappedRustChildDiagnostic::MessageLine(rd.message.clone());
        }
    };

    // If we have a primary span use its location, otherwise use the parent
    let location = map_span_to_location(&span, workspace_root);

    if let Some(suggested_replacement) = &span.suggested_replacement {
        // Include our replacement in the title unless it's empty
        let title = if !suggested_replacement.is_empty() {
            format!("{}: '{}'", rd.message, suggested_replacement)
        } else {
            rd.message.clone()
        };

        let edit = {
            let edits = vec![TextEdit::new(location.range, suggested_replacement.clone())];
            let mut edit_map = std::collections::HashMap::new();
            edit_map.insert(location.uri, edits);
            WorkspaceEdit::new(edit_map)
        };

        MappedRustChildDiagnostic::SuggestedFix(CodeAction {
            title,
            kind: Some("quickfix".to_string()),
            diagnostics: None,
            edit: Some(edit),
            command: None,
            is_preferred: None,
        })
    } else {
        MappedRustChildDiagnostic::Related(DiagnosticRelatedInformation {
            location,
            message: rd.message.clone(),
        })
    }
}

#[derive(Debug)]
pub(crate) struct MappedRustDiagnostic {
    pub location: Location,
    pub diagnostic: Diagnostic,
    pub fixes: Vec<CodeAction>,
}

/// Converts a Rust root diagnostic to LSP form
///
/// This flattens the Rust diagnostic by:
///
/// 1. Creating a LSP diagnostic with the root message and primary span.
/// 2. Adding any labelled secondary spans to `relatedInformation`
/// 3. Categorising child diagnostics as either `SuggestedFix`es,
///    `relatedInformation` or additional message lines.
///
/// If the diagnostic has no primary span this will return `None`
pub(crate) fn map_rust_diagnostic_to_lsp(
    rd: &RustDiagnostic,
    workspace_root: &PathBuf,
) -> Option<MappedRustDiagnostic> {
    let primary_span = rd.spans.iter().find(|s| s.is_primary)?;

    let location = map_span_to_location(&primary_span, workspace_root);

    let severity = map_level_to_severity(rd.level);
    let mut primary_span_label = primary_span.label.as_ref();

    let mut source = String::from("rustc");
    let mut code = rd.code.as_ref().map(|c| c.code.clone());
    if let Some(code_val) = &code {
        // See if this is an RFC #2103 scoped lint (e.g. from Clippy)
        let scoped_code: Vec<&str> = code_val.split("::").collect();
        if scoped_code.len() == 2 {
            source = String::from(scoped_code[0]);
            code = Some(String::from(scoped_code[1]));
        }
    }

    let mut related_information = vec![];
    let mut tags = vec![];

    // If error occurs from macro expansion, add related info pointing to
    // where the error originated
    if !is_from_macro(&primary_span.file_name) && primary_span.expansion.is_some() {
        let def_loc = map_span_to_location_naive(&primary_span, workspace_root);
        related_information.push(DiagnosticRelatedInformation {
            location: def_loc,
            message: "Error originated from macro here".to_string(),
        });
    }

    for secondary_span in rd.spans.iter().filter(|s| !s.is_primary) {
        let related = map_secondary_span_to_related(secondary_span, workspace_root);
        if let Some(related) = related {
            related_information.push(related);
        }
    }

    let mut fixes = vec![];
    let mut message = rd.message.clone();
    for child in &rd.children {
        let child = map_rust_child_diagnostic(&child, workspace_root);
        match child {
            MappedRustChildDiagnostic::Related(related) => related_information.push(related),
            MappedRustChildDiagnostic::SuggestedFix(code_action) => fixes.push(code_action),
            MappedRustChildDiagnostic::MessageLine(message_line) => {
                write!(&mut message, "\n{}", message_line).unwrap();

                // These secondary messages usually duplicate the content of the
                // primary span label.
                primary_span_label = None;
            }
        }
    }

    if let Some(primary_span_label) = primary_span_label {
        write!(&mut message, "\n{}", primary_span_label).unwrap();
    }

    if is_unused_or_unnecessary(rd) {
        tags.push(DiagnosticTag::Unnecessary);
    }

    if is_deprecated(rd) {
        tags.push(DiagnosticTag::Deprecated);
    }

    let diagnostic = Diagnostic {
        range: location.range,
        severity,
        code: code.map(NumberOrString::String),
        source: Some(source),
        message,
        related_information: if !related_information.is_empty() {
            Some(related_information)
        } else {
            None
        },
        tags: if !tags.is_empty() { Some(tags) } else { None },
    };

    Some(MappedRustDiagnostic { location, diagnostic, fixes })
}

/// Returns a `Url` object from a given path, will lowercase drive letters if present.
/// This will only happen when processing windows paths.
///
/// When processing non-windows path, this is essentially the same as `Url::from_file_path`.
pub fn url_from_path_with_drive_lowercasing(
    path: impl AsRef<Path>,
) -> Result<Url, Box<dyn std::error::Error + Send + Sync>> {
    let component_has_windows_drive = path.as_ref().components().any(|comp| {
        if let Component::Prefix(c) = comp {
            match c.kind() {
                Prefix::Disk(_) | Prefix::VerbatimDisk(_) => return true,
                _ => return false,
            }
        }
        false
    });

    // VSCode expects drive letters to be lowercased, where rust will uppercase the drive letters.
    if component_has_windows_drive {
        let url_original = Url::from_file_path(&path)
            .map_err(|_| format!("can't convert path to url: {}", path.as_ref().display()))?;

        let drive_partition: Vec<&str> = url_original.as_str().rsplitn(2, ':').collect();

        // There is a drive partition, but we never found a colon.
        // This should not happen, but in this case we just pass it through.
        if drive_partition.len() == 1 {
            return Ok(url_original);
        }

        let joined = drive_partition[1].to_ascii_lowercase() + ":" + drive_partition[0];
        let url = Url::from_str(&joined).expect("This came from a valid `Url`");

        Ok(url)
    } else {
        Ok(Url::from_file_path(&path)
            .map_err(|_| format!("can't convert path to url: {}", path.as_ref().display()))?)
    }
}

// `Url` is not able to parse windows paths on unix machines.
#[cfg(target_os = "windows")]
#[cfg(test)]
mod path_conversion_windows_tests {
    use super::url_from_path_with_drive_lowercasing;
    #[test]
    fn test_lowercase_drive_letter_with_drive() {
        let url = url_from_path_with_drive_lowercasing("C:\\Test").unwrap();

        assert_eq!(url.to_string(), "file:///c:/Test");
    }

    #[test]
    fn test_drive_without_colon_passthrough() {
        let url = url_from_path_with_drive_lowercasing(r#"\\localhost\C$\my_dir"#).unwrap();

        assert_eq!(url.to_string(), "file://localhost/C$/my_dir");
    }
}
