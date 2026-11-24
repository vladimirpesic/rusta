//! Which shell commands are dangerous enough to ask about in every
//! permission mode but the bypass one.
//!
//! A command is split into its simple commands — across `;`, `&&`, `||`,
//! `|`, `&`, newlines, groupings and command substitutions — and each is
//! judged on its literal words. A word that expands at run time (`$VAR`, a
//! glob, `~`, a command substitution) is not literal: a command whose name is
//! not literal cannot be judged, and neither can an `rm -rf` whose operands
//! are not all literal.

/// The judgement on a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing dangerous found.
    Safe,
    /// A dangerous command, named for the approval prompt (`rm -rf`,
    /// `shutdown`, `dd`, …).
    Dangerous(String),
    /// The command could not be judged: unbalanced quoting, or a command name
    /// that is only known at run time.
    Unanalyzable,
}

/// Commands dangerous in themselves.
const SIMPLE_DANGEROUS: &[&str] = &[
    "shutdown",
    "halt",
    "poweroff",
    "reboot",
    "bcdedit",
    "diskpart",
    "format",
    "restart-computer",
    "stop-computer",
    "mkfs",
    "wipefs",
];
const PRIVILEGE_WRAPPERS: &[&str] = &["sudo", "doas"];
const PRIVILEGE_VALUE_OPTIONS: &[&str] = &[
    "-u",
    "--user",
    "-g",
    "--group",
    "-h",
    "--host",
    "-p",
    "--prompt",
    "-C",
    "--close-from",
    "-T",
    "--command-timeout",
    "-U",
    "--other-user",
    "-r",
    "--role",
    "-t",
    "--type",
];
/// `env`'s options that take a value as the next word.
const ENV_VALUE_OPTIONS: &[&str] = &["-u", "--unset", "-C", "--chdir", "-S", "--split-string"];
/// Commands that run the command after them, each with its options that take
/// a value as the next word.
const LAUNCH_WRAPPERS: &[(&str, &[&str])] = &[
    ("env", ENV_VALUE_OPTIONS),
    ("command", &[]),
    ("exec", &["-a"]),
    ("nohup", &[]),
    ("builtin", &[]),
    ("nice", &["-n", "--adjustment"]),
    ("timeout", &["-s", "--signal", "-k", "--kill-after"]),
    ("time", &["-f", "--format", "-o", "--output"]),
    (
        "stdbuf",
        &["-i", "--input", "-o", "--output", "-e", "--error"],
    ),
    ("ionice", &["-c", "--class", "-n", "--classdata"]),
    ("setsid", &[]),
    (
        "xargs",
        &[
            "-a",
            "--arg-file",
            "-d",
            "--delimiter",
            "-E",
            "-I",
            "-L",
            "-n",
            "--max-args",
            "-P",
            "--max-procs",
            "-s",
            "--max-chars",
            "--process-slot-var",
        ],
    ),
];
const NESTED_SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "ash"];
const SYSTEMCTL_DANGEROUS: &[&str] = &["poweroff", "reboot", "halt", "kexec"];
const SYSTEMCTL_VALUE_OPTIONS: &[&str] = &["-H", "--host", "-M", "--machine"];
const DD_SAFE_TARGETS: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/stdin",
    "/dev/stdout",
    "/dev/stderr",
];
/// Roots `rm -rf` may clear without asking.
const RM_SAFE_TEMP_ROOTS: &[&str] = &["/tmp", "/temp"];
/// How deep `sh -c '…'`, `eval` and substitutions are followed.
const MAX_DEPTH: usize = 4;
/// Git's global options that take the next word as their value.
const GIT_VALUE_OPTIONS: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
    "--config-env",
];
/// Git's global options that stand alone.
const GIT_FLAGS: &[&str] = &[
    "-p",
    "--paginate",
    "-P",
    "--no-pager",
    "--bare",
    "--no-replace-objects",
    "--no-lazy-fetch",
    "--literal-pathspecs",
    "--glob-pathspecs",
    "--noglob-pathspecs",
    "--icase-pathspecs",
    "--no-optional-locks",
    "--no-advice",
];
/// Git's global options that print something and run no command.
const GIT_INFO_OPTIONS: &[&str] = &[
    "-v",
    "--version",
    "-h",
    "--help",
    "--exec-path",
    "--html-path",
    "--man-path",
    "--info-path",
];
/// Git's global options written `--option=value`.
const GIT_ASSIGNED_OPTIONS: &[&str] = &[
    "--git-dir=",
    "--work-tree=",
    "--namespace=",
    "--super-prefix=",
    "--exec-path=",
    "--list-cmds=",
];
/// Git commands that rewrite or publish history, asked about whatever their
/// options.
const GIT_DESTRUCTIVE: &[&str] = &["push", "reset", "rebase"];

/// What a command is judged for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Judge {
    /// Commands that destroy data or stop the machine ([`analyze`]).
    Danger,
    /// Git commands that rewrite or publish history, or discard work
    /// ([`git_operation`]).
    Git,
}

/// Judge `command`.
#[must_use]
pub fn analyze(command: &str) -> Verdict {
    analyze_source(command, 0, Judge::Danger)
}

/// Judge `command` for git operations that rewrite or publish history, or
/// discard uncommitted work: `push`, `reset`, `rebase`, a forced `checkout` or
/// `switch` and a forced `clean`, however git is invoked — after global
/// options (`-C`, `--git-dir`, `-c`), through wrappers (`sudo`, `env`,
/// `timeout`, `xargs`, …), in a nested shell or `eval`, or quoted (`g''it`).
///
/// [`Verdict::Dangerous`] names the operation (`git push`). A git command
/// whose operation cannot be established — its subcommand or a force flag is
/// only known at run time, an option is not one git documents, or a `-c`
/// defines an alias — is [`Verdict::Unanalyzable`]. A command that is not
/// recognizably git (its name only known at run time, say) is
/// [`Verdict::Safe`]: this judge looks for git, while [`analyze`] asks about
/// anything it cannot read.
#[must_use]
pub fn git_operation(command: &str) -> Verdict {
    analyze_source(command, 0, Judge::Git)
}

fn analyze_source(source: &str, depth: usize, judge: Judge) -> Verdict {
    let Some(commands) = split_commands(source) else {
        // Unbalanced quoting: the shell refuses it, but its words cannot be
        // read, so a git command in it cannot be ruled out either.
        return match judge {
            Judge::Danger => Verdict::Unanalyzable,
            Judge::Git if mentions_git(source) => Verdict::Unanalyzable,
            Judge::Git => Verdict::Safe,
        };
    };
    let mut unanalyzable = false;
    for command in commands {
        // A substitution's own commands run too.
        for inner in &command.substitutions {
            if depth >= MAX_DEPTH {
                return Verdict::Unanalyzable;
            }
            match analyze_source(inner, depth + 1, judge) {
                Verdict::Safe => {}
                Verdict::Dangerous(what) => return Verdict::Dangerous(what),
                Verdict::Unanalyzable => unanalyzable = true,
            }
        }
        match analyze_command(&command.words, depth, judge) {
            Verdict::Safe => {}
            Verdict::Dangerous(what) => return Verdict::Dangerous(what),
            Verdict::Unanalyzable => unanalyzable = true,
        }
    }
    if unanalyzable {
        Verdict::Unanalyzable
    } else {
        Verdict::Safe
    }
}

/// Whether `source` has the word `git` in it.
fn mentions_git(source: &str) -> bool {
    source
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .any(|word| {
            word.eq_ignore_ascii_case("git") || word.to_ascii_lowercase().starts_with("git-")
        })
}

/// A word of a simple command: its value with quotes removed, and whether
/// that value is what the shell would pass (no expansion).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    value: String,
    literal: bool,
}

/// One simple command's words, with variable assignments and redirections
/// already dropped, and the sources of its command substitutions.
#[derive(Debug, Default)]
struct SimpleCommand {
    words: Vec<Word>,
    substitutions: Vec<String>,
}

/// Characters that make a word expand at run time.
fn expands(value: &str) -> bool {
    value.contains(['$', '`', '*', '?', '[', ']', '~'])
}

/// Split `source` into simple commands. `None` when the quoting does not
/// balance.
fn split_commands(source: &str) -> Option<Vec<SimpleCommand>> {
    let chars: Vec<char> = source.chars().collect();
    let mut commands = Vec::new();
    let mut current = SimpleCommand::default();
    let mut word = String::new();
    let mut word_started = false;
    let mut word_literal = true;
    // Words after a redirection operator are its target, not an argument.
    let mut skip_next_word = false;
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut i = 0;

    let finish_word = |current: &mut SimpleCommand,
                       word: &mut String,
                       started: &mut bool,
                       literal: &mut bool,
                       skip: &mut bool| {
        if *started {
            let value = std::mem::take(word);
            if *skip {
                *skip = false;
            } else if current.words.is_empty() && is_assignment(&value) {
                // `FOO=bar cmd`: the assignment is not the command.
            } else {
                let literal_value = *literal && !expands(&value);
                current.words.push(Word {
                    value,
                    literal: literal_value,
                });
            }
        }
        *started = false;
        *literal = true;
    };
    let finish_command = |commands: &mut Vec<SimpleCommand>, current: &mut SimpleCommand| {
        let done = std::mem::take(current);
        if !done.words.is_empty() || !done.substitutions.is_empty() {
            commands.push(done);
        }
    };

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                // An escaped newline joins lines; anything else is literal.
                if chars.get(i + 1) == Some(&'\n') {
                    i += 2;
                    continue;
                }
                if let Some(&next) = chars.get(i + 1) {
                    word.push(next);
                    word_started = true;
                    // `\*` is a literal star, but the value still shows one,
                    // and `expands` would call it an expansion: be cautious.
                    i += 2;
                    continue;
                }
                i += 1;
            }
            '\'' => {
                let end = chars[i + 1..].iter().position(|&ch| ch == '\'')?;
                word.extend(&chars[i + 1..i + 1 + end]);
                word_started = true;
                i += end + 2;
            }
            '"' => {
                let mut j = i + 1;
                loop {
                    let ch = *chars.get(j)?;
                    match ch {
                        '"' => break,
                        '\\' => {
                            if let Some(&next) = chars.get(j + 1) {
                                word.push(next);
                            }
                            j += 2;
                        }
                        '$' | '`' => {
                            word_literal = false;
                            if let Some((inner, next)) = substitution_at(&chars, j) {
                                current.substitutions.push(inner);
                                j = next;
                            } else {
                                word.push(ch);
                                j += 1;
                            }
                        }
                        _ => {
                            word.push(ch);
                            j += 1;
                        }
                    }
                }
                word_started = true;
                i = j + 1;
            }
            '$' | '`' => {
                word_literal = false;
                word_started = true;
                if let Some((inner, next)) = substitution_at(&chars, i) {
                    current.substitutions.push(inner);
                    i = next;
                } else {
                    word.push(c);
                    i += 1;
                }
            }
            '#' if !word_started => {
                // A comment runs to the end of the line.
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            ' ' | '\t' => {
                finish_word(
                    &mut current,
                    &mut word,
                    &mut word_started,
                    &mut word_literal,
                    &mut skip_next_word,
                );
                i += 1;
            }
            '\n' | ';' | '&' | '|' | '(' | ')' | '{' | '}'
                if c != '{' && c != '}' || !word_started =>
            {
                // `2>&1`: an ampersand right after a redirection is part of it.
                if c == '&' && (i > 0 && chars[i - 1] == '>' || chars.get(i + 1) == Some(&'>')) {
                    i += 1;
                    continue;
                }
                finish_word(
                    &mut current,
                    &mut word,
                    &mut word_started,
                    &mut word_literal,
                    &mut skip_next_word,
                );
                finish_command(&mut commands, &mut current);
                if c == '\n' {
                    i = skip_heredocs(&chars, i + 1, &mut heredocs);
                } else {
                    i += 1;
                }
            }
            '<' | '>' => {
                // A redirection: the operator, then its target word.
                let fd_prefix = word_started && word.chars().all(|ch| ch.is_ascii_digit());
                if fd_prefix {
                    word.clear();
                    word_started = false;
                    word_literal = true;
                } else {
                    finish_word(
                        &mut current,
                        &mut word,
                        &mut word_started,
                        &mut word_literal,
                        &mut skip_next_word,
                    );
                }
                if c == '<' && chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') {
                    // A heredoc: its body starts on the next line.
                    let strip_tabs = chars.get(i + 2) == Some(&'-');
                    let mut j = i + if strip_tabs { 3 } else { 2 };
                    while chars.get(j).is_some_and(|ch| *ch == ' ' || *ch == '\t') {
                        j += 1;
                    }
                    let start = j;
                    while chars
                        .get(j)
                        .is_some_and(|ch| !ch.is_whitespace() && !";&|".contains(*ch))
                    {
                        j += 1;
                    }
                    let delimiter: String = chars[start..j]
                        .iter()
                        .filter(|ch| !matches!(ch, '\'' | '"' | '\\'))
                        .collect();
                    if delimiter.is_empty() {
                        return None;
                    }
                    heredocs.push((delimiter, strip_tabs));
                    i = j;
                    continue;
                }
                // `<(…)`, `>(…)`: process substitution.
                if chars.get(i + 1) == Some(&'(') {
                    let (inner, next) = balanced(&chars, i + 1)?;
                    current.substitutions.push(inner);
                    i = next;
                    continue;
                }
                let mut j = i + 1;
                while chars
                    .get(j)
                    .is_some_and(|ch| matches!(ch, '<' | '>' | '&' | '|'))
                {
                    j += 1;
                }
                // `>&2`, `2>&1`: a descriptor, no word follows.
                if chars.get(j).is_some_and(char::is_ascii_digit) && chars[i + 1..j].contains(&'&')
                {
                    while chars.get(j).is_some_and(char::is_ascii_digit) {
                        j += 1;
                    }
                } else {
                    skip_next_word = true;
                }
                i = j;
            }
            _ => {
                word.push(c);
                word_started = true;
                i += 1;
            }
        }
    }
    finish_word(
        &mut current,
        &mut word,
        &mut word_started,
        &mut word_literal,
        &mut skip_next_word,
    );
    finish_command(&mut commands, &mut current);
    Some(commands)
}

/// `NAME=value` at the start of a command.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The command substitution starting at `chars[i]` (`$(` or a backtick):
/// its source and the index after it.
fn substitution_at(chars: &[char], i: usize) -> Option<(String, usize)> {
    match chars[i] {
        '`' => {
            let end = chars[i + 1..].iter().position(|&c| c == '`')?;
            Some((chars[i + 1..i + 1 + end].iter().collect(), i + end + 2))
        }
        '$' if chars.get(i + 1) == Some(&'(') && chars.get(i + 2) != Some(&'(') => {
            balanced(chars, i + 1)
        }
        _ => None,
    }
}

/// The text inside the parenthesis opening at `chars[open]`, and the index
/// after its closing one. Quotes inside are respected.
fn balanced(chars: &[char], open: usize) -> Option<(String, usize)> {
    let mut depth = 0;
    let mut j = open;
    while j < chars.len() {
        match chars[j] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((chars[open + 1..j].iter().collect(), j + 1));
                }
            }
            '\'' => j += chars[j + 1..].iter().position(|&c| c == '\'')? + 1,
            '"' => j += chars[j + 1..].iter().position(|&c| c == '"')? + 1,
            '\\' => j += 1,
            _ => {}
        }
        j += 1;
    }
    None
}

/// Skip the bodies of the heredocs opened on the line just ended.
fn skip_heredocs(chars: &[char], mut i: usize, heredocs: &mut Vec<(String, bool)>) -> usize {
    for (delimiter, strip_tabs) in heredocs.drain(..) {
        loop {
            if i >= chars.len() {
                return i;
            }
            let end = chars[i..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(chars.len(), |p| i + p);
            let line: String = chars[i..end].iter().collect();
            let line = if strip_tabs {
                line.trim_start_matches('\t').to_string()
            } else {
                line
            };
            i = (end + 1).min(chars.len());
            if line == delimiter {
                break;
            }
        }
    }
    i
}

/// `/usr/bin/RM.exe` → `rm`.
fn normalize_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw).to_lowercase();
    base.strip_suffix(".exe")
        .map_or(base.clone(), str::to_string)
}

/// A word after a command's name, in its place: its value, and whether that
/// value is what the command receives. A word that expands at run time keeps
/// its place, so that the words around it are still read as what they are.
#[derive(Debug, Clone, Copy)]
struct Arg<'a> {
    value: &'a str,
    literal: bool,
}

fn analyze_command(words: &[Word], depth: usize, judge: Judge) -> Verdict {
    let Some((first, rest)) = words.split_first() else {
        return Verdict::Safe;
    };
    let args: Vec<Arg<'_>> = rest
        .iter()
        .map(|word| Arg {
            value: &word.value,
            literal: word.literal,
        })
        .collect();
    if !first.literal || first.value.is_empty() {
        // A command known only at run time: it cannot be read, but neither is
        // it recognizably git.
        return match judge {
            Judge::Danger => Verdict::Unanalyzable,
            Judge::Git => Verdict::Safe,
        };
    }
    invocation(&normalize_name(&first.value), &args, depth, judge)
}

/// What to make of a command whose words cannot all be read: for the danger
/// judge, a command that cannot be checked; for the git judge, one only when
/// git is among its words.
fn unknown(judge: Judge, args: &[Arg<'_>]) -> Verdict {
    match judge {
        Judge::Danger => Verdict::Unanalyzable,
        Judge::Git
            if args.iter().any(|arg| {
                let name = normalize_name(arg.value);
                name == "git" || name.starts_with("git-")
            }) =>
        {
            Verdict::Unanalyzable
        }
        Judge::Git => Verdict::Safe,
    }
}

impl Arg<'_> {
    /// Whether the word may reach the command as an option: it is one, or it
    /// starts with an expansion that may produce one.
    fn may_be_option(self) -> bool {
        self.value.starts_with('-')
            || (!self.literal && self.value.starts_with(['$', '`', '*', '?', '[', '~']))
    }
}

/// The words of `args` after its leading options, skipping the value of each
/// option in `value_options`. `None` when a word where an option may stand is
/// only known at run time: it could be an option or the command itself.
fn after_options<'a, 'b>(args: &'b [Arg<'a>], value_options: &[&str]) -> Option<&'b [Arg<'a>]> {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if !arg.literal && arg.may_be_option() {
            return None;
        }
        if arg.value == "--" {
            return Some(&args[i + 1..]);
        }
        if arg.value == "-" || !arg.may_be_option() {
            return Some(&args[i..]);
        }
        if !arg.value.contains('=') && value_options.contains(&arg.value) {
            // The option's value, whatever it expands to.
            i += 1;
        }
        i += 1;
    }
    Some(&[])
}

/// Judge the command that `rest` starts with: the one a wrapper runs.
fn wrapped(rest: &[Arg<'_>], depth: usize, judge: Judge) -> Verdict {
    match rest.split_first() {
        None => Verdict::Safe,
        Some((name, args)) if name.literal => {
            invocation(&normalize_name(name.value), args, depth, judge)
        }
        Some(_) => unknown(judge, rest),
    }
}

/// `source`, written out as the words of a nested shell or `eval`, judged as
/// commands of their own.
fn nested(source: &str, depth: usize, judge: Judge) -> Verdict {
    if depth >= MAX_DEPTH {
        return Verdict::Unanalyzable;
    }
    analyze_source(source, depth + 1, judge)
}

fn invocation(name: &str, args: &[Arg<'_>], depth: usize, judge: Judge) -> Verdict {
    if PRIVILEGE_WRAPPERS.contains(&name) {
        return match after_options(args, PRIVILEGE_VALUE_OPTIONS) {
            Some(rest) => wrapped(rest, depth, judge),
            None => unknown(judge, args),
        };
    }
    if let Some((_, value_options)) = LAUNCH_WRAPPERS.iter().find(|(wrapper, _)| *wrapper == name) {
        return launch_wrapper(name, value_options, args, depth, judge);
    }
    if NESTED_SHELLS.contains(&name) {
        return nested_shell(args, depth, judge);
    }
    if name == "eval" {
        if args.is_empty() {
            return Verdict::Safe;
        }
        let source: Vec<&str> = args.iter().map(|arg| arg.value).collect();
        return nested(&source.join(" "), depth, judge);
    }
    if name == "busybox" {
        return match args.split_first() {
            Some((applet, rest)) if applet.literal && !applet.value.starts_with('-') => {
                invocation(&normalize_name(applet.value), rest, depth, judge)
            }
            Some((applet, _)) if !applet.literal => unknown(judge, args),
            _ => Verdict::Safe,
        };
    }
    match judge {
        Judge::Danger => dangerous_invocation(name, args),
        Judge::Git => git_invocation(name, args),
    }
}

fn launch_wrapper(
    name: &str,
    value_options: &[&str],
    args: &[Arg<'_>],
    depth: usize,
    judge: Judge,
) -> Verdict {
    if name == "command" {
        // `command -v x` only looks `x` up.
        for arg in args {
            if !arg.literal || arg.value == "--" || !arg.value.starts_with('-') {
                break;
            }
            if arg.value.contains(['v', 'V']) {
                return Verdict::Safe;
            }
        }
    }
    if name == "env"
        && let Some((line, rest)) = env_split_string(args)
    {
        // `env -S 'cmd args'` splits its value into the command it runs.
        let rest: Vec<&str> = rest.iter().map(|arg| arg.value).collect();
        return nested(&format!("{line} {}", rest.join(" ")), depth, judge);
    }
    let Some(mut rest) = after_options(args, value_options) else {
        return unknown(judge, args);
    };
    match name {
        "env" => {
            if rest.first().is_some_and(|arg| arg.value == "-") {
                rest = &rest[1..];
            }
            // The assignments before the command, whatever their values.
            let assignments = rest
                .iter()
                .take_while(|arg| is_assignment(arg.value))
                .count();
            rest = &rest[assignments..];
        }
        // `timeout DURATION command`.
        "timeout" => rest = rest.get(1..).unwrap_or_default(),
        _ => {}
    }
    wrapped(rest, depth, judge)
}

/// The value of `env`'s `-S` option among its options, and the words after it.
fn env_split_string<'a, 'b>(args: &'b [Arg<'a>]) -> Option<(&'a str, &'b [Arg<'a>])> {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let value = arg.value;
        if !arg.literal || value == "--" || value == "-" || !value.starts_with('-') {
            return None;
        }
        if value == "-S" || value == "--split-string" {
            return args.get(i + 1).map(|line| (line.value, &args[i + 2..]));
        }
        if let Some(line) = value
            .strip_prefix("--split-string=")
            .or_else(|| value.strip_prefix("-S"))
        {
            return Some((line, &args[i + 1..]));
        }
        if ENV_VALUE_OPTIONS.contains(&value) {
            i += 1;
        }
        i += 1;
    }
    None
}

fn nested_shell(args: &[Arg<'_>], depth: usize, judge: Judge) -> Verdict {
    let mut payload = None;
    for (i, arg) in args.iter().enumerate() {
        if payload == Some(i) {
            break;
        }
        if !arg.literal {
            return unknown(judge, args);
        }
        if arg.value == "--" {
            break;
        }
        if arg.value.len() > 1
            && arg.value.starts_with('-')
            && arg.value[1..].chars().all(|c| c.is_ascii_alphabetic())
        {
            if arg.value.contains('c') {
                payload = Some(i + 1);
            }
        } else {
            break;
        }
    }
    match payload {
        // The command string, as the nested shell will read it; a part only
        // known at run time stays a word that expands.
        Some(index) => match args.get(index) {
            Some(source) => nested(source.value, depth, judge),
            None => Verdict::Safe,
        },
        // A script file or standard input: not readable here.
        None if args.iter().any(|arg| !arg.literal) => unknown(judge, args),
        None => Verdict::Safe,
    }
}

fn dangerous_invocation(name: &str, words: &[Arg<'_>]) -> Verdict {
    let args: Vec<&str> = words
        .iter()
        .filter(|arg| arg.literal)
        .map(|arg| arg.value)
        .collect();
    let dropped = args.len() != words.len();
    if SIMPLE_DANGEROUS.contains(&name) || name.starts_with("mkfs.") {
        return Verdict::Dangerous(name.to_string());
    }
    if name == "init" || name == "telinit" {
        if args.iter().any(|arg| *arg == "0" || *arg == "6") {
            return Verdict::Dangerous(name.to_string());
        }
        return unanalyzable_if(dropped);
    }
    if name == "systemctl" {
        let literal: Vec<Arg<'_>> = words.iter().copied().filter(|arg| arg.literal).collect();
        if let Some(rest) = after_options(&literal, SYSTEMCTL_VALUE_OPTIONS)
            && let Some(sub) = rest.first()
            && SYSTEMCTL_DANGEROUS.contains(&sub.value)
        {
            return Verdict::Dangerous(format!("systemctl {}", sub.value));
        }
        return unanalyzable_if(dropped);
    }
    if name == "dd" {
        let to_device = args.iter().any(|arg| {
            arg.strip_prefix("of=").is_some_and(|target| {
                target.starts_with("/dev/") && !DD_SAFE_TARGETS.contains(&target)
            })
        });
        if to_device {
            return Verdict::Dangerous("dd".to_string());
        }
        return unanalyzable_if(dropped);
    }
    if name == "rm" {
        let (mut recursive, mut force) = (false, false);
        let mut operands = Vec::new();
        let mut options_ended = false;
        for arg in &args {
            if !options_ended && *arg == "--" {
                options_ended = true;
            } else if options_ended {
                operands.push(*arg);
            } else if *arg == "--recursive" {
                recursive = true;
            } else if *arg == "--force" {
                force = true;
            } else if arg.len() > 1
                && arg.starts_with('-')
                && arg[1..].chars().all(|c| c.is_ascii_alphabetic())
            {
                recursive |= arg.contains(['r', 'R']);
                force |= arg.contains('f');
            } else {
                operands.push(*arg);
            }
        }
        if recursive && force {
            if !dropped && !operands.is_empty() && operands.iter().all(|op| safe_temp_operand(op)) {
                return Verdict::Safe;
            }
            return Verdict::Dangerous("rm -rf".to_string());
        }
        return unanalyzable_if(dropped);
    }
    Verdict::Safe
}

fn unanalyzable_if(dropped: bool) -> Verdict {
    if dropped {
        Verdict::Unanalyzable
    } else {
        Verdict::Safe
    }
}

/// `git` (or a `git-<command>` program) and its words, judged for an operation
/// [`git_operation`] asks about.
fn git_invocation(name: &str, args: &[Arg<'_>]) -> Verdict {
    if let Some(command) = name.strip_prefix("git-") {
        return git_command(command, args);
    }
    if name != "git" {
        return Verdict::Safe;
    }
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if !arg.literal {
            // An option or the command, known only at run time.
            return Verdict::Unanalyzable;
        }
        let value = arg.value;
        if !value.starts_with('-') {
            return git_command(value, &args[i + 1..]);
        }
        if GIT_INFO_OPTIONS.contains(&value) {
            return Verdict::Safe;
        }
        if GIT_FLAGS.contains(&value) || GIT_ASSIGNED_OPTIONS.iter().any(|o| value.starts_with(o)) {
            i += 1;
            continue;
        }
        if let Some(setting) = value.strip_prefix("--config-env=") {
            if defines_alias(setting) {
                return Verdict::Unanalyzable;
            }
            i += 1;
            continue;
        }
        if GIT_VALUE_OPTIONS.contains(&value) {
            let Some(setting) = args.get(i + 1) else {
                return Verdict::Safe;
            };
            if (value == "-c" || value == "--config-env") && defines_alias(setting.value) {
                return Verdict::Unanalyzable;
            }
            i += 2;
            continue;
        }
        // An option git does not document: what follows it cannot be read.
        return Verdict::Unanalyzable;
    }
    Verdict::Safe
}

/// Whether a `-c`/`--config-env` setting may define an alias, which would make
/// the command name stand for something else. A key known only at run time may.
fn defines_alias(setting: &str) -> bool {
    let key = setting.split('=').next().unwrap_or(setting);
    expands(key) || key.to_ascii_lowercase().starts_with("alias.")
}

/// A git command and the words after it.
fn git_command(command: &str, args: &[Arg<'_>]) -> Verdict {
    if GIT_DESTRUCTIVE.contains(&command) {
        return Verdict::Dangerous(format!("git {command}"));
    }
    let extra: &[&str] = match command {
        "checkout" => &[],
        "switch" => &["--discard-changes"],
        "clean" => &[],
        _ => return Verdict::Safe,
    };
    match forced(args, extra) {
        Some(true) => Verdict::Dangerous(format!("git {command} -f")),
        Some(false) => Verdict::Safe,
        None => Verdict::Unanalyzable,
    }
}

/// Whether `args` force the command: `-f`, `--force`, a bundle of short
/// options with `f` in it, or one of `extra`. `None` when a word that could be
/// one of them is only known at run time.
fn forced(args: &[Arg<'_>], extra: &[&str]) -> Option<bool> {
    for arg in args {
        if arg.value == "--" {
            break;
        }
        if !arg.literal {
            // A word whose expansion can start with `-` could be a force flag.
            if arg.may_be_option() {
                return None;
            }
            continue;
        }
        let value = arg.value;
        if value == "--force" || extra.contains(&value) {
            return Some(true);
        }
        if value.len() > 1
            && value.starts_with('-')
            && !value.starts_with("--")
            && value[1..].chars().all(|c| c.is_ascii_alphabetic())
            && value.contains('f')
        {
            return Some(true);
        }
    }
    Some(false)
}

/// A literal path under `/tmp` or `/temp`, with no `..` to climb out.
fn safe_temp_operand(operand: &str) -> bool {
    !operand.split('/').any(|segment| segment == "..")
        && RM_SAFE_TEMP_ROOTS
            .iter()
            .any(|root| operand == *root || operand.starts_with(&format!("{root}/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dangerous(command: &str) -> String {
        match analyze(command) {
            Verdict::Dangerous(what) => what,
            other => panic!("{command:?}: expected dangerous, got {other:?}"),
        }
    }

    #[test]
    fn everyday_commands_are_safe() {
        for command in [
            "cargo test --workspace",
            "git status && git diff",
            "ls -la | grep foo > out.txt 2>&1",
            "rm file.txt",
            "rm -r build",
            "echo 'rm -rf /'",
            "FOO=bar make",
            "grep -rn 'shutdown' src/",
            "cat <<EOF\nrm -rf /\nEOF\necho done",
            "command -v shutdown",
            "dd if=/dev/zero of=/dev/null bs=1M count=1",
            "rm -rf /tmp/build /temp/cache",
            "cd /repo && npm ci # reboot later",
        ] {
            assert_eq!(analyze(command), Verdict::Safe, "{command:?}");
        }
    }

    #[test]
    fn destructive_commands_are_dangerous() {
        assert_eq!(dangerous("rm -rf /"), "rm -rf");
        assert_eq!(dangerous("rm -fr ~/project"), "rm -rf");
        assert_eq!(dangerous("rm --recursive --force build"), "rm -rf");
        assert_eq!(dangerous("sudo -u root rm -Rf /var/lib"), "rm -rf");
        assert_eq!(dangerous("git status && rm -rf .git"), "rm -rf");
        assert_eq!(dangerous("sudo shutdown -h now"), "shutdown");
        assert_eq!(dangerous("/sbin/REBOOT"), "reboot");
        assert_eq!(dangerous("mkfs.ext4 /dev/sda1"), "mkfs.ext4");
        assert_eq!(dangerous("dd if=/dev/zero of=/dev/sda"), "dd");
        assert_eq!(
            dangerous("systemctl --host box poweroff"),
            "systemctl poweroff"
        );
        assert_eq!(dangerous("init 6"), "init");
        assert_eq!(dangerous("bash -c 'rm -rf /srv'"), "rm -rf");
        assert_eq!(dangerous("env FOO=1 nohup rm -rf data"), "rm -rf");
        assert_eq!(dangerous("echo $(rm -rf /)"), "rm -rf");
        assert_eq!(dangerous("eval rm -rf build"), "rm -rf");
        assert_eq!(dangerous("busybox rm -rf /x"), "rm -rf");
        assert_eq!(dangerous("rm -rf /tmp/../etc"), "rm -rf");
    }

    /// An `rm -rf` whose operands expand cannot be shown to stay in `/tmp`.
    #[test]
    fn expanding_operands_cannot_be_cleared() {
        assert_eq!(dangerous("rm -rf /tmp/$DIR"), "rm -rf");
        assert_eq!(dangerous("rm -rf /tmp/*"), "rm -rf");
    }

    /// A word that expands keeps its place: `-u $U` is an option and its
    /// value, and the `rm -rf` after them is still seen.
    #[test]
    fn an_expanding_word_keeps_its_place() {
        assert_eq!(dangerous("sudo -u $U rm -rf /"), "rm -rf");
        assert_eq!(dangerous("env FOO=$BAR rm -rf data"), "rm -rf");
        assert_eq!(dangerous("bash -c \"rm -rf $DIR\""), "rm -rf");
        assert_eq!(dangerous("eval rm -rf $DIR"), "rm -rf");
        assert_eq!(analyze("sudo $CMD -rf /"), Verdict::Unanalyzable);
    }

    #[test]
    fn launch_wrappers_are_seen_through() {
        assert_eq!(dangerous("timeout -s KILL 30 rm -rf build"), "rm -rf");
        assert_eq!(dangerous("time rm -rf build"), "rm -rf");
        assert_eq!(dangerous("stdbuf -o L rm -rf build"), "rm -rf");
        assert_eq!(dangerous("ionice -c 3 nice -n 10 rm -rf build"), "rm -rf");
        assert_eq!(dangerous("setsid shutdown now"), "shutdown");
        assert_eq!(dangerous("find . -name x | xargs -0 -n 1 rm -rf"), "rm -rf");
        assert_eq!(dangerous("env -S 'rm -rf build'"), "rm -rf");
        assert_eq!(dangerous("env --split-string='rm -rf build'"), "rm -rf");
        assert_eq!(analyze("xargs echo"), Verdict::Safe);
        assert_eq!(analyze("timeout 5 cargo test"), Verdict::Safe);
        assert_eq!(analyze("env FOO=1 cargo test -S x"), Verdict::Safe);
    }

    fn git(command: &str) -> String {
        match git_operation(command) {
            Verdict::Dangerous(what) => what,
            other => panic!("{command:?}: expected a git operation, got {other:?}"),
        }
    }

    /// Every guarded git operation, in every way of invoking git the judge
    /// knows: plain, after global options, through wrappers, in nested shells
    /// and `eval`, quoted, by path and as a `git-<command>` program.
    #[test]
    fn every_form_of_a_guarded_git_operation_is_found() {
        let operations = [
            ("push origin main", "git push"),
            ("reset --hard HEAD~1", "git reset"),
            ("rebase -i main", "git rebase"),
            ("checkout -f main", "git checkout -f"),
            ("checkout --force main", "git checkout -f"),
            ("switch --discard-changes main", "git switch -f"),
            ("switch -f main", "git switch -f"),
            ("clean -fdx", "git clean -f"),
            ("clean --force", "git clean -f"),
        ];
        let forms = [
            "git {}",
            "git -C /repo {}",
            "git -C \"$DIR\" {}",
            "git --git-dir=.git --work-tree=. {}",
            "git --git-dir .git --work-tree . {}",
            "git -c core.pager=cat {}",
            "git -c user.name=\"$NAME\" {}",
            "git --no-pager -P --bare --namespace ns {}",
            "/usr/bin/git {}",
            "G\"I\"T {}",
            "g''it {}",
            "sudo -u root git {}",
            "env GIT_DIR=.git git {}",
            "command git {}",
            "exec git {}",
            "nohup nice -n 5 git {}",
            "timeout 10 git {}",
            "xargs -n 1 git {}",
            "busybox env git {}",
            "sh -c 'git {}'",
            "bash -lc \"cd /repo && git {}\"",
            "eval git {}",
            "echo $(git {})",
            "cd /repo; git status && git {}",
        ];
        for (operation, expected) in operations {
            for form in forms {
                let command = form.replace("{}", operation);
                assert_eq!(git(&command), expected, "{command:?}");
            }
        }
        assert_eq!(git("/usr/lib/git-core/git-push origin"), "git push");
    }

    #[test]
    fn everyday_git_and_other_commands_are_not_asked_about() {
        for command in [
            "git status",
            "git diff HEAD~1",
            "git log --oneline -5",
            "git stash push -m wip",
            "git checkout -b feature",
            "git checkout main -- src/lib.rs",
            "git checkout HEAD~1",
            "git switch --force-create topic",
            "git clean -n",
            "git -C /repo commit -m 'push it'",
            "git --version",
            "git --help push",
            "echo git push",
            "grep -rn 'git reset' docs",
            "cargo test",
            "$GIT push",
            "rm -rf build",
        ] {
            assert_eq!(git_operation(command), Verdict::Safe, "{command:?}");
        }
    }

    /// A git command whose operation cannot be established is asked about.
    #[test]
    fn a_git_command_that_cannot_be_read_is_unanalyzable() {
        for command in [
            "git $SUBCOMMAND",
            "git -c alias.p=push p origin",
            "git -c \"$KEY\"=push p",
            "git --config-env=alias.p=CMD p",
            "git --frobnicate push",
            "git checkout $FLAG main",
            "git clean \"$OPTS\"",
            "sudo $FLAGS git push",
            "echo 'git push",
        ] {
            assert_eq!(git_operation(command), Verdict::Unanalyzable, "{command:?}");
        }
    }

    #[test]
    fn commands_known_only_at_run_time_are_unanalyzable() {
        for command in [
            "$CMD --all",
            "`which rm` -rf x",
            "echo 'unbalanced",
            "\"$SHELL\" -c ls",
        ] {
            assert_eq!(analyze(command), Verdict::Unanalyzable, "{command:?}");
        }
    }
}
