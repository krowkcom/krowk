//! JSON the way Go's encoding/json reads and writes it, because Go is the
//! oracle: the registry this replaces decoded request bodies with it and
//! answered with `json.Encoder` + `SetIndent("", "  ")`.
//!
//! Hand-rolled rather than serde because the parts that matter are the parts
//! serde normalizes away: a number keeps the literal it was written as (the
//! Idempotency-Key digest depends on 1 and 1.0 differing), metadata goes back
//! out in the order and spelling it came in, and a struct field is matched
//! case-insensitively with the last duplicate winning.

use std::ops::Range;

/// A parsed request value. Objects keep every member, duplicates included, and
/// each member's byte range in the source — Go's `json.RawMessage` is exactly
/// those bytes.
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<Member>),
}

#[derive(Debug, Clone)]
pub struct Member {
    pub key: String,
    pub value: Value,
    pub raw: Range<usize>,
}

/// Why a body did not decode, in the three shapes Go's Decoder tells apart:
/// nothing at all (`io.EOF`), a value cut short (`io.ErrUnexpectedEOF`), and
/// bytes that are not JSON (`*json.SyntaxError`).
#[derive(Debug, PartialEq)]
pub enum ParseError {
    Empty,
    Truncated,
    Syntax,
}

impl ParseError {
    /// Whether this is the refusal Go's `unreadableBody` answers `bad_request` for.
    pub fn unreadable(&self) -> bool {
        *self != ParseError::Empty
    }
}

/// Parses the first value in `src` and ignores whatever follows it, as
/// `json.Decoder.Decode` does.
pub fn parse_first(src: &[u8]) -> Result<Value, ParseError> {
    let mut p = Parser { src, pos: 0, depth: 0 };
    p.ws();
    if p.pos == src.len() {
        return Err(ParseError::Empty);
    }
    p.value()
}

/// Parses `src` as exactly one value — `json.Unmarshal`, for bytes already known
/// to hold one.
pub fn parse(src: &[u8]) -> Option<Value> {
    parse_first(src).ok()
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    depth: usize,
}

// Go's own ceiling, past which a body is a syntax error rather than a stack.
const MAX_DEPTH: usize = 10_000;

impl Parser<'_> {
    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.src.get(self.pos) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Result<u8, ParseError> {
        self.src.get(self.pos).copied().ok_or(ParseError::Truncated)
    }

    fn next(&mut self) -> Result<u8, ParseError> {
        let b = self.peek()?;
        self.pos += 1;
        Ok(b)
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        match self.peek()? {
            b'{' => self.nested(Self::object),
            b'[' => self.nested(Self::array),
            b'"' => Ok(Value::Str(self.string()?)),
            b't' => self.literal(b"true", Value::Bool(true)),
            b'f' => self.literal(b"false", Value::Bool(false)),
            b'n' => self.literal(b"null", Value::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(ParseError::Syntax),
        }
    }

    fn nested(&mut self, f: fn(&mut Self) -> Result<Value, ParseError>) -> Result<Value, ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ParseError::Syntax);
        }
        let v = f(self);
        self.depth -= 1;
        v
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.pos += 1;
        let mut members = Vec::new();
        self.ws();
        if self.peek()? == b'}' {
            self.pos += 1;
            return Ok(Value::Obj(members));
        }
        loop {
            self.ws();
            if self.peek()? != b'"' {
                return Err(ParseError::Syntax);
            }
            let key = self.string()?;
            self.ws();
            if self.next()? != b':' {
                return Err(ParseError::Syntax);
            }
            self.ws();
            let start = self.pos;
            let value = self.value()?;
            members.push(Member { key, value, raw: start..self.pos });
            self.ws();
            match self.next()? {
                b',' => continue,
                b'}' => return Ok(Value::Obj(members)),
                _ => return Err(ParseError::Syntax),
            }
        }
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.pos += 1;
        let mut items = Vec::new();
        self.ws();
        if self.peek()? == b']' {
            self.pos += 1;
            return Ok(Value::Arr(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            match self.next()? {
                b',' => continue,
                b']' => return Ok(Value::Arr(items)),
                _ => return Err(ParseError::Syntax),
            }
        }
    }

    fn literal(&mut self, word: &[u8], v: Value) -> Result<Value, ParseError> {
        for &want in word {
            if self.next()? != want {
                return Err(ParseError::Syntax);
            }
        }
        Ok(v)
    }

    fn digits(&mut self) -> Result<usize, ParseError> {
        let start = self.pos;
        while let Some(b'0'..=b'9') = self.src.get(self.pos) {
            self.pos += 1;
        }
        if self.pos == start {
            // A number cut off where its digits should be is truncated; any
            // other byte there is not a number.
            self.peek()?;
            return Err(ParseError::Syntax);
        }
        Ok(self.pos - start)
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        if self.peek()? == b'-' {
            self.pos += 1;
        }
        if self.peek()? == b'0' {
            self.pos += 1;
        } else {
            self.digits()?;
        }
        if self.src.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            self.digits()?;
        }
        if let Some(b'e' | b'E') = self.src.get(self.pos) {
            self.pos += 1;
            if let Some(b'+' | b'-') = self.src.get(self.pos) {
                self.pos += 1;
            }
            self.digits()?;
        }
        Ok(Value::Num(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned()))
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let mut n = 0;
        for _ in 0..4 {
            let d = (self.next()? as char).to_digit(16).ok_or(ParseError::Syntax)?;
            n = n * 16 + d;
        }
        Ok(n)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.pos += 1;
        let mut out = Vec::new();
        loop {
            match self.next()? {
                b'"' => return Ok(String::from_utf8_lossy(&out).into_owned()),
                b'\\' => {
                    let c = match self.next()? {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode()?,
                        _ => return Err(ParseError::Syntax),
                    };
                    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
                0..0x20 => return Err(ParseError::Syntax),
                b => out.push(b),
            }
        }
    }

    /// A `\u` escape, pairing surrogates the way Go does: a pair is one
    /// character, anything unpaired is U+FFFD.
    fn unicode(&mut self) -> Result<char, ParseError> {
        let hi = self.hex4()?;
        if (0xD800..0xDC00).contains(&hi) && self.src[self.pos..].starts_with(b"\\u") {
            let save = self.pos;
            self.pos += 2;
            let lo = self.hex4()?;
            if (0xDC00..0xE000).contains(&lo) {
                return Ok(char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)).unwrap());
            }
            self.pos = save;
        }
        Ok(char::from_u32(hi).unwrap_or('\u{FFFD}'))
    }
}

/// A Go struct being decoded out of a value: fields are found by
/// case-insensitive name, the last one wins, a null leaves the field alone,
/// and a value of the wrong type is remembered as the error Go would return
/// once it had filled in everything else.
pub struct Fields<'a> {
    members: Vec<&'a Member>,
    pub mismatched: bool,
}

impl Value {
    pub fn fields(&self) -> Fields<'_> {
        match self {
            Value::Obj(m) => Fields { members: m.iter().collect(), mismatched: false },
            Value::Null => Fields { members: vec![], mismatched: false },
            _ => Fields { members: vec![], mismatched: true },
        }
    }

    /// The last member named exactly `key` — a Go map, not a struct.
    pub fn get(&self, key: &str) -> Option<&Member> {
        match self {
            Value::Obj(m) => m.iter().rev().find(|m| m.key == key),
            _ => None,
        }
    }
}

impl<'a> Fields<'a> {
    fn named(&self, name: &str) -> impl Iterator<Item = &'a Member> + '_ {
        let name = name.to_owned();
        self.members.iter().copied().filter(move |m| m.key.eq_ignore_ascii_case(&name))
    }

    pub fn string(&mut self, name: &str) -> String {
        let mut out = String::new();
        let mut bad = false;
        for m in self.named(name) {
            match &m.value {
                Value::Str(s) => out = s.clone(),
                Value::Null => {}
                _ => bad = true,
            }
        }
        self.mismatched |= bad;
        out
    }

    pub fn int(&mut self, name: &str) -> i64 {
        let mut out = 0;
        let mut bad = false;
        for m in self.named(name) {
            match &m.value {
                Value::Num(n) => match n.parse() {
                    Ok(v) => out = v,
                    Err(_) => bad = true,
                },
                Value::Null => {}
                _ => bad = true,
            }
        }
        self.mismatched |= bad;
        out
    }

    /// A number field that was sent, as an integer; None when it was absent,
    /// null, or not a whole number.
    pub fn get_num(&self, name: &str) -> Option<i64> {
        match &self.named(name).last()?.value {
            Value::Num(n) => n.parse().ok(),
            _ => None,
        }
    }

    /// A `json.RawMessage` field: the last member's bytes, whatever they hold.
    pub fn raw(&self, name: &str) -> Option<Range<usize>> {
        self.named(name).last().map(|m| m.raw.clone())
    }

    /// A nested struct field, merged across duplicates as Go merges them.
    pub fn nested(&mut self, name: &str) -> Fields<'a> {
        let mut out = Fields { members: vec![], mismatched: false };
        let mut bad = false;
        for m in self.named(name) {
            match &m.value {
                Value::Obj(inner) => out.members.extend(inner.iter()),
                Value::Null => {}
                _ => bad = true,
            }
        }
        self.mismatched |= bad;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_told_apart_the_way_go_tells_them() {
        assert_eq!(parse_first(b"  ").unwrap_err(), ParseError::Empty);
        assert_eq!(parse_first(br#"{"artifact": "#).unwrap_err(), ParseError::Truncated);
        assert_eq!(parse_first(b"{not json").unwrap_err(), ParseError::Syntax);
        assert!(parse_first(br#"{"a":1} trailing"#).is_ok());
    }

}
