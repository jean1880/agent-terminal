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

/// Whether `c` is a bidi control or a zero-width character: the narrow set a command card
/// refuses, narrower than [`is_hidden`].
pub fn is_bidi_or_zero_width(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}' | '\u{FEFF}' | '\u{E0000}'..='\u{E007F}')
}

/// Whether `s` holds a control character other than those in `allowed`, or an
/// [`is_bidi_or_zero_width`] one: what makes a command line unsafe to show as text.
pub fn has_control_or_bidi(s: &str, allowed: &[char]) -> bool {
    s.chars()
        .any(|c| (c.is_control() && !allowed.contains(&c)) || is_bidi_or_zero_width(c))
}

/// `s` cut to at most `max` bytes at a character boundary.
pub fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The length of `v` serialised as JSON, or `usize::MAX` if it cannot be: a size check then
/// refuses what it could not measure.
pub fn json_len<T: serde::Serialize + ?Sized>(v: &T) -> usize {
    serde_json::to_string(v).map_or(usize::MAX, |s| s.len())
}

/// `text` with CRLF and every other line separator a model may honour (`\r`, vertical tab,
/// form feed, NEL, U+2028, U+2029) turned into `\n`, so a line check sees every line.
pub fn normalise_line_breaks(text: &str) -> String {
    text.replace("\r\n", "\n")
        .chars()
        .map(|c| match c {
            '\r' | '\u{0b}' | '\u{0c}' | '\u{85}' | '\u{2028}' | '\u{2029}' => '\n',
            c => c,
        })
        .collect()
}

/// `lines` joined with `\n`, each one `matches` accepts quoted with `> `. The caller picks the
/// iterator (`lines()` or `split('\n')`), and so whether a trailing empty line is kept.
pub fn quote_matching_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    matches: impl Fn(&str) -> bool,
) -> String {
    lines
        .map(|line| {
            if matches(line) {
                format!("> {line}")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
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

    #[test]
    fn control_or_bidi_honours_the_allowed_controls_only() {
        assert!(!has_control_or_bidi("a\nb", &['\n']));
        assert!(has_control_or_bidi("a\tb", &['\n']));
        assert!(!has_control_or_bidi("a\tb\n", &['\n', '\t']));
        assert!(has_control_or_bidi("a\u{202E}b", &['\n', '\t']));
        assert!(has_control_or_bidi("\u{E0041}", &[]));
        // Wider hidden characters are not this check's business.
        assert!(!has_control_or_bidi("a\u{00A0}b\u{3164}", &[]));
    }

    #[test]
    fn truncate_bytes_keeps_whole_characters() {
        assert_eq!(truncate_bytes("héllo", 2), "h");
        assert_eq!(truncate_bytes("héllo", 3), "hé");
        assert_eq!(truncate_bytes("abc", 9), "abc");
        assert_eq!(truncate_bytes("abc", 0), "");
    }

    #[test]
    fn json_len_measures_the_encoding() {
        assert_eq!(json_len("a\"b"), 6);
        assert_eq!(json_len(&serde_json::json!({"k": 1})), 7);
    }

    #[test]
    fn line_breaks_normalise_and_quoting_follows_the_iterator() {
        let n = normalise_line_breaks("a\r\nb\rc\u{2028}d\u{85}e\u{0b}f\u{0c}g\u{2029}");
        assert_eq!(n, "a\nb\nc\nd\ne\nf\ng\n");
        let is_x = |l: &str| l.starts_with('x');
        assert_eq!(quote_matching_lines("x1\ny\n".lines(), is_x), "> x1\ny");
        assert_eq!(
            quote_matching_lines("x1\ny\n".split('\n'), is_x),
            "> x1\ny\n"
        );
    }
}
