//! Lightweight backtrace capture.

use serde::Serialize;
use std::backtrace::Backtrace;

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
}

/// Capture the current backtrace.
///
/// Returns an empty vector when `RUST_BACKTRACE` is unset or disabled — the
/// SDK does not force-enable backtraces because they can be expensive.
pub(crate) fn capture() -> Vec<Frame> {
    let bt = Backtrace::capture();
    if bt.status() != std::backtrace::BacktraceStatus::Captured {
        return Vec::new();
    }
    parse_backtrace_text(&bt.to_string())
}

/// Parse a `Backtrace::to_string()` rendering into [`Frame`]s.
fn parse_backtrace_text(text: &str) -> Vec<Frame> {
    let mut frames = Vec::new();
    let mut index = 0usize;
    let mut current_fn: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim_start();

        // Frame headers look like `   0: errorgap::tests::case_a`.
        if let Some(rest) = strip_frame_header(trimmed) {
            // Flush previous frame with no location info.
            if let Some(func) = current_fn.take() {
                frames.push(Frame {
                    file: None,
                    line: None,
                    function: Some(func),
                    in_app: false,
                    index,
                });
                index += 1;
            }
            current_fn = Some(rest.to_string());
            continue;
        }

        // Location lines look like `             at src/lib.rs:42`.
        if let Some(loc) = trimmed.strip_prefix("at ") {
            let (file, line_no) = split_file_line(loc);
            let function = current_fn.take();
            let in_app = is_in_app(file.as_deref(), function.as_deref());
            frames.push(Frame {
                file,
                line: line_no,
                function,
                in_app,
                index,
            });
            index += 1;
        }
    }

    if let Some(func) = current_fn.take() {
        let in_app = is_in_app(None, Some(&func));
        frames.push(Frame {
            file: None,
            line: None,
            function: Some(func),
            in_app,
            index,
        });
    }

    frames
}

fn strip_frame_header(line: &str) -> Option<&str> {
    let colon = line.find(':')?;
    let prefix = &line[..colon];
    if !prefix.chars().all(|c| c.is_ascii_digit()) || prefix.is_empty() {
        return None;
    }
    Some(line[colon + 1..].trim())
}

fn split_file_line(loc: &str) -> (Option<String>, Option<u32>) {
    let loc = loc.trim();
    if let Some(idx) = loc.rfind(':') {
        let (file, rest) = loc.split_at(idx);
        let line = rest.trim_start_matches(':').parse::<u32>().ok();
        if line.is_some() {
            return (Some(file.to_string()), line);
        }
    }
    (Some(loc.to_string()), None)
}

fn is_in_app(file: Option<&str>, function: Option<&str>) -> bool {
    if let Some(func) = function {
        if func.starts_with("std::")
            || func.starts_with("core::")
            || func.starts_with("alloc::")
            || func.starts_with("rustc_")
            || func.starts_with("__rust_")
            || func.starts_with("tokio::")
        {
            return false;
        }
    }
    if let Some(file) = file {
        if file.contains("/rustc/")
            || file.contains("/.cargo/registry/")
            || file.contains("/.cargo/git/")
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
   0: errorgap::tests::case_a
             at src/lib.rs:42
   1: main
             at src/main.rs:7";
        let frames = parse_backtrace_text(text);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].function.as_deref(), Some("errorgap::tests::case_a"));
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
        let frames = parse_backtrace_text(text);
        assert_eq!(frames[0].in_app, false);
        assert_eq!(frames[1].in_app, true);
    }
}
