//! Word expansion: tilde, parameter, command substitution, arithmetic, field
//! splitting, quote removal and pathname expansion.
//!
//! Quoting is tracked per segment (see [`crate::shell::lexer::Seg`]): the
//! result of a quoted expansion is never split and never globbed, while an
//! unquoted expansion is split on `IFS` and may expand to pathnames.

use super::lexer::{ParamExp, ParamKind, ParamName, Seg, Word};

/// How a word is being expanded.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExpandMode {
    /// Ordinary command words: field splitting and pathname expansion apply.
    Normal,
    /// Assignment values and redirection targets: no splitting, no globbing.
    Assign,
}

#[derive(Debug)]
pub struct ExpandError {
    pub message: String,
    /// `set -u` and `${x:?}` failures are fatal to a non-interactive shell.
    pub fatal: bool,
}

impl ExpandError {
    fn message(message: impl Into<String>) -> Self {
        ExpandError {
            message: message.into(),
            fatal: false,
        }
    }

    fn fatal(message: impl Into<String>) -> Self {
        ExpandError {
            message: message.into(),
            fatal: true,
        }
    }
}

type EResult<T> = Result<T, ExpandError>;

fn is_glob_char(c: char) -> bool {
    matches!(c, '*' | '?' | '[')
}

impl super::Shell {
    /// Expand one word into its fields.
    pub(crate) fn expand_word(&mut self, word: &Word, mode: ExpandMode) -> EResult<Vec<String>> {
        let mut fields: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut have_field = false;
        let mut glob_candidate = false;

        for seg in &word.segs {
            match seg {
                Seg::Lit { text, quoted } => {
                    if *quoted {
                        have_field = true;
                    } else if text.chars().any(is_glob_char) {
                        glob_candidate = true;
                    }
                    if !text.is_empty() {
                        have_field = true;
                    }
                    cur.push_str(text);
                }
                Seg::Tilde => {
                    let home = dirs::home_dir()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "~".to_string());
                    cur.push_str(&home);
                    have_field = true;
                }
                Seg::Param { exp, quoted } => {
                    if let Some(values) = self.expand_positional_list(exp)? {
                        if *quoted && matches!(exp.kind, ParamKind::Plain) {
                            // "$@" is one field per positional parameter.
                            for (index, value) in values.iter().enumerate() {
                                if index > 0 {
                                    fields.push(std::mem::take(&mut cur));
                                }
                                cur.push_str(value);
                            }
                            if !values.is_empty() {
                                have_field = true;
                            }
                        } else {
                            for value in &values {
                                self.splice_expansion(
                                    value,
                                    *quoted || mode == ExpandMode::Assign,
                                    &mut cur,
                                    &mut fields,
                                    &mut have_field,
                                    &mut glob_candidate,
                                );
                            }
                            if *quoted && !values.is_empty() {
                                have_field = true;
                            }
                        }
                        continue;
                    }
                    let value = self.expand_param(exp)?;
                    self.splice_expansion(
                        &value,
                        *quoted || mode == ExpandMode::Assign,
                        &mut cur,
                        &mut fields,
                        &mut have_field,
                        &mut glob_candidate,
                    );
                    if *quoted {
                        have_field = true;
                    }
                }
                Seg::CmdSub { text, quoted } => {
                    let value = self.command_substitute(text)?;
                    let value = value.trim_end_matches('\n').to_string();
                    self.splice_expansion(
                        &value,
                        *quoted || mode == ExpandMode::Assign,
                        &mut cur,
                        &mut fields,
                        &mut have_field,
                        &mut glob_candidate,
                    );
                    if *quoted {
                        have_field = true;
                    }
                }
                Seg::BadSub { text, .. } => {
                    // Unsupported `${…}`, reported where a real shell would:
                    // when the word expands. A non-interactive shell then ends
                    // (POSIX expansion error), an interactive one carries on.
                    return Err(ExpandError::fatal(format!(
                        "ash: bad substitution: ${{{text}}}"
                    )));
                }
                Seg::Arith { text, quoted } => {
                    let value = self.evaluate_arithmetic(text)?.to_string();
                    self.splice_expansion(
                        &value,
                        *quoted || mode == ExpandMode::Assign,
                        &mut cur,
                        &mut fields,
                        &mut have_field,
                        &mut glob_candidate,
                    );
                    if *quoted {
                        have_field = true;
                    }
                }
            }
        }
        if have_field {
            fields.push(cur);
        }

        if mode == ExpandMode::Normal && glob_candidate {
            let mut expanded = Vec::with_capacity(fields.len());
            for field in fields {
                if field.chars().any(is_glob_char) {
                    let matches = glob_expand(&field);
                    if matches.is_empty() {
                        expanded.push(field);
                    } else {
                        expanded.extend(matches);
                    }
                } else {
                    expanded.push(field);
                }
            }
            return Ok(expanded);
        }
        Ok(fields)
    }

    /// Splice one expansion result into the word under construction: quoted
    /// (or non-splitting) values join the current field, unquoted values are
    /// split on `IFS` with the first piece joining the current field.
    #[allow(clippy::too_many_arguments)]
    fn splice_expansion(
        &self,
        value: &str,
        quoted: bool,
        cur: &mut String,
        fields: &mut Vec<String>,
        have_field: &mut bool,
        glob_candidate: &mut bool,
    ) {
        if quoted {
            cur.push_str(value);
            *have_field = true;
            return;
        }
        let pieces = split_fields(value, &self.ifs());
        if pieces.is_empty() {
            return;
        }
        *have_field = true;
        if pieces[0].chars().any(is_glob_char) {
            *glob_candidate = true;
        }
        cur.push_str(&pieces[0]);
        for piece in &pieces[1..] {
            fields.push(std::mem::take(cur));
            cur.push_str(piece);
        }
    }

    /// Expand a word to a single string (assignment values, redirection
    /// targets, case words): no splitting, no globbing.
    pub(crate) fn expand_to_string(&mut self, word: &Word) -> EResult<String> {
        let fields = self.expand_word(word, ExpandMode::Assign)?;
        Ok(fields.join(" "))
    }

    fn ifs(&self) -> String {
        match self.get_var("IFS") {
            Some(ifs) => ifs,
            None => " \t\n".to_string(),
        }
    }

    /// For `$@` and `$*` in their plain forms return the values they stand
    /// for; `None` means the parameter is an ordinary single value.
    fn expand_positional_list(&mut self, exp: &ParamExp) -> EResult<Option<Vec<String>>> {
        let ParamName::Special(special @ ('@' | '*')) = exp.name else {
            return Ok(None);
        };
        match exp.kind {
            ParamKind::Length => Ok(Some(vec![self.positionals.len().to_string()])),
            ParamKind::Plain => {
                if special == '@' {
                    Ok(Some(self.positionals.clone()))
                } else {
                    let ifs = self.ifs();
                    let sep = if ifs.is_empty() {
                        String::new()
                    } else {
                        ifs.chars().next().unwrap_or(' ').to_string()
                    };
                    Ok(Some(vec![self.positionals.join(&sep)]))
                }
            }
            ParamKind::Op { .. } | ParamKind::Trim { .. } => {
                // Treat the joined value like any other parameter for the
                // default/trim operators.
                let value = self.positionals.join(" ");
                Ok(Some(vec![value]))
            }
        }
    }

    fn expand_param(&mut self, exp: &ParamExp) -> EResult<String> {
        match &exp.kind {
            ParamKind::Plain => match self.param_value(&exp.name) {
                Some(value) => Ok(value),
                None => {
                    if self.options.nounset {
                        return Err(ExpandError::fatal(format!(
                            "ash: {}: parameter not set",
                            param_display(&exp.name)
                        )));
                    }
                    Ok(String::new())
                }
            },
            ParamKind::Length => {
                let value = self.param_value(&exp.name).unwrap_or_default();
                Ok(value.chars().count().to_string())
            }
            ParamKind::Op { op, colon, word } => {
                let value = self.param_value(&exp.name);
                let set = match &value {
                    Some(value) => !(*colon && value.is_empty()),
                    None => false,
                };
                match op {
                    '-' => {
                        if set {
                            Ok(value.unwrap_or_default())
                        } else {
                            self.expand_to_string(word)
                        }
                    }
                    '+' => {
                        if set {
                            self.expand_to_string(word)
                        } else {
                            Ok(String::new())
                        }
                    }
                    '=' => {
                        if set {
                            return Ok(value.unwrap_or_default());
                        }
                        let default = self.expand_to_string(word)?;
                        self.assign_param(&exp.name, &default)?;
                        Ok(default)
                    }
                    '?' => {
                        if set {
                            return Ok(value.unwrap_or_default());
                        }
                        let detail = if word.is_empty() {
                            "parameter null or not set".to_string()
                        } else {
                            self.expand_to_string(word)?
                        };
                        Err(ExpandError::fatal(format!(
                            "ash: {}: {detail}",
                            param_display(&exp.name)
                        )))
                    }
                    _ => Err(ExpandError::message(format!(
                        "ash: bad substitution: ${{{}}}",
                        param_display(&exp.name)
                    ))),
                }
            }
            ParamKind::Trim {
                suffix,
                longest,
                pattern,
            } => {
                let value = self.param_value(&exp.name).unwrap_or_default();
                let pattern = self.expand_to_string(pattern)?;
                Ok(trim_match(&value, &pattern, *suffix, *longest))
            }
        }
    }

    fn param_value(&self, name: &ParamName) -> Option<String> {
        match name {
            ParamName::Named(name) => self.get_var(name),
            ParamName::Positional(0) => Some(self.script_name.clone()),
            ParamName::Positional(index) => self.positionals.get(index - 1).cloned(),
            ParamName::Special('?') => Some(self.last_status.to_string()),
            ParamName::Special('#') => Some(self.positionals.len().to_string()),
            ParamName::Special('$') => Some(std::process::id().to_string()),
            ParamName::Special('!') => self.last_bg_pid.map(|pid| pid.to_string()),
            ParamName::Special('-') => Some(self.options.flags()),
            ParamName::Special('@') | ParamName::Special('*') => {
                Some(self.positionals.join(" "))
            }
            ParamName::Special(_) => None,
        }
    }

    fn assign_param(&mut self, name: &ParamName, value: &str) -> EResult<()> {
        match name {
            ParamName::Named(name) => self
                .set_var(name, value, false)
                .map_err(ExpandError::message),
            _ => Err(ExpandError::message(format!(
                "ash: {}: cannot assign in this way",
                param_display(name)
            ))),
        }
    }

    // -- arithmetic ---------------------------------------------------------

    pub(crate) fn evaluate_arithmetic(&mut self, text: &str) -> EResult<i64> {
        let mut parser = ArithParser {
            chars: text.chars().collect(),
            pos: 0,
            shell: self,
            no_eval: false,
        };
        let value = parser.parse_expression()?;
        parser.skip_space();
        if parser.pos < parser.chars.len() {
            return Err(ExpandError::message(format!(
                "ash: arithmetic syntax error: {text}"
            )));
        }
        Ok(value)
    }
}

fn param_display(name: &ParamName) -> String {
    match name {
        ParamName::Named(name) => name.clone(),
        ParamName::Positional(index) => index.to_string(),
        ParamName::Special(c) => c.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Field splitting
// ---------------------------------------------------------------------------

/// Split an unquoted expansion result on `IFS`. Whitespace separators
/// collapse; other separators produce empty fields between them.
pub fn split_fields(text: &str, ifs: &str) -> Vec<String> {
    if ifs.is_empty() {
        return vec![text.to_string()];
    }
    let is_ifs = |c: char| ifs.contains(c);
    let is_ifs_space = |c: char| c.is_whitespace() && ifs.contains(c);
    let chars: Vec<char> = text.chars().collect();
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut have_field = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if !is_ifs(c) {
            cur.push(c);
            have_field = true;
            i += 1;
            continue;
        }
        if is_ifs_space(c) {
            while i < chars.len() && is_ifs_space(chars[i]) {
                i += 1;
            }
            // Whitespace directly followed by a non-whitespace separator acts
            // as a single separator.
            if i < chars.len() && is_ifs(chars[i]) && !is_ifs_space(chars[i]) {
                i += 1;
                while i < chars.len() && is_ifs_space(chars[i]) {
                    i += 1;
                }
            }
            if i >= chars.len() {
                break;
            }
            if have_field {
                fields.push(std::mem::take(&mut cur));
                have_field = false;
            }
            continue;
        }
        fields.push(std::mem::take(&mut cur));
        have_field = false;
        i += 1;
        while i < chars.len() && is_ifs_space(chars[i]) {
            i += 1;
        }
    }
    if have_field {
        fields.push(cur);
    }
    fields
}

// ---------------------------------------------------------------------------
// Pattern matching, trimming and pathname expansion
// ---------------------------------------------------------------------------

/// POSIX pattern match: `*`, `?`, `[...]` (with `!`/`^` negation and ranges),
/// and `\` escapes.
pub fn pattern_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    match_from(&pattern, 0, &text, 0)
}

fn match_from(pattern: &[char], mut pi: usize, text: &[char], mut ti: usize) -> bool {
    while pi < pattern.len() {
        match pattern[pi] {
            '*' => {
                while pi < pattern.len() && pattern[pi] == '*' {
                    pi += 1;
                }
                if pi == pattern.len() {
                    return true;
                }
                for start in ti..=text.len() {
                    if match_from(pattern, pi, text, start) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if ti >= text.len() {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            '[' => {
                if ti >= text.len() {
                    return false;
                }
                let (matched, next) = match_class(pattern, pi, text[ti]);
                if !matched {
                    return false;
                }
                pi = next;
                ti += 1;
            }
            '\\' if pi + 1 < pattern.len() => {
                if ti >= text.len() || text[ti] != pattern[pi + 1] {
                    return false;
                }
                pi += 2;
                ti += 1;
            }
            c => {
                if ti >= text.len() || text[ti] != c {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == text.len()
}

/// Match one `[...]` class at `pattern[start]` against `ch`, returning whether
/// it matched and the index just past the closing `]`.
fn match_class(pattern: &[char], start: usize, ch: char) -> (bool, usize) {
    let mut i = start + 1;
    let mut negate = false;
    if matches!(pattern.get(i), Some('!') | Some('^')) {
        negate = true;
        i += 1;
    }
    let mut matched = false;
    if pattern.get(i) == Some(&']') {
        matched = ch == ']';
        i += 1;
    }
    while i < pattern.len() && pattern[i] != ']' {
        if pattern.get(i + 1) == Some(&'-')
            && pattern.get(i + 2).is_some_and(|next| *next != ']')
        {
            let low = pattern[i];
            let high = pattern[i + 2];
            if low <= ch && ch <= high {
                matched = true;
            }
            i += 3;
        } else {
            if pattern[i] == ch {
                matched = true;
            }
            i += 1;
        }
    }
    let next = if i < pattern.len() { i + 1 } else { i };
    (matched != negate, next)
}

/// `${x%pattern}` and friends: remove the shortest/longest matching suffix
/// (`suffix`) or prefix.
fn trim_match(value: &str, pattern: &str, suffix: bool, longest: bool) -> String {
    let chars: Vec<char> = value.chars().collect();
    let limits: Vec<usize> = if longest {
        (0..=chars.len()).rev().collect()
    } else {
        (0..=chars.len()).collect()
    };
    for len in limits {
        let candidate: String = if suffix {
            chars[chars.len() - len..].iter().collect()
        } else {
            chars[..len].iter().collect()
        };
        if pattern_match(pattern, &candidate) {
            return if suffix {
                chars[..chars.len() - len].iter().collect()
            } else {
                chars[len..].iter().collect()
            };
        }
    }
    value.to_string()
}

/// Pathname expansion. An empty result means "no match", and the caller keeps
/// the pattern literal.
pub fn glob_expand(pattern: &str) -> Vec<String> {
    if !pattern.chars().any(is_glob_char) {
        return Vec::new();
    }
    let absolute = pattern.starts_with('/');
    let dirs_only = pattern.ends_with('/');
    let parts: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return Vec::new();
    }

    let mut candidates: Vec<String> = vec![if absolute {
        "/".to_string()
    } else {
        String::new()
    }];
    for (index, part) in parts.iter().enumerate() {
        let last = index == parts.len() - 1;
        let mut next = Vec::new();
        if part.chars().any(is_glob_char) {
            for base in &candidates {
                let dir = if base.is_empty() {
                    ".".to_string()
                } else {
                    base.clone()
                };
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                let mut names: Vec<String> = entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                for name in names {
                    if part.starts_with('.') != name.starts_with('.') {
                        continue;
                    }
                    if !pattern_match(part, &name) {
                        continue;
                    }
                    let path = join_path(base, &name);
                    if last {
                        if !dirs_only || std::path::Path::new(&path).is_dir() {
                            next.push(path);
                        }
                    } else if std::path::Path::new(&path).is_dir() {
                        next.push(path);
                    }
                }
            }
        } else {
            for base in &candidates {
                let path = join_path(base, part);
                let exists = std::path::Path::new(&path).exists();
                if last {
                    if exists && (!dirs_only || std::path::Path::new(&path).is_dir()) {
                        next.push(path);
                    }
                } else if std::path::Path::new(&path).is_dir() {
                    next.push(path);
                }
            }
        }
        next.sort();
        next.dedup();
        candidates = next;
        if candidates.is_empty() {
            break;
        }
    }
    candidates
}

fn join_path(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

// ---------------------------------------------------------------------------
// Arithmetic
// ---------------------------------------------------------------------------

/// Read an integer the way arithmetic does: `0x…` is hex, anything else is
/// decimal (dash semantics, so a leading zero is not octal).
fn parse_constant(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()
    } else {
        text.parse::<i64>().ok()
    }
}

struct ArithParser<'a> {
    chars: Vec<char>,
    pos: usize,
    shell: &'a mut super::Shell,
    /// Set while scanning the branch a short circuit skipped: the text still
    /// has to be parsed, but errors it would raise (division by zero) must
    /// not fire, exactly like bash and dash.
    no_eval: bool,
}

impl ArithParser<'_> {
    fn skip_space(&mut self) {
        while self
            .chars
            .get(self.pos)
            .is_some_and(|c| c.is_whitespace())
        {
            self.pos += 1;
        }
    }

    /// Where the next operator sits, ignoring whitespace before it.
    fn op_pos(&self, text: &str) -> Option<usize> {
        let mut pos = self.pos;
        while self
            .chars
            .get(pos)
            .is_some_and(|c| c.is_whitespace())
        {
            pos += 1;
        }
        let rest: String = self.chars[pos.min(self.chars.len())..].iter().collect();
        rest.starts_with(text).then_some(pos)
    }

    fn eat(&mut self, text: &str) -> bool {
        match self.op_pos(text) {
            Some(pos) => {
                self.pos = pos + text.chars().count();
                self.skip_space();
                true
            }
            None => false,
        }
    }

    /// Eat a one-character operator only when it is not half of a doubled
    /// one: `&` must not swallow the first `&` of `&&`.
    fn eat_single(&mut self, ch: char) -> bool {
        match self.op_pos(&ch.to_string()) {
            Some(pos) if self.chars.get(pos + 1) != Some(&ch) => {
                self.pos = pos + 1;
                self.skip_space();
                true
            }
            _ => false,
        }
    }

    fn parse_expression(&mut self) -> EResult<i64> {
        self.skip_space();
        let mut value = self.parse_ternary()?;
        // A comma expression's value is the last term.
        while self.eat(",") {
            value = self.parse_ternary()?;
        }
        Ok(value)
    }

    /// `cond ? then : else` — only the branch that runs may fail.
    fn parse_ternary(&mut self) -> EResult<i64> {
        let condition = self.parse_or()?;
        if !self.eat("?") {
            return Ok(condition);
        }
        let take_then = condition != 0;
        let then_branch = self.parse_branch(!take_then)?;
        if !self.eat(":") {
            return Err(ExpandError::message(
                "ash: arithmetic syntax error: missing ':'".to_string(),
            ));
        }
        let else_branch = self.parse_branch(take_then)?;
        Ok(if take_then { then_branch } else { else_branch })
    }

    /// Parse one `? :` branch, suspending evaluation errors when it turns out
    /// to be the branch that is not taken.
    fn parse_branch(&mut self, skip: bool) -> EResult<i64> {
        let saved = self.no_eval;
        self.no_eval = saved || skip;
        let value = self.parse_ternary();
        self.no_eval = saved;
        value
    }

    fn parse_or(&mut self) -> EResult<i64> {
        let mut value = self.parse_and()?;
        while self.eat("||") {
            // The right side still has to be parsed, but a skipped one may
            // not raise errors.
            let right = self.parse_skipping(value != 0)?;
            value = i64::from(value != 0 || right != 0);
        }
        Ok(value)
    }

    fn parse_and(&mut self) -> EResult<i64> {
        let mut value = self.parse_bit_or()?;
        while self.eat("&&") {
            let right = self.parse_skipping(value == 0)?;
            value = i64::from(value != 0 && right != 0);
        }
        Ok(value)
    }

    /// Parse the next level with error reporting suspended when `skip`.
    fn parse_skipping(&mut self, skip: bool) -> EResult<i64> {
        let saved = self.no_eval;
        self.no_eval = saved || skip;
        let value = self.parse_bit_or();
        self.no_eval = saved;
        value
    }

    fn parse_bit_or(&mut self) -> EResult<i64> {
        let mut value = self.parse_bit_xor()?;
        while self.eat_single('|') {
            value |= self.parse_bit_xor()?;
        }
        Ok(value)
    }

    fn parse_bit_xor(&mut self) -> EResult<i64> {
        let mut value = self.parse_bit_and()?;
        while self.eat("^") {
            value ^= self.parse_bit_and()?;
        }
        Ok(value)
    }

    fn parse_bit_and(&mut self) -> EResult<i64> {
        let mut value = self.parse_equality()?;
        while self.eat_single('&') {
            value &= self.parse_equality()?;
        }
        Ok(value)
    }

    fn parse_equality(&mut self) -> EResult<i64> {
        let mut value = self.parse_relational()?;
        loop {
            if self.eat("==") {
                value = i64::from(value == self.parse_relational()?);
            } else if self.eat("!=") {
                value = i64::from(value != self.parse_relational()?);
            } else {
                break;
            }
        }
        Ok(value)
    }

    fn parse_relational(&mut self) -> EResult<i64> {
        let mut value = self.parse_shift()?;
        loop {
            let op = if self.eat("<=") {
                "<="
            } else if self.eat(">=") {
                ">="
            } else if self.eat("<") {
                "<"
            } else if self.eat(">") {
                ">"
            } else {
                break;
            };
            let right = self.parse_shift()?;
            value = i64::from(match op {
                "<=" => value <= right,
                ">=" => value >= right,
                "<" => value < right,
                _ => value > right,
            });
        }
        Ok(value)
    }

    fn parse_shift(&mut self) -> EResult<i64> {
        let mut value = self.parse_sum()?;
        loop {
            if self.eat("<<") {
                value = value.wrapping_shl(self.parse_sum()? as u32);
            } else if self.eat(">>") {
                value = value.wrapping_shr(self.parse_sum()? as u32);
            } else {
                break;
            }
        }
        Ok(value)
    }

    fn parse_sum(&mut self) -> EResult<i64> {
        let mut value = self.parse_product()?;
        loop {
            if self.eat("+") {
                value = value.wrapping_add(self.parse_product()?);
            } else if self.eat("-") {
                value = value.wrapping_sub(self.parse_product()?);
            } else {
                break;
            }
        }
        Ok(value)
    }

    fn parse_product(&mut self) -> EResult<i64> {
        let mut value = self.parse_unary()?;
        loop {
            if self.eat("*") {
                value = value.wrapping_mul(self.parse_unary()?);
            } else if self.eat("/") {
                let divisor = self.parse_unary()?;
                if divisor == 0 {
                    if self.no_eval {
                        value = 0;
                    } else {
                        return Err(ExpandError::message(
                            "ash: division by zero".to_string(),
                        ));
                    }
                } else {
                    value /= divisor;
                }
            } else if self.eat("%") {
                let divisor = self.parse_unary()?;
                if divisor == 0 {
                    if self.no_eval {
                        value = 0;
                    } else {
                        return Err(ExpandError::message(
                            "ash: division by zero".to_string(),
                        ));
                    }
                } else {
                    value %= divisor;
                }
            } else {
                break;
            }
        }
        Ok(value)
    }

    fn parse_unary(&mut self) -> EResult<i64> {
        if self.eat("!") {
            return Ok(i64::from(self.parse_unary()? == 0));
        }
        if self.eat("~") {
            return Ok(!self.parse_unary()?);
        }
        if self.eat("-") {
            return Ok(self.parse_unary()?.wrapping_neg());
        }
        if self.eat("+") {
            return self.parse_unary();
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> EResult<i64> {
        self.skip_space();
        if self.eat("(") {
            let value = self.parse_expression()?;
            if !self.eat(")") {
                return Err(ExpandError::message(
                    "ash: arithmetic syntax error: missing ')'".to_string(),
                ));
            }
            return Ok(value);
        }
        if self.eat("$") {
            // `$name` / `$((…))` style prefix: read the name or a literal.
        }
        self.skip_space();
        let Some(first) = self.chars.get(self.pos).copied() else {
            return Err(ExpandError::message(
                "ash: arithmetic syntax error: unexpected end of expression".to_string(),
            ));
        };
        if first.is_ascii_digit() {
            let start = self.pos;
            let hex = first == '0' && matches!(self.chars.get(self.pos + 1), Some('x' | 'X'));
            if hex {
                self.pos += 2;
            }
            while self.chars.get(self.pos).is_some_and(|c| {
                if hex {
                    c.is_ascii_hexdigit()
                } else {
                    c.is_ascii_digit()
                }
            }) {
                self.pos += 1;
            }
            let mut text: String = self.chars[start..self.pos].iter().collect();
            // Hex constants are `0x…`; a leading zero stays decimal, as in
            // dash (bash would read it as octal).
            let radix = if text.starts_with("0x") || text.starts_with("0X") {
                text.drain(..2);
                16
            } else {
                10
            };
            return i64::from_str_radix(&text, radix).map_err(|_| {
                ExpandError::message(format!("ash: arithmetic syntax error: {text}"))
            });
        }
        if first.is_ascii_alphabetic() || first == '_' {
            let start = self.pos;
            while self
                .chars
                .get(self.pos)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                self.pos += 1;
            }
            let name: String = self.chars[start..self.pos].iter().collect();
            let value = self.shell.get_var(&name).unwrap_or_default();
            return Ok(parse_constant(&value).unwrap_or(0));
        }
        Err(ExpandError::message(format!(
            "ash: arithmetic syntax error: unexpected character '{first}'"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::Shell;

    fn new_shell() -> Shell {
        Shell::new(false)
    }

    #[test]
    fn parameters_expand_from_the_environment() {
        // A variable inherited from the environment expands from the shell's
        // start-up copy of it.
        if let Ok(home) = std::env::var("HOME") {
            let mut shell = new_shell();
            let word = crate::shell::lexer::lex_fragment("$HOME/x");
            assert_eq!(
                shell.expand_word(&word, ExpandMode::Normal).unwrap(),
                [format!("{home}/x")]
            );
            let word = crate::shell::lexer::lex_fragment("${HOME}");
            assert_eq!(shell.expand_word(&word, ExpandMode::Normal).unwrap(), [home]);
        }
        // A variable set inside the shell expands too. Tests keep it
        // unexported so they never disturb the process environment.
        let mut shell = new_shell();
        shell
            .set_var("ASHTEST_PARAM", "/home/tester", false)
            .unwrap();
        let word = crate::shell::lexer::lex_fragment("\"$ASHTEST_PARAM\"");
        assert_eq!(
            shell.expand_word(&word, ExpandMode::Normal).unwrap(),
            ["/home/tester"]
        );
    }

    #[test]
    fn unset_and_empty_expansions_disappear_unless_quoted() {
        let mut shell = new_shell();
        shell.set_var("ASHTEST_EMPTY", "", false).unwrap();
        let word = crate::shell::lexer::lex_fragment("a$UNSET b");
        // Wait: the fragment keeps no whitespace boundaries; use separate words.
        let _ = word;
        let word = crate::shell::lexer::lex_fragment("a$UNSET");
        assert_eq!(shell.expand_word(&word, ExpandMode::Normal).unwrap(), ["a"]);
        let word = crate::shell::lexer::lex_fragment("$UNSET");
        assert!(shell.expand_word(&word, ExpandMode::Normal).unwrap().is_empty());
        let word = crate::shell::lexer::lex_fragment("\"$ASHTEST_EMPTY\"");
        assert_eq!(shell.expand_word(&word, ExpandMode::Normal).unwrap(), [""]);
        let word = crate::shell::lexer::lex_fragment("\"$UNSET\"");
        assert_eq!(shell.expand_word(&word, ExpandMode::Normal).unwrap(), [""]);
    }

    #[test]
    fn unquoted_expansions_split_on_whitespace() {
        let mut shell = new_shell();
        shell.set_var("ASHTEST_LIST", "a b  c", false).unwrap();
        let word = crate::shell::lexer::lex_fragment("x$ASHTEST_LIST");
        assert_eq!(
            shell.expand_word(&word, ExpandMode::Normal).unwrap(),
            ["xa", "b", "c"]
        );
        // A custom IFS splits as well.
        shell.set_var("IFS", ":", false).unwrap();
        shell.set_var("ASHTEST_LIST", "a::b:", false).unwrap();
        let word = crate::shell::lexer::lex_fragment("$ASHTEST_LIST");
        assert_eq!(
            shell.expand_word(&word, ExpandMode::Normal).unwrap(),
            ["a", "", "b"]
        );
        // Leave the process environment as we found it.
        shell.unset_var("IFS");
    }

    #[test]
    fn positional_parameters_and_specials() {
        let mut shell = new_shell();
        shell.positionals = vec!["a".into(), "b c".into()];
        shell.last_status = 7;
        let word = crate::shell::lexer::lex_fragment("$1-$2-$#-$?");
        assert_eq!(
            shell.expand_word(&word, ExpandMode::Normal).unwrap(),
            ["a-b", "c-2-7"]
        );
        let word = crate::shell::lexer::lex_fragment("\"$@\"");
        assert_eq!(
            shell.expand_word(&word, ExpandMode::Normal).unwrap(),
            ["a", "b c"]
        );
        let word = crate::shell::lexer::lex_fragment("\"$*\"");
        assert_eq!(
            shell.expand_word(&word, ExpandMode::Normal).unwrap(),
            ["a b c"]
        );
        let word = crate::shell::lexer::lex_fragment("pre\"$@\"post");
        assert_eq!(
            shell.expand_word(&word, ExpandMode::Normal).unwrap(),
            ["prea", "b cpost"]
        );
    }

    #[test]
    fn parameter_operators() {
        let mut shell = new_shell();
        let check = |shell: &mut Shell, line: &str| {
            let word = crate::shell::lexer::lex_fragment(line);
            shell
                .expand_word(&word, ExpandMode::Normal)
                .unwrap_or_else(|e| panic!("{line}: {}", e.message))
                .join("")
        };
        assert_eq!(check(&mut shell, "${MISSING:-fallback}"), "fallback");
        assert_eq!(check(&mut shell, "${MISSING-fallback}"), "fallback");
        assert_eq!(check(&mut shell, "${MISSING:+set}"), "");
        assert_eq!(check(&mut shell, "${MISSING:=assigned}"), "assigned");
        assert_eq!(shell.get_var("MISSING").unwrap(), "assigned");
        assert_eq!(check(&mut shell, "${MISSING:+set}"), "set");
        assert_eq!(check(&mut shell, "${MISSING:+set}"), "set");
        shell.set_var("EMPTY", "", false).unwrap();
        assert_eq!(check(&mut shell, "${EMPTY:-fallback}"), "fallback");
        assert_eq!(check(&mut shell, "${EMPTY-fallback}"), "");
        let word = crate::shell::lexer::lex_fragment("${EMPTY:?must be set}");
        let error = shell.expand_word(&word, ExpandMode::Normal).unwrap_err();
        assert!(error.fatal);
        assert!(error.message.contains("must be set"));
    }

    #[test]
    fn length_and_trim() {
        let mut shell = new_shell();
        shell.set_var("V", "abcdef", false).unwrap();
        let word = crate::shell::lexer::lex_fragment("${#V}");
        assert_eq!(shell.expand_word(&word, ExpandMode::Normal).unwrap(), ["6"]);
        let word = crate::shell::lexer::lex_fragment("${V#a*c}");
        assert_eq!(shell.expand_word(&word, ExpandMode::Normal).unwrap(), ["def"]);
        let word = crate::shell::lexer::lex_fragment("${V%ef}");
        assert_eq!(shell.expand_word(&word, ExpandMode::Normal).unwrap(), ["abcd"]);
        let word = crate::shell::lexer::lex_fragment("${V%%c*}");
        assert_eq!(shell.expand_word(&word, ExpandMode::Normal).unwrap(), ["ab"]);
    }

    #[test]
    fn nounset_reports_a_fatal_error() {
        let mut shell = new_shell();
        shell.options.nounset = true;
        let word = crate::shell::lexer::lex_fragment("$NOPE");
        let error = shell.expand_word(&word, ExpandMode::Normal).unwrap_err();
        assert!(error.fatal, "{error:?}");
    }

    #[test]
    fn assignment_mode_does_not_split() {
        let mut shell = new_shell();
        shell.set_var("LIST", "a b", false).unwrap();
        let word = crate::shell::lexer::lex_fragment("$LIST");
        assert_eq!(
            shell.expand_word(&word, ExpandMode::Assign).unwrap(),
            ["a b"]
        );
    }

    #[test]
    fn arithmetic_follows_the_usual_precedence() {
        let mut shell = new_shell();
        shell.set_var("i", "2", false).unwrap();
        let value = |shell: &mut Shell, expr: &str| shell.evaluate_arithmetic(expr).unwrap();
        assert_eq!(value(&mut shell, "1+2*3"), 7);
        assert_eq!(value(&mut shell, "(1+2)*3"), 9);
        assert_eq!(value(&mut shell, "i+1"), 3);
        assert_eq!(value(&mut shell, "$i*3"), 6);
        assert_eq!(value(&mut shell, "10/3"), 3);
        assert_eq!(value(&mut shell, "10%3"), 1);
        assert_eq!(value(&mut shell, "1<2"), 1);
        assert_eq!(value(&mut shell, "!0"), 1);
        assert_eq!(value(&mut shell, "-3+5"), 2);
    }

    #[test]
    fn tilde_expands_at_the_start_of_a_word() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let mut shell = new_shell();
        let word = crate::shell::lexer::lex_fragment("~/x");
        assert_eq!(
            shell.expand_word(&word, ExpandMode::Normal).unwrap(),
            [format!("{}/x", home.to_string_lossy())]
        );
        let word = crate::shell::lexer::lex_fragment("a~");
        assert_eq!(shell.expand_word(&word, ExpandMode::Normal).unwrap(), ["a~"]);
    }

    // What a substitution captures is asserted through the real binary
    // (`tests/cli.rs`): the harness prints on the descriptor the shell
    // captures, so comparing the captured text in-process races with it.

    #[test]
    fn splitting_handles_separator_classes() {
        assert_eq!(split_fields("a b", " \t\n"), ["a", "b"]);
        assert_eq!(split_fields("  a  b  ", " \t\n"), ["a", "b"]);
        assert_eq!(split_fields("", " \t\n"), Vec::<String>::new());
        assert_eq!(split_fields("a::b", ":"), ["a", "", "b"]);
        assert_eq!(split_fields(":a", ":"), ["", "a"]);
        assert_eq!(split_fields("a:", ":"), ["a"]);
        assert_eq!(split_fields("a b", ""), ["a b"]);
        assert_eq!(split_fields("a :b", " :"), ["a", "b"]);
    }

    #[test]
    fn pattern_matching_covers_the_usual_cases() {
        assert!(pattern_match("*", "anything"));
        assert!(pattern_match("a*c", "abc"));
        assert!(!pattern_match("a*c", "ab"));
        assert!(pattern_match("a?c", "abc"));
        assert_eq!(pattern_match("a[bc]d", "abd"), true);
        assert_eq!(pattern_match("a[!bc]d", "abd"), false);
        assert_eq!(pattern_match("a[!bc]d", "axd"), true);
        assert_eq!(pattern_match("a[a-c]d", "abd"), true);
        assert_eq!(pattern_match("a[a-c]d", "azd"), false);
        assert_eq!(pattern_match(r"a\*d", "a*d"), true);
        assert_eq!(pattern_match(r"a\*d", "axd"), false);
        assert_eq!(pattern_match("", ""), true);
        assert_eq!(pattern_match("", "x"), false);
    }

    #[test]
    fn glob_expansion_finds_matching_files() {
        let dir = std::env::temp_dir().join(format!("ash-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("one.txt"), "").unwrap();
        std::fs::write(dir.join("two.txt"), "").unwrap();
        std::fs::write(dir.join(".hidden.txt"), "").unwrap();
        let pattern = format!("{}/*.txt", dir.to_string_lossy());
        let matches = glob_expand(&pattern);
        assert_eq!(matches.len(), 2, "{matches:?}");
        assert!(matches.iter().all(|m| m.ends_with(".txt")));
        assert!(matches.iter().all(|m| !m.contains(".hidden")));
        // No match keeps the caller on its literal.
        assert!(glob_expand(&format!("{}/*.none", dir.to_string_lossy())).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
