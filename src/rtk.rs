use regex::Regex;
use std::sync::LazyLock;

static RE_DIFF_INDEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"index [0-9a-f]+\.\.[0-9a-f]+( [0-9]+)?\n").unwrap());
static RE_DEDUP_LOG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^(.+)\n(?:\1\n)+").unwrap());
static RE_ANSI_ESCAPES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap());

/// Apply 9router RTK filters to compress prompt / tool results
pub fn compress_text(text: &str) -> String {
    if text.len() < 256 {
        return text.to_string();
    }

    let mut result = text.to_string();

    // 1. Strip ANSI escape sequences
    if result.contains("\x1b[") {
        result = RE_ANSI_ESCAPES.replace_all(&result, "").to_string();
    }

    // 2. Git diff index hashes reduction
    if result.contains("diff --git") {
        result = RE_DIFF_INDEX.replace_all(&result, "").to_string();
    }

    // 3. Consecutive duplicate log lines deduplication
    result = RE_DEDUP_LOG.replace_all(&result, "$1\n[...repeated lines deduped...]\n").to_string();

    result
}
