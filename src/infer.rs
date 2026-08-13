//! Hindley-Milner type inference with algebraic data types, type classes,
//! and reference types.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::error::{Diagnostic, Span};
use crate::types::{Constraint, Scheme, Ty, TyCon};

#[derive(Debug, Clone)]
enum VarState {
    Unbound,
    Link(Ty),
}

#[derive(Debug, Clone)]
pub struct AdtDef {
    pub name: String,
    pub params: Vec<String>,
    pub ctors: Vec<CtorDef>,
}

#[derive(Debug, Clone)]
pub struct CtorDef {
    pub name: String,
    pub fields: Vec<Type>,
    pub record: Vec<(String, Type)>,
}

#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub name: String,
    pub param: String,
    pub param_var: u32,
    pub superclasses: Vec<String>,
    pub methods: HashMap<String, TypeSig>,
}

#[derive(Debug, Clone)]
pub struct InstanceInfo {
    pub class: String,
    pub head: Ty,
    pub context: Vec<Constraint>,
    pub methods: Vec<Binding>,
}

/// The result of inference: types of bindings keyed by their span, used by
/// the ownership checker, plus the top-level environment.
#[derive(Debug, Default)]
pub struct TypeInfo {
    pub binding_types: HashMap<usize, Ty>,
    pub top_types: HashMap<String, Ty>,
    pub adts: Vec<AdtDef>,
    pub classes: HashMap<String, ClassInfo>,
    pub instances: Vec<InstanceInfo>,
    pub adt_ids: HashMap<String, usize>,
}

pub fn infer_program(program: &Program) -> Result<TypeInfo, Vec<Diagnostic>> {
    let mut inf = Infer::new();
    inf.collect(program);
    inf.check(program);
    if inf.errors.is_empty() {
        Ok(inf.type_info())
    } else {
        Err(inf.errors)
    }
}

pub struct Infer {
    vars: Vec<VarState>,
    constraints: Vec<Constraint>,
    env: Vec<HashMap<String, Scheme>>,
    errors: Vec<Diagnostic>,

    // program info
    adts: Vec<AdtDef>,
    adt_ids: HashMap<String, usize>,
    ctor_map: HashMap<String, (usize, usize)>, // ctor name -> (adt id, ctor idx)
    classes: HashMap<String, ClassInfo>,
    class_methods: HashMap<String, String>, // method name -> class name
    instances: Vec<InstanceInfo>,
    type_aliases: HashMap<String, (Vec<String>, Type)>,

    // binding types for ownership
    binding_types: HashMap<usize, Ty>,
    top_types: HashMap<String, Ty>,

    // pending signatures
    sigs: HashMap<String, TypeSig>,
    // mono vars for recursive top-level functions
    top_mono: HashMap<String, u32>,
}

impl Infer {
    fn new() -> Self {
        let mut inf = Infer {
            vars: Vec::new(),
            constraints: Vec::new(),
            env: vec![HashMap::new()],
            errors: Vec::new(),
            adts: Vec::new(),
            adt_ids: HashMap::new(),
            ctor_map: HashMap::new(),
            classes: HashMap::new(),
            class_methods: HashMap::new(),
            instances: Vec::new(),
            type_aliases: HashMap::new(),
            binding_types: HashMap::new(),
            top_types: HashMap::new(),
            sigs: HashMap::new(),
            top_mono: HashMap::new(),
        };
        inf.install_prelude();
        inf
    }

    fn type_info(&self) -> TypeInfo {
        let resolve_map = |m: &HashMap<usize, Ty>| -> HashMap<usize, Ty> {
            m.iter()
                .map(|(k, v)| (*k, self.resolve(v.clone())))
                .collect()
        };
        let resolve_top = |m: &HashMap<String, Ty>| -> HashMap<String, Ty> {
            m.iter()
                .map(|(k, v)| (k.clone(), self.resolve(v.clone())))
                .collect()
        };
        TypeInfo {
            binding_types: resolve_map(&self.binding_types),
            top_types: resolve_top(&self.top_types),
            adts: self.adts.clone(),
            classes: self.classes.clone(),
            instances: self.instances.clone(),
            adt_ids: self.adt_ids.clone(),
        }
    }

    // ==================== Variable management ====================

    fn new_var(&mut self) -> u32 {
        self.vars.push(VarState::Unbound);
        (self.vars.len() - 1) as u32
    }

    fn fresh_ty(&mut self) -> Ty {
        Ty::Var(self.new_var())
    }

    fn resolve(&self, ty: Ty) -> Ty {
        match ty {
            Ty::Var(id) => match self.vars.get(id as usize) {
                Some(VarState::Link(t)) => self.resolve(t.clone()),
                _ => Ty::Var(id),
            },
            Ty::Con(tc, args) => Ty::Con(
                tc,
                args.iter().map(|a| self.resolve(a.clone())).collect(),
            ),
        }
    }

    fn occurs(&self, id: u32, ty: &Ty) -> bool {
        let ty = self.resolve(ty.clone());
        match ty {
            Ty::Var(x) => x == id,
            Ty::Con(_, args) => args.iter().any(|a| self.occurs(id, a)),
        }
    }

    fn unify(&mut self, a: Ty, b: Ty, span: Span) -> Result<(), ()> {
        let a = self.resolve(a);
        let b = self.resolve(b);
        if a == b {
            return Ok(());
        }
        match (&a, &b) {
            (Ty::Var(x), t) => {
                if self.occurs(*x, t) {
                    self.errors.push(Diagnostic::error(
                        3002,
                        "infinite type: occurs check failed",
                        Some(span),
                    ));
                    return Err(());
                }
                self.vars[*x as usize] = VarState::Link(t.clone());
                Ok(())
            }
            (_, Ty::Var(_)) => self.unify(b, a, span),
            (Ty::Con(c1, a1), Ty::Con(c2, a2)) if c1 == c2 && a1.len() == a2.len() => {
                for (x, y) in a1.iter().zip(a2.iter()) {
                    self.unify(x.clone(), y.clone(), span)?;
                }
                Ok(())
            }
            _ => {
                self.errors.push(Diagnostic::error(
                    3001,
                    format!("type mismatch: expected `{}`, found `{}`", self.render(&a), self.render(&b)),
                    Some(span),
                ));
                Err(())
            }
        }
    }

    fn render(&self, ty: &Ty) -> String {
        self.render_ty(ty, 0)
    }

    fn render_ty(&self, ty: &Ty, depth: usize) -> String {
        if depth > 20 {
            return "...".into();
        }
        let ty = self.resolve(ty.clone());
        match ty {
            Ty::Var(id) => {
                const LETTERS: &str = "abcdefghijklmnopqrstuvwxyz";
                if (id as usize) < LETTERS.len() {
                    LETTERS.chars().nth(id as usize).unwrap().to_string()
                } else {
                    format!("t{id}")
                }
            }
            Ty::Con(tc, args) => match tc {
                TyCon::Int => "Int".into(),
                TyCon::Float => "Float".into(),
                TyCon::Bool => "Bool".into(),
                TyCon::Char => "Char".into(),
                TyCon::String => "String".into(),
                TyCon::Unit => "()".into(),
                TyCon::Func => {
                    let l = self.render_ty(&args[0], depth + 1);
                    let r = self.render_ty(&args[1], depth + 1);
                    let l = if matches!(self.resolve(args[0].clone()), Ty::Con(TyCon::Func, _)) {
                        format!("({l})")
                    } else {
                        l
                    };
                    format!("{l} -> {r}")
                }
                TyCon::Tuple(_) => {
                    let inner = args
                        .iter()
                        .map(|a| self.render_ty(a, depth + 1))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("({inner})")
                }
                TyCon::List => format!("[{}]", self.render_ty(&args[0], depth + 1)),
                TyCon::Ref => format!("&{}", self.render_ty(&args[0], depth + 1)),
                TyCon::MutRef => format!("&mut {}", self.render_ty(&args[0], depth + 1)),
                TyCon::Adt(id) => {
                    let name = self
                        .adts
                        .get(id)
                        .map(|a| a.name.clone())
                        .unwrap_or_else(|| format!("<adt {id}>"));
                    if args.is_empty() {
                        name
                    } else {
                        let inner = args
                            .iter()
                            .map(|a| self.render_ty(a, depth + 1))
                            .collect::<Vec<_>>()
                            .join(" ");
                        format!("{name} {inner}")
                    }
                }
            },
        }
    }

    // ==================== Constraints ====================

    fn add_constraint(&mut self, class: String, ty: Ty) {
        self.constraints.push(Constraint { class, ty });
    }

    fn collect_free(&self, ty: &Ty, out: &mut HashSet<u32>) {
        let ty = self.resolve(ty.clone());
        match ty {
            Ty::Var(id) => {
                out.insert(id);
            }
            Ty::Con(_, args) => {
                for a in args {
                    self.collect_free(&a, out);
                }
            }
        }
    }

    fn env_free_vars(&self) -> HashSet<u32> {
        let mut out = HashSet::new();
        for frame in &self.env {
            for scheme in frame.values() {
                for v in &scheme.vars {
                    out.insert(*v);
                }
                self.collect_free(&scheme.ty, &mut out);
                for c in &scheme.constraints {
                    self.collect_free(&c.ty, &mut out);
                }
            }
        }
        out
    }

    fn generalize(&mut self, ty: Ty, mark: usize) -> Scheme {
        // `mark` may exceed the current length after constraint solving has
        // removed concrete constraints; clamp it.
        let mark = mark.min(self.constraints.len());
        let captured: Vec<Constraint> = self.constraints.drain(mark..).collect();
        let env_frees = self.env_free_vars();
        // A constraint is captured only if none of its free vars are free in
        // the environment.
        let (owned, ambient): (Vec<Constraint>, Vec<Constraint>) = captured
            .into_iter()
            .partition(|c| {
                let mut fv = HashSet::new();
                self.collect_free(&c.ty, &mut fv);
                fv.is_disjoint(&env_frees)
            });
        self.constraints.extend(ambient);

        let mut frees = HashSet::new();
        self.collect_free(&ty, &mut frees);
        for c in &owned {
            self.collect_free(&c.ty, &mut frees);
        }
        let quantified: Vec<u32> = frees
            .into_iter()
            .filter(|v| !env_frees.contains(v))
            .collect();
        Scheme {
            vars: quantified,
            constraints: owned,
            ty,
        }
    }

    fn instantiate(&mut self, scheme: &Scheme) -> Ty {
        let mut subst: HashMap<u32, Ty> = HashMap::new();
        for v in &scheme.vars {
            subst.insert(*v, self.fresh_ty());
        }
        let ty = self.subst_ty(&scheme.ty, &subst);
        for c in &scheme.constraints {
            let cty = self.subst_ty(&c.ty, &subst);
            self.constraints.push(Constraint {
                class: c.class.clone(),
                ty: cty,
            });
        }
        ty
    }

    fn subst_ty(&self, ty: &Ty, subst: &HashMap<u32, Ty>) -> Ty {
        match ty {
            Ty::Var(id) => subst.get(id).cloned().unwrap_or(Ty::Var(*id)),
            Ty::Con(tc, args) => Ty::Con(
                *tc,
                args.iter().map(|a| self.subst_ty(a, subst)).collect(),
            ),
        }
    }

    fn solve_constraints(&mut self) {
        let mut i = 0;
        while i < self.constraints.len() {
            let c = self.constraints[i].clone();
            let ty = self.resolve(c.ty.clone());
            if matches!(ty, Ty::Con(_, _)) {
                self.constraints.remove(i);
                self.resolve_instance(&c.class, &ty);
                // New constraints may have been appended; restart the scan.
                i = 0;
            } else {
                i += 1;
            }
        }
    }

    fn resolve_instance(&mut self, class: &str, ty: &Ty) {
        // Add superclass constraints.
        if let Some(ci) = self.classes.get(class).cloned() {
            for sc in &ci.superclasses {
                self.add_constraint(sc.clone(), ty.clone());
            }
        }
        let Ty::Con(tc, args) = ty else { return };
        let Some(inst) = self
            .instances
            .iter()
            .find(|inst| inst.class == class && self.instance_matches(inst, tc, args))
            .cloned()
        else {
            self.errors.push(Diagnostic::error(
                3004,
                format!("no instance for `{} {}`", class, self.render(ty)),
                None,
            ));
            return;
        };
        // Instantiate the instance's context by substituting the head's type
        // variables with the concrete arguments.
        let head_args = match self.resolve(inst.head.clone()) {
            Ty::Con(_, ha) => ha,
            _ => vec![],
        };
        let mut subst: HashMap<u32, Ty> = HashMap::new();
        for (hv, arg) in head_args.iter().zip(args.iter()) {
            if let Ty::Var(v) = hv {
                subst.insert(*v, arg.clone());
            }
        }
        for c in &inst.context {
            let cty = self.subst_ty(&c.ty, &subst);
            self.add_constraint(c.class.clone(), cty);
        }
    }

    fn instance_matches(&self, inst: &InstanceInfo, tc: &TyCon, args: &[Ty]) -> bool {
        let head = self.resolve(inst.head.clone());
        match head {
            Ty::Con(htc, hargs) => htc == *tc && hargs.len() == args.len(),
            _ => false,
        }
    }

    // ==================== Environment ====================

    fn lookup(&mut self, name: &str) -> Option<Scheme> {
        for frame in self.env.iter().rev() {
            if let Some(s) = frame.get(name) {
                return Some(s.clone());
            }
        }
        None
    }

    fn insert_local(&mut self, name: &str, scheme: Scheme) {
        self.env.last_mut().unwrap().insert(name.to_string(), scheme);
    }

    fn push_scope(&mut self) {
        self.env.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.env.pop();
    }

    // ==================== Type conversion ====================

    fn convert_type(&mut self, ty: &Type, params: &mut HashMap<String, Ty>) -> Result<Ty, ()> {
        match ty {
            Type::Var(name, _) => {
                if let Some(t) = params.get(name) {
                    Ok(t.clone())
                } else {
                    // A free type variable not bound: treat as a fresh var
                    // (used in signatures without explicit forall). Insert so
                    // later occurrences of the same name reuse it.
                    let v = self.fresh_ty();
                    params.insert(name.clone(), v.clone());
                    Ok(v)
                }
            }
            Type::Con(name, args, _) => {
                let tc = self.type_con(name)?;
                let mut converted = Vec::new();
                for a in args {
                    converted.push(self.convert_type(a, params)?);
                }
                Ok(Ty::Con(tc, converted))
            }
            Type::Func(a, b, _) => {
                let ta = self.convert_type(a, params)?;
                let tb = self.convert_type(b, params)?;
                Ok(Ty::func(ta, tb))
            }
            Type::Tuple(ts, _) => {
                let mut conv = Vec::new();
                for t in ts {
                    conv.push(self.convert_type(t, params)?);
                }
                Ok(Ty::Con(TyCon::Tuple(ts.len()), conv))
            }
            Type::List(t, _) => {
                let te = self.convert_type(t, params)?;
                Ok(Ty::list(te))
            }
            Type::Ref { mutable, inner, .. } => {
                let ti = self.convert_type(inner, params)?;
                Ok(Ty::Con(if *mutable { TyCon::MutRef } else { TyCon::Ref }, vec![ti]))
            }
            Type::Unit(_) => Ok(Ty::unit()),
        }
    }

    fn type_con(&mut self, name: &str) -> Result<TyCon, ()> {
        match name {
            "Int" => Ok(TyCon::Int),
            "Float" => Ok(TyCon::Float),
            "Bool" => Ok(TyCon::Bool),
            "Char" => Ok(TyCon::Char),
            "String" => Ok(TyCon::String),
            "()" => Ok(TyCon::Unit),
            _ => {
                if let Some(id) = self.adt_ids.get(name) {
                    Ok(TyCon::Adt(*id))
                } else {
                    self.errors.push(Diagnostic::error(
                        3006,
                        format!("unknown type constructor `{name}`"),
                        None,
                    ));
                    Err(())
                }
            }
        }
    }

    /// Substitute an ADT's parameters with actual types in a field type.
    fn subst_field(&mut self, field: &Type, params: &[String], args: &[Ty]) -> Result<Ty, ()> {
        let mut map = HashMap::new();
        for (i, p) in params.iter().enumerate() {
            if let Some(a) = args.get(i) {
                map.insert(p.clone(), a.clone());
            }
        }
        self.convert_type(field, &mut map)
    }

    // ==================== Prelude ====================

    fn install_prelude(&mut self) {
        // Register built-in classes.
        let mk_class = |inf: &mut Self, name: &str| {
            let v = inf.new_var();
            inf.classes.insert(
                name.to_string(),
                ClassInfo {
                    name: name.to_string(),
                    param: "a".to_string(),
                    param_var: v,
                    superclasses: vec![],
                    methods: HashMap::new(),
                },
            );
        };
        mk_class(self, "Num");
        mk_class(self, "Eq");
        mk_class(self, "Ord");
        mk_class(self, "Show");
        mk_class(self, "Append");

        // Built-in instances.
        let builtin_instances: Vec<(&str, Ty, Vec<Constraint>)> = vec![
            ("Num", Ty::int(), vec![]),
            ("Num", Ty::float(), vec![]),
            ("Eq", Ty::int(), vec![]),
            ("Eq", Ty::float(), vec![]),
            ("Eq", Ty::bool(), vec![]),
            ("Eq", Ty::char(), vec![]),
            ("Eq", Ty::string(), vec![]),
            ("Eq", Ty::unit(), vec![]),
            (
                "Eq",
                Ty::list(Ty::Var(1)),
                vec![Constraint { class: "Eq".into(), ty: Ty::Var(1) }],
            ),
            (
                "Eq",
                Ty::Con(TyCon::Tuple(2), vec![Ty::Var(1), Ty::Var(2)]),
                vec![
                    Constraint { class: "Eq".into(), ty: Ty::Var(1) },
                    Constraint { class: "Eq".into(), ty: Ty::Var(2) },
                ],
            ),
            ("Ord", Ty::int(), vec![]),
            ("Ord", Ty::float(), vec![]),
            ("Ord", Ty::bool(), vec![]),
            ("Ord", Ty::char(), vec![]),
            ("Ord", Ty::string(), vec![]),
            ("Show", Ty::int(), vec![]),
            ("Show", Ty::float(), vec![]),
            ("Show", Ty::bool(), vec![]),
            ("Show", Ty::char(), vec![]),
            ("Show", Ty::string(), vec![]),
            ("Show", Ty::unit(), vec![]),
            (
                "Show",
                Ty::list(Ty::Var(1)),
                vec![Constraint { class: "Show".into(), ty: Ty::Var(1) }],
            ),
            (
                "Show",
                Ty::Con(TyCon::Tuple(2), vec![Ty::Var(1), Ty::Var(2)]),
                vec![
                    Constraint { class: "Show".into(), ty: Ty::Var(1) },
                    Constraint { class: "Show".into(), ty: Ty::Var(2) },
                ],
            ),
            (
                "Show",
                Ty::Con(TyCon::Tuple(3), vec![Ty::Var(1), Ty::Var(2), Ty::Var(3)]),
                vec![
                    Constraint { class: "Show".into(), ty: Ty::Var(1) },
                    Constraint { class: "Show".into(), ty: Ty::Var(2) },
                    Constraint { class: "Show".into(), ty: Ty::Var(3) },
                ],
            ),
            ("Append", Ty::string(), vec![]),
            ("Append", Ty::list(Ty::Var(1)), vec![]),
        ];
        for (class, head, context) in builtin_instances {
            self.instances.push(InstanceInfo {
                class: class.to_string(),
                head,
                context,
                methods: vec![],
            });
        }

        // Built-in function schemes.
        let mut prelude: Vec<(String, Scheme)> = Vec::new();

        // Num class methods.
        let a = self.new_var();
        let num_ctr = |t: Ty| vec![Constraint { class: "Num".into(), ty: t }];
        prelude.push((
            "+".into(),
            Scheme { vars: vec![a], constraints: num_ctr(Ty::Var(a)), ty: Ty::func(Ty::Var(a), Ty::func(Ty::Var(a), Ty::Var(a))) },
        ));
        let a = self.new_var();
        prelude.push((
            "-".into(),
            Scheme { vars: vec![a], constraints: num_ctr(Ty::Var(a)), ty: Ty::func(Ty::Var(a), Ty::func(Ty::Var(a), Ty::Var(a))) },
        ));
        let a = self.new_var();
        prelude.push((
            "*".into(),
            Scheme { vars: vec![a], constraints: num_ctr(Ty::Var(a)), ty: Ty::func(Ty::Var(a), Ty::func(Ty::Var(a), Ty::Var(a))) },
        ));
        let a = self.new_var();
        prelude.push((
            "/".into(),
            Scheme { vars: vec![a], constraints: num_ctr(Ty::Var(a)), ty: Ty::func(Ty::Var(a), Ty::func(Ty::Var(a), Ty::Var(a))) },
        ));
        let a = self.new_var();
        prelude.push((
            "%".into(),
            Scheme { vars: vec![a], constraints: num_ctr(Ty::Var(a)), ty: Ty::func(Ty::Var(a), Ty::func(Ty::Var(a), Ty::Var(a))) },
        ));
        let a = self.new_var();
        prelude.push((
            "negate".into(),
            Scheme { vars: vec![a], constraints: num_ctr(Ty::Var(a)), ty: Ty::func(Ty::Var(a), Ty::Var(a)) },
        ));

        // Structural comparison, ordering, and formatting are builtins that
        // work on any value (immutability makes deep comparison safe), so
        // they are unconstrained. `++` requires the `Append` class.
        let a = self.new_var();
        let e = Scheme { vars: vec![a], constraints: vec![], ty: Ty::func(Ty::Var(a), Ty::func(Ty::Var(a), Ty::bool())) };
        let a = self.new_var();
        let ne = Scheme { vars: vec![a], constraints: vec![], ty: Ty::func(Ty::Var(a), Ty::func(Ty::Var(a), Ty::bool())) };
        let a = self.new_var();
        let lt = Scheme { vars: vec![a], constraints: vec![], ty: Ty::func(Ty::Var(a), Ty::func(Ty::Var(a), Ty::bool())) };
        let a = self.new_var();
        let le = Scheme { vars: vec![a], constraints: vec![], ty: Ty::func(Ty::Var(a), Ty::func(Ty::Var(a), Ty::bool())) };
        let a = self.new_var();
        let gt = Scheme { vars: vec![a], constraints: vec![], ty: Ty::func(Ty::Var(a), Ty::func(Ty::Var(a), Ty::bool())) };
        let a = self.new_var();
        let ge = Scheme { vars: vec![a], constraints: vec![], ty: Ty::func(Ty::Var(a), Ty::func(Ty::Var(a), Ty::bool())) };
        let a = self.new_var();
        let show = Scheme { vars: vec![a], constraints: vec![], ty: Ty::func(Ty::Var(a), Ty::string()) };
        let a = self.new_var();
        let app = Scheme { vars: vec![a], constraints: vec![Constraint { class: "Append".into(), ty: Ty::Var(a) }], ty: Ty::func(Ty::Var(a), Ty::func(Ty::Var(a), Ty::Var(a))) };

        for (mname, mclass) in [
            ("==", "Eq"), ("/=", "Eq"),
            ("<", "Ord"), ("<=", "Ord"), (">", "Ord"), (">=", "Ord"),
            ("show", "Show"), ("++", "Append"),
        ] {
            self.class_methods.insert(mname.to_string(), mclass.to_string());
        }

        prelude.push(("==".into(), e));
        prelude.push(("/=".into(), ne));
        prelude.push(("<".into(), lt));
        prelude.push(("<=".into(), le));
        prelude.push((">".into(), gt));
        prelude.push((">=".into(), ge));
        prelude.push(("show".into(), show));
        prelude.push(("++".into(), app));

        // Other builtins.
        let a = self.new_var();
        let b = self.new_var();
        prelude.push((":".into(), Scheme { vars: vec![a, b], constraints: vec![], ty: Ty::func(Ty::Var(a), Ty::func(Ty::list(Ty::Var(a)), Ty::list(Ty::Var(a)))) }));
        prelude.push(("&&".into(), Scheme::mono(Ty::func(Ty::bool(), Ty::func(Ty::bool(), Ty::bool())))));
        prelude.push(("||".into(), Scheme::mono(Ty::func(Ty::bool(), Ty::func(Ty::bool(), Ty::bool())))));
        prelude.push(("not".into(), Scheme::mono(Ty::func(Ty::bool(), Ty::bool()))));

        let a = self.new_var();
        prelude.push(("println".into(), Scheme { vars: vec![a], constraints: vec![], ty: Ty::func(Ty::Var(a), Ty::unit()) }));
        prelude.push(("print".into(), Scheme { vars: vec![a], constraints: vec![], ty: Ty::func(Ty::Var(a), Ty::unit()) }));
        prelude.push(("sqrt".into(), Scheme::mono(Ty::func(Ty::float(), Ty::float()))));
        prelude.push(("chr".into(), Scheme::mono(Ty::func(Ty::int(), Ty::char()))));
        prelude.push(("snoc".into(), Scheme::mono(Ty::func(Ty::string(), Ty::func(Ty::char(), Ty::string())))));
        let a = self.new_var();
        prelude.push(("error".into(), Scheme { vars: vec![a], constraints: vec![], ty: Ty::func(Ty::string(), Ty::Var(a)) }));
        prelude.push(("otherwise".into(), Scheme::mono(Ty::bool())));

        for (name, scheme) in prelude {
            self.insert_local(&name, scheme);
        }
    }

    // ==================== Collection ====================

    fn collect(&mut self, program: &Program) {
        // Data types.
        for item in &program.items {
            if let Item::Data { name, params, ctors, .. } = item {
                let id = self.adts.len();
                self.adts.push(AdtDef {
                    name: name.clone(),
                    params: params.clone(),
                    ctors: ctors
                        .iter()
                        .map(|c| CtorDef {
                            name: c.name.clone(),
                            fields: c.fields.clone(),
                            record: c.record.iter().map(|f| (f.name.clone(), f.ty.clone())).collect(),
                        })
                        .collect(),
                });
                self.adt_ids.insert(name.clone(), id);
                for (i, c) in ctors.iter().enumerate() {
                    self.ctor_map.insert(c.name.clone(), (id, i));
                }
            }
        }
        // Type aliases.
        for item in &program.items {
            if let Item::TypeAlias { name, params, ty, .. } = item {
                self.type_aliases.insert(name.clone(), (params.clone(), ty.clone()));
            }
        }
        // Classes.
        for item in &program.items {
            if let Item::Class { name, param, superclasses, methods, .. } = item {
                let v = self.new_var();
                let ci = ClassInfo {
                    name: name.clone(),
                    param: param.clone(),
                    param_var: v,
                    superclasses: superclasses
                        .iter()
                        .map(|s| match s {
                            Type::Con(n, _, _) => n.clone(),
                            _ => String::new(),
                        })
                        .collect(),
                    methods: methods.iter().map(|m| (m.name.clone(), m.clone())).collect(),
                };
                self.classes.insert(name.clone(), ci);
                for m in methods {
                    self.class_methods.insert(m.name.clone(), name.clone());
                }
            }
        }
        // Instances.
        for item in &program.items {
            if let Item::Instance { class, context, methods, .. } = item {
                let mut pm = HashMap::new();
                let head = match self.convert_type_head(class, &mut pm) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let mut ctx = Vec::new();
                for c in context {
                    if let Some(cn) = self.convert_constraint(c) {
                        ctx.push(cn);
                    }
                }
                self.instances.push(InstanceInfo {
                    class: self.instance_class_name(class),
                    head,
                    context: ctx,
                    methods: methods.clone(),
                });
            }
        }
        // Signatures.
        for item in &program.items {
            if let Item::Signature { sig, .. } = item {
                self.sigs.insert(sig.name.clone(), sig.clone());
            }
        }
        // Pre-register top-level functions for recursion.
        for item in &program.items {
            if let Item::Function(f) = item {
                let v = self.new_var();
                self.top_mono.insert(f.name.clone(), v);
                self.env[0].insert(f.name.clone(), Scheme::mono(Ty::Var(v)));
            }
        }
    }

    fn instance_class_name(&self, class: &Type) -> String {
        match class {
            Type::Con(name, _, _) => name.clone(),
            _ => String::new(),
        }
    }

    fn convert_type_head(&mut self, class: &Type, pm: &mut HashMap<String, Ty>) -> Result<Ty, ()> {
        // `class` has the form `C arg1 ... argn`; the head type is the first
        // argument (the class parameter). The class name itself is not a type.
        match class {
            Type::Con(_, args, _) => match args.first() {
                Some(a) => self.convert_type(a, pm),
                None => Err(()),
            },
            _ => Err(()),
        }
    }

    // ==================== Checking ====================

    fn check(&mut self, program: &Program) {
        // Register class methods in the environment.
        let class_methods: Vec<(String, u32, String, TypeSig)> = self
            .classes
            .iter()
            .flat_map(|(cname, ci)| {
                ci.methods
                    .iter()
                    .map(|(mname, sig)| (mname.clone(), ci.param_var, cname.clone(), sig.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (mname, pvar, cname, sig) in class_methods {
            let mut map = HashMap::new();
            let class_param = self.classes.get(&cname).unwrap().param.clone();
            map.insert(class_param, Ty::Var(pvar));
            if let Ok(ty) = self.convert_type(&sig.body, &mut map) {
                let scheme = Scheme {
                    vars: vec![pvar],
                    constraints: vec![Constraint { class: cname.clone(), ty: Ty::Var(pvar) }],
                    ty,
                };
                self.env[0].insert(mname, scheme);
            }
        }

        for item in &program.items.clone() {
            match item {
                Item::Function(f) => self.check_function(f),
                Item::Instance { class, methods, .. } => self.check_instance(class, methods),
                _ => {}
            }
        }
    }

    fn check_function(&mut self, f: &Function) {
        let name = f.name.clone();
        let mono_var = match self.top_mono.get(&name) {
            Some(v) => *v,
            None => self.new_var(),
        };
        let mark = self.constraints.len();

        // If a signature exists, compute the declared type and its argument
        // types up front so patterns are typed before the body is checked
        // (needed for record field access).
        let declared = match self.sigs.get(&name).cloned() {
            Some(sig) => {
                let mut map = HashMap::new();
                self.convert_type(&sig.body, &mut map).ok()
            }
            None => None,
        };
        let expected_args = declared.as_ref().map(|d| self.func_args(d.clone()));

        let mut ok = true;
        let inferred = Ty::Var(mono_var);
        for clause in &f.clauses {
            match self.infer_clause(clause, expected_args.as_deref()) {
                Some(t) => {
                    if self.unify(inferred.clone(), t, clause.span).is_err() {
                        ok = false;
                    }
                }
                None => ok = false,
            }
        }
        if !ok {
            return;
        }

        // Check against the signature, and add its constraints.
        if let Some(sig) = self.sigs.get(&name).cloned() {
            if let Some(declared) = &declared {
                let _ = self.unify(inferred.clone(), declared.clone(), sig.span);
            }
            for c in &sig.constraints {
                if let Some(cn) = self.convert_constraint(c) {
                    self.add_constraint(cn.class, cn.ty);
                }
            }
        }

        self.solve_constraints();
        let resolved = self.resolve(inferred.clone());
        // Remove the function's own provisional mono entry so its type
        // variables are not seen as "ambient" during generalization.
        self.env[0].remove(&name);
        let scheme = self.generalize(resolved.clone(), mark);
        self.top_types.insert(name.clone(), resolved);
        self.env[0].insert(name, scheme);
    }

    /// Split a (possibly curried) function type into its argument types.
    fn func_args(&self, ty: Ty) -> Vec<Ty> {
        let mut args = Vec::new();
        let mut cur = ty;
        loop {
            match self.resolve(cur) {
                Ty::Con(TyCon::Func, fargs) => {
                    args.push(fargs[0].clone());
                    cur = fargs[1].clone();
                }
                _ => break,
            }
        }
        args
    }

    /// Convert a class constraint of the form `C ty1 ... tyn` into a
    /// `Constraint`, converting only the argument types (the class name is
    /// not a type constructor).
    fn convert_constraint(&mut self, c: &Type) -> Option<Constraint> {
        match c {
            Type::Con(class, args, _) => {
                let arg = args.first()?.clone();
                let mut map = HashMap::new();
                let ty = self.convert_type(&arg, &mut map).ok()?;
                Some(Constraint {
                    class: class.clone(),
                    ty,
                })
            }
            _ => None,
        }
    }

    fn infer_clause(&mut self, clause: &Clause, expected: Option<&[Ty]>) -> Option<Ty> {
        self.push_scope();
        let mut arg_tys = Vec::new();
        let mut ok = true;
        for (i, p) in clause.pats.iter().enumerate() {
            match self.infer_pattern(p, expected.and_then(|e| e.get(i))) {
                Some(t) => {
                    if let Some(exp) = expected.and_then(|e| e.get(i)) {
                        let _ = self.unify(t.clone(), exp.clone(), p.span());
                    }
                    arg_tys.push(t);
                }
                None => ok = false,
            }
        }
        let body_ty = self.infer_expr(&clause.body);
        self.pop_scope();
        if !ok {
            return None;
        }
        let body_ty = body_ty?;
        let mut ty = body_ty;
        for a in arg_tys.into_iter().rev() {
            ty = Ty::func(a, ty);
        }
        Some(ty)
    }

    // ==================== Patterns ====================

    fn infer_pattern(&mut self, pat: &Pattern, expected: Option<&Ty>) -> Option<Ty> {
        match pat {
            Pattern::Var(name, span) => {
                let t = self.fresh_ty();
                self.binding_types.insert(span.lo, t.clone());
                self.insert_local(name, Scheme::mono(t.clone()));
                Some(t)
            }
            Pattern::Wildcard(_) => Some(self.fresh_ty()),
            Pattern::Int(_, _) => Some(Ty::int()),
            Pattern::Float(_, _) => Some(Ty::float()),
            Pattern::Str(_, _) => Some(Ty::string()),
            Pattern::Char(_, _) => Some(Ty::char()),
            Pattern::Ctor { name, args, span } => {
                if (name == "True" || name == "False") && args.is_empty() {
                    return Some(Ty::bool());
                }
                let (adt_id, ctor_idx) = *self.ctor_map.get(name)?;
                let params = self.adts[adt_id].params.clone();
                let fields = self.adts[adt_id].ctors[ctor_idx].fields.clone();
                let record = self.adts[adt_id].ctors[ctor_idx].record.clone();
                let n_args = if record.is_empty() { fields.len() } else { 1 };
                if args.len() != n_args {
                    self.errors.push(Diagnostic::error(
                        3007,
                        format!("constructor `{name}` expects {n_args} argument(s), got {}", args.len()),
                        Some(*span),
                    ));
                    return None;
                }
                let fresh: Vec<Ty> = params.iter().map(|_| self.fresh_ty()).collect();
                for (i, ap) in args.iter().enumerate() {
                    let field_ty = if record.is_empty() {
                        self.subst_field(&fields[i], &params, &fresh)
                    } else {
                        self.subst_field(&record[0].1, &params, &fresh)
                    };
                    let apt = self.infer_pattern(ap, None)?;
                    if let Ok(ft) = field_ty {
                        let _ = self.unify(apt, ft, ap.span());
                    }
                }
                Some(Ty::Con(TyCon::Adt(adt_id), fresh))
            }
            Pattern::Tuple(ps, _) => {
                let mut ts = Vec::new();
                for p in ps {
                    ts.push(self.infer_pattern(p, None)?);
                }
                Some(Ty::Con(TyCon::Tuple(ps.len()), ts))
            }
            Pattern::List(ps, _) => {
                let te = self.fresh_ty();
                for p in ps {
                    let pt = self.infer_pattern(p, None)?;
                    let _ = self.unify(pt, te.clone(), p.span());
                }
                Some(Ty::list(te))
            }
            Pattern::Cons(a, b, _) => {
                // A cons pattern destructures a list, or — when the expected
                // type is `String` — a string (head `Char`, tail `String`).
                if let Some(Ty::Con(TyCon::String, _)) =
                    expected.map(|e| self.resolve(e.clone()))
                {
                    let ta = self.infer_pattern(a, None)?;
                    let tb = self.infer_pattern(b, None)?;
                    let _ = self.unify(ta, Ty::char(), a.span());
                    let _ = self.unify(tb, Ty::string(), b.span());
                    Some(Ty::string())
                } else {
                    let ta = self.infer_pattern(a, None)?;
                    let tb = self.infer_pattern(b, None)?;
                    let _ = self.unify(tb, Ty::list(ta.clone()), b.span());
                    Some(Ty::list(ta))
                }
            }
            Pattern::As(name, inner, span) => {
                let t = self.infer_pattern(inner, None)?;
                self.binding_types.insert(span.lo, t.clone());
                self.insert_local(name, Scheme::mono(t.clone()));
                Some(t)
            }
            Pattern::Record { ctor, fields, span } => {
                let (adt_id, ctor_idx) = *self.ctor_map.get(ctor)?;
                let params = self.adts[adt_id].params.clone();
                let record = self.adts[adt_id].ctors[ctor_idx].record.clone();
                let fresh: Vec<Ty> = params.iter().map(|_| self.fresh_ty()).collect();
                for (fname, fp) in fields {
                    if let Some((_, fty)) = record.iter().find(|(n, _)| n == fname) {
                        let ft = self.subst_field(fty, &params, &fresh).ok()?;
                        let pt = self.infer_pattern(fp, None)?;
                        let _ = self.unify(pt, ft, fp.span());
                    } else {
                        self.errors.push(Diagnostic::error(
                            3008,
                            format!("unknown field `{fname}` in constructor `{ctor}`"),
                            Some(*span),
                        ));
                        return None;
                    }
                }
                Some(Ty::Con(TyCon::Adt(adt_id), fresh))
            }
        }
    }

    // ==================== Expressions ====================

    fn infer_expr(&mut self, e: &Expr) -> Option<Ty> {
        match e {
            Expr::Int(_, _) => Some(Ty::int()),
            Expr::Float(_, _) => Some(Ty::float()),
            Expr::Str(_, _) => Some(Ty::string()),
            Expr::Char(_, _) => Some(Ty::char()),
            Expr::Unit(_) => Some(Ty::unit()),
            Expr::Var(name, span) => match self.lookup(name) {
                Some(scheme) => Some(self.instantiate(&scheme)),
                None => {
                    self.errors.push(Diagnostic::error(
                        3009,
                        format!("unbound name `{name}`"),
                        Some(*span),
                    ));
                    None
                }
            },
            Expr::Ctor(name, span) => self.infer_ctor(name, *span),
            Expr::App(f, a, span) => {
                let tf = self.infer_expr(f)?;
                let ta = self.infer_expr(a)?;
                let tr = self.fresh_ty();
                if self.unify(tf, Ty::func(ta, tr.clone()), *span).is_err() {
                    return None;
                }
                Some(tr)
            }
            Expr::Lambda(pats, body, span) => {
                self.push_scope();
                let mut arg_tys = Vec::new();
                let mut ok = true;
                for p in pats {
                    match self.infer_pattern(p, None) {
                        Some(t) => arg_tys.push(t),
                        None => ok = false,
                    }
                }
                let body_ty = self.infer_expr(body);
                self.pop_scope();
                if !ok {
                    return None;
                }
                let mut ty = body_ty?;
                for a in arg_tys.into_iter().rev() {
                    ty = Ty::func(a, ty);
                }
                let _ = span;
                Some(ty)
            }
            Expr::Let(bindings, body, _) => {
                self.push_scope();
                for b in bindings {
                    self.infer_binding(b);
                }
                let body_ty = self.infer_expr(body);
                self.pop_scope();
                body_ty
            }
            Expr::If(c, t, e, span) => {
                let tc = self.infer_expr(c)?;
                if self.unify(tc, Ty::bool(), c.span()).is_err() {
                    return None;
                }
                let tt = self.infer_expr(t)?;
                let te = self.infer_expr(e)?;
                if self.unify(tt.clone(), te, *span).is_err() {
                    return None;
                }
                Some(tt)
            }
            Expr::Case(scrut, alts, span) => {
                let ts = self.infer_expr(scrut)?;
                let tr = self.fresh_ty();
                for alt in alts {
                    self.push_scope();
                    if let Some(pt) = self.infer_pattern(&alt.pat, None) {
                        let _ = self.unify(ts.clone(), pt, alt.pat.span());
                    }
                    if let Some(bt) = self.infer_expr(&alt.body) {
                        let _ = self.unify(bt, tr.clone(), alt.span);
                    }
                    self.pop_scope();
                }
                let _ = span;
                Some(tr)
            }
            Expr::Tuple(es, _) => {
                let mut ts = Vec::new();
                for x in es {
                    ts.push(self.infer_expr(x)?);
                }
                Some(Ty::Con(TyCon::Tuple(es.len()), ts))
            }
            Expr::List(es, _) => {
                let te = self.fresh_ty();
                for x in es {
                    let xt = self.infer_expr(x)?;
                    let _ = self.unify(xt, te.clone(), x.span());
                }
                Some(Ty::list(te))
            }
            Expr::Assign(name, e, span) => {
                let tv = match self.lookup(name) {
                    Some(s) => self.instantiate(&s),
                    None => {
                        self.errors.push(Diagnostic::error(
                            3009,
                            format!("unbound name `{name}`"),
                            Some(*span),
                        ));
                        return None;
                    }
                };
                let te = self.infer_expr(e)?;
                let _ = self.unify(tv, te, *span);
                Some(Ty::unit())
            }
            Expr::Borrow { mutable, expr, span: _ } => {
                let te = self.infer_expr(expr)?;
                Some(Ty::Con(if *mutable { TyCon::MutRef } else { TyCon::Ref }, vec![te]))
            }
            Expr::Deref(e, span) => {
                let te = self.infer_expr(e)?;
                let te = self.resolve(te);
                match te {
                    Ty::Con(TyCon::Ref, args) | Ty::Con(TyCon::MutRef, args) => {
                        Some(args[0].clone())
                    }
                    Ty::Var(v) => {
                        let t = self.fresh_ty();
                        let _ = self.unify(Ty::Var(v), Ty::Con(TyCon::Ref, vec![t.clone()]), *span);
                        Some(t)
                    }
                    _ => {
                        self.errors.push(Diagnostic::error(
                            3014,
                            "cannot dereference a non-reference",
                            Some(*span),
                        ));
                        None
                    }
                }
            }
            Expr::Clone(e, _) => self.infer_expr(e),
            Expr::Qualified(q, m, span) => {
                self.errors.push(Diagnostic::error(
                    3015,
                    format!("unresolved qualified name `{q}.{m}`"),
                    Some(*span),
                ));
                None
            }
            Expr::Record { ctor, fields, span } => {
                let (adt_id, ctor_idx) = *self.ctor_map.get(ctor)?;
                let def = self.adts[adt_id].clone();
                let record = def.ctors[ctor_idx].record.clone();
                let fresh: Vec<Ty> = def.params.iter().map(|_| self.fresh_ty()).collect();
                for (fname, fexpr) in fields {
                    let Some((_, fty)) = record.iter().find(|(n, _)| n == fname) else {
                        self.errors.push(Diagnostic::error(
                            3008,
                            format!("unknown field `{fname}` in constructor `{ctor}`"),
                            Some(*span),
                        ));
                        return None;
                    };
                    let ft = self.subst_field(fty, &def.params, &fresh).ok()?;
                    let et = self.infer_expr(fexpr)?;
                    let _ = self.unify(et, ft, fexpr.span());
                }
                Some(Ty::Con(TyCon::Adt(adt_id), fresh))
            }
            Expr::Field(e, name, span) => {
                let te = self.infer_expr(e)?;
                let resolved = self.resolve(te);
                if let Ty::Con(TyCon::Adt(id), args) = resolved {
                    let def = self.adts[id].clone();
                    // Find record field across constructors.
                    let field = def
                        .ctors
                        .iter()
                        .find_map(|c| c.record.iter().find(|(n, _)| n == name))
                        .map(|(_, t)| t.clone());
                    match field {
                        Some(fty) => self.subst_field(&fty, &def.params, &args).ok(),
                        None => {
                            self.errors.push(Diagnostic::error(
                                3010,
                                format!("type `{}` has no field `{name}`", def.name),
                                Some(*span),
                            ));
                            None
                        }
                    }
                } else {
                    self.errors.push(Diagnostic::error(
                        3011,
                        "field access requires a record type",
                        Some(*span),
                    ));
                    None
                }
            }
            Expr::Ascribe(e, ty, span) => {
                let te = self.infer_expr(e)?;
                let mut map = HashMap::new();
                let expected = self.convert_type(ty, &mut map).ok()?;
                if self.unify(te, expected.clone(), *span).is_err() {
                    return None;
                }
                Some(expected)
            }
        }
    }

    fn infer_ctor(&mut self, name: &str, span: Span) -> Option<Ty> {
        if name == "True" || name == "False" {
            return Some(Ty::bool());
        }
        let (adt_id, ctor_idx) = *self.ctor_map.get(name)?;
        let def = self.adts[adt_id].clone();
        let ctor = def.ctors[ctor_idx].clone();
        let fresh: Vec<Ty> = def.params.iter().map(|_| self.fresh_ty()).collect();
        let mut field_tys = Vec::new();
        if ctor.record.is_empty() {
            for f in &ctor.fields {
                field_tys.push(self.subst_field(f, &def.params, &fresh).ok()?);
            }
        }
        let result = Ty::Con(TyCon::Adt(adt_id), fresh.clone());
        let mut ty = result;
        if ctor.record.is_empty() {
            for f in field_tys.into_iter().rev() {
                ty = Ty::func(f, ty);
            }
        }
        let _ = span;
        Some(ty)
    }

    fn infer_binding(&mut self, b: &Binding) {
        // lhs has a single pattern (function bindings were desugared to lambdas).
        if let Some(pat) = b.lhs.first() {
            if let Pattern::Var(name, span) = pat {
                let mark = self.constraints.len();
                let t = self.infer_expr(&b.expr);
                if let Some(t) = t {
                    let scheme = self.generalize(t.clone(), mark);
                    self.binding_types.insert(span.lo, t);
                    self.insert_local(name, scheme);
                }
            } else {
                if let Some(pt) = self.infer_pattern(pat, None)
                    && let Some(et) = self.infer_expr(&b.expr) {
                        let _ = self.unify(pt, et, b.span);
                    }
            }
        }
    }

    fn check_instance(&mut self, class: &Type, methods: &[Binding]) {
        let class_name = self.instance_class_name(class);
        let Some(ci) = self.classes.get(&class_name).cloned() else {
            self.errors.push(Diagnostic::error(
                3012,
                format!("unknown class `{class_name}`"),
                None,
            ));
            return;
        };
        // Build the instance head type.
        let mut pm = HashMap::new();
        let head = match self.convert_type_head(class, &mut pm) {
            Ok(t) => t,
            Err(_) => return,
        };
        // For each method, check its body against the class method type with
        // the head substituted.
        for m in methods {
            let Some(mname) = m.name().map(|s| s.to_string()) else {
                continue;
            };
            let Some(sig) = ci.methods.get(&mname).cloned() else {
                self.errors.push(Diagnostic::error(
                    3013,
                    format!("`{mname}` is not a method of class `{class_name}`"),
                    Some(m.span),
                ));
                continue;
            };
            let mut map = HashMap::new();
            map.insert(ci.param.clone(), head.clone());
            if let Ok(expected) = self.convert_type(&sig.body, &mut map) {
                // Method bindings are desugared to `name = \pats -> body`,
                // so the expected type applies directly to the body.
                let found = self.infer_expr(&m.expr);
                if let Some(found) = found {
                    let _ = self.unify(found, expected, m.span);
                }
                self.solve_constraints();
            }
        }
    }
}
