//! Native code generation via Cranelift.
//!
//! Compiles a type-checked program into a relocatable object file for later
//! linking. Functions are compiled uncurried: a top-level function of arity N
//! becomes a single native function of N parameters (partial application and
//! closures come in a later increment).
//!
//! Value representation (grows with the compiler):
//!   Int  -> i64
//!   Bool -> i8  (0 = false, 1 = true)

use std::collections::HashMap;

use cranelift_codegen::ir::{types, AbiParam, BlockArg, InstBuilder, Signature, TrapCode, UserFuncName, Value};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use target_lexicon::Triple;

use crate::ast::*;
use crate::infer::TypeInfo;

/// The representation type of a value in generated code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rep {
    Int,
    Bool,
}

impl Rep {
    fn cl_type(self) -> types::Type {
        match self {
            Rep::Int => types::I64,
            Rep::Bool => types::I8,
        }
    }
}

pub fn compile_object(program: &Program, types: &TypeInfo) -> Result<Vec<u8>, String> {
    let _ = types;
    let mut flag_builder = settings::builder();
    flag_builder.set("use_colocated_libcalls", "false").unwrap();
    flag_builder.set("is_pic", "false").unwrap();
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
        returns: HashMap::new(),
    };
    c.collect_signatures(program)?;
    c.compile_program(program)
}

struct Compiler {
    module: ObjectModule,
    arities: HashMap<String, usize>,
    returns: HashMap<String, Rep>,
}

impl Compiler {
    fn collect_signatures(&mut self, program: &Program) -> Result<(), String> {
        for item in &program.items {
            if let Item::Function(f) = item {
                let arity = f.clauses.first().map(|c| c.pats.len()).unwrap_or(0);
                self.arities.insert(f.name.clone(), arity);
                let ret = f
                    .clauses
                    .first()
                    .map(|c| infer_rep_of_expr(&c.body))
                    .unwrap_or(Rep::Int);
                self.returns.insert(f.name.clone(), ret);
            }
        }
        Ok(())
    }

    fn compile_program(mut self, program: &Program) -> Result<Vec<u8>, String> {
        // Declare every top-level function with its uncurried signature.
        let mut func_ids: HashMap<String, cranelift_module::FuncId> = HashMap::new();
        for item in &program.items {
            if let Item::Function(f) = item {
                let arity = self.arities[&f.name];
                let ret = self.returns[&f.name];
                let sig = plain_sig(arity, ret.cl_type());
                let sym = format!("q_{}", f.name);
                let id = self
                    .module
                    .declare_function(&sym, Linkage::Local, &sig)
                    .map_err(|e| format!("declare {}: {e}", f.name))?;
                func_ids.insert(f.name.clone(), id);
            }
        }

        let items: Vec<Item> = program.items.clone();
        for item in &items {
            if let Item::Function(f) = item {
                let id = func_ids[&f.name];
                let arity = self.arities[&f.name];
                let ret = self.returns[&f.name];
                let sig = plain_sig(arity, ret.cl_type());
                self.define_function(id, f, &sig, &func_ids)?;
            }
        }

        // Emit the C-ABI entry point `main` that calls the Quill `main`.
        if let Some(main_id) = func_ids.get("main").cloned() {
            let ret = self.returns.get("main").copied().unwrap_or(Rep::Int);
            self.define_entry(main_id, ret)?;
        } else {
            self.define_trivial_entry()?;
        }

        let product = self.module.finish();
        product.emit().map_err(|e| format!("emit object: {e}"))
    }

    fn define_entry(&mut self, main_id: cranelift_module::FuncId, ret: Rep) -> Result<(), String> {
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
        {
            let mut func_ctx = FunctionBuilderContext::new();
            let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut func_ctx);
            let block = bcx.create_block();
            bcx.switch_to_block(block);
            bcx.append_block_params_for_function_params(block);
            let call = bcx.ins().call(callee, &[]);
            let v = bcx.inst_results(call)[0];
            let exit = match ret {
                Rep::Int => bcx.ins().ireduce(types::I32, v),
                Rep::Bool => bcx.ins().uextend(types::I32, v),
            };
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
        sig: &Signature,
        func_ids: &HashMap<String, cranelift_module::FuncId>,
    ) -> Result<(), String> {
        let target_config = self.module.target_config();
        let mut ctx = self.module.make_context();
        ctx.func.name = UserFuncName::testcase(&f.name);
        ctx.func.signature = sig.clone();

        // Import all callees as FuncRefs for this function.
        let mut funcrefs: HashMap<String, cranelift_codegen::ir::FuncRef> = HashMap::new();
        for (name, fid) in func_ids {
            let fr = self.module.declare_func_in_func(*fid, &mut ctx.func);
            funcrefs.insert(name.clone(), fr);
        }

        {
            let mut func_ctx = FunctionBuilderContext::new();
            let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut func_ctx);
            let entry = bcx.create_block();
            bcx.switch_to_block(entry);
            bcx.append_block_params_for_function_params(entry);
            let params = bcx.block_params(entry).to_vec();

            let merge_block = bcx.create_block();
            let merge_param = bcx.append_block_param(merge_block, sig.returns[0].value_type);
            let mut clause_entries: Vec<cranelift_codegen::ir::Block> = Vec::new();
            for _ in &f.clauses {
                clause_entries.push(bcx.create_block());
            }
            let trap_block = bcx.create_block();

            // Entry falls through to the first clause.
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
                for (j, p) in clause.pats.iter().enumerate() {
                    match p {
                        Pattern::Var(name, _) => {
                            let v = bcx.declare_var(types::I64);
                            bcx.def_var(v, params[j]);
                            vars.insert(name.clone(), v);
                        }
                        Pattern::Wildcard(_) => {}
                        Pattern::Int(n, _) => {
                            let lit = bcx.ins().iconst(types::I64, *n);
                            let cond = bcx.ins().icmp(
                                cranelift_codegen::ir::condcodes::IntCC::Equal,
                                params[j],
                                lit,
                            );
                            let cont = bcx.create_block();
                            bcx.ins().brif(cond, cont, &[], next_block, &[]);
                            bcx.switch_to_block(cont);
                        }
                        other => return Err(format!("unsupported pattern: {other:?}")),
                    }
                }
                let (body, _rep) = self.gen_expr(&mut bcx, &clause.body, &vars, &funcrefs)?;
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

    fn gen_expr(
        &self,
        bcx: &mut FunctionBuilder,
        e: &Expr,
        vars: &HashMap<String, Variable>,
        funcrefs: &HashMap<String, cranelift_codegen::ir::FuncRef>,
    ) -> Result<(Value, Rep), String> {
        match e {
            Expr::Int(n, _) => Ok((bcx.ins().iconst(types::I64, *n), Rep::Int)),
            Expr::Ctor(name, _) if name == "True" => {
                Ok((bcx.ins().iconst(types::I8, 1), Rep::Bool))
            }
            Expr::Ctor(name, _) if name == "False" => {
                Ok((bcx.ins().iconst(types::I8, 0), Rep::Bool))
            }
            Expr::Var(name, _) => {
                let var = vars.get(name).ok_or_else(|| format!("unbound `{name}`"))?;
                Ok((bcx.use_var(*var), Rep::Int))
            }
            Expr::If(c, t, el, _) => {
                let (cond, _) = self.gen_expr(bcx, c, vars, funcrefs)?;
                let rep = infer_rep_of_expr(t);
                let then_b = bcx.create_block();
                let else_b = bcx.create_block();
                let merge_b = bcx.create_block();
                let merge_param = bcx.append_block_param(merge_b, rep.cl_type());
                bcx.ins().brif(cond, then_b, &[], else_b, &[]);
                bcx.switch_to_block(then_b);
                let (tv, _) = self.gen_expr(bcx, t, vars, funcrefs)?;
                bcx.ins().jump(merge_b, &[BlockArg::Value(tv)]);
                bcx.switch_to_block(else_b);
                let (ev, _) = self.gen_expr(bcx, el, vars, funcrefs)?;
                bcx.ins().jump(merge_b, &[BlockArg::Value(ev)]);
                bcx.seal_block(then_b);
                bcx.seal_block(else_b);
                bcx.seal_block(merge_b);
                bcx.switch_to_block(merge_b);
                Ok((merge_param, rep))
            }
            Expr::Ascribe(e, _, _) => self.gen_expr(bcx, e, vars, funcrefs),
            Expr::App(..) => self.gen_apply(bcx, e, vars, funcrefs),
            other => Err(format!("unsupported expression: {other:?}")),
        }
    }

    fn gen_apply(
        &self,
        bcx: &mut FunctionBuilder,
        e: &Expr,
        vars: &HashMap<String, Variable>,
        funcrefs: &HashMap<String, cranelift_codegen::ir::FuncRef>,
    ) -> Result<(Value, Rep), String> {
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

        let head_name = match &head {
            Expr::Var(n, _) => n.clone(),
            _ => return Err("function position must be a name".to_string()),
        };

        if is_builtin(&head_name) {
            return self.gen_builtin(bcx, &head_name, &args, vars, funcrefs);
        }

        let Some(&callee) = funcrefs.get(&head_name) else {
            return Err(format!("unbound function `{head_name}`"));
        };
        let arity = self.arities.get(&head_name).copied().unwrap_or(0);
        if args.len() != arity {
            return Err(format!(
                "`{head_name}` expects {arity} argument(s), got {} (partial application not yet compiled)",
                args.len()
            ));
        }
        let mut arg_vals = Vec::new();
        for a in &args {
            let (v, _) = self.gen_expr(bcx, a, vars, funcrefs)?;
            arg_vals.push(v);
        }
        let call = bcx.ins().call(callee, &arg_vals);
        let rep = self.returns.get(&head_name).copied().unwrap_or(Rep::Int);
        Ok((bcx.inst_results(call)[0], rep))
    }

    fn gen_builtin(
        &self,
        bcx: &mut FunctionBuilder,
        op: &str,
        args: &[Expr],
        vars: &HashMap<String, Variable>,
        funcrefs: &HashMap<String, cranelift_codegen::ir::FuncRef>,
    ) -> Result<(Value, Rep), String> {
        let mut vals = Vec::new();
        for a in args {
            let (v, _) = self.gen_expr(bcx, a, vars, funcrefs)?;
            vals.push(v);
        }
        match op {
            "+" | "-" | "*" | "/" | "%" => {
                let a = vals[0];
                let b = vals[1];
                let v = match op {
                    "+" => bcx.ins().iadd(a, b),
                    "-" => bcx.ins().isub(a, b),
                    "*" => bcx.ins().imul(a, b),
                    "/" => bcx.ins().sdiv(a, b),
                    "%" => bcx.ins().srem(a, b),
                    _ => unreachable!(),
                };
                Ok((v, Rep::Int))
            }
            "negate" => Ok((bcx.ins().ineg(vals[0]), Rep::Int)),
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
                Ok((bcx.ins().icmp(cc, vals[0], vals[1]), Rep::Bool))
            }
            "&&" => Ok((bcx.ins().band(vals[0], vals[1]), Rep::Bool)),
            "||" => Ok((bcx.ins().bor(vals[0], vals[1]), Rep::Bool)),
            "not" => Ok((bcx.ins().bxor_imm_u(vals[0], 1), Rep::Bool)),
            "error" => {
                bcx.ins().trap(TrapCode::unwrap_user(2));
                Ok((bcx.ins().iconst(types::I64, 0), Rep::Int))
            }
            other => Err(format!("unhandled builtin `{other}`")),
        }
    }
}

fn plain_sig(arity: usize, ret: types::Type) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    for _ in 0..arity {
        sig.params.push(AbiParam::new(types::I64));
    }
    sig.returns.push(AbiParam::new(ret));
    sig
}

fn is_builtin(op: &str) -> bool {
    matches!(
        op,
        "+" | "-" | "*" | "/" | "%" | "negate" | "==" | "/=" | "<" | "<=" | ">" | ">=" | "&&"
            | "||" | "not" | "error"
    )
}

/// Infer the representation of an expression (Int/Bool).
fn infer_rep_of_expr(e: &Expr) -> Rep {
    match e {
        Expr::Int(_, _) => Rep::Int,
        Expr::Ctor(n, _) if n == "True" || n == "False" => Rep::Bool,
        Expr::If(_, t, _, _) => infer_rep_of_expr(t),
        Expr::App(..) => match head_name(e).as_deref() {
            Some(op) if matches!(op, "==" | "/=" | "<" | "<=" | ">" | ">=" | "&&" | "||" | "not") => {
                Rep::Bool
            }
            _ => Rep::Int,
        },
        _ => Rep::Int,
    }
}

fn head_name(e: &Expr) -> Option<String> {
    match e {
        Expr::Var(n, _) => Some(n.clone()),
        Expr::App(f, _, _) => head_name(f),
        _ => None,
    }
}
