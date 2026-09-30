// Whether a shell command line ran `git commit`. A false negative only skips
// one anchoring pass and a false positive runs a harmless one, so this reads
// the line the way a shell would without trying to be a shell.

type Words = Vec<String>;

pub(super) fn runs_git_commit(command: &str) -> bool {
    // A line with an unbalanced quote is a syntax error: the shell ran nothing.
    split_simple_commands(command)
        .is_some_and(|commands| commands.iter().any(|words| is_git_commit(words)))
}

struct Lexer {
    commands: Vec<Words>,
    words: Words,
    word: String,
    in_word: bool,
}

impl Lexer {
    fn end_word(&mut self) {
        if self.in_word {
            self.words.push(std::mem::take(&mut self.word));
            self.in_word = false;
        }
    }

    fn end_command(&mut self) {
        self.end_word();
        if !self.words.is_empty() {
            self.commands.push(std::mem::take(&mut self.words));
        }
    }

    fn push(&mut self, c: char) {
        self.word.push(c);
        self.in_word = true;
    }
}

// Splits at `&&`, `||`, `;`, `|`, `&`, newlines, subshell parentheses and
// backticks, outside quotes. Heredoc bodies are dropped: they are text, and
// may hold an apostrophe that would otherwise read as an open quote.
// `None` on an unterminated quote.
fn split_simple_commands(input: &str) -> Option<Vec<Words>> {
    let mut lexer = Lexer {
        commands: Vec::new(),
        words: Vec::new(),
        word: String::new(),
        in_word: false,
    };
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                lexer.in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => lexer.word.push(other),
                    }
                }
            }
            '"' => {
                lexer.in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            escaped @ ('"' | '\\' | '$' | '`') => lexer.word.push(escaped),
                            '\n' => {}
                            other => {
                                lexer.word.push('\\');
                                lexer.word.push(other);
                            }
                        },
                        other => lexer.word.push(other),
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') => {}
                Some(escaped) => lexer.push(escaped),
                None => lexer.push('\\'),
            },
            ' ' | '\t' | '\r' => lexer.end_word(),
            '\n' => {
                lexer.end_command();
                skip_heredoc_bodies(&mut chars, &mut heredocs);
            }
            ';' | '(' | ')' | '`' => lexer.end_command(),
            '|' | '&' => {
                if chars
                    .peek()
                    .is_some_and(|next| *next == c || (c == '|' && *next == '&'))
                {
                    chars.next();
                }
                lexer.end_command();
            }
            '<' if chars.peek() == Some(&'<') => {
                chars.next();
                if chars.peek() == Some(&'<') {
                    // A here-string: its operand is an ordinary word.
                    chars.next();
                    lexer.end_word();
                } else {
                    lexer.end_word();
                    heredocs.push(read_heredoc_delimiter(&mut chars)?);
                }
            }
            '#' if !lexer.in_word => {
                while chars.peek().is_some_and(|next| *next != '\n') {
                    chars.next();
                }
            }
            other => lexer.push(other),
        }
    }
    lexer.end_command();
    Some(lexer.commands)
}

// The word after `<<` or `<<-`, with its quoting removed, and whether leading
// tabs may precede the closing line.
fn read_heredoc_delimiter(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Option<(String, bool)> {
    let strip_tabs = chars.peek() == Some(&'-');
    if strip_tabs {
        chars.next();
    }
    while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
        chars.next();
    }
    let mut delimiter = String::new();
    while let Some(&c) = chars.peek() {
        match c {
            '\'' | '"' => {
                chars.next();
                loop {
                    let inner = chars.next()?;
                    if inner == c {
                        break;
                    }
                    delimiter.push(inner);
                }
            }
            '\\' => {
                chars.next();
                if let Some(escaped) = chars.next() {
                    delimiter.push(escaped);
                }
            }
            c if c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '<' | '>') => break,
            c => {
                chars.next();
                delimiter.push(c);
            }
        }
    }
    Some((delimiter, strip_tabs))
}

fn skip_heredoc_bodies(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    heredocs: &mut Vec<(String, bool)>,
) {
    for (delimiter, strip_tabs) in heredocs.drain(..) {
        loop {
            let mut line = String::new();
            let mut saw_any = false;
            for c in chars.by_ref() {
                saw_any = true;
                if c == '\n' {
                    break;
                }
                line.push(c);
            }
            let candidate = if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line.as_str()
            };
            if !saw_any || candidate == delimiter {
                break;
            }
        }
    }
}

fn is_env_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_git_program(word: &str) -> bool {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    name == "git" || name.eq_ignore_ascii_case("git.exe")
}

fn is_git_commit(words: &[String]) -> bool {
    let mut words = words.iter().skip_while(|w| is_env_assignment(w));
    if !words.next().is_some_and(|program| is_git_program(program)) {
        return false;
    }
    while let Some(word) = words.next() {
        match word.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path"
            | "--config-env" | "--super-prefix" | "--attr-source" => {
                words.next();
            }
            flag if flag.starts_with('-') => {}
            subcommand => return subcommand == "commit",
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_detection_table() {
        let cases: &[(&str, bool)] = &[
            ("git commit", true),
            ("git commit -m 'fix it'", true),
            ("git commit -m \"fix it\"", true),
            ("git commit --amend", true),
            ("git commit --amend --no-edit", true),
            ("git add . && git commit -m x", true),
            ("git add .; git commit -m x", true),
            ("git add . || git commit -m x", true),
            ("git status | cat & git commit -m x", true),
            ("cd repo\ngit commit -m x", true),
            (
                "GIT_AUTHOR_NAME=a GIT_COMMITTER_NAME=b git commit -m x",
                true,
            ),
            ("/usr/bin/git commit -m x", true),
            ("\"C:\\Program Files\\Git\\cmd\\git.exe\" commit -m x", true),
            ("git.exe commit -m x", true),
            ("git -C ../other commit -m x", true),
            ("git -c user.name=a -c user.email=b commit -m x", true),
            ("git --git-dir .git --work-tree . commit", true),
            ("git --git-dir=.git commit", true),
            ("git --no-pager commit -m x", true),
            ("git --namespace ns --exec-path /x -p commit", true),
            ("(git commit -m x)", true),
            ("echo $(git commit -m x)", true),
            ("git commit -m \"a && b\"", true),
            (
                "git commit -F - <<'EOF'\nfix: don't crash\n\nbody\nEOF",
                true,
            ),
            (
                "git commit -m \"$(cat <<'EOF'\nfix: don't crash\nEOF\n)\"",
                true,
            ),
            ("git commit -F - <<-EOF\n\tit's\n\tEOF\ngit status", true),
            ("git log --grep commit", false),
            ("git log --oneline", false),
            ("echo git commit", false),
            ("echo 'git commit'", false),
            ("git commit-tree HEAD^{tree}", false),
            ("git commitx", false),
            ("git status && echo done", false),
            ("git", false),
            ("git -C", false),
            ("gitk commit", false),
            ("mygit commit", false),
            ("sudo git commit", false),
            ("FOO=1", false),
            ("", false),
            ("   ", false),
            ("git commit -m \"unterminated", false),
            ("git commit -m 'unterminated", false),
            ("git status && git commit -m 'unterminated", false),
            ("# git commit", false),
            ("git config alias.ci commit", false),
            ("git stash \"commit\"", false),
        ];
        for (command, expected) in cases {
            assert_eq!(
                runs_git_commit(command),
                *expected,
                "runs_git_commit({command:?})"
            );
        }
    }

    #[test]
    fn simple_commands_split_on_every_control_operator() {
        let commands = split_simple_commands("a b && c || d ; e | f & g\nh").unwrap();
        let joined: Vec<String> = commands.iter().map(|w| w.join(" ")).collect();
        assert_eq!(joined, ["a b", "c", "d", "e", "f", "g", "h"]);
    }

    #[test]
    fn quoting_joins_adjacent_pieces_into_one_word() {
        let commands = split_simple_commands("git\" \"commit 'a b'\"c d\"e\\ f").unwrap();
        assert_eq!(commands, [vec!["git commit", "a bc de f"]]);
    }

    #[test]
    fn a_heredoc_body_is_not_scanned_for_commands() {
        let commands = split_simple_commands("cat <<EOF\ngit commit\nEOF\nls").unwrap();
        let joined: Vec<String> = commands.iter().map(|w| w.join(" ")).collect();
        assert_eq!(joined, ["cat", "ls"]);
    }
}
