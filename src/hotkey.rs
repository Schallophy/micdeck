//! The microphone's global shortcut, named in the settings file and owned by the tray window.
//!
//! The wire spelling is the human one (`Ctrl+Alt+M`), and `label` is the only spelling this program
//! writes back, so a hand-edited file and a recorded chord converge on the same text. A binding is
//! either a chord (`RegisterHotKey` owns it) or a lone side-specific modifier, which Windows cannot
//! express as a hot key at all and which the tray watches as a tap instead ([`crate::tap`]).

/// The first-run shortcut, and what a settings file written before this feature existed gets.
pub const DEFAULT: &str = "Ctrl+Alt+M";

// The four Win32 modifier bits. Named here so parsing and formatting need no window handle.
const ALT: u32 = 0x0001;
const CONTROL: u32 = 0x0002;
const SHIFT: u32 = 0x0004;
const WIN: u32 = 0x0008;

/// `VK_F12` is reserved for debuggers; a hot key on it can never be reliable.
const VK_F12: u32 = 0x7B;

/// A modifier key standing alone. Left and right are distinct keys to the hardware, and `RegisterHotKey`
/// neither distinguishes nor delivers them, so a tap binding keeps the side it was recorded with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TapKey {
    LCtrl,
    RCtrl,
    LAlt,
    RAlt,
    LShift,
    RShift,
    LWin,
    RWin,
}

impl TapKey {
    pub fn label(self) -> &'static str {
        match self {
            TapKey::LCtrl => "LCtrl",
            TapKey::RCtrl => "RCtrl",
            TapKey::LAlt => "LAlt",
            TapKey::RAlt => "RAlt",
            TapKey::LShift => "LShift",
            TapKey::RShift => "RShift",
            TapKey::LWin => "LWin",
            TapKey::RWin => "RWin",
        }
    }

    /// The lower-case token of a recorded tap, or of a hand-written one.
    pub fn from_token(token: &str) -> Option<TapKey> {
        Some(match token.to_ascii_lowercase().as_str() {
            "lctrl" | "lcontrol" => TapKey::LCtrl,
            "rctrl" | "rcontrol" => TapKey::RCtrl,
            "lalt" => TapKey::LAlt,
            "ralt" => TapKey::RAlt,
            "lshift" => TapKey::LShift,
            "rshift" => TapKey::RShift,
            "lwin" | "lwindows" => TapKey::LWin,
            "rwin" | "rwindows" => TapKey::RWin,
            _ => return None,
        })
    }

    /// The AT scan code and the extended-key bit, which is how raw input tells the two sides apart.
    pub fn scan(self) -> u16 {
        match self {
            TapKey::LCtrl | TapKey::RCtrl => 0x1D,
            TapKey::LAlt | TapKey::RAlt => 0x38,
            TapKey::LShift => 0x2A,
            TapKey::RShift => 0x36,
            TapKey::LWin | TapKey::RWin => 0x5B,
        }
    }

    pub fn extended(self) -> bool {
        matches!(self, TapKey::RCtrl | TapKey::RAlt | TapKey::LWin | TapKey::RWin)
    }

    /// The virtual keys that report this key's state, so "is something else already held" does not
    /// mistake the tap key for a chord partner. RWin has no generic form.
    pub fn held_by(self) -> [i32; 2] {
        match self {
            TapKey::LCtrl => [0xA2, 0x11],
            TapKey::RCtrl => [0xA3, 0x11],
            TapKey::LAlt => [0xA4, 0x12],
            TapKey::RAlt => [0xA5, 0x12],
            TapKey::LShift => [0xA0, 0x10],
            TapKey::RShift => [0xA1, 0x10],
            TapKey::LWin => [0x5B, 0],
            TapKey::RWin => [0x5C, 0],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Binding {
    Chord {
        mods: u32,
        vk: u32,
    },
    Tap(TapKey),
}

impl Binding {
    /// Modifiers in Ctrl, Alt, Shift, Win order, then the key.
    pub fn label(self) -> String {
        match self {
            Binding::Tap(key) => key.label().to_string(),
            Binding::Chord { mods, vk } => {
                let mut parts: Vec<&str> = Vec::new();
                for (bit, name) in [(CONTROL, "Ctrl"), (ALT, "Alt"), (SHIFT, "Shift"), (WIN, "Win")] {
                    if mods & bit != 0 {
                        parts.push(name);
                    }
                }
                let key = key_label(vk);
                parts.push(&key);
                parts.join("+")
            }
        }
    }
}

/// Parses what the page sends and what the store holds. The empty string is "no shortcut", not an
/// error: that is how the page clears one.
pub fn parse(text: &str) -> Result<Option<Binding>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    // A lone token may be a side-specific modifier, which is a tap binding, not a chord.
    if !text.contains('+') {
        if let Some(key) = TapKey::from_token(text) {
            return Ok(Some(Binding::Tap(key)));
        }
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
                // Win32 chords cannot tell the two sides of a modifier apart, so a side-specific
                // spelling is only meaningful standing alone.
                if TapKey::from_token(token).is_some() {
                    return Err("左右 Ctrl/Alt/Shift/Win 不能用在组合键里，只能单独使用".to_string());
                }
                if key.is_some() {
                    return Err(format!("“{text}”只能有一个主键"));
                }
                key = Some(vk_of(token).ok_or_else(|| format!("不支持的按键：{token}"))?);
            }
        }
    }
    let vk = key.ok_or_else(|| {
        "快捷键需要一个主键；要单独使用修饰键，请写明左右，如 RCtrl"
            .to_string()
    })?;
    if vk == VK_F12 {
        return Err("F12 保留给调试器".to_string());
    }
    // A global hot key consumes the key in every program. Doing that to a letter or to Space would
    // break typing everywhere, which cannot be what a chord recorder meant; only keys that neither
    // type nor navigate may stand alone.
    if mods == 0 && !is_standalone_key(vk) {
        return Err("除了 F1–F24 等功能键，快捷键至少要有一个修饰键".to_string());
    }
    Ok(Some(Binding::Chord { mods, vk }))
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
        assert_eq!(binding, Binding::Chord { mods: CONTROL | ALT, vk: 'M' as u32 });
        assert_eq!(binding.label(), DEFAULT);
    }

    #[test]
    fn the_label_is_the_canonical_spelling_of_any_accepted_input() {
        let binding = parse(" ctrl + shift + f8 ").unwrap().unwrap();
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
    fn a_lone_modifier_is_a_tap_binding_and_keeps_its_side() {
        assert_eq!(parse("RCtrl").unwrap(), Some(Binding::Tap(TapKey::RCtrl)));
        assert_eq!(parse(" rctrl ").unwrap(), Some(Binding::Tap(TapKey::RCtrl)));
        assert_eq!(parse("LShift").unwrap(), Some(Binding::Tap(TapKey::LShift)));
        assert_eq!(parse("RWin").unwrap(), Some(Binding::Tap(TapKey::RWin)));
        assert_eq!(parse("ralt").unwrap().unwrap().label(), "RAlt");
        // The two sides are distinct keys and stay distinct through a round trip.
        assert_ne!(parse("LCtrl").unwrap(), parse("RCtrl").unwrap());
    }

    #[test]
    fn side_specific_modifiers_cannot_join_a_chord() {
        let error = parse("RCtrl+Alt+M").expect_err("chords cannot tell sides apart");
        assert!(error.contains("单独"), "{error}");
        assert!(parse("LCtrl+Shift").is_err());
    }

    #[test]
    fn a_sideless_modifier_alone_asks_for_a_side() {
        let error = parse("Ctrl").expect_err("Ctrl alone is ambiguous");
        assert!(error.contains("RCtrl"), "{error}");
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
