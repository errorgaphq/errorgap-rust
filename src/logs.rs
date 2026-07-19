//! Log-level canonicalization shared by the log delivery path.

/// Canonicalize a level to one of the six the ingestion API recognizes.
pub(crate) fn normalize_level(level: &str) -> &'static str {
    match level.trim().to_ascii_lowercase().as_str() {
        "warning" | "warn" => "warn",
        "err" | "severe" | "critical" | "alert" | "emergency" => "error",
        "notice" => "info",
        "trace" => "trace",
        "debug" => "debug",
        "info" => "info",
        "error" => "error",
        "fatal" => "fatal",
        _ => "info",
    }
}

/// Order levels so a minimum-level threshold can be applied client-side.
pub(crate) fn level_rank(level: &str) -> u8 {
    match level {
        "trace" => 0,
        "debug" => 10,
        "info" => 20,
        "warn" => 30,
        "error" => 40,
        "fatal" => 50,
        _ => 20,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_aliases() {
        assert_eq!(normalize_level("WARNING"), "warn");
        assert_eq!(normalize_level("critical"), "error");
        assert_eq!(normalize_level("notice"), "info");
        assert_eq!(normalize_level("nonsense"), "info");
        assert_eq!(normalize_level("fatal"), "fatal");
    }

    #[test]
    fn ranks_order() {
        assert!(level_rank("trace") < level_rank("debug"));
        assert!(level_rank("info") < level_rank("warn"));
        assert!(level_rank("warn") < level_rank("error"));
        assert!(level_rank("error") < level_rank("fatal"));
    }
}
