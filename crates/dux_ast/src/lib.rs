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
    Function {
        params: Vec<Type>,
        return_ty: Option<Box<Type>>,
        span: Span,
    },
    /// A value passed where a type is expected, as in `zeros<4>()` for a
    /// `const N: usize` parameter.
    ConstArgument {
        value: Box<Expr>,
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
            | Type::Function { span, .. }
            | Type::ConstArgument { span, .. }
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
    Range {
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        inclusive: bool,
        span: Span,
    },
    Member {
        base: Box<Expr>,
        index: u32,
        span: Span,
    },
    ArrayLiteral {
        elements: Vec<Expr>,
        span: Span,
    },
    RepeatedArray {
        value: Box<Expr>,
        len: Box<Expr>,
        span: Span,
    },
    Path {
        qualifier: String,
        name: String,
        span: Span,
    },
    StructLiteral {
        name: String,
        fields: Vec<(Ident, Expr)>,
        span: Span,
    },
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
            | Expr::Range { span, .. }
            | Expr::Member { span, .. }
            | Expr::ArrayLiteral { span, .. }
            | Expr::RepeatedArray { span, .. }
            | Expr::Path { span, .. }
            | Expr::StructLiteral { span, .. }
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
    Block {
        block: Block,
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
            | Stmt::Block { span, .. }
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
    pub attributes: Vec<Attribute>,
    pub name: Ident,
    pub type_params: Vec<TypeParam>,
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
pub struct Attribute {
    pub name: String,
    pub arguments: Vec<AttributeArgument>,
    pub span: Span,
}

impl Attribute {
    pub fn string_argument(&self) -> Option<&str> {
        self.arguments.first().and_then(|a| match a {
            AttributeArgument::Literal(text) => Some(text.as_str()),
            AttributeArgument::Named(_, _) => None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AttributeArgument {
    Literal(String),
    Named(String, String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeParam {
    pub name: Ident,
    pub is_const: bool,
    pub ty: Option<Type>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: Ident,
    pub ty: Type,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StructDecl {
    pub attributes: Vec<Attribute>,
    pub name: Ident,
    pub type_params: Vec<TypeParam>,
    pub fields: Vec<Field>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub name: Ident,
    pub body: VariantBody,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum VariantBody {
    Empty,
    Tuple(Vec<Type>),
    Named(Vec<Field>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumDecl {
    pub attributes: Vec<Attribute>,
    pub name: Ident,
    pub type_params: Vec<TypeParam>,
    pub variants: Vec<Variant>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConstDecl {
    pub attributes: Vec<Attribute>,
    pub name: Ident,
    pub ty: Option<Type>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GlobalDecl {
    pub attributes: Vec<Attribute>,
    pub name: Ident,
    pub ty: Option<Type>,
    pub value: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeAliasDecl {
    pub name: Ident,
    pub type_params: Vec<TypeParam>,
    pub aliased: Type,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImplDecl {
    pub type_name: Ident,
    pub methods: Vec<FnDecl>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModuleDecl {
    pub attributes: Vec<Attribute>,
    pub name: Ident,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExternBlock {
    pub abi: String,
    pub attributes: Vec<Attribute>,
    pub functions: Vec<FnDecl>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Decl {
    Fn(FnDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    Const(ConstDecl),
    Global(GlobalDecl),
    TypeAlias(TypeAliasDecl),
    Impl(ImplDecl),
    Module(ModuleDecl),
    Extern(ExternBlock),
    Error { span: Span },
}

impl Decl {
    pub fn span(&self) -> Span {
        match self {
            Decl::Fn(decl) => decl.span,
            Decl::Struct(decl) => decl.span,
            Decl::Enum(decl) => decl.span,
            Decl::Const(decl) => decl.span,
            Decl::Global(decl) => decl.span,
            Decl::TypeAlias(decl) => decl.span,
            Decl::Impl(decl) => decl.span,
            Decl::Module(decl) => decl.span,
            Decl::Extern(decl) => decl.span,
            Decl::Error { span } => *span,
        }
    }

    pub fn attributes(&self) -> &[Attribute] {
        match self {
            Decl::Fn(decl) => &decl.attributes,
            Decl::Struct(decl) => &decl.attributes,
            Decl::Enum(decl) => &decl.attributes,
            Decl::Const(decl) => &decl.attributes,
            Decl::Global(decl) => &decl.attributes,
            Decl::Module(decl) => &decl.attributes,
            Decl::Extern(decl) => &decl.attributes,
            Decl::TypeAlias(_) | Decl::Impl(_) | Decl::Error { .. } => &[],
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Program {
    pub package: Option<Ident>,
    pub imports: Vec<Import>,
    pub decls: Vec<Decl>,
}

impl Program {
    pub fn functions(&self) -> impl Iterator<Item = &FnDecl> {
        self.decls.iter().filter_map(|decl| match decl {
            Decl::Fn(function) => Some(function),
            _ => None,
        })
    }

    pub fn methods(&self) -> impl Iterator<Item = &FnDecl> {
        self.decls.iter().filter_map(|decl| match decl {
            Decl::Impl(impl_block) => Some(impl_block.methods.iter()),
            _ => None,
        }).flatten()
    }

    pub fn structs(&self) -> impl Iterator<Item = &StructDecl> {
        self.decls.iter().filter_map(|decl| match decl {
            Decl::Struct(decl) => Some(decl),
            _ => None,
        })
    }

    pub fn enums(&self) -> impl Iterator<Item = &EnumDecl> {
        self.decls.iter().filter_map(|decl| match decl {
            Decl::Enum(decl) => Some(decl),
            _ => None,
        })
    }

    pub fn find_function(&self, name: &str) -> Option<&FnDecl> {
        self.functions().find(|f| f.name.name == name)
    }

    pub fn find_struct(&self, name: &str) -> Option<&StructDecl> {
        self.structs().find(|s| s.name.name == name)
    }

    pub fn find_enum(&self, name: &str) -> Option<&EnumDecl> {
        self.enums().find(|e| e.name.name == name)
    }
}
