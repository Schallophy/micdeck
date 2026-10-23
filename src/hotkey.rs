//! The microphone's global shortcut: one chord, named in the settings file and owned by the tray window.
//!
//! The wire spelling is the human one (`Ctrl+Alt+M`), and `label` is the only spelling this program
//! writes back, so a hand-edited file and a recorded chord converge on the same text.

/// The first-run shortcut, and what a settings file written before this feature existed gets.
pub const DEFAULT: &str = "Ctrl+Alt+M";

// The four Win32 modifier bits. Named here so parsing and formatting need no window handle.
const ALT: u32 = 0x0001;
const CONTROL: u32 = 0x0002;
const SHIFT: u32 = 0x0004;
const WIN: u32 = 0x0008;

/// `VK_F12` is reserved for debuggers; a hot key on it can never be reliable.
const VK_F12: u32 = 0x7B;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Binding {
    pub mods: u32,
    pub vk: u32,
}

impl Binding {
    /// Modifiers in Ctrl, Alt, Shift, Win order, then the key.
    pub fn label(self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for (bit, name) in [(CONTROL, "Ctrl"), (ALT, "Alt"), (SHIFT, "Shift"), (WIN, "Win")] {
            if self.mods & bit != 0 {
                parts.push(name);
            }
        }
        let key = key_label(self.vk);
        parts.push(&key);
        parts.join("+")
    }
}

/// Parses what the page sends and what the store holds. The empty string is "no shortcut", not an
/// error: that is how the page clears one.
pub fn parse(text: &str) -> Result<Option<Binding>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let mut mods = 0;
    let mut key = None;
    for token in text.split('+') {
        let token = token.trim();
        if token.is_empty() {
            return Err(format!("“{text}”不是有效的快捷键"));
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= CONTROL,
            "alt" => mods |= ALT,
            "shift" => mods |= SHIFT,
            "win" | "windows" | "meta" => mods |= WIN,
            _ => {
                if key.is_some() {
                    return Err(format!("“{text}”只能有一个主键"));
                }
                key = Some(vk_of(token).ok_or_else(|| format!("不支持的按键：{token}"))?);
            }
        }
    }
    let vk = key.ok_or_else(|| "快捷键需要一个主键".to_string())?;
    if vk == VK_F12 {
        return Err("F12 保留给调试器".to_string());
    }
    // A global hot key consumes the key in every program. Doing that to a letter or to Space would
    // break typing everywhere, which cannot be what a chord recorder meant; only keys that neither
    // type nor navigate may stand alone.
    if mods == 0 && !is_standalone_key(vk) {
        return Err("除了 F1–F24 等功能键，快捷键至少要有一个修饰键".to_string());
    }
    Ok(Some(Binding { mods, vk }))
}

fn is_standalone_key(vk: u32) -> bool {
    (0x70..=0x87).contains(&vk) || matches!(vk, 0x13 | 0x91 | 0x2C)
}

fn vk_of(token: &str) -> Option<u32> {
    // The page names keys from `KeyboardEvent.code`, so letters and digits arrive as their own glyph;
    // the Win32 virtual-key codes for those are the ASCII values.
    let upper = token.to_ascii_uppercase();
    let mut chars = upper.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    if let Some(number) = upper.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&number) {
            return Some(0x70 + number - 1);
        }
    }
    Some(match upper.as_str() {
        "SPACE" => 0x20,
        "ENTER" | "RETURN" => 0x0D,
        "TAB" => 0x09,
        "BACKSPACE" => 0x08,
        "INSERT" => 0x2D,
        "DELETE" | "DEL" => 0x2E,
        "HOME" => 0x24,
        "END" => 0x23,
        "PAGEUP" | "PGUP" => 0x21,
        "PAGEDOWN" | "PGDN" => 0x22,
        "UP" => 0x26,
        "DOWN" => 0x28,
        "LEFT" => 0x25,
        "RIGHT" => 0x27,
        "PAUSE" => 0x13,
        "SCROLLLOCK" => 0x91,
        "PRINTSCREEN" => 0x2C,
        _ => return None,
    })
}

fn key_label(vk: u32) -> String {
    match vk {
        0x41..=0x5A | 0x30..=0x39 => char::from_u32(vk)
            .map(|c| c.to_string())
            .unwrap_or_else(|| format!("0x{vk:02X}")),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x20 => "Space".to_string(),
        0x0D => "Enter".to_string(),
        0x09 => "Tab".to_string(),
        0x08 => "Backspace".to_string(),
        0x2D => "Insert".to_string(),
        0x2E => "Delete".to_string(),
        0x24 => "Home".to_string(),
        0x23 => "End".to_string(),
        0x21 => "PageUp".to_string(),
        0x22 => "PageDown".to_string(),
        0x26 => "Up".to_string(),
        0x28 => "Down".to_string(),
        0x25 => "Left".to_string(),
        0x27 => "Right".to_string(),
        0x13 => "Pause".to_string(),
        0x91 => "ScrollLock".to_string(),
        0x2C => "PrintScreen".to_string(),
        _ => format!("0x{vk:02X}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_chord_round_trips_as_written() {
        let binding = parse(DEFAULT).unwrap().unwrap();
        assert_eq!(binding.mods, CONTROL | ALT);
        assert_eq!(binding.vk, 'M' as u32);
        assert_eq!(binding.label(), DEFAULT);
    }

    #[test]
    fn the_label_is_the_canonical_spelling_of_any_accepted_input() {
        let binding = parse(" ctrl + shift + f8 ").unwrap().unwrap();
        assert_eq!(binding.mods, CONTROL | SHIFT);
        assert_eq!(binding.label(), "Ctrl+Shift+F8");
        assert_eq!(
            parse("win+pageup").unwrap().unwrap().label(),
            "Win+PageUp"
        );
        assert_eq!(parse("Alt+Up").unwrap().unwrap().label(), "Alt+Up");
        assert_eq!(parse("F24").unwrap().unwrap().label(), "F24");
        assert_eq!(parse("Pause").unwrap().unwrap().label(), "Pause");
    }

    #[test]
    fn a_bare_typing_key_is_refused_but_a_bare_function_key_is_not() {
        for text in ["M", "7", "Space", "Enter", "Up"] {
            assert!(parse(text).is_err(), "{text} would steal a working key");
        }
        assert!(parse("F8").is_ok());
    }

    #[test]
    fn no_shortcut_is_an_empty_string_not_an_error() {
        assert_eq!(parse("").unwrap(), None);
        assert_eq!(parse("   ").unwrap(), None);
    }

    #[test]
    fn malformed_and_reserved_chords_are_refused_with_a_reason() {
        for text in ["Ctrl+Alt", "Ctrl+Alt+K+J", "Ctrl+Alt+Banana", "Ctrl++M", "F12", "Ctrl+Alt+F12"] {
            let error = parse(text).expect_err(text);
            assert!(!error.is_empty(), "{text} should say why");
        }
    }
}
