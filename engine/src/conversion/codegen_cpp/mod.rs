// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod function_wrapper_cpp;
pub(crate) mod type_to_cpp;

use crate::{
    conversion::analysis::fun::{function_wrapper::CppFunctionKind, FnAnalysis},
    types::{make_ident, QualifiedName},
    CppFilePair,
};
use autocxx_parser::IncludeCppConfig;
use itertools::Itertools;
use std::collections::{HashMap, HashSet};
use type_to_cpp::{original_name_map_from_apis, type_to_cpp, CppNameMap};

use super::{
    analysis::fun::{
        function_wrapper::{CppFunction, CppFunctionBody},
        FnPhase,
    },
    api::{Api, SubclassName},
    ConvertError,
};

#[derive(Ord, PartialOrd, Eq, PartialEq, Clone, Hash)]
struct Header {
    name: &'static str,
    system: bool,
}

impl Header {
    fn system(name: &'static str) -> Self {
        Header { name, system: true }
    }

    fn user(name: &'static str) -> Self {
        Header {
            name,
            system: false,
        }
    }

    fn include_stmt(&self) -> String {
        if self.system {
            format!("#include <{}>", self.name)
        } else {
            format!("#include \"{}\"", self.name)
        }
    }

    fn is_system(&self) -> bool {
        self.system
    }
}

enum ConversionDirection {
    RustCallsCpp,
    CppCallsCpp,
    CppCallsRust,
}

struct AdditionalFunction {
    type_definition: Option<String>, // are output before main declarations
    declaration: Option<String>,
    definition: Option<String>,
    headers: Vec<Header>,
    cpp_headers: Vec<Header>,
}

/// Generates additional C++ glue functions needed by autocxx.
/// In some ways it would be preferable to be able to pass snippets
/// of C++ through to `cxx` for inclusion in the C++ file which it
/// generates, and perhaps we'll explore that in future. But for now,
/// autocxx generates its own _additional_ C++ files which therefore
/// need to be built and included in linking procedures.
pub(crate) struct CppCodeGenerator<'a> {
    additional_functions: Vec<AdditionalFunction>,
    inclusions: String,
    original_name_map: CppNameMap,
    config: &'a IncludeCppConfig,
    suppress_system_headers: bool,
}

impl<'a> CppCodeGenerator<'a> {
    pub(crate) fn generate_cpp_code(
        inclusions: String,
        apis: &[Api<FnPhase>],
        config: &'a IncludeCppConfig,
        suppress_system_headers: bool,
    ) -> Result<Option<CppFilePair>, ConvertError> {
        let mut gen = CppCodeGenerator::new(
            inclusions,
            original_name_map_from_apis(apis),
            config,
            suppress_system_headers,
        );
        // The 'filter' on the following line is designed to ensure we don't accidentally
        // end up out of sync with needs_cpp_codegen
        gen.add_needs(apis.iter().filter(|api| api.needs_cpp_codegen()))?;
        Ok(gen.generate())
    }

    fn new(
        inclusions: String,
        original_name_map: CppNameMap,
        config: &'a IncludeCppConfig,
        suppress_system_headers: bool,
    ) -> Self {
        CppCodeGenerator {
            additional_functions: Vec::new(),
            inclusions,
            original_name_map,
            config,
            suppress_system_headers,
        }
    }

    // It's important to keep this in sync with Api::needs_cpp_codegen.
    fn add_needs<'b>(
        &mut self,
        apis: impl Iterator<Item = &'a Api<FnPhase>>,
    ) -> Result<(), ConvertError> {
        let mut constructors_by_subclass: HashMap<SubclassName, Vec<&CppFunction>> = HashMap::new();
        let mut methods_by_subclass: HashMap<SubclassName, Vec<&CppFunction>> = HashMap::new();
        let mut deferred_apis = Vec::new();
        for api in apis {
            match &api {
                Api::StringConstructor { .. } => self.generate_string_constructor(),
                Api::Function {
                    analysis:
                        FnAnalysis {
                            cpp_wrapper: Some(cpp_wrapper),
                            ..
                        },
                    ..
                } => self.generate_cpp_function(cpp_wrapper)?,
                Api::ConcreteType { rs_definition, .. } => self.generate_typedef(
                    api.name(),
                    type_to_cpp(rs_definition, &self.original_name_map)?,
                ),
                Api::CType { typename, .. } => self.generate_ctype_typedef(typename),
                Api::Subclass { .. } => deferred_apis.push(api),
                Api::RustSubclassFn {
                    subclass, details, ..
                } => {
                    methods_by_subclass
                        .entry(subclass.clone())
                        .or_default()
                        .push(&details.cpp_impl);
                }
                Api::RustSubclassConstructor {
                    cpp_impl, subclass, ..
                } => {
                    constructors_by_subclass
                        .entry(subclass.clone())
                        .or_default()
                        .push(cpp_impl);
                }
                _ => panic!("Should have filtered on needs_cpp_codegen"),
            }
        }

        for api in deferred_apis.into_iter() {
            match api {
                Api::Subclass { name, superclass } => self.generate_subclass(
                    superclass,
                    name,
                    constructors_by_subclass.remove(name).unwrap_or_default(),
                    methods_by_subclass.remove(name).unwrap_or_default(),
                )?,
                _ => panic!("Unexpected deferred API"),
            }
        }
        Ok(())
    }

    fn generate(&self) -> Option<CppFilePair> {
        if self.additional_functions.is_empty() {
            None
        } else {
            let headers = self.collect_headers(|additional_need| &additional_need.headers);
            let cpp_headers = self.collect_headers(|additional_need| &additional_need.cpp_headers);
            let type_definitions = self.concat_additional_items(|x| x.type_definition.as_ref());
            let declarations = self.concat_additional_items(|x| x.declaration.as_ref());
            let declarations = format!(
                "#ifndef __AUTOCXXGEN_H__\n#define __AUTOCXXGEN_H__\n\n{}\n{}\n{}\n{}#endif // __AUTOCXXGEN_H__\n",
                headers, self.inclusions, type_definitions, declarations
            );
            log::info!("Additional C++ decls:\n{}", declarations);
            let header_name = format!("autocxxgen_{}.h", self.config.get_mod_name());
            let implementation = if self
                .additional_functions
                .iter()
                .any(|x| x.definition.is_some())
            {
                let definitions = self.concat_additional_items(|x| x.definition.as_ref());
                let definitions = format!(
                    "#include \"{}\"\n{}\n{}",
                    header_name, cpp_headers, definitions
                );
                log::info!("Additional C++ defs:\n{}", definitions);
                Some(definitions.into_bytes())
            } else {
                None
            };
            Some(CppFilePair {
                header: declarations.into_bytes(),
                implementation,
                header_name,
            })
        }
    }

    fn collect_headers<F>(&self, filter: F) -> String
    where
        F: Fn(&AdditionalFunction) -> &[Header],
    {
        let cpp_headers: HashSet<_> = self
            .additional_functions
            .iter()
            .map(|x| filter(x).iter())
            .flatten()
            .filter(|x| !self.suppress_system_headers || !x.is_system())
            .collect(); // uniqify
        cpp_headers.iter().map(|x| x.include_stmt()).join("\n")
    }

    fn concat_additional_items<F>(&self, field_access: F) -> String
    where
        F: FnMut(&AdditionalFunction) -> Option<&String>,
    {
        let mut s = self
            .additional_functions
            .iter()
            .map(field_access)
            .flatten()
            .join("\n");
        s.push('\n');
        s
    }

    fn generate_string_constructor(&mut self) {
        let makestring_name = self.config.get_makestring_name();
        let declaration = Some(format!("inline std::unique_ptr<std::string> {}(::rust::Str str) {{ return std::make_unique<std::string>(std::string(str)); }}", makestring_name));
        self.additional_functions.push(AdditionalFunction {
            type_definition: None,
            declaration,
            definition: None,
            headers: vec![
                Header::system("memory"),
                Header::system("string"),
                Header::user("cxx.h"),
            ],
            cpp_headers: Vec::new(),
        })
    }

    fn generate_cpp_function(&mut self, details: &CppFunction) -> Result<(), ConvertError> {
        self.additional_functions
            .push(self.generate_cpp_function_inner(
                details,
                false,
                ConversionDirection::RustCallsCpp,
                false,
            )?);
        Ok(())
    }

    fn generate_cpp_function_inner(
        &self,
        details: &CppFunction,
        avoid_this: bool,
        conversion_direction: ConversionDirection,
        requires_rust_declarations: bool,
    ) -> Result<AdditionalFunction, ConvertError> {
        // Even if the original function call is in a namespace,
        // we generate this wrapper in the global namespace.
        // We could easily do this the other way round, and when
        // cxx::bridge comes to support nested namespace mods then
        // we wil wish to do that to avoid name conflicts. However,
        // at the moment this is simpler because it avoids us having
        // to generate namespace blocks in the generated C++.
        let is_a_method = !avoid_this
            && matches!(
                details.kind,
                CppFunctionKind::Method | CppFunctionKind::ConstMethod
            );
        let name = &details.wrapper_function_name;
        let get_arg_name = |counter: usize| -> String {
            if is_a_method && counter == 0 {
                // For method calls that we generate, the first
                // argument name needs to be such that we recognize
                // it as a method in the second invocation of
                // bridge_converter after it's flowed again through
                // bindgen.
                // TODO this may not be the case any longer. We
                // may be able to remove this.
                "autocxx_gen_this".to_string()
            } else {
                format!("arg{}", counter)
            }
        };
        let args: Result<Vec<_>, _> = details
            .argument_conversion
            .iter()
            .enumerate()
            .map(|(counter, ty)| {
                Ok(format!(
                    "{} {}",
                    match conversion_direction {
                        ConversionDirection::RustCallsCpp =>
                            ty.unconverted_type(&self.original_name_map)?,
                        ConversionDirection::CppCallsCpp =>
                            ty.converted_type(&self.original_name_map)?,
                        ConversionDirection::CppCallsRust =>
                            ty.inverse().unconverted_type(&self.original_name_map)?,
                    },
                    get_arg_name(counter)
                ))
            })
            .collect();
        let args = args?.join(", ");
        let default_return = match details.kind {
            CppFunctionKind::Constructor => "",
            _ => "void",
        };
        let ret_type = details
            .return_conversion
            .as_ref()
            .map(|x| match conversion_direction {
                ConversionDirection::RustCallsCpp => x.converted_type(&self.original_name_map),
                ConversionDirection::CppCallsCpp => x.unconverted_type(&self.original_name_map),
                ConversionDirection::CppCallsRust => {
                    x.inverse().converted_type(&self.original_name_map)
                }
            })
            .unwrap_or_else(|| Ok(default_return.to_string()))?;
        let constness = match details.kind {
            CppFunctionKind::ConstMethod => " const",
            _ => "",
        };
        let declaration = format!("{} {}({}){}", ret_type, name, args, constness);
        let qualification = if let Some(qualification) = &details.qualification {
            format!("{}::", qualification.to_cpp_name())
        } else {
            "".to_string()
        };
        let qualified_declaration = format!(
            "{} {}{}({}){}",
            ret_type, qualification, name, args, constness
        );
        let arg_list: Result<Vec<_>, _> = details
            .argument_conversion
            .iter()
            .enumerate()
            .map(|(counter, conv)| match conversion_direction {
                ConversionDirection::RustCallsCpp => {
                    conv.cpp_conversion(&get_arg_name(counter), &self.original_name_map, false)
                }
                ConversionDirection::CppCallsCpp => Ok(get_arg_name(counter)),
                ConversionDirection::CppCallsRust => conv.inverse().cpp_conversion(
                    &get_arg_name(counter),
                    &self.original_name_map,
                    false,
                ),
            })
            .collect();
        let mut arg_list = arg_list?.into_iter();
        let receiver = if is_a_method { arg_list.next() } else { None };
        if matches!(&details.payload, CppFunctionBody::ConstructSuperclass(_)) {
            arg_list.next();
        }
        let arg_list = if details.pass_obs_field {
            std::iter::once("*obs".to_string())
                .chain(arg_list)
                .join(",")
        } else {
            arg_list.join(", ")
        };
        let (mut underlying_function_call, field_assignments) = match &details.payload {
            CppFunctionBody::Constructor => (arg_list, "".to_string()),
            CppFunctionBody::FunctionCall(ns, id) => match receiver {
                Some(receiver) => (
                    format!("{}.{}({})", receiver, id.to_string(), arg_list),
                    "".to_string(),
                ),
                None => {
                    let underlying_function_call = ns
                        .into_iter()
                        .cloned()
                        .chain(std::iter::once(id.to_string()))
                        .join("::");
                    (
                        format!("{}({})", underlying_function_call, arg_list),
                        "".to_string(),
                    )
                }
            },
            CppFunctionBody::StaticMethodCall(ns, ty_id, fn_id) => {
                let underlying_function_call = ns
                    .into_iter()
                    .cloned()
                    .chain([ty_id.to_string(), fn_id.to_string()].iter().cloned())
                    .join("::");
                (
                    format!("{}({})", underlying_function_call, arg_list),
                    "".to_string(),
                )
            }
            CppFunctionBody::ConstructSuperclass(_) => ("".to_string(), arg_list),
        };
        if let Some(ret) = &details.return_conversion {
            underlying_function_call = format!(
                "return {}",
                match conversion_direction {
                    ConversionDirection::RustCallsCpp => ret.cpp_conversion(
                        &underlying_function_call,
                        &self.original_name_map,
                        true
                    )?,
                    ConversionDirection::CppCallsCpp => underlying_function_call,
                    ConversionDirection::CppCallsRust => ret.inverse().cpp_conversion(
                        &underlying_function_call,
                        &self.original_name_map,
                        true
                    )?,
                }
            );
        };
        if !underlying_function_call.is_empty() {
            underlying_function_call = format!("{};", underlying_function_call);
        }
        let field_assignments =
            if let CppFunctionBody::ConstructSuperclass(superclass_name) = &details.payload {
                let superclass_assignments = if field_assignments.is_empty() {
                    "".to_string()
                } else {
                    format!("{}({}), ", superclass_name, field_assignments)
                };
                format!(": {}obs(std::move(arg0))", superclass_assignments)
            } else {
                "".into()
            };
        let definition_after_sig =
            format!("{} {{ {} }}", field_assignments, underlying_function_call,);
        let (declaration, definition) = if requires_rust_declarations {
            (
                Some(format!("{};", declaration)),
                Some(format!(
                    "{} {}",
                    qualified_declaration, definition_after_sig
                )),
            )
        } else {
            (
                Some(format!("inline {} {}", declaration, definition_after_sig)),
                None,
            )
        };
        Ok(AdditionalFunction {
            type_definition: None,
            declaration,
            definition,
            headers: vec![Header::system("memory")],
            cpp_headers: Vec::new(),
        })
    }

    fn generate_ctype_typedef(&mut self, tn: &QualifiedName) {
        let cpp_name = tn.to_cpp_name();
        self.generate_typedef(tn, cpp_name)
    }

    fn generate_typedef(&mut self, tn: &QualifiedName, definition: String) {
        let our_name = tn.get_final_item();
        self.additional_functions.push(AdditionalFunction {
            type_definition: Some(format!("typedef {} {};", definition, our_name)),
            declaration: None,
            definition: None,
            headers: Vec::new(),
            cpp_headers: Vec::new(),
        })
    }

    fn generate_subclass(
        &mut self,
        superclass: &QualifiedName,
        subclass: &SubclassName,
        constructors: Vec<&CppFunction>,
        methods: Vec<&CppFunction>,
    ) -> Result<(), ConvertError> {
        let holder = subclass.holder();
        self.additional_functions.push(AdditionalFunction {
            type_definition: Some(format!("struct {};", holder.to_string())),
            declaration: None,
            definition: None,
            headers: Vec::new(),
            cpp_headers: Vec::new(),
        });
        let mut method_decls = Vec::new();
        for method in methods {
            // First the method which calls from C++ to Rust
            let mut fn_impl = self.generate_cpp_function_inner(
                method,
                true,
                ConversionDirection::CppCallsRust,
                true,
            )?;
            method_decls.push(fn_impl.declaration.take().unwrap());
            self.additional_functions.push(fn_impl);
            // And now the function to be called from Rust for default implementation (calls superclass in C++)
            let id = make_ident(method.wrapper_function_name.to_string());
            let mut super_method = method.clone();
            super_method.pass_obs_field = false;
            super_method.wrapper_function_name = SubclassName::get_super_fn_name(
                superclass.get_namespace(),
                &method.wrapper_function_name.to_string(),
            )
            .get_final_ident();
            super_method.payload = CppFunctionBody::StaticMethodCall(
                superclass.get_namespace().clone(),
                superclass.get_final_ident(),
                id,
            );
            let mut super_fn_impl = self.generate_cpp_function_inner(
                &super_method,
                true,
                ConversionDirection::CppCallsCpp,
                false,
            )?;
            method_decls.push(super_fn_impl.declaration.take().unwrap());
            self.additional_functions.push(super_fn_impl);
        }
        // In future, for each superclass..
        let super_name = superclass.get_final_item();
        method_decls.push(format!(
            "const {}& As_{}() const {{ return *this; }}",
            super_name, super_name,
        ));
        method_decls.push(format!(
            "{}& As_{}_mut() {{ return *this; }}",
            super_name, super_name
        ));
        // And now constructors
        let mut constructor_decls: Vec<String> = Vec::new();
        for constructor in constructors {
            let mut fn_impl = self.generate_cpp_function_inner(
                constructor,
                false,
                ConversionDirection::CppCallsCpp,
                false,
            )?;
            let decl = fn_impl.declaration.take().unwrap();
            constructor_decls.push(decl);
            self.additional_functions.push(fn_impl);
        }
        self.additional_functions.push(AdditionalFunction {
            type_definition: Some(format!(
                "class {} : {}\n{{\npublic:\n{}\n{}\nvoid {}() const;\nprivate:rust::Box<{}> obs;\nvoid really_remove_ownership();\n\n}};",
                subclass.cpp(),
                superclass.to_cpp_name(),
                constructor_decls.join("\n"),
                method_decls.join("\n"),
                subclass.cpp_remove_ownership(),
                holder
            )),
            definition: Some(format!(
                "void {}::{}() const {{\nconst_cast<{}*>(this)->really_remove_ownership();\n}}\n;void {}::really_remove_ownership() {{\nauto new_obs = {}(std::move(obs));\nobs = std::move(new_obs);\n}}\n",
                subclass.cpp(),
                subclass.cpp_remove_ownership().to_string(),
                subclass.cpp(),
                subclass.cpp(),
                subclass.remove_ownership().to_string()
            )),
            declaration: None,
            headers: Vec::new(),
            cpp_headers: vec![Header::user("cxxgen.h")],
        });
        Ok(())
    }
}
