//! Ownership and borrow checking: immutability-by-default, move semantics,
//! and explicit borrows — Quill's take on Rust's core power features.

use std::collections::HashMap;

use crate::ast::*;
use crate::error::{Diagnostic, Span};
use crate::infer::TypeInfo;

#[derive(Debug, Clone)]
struct Slot {
    copy: bool,
    mutable: bool,
    moved: bool,
}

struct Checker<'a> {
    types: &'a TypeInfo,
    errors: Vec<Diagnostic>,
    scopes: Vec<HashMap<String, Slot>>,
}

pub fn check(program: &Program, types: &TypeInfo) -> Vec<Diagnostic> {
    let mut c = Checker {
        types,
        errors: Vec::new(),
        scopes: vec![HashMap::new()],
    };
    // Register top-level names.
    for item in &program.items {
        if let Item::Function(f) = item {
            let copy = types
                .top_types
                .get(&f.name)
                .map(|t| t.is_copy())
                .unwrap_or(false);
            c.scopes[0].insert(
                f.name.clone(),
                Slot {
                    copy,
                    mutable: false,
                    moved: false,
                },
            );
        }
    }
    for item in &program.items {
        c.check_item(item);
    }
    c.errors
}

impl<'a> Checker<'a> {
    fn err(&mut self, code: u16, msg: impl Into<String>, span: Option<Span>) {
        self.errors.push(Diagnostic::error(code, msg, span));
    }

    fn lookup(&self, name: &str) -> Option<&Slot> {
        for frame in self.scopes.iter().rev() {
            if let Some(s) = frame.get(name) {
                return Some(s);
            }
        }
        None
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn bind_pattern(&mut self, pat: &Pattern, mutable: bool) {
        match pat {
            Pattern::Var(name, span) => {
                let copy = self
                    .types
                    .binding_types
                    .get(&span.lo)
                    .map(|t| t.is_copy())
                    .unwrap_or(false);
                self.scopes.last_mut().unwrap().insert(
                    name.clone(),
                    Slot {
                        copy,
                        mutable,
                        moved: false,
                    },
                );
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
            Pattern::As(name, inner, span) => {
                let copy = self
                    .types
                    .binding_types
                    .get(&span.lo)
                    .map(|t| t.is_copy())
                    .unwrap_or(false);
                self.scopes.last_mut().unwrap().insert(
                    name.clone(),
                    Slot {
                        copy,
                        mutable,
                        moved: false,
                    },
                );
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

    fn mark_moved(&mut self, name: &str, span: Span) {
        let mut already_moved = false;
        for frame in self.scopes.iter_mut().rev() {
            if let Some(slot) = frame.get_mut(name) {
                if slot.moved {
                    already_moved = true;
                }
                slot.moved = true;
                break;
            }
        }
        if already_moved {
            self.err(
                4001,
                format!("use of moved value `{name}`"),
                Some(span),
            );
        }
    }

    /// Check a variable use. `move_it` is false in borrow/`clone` contexts.
    fn use_var(&mut self, name: &str, span: Span, move_it: bool) {
        let is_copy = self
            .lookup(name)
            .map(|s| s.copy)
            .unwrap_or(false);
        if move_it && !is_copy {
            self.mark_moved(name, span);
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
                    self.check_expr(&clause.body, false);
                    self.pop_scope();
                }
            }
            Item::Instance { methods, .. } => {
                for m in methods {
                    self.push_scope();
                    for p in &m.lhs {
                        self.bind_pattern(p, false);
                    }
                    self.check_expr(&m.expr, false);
                    self.pop_scope();
                }
            }
            _ => {}
        }
    }

    fn check_expr(&mut self, e: &Expr, borrowed: bool) {
        match e {
            Expr::Var(name, span) => self.use_var(name, *span, !borrowed),
            Expr::App(f, a, _) => {
                self.check_expr(f, false);
                self.check_expr(a, false);
            }
            Expr::Lambda(pats, body, _) => {
                self.push_scope();
                for p in pats {
                    self.bind_pattern(p, false);
                }
                self.check_expr(body, false);
                self.pop_scope();
            }
            Expr::Let(bindings, body, _) => {
                self.push_scope();
                for b in bindings {
                    self.check_expr(&b.expr, false);
                    if let Some(Pattern::Var(name, span)) = b.lhs.first() {
                        let copy = self
                            .types
                            .binding_types
                            .get(&span.lo)
                            .map(|t| t.is_copy())
                            .unwrap_or(false);
                        self.scopes.last_mut().unwrap().insert(
                            name.clone(),
                            Slot {
                                copy,
                                mutable: b.mutable,
                                moved: false,
                            },
                        );
                    } else if let Some(pat) = b.lhs.first() {
                        self.bind_pattern(pat, b.mutable);
                    }
                }
                self.check_expr(body, false);
                self.pop_scope();
            }
            Expr::If(c, t, el, _) => {
                self.check_expr(c, false);
                self.check_expr(t, false);
                self.check_expr(el, false);
            }
            Expr::Case(scrut, alts, _) => {
                self.check_expr(scrut, false);
                for alt in alts {
                    self.push_scope();
                    self.bind_pattern(&alt.pat, false);
                    self.check_expr(&alt.body, false);
                    self.pop_scope();
                }
            }
            Expr::Tuple(es, _) | Expr::List(es, _) => {
                for x in es {
                    self.check_expr(x, false);
                }
            }
            Expr::Assign(name, e, span) => {
                let is_mut = self.lookup(name).map(|s| s.mutable).unwrap_or(false);
                if !is_mut {
                    self.err(
                        4002,
                        format!("cannot assign to immutable variable `{name}`; declare it with `mut`"),
                        Some(*span),
                    );
                }
                self.check_expr(e, false);
            }
            Expr::Borrow { mutable, expr, span } => {
                if *mutable
                    && let Expr::Var(name, _) = &**expr {
                        let is_mut = self.lookup(name).map(|s| s.mutable).unwrap_or(false);
                        if !is_mut {
                            self.err(
                                4003,
                                format!("cannot mutably borrow immutable variable `{name}`"),
                                Some(*span),
                            );
                        }
                    }
                self.check_expr(expr, true);
            }
            Expr::Deref(e, _) => self.check_expr(e, false),
            Expr::Clone(e, _) => self.check_expr(e, true),
            Expr::Field(e, _, _) => self.check_expr(e, true),
            Expr::Ascribe(e, _, _) => self.check_expr(e, false),
            Expr::Record { fields, .. } => {
                for (_, f) in fields {
                    self.check_expr(f, false);
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
