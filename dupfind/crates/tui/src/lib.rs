//! Stub for Phase 0. The real TUI (ratatui + crossterm) lands in Phase 7 and reads
//! `dupfind_core::report` directly, with no duplicated logic.

/// Placeholder so the crate exists and links against `core`.
pub fn placeholder() -> &'static str {
    "dupfind TUI: not implemented until Phase 7"
}

#[cfg(test)]
mod tests {
    #[test]
    fn stub_links_core() {
        assert!(!super::placeholder().is_empty());
    }
}
