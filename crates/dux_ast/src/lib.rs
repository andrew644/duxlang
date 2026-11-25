pub mod dump;
pub mod fmt;

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

/// One piece of a `$".."` literal.
#[derive(Clone, Debug, PartialEq)]
pub enum InterpolationPart {
    Text(String),
    Hole { written: Option<Expr>, span: Span },
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
    Div,
    Mul,
    Mod,
}

impl AssignOp {
    pub fn symbol(self) -> &'static str {
        match self {
            AssignOp::Set => "=",
            AssignOp::Add => "+=",
            AssignOp::Sub => "-=",
            AssignOp::Mul => "*=",
            AssignOp::Div => "/=",
            AssignOp::Mod => "%=",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
    AddressOf,
    Load,
}

impl UnaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            UnaryOp::Neg => "-",
            UnaryOp::Not => "!",
            UnaryOp::AddressOf => "&",
            UnaryOp::Load => "*",
        }
    }
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

    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Eq => "==",
            BinaryOp::NotEq => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::Gt => ">",
            BinaryOp::Le => "<=",
            BinaryOp::Ge => ">=",
            BinaryOp::And => "&&",
            BinaryOp::Or => "||",
        }
    }
}

/// A pattern: what a `match` arm or a pattern condition asks for. A bare
/// name is [`Pattern::Name`], because `None` and `pixels` look identical until
/// the checker knows whether the name names a variant.
#[derive(Clone, Debug, PartialEq)]
pub enum Pattern {
    Wildcard { span: Span },
    /// The `else =>` tail, which matches whatever is left.
    Else { span: Span },
    Name { name: Ident, span: Span },
    Literal { value: Box<Expr>, span: Span },
    Path { qualifier: String, name: String, span: Span },
    Tuple { qualifier: Option<String>, name: String, elements: Vec<Pattern>, span: Span },
    Struct {
        qualifier: Option<String>,
        name: String,
        fields: Vec<FieldPattern>,
        rest: bool,
        span: Span,
    },
    Range {
        start: Box<Pattern>,
        end: Box<Pattern>,
        inclusive: bool,
        span: Span,
    },
    Or { alternatives: Vec<Pattern>, span: Span },
    Error { span: Span },
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Wildcard { span }
            | Pattern::Else { span }
            | Pattern::Name { span, .. }
            | Pattern::Literal { span, .. }
            | Pattern::Path { span, .. }
            | Pattern::Tuple { span, .. }
            | Pattern::Struct { span, .. }
            | Pattern::Range { span, .. }
            | Pattern::Or { span, .. }
            | Pattern::Error { span } => *span,
        }
    }

    /// The names the pattern binds, in the order they are written.
    pub fn binders(&self) -> Vec<Ident> {
        match self {
            Pattern::Name { name, .. } => vec![name.clone()],
            Pattern::Tuple { elements, .. } => elements.iter().flat_map(Self::binders).collect(),
            Pattern::Struct { fields, .. } => fields.iter().flat_map(FieldPattern::binders).collect(),
            Pattern::Or { alternatives, .. } => {
                let mut names = Vec::new();
                for alternative in alternatives {
                    for binder in alternative.binders() {
                        if !names.iter().any(|known: &Ident| known.name == binder.name) {
                            names.push(binder);
                        }
                    }
                }
                names
            }
            Pattern::Range { start, end, .. } => {
                let mut binders = start.binders();
                binders.extend(end.binders());
                binders
            }
            _ => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldPattern {
    /// `{ w, h }`: the field is bound to a name of its own.
    Shorthand { name: Ident, span: Span },
    /// `{ w: Pattern::Literal(..) }`: the field is matched against a pattern.
    Nested { name: Ident, pattern: Box<Pattern>, span: Span },
}

impl FieldPattern {
    pub fn span(&self) -> Span {
        match self {
            FieldPattern::Shorthand { span, .. } | FieldPattern::Nested { span, .. } => *span,
        }
    }

    pub fn binders(&self) -> Vec<Ident> {
        match self {
            FieldPattern::Shorthand { name, .. } => vec![name.clone()],
            FieldPattern::Nested { pattern, .. } => pattern.binders(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MatchBody {
    Expr(Expr),
    Block(Block),
    /// `=> return e`, `=> break`: an arm may run a statement, which is how
    /// propagation is written while `?` is still on the shelf.
    Stmt(Stmt),
}

impl MatchBody {
    pub fn span(&self) -> Span {
        match self {
            MatchBody::Expr(expr) => expr.span(),
            MatchBody::Block(block) => block.span,
            MatchBody::Stmt(stmt) => stmt.span(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: MatchBody,
    pub span: Span,
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
    /// A name, possibly qualified (`Shape::Rect`), possibly with the type
    /// arguments written out (`max<i64>`, `HashMap<string, i32>`). An empty
    /// qualifier means the name stands alone.
    Path {
        qualifier: String,
        name: String,
        args: Vec<Type>,
        span: Span,
    },
    StructLiteral {
        name: String,
        args: Vec<Type>,
        fields: Vec<(Ident, Expr)>,
        span: Span,
    },
    /// The condition of `if P := e` / `while P := e`: a pattern matched against
    /// a value, binding on success.
    CondBinding {
        pattern: Pattern,
        value: Box<Expr>,
        span: Span,
    },
    Match {
        subject: Box<Expr>,
        arms: Vec<MatchArm>,
        span: Span,
    },
    /// A `$".."` literal broken into literal text and holes. A hole with no
    /// written expression (`{}`) takes its value from the trailing arguments of
    /// the call it is passed to, in order (README rule 10).
    InterpString {
        parts: Vec<InterpolationPart>,
        span: Span,
    },
    If {
        cond: Box<Expr>,
        then_arm: Box<Expr>,
        else_arm: Option<Box<Expr>>,
        span: Span,
    },
    /// A block used where a value is expected; its value is the block's last
    /// expression statement.
    Block {
        block: Block,
        value: Box<Expr>,
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
            | Expr::InterpString { span, .. }
            | Expr::CondBinding { span, .. }
            | Expr::Match { span, .. }
            | Expr::If { span, .. }
            | Expr::Block { span, .. }
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
    If {
        cond: Expr,
        then_branch: Block,
        else_branch: Option<Box<Stmt>>,
        span: Span,
    },
    While {
        cond: Expr,
        body: Block,
        span: Span,
    },
    Loop {
        body: Block,
        span: Span,
    },
    For {
        kind: ForKind,
        body: Block,
        span: Span,
    },
    Match {
        subject: Expr,
        arms: Vec<MatchArm>,
        span: Span,
    },
    Error {
        span: Span,
    },
}

/// `for` has three shapes that share a body: the C-style three clause form
/// (comma separated, `fizzbuzz.dux`), iteration over values or index+value, and
/// a bare `for { }` which is an infinite loop.
#[derive(Clone, Debug, PartialEq)]
pub enum ForKind {
    ThreeClause {
        init: Box<Stmt>,
        cond: Expr,
        step: Box<Stmt>,
    },
    In {
        index: Option<Ident>,
        value: Ident,
        iterable: Expr,
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
            | Stmt::If { span, .. }
            | Stmt::While { span, .. }
            | Stmt::Loop { span, .. }
            | Stmt::For { span, .. }
            | Stmt::Match { span, .. }
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
    /// The parameters of the type being extended: `impl Pair<A, B>`. A method
    /// of such an impl is code that means something once a record says what
    /// `A` and `B` are.
    pub type_params: Vec<TypeParam>,
    pub methods: Vec<FnDecl>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModuleDecl {
    pub attributes: Vec<Attribute>,
    pub name: Ident,
    /// A `mod ns;` says the namespace exists and nothing else. The body of an
    /// imported file arrives filled in: a `package`'s declarations, under the
    /// name the import wrote.
    pub decls: Vec<Decl>,
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

/// A declaration and the module it lives in, if any. `home` is written the way
/// a use would write it — `math.`, `std/io.`, or nothing at all for the file
/// that was named on the command line — so a name and its `home` say the one
/// name everything else knows the declaration by.
#[derive(Clone, Debug)]
pub struct Scoped<'a> {
    pub home: String,
    pub decl: &'a Decl,
}

impl Program {
    /// Every declaration in the program, including the ones a module body
    /// holds, each with the place it must be named from to be reached.
    pub fn scoped(&self) -> Vec<Scoped<'_>> {
        let mut out = Vec::new();
        Program::collect_scoped(&self.decls, "", &mut out);
        out
    }

    fn collect_scoped<'a>(decls: &'a [Decl], home: &str, out: &mut Vec<Scoped<'a>>) {
        for decl in decls {
            match decl {
                Decl::Module(module) => {
                    // The module itself is a declaration too: `mod c;` says
                    // nothing else, and a module with a body still names the
                    // place its contents live.
                    out.push(Scoped {
                        home: home.to_string(),
                        decl,
                    });
                    let nested = if home.is_empty() {
                        format!("{}.", module.name.name)
                    } else {
                        format!("{home}{}.", module.name.name)
                    };
                    Program::collect_scoped(&module.decls, &nested, out);
                }
                other => out.push(Scoped {
                    home: home.to_string(),
                    decl: other,
                }),
            }
        }
    }

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
