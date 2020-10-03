//! Provides validators for the item declarations.
//! This includes the following items:
//! - variable bindings (e.g. `let x = foo();`)
//! - struct fields (e.g. `struct Foo { field: u8 }`)
//! - enum fields (e.g. `enum Foo { Variant { field: u8 } }`)
//! - function/method arguments (e.g. `fn foo(arg: u8)`)

// TODO: Temporary, to not see warnings until module is somewhat complete.
// If you see these lines in the pull request, feel free to call me stupid :P.
#![allow(dead_code, unused_imports, unused_variables)]

mod str_helpers;

use std::sync::Arc;

use hir_def::{
    adt::VariantData,
    body::Body,
    db::DefDatabase,
    expr::{Expr, ExprId, UnaryOp},
    item_tree::ItemTreeNode,
    resolver::{resolver_for_expr, ResolveValueResult, ValueNs},
    src::HasSource,
    AdtId, EnumId, FunctionId, Lookup, ModuleDefId, StructId,
};
use hir_expand::{
    diagnostics::DiagnosticSink,
    name::{AsName, Name},
};
use syntax::{
    ast::{self, NameOwner},
    AstPtr,
};

use crate::{
    db::HirDatabase,
    diagnostics::{decl_check::str_helpers::*, CaseType, IncorrectCase},
    lower::CallableDefId,
    ApplicationTy, InferenceResult, Ty, TypeCtor,
};

pub(super) struct DeclValidator<'a, 'b: 'a> {
    owner: ModuleDefId,
    sink: &'a mut DiagnosticSink<'b>,
}

#[derive(Debug)]
struct Replacement {
    current_name: Name,
    suggested_text: String,
    expected_case: CaseType,
}

impl<'a, 'b> DeclValidator<'a, 'b> {
    pub(super) fn new(
        owner: ModuleDefId,
        sink: &'a mut DiagnosticSink<'b>,
    ) -> DeclValidator<'a, 'b> {
        DeclValidator { owner, sink }
    }

    pub(super) fn validate_item(&mut self, db: &dyn HirDatabase) {
        // let def = self.owner.into();
        match self.owner {
            ModuleDefId::FunctionId(func) => self.validate_func(db, func),
            ModuleDefId::AdtId(adt) => self.validate_adt(db, adt),
            _ => return,
        }
    }

    fn validate_func(&mut self, db: &dyn HirDatabase, func: FunctionId) {
        let data = db.function_data(func);

        // 1. Check the function name.
        let function_name = data.name.to_string();
        let fn_name_replacement = if let Some(new_name) = to_lower_snake_case(&function_name) {
            let replacement = Replacement {
                current_name: data.name.clone(),
                suggested_text: new_name,
                expected_case: CaseType::LowerSnakeCase,
            };
            Some(replacement)
        } else {
            None
        };

        // 2. Check the param names.
        let mut fn_param_replacements = Vec::new();

        for param_name in data.param_names.iter().cloned().filter_map(|i| i) {
            let name = param_name.to_string();
            if let Some(new_name) = to_lower_snake_case(&name) {
                let replacement = Replacement {
                    current_name: param_name,
                    suggested_text: new_name,
                    expected_case: CaseType::LowerSnakeCase,
                };
                fn_param_replacements.push(replacement);
            }
        }

        // 3. If there is at least one element to spawn a warning on, go to the source map and generate a warning.
        self.create_incorrect_case_diagnostic_for_func(
            func,
            db,
            fn_name_replacement,
            fn_param_replacements,
        )
    }

    /// Given the information about incorrect names in the function declaration, looks up into the source code
    /// for exact locations and adds diagnostics into the sink.
    fn create_incorrect_case_diagnostic_for_func(
        &mut self,
        func: FunctionId,
        db: &dyn HirDatabase,
        fn_name_replacement: Option<Replacement>,
        fn_param_replacements: Vec<Replacement>,
    ) {
        // XXX: only look at sources if we do have incorrect names
        if fn_name_replacement.is_none() && fn_param_replacements.is_empty() {
            return;
        }

        let fn_loc = func.lookup(db.upcast());
        let fn_src = fn_loc.source(db.upcast());

        if let Some(replacement) = fn_name_replacement {
            let ast_ptr = if let Some(name) = fn_src.value.name() {
                name
            } else {
                // We don't want rust-analyzer to panic over this, but it is definitely some kind of error in the logic.
                log::error!(
                    "Replacement ({:?}) was generated for a function without a name: {:?}",
                    replacement,
                    fn_src
                );
                return;
            };

            let diagnostic = IncorrectCase {
                file: fn_src.file_id,
                ident_type: "Function".to_string(),
                ident: AstPtr::new(&ast_ptr).into(),
                expected_case: replacement.expected_case,
                ident_text: replacement.current_name.to_string(),
                suggested_text: replacement.suggested_text,
            };

            self.sink.push(diagnostic);
        }

        let fn_params_list = match fn_src.value.param_list() {
            Some(params) => params,
            None => {
                if !fn_param_replacements.is_empty() {
                    log::error!(
                        "Replacements ({:?}) were generated for a function parameters which had no parameters list: {:?}",
                        fn_param_replacements, fn_src
                    );
                }
                return;
            }
        };
        let mut fn_params_iter = fn_params_list.params();
        for param_to_rename in fn_param_replacements {
            // We assume that parameters in replacement are in the same order as in the
            // actual params list, but just some of them (ones that named correctly) are skipped.
            let ast_ptr = loop {
                match fn_params_iter.next() {
                    Some(element)
                        if pat_equals_to_name(element.pat(), &param_to_rename.current_name) =>
                    {
                        break element.pat().unwrap()
                    }
                    Some(_) => {}
                    None => {
                        log::error!(
                            "Replacement ({:?}) was generated for a function parameter which was not found: {:?}",
                            param_to_rename, fn_src
                        );
                        return;
                    }
                }
            };

            let diagnostic = IncorrectCase {
                file: fn_src.file_id,
                ident_type: "Argument".to_string(),
                ident: AstPtr::new(&ast_ptr).into(),
                expected_case: param_to_rename.expected_case,
                ident_text: param_to_rename.current_name.to_string(),
                suggested_text: param_to_rename.suggested_text,
            };

            self.sink.push(diagnostic);
        }
    }

    fn validate_adt(&mut self, db: &dyn HirDatabase, adt: AdtId) {
        match adt {
            AdtId::StructId(struct_id) => self.validate_struct(db, struct_id),
            AdtId::EnumId(enum_id) => self.validate_enum(db, enum_id),
            AdtId::UnionId(_) => {
                // Unions aren't yet supported by this validator.
            }
        }
    }

    fn validate_struct(&mut self, db: &dyn HirDatabase, struct_id: StructId) {
        let data = db.struct_data(struct_id);

        // 1. Check the structure name.
        let struct_name = data.name.to_string();
        let struct_name_replacement = if let Some(new_name) = to_camel_case(&struct_name) {
            let replacement = Replacement {
                current_name: data.name.clone(),
                suggested_text: new_name,
                expected_case: CaseType::UpperCamelCase,
            };
            Some(replacement)
        } else {
            None
        };

        // 2. Check the field names.
        let mut struct_fields_replacements = Vec::new();

        if let VariantData::Record(fields) = data.variant_data.as_ref() {
            for (_, field) in fields.iter() {
                let field_name = field.name.to_string();
                if let Some(new_name) = to_lower_snake_case(&field_name) {
                    let replacement = Replacement {
                        current_name: field.name.clone(),
                        suggested_text: new_name,
                        expected_case: CaseType::LowerSnakeCase,
                    };
                    struct_fields_replacements.push(replacement);
                }
            }
        }

        // 3. If there is at least one element to spawn a warning on, go to the source map and generate a warning.
        self.create_incorrect_case_diagnostic_for_struct(
            struct_id,
            db,
            struct_name_replacement,
            struct_fields_replacements,
        )
    }

    /// Given the information about incorrect names in the struct declaration, looks up into the source code
    /// for exact locations and adds diagnostics into the sink.
    fn create_incorrect_case_diagnostic_for_struct(
        &mut self,
        struct_id: StructId,
        db: &dyn HirDatabase,
        struct_name_replacement: Option<Replacement>,
        struct_fields_replacements: Vec<Replacement>,
    ) {
        // XXX: only look at sources if we do have incorrect names
        if struct_name_replacement.is_none() && struct_fields_replacements.is_empty() {
            return;
        }

        let struct_loc = struct_id.lookup(db.upcast());
        let struct_src = struct_loc.source(db.upcast());

        if let Some(replacement) = struct_name_replacement {
            let ast_ptr = if let Some(name) = struct_src.value.name() {
                name
            } else {
                // We don't want rust-analyzer to panic over this, but it is definitely some kind of error in the logic.
                log::error!(
                    "Replacement ({:?}) was generated for a structure without a name: {:?}",
                    replacement,
                    struct_src
                );
                return;
            };

            let diagnostic = IncorrectCase {
                file: struct_src.file_id,
                ident_type: "Structure".to_string(),
                ident: AstPtr::new(&ast_ptr).into(),
                expected_case: replacement.expected_case,
                ident_text: replacement.current_name.to_string(),
                suggested_text: replacement.suggested_text,
            };

            self.sink.push(diagnostic);
        }

        let struct_fields_list = match struct_src.value.field_list() {
            Some(ast::FieldList::RecordFieldList(fields)) => fields,
            _ => {
                if !struct_fields_replacements.is_empty() {
                    log::error!(
                        "Replacements ({:?}) were generated for a structure fields which had no fields list: {:?}",
                        struct_fields_replacements, struct_src
                    );
                }
                return;
            }
        };
        let mut struct_fields_iter = struct_fields_list.fields();
        for field_to_rename in struct_fields_replacements {
            // We assume that parameters in replacement are in the same order as in the
            // actual params list, but just some of them (ones that named correctly) are skipped.
            let ast_ptr = loop {
                match struct_fields_iter.next() {
                    Some(element) if names_equal(element.name(), &field_to_rename.current_name) => {
                        break element.name().unwrap()
                    }
                    Some(_) => {}
                    None => {
                        log::error!(
                            "Replacement ({:?}) was generated for a function parameter which was not found: {:?}",
                            field_to_rename, struct_src
                        );
                        return;
                    }
                }
            };

            let diagnostic = IncorrectCase {
                file: struct_src.file_id,
                ident_type: "Field".to_string(),
                ident: AstPtr::new(&ast_ptr).into(),
                expected_case: field_to_rename.expected_case,
                ident_text: field_to_rename.current_name.to_string(),
                suggested_text: field_to_rename.suggested_text,
            };

            self.sink.push(diagnostic);
        }
    }

    fn validate_enum(&mut self, db: &dyn HirDatabase, enum_id: EnumId) {
        let data = db.enum_data(enum_id);
    }
}

fn names_equal(left: Option<ast::Name>, right: &Name) -> bool {
    if let Some(left) = left {
        &left.as_name() == right
    } else {
        false
    }
}

fn pat_equals_to_name(pat: Option<ast::Pat>, name: &Name) -> bool {
    if let Some(ast::Pat::IdentPat(ident)) = pat {
        ident.to_string() == name.to_string()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::diagnostics::tests::check_diagnostics;

    #[test]
    fn incorrect_function_name() {
        check_diagnostics(
            r#"
fn NonSnakeCaseName() {}
// ^^^^^^^^^^^^^^^^ Function `NonSnakeCaseName` should have a snake_case name, e.g. `non_snake_case_name`
"#,
        );
    }

    #[test]
    fn incorrect_function_params() {
        check_diagnostics(
            r#"
fn foo(SomeParam: u8) {}
    // ^^^^^^^^^ Argument `SomeParam` should have a snake_case name, e.g. `some_param`

fn foo2(ok_param: &str, CAPS_PARAM: u8) {}
                     // ^^^^^^^^^^ Argument `CAPS_PARAM` should have a snake_case name, e.g. `caps_param`
"#,
        );
    }

    #[test]
    fn incorrect_struct_name() {
        check_diagnostics(
            r#"
struct non_camel_case_name {}
    // ^^^^^^^^^^^^^^^^^^^ Structure `non_camel_case_name` should have a CamelCase name, e.g. `NonCamelCaseName`
"#,
        );
    }

    #[test]
    fn incorrect_struct_field() {
        check_diagnostics(
            r#"
struct SomeStruct { SomeField: u8 }
                 // ^^^^^^^^^ Field `SomeField` should have a snake_case name, e.g. `some_field`
"#,
        );
    }
}
