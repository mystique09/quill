//! Abstract syntax tree for Quill.

use crate::error::Span;

#[derive(Debug, Clone)]
pub struct Program {
    pub module: Option<String>,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone)]
pub enum Item {
    Use {
        path: String,
        span: Span,
    },
    TypeAlias {
        name: String,
        params: Vec<String>,
        ty: Type,
        span: Span,
    },
    Data {
        name: String,
        params: Vec<String>,
        ctors: Vec<Ctor>,
        span: Span,
    },
    Class {
        name: String,
        param: String,
        superclasses: Vec<Type>,
        methods: Vec<TypeSig>,
        span: Span,
    },
    Instance {
        class: Type,
        context: Vec<Type>,
        methods: Vec<Binding>,
        span: Span,
    },
    Signature {
        sig: TypeSig,
        span: Span,
    },
    Function(Function),
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub clauses: Vec<Clause>,
}

#[derive(Debug, Clone)]
pub struct Clause {
    pub pats: Vec<Pattern>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub mutable: bool,
    pub lhs: Vec<Pattern>,
    pub expr: Expr,
    pub span: Span,
}

impl Binding {
    /// Name of the binding if the left-hand side is a simple variable.
    pub fn name(&self) -> Option<&str> {
        match self.lhs.first() {
            Some(Pattern::Var(name, _)) => Some(name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Ctor {
    pub name: String,
    pub fields: Vec<Type>,
    pub record: Vec<FieldDecl>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FieldDecl {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TypeSig {
    pub name: String,
    pub constraints: Vec<Type>,
    pub body: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Type {
    Var(String, Span),
    Con(String, Vec<Type>, Span),
    Func(Box<Type>, Box<Type>, Span),
    Tuple(Vec<Type>, Span),
    List(Box<Type>, Span),
    Ref {
        mutable: bool,
        inner: Box<Type>,
        span: Span,
    },
    Unit(Span),
}

impl Type {
    pub fn span(&self) -> Span {
        match self {
            Type::Var(_, s)
            | Type::Con(_, _, s)
            | Type::Func(_, _, s)
            | Type::Tuple(_, s)
            | Type::List(_, s)
            | Type::Ref { span: s, .. }
            | Type::Unit(s) => *s,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Var(String, Span),
    Wildcard(Span),
    Int(i64, Span),
    Float(f64, Span),
    Str(String, Span),
    Char(char, Span),
    Ctor {
        name: String,
        args: Vec<Pattern>,
        span: Span,
    },
    Tuple(Vec<Pattern>, Span),
    List(Vec<Pattern>, Span),
    Cons(Box<Pattern>, Box<Pattern>, Span),
    As(String, Box<Pattern>, Span),
    Record {
        ctor: String,
        fields: Vec<(String, Pattern)>,
        span: Span,
    },
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Var(_, s)
            | Pattern::Wildcard(s)
            | Pattern::Int(_, s)
            | Pattern::Float(_, s)
            | Pattern::Str(_, s)
            | Pattern::Char(_, s)
            | Pattern::Ctor { span: s, .. }
            | Pattern::Tuple(_, s)
            | Pattern::List(_, s)
            | Pattern::Cons(_, _, s)
            | Pattern::As(_, _, s)
            | Pattern::Record { span: s, .. } => *s,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Expr {
    Int(i64, Span),
    Float(f64, Span),
    Str(String, Span),
    Char(char, Span),
    Unit(Span),
    Var(String, Span),
    /// A (possibly nullary) data constructor used as a value.
    Ctor(String, Span),
    Lambda(Vec<Pattern>, Box<Expr>, Span),
    App(Box<Expr>, Box<Expr>, Span),
    Let(Vec<Binding>, Box<Expr>, Span),
    If(Box<Expr>, Box<Expr>, Box<Expr>, Span),
    Case(Box<Expr>, Vec<Alt>, Span),
    Tuple(Vec<Expr>, Span),
    List(Vec<Expr>, Span),
    /// Mutable assignment `x := e`.
    Assign(String, Box<Expr>, Span),
    /// `&e` or `&mut e`.
    Borrow {
        mutable: bool,
        expr: Box<Expr>,
        span: Span,
    },
    /// `*e` dereference.
    Deref(Box<Expr>, Span),
    /// `clone e` explicit duplication.
    Clone(Box<Expr>, Span),
    /// `e.field` record projection.
    Field(Box<Expr>, String, Span),
    /// `Name { field = expr, ... }` record construction.
    Record {
        ctor: String,
        fields: Vec<(String, Expr)>,
        span: Span,
    },
    /// `e :: T` type ascription.
    Ascribe(Box<Expr>, Type, Span),
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Int(_, s)
            | Expr::Float(_, s)
            | Expr::Str(_, s)
            | Expr::Char(_, s)
            | Expr::Unit(s)
            | Expr::Var(_, s)
            | Expr::Ctor(_, s)
            | Expr::Lambda(_, _, s)
            | Expr::App(_, _, s)
            | Expr::Let(_, _, s)
            | Expr::If(_, _, _, s)
            | Expr::Case(_, _, s)
            | Expr::Tuple(_, s)
            | Expr::List(_, s)
            | Expr::Assign(_, _, s)
            | Expr::Borrow { span: s, .. }
            | Expr::Deref(_, s)
            | Expr::Clone(_, s)
            | Expr::Field(_, _, s)
            | Expr::Ascribe(_, _, s)
            | Expr::Record { span: s, .. } => *s,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Alt {
    pub pat: Pattern,
    pub body: Expr,
    pub span: Span,
}
