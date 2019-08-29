use std::borrow::Cow;

use rustc::ty::layout::{FloatTy, Integer, Primitive, Scalar};
use rustc_target::spec::abi::Abi;

use crate::prelude::*;

#[derive(Copy, Clone, Debug)]
enum PassMode {
    NoPass,
    ByVal(Type),
    ByValPair(Type, Type),
    ByRef,
}

#[derive(Copy, Clone, Debug)]
enum EmptySinglePair<T> {
    Empty,
    Single(T),
    Pair(T, T),
}

impl<T> EmptySinglePair<T> {
    fn into_iter(self) -> EmptySinglePairIter<T> {
        EmptySinglePairIter(self)
    }

    fn map<U>(self, mut f: impl FnMut(T) -> U) -> EmptySinglePair<U> {
        match self {
            Empty => Empty,
            Single(v) => Single(f(v)),
            Pair(a, b) => Pair(f(a), f(b)),
        }
    }
}

struct EmptySinglePairIter<T>(EmptySinglePair<T>);

impl<T> Iterator for EmptySinglePairIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        match std::mem::replace(&mut self.0, Empty) {
            Empty => None,
            Single(v) => Some(v),
            Pair(a, b) => {
                self.0 = Single(b);
                Some(a)
            }
        }
    }
}

impl<T: std::fmt::Debug> EmptySinglePair<T> {
    fn assert_single(self) -> T {
        match self {
            Single(v) => v,
            _ => panic!("Called assert_single on {:?}", self)
        }
    }

    fn assert_pair(self) -> (T, T) {
        match self {
            Pair(a, b) => (a, b),
            _ => panic!("Called assert_pair on {:?}", self)
        }
    }
}

use EmptySinglePair::*;

impl PassMode {
    fn get_param_ty(self, fx: &FunctionCx<impl Backend>) -> EmptySinglePair<Type> {
        match self {
            PassMode::NoPass => Empty,
            PassMode::ByVal(clif_type) => Single(clif_type),
            PassMode::ByValPair(a, b) => Pair(a, b),
            PassMode::ByRef => Single(fx.pointer_type),
        }
    }
}

pub fn scalar_to_clif_type(tcx: TyCtxt, scalar: Scalar) -> Type {
    match scalar.value {
        Primitive::Int(int, _sign) => match int {
            Integer::I8 => types::I8,
            Integer::I16 => types::I16,
            Integer::I32 => types::I32,
            Integer::I64 => types::I64,
            Integer::I128 => types::I128,
        },
        Primitive::Float(flt) => match flt {
            FloatTy::F32 => types::F32,
            FloatTy::F64 => types::F64,
        },
        Primitive::Pointer => pointer_ty(tcx),
    }
}

fn get_pass_mode<'tcx>(
    tcx: TyCtxt<'tcx>,
    layout: TyLayout<'tcx>,
) -> PassMode {
    assert!(!layout.is_unsized());

    if layout.is_zst() {
        // WARNING zst arguments must never be passed, as that will break CastKind::ClosureFnPointer
        PassMode::NoPass
    } else {
        match &layout.abi {
            layout::Abi::Uninhabited => PassMode::NoPass,
            layout::Abi::Scalar(scalar) => {
                PassMode::ByVal(scalar_to_clif_type(tcx, scalar.clone()))
            }
            layout::Abi::ScalarPair(a, b) => {
                let a = scalar_to_clif_type(tcx, a.clone());
                let b = scalar_to_clif_type(tcx, b.clone());
                if a == types::I128 && b == types::I128 {
                    // Returning (i128, i128) by-val-pair would take 4 regs, while only 3 are
                    // available on x86_64. Cranelift gets confused when too many return params
                    // are used.
                    PassMode::ByRef
                } else {
                    PassMode::ByValPair(a, b)
                }
            }

            // FIXME implement Vector Abi in a cg_llvm compatible way
            layout::Abi::Vector { .. } => PassMode::ByRef,

            layout::Abi::Aggregate { .. } => PassMode::ByRef,
        }
    }
}

fn adjust_arg_for_abi<'tcx>(
    fx: &mut FunctionCx<'_, 'tcx, impl Backend>,
    arg: CValue<'tcx>,
) -> EmptySinglePair<Value> {
    match get_pass_mode(fx.tcx, arg.layout()) {
        PassMode::NoPass => Empty,
        PassMode::ByVal(_) => Single(arg.load_scalar(fx)),
        PassMode::ByValPair(_, _) => {
            let (a, b) = arg.load_scalar_pair(fx);
            Pair(a, b)
        }
        PassMode::ByRef => Single(arg.force_stack(fx)),
    }
}

fn clif_sig_from_fn_sig<'tcx>(tcx: TyCtxt<'tcx>, sig: FnSig<'tcx>, is_vtable_fn: bool) -> Signature {
    let abi = match sig.abi {
        Abi::System => {
            if tcx.sess.target.target.options.is_like_windows {
                unimplemented!()
            } else {
                Abi::C
            }
        }
        abi => abi,
    };
    let (call_conv, inputs, output): (CallConv, Vec<Ty>, Ty) = match abi {
        Abi::Rust => (CallConv::SystemV, sig.inputs().to_vec(), sig.output()),
        Abi::C => (CallConv::SystemV, sig.inputs().to_vec(), sig.output()),
        Abi::RustCall => {
            assert_eq!(sig.inputs().len(), 2);
            let extra_args = match sig.inputs().last().unwrap().sty {
                ty::Tuple(ref tupled_arguments) => tupled_arguments,
                _ => bug!("argument to function with \"rust-call\" ABI is not a tuple"),
            };
            let mut inputs: Vec<Ty> = vec![sig.inputs()[0]];
            inputs.extend(extra_args.types());
            (CallConv::SystemV, inputs, sig.output())
        }
        Abi::System => unreachable!(),
        Abi::RustIntrinsic => (CallConv::SystemV, sig.inputs().to_vec(), sig.output()),
        _ => unimplemented!("unsupported abi {:?}", sig.abi),
    };

    let inputs = inputs
        .into_iter()
        .enumerate()
        .map(|(i, ty)| {
            let mut layout = tcx.layout_of(ParamEnv::reveal_all().and(ty)).unwrap();
            if i == 0 && is_vtable_fn {
                // Virtual calls turn their self param into a thin pointer.
                // See https://github.com/rust-lang/rust/blob/37b6a5e5e82497caf5353d9d856e4eb5d14cbe06/src/librustc/ty/layout.rs#L2519-L2572 for more info
                layout = tcx.layout_of(ParamEnv::reveal_all().and(tcx.mk_mut_ptr(tcx.mk_unit()))).unwrap();
            }
            match get_pass_mode(tcx, layout) {
                PassMode::NoPass => Empty,
                PassMode::ByVal(clif_ty) => Single(clif_ty),
                PassMode::ByValPair(clif_ty_a, clif_ty_b) => Pair(clif_ty_a, clif_ty_b),
                PassMode::ByRef => Single(pointer_ty(tcx)),
            }.into_iter()
        }).flatten();

    let (params, returns) = match get_pass_mode(tcx, tcx.layout_of(ParamEnv::reveal_all().and(output)).unwrap()) {
        PassMode::NoPass => (inputs.map(AbiParam::new).collect(), vec![]),
        PassMode::ByVal(ret_ty) => (
            inputs.map(AbiParam::new).collect(),
            vec![AbiParam::new(ret_ty)],
        ),
        PassMode::ByValPair(ret_ty_a, ret_ty_b) => (
            inputs.map(AbiParam::new).collect(),
            vec![AbiParam::new(ret_ty_a), AbiParam::new(ret_ty_b)],
        ),
        PassMode::ByRef => {
            (
                Some(pointer_ty(tcx)) // First param is place to put return val
                    .into_iter()
                    .chain(inputs)
                    .map(AbiParam::new)
                    .collect(),
                vec![],
            )
        }
    };

    Signature {
        params,
        returns,
        call_conv,
    }
}

pub fn get_function_name_and_sig<'tcx>(
    tcx: TyCtxt<'tcx>,
    inst: Instance<'tcx>,
    support_vararg: bool,
) -> (String, Signature) {
    assert!(!inst.substs.needs_infer() && !inst.substs.has_param_types());
    let fn_sig = tcx.normalize_erasing_late_bound_regions(ParamEnv::reveal_all(), &inst.fn_sig(tcx));
    if fn_sig.c_variadic && !support_vararg {
        unimpl!("Variadic function definitions are not yet supported");
    }
    let sig = clif_sig_from_fn_sig(tcx, fn_sig, false);
    (tcx.symbol_name(inst).as_str().to_string(), sig)
}

/// Instance must be monomorphized
pub fn import_function<'tcx>(
    tcx: TyCtxt<'tcx>,
    module: &mut Module<impl Backend>,
    inst: Instance<'tcx>,
) -> FuncId {
    let (name, sig) = get_function_name_and_sig(tcx, inst, true);
    module
        .declare_function(&name, Linkage::Import, &sig)
        .unwrap()
}

impl<'tcx, B: Backend + 'static> FunctionCx<'_, 'tcx, B> {
    /// Instance must be monomorphized
    pub fn get_function_ref(&mut self, inst: Instance<'tcx>) -> FuncRef {
        let func_id = import_function(self.tcx, self.module, inst);
        let func_ref = self
            .module
            .declare_func_in_func(func_id, &mut self.bcx.func);

        #[cfg(debug_assertions)]
        self.add_entity_comment(func_ref, format!("{:?}", inst));

        func_ref
    }

    fn lib_call(
        &mut self,
        name: &str,
        input_tys: Vec<types::Type>,
        output_tys: Vec<types::Type>,
        args: &[Value],
    ) -> &[Value] {
        let sig = Signature {
            params: input_tys.iter().cloned().map(AbiParam::new).collect(),
            returns: output_tys.iter().cloned().map(AbiParam::new).collect(),
            call_conv: CallConv::SystemV,
        };
        let func_id = self
            .module
            .declare_function(&name, Linkage::Import, &sig)
            .unwrap();
        let func_ref = self
            .module
            .declare_func_in_func(func_id, &mut self.bcx.func);
        let call_inst = self.bcx.ins().call(func_ref, args);
        #[cfg(debug_assertions)] {
            self.add_comment(call_inst, format!("easy_call {}", name));
        }
        let results = self.bcx.inst_results(call_inst);
        assert!(results.len() <= 2, "{}", results.len());
        results
    }

    pub fn easy_call(
        &mut self,
        name: &str,
        args: &[CValue<'tcx>],
        return_ty: Ty<'tcx>,
    ) -> CValue<'tcx> {
        let (input_tys, args): (Vec<_>, Vec<_>) = args
            .into_iter()
            .map(|arg| {
                (
                    self.clif_type(arg.layout().ty).unwrap(),
                    arg.load_scalar(self),
                )
            })
            .unzip();
        let return_layout = self.layout_of(return_ty);
        let return_tys = if let ty::Tuple(tup) = return_ty.sty {
            tup.types().map(|ty| self.clif_type(ty).unwrap()).collect()
        } else {
            vec![self.clif_type(return_ty).unwrap()]
        };
        let ret_vals = self.lib_call(name, input_tys, return_tys, &args);
        match *ret_vals {
            [] => CValue::by_ref(
                self.bcx
                    .ins()
                    .iconst(self.pointer_type, self.pointer_type.bytes() as i64),
                return_layout,
            ),
            [val] => CValue::by_val(val, return_layout),
            [val, extra] => CValue::by_val_pair(val, extra, return_layout),
            _ => unreachable!(),
        }
    }

    fn self_sig(&self) -> FnSig<'tcx> {
        self.tcx.normalize_erasing_late_bound_regions(ParamEnv::reveal_all(), &self.instance.fn_sig(self.tcx))
    }

    fn return_layout(&self) -> TyLayout<'tcx> {
        self.layout_of(self.self_sig().output())
    }
}

#[cfg(debug_assertions)]
fn add_arg_comment<'tcx>(
    fx: &mut FunctionCx<'_, 'tcx, impl Backend>,
    msg: &str,
    local: mir::Local,
    local_field: Option<usize>,
    params: EmptySinglePair<Value>,
    pass_mode: PassMode,
    ssa: crate::analyze::Flags,
    ty: Ty<'tcx>,
) {
    let local_field = if let Some(local_field) = local_field {
        Cow::Owned(format!(".{}", local_field))
    } else {
        Cow::Borrowed("")
    };
    let params = match params {
        Empty => Cow::Borrowed("-"),
        Single(param) => Cow::Owned(format!("= {:?}", param)),
        Pair(param_a, param_b) => Cow::Owned(format!("= {:?}, {:?}", param_a, param_b)),
    };
    let pass_mode = format!("{:?}", pass_mode);
    fx.add_global_comment(format!(
        "{msg:5} {local:>3}{local_field:<5} {params:10} {pass_mode:36} {ssa:10} {ty:?}",
        msg = msg,
        local = format!("{:?}", local),
        local_field = local_field,
        params = params,
        pass_mode = pass_mode,
        ssa = format!("{:?}", ssa),
        ty = ty,
    ));
}

#[cfg(debug_assertions)]
fn add_local_header_comment(fx: &mut FunctionCx<impl Backend>) {
    fx.add_global_comment(format!(
        "msg   loc.idx    param    pass mode                            ssa flags  ty"
    ));
}

fn local_place<'tcx>(
    fx: &mut FunctionCx<'_, 'tcx, impl Backend>,
    local: Local,
    layout: TyLayout<'tcx>,
    is_ssa: bool,
) -> CPlace<'tcx> {
    let place = if is_ssa {
        CPlace::new_var(fx, local, layout)
    } else {
        let place = CPlace::new_stack_slot(fx, layout.ty);

        #[cfg(debug_assertions)]
        {
            let TyLayout { ty, details } = layout;
            let ty::layout::LayoutDetails {
                size,
                align,
                abi: _,
                variants: _,
                fields: _,
                largest_niche: _,
            } = details;
            match *place.inner() {
                CPlaceInner::Stack(stack_slot) => fx.add_entity_comment(
                    stack_slot,
                    format!(
                        "{:?}: {:?} size={} align={},{}",
                        local,
                        ty,
                        size.bytes(),
                        align.abi.bytes(),
                        align.pref.bytes(),
                    ),
                ),
                CPlaceInner::NoPlace => fx.add_global_comment(format!(
                    "zst    {:?}: {:?} size={} align={}, {}",
                    local,
                    ty,
                    size.bytes(),
                    align.abi.bytes(),
                    align.pref.bytes(),
                )),
                _ => unreachable!(),
            }
        }

        place
    };

    let prev_place = fx.local_map.insert(local, place);
    debug_assert!(prev_place.is_none());
    fx.local_map[&local]
}

fn cvalue_for_param<'tcx>(
    fx: &mut FunctionCx<'_, 'tcx, impl Backend>,
    start_ebb: Ebb,
    local: mir::Local,
    local_field: Option<usize>,
    arg_ty: Ty<'tcx>,
    ssa_flags: crate::analyze::Flags,
) -> Option<CValue<'tcx>> {
    let layout = fx.layout_of(arg_ty);
    let pass_mode = get_pass_mode(fx.tcx, fx.layout_of(arg_ty));

    if let PassMode::NoPass = pass_mode {
        return None;
    }

    let clif_types = pass_mode.get_param_ty(fx);
    let ebb_params = clif_types.map(|t| fx.bcx.append_ebb_param(start_ebb, t));

    #[cfg(debug_assertions)]
    add_arg_comment(
        fx,
        "arg",
        local,
        local_field,
        ebb_params,
        pass_mode,
        ssa_flags,
        arg_ty,
    );

    match pass_mode {
        PassMode::NoPass => unreachable!(),
        PassMode::ByVal(_) => Some(CValue::by_val(ebb_params.assert_single(), layout)),
        PassMode::ByValPair(_, _) => {
            let (a, b) = ebb_params.assert_pair();
            Some(CValue::by_val_pair(a, b, layout))
        }
        PassMode::ByRef => Some(CValue::by_ref(ebb_params.assert_single(), layout)),
    }
}

pub fn codegen_fn_prelude(
    fx: &mut FunctionCx<'_, '_, impl Backend>,
    start_ebb: Ebb,
) {
    let ssa_analyzed = crate::analyze::analyze(fx);

    #[cfg(debug_assertions)]
    fx.add_global_comment(format!("ssa {:?}", ssa_analyzed));

    let ret_layout = fx.return_layout();
    let output_pass_mode = get_pass_mode(fx.tcx, fx.return_layout());
    let ret_param = match output_pass_mode {
        PassMode::NoPass | PassMode::ByVal(_) | PassMode::ByValPair(_, _) => None,
        PassMode::ByRef => Some(fx.bcx.append_ebb_param(start_ebb, fx.pointer_type)),
    };

    #[cfg(debug_assertions)]
    {
        add_local_header_comment(fx);
        let ret_param = match ret_param {
            Some(param) => Single(param),
            None => Empty,
        };
        add_arg_comment(
            fx,
            "ret",
            RETURN_PLACE,
            None,
            ret_param,
            output_pass_mode,
            ssa_analyzed[&RETURN_PLACE],
            ret_layout.ty,
        );
    }

    // None means pass_mode == NoPass
    enum ArgKind<'tcx> {
        Normal(Option<CValue<'tcx>>),
        Spread(Vec<Option<CValue<'tcx>>>),
    }

    let func_params = fx
        .mir
        .args_iter()
        .map(|local| {
            let arg_ty = fx.monomorphize(&fx.mir.local_decls[local].ty);

            // Adapted from https://github.com/rust-lang/rust/blob/145155dc96757002c7b2e9de8489416e2fdbbd57/src/librustc_codegen_llvm/mir/mod.rs#L442-L482
            if Some(local) == fx.mir.spread_arg {
                // This argument (e.g. the last argument in the "rust-call" ABI)
                // is a tuple that was spread at the ABI level and now we have
                // to reconstruct it into a tuple local variable, from multiple
                // individual function arguments.

                let tupled_arg_tys = match arg_ty.sty {
                    ty::Tuple(ref tys) => tys,
                    _ => bug!("spread argument isn't a tuple?! but {:?}", arg_ty),
                };

                let mut params = Vec::new();
                for (i, arg_ty) in tupled_arg_tys.types().enumerate() {
                    let param = cvalue_for_param(
                        fx,
                        start_ebb,
                        local,
                        Some(i),
                        arg_ty,
                        ssa_analyzed[&local],
                    );
                    params.push(param);
                }

                (local, ArgKind::Spread(params), arg_ty)
            } else {
                let param =
                    cvalue_for_param(fx, start_ebb, local, None, arg_ty, ssa_analyzed[&local]);
                (local, ArgKind::Normal(param), arg_ty)
            }
        })
        .collect::<Vec<(Local, ArgKind, Ty)>>();

    fx.bcx.switch_to_block(start_ebb);

    match output_pass_mode {
        PassMode::NoPass => {
            fx.local_map
                .insert(RETURN_PLACE, CPlace::no_place(ret_layout));
        }
        PassMode::ByVal(_) | PassMode::ByValPair(_, _) => {
            let is_ssa = !ssa_analyzed
                .get(&RETURN_PLACE)
                .unwrap()
                .contains(crate::analyze::Flags::NOT_SSA);

            local_place(fx, RETURN_PLACE, ret_layout, is_ssa);
        }
        PassMode::ByRef => {
            fx.local_map.insert(
                RETURN_PLACE,
                CPlace::for_addr(ret_param.unwrap(), ret_layout),
            );
        }
    }

    for (local, arg_kind, ty) in func_params {
        let layout = fx.layout_of(ty);

        let is_ssa = !ssa_analyzed
            .get(&local)
            .unwrap()
            .contains(crate::analyze::Flags::NOT_SSA);

        let place = local_place(fx, local, layout, is_ssa);

        match arg_kind {
            ArgKind::Normal(param) => {
                if let Some(param) = param {
                    place.write_cvalue(fx, param);
                }
            }
            ArgKind::Spread(params) => {
                for (i, param) in params.into_iter().enumerate() {
                    if let Some(param) = param {
                        place
                            .place_field(fx, mir::Field::new(i))
                            .write_cvalue(fx, param);
                    }
                }
            }
        }
    }

    for local in fx.mir.vars_and_temps_iter() {
        let ty = fx.mir.local_decls[local].ty;
        let layout = fx.layout_of(ty);

        let is_ssa = !ssa_analyzed
            .get(&local)
            .unwrap()
            .contains(crate::analyze::Flags::NOT_SSA);

        local_place(fx, local, layout, is_ssa);
    }

    fx.bcx
        .ins()
        .jump(*fx.ebb_map.get(&START_BLOCK).unwrap(), &[]);
}

pub fn codegen_terminator_call<'tcx>(
    fx: &mut FunctionCx<'_, 'tcx, impl Backend>,
    func: &Operand<'tcx>,
    args: &[Operand<'tcx>],
    destination: &Option<(Place<'tcx>, BasicBlock)>,
) {
    let fn_ty = fx.monomorphize(&func.ty(fx.mir, fx.tcx));
    let sig = fx.tcx.normalize_erasing_late_bound_regions(ParamEnv::reveal_all(), &fn_ty.fn_sig(fx.tcx));

    let destination = destination
        .as_ref()
        .map(|&(ref place, bb)| (trans_place(fx, place), bb));

    if let ty::FnDef(def_id, substs) = fn_ty.sty {
        let instance =
            ty::Instance::resolve(fx.tcx, ty::ParamEnv::reveal_all(), def_id, substs).unwrap();

        if fx.tcx.symbol_name(instance).as_str().starts_with("llvm.") {
            crate::llvm_intrinsics::codegen_llvm_intrinsic_call(fx, &fx.tcx.symbol_name(instance).as_str(), substs, args, destination);
            return;
        }

        match instance.def {
            InstanceDef::Intrinsic(_) => {
                crate::intrinsics::codegen_intrinsic_call(fx, def_id, substs, args, destination);
                return;
            }
            InstanceDef::DropGlue(_, None) => {
                // empty drop glue - a nop.
                let (_, dest) = destination.expect("Non terminating drop_in_place_real???");
                let ret_ebb = fx.get_ebb(dest);
                fx.bcx.ins().jump(ret_ebb, &[]);
                return;
            }
            _ => {}
        }
    }

    // Unpack arguments tuple for closures
    let args = if sig.abi == Abi::RustCall {
        assert_eq!(args.len(), 2, "rust-call abi requires two arguments");
        let self_arg = trans_operand(fx, &args[0]);
        let pack_arg = trans_operand(fx, &args[1]);
        let mut args = Vec::new();
        args.push(self_arg);
        match pack_arg.layout().ty.sty {
            ty::Tuple(ref tupled_arguments) => {
                for (i, _) in tupled_arguments.iter().enumerate() {
                    args.push(pack_arg.value_field(fx, mir::Field::new(i)));
                }
            }
            _ => bug!("argument to function with \"rust-call\" ABI is not a tuple"),
        }
        args
    } else {
        args.into_iter()
            .map(|arg| trans_operand(fx, arg))
            .collect::<Vec<_>>()
    };

    codegen_call_inner(
        fx,
        Some(func),
        fn_ty,
        args,
        destination.map(|(place, _)| place),
    );

    if let Some((_, dest)) = destination {
        let ret_ebb = fx.get_ebb(dest);
        fx.bcx.ins().jump(ret_ebb, &[]);
    } else {
        trap_unreachable(fx, "[corruption] Diverging function returned");
    }
}

fn codegen_call_inner<'tcx>(
    fx: &mut FunctionCx<'_, 'tcx, impl Backend>,
    func: Option<&Operand<'tcx>>,
    fn_ty: Ty<'tcx>,
    args: Vec<CValue<'tcx>>,
    ret_place: Option<CPlace<'tcx>>,
) {
    let fn_sig = fx.tcx.normalize_erasing_late_bound_regions(ParamEnv::reveal_all(), &fn_ty.fn_sig(fx.tcx));

    let ret_layout = fx.layout_of(fn_sig.output());

    let output_pass_mode = get_pass_mode(fx.tcx, fx.layout_of(fn_sig.output()));
    let return_ptr = match output_pass_mode {
        PassMode::NoPass => None,
        PassMode::ByRef => match ret_place {
            Some(ret_place) => Some(ret_place.to_addr(fx)),
            None => Some(fx.bcx.ins().iconst(fx.pointer_type, 43)),
        },
        PassMode::ByVal(_) | PassMode::ByValPair(_, _) => None,
    };

    let instance = match fn_ty.sty {
        ty::FnDef(def_id, substs) => {
            Some(Instance::resolve(fx.tcx, ParamEnv::reveal_all(), def_id, substs).unwrap())
        }
        _ => None,
    };

    //   | indirect call target
    //   |         | the first argument to be passed
    //   v         v          v virtual calls are special cased below
    let (func_ref, first_arg, is_virtual_call) = match instance {
        // Trait object call
        Some(Instance {
            def: InstanceDef::Virtual(_, idx),
            ..
        }) => {
            #[cfg(debug_assertions)]
            {
                let nop_inst = fx.bcx.ins().nop();
                fx.add_comment(
                    nop_inst,
                    format!("virtual call; self arg pass mode: {:?}", get_pass_mode(fx.tcx, args[0].layout())),
                );
            }
            let (ptr, method) = crate::vtable::get_ptr_and_method_ref(fx, args[0], idx);
            (Some(method), Single(ptr), true)
        }

        // Normal call
        Some(_) => (None, args.get(0).map(|arg| adjust_arg_for_abi(fx, *arg)).unwrap_or(Empty), false),

        // Indirect call
        None => {
            #[cfg(debug_assertions)]
            {
                let nop_inst = fx.bcx.ins().nop();
                fx.add_comment(nop_inst, "indirect call");
            }
            let func = trans_operand(fx, func.expect("indirect call without func Operand"))
                .load_scalar(fx);
            (
                Some(func),
                args.get(0).map(|arg| adjust_arg_for_abi(fx, *arg)).unwrap_or(Empty),
                false,
            )
        }
    };

    let call_args: Vec<Value> = return_ptr
        .into_iter()
        .chain(first_arg.into_iter())
        .chain(
            args.into_iter()
                .skip(1)
                .map(|arg| adjust_arg_for_abi(fx, arg).into_iter())
                .flatten(),
        )
        .collect::<Vec<_>>();

    let call_inst = if let Some(func_ref) = func_ref {
        let sig = fx
            .bcx
            .import_signature(clif_sig_from_fn_sig(fx.tcx, fn_sig, is_virtual_call));
        fx.bcx.ins().call_indirect(sig, func_ref, &call_args)
    } else {
        let func_ref = fx.get_function_ref(instance.expect("non-indirect call on non-FnDef type"));
        fx.bcx.ins().call(func_ref, &call_args)
    };

    // FIXME find a cleaner way to support varargs
    if fn_sig.c_variadic {
        if fn_sig.abi != Abi::C {
            unimpl!("Variadic call for non-C abi {:?}", fn_sig.abi);
        }
        let sig_ref = fx.bcx.func.dfg.call_signature(call_inst).unwrap();
        let abi_params = call_args
            .into_iter()
            .map(|arg| {
                let ty = fx.bcx.func.dfg.value_type(arg);
                if !ty.is_int() {
                    // FIXME set %al to upperbound on float args once floats are supported
                    unimpl!("Non int ty {:?} for variadic call", ty);
                }
                AbiParam::new(ty)
            })
            .collect::<Vec<AbiParam>>();
        fx.bcx.func.dfg.signatures[sig_ref].params = abi_params;
    }

    match output_pass_mode {
        PassMode::NoPass => {}
        PassMode::ByVal(_) => {
            if let Some(ret_place) = ret_place {
                let ret_val = fx.bcx.inst_results(call_inst)[0];
                ret_place.write_cvalue(fx, CValue::by_val(ret_val, ret_layout));
            }
        }
        PassMode::ByValPair(_, _) => {
            if let Some(ret_place) = ret_place {
                let ret_val_a = fx.bcx.inst_results(call_inst)[0];
                let ret_val_b = fx.bcx.inst_results(call_inst)[1];
                ret_place.write_cvalue(fx, CValue::by_val_pair(ret_val_a, ret_val_b, ret_layout));
            }
        }
        PassMode::ByRef => {}
    }
}

pub fn codegen_drop<'tcx>(
    fx: &mut FunctionCx<'_, 'tcx, impl Backend>,
    drop_place: CPlace<'tcx>,
) {
    let ty = drop_place.layout().ty;
    let drop_fn = Instance::resolve_drop_in_place(fx.tcx, ty);

    if let ty::InstanceDef::DropGlue(_, None) = drop_fn.def {
        // we don't actually need to drop anything
    } else {
        let drop_fn_ty = drop_fn.ty(fx.tcx);
        match ty.sty {
            ty::Dynamic(..) => {
                let (ptr, vtable) = drop_place.to_addr_maybe_unsized(fx);
                let drop_fn = crate::vtable::drop_fn_of_obj(fx, vtable.unwrap());

                let fn_sig = fx.tcx.normalize_erasing_late_bound_regions(ParamEnv::reveal_all(), &drop_fn_ty.fn_sig(fx.tcx));

                assert_eq!(fn_sig.output(), fx.tcx.mk_unit());

                let sig = fx
                    .bcx
                    .import_signature(clif_sig_from_fn_sig(fx.tcx, fn_sig, true));
                fx.bcx.ins().call_indirect(sig, drop_fn, &[ptr]);
            }
            _ => {
                let arg_place = CPlace::new_stack_slot(
                    fx,
                    fx.tcx.mk_ref(
                        &ty::RegionKind::ReErased,
                        TypeAndMut {
                            ty,
                            mutbl: crate::rustc::hir::Mutability::MutMutable,
                        },
                    ),
                );
                drop_place.write_place_ref(fx, arg_place);
                let arg_value = arg_place.to_cvalue(fx);
                codegen_call_inner(
                    fx,
                    None,
                    drop_fn_ty,
                    vec![arg_value],
                    None,
                );
            }
        }
    }
}

pub fn codegen_return(fx: &mut FunctionCx<impl Backend>) {
    match get_pass_mode(fx.tcx, fx.return_layout()) {
        PassMode::NoPass | PassMode::ByRef => {
            fx.bcx.ins().return_(&[]);
        }
        PassMode::ByVal(_) => {
            let place = fx.get_local_place(RETURN_PLACE);
            let ret_val = place.to_cvalue(fx).load_scalar(fx);
            fx.bcx.ins().return_(&[ret_val]);
        }
        PassMode::ByValPair(_, _) => {
            let place = fx.get_local_place(RETURN_PLACE);
            let (ret_val_a, ret_val_b) = place.to_cvalue(fx).load_scalar_pair(fx);
            fx.bcx.ins().return_(&[ret_val_a, ret_val_b]);
        }
    }
}
