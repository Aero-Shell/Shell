use crate::config;

struct Span {
    start: usize,
    end: usize,
    color: &'static str,
}

pub fn highlight(line: &str) -> String {
    let Ok(cfg) = config::active_config() else {
        return line.to_owned();
    };
    if !cfg.color {
        return line.to_owned();
    }

    render(line, &cfg, &spans(line))
}

fn render(line: &str, cfg: &config::Config, spans: &[Span]) -> String {
    let reset = cfg
        .colors
        .get("reset")
        .map(String::as_str)
        .unwrap_or("\x1b[0m");
    let mut rendered = String::with_capacity(line.len() + spans.len() * 12);
    let mut cursor = 0;
    for span in spans {
        rendered.push_str(&line[cursor..span.start]);
        if let Some(color) = cfg.colors.get(span.color).filter(|color| !color.is_empty()) {
            rendered.push_str(color);
            rendered.push_str(&line[span.start..span.end]);
            rendered.push_str(reset);
        } else {
            rendered.push_str(&line[span.start..span.end]);
        }
        cursor = span.end;
    }
    rendered.push_str(&line[cursor..]);
    rendered
}

fn spans(line: &str) -> Vec<Span> {
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let (mut i, mut command, mut redirect_target) = (0, true, false);

    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }

        let start = i;
        if bytes[i] == b'|' || bytes[i] == b';' || starts_with(bytes, i, b"&&") {
            i += if starts_with(bytes, i, b"&&") || starts_with(bytes, i, b"||") {
                2
            } else {
                1
            };
            spans.push(Span {
                start,
                end: i,
                color: "syntax_pipe",
            });
            command = true;
            redirect_target = false;
            continue;
        }

        let operator = if starts_with(bytes, i, b"&>") {
            Some((i + 2, true))
        } else {
            let digits_end = (i..bytes.len())
                .find(|&at| !bytes[at].is_ascii_digit())
                .unwrap_or(i);
            let op_start = if bytes.get(digits_end) == Some(&b'>') {
                digits_end
            } else {
                i
            };
            match bytes.get(op_start) {
                Some(b'<' | b'>') => {
                    let op = bytes[op_start];
                    let mut end = op_start + 1;
                    if bytes.get(end) == Some(&op) {
                        end += 1;
                        if op == b'<' && bytes.get(end) == Some(&b'-') {
                            end += 1;
                        }
                    }
                    let duplicate = op == b'>'
                        && bytes.get(end) == Some(&b'&')
                        && bytes.get(end + 1).is_some_and(u8::is_ascii_digit);
                    if duplicate {
                        end += 2;
                        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                            end += 1;
                        }
                    }
                    Some((end, !duplicate))
                }
                _ => None,
            }
        };
        if let Some((end, has_target)) = operator {
            i = end;
            spans.push(Span {
                start,
                end: i,
                color: "syntax_redirect",
            });
            redirect_target = has_target;
            continue;
        }

        let word_start = i;
        let mut quote = None;
        let mut quote_start = 0;
        let mut quoted = Vec::new();
        while i < bytes.len() {
            let byte = bytes[i];
            if let Some(delimiter) = quote {
                if delimiter == b'"' && byte == b'\\' {
                    i = next_char(bytes, i + 1);
                } else if byte == delimiter {
                    i += 1;
                    quoted.push((quote_start, i));
                    quote = None;
                } else {
                    i = next_char(bytes, i);
                }
            } else if byte.is_ascii_whitespace()
                || matches!(byte, b'|' | b';' | b'<' | b'>')
                || starts_with(bytes, i, b"&&")
                || starts_with(bytes, i, b"||")
                || starts_with(bytes, i, b"&>")
            {
                break;
            } else if byte == b'\\' {
                i = next_char(bytes, i + 1);
            } else if matches!(byte, b'\'' | b'"') {
                quote = Some(byte);
                quote_start = i;
                i += 1;
            } else {
                i = next_char(bytes, i);
            }
        }
        if let Some(_) = quote {
            quoted.push((quote_start, i));
        }
        if i == word_start {
            i = next_char(bytes, i);
        }

        let color = if redirect_target {
            redirect_target = false;
            "syntax_redirect"
        } else if command {
            command = false;
            "syntax_command"
        } else {
            "syntax_argument"
        };
        let mut cursor = word_start;
        for (quoted_start, quoted_end) in quoted {
            if cursor < quoted_start {
                spans.push(Span {
                    start: cursor,
                    end: quoted_start,
                    color,
                });
            }
            spans.push(Span {
                start: quoted_start,
                end: quoted_end,
                color: "syntax_quote",
            });
            cursor = quoted_end;
        }
        if cursor < i {
            spans.push(Span {
                start: cursor,
                end: i,
                color,
            });
        }
    }
    spans
}

fn starts_with(bytes: &[u8], at: usize, needle: &[u8]) -> bool {
    bytes.get(at..at + needle.len()) == Some(needle)
}

fn next_char(bytes: &[u8], at: usize) -> usize {
    if at >= bytes.len() {
        return at;
    }
    at + match bytes[at] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors(line: &str) -> Vec<(&str, &str)> {
        spans(line)
            .iter()
            .map(|span| (&line[span.start..span.end], span.color))
            .collect()
    }

    #[test]
    fn classifies_commands_arguments_quotes_redirects_and_pipelines() {
        assert_eq!(
            colors("git status | grep 'work tree'"),
            vec![
                ("git", "syntax_command"),
                ("status", "syntax_argument"),
                ("|", "syntax_pipe"),
                ("grep", "syntax_command"),
                ("'work tree'", "syntax_quote"),
            ]
        );
        assert_eq!(
            colors("cat < input >> output"),
            vec![
                ("cat", "syntax_command"),
                ("<", "syntax_redirect"),
                ("input", "syntax_redirect"),
                (">>", "syntax_redirect"),
                ("output", "syntax_redirect"),
            ]
        );
    }

    #[test]
    fn styles_segments_without_changing_input() {
        let line = "echo pre'fixed'";
        let mut cfg = config::default_config();
        cfg.colors.insert("syntax_command".into(), "<cmd>".into());
        cfg.colors.insert("syntax_argument".into(), "<arg>".into());
        cfg.colors.insert("syntax_quote".into(), "<quote>".into());
        cfg.colors.insert("reset".into(), "<reset>".into());
        assert_eq!(
            render(line, &cfg, &spans(line)),
            "<cmd>echo<reset> <arg>pre<reset><quote>'fixed'<reset>"
        );
    }
}
