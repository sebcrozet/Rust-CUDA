use crate::llvm::{self};
use crate::{builder::Builder, context::CodegenCx, lto::ThinBuffer, LlvmMod, NvvmCodegenBackend};
use libc::{c_char, size_t};
use rustc_codegen_ssa::back::write::{TargetMachineFactoryConfig, TargetMachineFactoryFn};
use rustc_codegen_ssa::traits::{DebugInfoMethods, MiscMethods};
use rustc_codegen_ssa::{
    back::write::{CodegenContext, ModuleConfig},
    base::maybe_create_entry_wrapper,
    mono_item::MonoItemExt,
    traits::{BaseTypeMethods, ThinBufferMethods},
    CompiledModule, ModuleCodegen, ModuleKind,
};
use rustc_data_structures::small_c_str::SmallCStr;
use rustc_errors::{FatalError, Handler};
use rustc_fs_util::path_to_c_string;
use rustc_middle::bug;
use rustc_middle::mir::mono::MonoItem;
use rustc_middle::{dep_graph, ty::TyCtxt};
use rustc_session::config::{self, DebugInfo, OutputType};
use rustc_session::Session;
use rustc_span::{sym, Symbol};
use rustc_target::spec::{CodeModel, RelocModel};
use std::ffi::CString;
use std::sync::Arc;
use std::{
    io::{self, Write},
    slice,
};

pub fn llvm_err(handler: &Handler, msg: &str) -> FatalError {
    match llvm::last_error() {
        Some(err) => handler.fatal(&format!("{}: {}", msg, err)),
        None => handler.fatal(msg),
    }
}

pub fn to_llvm_opt_settings(
    cfg: config::OptLevel,
) -> (llvm::CodeGenOptLevel, llvm::CodeGenOptSize) {
    use self::config::OptLevel::*;
    match cfg {
        No => (llvm::CodeGenOptLevel::None, llvm::CodeGenOptSizeNone),
        Less => (llvm::CodeGenOptLevel::Less, llvm::CodeGenOptSizeNone),
        Default => (llvm::CodeGenOptLevel::Default, llvm::CodeGenOptSizeNone),
        Aggressive => (llvm::CodeGenOptLevel::Aggressive, llvm::CodeGenOptSizeNone),
        Size => (llvm::CodeGenOptLevel::Default, llvm::CodeGenOptSizeDefault),
        SizeMin => (
            llvm::CodeGenOptLevel::Default,
            llvm::CodeGenOptSizeAggressive,
        ),
    }
}

fn to_llvm_relocation_model(relocation_model: RelocModel) -> llvm::RelocMode {
    match relocation_model {
        RelocModel::Static => llvm::RelocMode::Static,
        RelocModel::Pic => llvm::RelocMode::PIC,
        RelocModel::DynamicNoPic => llvm::RelocMode::DynamicNoPic,
        RelocModel::Ropi => llvm::RelocMode::ROPI,
        RelocModel::Rwpi => llvm::RelocMode::RWPI,
        RelocModel::RopiRwpi => llvm::RelocMode::ROPI_RWPI,
        RelocModel::Pie => panic!(),
    }
}

pub(crate) fn to_llvm_code_model(code_model: Option<CodeModel>) -> llvm::CodeModel {
    match code_model {
        Some(CodeModel::Tiny) => llvm::CodeModel::Small,
        Some(CodeModel::Small) => llvm::CodeModel::Small,
        Some(CodeModel::Kernel) => llvm::CodeModel::Kernel,
        Some(CodeModel::Medium) => llvm::CodeModel::Medium,
        Some(CodeModel::Large) => llvm::CodeModel::Large,
        None => llvm::CodeModel::None,
    }
}

pub fn target_machine_factory(
    sess: &Session,
    optlvl: config::OptLevel,
) -> TargetMachineFactoryFn<NvvmCodegenBackend> {
    let reloc_model = to_llvm_relocation_model(sess.relocation_model());

    let (opt_level, _) = to_llvm_opt_settings(optlvl);
    let use_softfp = sess.opts.cg.soft_float;

    let ffunction_sections = sess
        .opts
        .debugging_opts
        .function_sections
        .unwrap_or(sess.target.function_sections);
    let fdata_sections = ffunction_sections;

    let code_model = to_llvm_code_model(sess.code_model());

    let triple = SmallCStr::new(&sess.target.llvm_target);
    // let cpu = SmallCStr::new("sm_30");
    let features = CString::new("").unwrap();
    let trap_unreachable = sess
        .opts
        .debugging_opts
        .trap_unreachable
        .unwrap_or(sess.target.trap_unreachable);

    Arc::new(move |_config: TargetMachineFactoryConfig| {
        let tm = unsafe {
            llvm::LLVMRustCreateTargetMachine(
                triple.as_ptr(),
                std::ptr::null(),
                features.as_ptr(),
                code_model,
                reloc_model,
                opt_level,
                false,
                use_softfp,
                ffunction_sections,
                fdata_sections,
                trap_unreachable,
                false,
            )
        };
        tm.ok_or_else(|| {
            format!(
                "Could not create LLVM TargetMachine for triple: {}",
                triple.to_str().unwrap()
            )
        })
    })
}

/// Compile a single module (in an nvvm context this means getting the llvm bitcode out of it)
pub(crate) unsafe fn codegen(
    cgcx: &CodegenContext<NvvmCodegenBackend>,
    diag_handler: &Handler,
    module: ModuleCodegen<LlvmMod>,
    config: &ModuleConfig,
) -> Result<CompiledModule, FatalError> {
    // For NVVM, all the codegen we need to do is turn the llvm modules
    // into llvm bitcode and write them to a tempdir. nvvm expects llvm
    // bitcode as the modules to be added to the program. Then as the last step
    // we gather all those tasty bitcode files, add them to the nvvm program
    // and finally tell nvvm to compile it, which gives us a ptx file.
    //
    // we also implement emit_ir so we can dump the IR fed to nvvm in case we
    // feed it anything it doesnt like

    let _timer = cgcx
        .prof
        .generic_activity_with_arg("NVVM_module_codegen", &module.name[..]);

    let llmod = module.module_llvm.llmod.as_ref().unwrap();
    let mod_name = module.name.clone();
    let module_name = Some(&mod_name[..]);

    let out = cgcx
        .output_filenames
        .temp_path(OutputType::Object, module_name);

    // nvvm ir *is* llvm ir so emit_ir fits the expectation of llvm ir which is why we
    // implement this. this is copy and pasted straight from rustc_codegen_llvm
    // because im too lazy to make it seem like i rewrote this when its the same logic
    if config.emit_ir {
        let _timer = cgcx
            .prof
            .generic_activity_with_arg("NVVM_module_codegen_emit_ir", &module.name[..]);
        let out = cgcx
            .output_filenames
            .temp_path(OutputType::LlvmAssembly, module_name);
        let out_c = path_to_c_string(&out);

        extern "C" fn demangle_callback(
            input_ptr: *const c_char,
            input_len: size_t,
            output_ptr: *mut c_char,
            output_len: size_t,
        ) -> size_t {
            let input =
                unsafe { slice::from_raw_parts(input_ptr as *const u8, input_len as usize) };

            let input = match std::str::from_utf8(input) {
                Ok(s) => s,
                Err(_) => return 0,
            };

            let output =
                unsafe { slice::from_raw_parts_mut(output_ptr as *mut u8, output_len as usize) };
            let mut cursor = io::Cursor::new(output);

            let demangled = match rustc_demangle::try_demangle(input) {
                Ok(d) => d,
                Err(_) => return 0,
            };

            if write!(cursor, "{:#}", demangled).is_err() {
                // Possible only if provided buffer is not big enough
                return 0;
            }

            cursor.position() as size_t
        }

        let result = llvm::LLVMRustPrintModule(llmod, out_c.as_ptr(), demangle_callback);

        result.into_result().map_err(|()| {
            let msg = format!("failed to write NVVM IR to {}", out.display());
            llvm_err(diag_handler, &msg)
        })?;
    }

    let _bc_timer = cgcx
        .prof
        .generic_activity_with_arg("NVVM_module_codegen_make_bitcode", &module.name[..]);

    let thin = ThinBuffer::new(llmod);

    let data = thin.data();

    let _bc_emit_timer = cgcx
        .prof
        .generic_activity_with_arg("NVVM_module_codegen_emit_bitcode", &module.name[..]);

    if let Err(e) = std::fs::write(&out, data) {
        let msg = format!("failed to write bytecode to {}: {}", out.display(), e);
        diag_handler.err(&msg);
    }

    Ok(CompiledModule {
        name: mod_name,
        kind: module.kind,
        object: Some(out),
        dwarf_object: None,
        bytecode: None,
    })
}

/// compile a single codegen unit.
/// This involves getting its llvm module and doing some housekeeping such as
/// monomorphizing items and using RAUW on statics. This codegenned module is then
/// given to other functions to "compile it" (in our case not really because nvvm does
/// codegen on all the modules at once) and then link it (once again, nvvm does linking and codegen
/// in a single step)
pub fn compile_codegen_unit(tcx: TyCtxt<'_>, cgu_name: Symbol) -> (ModuleCodegen<LlvmMod>, u64) {
    let dep_node = tcx.codegen_unit(cgu_name).codegen_dep_node(tcx);
    let (module, _) = tcx.dep_graph.with_task(
        dep_node,
        tcx,
        cgu_name,
        module_codegen,
        dep_graph::hash_result,
    );

    fn module_codegen(tcx: TyCtxt<'_>, cgu_name: Symbol) -> ModuleCodegen<LlvmMod> {
        let cgu = tcx.codegen_unit(cgu_name);

        // Instantiate monomorphizations without filling out definitions yet...
        let llvm_module = LlvmMod::new(&cgu_name.as_str());
        {
            let cx = CodegenCx::new(tcx, cgu, &llvm_module);

            let mono_items = cx.codegen_unit.items_in_deterministic_order(cx.tcx);

            for &(mono_item, (linkage, visibility)) in &mono_items {
                mono_item.predefine::<Builder<'_, '_, '_>>(&cx, linkage, visibility);
            }

            // ... and now that we have everything pre-defined, fill out those definitions.
            for &(mono_item, _) in &mono_items {
                mono_item.define::<Builder<'_, '_, '_>>(&cx);
                if let MonoItem::Fn(inst) = mono_item {
                    let name = tcx.symbol_name(inst).name;
                    let attrs = tcx.get_attrs(inst.def_id());
                    let is_no_mangle = attrs.iter().any(|x| x.has_name(sym::no_mangle));

                    if name == "rust_begin_unwind"
                        || name.starts_with("__rg")
                        || name == "rust_oom"
                        || is_no_mangle
                    {
                        let func = cx.get_fn(inst);
                        let llval =
                            unsafe { llvm::LLVMConstBitCast(func, cx.type_ptr_to(cx.type_i8())) };
                        cx.used_statics.borrow_mut().push(llval);
                    }
                }
            }

            // a main function for gpu kernels really makes no sense but
            // codegen it anyways.
            // sanitize attrs are not allowed in nvvm so do nothing further.
            maybe_create_entry_wrapper::<Builder<'_, '_, '_>>(&cx);

            // Run replace-all-uses-with for statics that need it
            for &(old_g, new_g) in cx.statics_to_rauw.borrow().iter() {
                unsafe {
                    let bitcast = llvm::LLVMConstPointerCast(new_g, cx.val_ty(old_g));
                    llvm::LLVMReplaceAllUsesWith(old_g, bitcast);
                    llvm::LLVMDeleteGlobal(old_g);
                }
            }

            // Create the llvm.used and llvm.compiler.used variables.
            if !cx.used_statics().borrow().is_empty() {
                cx.create_used_variable();
            }
            if !cx.compiler_used_statics().borrow().is_empty() {
                cx.create_compiler_used_variable();
            }

            // Finalize debuginfo
            if cx.sess().opts.debuginfo != DebugInfo::None {
                cx.debuginfo_finalize();
            }
        }

        ModuleCodegen {
            name: cgu_name.to_string(),
            module_llvm: llvm_module,
            kind: ModuleKind::Regular,
        }
    }

    // TODO(RDambrosio016): maybe the same cost as the llvm codegen works?
    // nvvm does some exotic things and does linking too so it might be inaccurate
    (module, 0)
}

// TODO: We use rustc's optimization approach from when it used llvm 7, because many things
// are incompatible with llvm 7 nowadays. Although we should probably consult a rustc dev on whether
// any big things were discovered in that timespan that we should modify.
pub(crate) unsafe fn optimize(
    cgcx: &CodegenContext<NvvmCodegenBackend>,
    diag_handler: &Handler,
    module: &ModuleCodegen<LlvmMod>,
    config: &ModuleConfig,
) -> Result<(), FatalError> {
    let _timer = cgcx
        .prof
        .generic_activity_with_arg("LLVM_module_optimize", &module.name[..]);

    let llmod = &*module.module_llvm.llmod;

    let module_name = module.name.clone();
    let module_name = Some(&module_name[..]);

    if config.emit_no_opt_bc {
        let out = cgcx
            .output_filenames
            .temp_path_ext("no-opt.bc", module_name);
        let out = path_to_c_string(&out);
        llvm::LLVMWriteBitcodeToFile(llmod, out.as_ptr());
    }

    let tm_factory_config = TargetMachineFactoryConfig {
        split_dwarf_file: None,
    };

    let tm = (cgcx.tm_factory)(tm_factory_config).expect("failed to create target machine");

    if config.opt_level.is_some() {
        let fpm = llvm::LLVMCreateFunctionPassManagerForModule(llmod);
        let mpm = llvm::LLVMCreatePassManager();

        let addpass = |pass_name: &str| {
            let pass_name = CString::new(pass_name).unwrap();
            let pass = llvm::LLVMRustFindAndCreatePass(pass_name.as_ptr());
            if pass.is_none() {
                return false;
            }
            let pass = pass.unwrap();
            let pass_manager = match llvm::LLVMRustPassKind(pass) {
                llvm::PassKind::Function => &fpm,
                llvm::PassKind::Module => &mpm,
                llvm::PassKind::Other => {
                    diag_handler.err("Encountered LLVM pass kind we can't handle");
                    return true;
                }
            };
            llvm::LLVMRustAddPass(pass_manager, pass);
            true
        };

        if !config.no_prepopulate_passes {
            llvm::LLVMRustAddAnalysisPasses(tm, fpm, llmod);
            llvm::LLVMRustAddAnalysisPasses(tm, mpm, llmod);
            let opt_level = config
                .opt_level
                .map_or(llvm::CodeGenOptLevel::None, |x| to_llvm_opt_settings(x).0);
            with_llvm_pmb(llmod, config, opt_level, &mut |b| {
                llvm::LLVMPassManagerBuilderPopulateFunctionPassManager(b, fpm);
                llvm::LLVMPassManagerBuilderPopulateModulePassManager(b, mpm);
            })
        }

        for pass in &config.passes {
            if !addpass(pass) {
                diag_handler.warn(&format!("unknown pass `{}`, ignoring", pass));
            }
        }

        diag_handler.abort_if_errors();

        // Finally, run the actual optimization passes
        llvm::LLVMRustRunFunctionPassManager(fpm, llmod);
        llvm::LLVMRunPassManager(mpm, llmod);

        // Deallocate managers that we're now done with
        llvm::LLVMDisposePassManager(fpm);
        llvm::LLVMDisposePassManager(mpm);
    }

    Ok(())
}

unsafe fn with_llvm_pmb(
    llmod: &llvm::Module,
    config: &ModuleConfig,
    opt_level: llvm::CodeGenOptLevel,
    f: &mut impl FnMut(&llvm::PassManagerBuilder),
) {
    use std::ptr;

    let builder = llvm::LLVMPassManagerBuilderCreate();
    let opt_size = config
        .opt_size
        .map_or(llvm::CodeGenOptSizeNone, |x| to_llvm_opt_settings(x).1);
    let inline_threshold = config.inline_threshold;

    llvm::LLVMRustConfigurePassManagerBuilder(
        builder,
        opt_level,
        config.merge_functions,
        config.vectorize_slp,
        config.vectorize_loop,
        false,
        ptr::null(),
        ptr::null(),
    );

    llvm::LLVMPassManagerBuilderSetSizeLevel(builder, opt_size as u32);

    if opt_size != llvm::CodeGenOptSizeNone {
        llvm::LLVMPassManagerBuilderSetDisableUnrollLoops(builder, 1);
    }

    llvm::LLVMRustAddBuilderLibraryInfo(builder, llmod, config.no_builtins);

    // Here we match what clang does (kinda). For O0 we only inline
    // always-inline functions (but don't add lifetime intrinsics), at O1 we
    // inline with lifetime intrinsics, and O2+ we add an inliner with a
    // thresholds copied from clang.
    match (opt_level, opt_size, inline_threshold) {
        (.., Some(t)) => {
            llvm::LLVMPassManagerBuilderUseInlinerWithThreshold(builder, t as u32);
        }
        (llvm::CodeGenOptLevel::Aggressive, ..) => {
            llvm::LLVMPassManagerBuilderUseInlinerWithThreshold(builder, 275);
        }
        (_, llvm::CodeGenOptSizeDefault, _) => {
            llvm::LLVMPassManagerBuilderUseInlinerWithThreshold(builder, 75);
        }
        (_, llvm::CodeGenOptSizeAggressive, _) => {
            llvm::LLVMPassManagerBuilderUseInlinerWithThreshold(builder, 25);
        }
        (llvm::CodeGenOptLevel::None, ..) => {
            llvm::LLVMRustAddAlwaysInlinePass(builder, false);
        }
        (llvm::CodeGenOptLevel::Less, ..) => {
            llvm::LLVMRustAddAlwaysInlinePass(builder, true);
        }
        (llvm::CodeGenOptLevel::Default, ..) => {
            llvm::LLVMPassManagerBuilderUseInlinerWithThreshold(builder, 225);
        }
        (llvm::CodeGenOptLevel::Other, ..) => {
            bug!("CodeGenOptLevel::Other selected")
        }
    }

    f(builder);
    llvm::LLVMPassManagerBuilderDispose(builder);
}
