/// Convert a human-readable key name (e.g. `"Return"`, `"F1"`, `"a"`) to an X11 keysym.
///
/// Returns `None` for unrecognized names. Single-character strings are
/// converted via [`char_to_keysym`]. Matching is case-insensitive.
pub fn key_name_to_keysym(key: &str) -> Option<u32> {
    match key.to_lowercase().as_str() {
        "return" | "enter" => Some(0xff0d),
        "tab" => Some(0xff09),
        "escape" | "esc" => Some(0xff1b),
        "backspace" => Some(0xff08),
        "delete" => Some(0xffff),
        "space" => Some(0x0020),
        "up" => Some(0xff52),
        "down" => Some(0xff54),
        "left" => Some(0xff51),
        "right" => Some(0xff53),
        "home" => Some(0xff50),
        "end" => Some(0xff57),
        "page_up" => Some(0xff55),
        "page_down" => Some(0xff56),
        "f1" => Some(0xffbe),
        "f2" => Some(0xffbf),
        "f3" => Some(0xffc0),
        "f4" => Some(0xffc1),
        "f5" => Some(0xffc2),
        "f6" => Some(0xffc3),
        "f7" => Some(0xffc4),
        "f8" => Some(0xffc5),
        "f9" => Some(0xffc6),
        "f10" => Some(0xffc7),
        "f11" => Some(0xffc8),
        "f12" => Some(0xffc9),
        _ if key.len() == 1 => Some(char_to_keysym(key.chars().next().unwrap())),
        _ => None,
    }
}

/// Convert a Unicode character to its X11 keysym value.
///
/// Latin-1 characters (U+0020..U+00FF) map directly to their code point.
/// Characters above U+00FF use the `0x01000000 + code_point` convention.
pub fn char_to_keysym(ch: char) -> u32 {
    // For ASCII, X11 keysyms match Unicode code points for printable chars
    // For Latin-1 (0x20-0xff), keysym == Unicode code point
    let cp = ch as u32;
    if (0x20..=0xff).contains(&cp) {
        cp
    } else if cp > 0xff {
        // Unicode keysyms: 0x01000000 + Unicode code point
        0x01000000 + cp
    } else {
        cp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_named_keys() {
        assert_eq!(key_name_to_keysym("Return"), Some(0xff0d));
        assert_eq!(key_name_to_keysym("enter"), Some(0xff0d));
        assert_eq!(key_name_to_keysym("Tab"), Some(0xff09));
        assert_eq!(key_name_to_keysym("Escape"), Some(0xff1b));
        assert_eq!(key_name_to_keysym("esc"), Some(0xff1b));
        assert_eq!(key_name_to_keysym("BackSpace"), Some(0xff08));
        assert_eq!(key_name_to_keysym("Delete"), Some(0xffff));
        assert_eq!(key_name_to_keysym("Space"), Some(0x0020));
        assert_eq!(key_name_to_keysym("Up"), Some(0xff52));
        assert_eq!(key_name_to_keysym("Down"), Some(0xff54));
        assert_eq!(key_name_to_keysym("Left"), Some(0xff51));
        assert_eq!(key_name_to_keysym("Right"), Some(0xff53));
        assert_eq!(key_name_to_keysym("Home"), Some(0xff50));
        assert_eq!(key_name_to_keysym("End"), Some(0xff57));
        assert_eq!(key_name_to_keysym("Page_Up"), Some(0xff55));
        assert_eq!(key_name_to_keysym("Page_Down"), Some(0xff56));
        assert_eq!(key_name_to_keysym("F1"), Some(0xffbe));
        assert_eq!(key_name_to_keysym("F6"), Some(0xffc3));
        assert_eq!(key_name_to_keysym("F12"), Some(0xffc9));
    }

    #[test]
    fn test_key_name_case_insensitive() {
        assert_eq!(key_name_to_keysym("RETURN"), Some(0xff0d));
        assert_eq!(key_name_to_keysym("rEtUrN"), Some(0xff0d));
        assert_eq!(key_name_to_keysym("TAB"), Some(0xff09));
        assert_eq!(key_name_to_keysym("ESCAPE"), Some(0xff1b));
    }

    #[test]
    fn test_single_char_keys() {
        assert_eq!(key_name_to_keysym("a"), Some(char_to_keysym('a')));
        assert_eq!(key_name_to_keysym("z"), Some(char_to_keysym('z')));
        assert_eq!(key_name_to_keysym("0"), Some(char_to_keysym('0')));
        assert_eq!(key_name_to_keysym("!"), Some(char_to_keysym('!')));
    }

    #[test]
    fn test_unknown_key_returns_none() {
        assert_eq!(key_name_to_keysym("ctrl"), None);
        assert_eq!(key_name_to_keysym("alt"), None);
        assert_eq!(key_name_to_keysym("super"), None);
        assert_eq!(key_name_to_keysym("shift"), None);
        assert_eq!(key_name_to_keysym("unknown_key"), None);
    }

    #[test]
    fn test_char_to_keysym_printable_ascii() {
        assert_eq!(char_to_keysym('a'), 0x61);
        assert_eq!(char_to_keysym('A'), 0x41);
        assert_eq!(char_to_keysym('0'), 0x30);
        assert_eq!(char_to_keysym(' '), 0x20);
        assert_eq!(char_to_keysym('~'), 0x7e);
        // Latin-1 range
        assert_eq!(char_to_keysym('ñ'), 0xf1);
        assert_eq!(char_to_keysym('ÿ'), 0xff);
    }

    #[test]
    fn test_char_to_keysym_unicode() {
        // '€' is U+20AC
        assert_eq!(char_to_keysym('€'), 0x01000000 + 0x20AC);
        // '中' is U+4E2D
        assert_eq!(char_to_keysym('中'), 0x01000000 + 0x4E2D);
    }

    #[test]
    fn test_char_to_keysym_control() {
        // Control characters (< 0x20) map directly
        assert_eq!(char_to_keysym('\x00'), 0x00);
        assert_eq!(char_to_keysym('\x01'), 0x01);
        assert_eq!(char_to_keysym('\x1f'), 0x1f);
    }
}
