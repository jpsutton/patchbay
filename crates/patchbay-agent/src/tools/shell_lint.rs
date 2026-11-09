//! Checks a Bash command for two mistakes that models make again and again, before it runs:
//!
//! - A process search that finds itself: `ps aux | grep foo` lists the `grep foo` process, and
//!   `pgrep -f foo` or `pkill -f foo` matches the shell that runs the command (its command line
//!   holds `foo` too). Putting square brackets around the last character of the pattern fixes
//!   it, because `[o]` matches `o` but the text `[o]` is not `o`.
//! - A long fixed `sleep` before a check on slow work. The model should run the work in the
//!   background and wait with `BashOutput`, or poll a condition in a bounded loop.
//!
//! A command with a problem is refused with a message that says how to fix it; nothing runs.
//! The check is a small tokenizer, not a shell parser, and it only flags what it is sure of: when
//! it cannot read a command (a heredoc, a pattern it cannot compile), it lets the command run.

use regex::RegexBuilder;

use crate::prompts::Prompts;

/// The longest `sleep` allowed outside a loop, in seconds.
pub const MAX_SLEEP_SECS: u64 = 30;
/// The longest `sleep` allowed in a poll loop (`while`, `until`, `for`), in seconds.
pub const MAX_LOOP_SLEEP_SECS: u64 = 120;
/// How deep `bash -c '...'` inside `bash -c '...'` is read.
const MAX_DEPTH: u32 = 3;

#[derive(Debug, PartialEq, Eq)]
pub enum Problem {
    /// A search pattern that matches its own command line; `fixed` is the pattern with brackets.
    SelfMatch { pattern: String, fixed: String },
    /// A fixed sleep longer than allowed.
    LongSleep { secs: u64 },
}

impl Problem {
    /// What the model reads instead of the command's output.
    pub fn message(&self, p: Prompts) -> String {
        match self {
            Problem::SelfMatch { pattern, fixed } => {
                p.msg("bash_self_match", &[("pattern", pattern.as_str()), ("fixed", fixed.as_str())])
            }
            Problem::LongSleep { secs } => {
                let secs = secs.to_string();
                let max = MAX_SLEEP_SECS.to_string();
                p.msg("bash_long_sleep", &[("secs", secs.as_str()), ("max", max.as_str())])
            }
        }
    }
}

/// The first problem in `command`, if any. A command that runs in the background may sleep.
pub fn check(command: &str, background: bool) -> Option<Problem> {
    check_script(command, background, 0)
}

fn check_script(script: &str, background: bool, depth: u32) -> Option<Problem> {
    // A heredoc body is text, not commands, and the tokenizer does not skip it.
    if depth > MAX_DEPTH || script.contains("<<") {
        return None;
    }
    let pipelines = parse(script);
    let in_loop = pipelines
        .iter()
        .flatten()
        .any(|stage| stage.first().is_some_and(|w| matches!(w.as_str(), "while" | "until" | "for")));
    for stages in &pipelines {
        let mut saw_ps = false;
        for (at, stage) in stages.iter().enumerate() {
            let words = head(stage);
            let Some(first) = words.first() else { continue };
            let cmd = name(first);
            if cmd == "ps" {
                saw_ps = true;
            }
            let found = match cmd {
                "sleep" if !background => long_sleep(&words[1..], in_loop),
                "pgrep" | "pkill" => pgrep_problem(&words[1..]),
                c if is_grep(c) && saw_ps => grep_problem(c, &words[1..], stages, at),
                _ => None,
            };
            if found.is_some() {
                return found;
            }
            if let Some(p) = nested(words, background, depth) {
                return Some(p);
            }
        }
    }
    None
}

// --- tokenizer ---

/// The commands of a pipeline, each as its words.
type Pipeline = Vec<Vec<String>>;

#[derive(Default)]
struct Parts {
    pipelines: Vec<Pipeline>,
    stages: Pipeline,
    words: Vec<String>,
    cur: String,
    in_word: bool,
}

impl Parts {
    fn push(&mut self, c: char) {
        self.cur.push(c);
        self.in_word = true;
    }

    fn end_word(&mut self) {
        if self.in_word {
            self.words.push(std::mem::take(&mut self.cur));
            self.in_word = false;
        }
    }

    /// A `|`: the next command reads this one's output.
    fn end_stage(&mut self) {
        self.end_word();
        if !self.words.is_empty() {
            self.stages.push(std::mem::take(&mut self.words));
        }
    }

    /// `;`, `&&`, `||`, `&`, a newline, a parenthesis or a backtick: a new, separate command.
    fn end_pipeline(&mut self) {
        self.end_stage();
        if !self.stages.is_empty() {
            self.pipelines.push(std::mem::take(&mut self.stages));
        }
    }
}

/// Split `script` into pipelines of commands of words, with quotes removed. `$(` and `(` start a
/// new command, so a command substitution reads as a command of its own.
fn parse(script: &str) -> Vec<Pipeline> {
    let s: Vec<char> = script.chars().collect();
    let mut p = Parts::default();
    let mut i = 0;
    while i < s.len() {
        let ch = s[i];
        let next = s.get(i + 1).copied();
        match ch {
            '\'' => {
                p.in_word = true;
                i += 1;
                while i < s.len() && s[i] != '\'' {
                    p.cur.push(s[i]);
                    i += 1;
                }
            }
            '"' => {
                p.in_word = true;
                i += 1;
                while i < s.len() && s[i] != '"' {
                    if s[i] == '\\' && matches!(s.get(i + 1).copied(), Some('"' | '\\' | '$' | '`')) {
                        i += 1;
                    }
                    p.cur.push(s[i]);
                    i += 1;
                }
            }
            '\\' => {
                if let Some(n) = next {
                    // A backslash before a newline joins the lines.
                    if n != '\n' {
                        p.push(n);
                    }
                    i += 1;
                }
            }
            '#' if !p.in_word => {
                while i < s.len() && s[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '\n' | ';' | '`' | '(' | ')' => p.end_pipeline(),
            '$' if next == Some('(') => {
                p.end_pipeline();
                i += 1;
            }
            '|' => {
                if next == Some('|') {
                    p.end_pipeline();
                    i += 1;
                } else {
                    p.end_stage();
                    // `|&` pipes stderr too.
                    if next == Some('&') {
                        i += 1;
                    }
                }
            }
            '&' => {
                let prev = if i > 0 { s[i - 1] } else { ' ' };
                if prev == '>' || prev == '<' || next == Some('>') {
                    // `2>&1`, `&>file`.
                    p.push('&');
                } else {
                    p.end_pipeline();
                    if next == Some('&') {
                        i += 1;
                    }
                }
            }
            w if w.is_whitespace() => p.end_word(),
            other => p.push(other),
        }
        i += 1;
    }
    p.end_pipeline();
    p.pipelines
}

/// Words that come before the command name: shell keywords, wrappers and `VAR=value`.
const PREFIXES: &[&str] = &[
    "do", "then", "else", "elif", "if", "while", "until", "!", "{", "}", "time", "exec", "command", "sudo", "nohup",
];

fn is_assignment(w: &str) -> bool {
    match w.find('=') {
        Some(i) if i > 0 => w[..i].chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        _ => false,
    }
}

/// The command of a stage: its words without the prefixes.
fn head(stage: &[String]) -> &[String] {
    let skip = stage.iter().take_while(|w| PREFIXES.contains(&w.as_str()) || is_assignment(w.as_str())).count();
    &stage[skip..]
}

/// The program's name without its directory.
fn name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Look inside `bash -c '...'` (also behind `timeout 600`, `sudo` and the like).
fn nested(words: &[String], background: bool, depth: u32) -> Option<Problem> {
    for (i, w) in words.iter().enumerate() {
        if !matches!(name(w), "bash" | "sh" | "zsh" | "dash") {
            continue;
        }
        if let (Some(flag), Some(body)) = (words.get(i + 1), words.get(i + 2)) {
            if flag.starts_with('-') && !flag.starts_with("--") && flag.contains('c') {
                return check_script(body, background, depth + 1);
            }
        }
    }
    None
}

// --- sleep ---

/// The seconds `sleep` waits for these arguments (`5`, `1.5`, `2m`, `1h 30m`); `None` when one is
/// not a plain number, such as `$DELAY`.
fn sleep_secs(args: &[String]) -> Option<u64> {
    let mut total = 0.0_f64;
    let mut any = false;
    for a in args {
        if a.starts_with('-') {
            continue;
        }
        let (num, mult) = match a.chars().last()? {
            's' => (&a[..a.len() - 1], 1.0),
            'm' => (&a[..a.len() - 1], 60.0),
            'h' => (&a[..a.len() - 1], 3600.0),
            'd' => (&a[..a.len() - 1], 86400.0),
            _ => (a.as_str(), 1.0),
        };
        let n: f64 = num.parse().ok()?;
        total += n * mult;
        any = true;
    }
    if any { Some(total.ceil() as u64) } else { None }
}

fn long_sleep(args: &[String], in_loop: bool) -> Option<Problem> {
    let secs = sleep_secs(args)?;
    let limit = if in_loop { MAX_LOOP_SLEEP_SECS } else { MAX_SLEEP_SECS };
    if secs > limit { Some(Problem::LongSleep { secs }) } else { None }
}

// --- process search ---

struct Search {
    pattern: String,
    /// A fixed string (`grep -F`): brackets do not help, so it is left alone.
    fixed: bool,
    icase: bool,
    /// `grep -v`: it keeps the lines that do not match.
    invert: bool,
}

fn is_grep(cmd: &str) -> bool {
    matches!(cmd, "grep" | "egrep" | "fgrep" | "rg")
}

/// The pattern and flags of a grep; `None` when the pattern comes from a file or is missing.
fn parse_grep(cmd: &str, args: &[String]) -> Option<Search> {
    let mut s = Search { pattern: String::new(), fixed: cmd == "fgrep", icase: false, invert: false };
    let mut pattern: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            pattern = it.next().cloned();
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            match long {
                "ignore-case" => s.icase = true,
                "fixed-strings" => s.fixed = true,
                "invert-match" => s.invert = true,
                "regexp" => {
                    pattern = it.next().cloned();
                    break;
                }
                "file" => return None,
                "max-count" | "after-context" | "before-context" | "context" | "include" | "exclude" => {
                    it.next();
                }
                _ => {
                    if let Some(p) = long.strip_prefix("regexp=") {
                        pattern = Some(p.to_owned());
                        break;
                    }
                    if long.starts_with("file=") {
                        return None;
                    }
                }
            }
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            let flags = &a[1..];
            if flags.contains('f') {
                return None;
            }
            s.icase |= flags.contains('i');
            s.fixed |= flags.contains('F');
            s.invert |= flags.contains('v');
            if flags.ends_with('e') {
                pattern = it.next().cloned();
                break;
            }
            if matches!(flags, "A" | "B" | "C" | "m" | "d" | "D") {
                it.next();
            }
            continue;
        }
        pattern = Some(a.clone());
        break;
    }
    s.pattern = pattern?;
    Some(s)
}

/// `grep -v grep` (or `[g]rep`): it drops the grep process from the list.
fn is_grep_filter(stage: &[String]) -> bool {
    let words = head(stage);
    let Some(first) = words.first() else { return false };
    let cmd = name(first);
    is_grep(cmd)
        && parse_grep(cmd, &words[1..]).is_some_and(|f| f.invert && (f.pattern.contains("grep") || f.pattern.contains("[g]rep")))
}

/// `ps ... | grep PATTERN` where the pattern matches the grep's own line in the `ps` output.
fn grep_problem(cmd: &str, args: &[String], stages: &[Vec<String>], at: usize) -> Option<Problem> {
    let s = parse_grep(cmd, args)?;
    if s.invert {
        return None;
    }
    if stages.iter().enumerate().any(|(k, st)| k != at && is_grep_filter(st)) {
        return None;
    }
    self_match(&s)
}

/// `pgrep -f PATTERN` and `pkill -f PATTERN`: `-f` reads whole command lines, and the shell that
/// runs the command has this one in its own.
fn pgrep_problem(args: &[String]) -> Option<Problem> {
    let mut s = Search { pattern: String::new(), fixed: false, icase: false, invert: false };
    let mut full = false;
    let mut pattern: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(long) = a.strip_prefix("--") {
            match long {
                "full" => full = true,
                "ignore-case" => s.icase = true,
                "euid" | "uid" | "pgroup" | "group" | "parent" | "session" | "terminal" | "delimiter" | "pidfile" | "signal" => {
                    it.next();
                }
                _ => {}
            }
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            let flags = &a[1..];
            full |= flags.contains('f');
            s.icase |= flags.contains('i');
            if matches!(flags, "u" | "U" | "g" | "G" | "P" | "s" | "t" | "d" | "F" | "O") {
                it.next();
            }
            continue;
        }
        pattern = Some(a.clone());
        break;
    }
    if !full {
        return None;
    }
    s.pattern = pattern?;
    self_match(&s)
}

/// A problem when the pattern matches the text of its own command line and brackets can fix it.
fn self_match(s: &Search) -> Option<Problem> {
    if s.fixed || s.invert || s.pattern.is_empty() {
        return None;
    }
    // Leave a pattern the `regex` crate cannot read (a backreference, say) alone.
    let re = RegexBuilder::new(&s.pattern).case_insensitive(s.icase).build().ok()?;
    if !re.is_match(&format!("x {}", s.pattern)) {
        return None;
    }
    let fixed = bracket_last(&s.pattern)?;
    Some(Problem::SelfMatch { pattern: s.pattern.clone(), fixed })
}

/// The pattern with square brackets around the last character of each `|` alternative; `None` when
/// an alternative ends in something that brackets would change (`*`, `)`, `\`).
fn bracket_last(pattern: &str) -> Option<String> {
    let parts: Option<Vec<String>> = pattern
        .split('|')
        .map(|alt| {
            let last = alt.chars().last()?;
            if !(last.is_ascii_alphanumeric() || matches!(last, '_' | '-' | '.' | '/' | ':')) {
                return None;
            }
            let cut = alt.len() - last.len_utf8();
            Some(format!("{}[{}]", &alt[..cut], last))
        })
        .collect();
    Some(parts?.join("|"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bad(cmd: &str) -> Option<Problem> {
        check(cmd, false)
    }

    fn self_match_of(pattern: &str, fixed: &str) -> Option<Problem> {
        Some(Problem::SelfMatch { pattern: pattern.into(), fixed: fixed.into() })
    }

    fn sleep_of(secs: u64) -> Option<Problem> {
        Some(Problem::LongSleep { secs })
    }

    #[test]
    fn flags_a_ps_grep_that_finds_itself() {
        assert_eq!(bad("ps aux | grep foo"), self_match_of("foo", "fo[o]"));
        assert_eq!(bad("ps -ef | grep -i 'my server'"), self_match_of("my server", "my serve[r]"));
        assert_eq!(bad("ps aux | grep -E 'foo|bar'"), self_match_of("foo|bar", "fo[o]|ba[r]"));
        assert_eq!(bad("ps aux | grep -e foo"), self_match_of("foo", "fo[o]"));
        assert_eq!(bad("ps aux | sort | grep python3"), self_match_of("python3", "python[3]"));
        assert_eq!(bad("cd /tmp && ps aux | grep foo | wc -l"), self_match_of("foo", "fo[o]"));
    }

    #[test]
    fn leaves_a_ps_grep_that_cannot_find_itself() {
        assert_eq!(bad("ps aux | grep '[f]oo'"), None);
        assert_eq!(bad("ps aux | grep foo | grep -v grep"), None);
        assert_eq!(bad("ps aux | grep -v grep | grep foo"), None);
        assert_eq!(bad("ps aux | grep -F foo"), None);
        assert_eq!(bad("ps aux | grep '^root'"), None);
        assert_eq!(bad("ps aux | grep 'foo[0-9]'"), None);
        assert_eq!(bad("echo foo | grep foo"), None);
        assert_eq!(bad("grep foo file.txt"), None);
    }

    #[test]
    fn flags_pgrep_and_pkill_with_full_command_lines() {
        assert_eq!(bad("pgrep -f foo"), self_match_of("foo", "fo[o]"));
        assert_eq!(bad("pkill -f 'cargo build'"), self_match_of("cargo build", "cargo buil[d]"));
        assert_eq!(bad("pgrep -fl -u root foo"), self_match_of("foo", "fo[o]"));
        assert_eq!(bad("pgrep --full foo"), self_match_of("foo", "fo[o]"));
        assert_eq!(bad("kill $(pgrep -f foo)"), self_match_of("foo", "fo[o]"));
        assert_eq!(bad("until pgrep -f foo; do sleep 1; done"), self_match_of("foo", "fo[o]"));
    }

    #[test]
    fn leaves_pgrep_that_cannot_find_itself() {
        assert_eq!(bad("pgrep foo"), None);
        assert_eq!(bad("pgrep -f '[f]oo'"), None);
        assert_eq!(bad("pgrep -x foo"), None);
        assert_eq!(bad("pgrep -f 'foo)'"), None);
    }

    #[test]
    fn flags_a_long_fixed_sleep() {
        assert_eq!(bad("sleep 100; gh pr checks 1"), sleep_of(100));
        assert_eq!(bad("sleep 2m && ls"), sleep_of(120));
        assert_eq!(bad("sleep 1h"), sleep_of(3600));
        assert_eq!(bad("sleep 31"), sleep_of(31));
        assert_eq!(bad("bash -c 'sleep 100'"), sleep_of(100));
        assert_eq!(bad("cargo build 2>&1 | tail -3; sleep 45"), sleep_of(45));
        assert_eq!(bad("while true; do sleep 300; done"), sleep_of(300));
    }

    #[test]
    fn leaves_a_short_sleep_and_a_poll_loop() {
        assert_eq!(bad("sleep 30"), None);
        assert_eq!(bad("sleep 5"), None);
        assert_eq!(bad("sleep 1.5"), None);
        assert_eq!(bad("sleep $DELAY"), None);
        assert_eq!(bad("while true; do sleep 60; done"), None);
        assert_eq!(bad("timeout 600 bash -c 'until test -f x; do sleep 10; done'"), None);
        assert_eq!(bad("echo 'sleep 100'"), None);
        assert_eq!(bad("ls # sleep 100"), None);
        assert_eq!(bad("cat <<EOF\nsleep 100\nEOF"), None);
    }

    #[test]
    fn a_background_command_may_sleep() {
        assert_eq!(check("sleep 100", true), None);
        // A background command still must not search for itself.
        assert_eq!(check("pgrep -f foo", true), self_match_of("foo", "fo[o]"));
    }

    #[test]
    fn the_messages_name_the_fix() {
        let m = Problem::SelfMatch { pattern: "foo".into(), fixed: "fo[o]".into() }.message(Prompts::new(false));
        assert!(m.contains("`foo`") && m.contains("`fo[o]`"), "{m}");
        for minify in [false, true] {
            let m = Problem::LongSleep { secs: 100 }.message(Prompts::new(minify));
            assert!(m.contains("`sleep 100`") && m.contains("wait_seconds") && m.contains("30"), "{m}");
        }
    }
}
