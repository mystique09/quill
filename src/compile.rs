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
//! captured-args list); and each ADT constructor uses its constructor index
//! (>= 0) as the tag, with the fields as payload. Because every value is a
//! pointer, polymorphic functions are reference-counted uniformly.
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
use crate::infer::TypeInfo;

type FuncRef = cranelift_codegen::ir::FuncRef;

/// Reserved tags for the boxed scalar types. ADT constructor tags are the
/// constructor index (>= 0) and never collide with these.
const TAG_INT: i64 = -1;
const TAG_BOOL: i64 = -2;
const TAG_STR: i64 = -3;
const TAG_NIL: i64 = -4;
const TAG_CONS: i64 = -5;
const TAG_FUN: i64 = -6;

struct CodegenInfo {
    ctors: HashMap<String, (usize, usize)>,
}

impl CodegenInfo {
    fn from(types: &TypeInfo) -> Self {
        let mut ctors = HashMap::new();
        for (adt_id, adt) in types.adts.iter().enumerate() {
            for (i, c) in adt.ctors.iter().enumerate() {
                ctors.insert(c.name.clone(), (adt_id, i));
            }
        }
        CodegenInfo { ctors }
    }

    fn ctor_of(&self, name: &str) -> Option<(usize, usize)> {
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
    cons: FuncRef,
    append: FuncRef,
    make_closure: FuncRef,
    apply: FuncRef,
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

        self.emit_runtime(malloc, free, memcpy, puts)?;

        let mut func_ids: HashMap<String, cranelift_module::FuncId> = HashMap::new();
        for item in &program.items {
            if let Item::Function(f) = item {
                let sym = format!("q_{}", f.name);
                let id = self
                    .module
                    .declare_function(&sym, Linkage::Local, &plain_sig(2))
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
        let mut lambda_fns: Vec<(Function, Vec<String>)> = Vec::new();
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
                .declare_function(&name, Linkage::Local, &plain_sig(2))
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
            ));
        }

        let items: Vec<Item> = program.items.clone();
        for item in &items {
            if let Item::Function(f) = item {
                let id = func_ids[&f.name];
                let arity = self.arities[&f.name];
                self.define_function(id, f, arity, &[], &func_ids)?;
            }
        }
        for (lambda_fn, free_vars) in &lambda_fns {
            let id = func_ids[&lambda_fn.name];
            let arity = self.arities[&lambda_fn.name];
            self.define_function(id, lambda_fn, arity, free_vars, &func_ids)?;
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

    fn emit_runtime(
        &mut self,
        malloc: cranelift_module::FuncId,
        free: cranelift_module::FuncId,
        memcpy: cranelift_module::FuncId,
        puts: cranelift_module::FuncId,
    ) -> Result<(), String> {
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
                let entry_sig = bcx.import_signature(plain_sig(2));
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

                // Int/Bool/String boxes have a scalar/byte payload; ADTs have
                // pointer fields. Only release the payload of an ADT.
                let tag = bcx.ins().load(types::I64, MemFlagsData::new(), p, 8);
                let int_tag = bcx.ins().iconst(types::I64, TAG_INT);
                let bool_tag = bcx.ins().iconst(types::I64, TAG_BOOL);
                let str_tag = bcx.ins().iconst(types::I64, TAG_STR);
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
                let is_num = bcx.ins().bor(is_int, is_bool);
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
                bcx.ins().call(release_ref, &[args_list]);
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
        func_ids: &HashMap<String, cranelift_module::FuncId>,
    ) -> Result<(), String> {
        let target_config = self.module.target_config();
        let mut ctx = self.module.make_context();
        ctx.func.name = UserFuncName::testcase(&f.name);
        // Every function takes the closure that invoked it (used to read the
        // captured environment) and a boxed list of its actual arguments. This
        // uniform convention is what lets closures apply a function value
        // generically.
        ctx.func.signature = plain_sig(2);

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
            cons: self.declare_rt_in(&mut ctx, "quill_cons", &plain_sig(2)),
            append: self.declare_rt_in(&mut ctx, "quill_append", &plain_sig(2)),
            make_closure: self.declare_rt_in(&mut ctx, "quill_make_closure", &plain_sig(5)),
            apply: self.declare_rt_in(&mut ctx, "quill_apply", &plain_sig(2)),
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
                            let (_, ctor_idx) = self
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
                            // The matched object itself is consumed.
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
                        Pattern::Cons(h, t, _) => {
                            check_tag(&mut bcx, arg_values[j], TAG_CONS, next_block);
                            let head = bcx.ins().load(types::I64, MemFlagsData::new(), arg_values[j], 24);
                            let tail = bcx.ins().load(types::I64, MemFlagsData::new(), arg_values[j], 32);
                            self.bind_pattern(&mut bcx, h, head, &mut vars, &mut owned, next_block, rt)?;
                            self.bind_pattern(&mut bcx, t, tail, &mut vars, &mut owned, next_block, rt)?;
                            owned.push(arg_values[j]);
                        }
                        other => return Err(format!("unsupported pattern: {other:?}")),
                    }
                }

                let body = self.gen_expr(&mut bcx, &clause.body, &vars, &funcrefs, rt)?;

                for val in &owned {
                    release(&mut bcx, rt, *val);
                }

                bcx.ins().jump(merge_block, &[BlockArg::Value(body)]);
                bcx.seal_block(this_block);
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

    /// Bind a sub-pattern (used for constructor fields). A bound field is
    /// retained because the binding shares the object. `next` is the block to
    /// jump to when the pattern does not match.
    fn bind_pattern(
        &self,
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
                let (_, ctor_idx) = self
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
            Pattern::Cons(h, t, _) => {
                check_tag(bcx, value, TAG_CONS, next);
                let head = bcx.ins().load(types::I64, MemFlagsData::new(), value, 24);
                let tail = bcx.ins().load(types::I64, MemFlagsData::new(), value, 32);
                self.bind_pattern(bcx, h, head, vars, owned, next, rt)?;
                self.bind_pattern(bcx, t, tail, vars, owned, next, rt)?;
            }
            other => return Err(format!("unsupported nested pattern: {other:?}")),
        }
        Ok(())
    }

    fn gen_expr(
        &self,
        bcx: &mut FunctionBuilder,
        e: &Expr,
        vars: &HashMap<String, Variable>,
        funcrefs: &HashMap<String, FuncRef>,
        rt: Rt,
    ) -> Result<Value, String> {
        match e {
            Expr::Int(n, _) => {
                let nv = bcx.ins().iconst(types::I64, *n);
                Ok(box_int(bcx, nv, rt))
            }
            Expr::Str(s, _) => Ok(gen_str_literal(bcx, s, rt)),
            Expr::Unit(_) => {
                // `()` has no native representation of its own; use a boxed 0.
                let zero = bcx.ins().iconst(types::I64, 0);
                Ok(box_int(bcx, zero, rt))
            }
            Expr::List(es, _) => {
                // Build from the tail: `[]` then fold cons from right to left.
                let nil_tag = bcx.ins().iconst(types::I64, TAG_NIL);
                let zero = bcx.ins().iconst(types::I64, 0);
                let call = bcx.ins().call(rt.alloc, &[nil_tag, zero]);
                let mut acc = bcx.inst_results(call)[0];
                for e in es.iter().rev() {
                    let v = self.gen_expr(bcx, e, vars, funcrefs, rt)?;
                    let call = bcx.ins().call(rt.cons, &[v, acc]);
                    acc = bcx.inst_results(call)[0];
                }
                Ok(acc)
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
                let (_, ctor_idx) = self
                    .info
                    .ctor_of(name)
                    .ok_or_else(|| format!("unknown constructor `{name}`"))?;
                let tag = bcx.ins().iconst(types::I64, ctor_idx as i64);
                let nfields = bcx.ins().iconst(types::I64, 0);
                let call = bcx.ins().call(rt.alloc, &[tag, nfields]);
                Ok(bcx.inst_results(call)[0])
            }
            Expr::Var(name, _) => {
                if let Some(var) = vars.get(name) {
                    let val = bcx.use_var(*var);
                    bcx.ins().call(rt.retain, &[val]);
                    Ok(val)
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
                let cond = self.gen_expr(bcx, c, vars, funcrefs, rt)?;
                let bit = unbox(bcx, cond);
                release(bcx, rt, cond);
                let then_b = bcx.create_block();
                let else_b = bcx.create_block();
                let merge_b = bcx.create_block();
                let merge_param = bcx.append_block_param(merge_b, types::I64);
                bcx.ins().brif(bit, then_b, &[], else_b, &[]);
                bcx.switch_to_block(then_b);
                let tv = self.gen_expr(bcx, t, vars, funcrefs, rt)?;
                bcx.ins().jump(merge_b, &[BlockArg::Value(tv)]);
                bcx.switch_to_block(else_b);
                let ev = self.gen_expr(bcx, el, vars, funcrefs, rt)?;
                bcx.ins().jump(merge_b, &[BlockArg::Value(ev)]);
                bcx.seal_block(then_b);
                bcx.seal_block(else_b);
                bcx.seal_block(merge_b);
                bcx.switch_to_block(merge_b);
                Ok(merge_param)
            }
            Expr::Let(bindings, body, _) => {
                let mut new_vars = vars.clone();
                let mut owned = Vec::new();
                for b in bindings {
                    match b.lhs.first() {
                        Some(Pattern::Var(name, _)) => {
                            let v = self.gen_expr(bcx, &b.expr, &new_vars, funcrefs, rt)?;
                            let var = bcx.declare_var(types::I64);
                            bcx.def_var(var, v);
                            owned.push(v);
                            new_vars.insert(name.clone(), var);
                        }
                        _ => {
                            return Err(
                                "`let` with a non-variable pattern is not yet supported".to_string(),
                            );
                        }
                    }
                }
                let result = self.gen_expr(bcx, body, &new_vars, funcrefs, rt)?;
                for val in &owned {
                    release(bcx, rt, *val);
                }
                Ok(result)
            }
            Expr::Ascribe(e, _, _) => self.gen_expr(bcx, e, vars, funcrefs, rt),
            Expr::Case(scrut, alts, _) => self.gen_case(bcx, scrut, alts, vars, funcrefs, rt),
            Expr::App(..) => self.gen_apply(bcx, e, vars, funcrefs, rt),
            other => Err(format!("unsupported expression: {other:?}")),
        }
    }

    fn gen_case(
        &self,
        bcx: &mut FunctionBuilder,
        scrut: &Expr,
        alts: &[Alt],
        vars: &HashMap<String, Variable>,
        funcrefs: &HashMap<String, FuncRef>,
        rt: Rt,
    ) -> Result<Value, String> {
        let sv = self.gen_expr(bcx, scrut, vars, funcrefs, rt)?;
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
            let body = self.gen_expr(bcx, &alt.body, &avars, funcrefs, rt)?;
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
        &self,
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
                let (_, ctor_idx) = self
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
            Pattern::Cons(h, t, _) => {
                check_tag(bcx, value, TAG_CONS, next);
                let head = bcx.ins().load(types::I64, MemFlagsData::new(), value, 24);
                let tail = bcx.ins().load(types::I64, MemFlagsData::new(), value, 32);
                self.bind_pattern(bcx, h, head, vars, owned, next, rt)?;
                self.bind_pattern(bcx, t, tail, vars, owned, next, rt)?;
            }
            other => return Err(format!("unsupported alt pattern: {other:?}")),
        }
        Ok(())
    }

    fn gen_apply(
        &self,
        bcx: &mut FunctionBuilder,
        e: &Expr,
        vars: &HashMap<String, Variable>,
        funcrefs: &HashMap<String, FuncRef>,
        rt: Rt,
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
                return self.gen_builtin(bcx, name, &args, vars, funcrefs, rt);
            }

            // Constructor application.
            if let Some((_, ctor_idx)) = self.info.ctor_of(name) {
                let mut arg_vals = Vec::new();
                for a in &args {
                    arg_vals.push(self.gen_expr(bcx, a, vars, funcrefs, rt)?);
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
                    for a in &args {
                        let av = self.gen_expr(bcx, a, vars, funcrefs, rt)?;
                        let call = bcx.ins().call(rt.apply, &[closure, av]);
                        let r = bcx.inst_results(call)[0];
                        release(bcx, rt, closure);
                        closure = r;
                    }
                    return Ok(closure);
                }
                let mut arg_vals = Vec::new();
                for a in &args {
                    arg_vals.push(self.gen_expr(bcx, a, vars, funcrefs, rt)?);
                }
                if args.len() == arity {
                    // Full application: call directly (the closure is unused).
                    let list = build_args_list(bcx, &arg_vals, rt);
                    let nil = make_nil(bcx, rt);
                    let call = bcx.ins().call(callee, &[nil, list]);
                    return Ok(bcx.inst_results(call)[0]);
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
                for v in &arg_vals[arity..] {
                    let call = bcx.ins().call(rt.apply, &[closure, *v]);
                    let r = bcx.inst_results(call)[0];
                    release(bcx, rt, closure);
                    closure = r;
                }
                return Ok(closure);
            }
        }

        // A function value in head position (a local variable or a lambda):
        // evaluate it and apply the arguments one at a time.
        let mut closure = self.gen_expr(bcx, &head, vars, funcrefs, rt)?;
        for a in &args {
            let av = self.gen_expr(bcx, a, vars, funcrefs, rt)?;
            let call = bcx.ins().call(rt.apply, &[closure, av]);
            let r = bcx.inst_results(call)[0];
            release(bcx, rt, closure);
            closure = r;
        }
        Ok(closure)
    }

    fn gen_builtin(
        &self,
        bcx: &mut FunctionBuilder,
        op: &str,
        args: &[Expr],
        vars: &HashMap<String, Variable>,
        funcrefs: &HashMap<String, FuncRef>,
        rt: Rt,
    ) -> Result<Value, String> {
        let mut vals = Vec::new();
        for a in args {
            vals.push(self.gen_expr(bcx, a, vars, funcrefs, rt)?);
        }
        match op {
            "+" | "-" | "*" | "/" | "%" => {
                let a = unbox(bcx, vals[0]);
                let b = unbox(bcx, vals[1]);
                let r = match op {
                    "+" => bcx.ins().iadd(a, b),
                    "-" => bcx.ins().isub(a, b),
                    "*" => bcx.ins().imul(a, b),
                    "/" => bcx.ins().sdiv(a, b),
                    "%" => bcx.ins().srem(a, b),
                    _ => unreachable!(),
                };
                release(bcx, rt, vals[0]);
                release(bcx, rt, vals[1]);
                Ok(box_int(bcx, r, rt))
            }
            "negate" => {
                let a = unbox(bcx, vals[0]);
                let r = bcx.ins().ineg(a);
                release(bcx, rt, vals[0]);
                Ok(box_int(bcx, r, rt))
            }
            "==" | "/=" | "<" | "<=" | ">" | ">=" => {
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
                release(bcx, rt, vals[0]);
                release(bcx, rt, vals[1]);
                Ok(box_bool(bcx, r, rt))
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
            "error" => {
                bcx.ins().trap(TrapCode::unwrap_user(2));
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
            | "error"
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

/// Read the single-payload word of a boxed `Int`/`Bool` (offset 24, after the
/// 3-word header).
fn unbox(bcx: &mut FunctionBuilder, v: Value) -> Value {
    bcx.ins().load(types::I64, MemFlagsData::new(), v, 24)
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
