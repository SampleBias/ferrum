use crate::WireError;
use policy_types::MAX_JSON_DEPTH;

pub fn validate_json(text: &str) -> Result<(), WireError> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        index: 0,
        depth: 0,
    };
    parser.skip_ws()?;
    if parser.eof() {
        return Err(WireError::Structure);
    }
    parser.value()?;
    parser.skip_ws()?;
    if !parser.eof() {
        return Err(WireError::Structure);
    }
    Ok(())
}

struct Parser<'a> {
    bytes: &'a [u8],
    index: usize,
    depth: usize,
}

impl Parser<'_> {
    fn eof(&self) -> bool {
        self.index >= self.bytes.len()
    }

    fn peek(&self) -> Result<u8, WireError> {
        self.bytes.get(self.index).copied().ok_or(WireError::Structure)
    }

    fn bump(&mut self) -> Result<u8, WireError> {
        let byte = self.peek()?;
        self.index += 1;
        Ok(byte)
    }

    fn skip_ws(&mut self) -> Result<(), WireError> {
        while !self.eof() {
            match self.peek()? {
                b' ' | b'\n' | b'\r' | b'\t' => self.index += 1,
                _ => break,
            }
        }
        Ok(())
    }

    fn value(&mut self) -> Result<(), WireError> {
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => {
                self.string()?;
                Ok(())
            }
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            b'-' => Err(WireError::OutOfRange),
            b'0'..=b'9' => self.number(),
            _ => Err(WireError::Structure),
        }
    }

    fn enter(&mut self) -> Result<(), WireError> {
        self.depth += 1;
        if self.depth > MAX_JSON_DEPTH {
            return Err(WireError::TooDeep);
        }
        Ok(())
    }

    fn object(&mut self) -> Result<(), WireError> {
        self.bump()?;
        self.enter()?;
        self.skip_ws()?;
        let mut keys: Vec<String> = Vec::new();
        if self.peek()? == b'}' {
            self.bump()?;
            self.depth -= 1;
            return Ok(());
        }
        loop {
            if self.peek()? != b'"' {
                return Err(WireError::Structure);
            }
            let key = self.string()?;
            if keys.iter().any(|existing| existing == &key) {
                return Err(WireError::DuplicateKey);
            }
            keys.push(key);
            self.skip_ws()?;
            if self.bump()? != b':' {
                return Err(WireError::Structure);
            }
            self.skip_ws()?;
            self.value()?;
            self.skip_ws()?;
            match self.bump()? {
                b'}' => {
                    self.depth -= 1;
                    return Ok(());
                }
                b',' => {
                    self.skip_ws()?;
                }
                _ => return Err(WireError::Structure),
            }
        }
    }

    fn array(&mut self) -> Result<(), WireError> {
        self.bump()?;
        self.enter()?;
        self.skip_ws()?;
        if self.peek()? == b']' {
            self.bump()?;
            self.depth -= 1;
            return Ok(());
        }
        loop {
            self.value()?;
            self.skip_ws()?;
            match self.bump()? {
                b']' => {
                    self.depth -= 1;
                    return Ok(());
                }
                b',' => self.skip_ws()?,
                _ => return Err(WireError::Structure),
            }
        }
    }

    fn string(&mut self) -> Result<String, WireError> {
        if self.bump()? != b'"' {
            return Err(WireError::Structure);
        }
        let mut out = String::new();
        loop {
            match self.bump()? {
                b'"' => return Ok(out),
                b'\\' => {
                    let escaped = match self.bump()? {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{0008}',
                        b'f' => '\u{000c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode_escape()?,
                        _ => return Err(WireError::Structure),
                    };
                    out.push(escaped);
                }
                byte if byte < 0x20 => return Err(WireError::Structure),
                byte => {
                    let width = utf8_width(byte).ok_or(WireError::Utf8)?;
                    let start = self.index - 1;
                    if self.index + width - 1 > self.bytes.len() {
                        return Err(WireError::Utf8);
                    }
                    self.index += width - 1;
                    let text = std::str::from_utf8(&self.bytes[start..self.index])
                        .map_err(|_| WireError::Utf8)?;
                    out.push_str(text);
                }
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, WireError> {
        let mut unit = self.hex4()?;
        if (0xD800..=0xDBFF).contains(&unit) {
            if self.bump()? != b'\\' || self.bump()? != b'u' {
                return Err(WireError::Structure);
            }
            let low = self.hex4()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(WireError::Structure);
            }
            let value = 0x10000 + (((unit as u32) - 0xD800) << 10) + ((low as u32) - 0xDC00);
            return char::from_u32(value).ok_or(WireError::Structure);
        }
        if (0xDC00..=0xDFFF).contains(&unit) {
            return Err(WireError::Structure);
        }
        char::from_u32(u32::from(unit)).ok_or(WireError::Structure)
    }

    fn hex4(&mut self) -> Result<u16, WireError> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = self.bump()?;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(WireError::Structure),
            };
            value = (value << 4) | u16::from(digit);
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<(), WireError> {
        let start = self.peek()?;
        if start == b'0' {
            self.bump()?;
            if !self.eof() && self.peek()?.is_ascii_digit() {
                return Err(WireError::Structure);
            }
        } else {
            while !self.eof() && self.peek()?.is_ascii_digit() {
                self.bump()?;
            }
        }
        if !self.eof() {
            match self.peek()? {
                b'.' | b'e' | b'E' => return Err(WireError::Float),
                _ => {}
            }
        }
        Ok(())
    }

    fn literal(&mut self, text: &[u8]) -> Result<(), WireError> {
        for expected in text {
            if self.bump()? != *expected {
                return Err(WireError::Structure);
            }
        }
        Ok(())
    }
}

fn utf8_width(first: u8) -> Option<usize> {
    if first < 0x80 {
        Some(1)
    } else if first & 0xE0 == 0xC0 {
        Some(2)
    } else if first & 0xF0 == 0xE0 {
        Some(3)
    } else if first & 0xF8 == 0xF0 {
        Some(4)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nest(depth: usize) -> String {
        let mut text = String::new();
        for _ in 0..depth {
            text.push_str("{\"a\":");
        }
        text.push('1');
        for _ in 0..depth {
            text.push('}');
        }
        text
    }

    #[test]
    fn rejects_duplicates_depth_and_floats() {
        assert_eq!(
            validate_json(r#"{"a":1,"a":2}"#),
            Err(WireError::DuplicateKey)
        );
        assert_eq!(validate_json(r#"{"a":"\u0061"}"#).is_ok(), true);
        assert_eq!(
            validate_json(r#"{"a":1,"\u0061":2}"#),
            Err(WireError::DuplicateKey)
        );
        assert!(validate_json(&nest(6)).is_ok());
        assert_eq!(validate_json(&nest(7)), Err(WireError::TooDeep));
        assert_eq!(validate_json(r#"{"n":1.5}"#), Err(WireError::Float));
        assert_eq!(validate_json(r#"{"n":-1}"#), Err(WireError::OutOfRange));
    }
}
