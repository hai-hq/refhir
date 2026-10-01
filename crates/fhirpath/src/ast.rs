use rust_decimal::Decimal;

use crate::value::Temporal;

#[derive(Clone, Debug)]
pub enum Expr {
    Empty,
    Bool(bool),
    Int(i64),
    Decimal(Decimal, u32),
    Str(String),
    Date(Temporal),
    DateTime(Temporal),
    Time(Temporal),
    Quantity(Decimal, u32, String),
    Ident(String),
    TypeName(String),
    This,
    IndexVar,
    Total,
    Variable(String),
    Member(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Call {
        focus: Option<Box<Expr>>,
        name: String,
        args: Vec<Expr>,
    },
    Unary(UnaryOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Is(Box<Expr>, String),
    As(Box<Expr>, String),
}

#[derive(Clone, Copy, Debug)]
pub enum UnaryOp {
    Plus,
    Minus,
}

#[derive(Clone, Copy, Debug)]
pub enum BinOp {
    Implies,
    Or,
    Xor,
    And,
    In,
    Contains,
    Eq,
    NotEq,
    Equiv,
    NotEquiv,
    Lt,
    Gt,
    LtEq,
    GtEq,
    Union,
    Add,
    Sub,
    Concat,
    Mul,
    Div,
    TruncDiv,
    Mod,
}

impl Expr {
    pub fn binary(op: BinOp, left: Expr, right: Expr) -> Self {
        Self::Binary(op, Box::new(left), Box::new(right))
    }

    pub fn calls(&self, name: &str) -> bool {
        match self {
            Self::Call {
                name: called,
                focus,
                args,
                ..
            } => {
                called == name
                    || focus.as_ref().is_some_and(|focus| focus.calls(name))
                    || args.iter().any(|arg| arg.calls(name))
            }
            Self::Member(focus, _)
            | Self::Unary(_, focus)
            | Self::Is(focus, _)
            | Self::As(focus, _) => focus.calls(name),
            Self::Index(left, right) | Self::Binary(_, left, right) => {
                left.calls(name) || right.calls(name)
            }
            _ => false,
        }
    }
}
