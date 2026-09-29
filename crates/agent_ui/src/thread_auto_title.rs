//! OTerminal: short, task-describing titles for Claude Code threads.
//!
//! Titles come from two places:
//!
//! * Claude Code itself. The ACP adapter (`claude-agent-acp`) asks the CLI's
//!   title generator for a title at the end of the first turn and publishes it
//!   as a `session_info_update`. It generates **once** per session and then
//!   re-publishes that same title after every later turn (and after every
//!   restart), so a first turn like "hi" yields a permanent title such as
//!   "OTHSimple repo".
//! * A local heuristic over the user's prompts ([`local_title_from_prompt`]),
//!   used immediately when a prompt is sent, when the adapter's title is poor,
//!   and when the user clearly switches to a new task.
//!
//! [`AutoTitle`] holds the per-thread policy. It is pure (no gpui) so it can be
//! unit tested; `ThreadView` feeds it prompts and title changes and applies the
//! titles it returns. A manual rename (a persisted `title_override`) disables
//! it entirely; that check lives with the callers.

use gpui::SharedString;

/// Longest local title, in characters.
const MAX_TITLE_CHARS: usize = 40;
/// Most words in a local title.
const MAX_TITLE_WORDS: usize = 6;
/// Longest agent title we accept as a "short title"; longer ones are usually
/// the adapter's fallback of the raw first prompt.
const MAX_AGENT_TITLE_CHARS: usize = 60;
const MAX_AGENT_TITLE_WORDS: usize = 9;

/// What to do with the thread's title after it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TitleDecision {
    /// The current title is fine.
    Keep,
    /// Replace the current title (`None` clears it, so the UI shows its
    /// "Claude Code" fallback).
    Set(Option<SharedString>),
}

#[derive(Debug, Default)]
pub(crate) struct AutoTitle {
    /// Title this policy last applied itself, so its own change notification
    /// is recognized and kept.
    applied: Option<SharedString>,
    /// Last title judged good, restored when a poor or stale one arrives.
    last_good: Option<SharedString>,
    /// Whether the title is final with respect to the adapter: an adapter
    /// title was accepted, the user switched task, or the thread was restored
    /// with a good title. The adapter only ever re-sends its one generated
    /// title after that, so a different adapter title is stale.
    settled: bool,
}

impl AutoTitle {
    /// State for a thread that starts with `initial` (e.g. a restored title).
    pub(crate) fn new(initial: Option<&str>, project_names: &[String]) -> Self {
        let mut this = Self::default();
        if let Some(initial) = initial
            && let Some(clean) = clean_agent_title(initial, project_names)
            && clean == initial
        {
            this.last_good = Some(clean.into());
            this.settled = true;
        }
        this
    }

    /// A user prompt was sent. Returns a title to apply now: the first
    /// substantive prompt of a thread without a good title, or a prompt that
    /// clearly starts a new task.
    pub(crate) fn on_user_prompt(
        &mut self,
        prompt: &str,
        current: Option<&str>,
        project_names: &[String],
    ) -> Option<SharedString> {
        let candidate = local_title_from_prompt(prompt)?;
        let current_is_good = current
            .and_then(|current| clean_agent_title(current, project_names))
            .is_some();
        let task_switch = is_task_switch(prompt);
        if current_is_good && !task_switch {
            return None;
        }
        if current == Some(candidate.as_str()) {
            return None;
        }
        let candidate: SharedString = candidate.into();
        self.applied = Some(candidate.clone());
        self.last_good = Some(candidate.clone());
        self.settled = task_switch;
        Some(candidate)
    }

    /// The thread's title changed to `current` (from the adapter, or from us).
    pub(crate) fn on_title_changed(
        &mut self,
        current: Option<&str>,
        project_names: &[String],
    ) -> TitleDecision {
        let Some(current) = current else {
            return TitleDecision::Keep;
        };
        if self.applied.as_deref() == Some(current) || self.last_good.as_deref() == Some(current)
        {
            return TitleDecision::Keep;
        }
        match clean_agent_title(current, project_names) {
            Some(_) if self.settled && self.last_good.is_some() => {
                // A different adapter title after the title settled is the
                // adapter re-publishing its original title.
                TitleDecision::Set(self.last_good.clone())
            }
            Some(clean) => {
                let clean: SharedString = clean.into();
                self.last_good = Some(clean.clone());
                self.settled = true;
                if clean.as_ref() == current {
                    TitleDecision::Keep
                } else {
                    self.applied = Some(clean.clone());
                    TitleDecision::Set(Some(clean))
                }
            }
            None => {
                self.applied = self.last_good.clone();
                TitleDecision::Set(self.last_good.clone())
            }
        }
    }
}

/// Words that say nothing about the task when they are all a title has.
const NON_TASK_WORDS: &[&str] = &[
    "repo",
    "repository",
    "project",
    "codebase",
    "code",
    "base",
    "workspace",
    "folder",
    "the",
    "a",
    "an",
    "this",
    "my",
    "in",
    "on",
    "for",
    "of",
    "with",
    "to",
    "and",
    "help",
    "helping",
    "assistance",
    "assist",
    "session",
    "greeting",
    "greetings",
    "hello",
    "hi",
    "hey",
    "conversation",
    "chat",
    "new",
    "thread",
    "untitled",
    "introduction",
    "intro",
    "question",
    "questions",
    "general",
    "request",
    "task",
    "ready",
    "claude",
    "agent",
];

/// Trailing words that just name the repository.
const REPO_WORDS: &[&str] = &["repo", "repository", "codebase", "project", "workspace"];
/// Connectives left dangling once a trailing repository phrase is removed.
const DANGLING_WORDS: &[&str] = &["in", "on", "for", "of", "the", "to", "at", "from", "within"];
const ARTICLES: &[&str] = &["the", "a", "an"];

fn normalize_word(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

fn is_project_word(word: &str, project_names: &[String]) -> bool {
    let word = normalize_word(word);
    !word.is_empty()
        && project_names
            .iter()
            .any(|name| name.to_lowercase() == word)
}

/// Cleans a title reported by the agent: drops a trailing "in <project> repo"
/// style phrase. Returns `None` when the title says nothing about the task
/// (e.g. "OTHSimple repo", "Greeting") or is really a raw prompt.
pub(crate) fn clean_agent_title(title: &str, project_names: &[String]) -> Option<String> {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = title.trim_end_matches(['.', '!', '?', ':', ';', ',']).trim();
    if title.is_empty()
        || title.chars().count() > MAX_AGENT_TITLE_CHARS
        || title.split_whitespace().count() > MAX_AGENT_TITLE_WORDS
    {
        return None;
    }

    let mut words: Vec<&str> = title.split_whitespace().collect();
    let original_len = words.len();
    let mut stripped_repo = false;
    while let Some(last) = words.last() {
        let normalized = normalize_word(last);
        if REPO_WORDS.contains(&normalized.as_str()) || is_project_word(last, project_names) {
            stripped_repo = true;
            words.pop();
        } else {
            break;
        }
    }
    if stripped_repo {
        while let Some(last) = words.last() {
            if DANGLING_WORDS.contains(&normalize_word(last).as_str()) {
                words.pop();
            } else {
                break;
            }
        }
    }

    let has_task_word = words.iter().any(|word| {
        let normalized = normalize_word(word);
        !normalized.is_empty()
            && !NON_TASK_WORDS.contains(&normalized.as_str())
            && !is_project_word(word, project_names)
    });
    if !has_task_word {
        return None;
    }
    if words.len() == original_len {
        return Some(title.to_string());
    }
    Some(capitalize_first(&words.join(" ")))
}

/// Leading phrases that carry no task information, lowercase. Longer phrases
/// come first so they win over their prefixes.
const FILLER_PREFIXES: &[&str] = &[
    "i would like you to",
    "i'd like you to",
    "id like you to",
    "i want you to",
    "i need you to",
    "i would like to",
    "i'd like to",
    "can you please",
    "could you please",
    "would you please",
    "will you please",
    "can you help me",
    "could you help me",
    "can you",
    "could you",
    "would you",
    "will you",
    "please help me",
    "help me to",
    "help me",
    "go ahead and",
    "we need to",
    "we should",
    "you should",
    "i need to",
    "i want to",
    "try to",
    "let's",
    "lets",
    "let us",
    "new task",
    "next task",
    "different task",
    "unrelated",
    "moving on",
    "switching gears",
    "now",
    "next",
    "then",
    "also",
    "so",
    "ok",
    "okay",
    "alright",
    "hey claude",
    "hi claude",
    "hello claude",
    "hey there",
    "hi there",
    "hello there",
    "hey",
    "hi",
    "hello",
    "claude",
    "please",
    "pls",
    "plz",
    "kindly",
    "just",
    "quickly",
];

/// Whole sentences that are pleasantries, not tasks.
const GREETING_WORDS: &[&str] = &[
    "hi", "hey", "hello", "yo", "sup", "thanks", "thank", "you", "thx", "ty", "ok", "okay",
    "cool", "great", "nice", "claude", "there", "good", "morning", "afternoon", "evening", "how",
    "are", "is", "it", "going", "what's", "whats", "up", "please", "continue", "go", "on", "yes",
    "no", "sure", "done", "test", "testing", "looks", "look", "lgtm", "perfect", "awesome", "works",
    "working", "fine", "all", "that", "this",
];

/// Clause boundaries after which a prompt stops being the imperative task.
const CLAUSE_BREAKS: &[&str] = &[
    " because ",
    " so that ",
    " since ",
    " which ",
    " but ",
    " when ",
    " where ",
    " and then ",
    " - ",
    " \u{2014} ",
    " \u{2013} ",
    ", ",
    "; ",
    ": ",
    " (",
];

/// Trailing phrases that carry no task information, lowercase.
const FILLER_SUFFIXES: &[&str] = &["please", "for me", "thanks", "thank you", "asap", "pls"];

/// Derives a short title from a user prompt, or `None` when the prompt is not
/// a task (a greeting, "thanks", ...). Takes the first sentence that states a
/// task, strips filler ("can you please", "hi claude"), keeps the leading
/// imperative clause, and limits it to ~40 characters / 6 words.
pub(crate) fn local_title_from_prompt(prompt: &str) -> Option<String> {
    let text = prompt
        .lines()
        .filter(|line| !line.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n");
    for sentence in split_sentences(&text) {
        if let Some(title) = title_from_sentence(&sentence) {
            return Some(title);
        }
    }
    None
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (ix, &c) in chars.iter().enumerate() {
        let next = chars.get(ix + 1).copied();
        let ends_sentence = c == '\n'
            || c == '!'
            || c == '?'
            || (c == '.' && next.is_none_or(char::is_whitespace));
        if ends_sentence {
            if !current.trim().is_empty() {
                sentences.push(current.trim().to_string());
            }
            current.clear();
        } else {
            current.push(c);
        }
    }
    if !current.trim().is_empty() {
        sentences.push(current.trim().to_string());
    }
    sentences
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest = &text[prefix.len()..];
    // Only strip whole words.
    if rest
        .chars()
        .next()
        .is_some_and(|c| !c.is_whitespace() && !",:;!?".contains(c))
    {
        return None;
    }
    Some(rest)
}

fn title_from_sentence(sentence: &str) -> Option<String> {
    // Mentions render as "@file.rs"; keep the name.
    let sentence = sentence
        .split_whitespace()
        .map(|word| word.trim_start_matches('@'))
        .collect::<Vec<_>>()
        .join(" ");

    let mut text = sentence.as_str();
    loop {
        let trimmed = text.trim_start_matches(|c: char| c.is_whitespace() || ",:;-".contains(c));
        let stripped = FILLER_PREFIXES
            .iter()
            .find_map(|prefix| strip_prefix_ci(trimmed, prefix));
        match stripped {
            Some(rest) => text = rest,
            None => {
                text = trimmed;
                break;
            }
        }
    }

    // Keep the leading clause, as long as it still says something.
    let mut clause = text.to_string();
    let lower = clause.to_lowercase();
    if let Some(cut) = CLAUSE_BREAKS
        .iter()
        .filter_map(|brk| lower.find(brk))
        .filter(|&ix| lower[..ix].split_whitespace().count() >= 2)
        .min()
    {
        clause.truncate(cut);
    }

    let mut words: Vec<String> = clause
        .split_whitespace()
        .map(|word| word.to_string())
        .collect();
    loop {
        let joined = words.join(" ").to_lowercase();
        let Some(suffix) = FILLER_SUFFIXES
            .iter()
            .find(|suffix| joined.trim_end_matches(['.', '!', '?', ',']).ends_with(*suffix))
        else {
            break;
        };
        let count = suffix.split_whitespace().count();
        if count >= words.len() {
            break;
        }
        words.truncate(words.len() - count);
    }

    let is_greeting = words
        .iter()
        .all(|word| GREETING_WORDS.contains(&normalize_word(word).as_str()));
    let meaningful = words
        .iter()
        .filter(|word| normalize_word(word).chars().count() >= 2)
        .count();
    if words.is_empty() || is_greeting || meaningful < 2 {
        return None;
    }

    // Titles read like "Fix login redirect": drop articles after the verb.
    let words: Vec<String> = words
        .into_iter()
        .enumerate()
        .filter(|(ix, word)| *ix == 0 || !ARTICLES.contains(&normalize_word(word).as_str()))
        .map(|(_, word)| word)
        .collect();

    let mut title_words: Vec<&str> = Vec::new();
    let mut title_len = 0;
    for word in words.iter().take(MAX_TITLE_WORDS) {
        let word_len = word.chars().count();
        let added = if title_words.is_empty() {
            word_len
        } else {
            word_len + 1
        };
        if !title_words.is_empty() && title_len + added > MAX_TITLE_CHARS {
            break;
        }
        title_words.push(word);
        title_len += added;
    }
    while title_words.len() > 1
        && title_words.last().is_some_and(|word| {
            let word = normalize_word(word);
            DANGLING_WORDS.contains(&word.as_str()) || matches!(word.as_str(), "and" | "or" | "with")
        })
    {
        title_words.pop();
    }
    let mut title = title_words.join(" ");
    if title.chars().count() > MAX_TITLE_CHARS {
        title = title.chars().take(MAX_TITLE_CHARS).collect();
    }
    let title = title
        .trim_end_matches(|c: char| !c.is_alphanumeric() && c != ')' && c != '"' && c != '\'')
        .to_string();
    if title.is_empty() {
        return None;
    }
    Some(capitalize_first(&title))
}

fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Cues that a prompt starts a different task, lowercase.
const TASK_SWITCH_PREFIXES: &[&str] = &[
    "new task",
    "next task",
    "different task",
    "another task",
    "unrelated",
    "switching gears",
    "switch gears",
    "moving on",
    "separate question",
    "different question",
    "forget that",
    "forget about that",
    "forget the previous",
    "now let's",
    "now lets",
    "now can you",
    "now could you",
    "now please",
    "next, ",
    "next: ",
    "next let's",
    "next lets",
];

/// Whether `prompt` clearly starts a different task.
pub(crate) fn is_task_switch(prompt: &str) -> bool {
    let lower = prompt.trim_start().to_lowercase();
    TASK_SWITCH_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["OTHSimple".to_string()]
    }

    #[test]
    fn local_title_strips_filler_and_takes_imperative_clause() {
        assert_eq!(
            local_title_from_prompt("Hi Claude, can you please fix the login redirect?").as_deref(),
            Some("Fix login redirect")
        );
        assert_eq!(
            local_title_from_prompt(
                "I want you to add dark mode to the settings page because users asked for it"
            )
            .as_deref(),
            Some("Add dark mode to settings page")
        );
        assert_eq!(
            local_title_from_prompt("hello\nrefactor @src/main.rs, it is too long").as_deref(),
            Some("Refactor src/main.rs")
        );
    }

    #[test]
    fn local_title_is_short() {
        let title = local_title_from_prompt(
            "Investigate why the websocket reconnect logic in the collaboration client drops messages",
        )
        .unwrap();
        assert!(title.chars().count() <= MAX_TITLE_CHARS, "{title}");
        assert!(title.split_whitespace().count() <= MAX_TITLE_WORDS, "{title}");
        assert_eq!(title, "Investigate why websocket reconnect");
    }

    #[test]
    fn local_title_ignores_greetings() {
        assert_eq!(local_title_from_prompt("hi"), None);
        assert_eq!(local_title_from_prompt("Hello Claude!"), None);
        assert_eq!(local_title_from_prompt("thanks, looks good"), None);
        assert_eq!(local_title_from_prompt("   "), None);
    }

    #[test]
    fn agent_titles_naming_only_the_repo_are_poor() {
        assert_eq!(clean_agent_title("OTHSimple repo", &names()), None);
        assert_eq!(clean_agent_title("Greeting", &names()), None);
        assert_eq!(
            clean_agent_title("Fix login redirect in OTHSimple repo", &names()).as_deref(),
            Some("Fix login redirect")
        );
        assert_eq!(
            clean_agent_title("Fix login redirect", &names()).as_deref(),
            Some("Fix login redirect")
        );
        let raw_prompt = "please look at the whole codebase and figure out why the build is \
            failing on windows when cargo check runs";
        assert_eq!(clean_agent_title(raw_prompt, &names()), None);
    }

    #[test]
    fn adapter_title_upgrades_local_title() {
        let mut auto = AutoTitle::new(None, &names());
        let local = auto.on_user_prompt("can you fix the login redirect", None, &names());
        assert_eq!(local.as_deref(), Some("Fix login redirect"));
        assert_eq!(
            auto.on_title_changed(local.as_deref(), &names()),
            TitleDecision::Keep
        );
        assert_eq!(
            auto.on_title_changed(Some("Fix login redirect loop"), &names()),
            TitleDecision::Keep
        );
        // The adapter re-sending an older title later doesn't win over it.
        assert_eq!(
            auto.on_title_changed(Some("Something else"), &names()),
            TitleDecision::Set(Some("Fix login redirect loop".into()))
        );
    }

    #[test]
    fn poor_adapter_title_after_greeting_is_replaced_by_later_prompt() {
        let mut auto = AutoTitle::new(None, &names());
        assert_eq!(auto.on_user_prompt("hi", None, &names()), None);
        // Adapter titles the greeting turn.
        assert_eq!(
            auto.on_title_changed(Some("OTHSimple repo"), &names()),
            TitleDecision::Set(None)
        );
        let title = auto.on_user_prompt("add a dark mode toggle", None, &names());
        assert_eq!(title.as_deref(), Some("Add dark mode toggle"));
        assert_eq!(
            auto.on_title_changed(title.as_deref(), &names()),
            TitleDecision::Keep
        );
        // The adapter re-publishes its poor title after each turn.
        assert_eq!(
            auto.on_title_changed(Some("OTHSimple repo"), &names()),
            TitleDecision::Set(Some("Add dark mode toggle".into()))
        );
    }

    #[test]
    fn task_switch_retitles_but_ordinary_follow_ups_do_not() {
        let mut auto = AutoTitle::new(Some("Fix login redirect"), &names());
        assert_eq!(
            auto.on_user_prompt(
                "also check the tests",
                Some("Fix login redirect"),
                &names()
            ),
            None
        );
        let switched = auto.on_user_prompt(
            "New task: update the README badges",
            Some("Fix login redirect"),
            &names(),
        );
        assert_eq!(switched.as_deref(), Some("Update README badges"));
        // The adapter's stale original title doesn't undo the switch.
        assert_eq!(
            auto.on_title_changed(Some("Fix login redirect"), &names()),
            TitleDecision::Set(Some("Update README badges".into()))
        );
    }

    #[test]
    fn restored_title_is_kept_against_stale_adapter_title() {
        let mut auto = AutoTitle::new(Some("Add dark mode toggle"), &names());
        assert_eq!(
            auto.on_title_changed(Some("OTHSimple repo"), &names()),
            TitleDecision::Set(Some("Add dark mode toggle".into()))
        );
    }
}
