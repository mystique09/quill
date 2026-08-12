//! Module loading: resolves `use` imports, honours export lists, rewrites
//! definitions to globally-unique flat names, and merges modules into a
//! single program for the rest of the pipeline.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::ast::*;
use crate::error::{Diagnostic, Span};
use crate::parser;

pub struct Loader {
    search_paths: Vec<PathBuf>,
    embedded: HashMap<String, &'static str>,
    include_prelude: bool,
}

struct LoadedModule {
    canonical: String,
    is_root: bool,
    program: Program,
}

/// The top-level names a module defines, and which of them are exported.
#[derive(Default)]
struct ModuleDefs {
    /// unqualified name -> flat name, for every definable name.
    own: HashMap<String, String>,
    /// constructor name -> its data type name.
    ctor_of: HashMap<String, String>,
    /// method name -> its class name.
    method_of: HashMap<String, String>,
    /// exported unqualified name -> flat name.
    exported: HashMap<String, String>,
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

impl Loader {
    pub fn new() -> Self {
        let mut embedded: HashMap<String, &'static str> = HashMap::new();
        embedded.insert("Std/List.q".to_string(), include_str!("../stdlib/List.q"));
        embedded.insert("Std/Option.q".to_string(), include_str!("../stdlib/Option.q"));
        embedded.insert("Std/Result.q".to_string(), include_str!("../stdlib/Result.q"));
        embedded.insert("Std/Function.q".to_string(), include_str!("../stdlib/Function.q"));
        Loader {
            search_paths: vec![PathBuf::from(".")],
            embedded,
            include_prelude: true,
        }
    }

    /// A loader that does not auto-import the standard prelude (used by the
    /// native compiler while it catches up with the full language).
    pub fn without_prelude() -> Self {
        let mut l = Self::new();
        l.include_prelude = false;
        l
    }

    pub fn add_search_path(&mut self, path: PathBuf) {
        self.search_paths.push(path);
    }

    /// Load a program given its source text (treated as the `Main` module).
    pub fn load_source(&self, src: &str) -> Result<Program, Vec<Diagnostic>> {
        let mut errors = Vec::new();
        let mut loaded = Vec::new();
        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();
        // Load the implicit prelude module.
        if self.include_prelude {
            if let Ok(prelude_src) = self.resolve("Std.List", Span::default()) {
                self.load_module(
                    "Std.List".to_string(),
                    &prelude_src,
                    false,
                    &mut loaded,
                    &mut visiting,
                    &mut visited,
                    &mut errors,
                );
            }
        }
        self.load_module(
            "Main".to_string(),
            src,
            true,
            &mut loaded,
            &mut visiting,
            &mut visited,
            &mut errors,
        );
        if !errors.is_empty() {
            return Err(errors);
        }
        self.merge(loaded, &mut errors)
    }

    /// Load a program from a file, resolving imports relative to the file.
    pub fn load_file(&self, path: &Path) -> Result<Program, Vec<Diagnostic>> {
        let src = std::fs::read_to_string(path).map_err(|e| {
            vec![Diagnostic::error(
                6000,
                format!("cannot read `{}`: {e}", path.display()),
                None,
            )]
        })?;
        let mut loader = Loader {
            search_paths: self.search_paths.clone(),
            embedded: self.embedded.clone(),
            include_prelude: self.include_prelude,
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty() {
                loader.search_paths.push(parent.to_path_buf());
            }
        loader.load_source(&src)
    }

    fn load_module(
        &self,
        canonical: String,
        src: &str,
        is_root: bool,
        loaded: &mut Vec<LoadedModule>,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        errors: &mut Vec<Diagnostic>,
    ) {
        if visited.contains(&canonical) {
            return;
        }
        if !visiting.insert(canonical.clone()) {
            errors.push(Diagnostic::error(
                6003,
                format!("cyclic module import involving `{canonical}`"),
                None,
            ));
            return;
        }
        let program = match parser::parse(src) {
            Ok(p) => p,
            Err(e) => {
                errors.extend(e);
                visiting.remove(&canonical);
                return;
            }
        };
        for item in &program.items {
            if let Item::Use { path, span, .. } = item {
                match self.resolve(path, *span) {
                    Ok(dep_src) => {
                        self.load_module(
                            path.clone(),
                            &dep_src,
                            false,
                            loaded,
                            visiting,
                            visited,
                            errors,
                        );
                    }
                    Err(d) => errors.push(d),
                }
            }
        }
        visiting.remove(&canonical);
        visited.insert(canonical.clone());
        loaded.push(LoadedModule {
            canonical,
            is_root,
            program,
        });
    }

    fn resolve(&self, path: &str, span: Span) -> Result<String, Diagnostic> {
        let rel = format!("{}.q", path.replace('.', "/"));
        if let Some(src) = self.embedded.get(&rel) {
            return Ok(src.to_string());
        }
        for p in &self.search_paths {
            let full = p.join(&rel);
            if full.exists() {
                return std::fs::read_to_string(&full).map_err(|e| {
                    Diagnostic::error(
                        6001,
                        format!("cannot read module `{path}` from `{}`: {e}", full.display()),
                        Some(span),
                    )
                });
            }
        }
        Err(Diagnostic::error(
            6002,
            format!("module `{path}` not found (looked for `{rel}`)"),
            Some(span),
        ))
    }

    fn flat(&self, canonical: &str, is_root: bool, name: &str) -> String {
        if is_root {
            name.to_string()
        } else {
            format!("{canonical}.{name}")
        }
    }

    fn collect_defs(&self, m: &LoadedModule) -> ModuleDefs {
        let mut defs = ModuleDefs::default();
        let flat = |name: &str| self.flat(&m.canonical, m.is_root, name);
        for item in &m.program.items {
            match item {
                Item::Function(f) => {
                    defs.own.insert(f.name.clone(), flat(&f.name));
                }
                Item::Signature { sig, .. } => {
                    defs.own.insert(sig.name.clone(), flat(&sig.name));
                }
                Item::TypeAlias { name, .. } => {
                    defs.own.insert(name.clone(), flat(name));
                }
                Item::Data { name, ctors, .. } => {
                    defs.own.insert(name.clone(), flat(name));
                    for c in ctors {
                        defs.own.insert(c.name.clone(), flat(&c.name));
                        defs.ctor_of.insert(c.name.clone(), name.clone());
                    }
                }
                Item::Class { name, methods, .. } => {
                    defs.own.insert(name.clone(), flat(name));
                    for m in methods {
                        defs.own.insert(m.name.clone(), flat(&m.name));
                        defs.method_of.insert(m.name.clone(), name.clone());
                    }
                }
                _ => {}
            }
        }
        // Determine the exported set.
        let all: Vec<String> = defs.own.keys().cloned().collect();
        let mut export_all = || {
            for n in &all {
                defs.exported.insert(n.clone(), defs.own[n].clone());
            }
        };
        match &m.program.exports {
            None => export_all(),
            Some(list) => {
                for name in list {
                    if let Some(flat) = defs.own.get(name) {
                        defs.exported.insert(name.clone(), flat.clone());
                    }
                    // Export constructors of a listed data type and methods
                    // of a listed class.
                    for (ctor, ty) in &defs.ctor_of {
                        if ty == name {
                            defs.exported
                                .insert(ctor.clone(), defs.own[ctor].clone());
                        }
                    }
                    for (method, class) in &defs.method_of {
                        if class == name {
                            defs.exported
                                .insert(method.clone(), defs.own[method].clone());
                        }
                    }
                }
            }
        }
        defs
    }

    fn merge(&self, loaded: Vec<LoadedModule>, errors: &mut Vec<Diagnostic>) -> Result<Program, Vec<Diagnostic>> {
        // Export table: canonical -> (unqualified -> flat).
        let mut defs_map: HashMap<String, ModuleDefs> = HashMap::new();
        for m in &loaded {
            defs_map.insert(m.canonical.clone(), self.collect_defs(m));
        }
        let exports: HashMap<String, HashMap<String, String>> = defs_map
            .iter()
            .map(|(c, d)| (c.clone(), d.exported.clone()))
            .collect();

        let mut all_items: Vec<Item> = Vec::new();
        for m in &loaded {
            let defs = &defs_map[&m.canonical];
            // Build this module's resolution scope.
            let mut names: HashMap<String, String> = HashMap::new();
            let mut local: HashSet<String> = HashSet::new();
            for (n, flat) in &defs.own {
                names.insert(n.clone(), flat.clone());
                local.insert(n.clone());
            }
            let mut aliases: HashMap<String, String> = HashMap::new();
            // The root module implicitly imports the prelude (Std.List).
            if m.is_root
                && let Some(prelude) = exports.get("Std.List") {
                    for (unqual, flat) in prelude {
                        if !local.contains(unqual) {
                            names.insert(unqual.clone(), flat.clone());
                        }
                    }
                }
            let mut usages: Vec<&Item> = m
                .program
                .items
                .iter()
                .filter(|i| matches!(i, Item::Use { .. }))
                .collect();
            // Order deps deterministically: process uses in source order.
            usages.sort_by_key(|_| 0u8);
            for use_item in usages {
                if let Item::Use { path, alias, span } = use_item {
                    let Some(dep_exports) = exports.get(path) else {
                        continue;
                    };
                    match alias {
                        None => {
                            for (unqual, flat) in dep_exports {
                                if local.contains(unqual) {
                                    continue;
                                }
                                if let Some(existing) = names.get(unqual) {
                                    if existing != flat {
                                        errors.push(Diagnostic::error(
                                            6004,
                                            format!(
                                                "ambiguous import of `{unqual}` (from `{path}` and another module)"
                                            ),
                                            Some(*span),
                                        ));
                                    }
                                } else {
                                    names.insert(unqual.clone(), flat.clone());
                                }
                            }
                        }
                        Some(a) => {
                            if aliases.insert(a.clone(), path.clone()).is_some() {
                                errors.push(Diagnostic::error(
                                    6005,
                                    format!("duplicate module alias `{a}`"),
                                    Some(*span),
                                ));
                            }
                        }
                    }
                }
            }

            let mut resolver = Resolver {
                names,
                aliases,
                exports: &exports,
            };
            for mut item in m.program.items.clone() {
                if matches!(item, Item::Use { .. }) {
                    continue;
                }
                resolver.rewrite_item(&mut item);
                all_items.push(item);
            }
        }

        if !errors.is_empty() {
            return Err(std::mem::take(errors));
        }
        Ok(Program {
            module: Some("Main".to_string()),
            exports: None,
            items: all_items,
        })
    }
}

struct Resolver<'a> {
    names: HashMap<String, String>,
    aliases: HashMap<String, String>,
    exports: &'a HashMap<String, HashMap<String, String>>,
}

impl<'a> Resolver<'a> {
    fn resolve_name(&self, name: &str) -> String {
        self.names.get(name).cloned().unwrap_or_else(|| name.to_string())
    }

    fn resolve_qualified(&self, alias: &str, member: &str, span: Span) -> String {
        match self.aliases.get(alias) {
            None => {
                let _ = span;
                // Unknown alias: keep the name untouched so the type checker
                // reports an unbound-name error at the call site.
                format!("{alias}.{member}")
            }
            Some(canonical) => match self.exports.get(canonical).and_then(|e| e.get(member)) {
                Some(flat) => flat.clone(),
                None => {
                    let _ = span;
                    format!("{alias}.{member}")
                }
            },
        }
    }

    fn rewrite_item(&mut self, item: &mut Item) {
        match item {
            Item::Function(f) => {
                f.name = self.resolve_name(&f.name);
                for clause in &mut f.clauses {
                    for p in &mut clause.pats {
                        self.rewrite_pattern(p);
                    }
                    self.rewrite_expr(&mut clause.body);
                }
            }
            Item::Signature { sig, .. } => {
                sig.name = self.resolve_name(&sig.name);
                for c in &mut sig.constraints {
                    self.rewrite_type(c);
                }
                self.rewrite_type(&mut sig.body);
            }
            Item::TypeAlias { name, ty, .. } => {
                *name = self.resolve_name(name);
                self.rewrite_type(ty);
            }
            Item::Data { name, ctors, .. } => {
                *name = self.resolve_name(name);
                for c in ctors {
                    c.name = self.resolve_name(&c.name);
                    for f in &mut c.fields {
                        self.rewrite_type(f);
                    }
                    for f in &mut c.record {
                        self.rewrite_type(&mut f.ty);
                    }
                }
            }
            Item::Class { name, superclasses, methods, .. } => {
                *name = self.resolve_name(name);
                for s in superclasses {
                    self.rewrite_type(s);
                }
                for m in methods {
                    m.name = self.resolve_name(&m.name);
                    for c in &mut m.constraints {
                        self.rewrite_type(c);
                    }
                    self.rewrite_type(&mut m.body);
                }
            }
            Item::Instance { class, methods, .. } => {
                self.rewrite_type(class);
                for b in methods {
                    for p in &mut b.lhs {
                        self.rewrite_pattern(p);
                    }
                    self.rewrite_expr(&mut b.expr);
                }
            }
            Item::Use { .. } => {}
        }
    }

    fn rewrite_expr(&mut self, e: &mut Expr) {
        match e {
            Expr::Var(name, _) => *name = self.resolve_name(name),
            Expr::Ctor(name, _) => *name = self.resolve_name(name),
            Expr::Qualified(alias, member, span) => {
                let flat = self.resolve_qualified(alias, member, *span);
                let is_ctor = member
                    .chars()
                    .next()
                    .map(|c| c.is_uppercase())
                    .unwrap_or(false);
                if is_ctor {
                    *e = Expr::Ctor(flat, *span);
                } else {
                    *e = Expr::Var(flat, *span);
                }
            }
            Expr::Lambda(pats, body, _) => {
                for p in pats {
                    self.rewrite_pattern(p);
                }
                self.rewrite_expr(body);
            }
            Expr::App(f, a, _) => {
                self.rewrite_expr(f);
                self.rewrite_expr(a);
            }
            Expr::Let(bindings, body, _) => {
                for b in bindings {
                    for p in &mut b.lhs {
                        self.rewrite_pattern(p);
                    }
                    self.rewrite_expr(&mut b.expr);
                }
                self.rewrite_expr(body);
            }
            Expr::If(c, t, el, _) => {
                self.rewrite_expr(c);
                self.rewrite_expr(t);
                self.rewrite_expr(el);
            }
            Expr::Case(scrut, alts, _) => {
                self.rewrite_expr(scrut);
                for alt in alts {
                    self.rewrite_pattern(&mut alt.pat);
                    self.rewrite_expr(&mut alt.body);
                }
            }
            Expr::Tuple(es, _) | Expr::List(es, _) => {
                for x in es {
                    self.rewrite_expr(x);
                }
            }
            Expr::Assign(_, e, _) => self.rewrite_expr(e),
            Expr::Borrow { expr, .. } => self.rewrite_expr(expr),
            Expr::Deref(e, _) => self.rewrite_expr(e),
            Expr::Clone(e, _) => self.rewrite_expr(e),
            Expr::Field(e, _, _) => self.rewrite_expr(e),
            Expr::Ascribe(e, ty, _) => {
                self.rewrite_expr(e);
                self.rewrite_type(ty);
            }
            Expr::Record { ctor, fields, .. } => {
                *ctor = self.resolve_name(ctor);
                for (_, fe) in fields {
                    self.rewrite_expr(fe);
                }
            }
            Expr::Int(_, _)
            | Expr::Float(_, _)
            | Expr::Str(_, _)
            | Expr::Char(_, _)
            | Expr::Unit(_) => {}
        }
    }

    fn rewrite_pattern(&mut self, p: &mut Pattern) {
        match p {
            Pattern::Ctor { name, args, .. } => {
                *name = self.resolve_name(name);
                for a in args {
                    self.rewrite_pattern(a);
                }
            }
            Pattern::Tuple(ps, _) | Pattern::List(ps, _) => {
                for x in ps {
                    self.rewrite_pattern(x);
                }
            }
            Pattern::Cons(a, b, _) => {
                self.rewrite_pattern(a);
                self.rewrite_pattern(b);
            }
            Pattern::As(_, inner, _) => self.rewrite_pattern(inner),
            Pattern::Record { ctor, fields, .. } => {
                *ctor = self.resolve_name(ctor);
                for (_, fp) in fields {
                    self.rewrite_pattern(fp);
                }
            }
            Pattern::Var(_, _)
            | Pattern::Wildcard(_)
            | Pattern::Int(_, _)
            | Pattern::Float(_, _)
            | Pattern::Str(_, _)
            | Pattern::Char(_, _) => {}
        }
    }

    fn rewrite_type(&mut self, t: &mut Type) {
        match t {
            Type::Con(name, args, _) => {
                *name = self.resolve_name(name);
                for a in args {
                    self.rewrite_type(a);
                }
            }
            Type::Func(a, b, _) => {
                self.rewrite_type(a);
                self.rewrite_type(b);
            }
            Type::Tuple(ts, _) => {
                for x in ts {
                    self.rewrite_type(x);
                }
            }
            Type::List(inner, _) => self.rewrite_type(inner),
            Type::Ref { inner, .. } => self.rewrite_type(inner),
            Type::Var(_, _) | Type::Unit(_) => {}
        }
    }
}
