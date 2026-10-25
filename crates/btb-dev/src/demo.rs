//! JSON convenience for the todo demo: positional JSON arguments and JSON results.
//!
//! Arguments: a flat JSON array of strings, non-negative integers and booleans, encoded as the
//! tuple `(String | u64 | bool, ...)` with `btb-codec`. Results: known demo functions are shown
//! as JSON; anything else is `{"hex": "..."}`.

use btb_codec::{Decode, Encode};

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

pub fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Encode a JSON array body as contract arguments.
pub fn args_from_json(body: &str) -> Result<Vec<u8>, String> {
    let mut p = Parser {
        s: body.trim().as_bytes(),
        i: 0,
    };
    p.expect(b'[')?;
    let mut out = Vec::new();
    p.ws();
    if p.peek() == Some(b']') {
        p.i += 1;
    } else {
        loop {
            p.ws();
            match p.peek() {
                Some(b'"') => p.string()?.encode_to(&mut out),
                Some(b't') => {
                    p.word("true")?;
                    true.encode_to(&mut out)
                }
                Some(b'f') => {
                    p.word("false")?;
                    false.encode_to(&mut out)
                }
                Some(c) if c.is_ascii_digit() => p.number()?.encode_to(&mut out),
                _ => {
                    return Err(
                        "arguments must be strings, non-negative integers or booleans".into(),
                    )
                }
            }
            p.ws();
            match p.next() {
                Some(b',') => {}
                Some(b']') => break,
                _ => return Err("expected , or ]".into()),
            }
        }
    }
    p.ws();
    if p.i != p.s.len() {
        return Err("trailing characters after the argument array".into());
    }
    Ok(out)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn next(&mut self) -> Option<u8> {
        let c = self.peek();
        self.i += 1;
        c
    }
    fn ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }
    fn expect(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.next() == Some(c) {
            Ok(())
        } else {
            Err(format!("expected {:?}", c as char))
        }
    }
    fn word(&mut self, w: &str) -> Result<(), String> {
        if self.s[self.i..].starts_with(w.as_bytes()) {
            self.i += w.len();
            Ok(())
        } else {
            Err("bad literal".into())
        }
    }
    fn number(&mut self) -> Result<u64, String> {
        let st = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[st..self.i])
            .ok()
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| "number out of range".to_string())
    }
    fn string(&mut self) -> Result<String, String> {
        self.next();
        let mut out = Vec::new();
        loop {
            match self.next().ok_or("unterminated string")? {
                b'"' => break,
                b'\\' => {
                    let c = match self.next().ok_or("bad escape")? {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'"' => b'"',
                        b'\\' => b'\\',
                        b'/' => b'/',
                        b'u' => {
                            let h = self.s.get(self.i..self.i + 4).ok_or("bad \\u escape")?;
                            let cp = u32::from_str_radix(
                                std::str::from_utf8(h).map_err(|_| "bad \\u escape")?,
                                16,
                            )
                            .map_err(|_| "bad \\u escape")?;
                            self.i += 4;
                            let ch = char::from_u32(cp).ok_or("unsupported \\u escape")?;
                            out.extend_from_slice(ch.to_string().as_bytes());
                            continue;
                        }
                        _ => return Err("bad escape".into()),
                    };
                    out.push(c);
                }
                c => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| "invalid UTF-8".into())
    }
}

/// Result bytes of `function` as JSON.
pub fn result_json(function: &str, out: &[u8]) -> String {
    let known = match function {
        "list_mine" => Vec::<(u64, bool, String)>::decode(out).ok().map(|v| {
            let items: Vec<String> = v
                .iter()
                .map(|(id, done, text)| {
                    format!(
                        "{{\"id\":{id},\"done\":{done},\"text\":{}}}",
                        json_str(text)
                    )
                })
                .collect();
            format!("[{}]", items.join(","))
        }),
        "add" => u64::decode(out).ok().map(|v| v.to_string()),
        "toggle" => bool::decode(out).ok().map(|v| v.to_string()),
        _ => None,
    };
    known.unwrap_or_else(|| format!("{{\"hex\":\"{}\"}}", hex(out)))
}
