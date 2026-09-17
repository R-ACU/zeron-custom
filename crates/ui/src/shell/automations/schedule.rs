//! Pure helpers behind the automation editor and list: human schedule words,
//! the frequency presets, next-run wording, path display and the model list
//! ordering. Nothing here touches gpui, so all of it is unit tested.
use chrono::Datelike as _;
use zeron_proto::{Deliver, HarnessId, Model};

/// The engine accepts intervals from one minute up to one year.
pub(crate) const MAX_INTERVAL_MINUTES: u32 = 525_600;

const HOUR: u32 = 60;
const DAY: u32 = 24 * HOUR;
const WEEK: u32 = 7 * DAY;

/// The Frequency dropdown's presets, in menu order. "Custom..." follows them.
pub(crate) const PRESETS: [u32; 7] = [15, 30, HOUR, 6 * HOUR, 12 * HOUR, DAY, WEEK];

/// The interval a new automation starts with.
pub(crate) const DEFAULT_INTERVAL: u32 = DAY;

/// The unit a custom frequency is typed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntervalUnit {
    Minutes,
    Hours,
    Days,
}

impl IntervalUnit {
    pub(crate) const ALL: [IntervalUnit; 3] = [Self::Minutes, Self::Hours, Self::Days];

    pub(crate) fn minutes(self) -> u32 {
        match self {
            Self::Minutes => 1,
            Self::Hours => HOUR,
            Self::Days => DAY,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Minutes => "Minutes",
            Self::Hours => "Hours",
            Self::Days => "Days",
        }
    }
}

/// Whether `minutes` is one of the dropdown presets.
pub(crate) fn is_preset(minutes: u32) -> bool {
    PRESETS.contains(&minutes)
}

fn every(count: u32, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("Every {singular}")
    } else {
        format!("Every {count} {plural}")
    }
}

/// A schedule in words: the largest whole unit that fits exactly
/// ("Every day", "Every 6 hours", "Every 90 minutes", "Every 2 weeks").
pub(crate) fn describe_interval(minutes: u32) -> String {
    match minutes {
        0 => "Not scheduled".into(),
        m if m % WEEK == 0 => every(m / WEEK, "week", "weeks"),
        m if m % DAY == 0 => every(m / DAY, "day", "days"),
        m if m % HOUR == 0 => every(m / HOUR, "hour", "hours"),
        m => every(m, "minute", "minutes"),
    }
}

/// Weekday names, index 0 = Monday (the wire value of `weekday`).
pub(crate) const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// The time of day a new day-scale automation starts with (09:00).
pub(crate) const DEFAULT_TIME_OF_DAY: u16 = 9 * 60;

/// The weekday a new weekly automation starts with (Monday).
pub(crate) const DEFAULT_WEEKDAY: u8 = 0;

/// Whether the interval is a whole number of days, so a time of day applies.
pub(crate) fn supports_time_of_day(minutes: u32) -> bool {
    minutes > 0 && minutes % DAY == 0
}

/// Whether the interval is a whole number of weeks, so a weekday applies.
pub(crate) fn supports_weekday(minutes: u32) -> bool {
    minutes > 0 && minutes % WEEK == 0
}

/// A time of day as "HH:MM".
pub(crate) fn format_time_of_day(minutes: u16) -> String {
    let minutes = minutes.min(1439);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Parse a typed "HH:MM" (or "8:5", or "0800") into minutes since midnight.
pub(crate) fn parse_time_of_day(value: &str) -> Result<u16, &'static str> {
    const BAD: &str = "Enter a time between 00:00 and 23:59.";
    let value = value.trim();
    let (hours, minutes) = match value.split_once(':') {
        Some((h, m)) => (h.trim(), m.trim()),
        None if value.len() == 4 && value.bytes().all(|b| b.is_ascii_digit()) => {
            (&value[..2], &value[2..])
        }
        None => return Err(BAD),
    };
    let hours: u16 = hours.parse().map_err(|_| BAD)?;
    let minutes: u16 = minutes.parse().map_err(|_| BAD)?;
    if hours > 23 || minutes > 59 {
        return Err(BAD);
    }
    Ok(hours * 60 + minutes)
}

/// A weekday name for the wire value (0 = Monday).
pub(crate) fn weekday_name(weekday: u8) -> &'static str {
    WEEKDAYS[usize::from(weekday.min(6))]
}

/// The whole schedule in words: the interval plus its anchor, if any
/// ("Every day at 08:00", "Every Monday at 09:00", "Every 2 weeks on Sat").
pub(crate) fn describe_schedule(
    minutes: u32,
    time_of_day: Option<u16>,
    weekday: Option<u8>,
) -> String {
    let mut words = match weekday.filter(|_| supports_weekday(minutes)) {
        Some(day) if minutes == WEEK => format!("Every {}", weekday_name(day)),
        Some(day) => format!("{} on {}", describe_interval(minutes), weekday_name(day)),
        None => describe_interval(minutes),
    };
    if let Some(time) = time_of_day.filter(|_| minutes > 0) {
        words.push_str(&format!(" at {}", format_time_of_day(time)));
    }
    words
}

pub(crate) fn describe_days(days: &[u8], time: u16) -> String {
    let label = if days.len() == 7 { "Every day".to_string() }
        else { days.iter().map(|day| weekday_name(*day)).collect::<Vec<_>>().join(", ") };
    format!("{label} at {}", format_time_of_day(time))
}

/// Prefill for the custom fields: the largest unit that divides the interval
/// exactly, so 2160 reads as "36 hours" rather than "2160 minutes".
pub(crate) fn split_interval(minutes: u32) -> (u32, IntervalUnit) {
    let minutes = minutes.max(1);
    for unit in [IntervalUnit::Days, IntervalUnit::Hours] {
        if minutes % unit.minutes() == 0 {
            return (minutes / unit.minutes(), unit);
        }
    }
    (minutes, IntervalUnit::Minutes)
}

/// Parse the custom frequency fields into minutes, with a plain-language error.
pub(crate) fn custom_interval(value: &str, unit: IntervalUnit) -> Result<u32, &'static str> {
    let count: u32 = value
        .trim()
        .parse()
        .map_err(|_| "Enter a whole number, for example 2.")?;
    let minutes = count
        .checked_mul(unit.minutes())
        .filter(|m| (1..=MAX_INTERVAL_MINUTES).contains(m))
        .ok_or("Choose an interval between 1 minute and 1 year.")?;
    Ok(minutes)
}

/// The next run in words, relative to `now`: "Today at 14:30",
/// "Tomorrow at 09:00", otherwise "Sep 18 at 09:00".
pub(crate) fn next_run_words(at: chrono::NaiveDateTime, now: chrono::NaiveDateTime) -> String {
    let time = at.format("%H:%M");
    let days = (at.date() - now.date()).num_days();
    match days {
        0 => format!("Today at {time}"),
        1 => format!("Tomorrow at {time}"),
        _ if at.year() == now.year() => {
            format!("{} at {time}", at.format("%b %-d"))
        }
        _ => format!("{} at {time}", at.format("%b %-d, %Y")),
    }
}

/// A run status (the engine's camelCase wire value) in words.
pub(crate) fn run_status_label(status: &str) -> &'static str {
    match status {
        "running" => "Running",
        "awaitingInput" => "Waiting for your input",
        "succeeded" => "Succeeded",
        "failed" => "Failed",
        "interrupted" => "Interrupted",
        _ => "Unknown",
    }
}

/// Whether a run status should read as a problem.
pub(crate) fn run_status_is_problem(status: &str) -> bool {
    matches!(status, "failed" | "interrupted")
}

/// A folder path as the user would type it: a Windows drive path uses
/// backslashes throughout, and doubled separators collapse ("D:\/AI-OS" ->
/// "D:\AI-OS"). Display only; the saved path is never rewritten.
pub(crate) fn display_path(path: &str) -> String {
    let windows =
        path.len() >= 2 && path.as_bytes()[1] == b':' && path.as_bytes()[0].is_ascii_alphabetic()
            || path.starts_with("\\\\");
    if !windows {
        return path.to_string();
    }
    let unc = path.starts_with("\\\\");
    let mut out = String::with_capacity(path.len());
    if unc {
        out.push_str("\\\\");
    }
    let body = if unc { &path[2..] } else { path };
    for ch in body.chars() {
        let ch = if ch == '/' { '\\' } else { ch };
        if ch == '\\' && out.ends_with('\\') && out.len() > usize::from(unc) * 2 {
            continue;
        }
        out.push(ch);
    }
    out
}

/// The last path segment ("AI-OS" for "D:\AI-OS"), or the path itself.
pub(crate) fn folder_name(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// One item of the automation model list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelItem {
    /// "Default model": leave the pick to the agent.
    Default,
    /// The "OTHER" caption between flagship models and the rest.
    Divider,
    /// An index into the harness catalog.
    Model(usize),
}

/// The rows the model dropdown shows. Without a query: "Default model", the
/// harness's flagship models, an OTHER divider, then the rest in catalog
/// order (the composer picker's grouping). With a query: one ranked run of
/// matches on the label, id or description.
pub(crate) fn model_items(harness: HarnessId, models: &[Model], query: &str) -> Vec<ModelItem> {
    let query = query.trim();
    if !query.is_empty() {
        let mut ranked: Vec<(usize, usize)> = models
            .iter()
            .enumerate()
            .filter_map(|(ix, model)| {
                let by_label = crate::popover::match_rank(query, &model.label);
                let by_rest = crate::popover::match_rank(
                    query,
                    &format!(
                        "{} {} {}",
                        model.description.as_deref().unwrap_or(""),
                        model.label,
                        model.id
                    ),
                )
                .map(|rank| rank + 2);
                by_label.into_iter().chain(by_rest).min().map(|r| (r, ix))
            })
            .collect();
        ranked.sort_unstable();
        return ranked
            .into_iter()
            .map(|(_, ix)| ModelItem::Model(ix))
            .collect();
    }
    let lead = crate::pickers::flagship_lead_rows(harness);
    let mut taken = 0usize;
    let mut flagship = Vec::new();
    let mut other = Vec::new();
    for (ix, model) in models.iter().enumerate() {
        if crate::pickers::is_flagship_model(harness, &model.id) || taken < lead {
            if !crate::pickers::is_flagship_model(harness, &model.id) {
                taken += 1;
            }
            flagship.push(ModelItem::Model(ix));
        } else {
            other.push(ModelItem::Model(ix));
        }
    }
    let mut items = vec![ModelItem::Default];
    let divide = !flagship.is_empty() && !other.is_empty();
    items.extend(flagship);
    if divide {
        items.push(ModelItem::Divider);
    }
    items.extend(other);
    items
}

/// The roles the wizard offers as chips, in menu order.
pub(crate) const ROLE_SUGGESTIONS: [&str; 6] = [
    "CEO",
    "Web Researcher",
    "Release Manager",
    "Inbox Triage",
    "QA",
    "Analyst",
];

/// The instruction templates behind the chips above the instructions box.
pub(crate) const TEMPLATES: [(&str, &str); 4] = [
    (
        "Daily briefing",
        "Read recent-changes.md and TODO.md. Summarize what changed since yesterday and what is          due today in five bullets. Post the summary to the AI-OS worklog.",
    ),
    (
        "Dependency check",
        "Run the dependency audit, list outdated or vulnerable packages with the safe upgrade          path, and open a chat with the findings. Do not upgrade anything.",
    ),
    (
        "Inbox triage",
        "Read new mail with Read-Mail.ps1. Group by urgency, draft replies for anything that          needs one, and leave the drafts in the chat for review.",
    ),
    (
        "Site check",
        "Open the production site, click through the main pages, and report broken links,          console errors, or layout regressions with screenshots.",
    ),
];

/// The "Stop after" choices, in menu order; `None` is no limit.
pub(crate) const STOP_AFTER: [Option<u32>; 5] = [Some(15), Some(30), Some(60), Some(120), None];

/// The run budget a new automation starts with.
pub(crate) const DEFAULT_MAX_RUN_MINUTES: u32 = 30;

/// A "Stop after" choice in words.
pub(crate) fn stop_after_label(minutes: Option<u32>) -> String {
    match minutes {
        None => "No limit".into(),
        Some(1) => "1 minute".into(),
        Some(minutes) => format!("{minutes} minutes"),
    }
}

/// The "Deliver result to" choices, in menu order.
pub(crate) const DELIVER_CHOICES: [Deliver; 3] = [
    Deliver {
        inbox: true,
        desktop_notification: true,
    },
    Deliver {
        inbox: true,
        desktop_notification: false,
    },
    Deliver {
        inbox: false,
        desktop_notification: false,
    },
];

/// A delivery target in words.
pub(crate) fn deliver_label(deliver: Deliver) -> &'static str {
    match (deliver.inbox, deliver.desktop_notification) {
        (true, true) => "Inbox + desktop notification",
        (true, false) => "Inbox only",
        (false, true) => "Desktop notification only",
        (false, false) => "Nothing",
    }
}

/// The result manifest path the "Use default" action fills in.
pub(crate) const DEFAULT_MANIFEST: &str = "results/{{run_id}}.json";

/// A paragraph that tells the agent to write the result manifest at `path`
/// (the engine replaces {{run_id}} in the instructions before each run).
pub(crate) fn manifest_instructions(path: &str) -> String {
    format!(
        "When you are done, write a JSON file to {path} (relative to the working folder) \
         in this shape: {{\"runId\": \"{{{{run_id}}}}\", \"summary\": \"One or two sentences about \
         the result\", \"links\": [{{\"label\": \"Report\", \"type\": \"file\", \"target\": \
         \"results/report.md\"}}]}}. Links can also use \"type\": \"url\" with a web address."
    )
}

/// Append the manifest paragraph to the instructions unless it is already there.
pub(crate) fn with_manifest_instructions(prompt: &str, path: &str) -> String {
    let paragraph = manifest_instructions(path);
    if prompt.contains(&paragraph) {
        return prompt.to_string();
    }
    let trimmed = prompt.trim_end();
    if trimmed.is_empty() {
        paragraph
    } else {
        format!("{trimmed}\n\n{paragraph}")
    }
}

/// An agent's display name, for rows whose catalog has not loaded.
pub(crate) fn agent_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Claude Code",
        HarnessId::Codex => "Codex",
        HarnessId::Cursor => "Cursor",
        HarnessId::Devin => "Devin",
        HarnessId::Grok => "Grok",
        HarnessId::Hermes => "Hermes",
        HarnessId::Pi => "Pi",
        HarnessId::Kimi => "Kimi",
        HarnessId::Cline => "Cline",
        HarnessId::Opencode => "OpenCode",
        HarnessId::Mock => "Mock",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn model(id: &str, label: &str) -> Model {
        Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
            pricing: None,
        }
    }

    #[test]
    fn intervals_read_in_the_largest_whole_unit() {
        assert_eq!(describe_interval(1), "Every minute");
        assert_eq!(describe_interval(15), "Every 15 minutes");
        assert_eq!(describe_interval(90), "Every 90 minutes");
        assert_eq!(describe_interval(60), "Every hour");
        assert_eq!(describe_interval(360), "Every 6 hours");
        assert_eq!(describe_interval(1440), "Every day");
        assert_eq!(describe_interval(2 * 1440), "Every 2 days");
        assert_eq!(describe_interval(10080), "Every week");
        assert_eq!(describe_interval(20160), "Every 2 weeks");
        assert_eq!(describe_interval(MAX_INTERVAL_MINUTES), "Every 365 days");
    }

    #[test]
    fn a_time_of_day_round_trips_through_its_field() {
        assert_eq!(format_time_of_day(DEFAULT_TIME_OF_DAY), "09:00");
        assert_eq!(format_time_of_day(0), "00:00");
        assert_eq!(format_time_of_day(23 * 60 + 59), "23:59");
        for minutes in [0u16, 5, 540, 1439] {
            assert_eq!(parse_time_of_day(&format_time_of_day(minutes)), Ok(minutes));
        }
        assert_eq!(parse_time_of_day(" 8:5 "), Ok(8 * 60 + 5));
        assert_eq!(parse_time_of_day("0730"), Ok(7 * 60 + 30));
        for bad in ["", "24:00", "8", "12:60", "-1:00", "ab:cd", "12:00:00"] {
            assert!(parse_time_of_day(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn only_whole_days_and_weeks_take_an_anchor() {
        assert!(supports_time_of_day(1440));
        assert!(supports_time_of_day(2 * 1440));
        assert!(supports_time_of_day(10080));
        assert!(!supports_time_of_day(60));
        assert!(!supports_time_of_day(0));
        assert!(supports_weekday(10080));
        assert!(!supports_weekday(1440));
    }

    #[test]
    fn the_schedule_reads_with_its_anchor() {
        assert_eq!(describe_schedule(1440, None, None), "Every day");
        assert_eq!(
            describe_schedule(1440, Some(8 * 60), None),
            "Every day at 08:00"
        );
        assert_eq!(
            describe_schedule(2 * 1440, Some(DEFAULT_TIME_OF_DAY), None),
            "Every 2 days at 09:00"
        );
        assert_eq!(
            describe_schedule(10080, Some(9 * 60), Some(DEFAULT_WEEKDAY)),
            "Every Mon at 09:00"
        );
        assert_eq!(
            describe_schedule(20160, Some(9 * 60), Some(5)),
            "Every 2 weeks on Sat at 09:00"
        );
        // A weekday on a daily interval is ignored, never printed.
        assert_eq!(
            describe_schedule(1440, Some(0), Some(3)),
            "Every day at 00:00"
        );
        assert_eq!(weekday_name(0), "Mon");
        assert_eq!(weekday_name(9), "Sun");
    }

    #[test]
    fn presets_are_valid_and_the_default_is_one_of_them() {
        assert!(is_preset(DEFAULT_INTERVAL));
        assert!(!is_preset(90));
        for minutes in PRESETS {
            assert!((1..=MAX_INTERVAL_MINUTES).contains(&minutes));
        }
    }

    #[test]
    fn custom_fields_round_trip_through_minutes() {
        assert_eq!(split_interval(2160), (36, IntervalUnit::Hours));
        assert_eq!(split_interval(2880), (2, IntervalUnit::Days));
        assert_eq!(split_interval(45), (45, IntervalUnit::Minutes));
        for minutes in [1, 45, 90, 2160, 2880, 10080, MAX_INTERVAL_MINUTES] {
            let (value, unit) = split_interval(minutes);
            assert_eq!(custom_interval(&value.to_string(), unit), Ok(minutes));
        }
    }

    #[test]
    fn custom_fields_reject_out_of_range_or_non_numbers() {
        assert!(custom_interval("", IntervalUnit::Hours).is_err());
        assert!(custom_interval("1.5", IntervalUnit::Hours).is_err());
        assert!(custom_interval("0", IntervalUnit::Minutes).is_err());
        assert!(custom_interval("366", IntervalUnit::Days).is_err());
        assert!(custom_interval("99999999", IntervalUnit::Days).is_err());
        assert_eq!(
            custom_interval(" 365 ", IntervalUnit::Days),
            Ok(MAX_INTERVAL_MINUTES)
        );
    }

    #[test]
    fn next_run_is_relative_to_today() {
        let now = NaiveDate::from_ymd_opt(2026, 9, 16)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap();
        let at = |d, h, m| {
            NaiveDate::from_ymd_opt(2026, 9, d)
                .unwrap()
                .and_hms_opt(h, m, 0)
                .unwrap()
        };
        assert_eq!(next_run_words(at(16, 14, 30), now), "Today at 14:30");
        assert_eq!(next_run_words(at(17, 9, 5), now), "Tomorrow at 09:05");
        assert_eq!(next_run_words(at(20, 9, 0), now), "Sep 20 at 09:00");
        let next_year = NaiveDate::from_ymd_opt(2027, 1, 2)
            .unwrap()
            .and_hms_opt(8, 0, 0)
            .unwrap();
        assert_eq!(next_run_words(next_year, now), "Jan 2, 2027 at 08:00");
    }

    #[test]
    fn statuses_read_in_words() {
        assert_eq!(run_status_label("awaitingInput"), "Waiting for your input");
        assert_eq!(run_status_label("succeeded"), "Succeeded");
        assert!(run_status_is_problem("failed"));
        assert!(!run_status_is_problem("running"));
    }

    #[test]
    fn windows_paths_display_with_one_separator_style() {
        assert_eq!(display_path("D:\\/AI-OS"), "D:\\AI-OS");
        assert_eq!(display_path("C:/Users/me/code"), "C:\\Users\\me\\code");
        assert_eq!(display_path("\\\\server\\share//x"), "\\\\server\\share\\x");
        assert_eq!(display_path("/home/me/code"), "/home/me/code");
        assert_eq!(folder_name("D:\\/AI-OS\\"), "AI-OS");
        assert_eq!(folder_name("/home/me/code"), "code");
    }

    #[test]
    fn model_list_leads_with_default_then_flagships() {
        let models = vec![
            model("claude-sonnet-5", "Sonnet 5"),
            model("claude-haiku-4", "Haiku 4"),
            model("claude-opus-5", "Opus 5"),
        ];
        assert_eq!(
            model_items(HarnessId::ClaudeCode, &models, ""),
            vec![
                ModelItem::Default,
                ModelItem::Model(0),
                ModelItem::Model(2),
                ModelItem::Divider,
                ModelItem::Model(1),
            ]
        );
        // Routed catalogs take the newest rows as flagships.
        let routed: Vec<Model> = (0..4)
            .map(|i| model(&format!("openrouter/m{i}"), &format!("M{i}")))
            .collect();
        let items = model_items(HarnessId::Opencode, &routed, "");
        assert_eq!(items[3], ModelItem::Divider);
        assert_eq!(items.len(), 6);
        // Pi lists the user's own provider config in its order: no divider.
        let items = model_items(HarnessId::Pi, &routed, "");
        assert!(!items.contains(&ModelItem::Divider));
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn model_search_ranks_label_prefix_first_and_matches_ids() {
        let models = vec![
            model(
                "openrouter/deepseek/deepseek-chat",
                "openrouter/DeepSeek: V3",
            ),
            model("anthropic/claude-opus-5", "Opus 5"),
            model("openai/gpt-6", "GPT-6"),
        ];
        assert_eq!(
            model_items(HarnessId::Pi, &models, "opus"),
            vec![ModelItem::Model(1)]
        );
        assert_eq!(
            model_items(HarnessId::Pi, &models, "deepseek"),
            vec![ModelItem::Model(0)]
        );
        assert_eq!(
            model_items(HarnessId::Pi, &models, "openai"),
            vec![ModelItem::Model(2)]
        );
        assert!(model_items(HarnessId::Pi, &models, "zzz").is_empty());
    }

    #[test]
    fn the_wizards_choice_lists_read_in_words() {
        assert_eq!(stop_after_label(None), "No limit");
        assert_eq!(stop_after_label(Some(30)), "30 minutes");
        assert_eq!(stop_after_label(Some(1)), "1 minute");
        assert!(STOP_AFTER.contains(&Some(DEFAULT_MAX_RUN_MINUTES)));
        assert!(STOP_AFTER.contains(&None));
        for choice in DELIVER_CHOICES {
            assert!(!deliver_label(choice).is_empty());
        }
        assert_eq!(Deliver::default(), DELIVER_CHOICES[0]);
        assert_eq!(
            deliver_label(Deliver::default()),
            "Inbox + desktop notification"
        );
        assert_eq!(deliver_label(DELIVER_CHOICES[2]), "Nothing");
        assert_eq!(ROLE_SUGGESTIONS.len(), 6);
        for (label, body) in TEMPLATES {
            assert!(!label.is_empty());
            // Every template is a whole instruction, not a fragment.
            assert!(body.len() > 60, "{label} is too short");
            assert!(body.trim_end().ends_with('.'), "{label} lacks a full stop");
        }
    }

    #[test]
    fn manifest_instructions_are_added_once_and_match_the_wire_shape() {
        let text = manifest_instructions(DEFAULT_MANIFEST);
        assert!(text.contains("results/{{run_id}}.json"));
        assert!(text.contains(r#""runId": "{{run_id}}""#));
        assert!(text.contains(r#""type": "file""#));
        let once = with_manifest_instructions("Summarize issues.", DEFAULT_MANIFEST);
        assert!(once.starts_with("Summarize issues.\n\nWhen you are done"));
        assert_eq!(with_manifest_instructions(&once, DEFAULT_MANIFEST), once);
        assert_eq!(with_manifest_instructions("  ", DEFAULT_MANIFEST), text);
        // The example itself must be a valid manifest once the id is filled.
        let json = &text[text.find("{\"runId\"").unwrap()..=text.rfind("}]}").unwrap() + 2];
        let parsed: zeron_proto::ResultManifest =
            serde_json::from_str(&json.replace("{{run_id}}", "run-1")).unwrap();
        assert_eq!(parsed.run_id, "run-1");
        assert_eq!(parsed.links.len(), 1);
    }
}
