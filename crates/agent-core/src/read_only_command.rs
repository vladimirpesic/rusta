//! Shell commands that only read, which `bash` runs without asking.
//!
//! Outside `bypass_permissions`, `bash` asks before it runs a command. A
//! command that can do nothing but read files inside the workspace has no more
//! to ask about than `read_file` or `grep`, so it runs unasked, in plan mode
//! too, where reading is the work.
//!
//! A command judged wrongly would run with nobody seeing it first, so the
//! judgement is narrow, and anything it does not recognise is an ordinary
//! command, asked about as before:
//!
//! - **Plain syntax.** Words, single quotes, double quotes without expansions,
//!   comments, and the operators `|`, `||`, `&&`, `;` and newlines. Whatever
//!   the shell would expand or interpret at run time makes a command an
//!   ordinary one: `$`, backquotes, backslashes, globs, braces, a `~` that
//!   would expand, groupings, background jobs, here documents and variable
//!   assignments. Output may only be thrown away (`2>/dev/null`, `>/dev/null`,
//!   `2>&1`), and input read from a file (`< file`).
//! - **Programs known to read.** Each simple command names one of a fixed
//!   table of programs by its bare name, without an option that writes a file,
//!   runs another program or follows links: `sort -o`, `find -exec`,
//!   `rg --pre`, `sed -i`, `grep -R` and the like make it an ordinary command.
//!   `git` runs only its reading subcommands, and only in a repository whose
//!   own config and hooks cannot make it run a program.
//! - **Paths inside the workspace.** Every argument that is not an option,
//!   and every option's attached value, must resolve inside the workspace, as
//!   the file tools' paths must, unless the program is known to take it as
//!   something else (a search pattern, a `sed` script, the text `echo` prints).
//!   A `cd` adds where the next paths may resolve from: each must stay inside
//!   from there and from where the command started, so that a `cd` that fails
//!   cannot lead a path out.
//!
//! The call itself must not change the environment the command runs in: the
//! caller refuses a call that sets `env`, and [`reads_only`] refuses every
//! command while `PATH` names a relative directory, where a file in the
//! workspace could stand in for a program.

use std::path::{Path, PathBuf};

/// Whether `command`, run from `cwd`, can only read, and only inside the
/// workspace: `inside(from, path)` says whether `path`, as a command running
/// in `from` would open it, resolves inside.
pub fn reads_only(command: &str, cwd: &Path, inside: &dyn Fn(&Path, &Path) -> bool) -> bool {
    if !std::env::var_os("PATH").is_none_or(|path| all_absolute(&path)) {
        return false;
    }
    let Some(commands) = lex(command) else {
        return false;
    };
    if commands.is_empty() {
        return false;
    }
    let mut cwds = vec![cwd.to_path_buf()];
    let fenced = |cwds: &[PathBuf], word: &str| {
        (word == "/dev/null" && cfg!(unix))
            || (plain_path(word) && cwds.iter().all(|from| inside(from, Path::new(word))))
    };
    for command in &commands {
        let Some((name, args)) = command.words.split_first() else {
            return false;
        };
        if !command.inputs.iter().all(|input| fenced(&cwds, input)) {
            return false;
        }
        if name == "cd" {
            let [dir] = args else {
                return false;
            };
            if dir.starts_with('-') || !fenced(&cwds, dir) {
                return false;
            }
            let next: Vec<PathBuf> = cwds.iter().map(|from| from.join(dir)).collect();
            cwds.extend(next);
            if cwds.len() > MAX_CWDS {
                return false;
            }
            continue;
        }
        let Some(paths) = program_paths(name, args, &cwds) else {
            return false;
        };
        if !paths.iter().all(|path| fenced(&cwds, path)) {
            return false;
        }
    }
    true
}

/// The most places a command's paths are judged from: each `cd` doubles
/// them, since it may succeed or fail.
const MAX_CWDS: usize = 16;

/// Whether every directory of a `PATH` value is absolute: a relative one
/// (`.`, or the empty entry that means it) lets a file in the working
/// directory stand in for a program of the table.
fn all_absolute(path: &std::ffi::OsStr) -> bool {
    std::env::split_paths(path).all(|dir| !dir.as_os_str().is_empty() && dir.is_absolute())
}

/// Whether `word` can be judged as a path on this platform. On Windows the
/// shell is Git Bash, which reads `/c/…` and `C:\…` its own way, so a word
/// that starts at a root, names a drive or holds a backslash is not judged.
fn plain_path(word: &str) -> bool {
    !cfg!(windows) || !(word.starts_with(['/', '\\']) || word.contains([':', '\\']))
}

/// One simple command: its words, quotes removed, and the files its input is
/// redirected from.
#[derive(Debug, Default, PartialEq, Eq)]
struct Simple {
    words: Vec<String>,
    inputs: Vec<String>,
    /// Whether it carried a redirection, so that one with no words is refused.
    redirected: bool,
}

/// The simple commands of `source`, in order, or `None` when it uses anything
/// beyond plain syntax (see the module's documentation).
fn lex(source: &str) -> Option<Vec<Simple>> {
    let chars: Vec<char> = source.chars().collect();
    let mut commands = Vec::new();
    let mut current = Simple::default();
    // After `|`, `||` or `&&` a command must follow.
    let mut need_command = false;
    let mut i = 0;
    let close = |commands: &mut Vec<Simple>, current: &mut Simple| -> Option<()> {
        if current.words.is_empty() {
            return None;
        }
        commands.push(std::mem::take(current));
        Some(())
    };
    while i < chars.len() {
        match chars[i] {
            ' ' | '\t' => i += 1,
            '\n' => {
                if !current.words.is_empty() || current.redirected {
                    close(&mut commands, &mut current)?;
                    need_command = false;
                }
                i += 1;
            }
            ';' => {
                if need_command {
                    return None;
                }
                close(&mut commands, &mut current)?;
                i += 1;
            }
            '|' => {
                close(&mut commands, &mut current)?;
                need_command = true;
                i += match chars.get(i + 1) {
                    Some('|') => 2,
                    Some('&') => return None,
                    _ => 1,
                };
            }
            '&' => match chars.get(i + 1) {
                Some('&') => {
                    close(&mut commands, &mut current)?;
                    need_command = true;
                    i += 2;
                }
                Some('>') => {
                    i = redirect_output(&chars, i + 1)?;
                    current.redirected = true;
                }
                _ => return None,
            },
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '>' => {
                i = redirect_output(&chars, i)?;
                current.redirected = true;
            }
            '<' => {
                let (input, next) = redirect_input(&chars, i)?;
                current.inputs.push(input);
                current.redirected = true;
                i = next;
            }
            _ => {
                let (word, quoted, next) = read_word(&chars, i)?;
                let descriptor = !quoted && word.chars().all(|c| c.is_ascii_digit());
                match chars.get(next) {
                    Some('>') if descriptor => {
                        i = redirect_output(&chars, next)?;
                        current.redirected = true;
                    }
                    Some('<') => return None,
                    _ => {
                        if current.words.is_empty() && !quoted && is_assignment(&word) {
                            return None;
                        }
                        current.words.push(word);
                        i = next;
                    }
                }
            }
        }
    }
    if need_command && current.words.is_empty() {
        return None;
    }
    if !current.words.is_empty() || current.redirected {
        close(&mut commands, &mut current)?;
    }
    Some(commands)
}

/// `NAME=value`, which before a command's name sets its environment.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Characters a word may hold outside quotes: none the shell interprets.
fn plain_char(c: char) -> bool {
    c.is_alphanumeric() || "_-./,:=+@%^#~".contains(c)
}

/// The word starting at `chars[start]`: its value with quotes removed,
/// whether any part of it was quoted, and the index after it. `None` for a
/// character the shell would interpret.
fn read_word(chars: &[char], start: usize) -> Option<(String, bool, usize)> {
    let mut word = String::new();
    let mut quoted = false;
    let mut i = start;
    while let Some(&c) = chars.get(i) {
        match c {
            ' ' | '\t' | '\n' | ';' | '|' | '&' | '<' | '>' => break,
            '\'' => {
                let end = chars[i + 1..].iter().position(|&ch| ch == '\'')?;
                word.extend(&chars[i + 1..i + 1 + end]);
                quoted = true;
                i += end + 2;
            }
            '"' => {
                let end = chars[i + 1..].iter().position(|&ch| ch == '"')?;
                let inner = &chars[i + 1..i + 1 + end];
                if inner.iter().any(|ch| matches!(ch, '$' | '`' | '\\')) {
                    return None;
                }
                word.extend(inner);
                quoted = true;
                i += end + 2;
            }
            // A `~` expands at the start of a word, and after the `=` or `:`
            // of what the shell may take for an assignment.
            '~' if i == start || matches!(chars[i - 1], '=' | ':') => return None,
            c if plain_char(c) => {
                word.push(c);
                i += 1;
            }
            _ => return None,
        }
    }
    Some((word, quoted, i))
}

/// An output redirection starting at `chars[at]` (its first `>`): it may only
/// duplicate a descriptor (`>&2`) or go to `/dev/null`. The index after it.
fn redirect_output(chars: &[char], at: usize) -> Option<usize> {
    let mut i = at + 1;
    if chars.get(i) == Some(&'>') {
        i += 1;
    }
    if chars.get(i) == Some(&'&') {
        let digits = chars[i + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        return (digits > 0).then_some(i + 1 + digits);
    }
    while chars.get(i).is_some_and(|c| matches!(c, ' ' | '\t')) {
        i += 1;
    }
    let (target, _, next) = read_word(chars, i)?;
    (target == "/dev/null").then_some(next)
}

/// An input redirection starting at `chars[at]`: the file it reads, and the
/// index after it. Here documents, process substitutions and the other forms
/// of `<` are refused.
fn redirect_input(chars: &[char], at: usize) -> Option<(String, usize)> {
    let mut i = at + 1;
    if chars
        .get(i)
        .is_some_and(|c| matches!(c, '<' | '(' | '&' | '>'))
    {
        return None;
    }
    while chars.get(i).is_some_and(|c| matches!(c, ' ' | '\t')) {
        i += 1;
    }
    let (target, _, next) = read_word(chars, i)?;
    (!target.is_empty()).then_some((target, next))
}

/// The words of a command of `name` that must resolve inside the workspace,
/// or `None` when it is not a program of the table or an option of its may
/// write, run a program or follow links.
fn program_paths<'a>(name: &str, args: &'a [String], cwds: &[PathBuf]) -> Option<Vec<&'a str>> {
    match name {
        // Nothing they do writes, runs another program or follows a link
        // out of a directory they were given.
        "ls" | "pwd" | "cat" | "head" | "tail" | "wc" | "nl" | "cut" | "tac" | "rev" | "fold"
        | "paste" | "column" | "expand" | "unexpand" | "comm" | "cmp" | "realpath" | "readlink"
        | "stat" | "du" | "df" | "strings" | "od" | "hexdump" | "cksum" | "md5sum" | "sha1sum"
        | "sha224sum" | "sha256sum" | "sha384sum" | "sha512sum" | "b2sum" => Some(operands(args)),
        // They read no file: their words are text.
        "echo" | "printf" | "true" | "false" | "whoami" | "id" | "uname" | "seq" | "basename"
        | "dirname" | "which" | "tr" => Some(Vec::new()),
        // Comparing directories, `diff` follows the links in them.
        "diff" => {
            (!short_option(args, "r") && !long_option(args, &["recursive"])).then(|| operands(args))
        }
        "date" => (!short_option(args, "s") && !long_option(args, &["set"])).then(Vec::new),
        "hostname" => args.is_empty().then(Vec::new),
        "grep" | "egrep" | "fgrep" => (!short_option(args, "R")
            && !long_option(args, &["dereference-recursive"]))
        .then(|| search_operands(args, cwds)),
        "rg" => (!short_option(args, "Lz")
            && !long_option(
                args,
                &["pre", "pre-glob", "follow", "search-zip", "hostname-bin"],
            ))
        .then(|| search_operands(args, cwds)),
        "find" => {
            const WRITES_OR_RUNS: &[&str] = &[
                "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf",
                "-fls", "-L", "-follow",
            ];
            (!args.iter().any(|a| WRITES_OR_RUNS.contains(&a.as_str()))).then(|| operands(args))
        }
        "fd" | "fdfind" => (!short_option(args, "xXL")
            && !long_option(args, &["exec", "exec-batch", "follow"]))
        .then(|| operands(args)),
        "sort" => (!short_option(args, "oT")
            && !long_option(args, &["output", "compress-program", "temporary-directory"]))
        .then(|| operands(args)),
        // `uniq INPUT OUTPUT` writes OUTPUT.
        "uniq" => (operands(args).len() <= 1).then(|| operands(args)),
        "tree" => {
            (!short_option(args, "oRl") && !long_option(args, &["output"])).then(|| operands(args))
        }
        "file" => {
            (!short_option(args, "C") && !long_option(args, &["compile"])).then(|| operands(args))
        }
        // `xxd -r` writes binary, and a second file is where xxd writes.
        "xxd" => (!short_option(args, "r") && operands(args).len() <= 1).then(|| operands(args)),
        "sed" => sed_paths(args),
        "git" => git_paths(args, cwds),
        _ => None,
    }
}

/// The words a command reads as paths: each argument that is not an option,
/// and each option's attached value (`--file=x`, `-fx`). `-` (stdin) and the
/// words after `--` are arguments.
fn operands(args: &[String]) -> Vec<&str> {
    let mut words = Vec::new();
    let mut options_end = false;
    for arg in args {
        if options_end || !arg.starts_with('-') || arg == "-" {
            if arg != "-" {
                words.push(arg.as_str());
            }
        } else if arg == "--" {
            options_end = true;
        } else if let Some(long) = arg.strip_prefix("--") {
            if let Some((_, value)) = long.split_once('=')
                && !value.is_empty()
            {
                words.push(value);
            }
        } else {
            // A cluster of short options; what follows its first letter may
            // be that option's value.
            let value = arg.char_indices().nth(2).map_or("", |(at, _)| &arg[at..]);
            if !value.is_empty() {
                words.push(value);
            }
        }
    }
    words
}

/// [`operands`] of `grep` or `rg`, less the pattern they search for: the
/// first argument, unless `-e`/`--regexp` or `-f`/`--file` names the pattern,
/// and the value of each `-e`/`--regexp`. A word taken for the pattern that
/// names something on disk is a path all the same: it may be an option's
/// value rather than the pattern (`--exclude-from FILE`), and either way it
/// is not read as one unless it stays inside.
fn search_operands<'a>(args: &'a [String], cwds: &[PathBuf]) -> Vec<&'a str> {
    let names_pattern = short_option(args, "ef") || long_option(args, &["regexp", "file"]);
    let names_something = |word: &str| cwds.iter().any(|cwd| cwd.join(word).exists());
    let mut words = Vec::new();
    let mut options_end = false;
    let mut pattern_ahead = !names_pattern;
    let mut next_is_pattern = false;
    for arg in args {
        if next_is_pattern {
            next_is_pattern = false;
            continue;
        }
        if options_end || !arg.starts_with('-') || arg == "-" {
            if pattern_ahead {
                pattern_ahead = false;
                if names_something(arg) {
                    words.push(arg.as_str());
                }
            } else if arg != "-" {
                words.push(arg.as_str());
            }
            continue;
        }
        if arg == "--" {
            options_end = true;
        } else if arg == "-e" || arg == "--regexp" {
            next_is_pattern = true;
        } else if arg.starts_with("-e") || arg.starts_with("--regexp=") {
            // `-epattern`, `--regexp=pattern`.
        } else {
            words.extend(operands(std::slice::from_ref(arg)));
        }
    }
    words
}

/// Whether a cluster of short options among `args` (before `--`) holds one of
/// `letters`. Read over the whole cluster, an attached value included, so a
/// value that happens to hold the letter makes the command an ordinary one.
fn short_option(args: &[String], letters: &str) -> bool {
    args.iter()
        .take_while(|a| a.as_str() != "--")
        .filter(|a| a.starts_with('-') && !a.starts_with("--") && a.len() > 1)
        .any(|a| a[1..].contains(|c| letters.contains(c)))
}

/// Whether a long option among `args` (before `--`) is one of `names`, or a
/// prefix of one: getopt-style programs take any unambiguous prefix.
fn long_option(args: &[String], names: &[&str]) -> bool {
    args.iter()
        .take_while(|a| a.as_str() != "--")
        .filter_map(|a| a.strip_prefix("--"))
        .map(|a| a.split_once('=').map_or(a, |(name, _)| name))
        .any(|given| !given.is_empty() && names.iter().any(|name| name.starts_with(given)))
}

/// The paths of a `sed` that only prints: its options read no script file and
/// edit nothing in place, and every script is one [`sed_script_reads_only`]
/// accepts. Its files are its paths; its scripts are not.
fn sed_paths(args: &[String]) -> Option<Vec<&str>> {
    const FLAGS: &str = "nErsuz";
    const LONG_FLAGS: &[&str] = &[
        "quiet",
        "silent",
        "regexp-extended",
        "separate",
        "unbuffered",
        "null-data",
        "posix",
        "debug",
        "sandbox",
    ];
    let named_script = args.iter().take_while(|a| a.as_str() != "--").any(|a| {
        a.starts_with("--expression")
            || a.starts_with('-') && !a.starts_with("--") && a[1..].contains('e')
    });
    let mut files = Vec::new();
    let mut script_seen = named_script;
    let mut options_end = false;
    let mut next_is_script = false;
    for arg in args {
        if next_is_script {
            next_is_script = false;
            if !sed_script_reads_only(arg) {
                return None;
            }
            continue;
        }
        if options_end || !arg.starts_with('-') || arg == "-" {
            if script_seen {
                if arg != "-" {
                    files.push(arg.as_str());
                }
            } else {
                if !sed_script_reads_only(arg) {
                    return None;
                }
                script_seen = true;
            }
            continue;
        }
        if arg == "--" {
            options_end = true;
        } else if let Some(long) = arg.strip_prefix("--") {
            if let Some(script) = long.strip_prefix("expression=") {
                if !sed_script_reads_only(script) {
                    return None;
                }
            } else if long == "expression" {
                next_is_script = true;
            } else if !LONG_FLAGS.contains(&long) {
                return None;
            }
        } else {
            for (at, letter) in arg.char_indices().skip(1) {
                if letter == 'e' {
                    let rest = &arg[at + 1..];
                    if rest.is_empty() {
                        next_is_script = true;
                    } else if !sed_script_reads_only(rest) {
                        return None;
                    }
                    break;
                }
                if !FLAGS.contains(letter) {
                    return None;
                }
            }
        }
    }
    (script_seen && !next_is_script).then_some(files)
}

/// Whether a `sed` script only prints and moves through its input: each of
/// its commands is `p`, `=`, `l`, `q`, `Q`, `d`, `n`, `N`, a brace or a
/// negation, after line-number, `$`, `first~step` or `/regex/` addresses.
/// Substitution, file reading and writing, and command execution are not
/// among them.
fn sed_script_reads_only(script: &str) -> bool {
    let chars: Vec<char> = script.chars().collect();
    let mut i = 0;
    let mut commands = 0;
    while i < chars.len() {
        match chars[i] {
            ' ' | '\t' | '\n' | ';' | '{' | '}' | '!' => i += 1,
            '/' => {
                // A regex address: anything up to the next unescaped slash.
                let mut j = i + 1;
                loop {
                    match chars.get(j) {
                        None | Some('\n') => return false,
                        Some('\\') => j += 2,
                        Some('/') => break,
                        Some(_) => j += 1,
                    }
                }
                i = j + 1;
            }
            c if c.is_ascii_digit() || matches!(c, '$' | ',' | '~') => i += 1,
            'p' | '=' | 'l' | 'q' | 'Q' | 'd' | 'n' | 'N' => {
                commands += 1;
                i += 1;
            }
            _ => return false,
        }
    }
    commands > 0
}

/// The paths of a `git` command that only reads, or `None`: a global option
/// beyond those that change nothing, a subcommand that writes or contacts a
/// remote, an option that writes a file or runs a program, or a repository
/// whose config or hooks could run one ([`plain_repository`]).
fn git_paths<'a>(args: &'a [String], cwds: &[PathBuf]) -> Option<Vec<&'a str>> {
    const GLOBAL: &[&str] = &[
        "--no-pager",
        "-P",
        "--no-optional-locks",
        "--literal-pathspecs",
        "--glob-pathspecs",
        "--noglob-pathspecs",
        "--icase-pathspecs",
        "--no-replace-objects",
    ];
    let at = args.iter().position(|a| !GLOBAL.contains(&a.as_str()))?;
    let (subcommand, rest) = (args[at].as_str(), &args[at + 1..]);
    if long_option(
        rest,
        &["output", "ext-diff", "open-files-in-pager", "upload-pack"],
    ) {
        return None;
    }
    let reads = match subcommand {
        "status" | "log" | "show" | "diff" | "diff-tree" | "diff-files" | "diff-index"
        | "shortlog" | "blame" | "annotate" | "rev-parse" | "rev-list" | "ls-files" | "ls-tree"
        | "cat-file" | "describe" | "name-rev" | "merge-base" | "show-ref" | "for-each-ref"
        | "whatchanged" | "count-objects" | "check-ignore" | "check-attr" | "version" => true,
        "grep" => !short_option(rest, "O"),
        "branch" => lists_only(
            rest,
            &[
                "-a",
                "--all",
                "-r",
                "--remotes",
                "-v",
                "-vv",
                "--verbose",
                "-l",
                "--list",
                "--show-current",
                "--color",
                "--no-color",
                "--column",
                "--no-column",
                "-i",
                "--ignore-case",
                "--omit-empty",
                "--no-abbrev",
            ],
            &[
                "--contains",
                "--no-contains",
                "--merged",
                "--no-merged",
                "--points-at",
                "--sort",
                "--format",
                "--abbrev",
            ],
        ),
        "tag" => lists_only(
            rest,
            &[
                "-l",
                "--list",
                "--color",
                "--no-color",
                "--column",
                "--no-column",
                "-i",
                "--ignore-case",
                "--omit-empty",
            ],
            &[
                "--contains",
                "--no-contains",
                "--merged",
                "--no-merged",
                "--points-at",
                "--sort",
                "--format",
                "-n",
            ],
        ),
        "remote" => match rest.first().map(String::as_str) {
            None | Some("-v" | "--verbose") => rest.len() <= 1,
            Some("get-url") => true,
            Some(_) => false,
        },
        "stash" => matches!(rest.first().map(String::as_str), Some("list" | "show")),
        "reflog" => match rest.first().map(String::as_str) {
            None | Some("show" | "exists") => true,
            Some(first) => first.starts_with('-'),
        },
        "worktree" => rest.first().map(String::as_str) == Some("list"),
        _ => false,
    };
    (reads && cwds.iter().all(|cwd| plain_repository(cwd))).then(|| operands(rest))
}

/// Whether a `git branch` or `git tag` only lists: each option is one of
/// `flags`, or one of `valued` with its value, and a name or pattern comes only
/// with `-l`/`--list`, where it is a pattern to list by.
fn lists_only(args: &[String], flags: &[&str], valued: &[&str]) -> bool {
    let listing = args.iter().any(|a| a == "-l" || a == "--list");
    let mut expecting_value = false;
    for arg in args {
        if expecting_value {
            expecting_value = false;
            continue;
        }
        if !arg.starts_with('-') {
            if !listing {
                return false;
            }
            continue;
        }
        let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
        if flags.contains(&arg.as_str()) {
            continue;
        }
        if valued.contains(&name) {
            expecting_value = name == arg;
            continue;
        }
        // `-n5`: a short valued option with its value attached.
        if valued
            .iter()
            .any(|v| v.len() == 2 && arg.starts_with(v) && arg.len() > 2)
        {
            continue;
        }
        return false;
    }
    true
}

/// Whether the repository `cwd` is in can run no program of its own while
/// git reads it: its config sets nothing that runs a program (an fsmonitor, a
/// diff driver, a filter, a pager, an include, another work tree), no
/// `post-index-change` hook waits for `git status` to refresh the index, and no
/// submodule's own config is in play. Outside a repository there is none.
fn plain_repository(cwd: &Path) -> bool {
    let Some(git_dir) = git_dir_of(cwd) else {
        return true;
    };
    let common = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|dir| git_dir.join(dir.trim()));
    let dirs: Vec<&Path> = std::iter::once(git_dir.as_path())
        .chain(common.as_deref())
        .collect();
    dirs.iter().all(|dir| {
        !dir.join("hooks").join("post-index-change").exists()
            && !dir.join("modules").exists()
            && ["config", "config.worktree"].iter().all(|name| {
                match std::fs::read_to_string(dir.join(name)) {
                    Ok(text) => plain_config(&text),
                    Err(e) => e.kind() == std::io::ErrorKind::NotFound,
                }
            })
    })
}

/// The git directory of the repository `cwd` is in: the first `.git` found
/// walking up, or where a `.git` file (a worktree's, a submodule's) points.
fn git_dir_of(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find_map(|dir| {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let target = text.trim().strip_prefix("gitdir:")?.trim();
        Some(dir.join(target))
    })
}

/// Whether a git config file sets nothing that makes reading run a program
/// or read another work tree. Judged on section and key names alone.
fn plain_config(text: &str) -> bool {
    const SECTIONS: &[&str] = &["filter", "include", "includeif", "pager"];
    const KEYS: &[&str] = &[
        "fsmonitor",
        "hookspath",
        "pager",
        "sshcommand",
        "askpass",
        "worktree",
        "textconv",
        "command",
        "external",
        "program",
        "gitproxy",
        "alternaterefscommand",
        "packobjectshook",
    ];
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        let key = if let Some(header) = line.strip_prefix('[') {
            let header = header.split(']').next().unwrap_or(header);
            let name = header.split([' ', '"', '.']).next().unwrap_or(header);
            section = name.trim().to_ascii_lowercase();
            // `[section] key = value` on one line.
            match line.split_once(']') {
                Some((_, rest)) if !rest.trim().is_empty() => rest.trim(),
                _ => {
                    if SECTIONS.contains(&section.as_str()) {
                        return false;
                    }
                    continue;
                }
            }
        } else {
            line
        };
        if SECTIONS.contains(&section.as_str()) {
            return false;
        }
        let name = key
            .split(['=', ' ', '\t'])
            .next()
            .unwrap_or(key)
            .to_ascii_lowercase();
        if KEYS.contains(&name.as_str()) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Judge `command` from `root`, with the workspace being `root`.
    fn reads(root: &Path, command: &str) -> bool {
        let inside = |from: &Path, path: &Path| {
            let joined = if path.is_absolute() {
                path.to_path_buf()
            } else {
                from.join(path)
            };
            let mut normal = PathBuf::new();
            for part in joined.components() {
                match part {
                    std::path::Component::ParentDir => {
                        normal.pop();
                    }
                    std::path::Component::CurDir => {}
                    other => normal.push(other),
                }
            }
            normal.starts_with(root)
        };
        reads_only(command, root, &inside)
    }

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("main.rs"), "fn main() {}\n").unwrap();
        dir
    }

    #[test]
    fn plain_reads_inside_the_workspace_run_unasked() {
        let dir = workspace();
        for command in [
            "ls",
            "ls -la src",
            "cat src/main.rs",
            "head -n 20 src/main.rs | tail -n 5",
            "grep -rn 'fn main' src",
            "grep -rn \"/api/v1\" src",
            "rg -n --type rust 'fn main'",
            "rg -e '/etc/passwd' src",
            "find . -name '*.rs' -type f",
            "wc -l src/main.rs && echo done",
            "sed -n '1,20p' src/main.rs",
            "sed -n '/fn main/,/^}/p' src/main.rs",
            "sort src/main.rs | uniq -c",
            "cat src/main.rs 2>/dev/null",
            "grep -c main src/main.rs > /dev/null 2>&1",
            "wc -l < src/main.rs",
            "cd src && ls",
            "echo /etc/passwd",
            "ls # list it\n",
            "ls\n\ncat src/main.rs\n",
            "diff src/main.rs /dev/null",
            "stat src/main.rs; du -sh src",
        ] {
            if cfg!(windows) && command.starts_with("diff") {
                continue;
            }
            assert!(reads(dir.path(), command), "{command}");
        }
    }

    #[test]
    fn expansions_and_shell_constructs_are_ordinary_commands() {
        let dir = workspace();
        for command in [
            "cat $FILE",
            "cat \"$HOME/x\"",
            "ls `pwd`",
            "ls $(pwd)",
            "ls *.rs",
            "ls src/?ain.rs",
            "ls {a,b}",
            "cat ~/.ssh/id_rsa",
            "ls x=~/y",
            "ls \\\n src",
            "(ls)",
            "{ ls; }",
            "ls &",
            "ls |& cat",
            "cat <<EOF\nx\nEOF",
            "cat <(ls)",
            "FOO=bar ls",
            "ls > out.txt",
            "ls >> out.txt",
            "ls 2> err.txt",
            "ls >&out",
            "ls |",
            "ls &&",
            "; ls",
            "ls ;; ls",
            "",
            "   ",
            "! ls",
            "cat 'unterminated",
        ] {
            assert!(!reads(dir.path(), command), "{command}");
        }
    }

    #[test]
    fn a_path_out_of_the_workspace_is_an_ordinary_command() {
        let dir = workspace();
        for command in [
            "cat /etc/passwd",
            "cat ../secret",
            "cat src/../../secret",
            "grep -r root /etc",
            "grep -f /etc/patterns src",
            "rg --ignore-file=/etc/x pattern",
            "wc -l < /etc/passwd",
            "cd .. && ls",
            "cd src && cat ../../x",
            "cd /etc && cat passwd",
            "ls -I/etc",
        ] {
            assert!(!reads(dir.path(), command), "{command}");
        }
    }

    /// Paths after a `cd` must stay inside from where it started too: a `cd`
    /// that fails leaves the command there.
    #[test]
    fn a_cd_does_not_lead_paths_out_if_it_fails() {
        let dir = workspace();
        assert!(!reads(dir.path(), "cd src || cat ../x"));
        assert!(!reads(dir.path(), "cd src; cat ../main.rs"));
        assert!(reads(dir.path(), "cd src && cat main.rs"));
        assert!(!reads(dir.path(), "cd"));
        assert!(!reads(dir.path(), "cd -"));
        assert!(!reads(dir.path(), "cd src extra"));
    }

    #[test]
    fn options_that_write_run_or_follow_links_are_ordinary_commands() {
        let dir = workspace();
        for command in [
            "sort -o out src/main.rs",
            "sort --output=out src/main.rs",
            "sort --out=out src/main.rs",
            "sort --compress-program=gzip src/main.rs",
            "find . -exec rm {} ;",
            "find . -delete",
            "find . -fprint out",
            "find -L .",
            "diff -r src other",
            "diff --recursive src other",
            "rg --pre cat x",
            "rg --pre=cat x",
            "rg -L x",
            "rg -z x",
            "grep -R x .",
            "fd -x rm",
            "uniq src/main.rs out",
            "tree -o out",
            "file -C",
            "xxd -r dump out",
            "date -s 2020-01-01",
            "hostname newname",
            "sed -i s/a/b/ src/main.rs",
            "sed 's/a/b/w out' src/main.rs",
            "sed -n '1r /etc/passwd' src/main.rs",
            "sed -f script.sed src/main.rs",
            "sed -n 1e src/main.rs",
            "sed -n '1w out' src/main.rs",
            "awk '{print}' src/main.rs",
            "xargs rm",
            "env",
            "printenv",
            "jq . x.json",
            "rm -rf src",
            "./script.sh",
            "/bin/cat src/main.rs",
            "python3 -c 'print(1)'",
        ] {
            assert!(!reads(dir.path(), command), "{command}");
        }
    }

    #[test]
    fn sed_reads_only_when_it_only_prints() {
        let dir = workspace();
        for command in [
            "sed -n 5p src/main.rs",
            "sed -n '$p' src/main.rs",
            "sed '10q' src/main.rs",
            "sed -n -e 1p -e 2p src/main.rs",
            "sed -ne '1,3p' src/main.rs",
            "sed -n --expression=1p src/main.rs",
            "sed -nE '/a|b/p' src/main.rs",
            "sed -n '/x\\/y/p' src/main.rs",
            "sed -n '1~2p' src/main.rs",
            "sed -n '/a/!p' src/main.rs",
        ] {
            assert!(reads(dir.path(), command), "{command}");
        }
        assert!(!reads(dir.path(), "sed -n 1p /etc/passwd"));
        assert!(!reads(dir.path(), "sed -n"));
        assert!(!reads(dir.path(), "sed -n -e"));
    }

    fn git_workspace() -> tempfile::TempDir {
        let dir = workspace();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n[remote \"origin\"]\n\turl = https://example.com/command-line.git\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn git_reads_only_with_its_reading_subcommands() {
        let dir = git_workspace();
        for command in [
            "git status",
            "git status --short",
            "git --no-pager log --oneline -20",
            "git diff HEAD~1 -- src",
            "git log -p --stat src/main.rs",
            "git blame src/main.rs",
            "git branch",
            "git branch -a -v",
            "git branch --contains HEAD",
            "git branch -l 'feat*'",
            "git tag",
            "git tag -l 'v1.*'",
            "git remote -v",
            "git stash list",
            "git rev-parse --show-toplevel",
            "git ls-files src",
            "git grep -n main",
            "git worktree list",
            "git reflog",
        ] {
            assert!(reads(dir.path(), command), "{command}");
        }
        for command in [
            "git commit -m x",
            "git push",
            "git checkout src",
            "git branch new-branch",
            "git branch -d old",
            "git branch -D old",
            "git tag v2",
            "git tag -d v1",
            "git remote add x y",
            "git remote show origin",
            "git stash",
            "git stash pop",
            "git reflog expire --all",
            "git worktree add ../x",
            "git config --list",
            "git -c core.pager=x log",
            "git -C .. status",
            "git diff --output=x",
            "git diff --out=x",
            "git diff --ext-diff",
            "git grep -O x",
            "git grep --open-files-in-pager x",
            "git fetch",
            "git log -- /etc/passwd",
            "git diff --no-index /etc/passwd src/main.rs",
        ] {
            assert!(!reads(dir.path(), command), "{command}");
        }
    }

    #[test]
    fn a_repository_whose_config_runs_programs_is_asked_about() {
        for config in [
            "[core]\n\tfsmonitor = ./monitor.sh\n",
            "[core]\n\tFsMonitor = x\n",
            "[core]\n\thooksPath = hooks\n",
            "[core]\n\tpager = less\n",
            "[core]\n\tworktree = /elsewhere\n",
            "[diff \"bin\"]\n\ttextconv = ./conv\n",
            "[diff]\n\texternal = ./ext\n",
            "[filter \"lfs\"]\n\tclean = git-lfs clean -- %f\n",
            "[include]\n\tpath = other.config\n",
            "[includeIf \"gitdir:x\"]\n\tpath = y\n",
            "[gpg]\n\tprogram = ./gpg\n",
            "[core] fsmonitor = x\n",
        ] {
            let dir = git_workspace();
            std::fs::write(dir.path().join(".git").join("config"), config).unwrap();
            assert!(!reads(dir.path(), "git status"), "{config}");
            assert!(reads(dir.path(), "ls"), "{config}");
        }
        let dir = git_workspace();
        std::fs::write(
            dir.path()
                .join(".git")
                .join("hooks")
                .join("post-index-change"),
            "#!/bin/sh\n",
        )
        .unwrap();
        assert!(!reads(dir.path(), "git status"));
        let dir = git_workspace();
        std::fs::create_dir(dir.path().join(".git").join("modules")).unwrap();
        assert!(!reads(dir.path(), "git status"));
    }

    /// A worktree's `.git` file points at its git directory, whose
    /// `commondir` names the repository's: the configs of both count.
    #[test]
    fn a_worktree_is_judged_by_its_repository_config() {
        let dir = workspace();
        let common = dir.path().join("main-git");
        let own = common.join("worktrees").join("w");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            dir.path().join(".git"),
            format!("gitdir: {}\n", own.display()),
        )
        .unwrap();
        std::fs::write(common.join("config"), "[core]\n\tbare = false\n").unwrap();
        assert!(reads(dir.path(), "git status"));
        std::fs::write(common.join("config"), "[core]\n\tfsmonitor = x\n").unwrap();
        assert!(!reads(dir.path(), "git status"));
    }

    #[test]
    fn options_and_operands_are_told_apart() {
        let words = |args: &[&str]| args.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let args = words(&["-n", "--file=x", "-fy", "a", "--", "-b", "-"]);
        assert_eq!(operands(&args), ["x", "y", "a", "-b"]);

        let dir = workspace();
        let cwds = [dir.path().to_path_buf()];
        let args = words(&["-rn", "pattern", "src"]);
        assert_eq!(search_operands(&args, &cwds), ["n", "src"]);
        let args = words(&["-e", "/pat", "-f", "list", "src"]);
        assert_eq!(search_operands(&args, &cwds), ["list", "src"]);
        // A pattern that names a file may be an option's value: it is a path.
        let args = words(&["--exclude-from", "src", "pattern"]);
        assert_eq!(search_operands(&args, &cwds), ["src", "pattern"]);
    }

    /// `grep --exclude-from FILE` reads FILE: a word taken for the pattern
    /// that names a file outside is not read unasked.
    #[test]
    fn a_pattern_that_names_a_file_outside_is_a_path() {
        let dir = workspace();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let command = format!("grep --exclude-from {} x src", outside.path().display());
        assert!(!reads(dir.path(), &command), "{command}");
        assert!(reads(dir.path(), "grep -rn /no/such/dir src"));
    }

    #[test]
    fn a_relative_path_entry_disables_the_judgement() {
        let absolute = std::env::temp_dir();
        let ok = std::env::join_paths([&absolute, &absolute.join("bin")]).unwrap();
        assert!(all_absolute(&ok));
        for relative in ["", ".", "bin"] {
            let path = std::env::join_paths([absolute.clone(), std::path::PathBuf::from(relative)])
                .unwrap();
            assert!(!all_absolute(&path), "{relative:?}");
        }
    }
}
