//! Recursive-descent parser for the FHIRPath 2.0.0 grammar.

use rust_decimal::Decimal;

use crate::ast::{BinOp, Expr, UnaryOp};
use crate::error::Error;
use crate::value::Temporal;

pub fn parse(input: &str) -> Result<Expr, Error> {
    let mut parser = Parser {
        tokens: lex(input)?,
        index: 0,
    };
    if parser.tokens.is_empty() {
        return Err(Error::parse("empty expression"));
    }
    let expr = parser.parse_implies()?;
    if parser.index != parser.tokens.len() {
        return Err(Error::parse("unexpected token after expression"));
    }
    Ok(expr)
}

struct Parser {
    tokens: Vec<Tok>,
    index: usize,
}

impl Parser {
    fn parse_implies(&mut self) -> Result<Expr, Error> {
        let mut expr = self.parse_or()?;
        if self.eat_ident("implies") {
            let right = self.parse_implies()?;
            expr = Expr::binary(BinOp::Implies, expr, right);
        }
        Ok(expr)
    }

    fn parse_or(&mut self) -> Result<Expr, Error> {
        let mut expr = self.parse_and()?;
        loop {
            let op = if self.eat_ident("or") {
                BinOp::Or
            } else if self.eat_ident("xor") {
                BinOp::Xor
            } else {
                break;
            };
            expr = Expr::binary(op, expr, self.parse_and()?);
        }
        Ok(expr)
    }

    fn parse_and(&mut self) -> Result<Expr, Error> {
        let mut expr = self.parse_membership()?;
        while self.eat_ident("and") {
            expr = Expr::binary(BinOp::And, expr, self.parse_membership()?);
        }
        Ok(expr)
    }

    fn parse_membership(&mut self) -> Result<Expr, Error> {
        let mut expr = self.parse_equality()?;
        loop {
            let op = if self.eat_ident("in") {
                BinOp::In
            } else if self.eat_ident("contains") {
                BinOp::Contains
            } else {
                break;
            };
            expr = Expr::binary(op, expr, self.parse_equality()?);
        }
        Ok(expr)
    }

    fn parse_equality(&mut self) -> Result<Expr, Error> {
        let mut expr = self.parse_union()?;
        loop {
            let op = if self.eat(&Tok::NotEq) {
                BinOp::NotEq
            } else if self.eat(&Tok::Eq) {
                BinOp::Eq
            } else if self.eat(&Tok::NotEquiv) {
                BinOp::NotEquiv
            } else if self.eat(&Tok::Equiv) {
                BinOp::Equiv
            } else {
                break;
            };
            expr = Expr::binary(op, expr, self.parse_union()?);
        }
        Ok(expr)
    }

    fn parse_comparison(&mut self) -> Result<Expr, Error> {
        let mut expr = self.parse_add()?;
        loop {
            let op = if self.eat(&Tok::LtEq) {
                BinOp::LtEq
            } else if self.eat(&Tok::GtEq) {
                BinOp::GtEq
            } else if self.eat(&Tok::Lt) {
                BinOp::Lt
            } else if self.eat(&Tok::Gt) {
                BinOp::Gt
            } else {
                break;
            };
            expr = Expr::binary(op, expr, self.parse_add()?);
        }
        Ok(expr)
    }

    fn parse_type(&mut self) -> Result<Expr, Error> {
        let mut expr = self.parse_comparison()?;
        loop {
            if self.eat_ident("is") {
                expr = Expr::Is(Box::new(expr), self.parse_type_name()?);
            } else if self.eat_ident("as") {
                expr = Expr::As(Box::new(expr), self.parse_type_name()?);
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_union(&mut self) -> Result<Expr, Error> {
        let mut expr = self.parse_type()?;
        while self.eat(&Tok::Pipe) {
            expr = Expr::binary(BinOp::Union, expr, self.parse_type()?);
        }
        Ok(expr)
    }

    fn parse_add(&mut self) -> Result<Expr, Error> {
        let mut expr = self.parse_mul()?;
        loop {
            let op = if self.eat(&Tok::Plus) {
                BinOp::Add
            } else if self.eat(&Tok::Minus) {
                BinOp::Sub
            } else if self.eat(&Tok::Amp) {
                BinOp::Concat
            } else {
                break;
            };
            expr = Expr::binary(op, expr, self.parse_mul()?);
        }
        Ok(expr)
    }

    fn parse_mul(&mut self) -> Result<Expr, Error> {
        let mut expr = self.parse_unary()?;
        loop {
            let op = if self.eat(&Tok::Star) {
                BinOp::Mul
            } else if self.eat(&Tok::Slash) {
                BinOp::Div
            } else if self.eat_ident("div") {
                BinOp::TruncDiv
            } else if self.eat_ident("mod") {
                BinOp::Mod
            } else {
                break;
            };
            expr = Expr::binary(op, expr, self.parse_unary()?);
        }
        Ok(expr)
    }

    fn parse_unary(&mut self) -> Result<Expr, Error> {
        if self.eat(&Tok::Plus) {
            return Ok(Expr::Unary(UnaryOp::Plus, Box::new(self.parse_unary()?)));
        }
        if self.eat(&Tok::Minus) {
            return Ok(Expr::Unary(UnaryOp::Minus, Box::new(self.parse_unary()?)));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, Error> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.eat(&Tok::Dot) {
                let name = self.ident()?;
                if self.eat(&Tok::LParen) {
                    let args = self.parse_args(&name)?;
                    expr = Expr::Call {
                        focus: Some(Box::new(expr)),
                        name,
                        args,
                    };
                } else {
                    expr = Expr::Member(Box::new(expr), name);
                }
            } else if self.eat(&Tok::LBrack) {
                let index = self.parse_implies()?;
                self.expect(&Tok::RBrack)?;
                expr = Expr::Index(Box::new(expr), Box::new(index));
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, Error> {
        if self.eat(&Tok::LParen) {
            let expr = self.parse_implies()?;
            self.expect(&Tok::RParen)?;
            return Ok(expr);
        }
        if self.eat(&Tok::LBrace) {
            self.expect(&Tok::RBrace)?;
            return Ok(Expr::Empty);
        }
        if let Some(Tok::Int(value)) = self.peek().cloned() {
            self.index += 1;
            return Ok(self.finish_number(Expr::Int(value), true)?);
        }
        if let Some(Tok::Decimal(value, scale)) = self.peek().cloned() {
            self.index += 1;
            return Ok(self.finish_number(Expr::Decimal(value, scale), false)?);
        }
        if let Some(Tok::String(value)) = self.peek().cloned() {
            self.index += 1;
            return Ok(Expr::Str(value));
        }
        if let Some(Tok::Date(value)) = self.peek().cloned() {
            self.index += 1;
            return Ok(Expr::Date(value));
        }
        if let Some(Tok::DateTime(value)) = self.peek().cloned() {
            self.index += 1;
            return Ok(Expr::DateTime(value));
        }
        if let Some(Tok::Time(value)) = self.peek().cloned() {
            self.index += 1;
            return Ok(Expr::Time(value));
        }
        if self.eat(&Tok::Dollar) {
            return match self.ident()?.as_str() {
                "this" => Ok(Expr::This),
                "index" => Ok(Expr::IndexVar),
                "total" => Ok(Expr::Total),
                other => Err(Error::parse(format!("unknown ${other}"))),
            };
        }
        if self.eat(&Tok::Percent) {
            return Ok(Expr::Variable(self.ident()?));
        }
        if matches!(self.peek(), Some(Tok::Ident(_))) {
            let name = self.ident()?;
            if name == "true" {
                return Ok(Expr::Bool(true));
            }
            if name == "false" {
                return Ok(Expr::Bool(false));
            }
            if self.eat(&Tok::LParen) {
                let args = self.parse_args(&name)?;
                return Ok(Expr::Call {
                    focus: None,
                    name,
                    args,
                });
            }
            return Ok(Expr::Ident(name));
        }
        Err(Error::parse("expected a value"))
    }

    fn finish_number(&mut self, literal: Expr, _integer: bool) -> Result<Expr, Error> {
        let (value, scale) = match &literal {
            Expr::Int(value) => (Decimal::from(*value), 0),
            Expr::Decimal(value, scale) => (*value, *scale),
            _ => return Ok(literal),
        };
        if let Some(Tok::String(unit)) = self.peek().cloned() {
            self.index += 1;
            return Ok(Expr::Quantity(value, scale, unit));
        }
        if let Some(Tok::Ident(unit)) = self.peek().cloned() {
            if is_calendar_unit(&unit) {
                self.index += 1;
                return Ok(Expr::Quantity(value, scale, unit));
            }
        }
        Ok(literal)
    }

    fn parse_args(&mut self, name: &str) -> Result<Vec<Expr>, Error> {
        let mut args = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                if matches!(name, "as" | "ofType" | "is") {
                    args.push(Expr::TypeName(self.parse_type_name()?));
                } else {
                    args.push(self.parse_implies()?);
                }
                if self.eat(&Tok::Comma) {
                    continue;
                }
                self.expect(&Tok::RParen)?;
                break;
            }
        }
        Ok(args)
    }

    fn parse_type_name(&mut self) -> Result<String, Error> {
        let mut name = self.ident()?;
        while self.eat(&Tok::Dot) {
            name.push('.');
            name.push_str(&self.ident()?);
        }
        Ok(name)
    }

    fn ident(&mut self) -> Result<String, Error> {
        match self.tokens.get(self.index).cloned() {
            Some(Tok::Ident(value)) => {
                self.index += 1;
                Ok(value)
            }
            _ => Err(Error::parse("expected an identifier")),
        }
    }

    fn expect(&mut self, token: &Tok) -> Result<(), Error> {
        if self.eat(token) {
            Ok(())
        } else {
            Err(Error::parse(format!("expected {token:?}")))
        }
    }

    fn eat_ident(&mut self, name: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Ident(value)) if value == name) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn eat(&mut self, token: &Tok) -> bool {
        if self.peek() == Some(token) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.index)
    }
}

fn is_calendar_unit(name: &str) -> bool {
    matches!(
        name,
        "year"
            | "years"
            | "month"
            | "months"
            | "week"
            | "weeks"
            | "day"
            | "days"
            | "hour"
            | "hours"
            | "minute"
            | "minutes"
            | "second"
            | "seconds"
            | "millisecond"
            | "milliseconds"
    )
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    String(String),
    Int(i64),
    Decimal(Decimal, u32),
    Date(Temporal),
    DateTime(Temporal),
    Time(Temporal),
    Plus,
    Minus,
    Star,
    Slash,
    Pipe,
    Amp,
    Eq,
    NotEq,
    Equiv,
    NotEquiv,
    Lt,
    Gt,
    LtEq,
    GtEq,
    Dot,
    Comma,
    LParen,
    RParen,
    LBrack,
    RBrack,
    LBrace,
    RBrace,
    Dollar,
    Percent,
}

fn lex(input: &str) -> Result<Vec<Tok>, Error> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch.is_whitespace() {
            index += 1;
            continue;
        }
        if ch == '/' && chars.get(index + 1) == Some(&'/') {
            index += 2;
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            continue;
        }
        if ch == '/' && chars.get(index + 1) == Some(&'*') {
            index += 2;
            let mut closed = false;
            while index + 1 < chars.len() {
                if chars[index] == '*' && chars[index + 1] == '/' {
                    index += 2;
                    closed = true;
                    break;
                }
                index += 1;
            }
            if !closed {
                return Err(Error::parse("unterminated comment"));
            }
            continue;
        }
        if ch == '@' {
            let (token, next) = lex_temporal(&chars, index)?;
            tokens.push(token);
            index = next;
            continue;
        }
        if ch == '\'' || ch == '"' {
            let (value, next) = lex_string(&chars, index)?;
            tokens.push(Tok::String(value));
            index = next;
            continue;
        }
        if ch == '`' {
            let end = chars[index + 1..]
                .iter()
                .position(|item| *item == '`')
                .ok_or_else(|| Error::parse("unterminated identifier"))?;
            let name: String = chars[index + 1..index + 1 + end].iter().collect();
            tokens.push(Tok::Ident(name));
            index += end + 2;
            continue;
        }
        if ch.is_ascii_digit() {
            let (token, next) = lex_number(&chars, index)?;
            tokens.push(token);
            index = next;
            continue;
        }
        if ch == '_' || ch.is_ascii_alphabetic() {
            let start = index;
            index += 1;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '_')
            {
                index += 1;
            }
            let name: String = chars[start..index].iter().collect();
            tokens.push(Tok::Ident(name));
            continue;
        }
        let two = chars.get(index + 1).copied();
        let token = match (ch, two) {
            ('!', Some('=')) => {
                index += 2;
                Tok::NotEq
            }
            ('!', Some('~')) => {
                index += 2;
                Tok::NotEquiv
            }
            ('<', Some('=')) => {
                index += 2;
                Tok::LtEq
            }
            ('>', Some('=')) => {
                index += 2;
                Tok::GtEq
            }
            ('+', _) => {
                index += 1;
                Tok::Plus
            }
            ('-', _) => {
                index += 1;
                Tok::Minus
            }
            ('*', _) => {
                index += 1;
                Tok::Star
            }
            ('/', _) => {
                index += 1;
                Tok::Slash
            }
            ('|', _) => {
                index += 1;
                Tok::Pipe
            }
            ('&', _) => {
                index += 1;
                Tok::Amp
            }
            ('=', _) => {
                index += 1;
                Tok::Eq
            }
            ('~', _) => {
                index += 1;
                Tok::Equiv
            }
            ('<', _) => {
                index += 1;
                Tok::Lt
            }
            ('>', _) => {
                index += 1;
                Tok::Gt
            }
            ('.', _) => {
                index += 1;
                Tok::Dot
            }
            (',', _) => {
                index += 1;
                Tok::Comma
            }
            ('(', _) => {
                index += 1;
                Tok::LParen
            }
            (')', _) => {
                index += 1;
                Tok::RParen
            }
            ('[', _) => {
                index += 1;
                Tok::LBrack
            }
            (']', _) => {
                index += 1;
                Tok::RBrack
            }
            ('{', _) => {
                index += 1;
                Tok::LBrace
            }
            ('}', _) => {
                index += 1;
                Tok::RBrace
            }
            ('$', _) => {
                index += 1;
                Tok::Dollar
            }
            ('%', _) => {
                index += 1;
                Tok::Percent
            }
            _ => return Err(Error::parse(format!("unexpected character {ch}"))),
        };
        tokens.push(token);
    }
    Ok(tokens)
}

fn lex_string(chars: &[char], start: usize) -> Result<(String, usize), Error> {
    let quote = chars[start];
    let mut index = start + 1;
    let mut value = String::new();
    while index < chars.len() {
        let ch = chars[index];
        if ch == quote {
            return Ok((value, index + 1));
        }
        if ch == '\\' {
            let next = chars
                .get(index + 1)
                .copied()
                .ok_or_else(|| Error::parse("bad escape"))?;
            let escaped = match next {
                '\'' | '"' | '\\' | '/' => next,
                'r' => '\r',
                'n' => '\n',
                't' => '\t',
                'f' => '\u{000c}',
                '`' => '`',
                'u' => {
                    let hex: String = chars
                        .get(index + 2..index + 6)
                        .unwrap_or(&[])
                        .iter()
                        .collect();
                    index += 4;
                    char::from_u32(
                        u32::from_str_radix(&hex, 16)
                            .map_err(|_| Error::parse("bad unicode escape"))?,
                    )
                    .ok_or_else(|| Error::parse("bad unicode escape"))?
                }
                _ => return Err(Error::parse("bad escape")),
            };
            value.push(escaped);
            index += 2;
            continue;
        }
        value.push(ch);
        index += 1;
    }
    Err(Error::parse("unterminated string"))
}

fn lex_number(chars: &[char], start: usize) -> Result<(Tok, usize), Error> {
    let mut index = start;
    while index < chars.len() && chars[index].is_ascii_digit() {
        index += 1;
    }
    let mut scale = 0;
    let mut decimal = false;
    if chars.get(index) == Some(&'.') && chars.get(index + 1).is_some_and(|ch| ch.is_ascii_digit())
    {
        decimal = true;
        index += 1;
        let mark = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        scale = (index - mark) as u32;
    }
    let text: String = chars[start..index].iter().collect();
    if decimal {
        let value = Decimal::from_str_exact(&text).map_err(|_| Error::parse("bad decimal"))?;
        Ok((Tok::Decimal(value, scale), index))
    } else {
        let value = text
            .parse::<i64>()
            .map_err(|_| Error::parse("bad integer"))?;
        Ok((Tok::Int(value), index))
    }
}

fn lex_temporal(chars: &[char], start: usize) -> Result<(Tok, usize), Error> {
    let mut index = start + 1;
    if chars.get(index) == Some(&'T') {
        let (time, next) = lex_time(chars, index + 1)?;
        return Ok((Tok::Time(time), next));
    }
    let year = take_n(chars, &mut index, 4).ok_or_else(|| Error::parse("bad date"))?;
    let mut temporal = Temporal::date(
        year.parse().map_err(|_| Error::parse("bad date"))?,
        None,
        None,
    );
    if chars.get(index) == Some(&'-') {
        index += 1;
        temporal.month = Some(
            take_n(chars, &mut index, 2)
                .ok_or_else(|| Error::parse("bad date"))?
                .parse()
                .map_err(|_| Error::parse("bad date"))?,
        );
        if chars.get(index) == Some(&'-') {
            index += 1;
            temporal.day = Some(
                take_n(chars, &mut index, 2)
                    .ok_or_else(|| Error::parse("bad date"))?
                    .parse()
                    .map_err(|_| Error::parse("bad date"))?,
            );
        }
    }
    if chars.get(index) == Some(&'T') {
        index += 1;
        if chars.get(index).is_some_and(|ch| ch.is_ascii_digit()) {
            let time = lex_time_into(chars, &mut index)?;
            temporal.hour = time.hour;
            temporal.minute = time.minute;
            temporal.second = time.second;
            temporal.millis = time.millis;
            temporal.offset = time.offset;
        }
        return Ok((Tok::DateTime(temporal), index));
    }
    Ok((Tok::Date(temporal), index))
}

fn lex_time(chars: &[char], start: usize) -> Result<(Temporal, usize), Error> {
    let mut index = start;
    let time = lex_time_into(chars, &mut index)?;
    if time.offset.is_some() {
        return Err(Error::parse("time cannot carry a timezone"));
    }
    Ok((time, index))
}

fn lex_time_into(chars: &[char], index: &mut usize) -> Result<Temporal, Error> {
    let hour = take_n(chars, index, 2)
        .ok_or_else(|| Error::parse("bad time"))?
        .parse()
        .map_err(|_| Error::parse("bad time"))?;
    let mut time = Temporal {
        year: None,
        month: None,
        day: None,
        hour: Some(hour),
        minute: None,
        second: None,
        millis: None,
        offset: None,
    };
    if chars.get(*index) == Some(&':') {
        *index += 1;
        time.minute = Some(
            take_n(chars, index, 2)
                .ok_or_else(|| Error::parse("bad time"))?
                .parse()
                .map_err(|_| Error::parse("bad time"))?,
        );
        if chars.get(*index) == Some(&':') {
            *index += 1;
            time.second = Some(
                take_n(chars, index, 2)
                    .ok_or_else(|| Error::parse("bad time"))?
                    .parse()
                    .map_err(|_| Error::parse("bad time"))?,
            );
            if chars.get(*index) == Some(&'.')
                && chars.get(*index + 1).is_some_and(|ch| ch.is_ascii_digit())
            {
                *index += 1;
                let start = *index;
                while chars.get(*index).is_some_and(|ch| ch.is_ascii_digit()) {
                    *index += 1;
                }
                let digits: String = chars[start..*index].iter().collect();
                let padded = format!("{digits:0<3}");
                time.millis = Some(padded[..3].parse().map_err(|_| Error::parse("bad time"))?);
            }
        }
    }
    time.offset = lex_offset(chars, index)?;
    Ok(time)
}

fn lex_offset(chars: &[char], index: &mut usize) -> Result<Option<i16>, Error> {
    if chars.get(*index) == Some(&'Z') {
        *index += 1;
        return Ok(Some(0));
    }
    let sign = match chars.get(*index) {
        Some('+') => 1,
        Some('-') => -1,
        _ => return Ok(None),
    };
    *index += 1;
    let hour: i16 = take_n(chars, index, 2)
        .ok_or_else(|| Error::parse("bad timezone"))?
        .parse()
        .map_err(|_| Error::parse("bad timezone"))?;
    if chars.get(*index) == Some(&':') {
        *index += 1;
    }
    let minute: i16 = take_n(chars, index, 2)
        .ok_or_else(|| Error::parse("bad timezone"))?
        .parse()
        .map_err(|_| Error::parse("bad timezone"))?;
    Ok(Some(sign * (hour * 60 + minute)))
}

fn take_n(chars: &[char], index: &mut usize, len: usize) -> Option<String> {
    if *index + len > chars.len() {
        return None;
    }
    let text: String = chars[*index..*index + len].iter().collect();
    if !text.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    *index += len;
    Some(text)
}
