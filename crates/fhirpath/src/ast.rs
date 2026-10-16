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

    /// Whether evaluating this expression can consult the [`Host`](crate::Host): it calls one of
    /// [`HOST_FUNCTIONS`](crate::host::HOST_FUNCTIONS) or uses a `%variable`, which the host may
    /// bind. Such an expression can give a different answer for the same resource at another time.
    pub fn depends_on_host(&self) -> bool {
        match self {
            Self::Variable(_) => true,
            Self::Call { name, focus, args } => {
                crate::host::HOST_FUNCTIONS.contains(&name.as_str())
                    || focus.as_ref().is_some_and(|focus| focus.depends_on_host())
                    || args.iter().any(|arg| arg.depends_on_host())
            }
            Self::Member(focus, _)
            | Self::Unary(_, focus)
            | Self::Is(focus, _)
            | Self::As(focus, _) => focus.depends_on_host(),
            Self::Index(left, right) | Self::Binary(_, left, right) => {
                left.depends_on_host() || right.depends_on_host()
            }
            _ => false,
        }
    }

    /// The operands of the `|` unions at the top of this expression: `a | b | c` gives `a`, `b` and
    /// `c`, and any other expression gives itself.
    pub fn union_operands(&self) -> Vec<&Expr> {
        match self {
            Self::Binary(BinOp::Union, left, right) => {
                let mut operands = left.union_operands();
                operands.extend(right.union_operands());
                operands
            }
            _ => vec![self],
        }
    }

    /// The name an expression reads from: `Observation` in `Observation.subject.where(..)`.
    pub fn root_name(&self) -> Option<&str> {
        match self {
            Self::Ident(name) => Some(name.as_str()),
            Self::Member(focus, _)
            | Self::Index(focus, _)
            | Self::Is(focus, _)
            | Self::As(focus, _)
            | Self::Call {
                focus: Some(focus), ..
            } => focus.root_name(),
            _ => None,
        }
    }

    /// `Type.a.b` as `("Type", ["a", "b"])`: a name followed by member accesses and nothing else.
    pub fn member_path(&self) -> Option<(&str, Vec<&str>)> {
        match self {
            Self::Ident(name) => Some((name.as_str(), Vec::new())),
            Self::Member(focus, member) => {
                let (root, mut members) = focus.member_path()?;
                members.push(member.as_str());
                Some((root, members))
            }
            _ => None,
        }
    }

    /// `focus.where(resolve() is Type)`: the focus, and the type its references are limited to.
    pub fn resolve_filter(&self) -> Option<(&Expr, &str)> {
        let Self::Call {
            focus: Some(focus),
            name,
            args,
        } = self
        else {
            return None;
        };
        let [Self::Is(subject, type_name)] = args.as_slice() else {
            return None;
        };
        let resolves = matches!(
            subject.as_ref(),
            Self::Call { focus: None, name, args } if name == "resolve" && args.is_empty()
        );
        (name == "where" && resolves).then_some((focus.as_ref(), type_name.as_str()))
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

#[cfg(test)]
mod tests {
    use crate::parse;

    #[test]
    fn a_union_is_read_operand_by_operand() {
        let expr = parse("A.x | B.y.where(resolve() is Patient) | (C.z as Reference)").unwrap();
        let roots: Vec<_> = expr
            .union_operands()
            .iter()
            .map(|operand| operand.root_name())
            .collect();
        assert_eq!(roots, [Some("A"), Some("B"), Some("C")]);
        // Nothing to split: the expression is its own operand.
        assert_eq!(parse("A.x").unwrap().union_operands().len(), 1);
    }

    #[test]
    fn a_path_is_a_name_and_its_members() {
        let path = |text: &str| {
            parse(text)
                .unwrap()
                .member_path()
                .map(|(root, members)| (root.to_string(), members.join(".")))
        };
        assert_eq!(path("A.b.c"), Some(("A".into(), "b.c".into())));
        assert_eq!(path("A"), Some(("A".into(), String::new())));
        assert_eq!(path("A.b.where(x = 1)"), None);
        assert_eq!(path("A.b[0]"), None);
        assert_eq!(path("(A.b as Reference)"), None);
    }

    #[test]
    fn a_resolve_filter_names_the_type_it_keeps() {
        let filter = |text: &str| {
            parse(text)
                .unwrap()
                .resolve_filter()
                .map(|(focus, type_name)| {
                    (
                        focus.member_path().map(|(_, m)| m.join(".")),
                        type_name.to_string(),
                    )
                })
        };
        assert_eq!(
            filter("Observation.subject.where(resolve() is Patient)"),
            Some((Some("subject".to_string()), "Patient".to_string()))
        );
        assert_eq!(filter("Observation.subject"), None);
        assert_eq!(
            filter("Observation.subject.where(resolve() is Patient).first()"),
            None
        );
        assert_eq!(filter("Observation.subject.where(type = 'x')"), None);
        assert_eq!(
            filter("Observation.subject.where(resolve().exists())"),
            None
        );
        assert_eq!(
            filter("Observation.subject.select(resolve() is Patient)"),
            None
        );
    }

    #[test]
    fn the_name_an_expression_reads_from_is_found_through_calls_and_casts() {
        let root = |text: &str| parse(text).unwrap().root_name().map(str::to_string);
        assert_eq!(root("Observation.subject"), Some("Observation".into()));
        assert_eq!(
            root("(DeviceRequest.code as Reference)"),
            Some("DeviceRequest".into())
        );
        assert_eq!(root("Bundle.entry[0].resource"), Some("Bundle".into()));
        assert_eq!(
            root("Measure.relatedArtifact.where(type = 'x').resource"),
            Some("Measure".into())
        );
        assert_eq!(root("%resource.id"), None);
        assert_eq!(root("resolve()"), None);
    }
}
