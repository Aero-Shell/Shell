//! Lexer: shell text into tokens.
//!
//! Words keep their structure instead of being flattened to text: each word is
//! a list of [`Seg`]ments that remember whether they were quoted. Expansion
//! (see [`crate::shell::expand`]) needs that distinction — an unquoted
//! expansion is subject to field splitting and pathname expansion, a quoted
//! one is not.
//!
//! Heredoc bodies are not part of the token stream: when the lexer reaches the
//! newline that ends a line opening one or more `<<` redirects, it reads the
//! bodies from the following lines and stores them under the token index of
//! their `<<` operator.

// ---------------------------------------------------------------------------
// Word structure
// ---------------------------------------------------------------------------

/// A parameter reference: `$name`, `${name}`, `$1`, `$@`, `$?`, …
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamName {
    Named(String),
    Positional(usize),
    /// `?`, `#`, `$`, `!`, `*`, `@`, `-`.
    Special(char),
}

/// Which form of parameter expansion was written.
#[derive(Clone, Debug, PartialEq)]
pub enum ParamKind {
    /// `$name`, `${name}`
    Plain,
    /// `${#name}`
    Length,
    /// `${name:-word}` and friends. `op` is one of `-`, `=`, `+`, `?`;
    /// `colon` is set for the `:` variants (empty counts as unset).
    Op {
        op: char,
        colon: bool,
        word: Word,
    },
    /// `${name%word}`, `${name%%word}`, `${name#word}`, `${name##word}`.
    Trim {
        suffix: bool,
        longest: bool,
        pattern: Word,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParamExp {
    pub name: ParamName,
    pub kind: ParamKind,
}

/// One piece of a word. `quoted` marks text that came from `'…'`, `"…"`, or a
/// backslash escape; such text is never split and never globbed.
#[derive(Clone, Debug, PartialEq)]
pub enum Seg {
    Lit { text: String, quoted: bool },
    Param { exp: ParamExp, quoted: bool },
    CmdSub { text: String, quoted: bool },
    Arith { text: String, quoted: bool },
    /// A `${…}` construct this shell does not implement (`${x//a/b}`,
    /// `${x:2}`, …). It is kept as text and reported when the word expands,
    /// so a function definition or a branch that never runs stays harmless —
    /// profiles in the wild are full of bash-only spread.
    BadSub { text: String, quoted: bool },
    /// An unquoted `~` at the start of a word.
    Tilde,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Word {
    pub segs: Vec<Seg>,
}

impl Word {
    /// The text of a word made of one unquoted literal, e.g. a reserved word
    /// or a command name.
    pub fn plain_literal(&self) -> Option<&str> {
        match self.segs.as_slice() {
            [Seg::Lit {
                text,
                quoted: false,
            }] => Some(text),
            _ => None,
        }
    }

    /// The text of a word with no expansions, quotes removed.
    pub fn literal_text(&self) -> Option<String> {
        let mut out = String::new();
        for seg in &self.segs {
            match seg {
                Seg::Lit { text, .. } => out.push_str(text),
                _ => return None,
            }
        }
        Some(out)
    }

    /// Whether this word is empty (`""` is not: it has a quoted segment).
    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }

    /// If the word starts with `name=`, split it into the name and the value
    /// word (with the `name=` prefix removed). Only an unquoted leading
    /// literal can name a variable.
    pub fn as_assignment(&self) -> Option<(String, Word)> {
        let Some(Seg::Lit {
            text,
            quoted: false,
        }) = self.segs.first()
        else {
            return None;
        };
        let (name, rest) = text.split_once('=')?;
        if !is_valid_name(name) {
            return None;
        }
        let mut value = Word::default();
        if !rest.is_empty() {
            value.segs.push(Seg::Lit {
                text: rest.to_string(),
                quoted: false,
            });
        }
        value.segs.extend(self.segs[1..].iter().cloned());
        Some((name.to_string(), value))
    }
}

pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RedirOp {
    In,
    Out,
    Append,
    ErrOut,
    ErrAppend,
    OutErr,
    /// `2>&1` — duplicate fd `from` onto fd `fd`.
    Dup { fd: i32, from: i32 },
    /// `<<` / `<<-`. The delimiter word is consumed by the lexer and the body
    /// is filled in once the following lines have been read, so both travel
    /// with the token (alias expansion splices tokens around).
    HereDoc {
        strip_tabs: bool,
        delimiter: String,
        body: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Semi,
    DoubleSemi,
    And,
    Or,
    Pipe,
    Amp,
    LParen,
    RParen,
    Redir(RedirOp),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    Word(Word),
    Op(Op),
    Newline,
}

impl Token {
    pub fn is_op(&self, op: &Op) -> bool {
        matches!(self, Token::Op(found) if found == op)
    }
}

pub struct Lexed {
    pub tokens: Vec<Token>,
}

#[derive(Debug)]
pub enum LexError {
    /// Input ran out in the middle of a construct; an interactive shell can
    /// ask for more lines.
    Incomplete,
    Message(String),
}

type LexResult<T> = Result<T, LexError>;

// ---------------------------------------------------------------------------
// The lexer
// ---------------------------------------------------------------------------

struct PendingHere {
    token_index: usize,
    delimiter: String,
}

struct Lexer<'a> {
    chars: &'a [char],
    pos: usize,
    tokens: Vec<Token>,
    pending: Vec<PendingHere>,
}

pub fn lex(text: &str) -> LexResult<Lexed> {
    let chars: Vec<char> = text.chars().collect();
    let mut lexer = Lexer {
        chars: &chars,
        pos: 0,
        tokens: Vec::new(),
        pending: Vec::new(),
    };
    lexer.run()?;
    Ok(Lexed {
        tokens: lexer.tokens,
    })
}

/// Lex `text` as a single word fragment (the `word` in `${x:-word}`), where
/// whitespace and operators are ordinary characters.
pub fn lex_fragment(text: &str) -> Word {
    let chars: Vec<char> = text.chars().collect();
    let mut lexer = Lexer {
        chars: &chars,
        pos: 0,
        tokens: Vec::new(),
        pending: Vec::new(),
    };
    match lexer.scan_fragment() {
        Ok(word) => word,
        Err(_) => Word {
            segs: vec![Seg::Lit {
                text: text.to_string(),
                quoted: false,
            }],
        },
    }
}

impl Lexer<'_> {
    fn run(&mut self) -> LexResult<()> {
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            match ch {
                ' ' | '\t' | '\r' => self.pos += 1,
                '\n' => {
                    self.tokens.push(Token::Newline);
                    self.pos += 1;
                    self.read_pending_heredocs()?;
                }
                '#' if self.at_word_start() => {
                    while self.pos < self.chars.len() && self.chars[self.pos] != '\n' {
                        self.pos += 1;
                    }
                }
                '|' => {
                    self.pos += 1;
                    if self.peek() == Some('|') {
                        self.pos += 1;
                        self.tokens.push(Token::Op(Op::Or));
                    } else {
                        self.tokens.push(Token::Op(Op::Pipe));
                    }
                }
                '&' => {
                    self.pos += 1;
                    if self.peek() == Some('&') {
                        self.pos += 1;
                        self.tokens.push(Token::Op(Op::And));
                    } else if self.peek() == Some('>') {
                        self.pos += 1;
                        self.tokens.push(Token::Op(Op::Redir(RedirOp::OutErr)));
                    } else {
                        self.tokens.push(Token::Op(Op::Amp));
                    }
                }
                ';' => {
                    self.pos += 1;
                    if self.peek() == Some(';') {
                        self.pos += 1;
                        self.tokens.push(Token::Op(Op::DoubleSemi));
                    } else {
                        self.tokens.push(Token::Op(Op::Semi));
                    }
                }
                '<' => {
                    self.pos += 1;
                    if self.peek() == Some('<') {
                        self.pos += 1;
                        let strip_tabs = self.peek() == Some('-');
                        if strip_tabs {
                            self.pos += 1;
                        }
                        self.scan_heredoc(strip_tabs)?;
                    } else {
                        self.tokens.push(Token::Op(Op::Redir(RedirOp::In)));
                    }
                }
                '>' => {
                    self.pos += 1;
                    self.scan_redirect(1)?;
                }
                '(' => {
                    self.pos += 1;
                    self.tokens.push(Token::Op(Op::LParen));
                }
                ')' => {
                    self.pos += 1;
                    self.tokens.push(Token::Op(Op::RParen));
                }
                d if d.is_ascii_digit() && self.redirect_fd_ahead() => {
                    let mut j = self.pos;
                    while j < self.chars.len() && self.chars[j].is_ascii_digit() {
                        j += 1;
                    }
                    let fd: i32 = self.chars[self.pos..j]
                        .iter()
                        .collect::<String>()
                        .parse()
                        .unwrap_or(1);
                    self.pos = j + 1; // skip the `>`
                    self.scan_redirect(fd)?;
                }
                _ => {
                    let word = self.scan_word()?;
                    if !word.is_empty() {
                        self.tokens.push(Token::Word(word));
                    }
                }
            }
        }
        if !self.pending.is_empty() {
            return Err(LexError::Incomplete);
        }
        Ok(())
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn at_word_start(&self) -> bool {
        if self.pos == 0 {
            return true;
        }
        let prev = self.chars[self.pos - 1];
        prev.is_whitespace() || matches!(prev, '|' | '&' | ';' | '<' | '>' | '(' | ')')
    }

    /// Whether the digit run starting here is immediately followed by `>` and
    /// so names a descriptor, as in `2>file`.
    fn redirect_fd_ahead(&self) -> bool {
        let mut j = self.pos;
        while j < self.chars.len() && self.chars[j].is_ascii_digit() {
            j += 1;
        }
        self.chars.get(j) == Some(&'>')
    }

    /// Parse the redirect operator after the `>` at the cursor was consumed,
    /// with `fd` the descriptor it redirects.
    fn scan_redirect(&mut self, fd: i32) -> LexResult<()> {
        let op = if self.peek() == Some('>') {
            self.pos += 1;
            match fd {
                1 => RedirOp::Append,
                _ => RedirOp::ErrAppend,
            }
        } else if self.peek() == Some('&') && self.chars.get(self.pos + 1).is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
            let mut from = String::new();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    from.push(c);
                    self.pos += 1;
                } else {
                    break;
                }
            }
            RedirOp::Dup {
                fd,
                from: from.parse().unwrap_or(1),
            }
        } else {
            match fd {
                1 => RedirOp::Out,
                _ => RedirOp::ErrOut,
            }
        };
        self.tokens.push(Token::Op(Op::Redir(op)));
        Ok(())
    }

    fn scan_heredoc(&mut self, strip_tabs: bool) -> LexResult<()> {
        let token_index = self.tokens.len();
        self.tokens.push(Token::Op(Op::Redir(RedirOp::HereDoc {
            strip_tabs,
            delimiter: String::new(),
            body: String::new(),
        })));
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.pos += 1;
        }
        let start = self.pos;
        let word = self.scan_word()?;
        if word.is_empty() {
            return if self.pos >= self.chars.len() {
                Err(LexError::Incomplete)
            } else {
                Err(LexError::Message(
                    "ash: syntax error near unexpected token '<<'".to_string(),
                ))
            };
        }
        let delimiter = word
            .literal_text()
            .unwrap_or_else(|| self.chars[start..self.pos].iter().collect());
        self.pending.push(PendingHere {
            token_index,
            delimiter,
        });
        Ok(())
    }

    /// Read the bodies of the heredocs opened on the line that just ended and
    /// patch each body into its `<<` token.
    fn read_pending_heredocs(&mut self) -> LexResult<()> {
        for spec in std::mem::take(&mut self.pending) {
            let mut body = String::new();
            let strip_tabs = match &self.tokens[spec.token_index] {
                Token::Op(Op::Redir(RedirOp::HereDoc { strip_tabs, .. })) => *strip_tabs,
                _ => false,
            };
            loop {
                if self.pos >= self.chars.len() {
                    return Err(LexError::Incomplete);
                }
                let start = self.pos;
                while self.pos < self.chars.len() && self.chars[self.pos] != '\n' {
                    self.pos += 1;
                }
                let line: String = self.chars[start..self.pos].iter().collect();
                let ended = self.pos < self.chars.len();
                if ended {
                    self.pos += 1;
                }
                let matching = if strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if matching == spec.delimiter {
                    if let Token::Op(Op::Redir(RedirOp::HereDoc {
                        delimiter,
                        body: slot,
                        ..
                    })) = &mut self.tokens[spec.token_index]
                    {
                        *delimiter = spec.delimiter.clone();
                        *slot = body;
                    }
                    break;
                }
                body.push_str(matching);
                body.push('\n');
                if !ended {
                    return Err(LexError::Incomplete);
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Words
    // -----------------------------------------------------------------------

    fn append_lit(&self, word: &mut Word, text: &str, quoted: bool) {
        if text.is_empty() && !quoted {
            return;
        }
        match word.segs.last_mut() {
            Some(Seg::Lit {
                text: last,
                quoted: last_quoted,
            }) if *last_quoted == quoted => last.push_str(text),
            _ => word.segs.push(Seg::Lit {
                text: text.to_string(),
                quoted,
            }),
        }
    }

    fn scan_word(&mut self) -> LexResult<Word> {
        let mut word = Word::default();
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            match ch {
                c if c.is_whitespace() => break,
                '|' | '&' | ';' | '<' | '>' | '(' | ')' => break,
                '\'' => {
                    self.pos += 1;
                    let text = self.read_single_quoted()?;
                    self.append_lit(&mut word, &text, true);
                }
                '"' => {
                    self.pos += 1;
                    let before = word.segs.len();
                    self.scan_double_quoted(&mut word)?;
                    // `""` is an empty field, not a missing word: it needs a
                    // segment of its own so the token survives.
                    if word.segs.len() == before {
                        self.append_lit(&mut word, "", true);
                    }
                }
                '\\' => {
                    self.pos += 1;
                    self.scan_escape(&mut word)?;
                }
                '$' => self.scan_dollar(&mut word, false)?,
                '`' => self.scan_backtick(&mut word, false)?,
                '~' if word.is_empty() => {
                    word.segs.push(Seg::Tilde);
                    self.pos += 1;
                }
                c => {
                    self.append_lit(&mut word, &c.to_string(), false);
                    self.pos += 1;
                }
            }
        }
        Ok(word)
    }

    /// Like [`Lexer::scan_word`] but runs to the end of the input, treating
    /// whitespace and operator characters as literal text.
    fn scan_fragment(&mut self) -> LexResult<Word> {
        let mut word = Word::default();
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            match ch {
                '\'' => {
                    self.pos += 1;
                    let text = self.read_single_quoted()?;
                    self.append_lit(&mut word, &text, true);
                }
                '"' => {
                    self.pos += 1;
                    let before = word.segs.len();
                    self.scan_double_quoted(&mut word)?;
                    // `""` is an empty field, not a missing word: it needs a
                    // segment of its own so the token survives.
                    if word.segs.len() == before {
                        self.append_lit(&mut word, "", true);
                    }
                }
                '\\' => {
                    self.pos += 1;
                    self.scan_escape(&mut word)?;
                }
                '$' => self.scan_dollar(&mut word, false)?,
                '`' => self.scan_backtick(&mut word, false)?,
                '~' if word.is_empty() => {
                    word.segs.push(Seg::Tilde);
                    self.pos += 1;
                }
                c => {
                    self.append_lit(&mut word, &c.to_string(), false);
                    self.pos += 1;
                }
            }
        }
        Ok(word)
    }

    fn read_single_quoted(&mut self) -> LexResult<String> {
        let mut text = String::new();
        loop {
            let Some(ch) = self.peek() else {
                return Err(LexError::Incomplete);
            };
            self.pos += 1;
            if ch == '\'' {
                return Ok(text);
            }
            text.push(ch);
        }
    }

    fn scan_escape(&mut self, word: &mut Word) -> LexResult<()> {
        match self.peek() {
            None => {
                self.append_lit(word, "\\", true);
                Ok(())
            }
            Some('\n') => {
                // Backslash-newline is a line continuation.
                self.pos += 1;
                Ok(())
            }
            Some(ch) => {
                self.pos += 1;
                self.append_lit(word, &ch.to_string(), true);
                Ok(())
            }
        }
    }

    fn scan_double_quoted(&mut self, word: &mut Word) -> LexResult<()> {
        loop {
            let Some(ch) = self.peek() else {
                return Err(LexError::Incomplete);
            };
            match ch {
                '"' => {
                    self.pos += 1;
                    return Ok(());
                }
                '\\' => {
                    self.pos += 1;
                    match self.peek() {
                        None => self.append_lit(word, "\\", true),
                        Some('\n') => self.pos += 1,
                        Some(c) if matches!(c, '$' | '`' | '"' | '\\') => {
                            self.pos += 1;
                            self.append_lit(word, &c.to_string(), true);
                        }
                        Some(c) => {
                            self.pos += 1;
                            self.append_lit(word, &format!("\\{c}"), true);
                        }
                    }
                }
                '$' => self.scan_dollar(word, true)?,
                '`' => self.scan_backtick(word, true)?,
                c => {
                    self.pos += 1;
                    self.append_lit(word, &c.to_string(), true);
                }
            }
        }
    }

    fn scan_dollar(&mut self, word: &mut Word, quoted: bool) -> LexResult<()> {
        self.pos += 1; // the `$`
        let Some(ch) = self.peek() else {
            self.append_lit(word, "$", quoted);
            return Ok(());
        };
        match ch {
            '{' => {
                self.pos += 1;
                let inner = self.read_braced()?;
                match parse_param_inner(&inner) {
                    Ok(exp) => word.segs.push(Seg::Param { exp, quoted }),
                    Err(_) => word.segs.push(Seg::BadSub { text: inner, quoted }),
                }
            }
            '(' => {
                if self.chars.get(self.pos + 1) == Some(&'(') {
                    self.pos += 2;
                    let text = self.read_arithmetic()?;
                    word.segs.push(Seg::Arith { text, quoted });
                } else {
                    self.pos += 1;
                    let text = self.read_command_substitution()?;
                    word.segs.push(Seg::CmdSub { text, quoted });
                }
            }
            d if d.is_ascii_digit() => {
                self.pos += 1;
                word.segs.push(Seg::Param {
                    exp: ParamExp {
                        name: ParamName::Positional(d.to_digit(10).unwrap_or(0) as usize),
                        kind: ParamKind::Plain,
                    },
                    quoted,
                });
            }
            c @ ('?' | '#' | '$' | '!' | '*' | '@' | '-') => {
                self.pos += 1;
                word.segs.push(Seg::Param {
                    exp: ParamExp {
                        name: ParamName::Special(c),
                        kind: ParamKind::Plain,
                    },
                    quoted,
                });
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut name = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        name.push(c);
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                word.segs.push(Seg::Param {
                    exp: ParamExp {
                        name: ParamName::Named(name),
                        kind: ParamKind::Plain,
                    },
                    quoted,
                });
            }
            _ => self.append_lit(word, "$", quoted),
        }
        Ok(())
    }

    fn scan_backtick(&mut self, word: &mut Word, quoted: bool) -> LexResult<()> {
        self.pos += 1; // the opening backtick
        let mut text = String::new();
        loop {
            let Some(ch) = self.peek() else {
                return Err(LexError::Incomplete);
            };
            self.pos += 1;
            match ch {
                '`' => break,
                '\\' => match self.peek() {
                    Some(c @ ('`' | '\\' | '$')) => {
                        text.push(c);
                        self.pos += 1;
                    }
                    _ => text.push('\\'),
                },
                c => text.push(c),
            }
        }
        word.segs.push(Seg::CmdSub { text, quoted });
        Ok(())
    }

    /// Read the body of `${…}`, returning the text between the braces.
    fn read_braced(&mut self) -> LexResult<String> {
        let mut text = String::new();
        let mut depth = 0usize;
        let mut quote: Option<char> = None;
        loop {
            let Some(ch) = self.peek() else {
                return Err(LexError::Incomplete);
            };
            match quote {
                Some(q) => {
                    self.pos += 1;
                    text.push(ch);
                    if q == '"' && ch == '\\' {
                        if let Some(next) = self.peek() {
                            self.pos += 1;
                            text.push(next);
                        }
                        continue;
                    }
                    if ch == q {
                        quote = None;
                    }
                }
                None => match ch {
                    '}' if depth == 0 => {
                        self.pos += 1;
                        return Ok(text);
                    }
                    '}' => {
                        depth -= 1;
                        self.pos += 1;
                        text.push(ch);
                    }
                    '$' if self.chars.get(self.pos + 1) == Some(&'{') => {
                        depth += 1;
                        self.pos += 2;
                        text.push('$');
                        text.push('{');
                    }
                    '\'' | '"' => {
                        quote = Some(ch);
                        self.pos += 1;
                        text.push(ch);
                    }
                    '\\' => {
                        self.pos += 1;
                        text.push(ch);
                        if let Some(next) = self.peek() {
                            self.pos += 1;
                            text.push(next);
                        }
                    }
                    _ => {
                        self.pos += 1;
                        text.push(ch);
                    }
                },
            }
        }
    }

    /// Read `$(…)` text, with parens balanced and quotes respected.
    fn read_command_substitution(&mut self) -> LexResult<String> {
        let mut text = String::new();
        let mut depth = 1usize;
        let mut quote: Option<char> = None;
        loop {
            let Some(ch) = self.peek() else {
                return Err(LexError::Incomplete);
            };
            match quote {
                Some(q) => {
                    self.pos += 1;
                    text.push(ch);
                    if q == '"' && ch == '\\' {
                        if let Some(next) = self.peek() {
                            self.pos += 1;
                            text.push(next);
                        }
                        continue;
                    }
                    if ch == q {
                        quote = None;
                    }
                }
                None => match ch {
                    '(' => {
                        depth += 1;
                        self.pos += 1;
                        text.push(ch);
                    }
                    ')' => {
                        depth -= 1;
                        self.pos += 1;
                        if depth == 0 {
                            return Ok(text);
                        }
                        text.push(ch);
                    }
                    '\'' | '"' => {
                        quote = Some(ch);
                        self.pos += 1;
                        text.push(ch);
                    }
                    '\\' => {
                        self.pos += 1;
                        text.push(ch);
                        if let Some(next) = self.peek() {
                            self.pos += 1;
                            text.push(next);
                        }
                    }
                    '`' => {
                        // A backtick substitution may contain `)`.
                        self.pos += 1;
                        text.push(ch);
                        loop {
                            let Some(inner) = self.peek() else {
                                return Err(LexError::Incomplete);
                            };
                            self.pos += 1;
                            text.push(inner);
                            if inner == '`' {
                                break;
                            }
                            if inner == '\\' {
                                if let Some(escaped) = self.peek() {
                                    self.pos += 1;
                                    text.push(escaped);
                                }
                            }
                        }
                    }
                    _ => {
                        self.pos += 1;
                        text.push(ch);
                    }
                },
            }
        }
    }

    /// Read `$((…))` text.
    fn read_arithmetic(&mut self) -> LexResult<String> {
        let mut text = String::new();
        // The `((` is already consumed: depth 2 is the arithmetic's own paren
        // plus the `$(` that opened the expansion.
        let mut depth = 2usize;
        loop {
            let Some(ch) = self.peek() else {
                return Err(LexError::Incomplete);
            };
            self.pos += 1;
            match ch {
                '(' => {
                    depth += 1;
                    text.push(ch);
                }
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(text);
                    }
                    // The closer that takes depth to 1 belongs to the
                    // arithmetic itself and is not part of the expression.
                    if depth > 1 {
                        text.push(ch);
                    }
                }
                c => text.push(c),
            }
        }
    }
}

/// Parse the inside of `${…}`.
fn parse_param_inner(inner: &str) -> Result<ParamExp, String> {
    let chars: Vec<char> = inner.chars().collect();
    let mut i = 0;
    let mut kind = ParamKind::Plain;

    if chars.len() > 1 && chars[0] == '#' {
        kind = ParamKind::Length;
        i = 1;
    }

    let name = if i < chars.len() && chars[i].is_ascii_digit() {
        let mut digits = String::new();
        while i < chars.len() && chars[i].is_ascii_digit() {
            digits.push(chars[i]);
            i += 1;
        }
        ParamName::Positional(digits.parse().unwrap_or(0))
    } else if i < chars.len() && matches!(chars[i], '@' | '*' | '#' | '?' | '$' | '!' | '-') {
        let c = chars[i];
        i += 1;
        ParamName::Special(c)
    } else {
        let mut name = String::new();
        while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
            name.push(chars[i]);
            i += 1;
        }
        if name.is_empty() || !is_valid_name(&name) {
            return Err(format!("ash: bad substitution: ${{{inner}}}"));
        }
        ParamName::Named(name)
    };

    let rest: String = chars[i..].iter().collect();
    if rest.is_empty() {
        return Ok(ParamExp { name, kind });
    }
    if kind == ParamKind::Length {
        return Err(format!("ash: bad substitution: ${{{inner}}}"));
    }

    let first = rest.chars().next().unwrap_or('\0');
    if first == ':' {
        let op = rest.chars().nth(1).unwrap_or('\0');
        if !matches!(op, '-' | '=' | '+' | '?') {
            return Err(format!("ash: bad substitution: ${{{inner}}}"));
        }
        let word = lex_fragment(&rest[1 + op.len_utf8()..]);
        return Ok(ParamExp {
            name,
            kind: ParamKind::Op {
                op,
                colon: true,
                word,
            },
        });
    }
    if matches!(first, '-' | '=' | '+' | '?') {
        let word = lex_fragment(&rest[first.len_utf8()..]);
        return Ok(ParamExp {
            name,
            kind: ParamKind::Op {
                op: first,
                colon: false,
                word,
            },
        });
    }
    if matches!(first, '%' | '#') {
        let doubled = rest.chars().nth(1) == Some(first);
        let skip = if doubled {
            1 + first.len_utf8()
        } else {
            first.len_utf8()
        };
        let pattern = lex_fragment(&rest[skip..]);
        return Ok(ParamExp {
            name,
            kind: ParamKind::Trim {
                suffix: first == '%',
                longest: doubled,
                pattern,
            },
        });
    }
    Err(format!("ash: bad substitution: ${{{inner}}}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        lex(text)
            .unwrap()
            .tokens
            .into_iter()
            .filter_map(|token| match token {
                Token::Word(word) => Some(word.literal_text().unwrap_or_else(|| "<exp>".into())),
                _ => None,
            })
            .collect()
    }

    fn ops(text: &str) -> Vec<Op> {
        lex(text)
            .unwrap()
            .tokens
            .into_iter()
            .filter_map(|token| match token {
                Token::Op(op) => Some(op),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_words_and_quotes() {
        assert_eq!(words("echo hello world"), ["echo", "hello", "world"]);
        assert_eq!(words(r#"echo "a b" 'c d'"#), ["echo", "a b", "c d"]);
        assert_eq!(words(r"echo a\ b"), ["echo", "a b"]);
        assert_eq!(words(r#"echo "a\"b""#), ["echo", "a\"b"]);
        assert_eq!(words("echo ''"), ["echo", ""]);
    }

    #[test]
    fn quotes_are_kept_per_segment() {
        let lexed = lex(r#"echo "$HOME"/x"#).unwrap();
        let Token::Word(word) = &lexed.tokens[1] else {
            panic!("expected word");
        };
        assert_eq!(
            word.segs,
            vec![
                Seg::Param {
                    exp: ParamExp {
                        name: ParamName::Named("HOME".into()),
                        kind: ParamKind::Plain,
                    },
                    quoted: true,
                },
                Seg::Lit {
                    text: "/x".into(),
                    quoted: false,
                },
            ]
        );
    }

    #[test]
    fn operators_and_redirects() {
        assert_eq!(
            ops("a && b || c | d ; e"),
            [
                Op::And,
                Op::Or,
                Op::Pipe,
                Op::Semi,
            ]
        );
        assert_eq!(
            ops("cmd < in > out >> app 2> err 2>> era &> all 2>&1"),
            [
                Op::Redir(RedirOp::In),
                Op::Redir(RedirOp::Out),
                Op::Redir(RedirOp::Append),
                Op::Redir(RedirOp::ErrOut),
                Op::Redir(RedirOp::ErrAppend),
                Op::Redir(RedirOp::OutErr),
                Op::Redir(RedirOp::Dup { fd: 2, from: 1 }),
            ]
        );
        // A digit run not glued to `>` is an ordinary word.
        assert_eq!(words("echo 12 x"), ["echo", "12", "x"]);
        assert_eq!(ops("12>f").len(), 1);
    }

    #[test]
    fn comments_start_at_word_starts_only() {
        assert_eq!(words("echo a # trailing"), ["echo", "a"]);
        assert_eq!(words("echo a#b"), ["echo", "a#b"]);
        assert_eq!(words("# whole line"), Vec::<String>::new());
    }

    #[test]
    fn expansions_are_recognized() {
        let lexed = lex("echo $HOME ${HOME} `ls` $(ls) $((1+2)) $# $? $1 ${x:-d} ${#x} ${x%y}").unwrap();
        let segs: Vec<Seg> = lexed
            .tokens
            .into_iter()
            .filter_map(|token| match token {
                Token::Word(word) => Some(word.segs),
                _ => None,
            })
            .flatten()
            .collect();
        let kinds: Vec<&str> = segs
            .iter()
            .map(|seg| match seg {
                Seg::Lit { .. } => "lit",
                Seg::Tilde => "tilde",
                Seg::Param { exp, .. } => match &exp.kind {
                    ParamKind::Plain => "param",
                    ParamKind::Length => "len",
                    ParamKind::Op { .. } => "op",
                    ParamKind::Trim { .. } => "trim",
                },
                Seg::CmdSub { .. } => "cmdsub",
                Seg::Arith { .. } => "arith",
                Seg::BadSub { .. } => "badsub",
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "lit", "param", "param", "cmdsub", "cmdsub", "arith", "param", "param", "param",
                "op", "len", "trim"
            ]
        );
    }

    #[test]
    fn unclosed_constructs_are_incomplete() {
        for text in ["echo \"x", "echo 'x", "echo $(x", "echo `x", "echo ${x", "cat <<EOF\nbody"] {
            assert!(
                matches!(lex(text), Err(LexError::Incomplete)),
                "{text:?} should be incomplete"
            );
        }
    }

    fn heredoc_bodies(text: &str) -> Vec<String> {
        lex(text)
            .unwrap()
            .tokens
            .into_iter()
            .filter_map(|token| match token {
                Token::Op(Op::Redir(RedirOp::HereDoc { body, .. })) => Some(body),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn heredocs_carry_their_bodies() {
        assert_eq!(heredoc_bodies("cat <<EOF\none\ntwo\nEOF\n"), ["one\ntwo\n"]);
        assert_eq!(heredoc_bodies("cat <<-EOF\n\tone\n\tEOF\n"), ["one\n"]);
        // Two heredocs on one line, bodies in order.
        assert_eq!(
            heredoc_bodies("cat <<A <<B\nfirst\nA\nsecond\nB\n"),
            ["first\n", "second\n"]
        );
        // A quoted delimiter is the same delimiter after quote removal.
        assert_eq!(heredoc_bodies("cat <<'EOF'\nhi\nEOF\n"), ["hi\n"]);
    }

    #[test]
    fn assignment_words_split_at_the_first_equals() {
        let lexed = lex("X=1").unwrap();
        let Token::Word(word) = &lexed.tokens[0] else {
            panic!("expected word");
        };
        let (name, value) = word.as_assignment().unwrap();
        assert_eq!(name, "X");
        assert_eq!(value.literal_text().unwrap(), "1");

        let lexed = lex(r#"Y="$Z" tail"#).unwrap();
        let Token::Word(word) = &lexed.tokens[0] else {
            panic!("expected word");
        };
        let (name, value) = word.as_assignment().unwrap();
        assert_eq!(name, "Y");
        assert!(matches!(value.segs[0], Seg::Param { quoted: true, .. }));
        assert!(lex("a b").unwrap().tokens.first().is_some());
    }

    #[test]
    fn fragments_keep_whitespace() {
        let word = lex_fragment("a b;c");
        assert_eq!(word.literal_text().unwrap(), "a b;c");
    }
}
