use std::ops::Range;

use crate::debug_info;
use crate::llvm::{self, Bool, True, Type, Value};
use libc::{c_char, c_uint};
use rustc_codegen_ssa::traits::{
    BaseTypeMethods, ConstMethods, DerivedTypeMethods, MiscMethods, StaticMethods,
};
use rustc_hir::def_id::DefId;
use rustc_middle::mir::interpret::{
    read_target_uint, Allocation, ErrorHandled, GlobalAlloc, Pointer,
};
use rustc_middle::{
    bug,
    middle::codegen_fn_attrs::{CodegenFnAttrFlags, CodegenFnAttrs},
    mir::interpret::{InitChunk, Scalar as InterpScalar},
    mir::mono::{Linkage, MonoItem},
    span_bug,
    ty::{self, layout::LayoutOf, Instance, Ty},
};
use rustc_span::Symbol;
use rustc_target::abi::{
    AddressSpace, Align, HasDataLayout, Primitive, Scalar, Size, WrappingRange,
};
use tracing::trace;

use crate::{context::CodegenCx, ty::LayoutLlvmExt};

pub(crate) fn bytes_in_context<'ll>(llcx: &'ll llvm::Context, bytes: &[u8]) -> &'ll Value {
    unsafe {
        let ptr = bytes.as_ptr() as *const c_char;
        llvm::LLVMConstStringInContext(llcx, ptr, bytes.len() as c_uint, True)
    }
}

impl<'ll, 'tcx> CodegenCx<'ll, 'tcx> {
    pub fn const_array(&self, ty: &'ll Type, elts: &[&'ll Value]) -> &'ll Value {
        unsafe { llvm::LLVMConstArray(ty, elts.as_ptr(), elts.len() as c_uint) }
    }

    pub fn const_bytes(&self, bytes: &[u8]) -> &'ll Value {
        bytes_in_context(self.llcx, bytes)
    }

    pub(crate) fn const_cstr(&self, s: Symbol, null_terminated: bool) -> &'ll Value {
        trace!("Const cstr for symbol `{:?}`", s);
        unsafe {
            if let Some(&llval) = self.const_cstr_cache.borrow().get(&s) {
                return llval;
            }

            let s_str = s.as_str();
            let sc = llvm::LLVMConstStringInContext(
                self.llcx,
                s_str.as_ptr() as *const c_char,
                s_str.len() as c_uint,
                !null_terminated as Bool,
            );
            let sym = self.generate_local_symbol_name("str");
            let g = self
                .define_global(&sym[..], self.val_ty(sc), AddressSpace::DATA)
                .unwrap_or_else(|| {
                    bug!("symbol `{}` is already defined", sym);
                });
            llvm::LLVMSetInitializer(g, sc);
            llvm::LLVMSetGlobalConstant(g, True);
            llvm::LLVMRustSetLinkage(g, llvm::Linkage::InternalLinkage);

            self.const_cstr_cache.borrow_mut().insert(s, g);
            g
        }
    }
}

pub(crate) fn const_alloc_to_llvm<'ll>(cx: &CodegenCx<'ll, '_>, alloc: &Allocation) -> &'ll Value {
    trace!("Const alloc to llvm");
    let mut llvals = Vec::with_capacity(alloc.relocations().len() + 1);
    let dl = cx.data_layout();
    let pointer_size = dl.pointer_size.bytes() as usize;

    // Note: this function may call `inspect_with_uninit_and_ptr_outside_interpreter`,
    // so `range` must be within the bounds of `alloc` and not contain or overlap a relocation.
    fn append_chunks_of_init_and_uninit_bytes<'ll, 'a, 'b>(
        llvals: &mut Vec<&'ll Value>,
        cx: &'a CodegenCx<'ll, 'b>,
        alloc: &'a Allocation,
        range: Range<usize>,
    ) {
        let mut chunks = alloc
            .init_mask()
            .range_as_init_chunks(Size::from_bytes(range.start), Size::from_bytes(range.end));

        let chunk_to_llval = move |chunk| match chunk {
            InitChunk::Init(range) => {
                let range = (range.start.bytes() as usize)..(range.end.bytes() as usize);
                let bytes = alloc.inspect_with_uninit_and_ptr_outside_interpreter(range);
                cx.const_bytes(bytes)
            }
            InitChunk::Uninit(range) => {
                let len = range.end.bytes() - range.start.bytes();
                cx.const_undef(cx.type_array(cx.type_i8(), len))
            }
        };

        // Generating partially-uninit consts inhibits optimizations, so it is disabled by default.
        // See https://github.com/rust-lang/rust/issues/84565.
        let allow_partially_uninit = match cx
            .sess()
            .opts
            .debugging_opts
            .partially_uninit_const_threshold
        {
            Some(max) => range.len() <= max,
            None => false,
        };

        if allow_partially_uninit {
            llvals.extend(chunks.map(chunk_to_llval));
        } else {
            let llval = match (chunks.next(), chunks.next()) {
                (Some(chunk), None) => {
                    // exactly one chunk, either fully init or fully uninit
                    chunk_to_llval(chunk)
                }
                _ => {
                    // partially uninit, codegen as if it was initialized
                    // (using some arbitrary value for uninit bytes)
                    let bytes = alloc.inspect_with_uninit_and_ptr_outside_interpreter(range);
                    cx.const_bytes(bytes)
                }
            };
            llvals.push(llval);
        }
    }

    let mut next_offset = 0;
    for &(offset, alloc_id) in alloc.relocations().iter() {
        let offset = offset.bytes();
        assert_eq!(offset as usize as u64, offset);
        let offset = offset as usize;
        if offset > next_offset {
            // This `inspect` is okay since we have checked that it is not within a relocation, it
            // is within the bounds of the allocation, and it doesn't affect interpreter execution
            // (we inspect the result after interpreter execution).
            append_chunks_of_init_and_uninit_bytes(&mut llvals, cx, alloc, next_offset..offset);
        }
        let ptr_offset = read_target_uint(
            dl.endian,
            // This `inspect` is okay since it is within the bounds of the allocation, it doesn't
            // affect interpreter execution (we inspect the result after interpreter execution),
            // and we properly interpret the relocation as a relocation pointer offset.
            alloc.inspect_with_uninit_and_ptr_outside_interpreter(offset..(offset + pointer_size)),
        )
        .expect("const_alloc_to_llvm: could not read relocation pointer")
            as u64;

        let address_space = match cx.tcx.global_alloc(alloc_id) {
            GlobalAlloc::Function(..) => cx.data_layout().instruction_address_space,
            GlobalAlloc::Static(..) | GlobalAlloc::Memory(..) => AddressSpace::DATA,
        };

        llvals.push(cx.scalar_to_backend(
            InterpScalar::from_pointer(
                Pointer::new(alloc_id, Size::from_bytes(ptr_offset)),
                &cx.tcx,
            ),
            Scalar {
                value: Primitive::Pointer,
                valid_range: WrappingRange { start: 0, end: !0 },
            },
            cx.type_i8p_ext(address_space),
        ));
        next_offset = offset + pointer_size;
    }
    if alloc.len() >= next_offset {
        let range = next_offset..alloc.len();
        // This `inspect` is okay since we have check that it is after all relocations, it is
        // within the bounds of the allocation, and it doesn't affect interpreter execution (we
        // inspect the result after interpreter execution).
        append_chunks_of_init_and_uninit_bytes(&mut llvals, cx, alloc, range);
    }

    cx.const_struct(&llvals, true)
}

pub(crate) fn codegen_static_initializer<'ll, 'tcx>(
    cx: &CodegenCx<'ll, 'tcx>,
    def_id: DefId,
) -> Result<(&'ll Value, &'tcx Allocation), ErrorHandled> {
    let alloc = cx.tcx.eval_static_initializer(def_id)?;
    Ok((const_alloc_to_llvm(cx, alloc), alloc))
}

pub(crate) fn linkage_to_llvm(linkage: Linkage) -> llvm::Linkage {
    match linkage {
        Linkage::External => llvm::Linkage::ExternalLinkage,
        Linkage::AvailableExternally => llvm::Linkage::AvailableExternallyLinkage,
        Linkage::LinkOnceAny => llvm::Linkage::LinkOnceAnyLinkage,
        Linkage::LinkOnceODR => llvm::Linkage::LinkOnceODRLinkage,
        Linkage::WeakAny => llvm::Linkage::WeakAnyLinkage,
        Linkage::WeakODR => llvm::Linkage::WeakODRLinkage,
        Linkage::Appending => llvm::Linkage::AppendingLinkage,
        Linkage::Internal => llvm::Linkage::InternalLinkage,
        Linkage::Private => llvm::Linkage::PrivateLinkage,
        Linkage::ExternalWeak => llvm::Linkage::ExternalWeakLinkage,
        Linkage::Common => llvm::Linkage::CommonLinkage,
    }
}

fn check_and_apply_linkage<'ll, 'tcx>(
    cx: &CodegenCx<'ll, 'tcx>,
    attrs: &CodegenFnAttrs,
    ty: Ty<'tcx>,
    sym: &str,
    span_def_id: DefId,
) -> &'ll Value {
    let llty = cx.layout_of(ty).llvm_type(cx);
    if let Some(linkage) = attrs.linkage {
        // https://docs.nvidia.com/cuda/nvvm-ir-spec/index.html#linkage-types-nvvm
        use Linkage::*;
        match linkage {
            External | Private | Internal | Common | AvailableExternally | LinkOnceAny
            | LinkOnceODR | WeakAny | WeakODR => {}
            _ => cx.fatal(&format!("Unsupported linkage kind: {:?}", linkage)),
        }

        // If this is a static with a linkage specified, then we need to handle
        // it a little specially. The typesystem prevents things like &T and
        // extern "C" fn() from being non-null, so we can't just declare a
        // static and call it a day. Some linkages (like weak) will make it such
        // that the static actually has a null value.
        let llty2 = if let ty::RawPtr(ref mt) = ty.kind() {
            cx.layout_of(mt.ty).llvm_type(cx)
        } else {
            cx.sess().span_fatal(
                cx.tcx.def_span(span_def_id),
                "must have type `*const T` or `*mut T` due to `#[linkage]` attribute",
            )
        };
        unsafe {
            // Declare a symbol `foo` with the desired linkage.
            let g1 = cx.declare_global(sym, llty2, AddressSpace::DATA);
            llvm::LLVMRustSetLinkage(g1, linkage_to_llvm(linkage));

            // Declare an internal global `extern_with_linkage_foo` which
            // is initialized with the address of `foo`.  If `foo` is
            // discarded during linking (for example, if `foo` has weak
            // linkage and there are no definitions), then
            // `extern_with_linkage_foo` will instead be initialized to
            // zero.
            let mut real_name = "_rust_extern_with_linkage_".to_string();
            real_name.push_str(sym);
            let g2 = cx
                .define_global(&real_name, llty, AddressSpace::DATA)
                .unwrap_or_else(|| {
                    cx.sess().span_fatal(
                        cx.tcx.def_span(span_def_id),
                        &format!("symbol `{}` is already defined", &sym),
                    )
                });
            llvm::LLVMRustSetLinkage(g2, llvm::Linkage::InternalLinkage);
            llvm::LLVMSetInitializer(g2, g1);
            g2
        }
    } else {
        cx.declare_global(sym, llty, AddressSpace::DATA)
    }
}

impl<'ll, 'tcx> CodegenCx<'ll, 'tcx> {
    pub(crate) fn const_bitcast(&self, val: &'ll Value, ty: &'ll Type) -> &'ll Value {
        trace!("Const bitcast: `{:?}` to `{:?}`", val, ty);
        unsafe { llvm::LLVMConstBitCast(val, ty) }
    }

    pub(crate) fn static_addr_of_mut(
        &self,
        cv: &'ll Value,
        align: Align,
        kind: Option<&str>,
    ) -> &'ll Value {
        unsafe {
            // TODO(RDambrosio016): replace this with latest rustc's handling when we use llvm 13
            let name = self.generate_local_symbol_name(kind.unwrap_or("private"));
            let gv = self
                .define_global(&name[..], self.val_ty(cv), AddressSpace::DATA)
                .unwrap_or_else(|| bug!("symbol `{}` is already defined", name));
            llvm::LLVMRustSetLinkage(gv, llvm::Linkage::PrivateLinkage);
            llvm::LLVMSetInitializer(gv, cv);
            llvm::LLVMSetAlignment(gv, align.bytes() as c_uint);
            llvm::SetUnnamedAddress(gv, llvm::UnnamedAddr::Global);
            gv
        }
    }

    pub(crate) fn get_static(&self, def_id: DefId) -> &'ll Value {
        let instance = Instance::mono(self.tcx, def_id);
        if let Some(&g) = self.instances.borrow().get(&instance) {
            return g;
        }

        let defined_in_current_codegen_unit = self
            .codegen_unit
            .items()
            .contains_key(&MonoItem::Static(def_id));
        assert!(
            !defined_in_current_codegen_unit,
            "consts::get_static() should always hit the cache for \
                 statics defined in the same CGU, but did not for `{:?}`",
            def_id
        );

        let ty = instance.ty(self.tcx, ty::ParamEnv::reveal_all());
        let sym = self.tcx.symbol_name(instance).name;
        let fn_attrs = self.tcx.codegen_fn_attrs(def_id);

        let g = if def_id.is_local() && !self.tcx.is_foreign_item(def_id) {
            let llty = self.layout_of(ty).llvm_type(self);
            if let Some(g) = self.get_declared_value(sym) {
                if self.val_ty(g) != self.type_ptr_to(llty) {
                    span_bug!(self.tcx.def_span(def_id), "Conflicting types for static");
                }
            }

            let g = self.declare_global(sym, llty, AddressSpace::DATA);

            if !self.tcx.is_reachable_non_generic(def_id) {
                unsafe {
                    llvm::LLVMRustSetVisibility(g, llvm::Visibility::Hidden);
                }
            }

            g
        } else {
            check_and_apply_linkage(self, fn_attrs, ty, sym, def_id)
        };

        if fn_attrs.flags.contains(CodegenFnAttrFlags::THREAD_LOCAL) {
            self.unsupported("thread locals");
        }

        self.instances.borrow_mut().insert(instance, g);
        g
    }
}

impl<'ll, 'tcx> StaticMethods for CodegenCx<'ll, 'tcx> {
    fn static_addr_of(&self, cv: &'ll Value, align: Align, kind: Option<&str>) -> &'ll Value {
        if let Some(&gv) = self.const_globals.borrow().get(&cv) {
            unsafe {
                // Upgrade the alignment in cases where the same constant is used with different
                // alignment requirements
                let llalign = align.bytes() as u32;
                if llalign > llvm::LLVMGetAlignment(gv) {
                    llvm::LLVMSetAlignment(gv, llalign);
                }
            }
            return gv;
        }
        let gv = self.static_addr_of_mut(cv, align, kind);
        unsafe {
            llvm::LLVMSetGlobalConstant(gv, True);
        }
        self.const_globals.borrow_mut().insert(cv, gv);
        gv
    }

    fn codegen_static(&self, def_id: DefId, is_mutable: bool) {
        unsafe {
            let attrs = self.tcx.codegen_fn_attrs(def_id);

            let (v, _) = match codegen_static_initializer(self, def_id) {
                Ok(v) => v,
                // Error has already been reported
                Err(_) => return,
            };

            let g = self.get_static(def_id);

            let mut val_llty = self.val_ty(v);
            let v = if val_llty == self.type_i1() {
                val_llty = self.type_i8();
                llvm::LLVMConstZExt(v, val_llty)
            } else {
                v
            };

            let instance = Instance::mono(self.tcx, def_id);
            let ty = instance.ty(self.tcx, ty::ParamEnv::reveal_all());
            let llty = self.layout_of(ty).llvm_type(self);
            let g = if val_llty == llty {
                g
            } else {
                trace!(
                    "Making new RAUW global: from ty `{:?}` to `{:?}`, initializer: `{:?}`",
                    llty,
                    val_llty,
                    v
                );
                // If we created the global with the wrong type,
                // correct the type.
                let name = llvm::get_value_name(g).to_vec();

                llvm::set_value_name(g, b"");

                let linkage = llvm::LLVMRustGetLinkage(g);
                let visibility = llvm::LLVMRustGetVisibility(g);

                let new_g = llvm::LLVMRustGetOrInsertGlobal(
                    self.llmod,
                    name.as_ptr().cast(),
                    name.len(),
                    val_llty,
                    AddressSpace::DATA.0,
                );

                llvm::LLVMRustSetLinkage(new_g, linkage);
                llvm::LLVMRustSetVisibility(new_g, visibility);

                // To avoid breaking any invariants, we leave around the old
                // global for the moment; we'll replace all references to it
                // with the new global later. (See base::codegen_backend.)
                self.statics_to_rauw.borrow_mut().push((g, new_g));
                new_g
            };
            trace!("Codegen static `{:?}`", g);
            llvm::LLVMSetAlignment(g, self.align_of(ty).bytes() as c_uint);
            llvm::LLVMSetInitializer(g, v);

            debug_info::create_global_var_metadata(self, def_id, g);

            // As an optimization, all shared statics which do not have interior
            // mutability are placed into read-only memory.
            if !is_mutable && self.type_is_freeze(ty) {
                // TODO(RDambrosio016): is this the same as putting this in
                // the __constant__ addrspace for nvvm? should we set this addrspace explicitly?
                llvm::LLVMSetGlobalConstant(g, llvm::True);
            }

            debug_info::create_global_var_metadata(self, def_id, g);

            if attrs.flags.contains(CodegenFnAttrFlags::THREAD_LOCAL) {
                self.unsupported("thread locals");
            }

            if attrs.flags.contains(CodegenFnAttrFlags::USED) {
                self.add_used_global(g);
            }
        }
    }

    /// Add a global value to a list to be stored in the `llvm.used` variable, an array of i8*.
    fn add_used_global(&self, global: &'ll Value) {
        let cast = unsafe { llvm::LLVMConstPointerCast(global, self.type_i8p()) };
        self.used_statics.borrow_mut().push(cast);
    }

    /// Add a global value to a list to be stored in the `llvm.compiler.used` variable,
    /// an array of i8*.
    fn add_compiler_used_global(&self, global: &'ll Value) {
        let cast = unsafe { llvm::LLVMConstPointerCast(global, self.type_i8p()) };
        self.compiler_used_statics.borrow_mut().push(cast);
    }
}
