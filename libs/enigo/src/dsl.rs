use crate::{Key, KeyboardControllable};
use std::error::Error;
use std::fmt;

/// 解析 DSL 时可能出现的错误
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// 当某个标签不存在时。
    /// 示例：{+TEST}{-TEST}
    ///            ^^^^   ^^^^
    UnknownTag(String),

    /// 当在 {TAG} 内部遇到 { 时。
    /// 示例：{+HELLO{WORLD}
    ///                 ^
    UnexpectedOpen,

    /// 当 { 始终没有与 } 匹配时。
    /// 示例：{+SHIFT}Hello{-SHIFT
    ///                              ^
    UnmatchedOpen,

    /// 与 UnmatchedOpen 相对的情况。
    /// 示例：+SHIFT}Hello{-SHIFT}
    ///         ^
    UnmatchedClose,
}
impl Error for ParseError {
    fn description(&self) -> &str {
        match *self {
            ParseError::UnknownTag(_) => "Unknown tag",
            ParseError::UnexpectedOpen => "Unescaped open bracket ({) found inside tag name",
            ParseError::UnmatchedOpen => "Unmatched open bracket ({). No matching close (})",
            ParseError::UnmatchedClose => "Unmatched close bracket (}). No previous open ({)",
        }
    }
}
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string())
    }
}

/// 求值 DSL。它会把输入分词并按下相应的按键。
pub fn eval<K>(enigo: &mut K, input: &str) -> Result<(), ParseError>
where
    K: KeyboardControllable,
{
    for token in tokenize(input)? {
        match token {
            Token::Sequence(buffer) => {
                for key in buffer.chars() {
                    enigo.key_click(Key::Layout(key));
                }
            }
            Token::Unicode(buffer) => enigo.key_sequence(&buffer),
            Token::KeyUp(key) => enigo.key_up(key),
            Token::KeyDown(key) => enigo.key_down(key).unwrap_or(()),
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Sequence(String),
    Unicode(String),
    KeyUp(Key),
    KeyDown(Key),
}

fn tokenize(input: &str) -> Result<Vec<Token>, ParseError> {
    let mut unicode = false;

    let mut tokens = Vec::new();
    let mut buffer = String::new();
    let mut iter = input.chars().peekable();

    fn flush(tokens: &mut Vec<Token>, buffer: String, unicode: bool) {
        if !buffer.is_empty() {
            if unicode {
                tokens.push(Token::Unicode(buffer));
            } else {
                tokens.push(Token::Sequence(buffer));
            }
        }
    }

    while let Some(c) = iter.next() {
        if c == '{' {
            match iter.next() {
                Some('{') => buffer.push('{'),
                Some(mut c) => {
                    flush(&mut tokens, buffer, unicode);
                    buffer = String::new();

                    let mut tag = String::new();
                    loop {
                        tag.push(c);
                        match iter.next() {
                            Some('{') => match iter.peek() {
                                Some(&'{') => {
                                    iter.next();
                                    c = '{'
                                }
                                _ => return Err(ParseError::UnexpectedOpen),
                            },
                            Some('}') => match iter.peek() {
                                Some(&'}') => {
                                    iter.next();
                                    c = '}'
                                }
                                _ => break,
                            },
                            Some(new) => c = new,
                            None => return Err(ParseError::UnmatchedOpen),
                        }
                    }
                    match &*tag {
                        "+UNICODE" => unicode = true,
                        "-UNICODE" => unicode = false,
                        "+SHIFT" => tokens.push(Token::KeyDown(Key::Shift)),
                        "-SHIFT" => tokens.push(Token::KeyUp(Key::Shift)),
                        "+CTRL" => tokens.push(Token::KeyDown(Key::Control)),
                        "-CTRL" => tokens.push(Token::KeyUp(Key::Control)),
                        "+META" => tokens.push(Token::KeyDown(Key::Meta)),
                        "-META" => tokens.push(Token::KeyUp(Key::Meta)),
                        "+ALT" => tokens.push(Token::KeyDown(Key::Alt)),
                        "-ALT" => tokens.push(Token::KeyUp(Key::Alt)),
                        _ => return Err(ParseError::UnknownTag(tag)),
                    }
                }
                None => return Err(ParseError::UnmatchedOpen),
            }
        } else if c == '}' {
            match iter.next() {
                Some('}') => buffer.push('}'),
                _ => return Err(ParseError::UnmatchedClose),
            }
        } else {
            buffer.push(c);
        }
    }

    flush(&mut tokens, buffer, unicode);

    Ok(tokens)
}
