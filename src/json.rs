// Just enough JSON to read `cargo metadata`, to stay dependency-free.

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The elements of an array; empty for anything else, including null.
    pub fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(items) => items,
            _ => &[],
        }
    }
}

pub fn parse(text: &str) -> Option<Json> {
    let mut p = Parser {
        s: text.as_bytes(),
        at: 0,
    };
    let value = p.value()?;
    p.ws();
    (p.at == p.s.len()).then_some(value)
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        let hit = self.s[self.at..].starts_with(lit.as_bytes());
        if hit {
            self.at += lit.len();
        }
        hit
    }

    fn value(&mut self) -> Option<Json> {
        self.ws();
        match *self.s.get(self.at)? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Json::Str),
            b't' => self.eat("true").then_some(Json::Bool(true)),
            b'f' => self.eat("false").then_some(Json::Bool(false)),
            b'n' => self.eat("null").then_some(Json::Null),
            _ => self.number(),
        }
    }

    fn object(&mut self) -> Option<Json> {
        self.at += 1;
        let mut fields = Vec::new();
        self.ws();
        if self.eat("}") {
            return Some(Json::Obj(fields));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            if !self.eat(":") {
                return None;
            }
            fields.push((key, self.value()?));
            self.ws();
            if self.eat("}") {
                return Some(Json::Obj(fields));
            }
            if !self.eat(",") {
                return None;
            }
        }
    }

    fn array(&mut self) -> Option<Json> {
        self.at += 1;
        let mut items = Vec::new();
        self.ws();
        if self.eat("]") {
            return Some(Json::Arr(items));
        }
        loop {
            items.push(self.value()?);
            self.ws();
            if self.eat("]") {
                return Some(Json::Arr(items));
            }
            if !self.eat(",") {
                return None;
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat("\"") {
            return None;
        }
        let mut out = String::new();
        loop {
            let start = self.at;
            while !matches!(self.s.get(self.at)?, b'"' | b'\\') {
                self.at += 1;
            }
            out.push_str(std::str::from_utf8(&self.s[start..self.at]).ok()?);
            if self.eat("\"") {
                return Some(out);
            }
            self.at += 1;
            let escaped = *self.s.get(self.at)?;
            self.at += 1;
            match escaped {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'/' => out.push('/'),
                b'b' => out.push('\u{8}'),
                b'f' => out.push('\u{c}'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    let high = self.hex4()?;
                    let code = if (0xD800..0xDC00).contains(&high) && self.eat("\\u") {
                        let low = self.hex4()?;
                        0x10000 + ((high - 0xD800) << 10) + (low.checked_sub(0xDC00)?)
                    } else {
                        high
                    };
                    out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                }
                _ => return None,
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = std::str::from_utf8(self.s.get(self.at..self.at + 4)?).ok()?;
        self.at += 4;
        u32::from_str_radix(digits, 16).ok()
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.at;
        while self
            .s
            .get(self.at)
            .is_some_and(|b| matches!(b, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'))
        {
            self.at += 1;
        }
        let text = std::str::from_utf8(&self.s[start..self.at]).ok()?;
        text.parse().ok().map(Json::Num)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_cargo_metadata_emits() {
        let doc = parse(
            r#" {"packages":[{"name":"md-5","rename":null,"optional":false,
                "targets":[{"kind":["lib"],"name":"md5"}]}],
                "version":1, "ratio":-1.5e2, "ok":true,
                "path":"C:\\code\\x", "quote":"a\"b", "uni":"\u00e9\ud83d\ude00"} "#,
        )
        .unwrap();
        let pkg = &doc.get("packages").unwrap().arr()[0];
        assert_eq!(pkg.get("name").and_then(Json::str), Some("md-5"));
        assert_eq!(pkg.get("rename"), Some(&Json::Null));
        assert_eq!(pkg.get("rename").unwrap().arr(), &[] as &[Json]);
        assert_eq!(
            pkg.get("targets").unwrap().arr()[0]
                .get("name")
                .and_then(Json::str),
            Some("md5")
        );
        assert_eq!(doc.get("version"), Some(&Json::Num(1.0)));
        assert_eq!(doc.get("ratio"), Some(&Json::Num(-150.0)));
        assert_eq!(doc.get("ok"), Some(&Json::Bool(true)));
        assert_eq!(doc.get("path").and_then(Json::str), Some("C:\\code\\x"));
        assert_eq!(doc.get("quote").and_then(Json::str), Some("a\"b"));
        assert_eq!(doc.get("uni").and_then(Json::str), Some("é😀"));
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\" 1}",
            "\"unterminated",
            "{} extra",
            "tru",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }
}
