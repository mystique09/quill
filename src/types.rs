//! Type representation for Quill's Hindley-Milner inference engine.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TyCon {
    Int,
    Float,
    Bool,
    Char,
    String,
    Unit,
    /// Function arrow `->`.
    Func,
    /// Tuple of a given arity.
    Tuple(usize),
    /// Built-in list ADT.
    List,
    /// Immutable reference `&T`.
    Ref,
    /// Mutable reference `&mut T`.
    MutRef,
    /// A user-defined algebraic data type, indexed into the ADT table.
    Adt(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    /// A unification variable.
    Var(u32),
    /// A type constructor applied to arguments.
    Con(TyCon, Vec<Ty>),
}

/// A class constraint, e.g. `Eq a`.
#[derive(Debug, Clone, PartialEq)]
pub struct Constraint {
    pub class: String,
    pub ty: Ty,
}

#[derive(Debug, Clone)]
pub struct Scheme {
    pub vars: Vec<u32>,
    pub constraints: Vec<Constraint>,
    pub ty: Ty,
}

impl Scheme {
    pub fn mono(ty: Ty) -> Scheme {
        Scheme {
            vars: Vec::new(),
            constraints: Vec::new(),
            ty,
        }
    }
}

impl Ty {
    pub fn func(a: Ty, b: Ty) -> Ty {
        Ty::Con(TyCon::Func, vec![a, b])
    }
    pub fn int() -> Ty {
        Ty::Con(TyCon::Int, vec![])
    }
    pub fn float() -> Ty {
        Ty::Con(TyCon::Float, vec![])
    }
    pub fn bool() -> Ty {
        Ty::Con(TyCon::Bool, vec![])
    }
    pub fn char() -> Ty {
        Ty::Con(TyCon::Char, vec![])
    }
    pub fn string() -> Ty {
        Ty::Con(TyCon::String, vec![])
    }
    pub fn unit() -> Ty {
        Ty::Con(TyCon::Unit, vec![])
    }
    pub fn list(elem: Ty) -> Ty {
        Ty::Con(TyCon::List, vec![elem])
    }

    pub fn is_copy(&self) -> bool {
        matches!(
            self,
            Ty::Con(TyCon::Int, _)
                | Ty::Con(TyCon::Float, _)
                | Ty::Con(TyCon::Bool, _)
                | Ty::Con(TyCon::Char, _)
                | Ty::Con(TyCon::Ref, _)
                | Ty::Con(TyCon::Func, _)
        )
    }
}

fn tyvar_name(id: u32) -> String {
    const LETTERS: &str = "abcdefghijklmnopqrstuvwxyz";
    if (id as usize) < LETTERS.len() {
        LETTERS.chars().nth(id as usize).unwrap().to_string()
    } else {
        format!("t{}", id)
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Var(id) => write!(f, "{}", tyvar_name(*id)),
            Ty::Con(tc, args) => match tc {
                TyCon::Int => write!(f, "Int"),
                TyCon::Float => write!(f, "Float"),
                TyCon::Bool => write!(f, "Bool"),
                TyCon::Char => write!(f, "Char"),
                TyCon::String => write!(f, "String"),
                TyCon::Unit => write!(f, "()"),
                TyCon::Func => {
                    write!(f, "{}", paren_if_func(&args[0]))?;
                    write!(f, " -> ")?;
                    write!(f, "{}", args[1])
                }
                TyCon::Tuple(n) => {
                    write!(f, "(")?;
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{a}")?;
                    }
                    if *n == 1 {
                        write!(f, ",")?;
                    }
                    write!(f, ")")
                }
                TyCon::List => write!(f, "[{}]", args[0]),
                TyCon::Ref => write!(f, "&{}", args[0]),
                TyCon::MutRef => write!(f, "&mut {}", args[0]),
                TyCon::Adt(id) => {
                    // The name is supplied via a display context; fall back to index.
                    write!(f, "<adt {id}>")?;
                    if !args.is_empty() {
                        write!(f, " {}", args.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(" "))?;
                    }
                    Ok(())
                }
            },
        }
    }
}

fn paren_if_func(t: &Ty) -> String {
    if matches!(t, Ty::Con(TyCon::Func, _)) {
        format!("({t})")
    } else {
        t.to_string()
    }
}

impl fmt::Display for Constraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.class, self.ty)
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.constraints.is_empty() {
            write!(f, "{}", self.ty)
        } else {
            let ctx = self
                .constraints
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            write!(f, "({ctx}) => {}", self.ty)
        }
    }
}
