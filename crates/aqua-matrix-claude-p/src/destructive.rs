//! The destructive-command matcher — the single auditable place that decides
//! whether a request is dangerous enough to gate behind a chat confirmation.
//!
//! Phase A keys the plan/approve/execute flow off [`looks_destructive`]. The
//! same table is the substrate for Phase C's per-tool gate (which classifies a
//! concrete `Bash(...)` tool call rather than the free-text prompt), so it is
//! kept deliberately small, table-driven, and extensible. Each entry pairs a
//! human-readable class with the substrings/patterns that flag it and the
//! Claude Code `--allowedTools` scope pattern Phase C will grant.
//!
//! Matching is intentionally *coarse and fail-loud*: we would rather gate a
//! borderline-safe prompt (the user just answers "yes") than let a destructive
//! one slip through. It runs over lowercased, whitespace-normalised text so
//! `rm  -RF` and `git push   --force` still match.

/// One row of the matcher table.
pub struct Rule {
    /// Human-readable class, surfaced in the confirmation prompt.
    pub class: &'static str,
    /// Whole-WORD needles: flag the class only when the needle appears as a
    /// space-delimited token (e.g. `rm` matches "rm -rf x" but NOT "alarm").
    pub words: &'static [&'static str],
    /// Substring needles: flag the class when present anywhere (e.g. the
    /// distinctive multi-word `"git push --force"` or the `"-delete"` flag).
    pub substrings: &'static [&'static str],
    /// The Claude Code `--allowedTools` scope pattern this class maps onto,
    /// reused by Phase C's scope grants. Not used by Phase A directly (only the
    /// test asserts it), hence `allow(dead_code)` until Phase C wires it.
    #[allow(dead_code)]
    pub scope: &'static str,
}

/// The matcher table. Add a row to extend coverage (Phase C: `git reset
/// --hard`, `> file` truncation, `dd`, `mkfs`). Order is irrelevant — the
/// first matching rule wins only for the *reported class*, never for the
/// gate decision (which is "any rule matched").
pub const RULES: &[Rule] = &[
    Rule {
        class: "file deletion",
        // `rm` / `rm -rf` / `rmdir` as whole words (so "alarm"/"form" are safe);
        // `find … -delete` via the distinctive `-delete` flag substring.
        words: &["rm", "rmdir"],
        substrings: &["-delete"],
        scope: "Bash(rm:*)",
    },
    Rule {
        class: "force push",
        // `git push --force` / `git push -f` / `--force-with-lease`. These are
        // distinctive enough to match as substrings of the normalised text.
        words: &[],
        substrings: &[
            "git push --force",
            "git push -f",
            "--force-with-lease",
            "push --force",
            "push -f",
        ],
        scope: "Bash(git push --force*)",
    },
];

/// Normalise text for matching: lowercase and collapse all runs of whitespace
/// to a single space. This makes `rm   -RF` and `git push\t--force` match the
/// table needles without an entry per spacing variant.
fn normalise(text: &str) -> String {
    let lower = text.to_lowercase();
    lower.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Does `text` (a prompt, or a concrete command) look destructive? Returns the
/// matched class for logging/UX, or `None` if nothing matched. Word needles are
/// matched on space-delimited token boundaries; substring needles anywhere.
pub fn classify(text: &str) -> Option<&'static str> {
    let norm = normalise(text);
    let tokens: Vec<&str> = norm.split(' ').collect();
    for rule in RULES {
        let word_hit = rule.words.iter().any(|w| tokens.contains(w));
        let substr_hit = rule.substrings.iter().any(|s| norm.contains(s));
        if word_hit || substr_hit {
            return Some(rule.class);
        }
    }
    None
}

/// Convenience boolean wrapper around [`classify`].
pub fn looks_destructive(text: &str) -> bool {
    classify(text).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_rm_variants() {
        assert_eq!(classify("rm /tmp/x"), Some("file deletion"));
        assert_eq!(classify("rm -rf /tmp/x"), Some("file deletion"));
        assert_eq!(classify("please rm -RF  /tmp/confirm-test"), Some("file deletion"));
        assert_eq!(classify("rmdir /tmp/x"), Some("file deletion"));
        assert_eq!(
            classify("find /tmp -name '*.log' -delete"),
            Some("file deletion")
        );
        // bare trailing rm
        assert_eq!(classify("now run rm"), Some("file deletion"));
    }

    #[test]
    fn flags_force_push_variants() {
        assert_eq!(classify("git push --force"), Some("force push"));
        assert_eq!(classify("git push -f origin main"), Some("force push"));
        assert_eq!(
            classify("git push --force-with-lease"),
            Some("force push")
        );
        assert_eq!(classify("git   push   --force"), Some("force push"));
    }

    #[test]
    fn ignores_benign_prompts() {
        assert!(!looks_destructive("what is the weather today?"));
        assert!(!looks_destructive("summarize this file for me"));
        // words that merely contain "rm" must not trip the matcher
        assert!(!looks_destructive("set an alarm and tell me a form"));
        assert!(!looks_destructive("git push origin feature-branch"));
        assert!(!looks_destructive("git status && git log"));
    }

    #[test]
    fn scope_patterns_are_present_for_phase_c() {
        // Phase C reuses these; assert they stay stable.
        let del = RULES.iter().find(|r| r.class == "file deletion").unwrap();
        assert_eq!(del.scope, "Bash(rm:*)");
        let fp = RULES.iter().find(|r| r.class == "force push").unwrap();
        assert_eq!(fp.scope, "Bash(git push --force*)");
    }
}
