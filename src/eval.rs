//! Tree-walking interpreter with Rust-like ownership semantics (move by
//! default, explicit `clone`, borrows, mutability) enforced at runtime.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::*;
use crate::error::{Diagnostic, Span};

type Cell = Rc<RefCell<VarCell>>;
type Scope = HashMap<String, Cell>;
type Env = Rc<RefCell<Vec<Scope>>>;

#[derive(Clone)]
struct VarCell {
    value: Option<Value>,
    mutable: bool,
}

#[derive(Clone)]
struct FuncClause {
    pats: Vec<Pattern>,
    body: Expr,
    span: Span,
    /// Bindings captured by earlier partial applications.
    captured: Vec<(String, Value)>,
}

#[derive(Clone)]
struct FuncVal {
    name: String,
    clauses: Vec<FuncClause>,
    env: Vec<Scope>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Builtin {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Negate,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Not,
    Cons,
    Append,
    Show,
    Println,
    Print,
    Error,
    Sqrt,
    Chr,
    Snoc,
}

impl Builtin {
    fn arity(&self) -> usize {
        match self {
            Builtin::Add
            | Builtin::Sub
            | Builtin::Mul
            | Builtin::Div
            | Builtin::Mod
            | Builtin::Eq
            | Builtin::Neq
            | Builtin::Lt
            | Builtin::Le
            | Builtin::Gt
            | Builtin::Ge
            | Builtin::And
            | Builtin::Or
            | Builtin::Cons
            | Builtin::Append
            | Builtin::Snoc => 2,
            _ => 1,
        }
    }
}

#[derive(Clone)]
enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    Str(String),
    Unit,
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Adt {
        adt_id: usize,
        name: String,
        ctor: String,
        fields: Vec<Value>,
        record: Vec<(String, Value)>,
    },
    Func(FuncVal),
    CtorFn {
        adt_id: usize,
        ctor_idx: usize,
        name: String,
        arity: usize,
        args: Vec<Value>,
    },
    Ref {
        cell: Cell,
        mutable: bool,
    },
    Builtin(Builtin),
    /// A partially-applied builtin, accumulating arguments.
    Partial {
        builtin: Builtin,
        args: Vec<Value>,
    },
    Method {
        class: String,
        name: String,
    },
}

impl Value {
    fn deep_clone(&self) -> Value {
        match self {
            Value::Int(i) => Value::Int(*i),
            Value::Float(f) => Value::Float(*f),
            Value::Bool(b) => Value::Bool(*b),
            Value::Char(c) => Value::Char(*c),
            Value::Str(s) => Value::Str(s.clone()),
            Value::Unit => Value::Unit,
            Value::List(v) => Value::List(v.iter().map(|x| x.deep_clone()).collect()),
            Value::Tuple(v) => Value::Tuple(v.iter().map(|x| x.deep_clone()).collect()),
            Value::Adt { adt_id, name, ctor, fields, record } => Value::Adt {
                adt_id: *adt_id,
                name: name.clone(),
                ctor: ctor.clone(),
                fields: fields.iter().map(|x| x.deep_clone()).collect(),
                record: record
                    .iter()
                    .map(|(n, v)| (n.clone(), v.deep_clone()))
                    .collect(),
            },
            Value::Func(f) => Value::Func(FuncVal {
                name: f.name.clone(),
                clauses: f.clauses.clone(),
                env: f.env.clone(),
            }),
            Value::CtorFn { adt_id, ctor_idx, name, arity, args } => Value::CtorFn {
                adt_id: *adt_id,
                ctor_idx: *ctor_idx,
                name: name.clone(),
                arity: *arity,
                args: args.iter().map(|x| x.deep_clone()).collect(),
            },
            Value::Ref { cell, mutable } => Value::Ref {
                cell: cell.clone(),
                mutable: *mutable,
            },
            Value::Builtin(b) => Value::Builtin(*b),
            Value::Partial { builtin, args } => Value::Partial {
                builtin: *builtin,
                args: args.iter().map(|x| x.deep_clone()).collect(),
            },
            Value::Method { class, name } => Value::Method {
                class: class.clone(),
                name: name.clone(),
            },
        }
    }
}

#[derive(Clone)]
struct CtorRuntime {
    name: String,
    arity: usize,
    record_fields: Vec<String>,
}

#[derive(Clone)]
struct AdtRuntime {
    name: String,
    ctors: Vec<CtorRuntime>,
}

struct ProgramInfo {
    adts: Vec<AdtRuntime>,
    ctor_by_name: HashMap<String, (usize, usize)>,
    class_methods: HashMap<String, String>,
    instances: HashMap<(String, String), HashMap<String, Vec<Binding>>>,
}

fn build_info(program: &Program) -> ProgramInfo {
    let mut info = ProgramInfo {
        adts: Vec::new(),
        ctor_by_name: HashMap::new(),
        class_methods: HashMap::new(),
        instances: HashMap::new(),
    };
    for item in &program.items {
        match item {
            Item::Data { name, ctors, .. } => {
                let id = info.adts.len();
                let mut ct = Vec::new();
                for (i, c) in ctors.iter().enumerate() {
                    let record_fields: Vec<String> =
                        c.record.iter().map(|f| f.name.clone()).collect();
                    let arity = if c.record.is_empty() {
                        c.fields.len()
                    } else {
                        record_fields.len()
                    };
                    ct.push(CtorRuntime {
                        name: c.name.clone(),
                        arity,
                        record_fields,
                    });
                    info.ctor_by_name.insert(c.name.clone(), (id, i));
                }
                info.adts.push(AdtRuntime {
                    name: name.clone(),
                    ctors: ct,
                });
            }
            Item::Class { name, methods, .. } => {
                for m in methods {
                    info.class_methods.insert(m.name.clone(), name.clone());
                }
            }
            Item::Instance { class, methods, .. } => {
                let class_name = match class {
                    Type::Con(n, _, _) => n.clone(),
                    _ => continue,
                };
                let head = match class {
                    Type::Con(_, args, _) => args
                        .first()
                        .map(|a| match a {
                            Type::Con(n, _, _) => n.clone(),
                            Type::List(..) => "List".to_string(),
                            _ => String::new(),
                        })
                        .unwrap_or_default(),
                    _ => continue,
                };
                let mut m: HashMap<String, Vec<Binding>> = HashMap::new();
                for b in methods {
                    if let Some(n) = b.name() {
                        m.entry(n.to_string()).or_default().push(b.clone());
                    }
                }
                info.instances.insert((class_name, head), m);
            }
            _ => {}
        }
    }
    info
}

fn rt_err(msg: impl Into<String>, span: Span) -> Diagnostic {
    Diagnostic::error(5000, msg, Some(span))
}

pub fn evaluate_program(program: &Program) -> Result<String, Vec<Diagnostic>> {
    let info = build_info(program);
    let output = Rc::new(RefCell::new(String::new()));

    let mut global: Scope = HashMap::new();
    for item in &program.items {
        if let Item::Function(f) = item {
            global.insert(
                f.name.clone(),
                Rc::new(RefCell::new(VarCell {
                    value: None,
                    mutable: false,
                })),
            );
        }
    }
    install_prelude(&mut global, &info);

    let eval = Eval {
        info: &info,
        output: output.clone(),
        global: global.clone(),
    };
    let global_snapshot = vec![global.clone()];
    for item in &program.items {
        if let Item::Function(f) = item {
            let fv = eval.func_from_function(f, global_snapshot.clone());
            if let Some(cell) = global.get(&f.name) {
                cell.borrow_mut().value = Some(Value::Func(fv));
            }
        }
    }

    let env: Env = Rc::new(RefCell::new(global_snapshot.clone()));
    if global.contains_key("main") {
        let span = program
            .items
            .iter()
            .find_map(|i| match i {
                Item::Function(f) if f.name == "main" => Some(f.clauses[0].span),
                _ => None,
            })
            .unwrap_or_default();
        match eval.read_var(&env, "main", span) {
            Ok(v) => {
                if !matches!(v, Value::Unit) {
                    output.borrow_mut().push_str(&format_value(&v));
                    output.borrow_mut().push('\n');
                }
            }
            Err(d) => return Err(vec![d]),
        }
    } else {
        for item in &program.items {
            if let Item::Function(f) = item
                && f.clauses.iter().all(|c| c.pats.is_empty()) {
                    let span = f.clauses.first().map(|c| c.span).unwrap_or_default();
                    if let Err(d) = eval.read_var(&env, &f.name, span) {
                        return Err(vec![d]);
                    }
                }
        }
    }

    Ok(output.borrow().clone())
}

struct Eval<'a> {
    info: &'a ProgramInfo,
    output: Rc<RefCell<String>>,
    global: Scope,
}

impl<'a> Eval<'a> {
    fn func_from_function(&self, f: &Function, env: Vec<Scope>) -> FuncVal {
        FuncVal {
            name: f.name.clone(),
            clauses: f
                .clauses
                .iter()
                .map(|c| FuncClause {
                    pats: c.pats.clone(),
                    body: c.body.clone(),
                    span: c.span,
                    captured: Vec::new(),
                })
                .collect(),
            env,
        }
    }

    fn lookup(&self, env: &Env, name: &str) -> Option<Cell> {
        for frame in env.borrow().iter().rev() {
            if let Some(c) = frame.get(name) {
                return Some(c.clone());
            }
        }
        None
    }

    fn read_var(&self, env: &Env, name: &str, span: Span) -> Result<Value, Diagnostic> {
        let cell = self
            .lookup(env, name)
            .ok_or_else(|| rt_err(format!("unbound name `{name}`"), span))?;
        self.force(&cell, span)?;
        let c = cell.borrow();
        match &c.value {
            None => Err(rt_err(format!("use of uninitialized value `{name}`"), span)),
            // Values are shareable (immutability makes this safe); the cell
            // is not consumed.
            Some(v) => Ok(v.deep_clone()),
        }
    }

    fn peek_var(&self, env: &Env, name: &str, span: Span) -> Result<Value, Diagnostic> {
        self.read_var(env, name, span)
    }

    /// Lazily evaluate a nullary function cell (a "thunk") in place.
    fn force(&self, cell: &Cell, _span: Span) -> Result<(), Diagnostic> {
        loop {
            let is_thunk = {
                let c = cell.borrow();
                matches!(&c.value, Some(Value::Func(fv)) if fv.clauses.iter().all(|cl| cl.pats.is_empty()))
            };
            if !is_thunk {
                return Ok(());
            }
            let fv = match cell.borrow().value.clone() {
                Some(Value::Func(fv)) => fv,
                _ => return Ok(()),
            };
            let env: Env = Rc::new(RefCell::new(fv.env.clone()));
            let body = fv.clauses[0].body.clone();
            let result = self.eval(&env, &body)?;
            cell.borrow_mut().value = Some(result);
        }
    }

    fn eval(&self, env: &Env, e: &Expr) -> Result<Value, Diagnostic> {
        match e {
            Expr::Int(n, _) => Ok(Value::Int(*n)),
            Expr::Float(f, _) => Ok(Value::Float(*f)),
            Expr::Str(s, _) => Ok(Value::Str(s.clone())),
            Expr::Char(c, _) => Ok(Value::Char(*c)),
            Expr::Unit(_) => Ok(Value::Unit),
            Expr::Var(name, span) => self.read_var(env, name, *span),
            Expr::Ctor(name, _) => self.ctor_value(name),
            Expr::Lambda(pats, body, _) => {
                let snapshot = env.borrow().clone();
                Ok(Value::Func(FuncVal {
                    name: "<lambda>".to_string(),
                    clauses: vec![FuncClause {
                        pats: pats.clone(),
                        body: (**body).clone(),
                        span: body.span(),
                        captured: Vec::new(),
                    }],
                    env: snapshot,
                }))
            }
            Expr::App(f, a, span) => {
                let fv = self.eval(env, f)?;
                let av = self.eval(env, a)?;
                self.apply(fv, av, *span)
            }
            Expr::Let(bindings, body, _) => {
                env.borrow_mut().push(Scope::new());
                for b in bindings {
                    let v = self.eval(env, &b.expr)?;
                    let name = b.name().unwrap_or("<binding>").to_string();
                    env.borrow_mut().last_mut().unwrap().insert(
                        name.clone(),
                        Rc::new(RefCell::new(VarCell {
                            value: Some(v),
                            mutable: b.mutable,
                        })),
                    );
                }
                let result = self.eval(env, body);
                env.borrow_mut().pop();
                result
            }
            Expr::If(c, t, el, _) => {
                let cv = self.eval(env, c)?;
                match cv {
                    Value::Bool(true) => self.eval(env, t),
                    Value::Bool(false) => self.eval(env, el),
                    _ => Err(rt_err("`if` condition must be a Bool", c.span())),
                }
            }
            Expr::Case(scrut, alts, span) => {
                let sv = self.eval(env, scrut)?;
                for alt in alts {
                    let mut bindings = Vec::new();
                    if match_pattern(&alt.pat, sv.deep_clone(), &mut bindings) {
                        env.borrow_mut().push(Scope::new());
                        for (name, v) in bindings {
                            env.borrow_mut().last_mut().unwrap().insert(
                                name,
                                Rc::new(RefCell::new(VarCell {
                                    value: Some(v),
                                    mutable: false,
                                })),
                            );
                        }
                        let result = self.eval(env, &alt.body);
                        env.borrow_mut().pop();
                        return result;
                    }
                }
                Err(rt_err("non-exhaustive patterns in `case`", *span))
            }
            Expr::Tuple(es, _) => {
                let mut vs = Vec::new();
                for x in es {
                    vs.push(self.eval(env, x)?);
                }
                Ok(Value::Tuple(vs))
            }
            Expr::List(es, _) => {
                let mut vs = Vec::new();
                for x in es {
                    vs.push(self.eval(env, x)?);
                }
                Ok(Value::List(vs))
            }
            Expr::Assign(name, e, span) => {
                let v = self.eval(env, e)?;
                let cell = self
                    .lookup(env, name)
                    .ok_or_else(|| rt_err(format!("unbound name `{name}`"), *span))?;
                let mut c = cell.borrow_mut();
                if !c.mutable {
                    return Err(rt_err(
                        format!("cannot assign to immutable variable `{name}`; declare it with `mut`"),
                        *span,
                    ));
                }
                c.value = Some(v);
                Ok(Value::Unit)
            }
            Expr::Borrow { mutable, expr, span } => {
                if let Expr::Var(name, _) = &**expr {
                    let cell = self
                        .lookup(env, name)
                        .ok_or_else(|| rt_err(format!("unbound name `{name}`"), *span))?;
                    if *mutable {
                        let c = cell.borrow();
                        if !c.mutable {
                            return Err(rt_err(
                                format!("cannot mutably borrow immutable variable `{name}`"),
                                *span,
                            ));
                        }
                    }
                    return Ok(Value::Ref {
                        cell,
                        mutable: *mutable,
                    });
                }
                Err(rt_err("borrow target must be a variable", *span))
            }
            Expr::Deref(e, span) => {
                let v = self.eval(env, e)?;
                match v {
                    Value::Ref { cell, .. } => {
                        let c = cell.borrow();
                        match &c.value {
                            None => Err(rt_err("use of moved value through reference", *span)),
                            Some(v) => Ok(v.clone()),
                        }
                    }
                    _ => Err(rt_err("cannot dereference a non-reference", *span)),
                }
            }
            Expr::Clone(e, span) => {
                if let Expr::Var(name, _) = &**e {
                    Ok(self.peek_var(env, name, *span)?)
                } else {
                    let v = self.eval(env, e)?;
                    Ok(v.deep_clone())
                }
            }
            Expr::Field(e, name, span) => {
                // Field access borrows the receiver rather than moving it.
                let v = if let Expr::Var(vname, _) = &**e {
                    self.peek_var(env, vname, *span)?
                } else {
                    self.eval(env, e)?
                };
                match v {
                    Value::Adt { record, .. } => record
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, v)| v.deep_clone())
                        .ok_or_else(|| rt_err(format!("no field `{name}`"), *span)),
                    _ => Err(rt_err("field access requires a record", *span)),
                }
            }
            Expr::Record { ctor, fields, span } => {
                let (adt_id, ctor_idx) = *self
                    .info
                    .ctor_by_name
                    .get(ctor)
                    .ok_or_else(|| rt_err(format!("unknown constructor `{ctor}`"), *span))?;
                let adt = &self.info.adts[adt_id];
                let cr = &adt.ctors[ctor_idx];
                let mut record = Vec::new();
                for (fname, fexpr) in fields {
                    if !cr.record_fields.contains(fname) {
                        return Err(rt_err(
                            format!("constructor `{ctor}` has no field `{fname}`"),
                            *span,
                        ));
                    }
                    record.push((fname.clone(), self.eval(env, fexpr)?));
                }
                if record.len() != cr.record_fields.len() {
                    return Err(rt_err(
                        format!("constructor `{ctor}` requires {} field(s)", cr.record_fields.len()),
                        *span,
                    ));
                }
                let mut positional = Vec::new();
                for fname in &cr.record_fields {
                    positional.push(record.iter().find(|(n, _)| n == fname).unwrap().1.clone());
                }
                Ok(Value::Adt {
                    adt_id,
                    name: adt.name.clone(),
                    ctor: ctor.clone(),
                    fields: positional,
                    record,
                })
            }
            Expr::Ascribe(e, _, _) => self.eval(env, e),
            Expr::Qualified(q, m, span) => Err(rt_err(
                format!("unresolved qualified name `{q}.{m}`"),
                *span,
            )),
        }
    }

    fn ctor_value(&self, name: &str) -> Result<Value, Diagnostic> {
        match name {
            "True" => return Ok(Value::Bool(true)),
            "False" => return Ok(Value::Bool(false)),
            _ => {}
        }
        let (adt_id, ctor_idx) = *self
            .info
            .ctor_by_name
            .get(name)
            .ok_or_else(|| rt_err(format!("unknown constructor `{name}`"), Span::default()))?;
        let adt = &self.info.adts[adt_id];
        let cr = &adt.ctors[ctor_idx];
        if cr.arity == 0 {
            Ok(Value::Adt {
                adt_id,
                name: adt.name.clone(),
                ctor: cr.name.clone(),
                fields: vec![],
                record: vec![],
            })
        } else {
            Ok(Value::CtorFn {
                adt_id,
                ctor_idx,
                name: adt.name.clone(),
                arity: cr.arity,
                args: vec![],
            })
        }
    }

    fn apply(&self, f: Value, arg: Value, span: Span) -> Result<Value, Diagnostic> {
        match f {
            Value::Func(fv) => self.apply_func(fv, arg, span),
            Value::CtorFn { adt_id, ctor_idx, name, arity, mut args } => {
                args.push(arg);
                if args.len() == arity {
                    let adt = &self.info.adts[adt_id];
                    let cr = &adt.ctors[ctor_idx];
                    let mut record = Vec::new();
                    for (i, fname) in cr.record_fields.iter().enumerate() {
                        record.push((fname.clone(), args[i].clone()));
                    }
                    Ok(Value::Adt {
                        adt_id,
                        name: name.clone(),
                        ctor: cr.name.clone(),
                        fields: args,
                        record,
                    })
                } else {
                    Ok(Value::CtorFn {
                        adt_id,
                        ctor_idx,
                        name,
                        arity,
                        args,
                    })
                }
            }
            Value::Builtin(b) => {
                if b.arity() == 1 {
                    self.run_builtin(b, vec![arg], span)
                } else {
                    Ok(Value::Partial {
                        builtin: b,
                        args: vec![arg],
                    })
                }
            }
            Value::Partial { builtin, mut args } => {
                args.push(arg);
                if args.len() == builtin.arity() {
                    self.run_builtin(builtin, args, span)
                } else {
                    Ok(Value::Partial { builtin, args })
                }
            }
            Value::Method { class, name } => self.dispatch_method(&class, &name, arg, span),
            _ => Err(rt_err("attempted to apply a non-function", span)),
        }
    }

    fn run_builtin(&self, b: Builtin, args: Vec<Value>, span: Span) -> Result<Value, Diagnostic> {
        match b {
            Builtin::Add | Builtin::Sub | Builtin::Mul | Builtin::Div | Builtin::Mod => {
                let lhs = args[0].clone();
                let rhs = args[1].clone();
                numeric_binop(b, lhs, rhs)
            }
            Builtin::Negate => match &args[0] {
                Value::Int(x) => Ok(Value::Int(-x)),
                Value::Float(x) => Ok(Value::Float(-x)),
                _ => Err(rt_err("`negate` expects a number", span)),
            },
            Builtin::Eq => Ok(Value::Bool(values_equal(&args[0], &args[1]))),
            Builtin::Neq => Ok(Value::Bool(!values_equal(&args[0], &args[1]))),
            Builtin::Lt | Builtin::Le | Builtin::Gt | Builtin::Ge => {
                ordering(b, &args[0], &args[1], span)
            }
            Builtin::And => match (&args[0], &args[1]) {
                (Value::Bool(x), Value::Bool(y)) => Ok(Value::Bool(*x && *y)),
                _ => Err(rt_err("`&&` expects Bools", span)),
            },
            Builtin::Or => match (&args[0], &args[1]) {
                (Value::Bool(x), Value::Bool(y)) => Ok(Value::Bool(*x || *y)),
                _ => Err(rt_err("`||` expects Bools", span)),
            },
            Builtin::Not => match &args[0] {
                Value::Bool(x) => Ok(Value::Bool(!x)),
                _ => Err(rt_err("`not` expects a Bool", span)),
            },
            Builtin::Cons => match &args[1] {
                Value::List(xs) => {
                    let mut v = xs.clone();
                    v.insert(0, args[0].clone());
                    Ok(Value::List(v))
                }
                _ => Err(rt_err("`:` expects a list on the right", span)),
            },
            Builtin::Append => match (&args[0], &args[1]) {
                (Value::Str(x), Value::Str(y)) => Ok(Value::Str(format!("{x}{y}"))),
                (Value::List(xs), Value::List(ys)) => {
                    let mut v = xs.clone();
                    v.extend(ys.iter().cloned());
                    Ok(Value::List(v))
                }
                _ => Err(rt_err("`++` expects two lists or two strings", span)),
            },
            Builtin::Show => Ok(Value::Str(format_value(&args[0]))),
            Builtin::Println => {
                self.output.borrow_mut().push_str(&format_value(&args[0]));
                self.output.borrow_mut().push('\n');
                Ok(Value::Unit)
            }
            // `print` is `println` without the trailing newline; the native
            // backend streams it to stdout immediately.
            Builtin::Print => {
                self.output.borrow_mut().push_str(&format_value(&args[0]));
                Ok(Value::Unit)
            }
            Builtin::Error => {
                let msg = match &args[0] {
                    Value::Str(s) => s.clone(),
                    other => format_value(other),
                };
                Err(rt_err(msg, span))
            }
            Builtin::Sqrt => match &args[0] {
                Value::Float(x) => Ok(Value::Float(x.sqrt())),
                Value::Int(x) => Ok(Value::Float((*x as f64).sqrt())),
                _ => Err(rt_err("`sqrt` expects a number", span)),
            },
            // `chr` maps a byte (mod 256) to a character; `snoc` appends a
            // character to a string (the cons for strings).
            Builtin::Chr => match &args[0] {
                Value::Int(n) => {
                    let byte = n.rem_euclid(256) as u32;
                    char::from_u32(byte)
                        .map(Value::Char)
                        .ok_or_else(|| rt_err("`chr` produced an invalid character", span))
                }
                _ => Err(rt_err("`chr` expects an Int", span)),
            },
            Builtin::Snoc => match (&args[0], &args[1]) {
                (Value::Str(s), Value::Char(c)) => {
                    let mut out = s.clone();
                    out.push(*c);
                    Ok(Value::Str(out))
                }
                _ => Err(rt_err("`snoc` expects a String and a Char", span)),
            },
        }
    }

    fn apply_func(&self, fv: FuncVal, arg: Value, span: Span) -> Result<Value, Diagnostic> {
        // A nullary function is a thunk; force it instead of applying.
        if fv.clauses.iter().all(|c| c.pats.is_empty()) {
            let env: Env = Rc::new(RefCell::new(fv.env.clone()));
            let body = fv.clauses[0].body.clone();
            let v = self.eval(&env, &body)?;
            return self.apply(v, arg, span);
        }
        let mut residual = Vec::new();
        for clause in &fv.clauses {
            let mut bindings = Vec::new();
            if match_pattern(&clause.pats[0], arg.clone(), &mut bindings) {
                let mut captured = clause.captured.clone();
                captured.extend(bindings);
                if clause.pats.len() == 1 {
                    let mut env = fv.env.clone();
                    env.push(bindings_scope(captured));
                    let env: Env = Rc::new(RefCell::new(env));
                    return self.eval(&env, &clause.body);
                }
                residual.push(FuncClause {
                    pats: clause.pats[1..].to_vec(),
                    body: clause.body.clone(),
                    span: clause.span,
                    captured,
                });
            }
        }
        if !residual.is_empty() {
            return Ok(Value::Func(FuncVal {
                name: fv.name.clone(),
                clauses: residual,
                env: fv.env.clone(),
            }));
        }
        Err(rt_err(
            format!("non-exhaustive patterns in function `{}`", fv.name),
            span,
        ))
    }

    fn dispatch_method(
        &self,
        class: &str,
        name: &str,
        arg: Value,
        span: Span,
    ) -> Result<Value, Diagnostic> {
        let head = runtime_head(&arg);
        let methods = self
            .info
            .instances
            .get(&(class.to_string(), head.clone()))
            .ok_or_else(|| rt_err(format!("no instance for `{class} {head}` at runtime"), span))?;
        let bindings = methods
            .get(name)
            .ok_or_else(|| rt_err(format!("method `{name}` not implemented"), span))?;
        // Each clause was desugared to `name = \pats -> body`; recover the
        // patterns to build a multi-clause function.
        let mut clauses = Vec::new();
        for b in bindings {
            match &b.expr {
                Expr::Lambda(pats, body, _) => clauses.push(FuncClause {
                    pats: pats.clone(),
                    body: (**body).clone(),
                    span: b.span,
                    captured: Vec::new(),
                }),
                other => clauses.push(FuncClause {
                    pats: Vec::new(),
                    body: other.clone(),
                    span: b.span,
                    captured: Vec::new(),
                }),
            }
        }
        let fv = FuncVal {
            name: name.to_string(),
            clauses,
            env: vec![self.global.clone()],
        };
        self.apply_func(fv, arg, span)
    }
}

fn numeric_binop(b: Builtin, a: Value, b2: Value) -> Result<Value, Diagnostic> {
    match (a, b2) {
        (Value::Int(x), Value::Int(y)) => {
            let r = match b {
                Builtin::Add => x + y,
                Builtin::Sub => x - y,
                Builtin::Mul => x * y,
                Builtin::Div => x / y,
                Builtin::Mod => x % y,
                _ => unreachable!(),
            };
            Ok(Value::Int(r))
        }
        (Value::Float(x), Value::Float(y)) => {
            let r = match b {
                Builtin::Add => x + y,
                Builtin::Sub => x - y,
                Builtin::Mul => x * y,
                Builtin::Div => x / y,
                Builtin::Mod => x % y,
                _ => unreachable!(),
            };
            Ok(Value::Float(r))
        }
        (Value::Int(x), Value::Float(y)) => Ok(Value::Float(arith_float(b, x as f64, y))),
        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(arith_float(b, x, y as f64))),
        _ => Err(rt_err("arithmetic on non-numeric values", Span::default())),
    }
}

fn arith_float(b: Builtin, x: f64, y: f64) -> f64 {
    match b {
        Builtin::Add => x + y,
        Builtin::Sub => x - y,
        Builtin::Mul => x * y,
        Builtin::Div => x / y,
        Builtin::Mod => x % y,
        _ => unreachable!(),
    }
}

fn ordering(b: Builtin, a: &Value, b2: &Value, span: Span) -> Result<Value, Diagnostic> {
    let cmp = match (a, b2) {
        (Value::Int(x), Value::Int(y)) => x.partial_cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y),
        (Value::Char(x), Value::Char(y)) => x.partial_cmp(y),
        (Value::Str(x), Value::Str(y)) => x.partial_cmp(y),
        _ => None,
    };
    let Some(cmp) = cmp else {
        return Err(rt_err("cannot compare these values", span));
    };
    let r = match b {
        Builtin::Lt => cmp.is_lt(),
        Builtin::Le => cmp.is_le(),
        Builtin::Gt => cmp.is_gt(),
        Builtin::Ge => cmp.is_ge(),
        _ => unreachable!(),
    };
    Ok(Value::Bool(r))
}

fn bindings_scope(bindings: Vec<(String, Value)>) -> Scope {
    let mut scope = Scope::new();
    for (n, v) in bindings {
        scope.insert(
            n,
            Rc::new(RefCell::new(VarCell {
                value: Some(v),
                mutable: false,
            })),
        );
    }
    scope
}

fn runtime_head(v: &Value) -> String {
    match v {
        Value::Int(_) => "Int".into(),
        Value::Float(_) => "Float".into(),
        Value::Bool(_) => "Bool".into(),
        Value::Char(_) => "Char".into(),
        Value::Str(_) => "String".into(),
        Value::Unit => "()".into(),
        Value::List(_) => "List".into(),
        Value::Tuple(_) => "Tuple".into(),
        Value::Adt { name, .. } => name.clone(),
        Value::Func(_) => "->".into(),
        _ => "?".into(),
    }
}

fn match_pattern(pat: &Pattern, value: Value, out: &mut Vec<(String, Value)>) -> bool {
    match pat {
        Pattern::Var(name, _) => {
            out.push((name.clone(), value));
            true
        }
        Pattern::Wildcard(_) => true,
        Pattern::Int(n, _) => matches!(value, Value::Int(x) if x == *n),
        Pattern::Float(f, _) => matches!(value, Value::Float(x) if x == *f),
        Pattern::Str(s, _) => matches!(value, Value::Str(x) if &x == s),
        Pattern::Char(c, _) => matches!(value, Value::Char(x) if x == *c),
        Pattern::Ctor { name, args, .. } => {
            if args.is_empty() && (name == "True" || name == "False") {
                return matches!(value, Value::Bool(b) if name == if b { "True" } else { "False" });
            }
            if let Value::Adt { ctor, fields, .. } = value {
                if &ctor != name || fields.len() != args.len() {
                    return false;
                }
                for (ap, fv) in args.iter().zip(fields) {
                    if !match_pattern(ap, fv, out) {
                        return false;
                    }
                }
                true
            } else {
                false
            }
        }
        Pattern::Tuple(ps, _) => {
            if let Value::Tuple(vs) = value {
                if ps.len() != vs.len() {
                    return false;
                }
                for (p, v) in ps.iter().zip(vs) {
                    if !match_pattern(p, v, out) {
                        return false;
                    }
                }
                true
            } else {
                false
            }
        }
        Pattern::List(ps, _) => {
            if let Value::List(vs) = value {
                if ps.len() != vs.len() {
                    return false;
                }
                for (p, v) in ps.iter().zip(vs) {
                    if !match_pattern(p, v, out) {
                        return false;
                    }
                }
                true
            } else {
                false
            }
        }
        Pattern::Cons(a, b, _) => {
            if let Value::List(mut vs) = value {
                if vs.is_empty() {
                    return false;
                }
                let head = vs.remove(0);
                let tail = Value::List(vs);
                match_pattern(a, head, out) && match_pattern(b, tail, out)
            } else if let Value::Str(s) = value {
                let mut chars = s.chars();
                let Some(head) = chars.next() else {
                    return false;
                };
                let tail = Value::Str(chars.as_str().to_string());
                match_pattern(a, Value::Char(head), out) && match_pattern(b, tail, out)
            } else {
                false
            }
        }
        Pattern::As(name, inner, _) => {
            let clone = value.deep_clone();
            out.push((name.clone(), clone));
            match_pattern(inner, value, out)
        }
        Pattern::Record { ctor, fields, .. } => {
            if let Value::Adt { ctor: vctor, record, .. } = value {
                if &vctor != ctor {
                    return false;
                }
                for (fname, fp) in fields {
                    let Some((_, fv)) = record.iter().find(|(n, _)| n == fname) else {
                        return false;
                    };
                    if !match_pattern(fp, fv.clone(), out) {
                        return false;
                    }
                }
                true
            } else {
                false
            }
        }
    }
}

// ==================== Prelude ====================

fn install_prelude(global: &mut Scope, info: &ProgramInfo) {
    let builtins: Vec<(&str, Builtin)> = vec![
        ("+", Builtin::Add),
        ("-", Builtin::Sub),
        ("*", Builtin::Mul),
        ("/", Builtin::Div),
        ("%", Builtin::Mod),
        ("negate", Builtin::Negate),
        ("==", Builtin::Eq),
        ("/=", Builtin::Neq),
        ("<", Builtin::Lt),
        ("<=", Builtin::Le),
        (">", Builtin::Gt),
        (">=", Builtin::Ge),
        ("&&", Builtin::And),
        ("||", Builtin::Or),
        ("not", Builtin::Not),
        (":", Builtin::Cons),
        ("++", Builtin::Append),
        ("show", Builtin::Show),
        ("println", Builtin::Println),
        ("print", Builtin::Print),
        ("error", Builtin::Error),
        ("sqrt", Builtin::Sqrt),
        ("chr", Builtin::Chr),
        ("snoc", Builtin::Snoc),
    ];
    for (name, b) in builtins {
        global.insert(
            name.to_string(),
            Rc::new(RefCell::new(VarCell {
                value: Some(Value::Builtin(b)),
                mutable: false,
            })),
        );
    }
    // Class methods become dispatch stubs.
    for (mname, cname) in &info.class_methods {
        global.insert(
            mname.clone(),
            Rc::new(RefCell::new(VarCell {
                value: Some(Value::Method {
                    class: cname.clone(),
                    name: mname.clone(),
                }),
                mutable: false,
            })),
        );
    }
    global.insert(
        "otherwise".to_string(),
        Rc::new(RefCell::new(VarCell {
            value: Some(Value::Bool(true)),
            mutable: false,
        })),
    );
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Char(x), Value::Char(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Unit, Value::Unit) => true,
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| values_equal(a, b))
        }
        (Value::Tuple(x), Value::Tuple(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| values_equal(a, b))
        }
        (Value::Adt { ctor: c1, fields: f1, .. }, Value::Adt { ctor: c2, fields: f2, .. }) => {
            c1 == c2
                && f1.len() == f2.len()
                && f1.iter().zip(f2).all(|(a, b)| values_equal(a, b))
        }
        _ => false,
    }
}

/// Strip a module prefix, leaving the final name component.
fn short_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn format_value(v: &Value) -> String {    match v {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            if f.fract() == 0.0 {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::Char(c) => format!("'{c}'"),
        Value::Str(s) => s.clone(),
        Value::Unit => "()".into(),
        Value::List(xs) => {
            let inner = xs.iter().map(format_value).collect::<Vec<_>>().join(", ");
            format!("[{inner}]")
        }
        Value::Tuple(xs) => {
            let inner = xs.iter().map(format_value).collect::<Vec<_>>().join(", ");
            format!("({inner})")
        }
        Value::Adt { ctor, fields, record, .. } => {
            let ctor = short_name(ctor);
            if !record.is_empty() {
                let inner = record
                    .iter()
                    .map(|(n, x)| format!("{n} = {}", format_value(x)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{ctor} {{ {inner} }}")
            } else if fields.is_empty() {
                ctor.to_string()
            } else {
                let inner = fields.iter().map(format_value).collect::<Vec<_>>().join(" ");
                format!("{ctor} {inner}")
            }
        }
        Value::Func(f) => format!("<function {}>", f.name),
        Value::CtorFn { name, .. } => format!("<constructor {}>", name),
        Value::Ref { .. } => "<reference>".into(),
        Value::Builtin(_) => "<builtin>".into(),
        Value::Partial { .. } => "<partially-applied builtin>".into(),
        Value::Method { class, name } => format!("<method {class}.{name}>"),
    }
}
