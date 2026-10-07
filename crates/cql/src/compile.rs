//! Recursive-descent compiler for the CQL subset SDC questionnaires use.
//! Output is ELM JSON evaluated by `elm`.

use serde_json::{json, Value as Json};

pub fn compile(source: &str) -> Result<Json, String> {
    let mut parser = Parser::new(source);
    parser.parse_library()
}

struct Parser<'a> {
    source: &'a str,
    index: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, index: 0 }
    }

    fn parse_library(&mut self) -> Result<Json, String> {
        let mut statements = Vec::new();
        let mut name = "inline".to_string();
        self.skip();
        if self.eat_word("library") {
            name = self.ident()?;
            if self.eat_word("version") {
                let _ = self.string()?;
            }
            self.skip();
        }
        while self.eat_word("using") {
            self.skip_line();
        }
        while self.eat_word("parameter") {
            let _ = self.ident()?;
            let _ = self.ident().ok();
            self.skip();
        }
        if self.eat_word("context") {
            let _ = self.ident()?;
            self.skip();
        }
        if self.eat_word("define") {
            while self.peek().is_some() {
                let stmt = self.string().or_else(|_| self.ident())?;
                self.eat(':');
                let expression = self.expr()?;
                statements.push(json!({"name": stmt, "expression": expression}));
                self.skip();
                if !self.eat_word("define") {
                    break;
                }
            }
        } else if self.peek().is_some() {
            let expression = self.expr()?;
            statements.push(json!({"name": "expr", "expression": expression}));
        }
        if statements.is_empty() {
            return Err("cql expression is empty".into());
        }
        Ok(json!({
            "library": {
                "identifier": {"id": name, "version": "1.0.0"},
                "statements": {"def": statements}
            }
        }))
    }

    fn expr(&mut self) -> Result<Json, String> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Json, String> {
        let mut left = self.and_expr()?;
        while self.eat_word("or") {
            let right = self.and_expr()?;
            left = json!({"type": "Or", "operand": [left, right]});
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Json, String> {
        let mut left = self.not_expr()?;
        while self.eat_word("and") {
            let right = self.not_expr()?;
            left = json!({"type": "And", "operand": [left, right]});
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Json, String> {
        if self.eat_word("not") {
            let operand = self.not_expr()?;
            return Ok(json!({"type": "Not", "operand": operand}));
        }
        if self.eat_word("if") {
            let condition = self.expr()?;
            if !self.eat_word("then") {
                return Err("cql if is missing then".into());
            }
            let then_expr = self.expr()?;
            if !self.eat_word("else") {
                return Err("cql if is missing else".into());
            }
            let else_expr = self.expr()?;
            return Ok(json!({
                "type": "If",
                "condition": condition,
                "then": then_expr,
                "else": else_expr
            }));
        }
        self.cmp_expr()
    }

    fn cmp_expr(&mut self) -> Result<Json, String> {
        let mut left = self.add_expr()?;
        loop {
            let op = if self.eat_str("!=") {
                "NotEqual"
            } else if self.eat_str(">=") {
                "GreaterOrEqual"
            } else if self.eat_str("<=") {
                "LessOrEqual"
            } else if self.eat('=') {
                "Equal"
            } else if self.eat('>') {
                "Greater"
            } else if self.eat('<') {
                "Less"
            } else {
                break;
            };
            let right = self.add_expr()?;
            left = json!({"type": op, "operand": [left, right]});
        }
        Ok(left)
    }

    fn add_expr(&mut self) -> Result<Json, String> {
        let mut left = self.mul_expr()?;
        loop {
            let op = if self.eat('+') {
                "Add"
            } else if self.eat('-') {
                "Subtract"
            } else {
                break;
            };
            let right = self.mul_expr()?;
            left = json!({"type": op, "operand": [left, right]});
        }
        Ok(left)
    }

    fn mul_expr(&mut self) -> Result<Json, String> {
        let mut left = self.unary()?;
        loop {
            let op = if self.eat('*') {
                "Multiply"
            } else if self.eat('/') {
                "Divide"
            } else {
                break;
            };
            let right = self.unary()?;
            left = json!({"type": op, "operand": [left, right]});
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Json, String> {
        if self.eat('-') {
            let operand = self.unary()?;
            return Ok(json!({
                "type": "Subtract",
                "operand": [{"type": "Literal", "valueType": "Integer", "value": 0}, operand]
            }));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Json, String> {
        let mut left = self.primary()?;
        loop {
            if self.eat('.') {
                let name = self.ident()?;
                if self.eat('(') {
                    let args = self.arg_list()?;
                    left = json!({"type": name, "operand": args, "source": left});
                } else {
                    left = json!({"type": "Property", "path": name, "source": left});
                }
            } else if self.eat('(') {
                let args = self.arg_list()?;
                let name = left
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("expr")
                    .to_string();
                left = json!({"type": name, "operand": args});
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn arg_list(&mut self) -> Result<Vec<Json>, String> {
        let mut args = Vec::new();
        self.skip();
        if self.eat(')') {
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            if self.eat(')') {
                break;
            }
            if !self.eat(',') {
                return Err("cql argument list is not closed".into());
            }
        }
        Ok(args)
    }

    fn primary(&mut self) -> Result<Json, String> {
        self.skip();
        if self.eat('[') {
            return self.retrieve();
        }
        if self.eat('(') {
            let expr = self.expr()?;
            if !self.eat(')') {
                return Err("cql group is not closed".into());
            }
            return Ok(expr);
        }
        if self.peek_char() == Some('\'') || self.peek_char() == Some('"') {
            let text = self.string()?;
            return Ok(json!({"type": "Literal", "valueType": "String", "value": text}));
        }
        if self.peek_char().is_some_and(|ch| ch.is_ascii_digit()) {
            let number = self.number()?;
            if number.contains('.') {
                return Ok(json!({"type": "Literal", "valueType": "Decimal", "value": number}));
            }
            let value: i64 = number.parse().map_err(|err| format!("{err}"))?;
            return Ok(json!({"type": "Literal", "valueType": "Integer", "value": value}));
        }
        let name = self.ident()?;
        match name.as_str() {
            "true" => Ok(json!({"type": "Literal", "valueType": "Boolean", "value": true})),
            "false" => Ok(json!({"type": "Literal", "valueType": "Boolean", "value": false})),
            "null" => Ok(json!({"type": "Null"})),
            "today" => Ok(json!({"type": "Today"})),
            "now" => Ok(json!({"type": "Now"})),
            _ => Ok(json!({"type": "Identifier", "name": name})),
        }
    }

    fn retrieve(&mut self) -> Result<Json, String> {
        let type_name = self.ident()?;
        let mut code = None;
        self.skip();
        if self.eat(':') {
            code = Some(self.string()?);
        }
        if !self.eat(']') {
            return Err("cql retrieve is not closed".into());
        }
        let mut expr = json!({"type": "Retrieve", "dataType": type_name});
        if let Some(code) = code {
            expr["code"] = json!(code);
        }
        Ok(expr)
    }

    fn ident(&mut self) -> Result<String, String> {
        self.skip();
        let start = self.index;
        let bytes = self.source.as_bytes();
        if start >= bytes.len() || !is_ident_start(bytes[start] as char) {
            return Err(format!(
                "expected a name near '{}'",
                self.source.get(start..).unwrap_or("").chars().take(12).collect::<String>()
            ));
        }
        self.index += 1;
        while self.index < bytes.len() && is_ident_continue(bytes[self.index] as char) {
            self.index += 1;
        }
        Ok(self.source[start..self.index].to_string())
    }

    fn string(&mut self) -> Result<String, String> {
        self.skip();
        let quote = self.peek_char().ok_or("expected a string")?;
        if quote != '\'' && quote != '"' {
            return Err("expected a string".into());
        }
        self.index += 1;
        let start = self.index;
        while self.index < self.source.len() {
            let ch = self.source[self.index..].chars().next().unwrap();
            if ch == quote {
                let text = self.source[start..self.index].to_string();
                self.index += quote.len_utf8();
                return Ok(text);
            }
            self.index += ch.len_utf8();
        }
        Err("unterminated string".into())
    }

    fn number(&mut self) -> Result<String, String> {
        self.skip();
        let start = self.index;
        while self.peek_char().is_some_and(|ch| ch.is_ascii_digit() || ch == '.') {
            self.index += 1;
        }
        if start == self.index {
            return Err("expected a number".into());
        }
        Ok(self.source[start..self.index].to_string())
    }

    fn eat_word(&mut self, word: &str) -> bool {
        self.skip();
        let rest = &self.source[self.index..];
        if rest.len() < word.len() || !rest[..word.len()].eq_ignore_ascii_case(word) {
            return false;
        }
        let after = rest[word.len()..].chars().next();
        if after.is_some_and(|ch| is_ident_continue(ch)) {
            return false;
        }
        self.index += word.len();
        true
    }

    fn eat_str(&mut self, token: &str) -> bool {
        self.skip();
        if self.source[self.index..].starts_with(token) {
            self.index += token.len();
            true
        } else {
            false
        }
    }

    fn eat(&mut self, ch: char) -> bool {
        self.skip();
        if self.peek_char() == Some(ch) {
            self.index += ch.len_utf8();
            true
        } else {
            false
        }
    }

    fn skip_line(&mut self) {
        while self.peek_char().is_some_and(|ch| ch != '\n') {
            self.index += 1;
        }
        self.skip();
    }

    fn skip(&mut self) {
        while self.peek_char().is_some_and(|ch| ch.is_whitespace()) {
            self.index += self.peek_char().unwrap().len_utf8();
        }
    }

    fn peek(&self) -> Option<char> {
        self.skip_view()
    }

    fn skip_view(&self) -> Option<char> {
        self.source[self.index..]
            .chars()
            .find(|ch| !ch.is_whitespace())
    }

    fn peek_char(&self) -> Option<char> {
        self.source[self.index..].chars().next()
    }
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || ch == '%'
}

fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '%'
}
