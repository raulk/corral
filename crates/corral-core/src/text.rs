/// Truncate `s` to at most `max` chars; on overflow, replaces the tail with `…`.
/// The ellipsis counts toward `max`, so output is always `<= max` chars.
pub fn truncate_end(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}
