//! Starting with the session.
//!
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` is the mechanism a per-user app that installs
//! nothing should use: no service, no elevation, and it is the very list Windows shows under
//! "启动应用", so the user can see and undo it without opening MicDeck.

use std::path::Path;

use windows::core::{Error, HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// What the entry is called in the registry and in Windows' own startup list, so it is the product name.
pub const VALUE_NAME: &str = "MicDeck";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub enabled: bool,
    /// The command the entry runs; empty when there is no entry.
    pub command: String,
}

pub fn status() -> Result<Status, String> {
    read(VALUE_NAME)
}

pub fn set_enabled(exe: &Path, enabled: bool) -> Result<Status, String> {
    set_named(VALUE_NAME, exe, enabled)?;
    read(VALUE_NAME)
}

/// The value is a command line, so the path is quoted: a program under `C:\Program Files` would
/// otherwise be split at its first space when the shell runs it.
fn command_line(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

fn read(name: &str) -> Result<Status, String> {
    let subkey = HSTRING::from(RUN_KEY);
    let value = HSTRING::from(name);
    let mut buffer = vec![0u16; 1024];
    let mut bytes = (buffer.len() * 2) as u32;
    let error = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &subkey,
            &value,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if error == ERROR_FILE_NOT_FOUND {
        return Ok(Status {
            enabled: false,
            command: String::new(),
        });
    }
    if error != ERROR_SUCCESS {
        return Err(format!("read autostart: {}", Error::from(error)));
    }
    let units = (bytes as usize / 2).saturating_sub(1);
    Ok(Status {
        enabled: true,
        command: String::from_utf16_lossy(&buffer[..units]),
    })
}

fn set_named(name: &str, exe: &Path, enabled: bool) -> Result<(), String> {
    let mut key = HKEY::default();
    let subkey = HSTRING::from(RUN_KEY);
    let opened = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &subkey,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    if opened != ERROR_SUCCESS {
        return Err(format!("open autostart key: {}", Error::from(opened)));
    }
    let value = HSTRING::from(name);
    let result = if enabled {
        let text = command_line(exe);
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes: Vec<u8> = wide.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        let written = unsafe { RegSetValueExW(key, &value, None, REG_SZ, Some(&bytes)) };
        if written == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("write autostart: {}", Error::from(written)))
        }
    } else {
        let removed = unsafe { RegDeleteValueW(key, &value) };
        // Deleting what is not there is the state that was asked for.
        if removed == ERROR_SUCCESS || removed == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!("clear autostart: {}", Error::from(removed)))
        }
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_quoted_so_a_path_with_spaces_survives() {
        let command = command_line(Path::new(r"C:\Program Files\MicDeck\micdeck.exe"));
        assert_eq!(
            command,
            r#""C:\Program Files\MicDeck\micdeck.exe""#
        );
        assert!(command.starts_with('"') && command.ends_with('"'));
    }

    /// Round trip against the real key under a name of its own. The value is removed before anything is
    /// asserted, so a failure cannot leave the test binary in the user's startup list.
    #[test]
    fn a_value_can_be_written_read_and_cleared() {
        let name = format!("MicDeck-test-{}", std::process::id());
        let written = set_named(&name, Path::new(r"C:\tmp\micdeck.exe"), true);
        let seen = read(&name);
        let cleared = set_named(&name, Path::new(r"C:\tmp\micdeck.exe"), false);
        let after = read(&name);
        assert!(written.is_ok(), "write failed: {written:?}");
        assert!(cleared.is_ok(), "clear failed: {cleared:?}");
        assert_eq!(
            seen.map(|s| (s.enabled, s.command)),
            Ok((true, r#""C:\tmp\micdeck.exe""#.to_string()))
        );
        assert_eq!(after.map(|s| s.enabled), Ok(false));
    }

    /// The route adds exactly one thing to the module: it uses the running executable. That link is
    /// exercised against the real value name, but only when the entry does not exist — an entry this
    /// test did not write is never overwritten, and the test puts the empty state back.
    #[test]
    fn the_running_executable_round_trips_through_the_real_entry() {
        let original = read(VALUE_NAME).expect("read");
        if original.enabled {
            return;
        }
        let exe = std::env::current_exe().expect("current exe");
        let enabled = set_enabled(&exe, true);
        let seen = read(VALUE_NAME);
        let restored = set_named(VALUE_NAME, &exe, false);
        let after = read(VALUE_NAME);
        assert!(enabled.is_ok(), "enable failed: {enabled:?}");
        let seen = seen.expect("read back");
        assert!(seen.enabled);
        assert_eq!(seen.command, format!("\"{}\"", exe.display()));
        assert!(restored.is_ok(), "restore failed: {restored:?}");
        assert_eq!(after.map(|s| s.enabled), Ok(false));
    }
}
