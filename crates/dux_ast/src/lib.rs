use dux_source::Span;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

impl Ident {
    pub fn new(name: impl Into<String>, span: Span) -> Self {
        Self { name: name.into(), span }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrKind {
    Utf8,
    Raw,
    CString,
    Interpolated,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    Named {
        name: String,
        args: Vec<Type>,
        span: Span,
    },
    Pointer {
        pointee: Box<Type>,
        span: Span,
    },
    Slice {
        element: Box<Type>,
        span: Span,
    },
    Array {
        element: Box<Type>,
        len: Box<Expr>,
        span: Span,
    },
    Tuple {
        elements: Vec<Type>,
        span: Span,
    },
    Error {
        span: Span,
    },
}

impl Type {
    pub fn span(&self) -> Span {
        match self {
            Type::Named { span, .. }
            | Type::Pointer { span, .. }
            | Type::Slice { span, .. }
            | Type::Array { span, .. }
            | Type::Tuple { span, .. }
            | Type::Error { span } => *span,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Set,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
    AddressOf,
    Load,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
}

impl BinaryOp {
    pub fn precedence(self) -> u8 {
        match self {
            BinaryOp::Or => 1,
            BinaryOp::And => 2,
            BinaryOp::Eq | BinaryOp::NotEq => 3,
            BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => 4,
            BinaryOp::Add | BinaryOp::Sub => 5,
            BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => 6,
        }
    }

    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            BinaryOp::Eq | BinaryOp::NotEq | BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Int { value: u128, span: Span },
    Float { value: f64, span: Span },
    Str { text: String, kind: StrKind, span: Span },
    Char { value: char, span: Span },
    Bool { value: bool, span: Span },
    Ident { name: String, span: Span },
    Call { callee: Box<Expr>, args: Vec<Expr>, span: Span },
    Field { base: Box<Expr>, name: Ident, span: Span },
    Index { base: Box<Expr>, index: Box<Expr>, span: Span },
    Unary { op: UnaryOp, operand: Box<Expr>, span: Span },
    Binary { op: BinaryOp, lhs: Box<Expr>, rhs: Box<Expr>, span: Span },
    Cast { value: Box<Expr>, target: Type, span: Span },
    Paren { inner: Box<Expr>, span: Span },
    Error { span: Span },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Int { span, .. }
            | Expr::Float { span, .. }
            | Expr::Str { span, .. }
            | Expr::Char { span, .. }
            | Expr::Bool { span, .. }
            | Expr::Ident { span, .. }
            | Expr::Call { span, .. }
            | Expr::Field { span, .. }
            | Expr::Index { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Cast { span, .. }
            | Expr::Paren { span, .. }
            | Expr::Error { span } => *span,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Binding {
        name: Ident,
        ty: Option<Type>,
        value: Option<Expr>,
        span: Span,
    },
    Assign {
        target: Expr,
        op: AssignOp,
        value: Expr,
        span: Span,
    },
    Expr {
        expr: Expr,
        span: Span,
    },
    Return {
        value: Option<Expr>,
        span: Span,
    },
    Break {
        span: Span,
    },
    Continue {
        span: Span,
    },
    Defer {
        stmt: Box<Stmt>,
        span: Span,
    },
    Error {
        span: Span,
    },
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Binding { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::Expr { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::Break { span }
            | Stmt::Continue { span }
            | Stmt::Defer { span, .. }
            | Stmt::Error { span } => *span,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: Ident,
    pub ty: Type,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FnDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub return_ty: Option<Type>,
    pub body: Block,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Import {
    pub path: String,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Decl {
    Fn(FnDecl),
    Error { span: Span },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Program {
    pub package: Option<Ident>,
    pub imports: Vec<Import>,
    pub decls: Vec<Decl>,
}

impl Program {
    pub fn functions(&self) -> impl Iterator<Item = &FnDecl> {
        self.decls.iter().filter_map(|d| match d {
            Decl::Fn(f) => Some(f),
            Decl::Error { .. } => None,
        })
    }

    pub fn find_function(&self, name: &str) -> Option<&FnDecl> {
        self.functions().find(|f| f.name.name == name)
    }
}
