//! Native code generation via Cranelift.
//!
//! Compiles a type-checked program into a relocatable object file for later
//! linking.
//!
//! Every value is a pointer to a reference-counted heap object:
//!
//!   [refcount: i64][tag: i64][nfields: i64][payload...]
//!
//! `Int` and `Bool` are boxed with the reserved tags `TAG_INT`/`TAG_BOOL` and a
//! single unboxed payload word; `String` is `TAG_STR` (a byte-length and raw
//! NUL-terminated bytes); lists are `TAG_NIL`/`TAG_CONS` cons cells; functions
//! are `TAG_FUN` closures (arity, applied-arg count, a code pointer, and a
//! captured-args list); tuples are `TAG_TUPLE`; mutable let-bindings are
//! `TAG_CELL` boxes (the payload is the current value, so closures share the
//! same storage); and each ADT constructor uses its constructor index
//! (>= 0) as the tag, with the fields as payload. Because every value is a
//! pointer, polymorphic functions are reference-counted uniformly.
//!
//! Number formatting for `show` is implemented in the C runtime
//! (`runtime/quill-rt.c`): `snprintf` is variadic and cannot be driven through
//! Cranelift's fixed call ABI, so the helpers are plain C.
//!
//! Every compiled function takes the closure that invoked it (for reading the
//! captured environment) and a boxed list of its actual arguments, so a
//! function value can be applied generically via `quill_apply` (which curries
//! partial applications). Lambdas capture their free variables in a boxed
//! environment stored on the closure.
//!
//! Memory is reference-counted. `quill_alloc`/`quill_box` start an object at
//! refcount 1; `quill_retain` and `quill_release` adjust it, and `quill_release`
//! frees the object once the count reaches zero, first recursively releasing an
//! ADT's fields. (Cycles are unrepresentable — fields are fixed at construction
//! — so this terminates.)

use std::collections::HashMap;

use cranelift_codegen::ir::{
    AbiParam, BlockArg, InstBuilder, MemFlagsData, Signature, TrapCode, UserFuncName, Value, types,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use target_lexicon::Triple;

use crate::ast::*;
use crate::infer::{InstanceInfo, TypeInfo};
use crate::types::{Ty, TyCon};

type FuncRef = cranelift_codegen::ir::FuncRef;

/// Reserved tags for the boxed scalar types. ADT constructor tags are the
/// constructor index (>= 0) and never collide with these.
const TAG_INT: i64 = -1;
const TAG_BOOL: i64 = -2;
const TAG_STR: i64 = -3;
const TAG_NIL: i64 = -4;
const TAG_CONS: i64 = -5;
const TAG_FUN: i64 = -6;
const TAG_FLOAT: i64 = -7;
const TAG_TUPLE: i64 = -8;
/// A mutable-binding cell: a box whose payload is the current value. Reads
/// and assignments go through the payload; captures share the cell pointer.
const TAG_CELL: i64 = -9;
/// A single character; the payload word holds its (ASCII) byte value.
const TAG_CHAR: i64 = -10;

struct CodegenInfo {
    /// Constructor name -> globally unique constructor index (the object tag).
    ctors: HashMap<String, usize>,
    /// Global constructor index -> constructor name (for structural `show`).
    ctor_names: Vec<String>,
    /// Constructor name -> record field names in declaration order.
    ctor_fields: HashMap<String, Vec<String>>,
    /// Record field name -> (constructor, declaration index).
    field_index: HashMap<String, (String, usize)>,
    /// ADT name -> (first constructor index, constructor count).
    adt_ranges: HashMap<String, (usize, usize)>,
}

impl CodegenInfo {
    fn from(types: &TypeInfo) -> Self {
        let mut ctors = HashMap::new();
        let mut ctor_names = Vec::new();
        let mut ctor_fields = HashMap::new();
        let mut field_index = HashMap::new();
        let mut adt_ranges = HashMap::new();
        for adt in &types.adts {
            let first = ctor_names.len();
            for c in &adt.ctors {
                ctors.insert(c.name.clone(), ctor_names.len());
                // `show` prints the constructor without its module prefix
                // (matching the interpreter's `short_name`).
                ctor_names.push(short_name(&c.name));
                let names: Vec<String> = c.record.iter().map(|(n, _)| n.clone()).collect();
                for (i, (n, _)) in c.record.iter().enumerate() {
                    field_index.insert(n.clone(), (c.name.clone(), i));
                }
                ctor_fields.insert(c.name.clone(), names);
            }
            adt_ranges.insert(adt.name.clone(), (first, ctor_names.len() - first));
        }
        CodegenInfo {
            ctors,
            ctor_names,
            ctor_fields,
            field_index,
            adt_ranges,
        }
    }

    fn ctor_of(&self, name: &str) -> Option<usize> {
        self.ctors.get(name).copied()
    }
}

/// References to the emitted runtime functions.
#[derive(Clone, Copy)]
struct Rt {
    alloc: FuncRef,
    box_value: FuncRef,
    retain: FuncRef,
    release: FuncRef,
    str_alloc: FuncRef,
    println: FuncRef,
    print: FuncRef,
    cons: FuncRef,
    append: FuncRef,
    make_closure: FuncRef,
    apply: FuncRef,
    show: FuncRef,
    str_tail: FuncRef,
    str_eq: FuncRef,
    str_snoc: FuncRef,
    /// Reads a line from stdin as a fresh string box (0 at end of input).
    read_line: FuncRef,
}

pub fn compile_object(program: &Program, types: &TypeInfo) -> Result<Vec<u8>, String> {
    let info = CodegenInfo::from(types);
    let mut flag_builder = settings::builder();
    flag_builder.set("use_colocated_libcalls", "false").unwrap();
    // PIC keeps calls to imported libc functions (malloc/free) indirect, which
    // the macOS linker requires (absolute references would be "illegal text
    // relocations").
    flag_builder.set("is_pic", "true").unwrap();
    let flags = settings::Flags::new(flag_builder);
    let triple = Triple::host();
    let isa = cranelift_codegen::isa::lookup(triple)
        .map_err(|e| format!("unsupported target: {e}"))?
        .finish(flags)
        .map_err(|e| format!("target setup failed: {e}"))?;
    let builder = ObjectBuilder::new(
        isa,
        "quill".to_string(),
        cranelift_module::default_libcall_names(),
    )
    .map_err(|e| format!("object builder: {e}"))?;
    let module = ObjectModule::new(builder);

    let mut c = Compiler {
        module,
        arities: HashMap::new(),
        lambda_info: HashMap::new(),
        info,
        method_dispatch: HashMap::new(),
        instances: types.instances.clone(),
        adts: types.adts.clone(),
    };
    c.collect_signatures(program)?;
    c.compile_program(program)
}

/// A compiled lambda: its generated function name and the enclosing-scope
/// variables it captures.
struct LambdaInfo {
    name: String,
    free_vars: Vec<String>,
}

struct Compiler {
    module: ObjectModule,
    arities: HashMap<String, usize>,
    /// Maps a lambda's span (keyed by `span.lo`) to its generated function.
    lambda_info: HashMap<usize, LambdaInfo>,
    info: CodegenInfo,
    /// Class method name -> instance implementations (constructor indices of
    /// the instance head + the compiled method function).
    method_dispatch: HashMap<String, Vec<(Vec<usize>, cranelift_module::FuncId)>>,
    instances: Vec<InstanceInfo>,
    adts: Vec<crate::infer::AdtDef>,
}

/// Imported libc/C-runtime helpers handed to `emit_runtime`.
struct RtImports {
    malloc: cranelift_module::FuncId,
    free: cranelift_module::FuncId,
    memcpy: cranelift_module::FuncId,
    puts: cranelift_module::FuncId,
    strlen: cranelift_module::FuncId,
    itoa: cranelift_module::FuncId,
    ftoa: cranelift_module::FuncId,
    write: cranelift_module::FuncId,
    read_line_raw: cranelift_module::FuncId,
}

impl Compiler {
    fn collect_signatures(&mut self, program: &Program) -> Result<(), String> {
        for item in &program.items {
            if let Item::Function(f) = item {
                let arity = f.clauses.first().map(|c| c.pats.len()).unwrap_or(0);
                self.arities.insert(f.name.clone(), arity);
            }
        }
        Ok(())
    }

    fn compile_program(mut self, program: &Program) -> Result<Vec<u8>, String> {
        // Import libc helpers (linked by `cc`).
        let malloc = self.declare_import("malloc", &mut [types::I64], &mut [types::I64])?;
        let free = self.declare_import("free", &mut [types::I64], &mut [])?;
        let memcpy = self.declare_import("memcpy", &mut [types::I64, types::I64, types::I64], &mut [types::I64])?;
        let puts = self.declare_import("puts", &mut [types::I64], &mut [types::I64])?;
        let strlen = self.declare_import("strlen", &mut [types::I64], &mut [types::I64])?;
        // `print` streams bytes straight to stdout via write(2).
        let write_import = self.declare_import(
            "write",
            &mut [types::I64, types::I64, types::I64],
            &mut [types::I64],
        )?;
        // Number formatting lives in the C runtime (runtime/quill-rt.c): it
        // needs variadic `snprintf`, which Cranelift's fixed ABI cannot drive.
        let itoa = self.declare_import("quill_itoa", &mut [types::I64], &mut [types::I64])?;
        let ftoa = self.declare_import("quill_ftoa", &mut [types::I64], &mut [types::I64])?;
        // Line-at-a-time stdin reads also live in the C runtime (getline(3)).
        let read_line_raw = self.declare_import("quill_read_line_raw", &mut [], &mut [types::I64])?;

        self.emit_runtime(RtImports {
            malloc,
            free,
            memcpy,
            puts,
            strlen,
            itoa,
            ftoa,
            write: write_import,
            read_line_raw,
        })?;

        let mut func_ids: HashMap<String, cranelift_module::FuncId> = HashMap::new();
        for item in &program.items {
            if let Item::Function(f) = item {
                let sym = format!("q_{}", f.name);
                let id = self
                    .module
                    .declare_function(&sym, Linkage::Local, &fn_sig(2))
                    .map_err(|e| format!("declare {}: {e}", f.name))?;
                func_ids.insert(f.name.clone(), id);
            }
        }

        // Collect every lambda (with its captured variables), assign it a
        // generated name, and declare it.
        let mut raw_lambdas: Vec<(usize, Vec<Pattern>, Box<Expr>, crate::error::Span)> =
            Vec::new();
        for item in &program.items {
            if let Item::Function(f) = item {
                for clause in &f.clauses {
                    collect_lambdas(&clause.body, &mut raw_lambdas);
                }
            }
        }
        let mut lambda_fns: Vec<(Function, Vec<String>, std::collections::HashSet<String>)> =
            Vec::new();
        for (i, (span_lo, params, body, span)) in raw_lambdas.iter().enumerate() {
            let name = format!("q_lambda_{i}");
            // Captured variables are the free variables that resolve to the
            // enclosing scope's locals — i.e. not builtins, constructors, or
            // top-level functions (those resolve statically).
            let free_vars: Vec<String> = lambda_free_vars(params, body)
                .into_iter()
                .filter(|n| {
                    !is_builtin(n)
                        && self.info.ctor_of(n).is_none()
                        && !self.arities.contains_key(n)
                })
                .collect();
            // Captured variables the body mutates must be cells so the
            // mutation is shared with the enclosing scope.
            let assigned = assigned_names(body);
            let cell_vars: std::collections::HashSet<String> = free_vars
                .iter()
                .filter(|n| assigned.contains(*n))
                .cloned()
                .collect();
            self.lambda_info.insert(
                *span_lo,
                LambdaInfo {
                    name: name.clone(),
                    free_vars: free_vars.clone(),
                },
            );
            self.arities.insert(name.clone(), params.len());
            let id = self
                .module
                .declare_function(&name, Linkage::Local, &fn_sig(2))
                .map_err(|e| format!("declare {name}: {e}"))?;
            func_ids.insert(name.clone(), id);
            lambda_fns.push((
                Function {
                    name,
                    clauses: vec![Clause {
                        pats: params.clone(),
                        body: (**body).clone(),
                        span: *span,
                    }],
                },
                free_vars,
                cell_vars,
            ));
        }

        // Compile every instance method as a standalone function and record
        // the class-method dispatch: method name -> (head's constructor
        // indices, function). Method calls dispatch on the runtime tag of the
        // first argument, mirroring the interpreter. Builtin instances (Int,
        // Float, …) have no method bodies and are skipped; user instances
        // follow them in `TypeInfo.instances`, in program order.
        let mut inst_fns: Vec<(Function, Vec<String>, std::collections::HashSet<String>)> =
            Vec::new();
        let mut inst_count = 0usize;
        for inst in self.instances.clone().iter() {
            if inst.methods.is_empty() {
                continue;
            }
            inst_count += 1;
            // The instance head must be an ADT: dispatch is by constructor
            // tag. Builtin heads (Int, Float, …) carry no user methods.
            let adt_id = match &inst.head {
                Ty::Con(TyCon::Adt(id), _) => *id,
                other => {
                    return Err(format!(
                        "native compiler: instance on unsupported head {other:?}"
                    ));
                }
            };
            let adt_name = self
                .adts
                .get(adt_id)
                .map(|a| a.name.clone())
                .unwrap_or_default();
            let (first, count) = self
                .info
                .adt_ranges
                .get(&adt_name)
                .copied()
                .unwrap_or((0, 0));
            let idx_list: Vec<usize> = (first..first + count).collect();
            // Each method has one Binding per clause; group them by name into
            // a single multi-clause function.
            let mut clauses_by_name: Vec<(String, Vec<Clause>)> = Vec::new();
            for m in inst.methods.iter() {
                let mname = m
                    .name()
                    .ok_or_else(|| "instance method without a name".to_string())?
                    .to_string();
                let (pats, body, mspan) = match &m.expr {
                    Expr::Lambda(pats, body, span) => {
                        (pats.clone(), (**body).clone(), *span)
                    }
                    other => {
                        return Err(format!(
                            "instance method `{mname}` is not a lambda: {other:?}"
                        ));
                    }
                };
                let entry = match clauses_by_name.iter_mut().find(|(n, _)| *n == mname) {
                    Some(e) => e,
                    None => {
                        clauses_by_name.push((mname.clone(), Vec::new()));
                        clauses_by_name.last_mut().unwrap()
                    }
                };
                entry.1.push(Clause {
                    pats,
                    body,
                    span: mspan,
                });
            }
            for (k, (mname, clauses)) in clauses_by_name.into_iter().enumerate() {
                let name = format!("q_inst_{inst_count}_{k}");
                let arity = clauses.first().map(|c| c.pats.len()).unwrap_or(0);
                let free_vars: Vec<String> = clauses
                    .iter()
                    .flat_map(|c| lambda_free_vars(&c.pats, &c.body))
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .filter(|n| {
                        !is_builtin(n)
                            && self.info.ctor_of(n).is_none()
                            && !self.arities.contains_key(n)
                    })
                    .collect();
                let cell_vars: std::collections::HashSet<String> = free_vars
                    .iter()
                    .filter(|n| {
                        clauses
                            .iter()
                            .any(|c| assigned_names(&c.body).contains(*n))
                    })
                    .cloned()
                    .collect();
                for c in &clauses {
                    self.lambda_info.insert(
                        c.span.lo,
                        LambdaInfo {
                            name: name.clone(),
                            free_vars: free_vars.clone(),
                        },
                    );
                }
                self.arities.insert(name.clone(), arity);
                let id = self
                    .module
                    .declare_function(&name, Linkage::Local, &fn_sig(2))
                    .map_err(|e| format!("declare {name}: {e}"))?;
                func_ids.insert(name.clone(), id);
                inst_fns.push((Function { name, clauses }, free_vars, cell_vars));
                self.method_dispatch
                    .entry(mname)
                    .or_default()
                    .push((idx_list.clone(), id));
            }

            // Nested lambdas inside the instance's method bodies are compiled
            // like top-level lambdas (own function, captured free variables).
            let mut raw_inst_lambdas: Vec<(usize, Vec<Pattern>, Box<Expr>, crate::error::Span)> =
                Vec::new();
            for (f, _, _) in &inst_fns {
                for c in &f.clauses {
                    collect_lambdas(&c.body, &mut raw_inst_lambdas);
                }
            }
            for (i, (span_lo, params, body, span)) in raw_inst_lambdas.iter().enumerate() {
                let name = format!("q_instlam_{inst_count}_{i}");
                let free_vars: Vec<String> = lambda_free_vars(params, body)
                    .into_iter()
                    .filter(|n| {
                        !is_builtin(n)
                            && self.info.ctor_of(n).is_none()
                            && !self.arities.contains_key(n)
                    })
                    .collect();
                let assigned = assigned_names(body);
                let cell_vars: std::collections::HashSet<String> = free_vars
                    .iter()
                    .filter(|n| assigned.contains(*n))
                    .cloned()
                    .collect();
                self.lambda_info.insert(
                    *span_lo,
                    LambdaInfo {
                        name: name.clone(),
                        free_vars: free_vars.clone(),
                    },
                );
                self.arities.insert(name.clone(), params.len());
                let id = self
                    .module
                    .declare_function(&name, Linkage::Local, &fn_sig(2))
                    .map_err(|e| format!("declare {name}: {e}"))?;
                func_ids.insert(name.clone(), id);
                inst_fns.push((
                    Function {
                        name,
                        clauses: vec![Clause {
                            pats: params.clone(),
                            body: (**body).clone(),
                            span: *span,
                        }],
                    },
                    free_vars,
                    cell_vars,
                ));
            }
        }

        let items: Vec<Item> = program.items.clone();
        for item in &items {
            if let Item::Function(f) = item {
                let id = func_ids[&f.name];
                let arity = self.arities[&f.name];
                let no_cells = std::collections::HashSet::new();
                self.define_function(id, f, arity, &[], &no_cells, &func_ids)?;
            }
        }
        for (lambda_fn, free_vars, cell_vars) in &lambda_fns {
            let id = func_ids[&lambda_fn.name];
            let arity = self.arities[&lambda_fn.name];
            self.define_function(id, lambda_fn, arity, free_vars, cell_vars, &func_ids)?;
        }
        for (inst_fn, free_vars, cell_vars) in &inst_fns {
            let id = func_ids[&inst_fn.name];
            let arity = self.arities[&inst_fn.name];
            self.define_function(id, inst_fn, arity, free_vars, cell_vars, &func_ids)?;
        }

        if let Some(main_id) = func_ids.get("main").cloned() {
            self.define_entry(main_id)?;
        } else {
            self.define_trivial_entry()?;
        }

        let product = self.module.finish();
        product.emit().map_err(|e| format!("emit object: {e}"))
    }

    fn declare_import(
        &mut self,
        name: &str,
        params: &mut [types::Type],
        returns: &mut [types::Type],
    ) -> Result<cranelift_module::FuncId, String> {
        let mut sig = Signature::new(CallConv::SystemV);
        for p in params.iter() {
            sig.params.push(AbiParam::new(*p));
        }
        for r in returns.iter() {
            sig.returns.push(AbiParam::new(*r));
        }
        self.module
            .declare_function(name, Linkage::Import, &sig)
            .map_err(|e| format!("declare {name}: {e}"))
    }

    fn emit_runtime(&mut self, imports: RtImports) -> Result<(), String> {
        let RtImports {
            malloc,
            free,
            memcpy,
            puts,
            strlen,
            itoa,
            ftoa,
            write: write_import,
            read_line_raw,
        } = imports;
        // quill_alloc(tag, nfields) -> ptr
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(2);
            let id = self
                .module
                .declare_function("quill_alloc", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_alloc: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_alloc");
            ctx.func.signature = sig;
            let malloc_ref = self.module.declare_func_in_func(malloc, &mut ctx.func);
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let params = bcx.block_params(block).to_vec();
                // Header is [refcount][tag][nfields], then `nfields` payload
                // words — 3 extra words beyond the fields.
                let words = bcx.ins().iadd_imm_s(params[1], 3);
                let size = bcx.ins().imul_imm_s(words, 8);
                let call = bcx.ins().call(malloc_ref, &[size]);
                let p = bcx.inst_results(call)[0];
                let one = bcx.ins().iconst(types::I64, 1);
                bcx.ins().store(MemFlagsData::new(), one, p, 0);
                bcx.ins().store(MemFlagsData::new(), params[0], p, 8);
                bcx.ins().store(MemFlagsData::new(), params[1], p, 16);
                bcx.ins().return_(&[p]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_alloc: {e}"))?;
        }

        // quill_box(tag, value) -> ptr — a single-payload box (Int/Bool).
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(2);
            let id = self
                .module
                .declare_function("quill_box", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_box: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_box");
            ctx.func.signature = sig;
            let alloc_ref = self.declare_rt_in(&mut ctx, "quill_alloc", &plain_sig(2));
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let params = bcx.block_params(block).to_vec();
                let one = bcx.ins().iconst(types::I64, 1);
                let call = bcx.ins().call(alloc_ref, &[params[0], one]);
                let p = bcx.inst_results(call)[0];
                bcx.ins().store(MemFlagsData::new(), params[1], p, 24);
                bcx.ins().return_(&[p]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_box: {e}"))?;
        }

        // quill_str_alloc(len) -> ptr — a zero-filled, NUL-terminated string
        // box with room for `len` bytes of content.
        {
            let target_config = self.module.target_config();
            let mut sig = Signature::new(CallConv::SystemV);
            sig.params.push(AbiParam::new(types::I64));
            sig.returns.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("quill_str_alloc", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_str_alloc: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_str_alloc");
            ctx.func.signature = sig;
            let malloc_ref = self.module.declare_func_in_func(malloc, &mut ctx.func);
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let len = bcx.block_params(block)[0];
                // nwords = (len + 8) / 8 == ceil((len + 1) / 8): enough words
                // for the content plus the NUL terminator.
                let len8 = bcx.ins().iadd_imm_s(len, 8);
                let nwords = bcx.ins().udiv_imm_s(len8, 8);
                let words = bcx.ins().iadd_imm_s(nwords, 3);
                let size = bcx.ins().imul_imm_s(words, 8);
                let call = bcx.ins().call(malloc_ref, &[size]);
                let p = bcx.inst_results(call)[0];
                let one = bcx.ins().iconst(types::I64, 1);
                bcx.ins().store(MemFlagsData::new(), one, p, 0);
                let str_tag = bcx.ins().iconst(types::I64, TAG_STR);
                bcx.ins().store(MemFlagsData::new(), str_tag, p, 8);
                bcx.ins().store(MemFlagsData::new(), len, p, 16);
                // Zero-fill the payload so it is NUL-terminated.
                let zero = bcx.ins().iconst(types::I64, 0);
                let loop_h = bcx.create_block();
                let iv = bcx.append_block_param(loop_h, types::I64);
                bcx.ins().jump(loop_h, &[BlockArg::Value(zero)]);
                bcx.switch_to_block(loop_h);
                let cond = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::SignedLessThan,
                    iv,
                    nwords,
                );
                let loop_body = bcx.create_block();
                let loop_done = bcx.create_block();
                bcx.ins().brif(cond, loop_body, &[], loop_done, &[]);
                bcx.switch_to_block(loop_body);
                let word_off = bcx.ins().imul_imm_s(iv, 8);
                let offset = bcx.ins().iadd_imm_s(word_off, 24);
                let addr = bcx.ins().iadd(p, offset);
                bcx.ins().store(MemFlagsData::new(), zero, addr, 0);
                let next = bcx.ins().iadd_imm_s(iv, 1);
                bcx.ins().jump(loop_h, &[BlockArg::Value(next)]);
                bcx.switch_to_block(loop_done);
                bcx.ins().return_(&[p]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_str_alloc: {e}"))?;
        }

        // quill_str_concat(a, b) -> ptr — a new box with a's bytes followed by
        // b's bytes.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(2);
            let id = self
                .module
                .declare_function("quill_str_concat", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_str_concat: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_str_concat");
            ctx.func.signature = sig;
            let alloc_ref = self.declare_rt_in(&mut ctx, "quill_str_alloc", &plain_sig(1));
            let memcpy_ref = self.module.declare_func_in_func(memcpy, &mut ctx.func);
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let params = bcx.block_params(block).to_vec();
                let (a, b) = (params[0], params[1]);
                let lena = bcx.ins().load(types::I64, MemFlagsData::new(), a, 16);
                let lenb = bcx.ins().load(types::I64, MemFlagsData::new(), b, 16);
                let total = bcx.ins().iadd(lena, lenb);
                let call = bcx.ins().call(alloc_ref, &[total]);
                let p = bcx.inst_results(call)[0];
                let twenty4 = bcx.ins().iconst(types::I64, 24);
                let dst = bcx.ins().iadd(p, twenty4);
                let src_a = bcx.ins().iadd(a, twenty4);
                bcx.ins().call(memcpy_ref, &[dst, src_a, lena]);
                let src_b_off = bcx.ins().iadd(twenty4, lena);
                let dst_b = bcx.ins().iadd(p, src_b_off);
                let src_b = bcx.ins().iadd(b, twenty4);
                bcx.ins().call(memcpy_ref, &[dst_b, src_b, lenb]);
                bcx.ins().return_(&[p]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_str_concat: {e}"))?;
        }

        // quill_println(s) — write the string's bytes and a newline to stdout.
        {
            let target_config = self.module.target_config();
            let mut sig = Signature::new(CallConv::SystemV);
            sig.params.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("quill_println", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_println: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_println");
            ctx.func.signature = sig;
            let puts_ref = self.module.declare_func_in_func(puts, &mut ctx.func);
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let p = bcx.block_params(block)[0];
                // `println` currently only formats strings; trap on any other
                // value rather than misreading its payload as a C string.
                let tag = bcx.ins().load(types::I64, MemFlagsData::new(), p, 8);
                let str_tag = bcx.ins().iconst(types::I64, TAG_STR);
                let is_str = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    str_tag,
                );
                let ok = bcx.create_block();
                let bad = bcx.create_block();
                bcx.ins().brif(is_str, ok, &[], bad, &[]);
                bcx.switch_to_block(bad);
                bcx.ins().trap(TrapCode::unwrap_user(3));
                bcx.switch_to_block(ok);
                let data = bcx.ins().iadd_imm_s(p, 24);
                bcx.ins().call(puts_ref, &[data]);
                bcx.ins().return_(&[]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_println: {e}"))?;
        }

        // quill_print(s) — write the string's bytes to stdout (no newline),
        // so `.` output appears as it is produced.
        {
            let target_config = self.module.target_config();
            let mut sig = Signature::new(CallConv::SystemV);
            sig.params.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("quill_print", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_print: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_print");
            ctx.func.signature = sig;
            let write_ref = self.module.declare_func_in_func(write_import, &mut ctx.func);
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let p = bcx.block_params(block)[0];
                // Trap on non-strings, like quill_println.
                let tag = bcx.ins().load(types::I64, MemFlagsData::new(), p, 8);
                let str_tag = bcx.ins().iconst(types::I64, TAG_STR);
                let is_str = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    str_tag,
                );
                let ok = bcx.create_block();
                let bad = bcx.create_block();
                bcx.ins().brif(is_str, ok, &[], bad, &[]);
                bcx.switch_to_block(bad);
                bcx.ins().trap(TrapCode::unwrap_user(3));
                bcx.switch_to_block(ok);
                let len = bcx.ins().load(types::I64, MemFlagsData::new(), p, 16);
                let data = bcx.ins().iadd_imm_s(p, 24);
                let one = bcx.ins().iconst(types::I64, 1);
                bcx.ins().call(write_ref, &[one, data, len]);
                bcx.ins().return_(&[]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_print: {e}"))?;
        }

        // quill_cons(head, tail) -> ptr — a list cons cell.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(2);
            let id = self
                .module
                .declare_function("quill_cons", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_cons: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_cons");
            ctx.func.signature = sig;
            let alloc_ref = self.declare_rt_in(&mut ctx, "quill_alloc", &plain_sig(2));
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let params = bcx.block_params(block).to_vec();
                let cons_tag = bcx.ins().iconst(types::I64, TAG_CONS);
                let two = bcx.ins().iconst(types::I64, 2);
                let call = bcx.ins().call(alloc_ref, &[cons_tag, two]);
                let p = bcx.inst_results(call)[0];
                bcx.ins().store(MemFlagsData::new(), params[0], p, 24);
                bcx.ins().store(MemFlagsData::new(), params[1], p, 32);
                bcx.ins().return_(&[p]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_cons: {e}"))?;
        }

        // quill_list_append(a, b) -> ptr — a new list with a's elements
        // followed by b's.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(2);
            let id = self
                .module
                .declare_function("quill_list_append", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_list_append: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_list_append");
            ctx.func.signature = sig;
            let cons_ref = self.declare_rt_in(&mut ctx, "quill_cons", &plain_sig(2));
            let self_ref = self.declare_rt_in(&mut ctx, "quill_list_append", &plain_sig(2));
            let retain_ref = self.declare_rt_in(&mut ctx, "quill_retain", &unit_sig(1));
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let params = bcx.block_params(block).to_vec();
                let (a, b) = (params[0], params[1]);
                let tag = bcx.ins().load(types::I64, MemFlagsData::new(), a, 8);
                let nil_tag = bcx.ins().iconst(types::I64, TAG_NIL);
                let is_nil = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    nil_tag,
                );
                let nil_block = bcx.create_block();
                let cons_block = bcx.create_block();
                bcx.ins().brif(is_nil, nil_block, &[], cons_block, &[]);
                bcx.switch_to_block(nil_block);
                bcx.ins().call(retain_ref, &[b]);
                bcx.ins().return_(&[b]);
                bcx.switch_to_block(cons_block);
                let head = bcx.ins().load(types::I64, MemFlagsData::new(), a, 24);
                let tail = bcx.ins().load(types::I64, MemFlagsData::new(), a, 32);
                // The new list shares `head`, so retain it before storing.
                bcx.ins().call(retain_ref, &[head]);
                let call = bcx.ins().call(self_ref, &[tail, b]);
                let new_tail = bcx.inst_results(call)[0];
                let call2 = bcx.ins().call(cons_ref, &[head, new_tail]);
                let result = bcx.inst_results(call2)[0];
                bcx.ins().return_(&[result]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_list_append: {e}"))?;
        }

        // quill_append(a, b) -> ptr — dispatch `++` on the tag of `a`.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(2);
            let id = self
                .module
                .declare_function("quill_append", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_append: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_append");
            ctx.func.signature = sig;
            let str_concat_ref = self.declare_rt_in(&mut ctx, "quill_str_concat", &plain_sig(2));
            let list_append_ref = self.declare_rt_in(&mut ctx, "quill_list_append", &plain_sig(2));
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let params = bcx.block_params(block).to_vec();
                let (a, b) = (params[0], params[1]);
                let tag = bcx.ins().load(types::I64, MemFlagsData::new(), a, 8);
                let str_tag = bcx.ins().iconst(types::I64, TAG_STR);
                let is_str = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    str_tag,
                );
                let str_block = bcx.create_block();
                let list_block = bcx.create_block();
                bcx.ins().brif(is_str, str_block, &[], list_block, &[]);
                bcx.switch_to_block(str_block);
                let call = bcx.ins().call(str_concat_ref, &[a, b]);
                let r = bcx.inst_results(call)[0];
                bcx.ins().return_(&[r]);
                bcx.switch_to_block(list_block);
                let call2 = bcx.ins().call(list_append_ref, &[a, b]);
                let r2 = bcx.inst_results(call2)[0];
                bcx.ins().return_(&[r2]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_append: {e}"))?;
        }

        // quill_make_closure(arity, nargs, code_ptr, args_list, env) -> closure.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(5);
            let id = self
                .module
                .declare_function("quill_make_closure", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_make_closure: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_make_closure");
            ctx.func.signature = sig;
            let alloc_ref = self.declare_rt_in(&mut ctx, "quill_alloc", &plain_sig(2));
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let params = bcx.block_params(block).to_vec();
                let fun_tag = bcx.ins().iconst(types::I64, TAG_FUN);
                let five = bcx.ins().iconst(types::I64, 5);
                let call = bcx.ins().call(alloc_ref, &[fun_tag, five]);
                let p = bcx.inst_results(call)[0];
                bcx.ins().store(MemFlagsData::new(), params[0], p, 24); // arity
                bcx.ins().store(MemFlagsData::new(), params[1], p, 32); // nargs
                bcx.ins().store(MemFlagsData::new(), params[2], p, 40); // code_ptr
                bcx.ins().store(MemFlagsData::new(), params[3], p, 48); // args_list
                bcx.ins().store(MemFlagsData::new(), params[4], p, 56); // env
                bcx.ins().return_(&[p]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_make_closure: {e}"))?;
        }

        // quill_apply(f, a) -> result-or-closure. Prepend `a` to the closure's
        // captured args; call the entry when fully applied, otherwise return a
        // new closure with one more captured argument.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(2);
            let id = self
                .module
                .declare_function("quill_apply", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_apply: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_apply");
            ctx.func.signature = sig;
            let cons_ref = self.declare_rt_in(&mut ctx, "quill_cons", &plain_sig(2));
            let make_ref = self.declare_rt_in(&mut ctx, "quill_make_closure", &plain_sig(5));
            let retain_ref = self.declare_rt_in(&mut ctx, "quill_retain", &unit_sig(1));
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let entry_sig = bcx.import_signature(fn_sig(2));
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let params = bcx.block_params(block).to_vec();
                let (f, a) = (params[0], params[1]);
                let arity = bcx.ins().load(types::I64, MemFlagsData::new(), f, 24);
                let nargs = bcx.ins().load(types::I64, MemFlagsData::new(), f, 32);
                let code_ptr = bcx.ins().load(types::I64, MemFlagsData::new(), f, 40);
                let args_list = bcx.ins().load(types::I64, MemFlagsData::new(), f, 48);
                let env = bcx.ins().load(types::I64, MemFlagsData::new(), f, 56);
                // The new list shares `args_list` as its tail, so retain it
                // (the closure still holds its own reference).
                bcx.ins().call(retain_ref, &[args_list]);
                let call = bcx.ins().call(cons_ref, &[a, args_list]);
                let new_list = bcx.inst_results(call)[0];
                let one = bcx.ins().iconst(types::I64, 1);
                let nargs2 = bcx.ins().iadd(nargs, one);
                let full = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    nargs2,
                    arity,
                );
                let call_block = bcx.create_block();
                let partial_block = bcx.create_block();
                bcx.ins().brif(full, call_block, &[], partial_block, &[]);
                bcx.switch_to_block(call_block);
                // Pass the closure (for its captured environment) and the args.
                let call2 = bcx.ins().call_indirect(entry_sig, code_ptr, &[f, new_list]);
                let r = bcx.inst_results(call2)[0];
                bcx.ins().return_(&[r]);
                bcx.switch_to_block(partial_block);
                // The new closure shares `env`, so retain it before storing.
                bcx.ins().call(retain_ref, &[env]);
                let call3 = bcx
                    .ins()
                    .call(make_ref, &[arity, nargs2, code_ptr, new_list, env]);
                let r2 = bcx.inst_results(call3)[0];
                bcx.ins().return_(&[r2]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_apply: {e}"))?;
        }

        // quill_retain(p) — increment the refcount.
        {
            let target_config = self.module.target_config();
            let mut sig = Signature::new(CallConv::SystemV);
            sig.params.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("quill_retain", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_retain: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_retain");
            ctx.func.signature = sig;
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let p = bcx.block_params(block)[0];
                let rc = bcx.ins().load(types::I64, MemFlagsData::new(), p, 0);
                let one = bcx.ins().iconst(types::I64, 1);
                let new_rc = bcx.ins().iadd(rc, one);
                bcx.ins().store(MemFlagsData::new(), new_rc, p, 0);
                bcx.ins().return_(&[]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_retain: {e}"))?;
        }

        // quill_release(p) — decrement the refcount; when it reaches zero,
        // release each field of an ADT (recursively) and free the object. The
        // single payload word of an Int/Bool box is not a pointer and is not
        // released.
        {
            let target_config = self.module.target_config();
            let mut sig = Signature::new(CallConv::SystemV);
            sig.params.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("quill_release", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_release: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_release");
            ctx.func.signature = sig;
            let free_ref = self.module.declare_func_in_func(free, &mut ctx.func);
            let release_ref = self.declare_rt_in(&mut ctx, "quill_release", &unit_sig(1));
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let p = bcx.block_params(block)[0];

                let rc = bcx.ins().load(types::I64, MemFlagsData::new(), p, 0);
                let one = bcx.ins().iconst(types::I64, 1);
                let new_rc = bcx.ins().isub(rc, one);
                bcx.ins().store(MemFlagsData::new(), new_rc, p, 0);

                let zero = bcx.ins().iconst(types::I64, 0);
                let dead = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    new_rc,
                    zero,
                );
                let reclaim = bcx.create_block();
                let done = bcx.create_block();
                bcx.ins().brif(dead, reclaim, &[], done, &[]);
                bcx.switch_to_block(reclaim);

                // Int/Bool/Float/String boxes have a scalar/byte payload; ADTs
                // have pointer fields. Only release the payload of an ADT.
                let tag = bcx.ins().load(types::I64, MemFlagsData::new(), p, 8);
                let int_tag = bcx.ins().iconst(types::I64, TAG_INT);
                let bool_tag = bcx.ins().iconst(types::I64, TAG_BOOL);
                let str_tag = bcx.ins().iconst(types::I64, TAG_STR);
                let float_tag = bcx.ins().iconst(types::I64, TAG_FLOAT);
                let is_int = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    int_tag,
                );
                let is_bool = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    bool_tag,
                );
                let is_str = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    str_tag,
                );
                let is_float = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    float_tag,
                );
                let char_tag = bcx.ins().iconst(types::I64, TAG_CHAR);
                let is_char = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    char_tag,
                );
                let is_num = bcx.ins().bor(is_int, is_bool);
                let is_num = bcx.ins().bor(is_num, is_float);
                let is_num = bcx.ins().bor(is_num, is_char);
                let is_scalar = bcx.ins().bor(is_num, is_str);
                let release_fields = bcx.create_block();
                let just_free = bcx.create_block();
                // A closure (TAG_FUN) has exactly one heap field to release:
                // its args list at offset 48 (arity/nargs/code_ptr are not
                // pointers).
                let fun_tag = bcx.ins().iconst(types::I64, TAG_FUN);
                let is_fun = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    fun_tag,
                );
                let fun_block = bcx.create_block();
                let not_fun = bcx.create_block();
                bcx.ins().brif(is_fun, fun_block, &[], not_fun, &[]);
                bcx.switch_to_block(fun_block);
                let args_list = bcx.ins().load(types::I64, MemFlagsData::new(), p, 48);
                let env = bcx.ins().load(types::I64, MemFlagsData::new(), p, 56);
                bcx.ins().call(release_ref, &[args_list]);
                bcx.ins().call(release_ref, &[env]);
                bcx.ins().jump(just_free, &[]);
                bcx.switch_to_block(not_fun);
                bcx.ins().brif(is_scalar, just_free, &[], release_fields, &[]);
                bcx.switch_to_block(release_fields);

                // for i in 0..nfields: quill_release(field[i]).
                let nfields = bcx.ins().load(types::I64, MemFlagsData::new(), p, 16);
                let loop_h = bcx.create_block();
                let iv = bcx.append_block_param(loop_h, types::I64);
                bcx.ins().jump(loop_h, &[BlockArg::Value(zero)]);
                bcx.switch_to_block(loop_h);
                let cond = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::SignedLessThan,
                    iv,
                    nfields,
                );
                let loop_body = bcx.create_block();
                let loop_done = bcx.create_block();
                bcx.ins().brif(cond, loop_body, &[], loop_done, &[]);
                bcx.switch_to_block(loop_body);
                let word_off = bcx.ins().imul_imm_s(iv, 8);
                let offset = bcx.ins().iadd_imm_s(word_off, 24);
                let addr = bcx.ins().iadd(p, offset);
                let field = bcx.ins().load(types::I64, MemFlagsData::new(), addr, 0);
                bcx.ins().call(release_ref, &[field]);
                let next = bcx.ins().iadd_imm_s(iv, 1);
                bcx.ins().jump(loop_h, &[BlockArg::Value(next)]);
                bcx.switch_to_block(loop_done);
                bcx.ins().jump(just_free, &[]);
                bcx.switch_to_block(just_free);

                bcx.ins().call(free_ref, &[p]);
                bcx.ins().jump(done, &[]);
                bcx.switch_to_block(done);
                bcx.ins().return_(&[]);

                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_release: {e}"))?;
        }

        // ==================== `show` runtime ====================
        //
        // Static read-only data: printf formats, literal strings, and the
        // constructor-name table (`quill_ctor_table` maps a constructor index
        // to a NUL-terminated name). `quill_show` walks a value structurally
        // and produces a boxed String in the interpreter's `format_value`
        // style (`Some 42`, `[1, 2]`, `6.0`, `true`, …). Records are formatted
        // like positional constructors (field names are not preserved by the
        // native representation).

        let s_true_id = self.declare_bytes("quill_s_true", b"true\0");
        let s_false_id = self.declare_bytes("quill_s_false", b"false\0");
        let s_open_id = self.declare_bytes("quill_s_open", b"[\0");
        let s_paren_id = self.declare_bytes("quill_s_paren", b"(\0");
        let s_closep_id = self.declare_bytes("quill_s_closep", b")\0");
        let s_close_id = self.declare_bytes("quill_s_close", b"]\0");
        let s_comma_id = self.declare_bytes("quill_s_comma", b", \0");
        let s_space_id = self.declare_bytes("quill_s_space", b" \0");
        let s_nil_id = self.declare_bytes("quill_s_nil", b"[]\0");
        let s_fun_id = self.declare_bytes("quill_s_fun", b"<function>\0");
        let s_quote_id = self.declare_bytes("quill_s_quote", b"'\0");

        let ctor_names = self.info.ctor_names.clone();
        let mut ctor_name_ids = Vec::new();
        for (i, name) in ctor_names.iter().enumerate() {
            let mut bytes = name.clone().into_bytes();
            bytes.push(0);
            ctor_name_ids.push(self.declare_bytes(&format!("quill_ctor_{i}"), &bytes));
        }
        let ctor_table_id = self
            .module
            .declare_data("quill_ctor_table", Linkage::Local, false, false)
            .map_err(|e| format!("declare quill_ctor_table: {e}"))?;
        {
            let mut desc = cranelift_module::DataDescription::new();
            desc.define(vec![0u8; ctor_name_ids.len() * 8].into_boxed_slice());
            // The table holds 8-byte pointers, so it must be pointer-aligned.
            desc.set_align(8);
            for (i, nid) in ctor_name_ids.iter().enumerate() {
                let gv = self.module.declare_data_in_data(*nid, &mut desc);
                desc.write_data_addr((i * 8) as u32, gv, 0);
            }
            self.module
                .define_data(ctor_table_id, &desc)
                .map_err(|e| format!("define quill_ctor_table: {e}"))?;
        }

        // quill_str_from_bytes(ptr, len) -> str — a fresh string box copying
        // `len` bytes from `ptr`.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(2);
            let id = self
                .module
                .declare_function("quill_str_from_bytes", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_str_from_bytes: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_str_from_bytes");
            ctx.func.signature = sig;
            let alloc_ref = self.declare_rt_in(&mut ctx, "quill_str_alloc", &plain_sig(1));
            let memcpy_ref = self.module.declare_func_in_func(memcpy, &mut ctx.func);
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let params = bcx.block_params(block).to_vec();
                let call = bcx.ins().call(alloc_ref, &[params[1]]);
                let p = bcx.inst_results(call)[0];
                let dst = bcx.ins().iadd_imm_s(p, 24);
                bcx.ins().call(memcpy_ref, &[dst, params[0], params[1]]);
                bcx.ins().return_(&[p]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_str_from_bytes: {e}"))?;
        }

        // quill_str_cstr(ptr) -> str — a string box holding the NUL-terminated
        // string at `ptr`.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(1);
            let id = self
                .module
                .declare_function("quill_str_cstr", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_str_cstr: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_str_cstr");
            ctx.func.signature = sig;
            let strlen_ref = self.module.declare_func_in_func(strlen, &mut ctx.func);
            let from_bytes_ref =
                self.declare_rt_in(&mut ctx, "quill_str_from_bytes", &plain_sig(2));
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let ptr = bcx.block_params(block)[0];
                let call = bcx.ins().call(strlen_ref, &[ptr]);
                let len = bcx.inst_results(call)[0];
                let call2 = bcx.ins().call(from_bytes_ref, &[ptr, len]);
                let p = bcx.inst_results(call2)[0];
                bcx.ins().return_(&[p]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_str_cstr: {e}"))?;
        }

        // quill_read_line() -> str box, or 0 at end of input. The raw C
        // helper returns a malloc'd NUL-terminated line (NULL at EOF); here
        // it is copied into a string box and freed.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(0);
            let id = self
                .module
                .declare_function("quill_read_line", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_read_line: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_read_line");
            ctx.func.signature = sig;
            let raw_ref = self.module.declare_func_in_func(read_line_raw, &mut ctx.func);
            let free_ref = self.module.declare_func_in_func(free, &mut ctx.func);
            let strlen_ref = self.module.declare_func_in_func(strlen, &mut ctx.func);
            let from_bytes_ref =
                self.declare_rt_in(&mut ctx, "quill_str_from_bytes", &plain_sig(2));
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                let call = bcx.ins().call(raw_ref, &[]);
                let ptr = bcx.inst_results(call)[0];
                let zero = bcx.ins().iconst(types::I64, 0);
                let is_null = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    ptr,
                    zero,
                );
                let eof_b = bcx.create_block();
                let line_b = bcx.create_block();
                bcx.ins().brif(is_null, eof_b, &[], line_b, &[]);
                bcx.switch_to_block(eof_b);
                bcx.ins().return_(&[zero]);
                bcx.switch_to_block(line_b);
                let call2 = bcx.ins().call(strlen_ref, &[ptr]);
                let len = bcx.inst_results(call2)[0];
                let call3 = bcx.ins().call(from_bytes_ref, &[ptr, len]);
                let boxed = bcx.inst_results(call3)[0];
                bcx.ins().call(free_ref, &[ptr]);
                bcx.ins().return_(&[boxed]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_read_line: {e}"))?;
        }

        // quill_char_str(byte) -> str — a one-character string box.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(1);
            let id = self
                .module
                .declare_function("quill_char_str", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_char_str: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_char_str");
            ctx.func.signature = sig;
            let alloc_ref = self.declare_rt_in(&mut ctx, "quill_str_alloc", &plain_sig(1));
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let byte = bcx.block_params(block)[0];
                let one = bcx.ins().iconst(types::I64, 1);
                let call = bcx.ins().call(alloc_ref, &[one]);
                let p = bcx.inst_results(call)[0];
                bcx.ins().store(MemFlagsData::new(), byte, p, 24);
                bcx.ins().return_(&[p]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_char_str: {e}"))?;
        }

        // quill_str_tail(s) -> str — a fresh box holding s[1..].
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(1);
            let id = self
                .module
                .declare_function("quill_str_tail", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_str_tail: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_str_tail");
            ctx.func.signature = sig;
            let alloc_ref = self.declare_rt_in(&mut ctx, "quill_str_alloc", &plain_sig(1));
            let memcpy_ref = self.module.declare_func_in_func(memcpy, &mut ctx.func);
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let s = bcx.block_params(block)[0];
                let len = bcx.ins().load(types::I64, MemFlagsData::new(), s, 16);
                let one = bcx.ins().iconst(types::I64, 1);
                let newlen = bcx.ins().isub(len, one);
                let call = bcx.ins().call(alloc_ref, &[newlen]);
                let p = bcx.inst_results(call)[0];
                let twenty4 = bcx.ins().iconst(types::I64, 24);
                let dst = bcx.ins().iadd(p, twenty4);
                let twenty5 = bcx.ins().iconst(types::I64, 25);
                let src = bcx.ins().iadd(s, twenty5);
                bcx.ins().call(memcpy_ref, &[dst, src, newlen]);
                bcx.ins().return_(&[p]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_str_tail: {e}"))?;
        }

        // quill_str_eq(s, bytes, len) -> i64 — does the string box equal the
        // given bytes?
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(3);
            let id = self
                .module
                .declare_function("quill_str_eq", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_str_eq: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_str_eq");
            ctx.func.signature = sig;
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let params = bcx.block_params(block).to_vec();
                let (s, bytes, want) = (params[0], params[1], params[2]);
                let slen = bcx.ins().load(types::I64, MemFlagsData::new(), s, 16);
                let len_ok = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    slen,
                    want,
                );
                let zero = bcx.ins().iconst(types::I64, 0);
                let one = bcx.ins().iconst(types::I64, 1);
                let check_b = bcx.create_block();
                let false_b = bcx.create_block();
                bcx.ins().brif(len_ok, check_b, &[], false_b, &[]);
                bcx.switch_to_block(check_b);
                // Compare byte by byte.
                let loop_h = bcx.create_block();
                let iv = bcx.append_block_param(loop_h, types::I64);
                bcx.ins().jump(loop_h, &[BlockArg::Value(zero)]);
                bcx.switch_to_block(loop_h);
                let cond = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::SignedLessThan,
                    iv,
                    want,
                );
                let body_b = bcx.create_block();
                let done_true = bcx.create_block();
                bcx.ins().brif(cond, body_b, &[], done_true, &[]);
                bcx.switch_to_block(body_b);
                let soff = bcx.ins().iadd_imm_s(iv, 24);
                let saddr = bcx.ins().iadd(s, soff);
                let baddr = bcx.ins().iadd(bytes, iv);
                let sb = bcx.ins().load(types::I8, MemFlagsData::new(), saddr, 0);
                let bb = bcx.ins().load(types::I8, MemFlagsData::new(), baddr, 0);
                let eq = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    sb,
                    bb,
                );
                let next_b = bcx.create_block();
                bcx.ins().brif(eq, next_b, &[], false_b, &[]);
                bcx.switch_to_block(next_b);
                let next = bcx.ins().iadd_imm_s(iv, 1);
                bcx.ins().jump(loop_h, &[BlockArg::Value(next)]);
                bcx.switch_to_block(done_true);
                bcx.ins().return_(&[one]);
                bcx.switch_to_block(false_b);
                bcx.ins().return_(&[zero]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_str_eq: {e}"))?;
        }

        // quill_str_snoc(s, char_box) -> str — a fresh string with the
        // character appended.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(2);
            let id = self
                .module
                .declare_function("quill_str_snoc", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_str_snoc: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_str_snoc");
            ctx.func.signature = sig;
            let alloc_ref = self.declare_rt_in(&mut ctx, "quill_str_alloc", &plain_sig(1));
            let memcpy_ref = self.module.declare_func_in_func(memcpy, &mut ctx.func);
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let (s, c) = (bcx.block_params(block)[0], bcx.block_params(block)[1]);
                let len = bcx.ins().load(types::I64, MemFlagsData::new(), s, 16);
                let one = bcx.ins().iconst(types::I64, 1);
                let newlen = bcx.ins().iadd(len, one);
                let call = bcx.ins().call(alloc_ref, &[newlen]);
                let p = bcx.inst_results(call)[0];
                let twenty4 = bcx.ins().iconst(types::I64, 24);
                let dst = bcx.ins().iadd(p, twenty4);
                let src = bcx.ins().iadd(s, twenty4);
                bcx.ins().call(memcpy_ref, &[dst, src, len]);
                // Store a single byte (an 8-byte word would overrun the box
                // when the length isn't word-aligned).
                let byte = bcx.ins().load(types::I8, MemFlagsData::new(), c, 24);
                let last = bcx.ins().iadd(p, len);
                let last = bcx.ins().iadd_imm_s(last, 24);
                bcx.ins().store(MemFlagsData::new(), byte, last, 0);
                bcx.ins().return_(&[p]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_str_snoc: {e}"))?;
        }

        // quill_show_rest(tail) -> str — the recursive tail of a list literal:
        // `"]"` for the empty list, otherwise `", " ++ show(head) ++ …`.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(1);
            let id = self
                .module
                .declare_function("quill_show_rest", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_show_rest: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_show_rest");
            ctx.func.signature = sig;
            let from_bytes_ref =
                self.declare_rt_in(&mut ctx, "quill_str_from_bytes", &plain_sig(2));
            let show_ref = self.declare_rt_in(&mut ctx, "quill_show", &plain_sig(1));
            let self_ref = self.declare_rt_in(&mut ctx, "quill_show_rest", &plain_sig(1));
            let concat_ref = self.declare_rt_in(&mut ctx, "quill_str_concat", &plain_sig(2));
            let release_ref = self.declare_rt_in(&mut ctx, "quill_release", &unit_sig(1));
            let comma_gv = self.module.declare_data_in_func(s_comma_id, &mut ctx.func);
            let close_gv = self.module.declare_data_in_func(s_close_id, &mut ctx.func);
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let ys = bcx.block_params(block)[0];
                let tag = bcx.ins().load(types::I64, MemFlagsData::new(), ys, 8);
                let nil_tag = bcx.ins().iconst(types::I64, TAG_NIL);
                let is_nil = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    nil_tag,
                );
                let two = bcx.ins().iconst(types::I64, 2);
                let one = bcx.ins().iconst(types::I64, 1);
                let nil_b = bcx.create_block();
                let cons_b = bcx.create_block();
                bcx.ins().brif(is_nil, nil_b, &[], cons_b, &[]);
                bcx.switch_to_block(nil_b);
                let close = bcx.ins().symbol_value(types::I64, close_gv);
                let c = bcx.ins().call(from_bytes_ref, &[close, one]);
                let r = bcx.inst_results(c)[0];
                bcx.ins().return_(&[r]);
                bcx.switch_to_block(cons_b);
                let head = bcx.ins().load(types::I64, MemFlagsData::new(), ys, 24);
                let tail = bcx.ins().load(types::I64, MemFlagsData::new(), ys, 32);
                let comma = bcx.ins().symbol_value(types::I64, comma_gv);
                let c2 = bcx.ins().call(from_bytes_ref, &[comma, two]);
                let comma_s = bcx.inst_results(c2)[0];
                let c3 = bcx.ins().call(show_ref, &[head]);
                let sh = bcx.inst_results(c3)[0];
                let c4 = bcx.ins().call(concat_ref, &[comma_s, sh]);
                let r1 = bcx.inst_results(c4)[0];
                bcx.ins().call(release_ref, &[comma_s]);
                bcx.ins().call(release_ref, &[sh]);
                let c5 = bcx.ins().call(self_ref, &[tail]);
                let rest = bcx.inst_results(c5)[0];
                let c6 = bcx.ins().call(concat_ref, &[r1, rest]);
                let r2 = bcx.inst_results(c6)[0];
                bcx.ins().call(release_ref, &[r1]);
                bcx.ins().call(release_ref, &[rest]);
                bcx.ins().return_(&[r2]);
                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_show_rest: {e}"))?;
        }

        // quill_show(p) -> str — structural `show`.
        {
            let target_config = self.module.target_config();
            let sig = plain_sig(1);
            let id = self
                .module
                .declare_function("quill_show", Linkage::Local, &sig)
                .map_err(|e| format!("declare quill_show: {e}"))?;
            let mut ctx = self.module.make_context();
            ctx.func.name = UserFuncName::testcase("quill_show");
            ctx.func.signature = sig;
            let itoa_ref = self.module.declare_func_in_func(itoa, &mut ctx.func);
            let ftoa_ref = self.module.declare_func_in_func(ftoa, &mut ctx.func);
            let free_ref = self.module.declare_func_in_func(free, &mut ctx.func);
            let from_bytes_ref =
                self.declare_rt_in(&mut ctx, "quill_str_from_bytes", &plain_sig(2));
            let str_cstr_ref = self.declare_rt_in(&mut ctx, "quill_str_cstr", &plain_sig(1));
            let concat_ref = self.declare_rt_in(&mut ctx, "quill_str_concat", &plain_sig(2));
            let retain_ref = self.declare_rt_in(&mut ctx, "quill_retain", &unit_sig(1));
            let release_ref = self.declare_rt_in(&mut ctx, "quill_release", &unit_sig(1));
            let self_ref = self.declare_rt_in(&mut ctx, "quill_show", &plain_sig(1));
            let show_rest_ref = self.declare_rt_in(&mut ctx, "quill_show_rest", &plain_sig(1));
            let char_str_ref = self.declare_rt_in(&mut ctx, "quill_char_str", &plain_sig(1));
            let s_true_gv = self.module.declare_data_in_func(s_true_id, &mut ctx.func);
            let s_false_gv = self.module.declare_data_in_func(s_false_id, &mut ctx.func);
            let s_open_gv = self.module.declare_data_in_func(s_open_id, &mut ctx.func);
            let s_space_gv = self.module.declare_data_in_func(s_space_id, &mut ctx.func);
            let s_nil_gv = self.module.declare_data_in_func(s_nil_id, &mut ctx.func);
            let s_fun_gv = self.module.declare_data_in_func(s_fun_id, &mut ctx.func);
            let s_quote_gv = self.module.declare_data_in_func(s_quote_id, &mut ctx.func);
            let s_paren_gv = self.module.declare_data_in_func(s_paren_id, &mut ctx.func);
            let s_closep_gv = self.module.declare_data_in_func(s_closep_id, &mut ctx.func);
            let s_comma_gv = self.module.declare_data_in_func(s_comma_id, &mut ctx.func);
            let table_gv = self.module.declare_data_in_func(ctor_table_id, &mut ctx.func);
            {
                let mut fctx = FunctionBuilderContext::new();
                let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                let block = bcx.create_block();
                bcx.switch_to_block(block);
                bcx.append_block_params_for_function_params(block);
                let p = bcx.block_params(block)[0];
                let tag = bcx.ins().load(types::I64, MemFlagsData::new(), p, 8);
                let payload = bcx.ins().load(types::I64, MemFlagsData::new(), p, 24);

                // Int -> itoa.
                let int_tag = bcx.ins().iconst(types::I64, TAG_INT);
                let is_int = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    int_tag,
                );
                let int_b = bcx.create_block();
                let chk_float = bcx.create_block();
                bcx.ins().brif(is_int, int_b, &[], chk_float, &[]);
                bcx.switch_to_block(int_b);
                let c = bcx.ins().call(itoa_ref, &[payload]);
                let s = bcx.inst_results(c)[0];
                let cbox = bcx.ins().call(str_cstr_ref, &[s]);
                let r = bcx.inst_results(cbox)[0];
                bcx.ins().call(free_ref, &[s]);
                bcx.ins().return_(&[r]);

                // Float -> ftoa.
                bcx.switch_to_block(chk_float);
                let float_tag = bcx.ins().iconst(types::I64, TAG_FLOAT);
                let is_float = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    float_tag,
                );
                let float_b = bcx.create_block();
                let chk_bool = bcx.create_block();
                bcx.ins().brif(is_float, float_b, &[], chk_bool, &[]);
                bcx.switch_to_block(float_b);
                let c2 = bcx.ins().call(ftoa_ref, &[payload]);
                let s2 = bcx.inst_results(c2)[0];
                let cbox2 = bcx.ins().call(str_cstr_ref, &[s2]);
                let r2 = bcx.inst_results(cbox2)[0];
                bcx.ins().call(free_ref, &[s2]);
                bcx.ins().return_(&[r2]);

                // Bool -> "true"/"false".
                bcx.switch_to_block(chk_bool);
                let bool_tag = bcx.ins().iconst(types::I64, TAG_BOOL);
                let is_bool = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    bool_tag,
                );
                let bool_b = bcx.create_block();
                let chk_str = bcx.create_block();
                bcx.ins().brif(is_bool, bool_b, &[], chk_str, &[]);
                bcx.switch_to_block(bool_b);
                let one = bcx.ins().iconst(types::I64, 1);
                let is_true = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    payload,
                    one,
                );
                let true_b = bcx.create_block();
                let false_b = bcx.create_block();
                bcx.ins().brif(is_true, true_b, &[], false_b, &[]);
                bcx.switch_to_block(true_b);
                let s_true = bcx.ins().symbol_value(types::I64, s_true_gv);
                let four = bcx.ins().iconst(types::I64, 4);
                let c3 = bcx.ins().call(from_bytes_ref, &[s_true, four]);
                let r3 = bcx.inst_results(c3)[0];
                bcx.ins().return_(&[r3]);
                bcx.switch_to_block(false_b);
                let s_false = bcx.ins().symbol_value(types::I64, s_false_gv);
                let five = bcx.ins().iconst(types::I64, 5);
                let c4 = bcx.ins().call(from_bytes_ref, &[s_false, five]);
                let r4 = bcx.inst_results(c4)[0];
                bcx.ins().return_(&[r4]);

                // Str -> itself.
                bcx.switch_to_block(chk_str);
                let str_tag = bcx.ins().iconst(types::I64, TAG_STR);
                let is_str = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    str_tag,
                );
                let str_b = bcx.create_block();
                let chk_nil = bcx.create_block();
                bcx.ins().brif(is_str, str_b, &[], chk_nil, &[]);
                bcx.switch_to_block(str_b);
                bcx.ins().call(retain_ref, &[p]);
                bcx.ins().return_(&[p]);

                // Nil -> "[]".
                bcx.switch_to_block(chk_nil);
                let nil_tag = bcx.ins().iconst(types::I64, TAG_NIL);
                let is_nil = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    nil_tag,
                );
                let nil_b = bcx.create_block();
                let chk_cons = bcx.create_block();
                bcx.ins().brif(is_nil, nil_b, &[], chk_cons, &[]);
                bcx.switch_to_block(nil_b);
                let s_nil = bcx.ins().symbol_value(types::I64, s_nil_gv);
                let two = bcx.ins().iconst(types::I64, 2);
                let c5 = bcx.ins().call(from_bytes_ref, &[s_nil, two]);
                let r5 = bcx.inst_results(c5)[0];
                bcx.ins().return_(&[r5]);

                // Cons -> "[" ++ show(head) ++ show_rest(tail).
                bcx.switch_to_block(chk_cons);
                let cons_tag = bcx.ins().iconst(types::I64, TAG_CONS);
                let is_cons = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    cons_tag,
                );
                let cons_b = bcx.create_block();
                let chk_fun = bcx.create_block();
                bcx.ins().brif(is_cons, cons_b, &[], chk_fun, &[]);
                bcx.switch_to_block(cons_b);
                let head = bcx.ins().load(types::I64, MemFlagsData::new(), p, 24);
                let tail = bcx.ins().load(types::I64, MemFlagsData::new(), p, 32);
                let s_open = bcx.ins().symbol_value(types::I64, s_open_gv);
                let one2 = bcx.ins().iconst(types::I64, 1);
                let c6 = bcx.ins().call(from_bytes_ref, &[s_open, one2]);
                let lb = bcx.inst_results(c6)[0];
                let c7 = bcx.ins().call(self_ref, &[head]);
                let sh = bcx.inst_results(c7)[0];
                let c8 = bcx.ins().call(concat_ref, &[lb, sh]);
                let r1 = bcx.inst_results(c8)[0];
                bcx.ins().call(release_ref, &[lb]);
                bcx.ins().call(release_ref, &[sh]);
                let c9 = bcx.ins().call(show_rest_ref, &[tail]);
                let rest = bcx.inst_results(c9)[0];
                let c10 = bcx.ins().call(concat_ref, &[r1, rest]);
                let r2 = bcx.inst_results(c10)[0];
                bcx.ins().call(release_ref, &[r1]);
                bcx.ins().call(release_ref, &[rest]);
                bcx.ins().return_(&[r2]);

                // Fun -> "<function>".
                bcx.switch_to_block(chk_fun);
                let fun_tag = bcx.ins().iconst(types::I64, TAG_FUN);
                let is_fun = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    fun_tag,
                );
                let fun_b = bcx.create_block();
                let chk_tuple = bcx.create_block();
                bcx.ins().brif(is_fun, fun_b, &[], chk_tuple, &[]);
                bcx.switch_to_block(fun_b);
                let s_fun = bcx.ins().symbol_value(types::I64, s_fun_gv);
                let ten = bcx.ins().iconst(types::I64, 10);
                let c11 = bcx.ins().call(from_bytes_ref, &[s_fun, ten]);
                let r11 = bcx.inst_results(c11)[0];
                bcx.ins().return_(&[r11]);

                // Tuple -> "(" ++ show(f0) ++ ", " ++ … ++ ")".
                bcx.switch_to_block(chk_tuple);
                let tuple_tag = bcx.ins().iconst(types::I64, TAG_TUPLE);
                let is_tuple = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    tuple_tag,
                );
                let tuple_b = bcx.create_block();
                let chk_tuple_2 = bcx.create_block();
                bcx.ins().brif(is_tuple, tuple_b, &[], chk_tuple_2, &[]);
                bcx.switch_to_block(tuple_b);
                let s_paren = bcx.ins().symbol_value(types::I64, s_paren_gv);
                let one4 = bcx.ins().iconst(types::I64, 1);
                let ct0 = bcx.ins().call(from_bytes_ref, &[s_paren, one4]);
                let open_p = bcx.inst_results(ct0)[0];
                let s_closep = bcx.ins().symbol_value(types::I64, s_closep_gv);
                let ct1 = bcx.ins().call(from_bytes_ref, &[s_closep, one4]);
                let close_p = bcx.inst_results(ct1)[0];
                let s_comma = bcx.ins().symbol_value(types::I64, s_comma_gv);
                let two_c = bcx.ins().iconst(types::I64, 2);
                let ct2 = bcx.ins().call(from_bytes_ref, &[s_comma, two_c]);
                let comma = bcx.inst_results(ct2)[0];
                let tnfields = bcx.ins().load(types::I64, MemFlagsData::new(), p, 16);
                let tzero = bcx.ins().iconst(types::I64, 0);
                let tloop_h = bcx.create_block();
                let tiv = bcx.append_block_param(tloop_h, types::I64);
                let tacc = bcx.append_block_param(tloop_h, types::I64);
                bcx.ins().jump(tloop_h, &[BlockArg::Value(tzero), BlockArg::Value(open_p)]);
                bcx.switch_to_block(tloop_h);
                let tcond = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::SignedLessThan,
                    tiv,
                    tnfields,
                );
                let tbody_b = bcx.create_block();
                let tdone_b = bcx.create_block();
                let tdone_acc = bcx.append_block_param(tdone_b, types::I64);
                bcx.ins().brif(
                    tcond,
                    tbody_b,
                    &[],
                    tdone_b,
                    &[BlockArg::Value(tacc)],
                );
                bcx.switch_to_block(tbody_b);
                let twoff = bcx.ins().imul_imm_s(tiv, 8);
                let tfoff = bcx.ins().iadd_imm_s(twoff, 24);
                let tfaddr = bcx.ins().iadd(p, tfoff);
                let tfield = bcx.ins().load(types::I64, MemFlagsData::new(), tfaddr, 0);
                let ct3 = bcx.ins().call(self_ref, &[tfield]);
                let tfs = bcx.inst_results(ct3)[0];
                let is_first = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tiv,
                    tzero,
                );
                let skip_b = bcx.create_block();
                let join_b = bcx.create_block();
                let skip_acc = bcx.append_block_param(skip_b, types::I64);
                let join_acc = bcx.append_block_param(join_b, types::I64);
                bcx.ins().brif(
                    is_first,
                    skip_b,
                    &[BlockArg::Value(tacc)],
                    join_b,
                    &[BlockArg::Value(tacc)],
                );
                bcx.switch_to_block(join_b);
                let ct4 = bcx.ins().call(concat_ref, &[join_acc, comma]);
                let with_comma = bcx.inst_results(ct4)[0];
                bcx.ins().call(release_ref, &[join_acc]);
                bcx.ins().jump(skip_b, &[BlockArg::Value(with_comma)]);
                bcx.switch_to_block(skip_b);
                let base = skip_acc;
                let ct5 = bcx.ins().call(concat_ref, &[base, tfs]);
                let tnew = bcx.inst_results(ct5)[0];
                bcx.ins().call(release_ref, &[base]);
                bcx.ins().call(release_ref, &[tfs]);
                let tnext = bcx.ins().iadd_imm_s(tiv, 1);
                bcx.ins().jump(tloop_h, &[BlockArg::Value(tnext), BlockArg::Value(tnew)]);
                bcx.switch_to_block(tdone_b);
                let ct6 = bcx.ins().call(concat_ref, &[tdone_acc, close_p]);
                let tresult = bcx.inst_results(ct6)[0];
                bcx.ins().call(release_ref, &[tdone_acc]);
                bcx.ins().call(release_ref, &[close_p]);
                bcx.ins().call(release_ref, &[comma]);
                bcx.ins().return_(&[tresult]);

                // Cell (mutable binding) -> show its payload. Cells are
                // internal; this keeps `show` total on any value.
                bcx.switch_to_block(chk_tuple_2);
                let cell_tag = bcx.ins().iconst(types::I64, TAG_CELL);
                let is_cell = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    cell_tag,
                );
                let cell_b = bcx.create_block();
                let chk_char = bcx.create_block();
                bcx.ins().brif(is_cell, cell_b, &[], chk_char, &[]);
                bcx.switch_to_block(cell_b);
                let cell_payload = payload;
                let ccell = bcx.ins().call(self_ref, &[cell_payload]);
                let cell_r = bcx.inst_results(ccell)[0];
                bcx.ins().return_(&[cell_r]);

                // Char -> `'c'`.
                bcx.switch_to_block(chk_char);
                let char_tag = bcx.ins().iconst(types::I64, TAG_CHAR);
                let is_char = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    char_tag,
                );
                let char_b = bcx.create_block();
                let adt_b = bcx.create_block();
                bcx.ins().brif(is_char, char_b, &[], adt_b, &[]);
                bcx.switch_to_block(char_b);
                let s_quote = bcx.ins().symbol_value(types::I64, s_quote_gv);
                let one4 = bcx.ins().iconst(types::I64, 1);
                let cq0 = bcx.ins().call(from_bytes_ref, &[s_quote, one4]);
                let open_q = bcx.inst_results(cq0)[0];
                let cq1 = bcx.ins().call(from_bytes_ref, &[s_quote, one4]);
                let close_q = bcx.inst_results(cq1)[0];
                let cq2 = bcx.ins().call(char_str_ref, &[payload]);
                let ch_s = bcx.inst_results(cq2)[0];
                let cq3 = bcx.ins().call(concat_ref, &[open_q, ch_s]);
                let with_open = bcx.inst_results(cq3)[0];
                bcx.ins().call(release_ref, &[open_q]);
                bcx.ins().call(release_ref, &[ch_s]);
                let cq4 = bcx.ins().call(concat_ref, &[with_open, close_q]);
                let char_r = bcx.inst_results(cq4)[0];
                bcx.ins().call(release_ref, &[with_open]);
                bcx.ins().call(release_ref, &[close_q]);
                bcx.ins().return_(&[char_r]);

                // Constructor (tag >= 0): `ctor` (nullary) or
                // `ctor field0 field1 …`.
                bcx.switch_to_block(adt_b);
                let table = bcx.ins().symbol_value(types::I64, table_gv);
                let off = bcx.ins().imul_imm_s(tag, 8);
                let name_addr_addr = bcx.ins().iadd(table, off);
                let name_ptr = bcx.ins().load(types::I64, MemFlagsData::new(), name_addr_addr, 0);
                let c12 = bcx.ins().call(str_cstr_ref, &[name_ptr]);
                let name = bcx.inst_results(c12)[0];
                let nfields = bcx.ins().load(types::I64, MemFlagsData::new(), p, 16);
                let zero = bcx.ins().iconst(types::I64, 0);
                let is_nullary = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    nfields,
                    zero,
                );
                let nullary_b = bcx.create_block();
                let fields_b = bcx.create_block();
                bcx.ins().brif(is_nullary, nullary_b, &[], fields_b, &[]);
                bcx.switch_to_block(nullary_b);
                bcx.ins().return_(&[name]);
                bcx.switch_to_block(fields_b);
                let s_space = bcx.ins().symbol_value(types::I64, s_space_gv);
                let one3 = bcx.ins().iconst(types::I64, 1);
                let c13 = bcx.ins().call(from_bytes_ref, &[s_space, one3]);
                let space = bcx.inst_results(c13)[0];
                let loop_h = bcx.create_block();
                let iv = bcx.append_block_param(loop_h, types::I64);
                let acc = bcx.append_block_param(loop_h, types::I64);
                bcx.ins().jump(loop_h, &[BlockArg::Value(zero), BlockArg::Value(name)]);
                bcx.switch_to_block(loop_h);
                let cond = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::SignedLessThan,
                    iv,
                    nfields,
                );
                let body_b = bcx.create_block();
                let done_b = bcx.create_block();
                let done_acc = bcx.append_block_param(done_b, types::I64);
                bcx.ins().brif(
                    cond,
                    body_b,
                    &[],
                    done_b,
                    &[BlockArg::Value(acc)],
                );
                bcx.switch_to_block(body_b);
                let woff = bcx.ins().imul_imm_s(iv, 8);
                let foff = bcx.ins().iadd_imm_s(woff, 24);
                let faddr = bcx.ins().iadd(p, foff);
                let field = bcx.ins().load(types::I64, MemFlagsData::new(), faddr, 0);
                let c14 = bcx.ins().call(self_ref, &[field]);
                let fs = bcx.inst_results(c14)[0];
                let c15 = bcx.ins().call(concat_ref, &[acc, space]);
                let r3 = bcx.inst_results(c15)[0];
                bcx.ins().call(release_ref, &[acc]);
                let c16 = bcx.ins().call(concat_ref, &[r3, fs]);
                let r4 = bcx.inst_results(c16)[0];
                bcx.ins().call(release_ref, &[r3]);
                bcx.ins().call(release_ref, &[fs]);
                let next = bcx.ins().iadd_imm_s(iv, 1);
                bcx.ins().jump(loop_h, &[BlockArg::Value(next), BlockArg::Value(r4)]);
                bcx.switch_to_block(done_b);
                bcx.ins().call(release_ref, &[space]);
                bcx.ins().return_(&[done_acc]);

                bcx.seal_all_blocks();
                bcx.finalize(target_config);
            }
            if let Err(errs) =
                cranelift_codegen::verifier::verify_function(&ctx.func, self.module.isa())
            {
                eprintln!("VERIFIER quill_show: {errs}");
            }
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| format!("define quill_show: {e}"))?;
        }

        Ok(())
    }

    fn define_entry(&mut self, main_id: cranelift_module::FuncId) -> Result<(), String> {
        let target_config = self.module.target_config();
        let mut sig = Signature::new(CallConv::SystemV);
        sig.returns.push(AbiParam::new(types::I32));
        let entry = self
            .module
            .declare_function("main", Linkage::Export, &sig)
            .map_err(|e| format!("declare main: {e}"))?;
        let mut ctx = self.module.make_context();
        ctx.func.name = UserFuncName::testcase("main");
        ctx.func.signature = sig;
        let callee = self.module.declare_func_in_func(main_id, &mut ctx.func);
        let alloc_ref = self.declare_rt_in(&mut ctx, "quill_alloc", &plain_sig(2));
        {
            let mut func_ctx = FunctionBuilderContext::new();
            let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut func_ctx);
            let block = bcx.create_block();
            bcx.switch_to_block(block);
            bcx.append_block_params_for_function_params(block);
            // `main` takes a closure (unused — nullary, no capture) and an
            // (empty) args-list parameter.
            let nil_tag = bcx.ins().iconst(types::I64, TAG_NIL);
            let zero = bcx.ins().iconst(types::I64, 0);
            let alloc = bcx.ins().call(alloc_ref, &[nil_tag, zero]);
            let nil = bcx.inst_results(alloc)[0];
            let call = bcx.ins().call(callee, &[nil, nil]);
            let v = bcx.inst_results(call)[0];
            // `main` returns an Int box; read its payload as the exit code.
            // For a Bool box that's 0/1; for an ADT (which has no integer
            // payload) fall back to the constructor tag so we never read past
            // the end of the allocation.
            let tag = tag_of(&mut bcx, v);
            let int_tag = bcx.ins().iconst(types::I64, TAG_INT);
            let is_int =
                bcx.ins()
                    .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, tag, int_tag);
            let bool_tag = bcx.ins().iconst(types::I64, TAG_BOOL);
            let is_bool = bcx.ins().icmp(
                cranelift_codegen::ir::condcodes::IntCC::Equal,
                tag,
                bool_tag,
            );
            let is_scalar = bcx.ins().bor(is_int, is_bool);
            let scalar_b = bcx.create_block();
            let other_b = bcx.create_block();
            let merge_b = bcx.create_block();
            let merge_p = bcx.append_block_param(merge_b, types::I64);
            bcx.ins().brif(is_scalar, scalar_b, &[], other_b, &[]);
            bcx.switch_to_block(scalar_b);
            let payload = unbox(&mut bcx, v);
            bcx.ins().jump(merge_b, &[BlockArg::Value(payload)]);
            bcx.switch_to_block(other_b);
            bcx.ins().jump(merge_b, &[BlockArg::Value(tag)]);
            bcx.seal_block(scalar_b);
            bcx.seal_block(other_b);
            bcx.seal_block(merge_b);
            bcx.switch_to_block(merge_b);
            let exit = bcx.ins().ireduce(types::I32, merge_p);
            bcx.ins().return_(&[exit]);
            bcx.seal_all_blocks();
            bcx.finalize(target_config);
        }
        self.module
            .define_function(entry, &mut ctx)
            .map_err(|e| format!("define main: {e}"))?;
        Ok(())
    }

    fn define_trivial_entry(&mut self) -> Result<(), String> {
        let target_config = self.module.target_config();
        let mut sig = Signature::new(CallConv::SystemV);
        sig.returns.push(AbiParam::new(types::I32));
        let entry = self
            .module
            .declare_function("main", Linkage::Export, &sig)
            .map_err(|e| format!("declare main: {e}"))?;
        let mut ctx = self.module.make_context();
        ctx.func.name = UserFuncName::testcase("main");
        ctx.func.signature = sig;
        {
            let mut func_ctx = FunctionBuilderContext::new();
            let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut func_ctx);
            let block = bcx.create_block();
            bcx.switch_to_block(block);
            bcx.append_block_params_for_function_params(block);
            let zero = bcx.ins().iconst(types::I32, 0);
            bcx.ins().return_(&[zero]);
            bcx.seal_all_blocks();
            bcx.finalize(target_config);
        }
        self.module
            .define_function(entry, &mut ctx)
            .map_err(|e| format!("define main: {e}"))?;
        Ok(())
    }

    fn define_function(
        &mut self,
        id: cranelift_module::FuncId,
        f: &Function,
        arity: usize,
        free_vars: &[String],
        cell_vars: &std::collections::HashSet<String>,
        func_ids: &HashMap<String, cranelift_module::FuncId>,
    ) -> Result<(), String> {
        let target_config = self.module.target_config();
        let mut ctx = self.module.make_context();
        ctx.func.name = UserFuncName::testcase(&f.name);
        // Every function takes the closure that invoked it (used to read the
        // captured environment) and a boxed list of its actual arguments. This
        // uniform convention is what lets closures apply a function value
        // generically.
        ctx.func.signature = fn_sig(2);

        let mut funcrefs: HashMap<String, FuncRef> = HashMap::new();
        for (name, fid) in func_ids {
            let fr = self.module.declare_func_in_func(*fid, &mut ctx.func);
            funcrefs.insert(name.clone(), fr);
        }
        let rt = Rt {
            alloc: self.declare_rt_in(&mut ctx, "quill_alloc", &plain_sig(2)),
            box_value: self.declare_rt_in(&mut ctx, "quill_box", &plain_sig(2)),
            retain: self.declare_rt_in(&mut ctx, "quill_retain", &unit_sig(1)),
            release: self.declare_rt_in(&mut ctx, "quill_release", &unit_sig(1)),
            str_alloc: self.declare_rt_in(&mut ctx, "quill_str_alloc", &plain_sig(1)),
            println: self.declare_rt_in(&mut ctx, "quill_println", &unit_sig(1)),
            print: self.declare_rt_in(&mut ctx, "quill_print", &unit_sig(1)),
            cons: self.declare_rt_in(&mut ctx, "quill_cons", &plain_sig(2)),
            append: self.declare_rt_in(&mut ctx, "quill_append", &plain_sig(2)),
            make_closure: self.declare_rt_in(&mut ctx, "quill_make_closure", &plain_sig(5)),
            apply: self.declare_rt_in(&mut ctx, "quill_apply", &plain_sig(2)),
            show: self.declare_rt_in(&mut ctx, "quill_show", &plain_sig(1)),
            str_tail: self.declare_rt_in(&mut ctx, "quill_str_tail", &plain_sig(1)),
            str_eq: self.declare_rt_in(&mut ctx, "quill_str_eq", &plain_sig(3)),
            str_snoc: self.declare_rt_in(&mut ctx, "quill_str_snoc", &plain_sig(2)),
            read_line: self.declare_rt_in(&mut ctx, "quill_read_line", &plain_sig(0)),
        };

        {
            let mut func_ctx = FunctionBuilderContext::new();
            let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut func_ctx);
            let entry = bcx.create_block();
            bcx.switch_to_block(entry);
            bcx.append_block_params_for_function_params(entry);
            let params = bcx.block_params(entry).to_vec();

            // Destructure the args-list parameter (param 1) into `arity`
            // argument values, taking ownership of each (retain) before
            // releasing the cons-cell plumbing.
            let arg_values = destructure_args(&mut bcx, params[1], arity, rt);

            // Pre-bind any captured variables from the closure's environment
            // (offset 56). The closure still owns the env list, so we retain
            // each captured value and release it when this function exits.
            let mut env_values: Vec<Value> = Vec::new();
            if !free_vars.is_empty() {
                let env = bcx.ins().load(types::I64, MemFlagsData::new(), params[0], 56);
                let mut cur = env;
                for _ in 0..free_vars.len() {
                    let head = bcx.ins().load(types::I64, MemFlagsData::new(), cur, 24);
                    let tail = bcx.ins().load(types::I64, MemFlagsData::new(), cur, 32);
                    bcx.ins().call(rt.retain, &[head]);
                    env_values.push(head);
                    cur = tail;
                }
            }

            let merge_block = bcx.create_block();
            let merge_param = bcx.append_block_param(merge_block, types::I64);
            let mut clause_entries: Vec<cranelift_codegen::ir::Block> = Vec::new();
            for _ in &f.clauses {
                clause_entries.push(bcx.create_block());
            }
            let trap_block = bcx.create_block();

            bcx.ins().jump(clause_entries[0], &[]);
            bcx.seal_block(entry);

            for (i, clause) in f.clauses.iter().enumerate() {
                let this_block = clause_entries[i];
                let next_block = if i + 1 < clause_entries.len() {
                    clause_entries[i + 1]
                } else {
                    trap_block
                };
                bcx.switch_to_block(this_block);

                let mut vars: HashMap<String, Variable> = HashMap::new();
                let mut owned: Vec<Value> = Vec::new(); // boxes to release at exit
                // Bind captured variables (retained from the environment).
                for (k, fv) in free_vars.iter().enumerate() {
                    let v = bcx.declare_var(types::I64);
                    bcx.def_var(v, env_values[k]);
                    owned.push(env_values[k]);
                    vars.insert(fv.clone(), v);
                }
                for (j, p) in clause.pats.iter().enumerate() {
                    match p {
                        Pattern::Var(name, _) => {
                            let v = bcx.declare_var(types::I64);
                            bcx.def_var(v, arg_values[j]);
                            owned.push(arg_values[j]);
                            vars.insert(name.clone(), v);
                        }
                        Pattern::Wildcard(_) => {
                            owned.push(arg_values[j]);
                        }
                        Pattern::Int(n, _) => {
                            let val = unbox(&mut bcx, arg_values[j]);
                            let lit = bcx.ins().iconst(types::I64, *n);
                            let cond = bcx.ins().icmp(
                                cranelift_codegen::ir::condcodes::IntCC::Equal,
                                val,
                                lit,
                            );
                            let cont = bcx.create_block();
                            bcx.ins().brif(cond, cont, &[], next_block, &[]);
                            bcx.switch_to_block(cont);
                            owned.push(arg_values[j]);
                        }
                        Pattern::Ctor { name, args, .. }
                            if (name == "True" || name == "False") && args.is_empty() =>
                        {
                            let val = unbox(&mut bcx, arg_values[j]);
                            let lit = bcx
                                .ins()
                                .iconst(types::I64, if name == "True" { 1 } else { 0 });
                            let cond = bcx.ins().icmp(
                                cranelift_codegen::ir::condcodes::IntCC::Equal,
                                val,
                                lit,
                            );
                            let cont = bcx.create_block();
                            bcx.ins().brif(cond, cont, &[], next_block, &[]);
                            bcx.switch_to_block(cont);
                            owned.push(arg_values[j]);
                        }
                        Pattern::Ctor { name, args, .. } => {
                            let ctor_idx = self
                                .info
                                .ctor_of(name)
                                .ok_or_else(|| format!("unknown constructor `{name}`"))?;
                            let obj = arg_values[j];
                            let obj_tag = tag_of(&mut bcx, obj);
                            let tagc = bcx.ins().iconst(types::I64, ctor_idx as i64);
                            let cond = bcx.ins().icmp(
                                cranelift_codegen::ir::condcodes::IntCC::Equal,
                                obj_tag,
                                tagc,
                            );
                            let cont = bcx.create_block();
                            bcx.ins().brif(cond, cont, &[], next_block, &[]);
                            bcx.switch_to_block(cont);
                            for (k, ap) in args.iter().enumerate() {
                                let offset = 24 + (k as i64) * 8;
                                let field_val = bcx.ins().load(
                                    types::I64,
                                    MemFlagsData::new(),
                                    obj,
                                    offset as i32,
                                );
                                self.bind_pattern(
                                    &mut bcx, ap, field_val, &mut vars, &mut owned, next_block, rt,
                                )?;
                            }
                            owned.push(obj);
                        }
                        Pattern::List(ps, _) if ps.is_empty() => {
                            check_tag(&mut bcx, arg_values[j], TAG_NIL, next_block);
                            owned.push(arg_values[j]);
                        }
                        Pattern::List(ps, _) => {
                            check_tag(&mut bcx, arg_values[j], TAG_CONS, next_block);
                            let head = bcx.ins().load(types::I64, MemFlagsData::new(), arg_values[j], 24);
                            let tail = bcx.ins().load(types::I64, MemFlagsData::new(), arg_values[j], 32);
                            self.bind_pattern(&mut bcx, &ps[0], head, &mut vars, &mut owned, next_block, rt)?;
                            let rest = Pattern::List(ps[1..].to_vec(), crate::error::Span { lo: 0, hi: 0 });
                            self.bind_pattern(&mut bcx, &rest, tail, &mut vars, &mut owned, next_block, rt)?;
                            owned.push(arg_values[j]);
                        }
                        Pattern::Char(ch, _) => {
                            let val = unbox(&mut bcx, arg_values[j]);
                            let lit = bcx.ins().iconst(types::I64, *ch as i64);
                            let cond = bcx.ins().icmp(
                                cranelift_codegen::ir::condcodes::IntCC::Equal,
                                val,
                                lit,
                            );
                            let cont = bcx.create_block();
                            bcx.ins().brif(cond, cont, &[], next_block, &[]);
                            bcx.switch_to_block(cont);
                            owned.push(arg_values[j]);
                        }
                        Pattern::Str(s, span) => {
                            // A string literal pattern: tag check + byte
                            // comparison against the literal's static data.
                            let bytes = self
                                .declare_bytes(&format!("quill_strpat_{}", span.lo), s.as_bytes());
                            let bytes_gv = self.module.declare_data_in_func(bytes, &mut bcx.func);
                            let bytes_ptr = bcx.ins().symbol_value(types::I64, bytes_gv);
                            let len = bcx.ins().iconst(types::I64, s.len() as i64);
                            let call = bcx.ins().call(rt.str_eq, &[arg_values[j], bytes_ptr, len]);
                            let matched = bcx.inst_results(call)[0];
                            let one = bcx.ins().iconst(types::I64, 1);
                            let cond = bcx.ins().icmp(
                                cranelift_codegen::ir::condcodes::IntCC::Equal,
                                matched,
                                one,
                            );
                            let cont = bcx.create_block();
                            bcx.ins().brif(cond, cont, &[], next_block, &[]);
                            bcx.switch_to_block(cont);
                            owned.push(arg_values[j]);
                        }
                        Pattern::Cons(h, t, _) => {
                            self.bind_cons_pattern(
                                &mut bcx,
                                h,
                                t,
                                arg_values[j],
                                &mut vars,
                                &mut owned,
                                next_block,
                                rt,
                            )?;
                        }
                        Pattern::Tuple(ps, _) => {
                            check_tag(&mut bcx, arg_values[j], TAG_TUPLE, next_block);
                            for (k, ap) in ps.iter().enumerate() {
                                let offset = 24 + (k as i64) * 8;
                                let fv = bcx.ins().load(
                                    types::I64,
                                    MemFlagsData::new(),
                                    arg_values[j],
                                    offset as i32,
                                );
                                self.bind_pattern(
                                    &mut bcx, ap, fv, &mut vars, &mut owned, next_block, rt,
                                )?;
                            }
                            owned.push(arg_values[j]);
                        }
                        Pattern::Record { ctor, fields, .. } => {
                            let ctor_idx = self
                                .info
                                .ctor_of(ctor)
                                .ok_or_else(|| format!("unknown constructor `{ctor}`"))?;
                            let obj = arg_values[j];
                            let obj_tag = tag_of(&mut bcx, obj);
                            let tagc = bcx.ins().iconst(types::I64, ctor_idx as i64);
                            let cond = bcx.ins().icmp(
                                cranelift_codegen::ir::condcodes::IntCC::Equal,
                                obj_tag,
                                tagc,
                            );
                            let cont = bcx.create_block();
                            bcx.ins().brif(cond, cont, &[], next_block, &[]);
                            bcx.switch_to_block(cont);
                            let decl = self
                                .info
                                .ctor_fields
                                .get(ctor)
                                .cloned()
                                .unwrap_or_default();
                            for (fname, sub) in fields {
                                let idx = decl.iter().position(|n| n == fname).ok_or_else(|| {
                                    format!("constructor `{ctor}` has no field `{fname}`")
                                })?;
                                let offset = 24 + (idx as i64) * 8;
                                let fv = bcx.ins().load(
                                    types::I64,
                                    MemFlagsData::new(),
                                    obj,
                                    offset as i32,
                                );
                                self.bind_pattern(
                                    &mut bcx, sub, fv, &mut vars, &mut owned, next_block, rt,
                                )?;
                            }
                            owned.push(obj);
                        }
                        other => return Err(format!("unsupported pattern: {other:?}")),
                    }
                }

                let self_ref = funcrefs[&f.name];
                // A self-call in tail position jumps here with (closure,
                // args list); the pattern-bound originals are released once
                // the arguments are safely evaluated, then we tail-jump.
                let tail_b = bcx.create_block();
                let tail_nil = bcx.append_block_param(tail_b, types::I64);
                let tail_list = bcx.append_block_param(tail_b, types::I64);
                let body = self.gen_expr(
                    &mut bcx,
                    &clause.body,
                    &vars,
                    &funcrefs,
                    rt,
                    cell_vars,
                    Some((&f.name, arity, self_ref, tail_b)),
                )?;

                for val in &owned {
                    release(&mut bcx, rt, *val);
                }

                bcx.ins().jump(merge_block, &[BlockArg::Value(body)]);
                bcx.seal_block(this_block);

                bcx.switch_to_block(tail_b);
                for val in &owned {
                    release(&mut bcx, rt, *val);
                }
                // The caller-provided nil closure is never read by a
                // top-level function; release it before the tail jump.
                bcx.ins().call(rt.release, &[tail_nil]);
                bcx.ins().return_call(self_ref, &[tail_nil, tail_list]);
                bcx.seal_block(tail_b);
            }

            bcx.switch_to_block(trap_block);
            bcx.ins().trap(TrapCode::unwrap_user(1));
            bcx.seal_block(trap_block);

            bcx.seal_block(merge_block);
            bcx.switch_to_block(merge_block);
            bcx.ins().return_(&[merge_param]);
            bcx.seal_all_blocks();
            bcx.finalize(target_config);
        }
        if let Err(errs) =
            cranelift_codegen::verifier::verify_function(&ctx.func, self.module.isa())
        {
            eprintln!("VERIFIER ({}): {errs}", f.name);
        }
        self.module
            .define_function(id, &mut ctx)
            .map_err(|e| format!("define {}: {e}", f.name))?;
        Ok(())
    }

    fn declare_rt_in(
        &mut self,
        ctx: &mut cranelift_codegen::Context,
        name: &str,
        sig: &Signature,
    ) -> FuncRef {
        let fid = self
            .module
            .declare_function(name, Linkage::Local, sig)
            .expect("declare runtime fn");
        self.module.declare_func_in_func(fid, &mut ctx.func)
    }

    /// Bind a cons pattern: destructures a list cell, or a string (head
    /// Char, tail String — matched byte-wise, so ASCII strings behave like
    /// the interpreter's). Both paths feed the sub-pattern bindings through
    /// a merge block so the bound variables dominate the clause body.
    fn bind_cons_pattern(
        &mut self,
        bcx: &mut FunctionBuilder,
        h: &Pattern,
        t: &Pattern,
        value: Value,
        vars: &mut HashMap<String, Variable>,
        owned: &mut Vec<Value>,
        next: cranelift_codegen::ir::Block,
        rt: Rt,
    ) -> Result<(), String> {
        let tagv = tag_of(bcx, value);
        let cons_tag = bcx.ins().iconst(types::I64, TAG_CONS);
        let is_cons = bcx.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::Equal,
            tagv,
            cons_tag,
        );
        let merge_b = bcx.create_block();
        let hparam = bcx.append_block_param(merge_b, types::I64);
        let tparam = bcx.append_block_param(merge_b, types::I64);
        let cons_b = bcx.create_block();
        let chk_str_b = bcx.create_block();
        bcx.ins().brif(is_cons, cons_b, &[], chk_str_b, &[]);

        // List path: the cons cell's head and tail.
        bcx.switch_to_block(cons_b);
        let head = bcx.ins().load(types::I64, MemFlagsData::new(), value, 24);
        let tail = bcx.ins().load(types::I64, MemFlagsData::new(), value, 32);
        bcx.ins().jump(merge_b, &[BlockArg::Value(head), BlockArg::Value(tail)]);

        // String path: the first byte as a Char box and the rest as a fresh
        // string.
        bcx.switch_to_block(chk_str_b);
        let str_tag = bcx.ins().iconst(types::I64, TAG_STR);
        let is_str = bcx.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::Equal,
            tagv,
            str_tag,
        );
        let str_b = bcx.create_block();
        bcx.ins().brif(is_str, str_b, &[], next, &[]);
        bcx.switch_to_block(str_b);
        let slen = bcx.ins().load(types::I64, MemFlagsData::new(), value, 16);
        let zero = bcx.ins().iconst(types::I64, 0);
        let nonempty = bcx.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::SignedGreaterThan,
            slen,
            zero,
        );
        let nonempty_b = bcx.create_block();
        bcx.ins().brif(nonempty, nonempty_b, &[], next, &[]);
        bcx.switch_to_block(nonempty_b);
        let byte = bcx.ins().load(types::I8, MemFlagsData::new(), value, 24);
        let byte64 = bcx.ins().uextend(types::I64, byte);
        let char_box = box_char(bcx, byte64, rt);
        let call = bcx.ins().call(rt.str_tail, &[value]);
        let rest = bcx.inst_results(call)[0];
        bcx.ins().jump(merge_b, &[BlockArg::Value(char_box), BlockArg::Value(rest)]);

        // Bind the sub-patterns in the merge; the matched object itself is
        // consumed by the caller's `owned` list.
        bcx.switch_to_block(merge_b);
        self.bind_pattern(bcx, h, hparam, vars, owned, next, rt)?;
        self.bind_pattern(bcx, t, tparam, vars, owned, next, rt)?;
        // The matched cons cell / string's working ref is released at exit.
        owned.push(value);
        Ok(())
    }

    /// Declare a static read-only byte blob and return its `DataId`.
    fn declare_bytes(&mut self, name: &str, bytes: &[u8]) -> cranelift_module::DataId {
        let id = self
            .module
            .declare_data(name, Linkage::Local, false, false)
            .expect("declare data");
        let mut desc = cranelift_module::DataDescription::new();
        desc.define(bytes.to_vec().into_boxed_slice());
        self.module.define_data(id, &desc).expect("define data");
        id
    }

    /// Bind a sub-pattern (used for constructor fields). A bound field is
    /// retained because the binding shares the object. `next` is the block to
    /// jump to when the pattern does not match.
    fn bind_pattern(
        &mut self,
        bcx: &mut FunctionBuilder,
        p: &Pattern,
        value: Value,
        vars: &mut HashMap<String, Variable>,
        owned: &mut Vec<Value>,
        next: cranelift_codegen::ir::Block,
        rt: Rt,
    ) -> Result<(), String> {
        match p {
            Pattern::Var(name, _) => {
                let v = bcx.declare_var(types::I64);
                bcx.ins().call(rt.retain, &[value]);
                owned.push(value);
                bcx.def_var(v, value);
                vars.insert(name.clone(), v);
            }
            Pattern::Wildcard(_) => {}
            Pattern::Int(n, _) => {
                let val = unbox(bcx, value);
                let lit = bcx.ins().iconst(types::I64, *n);
                let cond = bcx
                    .ins()
                    .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, val, lit);
                let cont = bcx.create_block();
                bcx.ins().brif(cond, cont, &[], next, &[]);
                bcx.switch_to_block(cont);
            }
            Pattern::Ctor { name, args, .. }
                if (name == "True" || name == "False") && args.is_empty() =>
            {
                let val = unbox(bcx, value);
                let lit = bcx
                    .ins()
                    .iconst(types::I64, if name == "True" { 1 } else { 0 });
                let cond = bcx
                    .ins()
                    .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, val, lit);
                let cont = bcx.create_block();
                bcx.ins().brif(cond, cont, &[], next, &[]);
                bcx.switch_to_block(cont);
            }
            Pattern::Ctor { name, args, .. } => {
                let ctor_idx = self
                    .info
                    .ctor_of(name)
                    .ok_or_else(|| format!("unknown constructor `{name}`"))?;
                let obj_tag = tag_of(bcx, value);
                let tagc = bcx.ins().iconst(types::I64, ctor_idx as i64);
                let cond = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    obj_tag,
                    tagc,
                );
                let cont = bcx.create_block();
                bcx.ins().brif(cond, cont, &[], next, &[]);
                bcx.switch_to_block(cont);
                for (k, ap) in args.iter().enumerate() {
                    let offset = 24 + (k as i64) * 8;
                    let field_val =
                        bcx.ins()
                            .load(types::I64, MemFlagsData::new(), value, offset as i32);
                    self.bind_pattern(bcx, ap, field_val, vars, owned, next, rt)?;
                }
            }
            Pattern::List(ps, _) if ps.is_empty() => {
                check_tag(bcx, value, TAG_NIL, next);
            }
            Pattern::List(ps, _) => {
                check_tag(bcx, value, TAG_CONS, next);
                let head = bcx.ins().load(types::I64, MemFlagsData::new(), value, 24);
                let tail = bcx.ins().load(types::I64, MemFlagsData::new(), value, 32);
                self.bind_pattern(bcx, &ps[0], head, vars, owned, next, rt)?;
                let rest = Pattern::List(ps[1..].to_vec(), crate::error::Span { lo: 0, hi: 0 });
                self.bind_pattern(bcx, &rest, tail, vars, owned, next, rt)?;
            }
            Pattern::Char(ch, _) => {
                let val = unbox(bcx, value);
                let lit = bcx.ins().iconst(types::I64, *ch as i64);
                let cond = bcx
                    .ins()
                    .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, val, lit);
                let cont = bcx.create_block();
                bcx.ins().brif(cond, cont, &[], next, &[]);
                bcx.switch_to_block(cont);
            }
            Pattern::Str(s, span) => {
                let bytes = self.declare_bytes(&format!("quill_strpat_{}", span.lo), s.as_bytes());
                let bytes_gv = self.module.declare_data_in_func(bytes, &mut bcx.func);
                let bytes_ptr = bcx.ins().symbol_value(types::I64, bytes_gv);
                let len = bcx.ins().iconst(types::I64, s.len() as i64);
                let call = bcx.ins().call(rt.str_eq, &[value, bytes_ptr, len]);
                let matched = bcx.inst_results(call)[0];
                let one = bcx.ins().iconst(types::I64, 1);
                let cond = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    matched,
                    one,
                );
                let cont = bcx.create_block();
                bcx.ins().brif(cond, cont, &[], next, &[]);
                bcx.switch_to_block(cont);
            }
            Pattern::Cons(h, t, _) => {
                self.bind_cons_pattern(bcx, h, t, value, vars, owned, next, rt)?;
            }
            Pattern::Tuple(ps, _) => {
                check_tag(bcx, value, TAG_TUPLE, next);
                for (k, ap) in ps.iter().enumerate() {
                    let offset = 24 + (k as i64) * 8;
                    let field_val =
                        bcx.ins()
                            .load(types::I64, MemFlagsData::new(), value, offset as i32);
                    self.bind_pattern(bcx, ap, field_val, vars, owned, next, rt)?;
                }
            }
            Pattern::Record { ctor, fields, .. } => {
                let ctor_idx = self
                    .info
                    .ctor_of(ctor)
                    .ok_or_else(|| format!("unknown constructor `{ctor}`"))?;
                let obj_tag = tag_of(bcx, value);
                let tagc = bcx.ins().iconst(types::I64, ctor_idx as i64);
                let cond = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    obj_tag,
                    tagc,
                );
                let cont = bcx.create_block();
                bcx.ins().brif(cond, cont, &[], next, &[]);
                bcx.switch_to_block(cont);
                let decl = self
                    .info
                    .ctor_fields
                    .get(ctor)
                    .cloned()
                    .unwrap_or_default();
                for (fname, sub) in fields {
                    let idx = decl.iter().position(|n| n == fname).ok_or_else(|| {
                        format!("constructor `{ctor}` has no field `{fname}`")
                    })?;
                    let offset = 24 + (idx as i64) * 8;
                    let field_val =
                        bcx.ins()
                            .load(types::I64, MemFlagsData::new(), value, offset as i32);
                    self.bind_pattern(bcx, sub, field_val, vars, owned, next, rt)?;
                }
            }
            other => return Err(format!("unsupported nested pattern: {other:?}")),
        }
        Ok(())
    }

    fn gen_expr(
        &mut self,
        bcx: &mut FunctionBuilder,
        e: &Expr,
        vars: &HashMap<String, Variable>,
        funcrefs: &HashMap<String, FuncRef>,
        rt: Rt,
        cells: &std::collections::HashSet<String>,
        tail: Option<(&str, usize, FuncRef, cranelift_codegen::ir::Block)>,
    ) -> Result<Value, String> {
        match e {
            Expr::Int(n, _) => {
                let nv = bcx.ins().iconst(types::I64, *n);
                Ok(box_int(bcx, nv, rt))
            }
            Expr::Float(f, _) => {
                let bits = bcx.ins().iconst(types::I64, f.to_bits() as i64);
                Ok(box_value(bcx, TAG_FLOAT, bits, rt))
            }
            Expr::Str(s, _) => Ok(gen_str_literal(bcx, s, rt)),
            Expr::Char(c, _) => {
                let code = bcx.ins().iconst(types::I64, *c as i64);
                Ok(box_char(bcx, code, rt))
            }
            Expr::Unit(_) => {
                // `()` has no native representation of its own; use a boxed 0.
                let zero = bcx.ins().iconst(types::I64, 0);
                Ok(box_int(bcx, zero, rt))
            }
            // Under immutability + ARC, `clone`, `&`, and `*` are identities.
            Expr::Clone(e, _) | Expr::Borrow { expr: e, .. } | Expr::Deref(e, _) => {
                self.gen_expr(bcx, e, vars, funcrefs, rt, cells, None)
            }
            Expr::List(es, _) => {
                // Build from the tail: `[]` then fold cons from right to left.
                let nil_tag = bcx.ins().iconst(types::I64, TAG_NIL);
                let zero = bcx.ins().iconst(types::I64, 0);
                let call = bcx.ins().call(rt.alloc, &[nil_tag, zero]);
                let mut acc = bcx.inst_results(call)[0];
                for e in es.iter().rev() {
                    let v = self.gen_expr(bcx, e, vars, funcrefs, rt, cells, None)?;
                    let call = bcx.ins().call(rt.cons, &[v, acc]);
                    acc = bcx.inst_results(call)[0];
                }
                Ok(acc)
            }
            Expr::Tuple(es, _) => {
                let tag = bcx.ins().iconst(types::I64, TAG_TUPLE);
                let nfields = bcx.ins().iconst(types::I64, es.len() as i64);
                let call = bcx.ins().call(rt.alloc, &[tag, nfields]);
                let obj = bcx.inst_results(call)[0];
                for (k, e) in es.iter().enumerate() {
                    let v = self.gen_expr(bcx, e, vars, funcrefs, rt, cells, None)?;
                    let offset = 24 + (k as i64) * 8;
                    bcx.ins().store(MemFlagsData::new(), v, obj, offset as i32);
                }
                Ok(obj)
            }
            Expr::Record { ctor, fields, .. } => {
                // Records are stored like positional constructors: field
                // values in declaration order.
                let ctor_idx = self
                    .info
                    .ctor_of(ctor)
                    .ok_or_else(|| format!("unknown constructor `{ctor}`"))?;
                let decl = self
                    .info
                    .ctor_fields
                    .get(ctor)
                    .cloned()
                    .unwrap_or_default();
                let tag = bcx.ins().iconst(types::I64, ctor_idx as i64);
                let nfields = bcx.ins().iconst(types::I64, decl.len() as i64);
                let call = bcx.ins().call(rt.alloc, &[tag, nfields]);
                let obj = bcx.inst_results(call)[0];
                for (k, fname) in decl.iter().enumerate() {
                    let e = fields
                        .iter()
                        .find(|(n, _)| n == fname)
                        .map(|(_, e)| e)
                        .ok_or_else(|| {
                            format!("constructor `{ctor}` requires field `{fname}`")
                        })?;
                    let v = self.gen_expr(bcx, e, vars, funcrefs, rt, cells, None)?;
                    let offset = 24 + (k as i64) * 8;
                    bcx.ins().store(MemFlagsData::new(), v, obj, offset as i32);
                }
                Ok(obj)
            }
            Expr::Field(e, name, _) => {
                let idx = self
                    .info
                    .field_index
                    .get(name)
                    .map(|(_, i)| *i)
                    .ok_or_else(|| format!("no field `{name}`"))?;
                let obj = self.gen_expr(bcx, e, vars, funcrefs, rt, cells, None)?;
                let offset = 24 + (idx as i64) * 8;
                let fv = bcx
                    .ins()
                    .load(types::I64, MemFlagsData::new(), obj, offset as i32);
                bcx.ins().call(rt.retain, &[fv]);
                release(bcx, rt, obj);
                Ok(fv)
            }
            Expr::Ctor(name, _) if name == "True" => {
                let one = bcx.ins().iconst(types::I64, 1);
                Ok(box_bool(bcx, one, rt))
            }
            Expr::Ctor(name, _) if name == "False" => {
                let zero = bcx.ins().iconst(types::I64, 0);
                Ok(box_bool(bcx, zero, rt))
            }
            Expr::Ctor(name, _) => {
                let ctor_idx = self
                    .info
                    .ctor_of(name)
                    .ok_or_else(|| format!("unknown constructor `{name}`"))?;
                let tag = bcx.ins().iconst(types::I64, ctor_idx as i64);
                let nfields = bcx.ins().iconst(types::I64, 0);
                let call = bcx.ins().call(rt.alloc, &[tag, nfields]);
                Ok(bcx.inst_results(call)[0])
            }
            Expr::Var(name, _) if name == "otherwise" => {
                // `otherwise` is the predefined constant `True` (used in guards).
                let one = bcx.ins().iconst(types::I64, 1);
                Ok(box_bool(bcx, one, rt))
            }
            Expr::Var(name, _) => {
                if let Some(var) = vars.get(name) {
                    let val = bcx.use_var(*var);
                    if cells.contains(name) {
                        // A mutable binding's Variable holds its cell; reads
                        // share the current payload.
                        let payload = bcx.ins().load(types::I64, MemFlagsData::new(), val, 24);
                        bcx.ins().call(rt.retain, &[payload]);
                        Ok(payload)
                    } else {
                        bcx.ins().call(rt.retain, &[val]);
                        Ok(val)
                    }
                } else if let Some(&func_ref) = funcrefs.get(name) {
                    // A top-level function used as a value: nullary bindings
                    // are called, higher-arity ones become closures.
                    let arity = self.arities.get(name).copied().unwrap_or(0);
                    if arity == 0 {
                        let nil = make_nil(bcx, rt);
                        let call = bcx.ins().call(func_ref, &[nil, nil]);
                        Ok(bcx.inst_results(call)[0])
                    } else {
                        let nil = make_nil(bcx, rt);
                        Ok(make_closure(bcx, arity, func_ref, nil, rt))
                    }
                } else {
                    Err(format!("unbound `{name}`"))
                }
            }
            Expr::Lambda(_, _, span) => {
                let info = self
                    .lambda_info
                    .get(&span.lo)
                    .ok_or_else(|| "uncollected lambda".to_string())?;
                let func_ref = funcrefs
                    .get(&info.name)
                    .copied()
                    .ok_or_else(|| format!("unbound `{}`", info.name))?;
                let arity = self.arities.get(&info.name).copied().unwrap_or(0);
                // Build the captured environment (in `free_vars` order) from
                // the enclosing scope, retaining each captured value.
                let mut env_vals = Vec::new();
                for fv in &info.free_vars {
                    let var = vars
                        .get(fv)
                        .ok_or_else(|| format!("unbound `{fv}`"))?;
                    let val = bcx.use_var(*var);
                    bcx.ins().call(rt.retain, &[val]);
                    env_vals.push(val);
                }
                let mut env = make_nil(bcx, rt);
                for v in env_vals.iter().rev() {
                    let call = bcx.ins().call(rt.cons, &[*v, env]);
                    env = bcx.inst_results(call)[0];
                }
                Ok(make_closure(bcx, arity, func_ref, env, rt))
            }
            Expr::If(c, t, el, _) => {
                let cond = self.gen_expr(bcx, c, vars, funcrefs, rt, cells, None)?;
                let bit = unbox(bcx, cond);
                release(bcx, rt, cond);
                let then_b = bcx.create_block();
                let else_b = bcx.create_block();
                let merge_b = bcx.create_block();
                let merge_param = bcx.append_block_param(merge_b, types::I64);
                bcx.ins().brif(bit, then_b, &[], else_b, &[]);
                bcx.switch_to_block(then_b);
                let tv = self.gen_expr(bcx, t, vars, funcrefs, rt, cells, tail)?;
                bcx.ins().jump(merge_b, &[BlockArg::Value(tv)]);
                bcx.switch_to_block(else_b);
                let ev = self.gen_expr(bcx, el, vars, funcrefs, rt, cells, tail)?;
                bcx.ins().jump(merge_b, &[BlockArg::Value(ev)]);
                bcx.seal_block(then_b);
                bcx.seal_block(else_b);
                bcx.seal_block(merge_b);
                bcx.switch_to_block(merge_b);
                Ok(merge_param)
            }
            Expr::Let(bindings, body, _) => {
                let mut new_vars = vars.clone();
                let mut new_cells = cells.clone();
                let mut owned = Vec::new();
                for b in bindings {
                    match b.lhs.first() {
                        Some(Pattern::Var(name, _)) => {
                            let v = self.gen_expr(bcx, &b.expr, &new_vars, funcrefs, rt, cells, None)?;
                            let var = bcx.declare_var(types::I64);
                            let stored = if b.mutable {
                                // Mutable bindings live in a cell so closures
                                // capture (and mutate) the same storage.
                                let tag = bcx.ins().iconst(types::I64, TAG_CELL);
                                let one = bcx.ins().iconst(types::I64, 1);
                                let call = bcx.ins().call(rt.alloc, &[tag, one]);
                                let cell = bcx.inst_results(call)[0];
                                bcx.ins().store(MemFlagsData::new(), v, cell, 24);
                                new_cells.insert(name.clone());
                                cell
                            } else {
                                v
                            };
                            bcx.def_var(var, stored);
                            owned.push(stored);
                            new_vars.insert(name.clone(), var);
                        }
                        Some(Pattern::Wildcard(_)) => {
                            let v = self.gen_expr(bcx, &b.expr, &new_vars, funcrefs, rt, cells, None)?;
                            release(bcx, rt, v);
                        }
                        _ => {
                            return Err(
                                "`let` with a non-variable pattern is not yet supported".to_string(),
                            );
                        }
                    }
                }
                let result = self.gen_expr(bcx, body, &new_vars, funcrefs, rt, &new_cells, tail)?;
                for val in &owned {
                    release(bcx, rt, *val);
                }
                Ok(result)
            }
            Expr::Assign(name, e, _) => {
                let var = vars
                    .get(name)
                    .copied()
                    .ok_or_else(|| format!("unbound `{name}`"))?;
                if !cells.contains(name) {
                    return Err(format!(
                        "cannot assign to immutable variable `{name}`; declare it with `mut`"
                    ));
                }
                let v = self.gen_expr(bcx, e, vars, funcrefs, rt, cells, None)?;
                let cell = bcx.use_var(var);
                let old = bcx.ins().load(types::I64, MemFlagsData::new(), cell, 24);
                // The cell takes ownership of `v`; retain it once more for the
                // expression result (which the caller may release).
                bcx.ins().call(rt.retain, &[v]);
                bcx.ins().store(MemFlagsData::new(), v, cell, 24);
                release(bcx, rt, old);
                Ok(v)
            }
            Expr::Ascribe(e, _, _) => self.gen_expr(bcx, e, vars, funcrefs, rt, cells, tail),
            Expr::Case(scrut, alts, _) => self.gen_case(bcx, scrut, alts, vars, funcrefs, rt, cells, tail),
            Expr::App(..) => self.gen_apply(bcx, e, vars, funcrefs, rt, cells, tail),
            other => Err(format!("unsupported expression: {other:?}")),
        }
    }

    fn gen_case(
        &mut self,
        bcx: &mut FunctionBuilder,
        scrut: &Expr,
        alts: &[Alt],
        vars: &HashMap<String, Variable>,
        funcrefs: &HashMap<String, FuncRef>,
        rt: Rt,
        cells: &std::collections::HashSet<String>,
        tail: Option<(&str, usize, FuncRef, cranelift_codegen::ir::Block)>,
    ) -> Result<Value, String> {
        let sv = self.gen_expr(bcx, scrut, vars, funcrefs, rt, cells, None)?;
        let merge = bcx.create_block();
        let merge_param = bcx.append_block_param(merge, types::I64);
        let mut alt_blocks = Vec::new();
        for _ in alts {
            alt_blocks.push(bcx.create_block());
        }
        let trap = bcx.create_block();
        // Fall through from the scrutinee block into the first alternative.
        bcx.ins().jump(alt_blocks[0], &[]);
        for (i, alt) in alts.iter().enumerate() {
            let this = alt_blocks[i];
            let next = if i + 1 < alt_blocks.len() {
                alt_blocks[i + 1]
            } else {
                trap
            };
            bcx.switch_to_block(this);
            let mut avars = vars.clone();
            let mut owned = Vec::new();
            self.match_alt(bcx, &alt.pat, sv, &mut avars, &mut owned, next, rt)?;
            let body = self.gen_expr(bcx, &alt.body, &avars, funcrefs, rt, cells, tail)?;
            for val in &owned {
                release(bcx, rt, *val);
            }
            bcx.ins().jump(merge, &[BlockArg::Value(body)]);
            bcx.seal_block(this);
        }
        bcx.switch_to_block(trap);
        bcx.ins().trap(TrapCode::unwrap_user(1));
        bcx.seal_block(trap);
        bcx.seal_block(merge);
        bcx.switch_to_block(merge);
        // The scrutinee box is no longer needed once matching has completed.
        release(bcx, rt, sv);
        Ok(merge_param)
    }

    fn match_alt(
        &mut self,
        bcx: &mut FunctionBuilder,
        p: &Pattern,
        value: Value,
        vars: &mut HashMap<String, Variable>,
        owned: &mut Vec<Value>,
        next: cranelift_codegen::ir::Block,
        rt: Rt,
    ) -> Result<(), String> {
        match p {
            Pattern::Var(name, _) => {
                let v = bcx.declare_var(types::I64);
                bcx.ins().call(rt.retain, &[value]);
                owned.push(value);
                bcx.def_var(v, value);
                vars.insert(name.clone(), v);
            }
            Pattern::Wildcard(_) => {}
            Pattern::Int(n, _) => {
                let val = unbox(bcx, value);
                let lit = bcx.ins().iconst(types::I64, *n);
                let cond = bcx
                    .ins()
                    .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, val, lit);
                let cont = bcx.create_block();
                bcx.ins().brif(cond, cont, &[], next, &[]);
                bcx.switch_to_block(cont);
            }
            Pattern::Ctor { name, args, .. }
                if (name == "True" || name == "False") && args.is_empty() =>
            {
                let val = unbox(bcx, value);
                let lit = bcx
                    .ins()
                    .iconst(types::I64, if name == "True" { 1 } else { 0 });
                let cond = bcx
                    .ins()
                    .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, val, lit);
                let cont = bcx.create_block();
                bcx.ins().brif(cond, cont, &[], next, &[]);
                bcx.switch_to_block(cont);
            }
            Pattern::Ctor { name, args, .. } => {
                let ctor_idx = self
                    .info
                    .ctor_of(name)
                    .ok_or_else(|| format!("unknown constructor `{name}`"))?;
                let obj_tag = tag_of(bcx, value);
                let tagc = bcx.ins().iconst(types::I64, ctor_idx as i64);
                let cond = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    obj_tag,
                    tagc,
                );
                let cont = bcx.create_block();
                bcx.ins().brif(cond, cont, &[], next, &[]);
                bcx.switch_to_block(cont);
                for (k, ap) in args.iter().enumerate() {
                    let offset = 24 + (k as i64) * 8;
                    let fv = bcx
                        .ins()
                        .load(types::I64, MemFlagsData::new(), value, offset as i32);
                    self.bind_pattern(bcx, ap, fv, vars, owned, next, rt)?;
                }
            }
            Pattern::List(ps, _) if ps.is_empty() => {
                check_tag(bcx, value, TAG_NIL, next);
            }
            Pattern::List(ps, _) => {
                check_tag(bcx, value, TAG_CONS, next);
                let head = bcx.ins().load(types::I64, MemFlagsData::new(), value, 24);
                let tail = bcx.ins().load(types::I64, MemFlagsData::new(), value, 32);
                self.bind_pattern(bcx, &ps[0], head, vars, owned, next, rt)?;
                let rest = Pattern::List(ps[1..].to_vec(), crate::error::Span { lo: 0, hi: 0 });
                self.bind_pattern(bcx, &rest, tail, vars, owned, next, rt)?;
            }
            Pattern::Char(ch, _) => {
                let val = unbox(bcx, value);
                let lit = bcx.ins().iconst(types::I64, *ch as i64);
                let cond = bcx
                    .ins()
                    .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, val, lit);
                let cont = bcx.create_block();
                bcx.ins().brif(cond, cont, &[], next, &[]);
                bcx.switch_to_block(cont);
            }
            Pattern::Str(s, span) => {
                let bytes = self.declare_bytes(&format!("quill_strpat_{}", span.lo), s.as_bytes());
                let bytes_gv = self.module.declare_data_in_func(bytes, &mut bcx.func);
                let bytes_ptr = bcx.ins().symbol_value(types::I64, bytes_gv);
                let len = bcx.ins().iconst(types::I64, s.len() as i64);
                let call = bcx.ins().call(rt.str_eq, &[value, bytes_ptr, len]);
                let matched = bcx.inst_results(call)[0];
                let one = bcx.ins().iconst(types::I64, 1);
                let cond = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    matched,
                    one,
                );
                let cont = bcx.create_block();
                bcx.ins().brif(cond, cont, &[], next, &[]);
                bcx.switch_to_block(cont);
            }
            Pattern::Cons(h, t, _) => {
                self.bind_cons_pattern(bcx, h, t, value, vars, owned, next, rt)?;
            }
            Pattern::Tuple(ps, _) => {
                check_tag(bcx, value, TAG_TUPLE, next);
                for (k, ap) in ps.iter().enumerate() {
                    let offset = 24 + (k as i64) * 8;
                    let fv = bcx
                        .ins()
                        .load(types::I64, MemFlagsData::new(), value, offset as i32);
                    self.bind_pattern(bcx, ap, fv, vars, owned, next, rt)?;
                }
            }
            Pattern::Record { ctor, fields, .. } => {
                let ctor_idx = self
                    .info
                    .ctor_of(ctor)
                    .ok_or_else(|| format!("unknown constructor `{ctor}`"))?;
                let obj_tag = tag_of(bcx, value);
                let tagc = bcx.ins().iconst(types::I64, ctor_idx as i64);
                let cond = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    obj_tag,
                    tagc,
                );
                let cont = bcx.create_block();
                bcx.ins().brif(cond, cont, &[], next, &[]);
                bcx.switch_to_block(cont);
                let decl = self
                    .info
                    .ctor_fields
                    .get(ctor)
                    .cloned()
                    .unwrap_or_default();
                for (fname, sub) in fields {
                    let idx = decl.iter().position(|n| n == fname).ok_or_else(|| {
                        format!("constructor `{ctor}` has no field `{fname}`")
                    })?;
                    let offset = 24 + (idx as i64) * 8;
                    let fv = bcx
                        .ins()
                        .load(types::I64, MemFlagsData::new(), value, offset as i32);
                    self.bind_pattern(bcx, sub, fv, vars, owned, next, rt)?;
                }
            }
            other => return Err(format!("unsupported alt pattern: {other:?}")),
        }
        Ok(())
    }

    fn gen_apply(
        &mut self,
        bcx: &mut FunctionBuilder,
        e: &Expr,
        vars: &HashMap<String, Variable>,
        funcrefs: &HashMap<String, FuncRef>,
        rt: Rt,
        cells: &std::collections::HashSet<String>,
        tail: Option<(&str, usize, FuncRef, cranelift_codegen::ir::Block)>,
    ) -> Result<Value, String> {
        let mut args: Vec<Expr> = Vec::new();
        let mut head = e.clone();
        loop {
            match head {
                Expr::App(f, a, _) => {
                    args.push((*a).clone());
                    head = (*f).clone();
                }
                _ => break,
            }
        }
        args.reverse();

        // If the head names a builtin, constructor, or top-level function,
        // handle it directly. Otherwise it is a function value (a local
        // variable or a lambda), evaluated and applied argument by argument.
        let head_name = match &head {
            Expr::Var(n, _) | Expr::Ctor(n, _) => Some(n.clone()),
            _ => None,
        };

        if let Some(name) = head_name.as_deref() {
            if is_builtin(name) {
                return self.gen_builtin(bcx, name, &args, vars, funcrefs, rt, cells, None);
            }

            // Constructor application.
            if let Some(ctor_idx) = self.info.ctor_of(name) {
                let mut arg_vals = Vec::new();
                for a in &args {
                    arg_vals.push(self.gen_expr(bcx, a, vars, funcrefs, rt, cells, None)?);
                }
                let tag = bcx.ins().iconst(types::I64, ctor_idx as i64);
                let nfields = bcx.ins().iconst(types::I64, args.len() as i64);
                let call = bcx.ins().call(rt.alloc, &[tag, nfields]);
                let obj = bcx.inst_results(call)[0];
                for (k, v) in arg_vals.iter().enumerate() {
                    let offset = 24 + (k as i64) * 8;
                    bcx.ins().store(MemFlagsData::new(), *v, obj, offset as i32);
                }
                return Ok(obj);
            }

            if let Some(&callee) = funcrefs.get(name) {
                let arity = self.arities.get(name).copied().unwrap_or(0);
                if arity == 0 {
                    // A nullary top-level binding is a value; evaluate it, then
                    // apply any arguments to the (function) result.
                    let nil = make_nil(bcx, rt);
                    let call0 = bcx.ins().call(callee, &[nil, nil]);
                    let mut closure = bcx.inst_results(call0)[0];
                    release(bcx, rt, nil);
                    for a in &args {
                        let av = self.gen_expr(bcx, a, vars, funcrefs, rt, cells, None)?;
                        let call = bcx.ins().call(rt.apply, &[closure, av]);
                        let r = bcx.inst_results(call)[0];
                        release(bcx, rt, closure);
                        closure = r;
                    }
                    return Ok(closure);
                }
                let mut arg_vals = Vec::new();
                for a in &args {
                    arg_vals.push(self.gen_expr(bcx, a, vars, funcrefs, rt, cells, None)?);
                }
                if let Some((self_name, self_arity, _self_ref, tail_block)) = tail {
                    if name == self_name && args.len() == self_arity {
                        // A self-recursive call in tail position: jump to
                        // the clause's tail block (which releases the
                        // pattern-bound originals and then tail-jumps),
                        // so deep recursion runs in constant stack.
                        let list = build_args_list(bcx, &arg_vals, rt);
                        let nil = make_nil(bcx, rt);
                        let tb = bcx
                            .ins()
                            .jump(tail_block, &[BlockArg::Value(nil), BlockArg::Value(list)]);
                        let _ = tb;
                        let dead = bcx.create_block();
                        bcx.switch_to_block(dead);
                        let zero = bcx.ins().iconst(types::I64, 0);
                        return Ok(zero);
                    }
                }
                if args.len() == arity {
                    // Full application: call directly (the closure is unused).
                    let list = build_args_list(bcx, &arg_vals, rt);
                    let nil = make_nil(bcx, rt);
                    let call = bcx.ins().call(callee, &[nil, list]);
                    let r = bcx.inst_results(call)[0];
                    release(bcx, rt, nil);
                    return Ok(r);
                }
                if args.len() < arity {
                    // Partial application: a closure capturing the applied args.
                    let list = build_args_list(bcx, &arg_vals, rt);
                    let addr = bcx.ins().func_addr(types::I64, callee);
                    let arity_c = bcx.ins().iconst(types::I64, arity as i64);
                    let nargs_c = bcx.ins().iconst(types::I64, args.len() as i64);
                    let nil = make_nil(bcx, rt);
                    let call =
                        bcx.ins()
                            .call(rt.make_closure, &[arity_c, nargs_c, addr, list, nil]);
                    return Ok(bcx.inst_results(call)[0]);
                }
                // Over-application: apply the first `arity` arguments fully,
                // then apply the remaining arguments to the (function) result.
                let head_list = build_args_list(bcx, &arg_vals[..arity], rt);
                let nil = make_nil(bcx, rt);
                let call = bcx.ins().call(callee, &[nil, head_list]);
                let mut closure = bcx.inst_results(call)[0];
                release(bcx, rt, nil);
                for v in &arg_vals[arity..] {
                    let call = bcx.ins().call(rt.apply, &[closure, *v]);
                    let r = bcx.inst_results(call)[0];
                    release(bcx, rt, closure);
                    closure = r;
                }
                return Ok(closure);
            }

            // A class method: dispatch on the runtime tag of the first
            // argument (the interpreter dispatches on the runtime type).
            if let Some(impls) = self.method_dispatch.get(name).cloned() {
                let mut arg_vals = Vec::new();
                for a in &args {
                    arg_vals.push(self.gen_expr(bcx, a, vars, funcrefs, rt, cells, None)?);
                }
                if arg_vals.is_empty() {
                    return Err(format!("method `{name}` requires an argument"));
                }
                let recv_tag = tag_of(bcx, arg_vals[0]);
                let merge = bcx.create_block();
                let merge_param = bcx.append_block_param(merge, types::I64);
                let trap_b = bcx.create_block();
                // Blocks receive fall-through edges from later impls, so seal
                // them only after every edge is declared.
                let mut seal_later: Vec<cranelift_codegen::ir::Block> = Vec::new();
                let mut next_b = trap_b;
                let mut first = true;
                for (idx_list, fid) in impls.iter().rev() {
                    let check_b = bcx.create_block();
                    let call_b = bcx.create_block();
                    seal_later.push(check_b);
                    seal_later.push(call_b);
                    if first {
                        // Enter the dispatch from the (unterminated) current
                        // block of the caller.
                        bcx.ins().jump(check_b, &[]);
                        first = false;
                    }
                    bcx.switch_to_block(check_b);
                    for (k, idx) in idx_list.iter().enumerate() {
                        let idxc = bcx.ins().iconst(types::I64, *idx as i64);
                        let cond = bcx.ins().icmp(
                            cranelift_codegen::ir::condcodes::IntCC::Equal,
                            recv_tag,
                            idxc,
                        );
                        let matched_b = bcx.create_block();
                        let next_check = bcx.create_block();
                        seal_later.push(matched_b);
                        seal_later.push(next_check);
                        bcx.ins().brif(cond, matched_b, &[], next_check, &[]);
                        bcx.switch_to_block(matched_b);
                        bcx.ins().jump(call_b, &[]);
                        bcx.switch_to_block(next_check);
                        if k + 1 == idx_list.len() {
                            bcx.ins().jump(next_b, &[]);
                        }
                    }
                    bcx.switch_to_block(call_b);
                    let list = build_args_list(bcx, &arg_vals, rt);
                    let nil = make_nil(bcx, rt);
                    let fid_ref = self.module.declare_func_in_func(*fid, &mut bcx.func);
                    let call = bcx.ins().call(fid_ref, &[nil, list]);
                    let r = bcx.inst_results(call)[0];
                    release(bcx, rt, nil);
                    bcx.ins().jump(merge, &[BlockArg::Value(r)]);
                    next_b = check_b;
                }
                bcx.switch_to_block(trap_b);
                bcx.ins().trap(TrapCode::unwrap_user(4));
                for b in seal_later {
                    bcx.seal_block(b);
                }
                bcx.seal_block(trap_b);
                bcx.seal_block(merge);
                bcx.switch_to_block(merge);
                return Ok(merge_param);
            }
        }

        // A function value in head position (a local variable or a lambda):
        // evaluate it and apply the arguments one at a time.
        let mut closure = self.gen_expr(bcx, &head, vars, funcrefs, rt, cells, None)?;
        for a in &args {
            let av = self.gen_expr(bcx, a, vars, funcrefs, rt, cells, None)?;
            let call = bcx.ins().call(rt.apply, &[closure, av]);
            let r = bcx.inst_results(call)[0];
            release(bcx, rt, closure);
            closure = r;
        }
        Ok(closure)
    }

    fn gen_builtin(
        &mut self,
        bcx: &mut FunctionBuilder,
        op: &str,
        args: &[Expr],
        vars: &HashMap<String, Variable>,
        funcrefs: &HashMap<String, FuncRef>,
        rt: Rt,
        cells: &std::collections::HashSet<String>,
        _tail: Option<(&str, usize, FuncRef, cranelift_codegen::ir::Block)>,
    ) -> Result<Value, String> {
        let mut vals = Vec::new();
        for a in args {
            vals.push(self.gen_expr(bcx, a, vars, funcrefs, rt, cells, None)?);
        }
        match op {
            "+" | "-" | "*" | "/" | "%" => {
                if op == "%" {
                    let a = unbox(bcx, vals[0]);
                    let b = unbox(bcx, vals[1]);
                    let r = bcx.ins().srem(a, b);
                    release(bcx, rt, vals[0]);
                    release(bcx, rt, vals[1]);
                    return Ok(box_int(bcx, r, rt));
                }
                // Dispatch on the operand tag: Int or Float.
                let tag = tag_of(bcx, vals[0]);
                let int_tag = bcx.ins().iconst(types::I64, TAG_INT);
                let is_int = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    int_tag,
                );
                let int_block = bcx.create_block();
                let float_block = bcx.create_block();
                let merge_block = bcx.create_block();
                let merge_param = bcx.append_block_param(merge_block, types::I64);
                bcx.ins().brif(is_int, int_block, &[], float_block, &[]);
                bcx.switch_to_block(int_block);
                let a = unbox(bcx, vals[0]);
                let b = unbox(bcx, vals[1]);
                let r = match op {
                    "+" => bcx.ins().iadd(a, b),
                    "-" => bcx.ins().isub(a, b),
                    "*" => bcx.ins().imul(a, b),
                    "/" => bcx.ins().sdiv(a, b),
                    _ => unreachable!(),
                };
                let _v = r;
                let _boxed = box_int(bcx, _v, rt);
                bcx.ins().jump(merge_block, &[BlockArg::Value(_boxed)]);
                bcx.switch_to_block(float_block);
                let fa = unbox_float(bcx, vals[0]);
                let fb = unbox_float(bcx, vals[1]);
                let fr = match op {
                    "+" => bcx.ins().fadd(fa, fb),
                    "-" => bcx.ins().fsub(fa, fb),
                    "*" => bcx.ins().fmul(fa, fb),
                    "/" => bcx.ins().fdiv(fa, fb),
                    _ => unreachable!(),
                };
                let _v = fr;
                let _boxed = box_float(bcx, _v, rt);
                bcx.ins().jump(merge_block, &[BlockArg::Value(_boxed)]);
                bcx.seal_block(int_block);
                bcx.seal_block(float_block);
                bcx.seal_block(merge_block);
                bcx.switch_to_block(merge_block);
                release(bcx, rt, vals[0]);
                release(bcx, rt, vals[1]);
                Ok(merge_param)
            }
            "negate" => {
                let tag = tag_of(bcx, vals[0]);
                let int_tag = bcx.ins().iconst(types::I64, TAG_INT);
                let is_int = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    tag,
                    int_tag,
                );
                let int_block = bcx.create_block();
                let float_block = bcx.create_block();
                let merge_block = bcx.create_block();
                let merge_param = bcx.append_block_param(merge_block, types::I64);
                bcx.ins().brif(is_int, int_block, &[], float_block, &[]);
                bcx.switch_to_block(int_block);
                let a = unbox(bcx, vals[0]);
                let r = bcx.ins().ineg(a);
                let _v = r;
                let _boxed = box_int(bcx, _v, rt);
                bcx.ins().jump(merge_block, &[BlockArg::Value(_boxed)]);
                bcx.switch_to_block(float_block);
                let fa = unbox_float(bcx, vals[0]);
                let fr = bcx.ins().fneg(fa);
                let _v = fr;
                let _boxed = box_float(bcx, _v, rt);
                bcx.ins().jump(merge_block, &[BlockArg::Value(_boxed)]);
                bcx.seal_block(int_block);
                bcx.seal_block(float_block);
                bcx.seal_block(merge_block);
                bcx.switch_to_block(merge_block);
                release(bcx, rt, vals[0]);
                Ok(merge_param)
            }
            "==" | "/=" | "<" | "<=" | ">" | ">=" => {
                let ictag = tag_of(bcx, vals[0]);
                let int_tag = bcx.ins().iconst(types::I64, TAG_INT);
                let is_int = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    ictag,
                    int_tag,
                );
                let char_tag = bcx.ins().iconst(types::I64, TAG_CHAR);
                let is_char = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    ictag,
                    char_tag,
                );
                let is_int = bcx.ins().bor(is_int, is_char);
                let int_block = bcx.create_block();
                let float_block = bcx.create_block();
                let merge_block = bcx.create_block();
                let merge_param = bcx.append_block_param(merge_block, types::I64);
                bcx.ins().brif(is_int, int_block, &[], float_block, &[]);
                bcx.switch_to_block(int_block);
                let cc = match op {
                    "==" => cranelift_codegen::ir::condcodes::IntCC::Equal,
                    "/=" => cranelift_codegen::ir::condcodes::IntCC::NotEqual,
                    "<" => cranelift_codegen::ir::condcodes::IntCC::SignedLessThan,
                    "<=" => cranelift_codegen::ir::condcodes::IntCC::SignedLessThanOrEqual,
                    ">" => cranelift_codegen::ir::condcodes::IntCC::SignedGreaterThan,
                    ">=" => cranelift_codegen::ir::condcodes::IntCC::SignedGreaterThanOrEqual,
                    _ => unreachable!(),
                };
                let a = unbox(bcx, vals[0]);
                let b = unbox(bcx, vals[1]);
                let r = bcx.ins().icmp(cc, a, b);
                let _v = bcx.ins().uextend(types::I64, r);
                let _boxed = box_bool(bcx, _v, rt);
                bcx.ins().jump(merge_block, &[BlockArg::Value(_boxed)]);
                bcx.switch_to_block(float_block);
                let fcc = match op {
                    "==" => cranelift_codegen::ir::condcodes::FloatCC::Equal,
                    "/=" => cranelift_codegen::ir::condcodes::FloatCC::NotEqual,
                    "<" => cranelift_codegen::ir::condcodes::FloatCC::LessThan,
                    "<=" => cranelift_codegen::ir::condcodes::FloatCC::LessThanOrEqual,
                    ">" => cranelift_codegen::ir::condcodes::FloatCC::GreaterThan,
                    ">=" => cranelift_codegen::ir::condcodes::FloatCC::GreaterThanOrEqual,
                    _ => unreachable!(),
                };
                let fa = unbox_float(bcx, vals[0]);
                let fb = unbox_float(bcx, vals[1]);
                let fr = bcx.ins().fcmp(fcc, fa, fb);
                let _v = bcx.ins().uextend(types::I64, fr);
                let _boxed = box_bool(bcx, _v, rt);
                bcx.ins().jump(merge_block, &[BlockArg::Value(_boxed)]);
                bcx.seal_block(int_block);
                bcx.seal_block(float_block);
                bcx.seal_block(merge_block);
                bcx.switch_to_block(merge_block);
                release(bcx, rt, vals[0]);
                release(bcx, rt, vals[1]);
                Ok(merge_param)
            }
            "&&" | "||" => {
                let a = unbox(bcx, vals[0]);
                let b = unbox(bcx, vals[1]);
                let r = if op == "&&" {
                    bcx.ins().band(a, b)
                } else {
                    bcx.ins().bor(a, b)
                };
                release(bcx, rt, vals[0]);
                release(bcx, rt, vals[1]);
                Ok(box_bool(bcx, r, rt))
            }
            "not" => {
                let a = unbox(bcx, vals[0]);
                let r = bcx.ins().bxor_imm_u(a, 1);
                release(bcx, rt, vals[0]);
                Ok(box_bool(bcx, r, rt))
            }
            "++" => {
                let call = bcx.ins().call(rt.append, &[vals[0], vals[1]]);
                let r = bcx.inst_results(call)[0];
                release(bcx, rt, vals[0]);
                release(bcx, rt, vals[1]);
                Ok(r)
            }
            ":" => {
                // `cons` takes ownership of the head and tail (stores them),
                // so they are not released here.
                let call = bcx.ins().call(rt.cons, &[vals[0], vals[1]]);
                Ok(bcx.inst_results(call)[0])
            }
            "println" => {
                bcx.ins().call(rt.println, &[vals[0]]);
                release(bcx, rt, vals[0]);
                let zero = bcx.ins().iconst(types::I64, 0);
                Ok(box_int(bcx, zero, rt))
            }
            "print" => {
                bcx.ins().call(rt.print, &[vals[0]]);
                release(bcx, rt, vals[0]);
                let zero = bcx.ins().iconst(types::I64, 0);
                Ok(box_int(bcx, zero, rt))
            }
            "show" => {
                let call = bcx.ins().call(rt.show, &[vals[0]]);
                let r = bcx.inst_results(call)[0];
                release(bcx, rt, vals[0]);
                Ok(r)
            }
            "chr" => {
                // Byte (mod 256) to character.
                let n = unbox(bcx, vals[0]);
                let two56 = bcx.ins().iconst(types::I64, 256);
                let m = bcx.ins().srem(n, two56);
                let zero = bcx.ins().iconst(types::I64, 0);
                let neg = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::SignedLessThan,
                    m,
                    zero,
                );
                let neg_b = bcx.create_block();
                let pos_b = bcx.create_block();
                let merge_b = bcx.create_block();
                let merge_param = bcx.append_block_param(merge_b, types::I64);
                bcx.ins().brif(neg, neg_b, &[], pos_b, &[]);
                bcx.switch_to_block(neg_b);
                let adj = bcx.ins().iadd(m, two56);
                bcx.ins().jump(merge_b, &[BlockArg::Value(adj)]);
                bcx.switch_to_block(pos_b);
                bcx.ins().jump(merge_b, &[BlockArg::Value(m)]);
                bcx.seal_block(neg_b);
                bcx.seal_block(pos_b);
                bcx.seal_block(merge_b);
                bcx.switch_to_block(merge_b);
                release(bcx, rt, vals[0]);
                Ok(box_char(bcx, merge_param, rt))
            }
            "ord" => {
                // Character (its unboxed code point) to Int.
                let c = unbox(bcx, vals[0]);
                release(bcx, rt, vals[0]);
                Ok(box_int(bcx, c, rt))
            }
            "floor" | "ceil" => {
                let f = unbox_float(bcx, vals[0]);
                let r = if op == "floor" {
                    bcx.ins().floor(f)
                } else {
                    bcx.ins().ceil(f)
                };
                let i = bcx.ins().fcvt_to_sint_sat(types::I64, r);
                release(bcx, rt, vals[0]);
                Ok(box_int(bcx, i, rt))
            }
            "_readLine" => {
                // Internal for Std.IO: `None` at end of input, `Some line`
                // otherwise. Std.IO imports Std.Option, so the constructors
                // are always in the program when this is reached.
                let none_idx = self
                    .info
                    .ctor_of("Std.Option.None")
                    .ok_or_else(|| "`_readLine` requires Std.Option (import Std.IO)".to_string())?;
                let some_idx = self
                    .info
                    .ctor_of("Std.Option.Some")
                    .ok_or_else(|| "`_readLine` requires Std.Option (import Std.IO)".to_string())?;
                let call = bcx.ins().call(rt.read_line, &[]);
                let s = bcx.inst_results(call)[0];
                let zero = bcx.ins().iconst(types::I64, 0);
                let is_null = bcx.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::Equal,
                    s,
                    zero,
                );
                let none_b = bcx.create_block();
                let some_b = bcx.create_block();
                let merge_b = bcx.create_block();
                let merge_param = bcx.append_block_param(merge_b, types::I64);
                bcx.ins().brif(is_null, none_b, &[], some_b, &[]);
                bcx.switch_to_block(none_b);
                let tag0 = bcx.ins().iconst(types::I64, none_idx as i64);
                let n0 = bcx.ins().iconst(types::I64, 0);
                let none_call = bcx.ins().call(rt.alloc, &[tag0, n0]);
                let none_obj = bcx.inst_results(none_call)[0];
                bcx.ins().jump(merge_b, &[BlockArg::Value(none_obj)]);
                bcx.switch_to_block(some_b);
                let tag1 = bcx.ins().iconst(types::I64, some_idx as i64);
                let n1 = bcx.ins().iconst(types::I64, 1);
                let some_call = bcx.ins().call(rt.alloc, &[tag1, n1]);
                let some_obj = bcx.inst_results(some_call)[0];
                // The string box from quill_read_line becomes the field; the
                // constructor takes ownership of it.
                bcx.ins().store(MemFlagsData::new(), s, some_obj, 24);
                bcx.ins().jump(merge_b, &[BlockArg::Value(some_obj)]);
                bcx.seal_block(none_b);
                bcx.seal_block(some_b);
                bcx.seal_block(merge_b);
                bcx.switch_to_block(merge_b);
                Ok(merge_param)
            }
            "snoc" => {
                let call = bcx.ins().call(rt.str_snoc, &[vals[0], vals[1]]);
                let r = bcx.inst_results(call)[0];
                release(bcx, rt, vals[0]);
                release(bcx, rt, vals[1]);
                Ok(r)
            }
            "error" => {
                bcx.ins().trap(TrapCode::unwrap_user(2));
                // The trap terminates the block; emit the unreachable dummy
                // result in a fresh block.
                let dead = bcx.create_block();
                bcx.switch_to_block(dead);
                let zero = bcx.ins().iconst(types::I64, 0);
                Ok(box_int(bcx, zero, rt))
            }
            other => Err(format!("unhandled builtin `{other}`")),
        }
    }
}

fn plain_sig(arity: usize) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    for _ in 0..arity {
        sig.params.push(AbiParam::new(types::I64));
    }
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

/// The signature of a compiled Quill function (closure, args list) -> value.
/// These use the tail calling convention so self-recursive tail calls become
/// real jumps.
fn fn_sig(arity: usize) -> Signature {
    let mut sig = Signature::new(CallConv::Tail);
    for _ in 0..arity {
        sig.params.push(AbiParam::new(types::I64));
    }
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

/// Strip a module prefix, leaving the final name component.
fn short_name(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_string()
}

fn unit_sig(arity: usize) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    for _ in 0..arity {
        sig.params.push(AbiParam::new(types::I64));
    }
    sig
}

fn is_builtin(op: &str) -> bool {
    matches!(
        op,
        "+" | "-"
            | "*"
            | "/"
            | "%"
            | "negate"
            | "=="
            | "/="
            | "<"
            | "<="
            | ">"
            | ">="
            | "&&"
            | "||"
            | "not"
            | "++"
            | ":"
            | "println"
            | "print"
            | "show"
            | "error"
            | "chr"
            | "ord"
            | "floor"
            | "ceil"
            | "snoc"
            | "_readLine"
    )
}

/// Box a value with a reserved scalar tag (`TAG_INT`/`TAG_BOOL`).
fn box_value(bcx: &mut FunctionBuilder, tag: i64, v: Value, rt: Rt) -> Value {
    let t = bcx.ins().iconst(types::I64, tag);
    let call = bcx.ins().call(rt.box_value, &[t, v]);
    bcx.inst_results(call)[0]
}

fn box_int(bcx: &mut FunctionBuilder, v: Value, rt: Rt) -> Value {
    box_value(bcx, TAG_INT, v, rt)
}

fn box_bool(bcx: &mut FunctionBuilder, v: Value, rt: Rt) -> Value {
    box_value(bcx, TAG_BOOL, v, rt)
}

/// Box a character's byte value as a `TAG_CHAR` box.
fn box_char(bcx: &mut FunctionBuilder, byte: Value, rt: Rt) -> Value {
    box_value(bcx, TAG_CHAR, byte, rt)
}

/// Box an f64 value (given as an `F64` SSA value) as a `TAG_FLOAT` box.
fn box_float(bcx: &mut FunctionBuilder, f: Value, rt: Rt) -> Value {
    let bits = bcx.ins().bitcast(types::I64, MemFlagsData::new(), f);
    box_value(bcx, TAG_FLOAT, bits, rt)
}

/// Read the single-payload word of a boxed `Int`/`Bool` (offset 24, after the
/// 3-word header).
fn unbox(bcx: &mut FunctionBuilder, v: Value) -> Value {
    bcx.ins().load(types::I64, MemFlagsData::new(), v, 24)
}

/// Read a `TAG_FLOAT` box's payload as an `F64` SSA value.
fn unbox_float(bcx: &mut FunctionBuilder, v: Value) -> Value {
    let bits = unbox(bcx, v);
    bcx.ins().bitcast(types::F64, MemFlagsData::new(), bits)
}

/// Read a box's tag (offset 8).
fn tag_of(bcx: &mut FunctionBuilder, v: Value) -> Value {
    bcx.ins().load(types::I64, MemFlagsData::new(), v, 8)
}

/// Emit a tag check: if `value` has tag `tag`, fall through to the returned
/// (already-switched-to) continuation block; otherwise jump to `next`.
fn check_tag(
    bcx: &mut FunctionBuilder,
    value: Value,
    tag: i64,
    next: cranelift_codegen::ir::Block,
) -> cranelift_codegen::ir::Block {
    let tagv = tag_of(bcx, value);
    let tagc = bcx.ins().iconst(types::I64, tag);
    let cond = bcx.ins().icmp(
        cranelift_codegen::ir::condcodes::IntCC::Equal,
        tagv,
        tagc,
    );
    let cont = bcx.create_block();
    bcx.ins().brif(cond, cont, &[], next, &[]);
    bcx.switch_to_block(cont);
    cont
}

/// Release a box (decrement its refcount).
fn release(bcx: &mut FunctionBuilder, rt: Rt, v: Value) {
    bcx.ins().call(rt.release, &[v]);
}

/// Allocate the empty list.
fn make_nil(bcx: &mut FunctionBuilder, rt: Rt) -> Value {
    let nil_tag = bcx.ins().iconst(types::I64, TAG_NIL);
    let zero = bcx.ins().iconst(types::I64, 0);
    let call = bcx.ins().call(rt.alloc, &[nil_tag, zero]);
    bcx.inst_results(call)[0]
}

/// Create a closure for a function value with a (possibly empty) captured
/// environment.
fn make_closure(
    bcx: &mut FunctionBuilder,
    arity: usize,
    func_ref: cranelift_codegen::ir::FuncRef,
    env: Value,
    rt: Rt,
) -> Value {
    let addr = bcx.ins().func_addr(types::I64, func_ref);
    let arity_c = bcx.ins().iconst(types::I64, arity as i64);
    let zero = bcx.ins().iconst(types::I64, 0);
    let nil = make_nil(bcx, rt);
    let call = bcx
        .ins()
        .call(rt.make_closure, &[arity_c, zero, addr, nil, env]);
    bcx.inst_results(call)[0]
}

/// Split a boxed args-list into `arity` argument values, retaining each element
/// (so the caller's list can be released without freeing the arguments) and
/// releasing the cons-cell plumbing. The list is stored in reverse application
/// order (most recent argument first), so the result is reversed here.
fn destructure_args(
    bcx: &mut FunctionBuilder,
    args_list: Value,
    arity: usize,
    rt: Rt,
) -> Vec<Value> {
    let mut out = Vec::with_capacity(arity);
    let mut cur = args_list;
    for _ in 0..arity {
        let head = bcx.ins().load(types::I64, MemFlagsData::new(), cur, 24);
        let tail = bcx.ins().load(types::I64, MemFlagsData::new(), cur, 32);
        bcx.ins().call(rt.retain, &[head]);
        out.push(head);
        cur = tail;
    }
    bcx.ins().call(rt.release, &[args_list]);
    out.reverse();
    out
}

/// Build a boxed args-list from evaluated argument values, in reverse
/// application order (most recent argument first, to match `quill_apply`).
fn build_args_list(bcx: &mut FunctionBuilder, arg_vals: &[Value], rt: Rt) -> Value {
    let nil_tag = bcx.ins().iconst(types::I64, TAG_NIL);
    let zero = bcx.ins().iconst(types::I64, 0);
    let call = bcx.ins().call(rt.alloc, &[nil_tag, zero]);
    let mut list = bcx.inst_results(call)[0];
    for v in arg_vals {
        let call = bcx.ins().call(rt.cons, &[*v, list]);
        list = bcx.inst_results(call)[0];
    }
    list
}

/// Collect every lambda expression reachable from `expr` (including nested
/// ones), as `(span.lo, params, body, span)`.
fn collect_lambdas(
    expr: &Expr,
    out: &mut Vec<(usize, Vec<Pattern>, Box<Expr>, crate::error::Span)>,
) {
    match expr {
        Expr::Lambda(params, body, span) => {
            out.push((span.lo, params.clone(), body.clone(), *span));
            collect_lambdas(body, out);
        }
        Expr::App(f, a, _) => {
            collect_lambdas(f, out);
            collect_lambdas(a, out);
        }
        Expr::Let(bindings, body, _) => {
            for b in bindings {
                collect_lambdas(&b.expr, out);
            }
            collect_lambdas(body, out);
        }
        Expr::If(c, t, e, _) => {
            collect_lambdas(c, out);
            collect_lambdas(t, out);
            collect_lambdas(e, out);
        }
        Expr::Case(s, alts, _) => {
            collect_lambdas(s, out);
            for alt in alts {
                collect_lambdas(&alt.body, out);
            }
        }
        Expr::Ascribe(e, _, _) => collect_lambdas(e, out),
        Expr::Tuple(es, _) | Expr::List(es, _) => {
            for e in es {
                collect_lambdas(e, out);
            }
        }
        Expr::Borrow { expr, .. }
        | Expr::Deref(expr, _)
        | Expr::Clone(expr, _)
        | Expr::Field(expr, _, _) => collect_lambdas(expr, out),
        Expr::Record { fields, .. } => {
            for (_, e) in fields {
                collect_lambdas(e, out);
            }
        }
        _ => {}
    }
}

/// Compute the free variables of a lambda body: variables used in `body` that
/// are not bound by the lambda's own parameters (nor by inner binders), in
/// first-use order.
fn lambda_free_vars(params: &[Pattern], body: &Expr) -> Vec<String> {
    let mut bound: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in params {
        pat_vars(p, &mut bound);
    }
    let mut free: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    free_vars_of(body, &bound, &mut free, &mut seen);
    free
}

/// Names assigned (`:=`) anywhere in an expression, not shadowed by inner
/// binders. A captured variable that is assigned must be a cell so the
/// mutation is shared between the closure and the enclosing scope.
fn assigned_names(expr: &Expr) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    assigned_names_of(expr, &std::collections::HashSet::new(), &mut out);
    out
}

fn assigned_names_of(
    expr: &Expr,
    bound: &std::collections::HashSet<String>,
    out: &mut std::collections::HashSet<String>,
) {
    match expr {
        Expr::Assign(n, e, _) => {
            if !bound.contains(n) {
                out.insert(n.clone());
            }
            assigned_names_of(e, bound, out);
        }
        Expr::Lambda(params, body, _) => {
            let mut nb = bound.clone();
            for p in params {
                pat_vars(p, &mut nb);
            }
            assigned_names_of(body, &nb, out);
        }
        Expr::App(f, a, _) => {
            assigned_names_of(f, bound, out);
            assigned_names_of(a, bound, out);
        }
        Expr::Let(bindings, body, _) => {
            for b in bindings {
                assigned_names_of(&b.expr, bound, out);
            }
            let mut nb = bound.clone();
            for b in bindings {
                if let Some(n) = b.name() {
                    nb.insert(n.to_string());
                }
            }
            assigned_names_of(body, &nb, out);
        }
        Expr::If(c, t, e, _) => {
            assigned_names_of(c, bound, out);
            assigned_names_of(t, bound, out);
            assigned_names_of(e, bound, out);
        }
        Expr::Case(s, alts, _) => {
            assigned_names_of(s, bound, out);
            for alt in alts {
                let mut nb = bound.clone();
                pat_vars(&alt.pat, &mut nb);
                assigned_names_of(&alt.body, &nb, out);
            }
        }
        Expr::Ascribe(e, _, _) => assigned_names_of(e, bound, out),
        Expr::Tuple(es, _) | Expr::List(es, _) => {
            for e in es {
                assigned_names_of(e, bound, out);
            }
        }
        Expr::Borrow { expr, .. }
        | Expr::Deref(expr, _)
        | Expr::Clone(expr, _)
        | Expr::Field(expr, _, _) => assigned_names_of(expr, bound, out),
        Expr::Record { fields, .. } => {
            for (_, e) in fields {
                assigned_names_of(e, bound, out);
            }
        }
        _ => {}
    }
}

/// Add every variable bound by a pattern (recursively) to `out`.
fn pat_vars(p: &Pattern, out: &mut std::collections::HashSet<String>) {
    match p {
        Pattern::Var(n, _) => {
            out.insert(n.clone());
        }
        Pattern::Ctor { args, .. } | Pattern::Tuple(args, _) | Pattern::List(args, _) => {
            for a in args {
                pat_vars(a, out);
            }
        }
        Pattern::Cons(h, t, _) => {
            pat_vars(h, out);
            pat_vars(t, out);
        }
        Pattern::As(n, inner, _) => {
            out.insert(n.clone());
            pat_vars(inner, out);
        }
        Pattern::Record { fields, .. } => {
            for (_, p) in fields {
                pat_vars(p, out);
            }
        }
        _ => {}
    }
}

/// Collect the free variables of `expr` (used but not in `bound`), pushing new
/// names to `out` in first-use order and deduplicating via `seen`.
fn free_vars_of(
    expr: &Expr,
    bound: &std::collections::HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    match expr {
        Expr::Var(n, _) => {
            if !bound.contains(n) && seen.insert(n.clone()) {
                out.push(n.clone());
            }
        }
        Expr::Lambda(params, body, _) => {
            let mut nb = bound.clone();
            for p in params {
                pat_vars(p, &mut nb);
            }
            free_vars_of(body, &nb, out, seen);
        }
        Expr::App(f, a, _) => {
            free_vars_of(f, bound, out, seen);
            free_vars_of(a, bound, out, seen);
        }
        Expr::Assign(n, e, _) => {
            // The assigned variable is also a read of the (captured) binding.
            if !bound.contains(n) && seen.insert(n.clone()) {
                out.push(n.clone());
            }
            free_vars_of(e, bound, out, seen);
        }
        Expr::Let(bindings, body, _) => {
            for b in bindings {
                free_vars_of(&b.expr, bound, out, seen);
            }
            let mut nb = bound.clone();
            for b in bindings {
                if let Some(n) = b.name() {
                    nb.insert(n.to_string());
                }
            }
            free_vars_of(body, &nb, out, seen);
        }
        Expr::If(c, t, e, _) => {
            free_vars_of(c, bound, out, seen);
            free_vars_of(t, bound, out, seen);
            free_vars_of(e, bound, out, seen);
        }
        Expr::Case(s, alts, _) => {
            free_vars_of(s, bound, out, seen);
            for alt in alts {
                let mut nb = bound.clone();
                pat_vars(&alt.pat, &mut nb);
                free_vars_of(&alt.body, &nb, out, seen);
            }
        }
        Expr::Ascribe(e, _, _) => free_vars_of(e, bound, out, seen),
        Expr::Tuple(es, _) | Expr::List(es, _) => {
            for e in es {
                free_vars_of(e, bound, out, seen);
            }
        }
        Expr::Borrow { expr, .. }
        | Expr::Deref(expr, _)
        | Expr::Clone(expr, _)
        | Expr::Field(expr, _, _) => free_vars_of(expr, bound, out, seen),
        Expr::Record { fields, .. } => {
            for (_, e) in fields {
                free_vars_of(e, bound, out, seen);
            }
        }
        _ => {}
    }
}

/// Allocate a string box from a Rust string literal and fill in its bytes.
fn gen_str_literal(bcx: &mut FunctionBuilder, s: &str, rt: Rt) -> Value {
    let len = bcx.ins().iconst(types::I64, s.len() as i64);
    let call = bcx.ins().call(rt.str_alloc, &[len]);
    let p = bcx.inst_results(call)[0];
    for (i, chunk) in s.as_bytes().chunks(8).enumerate() {
        let mut word: u64 = 0;
        for (j, b) in chunk.iter().enumerate() {
            word |= u64::from(*b) << (j * 8);
        }
        let val = bcx.ins().iconst(types::I64, word as i64);
        let offset = 24 + (i as i32) * 8;
        bcx.ins().store(MemFlagsData::new(), val, p, offset);
    }
    p
}
