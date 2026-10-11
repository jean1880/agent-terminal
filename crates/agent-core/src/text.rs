//! Characters a reader cannot see or that change what they see: the one check every card that
//! shows agent- or server-supplied text (paths, server names, reply options) relies on.

/// Whether `c` is invisible, reorders text, or is a control or unusual space: zero-width and
/// joiner characters, bidi marks, embeddings and isolates, tag characters, variation selectors,
/// Hangul fillers, the Braille blank, the soft hyphen, and every space but the plain one.
pub fn is_hidden(c: char) -> bool {
    matches!(c,
        '\u{00A0}' | '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}'
        | '\u{17B4}' | '\u{17B5}' | '\u{180B}'..='\u{180F}'
        | '\u{1680}' | '\u{2800}' | '\u{2000}'..='\u{200F}' | '\u{2028}'..='\u{202F}'
        | '\u{205F}'..='\u{206F}' | '\u{3000}' | '\u{3164}'
        | '\u{FE00}'..='\u{FE0E}' | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF9}'..='\u{FFFC}'
        | '\u{E0000}'..='\u{E007F}' | '\u{E0100}'..='\u{E01EF}'
    ) || c.is_control()
}

/// Whether `text` holds any [`is_hidden`] character.
pub fn has_hidden(text: &str) -> bool {
    text.chars().any(is_hidden)
}

/// `text` without its [`is_hidden`] characters.
pub fn without_hidden(text: &str) -> String {
    text.chars().filter(|c| !is_hidden(*c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoofing_characters_are_hidden_and_ordinary_text_is_not() {
        for c in [
            '\u{200B}',
            '\u{202E}',
            '\u{2066}',
            '\u{E0041}',
            '\u{FE0E}',
            '\u{3164}',
            '\u{FFA0}',
            '\u{115F}',
            '\u{1160}',
            '\u{2000}',
            '\u{200A}',
            '\u{202F}',
            '\u{205F}',
            '\u{3000}',
            '\u{00A0}',
            '\n',
            '\u{7F}',
            '\u{180E}',
            '\u{2800}',
        ] {
            assert!(is_hidden(c), "{:x}", c as u32);
        }
        // An emoji's own presentation selector is part of what is seen.
        assert!(!is_hidden('\u{FE0F}'));
        for c in ['a', ' ', 'é', 'а', '日', '/', '~'] {
            assert!(!is_hidden(c), "{c}");
        }
        assert_eq!(without_hidden("ev\u{202E}il\u{200B}"), "evil");
        assert!(has_hidden("a\u{3164}b"));
    }
}
