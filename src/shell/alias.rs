//! Alias expansion at the token level.
//!
//! Aliases are expanded the way POSIX describes: when a word that can name a
//! simple command matches an alias, the alias body is lexed and spliced into
//! the token stream, and the shell re-scans the result. Doing this on tokens
//! (not on the raw line) means keyword positions such as `then`, `do` and
//! `else` are already understood, while operators inside an alias body still
//! behave as operators.
//!
//! Which word can name a command is tracked while the stream is walked, not
//! guessed from the word itself: a `case` pattern, a redirection's target, a
//! `for … in` list word and every argument are not command positions, so an
//! alias never expands there — while a command name may still follow a
//! redirection or an assignment prefix (`> out cmd`, `X=1 cmd`).

use std::collections::HashMap;

use super::lexer::{self, Op, RedirOp, Token, Word};

/// Maximum recursive expansion depth, so self-referencing aliases terminate.
const MAX_DEPTH: usize = 10;

/// Expand aliases in `tokens`, reading bodies from `aliases`. A non-interactive
/// shell passes an empty map: `ash -c '…'` and scripts must never change
/// meaning because of a personal alias.
pub fn expand_tokens(tokens: Vec<Token>, aliases: &HashMap<String, String>) -> Vec<Token> {
    let mut active: Vec<String> = Vec::new();
    let mut positions = Positions::default();
    expand_inner(tokens, aliases, &mut active, 0, &mut positions)
}

fn expand_inner(
    tokens: Vec<Token>,
    aliases: &HashMap<String, String>,
    active: &mut Vec<String>,
    depth: usize,
    positions: &mut Positions,
) -> Vec<Token> {
    let mut out = Vec::with_capacity(tokens.len());

    for token in tokens {
        if depth < MAX_DEPTH
            && positions.at_command()
            && let Token::Word(word) = &token
            && let Some(name) = word.plain_literal()
            && !name.is_empty()
            && !super::parser::is_reserved(name)
            && !active.iter().any(|alias| alias == name)
            && let Some(body) = aliases.get(name)
        {
            let unmigrated = unmigrate_stored_body(body);
            if let Ok(lexed) = lexer::lex(&unmigrated) {
                active.push(name.to_string());
                // The body replaces the alias in place and is walked with the
                // same positions, so a body that opens or closes a construct
                // leaves the stream where the body ended — a trailing `&&` in
                // the body still expects a command.
                let expanded = expand_inner(lexed.tokens, aliases, active, depth + 1, positions);
                active.pop();
                if !expanded.is_empty() {
                    out.extend(expanded);
                    continue;
                }
            }
        }
        positions.after(&token);
        out.push(token);
    }
    out
}

/// Whether the next word can name a command.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Spot {
    /// A command name may come next.
    Command,
    /// The next word is not a command name: an argument, a `case` pattern, a
    /// `for` list word, or a redirection's target.
    Word,
}

/// The walk's position in the command being read.
struct Positions {
    spot: Spot,
    /// The next word is the target of a redirection.
    redir_target: bool,
    /// What `spot` was before that redirection: a command name may still
    /// follow it (`> out cmd`).
    after_redir: Spot,
    /// One entry per open `case` construct, innermost last: true while its
    /// pattern list is being read, where the case word, `in`, the patterns and
    /// the pattern-separating `|` are all words rather than commands.
    case_patterns: Vec<bool>,
}

impl Default for Positions {
    fn default() -> Positions {
        Positions {
            spot: Spot::Command,
            redir_target: false,
            after_redir: Spot::Word,
            case_patterns: Vec::new(),
        }
    }
}

impl Positions {
    fn at_command(&self) -> bool {
        self.spot == Spot::Command && !self.redir_target
    }

    fn in_patterns(&self) -> bool {
        self.case_patterns.last().copied().unwrap_or(false)
    }

    /// Advance past `token`, which was not expanded.
    fn after(&mut self, token: &Token) {
        // A redirection's target is a word, but never a command name: put back
        // whatever was expected before the redirection once it is read.
        if self.redir_target {
            self.redir_target = false;
            if matches!(token, Token::Word(_)) {
                self.spot = self.after_redir;
                return;
            }
        }
        match token {
            Token::Newline => self.spot = self.after_separator(),
            Token::Op(op) => self.after_op(op),
            Token::Word(word) => self.after_word(word),
        }
    }

    /// After `;`, `&&`, `|`, `&`, `(` or a newline: a command name, unless a
    /// `case` pattern list is still waiting for a pattern, where `|` alternates
    /// patterns and `(` only opens the list.
    fn after_separator(&self) -> Spot {
        if self.in_patterns() {
            Spot::Word
        } else {
            Spot::Command
        }
    }

    fn after_op(&mut self, op: &Op) {
        self.spot = match op {
            Op::Semi | Op::And | Op::Or | Op::Amp | Op::Pipe | Op::LParen => self.after_separator(),
            // `)` closes a pattern list and the arm's body follows, or it ends
            // a subshell — after either, only an operator may come.
            Op::RParen => {
                if let Some(patterns) = self.case_patterns.last_mut() {
                    *patterns = false;
                }
                Spot::Command
            }
            // `;;` ends a case arm: the next words are patterns again.
            Op::DoubleSemi => {
                if let Some(patterns) = self.case_patterns.last_mut() {
                    *patterns = true;
                }
                Spot::Word
            }
            Op::Redir(kind) => {
                if takes_target(kind) {
                    self.redir_target = true;
                    self.after_redir = self.spot;
                    Spot::Word
                } else {
                    // `2>&1` and `<<EOF` have no target word, so the command
                    // name may still come (`2>&1 cmd`).
                    self.spot
                }
            }
        };
    }

    fn after_word(&mut self, word: &Word) {
        // `esac` ends a case whether or not a command name was expected, so
        // the frame stack stays balanced (`case x in a) : ;; esac`).
        if word.plain_literal() == Some("esac") && !self.case_patterns.is_empty() {
            self.case_patterns.pop();
            self.spot = Spot::Word;
            return;
        }
        // Inside a pattern list nothing is a command name, not even a keyword:
        // a pattern may be spelled `done` or `then`.
        if self.in_patterns() {
            self.spot = Spot::Word;
            return;
        }
        // A command name may still follow an assignment prefix (`X=1 cmd`).
        if self.spot == Spot::Command && word.as_assignment().is_some() {
            return;
        }
        // Reserved words are only reserved in command position: after `echo`,
        // the word `then` is an argument and does not make the next word a
        // command name.
        let keyword = match self.spot {
            Spot::Command => word.plain_literal(),
            Spot::Word => None,
        };
        self.spot = match keyword {
            Some("if" | "while" | "until" | "then" | "do" | "else" | "elif" | "!" | "{") => {
                Spot::Command
            }
            Some("case") => {
                self.case_patterns.push(true);
                Spot::Word
            }
            _ => Spot::Word,
        };
    }
}

/// Whether a redirection is followed by a target word (`> file`) or not
/// (`2>&1`, `<<EOF`).
fn takes_target(kind: &RedirOp) -> bool {
    !matches!(kind, RedirOp::Dup { .. } | RedirOp::HereDoc { .. })
}

/// Aliases created by the pre-1.2 `alias create` builder were stored with
/// every token re-quoted (`ls -la '&&' x`), which killed the operator
/// semantics of `&&`, `||`, `|`, `;`, `<` and `>`. Such bodies are un-quoted
/// here so the operators come back to life.
pub fn unmigrate_stored_body(body: &str) -> String {
    tokenize_stored_body(body).join(" ")
}

fn tokenize_stored_body(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        if chars[i] == '\'' {
            // Single-quoted token.
            i += 1;
            let start = i;
            while i < chars.len() && chars[i] != '\'' {
                i += 1;
            }
            let content: String = chars[start..i].iter().collect();
            if i < chars.len() {
                i += 1; // closing quote
            }
            if is_operator_content(&content) {
                tokens.push(content);
            } else {
                tokens.push(format!("'{content}'"));
            }
        } else if chars[i] == '"' {
            i += 1;
            let start = i;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            let content: String = chars[start..i].iter().collect();
            if i < chars.len() {
                i += 1;
            }
            tokens.push(format!("\"{content}\""));
        } else {
            let start = i;
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            tokens.push(chars[start..i].iter().collect());
        }
    }
    tokens
}

fn is_operator_content(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut has_operator = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            continue;
        }
        if matches!(c, '|' | '&' | ';' | '<' | '>') {
            has_operator = true;
            continue;
        }
        return false;
    }
    has_operator
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::lexer::Token;

    fn aliases(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, body)| (name.to_string(), body.to_string()))
            .collect()
    }

    fn expand(text: &str, pairs: &[(&str, &str)]) -> String {
        let tokens = lexer::lex(text).unwrap().tokens;
        // Render the token stream back to a comparable shape.
        let expanded = expand_tokens(tokens, &aliases(pairs));
        expanded
            .iter()
            .map(|token| match token {
                Token::Word(word) => word.literal_text().unwrap_or_else(|| "<exp>".to_string()),
                Token::Newline => ";".to_string(),
                Token::Op(op) => format!("{op:?}"),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Did `al` (defined as `echo AL`) expand in `text`?
    fn expands_al(text: &str) -> bool {
        expand(text, &[("al", "echo AL")]).contains("echo AL")
    }

    /// Did `x` (defined as `echo hi`) expand in `text`?
    fn expands_x(text: &str) -> bool {
        expand(text, &[("x", "echo hi")]).contains("echo hi")
    }

    #[test]
    fn aliases_expand_in_command_position_only() {
        let out = expand("ll x", &[("ll", "ls -la")]);
        assert!(out.starts_with("ls -la x"), "{out}");
        let out = expand("echo ll", &[("ll", "ls -la")]);
        assert_eq!(out, "echo ll");
    }

    #[test]
    fn commands_are_expanded_after_separators_keywords_and_prefixes() {
        // Every one of these runs a command named `al` in bash, so it expands.
        for line in [
            "al",
            "true; al",
            "true && al",
            "false || al",
            "true | al",
            "al & al",
            "( al )",
            "{ al; }",
            "! al",
            "if al; then :; fi",
            "if true; then al; fi",
            "if false; then :; else al; fi",
            "while al; do :; done",
            "until al; do :; done",
            "for k in 1; do al; done",
            "for k in 1\ndo al\ndone",
            "X=1 al",
            "X=1 > /tmp/ash-alias-test al",
            "> /tmp/ash-alias-test al",
            "2>&1 al",
        ] {
            assert!(expands_al(line), "{line:?} should expand");
        }
    }

    #[test]
    fn arguments_and_targets_are_never_expanded() {
        for line in [
            "echo al",
            "echo then al",
            "echo do al",
            "echo case al",
            "echo esac al",
            "echo data > al",
            "echo data 2> al",
            "echo data 2>&1 al",
            "echo X=1 al",
            "X=al",
            "for al in a b; do :; done",
            "for k in al; do :; done",
            "case al in *) echo hit;; esac",
        ] {
            assert!(!expands_al(line), "{line:?} should not expand");
        }
    }

    #[test]
    fn case_patterns_are_not_command_positions() {
        // The pattern never expands — with or without the optional `(`, across
        // lines, and with `|` between patterns — while the arm body does.
        for line in [
            "case x in a) :;; x) :;; esac",
            "case x in (x) :;; esac",
            "case x in a|x) :;; esac",
            "case x in\nx) :\n;;\nesac",
            "case x in\n  a |\n  x) :;;\nesac",
        ] {
            assert!(!expands_x(line), "{line:?} should not expand the pattern");
        }
        for line in [
            "case x in x) al;; esac",
            "case x in a) :;; x) al;; esac",
            "case x in x) al\n;;\nesac",
        ] {
            assert!(!expands_x(line), "{line:?} should not expand the pattern");
            assert!(expands_al(line), "{line:?} should expand the arm body");
        }
    }

    #[test]
    fn nested_cases_and_the_shell_after_them_keep_their_positions() {
        let line = "case x in x) case x in a) :;; x) al;; esac ;; esac";
        assert!(!expands_x(line), "{line}");
        assert!(expands_al(line), "{line}");
        // `esac` balances on one line, so the subshell after it is still a
        // command position.
        assert!(expands_al("case x in a) :;; esac; ( al )"));
    }

    #[test]
    fn a_body_ending_in_an_operator_keeps_expecting_a_command() {
        let out = expand("trail al", &[("trail", "echo TRAIL &&"), ("al", "echo AL")]);
        assert!(out.contains("echo TRAIL"), "{out}");
        assert!(out.contains("echo AL"), "{out}");
    }

    #[test]
    fn quoted_alias_names_do_not_expand() {
        let out = expand("'ll' x", &[("ll", "ls -la")]);
        assert!(out.starts_with("ll x"), "{out}");
    }

    #[test]
    fn operators_inside_bodies_stay_operators() {
        let out = expand("gs", &[("gs", "git status && echo done")]);
        assert!(out.contains("And"), "{out}");
        assert!(out.contains("echo"), "{out}");
    }

    #[test]
    fn aliases_expand_after_keywords() {
        let out = expand("if true; then ll; fi", &[("ll", "ls -la")]);
        assert!(out.contains("ls -la"), "{out}");
    }

    #[test]
    fn legacy_quoted_bodies_are_unmigrated() {
        let out = expand("gs", &[("gs", "'git' 'status' '&&' 'echo' 'done'")]);
        assert!(out.contains("And"), "{out}");
    }

    #[test]
    fn recursive_aliases_terminate() {
        let out = expand("a", &[("a", "b"), ("b", "a")]);
        assert!(!out.is_empty());
    }

    #[test]
    fn stored_bodies_keep_quotes_except_for_operators() {
        assert_eq!(
            tokenize_stored_body("ls -la '&&' x"),
            vec!["ls", "-la", "&&", "x"]
        );
        assert_eq!(tokenize_stored_body("'echo hi'"), vec!["'echo hi'"]);
        assert_eq!(
            unmigrate_stored_body("'echo' 'a' '&&' 'echo' 'b'"),
            "'echo' 'a' && 'echo' 'b'"
        );
        assert!(is_operator_content("2>&1"));
        assert!(is_operator_content("&&"));
        assert!(!is_operator_content("ls"));
        assert!(!is_operator_content(""));
    }
}
