// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the "hack" directory of this source tree.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Error;
use ascii::AsciiString;
use ffi::Str;
use ir::instr::HasLoc;
use ir::instr::HasLocals;
use ir::instr::Hhbc;
use ir::instr::Predicate;
use ir::instr::Special;
use ir::instr::Terminator;
use ir::instr::Textual;
use ir::BlockId;
use ir::ClassId;
use ir::ClassName;
use ir::ConstId;
use ir::Constant;
use ir::Func;
use ir::IncDecOp;
use ir::Instr;
use ir::InstrId;
use ir::LocId;
use ir::LocalId;
use ir::SpecialClsRef;
use ir::StringInterner;
use ir::UnitBytesId;
use ir::ValueId;
use itertools::Itertools;
use log::trace;

use crate::class;
use crate::class::IsStatic;
use crate::hack;
use crate::lower;
use crate::mangle::Mangle as _;
use crate::mangle::MangleWithClass as _;
use crate::state::UnitState;
use crate::textual;
use crate::textual::Sid;
use crate::textual::TextualFile;
use crate::typed_value;
use crate::types::convert_ty;
use crate::util;

type Result<T = (), E = Error> = std::result::Result<T, E>;

/// Functions are defined as taking a param bundle.
///
/// f(params: HackParams): mixed;
pub(crate) fn write_function(
    txf: &mut TextualFile<'_>,
    state: &mut UnitState,
    function: ir::Function<'_>,
) -> Result {
    trace!("Convert Function {}", function.name.as_bstr(&state.strings));

    let func = lower::lower_func(function.func, None, Arc::clone(&state.strings));
    ir::verify::verify_func(&func, &Default::default(), &state.strings)?;

    write_func(
        txf,
        state,
        function.name.id,
        textual::Ty::VoidPtr,
        func,
        None,
    )
}

pub(crate) fn compute_func_params<'a>(
    params: &Vec<ir::Param<'_>>,
    unit_state: &'a mut UnitState,
    this_ty: textual::Ty,
) -> Result<(Vec<AsciiString>, Vec<textual::Ty>, HashSet<LocalId>)> {
    // Prepend the 'this' parameter.
    let this_name = AsciiString::from_str("$this").unwrap();
    let this_lid = LocalId::Named(unit_state.strings.intern_str("$this"));
    let mut param_names = vec![this_name];
    let mut param_tys = vec![this_ty];
    let mut param_lids: HashSet<LocalId> = [this_lid].into_iter().collect();
    for p in params {
        let name_bytes = unit_state.strings.lookup_bytes(p.name);
        let name_string = util::escaped_string(&name_bytes);
        param_names.push(name_string);
        param_tys.push(convert_ty(&p.ty.enforced, &unit_state.strings));
        param_lids.insert(LocalId::Named(p.name));
    }

    Ok((param_names, param_tys, param_lids))
}

pub(crate) fn write_func(
    txf: &mut TextualFile<'_>,
    unit_state: &mut UnitState,
    name: UnitBytesId,
    this_ty: textual::Ty,
    mut func: ir::Func<'_>,
    method_info: Option<Arc<MethodInfo<'_>>>,
) -> Result {
    let params = std::mem::take(&mut func.params);
    let (param_names, param_tys, param_lids) = compute_func_params(&params, unit_state, this_ty)?;
    let tx_params = param_names
        .iter()
        .map(|s| s.as_str())
        .zip(param_tys.iter())
        .collect_vec();

    let ret_ty = convert_ty(&func.return_type.enforced, &unit_state.strings);

    let lids = func
        .body_instrs()
        .flat_map(HasLocals::locals)
        .cloned()
        .collect::<HashSet<_>>();
    // TODO(arr): figure out how to provide more precise types
    let local_ty = textual::Ty::VoidPtr;
    let mut locals = lids
        .into_iter()
        .filter(|lid| !param_lids.contains(lid))
        .sorted_by(|x, y| cmp_lid(&unit_state.strings, x, y))
        .zip(std::iter::repeat(&local_ty))
        .collect::<Vec<_>>();

    // Always add a temp var for use by member_ops.
    let base = crate::member_op::base_var(&unit_state.strings);
    let base_ty = textual::Ty::mixed();
    locals.push((base, &base_ty));

    let name_str = if let Some(method_info) = method_info.as_ref() {
        ir::MethodId::new(name).mangle_with_class(
            method_info.class.name,
            method_info.is_static,
            &unit_state.strings,
        )
    } else {
        ir::FunctionId::new(name).mangle(&unit_state.strings)
    };

    let span = func.loc(func.loc_id).clone();
    txf.define_function(&name_str, &span, &tx_params, &ret_ty, &locals, {
        let method_info = method_info.as_ref().map(Arc::clone);
        |fb| {
            let mut func = rewrite_jmp_ops(func);
            ir::passes::clean::run(&mut func);

            let mut state = FuncState::new(fb, Arc::clone(&unit_state.strings), &func, method_info);

            for bid in func.block_ids() {
                write_block(&mut state, bid)?;
            }

            Ok(())
        }
    })?;

    // For a static method also generate an instance stub which forwards to the
    // static method.

    if let Some(method_info) = &method_info {
        if method_info.is_static == IsStatic::Static {
            write_instance_stub(
                txf,
                unit_state,
                name,
                method_info,
                &tx_params,
                &ret_ty,
                &span,
            )?;
        }
    }

    Ok(())
}

/// For each static method we also write a non-static version of the method so
/// that callers of 'self::foo()' don't have to know if foo is static or
/// non-static.
fn write_instance_stub(
    txf: &mut TextualFile<'_>,
    unit_state: &mut UnitState,
    name: UnitBytesId,
    method_info: &MethodInfo<'_>,
    tx_params: &[(&str, &textual::Ty)],
    ret_ty: &textual::Ty,
    span: &ir::SrcLoc,
) -> Result {
    let strings = &unit_state.strings;
    let name_str = ir::MethodId::new(name).mangle_with_class(
        method_info.class.name,
        IsStatic::NonStatic,
        strings,
    );

    let mut tx_params = tx_params.to_vec();
    let inst_ty = method_info.non_static_ty();
    tx_params[0].1 = &inst_ty;

    let locals = Vec::default();
    txf.define_function(&name_str, span, &tx_params, ret_ty, &locals, |fb| {
        fb.comment("forward to the static method")?;
        let this_str = strings.intern_str("$this");
        let this_lid = LocalId::Named(this_str);
        let this = fb.load(&inst_ty, textual::Expr::deref(this_lid))?;
        let static_this = hack::call_builtin(fb, hack::Builtin::GetStaticClass, [this])?;
        let target = ir::MethodId::new(name).mangle_with_class(
            method_info.class.name,
            IsStatic::Static,
            strings,
        );

        let params: Vec<Sid> = std::iter::once(Ok(static_this))
            .chain(tx_params.iter().skip(1).map(|(name, ty)| {
                let lid = LocalId::Named(strings.intern_str(*name));
                fb.load(ty, textual::Expr::deref(lid))
            }))
            .try_collect()?;

        let call = fb.call(&target, params)?;
        fb.ret(call)?;
        Ok(())
    })?;

    Ok(())
}

fn write_block(state: &mut FuncState<'_, '_, '_>, bid: BlockId) -> Result {
    trace!("  Block {bid}");
    let block = state.func.block(bid);

    let params = block
        .params
        .iter()
        .map(|iid| state.alloc_sid_for_iid(*iid))
        .collect_vec();
    // The entry BID is always included for us.
    if bid != Func::ENTRY_BID {
        state.fb.write_label(bid, &params)?;
    }

    // All the non-terminators.
    let n_iids = block.iids.len() - 1;
    for iid in &block.iids[..n_iids] {
        write_instr(state, *iid)?;
    }

    // The terminator.
    write_terminator(state, block.terminator_iid())?;

    // Exception handler.
    let handler = state.func.catch_target(bid);
    if handler != BlockId::NONE {
        state.fb.write_exception_handler(handler)?;
    }

    Ok(())
}

fn write_instr(state: &mut FuncState<'_, '_, '_>, iid: InstrId) -> Result {
    let instr = state.func.instr(iid);
    trace!("    Instr {iid}: {instr:?}");

    state.update_loc(instr.loc_id())?;

    // In general don't write directly to `w` here - isolate the formatting to
    // the `textual` crate.

    match *instr {
        Instr::Call(ref call) => write_call(state, iid, call)?,
        Instr::Hhbc(Hhbc::ClsCnsD(const_id, cid, _)) => {
            let vid = write_get_class_const(state, cid, const_id)?;
            state.set_iid(iid, vid);
        }
        Instr::Hhbc(Hhbc::CreateCl {
            ref operands,
            clsid,
            loc: _,
        }) => {
            let ty = class::non_static_ty(clsid, &state.strings).deref();
            let cons = ir::MethodId::from_str("__construct", &state.strings).mangle_with_class(
                clsid,
                IsStatic::NonStatic,
                &state.strings,
            );
            let obj = state.fb.write_expr_stmt(textual::Expr::Alloc(ty))?;
            let operands = operands
                .iter()
                .map(|vid| state.lookup_vid(*vid))
                .collect_vec();
            state.fb.call_static(&cons, obj.into(), operands)?;
            state.set_iid(iid, obj);
        }
        Instr::Hhbc(Hhbc::CGetL(lid, _) | Hhbc::CUGetL(lid, _) | Hhbc::ConsumeL(lid, _)) => {
            write_load_var(state, iid, lid)?
        }
        Instr::Hhbc(Hhbc::IncDecL(lid, op, _)) => write_inc_dec_l(state, iid, lid, op)?,
        Instr::Hhbc(Hhbc::ResolveClass(cid, _)) => {
            let vid = state.load_static_class(cid)?;
            state.set_iid(iid, vid);
        }
        Instr::Hhbc(Hhbc::SelfCls(_)) => {
            let method_info = state
                .method_info
                .as_ref()
                .expect("SelfCls used in non-method context");
            let cid = method_info.class.name;
            let vid = state.load_static_class(cid)?;
            state.set_iid(iid, vid);
        }
        Instr::Hhbc(Hhbc::SetL(vid, lid, _)) => {
            write_set_var(state, lid, vid)?;
            // SetL emits the input as the output.
            state.copy_iid(iid, vid);
        }
        Instr::Hhbc(Hhbc::This(_)) => write_load_this(state, iid)?,
        Instr::Hhbc(Hhbc::UnsetL(lid, _)) => {
            state.store_mixed(
                textual::Expr::deref(lid),
                textual::Expr::Const(textual::Const::Null),
            )?;
        }
        Instr::MemberOp(ref mop) => crate::member_op::write(state, iid, mop)?,
        Instr::Special(Special::Textual(Textual::AssertFalse(vid, _))) => {
            // I think "prune_not" means "stop if this expression IS true"...
            let pred = hack::expr_builtin(hack::Builtin::IsTrue, [state.lookup_vid(vid)]);
            state.fb.prune_not(pred)?;
        }
        Instr::Special(Special::Textual(Textual::AssertTrue(vid, _))) => {
            // I think "prune" means "stop if this expression IS NOT true"...
            let pred = hack::expr_builtin(hack::Builtin::IsTrue, [state.lookup_vid(vid)]);
            state.fb.prune(pred)?;
        }
        Instr::Special(Special::Textual(Textual::Deref(..))) => {
            // Do nothing - the expectation is that this will be emitted as a
            // Expr inlined in the target instruction (from lookup_iid()).
        }
        Instr::Special(Special::Textual(Textual::HackBuiltin {
            ref target,
            ref values,
            loc: _,
        })) => write_builtin(state, iid, target, values)?,
        Instr::Special(Special::Textual(Textual::String(s))) => {
            let expr = {
                let s = state.strings.lookup_bstr(s);
                let s = util::escaped_string(&s);
                let s = hack::expr_builtin(hack::Builtin::String, [s]);
                state.fb.copy(s)?
            };
            state.set_iid(iid, expr);
        }

        Instr::Special(Special::Copy(vid)) => {
            write_copy(state, iid, vid)?;
        }
        Instr::Special(Special::IrToBc(..)) => todo!(),
        Instr::Special(Special::Param) => todo!(),
        Instr::Special(Special::Select(vid, _idx)) => {
            textual_todo! {
                let vid = state.lookup_vid(vid);
                let expr = state.fb.copy(vid)?;
                state.set_iid(iid, expr);
            }
        }
        Instr::Special(Special::Tmp(..)) => todo!(),
        Instr::Special(Special::Tombstone) => todo!(),

        Instr::Hhbc(ref hhbc) => {
            // This should only handle instructions that can't be rewritten into
            // a simpler form (like control flow and generic calls). Everything
            // else should be handled in lower().
            trace!("TODO: {hhbc:?}");
            textual_todo! {
                use ir::instr::HasOperands;
                let name = format!("TODO_hhbc_{}", hhbc);
                let operands = instr
                    .operands()
                    .iter()
                    .map(|vid| state.lookup_vid(*vid))
                    .collect_vec();
                let output = state.fb.call(&name,operands)?;
                state.set_iid(iid, output);
            }
        }

        Instr::Terminator(_) => unreachable!(),
    }

    Ok(())
}

fn write_copy(state: &mut FuncState<'_, '_, '_>, iid: InstrId, vid: ValueId) -> Result {
    use hack::Builtin;
    use textual::Const;
    use textual::Expr;
    use typed_value::typed_value_expr;

    match vid.full() {
        ir::FullInstrId::Constant(cid) => {
            let constant = state.func.constant(cid);
            let expr = match constant {
                Constant::Array(tv) => typed_value_expr(tv, &state.strings),
                Constant::Bool(false) => Expr::Const(Const::False),
                Constant::Bool(true) => Expr::Const(Const::True),
                Constant::Dir => todo!(),
                Constant::File => todo!(),
                Constant::Float(f) => Expr::Const(Const::Float(f.to_f64())),
                Constant::FuncCred => todo!(),
                Constant::Int(i) => hack::expr_builtin(Builtin::Int, [Expr::Const(Const::Int(*i))]),
                Constant::Method => todo!(),
                Constant::Named(..) => todo!(),
                Constant::NewCol(..) => todo!(),
                Constant::Null => Expr::Const(Const::Null),
                Constant::String(s) => {
                    let s = util::escaped_string(&state.strings.lookup_bytes(*s));
                    hack::expr_builtin(Builtin::String, [Expr::Const(Const::String(s))])
                }
                Constant::Uninit => Expr::Const(Const::Null),
            };

            let expr = state.fb.write_expr_stmt(expr)?;
            state.set_iid(iid, expr);
        }
        ir::FullInstrId::Instr(instr) => state.copy_iid(iid, ValueId::from_instr(instr)),
        ir::FullInstrId::None => unreachable!(),
    }
    Ok(())
}

fn write_get_class_const(
    state: &mut FuncState<'_, '_, '_>,
    class: ClassId,
    cid: ConstId,
) -> Result<Sid> {
    // TODO: should we load the class static to ensure that the constants are initialized?
    // let this = state.load_static_class(class)?;

    let name = cid.mangle_with_class(class, IsStatic::Static, &state.strings);
    let var = textual::Var::global(name);
    state.load_mixed(textual::Expr::deref(var))
}

fn write_terminator(state: &mut FuncState<'_, '_, '_>, iid: InstrId) -> Result {
    let terminator = match state.func.instr(iid) {
        Instr::Terminator(terminator) => terminator,
        _ => unreachable!(),
    };
    trace!("    Instr {iid}: {terminator:?}");

    state.update_loc(terminator.loc_id())?;

    // In general don't write directly to `w` here - isolate the formatting to
    // the `textual` crate.

    match *terminator {
        Terminator::Enter(bid, _) | Terminator::Jmp(bid, _) => {
            state.fb.jmp(&[bid], ())?;
        }
        Terminator::Exit(msg, _) => {
            let msg = state.lookup_vid(msg);
            state.call_builtin(hack::Builtin::Hhbc(hack::Hhbc::Exit), [msg])?;
            state.fb.unreachable()?;
        }
        Terminator::Fatal(msg, _, _) => {
            let msg = state.lookup_vid(msg);
            state.call_builtin(hack::Builtin::Hhbc(hack::Hhbc::Fatal), [msg])?;
            state.fb.unreachable()?;
        }
        Terminator::JmpArgs(bid, ref params, _) => {
            let params = params.iter().map(|v| state.lookup_vid(*v)).collect_vec();
            state.fb.jmp(&[bid], params)?;
        }
        Terminator::JmpOp {
            cond: _,
            pred: _,
            targets: [true_bid, false_bid],
            loc: _,
        } => {
            // We just need to emit the jmp - the rewrite_jmp_ops() pass should
            // have already inserted assert in place on the target bids.
            state.fb.jmp(&[true_bid, false_bid], ())?;
        }
        Terminator::Ret(vid, _) => {
            let sid = state.lookup_vid(vid);
            state.fb.ret(sid)?;
        }
        Terminator::Unreachable => {
            state.fb.unreachable()?;
        }

        Terminator::CallAsync(..)
        | Terminator::IterInit(..)
        | Terminator::IterNext(..)
        | Terminator::MemoGet(..)
        | Terminator::MemoGetEager(..)
        | Terminator::NativeImpl(..)
        | Terminator::RetCSuspended(..)
        | Terminator::RetM(..)
        | Terminator::SSwitch { .. }
        | Terminator::Switch { .. }
        | Terminator::ThrowAsTypeStructException { .. } => {
            state.write_todo(&format!("{}", terminator))?;
            state.fb.unreachable()?;
        }

        Terminator::Throw(vid, _) => {
            textual_todo! {
                let expr = state.lookup_vid(vid);
                state.fb.call("TODO_throw", [expr])?;
                state.fb.unreachable()?;
            }
        }
    }

    Ok(())
}

fn write_builtin(
    state: &mut FuncState<'_, '_, '_>,
    iid: InstrId,
    target: &str,
    values: &[ValueId],
) -> Result {
    let params = values
        .iter()
        .map(|vid| state.lookup_vid(*vid))
        .collect_vec();
    let output = state.fb.call(target, params)?;
    state.set_iid(iid, output);
    Ok(())
}

fn write_load_this(state: &mut FuncState<'_, '_, '_>, iid: InstrId) -> Result {
    let sid = state.load_this()?;
    state.set_iid(iid, sid);
    Ok(())
}

fn write_load_var(state: &mut FuncState<'_, '_, '_>, iid: InstrId, lid: LocalId) -> Result {
    let sid = state.load_mixed(textual::Expr::deref(lid))?;
    state.set_iid(iid, sid);
    Ok(())
}

fn write_set_var(state: &mut FuncState<'_, '_, '_>, lid: LocalId, vid: ValueId) -> Result {
    let value = state.lookup_vid(vid);
    state.store_mixed(textual::Expr::deref(lid), value)
}

fn write_call(state: &mut FuncState<'_, '_, '_>, iid: InstrId, call: &ir::Call) -> Result {
    use ir::instr::CallDetail;
    use ir::FCallArgsFlags;

    let ir::Call {
        ref operands,
        context,
        ref detail,
        flags,
        num_rets,
        ref inouts,
        ref readonly,
        loc: _,
    } = *call;

    if !inouts.as_ref().map_or(true, |inouts| inouts.is_empty()) {
        textual_todo! {
            state.fb.comment("TODO: inouts")?;
        }
    }

    assert!(readonly.as_ref().map_or(true, |ro| ro.is_empty()));

    if num_rets >= 2 {
        textual_todo! {
            state.fb.comment("TODO: num_rets >= 2")?;
        }
    }

    {
        let context = state.strings.lookup_bytes_or_none(context);
        if let Some(context) = context {
            if !context.is_empty() {
                textual_todo! {
                    state.fb.comment("TODO: write_call(Context: {context:?})")?;
                }
            }
        }
    }

    // flags &= FCallArgsFlags::LockWhileUnwinding - ignored
    let is_async = flags & FCallArgsFlags::HasAsyncEagerOffset != 0;

    if flags & FCallArgsFlags::HasUnpack != 0 {
        textual_todo! {
            state.fb.comment("TODO: FCallArgsFlags::HasUnpack")?;
        }
    }
    if flags & FCallArgsFlags::HasGenerics != 0 {
        textual_todo! {
            state.fb.comment("TODO: FCallArgsFlags::HasGenerics")?;
        }
    }
    if flags & FCallArgsFlags::SkipRepack != 0 {
        textual_todo! {
            state.fb.comment("TODO: FCallArgsFlags::SkipRepack")?;
        }
    }
    if flags & FCallArgsFlags::SkipCoeffectsCheck != 0 {
        textual_todo! {
            state.fb.comment("TODO: FCallArgsFlags::SkipCoeffectsCheck")?;
        }
    }
    if flags & FCallArgsFlags::EnforceMutableReturn != 0 {
        // todo!();
    }
    if flags & FCallArgsFlags::EnforceReadonlyThis != 0 {
        textual_todo! {
            state.fb.comment("TODO: FCallArgsFlags::EnforceReadonlyThis")?;
        }
    }
    if flags & FCallArgsFlags::ExplicitContext != 0 {
        textual_todo! {
            state.fb.comment("TODO: FCallArgsFlags::ExplicitContext")?;
        }
    }
    if flags & FCallArgsFlags::HasInOut != 0 {
        textual_todo! {
            state.fb.comment("TODO: FCallArgsFlags::HasInOut")?;
        }
    }
    if flags & FCallArgsFlags::EnforceInOut != 0 {
        textual_todo! {
            state.fb.comment("TODO: FCallArgsFlags::EnforceInOut")?;
        }
    }
    if flags & FCallArgsFlags::EnforceReadonly != 0 {
        textual_todo! {
            state.fb.comment("TODO: FCallArgsFlags::EnforceReadonly")?;
        }
    }
    if flags & FCallArgsFlags::NumArgsStart != 0 {
        textual_todo! {
            state.fb.comment("TODO: FCallArgsFlags::NumArgsStart")?;
        }
    }

    let args = detail.args(operands);
    let args = args
        .iter()
        .copied()
        .map(|vid| state.lookup_vid(vid))
        .collect_vec();

    let mut output = match *detail {
        CallDetail::FCallClsMethod { .. } => write_todo(state.fb, "FCallClsMethod")?,
        CallDetail::FCallClsMethodD { clsid, method } => {
            // C::foo()
            let target = method.mangle_with_class(clsid, IsStatic::Static, &state.strings);
            let this = state.load_static_class(clsid)?;
            state.fb.call_static(&target, this.into(), args)?
        }
        CallDetail::FCallClsMethodM { .. } => state.write_todo("TODO_FCallClsMethodM")?,
        CallDetail::FCallClsMethodS { .. } => state.write_todo("TODO_FCallClsMethodS")?,
        CallDetail::FCallClsMethodSD { clsref, method } => match clsref {
            SpecialClsRef::SelfCls => {
                // self::foo() - Static call to the method in the current class.
                let mi = state.expect_method_info();
                let target = method.mangle_with_class(mi.class.name, mi.is_static, &state.strings);
                let this = state.load_this()?;
                state.fb.call_static(&target, this.into(), args)?
            }
            SpecialClsRef::LateBoundCls => {
                // static::foo() - Virtual call to the method in the current class.
                let mi = state.expect_method_info();
                let target = method.mangle_with_class(mi.class.name, mi.is_static, &state.strings);
                let this = state.load_this()?;
                state.fb.call_virtual(&target, this.into(), args)?
            }
            SpecialClsRef::ParentCls => {
                // parent::foo() - Static call to the method in the parent class.
                let mi = state.expect_method_info();
                let is_static = mi.is_static;
                let base = if let Some(base) = mi.class.base {
                    base
                } else {
                    // Uh oh. We're asking to call parent::foo() when we don't
                    // have a known parent. This can happen in a trait...
                    ClassId::from_str("__parent__", &state.strings)
                };
                let this = state.load_this()?;
                let target = method.mangle_with_class(base, is_static, &state.strings);
                state.fb.call_static(&target, this.into(), args)?
            }
            _ => unreachable!(),
        },
        CallDetail::FCallCtor => unreachable!(),
        CallDetail::FCallFunc => state.write_todo("TODO_FCallFunc")?,
        CallDetail::FCallFuncD { func } => {
            // foo()
            let target = func.mangle(&state.strings);
            // A top-level function is called like a class static in a special
            // top-level class. Its 'this' pointer is null.
            state.fb.call_static(&target, textual::Expr::null(), args)?
        }
        CallDetail::FCallObjMethod { .. } => state.write_todo("FCallObjMethod")?,
        CallDetail::FCallObjMethodD { flavor, method } => {
            // $x->y()

            // This should have been handled in lowering.
            assert!(flavor != ir::ObjMethodOp::NullSafe);

            // TODO: need to try to figure out the type.
            let ty = ClassName::new(Str::new(b"HackMixed"));
            let target = method.mangle_with_class(&ty, IsStatic::NonStatic, &state.strings);
            let obj = state.lookup_vid(detail.obj(operands));
            state.fb.call_virtual(&target, obj, args)?
        }
    };

    if is_async {
        output = state.call_builtin(hack::Builtin::Await, [output])?;
    }

    state.set_iid(iid, output);
    Ok(())
}

fn write_inc_dec_l(
    state: &mut FuncState<'_, '_, '_>,
    iid: InstrId,
    lid: LocalId,
    op: IncDecOp,
) -> Result {
    let builtin = match op {
        IncDecOp::PreInc => hack::Hhbc::Add,
        IncDecOp::PostInc => hack::Hhbc::Add,
        IncDecOp::PreDec => hack::Hhbc::Sub,
        IncDecOp::PostDec => hack::Hhbc::Sub,
        _ => unreachable!(),
    };

    let pre = state.load_mixed(textual::Expr::deref(lid))?;
    let one = state.call_builtin(hack::Builtin::Int, [1])?;
    let post = state.call_builtin(hack::Builtin::Hhbc(builtin), (pre, one))?;
    state.store_mixed(textual::Expr::deref(lid), post)?;

    let sid = match op {
        IncDecOp::PreInc | IncDecOp::PreDec => pre,
        IncDecOp::PostInc | IncDecOp::PostDec => post,
        _ => unreachable!(),
    };
    state.set_iid(iid, sid);

    Ok(())
}

pub(crate) struct FuncState<'a, 'b, 'c> {
    pub(crate) fb: &'a mut textual::FuncBuilder<'b, 'c>,
    func: &'a ir::Func<'a>,
    iid_mapping: ir::InstrIdMap<textual::Expr>,
    method_info: Option<Arc<MethodInfo<'a>>>,
    pub(crate) strings: Arc<StringInterner>,
}

impl<'a, 'b, 'c> FuncState<'a, 'b, 'c> {
    fn new(
        fb: &'a mut textual::FuncBuilder<'b, 'c>,
        strings: Arc<StringInterner>,
        func: &'a ir::Func<'a>,
        method_info: Option<Arc<MethodInfo<'a>>>,
    ) -> Self {
        Self {
            fb,
            func,
            iid_mapping: Default::default(),
            method_info,
            strings,
        }
    }

    pub fn alloc_sid_for_iid(&mut self, iid: InstrId) -> Sid {
        let sid = self.fb.alloc_sid();
        self.set_iid(iid, sid);
        sid
    }

    pub(crate) fn call_builtin(
        &mut self,
        target: hack::Builtin,
        params: impl textual::VarArgs,
    ) -> Result<Sid> {
        hack::call_builtin(self.fb, target, params)
    }

    pub(crate) fn copy_iid(&mut self, iid: InstrId, input: ValueId) {
        let expr = self.lookup_vid(input);
        self.set_iid(iid, expr);
    }

    fn expect_method_info(&self) -> &MethodInfo<'_> {
        self.method_info.as_ref().expect("not in class context")
    }

    fn load_static_class(&mut self, cid: ClassId) -> Result<textual::Sid> {
        class::load_static_class(self.fb, cid, &self.strings)
    }

    pub(crate) fn load_mixed(&mut self, src: impl Into<textual::Expr>) -> Result<Sid> {
        self.fb.load(&textual::Ty::mixed(), src)
    }

    fn load_this(&mut self) -> Result<textual::Sid> {
        let var = LocalId::Named(self.strings.intern_str("$this"));
        let mi = self.expect_method_info();
        let ty = mi.class_ty();
        let this = self.fb.load(&ty, textual::Expr::deref(var))?;
        Ok(this)
    }

    pub(crate) fn lookup_iid(&self, iid: InstrId) -> textual::Expr {
        if let Some(expr) = self.iid_mapping.get(&iid) {
            return expr.clone();
        }

        // The iid wasn't found.  Maybe it's a "special" reference (like a
        // Deref()) - pessimistically look for that.
        match self.func.instr(iid) {
            Instr::Special(Special::Textual(Textual::Deref(lid))) => {
                return textual::Expr::deref(*lid);
            }
            _ => {}
        }

        panic!("failed to look up iid {iid}");
    }

    /// Look up a ValueId in the FuncState and return an Expr representing
    /// it. For InstrIds and complex ConstIds return an Expr containing the
    /// (already emitted) Sid. For simple ConstIds use an Expr representing the
    /// value directly.
    pub(crate) fn lookup_vid(&mut self, vid: ValueId) -> textual::Expr {
        match vid.full() {
            ir::FullInstrId::Instr(iid) => self.lookup_iid(iid),
            ir::FullInstrId::Constant(c) => {
                use hack::Builtin;
                use ir::CollectionType;
                let c = self.func.constant(c);
                match c {
                    Constant::Bool(false) => hack::expr_builtin(Builtin::Bool, [false]),
                    Constant::Bool(true) => hack::expr_builtin(Builtin::Bool, [true]),
                    Constant::Int(i) => hack::expr_builtin(Builtin::Int, [*i]),
                    Constant::Null => textual::Expr::null(),
                    Constant::String(s) => {
                        let s = self.strings.lookup_bstr(*s);
                        let s = util::escaped_string(&s);
                        hack::expr_builtin(Builtin::String, [s])
                    }
                    Constant::Array(..) => textual_todo! { textual::Expr::null() },
                    Constant::Dir => textual_todo! { textual::Expr::null() },
                    Constant::Float(f) => hack::expr_builtin(Builtin::Float, [f.to_f64()]),
                    Constant::File => textual_todo! { textual::Expr::null() },
                    Constant::FuncCred => textual_todo! { textual::Expr::null() },
                    Constant::Method => textual_todo! { textual::Expr::null() },
                    Constant::Named(..) => textual_todo! { textual::Expr::null() },
                    Constant::NewCol(CollectionType::ImmMap) => {
                        hack::expr_builtin(Builtin::Hhbc(hack::Hhbc::NewColImmMap), ())
                    }
                    Constant::NewCol(CollectionType::ImmSet) => {
                        hack::expr_builtin(Builtin::Hhbc(hack::Hhbc::NewColImmSet), ())
                    }
                    Constant::NewCol(CollectionType::ImmVector) => {
                        hack::expr_builtin(Builtin::Hhbc(hack::Hhbc::NewColImmVector), ())
                    }
                    Constant::NewCol(CollectionType::Map) => {
                        hack::expr_builtin(Builtin::Hhbc(hack::Hhbc::NewColMap), ())
                    }
                    Constant::NewCol(CollectionType::Pair) => {
                        hack::expr_builtin(Builtin::Hhbc(hack::Hhbc::NewColPair), ())
                    }
                    Constant::NewCol(CollectionType::Set) => {
                        hack::expr_builtin(Builtin::Hhbc(hack::Hhbc::NewColSet), ())
                    }
                    Constant::NewCol(CollectionType::Vector) => {
                        hack::expr_builtin(Builtin::Hhbc(hack::Hhbc::NewColVector), ())
                    }
                    Constant::NewCol(_) => unreachable!(),
                    Constant::Uninit => textual_todo! { textual::Expr::null() },
                }
            }
            ir::FullInstrId::None => unreachable!(),
        }
    }

    pub(crate) fn set_iid(&mut self, iid: InstrId, expr: impl Into<textual::Expr>) {
        let expr = expr.into();
        let old = self.iid_mapping.insert(iid, expr);
        assert!(old.is_none());
    }

    pub(crate) fn store_mixed(
        &mut self,
        dst: impl Into<textual::Expr>,
        src: impl Into<textual::Expr>,
    ) -> Result {
        self.fb.store(dst, src, &textual::Ty::mixed())
    }

    pub(crate) fn update_loc(&mut self, loc: LocId) -> Result {
        if loc != LocId::NONE {
            let new = &self.func.locs[loc];
            self.fb.write_loc(new)?;
        }
        Ok(())
    }

    pub(crate) fn write_todo(&mut self, msg: &str) -> Result<Sid> {
        trace!("TODO: {}", msg);
        textual_todo! {
            let target = format!("$todo.{msg}");
            self.fb.call(&target, ())
        }
    }
}

/// Convert from a deterministic jump model to a non-deterministic model.
///
/// In Textual instead of "jump if" you say "jump to a, b" and then in 'a' and 'b'
/// you say "stop if my condition isn't met".
///
/// This inserts the needed 'assert_true' and 'assert_false' statements but
/// leaves the original JmpOp as a marker for where to jump to.
fn rewrite_jmp_ops<'a>(mut func: ir::Func<'a>) -> ir::Func<'a> {
    for bid in func.block_ids() {
        match *func.terminator(bid) {
            Terminator::JmpOp {
                cond,
                pred,
                targets: [mut true_bid, mut false_bid],
                loc,
            } => {
                // We need to rewrite this jump. Because we don't allow critical
                // edges we can just insert the 'assert' at the start of the
                // target block since we must be the only caller.
                trace!("    JmpOp at {bid} needs to be rewritten");
                match pred {
                    Predicate::Zero => {
                        std::mem::swap(&mut true_bid, &mut false_bid);
                    }
                    Predicate::NonZero => {}
                }

                let iid = func.alloc_instr(Instr::Special(Special::Textual(Textual::AssertTrue(
                    cond, loc,
                ))));
                func.block_mut(true_bid).iids.insert(0, iid);

                let iid = func.alloc_instr(Instr::Special(Special::Textual(Textual::AssertFalse(
                    cond, loc,
                ))));
                func.block_mut(false_bid).iids.insert(0, iid);
            }
            _ => {}
        }
    }

    func
}

pub(crate) struct MethodInfo<'a> {
    pub(crate) class: &'a ir::Class<'a>,
    pub(crate) is_static: IsStatic,
    pub(crate) strings: Arc<ir::StringInterner>,
}

impl MethodInfo<'_> {
    pub(crate) fn non_static_ty(&self) -> textual::Ty {
        class::non_static_ty(self.class.name, &self.strings)
    }

    pub(crate) fn class_ty(&self) -> textual::Ty {
        class::class_ty(self.class.name, self.is_static, &self.strings)
    }
}

/// Compare locals such that named ones go first followed by unnamed ones.
/// Ordering for named locals is stable and is based on their source names.
/// Unnamed locals have only their id which may differ accross runs. In which
/// case the IR would be non-deterministic and hence unstable ordering would be
/// the least of our concerns.
fn cmp_lid(strings: &StringInterner, x: &LocalId, y: &LocalId) -> std::cmp::Ordering {
    match (x, y) {
        (LocalId::Named(x_bid), LocalId::Named(y_bid)) => {
            let x_name = strings.lookup_bytes(*x_bid);
            let y_name = strings.lookup_bytes(*y_bid);
            x_name.cmp(&y_name)
        }
        (LocalId::Named(_), LocalId::Unnamed(_)) => std::cmp::Ordering::Less,
        (LocalId::Unnamed(_), LocalId::Named(_)) => std::cmp::Ordering::Greater,
        (LocalId::Unnamed(x_id), LocalId::Unnamed(y_id)) => x_id.cmp(y_id),
    }
}

pub(crate) fn write_todo(fb: &mut textual::FuncBuilder<'_, '_>, msg: &str) -> Result<Sid> {
    trace!("TODO: {}", msg);
    textual_todo! {
        let target = format!("$todo.{msg}");
        fb.call(&target, ())
    }
}
