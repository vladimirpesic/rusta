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
const LAUNCH_WRAPPERS: &[&str] = &["env", "command", "exec", "nohup", "builtin", "nice"];
const WRAPPER_VALUE_OPTIONS: &[&str] = &[
    "-u",
    "--unset",
    "-C",
    "--chdir",
    "-S",
    "--split-string",
    "-a",
    "-n",
    "--adjustment",
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

/// Judge `command`.
#[must_use]
pub fn analyze(command: &str) -> Verdict {
    analyze_source(command, 0)
}

fn analyze_source(source: &str, depth: usize) -> Verdict {
    let Some(commands) = split_commands(source) else {
        return Verdict::Unanalyzable;
    };
    let mut unanalyzable = false;
    for command in commands {
        // A substitution's own commands run too.
        for inner in &command.substitutions {
            if depth >= MAX_DEPTH {
                return Verdict::Unanalyzable;
            }
            match analyze_source(inner, depth + 1) {
                Verdict::Safe => {}
                Verdict::Dangerous(what) => return Verdict::Dangerous(what),
                Verdict::Unanalyzable => unanalyzable = true,
            }
        }
        match analyze_command(&command.words, depth) {
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

fn analyze_command(words: &[Word], depth: usize) -> Verdict {
    let Some(first) = words.first() else {
        return Verdict::Safe;
    };
    if !first.literal || first.value.is_empty() {
        return Verdict::Unanalyzable;
    }
    let dropped = words[1..].iter().any(|w| !w.literal);
    let args: Vec<&str> = words[1..]
        .iter()
        .filter(|w| w.literal)
        .map(|w| w.value.as_str())
        .collect();
    invocation(&normalize_name(&first.value), &args, dropped, depth)
}

/// Drop leading options (and the values of `value_options`) from `args`.
fn drop_leading_options<'a>(args: &[&'a str], value_options: &[&str]) -> Vec<&'a str> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--" {
            return args[i + 1..].to_vec();
        }
        if arg == "-" || !arg.starts_with('-') {
            return args[i..].to_vec();
        }
        if !arg.contains('=') && value_options.contains(&arg) {
            i += 1;
        }
        i += 1;
    }
    Vec::new()
}

fn unanalyzable_if(dropped: bool) -> Verdict {
    if dropped {
        Verdict::Unanalyzable
    } else {
        Verdict::Safe
    }
}

fn invocation(name: &str, args: &[&str], dropped: bool, depth: usize) -> Verdict {
    if PRIVILEGE_WRAPPERS.contains(&name) {
        let rest = drop_leading_options(args, PRIVILEGE_VALUE_OPTIONS);
        return match rest.split_first() {
            Some((inner, rest)) => invocation(&normalize_name(inner), rest, dropped, depth),
            None => unanalyzable_if(dropped),
        };
    }
    if LAUNCH_WRAPPERS.contains(&name) {
        if name == "command" {
            // `command -v x` only looks `x` up.
            for arg in args {
                if *arg == "--" || !arg.starts_with('-') {
                    break;
                }
                if arg.contains(['v', 'V']) {
                    return Verdict::Safe;
                }
            }
        }
        let mut rest = drop_leading_options(args, WRAPPER_VALUE_OPTIONS);
        if name == "env" {
            let skip = rest
                .iter()
                .skip(usize::from(rest.first() == Some(&"-")))
                .take_while(|arg| is_assignment(arg))
                .count()
                + usize::from(rest.first() == Some(&"-"));
            rest = rest[skip.min(rest.len())..].to_vec();
        }
        return match rest.split_first() {
            Some((inner, rest)) => invocation(&normalize_name(inner), rest, dropped, depth),
            None => unanalyzable_if(dropped),
        };
    }
    if NESTED_SHELLS.contains(&name) {
        let mut payload = None;
        for (i, arg) in args.iter().enumerate() {
            if *arg == "--" {
                break;
            }
            if arg.len() > 1
                && arg.starts_with('-')
                && arg[1..].chars().all(|c| c.is_ascii_alphabetic())
            {
                if arg.contains('c') {
                    payload = Some(i + 1);
                }
            } else {
                break;
            }
        }
        let Some(index) = payload else {
            return unanalyzable_if(dropped);
        };
        return match args.get(index) {
            Some(source) if depth < MAX_DEPTH => analyze_source(source, depth + 1),
            _ => Verdict::Unanalyzable,
        };
    }
    if name == "eval" {
        if args.is_empty() {
            return unanalyzable_if(dropped);
        }
        if dropped || depth >= MAX_DEPTH {
            return Verdict::Unanalyzable;
        }
        return analyze_source(&args.join(" "), depth + 1);
    }
    if name == "busybox" {
        return match args.split_first() {
            Some((applet, rest)) if !applet.starts_with('-') => {
                invocation(&normalize_name(applet), rest, dropped, depth)
            }
            _ => unanalyzable_if(dropped),
        };
    }
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
        let rest = drop_leading_options(args, SYSTEMCTL_VALUE_OPTIONS);
        if let Some(sub) = rest.first()
            && SYSTEMCTL_DANGEROUS.contains(sub)
        {
            return Verdict::Dangerous(format!("systemctl {sub}"));
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
        for arg in args {
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
