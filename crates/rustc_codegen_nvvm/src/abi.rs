use crate::builder::Builder;
use crate::context::CodegenCx;
use crate::int_replace::{get_transformed_type, transmute_llval};
use crate::llvm::{self, *};
use crate::ty::LayoutLlvmExt;
use libc::c_uint;
use rustc_codegen_ssa::mir::operand::OperandValue;
use rustc_codegen_ssa::mir::place::PlaceRef;
use rustc_codegen_ssa::traits::BaseTypeMethods;
use rustc_codegen_ssa::{traits::*, MemFlags};
use rustc_middle::bug;
use rustc_middle::ty::layout::LayoutOf;
pub use rustc_middle::ty::layout::{FAT_PTR_ADDR, FAT_PTR_EXTRA};
use rustc_middle::ty::{Ty, TyCtxt, TyKind};
pub use rustc_target::abi::call::*;
use rustc_target::abi::call::{CastTarget, Reg, RegKind};
use rustc_target::abi::{self, HasDataLayout, Int};
pub use rustc_target::spec::abi::Abi;
use tracing::trace;

pub(crate) fn readjust_fn_abi<'tcx>(
    tcx: TyCtxt<'tcx>,
    fn_abi: &'tcx FnAbi<'tcx, Ty<'tcx>>,
) -> &'tcx FnAbi<'tcx, Ty<'tcx>> {
    // dont override anything in the rust abi for now
    if fn_abi.conv == Conv::Rust {
        return fn_abi;
    }
    let readjust_arg_abi = |arg: &ArgAbi<'tcx, Ty<'tcx>>| {
        let mut arg = ArgAbi {
            layout: arg.layout,
            mode: arg.mode,
            pad: arg.pad,
        };

        // ignore zsts
        if arg.layout.is_zst() {
            arg.mode = PassMode::Ignore;
        }

        if let TyKind::Ref(_, ty, _) = arg.layout.ty.kind() {
            if matches!(ty.kind(), TyKind::Slice(_)) {
                let mut ptr_attrs = ArgAttributes::new();
                if let PassMode::Indirect { attrs, .. } = arg.mode {
                    ptr_attrs.regular = attrs.regular;
                }
                arg.mode = PassMode::Pair(ptr_attrs, ArgAttributes::new());
            }
        }

        if arg.layout.ty.is_array() && !matches!(arg.mode, PassMode::Direct { .. }) {
            arg.mode = PassMode::Direct(ArgAttributes::new());
        }

        // pass all aggregates directly as values, ptx wants them to be passed all by value, but rustc's
        // ptx-kernel abi seems to be wrong, and it's unstable.
        if matches!(arg.layout.abi, abi::Abi::Aggregate { .. })
            && matches!(arg.mode, PassMode::Indirect { .. })
        {
            arg.mode = PassMode::Direct(ArgAttributes::new());
        }
        arg
    };
    tcx.arena.alloc(FnAbi {
        args: fn_abi.args.iter().map(readjust_arg_abi).collect(),
        ret: readjust_arg_abi(&fn_abi.ret),
        c_variadic: fn_abi.c_variadic,
        fixed_count: fn_abi.fixed_count,
        conv: fn_abi.conv,
        can_unwind: fn_abi.can_unwind,
    })
}

macro_rules! for_each_kind {
    ($flags: ident, $f: ident, $($kind: ident),+) => ({
        $(if $flags.contains(ArgAttribute::$kind) { $f(llvm::Attribute::$kind) })+
    })
}

trait ArgAttributeExt {
    fn for_each_kind<F>(&self, f: F)
    where
        F: FnMut(llvm::Attribute);
}

impl ArgAttributeExt for ArgAttribute {
    fn for_each_kind<F>(&self, mut f: F)
    where
        F: FnMut(llvm::Attribute),
    {
        for_each_kind!(self, f, NoAlias, NoCapture, NonNull, ReadOnly, InReg)
    }
}

pub(crate) trait ArgAttributesExt {
    fn apply_attrs_to_llfn(&self, idx: AttributePlace, cx: &CodegenCx<'_, '_>, llfn: &Value);
    fn apply_attrs_to_callsite(
        &self,
        idx: AttributePlace,
        cx: &CodegenCx<'_, '_>,
        callsite: &Value,
    );
}

impl ArgAttributesExt for ArgAttributes {
    fn apply_attrs_to_llfn(&self, idx: AttributePlace, _cx: &CodegenCx<'_, '_>, llfn: &Value) {
        let mut regular = self.regular;
        unsafe {
            let deref = self.pointee_size.bytes();
            if deref != 0 {
                if regular.contains(ArgAttribute::NonNull) {
                    llvm::LLVMRustAddDereferenceableAttr(llfn, idx.as_uint(), deref);
                } else {
                    llvm::LLVMRustAddDereferenceableOrNullAttr(llfn, idx.as_uint(), deref);
                }
                regular -= ArgAttribute::NonNull;
            }
            if let Some(align) = self.pointee_align {
                llvm::LLVMRustAddAlignmentAttr(llfn, idx.as_uint(), align.bytes() as u32);
            }
            regular.for_each_kind(|attr| attr.apply_llfn(idx, llfn));
            // TODO(RDambrosio016): Apply mutable noalias once we upgrade to LLVM 13
            match self.arg_ext {
                ArgExtension::None => {}
                ArgExtension::Zext => {
                    llvm::Attribute::ZExt.apply_llfn(idx, llfn);
                }
                ArgExtension::Sext => {
                    llvm::Attribute::SExt.apply_llfn(idx, llfn);
                }
            }
        }
    }

    fn apply_attrs_to_callsite(
        &self,
        idx: AttributePlace,
        _cx: &CodegenCx<'_, '_>,
        callsite: &Value,
    ) {
        let mut regular = self.regular;
        unsafe {
            let deref = self.pointee_size.bytes();
            if deref != 0 {
                if regular.contains(ArgAttribute::NonNull) {
                    llvm::LLVMRustAddDereferenceableCallSiteAttr(callsite, idx.as_uint(), deref);
                } else {
                    llvm::LLVMRustAddDereferenceableOrNullCallSiteAttr(
                        callsite,
                        idx.as_uint(),
                        deref,
                    );
                }
                regular -= ArgAttribute::NonNull;
            }
            if let Some(align) = self.pointee_align {
                llvm::LLVMRustAddAlignmentCallSiteAttr(
                    callsite,
                    idx.as_uint(),
                    align.bytes() as u32,
                );
            }
            regular.for_each_kind(|attr| attr.apply_callsite(idx, callsite));
            match self.arg_ext {
                ArgExtension::None => {}
                ArgExtension::Zext => {
                    llvm::Attribute::ZExt.apply_callsite(idx, callsite);
                }
                ArgExtension::Sext => {
                    llvm::Attribute::SExt.apply_callsite(idx, callsite);
                }
            }
        }
    }
}

pub(crate) trait LlvmType {
    fn llvm_type<'ll>(&self, cx: &CodegenCx<'ll, '_>) -> &'ll Type;
}

impl LlvmType for Reg {
    fn llvm_type<'ll>(&self, cx: &CodegenCx<'ll, '_>) -> &'ll Type {
        match self.kind {
            RegKind::Integer => cx.type_ix(self.size.bits()),
            RegKind::Float => match self.size.bits() {
                32 => cx.type_f32(),
                64 => cx.type_f64(),
                _ => bug!("unsupported float: {:?}", self),
            },
            RegKind::Vector => cx.type_vector(cx.type_i8(), self.size.bytes()),
        }
    }
}

impl LlvmType for CastTarget {
    fn llvm_type<'ll>(&self, cx: &CodegenCx<'ll, '_>) -> &'ll Type {
        let rest_ll_unit = self.rest.unit.llvm_type(cx);
        let (rest_count, rem_bytes) = if self.rest.unit.size.bytes() == 0 {
            (0, 0)
        } else {
            (
                self.rest.total.bytes() / self.rest.unit.size.bytes(),
                self.rest.total.bytes() % self.rest.unit.size.bytes(),
            )
        };

        if self.prefix.iter().all(|x| x.is_none()) {
            // Simplify to a single unit when there is no prefix and size <= unit size
            if self.rest.total <= self.rest.unit.size {
                return rest_ll_unit;
            }

            // Simplify to array when all chunks are the same size and type
            if rem_bytes == 0 {
                return cx.type_array(rest_ll_unit, rest_count);
            }
        }

        // Create list of fields in the main structure
        let mut args: Vec<_> = self
            .prefix
            .iter()
            .flat_map(|option_kind| {
                option_kind.map(|kind| {
                    Reg {
                        kind,
                        size: self.prefix_chunk_size,
                    }
                    .llvm_type(cx)
                })
            })
            .chain((0..rest_count).map(|_| rest_ll_unit))
            .collect();

        // Append final integer
        if rem_bytes != 0 {
            // Only integers can be really split further.
            assert_eq!(self.rest.unit.kind, RegKind::Integer);
            args.push(cx.type_ix(rem_bytes * 8));
        }

        cx.type_struct(&args, false)
    }
}

impl<'a, 'll, 'tcx> ArgAbiMethods<'tcx> for Builder<'a, 'll, 'tcx> {
    fn store_fn_arg(
        &mut self,
        arg_abi: &ArgAbi<'tcx, Ty<'tcx>>,
        idx: &mut usize,
        dst: PlaceRef<'tcx, Self::Value>,
    ) {
        arg_abi.store_fn_arg(self, idx, dst)
    }
    fn store_arg(
        &mut self,
        arg_abi: &ArgAbi<'tcx, Ty<'tcx>>,
        val: &'ll Value,
        dst: PlaceRef<'tcx, &'ll Value>,
    ) {
        arg_abi.store(self, val, dst)
    }
    fn arg_memory_ty(&self, arg_abi: &ArgAbi<'tcx, Ty<'tcx>>) -> &'ll Type {
        arg_abi.memory_ty(self)
    }
}

pub(crate) trait FnAbiLlvmExt<'ll, 'tcx> {
    fn llvm_type(&self, cx: &CodegenCx<'ll, 'tcx>) -> &'ll Type;
    fn ptr_to_llvm_type(&self, cx: &CodegenCx<'ll, 'tcx>) -> &'ll Type;
    fn apply_attrs_llfn(&self, cx: &CodegenCx<'ll, 'tcx>, llfn: &'ll Value);
    fn apply_attrs_callsite<'a>(&self, bx: &mut Builder<'a, 'll, 'tcx>, callsite: &'ll Value);
}

impl<'ll, 'tcx> FnAbiLlvmExt<'ll, 'tcx> for FnAbi<'tcx, Ty<'tcx>> {
    fn llvm_type(&self, cx: &CodegenCx<'ll, 'tcx>) -> &'ll Type {
        let args_capacity: usize = self.args.iter().map(|arg|
            if arg.pad.is_some() { 1 } else { 0 } +
            if let PassMode::Pair(_, _) = arg.mode { 2 } else { 1 }
        ).sum();

        // the current index of each parameter in the function. Cant use enumerate on args because
        // some pass modes pass args as multiple params, such as scalar pairs.
        let mut idx = 0;

        let mut llargument_tys = Vec::with_capacity(
            if let PassMode::Indirect { .. } = self.ret.mode {
                1
            } else {
                0
            } + args_capacity,
        );

        let mut llreturn_ty = match self.ret.mode {
            PassMode::Ignore => cx.type_void(),
            PassMode::Direct(_) | PassMode::Pair(..) => self.ret.layout.immediate_llvm_type(cx),
            PassMode::Cast(cast) => cast.llvm_type(cx),
            PassMode::Indirect { .. } => {
                idx += 1;
                llargument_tys.push(cx.type_ptr_to(self.ret.memory_ty(cx)));
                cx.type_void()
            }
        };

        let mut transformed_types = Vec::new();
        let mut old_ret_ty = Some(llreturn_ty);

        let (new_ret, changed) = get_transformed_type(cx, llreturn_ty);
        llreturn_ty = new_ret;
        if !changed {
            old_ret_ty = None;
        }

        for arg in self.args.iter() {
            // add padding
            if let Some(ty) = arg.pad {
                idx += 1;
                llargument_tys.push(ty.llvm_type(cx));
            }

            let llarg_ty = match arg.mode {
                PassMode::Ignore => continue,
                PassMode::Direct(_) => arg.layout.immediate_llvm_type(cx),
                PassMode::Pair(..) => {
                    llargument_tys.push(arg.layout.scalar_pair_element_llvm_type(cx, 0, true));
                    llargument_tys.push(arg.layout.scalar_pair_element_llvm_type(cx, 1, true));
                    idx += 2;
                    continue;
                }
                PassMode::Indirect {
                    attrs: _,
                    extra_attrs: Some(_),
                    on_stack: _,
                } => {
                    let ptr_ty = cx.tcx.mk_mut_ptr(arg.layout.ty);
                    let ptr_layout = cx.layout_of(ptr_ty);
                    llargument_tys.push(ptr_layout.scalar_pair_element_llvm_type(cx, 0, true));
                    llargument_tys.push(ptr_layout.scalar_pair_element_llvm_type(cx, 1, true));
                    idx += 2;
                    continue;
                }
                PassMode::Cast(cast) => cast.llvm_type(cx),
                PassMode::Indirect {
                    attrs: _,
                    extra_attrs: None,
                    on_stack: _,
                } => cx.type_ptr_to(arg.memory_ty(cx)),
            };
            let (new, changed) = get_transformed_type(cx, llarg_ty);
            if changed {
                transformed_types.push((idx, llarg_ty));
            }
            llargument_tys.push(new);
            idx += 1;
        }

        let ty = if self.c_variadic {
            cx.type_variadic_func(&llargument_tys, llreturn_ty)
        } else {
            cx.type_func(&llargument_tys, llreturn_ty)
        };
        if !transformed_types.is_empty() || old_ret_ty.is_some() {
            cx.remapped_integer_args
                .borrow_mut()
                .insert(ty, (old_ret_ty, transformed_types));
        }
        ty
    }

    fn ptr_to_llvm_type(&self, cx: &CodegenCx<'ll, 'tcx>) -> &'ll Type {
        unsafe {
            llvm::LLVMPointerType(
                self.llvm_type(cx),
                cx.data_layout().instruction_address_space.0 as c_uint,
            )
        }
    }

    fn apply_attrs_llfn(&self, cx: &CodegenCx<'ll, 'tcx>, llfn: &'ll Value) {
        if self.ret.layout.abi.is_uninhabited() {
            llvm::Attribute::NoReturn.apply_llfn(llvm::AttributePlace::Function, llfn);
        }

        // TODO(RDambrosio016): should this always/never be applied? unwinding
        // on the gpu doesnt exist.
        if !self.can_unwind {
            llvm::Attribute::NoUnwind.apply_llfn(llvm::AttributePlace::Function, llfn);
        }

        let mut i = 0;
        let mut apply = |attrs: &ArgAttributes| {
            attrs.apply_attrs_to_llfn(llvm::AttributePlace::Argument(i), cx, llfn);
            i += 1;
            i - 1
        };
        match self.ret.mode {
            PassMode::Direct(ref attrs) => {
                attrs.apply_attrs_to_llfn(llvm::AttributePlace::ReturnValue, cx, llfn);
            }
            PassMode::Indirect {
                ref attrs,
                extra_attrs: _,
                on_stack,
            } => {
                assert!(!on_stack);
                let i = apply(attrs);
                llvm::Attribute::StructRet.apply_llfn(llvm::AttributePlace::Argument(i), llfn);
            }
            _ => {}
        }
        for arg in &self.args {
            if arg.pad.is_some() {
                apply(&ArgAttributes::new());
            }
            match arg.mode {
                PassMode::Ignore => {}
                PassMode::Indirect {
                    ref attrs,
                    extra_attrs: None,
                    on_stack: true,
                } => {
                    apply(attrs);
                    // TODO(RDambrosio016): we should technically apply byval here,
                    // llvm 7 seems to have it, but i could not find a way to apply it through the
                    // C++ API, so somebody more experienced in the C++ API should look at this.
                    // it shouldnt do anything bad since it seems to be only for optimization.
                }
                PassMode::Direct(ref attrs)
                | PassMode::Indirect {
                    ref attrs,
                    extra_attrs: None,
                    on_stack: false,
                } => {
                    apply(attrs);
                }
                PassMode::Indirect {
                    ref attrs,
                    extra_attrs: Some(ref extra_attrs),
                    on_stack,
                } => {
                    assert!(!on_stack);
                    apply(attrs);
                    apply(extra_attrs);
                }
                PassMode::Pair(ref a, ref b) => {
                    apply(a);
                    apply(b);
                }
                PassMode::Cast(_) => {
                    apply(&ArgAttributes::new());
                }
            }
        }
    }

    fn apply_attrs_callsite<'a>(&self, bx: &mut Builder<'a, 'll, 'tcx>, mut callsite: &'ll Value) {
        // HACK(RDambrosio016): We sometimes lie to rustc with return values and give it a bitcast
        // instead of a call. This is because we sometimes have to bitcast return types like <2 x i64> to i128.
        // So we just check if the last call was remapped.
        if let Some(old) = bx.cx.last_call_llfn.get() {
            callsite = old;
            bx.cx.last_call_llfn.set(None);
        }

        let mut i = 0;
        let mut apply = |cx: &CodegenCx<'_, '_>, attrs: &ArgAttributes| {
            attrs.apply_attrs_to_callsite(llvm::AttributePlace::Argument(i), cx, callsite);
            i += 1;
            i - 1
        };
        match self.ret.mode {
            PassMode::Direct(ref attrs) => {
                attrs.apply_attrs_to_callsite(llvm::AttributePlace::ReturnValue, bx.cx, callsite);
            }
            PassMode::Indirect {
                ref attrs,
                extra_attrs: _,
                on_stack,
            } => {
                assert!(!on_stack);
                apply(bx.cx, attrs);
            }
            _ => {}
        }
        if let abi::Abi::Scalar(ref scalar) = self.ret.layout.abi {
            // If the value is a boolean, the range is 0..2 and that ultimately
            // become 0..0 when the type becomes i1, which would be rejected
            // by the LLVM verifier.
            if let Int(..) = scalar.value {
                if !scalar.is_bool() && !scalar.is_always_valid(bx) {
                    bx.range_metadata(callsite, scalar.valid_range);
                }
            }
        }
        for arg in &self.args {
            if arg.pad.is_some() {
                apply(bx.cx, &ArgAttributes::new());
            }
            match arg.mode {
                PassMode::Ignore => {}
                PassMode::Indirect {
                    ref attrs,
                    extra_attrs: None,
                    on_stack: true,
                } => {
                    apply(bx.cx, attrs);
                }
                PassMode::Direct(ref attrs)
                | PassMode::Indirect {
                    ref attrs,
                    extra_attrs: None,
                    on_stack: false,
                } => {
                    apply(bx.cx, attrs);
                }
                PassMode::Indirect {
                    ref attrs,
                    extra_attrs: Some(ref extra_attrs),
                    on_stack: _,
                } => {
                    apply(bx.cx, attrs);
                    apply(bx.cx, extra_attrs);
                }
                PassMode::Pair(ref a, ref b) => {
                    apply(bx.cx, a);
                    apply(bx.cx, b);
                }
                PassMode::Cast(_) => {
                    apply(bx.cx, &ArgAttributes::new());
                }
            }
        }
    }
}

impl<'a, 'll, 'tcx> AbiBuilderMethods<'tcx> for Builder<'a, 'll, 'tcx> {
    fn apply_attrs_callsite(&mut self, fn_abi: &FnAbi<'tcx, Ty<'tcx>>, callsite: Self::Value) {
        fn_abi.apply_attrs_callsite(self, callsite)
    }

    fn get_param(&self, index: usize) -> Self::Value {
        let val = llvm::get_param(self.llfn(), index as c_uint);
        trace!("Get param `{:?}`", val);
        unsafe {
            let llfnty = LLVMRustGetFunctionType(self.llfn());
            let map = self.remapped_integer_args.borrow();
            if let Some((_, key)) = map.get(llfnty) {
                if let Some((_, new_ty)) = key.iter().find(|t| t.0 == index) {
                    trace!("Casting irregular param {:?} to {:?}", val, new_ty);
                    return transmute_llval(*self.llbuilder.lock().unwrap(), self.cx, val, *new_ty);
                }
            }
            val
        }
    }
}

pub(crate) trait ArgAbiExt<'ll, 'tcx> {
    fn memory_ty(&self, cx: &CodegenCx<'ll, 'tcx>) -> &'ll Type;
    fn store(
        &self,
        bx: &mut Builder<'_, 'll, 'tcx>,
        val: &'ll Value,
        dst: PlaceRef<'tcx, &'ll Value>,
    );
    fn store_fn_arg(
        &self,
        bx: &mut Builder<'_, 'll, 'tcx>,
        idx: &mut usize,
        dst: PlaceRef<'tcx, &'ll Value>,
    );
}

impl<'ll, 'tcx> ArgAbiExt<'ll, 'tcx> for ArgAbi<'tcx, Ty<'tcx>> {
    /// Gets the LLVM type for a place of the original Rust type of
    /// this argument/return, i.e., the result of `type_of::type_of`.
    fn memory_ty(&self, cx: &CodegenCx<'ll, 'tcx>) -> &'ll Type {
        self.layout.llvm_type(cx)
    }

    /// Stores a direct/indirect value described by this ArgAbi into a
    /// place for the original Rust type of this argument/return.
    /// Can be used for both storing formal arguments into Rust variables
    /// or results of call/invoke instructions into their destinations.
    fn store(
        &self,
        bx: &mut Builder<'_, 'll, 'tcx>,
        val: &'ll Value,
        dst: PlaceRef<'tcx, &'ll Value>,
    ) {
        if self.is_ignore() {
            return;
        }

        if self.is_sized_indirect() {
            OperandValue::Ref(val, None, self.layout.align.abi).store(bx, dst)
        } else if self.is_unsized_indirect() {
            bug!("unsized `ArgAbi` must be handled through `store_fn_arg`");
        } else if let PassMode::Cast(cast) = self.mode {
            let can_store_through_cast_ptr = false;
            if can_store_through_cast_ptr {
                let cast_ptr_llty = bx.type_ptr_to(cast.llvm_type(bx));
                let cast_dst = bx.pointercast(dst.llval, cast_ptr_llty);
                bx.store(val, cast_dst, self.layout.align.abi);
            } else {
                let scratch_size = cast.size(bx);
                let scratch_align = cast.align(bx);
                let llscratch = bx.alloca(cast.llvm_type(bx), scratch_align);
                bx.lifetime_start(llscratch, scratch_size);

                bx.store(val, llscratch, scratch_align);

                bx.memcpy(
                    dst.llval,
                    self.layout.align.abi,
                    llscratch,
                    scratch_align,
                    bx.const_usize(self.layout.size.bytes()),
                    MemFlags::empty(),
                );

                bx.lifetime_end(llscratch, scratch_size);
            }
        } else {
            OperandValue::Immediate(val).store(bx, dst);
        }
    }

    fn store_fn_arg<'a>(
        &self,
        bx: &mut Builder<'a, 'll, 'tcx>,
        idx: &mut usize,
        dst: PlaceRef<'tcx, &'ll Value>,
    ) {
        let mut next = || {
            let val = llvm::get_param(bx.llfn(), *idx as c_uint);
            *idx += 1;
            val
        };
        match self.mode {
            PassMode::Ignore => {}
            PassMode::Pair(..) => {
                OperandValue::Pair(next(), next()).store(bx, dst);
            }
            PassMode::Indirect {
                attrs: _,
                extra_attrs: Some(_),
                on_stack: _,
            } => {
                OperandValue::Ref(next(), Some(next()), self.layout.align.abi).store(bx, dst);
            }
            PassMode::Direct(_)
            | PassMode::Indirect {
                attrs: _,
                extra_attrs: None,
                on_stack: _,
            }
            | PassMode::Cast(_) => {
                let next_arg = next();
                self.store(bx, next_arg, dst);
            }
        }
    }
}
