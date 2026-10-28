//! LLVM IR 代码生成（基于 inkwell 0.11，目标 LLVM 18）。
//!
//! 架构核心：
//! - Dial 编译器**只翻译 AST → 对这些 C 函数的调用**；自身不做容器/字符串的底层实现。
//! - String / Array 在 IR 中一律是「不透明指针」i8*；用户永远无法直接访问底层内存布局。
//! - 引用计数 / 写时复制完全由 C Runtime（runtime/ + stdlib/）掌控。
//! - 数据流分析（Sema 阶段写入的 move_hint）在此决定：移动语义（跳过 retain）还是共享（retain）。
//! - `.o` 由 inkwell 在内存中构建 IR 后 `write_to_file` 直接 emit，不导出 .ll 文本。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use inkwell::context::Context;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
};
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, StructType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, FloatValue, FunctionValue, IntValue, PointerValue,
};
use inkwell::AddressSpace;
use inkwell::FloatPredicate;
use inkwell::IntPredicate;
use inkwell::OptimizationLevel;

use crate::ast::*;
use crate::error::{DialcError, Result};
use crate::semantic::{type_of, FuncInfo, StructTable};

type Local<'ctx> = (Type, (bool, PointerValue<'ctx>));
type CgScopes<'ctx> = Vec<HashMap<String, Local<'ctx>>>;

pub struct CodeGen<'ctx> {
    context: &'ctx Context,
    module: inkwell::module::Module<'ctx>,
    builder: inkwell::builder::Builder<'ctx>,
    cfuns: HashMap<String, FunctionValue<'ctx>>,
    struct_types: HashMap<String, StructType<'ctx>>,
    str_id: std::cell::Cell<u32>,
    /// 已被“移动”的局部变量名（转移所有权，跳过作用域结束时的 release）。
    moved: RefCell<std::collections::HashSet<String>>,
}

/// 编译入口：接收校验后的 AST、函数签名表、结构体表，生成并写出目标文件 `.o`。
pub fn compile(
    program: &Program,
    functions: &HashMap<String, FuncInfo>,
    structs: &StructTable,
    out_path: &Path,
) -> Result<()> {
    let context = Context::create();
    let module = context.create_module("dialc");

    let mut cg = CodeGen {
        context: &context,
        module,
        builder: context.create_builder(),
        cfuns: HashMap::new(),
        struct_types: HashMap::new(),
        str_id: std::cell::Cell::new(0),
        moved: RefCell::new(std::collections::HashSet::new()),
    };

    cg.build_struct_types(program);
    cg.declare_stdlib();

    // 1) 声明所有 Dial 函数
    let mut func_values: HashMap<String, FunctionValue> = HashMap::new();
    for f in &program.functions {
        let fv = cg.declare_dial_function(f, functions);
        func_values.insert(f.name.clone(), fv);
    }

    // 2) 生成函数体
    for f in &program.functions {
        cg.gen_function(f, &func_values, functions, structs)?;
    }

    // 3) 写出目标文件
    cg.emit_object(out_path)?;
    Ok(())
}

impl<'ctx> CodeGen<'ctx> {
    fn i8ptr(&self) -> inkwell::types::PointerType<'ctx> {
        self.context.ptr_type(AddressSpace::default())
    }

    /// 构建用户结构体的 LLVM 命名结构体类型（先建 opaque，再补 body 以支持互相引用）。
    fn build_struct_types(&mut self, program: &Program) {
        for sd in &program.structs {
            self.struct_types
                .insert(sd.name.clone(), self.context.opaque_struct_type(&sd.name));
        }
        for sd in &program.structs {
            let fields: Vec<BasicTypeEnum> = sd
                .fields
                .iter()
                .map(|(_, t)| self.dialc_type_to_llvm(t))
                .collect();
            self.struct_types[&sd.name].set_body(&fields, false);
        }
    }

    /// 声明一个返回值类型为 ret 的 C 函数。
    fn decl_func(
        &mut self,
        name: &str,
        ret: BasicTypeEnum<'ctx>,
        params: &[BasicMetadataTypeEnum<'ctx>],
    ) {
        let ft = ret.fn_type(params, false);
        self.cfuns
            .insert(name.to_string(), self.module.add_function(name, ft, None));
    }

    /// 声明一个返回 void 的 C 函数。
    fn decl_void(&mut self, name: &str, params: &[BasicMetadataTypeEnum<'ctx>]) {
        let ft = self.context.void_type().fn_type(params, false);
        self.cfuns
            .insert(name.to_string(), self.module.add_function(name, ft, None));
    }

    /// 声明全部 C 标准库 / Runtime 导出函数（不透明指针契约）。
    fn declare_stdlib(&mut self) {
        let i8b: BasicTypeEnum = self.i8ptr().into();
        let i64b: BasicTypeEnum = self.context.i64_type().into();
        let f64b: BasicTypeEnum = self.context.f64_type().into();

        // String
        self.decl_func("dial_string_new", i8b, &[i8b.into()]);
        self.decl_func("dial_string_length", i64b, &[i8b.into()]);
        self.decl_func("dial_string_concat", i8b, &[i8b.into(), i8b.into()]);
        self.decl_func("dial_string_get", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_string_eq", i64b, &[i8b.into(), i8b.into()]);
        self.decl_func(
            "dial_string_substring",
            i8b,
            &[i8b.into(), i64b.into(), i64b.into()],
        );
        self.decl_func("dial_string_clone", i8b, &[i8b.into()]);
        self.decl_void("dial_string_retain", &[i8b.into()]);
        self.decl_void("dial_string_release", &[i8b.into()]);
        self.decl_void("dial_print_string", &[i8b.into()]);
        // Array<int>
        self.decl_func("dial_array_new", i8b, &[]);
        self.decl_func("dial_array_push", i8b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_array_get", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func(
            "dial_array_set",
            i8b,
            &[i8b.into(), i64b.into(), i64b.into()],
        );
        self.decl_func("dial_array_length", i64b, &[i8b.into()]);
        self.decl_func("dial_array_clone", i8b, &[i8b.into()]);
        self.decl_void("dial_array_retain", &[i8b.into()]);
        self.decl_void("dial_array_release", &[i8b.into()]);
        // Map (int -> int, ARC)
        self.decl_func("dial_map_new", i8b, &[]);
        self.decl_func(
            "dial_map_put",
            i8b,
            &[i8b.into(), i64b.into(), i64b.into()],
        );
        self.decl_func("dial_map_get", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_map_contains", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_map_remove", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_map_size", i64b, &[i8b.into()]);
        self.decl_void("dial_map_retain", &[i8b.into()]);
        self.decl_void("dial_map_release", &[i8b.into()]);
        // IO / 打印
        self.decl_void("dial_print_int", &[i64b.into()]);
        self.decl_void("dial_print_float", &[f64b.into()]);
        self.decl_void("dial_print_bool", &[i64b.into()]);
        self.decl_func("dial_read_line", i8b, &[]);
    }

    fn declare_dial_function(
        &self,
        f: &Func,
        functions: &HashMap<String, FuncInfo>,
    ) -> FunctionValue<'ctx> {
        let info = &functions[&f.name];
        let param_tys: Vec<BasicMetadataTypeEnum> = f
            .params
            .iter()
            .map(|(_, t)| self.dialc_type_to_llvm(t).into())
            .collect();
        let ft = if f.ret == Type::Void {
            self.context.void_type().fn_type(&param_tys, false)
        } else if f.name == "main" && f.ret == Type::Int {
            // main 走 C ABI：返回 int (i32)
            self.context.i32_type().fn_type(&param_tys, false)
        } else {
            self.dialc_type_to_llvm(&f.ret).fn_type(&param_tys, false)
        };
        let _ = info;
        self.module.add_function(&f.name, ft, None)
    }

    fn dialc_type_to_llvm(&self, t: &Type) -> BasicTypeEnum<'ctx> {
        match *t {
            Type::Void => self.context.i64_type().into(),
            Type::Int => self.context.i64_type().into(),
            Type::Float => self.context.f64_type().into(),
            Type::Bool => self.context.bool_type().into(),
            Type::String | Type::Array(_) | Type::Map => self.i8ptr().into(),
            Type::Struct(ref n) => (*self
                .struct_types
                .get(n)
                .expect("结构体类型应在 build_struct_types 中已注册"))
            .into(),
        }
    }

    fn to_float(&self, v: BasicValueEnum<'ctx>) -> FloatValue<'ctx> {
        if v.is_float_value() {
            v.into_float_value()
        } else {
            self.builder
                .build_signed_int_to_float(v.into_int_value(), self.context.f64_type(), "itof")
                .unwrap()
        }
    }

    /// 把值强制转换为 i64：float -> i64 用 fptosi；bool(i1) -> i64 用 zext；int 原样。
    fn to_i64(&self, v: BasicValueEnum<'ctx>, t: &Type) -> IntValue<'ctx> {
        if *t == Type::Float {
            self.builder
                .build_float_to_signed_int(v.into_float_value(), self.context.i64_type(), "ftoi")
                .unwrap()
        } else if *t == Type::Bool {
            self.builder
                .build_int_z_extend(v.into_int_value(), self.context.i64_type(), "b2i")
                .unwrap()
        } else {
            v.into_int_value()
        }
    }

    /// 把值规整到目标类型。
    ///
    /// 在“零隐式转换铁律”下，类型检查阶段已保证 `from == to`，因此这里恒为恒等返回。
    /// 任何 int↔float 的转换都必须经源码中的内置函数 `int_to_float` / `float_to_int`，
    /// 编译器绝不在背后偷偷做 sitofp / fptosi。保留此函数为显式关卡，杜绝隐性转换死灰复燃。
    fn coerce(
        &self,
        v: BasicValueEnum<'ctx>,
        _from: &Type,
        _to: &Type,
    ) -> BasicValueEnum<'ctx> {
        v
    }

    fn default_value(&self, t: &Type) -> BasicValueEnum<'ctx> {
        match *t {
            Type::Int => self.context.i64_type().const_int(0, false).into(),
            Type::Float => self.context.f64_type().const_float(0.0).into(),
            Type::Bool => self.context.bool_type().const_int(0, false).into(),
            Type::String | Type::Array(_) | Type::Map => self.i8ptr().const_null().into(),
            Type::Struct(_) => {
                // 结构体默认值：对 v0.2 仅用于 main 收尾，构造一个零初始化聚合
                let bte = self.dialc_type_to_llvm(t);
                // 用 alloca + store zero 过于复杂；这里仅支持 int/float/bool 默认，结构体路径不触发
                let _ = bte;
                self.context.i64_type().const_int(0, false).into()
            }
            Type::Void => self.context.i64_type().const_int(0, false).into(),
        }
    }

    fn is_terminated(&self) -> bool {
        if let Some(bb) = self.builder.get_insert_block() {
            bb.get_terminator().is_some()
        } else {
            false
        }
    }

    fn global_string(&self, name: &str, value: &str) -> PointerValue<'ctx> {
        let value = format!("{}\0", value);
        let arr_ty = self.context.i8_type().array_type(value.len() as u32);
        let g = self.module.add_global(arr_ty, None, name);
        let const_arr = self.context.const_string(value.as_bytes(), false);
        g.set_initializer(&const_arr);
        g.set_constant(true);
        self.builder
            .build_pointer_cast(g.as_pointer_value(), self.i8ptr(), "strcast")
            .unwrap()
    }

    fn next_str_id(&self) -> u32 {
        let n = self.str_id.get();
        self.str_id.set(n + 1);
        n
    }

    fn emit_object(&self, out: &Path) -> Result<()> {
        Target::initialize_all(&InitializationConfig::default());
        let triple = TargetMachine::get_default_triple();
        let target = Target::from_triple(&triple)
            .map_err(|e| DialcError::new(format!("LLVM 目标平台错误: {:?}", e), 0, 0))?;
        let tm = target
            .create_target_machine(
                &triple,
                "x86-64",
                "",
                OptimizationLevel::Default,
                RelocMode::Default,
                CodeModel::Default,
            )
            .ok_or_else(|| DialcError::new("创建目标机失败（LLVM 未支持当前平台？）", 0, 0))?;
        tm.write_to_file(&self.module, FileType::Object, out)
            .map_err(|e| DialcError::new(format!("生成目标文件失败: {:?}", e), 0, 0))?;
        Ok(())
    }

    // ---- 调用 C 函数 ----

    fn call_c(&self, name: &str, args: &[BasicMetadataValueEnum<'ctx>]) -> BasicValueEnum<'ctx> {
        let fv = self.cfuns[name];
        self.builder
            .build_call(fv, args, name)
            .unwrap()
            .try_as_basic_value()
            .basic()
            .unwrap()
    }

    fn call_c_void(&self, name: &str, args: &[BasicMetadataValueEnum<'ctx>]) {
        let fv = self.cfuns[name];
        self.builder.build_call(fv, args, name).unwrap();
    }

    fn retain(&self, ty: &Type, val: BasicValueEnum<'ctx>) {
        let name = match *ty {
            Type::String => "dial_string_retain",
            Type::Array(_) => "dial_array_retain",
            Type::Map => "dial_map_retain",
            _ => return,
        };
        self.call_c_void(name, &[val.into()]);
    }

    fn release(&self, ty: &Type, ptr: PointerValue<'ctx>) {
        let name = match *ty {
            Type::String => "dial_string_release",
            Type::Array(_) => "dial_array_release",
            Type::Map => "dial_map_release",
            _ => return,
        };
        let v = self.builder.build_load(self.i8ptr(), ptr, "ldrel").unwrap();
        self.call_c_void(name, &[v.into()]);
    }

    fn is_heap_type(t: &Type) -> bool {
        matches!(*t, Type::String | Type::Array(_) | Type::Map)
    }

    /// 在函数的 **entry 块** 里分配栈槽。
    ///
    /// 这是必须的，而非优化偏好：局部变量声明可能出现在循环体内，
    /// 若在当时所在块直接 `alloca`，每进入一次循环就会在栈上多压一个槽，
    /// 迭代次数一多就会把栈耗尽（段错误）。把 alloca 统一提到 entry 块，
    /// 每个变量只占一个固定栈槽、跨迭代复用，栈用量与循环次数无关。
    fn alloca_in_entry(
        &self,
        function: FunctionValue<'ctx>,
        ty: BasicTypeEnum<'ctx>,
        name: &str,
    ) -> PointerValue<'ctx> {
        let entry = function
            .get_first_basic_block()
            .expect("函数应至少有一个 entry 块");
        let builder = self.context.create_builder();
        match entry.get_first_instruction() {
            Some(first) => builder.position_before(&first),
            None => builder.position_at_end(entry),
        }
        builder.build_alloca(ty, name).unwrap()
    }

    /// 递归地对结构体值中的**堆字段**（string / int[]）执行 retain。
    ///
    /// 结构体本身是值类型（栈上 alloca），但它的堆字段是不透明指针：
    /// 结构体值被复制一次，其堆字段就多一个持有者，因此必须对称地 +1，
    /// 否则作用域结束时两侧的 release 会互相踩踏（double free）。
    fn retain_struct_fields(
        &self,
        sty: &Type,
        ptr: PointerValue<'ctx>,
        structs: &StructTable,
    ) {
        let name = match sty {
            Type::Struct(n) => n,
            _ => return,
        };
        let (fields, llvm_sty) = match (structs.get(name), self.struct_types.get(name)) {
            (Some(f), Some(t)) => (f.clone(), *t),
            _ => return,
        };
        for (i, (_fname, fty)) in fields.iter().enumerate() {
            let gep = self
                .builder
                .build_struct_gep(llvm_sty, ptr, i as u32, "frtn")
                .unwrap();
            match fty {
                Type::String | Type::Array(_) | Type::Map => {
                    let v = self.builder.build_load(self.i8ptr(), gep, "lfr").unwrap();
                    self.retain(fty, v);
                }
                Type::Struct(_) => self.retain_struct_fields(fty, gep, structs),
                _ => {}
            }
        }
    }

    /// `retain_struct_fields` 的对称操作：递归 release 结构体值中的堆字段。
    fn release_struct_fields(
        &self,
        sty: &Type,
        ptr: PointerValue<'ctx>,
        structs: &StructTable,
    ) {
        let name = match sty {
            Type::Struct(n) => n,
            _ => return,
        };
        let (fields, llvm_sty) = match (structs.get(name), self.struct_types.get(name)) {
            (Some(f), Some(t)) => (f.clone(), *t),
            _ => return,
        };
        for (i, (_fname, fty)) in fields.iter().enumerate() {
            let gep = self
                .builder
                .build_struct_gep(llvm_sty, ptr, i as u32, "frel")
                .unwrap();
            match fty {
                Type::String | Type::Array(_) | Type::Map => self.release(fty, gep),
                Type::Struct(_) => self.release_struct_fields(fty, gep, structs),
                _ => {}
            }
        }
    }

    // ---- 函数体 ----

    fn gen_function(
        &self,
        f: &Func,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<()> {
        let function = func_values[&f.name];
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);

        let mut scopes: CgScopes = vec![HashMap::new()];
        for (i, (pname, pty)) in f.params.iter().enumerate() {
            let llvm_ty = self.dialc_type_to_llvm(pty);
            let ptr = self.alloca_in_entry(function, llvm_ty, pname);
            let param_val = function.get_nth_param(i as u32).unwrap();
            self.builder.build_store(ptr, param_val).unwrap();
            scopes[0].insert(pname.clone(), (pty.clone(), (true, ptr)));
            // 形参取得共享所有权（进入时对称 +1，退出时 -1）：
            //   - 字符串/数组形参：直接 retain 指针；
            //   - 结构体形参：值拷贝本身不带所有权，需递归 retain 其中的堆字段。
            if Self::is_heap_type(pty) {
                let v = self.builder.build_load(self.i8ptr(), ptr, "parm").unwrap();
                self.retain(pty, v);
            } else if matches!(pty, Type::Struct(_)) {
                self.retain_struct_fields(pty, ptr, structs);
            }
        }
        for stmt in &f.body {
            self.gen_stmt(stmt, function, &mut scopes, func_values, functions, structs)?;
        }
        // 函数末尾未显式 return 时补默认返回
        if !self.is_terminated() {
            if f.ret == Type::Void {
                self.builder.build_return(None).unwrap();
            } else if f.name == "main" && f.ret == Type::Int {
                let zero = self
                    .context
                    .i32_type()
                    .const_int(0, false);
                self.builder.build_return(Some(&zero)).unwrap();
            } else {
                let dv = self.default_value(&f.ret);
                self.builder.build_return(Some(&dv)).unwrap();
            }
        }
        Ok(())
    }

    fn release_scope_locals(
        &self,
        scope: &HashMap<String, Local<'ctx>>,
        structs: &StructTable,
    ) {
        let moved = self.moved.borrow();
        for (name, (ty, (_m, ptr))) in scope.iter() {
            if moved.contains(name) {
                continue;
            }
            if Self::is_heap_type(ty) {
                self.release(ty, *ptr);
            } else if matches!(ty, Type::Struct(_)) {
                // 结构体值本身在栈上，无需释放；但其堆字段必须有对称的 release
                self.release_struct_fields(ty, *ptr, structs);
            }
        }
    }

    fn release_all(&self, scopes: &CgScopes<'ctx>, except: Option<&str>, structs: &StructTable) {
        for scope in scopes.iter() {
            let moved = self.moved.borrow();
            for (name, (ty, (_m, ptr))) in scope.iter() {
                if let Some(ex) = except {
                    if name == ex {
                        continue;
                    }
                }
                if moved.contains(name) {
                    continue;
                }
                if Self::is_heap_type(ty) {
                    self.release(ty, *ptr);
                } else if matches!(ty, Type::Struct(_)) {
                    self.release_struct_fields(ty, *ptr, structs);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_stmt(
        &self,
        stmt: &Stmt,
        function: FunctionValue<'ctx>,
        scopes: &mut CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<()> {
        match stmt {
            Stmt::VarDecl {
                name,
                ty,
                mutable,
                value,
                move_hint,
                ..
            } => {
                let llvm_ty = self.dialc_type_to_llvm(ty);
                let ptr = self.alloca_in_entry(function, llvm_ty, name);
                let val = self.gen_expr(value, function, scopes, func_values, functions, structs)?;
                let stored = self.coerce(val, &type_of(value, scopes, functions, structs)?, ty);
                // 绑定策略：Var 复制且非移动 -> retain（共享）；其余（构造/方法/call）-> 直接取得所有权
                if let Expr::Var(src) = value {
                    if Self::is_heap_type(ty) && !move_hint {
                        self.retain(ty, stored);
                    }
                    if *move_hint {
                        self.moved.borrow_mut().insert(src.clone());
                    }
                }
                self.builder.build_store(ptr, stored).unwrap();
                // 结构体：值拷贝已把堆字段的指针复制过来，需要对称地给这些字段 +1，
                // 否则本变量在作用域结束时的 release 会与源变量互相踩踏。
                if let Expr::Var(_) = value {
                    if matches!(ty, Type::Struct(_)) && !move_hint {
                        self.retain_struct_fields(ty, ptr, structs);
                    }
                }
                scopes.last_mut().unwrap().insert(name.clone(), (ty.clone(), (*mutable, ptr)));
                Ok(())
            }
            Stmt::Assign {
                name,
                value,
                move_hint,
                line,
                col,
            } => {
                let info = scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.get(name).map(|v| v.clone()))
                    .ok_or_else(|| DialcError::new(format!("未声明的变量 '{}'", name), *line, *col))?;
                let (vty, (_m, ptr)) = info;
                // 严格顺序：先取得新值的所有权（retain），再释放旧值（release）。
                // 这保证自赋值 `x = x` 安全——若先 release，引用计数可能归零并被 free，
                // 随后的 retain 就变成 use-after-free。
                let val = self.gen_expr(value, function, scopes, func_values, functions, structs)?;
                let stored = self.coerce(val, &type_of(value, scopes, functions, structs)?, &vty);

                if matches!(vty, Type::Struct(_)) {
                    // 结构体：新值先落到临时槽，再“先加后减”，最后写回。
                    // 这样即便 `x = x`，也是先 +1 再 -1，不会把对象提前释放。
                    let llvm_ty = self.dialc_type_to_llvm(&vty);
                    let tmp = self.alloca_in_entry(function, llvm_ty, "asgtmp");
                    self.builder.build_store(tmp, stored).unwrap();
                    if let Expr::Var(_) = value {
                        if !move_hint {
                            self.retain_struct_fields(&vty, tmp, structs);
                        }
                    }
                    self.release_struct_fields(&vty, ptr, structs);
                    let v = self.builder.build_load(llvm_ty, tmp, "asgval").unwrap();
                    self.builder.build_store(ptr, v).unwrap();
                } else {
                    if let Expr::Var(_) = value {
                        if Self::is_heap_type(&vty) && !move_hint {
                            self.retain(&vty, stored);
                        }
                    }
                    if Self::is_heap_type(&vty) {
                        self.release(&vty, ptr);
                    }
                    self.builder.build_store(ptr, stored).unwrap();
                }

                if let Expr::Var(src) = value {
                    if *move_hint {
                        self.moved.borrow_mut().insert(src.clone());
                    }
                }
                Ok(())
            }
            Stmt::AssignField {
                obj,
                field,
                value,
                line,
                col,
            } => {
                let lptr = self.gen_lvalue_ptr(obj, function, scopes, func_values, functions, structs)?;
                // 找到字段索引与类型
                let ot = type_of(obj, scopes, functions, structs)?;
                let sty = self.get_struct_type(&ot)?;
                let (fidx, fty) = self.field_info(&ot, field, structs, *line, *col)?;
                let fgep = self
                    .builder
                    .build_struct_gep(sty, lptr, fidx, "fgep")
                    .unwrap();
                // 覆盖前 release 旧字段值（堆类型）
                if Self::is_heap_type(&fty) {
                    self.release(&fty, fgep);
                }
                let val = self.gen_expr(value, function, scopes, func_values, functions, structs)?;
                let stored = self.coerce(val, &type_of(value, scopes, functions, structs)?, &fty);
                self.builder.build_store(fgep, stored).unwrap();
                Ok(())
            }
            Stmt::AssignIndex {
                arr,
                index,
                value,
                line,
                col,
            } => {
                // arr[i] = v  ->  arr = dial_array_set(arr, i, v)
                let root = lvalue_root(arr).ok_or_else(|| {
                    DialcError::new("语义错误：数组索引赋值目标必须是变量", *line, *col)
                })?;
                let info = scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.get(root).map(|v| v.clone()))
                    .ok_or_else(|| DialcError::new(format!("未声明的变量 '{}'", root), *line, *col))?;
                let (_aty, (_m, arr_ptr)) = info;
                let cur = self.builder.build_load(self.i8ptr(), arr_ptr, "arrptr").unwrap();
                let idx = self.gen_expr(index, function, scopes, func_values, functions, structs)?;
                let idxi64 = self.to_i64(idx, &type_of(index, scopes, functions, structs)?);
                let val = self.gen_expr(value, function, scopes, func_values, functions, structs)?;
                let vali64 = self.to_i64(val, &type_of(value, scopes, functions, structs)?);
                let newp = self.call_c(
                    "dial_array_set",
                    &[cur.into(), idxi64.into(), vali64.into()],
                );
                // 所有权契约：dial_array_set 返回的指针**替换**旧指针。
                //   - 独占时（refcount==1）原地写入并返回同一指针，旧引用计数不变；
                //   - 共享时（refcount>1）在 C 侧完成 COW 克隆并释放本调用者的旧引用。
                // 两种情形下调用方都**不得**再对旧指针 release，否则分别造成
                // 悬垂指针与双重释放。故此处只做写回。
                self.builder.build_store(arr_ptr, newp).unwrap();
                Ok(())
            }
            Stmt::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let cond_val = self.gen_expr(cond, function, scopes, func_values, functions, structs)?;
                let cond_i1 = cond_val.into_int_value();
                let then_bb = self.context.append_basic_block(function, "then");
                let else_bb = self.context.append_basic_block(function, "else");
                let merge_bb = self.context.append_basic_block(function, "ifcont");
                self.builder
                    .build_conditional_branch(cond_i1, then_bb, else_bb)
                    .unwrap();
                self.builder.position_at_end(then_bb);
                scopes.push(HashMap::new());
                for s in then_branch {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                }
                self.release_scope_locals(scopes.last().unwrap(), structs);
                scopes.pop();
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(merge_bb).unwrap();
                }
                self.builder.position_at_end(else_bb);
                if let Some(eb) = else_branch {
                    scopes.push(HashMap::new());
                    for s in eb {
                        self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                    }
                    self.release_scope_locals(scopes.last().unwrap(), structs);
                    scopes.pop();
                }
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(merge_bb).unwrap();
                }
                self.builder.position_at_end(merge_bb);
                Ok(())
            }
            Stmt::While { cond, body } => {
                let header_bb = self.context.append_basic_block(function, "while.cond");
                let body_bb = self.context.append_basic_block(function, "while.body");
                let exit_bb = self.context.append_basic_block(function, "while.end");
                self.builder.build_unconditional_branch(header_bb).unwrap();
                self.builder.position_at_end(header_bb);
                let cond_val = self.gen_expr(cond, function, scopes, func_values, functions, structs)?;
                let cond_i1 = cond_val.into_int_value();
                self.builder
                    .build_conditional_branch(cond_i1, body_bb, exit_bb)
                    .unwrap();
                self.builder.position_at_end(body_bb);
                scopes.push(HashMap::new());
                for s in body {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                }
                self.release_scope_locals(scopes.last().unwrap(), structs);
                scopes.pop();
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(header_bb).unwrap();
                }
                self.builder.position_at_end(exit_bb);
                Ok(())
            }
            Stmt::For { var, start, end, body } => {
                let i64t = self.context.i64_type();
                // 循环变量：在入口块分配，跨迭代存活。
                let var_ptr = self.alloca_in_entry(function, i64t.into(), var);
                let start_val = self.gen_expr(start, function, scopes, func_values, functions, structs)?;
                let start_i64 = self.to_i64(start_val, &type_of(start, scopes, functions, structs)?);
                self.builder.build_store(var_ptr, start_i64).unwrap();
                // 把循环变量登记进作用域（int 非堆类型，无需 retain/release）。
                scopes.push(HashMap::new());
                scopes
                    .last_mut()
                    .unwrap()
                    .insert(var.clone(), (Type::Int, (true, var_ptr)));

                let header_bb = self.context.append_basic_block(function, "for.cond");
                let body_bb = self.context.append_basic_block(function, "for.body");
                let exit_bb = self.context.append_basic_block(function, "for.end");
                self.builder.build_unconditional_branch(header_bb).unwrap();
                self.builder.position_at_end(header_bb);
                let cur = self.builder.build_load(i64t, var_ptr, "i").unwrap();
                let end_val = self.gen_expr(end, function, scopes, func_values, functions, structs)?;
                let end_i64 = self.to_i64(end_val, &type_of(end, scopes, functions, structs)?);
                let cond = self
                    .builder
                    .build_int_compare(IntPredicate::SLT, cur.into_int_value(), end_i64, "forcond")
                    .unwrap();
                self.builder
                    .build_conditional_branch(cond, body_bb, exit_bb)
                    .unwrap();
                self.builder.position_at_end(body_bb);
                for s in body {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                }
                self.release_scope_locals(scopes.last().unwrap(), structs);
                // i = i + 1
                let cur2 = self.builder.build_load(i64t, var_ptr, "i").unwrap();
                let next = self
                    .builder
                    .build_int_add(cur2.into_int_value(), i64t.const_int(1, false), "iinc")
                    .unwrap();
                self.builder.build_store(var_ptr, next).unwrap();
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(header_bb).unwrap();
                }
                self.builder.position_at_end(exit_bb);
                scopes.pop();
                Ok(())
            }
            Stmt::Return(e, line, col) => {
                match e {
                    None => {
                        if !f_ret_is_void(function) {
                            return Err(DialcError::new(
                                "内部错误：非 void 函数不应有无值 return",
                                *line, *col,
                            ));
                        }
                        // 无返回值：释放本函数持有的全部堆对象后返回
                        self.release_all(scopes, None, structs);
                        self.builder.build_return(None).unwrap();
                    }
                    Some(expr) => {
                        // 严格顺序（必须如此，否则会把返回值提前释放）：
                        //   1) 先求值返回表达式，取得待返回的值；
                        //   2) 若返回的是局部变量，标记 moved，使其所有权随返回值转移；
                        //   3) 最后释放其余**未被转移**的局部变量；
                        //   4) 生成 ret。
                        let val = self.gen_expr(expr, function, scopes, func_values, functions, structs)?;
                        if let Expr::Var(n) = expr {
                            self.moved.borrow_mut().insert(n.clone());
                        }
                        // 处理 main 返回 i32（C ABI）
                        let ret_bte = function.get_type().get_return_type();
                        let is_main_i32 = matches!(
                            ret_bte,
                            Some(BasicTypeEnum::IntType(it)) if it.get_bit_width() == 32
                        );
                        let ret_val: BasicValueEnum = if is_main_i32 {
                            let i = self.to_i64(val, &type_of(expr, scopes, functions, structs)?);
                            self.builder
                                .build_int_truncate(i, self.context.i32_type(), "i32")
                                .unwrap()
                                .into()
                        } else {
                            val
                        };
                        self.release_all(scopes, None, structs);
                        self.builder.build_return(Some(&ret_val)).unwrap();
                    }
                }
                Ok(())
            }
            Stmt::Expr(e) => {
                // 语句级方法调用且返回新容器指针（push/set）-> 把结果写回接收变量
                if let Expr::Method(obj, method, args) = e {
                    if matches!(method.as_str(), "push" | "set" | "put") {
                        if let Expr::Var(name) = obj.as_ref() {
                            let info = scopes
                                .iter()
                                .rev()
                                .find_map(|s| s.get(name).map(|v| v.clone()));
                            if let Some((_ty, (_m, ptr))) = info {
                                let newp = self.gen_method_call(obj, method, args, function, scopes, func_values, functions, structs)?;
                                // 同上：push/set 返回的是替换指针，禁止对旧指针 release。
                                self.builder.build_store(ptr, newp).unwrap();
                                return Ok(());
                            }
                        }
                    }
                }
                self.gen_expr(e, function, scopes, func_values, functions, structs)?;
                Ok(())
            }
        }
    }

    /// 求结构体字段的 (索引, 类型)。
    fn field_info(
        &self,
        ot: &Type,
        field: &str,
        structs: &StructTable,
        line: usize,
        col: usize,
    ) -> Result<(u32, Type)> {
        if let Type::Struct(sname) = ot {
            let fields = structs
                .get(sname)
                .ok_or_else(|| DialcError::new(format!("未知结构体 '{}'", sname), line, col))?;
            fields
                .iter()
                .enumerate()
                .find(|(_, (f, _))| f == field)
                .map(|(i, (_, t))| (i as u32, t.clone()))
                .ok_or_else(|| DialcError::new(format!("结构体 '{}' 无字段 '{}'", sname, field), line, col))
        } else {
            Err(DialcError::new(
                format!("类型 '{}' 没有字段 '{}'", ot.name(), field),
                line, col,
            ))
        }
    }

    /// 由类型取得其 LLVM 结构体类型（仅对 struct 有效）。
    fn get_struct_type(&self, ot: &Type) -> Result<StructType<'ctx>> {
        match ot {
            Type::Struct(name) => Ok(self.struct_types[name]),
            other => Err(DialcError::new(
                format!("语义错误：类型 '{}' 不是结构体，无法做字段访问", other.name()),
                0,
                0,
            )),
        }
    }

    /// 生成“字段/数组元素”的 lvalue 指针（仅支持 Var / Field 链）。
    fn gen_lvalue_ptr(
        &self,
        e: &Expr,
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<PointerValue<'ctx>> {
        match e {
            Expr::Var(name) => {
                let info = scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.get(name).map(|v| v.clone()))
                    .ok_or_else(|| DialcError::new(format!("未声明的变量 '{}'", name), 0, 0))?;
                Ok(info.1 .1)
            }
            Expr::Field(obj, field) => {
                let base = self.gen_lvalue_ptr(obj, function, scopes, func_values, functions, structs)?;
                let ot = type_of(obj, scopes, functions, structs)?;
                let sty = self.get_struct_type(&ot)?;
                let (fidx, _fty) = self.field_info(&ot, field, structs, 0, 0)?;
                Ok(self.builder.build_struct_gep(sty, base, fidx, "fgep").unwrap())
            }
            _ => Err(DialcError::new(
                "语义错误：仅支持对变量/字段进行赋值",
                0, 0,
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_expr(
        &self,
        expr: &Expr,
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<BasicValueEnum<'ctx>> {
        match expr {
            Expr::IntLit(n) => Ok(self.context.i64_type().const_int(*n as u64, false).into()),
            Expr::FloatLit(f) => Ok(self.context.f64_type().const_float(*f).into()),
            Expr::BoolLit(b) => Ok(self.context.bool_type().const_int(if *b { 1 } else { 0 }, false).into()),
            Expr::StringLit(s) => {
                let name = format!("dialc.str.{}", self.next_str_id());
                let g = self.global_string(&name, s);
                Ok(self.call_c("dial_string_new", &[g.into()]))
            }
            Expr::Var(name) => {
                let info = scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.get(name).map(|v| v.clone()))
                    .ok_or_else(|| DialcError::new(format!("未声明的变量 '{}'", name), 0, 0))?;
                let (ty, (_m, ptr)) = info;
                let llvm_ty = self.dialc_type_to_llvm(&ty);
                Ok(self.builder.build_load(llvm_ty, ptr, name).unwrap())
            }
            Expr::Field(obj, field) => {
                let lptr = self.gen_lvalue_ptr(obj, function, scopes, func_values, functions, structs)?;
                let ot = type_of(obj, scopes, functions, structs)?;
                let sty = self.get_struct_type(&ot)?;
                let (fidx, fty) = self.field_info(&ot, field, structs, 0, 0)?;
                let fgep = self.builder.build_struct_gep(sty, lptr, fidx, "fgep").unwrap();
                Ok(self.builder.build_load(self.dialc_type_to_llvm(&fty), fgep, "fld").unwrap())
            }
            Expr::Index(arr, idx) => {
                let at = type_of(arr, scopes, functions, structs)?;
                let aptr = self.gen_expr(arr, function, scopes, func_values, functions, structs)?;
                let ival = self.gen_expr(idx, function, scopes, func_values, functions, structs)?;
                let ii64 = self.to_i64(ival, &type_of(idx, scopes, functions, structs)?);
                let elem_t = match at {
                    Type::Array(e) => *e,
                    _ => Type::Int,
                };
                let res = self.call_c("dial_array_get", &[aptr.into(), ii64.into()]);
                // 元素是 int（v0.2）；若将来支持其他元素类型在此分支
                let _ = elem_t;
                Ok(res)
            }
            Expr::ArrayLit(elems) => {
                let mut cur = self.call_c("dial_array_new", &[]);
                for el in elems {
                    let ev = self.gen_expr(el, function, scopes, func_values, functions, structs)?;
                    let ei64 = self.to_i64(ev, &type_of(el, scopes, functions, structs)?);
                    cur = self.call_c("dial_array_push", &[cur.into(), ei64.into()]);
                }
                Ok(cur)
            }
            Expr::StructLit(name, fields) => {
                let sty = self.struct_types[name];
                let sdef = structs[name].clone();
                let ptr = self.alloca_in_entry(function, sty.into(), "slit");
                // 零初始化：结构体字面量允许省略字段，省略者取其类型的零值（确定性语义，杜绝未初始化内存）
                let zero = sty.const_zero();
                self.builder.build_store(ptr, zero).unwrap();
                // 按字段名定位下标，而非字面量书写顺序（顺序无关，杜绝错位）
                for (fname, fe) in fields.iter() {
                    let fidx = sdef
                        .iter()
                        .position(|(n, _)| n == fname)
                        .ok_or_else(|| {
                            DialcError::new(
                                format!("语义错误：结构体 '{}' 不存在字段 '{}'", name, fname),
                                0, 0,
                            )
                        })?;
                    let fty = sdef[fidx].1.clone();
                    let gep = self
                        .builder
                        .build_struct_gep(sty, ptr, fidx as u32, "fgep")
                        .unwrap();
                    let fv = self.gen_expr(fe, function, scopes, func_values, functions, structs)?;
                    let stored = self.coerce(fv, &type_of(fe, scopes, functions, structs)?, &fty);
                    self.builder.build_store(gep, stored).unwrap();
                }
                Ok(self.builder.build_load(sty, ptr, "sval").unwrap())
            }
            Expr::Unary(op, e) => match op {
                UnOp::Neg => {
                    let v = self.gen_expr(e, function, scopes, func_values, functions, structs)?;
                    if v.is_float_value() {
                        Ok(self.builder.build_float_neg(v.into_float_value(), "neg").unwrap().into())
                    } else {
                        Ok(self.builder.build_int_neg(v.into_int_value(), "neg").unwrap().into())
                    }
                }
                UnOp::Not => {
                    let v = self.gen_expr(e, function, scopes, func_values, functions, structs)?;
                    Ok(self.builder.build_not(v.into_int_value(), "not").unwrap().into())
                }
            },
            Expr::Method(obj, method, args) => self.gen_method_call(obj, method, args, function, scopes, func_values, functions, structs),
            Expr::Binary(left, op, right) => self.gen_binary(*op, left, right, function, scopes, func_values, functions, structs),
            Expr::Call(name, args) => {
                // 内置类型转换函数：与零隐式转换铁律配合，是 int<->float 的唯一合法通道。
                // 直接编译为 LLVM 的 sitofp / fptosi，不依赖任何 C 运行时。
                match name.as_str() {
                    "int_to_float" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self
                            .builder
                            .build_signed_int_to_float(
                                v.into_int_value(),
                                self.context.f64_type(),
                                "itof",
                            )
                            .unwrap()
                            .into())
                    }
                    "float_to_int" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self
                            .builder
                            .build_float_to_signed_int(
                                v.into_float_value(),
                                self.context.i64_type(),
                                "ftoi",
                            )
                            .unwrap()
                            .into())
                    }
                    "new_map" => Ok(self.call_c("dial_map_new", &[])),
                    "read_line" => Ok(self.call_c("dial_read_line", &[])),
                    _ => {
                        if name == "print" {
                            let arg = &args[0];
                            let ty = type_of(arg, scopes, functions, structs)?;
                            let val = self.gen_expr(arg, function, scopes, func_values, functions, structs)?;
                            self.emit_print(&ty, val)?;
                            Ok(self.context.i64_type().const_int(0, false).into())
                        } else {
                            let _info = functions.get(name).ok_or_else(|| {
                                DialcError::new(format!("调用了未定义的函数 '{}'", name), 0, 0)
                            })?;
                            let fv = func_values.get(name).ok_or_else(|| {
                                DialcError::new(format!("缺少函数 '{}' 的 LLVM 定义", name), 0, 0)
                            })?;
                            let mut arg_vals = Vec::new();
                            for a in args {
                                arg_vals.push(self.gen_expr(a, function, scopes, func_values, functions, structs)?);
                            }
                            let metadata: Vec<BasicMetadataValueEnum> =
                                arg_vals.iter().map(|v| (*v).into()).collect();
                            let call = self.builder.build_call(*fv, &metadata, name).unwrap();
                            let rv = call.try_as_basic_value().basic().ok_or_else(|| {
                                DialcError::new(
                                    format!("函数 '{}' 返回 void，当前版本暂不支持用作表达式", name),
                                    0, 0,
                                )
                            })?;
                            Ok(rv)
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_method_call(
        &self,
        obj: &Expr,
        method: &str,
        args: &[Expr],
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<BasicValueEnum<'ctx>> {
        let ot = type_of(obj, scopes, functions, structs)?;
        let optr = self.gen_expr(obj, function, scopes, func_values, functions, structs)?;
        let i64t = self.context.i64_type();
        match (&ot, method) {
            (Type::String, "length") => Ok(self.call_c("dial_string_length", &[optr.into()])),
            (Type::String, "concat") => {
                let b = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                Ok(self.call_c("dial_string_concat", &[optr.into(), b.into()]))
            }
            (Type::String, "get") => {
                let i = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_string_get", &[optr.into(), i.into()]))
            }
            (Type::String, "substring") => {
                let from = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                let to = self.to_i64(self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?, &type_of(&args[1], scopes, functions, structs)?);
                Ok(self.call_c("dial_string_substring", &[optr.into(), from.into(), to.into()]))
            }
            (Type::String, "clone") => Ok(self.call_c("dial_string_clone", &[optr.into()])),
            (Type::String, "eq") => {
                let b = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                let r = self.call_c("dial_string_eq", &[optr.into(), b.into()]);
                let zero = i64t.const_int(0, false);
                Ok(self.builder.build_int_compare(IntPredicate::NE, r.into_int_value(), zero, "eq").unwrap().into())
            }
            (Type::Array(_), "length") => Ok(self.call_c("dial_array_length", &[optr.into()])),
            (Type::Array(_), "push") => {
                let v = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_array_push", &[optr.into(), v.into()]))
            }
            (Type::Array(_), "get") => {
                let i = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_array_get", &[optr.into(), i.into()]))
            }
            (Type::Array(_), "set") => {
                let i = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                let v = self.to_i64(self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?, &type_of(&args[1], scopes, functions, structs)?);
                Ok(self.call_c("dial_array_set", &[optr.into(), i.into(), v.into()]))
            }
            (Type::Array(_), "clone") => Ok(self.call_c("dial_array_clone", &[optr.into()])),
            (Type::Map, "put") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                let v = self.to_i64(self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?, &type_of(&args[1], scopes, functions, structs)?);
                Ok(self.call_c("dial_map_put", &[optr.into(), k.into(), v.into()]))
            }
            (Type::Map, "get") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_map_get", &[optr.into(), k.into()]))
            }
            (Type::Map, "contains") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                let r = self.call_c("dial_map_contains", &[optr.into(), k.into()]);
                Ok(self.builder.build_int_compare(IntPredicate::NE, r.into_int_value(), i64t.const_int(0, false), "mcontains").unwrap().into())
            }
            (Type::Map, "remove") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_map_remove", &[optr.into(), k.into()]))
            }
            (Type::Map, "size") => Ok(self.call_c("dial_map_size", &[optr.into()])),
            _ => Err(DialcError::new(
                format!("语义错误：类型 '{}' 无方法 '{}'（应由 Sema 先行拦截）", ot.name(), method),
                0, 0,
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_binary(
        &self,
        op: BinOp,
        l: &Expr,
        r: &Expr,
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<BasicValueEnum<'ctx>> {
        match op {
            BinOp::And | BinOp::Or => {
                // 短路求值（与 C 的 && / || 一致，这是现代语言的常识）：
                //   AND：先求左操作数；若为假，结果即为假，不再求值右操作数；否则结果等于右操作数。
                //   OR ：先求左操作数；若为真，结果即为真，不再求值右操作数；否则结果等于右操作数。
                // 若照搬 build_and / build_or，两侧会被同时求值，既违背短路语义，也会让
                // “判空 + 副作用”这类代码在左操作数为假时崩溃。
                //
                // 实现：结果写入 entry 块的栈槽，两条分支各自 store，merge 处 load 出来
                // （不依赖 PhiValue，规避其不属于 BasicValueEnum 的 API 限制）。
                let lval = self.gen_expr(l, function, scopes, func_values, functions, structs)?;
                let lbit = lval.into_int_value();
                let bool_t = self.context.bool_type();
                let slot = self.alloca_in_entry(function, bool_t.into(), "sc.res");
                let eval_bb = self.context.append_basic_block(function, "sc.eval");
                let short_bb = self.context.append_basic_block(function, "sc.short");
                let merge_bb = self.context.append_basic_block(function, "sc.merge");
                // 短路分支直接给出的常量结果：AND 短路为假，OR 短路为真。
                let short_const = bool_t.const_int(if op == BinOp::And { 0 } else { 1 }, false);
                if op == BinOp::And {
                    self.builder.build_conditional_branch(lbit, eval_bb, short_bb).unwrap();
                } else {
                    self.builder.build_conditional_branch(lbit, short_bb, eval_bb).unwrap();
                }
                // 求值分支：求右操作数并写入结果槽
                self.builder.position_at_end(eval_bb);
                let rval = self.gen_expr(r, function, scopes, func_values, functions, structs)?;
                let rbit = rval.into_int_value();
                self.builder.build_store(slot, rbit).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                // 短路分支：直接写入常量结果
                self.builder.position_at_end(short_bb);
                self.builder.build_store(slot, short_const).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                // merge：取出结果
                self.builder.position_at_end(merge_bb);
                let res = self.builder.build_load(bool_t, slot, "sc.res").unwrap();
                Ok(res)
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                let tl = type_of(l, scopes, functions, structs)?;
                let tr = type_of(r, scopes, functions, structs)?;
                let lval = self.gen_expr(l, function, scopes, func_values, functions, structs)?;
                let rval = self.gen_expr(r, function, scopes, func_values, functions, structs)?;
                let res_ty = if tl == Type::Float || tr == Type::Float {
                    Type::Float
                } else {
                    Type::Int
                };
                let result: BasicValueEnum = if res_ty == Type::Float {
                    let a = self.to_float(lval);
                    let b = self.to_float(rval);
                    match op {
                        BinOp::Add => self.builder.build_float_add(a, b, "add").unwrap().into(),
                        BinOp::Sub => self.builder.build_float_sub(a, b, "sub").unwrap().into(),
                        BinOp::Mul => self.builder.build_float_mul(a, b, "mul").unwrap().into(),
                        BinOp::Div => self.builder.build_float_div(a, b, "div").unwrap().into(),
                        _ => unreachable!(),
                    }
                } else {
                    let a = lval.into_int_value();
                    let b = rval.into_int_value();
                    match op {
                        BinOp::Add => self.builder.build_int_add(a, b, "add").unwrap().into(),
                        BinOp::Sub => self.builder.build_int_sub(a, b, "sub").unwrap().into(),
                        BinOp::Mul => self.builder.build_int_mul(a, b, "mul").unwrap().into(),
                        BinOp::Div => self.builder.build_int_signed_div(a, b, "div").unwrap().into(),
                        _ => unreachable!(),
                    }
                };
                Ok(result)
            }
            BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                let tl = type_of(l, scopes, functions, structs)?;
                let tr = type_of(r, scopes, functions, structs)?;
                let lval = self.gen_expr(l, function, scopes, func_values, functions, structs)?;
                let rval = self.gen_expr(r, function, scopes, func_values, functions, structs)?;
                if tl == Type::String && tr == Type::String {
                    // dial_string_eq 的约定是“相等返回 1、不等返回 0”，
                    // 因此 `a == b` 对应“结果 != 0”，`a != b` 对应“结果 == 0”。
                    // 注意与数值比较的方向相反，切勿照抄 IntPredicate::EQ。
                    let cmp = self.call_c("dial_string_eq", &[lval.into(), rval.into()]);
                    let zero = self.context.i64_type().const_int(0, false);
                    let pred = if op == BinOp::Eq { IntPredicate::NE } else { IntPredicate::EQ };
                    return Ok(self.builder.build_int_compare(pred, cmp.into_int_value(), zero, "streq").unwrap().into());
                }
                let result: BasicValueEnum = if tl == Type::Bool {
                    let a = lval.into_int_value();
                    let b = rval.into_int_value();
                    let pred = if op == BinOp::Eq { IntPredicate::EQ } else { IntPredicate::NE };
                    self.builder.build_int_compare(pred, a, b, "cmp").unwrap().into()
                } else if tl == Type::Float || tr == Type::Float {
                    let a = self.to_float(lval);
                    let b = self.to_float(rval);
                    let pred = match op {
                        BinOp::Eq => FloatPredicate::OEQ,
                        BinOp::Neq => FloatPredicate::ONE,
                        BinOp::Lt => FloatPredicate::OLT,
                        BinOp::Gt => FloatPredicate::OGT,
                        BinOp::Le => FloatPredicate::OLE,
                        BinOp::Ge => FloatPredicate::OGE,
                        _ => unreachable!(),
                    };
                    self.builder.build_float_compare(pred, a, b, "cmp").unwrap().into()
                } else {
                    let a = lval.into_int_value();
                    let b = rval.into_int_value();
                    let pred = match op {
                        BinOp::Eq => IntPredicate::EQ,
                        BinOp::Neq => IntPredicate::NE,
                        BinOp::Lt => IntPredicate::SLT,
                        BinOp::Gt => IntPredicate::SGT,
                        BinOp::Le => IntPredicate::SLE,
                        BinOp::Ge => IntPredicate::SGE,
                        _ => unreachable!(),
                    };
                    self.builder.build_int_compare(pred, a, b, "cmp").unwrap().into()
                };
                Ok(result)
            }
        }
    }

    fn emit_print(&self, ty: &Type, val: BasicValueEnum<'ctx>) -> Result<()> {
        match *ty {
            Type::Int => {
                self.call_c_void("dial_print_int", &[val.into()]);
            }
            Type::Float => {
                self.call_c_void("dial_print_float", &[val.into()]);
            }
            Type::Bool => {
                let i = self.to_i64(val, &Type::Bool);
                self.call_c_void("dial_print_bool", &[i.into()]);
            }
            Type::String => {
                self.call_c_void("dial_print_string", &[val.into()]);
            }
            // 与语义层保持同一口径：数组/结构体/v 均无对应打印实现，
            // 绝不能退化成 dial_print_string（那样会把二进制 payload 当字符串吐出乱码）。
            Type::Array(_) => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print 数组，请逐元素打印 print(a.get(i))",
                    0, 0,
                ));
            }
            Type::Struct(_) => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print 结构体，请逐字段打印",
                    0, 0,
                ));
            }
            Type::Map => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print 哈希表，请逐值打印 print(m.get(k))",
                    0, 0,
                ));
            }
            Type::Void => {
                return Err(DialcError::new("语义错误：print 的参数不能是 void", 0, 0));
            }
        }
        Ok(())
    }
}

/// 判断函数是否返回 void。
fn f_ret_is_void(function: FunctionValue) -> bool {
    function.get_type().get_return_type().is_none()
}

/// 求 lvalue 表达式根部的变量名（用于数组索引赋值等）。
fn lvalue_root(e: &Expr) -> Option<&str> {
    match e {
        Expr::Var(n) => Some(n),
        Expr::Field(o, _) => lvalue_root(o),
        Expr::Index(o, _) => lvalue_root(o),
        _ => None,
    }
}
