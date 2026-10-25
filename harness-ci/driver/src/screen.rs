use agenttail::Harness;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenState {
    Starting,
    Idle,
    Working,
    PermissionPrompt,
    AskUserQuestion,
    Error,
    Unknown,
}

impl ScreenState {
    pub fn id(self) -> &'static str {
        match self {
            ScreenState::Starting => "starting",
            ScreenState::Idle => "idle",
            ScreenState::Working => "working",
            ScreenState::PermissionPrompt => "permission_prompt",
            ScreenState::AskUserQuestion => "ask_user_question",
            ScreenState::Error => "error",
            ScreenState::Unknown => "unknown",
        }
    }
}

pub struct ScreenReading {
    pub state: ScreenState,
    pub background_agents: bool,
}

pub fn read_screen(harness: Harness, screen: &str) -> ScreenReading {
    match harness {
        Harness::ClaudeCode => read_claude(screen),
        Harness::Codex => read_codex(screen),
        Harness::Cursor | Harness::OpenCode => ScreenReading { state: ScreenState::Unknown, background_agents: false },
        _ => ScreenReading { state: ScreenState::Unknown, background_agents: false },
    }
}

fn read_codex(screen: &str) -> ScreenReading {
    let lines = normalized_lines(screen);
    ScreenReading { state: codex_state(&lines), background_agents: false }
}

fn codex_state(lines: &[String]) -> ScreenState {
    if lines.iter().any(|line| line_codex_ask(line)) {
        return ScreenState::AskUserQuestion;
    }
    if lines.iter().any(|line| line_codex_permission(line)) {
        return ScreenState::PermissionPrompt;
    }
    if lines.iter().any(|line| line_codex_error(line)) {
        return ScreenState::Error;
    }
    if lines.iter().any(|line| line_codex_working(line)) {
        return ScreenState::Working;
    }
    if lines.iter().any(|line| line_codex_idle(line)) {
        return ScreenState::Idle;
    }
    if lines.iter().any(|line| line.contains("waiting for startup")) {
        return ScreenState::Starting;
    }
    ScreenState::Unknown
}

fn line_codex_permission(line: &str) -> bool {
    line.contains("would you like to run the following command?")
        || line.contains("would you like to make the following edits?")
        || line.contains("would you like to grant these permissions?")
        || line.contains("would you like to send input to the existing terminal?")
        || (line.contains("press enter to confirm or esc to cancel")
            && (line.contains("yes, proceed")
                || line.contains("yes, just this once")
                || line.contains("no, and tell codex")))
}

fn line_codex_ask(line: &str) -> bool {
    (line.contains("question ") && line.contains("unanswered)"))
        || line.contains("enter to submit answer")
        || line.contains("tab to add notes")
        || line.contains("yes, implement this plan")
        || line.contains("no, stay in plan mode")
}

fn line_codex_error(line: &str) -> bool {
    (line.starts_with('■') && !line.contains("conversation interrupted"))
        || line.starts_with("error:")
        || line.contains("turn failed")
        || line.contains("stream error")
        || line.contains("request failed")
}

fn line_codex_working(line: &str) -> bool {
    line.contains("esc to interrupt")
        || (line.contains("working (") && line.contains('•'))
        || line.contains("waiting for agents")
}

fn line_codex_idle(line: &str) -> bool {
    line.contains("ask codex to do anything")
        || line.contains("ask a follow-up question")
        || line.contains("conversation interrupted")
}

fn read_claude(screen: &str) -> ScreenReading {
    let lines = normalized_lines(screen);
    let background_agents = background_running(&lines);
    let state = claude_state(&lines);
    ScreenReading { state, background_agents }
}

fn claude_state(lines: &[String]) -> ScreenState {
    if let Some(idx) = lowest_permission(lines)
        && !later_settled_footer(lines, idx)
    {
        return ScreenState::PermissionPrompt;
    }
    if let Some(idx) = lines.iter().rposition(|line| line_ask(line))
        && !later_settled_footer(lines, idx)
    {
        return ScreenState::AskUserQuestion;
    }
    if let Some(idx) = lines.iter().rposition(|line| line_error(line))
        && idx + 8 >= lines.len()
        && !lines[idx + 1..].iter().any(|line| line_idle(line))
    {
        return ScreenState::Error;
    }
    if let Some(state) = lines.iter().rev().filter(|line| !line_mode_footer(line)).find_map(|line| line_status(line)) {
        return state;
    }
    if lines.iter().any(|line| line_idle(line)) {
        return ScreenState::Idle;
    }
    if lines.iter().any(|line| line.contains("welcome to claude code")) {
        return ScreenState::Starting;
    }
    ScreenState::Unknown
}

fn lowest_permission(lines: &[String]) -> Option<usize> {
    let mut found = None;
    for (i, line) in lines.iter().enumerate() {
        if line_permission(line) {
            found = Some(i);
        }
    }
    for i in 0..lines.len().saturating_sub(1) {
        let window = format!("{} {}", lines[i], lines[i + 1]);
        if lines[i].chars().count() <= 32
            && lines[i + 1].chars().count() <= 32
            && window.contains("esc to cancel")
            && window.contains("tab to amend")
        {
            found = Some(found.map_or(i + 1, |prev| prev.max(i + 1)));
        }
    }
    found
}

fn line_permission(line: &str) -> bool {
    phrase_near_start(line, "do you want to proceed?")
        || phrase_near_start(line, "do you want to make this edit")
        || (phrase_near_start(line, "esc to cancel") && line.contains("tab to amend"))
}

fn line_ask(line: &str) -> bool {
    if line_interrupt(line) {
        return false;
    }
    phrase_near_start(line, "enter to select")
        || phrase_near_start(line, "type something.")
        || phrase_near_start(line, "would you like to proceed?")
        || phrase_near_start(line, "ready to code?")
        || phrase_near_start(line, "exit plan mode?")
        || phrase_near_start(line, "here is claude's plan:")
        || line.contains("claude has written up a plan and is ready to execute")
}

fn phrase_near_start(line: &str, phrase: &str) -> bool {
    let Some(at) = line.find(phrase) else { return false };
    line[..at].chars().filter(|c| c.is_ascii_alphanumeric()).count() <= 3
}

fn line_error(line: &str) -> bool {
    let api_banner = line.contains("api error:") || (line.contains("api error") && line.contains("retrying"));
    let attempt = line.contains("api error") && attempt_count(line);
    if api_banner || attempt {
        return true;
    }
    let named = line.contains("authentication failed") || line.contains("request failed");
    named && line.chars().count() <= 80
}

fn attempt_count(line: &str) -> bool {
    let Some(at) = line.find("attempt") else { return false };
    line[at + "attempt".len()..].trim_start().starts_with(|c: char| c.is_ascii_digit())
}

fn line_interrupt(line: &str) -> bool {
    line.contains("esc to interrupt") || line.contains("ctrl+c to interrupt")
}

fn later_settled_footer(lines: &[String], idx: usize) -> bool {
    lines[idx + 1..].iter().any(|line| line_interrupt(line) || line_idle(line))
}

fn line_spinner(line: &str) -> bool {
    const VERBS: [&str; 16] = [
        "working…",
        "working...",
        "composing…",
        "composing...",
        "herding…",
        "cooking…",
        "pondering…",
        "pondering...",
        "sautéing…",
        "sauteing…",
        "processing…",
        "processing...",
        "brewing…",
        "churning…",
        "moseying…",
        "crystallizing…",
    ];
    let Some(at) = VERBS.iter().find_map(|verb| {
        line.match_indices(verb)
            .find(|(at, _)| *at == 0 || !line.as_bytes()[at - 1].is_ascii_alphanumeric())
            .map(|(at, _)| at)
    }) else {
        return false;
    };
    line[..at].trim().chars().all(|c| !c.is_ascii_alphanumeric())
}

fn line_working(line: &str) -> bool {
    line_interrupt(line) || line_spinner(line) || (line.contains("retrying in") && line.chars().count() <= 80)
}

fn line_idle(line: &str) -> bool {
    if line_interrupt(line) {
        return false;
    }
    line_mode_footer(line) || line_settled_interrupt(line) || line_settled(line)
}

fn line_mode_footer(line: &str) -> bool {
    if line_interrupt(line) || line_background(line) || line_spinner(line) {
        return false;
    }
    line.contains("? for shortcuts")
        || line.contains("bypass permissions on")
        || line.contains("auto mode on")
        || line.contains("accept edits on")
        || line.contains("plan mode on")
        || line.contains("manual mode on")
}

fn line_settled(line: &str) -> bool {
    let Some(at) = line.find("· done") else { return false };
    line[..at].contains(" for ") && line.chars().count() <= 64
}

fn line_settled_interrupt(line: &str) -> bool {
    line.contains("what should claude do instead") && line.chars().count() <= 96
}

fn line_background(line: &str) -> bool {
    let Some(at) = line.find("waiting for ") else { return false };
    let rest = &line[at + "waiting for ".len()..];
    let digit_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    if digit_end == 0 {
        return false;
    }
    let after_num = rest[digit_end..].trim_start();
    let Some(after_agents) =
        after_num.strip_prefix("background agents").or_else(|| after_num.strip_prefix("background agent"))
    else {
        return false;
    };
    let Some(after_finish) = after_agents.trim_start().strip_prefix("to finish") else { return false };
    let tail = after_finish.trim();
    tail.is_empty() || tail.starts_with('·') || tail.starts_with('•')
}

fn line_status(line: &str) -> Option<ScreenState> {
    if line_background(line) || line_working(line) {
        Some(ScreenState::Working)
    } else if line_idle(line) {
        Some(ScreenState::Idle)
    } else {
        None
    }
}

fn background_running(lines: &[String]) -> bool {
    let Some(idx) = lines.iter().rposition(|line| line_background(line)) else { return false };
    !lines[idx + 1..].iter().any(|line| line_settled(line) || (line_idle(line) && !line_mode_footer(line)))
}

fn normalized_lines(screen: &str) -> Vec<String> {
    screen
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase())
        .filter(|line| !line.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude(screen: &str) -> ScreenReading {
        read_screen(Harness::ClaudeCode, screen)
    }

    #[test]
    fn detects_the_captured_permission_prompt() {
        let screen = r#"
            ╭──────────────────────────────────────────────────────────────╮
            │ Bash command                                                 │
            │   Create a marker file                                       │
            │   touch permission-marker.txt                                │
            │ Do you want to proceed?                                      │
            │ ❯ 1. Yes                                                     │
            │   2. Yes, and always allow access …                          │
            │   3. No                                                      │
            │ Esc to cancel · Tab to amend                                 │
            ╰──────────────────────────────────────────────────────────────╯
        "#;
        let reading = claude(screen);
        assert_eq!(reading.state, ScreenState::PermissionPrompt);
        assert!(!reading.background_agents);
    }

    #[test]
    fn detects_a_captured_edit_permission_prompt() {
        let screen = r#"
            Do you want to make this edit to src/main.rs?
            ❯ 1. Yes
              2. Yes, allow all edits during this session (shift+tab)
              3. No, and tell Claude what to do differently (esc)
        "#;
        assert_eq!(claude(screen).state, ScreenState::PermissionPrompt);
    }

    #[test]
    fn detects_a_captured_permission_prompt_with_a_spinner_still_on_screen() {
        let screen = r#"
            Bash command

            echo "checking sandbox gitconfig"

            Do you want to proceed?
            ❯ 1. Yes
              2. No

            Esc to cancel · Tab to amend · ctrl+e to explain

            ✶ Herding… (53s · ↓ 7.0k tokens)
            Tip: Use /bts to ask a quick side question without interrupting Claude's current work
        "#;
        assert_eq!(claude(screen).state, ScreenState::PermissionPrompt);
    }

    #[test]
    fn detects_a_wrapped_permission_footer() {
        let screen = "Esc to cancel\nTab to amend\n";
        assert_eq!(claude(screen).state, ScreenState::PermissionPrompt);
    }

    #[test]
    fn quoted_permission_text_above_an_idle_footer_is_idle() {
        let screen = r#"
            The notes say Esc to cancel · Tab to amend, which is the old footer.
            ❯
            ? for shortcuts
        "#;
        assert_eq!(claude(screen).state, ScreenState::Idle);
    }

    #[test]
    fn a_quoted_permission_line_with_no_footer_is_not_a_prompt() {
        let screen = "The dialog would have said Do you want to proceed? and then waited.\n";
        assert_eq!(claude(screen).state, ScreenState::Unknown);
    }

    #[test]
    fn quoted_permission_text_above_the_working_footer_is_not_a_prompt() {
        let screen = r#"
            The dialog would have said Do you want to proceed? and then waited.
            ✶ Working… (4s · ↓ 88 tokens)
            esc to interrupt
        "#;
        assert_eq!(claude(screen).state, ScreenState::Working);
    }

    #[test]
    fn detects_the_captured_ask_user_question() {
        let screen = r#"
            How should the encryption key be managed?

            ❯ 1. Require OTARI_SECRET_KEY
              2. Auto-generate KEK to a file
              3. Auto-generate KEK in DB

            Enter to select · ↑/↓ to navigate · n to add notes · Tab to switch questions · Esc to cancel
        "#;
        let reading = claude(screen);
        assert_eq!(reading.state, ScreenState::AskUserQuestion);
        assert!(!reading.background_agents);
    }

    #[test]
    fn detects_a_captured_plan_approval_as_an_ask() {
        let screen = r#"
            Would you like to proceed?
            ❯ 1. Yes, and auto-accept edits
              2. Yes, and manually approve edits
              3. No, keep planning
        "#;
        assert_eq!(claude(screen).state, ScreenState::AskUserQuestion);
    }

    #[test]
    fn detects_an_ask_whose_option_says_type_something() {
        let screen = r#"
            Which database for the new service?
            ❯ PostgreSQL
              SQLite
              Type something.
        "#;
        assert_eq!(claude(screen).state, ScreenState::AskUserQuestion);
    }

    #[test]
    fn quoted_enter_to_select_while_the_spinner_is_up_is_working() {
        let screen = r#"
            The footer reads "Enter to select · ↑/↓ to navigate" while parked.
            ✶ Working… (4s · ↓ 88 tokens)
            esc to interrupt
        "#;
        assert_eq!(claude(screen).state, ScreenState::Working);
    }

    #[test]
    fn an_ask_footer_below_a_spinner_is_still_a_question() {
        let screen = r#"
            ✶ Herding… (12s · ↓ 1234 tokens)
            How should the encryption key be managed?
            ❯ 1. Require OTARI_SECRET_KEY
              2. Auto-generate KEK to a file
            Enter to select · ↑/↓ to navigate · Esc to cancel
        "#;
        assert_eq!(claude(screen).state, ScreenState::AskUserQuestion);
    }

    #[test]
    fn detects_the_captured_working_screen() {
        let screen = r#"
            ❯ run the command
            ✻ Composing…
            ❯
            esc to interrupt · ← for agents
        "#;
        let reading = claude(screen);
        assert_eq!(reading.state, ScreenState::Working);
        assert!(!reading.background_agents);
    }

    #[test]
    fn detects_a_captured_spinner_without_the_interrupt_footer() {
        let screen = "✶ Working… (4s · ↓ 88 tokens)\n";
        assert_eq!(claude(screen).state, ScreenState::Working);
    }

    #[test]
    fn prose_that_says_composing_is_not_a_spinner() {
        let screen = "I am composing… the rest of the reply in this paragraph.\n";
        assert_eq!(claude(screen).state, ScreenState::Unknown);
    }

    #[test]
    fn a_background_status_above_the_bypass_footer_is_working() {
        let screen = "\
● The background agent has started.
✻ Waiting for 1 background agent to finish
❯
⏵⏵ bypass permissions on (shift+tab to cycle)
";
        let reading = claude(screen);
        assert_eq!(reading.state, ScreenState::Working);
        assert!(reading.background_agents);
        let finished = "\
✻ Waiting for 1 background agent to finish
● The background agent finished.
✻ Churned for 10s · done 3:15 PM
❯
⏵⏵ bypass permissions on (shift+tab to cycle)
";
        let reading = claude(finished);
        assert_eq!(reading.state, ScreenState::Idle);
        assert!(!reading.background_agents);
    }

    #[test]
    fn detects_the_captured_background_agent_status_as_working() {
        let screen = "\
✻ Waiting for 1 background agent to finish
❯
◯ general-purpose Summarize tmux module pub fns 19s · ↓ 36.4k tokens";
        let reading = claude(screen);
        assert_eq!(reading.state, ScreenState::Working);
        assert!(reading.background_agents);
    }

    #[test]
    fn prose_about_background_agents_above_the_idle_footer_is_idle() {
        let screen = "\
● Waiting for background agent results before summarizing.
* Waiting for 2 background agents to finish before merging
❯
? for shortcuts · ← for agents";
        let reading = claude(screen);
        assert_eq!(reading.state, ScreenState::Idle);
        assert!(!reading.background_agents);
    }

    #[test]
    fn an_interrupted_composer_is_idle_without_the_shortcuts_footer() {
        let screen = "Interrupted · What should Claude do instead?\n";
        assert_eq!(claude(screen).state, ScreenState::Idle);
        let quoted =
            "The notes say What should Claude do instead? only as a quotation from an old session log that ran long.\n";
        assert_eq!(claude(quoted).state, ScreenState::Unknown);
        let working = "Interrupted · What should Claude do instead?\nesc to interrupt\n";
        assert_eq!(claude(working).state, ScreenState::Working);
    }

    #[test]
    fn detects_the_captured_idle_screen() {
        let screen = r#"
            ⏺ The marker file was created.
            ✻ Worked for 0s · done 9:43 PM

            ❯
            ? for shortcuts · ← for agents
        "#;
        let reading = claude(screen);
        assert_eq!(reading.state, ScreenState::Idle);
        assert!(!reading.background_agents);
    }

    #[test]
    fn detects_the_captured_bypass_idle_screen() {
        let screen = r#"
            ▗ ▗   ▖ ▖  Claude Code v2.1.310
                       Sonnet 4.5 · API Usage Billing
             ▘▘   ▝▝   /private/tmp/work
            ────────────────────────────────────────────────────────────────
            ❯
            ────────────────────────────────────────────────────────────────
              ⏵⏵ bypass permissions on (shift+tab to cycle)
        "#;
        assert_eq!(claude(screen).state, ScreenState::Idle);
    }

    #[test]
    fn a_bypass_status_line_that_can_still_be_interrupted_is_working() {
        let screen = "⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt · ← for agents\n";
        assert_eq!(claude(screen).state, ScreenState::Working);
    }

    #[test]
    fn detects_manual_mode_as_idle() {
        assert_eq!(claude("⏸ manual mode on\n").state, ScreenState::Idle);
    }

    #[test]
    fn detects_auto_mode_as_idle() {
        let screen = "auto mode on (shift+tab to cycle)\n";
        assert_eq!(claude(screen).state, ScreenState::Idle);
    }

    #[test]
    fn accept_edits_and_plan_footers_are_idle_until_they_can_be_interrupted() {
        assert_eq!(claude("⏵⏵ accept edits on (shift+tab to cycle)\n").state, ScreenState::Idle);
        assert_eq!(claude("plan mode on (shift+tab to cycle)\n").state, ScreenState::Idle);
        let working = "accept edits on (shift+tab to cycle) · esc to interrupt\n";
        assert_eq!(claude(working).state, ScreenState::Working);
        let quoted = "The notes say to leave plan mode on while reading.\n❯\n? for shortcuts\n";
        assert_eq!(claude(quoted).state, ScreenState::Idle);
    }

    #[test]
    fn detects_the_plan_exit_dialog() {
        let screen = r#"
            Ready to code?
            Here is Claude's plan:
            No plan found. Please write your plan to the plan file first.
            Claude has written up a plan and is ready to execute. Would you like to proceed?
            ❯ 1. Yes, and auto-accept edits
              2. Yes, and manually approve edits
              3. No, keep planning
        "#;
        assert_eq!(claude(screen).state, ScreenState::AskUserQuestion);
    }

    #[test]
    fn detects_the_captured_exit_plan_mode_dialog() {
        let screen = r#"
            Read 1 file, ran 1 shell command

            Exit plan mode?

              Claude wants to exit plan mode

              ❯ 1. Yes, and switch to default (ask each time) for this session
                2. No
        "#;
        assert_eq!(claude(screen).state, ScreenState::AskUserQuestion);
    }

    #[test]
    fn a_quoted_exit_plan_mode_above_the_idle_footer_is_idle() {
        let screen = "\
Exit plan mode?
The assistant quoted that title and then stopped.
❯
? for shortcuts
";
        assert_eq!(claude(screen).state, ScreenState::Idle);
    }

    #[test]
    fn a_wrapped_plan_question_is_still_an_ask() {
        let screen = "\
│ Claude has written up a plan and is ready to execute.
│ Would you like to proceed?
";
        assert_eq!(claude(screen).state, ScreenState::AskUserQuestion);
    }

    #[test]
    fn a_quoted_plan_question_above_the_idle_footer_is_idle() {
        let screen = "\
The assistant wrote: Claude has written up a plan and is ready to execute.
❯
? for shortcuts
";
        assert_eq!(claude(screen).state, ScreenState::Idle);
    }

    #[test]
    fn a_quoted_plan_question_above_the_working_footer_is_working() {
        let screen = "\
Claude has written up a plan and is ready to execute. Would you like to proceed?
✶ Working… (4s · ↓ 88 tokens)
esc to interrupt
";
        assert_eq!(claude(screen).state, ScreenState::Working);
    }

    #[test]
    fn errors_beat_the_spinner() {
        let screen = r#"
            ❯ run the command
            ✻ API error · Retrying in 1s · attempt 1/10
            esc to interrupt · ← for agents
        "#;
        assert_eq!(claude(screen).state, ScreenState::Error);
    }

    #[test]
    fn detects_an_api_error_banner() {
        let screen = "✕ API Error: 529 overloaded\n";
        assert_eq!(claude(screen).state, ScreenState::Error);
    }

    #[test]
    fn detects_authentication_failed_and_request_failed_banners() {
        assert_eq!(claude("Authentication failed\n").state, ScreenState::Error);
        assert_eq!(claude("Request failed\n").state, ScreenState::Error);
    }

    #[test]
    fn mentioning_an_api_error_in_the_transcript_is_not_a_banner() {
        let screen = r#"
            The API error was transient and the retry succeeded.
            ❯
            ? for shortcuts
        "#;
        assert_eq!(claude(screen).state, ScreenState::Idle);
    }

    #[test]
    fn a_long_sentence_that_says_request_failed_is_not_a_banner() {
        let screen = "The log line said request failed because the socket closed before the headers arrived and we moved on.\n? for shortcuts\n";
        assert_eq!(claude(screen).state, ScreenState::Idle);
    }

    #[test]
    fn an_error_banner_does_not_hide_a_permission_dialog() {
        let screen = r#"
            ✻ API error · Retrying in 1s · attempt 1/10
            Do you want to proceed?
            ❯ 1. Yes
            Esc to cancel · Tab to amend
        "#;
        assert_eq!(claude(screen).state, ScreenState::PermissionPrompt);
    }

    #[test]
    fn detects_the_welcome_screen_as_starting() {
        let screen = "Welcome to Claude Code\n";
        assert_eq!(claude(screen).state, ScreenState::Starting);
    }

    #[test]
    fn an_error_banner_above_the_idle_composer_is_idle() {
        let screen = "✕ API Error: 529 overloaded\n❯\n? for shortcuts\n";
        assert_eq!(claude(screen).state, ScreenState::Idle);
    }

    #[test]
    fn plural_background_agents_count_as_running() {
        let screen = "✶ Waiting for 12 background agents to finish\n";
        let reading = claude(screen);
        assert_eq!(reading.state, ScreenState::Working);
        assert!(reading.background_agents);
    }

    #[test]
    fn a_sentence_about_retrying_is_not_the_working_footer() {
        let screen = "I will keep retrying in the background until the socket comes back, then write the result.\n";
        assert_eq!(claude(screen).state, ScreenState::Unknown);
    }

    #[test]
    fn codex_and_cursor_do_not_reuse_claude_strings() {
        let screen = "Do you want to proceed?\nEsc to cancel · Tab to amend\n";
        assert_eq!(read_screen(Harness::Codex, screen).state, ScreenState::Unknown);
        assert_eq!(read_screen(Harness::Cursor, screen).state, ScreenState::Unknown);
        assert!(!read_screen(Harness::Codex, "✻ Waiting for 1 background agent to finish\n").background_agents);
    }

    #[test]
    fn detects_the_captured_codex_idle_composer_and_working_status() {
        let idle = "  >_ OpenAI Codex (v0.160.1)
     /tmp/work

  To get started, describe a task or try one of these commands:

  /init - create an AGENTS.md file with instructions for Codex
  /status - show current session configuration
  /permissions - choose what Codex is allowed to do
  /model - choose what model and reasoning effort to use
  /review - review any changes and find issues


› say hello


• Hello from the mock.

  Worked for 1s • 23:29


› Ask Codex to do anything

  gpt-5.4 default · /tmp/work · ⠋ · ← for agents                       ⚠ 1 warning · f2 to view
";
        assert_eq!(read_screen(Harness::Codex, idle).state, ScreenState::Idle);

        let working = "› run the command


Working (0s • esc to interrupt)


› Ask Codex to do anything

  gpt-5.4 default · /tmp/work · ⠋ · ← for agents
";
        assert_eq!(read_screen(Harness::Codex, working).state, ScreenState::Working);

        let starting = "Waiting for startup  · esc cancel\n";
        assert_eq!(read_screen(Harness::Codex, starting).state, ScreenState::Starting);
    }

    #[test]
    fn detects_the_captured_codex_approval_prompt() {
        let screen = "› run the command


• Running touch approval-marker.txt


  Would you like to run the following command?

  Environment: local

  Reason: Create a marker file

  $ touch approval-marker.txt


› 1. Yes, proceed (y)
  2. Yes, and don't ask again for commands that start with `touch approval-marker.txt` (p)
  3. No, and tell Codex what to do differently (esc)

  Press enter to confirm or esc to cancel
";
        assert_eq!(read_screen(Harness::Codex, screen).state, ScreenState::PermissionPrompt);
    }

    #[test]
    fn detects_the_captured_codex_request_user_input() {
        let screen = "› ask me which color


  Question 1/1 (1 unanswered)
  Which color should the app use?

  › 1. Blue (Recommended)  Use blue.
    2. Green               Use green.
    3. None of the above   Optionally, add details in notes (tab)

  tab to add notes | enter to submit answer | esc to interrupt
";
        assert_eq!(read_screen(Harness::Codex, screen).state, ScreenState::AskUserQuestion);
    }

    #[test]
    fn detects_the_captured_codex_error_banner() {
        let screen = r#"
› please fail


■ We’re currently experiencing high demand, which may cause temporary errors.


› Ask Codex to do anything

  gpt-5.4 default · /tmp/work
"#;
        assert_eq!(read_screen(Harness::Codex, screen).state, ScreenState::Error);
    }

    #[test]
    fn detects_the_captured_codex_interrupt_as_idle() {
        let screen = "› long answer please


■ Conversation interrupted - use /feedback if something went wrong


› Ask Codex to do anything

  gpt-5.4 default · /tmp/work · ⠋                                     ⚠ 1 warning · f2 to view
";
        assert_eq!(read_screen(Harness::Codex, screen).state, ScreenState::Idle);
    }
}
