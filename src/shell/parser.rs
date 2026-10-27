//! Parser: token stream into the command tree the executor walks.
//!
//! The grammar covers the POSIX constructs a script realistically uses:
//! pipelines joined by `&&`/`||`/`;`/`&`, simple commands with assignments and
//! redirections, functions, `if`/`elif`/`else`, `while`/`until`, `for`, `case`,
//! `{ …; }` groups and `( … )` subshells.
//!
//! Reserved words are only recognized in command position, exactly as POSIX
//! requires, so `echo then` passes `then` through as an ordinary argument.

use super::alias;
use super::lexer::{self, LexError, Op, RedirOp, Token, Word};

#[derive(Debug)]
pub enum ParseError {
    /// The input ended in the middle of a construct; an interactive shell asks
    /// for another line.
    Incomplete,
    Message(String),
}

impl From<LexError> for ParseError {
    fn from(error: LexError) -> Self {
        match error {
            LexError::Incomplete => ParseError::Incomplete,
            LexError::Message(message) => ParseError::Message(message),
        }
    }
}

// ---------------------------------------------------------------------------
// Syntax tree
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Program {
    pub items: Vec<ListItem>,
}

impl Program {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct ListItem {
    pub and_or: AndOr,
    /// `cmd &` — run without waiting.
    pub background: bool,
}

#[derive(Clone, Debug)]
pub struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(Join, Pipeline)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
    And,
    Or,
}

#[derive(Clone, Debug)]
pub struct Pipeline {
    pub negated: bool,
    pub commands: Vec<Command>,
}

#[derive(Clone, Debug)]
pub enum Command {
    Simple(Simple),
    Compound(Compound),
    Function {
        name: String,
        body: Box<Compound>,
    },
}

impl Command {
    pub fn redirs(&self) -> &[Redir] {
        match self {
            Command::Simple(simple) => &simple.redirs,
            Command::Compound(compound) => &compound.redirs,
            Command::Function { body, .. } => &body.redirs,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Simple {
    pub assigns: Vec<Assignment>,
    pub words: Vec<Word>,
    pub redirs: Vec<Redir>,
}

#[derive(Clone, Debug)]
pub struct Assignment {
    pub name: String,
    pub value: Word,
}

#[derive(Clone, Debug)]
pub struct Redir {
    pub target: Word,
    pub kind: RedirOp,
    /// Heredoc input, already collected by the lexer.
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct Compound {
    pub kind: CompoundKind,
    pub redirs: Vec<Redir>,
}

#[derive(Clone, Debug)]
pub enum CompoundKind {
    If {
        branches: Vec<(Program, Program)>,
        else_body: Option<Program>,
    },
    Loop {
        cond: Program,
        body: Program,
        until: bool,
    },
    For {
        name: String,
        words: Option<Vec<Word>>,
        body: Program,
    },
    Case {
        word: Word,
        arms: Vec<CaseArm>,
    },
    Group(Program),
    Subshell(Program),
}

#[derive(Clone, Debug)]
pub struct CaseArm {
    pub patterns: Vec<Word>,
    pub body: Program,
}

// ---------------------------------------------------------------------------
// Reserved words
// ---------------------------------------------------------------------------

const RESERVED: &[&str] = &[
    "if", "then", "elif", "else", "fi", "while", "until", "for", "in", "do", "done", "case",
    "esac", "{", "}", "!",
];

pub fn is_reserved(word: &str) -> bool {
    RESERVED.contains(&word)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub fn parse(text: &str) -> Result<Program, ParseError> {
    let lexed = lexer::lex(text)?;
    let aliases = crate::config::active_config()
        .map(|config| config.aliases)
        .unwrap_or_default();
    let tokens = alias::expand_tokens(lexed.tokens, &aliases);
    let mut parser = Parser { toks: tokens, pos: 0 };
    let program = parser.parse_program(&[])?;
    parser.skip_newlines();
    if let Some(token) = parser.toks.get(parser.pos) {
        let name = token_name(token);
        return Err(ParseError::Message(format!(
            "ash: syntax error near unexpected token '{name}'"
        )));
    }
    Ok(program)
}

/// Parse a single word fragment as used by `${x:-…}` defaults. Never fails.
pub fn parse_fragment(text: &str) -> Word {
    lexer::lex_fragment(text)
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

type PResult<T> = Result<T, ParseError>;

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(Token::Newline)) {
            self.pos += 1;
        }
    }

    fn skip_separators(&mut self) {
        while matches!(
            self.peek(),
            Some(Token::Newline) | Some(Token::Op(Op::Semi))
        ) {
            self.pos += 1;
        }
    }

    fn peek_op(&self) -> Option<Op> {
        match self.peek() {
            Some(Token::Op(op)) => Some(op.clone()),
            _ => None,
        }
    }

    fn eat_op(&mut self, op: &Op) -> bool {
        if self.peek_op().as_ref() == Some(op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// The plain literal of the token at the cursor, if it is an unquoted word.
    fn peek_lit(&self) -> Option<&str> {
        match self.peek() {
            Some(Token::Word(word)) => word.plain_literal(),
            _ => None,
        }
    }

    fn eat_lit(&mut self, lit: &str) -> bool {
        if self.peek_lit() == Some(lit) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn at_stop(&self, stops: &[&str]) -> bool {
        self.peek_lit().is_some_and(|lit| stops.contains(&lit))
    }

    fn error_here(&self) -> ParseError {
        match self.peek() {
            Some(token) => ParseError::Message(format!(
                "ash: syntax error near unexpected token '{}'",
                token_name(token)
            )),
            None => ParseError::Incomplete,
        }
    }

    fn expect_op(&mut self, op: Op, what: &str) -> PResult<()> {
        if self.eat_op(&op) {
            return Ok(());
        }
        match self.peek() {
            None => Err(ParseError::Incomplete),
            Some(_) => Err(ParseError::Message(format!(
                "ash: syntax error: expected '{what}' near unexpected token '{}'",
                token_name(self.peek().expect("checked"))
            ))),
        }
    }

    fn expect_lit(&mut self, lit: &str) -> PResult<()> {
        if self.eat_lit(lit) {
            return Ok(());
        }
        match self.peek() {
            None => Err(ParseError::Incomplete),
            Some(_) => Err(ParseError::Message(format!(
                "ash: syntax error: expected '{lit}' near unexpected token '{}'",
                token_name(self.peek().expect("checked"))
            ))),
        }
    }

    fn take_word(&mut self, what: &str) -> PResult<Word> {
        match self.peek() {
            Some(Token::Word(word)) => {
                let word = word.clone();
                self.pos += 1;
                Ok(word)
            }
            None => Err(ParseError::Incomplete),
            Some(_) => Err(ParseError::Message(format!(
                "ash: syntax error: expected {what} near unexpected token '{}'",
                token_name(self.peek().expect("checked"))
            ))),
        }
    }

    // -- programs -----------------------------------------------------------

    /// Parse list items until the input ends, a stop word appears in command
    /// position, `;;` shows up (a case arm boundary), or a `)` closes a
    /// subshell.
    fn parse_program(&mut self, stops: &[&str]) -> PResult<Program> {
        let mut items = Vec::new();
        loop {
            self.skip_separators();
            if self.at_end()
                || self.at_stop(stops)
                || self.peek_op() == Some(Op::DoubleSemi)
                || self.peek_op() == Some(Op::RParen)
            {
                break;
            }
            let and_or = self.parse_and_or(stops)?;
            let background = if self.eat_op(&Op::Amp) {
                true
            } else {
                self.eat_op(&Op::Semi);
                false
            };
            items.push(ListItem { and_or, background });
        }
        Ok(Program { items })
    }

    fn parse_and_or(&mut self, stops: &[&str]) -> PResult<AndOr> {
        let first = self.parse_pipeline(stops)?;
        let mut rest = Vec::new();
        loop {
            let join = if self.eat_op(&Op::And) {
                Join::And
            } else if self.eat_op(&Op::Or) {
                Join::Or
            } else {
                break;
            };
            self.skip_newlines();
            let pipeline = self.parse_pipeline(stops)?;
            rest.push((join, pipeline));
        }
        Ok(AndOr { first, rest })
    }

    fn parse_pipeline(&mut self, stops: &[&str]) -> PResult<Pipeline> {
        let negated = self.eat_lit("!");
        if negated {
            self.skip_newlines();
        }
        let mut commands = vec![self.parse_command(stops)?];
        while self.eat_op(&Op::Pipe) {
            self.skip_newlines();
            commands.push(self.parse_command(stops)?);
        }
        Ok(Pipeline {
            negated,
            commands,
        })
    }

    fn parse_command(&mut self, stops: &[&str]) -> PResult<Command> {
        if self.function_ahead() {
            let Some(Token::Word(word)) = self.peek() else {
                return Err(self.error_here());
            };
            let name = word
                .literal_text()
                .ok_or_else(|| self.error_here())?;
            self.pos += 2; // the name and `(`
            self.expect_op(Op::RParen, ")")?;
            self.skip_newlines();
            let body = self.parse_compound(stops, &[])?;
            return Ok(Command::Function {
                name,
                body: Box::new(body),
            });
        }

        if self.peek_op() == Some(Op::LParen) {
            self.pos += 1;
            let body = self.parse_program(&union_stops(stops, &[]))?;
            self.expect_op(Op::RParen, ")")?;
            let redirs = self.parse_trailing_redirs()?;
            return Ok(Command::Compound(Compound {
                kind: CompoundKind::Subshell(body),
                redirs,
            }));
        }

        match self.peek_lit().map(str::to_string).as_deref() {
            Some(lit) if compound_starter(lit) => {
                let compound = self.parse_compound(stops, &[])?;
                return Ok(Command::Compound(compound));
            }
            _ => {}
        }

        let simple = self.parse_simple(stops)?;
        if simple.words.is_empty() && simple.assigns.is_empty() && simple.redirs.is_empty() {
            return Err(self.error_here());
        }
        Ok(Command::Simple(simple))
    }

    /// Whether the cursor is on `name ( )`, a function definition.
    fn function_ahead(&self) -> bool {
        let Some(Token::Word(word)) = self.toks.get(self.pos) else {
            return false;
        };
        let Some(name) = word.literal_text() else {
            return false;
        };
        if name.is_empty() || is_reserved(&name) {
            return false;
        }
        matches!(
            (self.toks.get(self.pos + 1), self.toks.get(self.pos + 2)),
            (Some(Token::Op(Op::LParen)), Some(Token::Op(Op::RParen)))
        )
    }

    fn parse_simple(&mut self, stops: &[&str]) -> PResult<Simple> {
        let mut simple = Simple::default();
        loop {
            let token = self.peek().cloned();
            match token {
                None | Some(Token::Newline) => break,
                Some(Token::Op(op)) => match op {
                    Op::Semi
                    | Op::Amp
                    | Op::And
                    | Op::Or
                    | Op::Pipe
                    | Op::DoubleSemi
                    | Op::RParen => break,
                    Op::LParen => {
                        return Err(ParseError::Message(
                            "ash: syntax error near unexpected token '('".to_string(),
                        ));
                    }
                    Op::Redir(kind) => {
                        self.pos += 1;
                        let redir = self.parse_redir(kind)?;
                        simple.redirs.push(redir);
                    }
                },
                Some(Token::Word(word)) => {
                    if simple.words.is_empty() && self.at_stop(stops) {
                        break;
                    }
                    if simple.words.is_empty()
                        && let Some((name, value)) = word.as_assignment()
                    {
                        simple.assigns.push(Assignment { name, value });
                        self.pos += 1;
                        continue;
                    }
                    simple.words.push(word);
                    self.pos += 1;
                }
            }
        }
        Ok(simple)
    }

    fn parse_redir(&mut self, kind: RedirOp) -> PResult<Redir> {
        match kind {
            RedirOp::HereDoc {
                strip_tabs,
                quoted,
                delimiter,
                body,
            } => Ok(Redir {
                target: Word::default(),
                kind: RedirOp::HereDoc {
                    strip_tabs,
                    quoted,
                    delimiter,
                    body,
                },
                body: String::new(),
            }),
            RedirOp::Dup { .. } => Ok(Redir {
                target: Word::default(),
                kind,
                body: String::new(),
            }),
            other => {
                let target = self.take_word("a redirection target")?;
                Ok(Redir {
                    target,
                    kind: other,
                    body: String::new(),
                })
            }
        }
    }

    fn parse_trailing_redirs(&mut self) -> PResult<Vec<Redir>> {
        let mut redirs = Vec::new();
        while let Some(Token::Op(Op::Redir(kind))) = self.peek() {
            let kind = kind.clone();
            self.pos += 1;
            redirs.push(self.parse_redir(kind)?);
        }
        Ok(redirs)
    }

    /// Parse a compound command (and its trailing redirections) starting at
    /// the cursor.
    fn parse_compound(&mut self, stops: &[&str], _reserved: &[&str]) -> PResult<Compound> {
        let kind = match self.peek_lit() {
            Some("if") => self.parse_if(stops)?,
            Some("while") => self.parse_loop(stops, false)?,
            Some("until") => self.parse_loop(stops, true)?,
            Some("for") => self.parse_for(stops)?,
            Some("case") => self.parse_case(stops)?,
            Some("{") => {
                self.pos += 1;
                let body = self.parse_program(&union_stops(stops, &["}"]))?;
                self.expect_lit("}")?;
                CompoundKind::Group(body)
            }
            _ => return Err(self.error_here()),
        };
        let redirs = self.parse_trailing_redirs()?;
        Ok(Compound { kind, redirs })
    }

    fn parse_if(&mut self, stops: &[&str]) -> PResult<CompoundKind> {
        self.expect_lit("if")?;
        let cond = self.parse_program(&union_stops(stops, &["then"]))?;
        self.expect_lit("then")?;
        let body = self.parse_program(&union_stops(stops, &["elif", "else", "fi"]))?;
        let mut branches = vec![(cond, body)];
        while self.eat_lit("elif") {
            let cond = self.parse_program(&union_stops(stops, &["then"]))?;
            self.expect_lit("then")?;
            let body = self.parse_program(&union_stops(stops, &["elif", "else", "fi"]))?;
            branches.push((cond, body));
        }
        let else_body = if self.eat_lit("else") {
            Some(self.parse_program(&union_stops(stops, &["fi"]))?)
        } else {
            None
        };
        self.expect_lit("fi")?;
        Ok(CompoundKind::If {
            branches,
            else_body,
        })
    }

    fn parse_loop(&mut self, stops: &[&str], until: bool) -> PResult<CompoundKind> {
        self.pos += 1; // while / until
        let cond = self.parse_program(&union_stops(stops, &["do"]))?;
        self.expect_lit("do")?;
        let body = self.parse_program(&union_stops(stops, &["done"]))?;
        self.expect_lit("done")?;
        Ok(CompoundKind::Loop { cond, body, until })
    }

    fn parse_for(&mut self, stops: &[&str]) -> PResult<CompoundKind> {
        self.expect_lit("for")?;
        let name_word = self.take_word("a loop variable name")?;
        let name = name_word
            .literal_text()
            .filter(|name| lexer::is_valid_name(name))
            .ok_or_else(|| {
                ParseError::Message("ash: syntax error: bad for loop variable".to_string())
            })?;
        self.skip_separators();
        let words = if self.eat_lit("in") {
            let mut words = Vec::new();
            while let Some(Token::Word(word)) = self.peek() {
                words.push(word.clone());
                self.pos += 1;
            }
            self.skip_separators();
            Some(words)
        } else {
            None
        };
        self.expect_lit("do")?;
        let body = self.parse_program(&union_stops(stops, &["done"]))?;
        self.expect_lit("done")?;
        Ok(CompoundKind::For { name, words, body })
    }

    fn parse_case(&mut self, stops: &[&str]) -> PResult<CompoundKind> {
        self.expect_lit("case")?;
        let word = self.take_word("a case word")?;
        self.skip_newlines();
        self.expect_lit("in")?;
        let mut arms = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat_lit("esac") {
                break;
            }
            if self.at_end() {
                return Err(ParseError::Incomplete);
            }
            // An optional `(` may open the pattern list.
            let _ = self.eat_op(&Op::LParen);
            let mut patterns = Vec::new();
            loop {
                patterns.push(self.take_word("a case pattern")?);
                if !self.eat_op(&Op::Pipe) {
                    break;
                }
            }
            self.expect_op(Op::RParen, ")")?;
            let body = self.parse_program(&union_stops(stops, &["esac"]))?;
            arms.push(CaseArm { patterns, body });
            if self.eat_op(&Op::DoubleSemi) {
                continue;
            }
            if self.eat_lit("esac") {
                break;
            }
            return Err(self.error_here());
        }
        Ok(CompoundKind::Case { word, arms })
    }
}

fn compound_starter(lit: &str) -> bool {
    matches!(lit, "if" | "while" | "until" | "for" | "case" | "{")
}

/// Union of two stop-word sets, without duplicates.
fn union_stops<'a>(outer: &[&'a str], extra: &[&'a str]) -> Vec<&'a str> {
    let mut stops: Vec<&str> = outer.to_vec();
    for word in extra {
        if !stops.contains(word) {
            stops.push(word);
        }
    }
    stops
}

fn token_name(token: &Token) -> String {
    match token {
        Token::Word(word) => word
            .plain_literal()
            .map(str::to_string)
            .unwrap_or_else(|| "word".to_string()),
        Token::Newline => "newline".to_string(),
        Token::Op(op) => match op {
            Op::Semi => ";".to_string(),
            Op::DoubleSemi => ";;".to_string(),
            Op::And => "&&".to_string(),
            Op::Or => "||".to_string(),
            Op::Pipe => "|".to_string(),
            Op::Amp => "&".to_string(),
            Op::LParen => "(".to_string(),
            Op::RParen => ")".to_string(),
            Op::Redir(kind) => match kind {
                RedirOp::In => "<".to_string(),
                RedirOp::Out => ">".to_string(),
                RedirOp::Append => ">>".to_string(),
                RedirOp::ErrOut => "2>".to_string(),
                RedirOp::ErrAppend => "2>>".to_string(),
                RedirOp::OutErr => "&>".to_string(),
                RedirOp::Dup { .. } => ">&".to_string(),
                RedirOp::HereDoc { .. } => "<<".to_string(),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::lexer::Seg;

    fn parse_ok(line: &str) -> Program {
        parse(line).unwrap_or_else(|e| panic!("{line:?}: {e:?}"))
    }

    /// The words of every simple command, with expansions left untouched.
    fn words_of(line: &str) -> Vec<String> {
        let program = parse_ok(line);
        let mut out = Vec::new();
        for item in &program.items {
            collect_words(&item.and_or, &mut out);
        }
        out
    }

    fn collect_words(and_or: &AndOr, out: &mut Vec<String>) {
        let mut pipelines: Vec<&Pipeline> = vec![&and_or.first];
        pipelines.extend(and_or.rest.iter().map(|(_, p)| p));
        for pipeline in pipelines {
            for command in &pipeline.commands {
                if let Command::Simple(simple) = command {
                    for word in &simple.words {
                        out.push(word.literal_text().unwrap_or_else(|| "<exp>".to_string()));
                    }
                }
            }
        }
    }

    fn joins_of(line: &str) -> Vec<Join> {
        let program = parse_ok(line);
        program.items[0]
            .and_or
            .rest
            .iter()
            .map(|(join, _)| *join)
            .collect()
    }

    #[test]
    fn simple_commands_split_into_words() {
        assert_eq!(words_of("ls -la /tmp"), ["ls", "-la", "/tmp"]);
        assert_eq!(words_of(r#"echo "hello world" 'second arg'"#), [
            "echo",
            "hello world",
            "second arg"
        ]);
    }

    #[test]
    fn pipelines_and_sequencing() {
        let program = parse_ok("cat f | grep x | head -1");
        assert_eq!(program.items.len(), 1);
        assert_eq!(program.items[0].and_or.first.commands.len(), 3);
        assert_eq!(joins_of("a && b || c ; d"), [Join::And, Join::Or]);
        // `;` separates list items; `&&`/`||` stay inside one and-or list.
        assert_eq!(parse_ok("a && b || c ; d").items.len(), 2);
    }

    #[test]
    fn negation_and_background() {
        let program = parse_ok("! grep x f");
        assert!(program.items[0].and_or.first.negated);
        let program = parse_ok("sleep 1 & echo hi");
        assert!(program.items[0].background);
        assert!(!program.items[1].background);
    }

    #[test]
    fn assignments_are_separated_from_the_command() {
        let program = parse_ok("X=1 Y=2 cmd arg");
        let Command::Simple(simple) = &program.items[0].and_or.first.commands[0] else {
            panic!("expected simple command");
        };
        assert_eq!(
            simple
                .assigns
                .iter()
                .map(|a| a.name.clone())
                .collect::<Vec<_>>(),
            ["X", "Y"]
        );
        assert_eq!(simple.words.len(), 2);
        // After the command name, `A=b` is an ordinary argument.
        let program = parse_ok("echo X=1");
        let Command::Simple(simple) = &program.items[0].and_or.first.commands[0] else {
            panic!("expected simple command");
        };
        assert!(simple.assigns.is_empty());
        assert_eq!(simple.words.len(), 2);
    }

    #[test]
    fn redirections_keep_kind_and_target() {
        let program = parse_ok("echo hi > out.txt 2>> err.txt < in.txt &> all.txt 2>&1");
        let Command::Simple(simple) = &program.items[0].and_or.first.commands[0] else {
            panic!("expected simple command");
        };
        let kinds: Vec<RedirOp> = simple.redirs.iter().map(|r| r.kind.clone()).collect();
        assert_eq!(
            kinds,
            [
                RedirOp::Out,
                RedirOp::ErrAppend,
                RedirOp::In,
                RedirOp::OutErr,
                RedirOp::Dup { fd: 2, from: 1 },
            ]
        );
        assert_eq!(simple.redirs[0].target.literal_text().unwrap(), "out.txt");
    }

    #[test]
    fn reserved_words_are_only_reserved_in_command_position() {
        assert_eq!(words_of("echo then if fi"), ["echo", "then", "if", "fi"]);
        let program = parse_ok("if true; then echo yes; fi");
        assert!(matches!(
            program.items[0].and_or.first.commands[0],
            Command::Compound(Compound {
                kind: CompoundKind::If { .. },
                ..
            })
        ));
    }

    #[test]
    fn if_elif_else_and_loops() {
        let program = parse_ok("if a; then b; elif c; then d; else e; fi");
        let Command::Compound(compound) = &program.items[0].and_or.first.commands[0] else {
            panic!("expected if");
        };
        let CompoundKind::If {
            branches,
            else_body,
        } = &compound.kind
        else {
            panic!("expected if kind");
        };
        assert_eq!(branches.len(), 2);
        assert!(else_body.is_some());

        let program = parse_ok("while a; do b; done");
        let Command::Compound(compound) = &program.items[0].and_or.first.commands[0] else {
            panic!("expected while");
        };
        assert!(matches!(
            compound.kind,
            CompoundKind::Loop { until: false, .. }
        ));

        let program = parse_ok("until a; do b; done");
        let Command::Compound(compound) = &program.items[0].and_or.first.commands[0] else {
            panic!("expected until");
        };
        assert!(matches!(
            compound.kind,
            CompoundKind::Loop { until: true, .. }
        ));
    }

    #[test]
    fn for_loops_with_and_without_words() {
        let program = parse_ok("for i in a b; do echo $i; done");
        let Command::Compound(compound) = &program.items[0].and_or.first.commands[0] else {
            panic!("expected for");
        };
        let CompoundKind::For { name, words, .. } = &compound.kind else {
            panic!("expected for kind");
        };
        assert_eq!(name, "i");
        assert_eq!(words.as_ref().unwrap().len(), 2);

        let program = parse_ok("for i; do echo $i; done");
        let Command::Compound(compound) = &program.items[0].and_or.first.commands[0] else {
            panic!("expected for");
        };
        let CompoundKind::For { words, .. } = &compound.kind else {
            panic!("expected for kind");
        };
        assert!(words.is_none());
    }

    #[test]
    fn case_arms_group_patterns() {
        let program = parse_ok("case $x in a|b) echo lower;; *) echo other;; esac");
        let Command::Compound(compound) = &program.items[0].and_or.first.commands[0] else {
            panic!("expected case");
        };
        let CompoundKind::Case { arms, .. } = &compound.kind else {
            panic!("expected case kind");
        };
        assert_eq!(arms.len(), 2);
        assert_eq!(arms[0].patterns.len(), 2);
    }

    #[test]
    fn functions_are_recognized() {
        let program = parse_ok("f() { echo hi; }");
        assert!(matches!(
            program.items[0].and_or.first.commands[0],
            Command::Function { .. }
        ));
        let program = parse_ok("f () { echo hi; }");
        assert!(matches!(
            program.items[0].and_or.first.commands[0],
            Command::Function { .. }
        ));
    }

    #[test]
    fn groups_and_subshells() {
        let program = parse_ok("{ echo a; echo b; }");
        assert!(matches!(
            program.items[0].and_or.first.commands[0],
            Command::Compound(Compound {
                kind: CompoundKind::Group(_),
                ..
            })
        ));
        let program = parse_ok("(echo a; echo b)");
        assert!(matches!(
            program.items[0].and_or.first.commands[0],
            Command::Compound(Compound {
                kind: CompoundKind::Subshell(_),
                ..
            })
        ));
    }

    #[test]
    fn compounds_take_redirections() {
        let program = parse_ok("if true; then echo hi; fi > /tmp/out");
        let Command::Compound(compound) = &program.items[0].and_or.first.commands[0] else {
            panic!("expected compound");
        };
        assert_eq!(compound.redirs.len(), 1);
    }

    #[test]
    fn heredocs_reach_the_parser_with_their_bodies() {
        let program = parse_ok("cat <<EOF\none\ntwo\nEOF\n");
        let Command::Simple(simple) = &program.items[0].and_or.first.commands[0] else {
            panic!("expected simple");
        };
        let RedirOp::HereDoc { body, delimiter, .. } = &simple.redirs[0].kind else {
            panic!("expected heredoc");
        };
        assert_eq!(body, "one\ntwo\n");
        assert_eq!(delimiter, "EOF");
    }

    #[test]
    fn multi_line_compounds_parse() {
        let program = parse_ok("if true\nthen\n  echo yes\nfi\n");
        assert_eq!(program.items.len(), 1);
        let program = parse_ok("for i in 1 2 3\ndo\n  echo $i\ndone\n");
        assert_eq!(program.items.len(), 1);
    }

    #[test]
    fn incomplete_input_is_reported_as_incomplete() {
        for line in [
            "if true; then",
            "while a; do",
            "echo \"x",
            "cat <<EOF\nbody",
        ] {
            assert!(
                matches!(parse(line), Err(ParseError::Incomplete)),
                "{line:?} should be incomplete"
            );
        }
    }

    #[test]
    fn syntax_errors_name_the_offending_token() {
        for (line, token) in [
            ("| cat", "'|'"),
            (")", "')'"),
            ("a ;; b", "';;'"),
            ("echo (", "'('"),
            ("echo hi >", "end of file"),
        ] {
            match parse(line) {
                Err(ParseError::Incomplete) if token == "end of file" => {}
                Err(ParseError::Incomplete) => panic!("{line:?} unexpectedly incomplete"),
                Err(ParseError::Message(message)) => {
                    assert!(
                        token == "end of file" || message.contains(token),
                        "{line:?}: {message}"
                    );
                }
                Ok(program) => panic!("{line:?} parsed: {program:?}"),
            }
        }
    }

    #[test]
    fn expansion_segments_survive_parsing() {
        let program = parse_ok(r#"echo "$HOME"/x"#);
        let Command::Simple(simple) = &program.items[0].and_or.first.commands[0] else {
            panic!("expected simple");
        };
        assert!(matches!(simple.words[1].segs[0], Seg::Param { quoted: true, .. }));
    }
}
