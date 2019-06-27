macro_rules! file_for_module {
    ($ctx:ident, $common_files:ident, $module_name:ident) => {{
        let output_dir = &$ctx.cfg.output_dir;
        let target_pointer_width = $ctx.target_pointer_width;
        $common_files
            .entry($module_name.clone())
            .or_insert_with(|| {
                let c_header_path = output_dir.join($module_name.as_str());
                let mut c_header_f = FileWriteCache::new(&c_header_path);
                write!(
                    &mut c_header_f,
                    r##"// Automaticaly generated by rust_swig
#pragma once

//for (u)intX_t types
#include <stdint.h>

#ifdef __cplusplus
static_assert(sizeof(uintptr_t) == sizeof(uint8_t) * {sizeof_usize},
   "our conversation usize <-> uintptr_t is wrong");
#endif
            "##,
                    sizeof_usize = target_pointer_width / 8,
                )
                .expect("write to memory failed, no free mem?");
                c_header_f
            })
    }};
}

mod cpp_code;
mod fclass;
mod fenum;
mod finterface;
mod map_type;

use std::{fmt, io::Write, mem};

use log::{debug, trace};
use petgraph::Direction;
use proc_macro2::TokenStream;
use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;
use strum::IntoEnumIterator;
use syn::{parse_quote, spanned::Spanned, Type};

use crate::{
    cpp::map_type::map_type,
    error::{invalid_src_id_span, DiagnosticError, Result},
    file_cache::FileWriteCache,
    source_registry::SourceId,
    typemap::{
        ast::{parse_ty_with_given_span, parse_ty_with_given_span_checked, TypeName},
        ty::{
            FTypeConvCode, ForeignConversationIntermediate, ForeignConversationRule, ForeignType,
            ForeignTypeS, RustType,
        },
        utils::{
            boxed_type, unpack_from_heap_pointer, validate_cfg_options, ForeignMethodSignature,
            ForeignTypeInfoT,
        },
        CType, CTypes, ForeignTypeInfo, MapToForeignFlag, RustTypeIdx, TypeMapConvRuleInfo,
        FROM_VAR_TEMPLATE, TO_VAR_TEMPLATE,
    },
    types::{
        ForeignEnumInfo, ForeignInterface, ForeignerClassInfo, ForeignerMethod, ItemToExpand,
        MethodAccess, MethodVariant, SelfTypeDesc,
    },
    CppConfig, CppOptional, CppStrView, CppVariant, LanguageGenerator, SourceCode, TypeMap,
};

#[derive(Debug)]
struct CppConverter {
    typename: SmolStr,
    converter: String,
}

#[derive(Debug)]
struct CppForeignTypeInfo {
    base: ForeignTypeInfo,
    provides_by_module: Vec<SmolStr>,
    pub(in crate::cpp) cpp_converter: Option<CppConverter>,
}

impl ForeignTypeInfoT for CppForeignTypeInfo {
    fn name(&self) -> &str {
        self.base.name.as_str()
    }
    fn correspoding_rust_type(&self) -> &RustType {
        &self.base.correspoding_rust_type
    }
}

impl CppForeignTypeInfo {
    pub(in crate::cpp) fn try_new(
        tmap: &mut TypeMap,
        direction: petgraph::Direction,
        ftype_idx: ForeignType,
    ) -> Result<Self> {
        let ftype = &tmap[ftype_idx];
        let mut cpp_converter = None;

        let rule = match direction {
            petgraph::Direction::Outgoing => ftype.into_from_rust.as_ref(),
            petgraph::Direction::Incoming => ftype.from_into_rust.as_ref(),
        }
        .ok_or_else(|| {
            DiagnosticError::new2(
                ftype.src_id_span(),
                format!(
                    "No rule to convert foreign type {} as input/output type",
                    ftype.name
                ),
            )
        })?;
        let provides_by_module = ftype.provides_by_module.clone();
        let base_rt;
        let base_ft_name;
        if let Some(intermediate) = rule.intermediate.as_ref() {
            base_rt = intermediate.intermediate_ty;
            let typename = ftype.typename();
            let converter = intermediate.conv_code.to_string();
            let inter_ft = convert_rt_to_ft(tmap, intermediate.intermediate_ty)?;
            base_ft_name = tmap[inter_ft].typename();
            cpp_converter = Some(CppConverter {
                typename,
                converter,
            });
        } else {
            base_rt = rule.rust_ty;
            base_ft_name = ftype.typename();
        }
        trace!(
            "CppForeignTypeInfo::try_new base_ft_name {}, cpp_converter {:?}",
            base_ft_name,
            cpp_converter
        );
        Ok(CppForeignTypeInfo {
            base: ForeignTypeInfo {
                name: base_ft_name,
                correspoding_rust_type: tmap[base_rt].clone(),
            },
            provides_by_module,
            cpp_converter,
        })
    }

    fn add_provides_by_module<'a, S>(&'a mut self, mod_name: S)
    where
        for<'b> S: Into<SmolStr> + PartialEq<&'b str>,
    {
        if !self
            .provides_by_module
            .iter()
            .any(|x| mod_name == x.as_str())
        {
            self.provides_by_module.push(mod_name.into());
        }
    }
}

impl AsRef<ForeignTypeInfo> for CppForeignTypeInfo {
    fn as_ref(&self) -> &ForeignTypeInfo {
        &self.base
    }
}

struct CppForeignMethodSignature {
    output: CppForeignTypeInfo,
    input: Vec<CppForeignTypeInfo>,
}

impl From<ForeignTypeInfo> for CppForeignTypeInfo {
    fn from(x: ForeignTypeInfo) -> Self {
        CppForeignTypeInfo {
            base: ForeignTypeInfo {
                name: x.name,
                correspoding_rust_type: x.correspoding_rust_type,
            },
            provides_by_module: Vec::new(),
            cpp_converter: None,
        }
    }
}

impl ForeignMethodSignature for CppForeignMethodSignature {
    type FI = CppForeignTypeInfo;
    fn output(&self) -> &ForeignTypeInfoT {
        &self.output.base
    }
    fn input(&self) -> &[CppForeignTypeInfo] {
        &self.input[..]
    }
}

struct MethodContext<'a> {
    class: &'a ForeignerClassInfo,
    method: &'a ForeignerMethod,
    f_method: &'a CppForeignMethodSignature,
    c_func_name: &'a str,
    decl_func_args: &'a str,
    args_names: &'a str,
    real_output_typename: &'a str,
}

impl CppConfig {
    fn register_class(&self, conv_map: &mut TypeMap, class: &ForeignerClassInfo) -> Result<()> {
        class
            .validate_class()
            .map_err(|err| DiagnosticError::new(class.src_id, class.span(), err))?;
        if let Some(self_desc) = class.self_desc.as_ref() {
            let constructor_ret_type = &self_desc.constructor_ret_type;
            let this_type_for_method = constructor_ret_type;
            let mut traits = vec!["SwigForeignClass"];
            if class.clone_derived {
                traits.push("Copy");
            }
            if class.copy_derived {
                if !class.clone_derived {
                    traits.push("Clone");
                }
                traits.push("Copy");
            }
            let this_type = conv_map.find_or_alloc_rust_type_that_implements(
                this_type_for_method,
                &traits,
                class.src_id,
            );

            register_typemap_for_self_type(conv_map, class, this_type, self_desc)?;
        }
        conv_map.find_or_alloc_rust_type(&class.self_type_as_ty(), class.src_id);
        Ok(())
    }
}

struct CppContext<'a> {
    cfg: &'a CppConfig,
    conv_map: &'a mut TypeMap,
    target_pointer_width: usize,
    rust_code: &'a mut Vec<TokenStream>,
    common_files: &'a mut FxHashMap<SmolStr, FileWriteCache>,
}

impl LanguageGenerator for CppConfig {
    fn expand_items(
        &self,
        conv_map: &mut TypeMap,
        target_pointer_width: usize,
        code: &[SourceCode],
        items: Vec<ItemToExpand>,
    ) -> Result<Vec<TokenStream>> {
        let mut ret = Vec::with_capacity(items.len());
        let mut files = FxHashMap::<SmolStr, FileWriteCache>::default();

        {
            let mut ctx = CppContext {
                cfg: self,
                conv_map,
                target_pointer_width,
                rust_code: &mut ret,
                common_files: &mut files,
            };
            init(&mut ctx, code)?;
            for item in &items {
                if let ItemToExpand::Class(ref fclass) = item {
                    self.register_class(ctx.conv_map, fclass)?;
                }
            }
            for item in items {
                match item {
                    ItemToExpand::Class(fclass) => generate(&mut ctx, &fclass)?,

                    ItemToExpand::Enum(fenum) => generate_enum(&mut ctx, &fenum)?,
                    ItemToExpand::Interface(finterface) => {
                        generate_interface(&mut ctx, &finterface)?
                    }
                }
            }
        }

        for (module_name, c_header_f) in files {
            let c_header_path = self.output_dir.join(module_name.as_str());
            c_header_f.update_file_if_necessary().map_err(|err| {
                map_any_err_to_our_err(format!(
                    "write to {} failed: {}",
                    c_header_path.display(),
                    err
                ))
            })?;
        }

        Ok(ret)
    }
}

fn c_func_name(class: &ForeignerClassInfo, method: &ForeignerMethod) -> String {
    format!(
        "{access}{class_name}_{func}",
        access = match method.access {
            MethodAccess::Private => "private_",
            MethodAccess::Protected => "protected_",
            MethodAccess::Public => "",
        },
        class_name = class.name,
        func = method.short_name(),
    )
}

fn rust_generate_args_with_types(
    f_method: &CppForeignMethodSignature,
) -> std::result::Result<String, String> {
    use std::fmt::Write;

    let mut buf = String::new();
    for (i, f_type_info) in f_method.input.iter().enumerate() {
        write!(
            &mut buf,
            "a_{}: {}, ",
            i,
            f_type_info.as_ref().correspoding_rust_type.typename(),
        )
        .map_err(fmt_write_err_map)?;
    }
    Ok(buf)
}

fn fmt_write_err_map(err: fmt::Error) -> String {
    format!("fmt write error: {}", err)
}

fn map_write_err<Err: fmt::Display>(err: Err) -> String {
    format!("write failed: {}", err)
}

fn map_any_err_to_our_err<E: fmt::Display>(err: E) -> DiagnosticError {
    DiagnosticError::new_without_src_info(err)
}

fn n_arguments_list(n: usize) -> String {
    (0..n)
        .map(|v| format!("a_{}", v))
        .fold(String::new(), |mut acc, x| {
            if !acc.is_empty() {
                acc.push_str(", ");
            }
            acc.push_str(&x);
            acc
        })
}

fn convert_rt_to_ft(tmap: &mut TypeMap, rt: RustTypeIdx) -> Result<ForeignType> {
    let rtype = tmap[rt].clone();
    tmap.map_through_conversation_to_foreign(
        &rtype,
        Direction::Outgoing,
        MapToForeignFlag::FullSearch,
        rtype.src_id_span(),
        self::map_type::calc_this_type_for_method,
    )
    .ok_or_else(|| {
        DiagnosticError::new(
            rtype.src_id,
            rtype.ty.span(),
            format!(
                "Do not know conversation from \
                 such rust type '{}' to foreign",
                rtype
            ),
        )
    })
}

fn register_c_type(
    tmap: &mut TypeMap,
    c_types: &CTypes,
    fcode: &FileWriteCache,
    src_id: SourceId,
) -> Result<bool> {
    let mut something_defined = false;
    for c_type in &c_types.types {
        let (f_ident, c_name) = match c_type {
            CType::Struct(ref s) => (&s.ident, format!("struct {}", s.ident)),
            CType::Union(ref u) => (&u.ident, format!("union {}", u.ident)),
        };
        if fcode.is_item_defined(&c_name) {
            continue;
        }
        something_defined = true;
        let rust_ty = parse_ty_with_given_span(&f_ident.to_string(), f_ident.span())
            .map_err(|err| DiagnosticError::from_syn_err(src_id, err))?;
        let rust_ty = tmap.find_or_alloc_rust_type(&rust_ty, src_id);
        debug!("init::c_types add {} / {}", rust_ty, c_name);
        if let Some(ftype_idx) = tmap.find_foreign_type_related_to_rust_ty(rust_ty.to_idx()) {
            if tmap[ftype_idx].name.as_str() != c_name {
                return Err(DiagnosticError::new(
                    src_id,
                    f_ident.span(),
                    format!(
                        "There is already exists foreign type related to rust type '{}', \
                         but name is different: should be {}, have {}",
                        rust_ty,
                        c_name,
                        tmap[ftype_idx].name.as_str()
                    ),
                ));
            }
        } else {
            let rule = ForeignConversationRule {
                rust_ty: rust_ty.to_idx(),
                intermediate: None,
            };
            tmap.alloc_foreign_type(ForeignTypeS {
                name: TypeName::new(c_name, (src_id, f_ident.span())),
                provides_by_module: vec![format!("\"{}\"", c_types.header_name).into()],
                into_from_rust: Some(rule.clone()),
                from_into_rust: Some(rule),
                name_prefix: None,
            })?;
        }
    }
    Ok(something_defined)
}

fn register_typemap_for_self_type(
    conv_map: &mut TypeMap,
    class: &ForeignerClassInfo,
    this_type: RustType,
    self_desc: &SelfTypeDesc,
) -> Result<()> {
    let void_ptr_ty =
        parse_ty_with_given_span_checked("*mut ::std::os::raw::c_void", this_type.ty.span());
    let void_ptr_rust_ty = conv_map.find_or_alloc_rust_type_with_suffix(
        &void_ptr_ty,
        &this_type.normalized_name,
        SourceId::none(),
    );

    let const_void_ptr_ty =
        parse_ty_with_given_span_checked("*const ::std::os::raw::c_void", this_type.ty.span());
    let const_void_ptr_rust_ty = conv_map.find_or_alloc_rust_type_with_suffix(
        &const_void_ptr_ty,
        &this_type.normalized_name,
        SourceId::none(),
    );

    let this_type_inner = boxed_type(conv_map, &this_type);

    let code = format!("& {}", this_type_inner);
    let gen_ty = parse_ty_with_given_span_checked(&code, this_type_inner.ty.span());
    let this_type_ref = conv_map.find_or_alloc_rust_type(&gen_ty, class.src_id);

    let code = format!("&mut {}", this_type_inner);
    let gen_ty = parse_ty_with_given_span_checked(&code, this_type_inner.ty.span());
    let this_type_mut_ref = conv_map.find_or_alloc_rust_type(&gen_ty, class.src_id);

    register_intermidiate_pointer_types(
        conv_map,
        class,
        void_ptr_rust_ty.to_idx(),
        const_void_ptr_rust_ty.to_idx(),
    )?;
    register_rust_ty_conversation_rules(
        conv_map,
        class,
        this_type.clone(),
        this_type_inner.to_idx(),
        void_ptr_rust_ty.to_idx(),
        const_void_ptr_rust_ty.to_idx(),
        this_type_ref.to_idx(),
        this_type_mut_ref.to_idx(),
    )?;

    let self_type = conv_map.find_or_alloc_rust_type(&self_desc.self_type, class.src_id);

    register_main_foreign_types(
        conv_map,
        class,
        this_type.to_idx(),
        self_type.to_idx(),
        void_ptr_rust_ty.to_idx(),
        const_void_ptr_rust_ty.to_idx(),
        this_type_ref.to_idx(),
        this_type_mut_ref.to_idx(),
    )?;
    Ok(())
}

fn register_intermidiate_pointer_types(
    conv_map: &mut TypeMap,
    class: &ForeignerClassInfo,
    void_ptr_rust_ty: RustTypeIdx,
    const_void_ptr_rust_ty: RustTypeIdx,
) -> Result<()> {
    let c_ftype = ForeignTypeS {
        name: TypeName::new(
            format!("{} *", cpp_code::c_class_type(class)),
            (class.src_id, class.name.span()),
        ),
        provides_by_module: vec![format!("\"{}\"", cpp_code::c_header_name(class)).into()],
        into_from_rust: Some(ForeignConversationRule {
            rust_ty: void_ptr_rust_ty,
            intermediate: None,
        }),
        from_into_rust: Some(ForeignConversationRule {
            rust_ty: void_ptr_rust_ty,
            intermediate: None,
        }),
        name_prefix: None,
    };
    conv_map.alloc_foreign_type(c_ftype)?;

    let c_const_ftype = ForeignTypeS {
        name: TypeName::new(
            format!("const {} *", cpp_code::c_class_type(class)),
            (class.src_id, class.name.span()),
        ),
        provides_by_module: vec![format!("\"{}\"", cpp_code::c_header_name(class)).into()],
        into_from_rust: Some(ForeignConversationRule {
            rust_ty: const_void_ptr_rust_ty,
            intermediate: None,
        }),
        from_into_rust: Some(ForeignConversationRule {
            rust_ty: const_void_ptr_rust_ty,
            intermediate: None,
        }),
        name_prefix: None,
    };
    conv_map.alloc_foreign_type(c_const_ftype)?;
    Ok(())
}

fn register_rust_ty_conversation_rules(
    conv_map: &mut TypeMap,
    class: &ForeignerClassInfo,
    this_type: RustType,
    this_type_inner: RustTypeIdx,
    void_ptr_rust_ty: RustTypeIdx,
    const_void_ptr_rust_ty: RustTypeIdx,
    this_type_ref: RustTypeIdx,
    this_type_mut_ref: RustTypeIdx,
) -> Result<()> {
    // *const c_void -> &"class"
    conv_map.add_conversation_rule(
        const_void_ptr_rust_ty,
        this_type_ref,
        format!(
            r#"
    assert!(!{from_var}.is_null());
    let {to_var}: {this_type_ref} = unsafe {{ &*({from_var} as *const {this_type_inner}) }};
"#,
            to_var = TO_VAR_TEMPLATE,
            from_var = FROM_VAR_TEMPLATE,
            this_type_ref = conv_map[this_type_ref],
            this_type_inner = conv_map[this_type_inner],
        )
        .into(),
    );

    // *mut c_void -> &mut "class"
    conv_map.add_conversation_rule(
        void_ptr_rust_ty,
        this_type_mut_ref,
        format!(
            r#"
    assert!(!{from_var}.is_null());
    let {to_var}: {this_type_mut_ref} = unsafe {{ &mut *({from_var} as *mut {this_type_inner}) }};
"#,
            to_var = TO_VAR_TEMPLATE,
            from_var = FROM_VAR_TEMPLATE,
            this_type_mut_ref = conv_map[this_type_mut_ref],
            this_type_inner = conv_map[this_type_inner],
        )
        .into(),
    );

    // *const c_void -> "class", two steps to make it more expensive
    // for type graph path search
    let code = format!("*mut {}", conv_map[this_type_inner]);
    let gen_ty = parse_ty_with_given_span_checked(&code, conv_map[this_type_inner].ty.span());
    let this_type_mut_ptr = conv_map.find_or_alloc_rust_type(&gen_ty, class.src_id);

    conv_map.add_conversation_rule(
        void_ptr_rust_ty,
        this_type_mut_ptr.to_idx(),
        format!(
            r#"
            assert!(!{from_var}.is_null());
            let {to_var}: {this_type_mut_ptr} = {from_var} as {this_type_mut_ptr};
        "#,
            to_var = TO_VAR_TEMPLATE,
            from_var = FROM_VAR_TEMPLATE,
            this_type_mut_ptr = this_type_mut_ptr,
        )
        .into(),
    );

    let unpack_code = unpack_from_heap_pointer(&this_type, TO_VAR_TEMPLATE, true);
    conv_map.add_conversation_rule(
        this_type_mut_ptr.to_idx(),
        this_type.to_idx(),
        format!("\n{}\n", unpack_code,).into(),
    );

    //"class" -> *mut void
    conv_map.add_conversation_rule(
        this_type.to_idx(),
        void_ptr_rust_ty,
        format!(
            "let {to_var}: {ptr_type} = <{this_type}>::box_object({from_var});",
            to_var = TO_VAR_TEMPLATE,
            ptr_type = conv_map[void_ptr_rust_ty].typename(),
            this_type = this_type,
            from_var = FROM_VAR_TEMPLATE
        )
        .into(),
    );

    //&"class" -> *const void
    conv_map.add_conversation_rule(
        this_type_ref,
        const_void_ptr_rust_ty,
        format!(
            "let {to_var}: {ptr_type} = ({from_var} as *const {this_type}) as {ptr_type};",
            to_var = TO_VAR_TEMPLATE,
            ptr_type = conv_map[const_void_ptr_rust_ty].typename(),
            this_type = conv_map[this_type_inner],
            from_var = FROM_VAR_TEMPLATE,
        )
        .into(),
    );

    Ok(())
}

fn register_main_foreign_types(
    conv_map: &mut TypeMap,
    class: &ForeignerClassInfo,
    this_type: RustTypeIdx,
    self_type: RustTypeIdx,
    void_ptr_rust_ty: RustTypeIdx,
    const_void_ptr_rust_ty: RustTypeIdx,
    this_type_ref: RustTypeIdx,
    this_type_mut_ref: RustTypeIdx,
) -> Result<()> {
    let class_ftype = ForeignTypeS {
        name: TypeName::new(class.name.to_string(), (class.src_id, class.name.span())),
        provides_by_module: vec![format!("\"{}\"", cpp_code::cpp_header_name(class)).into()],
        into_from_rust: Some(ForeignConversationRule {
            rust_ty: this_type,
            intermediate: Some(ForeignConversationIntermediate {
                intermediate_ty: void_ptr_rust_ty,
                conv_code: FTypeConvCode::new(
                    format!(
                        "{class_name}(static_cast<{c_type} *>({var}))",
                        class_name = class.name,
                        c_type = cpp_code::c_class_type(class),
                        var = FROM_VAR_TEMPLATE
                    ),
                    invalid_src_id_span(),
                ),
            }),
        }),
        from_into_rust: Some(ForeignConversationRule {
            rust_ty: this_type,
            intermediate: Some(ForeignConversationIntermediate {
                intermediate_ty: void_ptr_rust_ty,
                conv_code: FTypeConvCode::new(
                    format!("{}.release()", FROM_VAR_TEMPLATE),
                    invalid_src_id_span(),
                ),
            }),
        }),
        name_prefix: None,
    };
    conv_map.alloc_foreign_type(class_ftype)?;

    let class_ftype_ref_in = ForeignTypeS {
        name: TypeName::new(
            format!("const {} &", class.name),
            (class.src_id, class.name.span()),
        ),
        provides_by_module: vec![format!("\"{}\"", cpp_code::cpp_header_name(class)).into()],
        from_into_rust: Some(ForeignConversationRule {
            rust_ty: this_type_ref,
            intermediate: Some(ForeignConversationIntermediate {
                intermediate_ty: const_void_ptr_rust_ty,
                conv_code: FTypeConvCode::new(
                    format!(
                        "static_cast<const {} *>({})",
                        cpp_code::c_class_type(class),
                        FROM_VAR_TEMPLATE
                    ),
                    invalid_src_id_span(),
                ),
            }),
        }),
        into_from_rust: None,
        name_prefix: None,
    };
    conv_map.alloc_foreign_type(class_ftype_ref_in)?;

    let class_ftype_ref_out = ForeignTypeS {
        name: TypeName::new(
            format!("{}Ref", class.name),
            (class.src_id, class.name.span()),
        ),
        provides_by_module: vec![format!("\"{}\"", cpp_code::cpp_header_name(class)).into()],
        into_from_rust: Some(ForeignConversationRule {
            rust_ty: this_type_ref,
            intermediate: Some(ForeignConversationIntermediate {
                intermediate_ty: const_void_ptr_rust_ty,
                conv_code: FTypeConvCode::new(
                    format!(
                        "{class}Ref{{ static_cast<const {c_type} *>({var}) }}",
                        class = class.name,
                        c_type = cpp_code::c_class_type(class),
                        var = FROM_VAR_TEMPLATE
                    ),
                    invalid_src_id_span(),
                ),
            }),
        }),
        from_into_rust: None,
        name_prefix: None,
    };
    conv_map.alloc_foreign_type(class_ftype_ref_out)?;

    let class_ftype_mut_ref_in = ForeignTypeS {
        name: TypeName::new(
            format!("{} &", class.name),
            (class.src_id, class.name.span()),
        ),
        provides_by_module: vec![format!("\"{}\"", cpp_code::cpp_header_name(class)).into()],
        from_into_rust: Some(ForeignConversationRule {
            rust_ty: this_type_mut_ref,
            intermediate: Some(ForeignConversationIntermediate {
                intermediate_ty: void_ptr_rust_ty,
                conv_code: FTypeConvCode::new(
                    format!(
                        "static_cast<{} *>({})",
                        cpp_code::c_class_type(class),
                        FROM_VAR_TEMPLATE
                    ),
                    invalid_src_id_span(),
                ),
            }),
        }),
        into_from_rust: None,
        name_prefix: None,
    };
    conv_map.alloc_foreign_type(class_ftype_mut_ref_in)?;

    if self_type != this_type {
        let self_type = conv_map[self_type].clone();
        {
            let code = format!("&mut {}", self_type);
            let gen_ty = parse_ty_with_given_span_checked(&code, self_type.ty.span());
            let self_type_mut_ref = conv_map.find_or_alloc_rust_type(&gen_ty, class.src_id);

            let class_ftype_mut_ref_in = ForeignTypeS {
                name: TypeName::new(
                    format!("/**/{} &", class.name),
                    (class.src_id, class.name.span()),
                ),
                provides_by_module: vec![format!("\"{}\"", cpp_code::cpp_header_name(class)).into()],
                from_into_rust: Some(ForeignConversationRule {
                    rust_ty: self_type_mut_ref.to_idx(),
                    intermediate: Some(ForeignConversationIntermediate {
                        intermediate_ty: void_ptr_rust_ty,
                        conv_code: FTypeConvCode::new(
                            format!(
                                "static_cast<{} *>({})",
                                cpp_code::c_class_type(class),
                                FROM_VAR_TEMPLATE
                            ),
                            invalid_src_id_span(),
                        ),
                    }),
                }),
                into_from_rust: None,
                name_prefix: Some("/**/"),
            };
            conv_map.alloc_foreign_type(class_ftype_mut_ref_in)?;
        }
        {
            let code = format!("& {}", self_type);
            let gen_ty = parse_ty_with_given_span_checked(&code, self_type.ty.span());
            let self_type_ref = conv_map.find_or_alloc_rust_type(&gen_ty, class.src_id);

            let class_ftype_ref_in = ForeignTypeS {
                name: TypeName::new(
                    format!("/**/const {} &", class.name),
                    (class.src_id, class.name.span()),
                ),
                provides_by_module: vec![format!("\"{}\"", cpp_code::cpp_header_name(class)).into()],
                from_into_rust: Some(ForeignConversationRule {
                    rust_ty: self_type_ref.to_idx(),
                    intermediate: Some(ForeignConversationIntermediate {
                        intermediate_ty: const_void_ptr_rust_ty,
                        conv_code: FTypeConvCode::new(
                            format!(
                                "static_cast<const {} *>({})",
                                cpp_code::c_class_type(class),
                                FROM_VAR_TEMPLATE
                            ),
                            invalid_src_id_span(),
                        ),
                    }),
                }),
                into_from_rust: None,
                name_prefix: Some("/**/"),
            };
            conv_map.alloc_foreign_type(class_ftype_ref_in)?;
        }
    }

    Ok(())
}

fn merge_rule(ctx: &mut CppContext, mut rule: TypeMapConvRuleInfo) -> Result<()> {
    let all_options = {
        let mut opts = FxHashSet::<&'static str>::default();
        opts.extend(CppOptional::iter().map(|x| -> &'static str { x.into() }));
        opts.extend(CppVariant::iter().map(|x| -> &'static str { x.into() }));
        opts.extend(CppStrView::iter().map(|x| -> &'static str { x.into() }));
        opts
    };

    validate_cfg_options(&rule, &all_options)?;
    let options = {
        let mut opts = FxHashSet::<&'static str>::default();
        opts.insert(ctx.cfg.cpp_variant.into());
        opts.insert(ctx.cfg.cpp_optional.into());
        opts.insert(ctx.cfg.cpp_str_view.into());
        opts
    };

    if let Some(c_types) = rule.c_types.take() {
        merge_c_types(ctx, c_types, MergeCTypesFlags::DefineOnlyCType, rule.src_id)?;
    }

    let f_codes = mem::replace(&mut rule.f_code, vec![]);
    for fcode in f_codes {
        let module_name = &fcode.module_name;
        let common_files = &mut ctx.common_files;
        let c_header_f = file_for_module!(ctx, common_files, module_name);
        let use_fcode = fcode
            .cfg_option
            .as_ref()
            .map(|opt| options.contains(opt.as_str()))
            .unwrap_or(true);

        if use_fcode {
            c_header_f
                .write_all(
                    fcode
                        .code
                        .replace("$RUST_SWIG_USER_NAMESPACE", &ctx.cfg.namespace_name)
                        .as_bytes(),
                )
                .map_err(map_any_err_to_our_err)?;
        }
    }

    macro_rules! configure_ftype_rule {
        ($f_type_rules:ident, $rule_type:tt) => {{
            $f_type_rules.retain(|rule| {
                rule.cfg_option
                    .as_ref()
                    .map(|opt| options.contains(opt.as_str()))
                    .unwrap_or(true)
            });
            if $f_type_rules.len() > 1 {
                let first_rule = $f_type_rules.remove(0);
                let mut err = DiagnosticError::new(
                    rule.src_id,
                    first_rule.left_right_ty.span(),
                    concat!(
                        "multiply f_type '",
                        stringify!($rule_type),
                        "' rules, that possible to use in this configuration, first"
                    ),
                );
                for other in $f_type_rules.iter() {
                    err.span_note(
                        (rule.src_id, other.left_right_ty.span()),
                        concat!("other f_type '", stringify!($rule_type), "' rule"),
                    );
                }
                return Err(err);
            }
            if $f_type_rules.len() == 1 {
                $f_type_rules[0].cfg_option = None;
            }
        }};
    }

    let ftype_left_to_right = &mut rule.ftype_left_to_right;
    configure_ftype_rule!(ftype_left_to_right, =>);

    let ftype_right_to_left = &mut rule.ftype_right_to_left;
    configure_ftype_rule!(ftype_right_to_left, <=);

    ctx.conv_map.merge_conv_rule(rule.src_id, rule)?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum MergeCTypesFlags {
    DefineAlsoRustType,
    DefineOnlyCType,
}

fn merge_c_types(
    ctx: &mut CppContext,
    c_types: CTypes,
    flags: MergeCTypesFlags,
    rule_src_id: SourceId,
) -> Result<()> {
    let module_name = &c_types.header_name;
    let common_files = &mut ctx.common_files;
    let mut c_header_f = file_for_module!(ctx, common_files, module_name);
    register_c_type(ctx.conv_map, &c_types, c_header_f, rule_src_id)?;

    ctx.rust_code.append(&mut cpp_code::generate_c_type(
        ctx.conv_map,
        &c_types,
        flags,
        rule_src_id,
        &mut c_header_f,
    )?);

    Ok(())
}

fn init(ctx: &mut CppContext, code: &[SourceCode]) -> Result<()> {
    //for enum
    ctx.conv_map
        .find_or_alloc_rust_type_no_src_id(&parse_type! { u32 });

    for cu in code {
        let src_path = ctx.cfg.output_dir.join(&cu.id_of_code);
        let mut src_file = FileWriteCache::new(&src_path);
        src_file
            .write_all(
                cu.code
                    .replace("RUST_SWIG_USER_NAMESPACE", &ctx.cfg.namespace_name)
                    .as_bytes(),
            )
            .map_err(|err| {
                map_any_err_to_our_err(format!("write to {} failed: {}", src_path.display(), err))
            })?;
        src_file.update_file_if_necessary().map_err(|err| {
            map_any_err_to_our_err(format!("update of {} failed: {}", src_path.display(), err))
        })?;
    }

    let not_merged_data = ctx.conv_map.take_not_merged_not_generic_rules();
    for rule in not_merged_data {
        merge_rule(ctx, rule)?;
    }

    Ok(())
}

fn generate(ctx: &mut CppContext, class: &ForeignerClassInfo) -> Result<()> {
    debug!(
        "generate: begin for {}, this_type_for_method {:?}",
        class.name, class.self_desc
    );
    let has_methods = class.methods.iter().any(|m| match m.variant {
        MethodVariant::Method(_) => true,
        _ => false,
    });
    let has_constructor = class
        .methods
        .iter()
        .any(|m| m.variant == MethodVariant::Constructor);

    if has_methods && !has_constructor {
        return Err(DiagnosticError::new(
            class.src_id,
            class.span(),
            format!(
                "namespace {}, class {}: has methods, but no constructor\n
May be you need to use `private constructor = empty;` syntax?",
                ctx.cfg.namespace_name, class.name
            ),
        ));
    }

    let mut m_sigs = fclass::find_suitable_foreign_types_for_methods(ctx, class)?;
    let mut req_includes = cpp_code::cpp_list_required_includes(&mut m_sigs);
    let my_self = format!("\"{}\"", cpp_code::cpp_header_name(class));
    req_includes.retain(|el| *el != my_self);
    fclass::generate(ctx, class, &req_includes, &m_sigs)?;
    Ok(())
}

fn generate_enum(ctx: &mut CppContext, fenum: &ForeignEnumInfo) -> Result<()> {
    if (fenum.items.len() as u64) >= u64::from(u32::max_value()) {
        return Err(DiagnosticError::new(
            fenum.src_id,
            fenum.span(),
            "Too many items in enum",
        ));
    }

    trace!("enum_ti: {}", fenum.name);
    let enum_name = &fenum.name;
    let enum_ti: Type = parse_ty_with_given_span(&enum_name.to_string(), fenum.name.span())
        .map_err(|err| DiagnosticError::from_syn_err(fenum.src_id, err))?;
    let enum_rty = ctx.conv_map.find_or_alloc_rust_type_that_implements(
        &enum_ti,
        &["SwigForeignEnum"],
        fenum.src_id,
    );

    fenum::generate_c_code_for_enum(&ctx.cfg.output_dir, fenum)
        .map_err(|err| DiagnosticError::new(fenum.src_id, fenum.span(), err))?;
    fenum::generate_rust_trait_for_enum(ctx, fenum)?;

    let u32_rty = ctx
        .conv_map
        .find_or_alloc_rust_type_no_src_id(&parse_type! { u32 });

    let enum_ftype = ForeignTypeS {
        name: TypeName::new(fenum.name.to_string(), (fenum.src_id, fenum.name.span())),
        provides_by_module: vec![
            format!("\"{}\"", cpp_code::cpp_header_name_for_enum(fenum)).into()
        ],
        into_from_rust: Some(ForeignConversationRule {
            rust_ty: enum_rty.to_idx(),
            intermediate: Some(ForeignConversationIntermediate {
                intermediate_ty: u32_rty.to_idx(),
                conv_code: FTypeConvCode::new(
                    format!(
                        "static_cast<{enum_name}>({var})",
                        enum_name = fenum.name,
                        var = FROM_VAR_TEMPLATE
                    ),
                    invalid_src_id_span(),
                ),
            }),
        }),
        from_into_rust: Some(ForeignConversationRule {
            rust_ty: enum_rty.to_idx(),
            intermediate: Some(ForeignConversationIntermediate {
                intermediate_ty: u32_rty.to_idx(),
                conv_code: FTypeConvCode::new(
                    format!("static_cast<uint32_t>({})", FROM_VAR_TEMPLATE),
                    invalid_src_id_span(),
                ),
            }),
        }),
        name_prefix: None,
    };
    ctx.conv_map.alloc_foreign_type(enum_ftype)?;
    ctx.conv_map.register_exported_enum(fenum);
    Ok(())
}

fn generate_interface(ctx: &mut CppContext, interface: &ForeignInterface) -> Result<()> {
    let mut f_methods = finterface::find_suitable_ftypes_for_interace_methods(ctx, interface)?;
    let req_includes = cpp_code::cpp_list_required_includes(&mut f_methods);
    finterface::generate_for_interface(ctx, interface, &req_includes, &f_methods)
        .map_err(|err| DiagnosticError::new(interface.src_id, interface.span(), err))?;
    finterface::rust_code_generate_interface(ctx, interface, &f_methods)?;

    let c_struct_name = format!("C_{}", interface.name);
    let rust_struct_pointer = format!("*const {}", c_struct_name);
    let rust_ty: Type = parse_ty_with_given_span(&rust_struct_pointer, interface.name.span())
        .map_err(|err| DiagnosticError::from_syn_err(interface.src_id, err))?;
    let c_struct_pointer = format!("const struct {} * const", c_struct_name);

    let rust_ty = ctx.conv_map.find_or_alloc_rust_type_no_src_id(&rust_ty);

    ctx.conv_map.add_foreign(
        rust_ty,
        TypeName::new(c_struct_pointer, interface.src_id_span()),
    )?;

    Ok(())
}
