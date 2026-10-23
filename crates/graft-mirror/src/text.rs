//! Cleaning text that GitHub will show or that GitHub sent back.

/// Whether `c` reorders or hides text (bidi controls and marks).
fn is_bidi(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Cleans text that GitHub will show or that comes from a draft: control
/// characters and bidi controls go, `@` gets a zero-width space so it does
/// not ping anyone, and the result is capped at `max` characters. With
/// `multiline`, newlines stay.
pub fn clean_text(text: &str, max: usize, multiline: bool) -> String {
    let mut out = String::new();
    for (n, c) in text.chars().enumerate() {
        if n >= max {
            break;
        }
        match c {
            '\n' if multiline => out.push('\n'),
            '\t' => out.push(' '),
            '@' => out.push_str("@\u{200b}"),
            c if c.is_control() || is_bidi(c) => {}
            c => out.push(c),
        }
    }
    out
}
