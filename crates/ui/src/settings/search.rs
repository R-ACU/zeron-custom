//! The settings sidebar's search box: a query narrows the section rows to
//! the ones whose label or keywords match, so "keep awake" finds
//! General and "api key" finds Accounts without knowing which page a
//! setting lives on. Pure ranking here; the box itself is rendered by the
//! shell's settings nav.

use crate::popover;
use crate::shell::SettingsSection;

/// Things a user might type when looking for a page: the page's own settings
/// and the words its cards use. Lowercase; matched as substrings.
pub fn keywords(section: SettingsSection) -> &'static [&'static str] {
    match section {
        SettingsSection::General => &[
            "general", "keep awake", "sleep", "lid", "power", "awake", "laptop", "startup",
            "login", "autostart", "sign in", "boot", "background", "engine", "launch",
        ],
        SettingsSection::Devices => &["device", "computer", "sync", "edge", "this device"],
        SettingsSection::Harnesses => &[
            "agent", "harness", "claude", "codex", "cursor", "pi", "opencode", "kimi", "grok",
            "hermes", "devin", "enable", "disable", "model",
        ],
        SettingsSection::Skills => &[
            "skill", "skills", "playbook", "skill.md", "agent skills", "folder", "enable",
            "disable",
        ],
        SettingsSection::Agents => &[
            "account", "login", "logout", "api key", "token", "usage", "anthropic", "openai",
            "openrouter", "deepseek", "provider", "billing",
        ],
        SettingsSection::Appearance => &[
            "theme", "appearance", "dark", "light", "glass", "frosted", "opaque", "font",
            "color", "colour", "wallpaper", "background", "transparency",
        ],
        SettingsSection::Files => &["file", "diff", "preview", "editor", "hidden", "ignore"],
        SettingsSection::Notifications => &[
            "notification", "sound", "toast", "alert", "banner", "attention", "completion",
        ],
        SettingsSection::Shortcuts => &["shortcut", "keyboard", "hotkey", "key", "binding", "chord"],
        SettingsSection::Appshots => &["appshot", "screenshot", "capture", "window", "snapshot"],
        SettingsSection::Archived => &["archive", "archived", "old", "history", "restore", "session"],
    }
}

/// The sections `query` matches, best first; every section (in nav order)
/// on an empty query. Ranking: label prefix, label substring, then keyword
/// hits — ties keep nav order so the list never reshuffles under the cursor.
pub fn matching_sections<'a>(
    query: &str,
    sections: impl IntoIterator<Item = &'a SettingsSection>,
) -> Vec<SettingsSection> {
    let query = query.trim();
    let mut ranked: Vec<(usize, usize, SettingsSection)> = sections
        .into_iter()
        .copied()
        .enumerate()
        .filter_map(|(ix, section)| {
            if query.is_empty() {
                return Some((0, ix, section));
            }
            let by_label = popover::match_rank(query, section.label());
            let by_keyword = keywords(section)
                .iter()
                .filter_map(|word| popover::match_rank(query, word))
                .min()
                .map(|rank| rank + 2);
            // A page whose settings match is a hit too (ranked with the
            // keyword hits, after label hits), so the nav shows every page
            // the results view has a card for.
            let by_entry = entries()
                .iter()
                .filter(|entry| entry.section == section)
                .filter_map(|entry| entry_rank(query, entry))
                .min()
                .map(|rank| rank + 2);
            by_label
                .into_iter()
                .chain(by_keyword)
                .chain(by_entry)
                .min()
                .map(|rank| (rank, ix, section))
        })
        .collect();
    ranked.sort_by_key(|(rank, ix, _)| (*rank, *ix));
    ranked.into_iter().map(|(_, _, section)| section).collect()
}

/// One user-facing setting row, as the results view shows it while the user
/// types: the page it lives on plus the title and description that page
/// renders, and the words someone might type for it instead.
#[derive(Debug)]
pub struct SettingEntry {
    pub section: SettingsSection,
    pub title: &'static str,
    pub description: &'static str,
    pub keywords: &'static [&'static str],
    /// What the result row offers besides opening the page.
    pub control: Control,
}

/// How a hit is operated straight from the results view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Nothing to flip inline (pickers, shortcuts, accounts, OS
    /// registrations): the row only opens its page.
    OpenPage,
    /// A plain on/off ui-setting: the row renders its live switch.
    Toggle(BoolSetting),
}

/// The boolean ui-settings a result row can flip in place. One variant per
/// switch on a settings page; the shell maps them to the field and to the same
/// side effects the page's event handler applies
/// ([`crate::shell::Shell::set_bool_setting`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolSetting {
    KeepAwakeWhileRunning,
    FilesAutosave,
    FilesWordWrap,
    FilesShowAll,
    SessionSounds,
    TaskCompletedSound,
    InputRequiredSound,
    ErrorSound,
    DesktopNotifications,
    NotificationsBackgroundOnly,
    EscapeStopsActiveAgent,
    AppshotsEnabled,
    AppshotSound,
}

impl BoolSetting {
    /// A stable motion key for this setting's switch — keyed by the variant,
    /// never by the row's position: the results list is rebuilt (and reordered)
    /// on every keystroke, and a flip makes the shell recreate the outlet, so
    /// an index-based key would restart or lose the glide
    /// ([`crate::settings::widgets::switch_progress`]).
    pub const fn motion_key(self) -> &'static str {
        match self {
            BoolSetting::KeepAwakeWhileRunning => "settings-switch-keep-awake",
            BoolSetting::FilesAutosave => "settings-switch-files-autosave",
            BoolSetting::FilesWordWrap => "settings-switch-files-word-wrap",
            BoolSetting::FilesShowAll => "settings-switch-files-show-all",
            BoolSetting::SessionSounds => "settings-switch-session-sounds",
            BoolSetting::TaskCompletedSound => "settings-switch-task-completed-sound",
            BoolSetting::InputRequiredSound => "settings-switch-input-required-sound",
            BoolSetting::ErrorSound => "settings-switch-error-sound",
            BoolSetting::DesktopNotifications => "settings-switch-desktop-notifications",
            BoolSetting::NotificationsBackgroundOnly => "settings-switch-notifications-background",
            BoolSetting::EscapeStopsActiveAgent => "settings-switch-escape-stops-agent",
            BoolSetting::AppshotsEnabled => "settings-switch-appshots-enabled",
            BoolSetting::AppshotSound => "settings-switch-appshot-sound",
        }
    }
}

macro_rules! entry {
    ($section:ident, $title:expr, $description:expr, [$($keyword:expr),* $(,)?]) => {
        entry!($section, $title, $description, [$($keyword),*], Control::OpenPage)
    };
    ($section:ident, $title:expr, $description:expr, [$($keyword:expr),* $(,)?], $control:expr) => {
        SettingEntry {
            section: SettingsSection::$section,
            title: $title,
            description: $description,
            keywords: &[$($keyword),*],
            control: $control,
        }
    };
}

/// Every setting row of every page, in nav order and then page order. Titles
/// and descriptions are the strings the pages render (rows whose copy is
/// computed at runtime carry a one-line summary instead). A new setting needs
/// a line here, or the search will not find it.
pub fn entries() -> &'static [SettingEntry] {
    static ENTRIES: &[SettingEntry] = &[
        // General
        entry!(
            General,
            "Open Zeron at login",
            "Opens the Zeron window when you sign in.",
            ["startup", "login", "autostart", "sign in", "boot", "launch", "start"]
        ),
        entry!(
            General,
            "Run engine in background at login",
            "Starts the engine hidden when you sign in, so automations keep running after the \
             window is closed.",
            ["startup", "login", "autostart", "engine", "background", "daemon", "automations"]
        ),
        entry!(
            General,
            "Keep laptop awake while agents are running",
            "Prevents sleep and lid-close sleep while an agent is working.",
            ["keep awake", "sleep", "lid", "power", "awake", "laptop", "standby"],
            Control::Toggle(BoolSetting::KeepAwakeWhileRunning)
        ),
        // Devices
        entry!(
            Devices,
            "Device names",
            "Rename this device and inspect the metadata of synced devices.",
            ["device", "rename", "computer", "sync", "edge", "this device"]
        ),
        // Agents (harnesses)
        entry!(
            Harnesses,
            "Enabled agents",
            "Choose which coding agents the composer offers. The setting is per device.",
            [
                "agent", "harness", "claude", "codex", "cursor", "pi", "opencode", "kimi", "grok",
                "hermes", "devin", "enable", "disable", "cli",
            ]
        ),
        entry!(
            Harnesses,
            "Session titles",
            "Choose the agent and model for automatic titles on this device.",
            ["title", "session title", "automatic", "name", "rename"]
        ),
        entry!(
            Harnesses,
            "Title harness",
            "The agent that generates session titles on this device.",
            ["title", "harness", "agent"]
        ),
        entry!(
            Harnesses,
            "Title model",
            "The model that generates session titles on this device.",
            ["title", "model"]
        ),
        // Skills
        entry!(
            Skills,
            "Skills",
            "Turn installed skills on or off per agent.",
            ["skill", "skills", "playbook", "skill.md", "agent", "folder", "enable", "disable"]
        ),
        // Accounts
        entry!(
            Agents,
            "Claude Code",
            "Claude Code logins on this device: add accounts, watch usage, and switch between them.",
            ["account", "login", "logout", "claude", "anthropic", "usage", "switch", "sign in"]
        ),
        entry!(
            Agents,
            "Codex",
            "Codex logins on this device: add accounts, watch usage, and switch between them.",
            ["account", "login", "logout", "codex", "openai", "usage", "switch", "sign in"]
        ),
        entry!(
            Agents,
            "Cursor",
            "The Cursor login on this device.",
            ["account", "login", "logout", "cursor", "sign in"]
        ),
        entry!(
            Agents,
            "Kimi",
            "The Kimi Code login on this device.",
            ["account", "login", "logout", "kimi", "moonshot", "sign in"]
        ),
        entry!(
            Agents,
            "API keys",
            "The provider API keys Zeron passes to every agent it starts.",
            [
                "api key", "token", "secret", "anthropic", "openai", "openrouter", "deepseek",
                "groq", "xai", "moonshot", "google", "gemini", "mistral", "fireworks", "together",
                "ollama", "provider", "billing", "environment variable",
            ]
        ),
        entry!(
            Agents,
            "Add API key",
            "Store a provider key for the agents on this device.",
            ["api key", "token", "provider", "add", "paste"]
        ),
        // Appearance
        entry!(
            Appearance,
            "Appearance",
            "Follow the system, or pin Light or Dark.",
            ["theme", "appearance", "dark", "light", "system", "mode"]
        ),
        entry!(
            Appearance,
            "Light theme",
            "Used whenever this appearance is active.",
            ["theme", "light", "palette"]
        ),
        entry!(
            Appearance,
            "Dark theme",
            "Used whenever this appearance is active.",
            ["theme", "dark", "palette"]
        ),
        entry!(
            Appearance,
            "Theme library",
            "Import or link custom themes.",
            ["theme", "import", "custom", "library", "link"]
        ),
        entry!(
            Appearance,
            "Accent color",
            "Controls, glyphs, selections, code, and activity.",
            ["accent", "color", "colour", "tint", "highlight"]
        ),
        entry!(
            Appearance,
            "Glass",
            "Frosted or opaque window surface.",
            ["glass", "frosted", "opaque", "transparency", "transparent", "blur", "acrylic"]
        ),
        entry!(
            Appearance,
            "Glass strength",
            "How much of the desktop shows through frosted windows.",
            ["glass", "frosted", "transparency", "blur", "strength", "opacity"]
        ),
        entry!(
            Appearance,
            "New thread composer background",
            "An image behind the new-thread composer, softened automatically on frosted themes.",
            ["background", "wallpaper", "image", "composer", "picture"]
        ),
        entry!(
            Appearance,
            "Background effect",
            "How the composer background image is treated.",
            ["background", "effect", "wallpaper", "blur", "dim"]
        ),
        entry!(
            Appearance,
            "Interface font",
            "Used across the interface and conversations. Code, diffs, and terminal keep their \
             current fonts and sizes.",
            ["font", "typeface", "interface font", "text", "typography"]
        ),
        // Files
        entry!(
            Files,
            "Autosave",
            "Save edited workspace files to disk automatically.",
            ["file", "autosave", "save", "editor"],
            Control::Toggle(BoolSetting::FilesAutosave)
        ),
        entry!(
            Files,
            "Autosave delay",
            "Save files after editing has been idle for this long.",
            ["file", "autosave", "delay", "idle", "seconds"]
        ),
        entry!(
            Files,
            "Editor font size",
            "Set the text size in workspace file editors.",
            ["file", "editor", "font", "size", "text size"]
        ),
        entry!(
            Files,
            "Word wrap",
            "Wrap long lines in every workspace file.",
            ["file", "wrap", "word wrap", "lines", "editor"],
            Control::Toggle(BoolSetting::FilesWordWrap)
        ),
        entry!(
            Files,
            "Show all files",
            "Include hidden and ignored files in every file tree.",
            ["file", "hidden", "ignore", "ignored", "gitignore", "tree", "dotfiles"],
            Control::Toggle(BoolSetting::FilesShowAll)
        ),
        // Notifications
        entry!(
            Notifications,
            "Session sounds",
            "Allow sounds for the selected session events below.",
            ["notification", "sound", "audio", "chime", "mute", "volume"],
            Control::Toggle(BoolSetting::SessionSounds)
        ),
        entry!(
            Notifications,
            "Task completed",
            "Play a sound when an agent finishes a run.",
            ["sound", "completion", "done", "finished", "complete"],
            Control::Toggle(BoolSetting::TaskCompletedSound)
        ),
        entry!(
            Notifications,
            "Input required",
            "Play a sound when an agent needs your response.",
            ["sound", "input", "question", "attention", "response"],
            Control::Toggle(BoolSetting::InputRequiredSound)
        ),
        entry!(
            Notifications,
            "Errors and disconnections",
            "Play a sound when a run fails or the connection remains unavailable.",
            ["sound", "error", "attention", "disconnect", "failure", "offline"],
            Control::Toggle(BoolSetting::ErrorSound)
        ),
        entry!(
            Notifications,
            "Desktop notifications",
            "Show a system banner on the same events, so pings reach you while Zeron is in the \
             background.",
            ["notification", "toast", "banner", "alert", "desktop", "system", "ping"],
            Control::Toggle(BoolSetting::DesktopNotifications)
        ),
        entry!(
            Notifications,
            "Only when in the background",
            "Skip the banner while a Zeron window is focused.",
            ["notification", "banner", "background", "focused", "foreground"],
            Control::Toggle(BoolSetting::NotificationsBackgroundOnly)
        ),
        // Shortcuts
        entry!(
            Shortcuts,
            "Stop active agent with Escape",
            "When no dialog, menu, picker, or terminal handles Escape, stop the agent in the \
             active session.",
            ["escape", "stop", "cancel", "interrupt", "abort", "keyboard"],
            Control::Toggle(BoolSetting::EscapeStopsActiveAgent)
        ),
        entry!(
            Shortcuts,
            "Send messages with",
            "Choose whether Enter sends immediately or starts a new paragraph. Cmd/Ctrl+Enter \
             always submits; with an empty composer it sends the most recently queued message. \
             Shift+Enter always inserts a line break.",
            ["enter", "send", "submit", "composer", "newline", "line break", "paragraph"]
        ),
        entry!(
            Shortcuts,
            "Save file",
            "Save the active workspace file.",
            ["shortcut", "keyboard", "hotkey", "binding", "save", "file"]
        ),
        entry!(
            Shortcuts,
            "Reload browser page",
            "Reload the focused browser tab.",
            ["shortcut", "keyboard", "hotkey", "binding", "browser", "reload", "refresh"]
        ),
        entry!(
            Shortcuts,
            "Toggle left sidebar",
            "Show or hide sessions and settings navigation.",
            ["shortcut", "keyboard", "hotkey", "binding", "sidebar", "panel", "navigation"]
        ),
        entry!(
            Shortcuts,
            "Toggle right sidebar",
            "Show or hide the right sidebar for the current session.",
            ["shortcut", "keyboard", "hotkey", "binding", "sidebar", "panel", "changes", "diff"]
        ),
        entry!(
            Shortcuts,
            "Toggle terminal",
            "Show or hide the terminal for the current session.",
            ["shortcut", "keyboard", "hotkey", "binding", "terminal", "panel", "console"]
        ),
        entry!(
            Shortcuts,
            "New session",
            "Open a blank session canvas to start a new session.",
            ["shortcut", "keyboard", "hotkey", "binding", "session", "new chat"]
        ),
        entry!(
            Shortcuts,
            "Next session",
            "Select the next session in the sidebar, wrapping at the end.",
            ["shortcut", "keyboard", "hotkey", "binding", "session", "next", "switch"]
        ),
        entry!(
            Shortcuts,
            "Previous session",
            "Select the previous session in the sidebar, wrapping at the start.",
            ["shortcut", "keyboard", "hotkey", "binding", "session", "previous", "switch"]
        ),
        entry!(
            Shortcuts,
            "Archive session",
            "Move the current session to the archived shelf.",
            ["shortcut", "keyboard", "hotkey", "binding", "session", "archive"]
        ),
        entry!(
            Shortcuts,
            "Jump to session",
            "Open the session at this place in the sidebar list.",
            ["shortcut", "keyboard", "hotkey", "binding", "session", "jump", "number"]
        ),
        // Appshots
        entry!(
            Appshots,
            "Capture Appshots",
            "Captures are staged for review and never sent automatically.",
            ["appshot", "screenshot", "capture", "snapshot", "window"],
            Control::Toggle(BoolSetting::AppshotsEnabled)
        ),
        entry!(
            Appshots,
            "Capture sound",
            "Play a sound when an Appshot is ready.",
            ["appshot", "sound", "capture", "chime"],
            Control::Toggle(BoolSetting::AppshotSound)
        ),
        entry!(
            Appshots,
            "Global shortcut",
            "The key combination that captures the focused application from anywhere on your \
             desktop.",
            ["appshot", "shortcut", "hotkey", "capture", "keyboard", "binding"]
        ),
        entry!(
            Appshots,
            "Destination",
            "Where captured Appshots are staged.",
            ["appshot", "destination", "folder", "save", "location"]
        ),
        entry!(
            Appshots,
            "Window capture",
            "Permission to capture the focused application window.",
            ["appshot", "window", "capture", "permission", "screen recording"]
        ),
        entry!(
            Appshots,
            "Application text",
            "Permission to read the focused application's text.",
            ["appshot", "text", "accessibility", "permission", "semantic"]
        ),
        // Archived
        entry!(
            Archived,
            "Archived sessions",
            "Hidden from the sidebar, never deleted. Unarchiving puts a session back on its \
             device.",
            ["archive", "archived", "old", "history", "restore", "unarchive", "session"]
        ),
    ];
    ENTRIES
}

/// How well `query` fits one entry: title prefix 0, title substring 1,
/// keyword hits from 2, description-word hits from 4. `None` when nothing
/// fits; an empty query never fits (the results view only exists for one).
fn entry_rank(query: &str, entry: &SettingEntry) -> Option<usize> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let by_title = popover::match_rank(query, entry.title);
    let by_keyword = entry
        .keywords
        .iter()
        .filter_map(|word| popover::match_rank(query, word))
        .min()
        .map(|rank| rank + 2);
    let by_description = entry
        .description
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|word| !word.is_empty())
        .filter_map(|word| popover::match_rank(query, word))
        .min()
        .map(|rank| rank + 4);
    by_title.into_iter().chain(by_keyword).chain(by_description).min()
}

/// The settings `query` matches, best first; ties keep index order so the
/// results never reshuffle under the cursor. Empty on an empty query.
pub fn matching_entries(query: &str) -> Vec<&'static SettingEntry> {
    let mut ranked: Vec<(usize, usize, &'static SettingEntry)> = entries()
        .iter()
        .enumerate()
        .filter_map(|(ix, entry)| entry_rank(query, entry).map(|rank| (rank, ix, entry)))
        .collect();
    ranked.sort_by_key(|(rank, ix, _)| (*rank, *ix));
    ranked.into_iter().map(|(_, _, entry)| entry).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lap_finds_the_keep_awake_setting_on_general_and_not_devices() {
        let hits = matching_entries("lap");
        let first = hits.first().expect("keep awake matches 'lap'");
        assert_eq!(first.title, "Keep laptop awake while agents are running");
        assert_eq!(first.section, SettingsSection::General);
        let sections = matching_sections("lap", SettingsSection::ALL.iter());
        assert!(sections.contains(&SettingsSection::General));
        assert!(!sections.contains(&SettingsSection::Devices));
    }

    #[test]
    fn sound_finds_the_notification_sounds() {
        let hits = matching_entries("sound");
        assert_eq!(hits[0].section, SettingsSection::Notifications);
        assert_eq!(hits[0].title, "Session sounds");
        for title in ["Task completed", "Input required", "Errors and disconnections"] {
            assert!(
                hits.iter()
                    .any(|e| e.section == SettingsSection::Notifications && e.title == title),
                "{title} should match 'sound'"
            );
        }
        assert!(matching_sections("sound", SettingsSection::ALL.iter())
            .contains(&SettingsSection::Notifications));
    }

    #[test]
    fn an_empty_query_lists_no_entries() {
        assert!(matching_entries("").is_empty());
        assert!(matching_entries("   ").is_empty());
    }

    #[test]
    fn title_hits_lead_and_ties_keep_index_order() {
        // "theme" is a title prefix of "Theme library" and a substring of
        // "Light theme"/"Dark theme"; the prefix hit leads, the substring
        // hits follow in page order.
        let hits = matching_entries("theme");
        assert_eq!(hits[0].title, "Theme library");
        let light = hits.iter().position(|e| e.title == "Light theme").unwrap();
        let dark = hits.iter().position(|e| e.title == "Dark theme").unwrap();
        assert!(light < dark);
    }

    #[test]
    fn every_entry_has_copy_and_a_page_in_the_nav() {
        for entry in entries() {
            assert!(!entry.title.is_empty());
            assert!(!entry.description.is_empty());
            assert!(SettingsSection::ALL.contains(&entry.section), "{}", entry.title);
        }
    }

    #[test]
    fn plain_switches_carry_their_toggle_and_pickers_do_not() {
        let hits = matching_entries("awake");
        let keep_awake = hits
            .iter()
            .find(|e| e.title == "Keep laptop awake while agents are running")
            .expect("keep awake matches 'awake'");
        assert_eq!(keep_awake.control, Control::Toggle(BoolSetting::KeepAwakeWhileRunning));
        // The login registrations are OS state, not ui-settings booleans, and
        // a theme picker is no switch at all.
        for title in ["Open Zeron at login", "Run engine in background at login", "Light theme"] {
            let entry = entries().iter().find(|e| e.title == title).expect(title);
            assert_eq!(entry.control, Control::OpenPage, "{title}");
        }
    }

    #[test]
    fn empty_query_keeps_every_section_in_nav_order() {
        let all = matching_sections("", SettingsSection::ALL.iter());
        assert_eq!(all, SettingsSection::ALL.to_vec());
        assert_eq!(matching_sections("   ", SettingsSection::ALL.iter()), all);
    }

    #[test]
    fn label_hits_outrank_keyword_hits() {
        // "app" is a label prefix of Appearance and Appshots, and a keyword
        // hit ("appshot") too — the label matches lead, in nav order.
        let hits = matching_sections("app", SettingsSection::ALL.iter());
        assert_eq!(&hits[..2], &[SettingsSection::Appearance, SettingsSection::Appshots]);
    }

    #[test]
    fn keywords_find_the_page_a_setting_lives_on() {
        assert_eq!(
            matching_sections("keep awake", SettingsSection::ALL.iter()),
            vec![SettingsSection::General]
        );
        // "laptop" belongs to the keep-awake switch alone, not to Devices.
        assert_eq!(
            matching_sections("lap", SettingsSection::ALL.iter()),
            vec![SettingsSection::General]
        );
        assert_eq!(
            matching_sections("API KEY", SettingsSection::ALL.iter()),
            vec![SettingsSection::Agents]
        );
        assert_eq!(
            matching_sections("theme", SettingsSection::ALL.iter()),
            vec![SettingsSection::Appearance]
        );
    }

    #[test]
    fn nothing_matches_gibberish() {
        assert!(matching_sections("qzxv", SettingsSection::ALL.iter()).is_empty());
    }

    #[test]
    fn a_filtered_nav_respects_the_offered_set() {
        // Appshots is hidden on non-desktop builds; a hidden section must
        // never be resurrected by the search.
        let offered: Vec<SettingsSection> = SettingsSection::ALL
            .into_iter()
            .filter(|s| *s != SettingsSection::Appshots)
            .collect();
        assert_eq!(
            matching_sections("screenshot", offered.iter()),
            Vec::<SettingsSection>::new()
        );
    }
}
