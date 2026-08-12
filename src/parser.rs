//! Recursive-descent parser producing the AST.

use crate::ast::*;
use crate::error::{Diagnostic, Span};
use crate::lexer::{lex, TokKind, Token};

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

type PResult<T> = Result<T, Diagnostic>;

pub fn parse(src: &str) -> Result<Program, Vec<Diagnostic>> {
    let toks = lex(src)?;
    let mut p = Parser { toks, pos: 0 };
    p.parse_program().map_err(|d| vec![d])
}

impl Parser {
    fn peek(&self) -> &TokKind {
        &self.toks[self.pos.min(self.toks.len() - 1)].kind
    }
    fn peek2(&self) -> &TokKind {
        &self.toks[(self.pos + 1).min(self.toks.len() - 1)].kind
    }
    fn span(&self) -> Span {
        self.toks[self.pos.min(self.toks.len() - 1)].span
    }
    fn advance(&mut self) -> Token {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn at(&self, k: &TokKind) -> bool {
        self.peek() == k
    }
    fn eat(&mut self, k: &TokKind) -> bool {
        if self.at(k) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, k: &TokKind, what: &str) -> PResult<Token> {
        if self.at(k) {
            Ok(self.advance())
        } else {
            Err(self.err_here(format!("expected {what}, found {}", self.peek().describe())))
        }
    }
    fn err_here(&self, msg: impl Into<String>) -> Diagnostic {
        Diagnostic::error(2001, msg, Some(self.span()))
    }
    fn ident(&mut self) -> PResult<(String, Span)> {
        if let TokKind::Ident(s) = self.peek().clone() {
            let t = self.advance();
            Ok((s, t.span))
        } else {
            Err(self.err_here(format!("expected identifier, found {}", self.peek().describe())))
        }
    }
    fn conid(&mut self) -> PResult<(String, Span)> {
        if let TokKind::ConId(s) = self.peek().clone() {
            let t = self.advance();
            Ok((s, t.span))
        } else {
            Err(self.err_here(format!(
                "expected type/constructor name, found {}",
                self.peek().describe()
            )))
        }
    }

    // ===================== Program =====================

    fn parse_program(&mut self) -> PResult<Program> {
        let mut module = None;
        let mut module_exports = None;
        let mut items = Vec::new();
        while !self.at(&TokKind::Eof) {
            if self.eat(&TokKind::Semi) {
                continue;
            }
            if self.eat(&TokKind::RBrace) {
                continue;
            }
            if self.eat(&TokKind::LBrace) {
                continue;
            }
            match self.peek().clone() {
                TokKind::Module => {
                    self.advance();
                    let mut name = String::new();
                    loop {
                        let (part, _) = self.conid()?;
                        name.push_str(&part);
                        if self.eat(&TokKind::Dot) {
                            name.push('.');
                        } else {
                            break;
                        }
                    }
                    // Optional export list `(a, b, c)`.
                    let mut exports = None;
                    if self.eat(&TokKind::LParen) {
                        let mut list = Vec::new();
                        loop {
                            match self.peek().clone() {
                                TokKind::Ident(s) | TokKind::ConId(s) => {
                                    self.advance();
                                    list.push(s);
                                }
                                _ => break,
                            }
                            if !self.eat(&TokKind::Comma) {
                                break;
                            }
                        }
                        self.expect(&TokKind::RParen, "`)`")?;
                        exports = Some(list);
                    }
                    if self.eat(&TokKind::Where) {
                        // `module Foo where` — block handled by layout tokens.
                    }
                    module = Some(name);
                    module_exports = exports;
                }
                TokKind::Use => {
                    self.advance();
                    let mut path = String::new();
                    loop {
                        // Stop before the `as` alias keyword.
                        if matches!(self.peek(), TokKind::Ident(s) if s == "as") {
                            break;
                        }
                        match self.peek().clone() {
                            TokKind::Ident(s) | TokKind::ConId(s) => {
                                self.advance();
                                path.push_str(&s);
                            }
                            _ => break,
                        }
                        if self.eat(&TokKind::Dot) {
                            path.push('.');
                        } else {
                            break;
                        }
                    }
                    // Optional alias `as X`.
                    let mut alias = None;
                    if let TokKind::Ident(s) = self.peek().clone()
                        && s == "as" {
                            self.advance();
                            let (a, _) = self.conid()?;
                            alias = Some(a);
                        }
                    items.push(Item::Use {
                        path,
                        alias,
                        span: self.span(),
                    });
                }
                TokKind::Type => items.push(self.parse_type_alias()?),
                TokKind::Data => items.push(self.parse_data()?),
                TokKind::Class => items.push(self.parse_class()?),
                TokKind::Instance => items.push(self.parse_instance()?),
                TokKind::Ident(_) => {
                    // signature or function definition
                    if matches!(self.peek2(), TokKind::DColon) {
                        items.push(self.parse_signature()?);
                    } else {
                        items.push(Item::Function(self.parse_function()?));
                    }
                }
                other => {
                    return Err(Diagnostic::error(
                        2000,
                        format!("unexpected {} at top level", other.describe()),
                        Some(self.span()),
                    ));
                }
            }
        }
        Ok(Program {
            module,
            exports: module_exports,
            items,
        })
    }

    fn parse_type_alias(&mut self) -> PResult<Item> {
        self.advance(); // `type`
        let (name, _) = self.conid()?;
        let mut params = Vec::new();
        while let TokKind::Ident(_) = self.peek() {
            if let TokKind::Ident(s) = self.peek().clone() {
                params.push(s);
                self.advance();
            }
        }
        self.expect(&TokKind::Eq, "`=`")?;
        let ty = self.parse_type()?;
        Ok(Item::TypeAlias {
            name,
            params,
            ty,
            span: self.span(),
        })
    }

    fn parse_data(&mut self) -> PResult<Item> {
        self.advance(); // `data`
        let (name, _) = self.conid()?;
        let mut params = Vec::new();
        while let TokKind::Ident(_) = self.peek() {
            if let TokKind::Ident(s) = self.peek().clone() {
                params.push(s);
                self.advance();
            }
        }
        self.expect(&TokKind::Eq, "`=`")?;
        let mut ctors = Vec::new();
        loop {
            let (cname, cspan) = self.conid()?;
            let mut fields = Vec::new();
            let mut record = Vec::new();
            if self.eat(&TokKind::LBrace) {
                // record constructor
                loop {
                    if self.eat(&TokKind::RBrace) {
                        break;
                    }
                    let (fname, fspan) = self.ident()?;
                    self.expect(&TokKind::DColon, "`::`")?;
                    let fty = self.parse_type()?;
                    record.push(FieldDecl {
                        name: fname,
                        ty: fty,
                        span: fspan,
                    });
                    if !self.eat(&TokKind::Comma) {
                        self.expect(&TokKind::RBrace, "`}`")?;
                        break;
                    }
                }
            } else {
                // Constructor fields are atomic types (`atype`), matching
                // Haskell: `C Int Char` has two fields, `C (Map K V)` one.
                while self.starts_atype() {
                    fields.push(self.parse_atype()?);
                }
            }
            ctors.push(Ctor {
                name: cname,
                fields,
                record,
                span: cspan,
            });
            if !self.eat(&TokKind::Pipe) {
                break;
            }
        }
        Ok(Item::Data {
            name,
            params,
            ctors,
            span: self.span(),
        })
    }

    fn parse_class(&mut self) -> PResult<Item> {
        self.advance(); // `class`
        let mut superclasses = Vec::new();
        if self.eat(&TokKind::LParen) {
            loop {
                superclasses.push(self.parse_btype()?);
                if !self.eat(&TokKind::Comma) {
                    self.expect(&TokKind::RParen, "`)`")?;
                    break;
                }
            }
            self.expect(&TokKind::FatArrow, "`=>`")?;
        }
        let (name, _) = self.conid()?;
        let (param, _) = self.ident()?;
        self.expect(&TokKind::Where, "`where`")?;
        // Layout has inserted `{ ... }`.
        self.eat(&TokKind::LBrace);
        let mut methods = Vec::new();
        while !self.at(&TokKind::RBrace) && !self.at(&TokKind::Eof) {
            if self.eat(&TokKind::Semi) {
                continue;
            }
            methods.push(self.parse_method_sig()?);
        }
        self.eat(&TokKind::RBrace);
        Ok(Item::Class {
            name,
            param,
            superclasses,
            methods,
            span: self.span(),
        })
    }

    fn parse_method_sig(&mut self) -> PResult<TypeSig> {
        let (name, span) = self.ident()?;
        self.expect(&TokKind::DColon, "`::`")?;
        let (constraints, body) = self.parse_qualified_type()?;
        Ok(TypeSig {
            name,
            constraints,
            body,
            span,
        })
    }

    fn parse_instance(&mut self) -> PResult<Item> {
        self.advance(); // `instance`
        let mut ctx = Vec::new();
        if self.eat(&TokKind::LParen) {
            loop {
                ctx.push(self.parse_btype()?);
                if !self.eat(&TokKind::Comma) {
                    self.expect(&TokKind::RParen, "`)`")?;
                    break;
                }
            }
            self.expect(&TokKind::FatArrow, "`=>`")?;
        }
        let class = self.parse_btype()?;
        self.expect(&TokKind::Where, "`where`")?;
        self.eat(&TokKind::LBrace);
        let mut methods = Vec::new();
        while !self.at(&TokKind::RBrace) && !self.at(&TokKind::Eof) {
            if self.eat(&TokKind::Semi) {
                continue;
            }
            methods.push(self.parse_binding()?);
        }
        self.eat(&TokKind::RBrace);
        Ok(Item::Instance {
            class,
            context: ctx,
            methods,
            span: self.span(),
        })
    }

    fn parse_signature(&mut self) -> PResult<Item> {
        let (name, span) = self.ident()?;
        self.expect(&TokKind::DColon, "`::`")?;
        let (constraints, body) = self.parse_qualified_type()?;
        Ok(Item::Signature {
            sig: TypeSig {
                name,
                constraints,
                body,
                span,
            },
            span,
        })
    }

    fn parse_qualified_type(&mut self) -> PResult<(Vec<Type>, Type)> {
        // A leading `(` is either a constraint list `(Eq a) => ...` or a
        // parenthesized type `(a -> b) -> ...`. Parse it as a type list and
        // decide by whether `=>` follows; a parenthesized type may still be
        // the left side of a function arrow.
        if self.eat(&TokKind::LParen) {
            if self.eat(&TokKind::RParen) {
                let t = Type::Unit(self.span());
                return Ok((Vec::new(), self.parse_type_rest(t)?));
            }
            let mut types = vec![self.parse_type()?];
            while self.eat(&TokKind::Comma) {
                types.push(self.parse_type()?);
            }
            self.expect(&TokKind::RParen, "`)`")?;
            if self.eat(&TokKind::FatArrow) {
                let body = self.parse_type()?;
                return Ok((types, body));
            }
            let t = if types.len() == 1 {
                types.into_iter().next().unwrap()
            } else {
                Type::Tuple(types, self.span())
            };
            return Ok((Vec::new(), self.parse_type_rest(t)?));
        }
        Ok((Vec::new(), self.parse_type()?))
    }

    /// Continue a type that may be the left side of a function arrow.
    fn parse_type_rest(&mut self, lhs: Type) -> PResult<Type> {
        if self.eat(&TokKind::Arrow) {
            let rhs = self.parse_type()?;
            let span = lhs.span().merge(rhs.span());
            Ok(Type::Func(Box::new(lhs), Box::new(rhs), span))
        } else {
            Ok(lhs)
        }
    }

    // ===================== Functions =====================

    fn parse_function(&mut self) -> PResult<Function> {
        let (name, _) = self.ident()?;
        let mut clauses = Vec::new();
        loop {
            let clause = self.parse_clause()?;
            clauses.push(clause);
            // A subsequent clause of the same function follows if the next
            // token (after `;`) is the same identifier at the same level.
            if self.at(&TokKind::Semi) {
                let save = self.pos;
                self.advance();
                if let TokKind::Ident(next) = self.peek()
                    && *next == name {
                        self.advance(); // consume the repeated function name
                        continue;
                    }
                self.pos = save;
            }
            break;
        }
        Ok(Function { name, clauses })
    }

    fn parse_clause(&mut self) -> PResult<Clause> {
        let start = self.span();
        let mut pats = Vec::new();
        while self.starts_pattern() && !self.at(&TokKind::Eq) && !self.at(&TokKind::Pipe) {
            pats.push(self.parse_pattern()?);
        }
        // Guards: `| guard = expr`
        let mut guards = Vec::new();
        if self.eat(&TokKind::Pipe) {
            loop {
                let cond = if self.at(&TokKind::Eq) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.expect(&TokKind::Eq, "`=`")?;
                let body = self.parse_expr()?;
                guards.push((cond, body));
                if !self.eat(&TokKind::Pipe) {
                    break;
                }
            }
        } else {
            self.expect(&TokKind::Eq, "`=`")?;
            let body = self.parse_expr()?;
            guards.push((None, body));
        }
        // Where bindings.
        let mut where_bindings = Vec::new();
        if self.eat(&TokKind::Where) {
            self.eat(&TokKind::LBrace);
            while !self.at(&TokKind::RBrace) && !self.at(&TokKind::Eof) {
                if self.eat(&TokKind::Semi) {
                    continue;
                }
                where_bindings.push(self.parse_binding()?);
            }
            self.eat(&TokKind::RBrace);
        }
        // Desugar guards and where into a single body expression.
        let body = desugar_guards(guards, start);
        let body = if where_bindings.is_empty() {
            body
        } else {
            Expr::Let(where_bindings, Box::new(body), start)
        };
        Ok(Clause {
            pats,
            body,
            span: start.merge(self.span()),
        })
    }

    // ===================== Bindings =====================

    fn parse_binding(&mut self) -> PResult<Binding> {
        let start = self.span();
        let mut mutable = false;
        if self.eat(&TokKind::Mut) {
            mutable = true;
        }
        let mut lhs = Vec::new();
        while self.starts_pattern() && !self.at(&TokKind::Eq) {
            lhs.push(self.parse_pattern()?);
        }
        if lhs.is_empty() {
            return Err(self.err_here("expected a binding pattern"));
        }
        self.expect(&TokKind::Eq, "`=`")?;
        let expr = self.parse_expr()?;
        // Desugar `f p1 p2 = e` into `f = \p1 -> \p2 -> e`.
        if lhs.len() > 1 {
            if let Pattern::Var(_, _) = &lhs[0] {
                let rest = lhs.split_off(1);
                let lam = rest
                    .into_iter()
                    .rev()
                    .fold(expr, |acc, pat| Expr::Lambda(vec![pat], Box::new(acc), start));
                Ok(Binding {
                    mutable,
                    lhs,
                    expr: lam,
                    span: start.merge(self.span()),
                })
            } else {
                Err(Diagnostic::error(
                    2002,
                    "function binding must have a variable name on the left",
                    Some(start),
                ))
            }
        } else {
            Ok(Binding {
                mutable,
                lhs,
                expr,
                span: start.merge(self.span()),
            })
        }
    }

    // ===================== Types =====================

    fn parse_type(&mut self) -> PResult<Type> {
        let lhs = self.parse_btype()?;
        if self.eat(&TokKind::Arrow) {
            let rhs = self.parse_type()?;
            let span = lhs.span().merge(rhs.span());
            Ok(Type::Func(Box::new(lhs), Box::new(rhs), span))
        } else {
            Ok(lhs)
        }
    }

    fn parse_btype(&mut self) -> PResult<Type> {
        let first = self.parse_atype()?;
        if let Type::Con(name, args0, span) = first {
            let mut args = args0;
            while self.starts_atype() {
                args.push(self.parse_atype()?);
            }
            let span = args.last().map(|a| span.merge(a.span())).unwrap_or(span);
            Ok(Type::Con(name, args, span))
        } else {
            // Type variables cannot be applied; the remaining type atoms
            // belong to the next field.
            Ok(first)
        }
    }

    fn starts_atype(&self) -> bool {
        matches!(
            self.peek(),
            TokKind::ConId(_)
                | TokKind::Ident(_)
                | TokKind::LParen
                | TokKind::LBracket
                | TokKind::Amp
                | TokKind::Mut
        )
    }

    fn parse_atype(&mut self) -> PResult<Type> {
        let start = self.span();
        match self.peek().clone() {
            TokKind::ConId(name) => {
                self.advance();
                Ok(Type::Con(name, Vec::new(), start))
            }
            TokKind::Ident(name) => {
                self.advance();
                Ok(Type::Var(name, start))
            }
            TokKind::LParen => {
                self.advance();
                if self.eat(&TokKind::RParen) {
                    return Ok(Type::Unit(start));
                }
                let t = self.parse_type()?;
                if self.eat(&TokKind::Comma) {
                    let mut ts = vec![t];
                    loop {
                        ts.push(self.parse_type()?);
                        if !self.eat(&TokKind::Comma) {
                            break;
                        }
                    }
                    self.expect(&TokKind::RParen, "`)`")?;
                    Ok(Type::Tuple(ts, start))
                } else {
                    self.expect(&TokKind::RParen, "`)`")?;
                    Ok(t)
                }
            }
            TokKind::LBracket => {
                self.advance();
                if self.eat(&TokKind::RBracket) {
                    return Err(self.err_here("empty list type needs an element type: use `[a]`"));
                }
                let t = self.parse_type()?;
                self.expect(&TokKind::RBracket, "`]`")?;
                Ok(Type::List(Box::new(t), start))
            }
            TokKind::Amp => {
                self.advance();
                let mutable = self.eat(&TokKind::Mut);
                let inner = self.parse_atype()?;
                Ok(Type::Ref {
                    mutable,
                    inner: Box::new(inner),
                    span: start,
                })
            }
            other => Err(self.err_here(format!("expected a type, found {}", other.describe()))),
        }
    }

    // ===================== Patterns =====================

    fn starts_pattern(&self) -> bool {
        matches!(
            self.peek(),
            TokKind::Ident(_)
                | TokKind::ConId(_)
                | TokKind::Int(_)
                | TokKind::Float(_)
                | TokKind::Str(_)
                | TokKind::Char(_)
                | TokKind::Underscore
                | TokKind::LParen
                | TokKind::LBracket
        )
    }

    fn starts_apat(&self) -> bool {
        matches!(
            self.peek(),
            TokKind::Ident(_)
                | TokKind::ConId(_)
                | TokKind::Int(_)
                | TokKind::Float(_)
                | TokKind::Str(_)
                | TokKind::Char(_)
                | TokKind::Underscore
                | TokKind::LParen
                | TokKind::LBracket
        )
    }

    fn parse_pattern(&mut self) -> PResult<Pattern> {
        // As-pattern `x @ pat`
        if let TokKind::Ident(name) = self.peek().clone()
            && matches!(self.peek2(), TokKind::At) {
                let t = self.advance(); // name
                self.advance(); // @
                let inner = self.parse_pattern()?;
                let ispan = inner.span();
                return Ok(Pattern::As(name, Box::new(inner), t.span.merge(ispan)));
            }
        self.parse_pat_cons()
    }

    fn parse_pat_cons(&mut self) -> PResult<Pattern> {
        let left = self.parse_pat_app()?;
        if let TokKind::Op(op) = self.peek().clone()
            && op == ":" {
                self.advance();
                let right = self.parse_pat_cons()?;
                let span = left.span().merge(right.span());
                return Ok(Pattern::Cons(Box::new(left), Box::new(right), span));
            }
        Ok(left)
    }

    fn parse_pat_app(&mut self) -> PResult<Pattern> {
        if let TokKind::ConId(name) = self.peek().clone() {
            let t = self.advance();
            // Record pattern?
            if self.eat(&TokKind::LBrace) {
                let mut fields = Vec::new();
                loop {
                    if self.eat(&TokKind::RBrace) {
                        break;
                    }
                    let (fname, _) = self.ident()?;
                    self.expect(&TokKind::Eq, "`=`")?;
                    let pat = self.parse_pattern()?;
                    fields.push((fname, pat));
                    if !self.eat(&TokKind::Comma) {
                        self.expect(&TokKind::RBrace, "`}`")?;
                        break;
                    }
                }
                return Ok(Pattern::Record {
                    ctor: name,
                    fields,
                    span: t.span,
                });
            }
            let mut args = Vec::new();
            while self.starts_apat() {
                args.push(self.parse_apat()?);
            }
            return Ok(Pattern::Ctor {
                name,
                args,
                span: t.span,
            });
        }
        self.parse_apat()
    }

    fn parse_apat(&mut self) -> PResult<Pattern> {
        let start = self.span();
        match self.peek().clone() {
            TokKind::Ident(name) => {
                self.advance();
                Ok(Pattern::Var(name, start))
            }
            TokKind::Underscore => {
                self.advance();
                Ok(Pattern::Wildcard(start))
            }
            TokKind::Int(n) => {
                self.advance();
                Ok(Pattern::Int(n, start))
            }
            TokKind::Float(f) => {
                self.advance();
                Ok(Pattern::Float(f, start))
            }
            TokKind::Str(s) => {
                self.advance();
                Ok(Pattern::Str(s, start))
            }
            TokKind::Char(c) => {
                self.advance();
                Ok(Pattern::Char(c, start))
            }
            TokKind::ConId(name) => {
                self.advance();
                Ok(Pattern::Ctor {
                    name,
                    args: Vec::new(),
                    span: start,
                })
            }
            TokKind::LParen => {
                self.advance();
                if self.eat(&TokKind::RParen) {
                    return Ok(Pattern::Tuple(Vec::new(), start));
                }
                let p = self.parse_pattern()?;
                if self.eat(&TokKind::Comma) {
                    let mut ps = vec![p];
                    loop {
                        ps.push(self.parse_pattern()?);
                        if !self.eat(&TokKind::Comma) {
                            break;
                        }
                    }
                    self.expect(&TokKind::RParen, "`)`")?;
                    Ok(Pattern::Tuple(ps, start))
                } else {
                    self.expect(&TokKind::RParen, "`)`")?;
                    Ok(p)
                }
            }
            TokKind::LBracket => {
                self.advance();
                let mut ps = Vec::new();
                if !self.at(&TokKind::RBracket) {
                    loop {
                        ps.push(self.parse_pattern()?);
                        if !self.eat(&TokKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&TokKind::RBracket, "`]`")?;
                Ok(Pattern::List(ps, start))
            }
            other => Err(self.err_here(format!("expected a pattern, found {}", other.describe()))),
        }
    }

    // ===================== Expressions =====================

    pub fn parse_expr(&mut self) -> PResult<Expr> {
        let e = self.parse_infix(0)?;
        if self.eat(&TokKind::DColon) {
            let ty = self.parse_type()?;
            let span = e.span().merge(ty.span());
            Ok(Expr::Ascribe(Box::new(e), ty, span))
        } else {
            Ok(e)
        }
    }

    fn parse_infix(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let Some(op) = self.peek_op() else { break };
            let Some((prec, right_assoc)) = op_info(&op) else { break };
            if prec < min_prec {
                break;
            }
            let optok = self.advance();
            let next_min = if right_assoc { prec } else { prec + 1 };
            let right = self.parse_infix(next_min)?;
            let span = left.span().merge(right.span());
            let op_span = optok.span;
            if op == ":=" {
                // Mutable assignment.
                let name = match &left {
                    Expr::Var(n, _) => n.clone(),
                    _ => {
                        return Err(Diagnostic::error(
                            2003,
                            "assignment target must be a variable",
                            Some(left.span()),
                        ));
                    }
                };
                left = Expr::Assign(name, Box::new(right), span);
                continue;
            }
            let op_var = Expr::Var(op, op_span);
            left = Expr::App(
                Box::new(Expr::App(Box::new(op_var), Box::new(left), span)),
                Box::new(right),
                span,
            );
        }
        Ok(left)
    }

    fn peek_op(&self) -> Option<String> {
        if let TokKind::Op(s) = self.peek().clone() {
            Some(s)
        } else {
            None
        }
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        let start = self.span();
        match self.peek().clone() {
            TokKind::Op(op) if op == "-" => {
                self.advance();
                let e = self.parse_unary()?;
                let span = start.merge(e.span());
                Ok(Expr::App(
                    Box::new(Expr::Var("negate".into(), start)),
                    Box::new(e),
                    span,
                ))
            }
            TokKind::Op(op) if op == "!" => {
                self.advance();
                let e = self.parse_unary()?;
                let span = start.merge(e.span());
                Ok(Expr::App(
                    Box::new(Expr::Var("not".into(), start)),
                    Box::new(e),
                    span,
                ))
            }
            TokKind::Op(op) if op == "*" => {
                self.advance();
                let e = self.parse_unary()?;
                let span = start.merge(e.span());
                Ok(Expr::Deref(Box::new(e), span))
            }
            TokKind::Amp => {
                self.advance();
                let mutable = self.eat(&TokKind::Mut);
                let e = self.parse_unary()?;
                let span = start.merge(e.span());
                Ok(Expr::Borrow {
                    mutable,
                    expr: Box::new(e),
                    span,
                })
            }
            TokKind::Clone => {
                self.advance();
                let e = self.parse_unary()?;
                let span = start.merge(e.span());
                Ok(Expr::Clone(Box::new(e), span))
            }
            _ => self.parse_app(),
        }
    }

    fn parse_app(&mut self) -> PResult<Expr> {
        let mut f = self.parse_atom()?;
        while self.starts_atom_arg() {
            let arg = self.parse_atom()?;
            let span = f.span().merge(arg.span());
            f = Expr::App(Box::new(f), Box::new(arg), span);
        }
        Ok(f)
    }

    fn starts_atom_arg(&self) -> bool {
        matches!(
            self.peek(),
            TokKind::Ident(_)
                | TokKind::ConId(_)
                | TokKind::Int(_)
                | TokKind::Float(_)
                | TokKind::Str(_)
                | TokKind::Char(_)
                | TokKind::LParen
                | TokKind::LBracket
                | TokKind::Backslash
                | TokKind::If
                | TokKind::Case
                | TokKind::Let
                | TokKind::Clone
                | TokKind::Amp
        )
    }

    fn parse_atom(&mut self) -> PResult<Expr> {
        let mut e = self.parse_atom_base()?;
        // Postfix record field access `expr.field`.
        while self.at(&TokKind::Dot) {
            self.advance();
            let (name, fspan) = self.ident()?;
            let span = e.span().merge(fspan);
            e = Expr::Field(Box::new(e), name, span);
        }
        Ok(e)
    }

    fn parse_atom_base(&mut self) -> PResult<Expr> {
        let start = self.span();
        match self.peek().clone() {
            TokKind::Int(n) => {
                self.advance();
                Ok(Expr::Int(n, start))
            }
            TokKind::Float(f) => {
                self.advance();
                Ok(Expr::Float(f, start))
            }
            TokKind::Str(s) => {
                self.advance();
                Ok(Expr::Str(s, start))
            }
            TokKind::Char(c) => {
                self.advance();
                Ok(Expr::Char(c, start))
            }
            TokKind::Ident(name) => {
                self.advance();
                Ok(Expr::Var(name, start))
            }
            TokKind::ConId(name) => {
                self.advance();
                // Record construction `Name { field = expr, ... }`.
                if self.eat(&TokKind::LBrace) {
                    let mut fields = Vec::new();
                    loop {
                        if self.eat(&TokKind::RBrace) {
                            break;
                        }
                        let (fname, _) = self.ident()?;
                        self.expect(&TokKind::Eq, "`=`")?;
                        let fexpr = self.parse_expr()?;
                        fields.push((fname, fexpr));
                        if !self.eat(&TokKind::Comma) {
                            self.expect(&TokKind::RBrace, "`}`")?;
                            break;
                        }
                    }
                    let span = start.merge(self.span());
                    return Ok(Expr::Record {
                        ctor: name,
                        fields,
                        span,
                    });
                }
                // Qualified module access `Alias.member`.
                if self.eat(&TokKind::Dot) {
                    match self.peek().clone() {
                        TokKind::Ident(m) | TokKind::ConId(m) => {
                            self.advance();
                            let span = start.merge(self.span());
                            return Ok(Expr::Qualified(name, m, span));
                        }
                        other => {
                            return Err(self.err_here(format!(
                                "expected a member name after `{name}.`, found {}",
                                other.describe()
                            )));
                        }
                    }
                }
                Ok(Expr::Ctor(name, start))
            }
            TokKind::Backslash => {
                self.advance();
                let mut pats = Vec::new();
                while self.starts_apat() {
                    pats.push(self.parse_apat()?);
                }
                self.expect(&TokKind::Arrow, "`->`")?;
                let body = self.parse_expr()?;
                let span = start.merge(body.span());
                Ok(Expr::Lambda(pats, Box::new(body), span))
            }
            TokKind::Let => {
                self.advance();
                self.eat(&TokKind::LBrace);
                let mut bindings = Vec::new();
                while !self.at(&TokKind::RBrace) && !self.at(&TokKind::Eof) {
                    if self.eat(&TokKind::Semi) {
                        continue;
                    }
                    bindings.push(self.parse_binding()?);
                }
                self.eat(&TokKind::RBrace);
                self.expect(&TokKind::In, "`in`")?;
                let body = self.parse_expr()?;
                let span = start.merge(body.span());
                Ok(Expr::Let(bindings, Box::new(body), span))
            }
            TokKind::If => {
                self.advance();
                let cond = self.parse_expr()?;
                self.expect(&TokKind::Then, "`then`")?;
                let then_e = self.parse_expr()?;
                self.expect(&TokKind::Else, "`else`")?;
                let else_e = self.parse_expr()?;
                let span = start.merge(else_e.span());
                Ok(Expr::If(Box::new(cond), Box::new(then_e), Box::new(else_e), span))
            }
            TokKind::Case => {
                self.advance();
                let scrutinee = self.parse_expr()?;
                self.expect(&TokKind::Of, "`of`")?;
                self.eat(&TokKind::LBrace);
                let mut alts = Vec::new();
                while !self.at(&TokKind::RBrace) && !self.at(&TokKind::Eof) {
                    if self.eat(&TokKind::Semi) {
                        continue;
                    }
                    let pat = self.parse_pattern()?;
                    self.expect(&TokKind::Arrow, "`->`")?;
                    let body = self.parse_expr()?;
                    let span = pat.span().merge(body.span());
                    alts.push(Alt { pat, body, span });
                }
                self.eat(&TokKind::RBrace);
                let span = start.merge(self.span());
                Ok(Expr::Case(Box::new(scrutinee), alts, span))
            }
            TokKind::LParen => {
                self.advance();
                if self.eat(&TokKind::RParen) {
                    return Ok(Expr::Unit(start));
                }
                let e = self.parse_expr()?;
                if self.eat(&TokKind::Comma) {
                    let mut es = vec![e];
                    loop {
                        es.push(self.parse_expr()?);
                        if !self.eat(&TokKind::Comma) {
                            break;
                        }
                    }
                    self.expect(&TokKind::RParen, "`)`")?;
                    let span = start.merge(self.span());
                    Ok(Expr::Tuple(es, span))
                } else {
                    self.expect(&TokKind::RParen, "`)`")?;
                    Ok(e)
                }
            }
            TokKind::LBracket => {
                self.advance();
                let mut es = Vec::new();
                if !self.at(&TokKind::RBracket) {
                    loop {
                        es.push(self.parse_expr()?);
                        if !self.eat(&TokKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&TokKind::RBracket, "`]`")?;
                let span = start.merge(self.span());
                Ok(Expr::List(es, span))
            }
            other => Err(self.err_here(format!("expected an expression, found {}", other.describe()))),
        }
    }
}

fn desugar_guards(guards: Vec<(Option<Expr>, Expr)>, span: Span) -> Expr {
    let mut body = Expr::App(
        Box::new(Expr::Var("error".into(), span)),
        Box::new(Expr::Str("non-exhaustive guards".into(), span)),
        span,
    );
    for (cond, e) in guards.iter().rev() {
        body = match cond {
            None => e.clone(),
            Some(c) => {
                let s = c.span().merge(e.span()).merge(body.span());
                Expr::If(Box::new(c.clone()), Box::new(e.clone()), Box::new(body), s)
            }
        };
    }
    body
}

/// Precedence table. `(precedence, right_associative)`.
fn op_info(op: &str) -> Option<(u8, bool)> {
    let p = match op {
        "$" => (0, true),
        "||" => (2, true),
        "&&" => (3, true),
        "==" | "/=" | "<" | "<=" | ">" | ">=" => (4, false),
        ":" | "++" => (5, true),
        "+" | "-" => (6, false),
        "*" | "/" | "%" => (7, false),
        "^" => (8, true),
        ":=" => (1, false),
        _ => return None,
    };
    Some(p)
}
