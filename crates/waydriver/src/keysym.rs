/// A parsed keyboard chord: zero or more modifier keys to hold, plus a
/// target key to press and release.
///
/// Produced by [`parse_chord`]. The target can itself be a modifier key
/// (e.g. `"Shift"` alone presses Shift_L with no other modifiers), so the
/// canonical "modifier + key" distinction is just positional — modifiers
/// are every token except the last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    /// Keysyms to hold down in order before pressing the target. Released
    /// in reverse order afterwards.
    pub modifiers: Vec<u32>,
    /// The target keysym — pressed and released while the modifiers are
    /// held.
    pub key: u32,
}

/// Parse a chord specification like `"Ctrl+Shift+A"` or `"Ctrl-A"`, or a
/// bare key name like `"Return"` (no chord, empty modifier list).
///
/// Tokens are split on `+` or `-`. Each non-final token must be a modifier
/// name (`Ctrl`/`Control`, `Shift`, `Alt`, `Super`/`Meta`). The final token
/// can be any modifier name OR anything [`key_name_to_keysym`] accepts.
///
/// Matching is case-insensitive. Empty input or any unrecognized token
/// returns `None`.
pub fn parse_chord(input: &str) -> Option<Chord> {
    let trimmed = input.trim();
    // Special-case: a single-character input is always a literal key, even
    // when that character is one of our separators (`+` or `-`). Without
    // this, `parse_chord("+")` would try to split on `+`, end up with no
    // tokens, and return None — making the arithmetic plus key unreachable
    // through the chord API.
    if trimmed.chars().count() == 1 {
        let key = key_name_to_keysym(trimmed)?;
        return Some(Chord {
            modifiers: Vec::new(),
            key,
        });
    }

    let tokens: Vec<&str> = input
        .split(['+', '-'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    let (target_token, modifier_tokens) = tokens.split_last()?;

    let key = modifier_name_to_keysym(target_token).or_else(|| key_name_to_keysym(target_token))?;

    let modifiers = modifier_tokens
        .iter()
        .map(|m| modifier_name_to_keysym(m))
        .collect::<Option<Vec<u32>>>()?;

    Some(Chord { modifiers, key })
}

/// Map a modifier name (case-insensitive) to the X11 keysym for its
/// left-hand variant. Returns `None` for non-modifiers.
///
/// Aliases follow Playwright/web convention: `Control` == `Ctrl`;
/// `Meta` == `Super` == `Win` == `Windows` == `Cmd` == `Command` (on Linux
/// these are all the Super key, but the aliases make cross-platform test
/// specs portable).
pub fn modifier_name_to_keysym(name: &str) -> Option<u32> {
    match name.to_lowercase().as_str() {
        "ctrl" | "control" => Some(0xffe3),
        "shift" => Some(0xffe1),
        "alt" => Some(0xffe9),
        "super" | "meta" | "win" | "windows" | "cmd" | "command" => Some(0xffeb),
        _ => None,
    }
}

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

    // ── Modifier + chord parsing ───────────────────────────────────────────

    #[test]
    fn modifier_name_to_keysym_aliases() {
        // Ctrl / Control are the same.
        assert_eq!(modifier_name_to_keysym("Ctrl"), Some(0xffe3));
        assert_eq!(modifier_name_to_keysym("control"), Some(0xffe3));
        assert_eq!(modifier_name_to_keysym("CONTROL"), Some(0xffe3));
        // Shift.
        assert_eq!(modifier_name_to_keysym("Shift"), Some(0xffe1));
        assert_eq!(modifier_name_to_keysym("shift"), Some(0xffe1));
        // Alt.
        assert_eq!(modifier_name_to_keysym("alt"), Some(0xffe9));
        // Super / Meta / Win / Cmd all resolve to Super_L on Linux.
        assert_eq!(modifier_name_to_keysym("Super"), Some(0xffeb));
        assert_eq!(modifier_name_to_keysym("Meta"), Some(0xffeb));
        assert_eq!(modifier_name_to_keysym("win"), Some(0xffeb));
        assert_eq!(modifier_name_to_keysym("Windows"), Some(0xffeb));
        assert_eq!(modifier_name_to_keysym("cmd"), Some(0xffeb));
        assert_eq!(modifier_name_to_keysym("Command"), Some(0xffeb));
    }

    #[test]
    fn modifier_name_to_keysym_rejects_non_modifiers() {
        assert_eq!(modifier_name_to_keysym("Return"), None);
        assert_eq!(modifier_name_to_keysym("a"), None);
        assert_eq!(modifier_name_to_keysym(""), None);
    }

    #[test]
    fn parse_chord_single_key_has_empty_modifiers() {
        let c = parse_chord("Return").unwrap();
        assert!(c.modifiers.is_empty());
        assert_eq!(c.key, 0xff0d);
    }

    #[test]
    fn parse_chord_single_char() {
        let c = parse_chord("a").unwrap();
        assert!(c.modifiers.is_empty());
        assert_eq!(c.key, char_to_keysym('a'));
    }

    #[test]
    fn parse_chord_basic_ctrl_a() {
        let c = parse_chord("Ctrl+A").unwrap();
        assert_eq!(c.modifiers, vec![0xffe3]);
        assert_eq!(c.key, char_to_keysym('A'));
    }

    #[test]
    fn parse_chord_multiple_modifiers_preserve_order() {
        // Order matters — key_down is issued left-to-right, key_up right-to-left.
        let c = parse_chord("Ctrl+Shift+Alt+A").unwrap();
        assert_eq!(c.modifiers, vec![0xffe3, 0xffe1, 0xffe9]);
        assert_eq!(c.key, char_to_keysym('A'));
    }

    #[test]
    fn parse_chord_dash_separator_works() {
        let c = parse_chord("Ctrl-Shift-A").unwrap();
        assert_eq!(c.modifiers, vec![0xffe3, 0xffe1]);
        assert_eq!(c.key, char_to_keysym('A'));
    }

    #[test]
    fn parse_chord_mixed_separators() {
        let c = parse_chord("Ctrl+Shift-A").unwrap();
        assert_eq!(c.modifiers, vec![0xffe3, 0xffe1]);
        assert_eq!(c.key, char_to_keysym('A'));
    }

    #[test]
    fn parse_chord_is_case_insensitive() {
        let c = parse_chord("CTRL+shift+A").unwrap();
        assert_eq!(c.modifiers, vec![0xffe3, 0xffe1]);
    }

    #[test]
    fn parse_chord_named_key_target() {
        // Target can be a named key, not just a character.
        let c = parse_chord("Alt+Return").unwrap();
        assert_eq!(c.modifiers, vec![0xffe9]);
        assert_eq!(c.key, 0xff0d);
    }

    #[test]
    fn parse_chord_bare_modifier_is_single_key() {
        // "Ctrl" with no target is just a Ctrl keypress — no modifiers held,
        // target is Control_L.
        let c = parse_chord("Ctrl").unwrap();
        assert!(c.modifiers.is_empty());
        assert_eq!(c.key, 0xffe3);
    }

    #[test]
    fn parse_chord_empty_returns_none() {
        assert_eq!(parse_chord(""), None);
        assert_eq!(parse_chord("   "), None);
        // "-+-" is 3 chars, not single-char, and splits into empty tokens.
        assert_eq!(parse_chord("-+-"), None);
    }

    #[test]
    fn parse_chord_separator_char_is_a_literal_key() {
        // A single `+` or `-` is the arithmetic key itself, not a dangling
        // chord separator. Essential for calculator-style keyboard input.
        let plus = parse_chord("+").unwrap();
        assert!(plus.modifiers.is_empty());
        assert_eq!(plus.key, char_to_keysym('+'));
        let minus = parse_chord("-").unwrap();
        assert_eq!(minus.key, char_to_keysym('-'));
    }

    #[test]
    fn parse_chord_unknown_modifier_returns_none() {
        assert_eq!(parse_chord("Hyper+A"), None);
    }

    #[test]
    fn parse_chord_unknown_target_returns_none() {
        assert_eq!(parse_chord("Ctrl+NoSuchKey"), None);
    }

    #[test]
    fn parse_chord_non_modifier_in_middle_rejected() {
        // "Ctrl+A+B" — "A" isn't a modifier, so position-wise it can't be
        // held while pressing B. Parser rejects.
        assert_eq!(parse_chord("Ctrl+A+B"), None);
    }

    #[test]
    fn parse_chord_whitespace_is_trimmed() {
        let c = parse_chord("  Ctrl +  A  ").unwrap();
        assert_eq!(c.modifiers, vec![0xffe3]);
        assert_eq!(c.key, char_to_keysym('A'));
    }
}
