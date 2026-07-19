//! Lightweight backtrace capture with source excerpts.

use serde::Serialize;
use std::backtrace::Backtrace;
use std::path::{Path, PathBuf};

const SOURCE_CONTEXT_LINES: usize = 6;
const MAX_SOURCE_LINE_LEN: usize = 400;
const MAX_SOURCE_FILE_BYTES: u64 = 2_000_000;

/// One frame in a notice's backtrace.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Frame {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    pub in_app: bool,
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceExcerpt>,
}

/// A source excerpt surrounding a frame's line, shipped so the dashboard can
/// render highlighted source without any repository integration.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SourceExcerpt {
    pub start_line: u32,
    pub lines: Vec<String>,
}

/// Capture the current backtrace, resolving source excerpts against `root`.
///
/// Returns an empty vector when `RUST_BACKTRACE` is unset or disabled — the
/// SDK does not force-enable backtraces because they can be expensive.
pub(crate) fn capture(root: Option<&str>) -> Vec<Frame> {
    let bt = Backtrace::capture();
    if bt.status() != std::backtrace::BacktraceStatus::Captured {
        return Vec::new();
    }
    parse_backtrace_text(&bt.to_string(), root)
}

/// Parse a `Backtrace::to_string()` rendering into [`Frame`]s.
fn parse_backtrace_text(text: &str, root: Option<&str>) -> Vec<Frame> {
    let mut frames = Vec::new();
    let mut index = 0usize;
    let mut current_fn: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim_start();

        // Frame headers look like `   0: errorgap::tests::case_a`. If the
        // previous header had no location line it carries no file; drop it,
        // since the ingestion contract requires a `file` per frame.
        if let Some(rest) = strip_frame_header(trimmed) {
            current_fn = Some(rest.to_string());
            continue;
        }

        // Location lines look like `             at src/lib.rs:42`.
        if let Some(loc) = trimmed.strip_prefix("at ") {
            let (file, line_no) = split_file_line(loc);
            let function = current_fn.take();
            frames.push(make_frame(file, line_no, function, root, index));
            index += 1;
        }
    }

    // Drop the leading internal frames — the backtrace-capture machinery and
    // the SDK's own `capture` / `build` / `notify…` frames — so the top frame
    // is the user's application code. Then re-index from zero.
    let internal = frames.iter().take_while(|f| is_internal_frame(f)).count();
    let mut frames: Vec<Frame> = frames.into_iter().skip(internal).collect();
    for (i, frame) in frames.iter_mut().enumerate() {
        frame.index = i;
    }
    frames
}

/// Frames belonging to the capture machinery or the SDK itself. Trait/impl
/// methods render as `<errorgap::Type>::method`, so match by substring.
fn is_internal_frame(frame: &Frame) -> bool {
    let Some(func) = frame.function.as_deref() else {
        return false;
    };
    func.contains("errorgap::")
        || func.contains("backtrace_rs")
        || func.contains("std::backtrace::")
}

fn make_frame(
    raw_file: Option<String>,
    line: Option<u32>,
    function: Option<String>,
    root: Option<&str>,
    index: usize,
) -> Frame {
    let in_app = is_in_app(raw_file.as_deref(), function.as_deref());
    let source = match (&raw_file, line) {
        (Some(file), Some(line)) => source_excerpt(&resolve_path(file, root), line),
        _ => None,
    };
    let display_file = raw_file.map(|file| display_path(&file, root));
    Frame {
        file: display_file,
        line,
        function,
        in_app,
        index,
        source,
    }
}

fn resolve_path(raw: &str, root: Option<&str>) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(root) = root {
        Path::new(root).join(raw)
    } else {
        path.to_path_buf()
    }
}

/// Present a friendly path: relative to the app root for local frames, or
/// `crate-x.y.z/src/…` for dependency frames pulled from the cargo registry.
fn display_path(raw: &str, root: Option<&str>) -> String {
    if let Some(root) = root {
        let root = root.trim_end_matches('/');
        if let Some(rest) = raw.strip_prefix(root) {
            return rest.trim_start_matches('/').to_string();
        }
    }
    for marker in ["/registry/src/", "/git/checkouts/"] {
        if let Some(idx) = raw.find(marker) {
            let tail = &raw[idx + marker.len()..];
            // Drop the registry index / checkout hash segment.
            if let Some(slash) = tail.find('/') {
                return tail[slash + 1..].to_string();
            }
            return tail.to_string();
        }
    }
    raw.to_string()
}

fn source_excerpt(path: &Path, line: u32) -> Option<SourceExcerpt> {
    if line == 0 {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SOURCE_FILE_BYTES {
        return None;
    }
    let contents = std::fs::read_to_string(path).ok()?;
    let all: Vec<&str> = contents.lines().collect();
    let line_idx = line as usize;
    if line_idx > all.len() {
        return None;
    }
    let start = line_idx.saturating_sub(SOURCE_CONTEXT_LINES).max(1);
    let end = (line_idx + SOURCE_CONTEXT_LINES).min(all.len());
    let lines: Vec<String> = all[start - 1..end]
        .iter()
        .map(|l| l.chars().take(MAX_SOURCE_LINE_LEN).collect())
        .collect();
    Some(SourceExcerpt {
        start_line: start as u32,
        lines,
    })
}

fn strip_frame_header(line: &str) -> Option<&str> {
    let colon = line.find(':')?;
    let prefix = &line[..colon];
    if !prefix.chars().all(|c| c.is_ascii_digit()) || prefix.is_empty() {
        return None;
    }
    Some(line[colon + 1..].trim())
}

/// Split a `file`, `file:line`, or `file:line:column` location. Peels up to
/// two trailing `:<number>` segments (column then line); the line is the last
/// one peeled.
fn split_file_line(loc: &str) -> (Option<String>, Option<u32>) {
    let mut file = loc.trim();
    let mut line = None;

    for _ in 0..2 {
        let Some(idx) = file.rfind(':') else { break };
        let tail = &file[idx + 1..];
        match tail.parse::<u32>() {
            Ok(n) => {
                line = Some(n);
                file = &file[..idx];
            }
            Err(_) => break,
        }
    }

    if line.is_some() {
        (Some(file.to_string()), line)
    } else {
        (Some(loc.trim().to_string()), None)
    }
}

fn is_in_app(file: Option<&str>, function: Option<&str>) -> bool {
    if let Some(func) = function {
        if func.starts_with("std::")
            || func.starts_with("core::")
            || func.starts_with("alloc::")
            || func.starts_with("rustc_")
            || func.starts_with("__rust_")
            || func.starts_with("tokio::")
            || func.starts_with("errorgap::")
        {
            return false;
        }
    }
    if let Some(file) = file {
        if file.contains("/rustc/")
            || file.contains("/.cargo/registry/")
            || file.contains("/.cargo/git/")
            || file.contains("/registry/src/")
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frame_with_location() {
        let text = "\
   0: my_app::tests::case_a
             at src/lib.rs:42
   1: main
             at src/main.rs:7";
        let frames = parse_backtrace_text(text, None);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].function.as_deref(), Some("my_app::tests::case_a"));
        assert_eq!(frames[0].file.as_deref(), Some("src/lib.rs"));
        assert_eq!(frames[0].line, Some(42));
    }

    #[test]
    fn marks_std_frames_as_not_in_app() {
        let text = "\
   0: std::panic::catch_unwind
             at /rustc/abc/library/std/src/panic.rs:1
   1: my_app::handler
             at src/handler.rs:10";
        let frames = parse_backtrace_text(text, None);
        assert!(!frames[0].in_app);
        assert!(frames[1].in_app);
    }

    #[test]
    fn resolves_source_excerpt_against_root() {
        // This crate's own source file exists on disk; resolve its excerpt.
        let root = env!("CARGO_MANIFEST_DIR");
        let text = "\
   0: my_app::backtrace::marker
             at src/backtrace.rs:1";
        let frames = parse_backtrace_text(text, Some(root));
        let source = frames[0].source.as_ref().expect("source excerpt");
        assert_eq!(source.start_line, 1);
        assert!(source.lines[0].contains("Lightweight backtrace"));
    }

    #[test]
    fn parses_file_line_and_column() {
        // Modern backtraces render `file:line:column`; the line, not the
        // column, must be captured.
        let text = "\
   0: my_app::handler
             at src/handler.rs:42:9";
        let frames = parse_backtrace_text(text, None);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].file.as_deref(), Some("src/handler.rs"));
        assert_eq!(frames[0].line, Some(42));
    }

    #[test]
    fn display_path_strips_registry_prefix() {
        let raw = "/usr/local/cargo/registry/src/index.crates.io-abc/reqwest-0.12.0/src/lib.rs";
        assert_eq!(display_path(raw, None), "reqwest-0.12.0/src/lib.rs");
    }

    #[test]
    fn drops_leading_sdk_frames() {
        let text = "\
   0: errorgap::backtrace::capture
             at src/backtrace.rs:39
   1: errorgap::notify_error
             at src/lib.rs:100
   2: my_app::checkout
             at src/checkout.rs:20
   3: my_app::main
             at src/main.rs:5";
        let frames = parse_backtrace_text(text, None);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].function.as_deref(), Some("my_app::checkout"));
        assert_eq!(frames[0].index, 0);
        assert_eq!(frames[1].function.as_deref(), Some("my_app::main"));
    }

    #[test]
    fn drops_frames_without_a_file() {
        // The middle header has no `at` location line and must be dropped:
        // the ingestion contract requires a `file` on every frame.
        let text = "\
   0: app::handler
             at src/handler.rs:10
   1: core::ops::function::FnOnce::call_once
   2: app::main
             at src/main.rs:3";
        let frames = parse_backtrace_text(text, None);
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f.file.is_some()));
        assert_eq!(frames[0].function.as_deref(), Some("app::handler"));
        assert_eq!(frames[1].function.as_deref(), Some("app::main"));
    }
}
