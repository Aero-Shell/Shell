//! Alias expansion at the token level.
//!
//! Aliases are expanded the way POSIX describes: when a word in command
//! position matches an alias, the alias body is lexed and spliced into the
//! token stream, and the shell re-scans the result. Doing this on tokens (not
//! on the raw line) means keyword positions such as `then`, `do` and `else`
//! are already understood, while operators inside an alias body still behave
//! as operators.

use std::collections::HashMap;

use super::lexer::{self, Op, Token};

/// Maximum recursive expansion depth, so self-referencing aliases terminate.
const MAX_DEPTH: usize = 10;

/// Expand aliases in `tokens`, reading bodies from `aliases`.
pub fn expand_tokens(tokens: Vec<Token>, aliases: &HashMap<String, String>) -> Vec<Token> {
    let mut active: Vec<String> = Vec::new();
    expand_inner(tokens, aliases, &mut active, 0)
}

fn expand_inner(
    tokens: Vec<Token>,
    aliases: &HashMap<String, String>,
    active: &mut Vec<String>,
    depth: usize,
) -> Vec<Token> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut command_position = true;

    for token in tokens {
        if depth < MAX_DEPTH && command_position {
            if let Token::Word(word) = &token
                && let Some(name) = word.plain_literal()
                && !name.is_empty()
                && !super::parser::is_reserved(name)
                && !active.iter().any(|a| a == name)
                && let Some(body) = aliases.get(name)
            {
                let unmigrated = unmigrate_stored_body(body);
                if let Ok(lexed) = lexer::lex(&unmigrated) {
                    active.push(name.to_string());
                    let expanded = expand_inner(lexed.tokens, aliases, active, depth + 1);
                    active.pop();
                    if !expanded.is_empty() {
                        // The state after the splice follows the last token of
                        // the body, so a trailing `&&`/`;`/`|` still expects a
                        // command.
                        command_position = state_after(expanded.last().expect("non-empty"));
                        out.extend(expanded);
                        continue;
                    }
                }
            }
        }
        command_position = state_after(&token);
        out.push(token);
    }
    out
}

/// Whether the token after this one starts a command.
fn state_after(token: &Token) -> bool {
    match token {
        Token::Newline => true,
        Token::Op(op) => matches!(
            op,
            Op::Semi
                | Op::DoubleSemi
                | Op::And
                | Op::Or
                | Op::Pipe
                | Op::Amp
                | Op::LParen
                | Op::Redir(_)
        ),
        Token::Word(word) => matches!(word.plain_literal(), Some("then" | "do" | "else" | "elif" | "!" | "{")),
    }
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

    #[test]
    fn aliases_expand_in_command_position_only() {
        let out = expand("ll x", &[("ll", "ls -la")]);
        assert!(out.starts_with("ls -la x"), "{out}");
        let out = expand("echo ll", &[("ll", "ls -la")]);
        assert_eq!(out, "echo ll");
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
