//! Mutability checking: immutability-by-default, `mut`/`:=` assignment,
//! and `&`/`&mut` borrows. Values are freely shareable (immutable = safe
//! to share); memory is managed by automatic reference counting.

use std::collections::HashMap;

use crate::ast::*;
use crate::error::{Diagnostic, Span};

#[derive(Debug, Clone)]
struct Slot {
    mutable: bool,
}

struct Checker {
    errors: Vec<Diagnostic>,
    scopes: Vec<HashMap<String, Slot>>,
}

pub fn check(program: &Program) -> Vec<Diagnostic> {
    let mut c = Checker {
        errors: Vec::new(),
        scopes: vec![HashMap::new()],
    };
    for item in &program.items {
        if let Item::Function(f) = item {
            c.scopes[0].insert(
                f.name.clone(),
                Slot { mutable: false },
            );
        }
    }
    for item in &program.items {
        c.check_item(item);
    }
    c.errors
}

impl Checker {
    fn err(&mut self, code: u16, msg: impl Into<String>, span: Option<Span>) {
        self.errors.push(Diagnostic::error(code, msg, span));
    }

    fn lookup_mutable(&self, name: &str) -> bool {
        for frame in self.scopes.iter().rev() {
            if let Some(s) = frame.get(name) {
                return s.mutable;
            }
        }
        false
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn bind_pattern(&mut self, pat: &Pattern, mutable: bool) {
        match pat {
            Pattern::Var(name, _) => {
                self.scopes
                    .last_mut()
                    .unwrap()
                    .insert(name.clone(), Slot { mutable });
            }
            Pattern::Ctor { args, .. } => {
                for a in args {
                    self.bind_pattern(a, mutable);
                }
            }
            Pattern::Tuple(ps, _) | Pattern::List(ps, _) => {
                for p in ps {
                    self.bind_pattern(p, mutable);
                }
            }
            Pattern::Cons(a, b, _) => {
                self.bind_pattern(a, mutable);
                self.bind_pattern(b, mutable);
            }
            Pattern::As(name, inner, _) => {
                self.scopes
                    .last_mut()
                    .unwrap()
                    .insert(name.clone(), Slot { mutable });
                self.bind_pattern(inner, mutable);
            }
            Pattern::Record { fields, .. } => {
                for (_, p) in fields {
                    self.bind_pattern(p, mutable);
                }
            }
            _ => {}
        }
    }

    fn check_item(&mut self, item: &Item) {
        match item {
            Item::Function(f) => {
                for clause in &f.clauses {
                    self.push_scope();
                    for p in &clause.pats {
                        self.bind_pattern(p, false);
                    }
                    self.check_expr(&clause.body);
                    self.pop_scope();
                }
            }
            Item::Instance { methods, .. } => {
                for m in methods {
                    self.push_scope();
                    for p in &m.lhs {
                        self.bind_pattern(p, false);
                    }
                    self.check_expr(&m.expr);
                    self.pop_scope();
                }
            }
            _ => {}
        }
    }

    fn check_expr(&mut self, e: &Expr) {
        match e {
            Expr::Var(_, _) => {}
            Expr::App(f, a, _) => {
                self.check_expr(f);
                self.check_expr(a);
            }
            Expr::Lambda(pats, body, _) => {
                self.push_scope();
                for p in pats {
                    self.bind_pattern(p, false);
                }
                self.check_expr(body);
                self.pop_scope();
            }
            Expr::Let(bindings, body, _) => {
                self.push_scope();
                for b in bindings {
                    self.check_expr(&b.expr);
                    if let Some(Pattern::Var(name, _)) = b.lhs.first() {
                        self.scopes
                            .last_mut()
                            .unwrap()
                            .insert(name.clone(), Slot { mutable: b.mutable });
                    } else if let Some(pat) = b.lhs.first() {
                        self.bind_pattern(pat, b.mutable);
                    }
                }
                self.check_expr(body);
                self.pop_scope();
            }
            Expr::If(c, t, el, _) => {
                self.check_expr(c);
                self.check_expr(t);
                self.check_expr(el);
            }
            Expr::Case(scrut, alts, _) => {
                self.check_expr(scrut);
                for alt in alts {
                    self.push_scope();
                    self.bind_pattern(&alt.pat, false);
                    self.check_expr(&alt.body);
                    self.pop_scope();
                }
            }
            Expr::Tuple(es, _) | Expr::List(es, _) => {
                for x in es {
                    self.check_expr(x);
                }
            }
            Expr::Assign(name, e, span) => {
                if !self.lookup_mutable(name) {
                    self.err(
                        4002,
                        format!(
                            "cannot assign to immutable variable `{name}`; declare it with `mut`"
                        ),
                        Some(*span),
                    );
                }
                self.check_expr(e);
            }
            Expr::Borrow { mutable, expr, span } => {
                if *mutable
                    && let Expr::Var(name, _) = &**expr
                        && !self.lookup_mutable(name) {
                            self.err(
                                4003,
                                format!("cannot mutably borrow immutable variable `{name}`"),
                                Some(*span),
                            );
                        }
                self.check_expr(expr);
            }
            Expr::Deref(e, _) => self.check_expr(e),
            Expr::Clone(e, _) => self.check_expr(e),
            Expr::Field(e, _, _) => self.check_expr(e),
            Expr::Ascribe(e, _, _) => self.check_expr(e),
            Expr::Qualified(_, _, _) => {}
            Expr::Record { fields, .. } => {
                for (_, f) in fields {
                    self.check_expr(f);
                }
            }
            Expr::Int(_, _)
            | Expr::Float(_, _)
            | Expr::Str(_, _)
            | Expr::Char(_, _)
            | Expr::Unit(_)
            | Expr::Ctor(_, _) => {}
        }
    }
}
