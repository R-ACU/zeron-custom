//! Settings → Skills: the skill folders each installed agent loads, with a
//! switch per skill.
//!
//! A skill is a folder holding a `SKILL.md` (optional YAML frontmatter with
//! `name:` and `description:`). Every agent reads them from its own places —
//! one global directory under the home dir and, for most of them, one inside
//! the current project — so this page groups the rows per agent and, inside an
//! agent's card, under a `Global` and a `Project · <folder>` sub-header.
//!
//! Switching a skill OFF renames its `SKILL.md` to `SKILL.md.disabled`: the
//! agent stops loading it and nothing else about the folder changes, so
//! switching back on is the reverse rename. A folder that only has
//! `SKILL.md.disabled` therefore lists as off. All of the file IO (the scan,
//! the renames, the copies) runs on the background executor, like the
//! autostart work on [`crate::settings::general`], and every change re-scans
//! so the page shows what is really on disk.
//!
//! "Transfer skills" in the page header copies whole skill folders from one
//! agent to one or more others (same scope, existing folders skipped) — a
//! single dialog instead of a copy button per row.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Task, Window, div, prelude::*,
    px,
};

use zeron_engine::registry::HarnessDescriptor;
use zeron_proto::HarnessId;
use zeron_rpc::methods;

use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::{Theme, ink};

/// The file an agent loads, and the name it carries while switched off.
pub const SKILL_FILE: &str = "SKILL.md";
pub const SKILL_FILE_DISABLED: &str = "SKILL.md.disabled";

/// How long a description line may get before it is cut.
const DESCRIPTION_MAX: usize = 120;

/// Where a skill folder lives relative to the agent's world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillScope {
    /// Under the user's home dir — loaded in every session.
    Global,
    /// Inside the current project folder.
    Project,
}

impl SkillScope {
    pub fn label(self) -> &'static str {
        match self {
            SkillScope::Global => "Global",
            SkillScope::Project => "Project",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            SkillScope::Global => "global",
            SkillScope::Project => "project",
        }
    }
}

/// The agents that load skill folders. Deliberately a small closed set: the
/// other harnesses in the catalog have no documented skills directory, and
/// guessing one would show an empty section forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillAgent {
    ClaudeCode,
    Codex,
    Pi,
    Opencode,
}

impl SkillAgent {
    pub const ALL: [SkillAgent; 4] = [
        SkillAgent::ClaudeCode,
        SkillAgent::Codex,
        SkillAgent::Pi,
        SkillAgent::Opencode,
    ];

    pub fn harness(self) -> HarnessId {
        match self {
            SkillAgent::ClaudeCode => HarnessId::ClaudeCode,
            SkillAgent::Codex => HarnessId::Codex,
            SkillAgent::Pi => HarnessId::Pi,
            SkillAgent::Opencode => HarnessId::Opencode,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SkillAgent::ClaudeCode => "Claude Code",
            SkillAgent::Codex => "Codex",
            SkillAgent::Pi => "Pi",
            SkillAgent::Opencode => "opencode",
        }
    }

    /// A stable slug for motion keys and element ids.
    pub fn slug(self) -> &'static str {
        match self {
            SkillAgent::ClaudeCode => "claude",
            SkillAgent::Codex => "codex",
            SkillAgent::Pi => "pi",
            SkillAgent::Opencode => "opencode",
        }
    }

    /// The directory patterns, tilde-abbreviated, as the section header
    /// annotates them. Pure and independent of what exists on disk.
    pub fn dir_hints(self) -> &'static [&'static str] {
        match self {
            SkillAgent::ClaudeCode => &["~/.claude/skills", ".claude/skills"],
            // Codex reads its own dirs plus the harness-neutral `.agents`
            // tree in a project.
            SkillAgent::Codex => &["~/.codex/skills", ".codex/skills", ".agents/skills"],
            // Pi keeps agent state under `~/.pi/agent` (see
            // `crates/harness/src/pi_thinking.rs`, which reads
            // `~/.pi/agent/models.json` from the same place). Pi has no
            // documented per-project skills dir, so only the global one.
            SkillAgent::Pi => &["~/.pi/agent/skills"],
            SkillAgent::Opencode => &["~/.config/opencode/skills", ".opencode/skills"],
        }
    }
}

/// Every candidate skills directory for `agent`, given a home dir and the
/// current project folder. Pure: the caller decides what to do with a path
/// that does not exist (scan skips it, a copy creates it).
///
/// Claude's plugin skills (`~/.claude/plugins/...`) are deliberately left
/// out: they belong to an installed plugin, are not the user's to rename,
/// and their layout is not part of the skills contract.
pub fn skill_dirs(
    agent: SkillAgent,
    home: Option<&Path>,
    project: Option<&Path>,
) -> Vec<(SkillScope, PathBuf)> {
    let mut dirs = Vec::new();
    let global = |parts: &[&str]| -> Option<PathBuf> {
        let mut path = home?.to_path_buf();
        for part in parts {
            path.push(part);
        }
        Some(path)
    };
    let local = |parts: &[&str]| -> Option<PathBuf> {
        let mut path = project?.to_path_buf();
        for part in parts {
            path.push(part);
        }
        Some(path)
    };
    match agent {
        SkillAgent::ClaudeCode => {
            dirs.extend(global(&[".claude", "skills"]).map(|p| (SkillScope::Global, p)));
            dirs.extend(local(&[".claude", "skills"]).map(|p| (SkillScope::Project, p)));
        }
        SkillAgent::Codex => {
            dirs.extend(global(&[".codex", "skills"]).map(|p| (SkillScope::Global, p)));
            dirs.extend(local(&[".codex", "skills"]).map(|p| (SkillScope::Project, p)));
            dirs.extend(local(&[".agents", "skills"]).map(|p| (SkillScope::Project, p)));
        }
        SkillAgent::Pi => {
            dirs.extend(global(&[".pi", "agent", "skills"]).map(|p| (SkillScope::Global, p)));
        }
        SkillAgent::Opencode => {
            dirs.extend(
                global(&[".config", "opencode", "skills"]).map(|p| (SkillScope::Global, p)),
            );
            dirs.extend(local(&[".opencode", "skills"]).map(|p| (SkillScope::Project, p)));
        }
    }
    dirs
}

/// The directory a copy into `agent`/`scope` lands in: the first candidate of
/// that scope (`.codex/skills` before `.agents/skills`).
pub fn copy_target_dir(
    agent: SkillAgent,
    scope: SkillScope,
    home: Option<&Path>,
    project: Option<&Path>,
) -> Option<PathBuf> {
    skill_dirs(agent, home, project)
        .into_iter()
        .find(|(s, _)| *s == scope)
        .map(|(_, path)| path)
}

/// One skill folder as the page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The frontmatter `name`, or the folder name when there is none.
    pub title: String,
    /// The folder name — the identity used to compare across agents.
    pub folder: String,
    /// The frontmatter `description`, one line, truncated.
    pub description: Option<String>,
    pub scope: SkillScope,
    pub dir: PathBuf,
    /// `SKILL.md` present (off = only `SKILL.md.disabled`).
    pub enabled: bool,
}

impl Skill {
    /// The stable motion key of this row's switch.
    pub fn motion_key(&self, agent: SkillAgent) -> String {
        format!(
            "skill-{}-{}-{}",
            agent.slug(),
            self.scope.slug(),
            self.folder
        )
    }
}

/// `name` and `description` out of a `SKILL.md` — the leading `---` block's
/// `key: value` lines, quotes stripped, whitespace collapsed, description cut
/// to [`DESCRIPTION_MAX`] characters. YAML folded/literal scalars
/// (`description: >` or `|` followed by indented lines) are joined into that
/// one line; without them the row would only ever show ">". No frontmatter
/// means no fields.
pub fn parse_frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let lines: Vec<&str> = text.lines().collect();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return (None, None);
    }
    let mut name = None;
    let mut description = None;
    let mut ix = 1;
    while ix < lines.len() {
        let trimmed = lines[ix].trim();
        ix += 1;
        if trimmed == "---" {
            break;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        if key != "name" && key != "description" {
            continue;
        }
        let mut value = value.trim().to_string();
        // A block scalar's text is the indented run that follows it.
        if matches!(value.as_str(), ">" | "|" | ">-" | "|-" | ">+" | "|+") {
            let mut parts: Vec<&str> = Vec::new();
            while ix < lines.len() {
                let next = lines[ix];
                if next.trim().is_empty() {
                    ix += 1;
                    continue;
                }
                if !next.starts_with([' ', '\t']) {
                    break;
                }
                parts.push(next.trim());
                ix += 1;
            }
            value = parts.join(" ");
        }
        let value: String = value
            .trim()
            .trim_matches(['"', '\''])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if value.is_empty() {
            continue;
        }
        if key == "name" {
            name = Some(value);
        } else {
            description = Some(truncate(&value, DESCRIPTION_MAX));
        }
    }
    (name, description)
}

/// `text` cut to `max` characters (not bytes), with an ellipsis when cut.
pub fn truncate(text: &str, max: usize) -> String {
    let mut out = String::new();
    for (ix, ch) in text.chars().enumerate() {
        if ix >= max {
            out.push('\u{2026}');
            break;
        }
        out.push(ch);
    }
    out
}

/// Every skill folder directly inside `dir`, sorted by title. A folder counts
/// as a skill when it holds `SKILL.md` (on) or `SKILL.md.disabled` (off);
/// anything else is ignored. A missing or unreadable `dir` yields nothing —
/// the agent simply has no skills there.
pub fn scan_dir(dir: &Path, scope: SkillScope) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut skills: Vec<Skill> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let enabled_file = path.join(SKILL_FILE);
        let disabled_file = path.join(SKILL_FILE_DISABLED);
        let (enabled, file) = if enabled_file.is_file() {
            (true, enabled_file)
        } else if disabled_file.is_file() {
            (false, disabled_file)
        } else {
            continue;
        };
        let folder = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if folder.is_empty() {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let (name, description) = parse_frontmatter(&text);
        skills.push(Skill {
            title: name.unwrap_or_else(|| folder.clone()),
            folder,
            description,
            scope,
            dir: path,
            enabled,
        });
    }
    skills.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    skills
}

/// Every skill of `agent`, global dirs first, duplicates by (scope, folder)
/// dropped (Codex lists `.codex/skills` before `.agents/skills`).
pub fn scan_agent(agent: SkillAgent, home: Option<&Path>, project: Option<&Path>) -> Vec<Skill> {
    let mut seen: Vec<(SkillScope, String)> = Vec::new();
    let mut skills = Vec::new();
    for (scope, dir) in skill_dirs(agent, home, project) {
        for skill in scan_dir(&dir, scope) {
            let key = (scope, skill.folder.clone());
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            skills.push(skill);
        }
    }
    skills
}

/// Rename `SKILL.md` ⇄ `SKILL.md.disabled` inside `dir`. Already in the
/// wanted state is success — the page re-scans anyway.
pub fn set_skill_enabled(dir: &Path, enabled: bool) -> std::io::Result<()> {
    let on = dir.join(SKILL_FILE);
    let off = dir.join(SKILL_FILE_DISABLED);
    let (from, to) = if enabled { (&off, &on) } else { (&on, &off) };
    if to.is_file() && !from.is_file() {
        return Ok(());
    }
    std::fs::rename(from, to)
}

/// Copy `from` recursively to `to`. Refuses when `to` exists: a transfer never
/// overwrites another agent's skill.
pub fn copy_skill_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} already exists", to.display()),
        ));
    }
    copy_tree(from, to)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// What a finished transfer says: how many folders were copied and how many
/// were skipped because the target already had them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransferReport {
    pub copied: usize,
    pub skipped: usize,
    pub failed: usize,
}

impl TransferReport {
    pub fn summary(self) -> String {
        let mut parts = vec![format!(
            "{} skill{} copied",
            self.copied,
            if self.copied == 1 { "" } else { "s" }
        )];
        if self.skipped > 0 {
            parts.push(format!("{} already there", self.skipped));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        parts.join(", ")
    }
}

/// `path` with the home dir replaced by `~`, always with forward slashes —
/// the short annotation under an agent's name.
pub fn display_dir(path: &Path, home: Option<&Path>) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if let Some(home) = home {
        let home = home.to_string_lossy().replace('\\', "/");
        let home = home.trim_end_matches('/');
        if !home.is_empty() && text.starts_with(home) {
            return format!("~{}", &text[home.len()..]);
        }
    }
    text
}

/// The last path segment of `path` — the project name in the `Project · …`
/// sub-header.
pub fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// The user's home dir, or `None` where the platform does not name one.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// What one scan produced.
#[derive(Debug, Clone, Default)]
struct Scan {
    /// Skills per agent, in [`SkillAgent::ALL`] order.
    by_agent: BTreeMap<SkillAgent, Vec<Skill>>,
}

impl Scan {
    fn run(home: Option<&Path>, project: Option<&Path>) -> Self {
        let mut by_agent = BTreeMap::new();
        for agent in SkillAgent::ALL {
            by_agent.insert(agent, scan_agent(agent, home, project));
        }
        Self { by_agent }
    }

    fn skills(&self, agent: SkillAgent) -> &[Skill] {
        self.by_agent.get(&agent).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Which agents get a section: the installed ones, plus any agent that has
/// skills anyway (a CLI probe can fail while the folder is right there).
pub fn sections(
    installed: &[SkillAgent],
    has_skills: impl Fn(SkillAgent) -> bool,
) -> Vec<SkillAgent> {
    SkillAgent::ALL
        .into_iter()
        .filter(|agent| installed.contains(agent) || has_skills(*agent))
        .collect()
}

/// The open "Transfer skills" dialog.
#[derive(Debug, Clone)]
struct Transfer {
    source: SkillAgent,
    /// The source skills to copy, by (scope, folder).
    picked: Vec<(SkillScope, String)>,
    targets: Vec<SkillAgent>,
}

impl Transfer {
    fn new(source: SkillAgent) -> Self {
        Self {
            source,
            picked: Vec::new(),
            targets: Vec::new(),
        }
    }
}

pub struct SkillsPage {
    state: Entity<AppState>,
    scan: Loadable<Scan>,
    /// Installed CLIs from `ListHarnesses`; empty until that lands.
    installed: Vec<SkillAgent>,
    home: Option<PathBuf>,
    /// The project folder whose per-project skills are listed.
    project: Option<PathBuf>,
    transfer: Option<Transfer>,
    /// The last transfer's summary, shown as a quiet line under the header.
    notice: Option<SharedString>,
    error: Option<SharedString>,
    /// A rename or copy in flight — the switches stay put until it lands.
    busy: bool,
    scan_task: Option<Task<()>>,
    harness_task: Option<Task<()>>,
}

impl SkillsPage {
    pub fn new(state: Entity<AppState>, project: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            scan: Loadable::Idle,
            installed: Vec::new(),
            home: home_dir(),
            project,
            transfer: None,
            notice: None,
            error: None,
            busy: false,
            scan_task: None,
            harness_task: None,
        };
        page.load_installed(cx);
        page.rescan(cx);
        page
    }

    /// `ListHarnesses` tells us which CLIs exist on this device — the same
    /// probe the Agents page uses.
    fn load_installed(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.harness_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_HARNESSES, serde_json::json!({}))
                .await;
            let installed: Vec<SkillAgent> = result
                .ok()
                .and_then(|value| serde_json::from_value::<Vec<HarnessDescriptor>>(value).ok())
                .map(|list| {
                    SkillAgent::ALL
                        .into_iter()
                        .filter(|agent| {
                            list.iter().any(|d| d.id == agent.harness() && d.installed)
                        })
                        .collect()
                })
                .unwrap_or_default();
            this.update(cx, |page, cx| {
                page.installed = installed;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Re-read every skills directory on the background executor.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        let home = self.home.clone();
        let project = self.project.clone();
        if matches!(self.scan, Loadable::Idle) {
            self.scan = Loadable::Loading;
        }
        let scan =
            cx.background_spawn(async move { Scan::run(home.as_deref(), project.as_deref()) });
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let scan = scan.await;
            this.update(cx, |page, cx| {
                page.scan = Loadable::Ready(scan);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Apply `work` on the background executor and re-scan in the same pass,
    /// so the page never shows a state the disk does not have.
    fn apply<W>(&mut self, work: W, cx: &mut Context<Self>)
    where
        W: FnOnce() -> Result<Option<SharedString>, String> + Send + 'static,
    {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        let home = self.home.clone();
        let project = self.project.clone();
        let apply = cx.background_spawn(async move {
            let applied = work();
            (applied, Scan::run(home.as_deref(), project.as_deref()))
        });
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let (applied, scan) = apply.await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match applied {
                    Ok(notice) => page.notice = notice,
                    Err(err) => page.error = Some(err.into()),
                }
                page.scan = Loadable::Ready(scan);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn toggle(&mut self, dir: PathBuf, enabled: bool, cx: &mut Context<Self>) {
        self.apply(
            move || {
                set_skill_enabled(&dir, enabled)
                    .map(|()| None)
                    .map_err(|err| err.to_string())
            },
            cx,
        );
    }

    // ---- transfer dialog ----

    fn open_transfer(&mut self, cx: &mut Context<Self>) {
        let source = self
            .visible_agents()
            .first()
            .copied()
            .unwrap_or(SkillAgent::ClaudeCode);
        self.transfer = Some(Transfer::new(source));
        self.notice = None;
        cx.notify();
    }

    fn close_transfer(&mut self, cx: &mut Context<Self>) {
        self.transfer = None;
        cx.notify();
    }

    fn set_transfer_source(&mut self, source: SkillAgent, cx: &mut Context<Self>) {
        if let Some(transfer) = self.transfer.as_mut() {
            transfer.source = source;
            transfer.picked.clear();
            transfer.targets.retain(|t| *t != source);
        }
        cx.notify();
    }

    fn toggle_pick(&mut self, key: (SkillScope, String), cx: &mut Context<Self>) {
        if let Some(transfer) = self.transfer.as_mut() {
            if let Some(ix) = transfer.picked.iter().position(|k| *k == key) {
                transfer.picked.remove(ix);
            } else {
                transfer.picked.push(key);
            }
        }
        cx.notify();
    }

    fn toggle_target(&mut self, agent: SkillAgent, cx: &mut Context<Self>) {
        if let Some(transfer) = self.transfer.as_mut() {
            if let Some(ix) = transfer.targets.iter().position(|t| *t == agent) {
                transfer.targets.remove(ix);
            } else {
                transfer.targets.push(agent);
            }
        }
        cx.notify();
    }

    /// Copy every picked folder into every picked target's dir of the same
    /// scope. Existing folders are skipped, never overwritten.
    fn run_transfer(&mut self, cx: &mut Context<Self>) {
        let Some(transfer) = self.transfer.clone() else {
            return;
        };
        let Loadable::Ready(scan) = self.scan.clone() else {
            return;
        };
        if transfer.picked.is_empty() || transfer.targets.is_empty() {
            return;
        }
        let home = self.home.clone();
        let project = self.project.clone();
        let jobs: Vec<(PathBuf, PathBuf)> = transfer
            .picked
            .iter()
            .filter_map(|(scope, folder)| {
                scan.skills(transfer.source)
                    .iter()
                    .find(|s| s.scope == *scope && s.folder == *folder)
            })
            .flat_map(|skill| {
                transfer
                    .targets
                    .iter()
                    .filter_map(|target| {
                        copy_target_dir(
                            *target,
                            skill.scope,
                            home.as_deref(),
                            project.as_deref(),
                        )
                        .map(|dir| (skill.dir.clone(), dir.join(&skill.folder)))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        self.transfer = None;
        self.apply(
            move || {
                let mut report = TransferReport::default();
                for (from, to) in jobs {
                    if to.exists() {
                        report.skipped += 1;
                    } else if copy_skill_dir(&from, &to).is_ok() {
                        report.copied += 1;
                    } else {
                        report.failed += 1;
                    }
                }
                Ok(Some(report.summary().into()))
            },
            cx,
        );
    }

    fn visible_agents(&self) -> Vec<SkillAgent> {
        let scan = self.scan.ready();
        sections(&self.installed, |agent| {
            scan.is_some_and(|s| !s.skills(agent).is_empty())
        })
    }

    /// The agent's brand mark in its ORIGINAL colours: coloured where the
    /// vendor's mark is coloured (Claude's orange sparkle), otherwise the
    /// monochrome mark in the foreground colour. Never the provider ink of the
    /// effort slider — that would invent brand colours the vendors do not use.
    fn brand(agent: SkillAgent, theme: &Theme, size: f32) -> gpui::Svg {
        let (mark, tint) = crate::pickers::harness_brand_icon(agent.harness());
        crate::icons::icon(mark)
            .size(px(size))
            .text_color(tint.unwrap_or(theme.text))
    }

    /// Section header: the brand mark, the agent name, and its skill
    /// directories as a short muted annotation.
    fn section_header(&self, agent: SkillAgent, theme: &Theme) -> AnyElement {
        let dirs = agent.dir_hints().join("   ");
        div()
            .mt(px(24.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_none()
                    .size(px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Self::brand(agent, theme, 16.0)),
            )
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(widgets::ROW_TITLE_SIZE))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(agent.label())),
                    )
                    .child(
                        div()
                            .mt(px(1.0))
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.5))
                            .text_color(theme.text_muted.opacity(0.55))
                            .child(SharedString::from(dirs)),
                    ),
            )
            .into_any_element()
    }

    /// A quiet sub-header inside an agent card, separating the global skills
    /// from the project ones.
    fn group_header(&self, label: String, first: bool, theme: &Theme) -> AnyElement {
        div()
            .px(px(20.0))
            .py(px(7.0))
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .bg(ink(0.02))
            .text_size(crate::typography::ui_rems(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.65))
            .child(SharedString::from(popover::tracked_upper(&label)))
            .into_any_element()
    }

    /// One skill row — the Notifications rhythm: tile, a flexible title +
    /// one-line description column, the switch in the same right-hand column
    /// as every other settings page.
    fn skill_row(
        &self,
        agent: SkillAgent,
        skill: &Skill,
        theme: &Theme,
        switch_t: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dir = skill.dir.clone();
        let enabled = skill.enabled;
        let interactive = !self.busy;
        let accent = theme.accent;
        let title = skill.title.clone();
        // Unique app-wide: the row index restarts per agent card, and a
        // duplicate element id panics the a11y tree in debug builds.
        let toggle_id = SharedString::from(format!("toggle-{}", skill.motion_key(agent)));

        let mut text = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(widgets::row_title(theme, title.clone()));
        if let Some(description) = skill.description.clone() {
            text = text.child(
                div()
                    .mt(px(Theme::TEXT_STACK_GAP))
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(widgets::ROW_DESCRIPTION_SIZE))
                    .text_color(theme.text_muted.opacity(0.65))
                    .child(SharedString::from(description)),
            );
        }

        widgets::card_row(theme, false)
            .child(widgets::row_tile(theme, crate::icons::DOCUMENT))
            .child(text)
            .child(
                div()
                    .id(toggle_id)
                    .flex_none()
                    .size(px(40.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .role(gpui::Role::Switch)
                    .aria_label(SharedString::from(title))
                    .aria_toggled(if enabled {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    })
                    .child(widgets::toggle_switch_t(theme, switch_t))
                    .when(interactive, |el| {
                        el.tab_index(0)
                            .focus_visible(move |style| style.border_2().border_color(accent))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle(dir.clone(), !enabled, cx);
                            }))
                    }),
            )
            .into_any_element()
    }

    /// The agent's card: the global group, then the project group.
    fn agent_card(
        &self,
        agent: SkillAgent,
        scan: &Scan,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let skills = scan.skills(agent).to_vec();
        if skills.is_empty() {
            return widgets::section_card(theme)
                .mt(px(8.0))
                .child(
                    div()
                        .px(px(20.0))
                        .py(px(24.0))
                        .text_center()
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text_muted.opacity(0.6))
                        .child(SharedString::from(format!(
                            "No skills installed for {}.",
                            agent.label()
                        ))),
                )
                .into_any_element();
        }
        // Switch progress first: the row closures borrow `cx` afterwards.
        let switch_t: Vec<f32> = skills
            .iter()
            .map(|skill| widgets::switch_progress(&skill.motion_key(agent), skill.enabled, cx))
            .collect();

        let project_label = self
            .project
            .as_deref()
            .map(|path| format!("Project \u{b7} {}", folder_name(path)))
            .unwrap_or_else(|| "Project".to_string());
        let mut card = widgets::section_card(theme).mt(px(8.0));
        let mut first_group = true;
        for (scope, label) in [
            (SkillScope::Global, "Global".to_string()),
            (SkillScope::Project, project_label),
        ] {
            let group: Vec<usize> = skills
                .iter()
                .enumerate()
                .filter(|(_, skill)| skill.scope == scope)
                .map(|(ix, _)| ix)
                .collect();
            // Global always announces itself, so a single group still reads as
            // "these are the global ones"; an empty project group is omitted.
            if group.is_empty() && scope != SkillScope::Global {
                continue;
            }
            card = card.child(self.group_header(label, first_group, theme));
            first_group = false;
            for ix in group {
                card = card.child(self.skill_row(agent, &skills[ix], theme, switch_t[ix], cx));
            }
        }
        card.into_any_element()
    }

    // ---- the transfer dialog ----

    fn render_transfer_dialog(
        &self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let transfer = self.transfer.clone()?;
        let scan = self.scan.ready()?.clone();
        let theme = Theme::of(cx).clone();
        let agents = self.visible_agents();
        let source_skills = scan.skills(transfer.source).to_vec();

        let chip = |label: String,
                    selected: bool,
                    id: (&'static str, usize),
                    theme: &Theme|
         -> gpui::Stateful<gpui::Div> {
            div()
                .id(id)
                .px(px(10.0))
                .py(px(4.0))
                .rounded_full()
                .border_1()
                .border_color(if selected { theme.accent } else { theme.border })
                .bg(if selected {
                    theme.accent.opacity(0.12)
                } else {
                    ink(0.02)
                })
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(if selected { theme.text } else { theme.text_muted })
                .cursor_pointer()
                .child(SharedString::from(label))
        };

        let mut source_row = div()
            .mt(px(12.0))
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(6.0));
        for (ix, agent) in agents.iter().copied().enumerate() {
            let selected = agent == transfer.source;
            source_row = source_row.child(
                chip(agent.label().to_string(), selected, ("transfer-source", ix), &theme)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_transfer_source(agent, cx)
                    })),
            );
        }

        let mut list = div()
            .id("transfer-skill-list")
            .mt(px(8.0))
            .max_h(px(190.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border);
        if source_skills.is_empty() {
            list = list.child(
                div()
                    .px(px(12.0))
                    .py(px(14.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("No skills to transfer.")),
            );
        }
        for (ix, skill) in source_skills.iter().enumerate() {
            let key = (skill.scope, skill.folder.clone());
            let picked = transfer.picked.contains(&key);
            let scope_tag = if skill.scope == SkillScope::Project {
                " \u{b7} project"
            } else {
                ""
            };
            list = list.child(
                div()
                    .id(("transfer-skill", ix))
                    .px(px(10.0))
                    .py(px(6.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(ink(0.04)))
                    .child(
                        div()
                            .flex_none()
                            .size(px(14.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(if picked { theme.accent } else { theme.border })
                            .bg(if picked {
                                theme.accent.opacity(0.9)
                            } else {
                                ink(0.02)
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(picked, |el| {
                                el.child(
                                    crate::icons::icon(crate::icons::CHECK)
                                        .size(px(10.0))
                                        .text_color(theme.on_solid),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(12.5))
                            .text_color(theme.text)
                            .child(SharedString::from(format!(
                                "{}{scope_tag}",
                                skill.title.clone()
                            ))),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_pick(key.clone(), cx)
                    })),
            );
        }

        let mut target_row = div()
            .mt(px(8.0))
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(6.0));
        for (ix, agent) in agents
            .iter()
            .copied()
            .filter(|a| *a != transfer.source)
            .enumerate()
        {
            let selected = transfer.targets.contains(&agent);
            target_row = target_row.child(
                chip(agent.label().to_string(), selected, ("transfer-target", ix), &theme)
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_target(agent, cx))),
            );
        }

        let ready = !transfer.picked.is_empty() && !transfer.targets.is_empty();
        let card = popover::dialog_card(&theme)
            .w(px(420.0))
            .child(popover::dialog_title(&theme, "Transfer skills"))
            .child(
                popover::dialog_body(
                    &theme,
                    "Copy skill folders into another agent's directory of the same scope. Skills \
                     the target already has are skipped.",
                )
                .mt(px(6.0)),
            )
            .child(widgets::field_label(&theme, "From").mt(px(14.0)))
            .child(source_row)
            .child(widgets::field_label(&theme, "Skills").mt(px(14.0)))
            .child(list)
            .child(widgets::field_label(&theme, "To").mt(px(14.0)))
            .child(target_row)
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "transfer-cancel")
                            .id("transfer-cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.close_transfer(cx))),
                    )
                    .child(
                        popover::btn_primary(&theme, "Transfer")
                            .id("transfer-confirm")
                            .when(!ready, |el| el.opacity(0.45))
                            .when(ready, |el| {
                                el.on_click(
                                    cx.listener(|this, _, _, cx| this.run_transfer(cx)),
                                )
                            }),
                    ),
            )
            .into_any_element();
        Some(popover::modal("skills-transfer-dialog", viewport, card))
    }
}

impl Render for SkillsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(widgets::page_header(&theme, "Skills", None))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        widgets::ghost_action(&theme)
                            .id("skills-transfer")
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .child(
                                crate::icons::icon(crate::icons::COPY)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Transfer skills"))
                            .on_click(cx.listener(|this, _, _, cx| this.open_transfer(cx))),
                    )
                    .child(
                        widgets::ghost_action(&theme)
                            .id("skills-refresh")
                            .hover(|s| widgets::ghost_hover(&theme, s))
                            .child(
                                crate::icons::icon(crate::icons::REFRESH)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Refresh"))
                            .on_click(cx.listener(|this, _, _, cx| this.rescan(cx))),
                    ),
            );

        let mut column = widgets::page_column().child(header).child(
            widgets::page_subtitle(
                &theme,
                "Installed skills per agent. Off renames SKILL.md so the agent no longer loads \
                 it.",
            )
            .max_w(px(512.0))
            .line_height(px(20.0)),
        );

        if let Some(notice) = self.notice.clone() {
            column = column.child(
                div()
                    .mt(px(8.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child(notice),
            );
        }

        let scan = match self.scan.clone() {
            Loadable::Ready(scan) => scan,
            _ => {
                return div()
                    .id("skills-page")
                    .size_full()
                    .overflow_y_scroll()
                    .child(column.child(
                        widgets::section_card(&theme).p(px(20.0)).child(
                            div()
                                .text_size(crate::typography::ui_rems(13.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from("Reading skill folders\u{2026}")),
                        ),
                    ))
                    .into_any_element();
            }
        };

        let visible = self.visible_agents();
        if visible.is_empty() {
            column = column.child(
                widgets::section_card(&theme).p(px(24.0)).child(
                    div()
                        .text_center()
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text_muted.opacity(0.6))
                        .child(SharedString::from(
                            "No agent with a skills directory is installed on this device.",
                        )),
                ),
            );
        }
        for agent in visible {
            column = column
                .child(self.section_header(agent, &theme))
                .child(self.agent_card(agent, &scan, &theme, cx));
        }

        if let Some(error) = self.error.clone() {
            column = column.child(widgets::error_strip(&theme, error));
        }

        let dialog = self.render_transfer_dialog(window.viewport_size(), cx);
        div()
            .id("skills-page")
            .size_full()
            .overflow_y_scroll()
            .child(column)
            .children(dialog)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch dir that exists on every platform (`/tmp` does not on
    /// Windows) — the repo's test convention.
    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zeron-skills-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn write_skill(root: &Path, name: &str, body: &str, enabled: bool) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(if enabled { SKILL_FILE } else { SKILL_FILE_DISABLED });
        std::fs::write(file, body).unwrap();
        dir
    }

    #[test]
    fn frontmatter_yields_name_and_description() {
        let (name, description) = parse_frontmatter(
            "---\nname: Grill Me\ndescription: Interviews the user relentlessly.\nextra: no\n---\n\n# Body\n",
        );
        assert_eq!(name.as_deref(), Some("Grill Me"));
        assert_eq!(
            description.as_deref(),
            Some("Interviews the user relentlessly.")
        );
    }

    #[test]
    fn folded_and_literal_descriptions_are_joined_into_one_line() {
        for marker in [">", "|", ">-", "|-"] {
            let (name, description) = parse_frontmatter(&format!(
                "---\nname: Folded\ndescription: {marker}\n  First line here\n  second line \
                 here\n---\n"
            ));
            assert_eq!(name.as_deref(), Some("Folded"), "{marker}");
            assert_eq!(
                description.as_deref(),
                Some("First line here second line here"),
                "{marker}"
            );
        }
        // A block scalar as the LAST key still stops at the closing fence.
        let (_, description) =
            parse_frontmatter("---\ndescription: >\n  only line\n---\nbody\n");
        assert_eq!(description.as_deref(), Some("only line"));
    }

    #[test]
    fn frontmatter_strips_quotes_and_truncates_long_descriptions() {
        let long = "x".repeat(200);
        let (name, description) =
            parse_frontmatter(&format!("---\nname: \"Quoted\"\ndescription: '{long}'\n---\n"));
        assert_eq!(name.as_deref(), Some("Quoted"));
        let description = description.unwrap();
        assert_eq!(description.chars().count(), DESCRIPTION_MAX + 1);
        assert!(description.ends_with('\u{2026}'));
    }

    #[test]
    fn a_file_without_frontmatter_has_no_fields() {
        assert_eq!(parse_frontmatter("# Just a heading\n"), (None, None));
        assert_eq!(parse_frontmatter(""), (None, None));
    }

    #[test]
    fn scan_lists_enabled_and_disabled_folders_and_ignores_the_rest() {
        let root = tmp_dir();
        write_skill(&root, "alpha", "---\nname: Alpha\ndescription: First.\n---\n", true);
        write_skill(&root, "zulu", "---\nname: Zulu\n---\n", false);
        std::fs::create_dir_all(root.join("not-a-skill")).unwrap();
        std::fs::write(root.join("loose.md"), "x").unwrap();

        let skills = scan_dir(&root, SkillScope::Global);
        assert_eq!(skills.len(), 2, "{skills:?}");
        assert_eq!(skills[0].title, "Alpha");
        assert_eq!(skills[0].folder, "alpha");
        assert_eq!(skills[0].description.as_deref(), Some("First."));
        assert!(skills[0].enabled);
        assert_eq!(skills[1].title, "Zulu");
        assert!(!skills[1].enabled);
        assert_eq!(skills[1].description, None);
        // A folder without frontmatter falls back to its own name.
        write_skill(&root, "bare", "# hi\n", true);
        let skills = scan_dir(&root, SkillScope::Global);
        assert!(skills.iter().any(|s| s.title == "bare"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn toggling_renames_the_skill_file_both_ways() {
        let root = tmp_dir();
        let dir = write_skill(&root, "alpha", "---\nname: Alpha\n---\n", true);

        set_skill_enabled(&dir, false).expect("disable");
        assert!(!dir.join(SKILL_FILE).exists());
        assert!(dir.join(SKILL_FILE_DISABLED).is_file());
        assert!(!scan_dir(&root, SkillScope::Global)[0].enabled);

        set_skill_enabled(&dir, true).expect("enable");
        assert!(dir.join(SKILL_FILE).is_file());
        assert!(!dir.join(SKILL_FILE_DISABLED).exists());
        assert!(scan_dir(&root, SkillScope::Global)[0].enabled);
        // Already in the wanted state is not an error.
        set_skill_enabled(&dir, true).expect("idempotent");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn copy_is_recursive_and_never_overwrites() {
        let root = tmp_dir();
        let from = write_skill(&root, "alpha", "---\nname: Alpha\n---\n", true);
        std::fs::create_dir_all(from.join("refs")).unwrap();
        std::fs::write(from.join("refs").join("note.md"), "deep").unwrap();

        let to = root.join("target").join("alpha");
        copy_skill_dir(&from, &to).expect("copy");
        assert!(to.join(SKILL_FILE).is_file());
        assert_eq!(
            std::fs::read_to_string(to.join("refs").join("note.md")).unwrap(),
            "deep"
        );
        assert!(copy_skill_dir(&from, &to).is_err(), "never overwrites");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skill_dirs_per_agent() {
        let home = PathBuf::from("/home/remo");
        let project = PathBuf::from("/work/proj");
        let dirs = |agent| {
            skill_dirs(agent, Some(home.as_path()), Some(project.as_path()))
                .into_iter()
                .map(|(scope, path)| (scope, path.to_string_lossy().replace('\\', "/")))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            dirs(SkillAgent::ClaudeCode),
            vec![
                (SkillScope::Global, "/home/remo/.claude/skills".to_string()),
                (SkillScope::Project, "/work/proj/.claude/skills".to_string()),
            ]
        );
        assert_eq!(
            dirs(SkillAgent::Codex),
            vec![
                (SkillScope::Global, "/home/remo/.codex/skills".to_string()),
                (SkillScope::Project, "/work/proj/.codex/skills".to_string()),
                (SkillScope::Project, "/work/proj/.agents/skills".to_string()),
            ]
        );
        assert_eq!(
            dirs(SkillAgent::Pi),
            vec![(
                SkillScope::Global,
                "/home/remo/.pi/agent/skills".to_string()
            )]
        );
        assert_eq!(
            dirs(SkillAgent::Opencode),
            vec![
                (
                    SkillScope::Global,
                    "/home/remo/.config/opencode/skills".to_string()
                ),
                (
                    SkillScope::Project,
                    "/work/proj/.opencode/skills".to_string()
                ),
            ]
        );
        // No project: only the global dirs survive.
        assert_eq!(
            skill_dirs(SkillAgent::Codex, Some(home.as_path()), None).len(),
            1
        );
        // No home: nothing global.
        assert!(skill_dirs(SkillAgent::Pi, None, Some(project.as_path())).is_empty());
        // A copy lands in the FIRST dir of the scope, never `.agents`.
        assert_eq!(
            copy_target_dir(
                SkillAgent::Codex,
                SkillScope::Project,
                Some(home.as_path()),
                Some(project.as_path())
            )
            .map(|p| p.to_string_lossy().replace('\\', "/")),
            Some("/work/proj/.codex/skills".to_string())
        );
        assert_eq!(
            copy_target_dir(
                SkillAgent::Pi,
                SkillScope::Project,
                Some(home.as_path()),
                None
            ),
            None
        );
    }

    #[test]
    fn sections_show_installed_agents_and_anything_with_skills() {
        let visible = sections(&[SkillAgent::ClaudeCode], |agent| {
            agent == SkillAgent::Opencode
        });
        assert_eq!(visible, vec![SkillAgent::ClaudeCode, SkillAgent::Opencode]);
        assert!(sections(&[], |_| false).is_empty());
    }

    #[test]
    fn display_dir_abbreviates_the_home_dir() {
        let home = PathBuf::from(r"C:\Users\remo");
        assert_eq!(
            display_dir(Path::new(r"C:\Users\remo\.claude\skills"), Some(&home)),
            "~/.claude/skills"
        );
        assert_eq!(
            display_dir(Path::new(r"D:\AI-OS\.claude\skills"), Some(&home)),
            "D:/AI-OS/.claude/skills"
        );
        assert_eq!(folder_name(Path::new(r"D:\AI-OS")), "AI-OS");
    }

    #[test]
    fn a_transfer_report_reads_as_one_line() {
        assert_eq!(
            TransferReport {
                copied: 1,
                skipped: 0,
                failed: 0
            }
            .summary(),
            "1 skill copied"
        );
        assert_eq!(
            TransferReport {
                copied: 3,
                skipped: 2,
                failed: 1
            }
            .summary(),
            "3 skills copied, 2 already there, 1 failed"
        );
    }
}
