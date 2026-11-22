//! Caller-controlled text made safe for a terminal row: whitespace folds to
//! single spaces, and the characters that would move the cursor, recolour the
//! row or reorder what is drawn after it are dropped. Dropping ESC outright
//! neutralises an ANSI sequence too — only its inert letters remain.
//!
//! One copy, because the listing, the store's ambiguity error and the output
//! renderer all need it, and security logic must not drift by copy.

/// Folds `s` to one terminal-safe row.
pub fn cell(s: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in s.trim().chars() {
        if c.is_whitespace() {
            space = true;
        } else if c.is_control() || reordering(c) {
            // Dropped rather than folded to a space: an escape sequence is ESC
            // plus ordinary letters, and spacing it out would leave the letters.
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    out
}

/// The characters that move or hide text while occupying no space of their
/// own: bidi overrides and isolates, zero-width spaces, the byte order mark.
/// ZERO WIDTH JOINER is kept — it holds multi-part emoji together.
pub fn reordering(c: char) -> bool {
    matches!(c, '\u{feff}' | '\u{200b}'..='\u{200c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_whitespace_and_drops_what_repaints_the_row() {
        assert_eq!(cell("  a \n\t b  "), "a b");
        assert_eq!(cell("\u{1b}[31mred\u{1b}[0m"), "[31mred[0m");
        assert_eq!(cell("a\u{202e}b\u{200b}c"), "abc");
        assert_eq!(cell("👨\u{200d}👩"), "👨\u{200d}👩");
    }
}
