//! `%APPDATA%\MicDeck\settings.json`. Wire format is camelCase, a value that did not change is not
//! written, and the legacy `lineRoutes` list folds into `sources` on read.
//!
//! Nothing is picked for the user; a remembered source is its executable path, not a pid.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::status::Identity;
use crate::log;

fn one() -> f32 {
    1.0
}

fn default_hotkey() -> String {
    crate::hotkey::DEFAULT.to_string()
}

const GAIN_EPSILON: f32 = 0.0001;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineSetting {
    pub id: u32,
    pub device_id: String,
    /// Optional remark the user typed for this line.
    #[serde(default)]
    pub label: String,
}

/// A remembered route from the old schema, kept only so an existing settings file migrates.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteSetting {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineRoutesSetting {
    pub line_id: u32,
    #[serde(default)]
    pub routes: Vec<RouteSetting>,
}

/// A source the user added, with the lines it feeds.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSetting {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub title: String,
    #[serde(default = "one")]
    pub gain: f32,
    #[serde(default)]
    pub lines: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub mic_id: String,
    #[serde(default = "one")]
    pub mic_gain: f32,
    #[serde(default)]
    pub mic_muted: bool,
    /// The global mute shortcut in the canonical spelling; empty disables it. A file written before
    /// this feature existed gets the chord the product has always advertised.
    #[serde(default = "default_hotkey")]
    pub hotkey: String,
    #[serde(default)]
    pub lines: Vec<LineSetting>,
    #[serde(default)]
    pub mic_routes: Vec<u32>,
    /// The output the monitor plays through; empty means the user has not picked one yet.
    #[serde(default)]
    pub monitor_device_id: String,
    #[serde(default = "one")]
    pub monitor_gain: f32,
    #[serde(default)]
    pub sources: Vec<SourceSetting>,
    /// Legacy: read once, folded into `sources`, then dropped from the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_routes: Option<Vec<LineRoutesSetting>>,
    /// Where `save` writes. Skipped on the wire. Tests point it at a temporary file, because the real
    /// store is the user's running configuration.
    #[serde(skip, default)]
    pub store: PathBuf,
}

impl Default for Settings {
    /// A first-run store is "no microphone chosen, and if one is chosen later it is at unity" — the gain
    /// is not zero, because a defaulted zero gain would look like a muted device in the UI.
    fn default() -> Self {
        Self {
            mic_id: String::new(),
            mic_gain: 1.0,
            mic_muted: false,
            hotkey: default_hotkey(),
            lines: Vec::new(),
            mic_routes: Vec::new(),
            monitor_device_id: String::new(),
            monitor_gain: 1.0,
            sources: Vec::new(),
            line_routes: None,
            store: PathBuf::new(),
        }
    }
}

impl Settings {
    /// The file this product keeps its state in.
    pub fn load() -> Settings {
        Self::load_at(&store_path())
    }

    pub fn load_at(path: &Path) -> Settings {
        // The path is the store even when the read fails, so a later save still targets it.
        let mut loaded = match std::fs::read_to_string(path) {
            // First run, or an unreadable file: fall back to "no microphone, no lines".
            Err(_) => Settings::default(),
            Ok(text) => match serde_json::from_str(&text) {
                Ok(settings) => settings,
                Err(_) => Settings::default(),
            },
        };
        loaded.store = path.to_path_buf();
        loaded.migrate_legacy_routes();
        loaded
    }

    /// The previous file stored routes per line; fold that into the source list so an upgrade keeps what
    /// the user had.
    fn migrate_legacy_routes(&mut self) {
        let legacy = match self.line_routes.take() {
            Some(entries) => entries,
            None => return,
        };
        for entry in legacy {
            for route in entry.routes {
                if route.name.trim().is_empty() {
                    continue;
                }
                let found = self.sources.iter().position(|source| {
                    source.name.eq_ignore_ascii_case(&route.name)
                        && source.path.eq_ignore_ascii_case(&route.path)
                });
                let index = match found {
                    Some(index) => index,
                    None => {
                        self.sources.push(SourceSetting {
                            name: route.name.clone(),
                            path: route.path.clone(),
                            title: route.name.clone(),
                            gain: 1.0,
                            lines: Vec::new(),
                        });
                        self.sources.len() - 1
                    }
                };
                if !self.sources[index].lines.contains(&entry.line_id) {
                    self.sources[index].lines.push(entry.line_id);
                }
            }
        }
    }

    pub fn set_mic(&mut self, device_id: &str, gain: f32) {
        if self.mic_id == device_id && (self.mic_gain - gain).abs() < GAIN_EPSILON {
            return;
        }
        self.mic_id = device_id.to_string();
        self.mic_gain = gain;
        self.save();
    }

    pub fn set_mic_muted(&mut self, muted: bool) {
        if self.mic_muted == muted {
            return;
        }
        self.mic_muted = muted;
        self.save();
    }

    pub fn set_hotkey(&mut self, hotkey: &str) {
        if self.hotkey == hotkey {
            return;
        }
        self.hotkey = hotkey.to_string();
        self.save();
    }

    pub fn set_lines(&mut self, lines: &[(u32, &str, &str)]) {
        let next: Vec<LineSetting> = lines
            .iter()
            .map(|(id, device_id, label)| LineSetting {
                id: *id,
                device_id: device_id.to_string(),
                label: label.to_string(),
            })
            .collect();
        let same = next.len() == self.lines.len()
            && next
                .iter()
                .zip(self.lines.iter())
                .all(|(a, b)| a.id == b.id && a.device_id == b.device_id && a.label == b.label);
        if same {
            return;
        }
        self.lines = next;
        // A route to a line that no longer exists can never be re-established.
        let kept: Vec<u32> = self.lines.iter().map(|line| line.id).collect();
        self.mic_routes.retain(|id| kept.contains(id));
        for source in self.sources.iter_mut() {
            source.lines.retain(|id| kept.contains(id));
        }
        self.save();
    }

    pub fn set_monitor_output(&mut self, device_id: &str, gain: f32) {
        let gain = gain.clamp(0.0, crate::engine::MAX_GAIN);
        if self.monitor_device_id == device_id && (self.monitor_gain - gain).abs() < f32::EPSILON {
            return;
        }
        self.monitor_device_id = device_id.to_string();
        self.monitor_gain = gain;
        self.save();
    }

    pub fn set_sources(&mut self, sources: &[(Identity, &str, f32, &[u32])]) {
        let mut next: Vec<SourceSetting> = sources
            .iter()
            .filter(|(identity, _title, _gain, _lines)| !identity.name.trim().is_empty())
            .map(|(identity, title, gain, lines)| {
                let mut lines = lines.to_vec();
                lines.sort_unstable();
                lines.dedup();
                SourceSetting {
                    name: identity.name.clone(),
                    path: identity.path.clone(),
                    title: title.to_string(),
                    gain: *gain,
                    lines,
                }
            })
            .collect();
        next.sort_by_key(|source| source.name.to_lowercase());
        let same = next.len() == self.sources.len()
            && next
                .iter()
                .zip(self.sources.iter())
                .all(|(a, b)| {
                    a.name == b.name
                        && a.path == b.path
                        && a.title == b.title
                        && (a.gain - b.gain).abs() < GAIN_EPSILON
                        && a.lines == b.lines
                });
        if same {
            return;
        }
        self.sources = next;
        self.save();
    }

    pub fn set_mic_routes(&mut self, line_ids: &[u32]) {
        let mut next = line_ids.to_vec();
        next.sort_unstable();
        next.dedup();
        if next == self.mic_routes {
            return;
        }
        self.mic_routes = next;
        self.save();
    }

    fn save(&self) {
        if self.store.as_os_str().is_empty() {
            log::line("settings save skipped: no store path");
            return;
        }
        if let Some(dir) = self.store.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(err) = std::fs::write(&self.store, text) {
                    log::line(&format!("settings save failed: {err}"));
                }
            }
            Err(err) => log::line(&format!("settings save failed: {err}")),
        }
    }
}

/// `%APPDATA%\MicDeck\settings.json`, resolved the way the shell resolves it: through the known folder,
/// not the `APPDATA` environment variable, which can point a test at a different file than the one the
/// user actually has.
pub fn store_path() -> PathBuf {
    app_data_dir().join("MicDeck").join("settings.json")
}

fn app_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        use windows::Win32::System::Com::CoTaskMemFree;
        use windows::Win32::UI::Shell::{
            SHGetKnownFolderPath, KNOWN_FOLDER_FLAG, FOLDERID_RoamingAppData,
        };
        match unsafe {
            SHGetKnownFolderPath(&FOLDERID_RoamingAppData, KNOWN_FOLDER_FLAG(0), None)
        } {
            Ok(pivot) => {
                // The buffer belongs to the shell's allocator: copy the address, read the text, then free.
                let raw = pivot.0;
                let base = unsafe { pivot.to_string() }.unwrap_or_default();
                unsafe {
                    let _ = CoTaskMemFree(Some(raw as *const core::ffi::c_void));
                }
                if !base.is_empty() {
                    return PathBuf::from(base);
                }
            }
            Err(_) => {}
        }
    }
    env_data_dir()
}

fn env_data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test gets its own file under the temp directory; the real store is never opened here.
    fn scratch(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "micdeck-settings-{}-{}.json",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Writes a sentinel so "nothing was written" is observable rather than asserted about internals.
    fn mark_unsaved(path: &Path) {
        std::fs::write(path, "{\"sentinel\":true}").unwrap();
    }

    #[test]
    fn a_round_trip_keeps_the_camel_case_wire_format() {
        let path = scratch("round-trip");
        let mut settings = Settings::load_at(&path);
        settings.store = path.clone();
        settings.set_mic("{0.0.1.00000000}.{mic}", 0.5);
        settings.set_mic_muted(true);
        settings.set_hotkey("Ctrl+Shift+F8");
        settings.set_lines(&[(
            1,
            "{0.0.0.00000000}.{out}",
            "客厅那对音箱",
        )]);
        settings.set_mic_routes(&[1]);
        let identity = Identity {
            name: "cloudmusic".to_string(),
            path: r"C:\Program Files\cloudmusic\cloudmusic.exe".to_string(),
        };
        settings.set_sources(&[(identity, "网易云音乐", 2.0, &[1])]);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"micId\""), "camelCase keys");
        assert!(text.contains("\"deviceId\""), "camelCase keys");
        assert!(text.contains("\"micMuted\": true"), "the switch is a stored fact");
        assert!(text.contains("\"hotkey\": \"Ctrl+Shift+F8\""), "{text}");
        assert!(text.contains("\"客厅那对音箱\""), "labels are UTF-8");
        assert!(!text.contains("lineRoutes"), "the legacy field is dropped on write");

        let reloaded = Settings::load_at(&path);
        assert_eq!(reloaded.mic_id, "{0.0.1.00000000}.{mic}");
        assert!(reloaded.mic_muted, "a muted microphone stays muted");
        assert_eq!(reloaded.hotkey, "Ctrl+Shift+F8");
        assert_eq!(reloaded.lines.len(), 1);
        assert_eq!(reloaded.lines[0].label, "客厅那对音箱");
        assert_eq!(reloaded.mic_routes, vec![1]);
        assert_eq!(reloaded.sources.len(), 1);
        assert_eq!(reloaded.sources[0].title, "网易云音乐");
        assert_eq!(reloaded.sources[0].gain, 2.0);
        assert_eq!(reloaded.sources[0].path, r"C:\Program Files\cloudmusic\cloudmusic.exe");
        assert_eq!(std::fs::remove_file(&path).is_ok(), true);
    }

    #[test]
    fn the_legacy_line_routes_fold_into_the_source_list() {
        let path = scratch("legacy");
        std::fs::write(
            &path,
            r#"{
              "micId": "{mic-id}",
              "micGain": 1.0,
              "lines": [ { "id": 2, "deviceId": "{out-id}", "label": "" } ],
              "micRoutes": [ 2 ],
              "lineRoutes": [
                { "lineId": 2, "routes": [ { "name": "Edge", "path": "C:\\msedge.exe" } ] },
                { "lineId": 3, "routes": [ { "name": "Edge", "path": "C:\\msedge.exe" } ] },
                { "lineId": 3, "routes": [ { "name": "  ", "path": "" } ] }
              ]
            }"#,
        )
        .unwrap();
        let settings = Settings::load_at(&path);
        assert_eq!(settings.sources.len(), 1, "one source, both lines folded in");
        assert_eq!(settings.sources[0].name, "Edge");
        assert_eq!(settings.sources[0].title, "Edge", "title falls back to the name");
        assert_eq!(settings.sources[0].lines, vec![2, 3]);
        assert!(settings.line_routes.is_none(), "legacy is consumed, not kept");
        assert_eq!(settings.mic_routes, vec![2]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_edit_that_changes_nothing_leaves_the_file_alone() {
        let path = scratch("no-write");
        let mut settings = Settings::load_at(&path);
        settings.store = path.clone();
        let identity = Identity {
            name: "edge".to_string(),
            path: String::new(),
        };
        settings.set_mic("{mic}", 1.0);
        settings.set_mic_muted(true);
        settings.set_hotkey("Ctrl+Shift+F8");
        settings.set_lines(&[(1, "{out}", "备注")]);
        settings.set_mic_routes(&[1]);
        settings.set_sources(&[(identity.clone(), "", 1.0, &[1])]);
        mark_unsaved(&path);

        // The same values again: every setter compares first, so a redragged slider or a re-sent
        // topology must not touch the disk.
        settings.set_mic("{mic}", 1.0);
        settings.set_mic_muted(true);
        settings.set_hotkey("Ctrl+Shift+F8");
        settings.set_lines(&[(1, "{out}", "备注")]);
        settings.set_mic_routes(&[1]);
        settings.set_sources(&[(identity.clone(), "", 1.0, &[1])]);
        settings.set_sources(&[(identity.clone(), "", 1.0, &[1])]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"sentinel\":true}",
            "no setter wrote"
        );

        // A real change does write.
        settings.set_mic("{mic}", 0.9);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("\"micId\""));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dropping_a_line_drops_the_routes_that_came_back_with_it() {
        let path = scratch("prune");
        let mut settings = Settings::load_at(&path);
        settings.store = path.clone();
        settings.set_lines(&[(1, "{a}", ""), (2, "{b}", "")]);
        let identity = Identity {
            name: "qq".to_string(),
            path: String::new(),
        };
        settings.set_sources(&[(identity, "QQ", 1.0, &[1, 2])]);
        settings.set_mic_routes(&[1, 2]);
        assert_eq!(settings.mic_routes, vec![1, 2]);
        assert_eq!(settings.sources[0].lines, vec![1, 2]);

        settings.set_lines(&[(2, "{b}", "")]);
        assert_eq!(settings.mic_routes, vec![2], "route to a dead line cannot be re-established");
        assert_eq!(settings.sources[0].lines, vec![2]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_we_cannot_parse_is_treated_as_first_run() {
        let path = scratch("corrupt");
        std::fs::write(&path, "{ not json").unwrap();
        let settings = Settings::load_at(&path);
        assert!(settings.mic_id.is_empty());
        assert!(settings.lines.is_empty());
        assert!(settings.sources.is_empty());
        assert_eq!(settings.mic_gain, 1.0, "a defaulted gain is 1.0, not 0");
        assert_eq!(
            settings.hotkey,
            crate::hotkey::DEFAULT,
            "an old store gets the shortcut the product advertises"
        );
        assert_eq!(
            settings.store, path,
            "an unreadable store still knows where it is"
        );

        // A partial file still loads: everything absent is a default, not an error.
        std::fs::write(&path, r#"{"micId":"{mic}"}"#).unwrap();
        let settings = Settings::load_at(&path);
        assert_eq!(settings.mic_id, "{mic}");
        assert_eq!(settings.mic_gain, 1.0);
        assert!(!settings.mic_muted, "an absent switch is an open microphone");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sources_are_stored_in_one_order_so_editing_is_quiet() {
        let path = scratch("order");
        let mut settings = Settings::load_at(&path);
        settings.store = path.clone();
        let one = Identity {
            name: "bApp".to_string(),
            path: String::new(),
        };
        let two = Identity {
            name: "AApp".to_string(),
            path: String::new(),
        };
        settings.set_sources(&[
            (one.clone(), "", 1.0, &[]),
            (two.clone(), "", 1.0, &[]),
        ]);
        assert_eq!(
            settings
                .sources
                .iter()
                .map(|source| source.name.clone())
                .collect::<Vec<_>>(),
            vec!["AApp", "bApp"],
            "name order so a reload cannot reshuffle"
        );
        // The same set handed over in the other order is not a change: the list is canonicalised
        // before it is compared, so a reload cannot reshuffle the file.
        mark_unsaved(&path);
        settings.set_sources(&[(two, "", 1.0, &[]), (one, "", 1.0, &[])]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"sentinel\":true}");
        let _ = std::fs::remove_file(&path);
    }
}
