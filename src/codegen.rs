// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Generates Rust source files from the parsed WebIDL model.
//!
//! Produces `#[webidl_interface]`, `#[webidl_methods]`, `#[webidl_dictionary]`,
//! and related scaffolding following the patterns in `web-streams`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;
use std::path::Path;

use heck::ToSnakeCase;

use crate::extract::SpecDefinitions;
use crate::idl::{
    Algorithm, Attribute, Callback, Constant, Constructor, Dictionary, Enum, Interface, Method,
    Param, SpecModel, Typedef,
};

/// A generated source file.
#[derive(Debug)]
pub struct GeneratedFile {
    pub filename: String,
    pub content: String,
}

/// Generate all Rust source files from a `SpecModel`.
pub fn generate(
    model: &SpecModel,
    spec_url: &str,
    spec_defs: &SpecDefinitions,
) -> Vec<GeneratedFile> {
    let mut files = Vec::new();
    let mut mod_names = Vec::new();

    use crate::idl::GLOBAL_INTERFACES;

    // Collect all non-mixin, non-global interface names for cross-reference imports
    let all_iface_names: HashSet<String> = model
        .interfaces
        .iter()
        .filter(|i| !i.is_mixin && !GLOBAL_INTERFACES.contains(&i.name.as_str()))
        .map(|i| i.name.clone())
        .collect();

    // Collect all dictionary names for cross-reference imports
    let all_dict_names: HashSet<String> =
        model.dictionaries.iter().map(|d| d.name.clone()).collect();

    // Collect all enum names for cross-reference imports
    let all_enum_names: HashSet<String> = model.enums.iter().map(|e| e.name.clone()).collect();

    // Collect typedef names that may need importing
    let all_typedef_names: HashSet<String> =
        model.typedefs.iter().map(|t| t.name.clone()).collect();

    // Callback type aliases — referenced by dictionaries that hold callbacks.
    let all_callback_names: HashSet<String> =
        model.callbacks.iter().map(|c| c.name.clone()).collect();

    // Generate per-interface files (skip pure mixins and globals)
    for iface in &model.interfaces {
        if iface.is_mixin {
            continue;
        }
        if GLOBAL_INTERFACES.contains(&iface.name.as_str()) {
            continue;
        }
        let snake = iface.name.to_snake_case();
        let content = generate_interface(
            iface,
            spec_url,
            spec_defs,
            &all_iface_names,
            &all_dict_names,
            &all_enum_names,
            &all_typedef_names,
        );
        files.push(GeneratedFile {
            filename: format!("{snake}.rs"),
            content,
        });
        mod_names.push((snake, iface.name.clone()));
    }

    // Collect global-scoped functions and constants from partial interface Window,
    // WindowOrWorkerGlobalScope, etc., into a globals.rs with #[jsglobals].
    let global_ifaces: Vec<&Interface> = model
        .interfaces
        .iter()
        .filter(|i| GLOBAL_INTERFACES.contains(&i.name.as_str()))
        .collect();
    let has_global_content = global_ifaces
        .iter()
        .any(|i| !i.methods.is_empty() || !i.static_methods.is_empty() || !i.constants.is_empty());
    if has_global_content {
        let content = generate_globals(&global_ifaces, spec_url, spec_defs);
        files.push(GeneratedFile {
            filename: "globals.rs".to_string(),
            content,
        });
        mod_names.push(("globals".to_string(), "globals".to_string()));
    }

    // Generate per-dictionary files
    for dict in &model.dictionaries {
        let snake = dict.name.to_snake_case();
        // If a file with this name already exists (from an interface),
        // append just the dictionary struct (no file headers)
        let existing = files
            .iter_mut()
            .find(|f| f.filename == format!("{snake}.rs"));
        if let Some(existing) = existing {
            existing.content.push_str("\n\n");
            existing.content.push_str(&generate_dictionary_block(
                dict,
                spec_url,
                spec_defs,
                &all_enum_names,
            ));
        } else {
            let content = generate_dictionary(
                dict,
                spec_url,
                spec_defs,
                &all_iface_names,
                &all_enum_names,
                &all_callback_names,
            );
            files.push(GeneratedFile {
                filename: format!("{snake}.rs"),
                content,
            });
            mod_names.push((snake, dict.name.clone()));
        }
    }

    // Generate enums into a single enums.rs if any exist
    if !model.enums.is_empty() {
        let content = generate_enums(&model.enums, spec_url);
        files.push(GeneratedFile {
            filename: "enums.rs".to_string(),
            content,
        });
        mod_names.push(("enums".to_string(), "enums".to_string()));
    }

    // Generate typedefs into types_defs.rs if any exist
    if !model.typedefs.is_empty() {
        let content = generate_typedefs(&model.typedefs, spec_url);
        files.push(GeneratedFile {
            filename: "type_defs.rs".to_string(),
            content,
        });
        mod_names.push(("type_defs".to_string(), "type_defs".to_string()));
    }

    // Generate callbacks into callbacks.rs if any exist
    if !model.callbacks.is_empty() {
        let content = generate_callbacks(&model.callbacks, spec_url);
        files.push(GeneratedFile {
            filename: "callbacks.rs".to_string(),
            content,
        });
        mod_names.push(("callbacks".to_string(), "callbacks".to_string()));
    }

    // Generate standalone algorithms into algorithms.rs if any exist
    if !model.algorithms.is_empty() {
        let content = generate_algorithms(&model.algorithms, spec_url);
        files.push(GeneratedFile {
            filename: "algorithms.rs".to_string(),
            content,
        });
        mod_names.push(("algorithms".to_string(), "algorithms".to_string()));
    }

    // Generate lib.rs with mod declarations and add_to_global
    let lib_content = generate_lib(&mod_names, &model.interfaces);
    files.push(GeneratedFile {
        filename: "lib.rs".to_string(),
        content: lib_content,
    });

    files
}

/// Write generated files to a directory.
pub fn write_files(files: &[GeneratedFile], output_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(output_dir)?;
    for file in files {
        let path = output_dir.join(&file.filename);
        std::fs::write(&path, &file.content)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Interface generation
// ---------------------------------------------------------------------------

fn generate_interface(
    iface: &Interface,
    spec_url: &str,
    spec_defs: &SpecDefinitions,
    all_iface_names: &HashSet<String>,
    all_dict_names: &HashSet<String>,
    all_enum_names: &HashSet<String>,
    all_typedef_names: &HashSet<String>,
) -> String {
    let mut out = String::new();
    let mut imports = ImportSet::new();

    // Always need these for an interface
    imports.add("core_runtime::webidl_interface");
    imports.add("core_runtime::webidl_methods");

    // Collect required imports from all members
    collect_interface_imports(iface, &mut imports);

    // Internal slots need Heap<Value> and Value (unless all slots resolve to typed Heap<XxxImpl>)
    let mut slot_iface_refs =
        collect_slot_interface_refs(&iface.internal_slots, &iface.name, all_iface_names);
    let has_untyped_slots = iface.internal_slots.len() > slot_iface_refs.len()
        || iface.internal_slots.iter().any(|s| {
            let lower = s.description.to_lowercase();
            lower.contains("a list")
                || lower.starts_with("list of")
                || lower.starts_with("an ordered list")
                || s.name.to_lowercase().ends_with("requests")
        });
    if !iface.internal_slots.is_empty() && has_untyped_slots {
        imports.add("js::gc::handle::Heap");
        imports.add("js::native::Value");
    }
    if !slot_iface_refs.is_empty() {
        imports.add("js::gc::handle::Heap");
    }

    // State enums need conversion traits and fmt
    let has_state_enums = iface.internal_slots.iter().any(|slot| {
        slot.name.eq_ignore_ascii_case("state") && slot.description.matches('"').count() >= 4
    });
    if has_state_enums {
        imports.add("std::borrow::Cow");
        imports.add("std::fmt");
        imports.add("js::conversion::ConversionError");
        imports.add("js::conversion::FromJSVal");
        imports.add("js::conversion::ToJSVal");
        imports.add("js::gc::scope::Scope");
        imports.add("js::prelude::HandleValue");
    }

    // Cross-interface references: split into those that appear in method or
    // attribute signatures (need the stack newtype `Xxx<'a>`) and those that
    // appear only in internal slot storage (need `XxxImpl` for `Heap<>`).
    let signature_refs = collect_cross_interface_refs(iface, all_iface_names);
    let mut cross_refs = signature_refs.clone();
    cross_refs.extend(slot_iface_refs.iter().cloned());

    // Parent interface needs Heap<ParentImpl> import
    if let Some(parent) = &iface.extends {
        cross_refs.insert(parent.clone());
        slot_iface_refs.insert(parent.clone());
        imports.add("js::gc::handle::Heap");
    }

    // Header
    writeln!(
        out,
        "// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "//! <{spec_url}>").unwrap();
    writeln!(out).unwrap();

    // Imports
    out.push_str(&imports.render());

    // Cross-interface imports from sibling modules
    if !cross_refs.is_empty() {
        let mut refs: Vec<_> = cross_refs.into_iter().collect();
        refs.sort();
        for name in &refs {
            let mod_name = name.to_snake_case();
            // Interface types stored in `Heap<>` (internal slots, parent slot)
            // need the inner `XxxImpl`. Stack newtypes only matter for refs
            // that show up in method/attribute signatures.
            if slot_iface_refs.contains(name) {
                writeln!(out, "use super::{mod_name}::{name}Impl;").unwrap();
            }
            if signature_refs.contains(name) {
                writeln!(out, "use super::{mod_name}::{name};").unwrap();
            }
        }
    }

    // Cross-dictionary imports from sibling modules
    let dict_refs = collect_cross_dict_refs(iface, all_dict_names);
    if !dict_refs.is_empty() {
        let mut refs: Vec<_> = dict_refs.into_iter().collect();
        refs.sort();
        for name in &refs {
            let mod_name = name.to_snake_case();
            writeln!(out, "use super::{mod_name}::{name};").unwrap();
        }
    }

    // Enum imports from the enums module
    let enum_refs = collect_type_refs_from_interface(iface, all_enum_names);
    if !enum_refs.is_empty() {
        let mut refs: Vec<_> = enum_refs.into_iter().collect();
        refs.sort();
        for name in &refs {
            writeln!(out, "use super::enums::{name};").unwrap();
        }
    }

    // Typedef imports from the type_defs module
    let typedef_refs = collect_type_refs_from_interface(iface, all_typedef_names);
    if !typedef_refs.is_empty() {
        let mut refs: Vec<_> = typedef_refs.into_iter().collect();
        refs.sort();
        for name in &refs {
            writeln!(out, "use super::type_defs::{name};").unwrap();
        }
    }

    writeln!(out).unwrap();

    // Interface struct doc comment with class section link
    if let Some(class_frag) = spec_defs.class_sections.get(&iface.name) {
        writeln!(out, "/// <{spec_url}#{class_frag}>").unwrap();
    }

    let extends_attr = if let Some(parent) = &iface.extends {
        format!("(extends = {parent})")
    } else {
        String::new()
    };

    writeln!(out, "#[webidl_interface{extends_attr}]").unwrap();
    writeln!(out, "pub struct {} {{", iface.name).unwrap();

    // Parent field for inheritance
    if let Some(parent) = &iface.extends {
        writeln!(out, "    parent: Heap<{parent}Impl>,").unwrap();
    }

    let state_enums = if iface.internal_slots.is_empty() {
        if iface.extends.is_none() {
            writeln!(out, "    // TODO: Add internal state fields").unwrap();
        }
        Vec::new()
    } else {
        let result = write_internal_slot_fields(
            &mut out,
            &iface.internal_slots,
            &iface.name,
            spec_url,
            all_iface_names,
        );
        result.state_enums
    };

    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    // Emit state enums at module level, after the struct
    for state_enum in &state_enums {
        write_state_enum(&mut out, state_enum);
    }

    // Methods impl block
    writeln!(out, "#[webidl_methods]").unwrap();
    writeln!(out, "impl {} {{", iface.name).unwrap();

    // Constants
    for constant in &iface.constants {
        write_constant(&mut out, constant);
    }
    if !iface.constants.is_empty() && (iface.constructor.is_some() || !iface.attributes.is_empty())
    {
        writeln!(out).unwrap();
    }

    // Constructor
    if let Some(ctor) = &iface.constructor {
        write_constructor(&mut out, ctor, &iface.name, spec_url, spec_defs);
    } else {
        write_default_constructor(&mut out, &iface.name, spec_url, spec_defs);
    }

    // Attributes (getters/setters)
    for attr in &iface.attributes {
        writeln!(out).unwrap();
        write_getter(&mut out, attr, &iface.name, spec_url, spec_defs);
        if !attr.readonly {
            writeln!(out).unwrap();
            write_setter(&mut out, attr, &iface.name, spec_url, spec_defs);
        }
    }

    // Collect instance method names for conflict detection with static methods
    let instance_method_names: Vec<String> = iface
        .methods
        .iter()
        .map(|m| m.name.to_snake_case())
        .collect();

    // Instance methods
    for method in &iface.methods {
        writeln!(out).unwrap();
        write_method(
            &mut out,
            method,
            false,
            false,
            &iface.name,
            spec_url,
            spec_defs,
        );
    }

    // Static methods
    for method in &iface.static_methods {
        writeln!(out).unwrap();
        if instance_method_names.contains(&method.name.to_snake_case()) {
            // Name collision: use a prefixed fn name with explicit name attribute.
            write_static_method_renamed(&mut out, method, &iface.name, spec_url, spec_defs);
        } else {
            write_method(
                &mut out,
                method,
                true,
                false,
                &iface.name,
                spec_url,
                spec_defs,
            );
        }
    }

    writeln!(out, "}}").unwrap();

    out
}

fn collect_interface_imports(iface: &Interface, imports: &mut ImportSet) {
    // Constructor params
    if let Some(ctor) = &iface.constructor {
        for p in &ctor.params {
            collect_type_imports(&p.rust_type, imports);
        }
    }

    // Attributes
    for attr in &iface.attributes {
        collect_type_imports(&attr.rust_type, imports);
    }

    // Getters returning GC-rooted types need Scope
    let getter_needs_scope = iface.attributes.iter().any(|a| {
        let ty = &a.rust_type.text;
        ty.contains("'_") || ty.contains("HandleValue") || ty.contains("Object<")
    });

    // Methods always need Scope and ExnThrown
    let has_methods = !iface.methods.is_empty() || !iface.static_methods.is_empty();
    let ctor_can_throw = iface.constructor.as_ref().is_some_and(|ctor| {
        ctor.algorithm_steps.iter().any(|s| {
            let lower = s.to_lowercase();
            lower.contains("throw") || lower.contains("exception")
        })
    });
    if has_methods || ctor_can_throw || getter_needs_scope {
        imports.add("js::gc::scope::Scope");
    }
    if has_methods || ctor_can_throw {
        imports.add("js::error::ExnThrown");
    }

    for method in iface.methods.iter().chain(iface.static_methods.iter()) {
        collect_type_imports(&method.return_type, imports);
        for p in &method.params {
            collect_type_imports(&p.rust_type, imports);
        }
    }
}

fn collect_type_imports(ty: &crate::types::RustType, imports: &mut ImportSet) {
    if ty.needs_handle_value {
        imports.add("js::prelude::HandleValue");
    }
    if ty.needs_object {
        imports.add("js::Object");
    }
    if ty.needs_promise {
        imports.add("js::Promise");
    }
    if ty.needs_function {
        imports.add("js::Function");
    }
}

/// Collect names of other interfaces from this spec that appear in an
/// interface's type signatures (params, return types, attribute types).
fn collect_cross_interface_refs(
    iface: &Interface,
    all_iface_names: &HashSet<String>,
) -> HashSet<String> {
    let mut refs = HashSet::new();

    let mut check_type = |text: &str| {
        for name in all_iface_names {
            if name != &iface.name && contains_ident(text, name) {
                refs.insert(name.clone());
            }
        }
    };

    if let Some(ctor) = &iface.constructor {
        for p in &ctor.params {
            check_type(&p.rust_type.text);
        }
    }
    for attr in &iface.attributes {
        check_type(&attr.rust_type.text);
    }
    for method in iface.methods.iter().chain(iface.static_methods.iter()) {
        check_type(&method.return_type.text);
        for p in &method.params {
            check_type(&p.rust_type.text);
        }
    }

    refs
}

/// Collect names of dictionaries from this spec that appear in an
/// interface's type signatures (constructor params, method params).
fn collect_cross_dict_refs(iface: &Interface, all_dict_names: &HashSet<String>) -> HashSet<String> {
    let mut refs = HashSet::new();

    let mut check_type = |text: &str| {
        for name in all_dict_names {
            if contains_ident(text, name) {
                refs.insert(name.clone());
            }
        }
    };

    if let Some(ctor) = &iface.constructor {
        for p in &ctor.params {
            check_type(&p.rust_type.text);
        }
    }
    for method in iface.methods.iter().chain(iface.static_methods.iter()) {
        for p in &method.params {
            check_type(&p.rust_type.text);
        }
    }

    refs
}

/// Collect references to types from a given set that appear anywhere in an
/// interface's type signatures (constructor params, method params/returns,
/// attribute types).
///
/// Matches on identifier boundaries (like [`collect_type_refs_from_dict`]) so a
/// type name does not match when it is merely a substring of a longer name.
fn collect_type_refs_from_interface(
    iface: &Interface,
    type_names: &HashSet<String>,
) -> HashSet<String> {
    let mut refs = HashSet::new();

    let mut check_type = |text: &str| {
        for name in type_names {
            if contains_ident(text, name) {
                refs.insert(name.clone());
            }
        }
    };

    if let Some(ctor) = &iface.constructor {
        for p in &ctor.params {
            check_type(&p.rust_type.text);
        }
    }
    for attr in &iface.attributes {
        check_type(&attr.rust_type.text);
    }
    for method in iface.methods.iter().chain(iface.static_methods.iter()) {
        check_type(&method.return_type.text);
        for p in &method.params {
            check_type(&p.rust_type.text);
        }
    }

    refs
}

/// Collect references to types from a given set that appear in a dictionary's
/// member types.
///
/// Matches on identifier boundaries — `ReadableStream` does not match the
/// `ReadableStream` prefix of `ReadableStreamType`, which would otherwise
/// produce spurious imports for substring-overlapping type names.
fn collect_type_refs_from_dict(dict: &Dictionary, type_names: &HashSet<String>) -> HashSet<String> {
    let mut refs = HashSet::new();
    for member in &dict.members {
        for name in type_names {
            if contains_ident(&member.rust_type.text, name) {
                refs.insert(name.clone());
            }
        }
    }
    refs
}

/// Return `true` if `text` contains `ident` as a complete identifier — i.e.
/// not as part of a longer ident-character run on either side.
fn contains_ident(text: &str, ident: &str) -> bool {
    let bytes = text.as_bytes();
    let needle = ident.as_bytes();
    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let prev_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
            let next_ok =
                i + needle.len() == bytes.len() || !is_ident_byte(bytes[i + needle.len()]);
            if prev_ok && next_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn write_constant(out: &mut String, constant: &Constant) {
    writeln!(
        out,
        "    pub const {}: {} = {};",
        constant.name, constant.rust_type.text, constant.value
    )
    .unwrap();
}

fn write_constructor(
    out: &mut String,
    ctor: &Constructor,
    iface_name: &str,
    spec_url: &str,
    spec_defs: &SpecDefinitions,
) {
    let params = format_params(&ctor.params);
    let frag = lookup_fragment(spec_defs, iface_name, "constructor");

    // Detect if this constructor can throw by scanning algorithm steps.
    let can_throw = ctor.algorithm_steps.iter().any(|s| {
        let lower = s.to_lowercase();
        lower.contains("throw") || lower.contains("exception")
    });

    writeln!(out, "    /// <{spec_url}#{frag}>").unwrap();
    writeln!(out, "    #[constructor]").unwrap();
    if can_throw {
        // Setup-style constructor: receives &self after JS object allocation.
        let mut all_params = "&self, scope: &Scope<'_>".to_string();
        if !params.is_empty() {
            all_params.push_str(", ");
            all_params.push_str(&params);
        }
        writeln!(out, "    fn new({all_params}) -> Result<(), ExnThrown> {{").unwrap();
        write_step_comments(out, &ctor.algorithm_steps, 8);
        writeln!(out, "        todo!()").unwrap();
        writeln!(out, "    }}").unwrap();
    } else {
        writeln!(out, "    fn new({params}) -> Self {{").unwrap();
        write_step_comments(out, &ctor.algorithm_steps, 8);
        writeln!(out, "        todo!()").unwrap();
        writeln!(out, "    }}").unwrap();
    }
}

fn write_default_constructor(
    out: &mut String,
    iface_name: &str,
    spec_url: &str,
    spec_defs: &SpecDefinitions,
) {
    let frag = lookup_fragment(spec_defs, iface_name, "constructor");
    writeln!(out, "    /// <{spec_url}#{frag}>").unwrap();
    writeln!(out, "    #[constructor]").unwrap();
    writeln!(out, "    fn new() -> Self {{").unwrap();
    writeln!(out, "        todo!()").unwrap();
    writeln!(out, "    }}").unwrap();
}

fn write_getter(
    out: &mut String,
    attr: &Attribute,
    iface_name: &str,
    spec_url: &str,
    spec_defs: &SpecDefinitions,
) {
    let snake_name = attr.name.to_snake_case();
    let rust_type = &attr.rust_type.text;
    let comment = attr
        .rust_type
        .comment
        .as_ref()
        .map(|c| format!(" // {c}"))
        .unwrap_or_default();
    let frag = lookup_fragment(spec_defs, iface_name, &attr.name);

    // Getters returning GC-rooted types need a scope parameter with a
    // proper lifetime tying the return value to the scope.
    let needs_lifetime = rust_type.contains("'_");
    let needs_scope =
        needs_lifetime || rust_type.contains("HandleValue") || rust_type.contains("Object<");

    let return_text = if needs_lifetime {
        rust_type.replace("'_", "'r")
    } else {
        rust_type.clone()
    };
    let lifetime_param = if needs_lifetime { "<'r>" } else { "" };
    let scope_param = if needs_scope {
        if needs_lifetime {
            ", scope: &'r Scope<'_>"
        } else {
            ", scope: &Scope<'_>"
        }
    } else {
        ""
    };

    writeln!(out, "    /// <{spec_url}#{frag}>").unwrap();
    if is_rust_keyword(&snake_name) {
        writeln!(out, "    #[getter(name = \"{}\")]", attr.name).unwrap();
        writeln!(
            out,
            "    fn get_{snake_name}{lifetime_param}(&self{scope_param}) -> {return_text} {{{comment}"
        )
        .unwrap();
    } else {
        writeln!(out, "    #[getter]").unwrap();
        writeln!(
            out,
            "    fn {snake_name}{lifetime_param}(&self{scope_param}) -> {return_text} {{{comment}"
        )
        .unwrap();
    }
    write_step_comments(out, &attr.getter_steps, 8);
    writeln!(out, "        todo!()").unwrap();
    writeln!(out, "    }}").unwrap();
}

fn write_setter(
    out: &mut String,
    attr: &Attribute,
    iface_name: &str,
    spec_url: &str,
    spec_defs: &SpecDefinitions,
) {
    let snake_name = attr.name.to_snake_case();
    let rust_type = &attr.rust_type.text;
    let comment = attr
        .rust_type
        .comment
        .as_ref()
        .map(|c| format!(" // {c}"))
        .unwrap_or_default();
    let frag = lookup_fragment(spec_defs, iface_name, &attr.name);

    writeln!(out, "    /// <{spec_url}#{frag}>").unwrap();
    if is_rust_keyword(&snake_name) {
        writeln!(out, "    #[setter(name = \"{}\")]", attr.name).unwrap();
        writeln!(
            out,
            "    fn set_{snake_name}(&self, value: {rust_type}) {{{comment}"
        )
        .unwrap();
    } else {
        writeln!(out, "    #[setter]").unwrap();
        writeln!(
            out,
            "    fn set_{snake_name}(&self, value: {rust_type}) {{{comment}"
        )
        .unwrap();
    }
    write_step_comments(out, &attr.setter_steps, 8);
    writeln!(out, "        todo!()").unwrap();
    writeln!(out, "    }}").unwrap();
}

fn write_method(
    out: &mut String,
    method: &Method,
    is_static: bool,
    is_global: bool,
    iface_name: &str,
    spec_url: &str,
    spec_defs: &SpecDefinitions,
) {
    let attr_name = if is_static { "static_method" } else { "method" };
    let snake_name = method.name.to_snake_case();
    let params = format_params(&method.params);
    let frag = lookup_fragment(spec_defs, iface_name, &method.name);

    // Check if the method name conflicts with Rust trait methods.
    let needs_rename = is_rust_keyword(&snake_name) || is_conflicting_method(&snake_name);

    // Determine if return type contains a GC-rooted lifetime
    let needs_lifetime = method.return_type.text.contains("'_");
    let return_text = if needs_lifetime {
        method.return_type.text.replace("'_", "'r")
    } else {
        method.return_type.text.clone()
    };

    // Build return type wrapped in Result
    let result_return = if return_text == "()" {
        "Result<(), ExnThrown>".to_string()
    } else {
        format!("Result<{return_text}, ExnThrown>")
    };

    let lifetime_param = if needs_lifetime { "<'r>" } else { "" };
    let scope_lifetime = if needs_lifetime {
        "&'r Scope<'_>"
    } else {
        "&Scope<'_>"
    };

    let self_param = if is_static || is_global {
        ""
    } else {
        "&self, "
    };
    let visibility = if is_global { "pub " } else { "" };

    // Build the full parameter list (always include scope)
    let mut all_params = format!("scope: {scope_lifetime}");
    if !params.is_empty() {
        all_params.push_str(", ");
        all_params.push_str(&params);
    }

    let comment = method
        .return_type
        .comment
        .as_ref()
        .map(|c| format!(" // returns {c}"))
        .unwrap_or_default();

    writeln!(out, "    /// <{spec_url}#{frag}>").unwrap();
    if is_global {
        // Global functions are plain `pub fn` — no #[method] attribute
        writeln!(
            out,
            "    {visibility}fn {snake_name}{lifetime_param}({all_params}) -> {result_return} {{{comment}"
        )
        .unwrap();
    } else if needs_rename {
        writeln!(out, "    #[{attr_name}(name = \"{}\")]", method.name).unwrap();
        writeln!(
            out,
            "    fn js_{snake_name}{lifetime_param}({self_param}{all_params}) -> {result_return} {{{comment}"
        )
        .unwrap();
    } else {
        writeln!(out, "    #[{attr_name}]").unwrap();
        writeln!(
            out,
            "    fn {snake_name}{lifetime_param}({self_param}{all_params}) -> {result_return} {{{comment}"
        )
        .unwrap();
    }

    // Write algorithm steps as comments
    write_step_comments(out, &method.algorithm_steps, 8);

    writeln!(out, "        todo!()").unwrap();
    writeln!(out, "    }}").unwrap();
}

/// Write a static method whose name collides with an instance method.
/// Prefixes the Rust fn name with `static_` and adds a `name` attribute.
fn write_static_method_renamed(
    out: &mut String,
    method: &Method,
    iface_name: &str,
    spec_url: &str,
    spec_defs: &SpecDefinitions,
) {
    let snake_name = method.name.to_snake_case();
    let params = format_params(&method.params);
    let frag = lookup_fragment(spec_defs, iface_name, &method.name);

    let needs_lifetime = method.return_type.text.contains("'_");
    let return_text = if needs_lifetime {
        method.return_type.text.replace("'_", "'r")
    } else {
        method.return_type.text.clone()
    };
    let result_return = if return_text == "()" {
        "Result<(), ExnThrown>".to_string()
    } else {
        format!("Result<{return_text}, ExnThrown>")
    };
    let lifetime_param = if needs_lifetime { "<'r>" } else { "" };
    let scope_lifetime = if needs_lifetime {
        "&'r Scope<'_>"
    } else {
        "&Scope<'_>"
    };

    let mut all_params = format!("scope: {scope_lifetime}");
    if !params.is_empty() {
        all_params.push_str(", ");
        all_params.push_str(&params);
    }

    writeln!(out, "    /// <{spec_url}#{frag}>").unwrap();
    writeln!(out, "    #[static_method(name = \"{}\")]", method.name).unwrap();
    writeln!(
        out,
        "    fn static_{snake_name}{lifetime_param}({all_params}) -> {result_return} {{"
    )
    .unwrap();
    write_step_comments(out, &method.algorithm_steps, 8);
    writeln!(out, "        todo!()").unwrap();
    writeln!(out, "    }}").unwrap();
}

fn format_params(params: &[Param]) -> String {
    params
        .iter()
        .map(|p| {
            let snake = p.name.to_snake_case();
            let name = if is_rust_keyword(&snake) {
                format!("r#{snake}")
            } else {
                snake
            };
            let ty = if p.optional {
                format!("Option<{}>", p.rust_type.text)
            } else if p.variadic {
                format!("RestArgs<{}>", p.rust_type.text)
            } else {
                p.rust_type.text.clone()
            };
            let comment = p
                .rust_type
                .comment
                .as_ref()
                .map(|c| format!(" /* {c} */"))
                .unwrap_or_default();
            format!("{name}: {ty}{comment}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_return_type(rt: &crate::types::RustType) -> String {
    if rt.text == "()" {
        String::new()
    } else {
        format!(" -> {}", rt.text)
    }
}

/// Write algorithm steps as numbered comments inside a function body, wrapped at 100 columns.
fn write_step_comments(out: &mut String, steps: &[String], indent: usize) {
    for (i, step) in steps.iter().enumerate() {
        write_wrapped_step(out, i + 1, step, indent);
    }
}

/// Write a single step comment, word-wrapping at 100 columns with aligned continuation.
fn write_wrapped_step(out: &mut String, step_num: usize, text: &str, indent: usize) {
    let step_marker = format!("Step {step_num}: ");
    let prefix = format!("{:indent$}// {step_marker}", "");
    let cont_pad = " ".repeat(step_marker.len());
    let cont_prefix = format!("{:indent$}// {cont_pad}", "");
    let available = 100usize.saturating_sub(prefix.len());

    let lines = word_wrap(text, available);
    for (j, line) in lines.iter().enumerate() {
        if j == 0 {
            writeln!(out, "{prefix}{line}").unwrap();
        } else {
            writeln!(out, "{cont_prefix}{line}").unwrap();
        }
    }
}

/// Word-wrap text to the given width, breaking at word boundaries.
fn word_wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![text.to_string()];
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in words {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() > width {
            lines.push(current);
            current = word.to_string();
        } else {
            current.push(' ');
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Rust keywords that need escaping when used as identifiers.
const RUST_KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate",
    "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "if", "impl", "in",
    "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "true", "try", "type",
    "typeof", "union", "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
];

/// Check if a name is a Rust keyword that needs escaping.
fn is_rust_keyword(name: &str) -> bool {
    RUST_KEYWORDS.contains(&name)
}

/// Method names that conflict with common Rust trait methods.
const CONFLICTING_METHODS: &[&str] = &["clone", "from", "into", "to_string", "eq", "ne", "default"];

/// Check if a method name would conflict with a Rust trait method.
fn is_conflicting_method(name: &str) -> bool {
    CONFLICTING_METHODS.contains(&name)
}

/// Look up the fragment ID for a class member from extracted spec definitions,
/// falling back to the `#dom-Class-member` convention.
fn lookup_fragment(spec_defs: &SpecDefinitions, iface_name: &str, member_name: &str) -> String {
    let key = (iface_name.to_string(), member_name.to_string());
    if let Some(frag) = spec_defs.member_fragments.get(&key) {
        return frag.clone();
    }
    // Fallback: construct the conventional dom-class-member fragment
    format!(
        "dom-{}-{}",
        iface_name.to_lowercase(),
        member_name.to_lowercase()
    )
}

/// A state enum extracted from an internal slot's quoted values.
struct StateEnum {
    name: String,
    /// Pairs of (original JS string value, Rust variant name).
    variants: Vec<(String, String)>,
}

/// Result of writing internal slot fields: state enums to define at module level.
struct SlotFieldsResult {
    state_enums: Vec<StateEnum>,
}

/// Write internal slot fields as struct fields with doc comments.
///
/// Uses simple heuristics to determine field types from slot descriptions:
/// - "boolean" → `bool`
/// - Known interface name in description → `Heap<XxxImpl>` (or `Option<Heap<XxxImpl>>`)
/// - "list" → `Vec<Heap<Value>>`
/// - "state" with quoted values → state enum (emitted separately at module level)
/// - Default → `Heap<Value>`
fn write_internal_slot_fields(
    out: &mut String,
    slots: &[crate::idl::InternalSlot],
    iface_name: &str,
    spec_url: &str,
    all_iface_names: &HashSet<String>,
) -> SlotFieldsResult {
    use heck::ToSnakeCase;

    let mut state_enums = Vec::new();

    for slot in slots {
        let snake_name = slot.name.to_snake_case();
        let name = if is_rust_keyword(&snake_name) {
            format!("r#{snake_name}")
        } else {
            snake_name
        };

        // Doc comment with slot description and spec link
        if !slot.fragment_id.is_empty() {
            writeln!(out, "    /// <{spec_url}#{}>", slot.fragment_id).unwrap();
        }
        if !slot.description.is_empty() {
            // Word-wrap the description at 96 columns (100 minus "    /// ")
            let lines = word_wrap(&slot.description, 93);
            for line in &lines {
                writeln!(out, "    /// {line}").unwrap();
            }
        }

        let lower_desc = slot.description.to_lowercase();
        let lower_name = slot.name.to_lowercase();

        // Strip leading punctuation/parens for pattern matching.
        let trimmed_desc = lower_desc.trim_start_matches(|c: char| c == '(' || c.is_whitespace());

        if lower_desc.contains("boolean")
            || lower_desc.starts_with("a boolean")
            || lower_desc.starts_with("whether")
        {
            writeln!(out, "    {name}: bool,").unwrap();
        } else if trimmed_desc.starts_with("a list")
            || trimmed_desc.starts_with("list of")
            || trimmed_desc.starts_with("an ordered list")
            || lower_name.ends_with("requests")
        {
            // If the list description mentions a known interface, use Vec<Heap<XxxImpl>>
            if let Some(ref_name) =
                detect_interface_ref(&slot.description, iface_name, all_iface_names)
            {
                writeln!(out, "    {name}: Vec<Heap<{ref_name}Impl>>,").unwrap();
            } else {
                writeln!(out, "    {name}: Vec<Heap<Value>>,").unwrap();
            }
        } else if lower_desc.contains("a nonnegative integer")
            || lower_desc.contains("a non-negative integer")
            || lower_desc.contains("total size")
        {
            // Check for "or undefined" → Option<u64>
            if lower_desc.contains("undefined") {
                writeln!(out, "    {name}: Option<u64>,").unwrap();
            } else {
                writeln!(out, "    {name}: u64,").unwrap();
            }
        } else if lower_desc.contains("a positive integer")
            || lower_desc.contains("a number")
            || lower_desc.contains("an integer")
        {
            if lower_desc.contains("undefined") {
                writeln!(out, "    {name}: Option<f64>,").unwrap();
            } else {
                writeln!(out, "    {name}: f64,").unwrap();
            }
        } else if let Some(ref_name) =
            detect_interface_ref(&slot.description, iface_name, all_iface_names)
        {
            let nullable =
                lower_desc.contains("null") || lower_desc.contains("initially undefined");
            if nullable {
                writeln!(out, "    {name}: Option<Heap<{ref_name}Impl>>,").unwrap();
            } else {
                writeln!(out, "    {name}: Heap<{ref_name}Impl>,").unwrap();
            }
        } else if lower_name == "state" {
            // Extract quoted state values from the description, e.g. "readable", "closed"
            let mut seen = HashSet::new();
            let states: Vec<&str> = slot
                .description
                .split('"')
                .enumerate()
                .filter_map(|(i, s)| if i % 2 == 1 { Some(s) } else { None })
                .filter(|s| seen.insert(s.to_string()))
                .collect();
            if states.len() >= 2 {
                let enum_name = format!("{iface_name}State");
                let variants: Vec<(String, String)> = states
                    .iter()
                    .map(|s| (s.to_string(), enum_variant_name(s)))
                    .collect();
                writeln!(out, "    #[no_trace]").unwrap();
                writeln!(out, "    {name}: {enum_name},").unwrap();
                state_enums.push(StateEnum {
                    name: enum_name,
                    variants,
                });
            } else {
                writeln!(out, "    // TODO: Define a state enum for this field").unwrap();
                writeln!(out, "    #[no_trace]").unwrap();
                writeln!(out, "    {name}: u8,").unwrap();
            }
        } else {
            writeln!(out, "    {name}: Heap<Value>,").unwrap();
        }
    }

    SlotFieldsResult { state_enums }
}

/// Check whether a slot description references a known interface type.
///
/// Matches patterns like "(an AbortSignal object)" or "(a ReadableStream)".
/// Also handles indirect references where PascalCase names appear as
/// space-separated lowercase words in spec prose (e.g., "potential event
/// target" → `EventTarget`).
///
/// Returns the interface name if found, excluding self-references.
fn detect_interface_ref<'a>(
    description: &str,
    self_iface: &str,
    all_iface_names: &'a HashSet<String>,
) -> Option<String> {
    // Among all interface names that match, prefer the one mentioned earliest in
    // the description, tie-breaking on the longest (most specific) name. This is
    // deterministic — iterating `all_iface_names` (a HashSet) yields names in an
    // arbitrary, run-to-run-varying order, so returning the first match would
    // make generated output unstable. Earliest-mention also tends to be the
    // right type: e.g. "null or an element in a different node tree" should
    // resolve to `Element`, not the `Node` buried in "node tree".
    let mut best: Option<(usize, &String)> = None;
    let mut consider = |pos: usize, name: &'a String| {
        let better = match best {
            Some((bpos, bname)) => pos < bpos || (pos == bpos && name.len() > bname.len()),
            None => true,
        };
        if better {
            best = Some((pos, name));
        }
    };

    // First pass: exact PascalCase match (e.g., "AbortSignal" in description).
    for name in all_iface_names {
        if name == self_iface {
            continue;
        }
        if let Some(pos) = description.find(name.as_str()) {
            let before_ok = pos == 0 || !description.as_bytes()[pos - 1].is_ascii_alphanumeric();
            let after_pos = pos + name.len();
            let after_ok = after_pos >= description.len()
                || !description.as_bytes()[after_pos].is_ascii_alphanumeric();
            if before_ok && after_ok {
                consider(pos, name);
            }
        }
    }
    if let Some((_, name)) = best {
        return Some(name.clone());
    }

    // Second pass: indirect match — convert PascalCase interface names to
    // lowercase space-separated words and check if that phrase appears in
    // the description (e.g., "EventTarget" → "event target" matches
    // "potential event target").
    let lower_desc = description.to_lowercase();
    let mut best: Option<(usize, &String)> = None;
    let mut consider = |pos: usize, name: &'a String| {
        let better = match best {
            Some((bpos, bname)) => pos < bpos || (pos == bpos && name.len() > bname.len()),
            None => true,
        };
        if better {
            best = Some((pos, name));
        }
    };
    for name in all_iface_names {
        if name == self_iface {
            continue;
        }
        let lowered = pascal_to_words(name);
        if let Some(pos) = lower_desc.find(&lowered) {
            let before_ok = pos == 0 || !lower_desc.as_bytes()[pos - 1].is_ascii_alphanumeric();
            let after_pos = pos + lowered.len();
            let after_ok = after_pos >= lower_desc.len()
                || !lower_desc.as_bytes()[after_pos].is_ascii_alphanumeric();
            if before_ok && after_ok {
                consider(pos, name);
            }
        }
    }

    best.map(|(_, name)| name.clone())
}

/// Convert a PascalCase name to lowercase space-separated words.
/// e.g., "EventTarget" → "event target", "AbortSignal" → "abort signal"
fn pascal_to_words(name: &str) -> String {
    let mut words = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            words.push(' ');
        }
        words.push(ch.to_lowercase().next().unwrap());
    }
    words
}

/// Pre-scan internal slots to find which ones reference known interface types.
///
/// Returns the set of interface names referenced. This is used to determine
/// imports before the struct fields are written.
fn collect_slot_interface_refs(
    slots: &[crate::idl::InternalSlot],
    self_iface: &str,
    all_iface_names: &HashSet<String>,
) -> HashSet<String> {
    let mut refs = HashSet::new();
    for slot in slots {
        let lower_desc = slot.description.to_lowercase();
        let lower_name = slot.name.to_lowercase();
        // Skip slots that will be typed as bool, numeric, or state enum —
        // but NOT lists, since those can contain typed interface refs.
        if lower_desc.contains("boolean")
            || lower_desc.starts_with("a boolean")
            || lower_desc.starts_with("whether")
            || lower_desc.contains("a nonnegative integer")
            || lower_desc.contains("a non-negative integer")
            || lower_desc.contains("total size")
            || lower_desc.contains("a positive integer")
            || lower_desc.contains("a number")
            || lower_desc.contains("an integer")
            || lower_name == "state"
        {
            continue;
        }
        // For both list and non-list slots, check for interface refs
        if let Some(name) = detect_interface_ref(&slot.description, self_iface, all_iface_names) {
            refs.insert(name);
        }
    }
    refs
}

/// Write a state enum definition with `Display`, `FromStr`, `FromJSVal`, and `ToJSVal` impls.
fn write_state_enum(out: &mut String, state_enum: &StateEnum) {
    writeln!(out, "#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]").unwrap();
    writeln!(out, "pub enum {} {{", state_enum.name).unwrap();
    for (i, (_value, variant)) in state_enum.variants.iter().enumerate() {
        if i == 0 {
            writeln!(out, "    #[default]").unwrap();
        }
        writeln!(out, "    {variant},").unwrap();
    }
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    // Display impl
    writeln!(out, "impl fmt::Display for {} {{", state_enum.name).unwrap();
    writeln!(
        out,
        "    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {{"
    )
    .unwrap();
    writeln!(out, "        f.write_str(match self {{").unwrap();
    for (value, variant) in &state_enum.variants {
        writeln!(out, "            Self::{variant} => \"{value}\",").unwrap();
    }
    writeln!(out, "        }})").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    // Collect original values for FromJSVal/ToJSVal
    let values: Vec<String> = state_enum.variants.iter().map(|(v, _)| v.clone()).collect();

    // FromJSVal impl
    write_enum_from_jsval(out, &state_enum.name, &values);

    // ToJSVal impl
    write_enum_to_jsval(out, &state_enum.name, &values);
}

// ---------------------------------------------------------------------------
// Dictionary generation
// ---------------------------------------------------------------------------

/// Generate a complete dictionary file (with SPDX header and imports).
fn generate_dictionary(
    dict: &Dictionary,
    spec_url: &str,
    spec_defs: &SpecDefinitions,
    all_iface_names: &HashSet<String>,
    all_enum_names: &HashSet<String>,
    all_callback_names: &HashSet<String>,
) -> String {
    let mut out = String::new();
    let mut imports = ImportSet::new();

    imports.add("core_runtime::webidl_dictionary");

    for member in &dict.members {
        collect_type_imports(&member.rust_type, &mut imports);
    }

    let needs_lifetime = dict.members.iter().any(|m| m.rust_type.text.contains("<'"));

    writeln!(
        out,
        "// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "//! <{spec_url}>").unwrap();
    writeln!(out).unwrap();
    out.push_str(&imports.render());

    // Cross-interface imports for stack-newtype fields (the `Xxx<'a>` form,
    // which has a `FromJSVal` impl — not the inner `XxxImpl` data struct).
    let iface_refs = collect_type_refs_from_dict(dict, all_iface_names);
    if !iface_refs.is_empty() {
        let mut refs: Vec<_> = iface_refs.into_iter().collect();
        refs.sort();
        for name in &refs {
            let mod_name = name.to_snake_case();
            writeln!(out, "use super::{mod_name}::{name};").unwrap();
        }
    }

    // Callback type aliases live in callbacks.rs.
    let callback_refs = collect_type_refs_from_dict(dict, all_callback_names);
    if !callback_refs.is_empty() {
        let mut refs: Vec<_> = callback_refs.into_iter().collect();
        refs.sort();
        for name in &refs {
            writeln!(out, "use super::callbacks::{name};").unwrap();
        }
    }

    // Enum imports from the enums module
    let enum_refs = collect_type_refs_from_dict(dict, all_enum_names);
    if !enum_refs.is_empty() {
        let mut refs: Vec<_> = enum_refs.into_iter().collect();
        refs.sort();
        for name in &refs {
            writeln!(out, "use super::enums::{name};").unwrap();
        }
    }

    writeln!(out).unwrap();

    write_dictionary_struct(
        &mut out,
        dict,
        spec_url,
        spec_defs,
        needs_lifetime,
        all_enum_names,
    );

    out
}

/// Generate just the struct block for a dictionary (no file headers or imports).
fn generate_dictionary_block(
    dict: &Dictionary,
    spec_url: &str,
    spec_defs: &SpecDefinitions,
    all_enum_names: &HashSet<String>,
) -> String {
    let mut out = String::new();
    // When appending to an existing file, add enum imports inline
    let enum_refs = collect_type_refs_from_dict(dict, all_enum_names);
    if !enum_refs.is_empty() {
        let mut refs: Vec<_> = enum_refs.into_iter().collect();
        refs.sort();
        for name in &refs {
            writeln!(out, "use super::enums::{name};").unwrap();
        }
        writeln!(out).unwrap();
    }
    let needs_lifetime = dict.members.iter().any(|m| m.rust_type.text.contains("<'"));
    write_dictionary_struct(
        &mut out,
        dict,
        spec_url,
        spec_defs,
        needs_lifetime,
        all_enum_names,
    );
    out
}

/// Write a `#[webidl_dictionary]` struct definition.
///
/// Field types are emitted verbatim (no `Heap<>` wrapping) so the
/// `#[webidl_dictionary]` macro can synthesize a `FromJSVal` impl that
/// deserializes each property directly. If any field carries a `<'_>`
/// lifetime, the struct itself gains an `<'a>` parameter and every `<'_>`
/// is rewritten to `<'a>`.
fn write_dictionary_struct(
    out: &mut String,
    dict: &Dictionary,
    spec_url: &str,
    spec_defs: &SpecDefinitions,
    needs_lifetime: bool,
    all_enum_names: &HashSet<String>,
) {
    // Dictionary doc comment with spec link
    if let Some(frag) = spec_defs.dictionary_fragments.get(&dict.name) {
        writeln!(out, "/// <{spec_url}#{frag}>").unwrap();
    }
    writeln!(out, "#[webidl_dictionary]").unwrap();
    if needs_lifetime {
        writeln!(out, "pub struct {}<'a> {{", dict.name).unwrap();
    } else {
        writeln!(out, "pub struct {} {{", dict.name).unwrap();
    }

    for member in &dict.members {
        let snake = member.name.to_snake_case();
        let name = if is_rust_keyword(&snake) {
            format!("r#{snake}")
        } else {
            snake
        };

        let base_type = member.rust_type.text.replace("<'_>", "<'a>");

        let ty = if member.required
            || (member.default_value.is_some() && member.default_value.as_deref() != Some("None"))
        {
            base_type
        } else {
            format!("Option<{base_type}>")
        };
        let comment = member
            .rust_type
            .comment
            .as_ref()
            .map(|c| format!(" // {c}"))
            .unwrap_or_default();

        if let Some(parent) = &member.inherited_from {
            writeln!(out, "    // inherited from {parent}").unwrap();
        }
        if let Some(default) = &member.default_value
            && default != "None"
        {
            let formatted_default =
                format_dict_default(default, &member.rust_type.text, all_enum_names);
            writeln!(out, "    #[webidl(default = {formatted_default})]").unwrap();
        }
        writeln!(out, "    pub {name}: {ty},{comment}").unwrap();
    }

    writeln!(out, "}}").unwrap();
}

/// Format a dictionary member's default value, converting string defaults to
/// enum variant expressions when the field type is an enum, and `""` to
/// `String::new()` for String fields.
fn format_dict_default(
    default: &str,
    field_type: &str,
    all_enum_names: &HashSet<String>,
) -> String {
    // If it's a quoted string default and the field type is an enum, convert
    // to EnumName::Variant form.
    if let Some(inner) = default.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        if all_enum_names.contains(field_type) {
            let variant = enum_variant_name(inner);
            return format!("{field_type}::{variant}");
        }
        // Empty string for String fields: use String::new()
        if field_type == "String" && inner.is_empty() {
            return "String::new()".to_string();
        }
    }
    default.to_string()
}

// ---------------------------------------------------------------------------
// Enum generation
// ---------------------------------------------------------------------------

fn generate_enums(enums: &[Enum], spec_url: &str) -> String {
    let mut out = String::new();

    writeln!(
        out,
        "// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "//! WebIDL enumerations from <{spec_url}>").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "use std::borrow::Cow;").unwrap();
    writeln!(out, "use std::fmt;").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "use js::conversion::{{ConversionError, FromJSVal, ToJSVal}};"
    )
    .unwrap();
    writeln!(out, "use js::gc::scope::Scope;").unwrap();
    writeln!(out, "use js::prelude::HandleValue;").unwrap();
    writeln!(out).unwrap();

    for e in enums {
        writeln!(out, "/// WebIDL enum `{}`", e.name).unwrap();
        writeln!(out, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]").unwrap();
        writeln!(out, "pub enum {} {{", e.name).unwrap();
        for variant in &e.variants {
            let rust_variant = enum_variant_name(variant);
            if rust_variant != *variant {
                writeln!(out, "    /// `\"{variant}\"`").unwrap();
            }
            writeln!(out, "    {rust_variant},").unwrap();
        }
        writeln!(out, "}}").unwrap();
        writeln!(out).unwrap();

        // Display impl
        writeln!(out, "impl fmt::Display for {} {{", e.name).unwrap();
        writeln!(
            out,
            "    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {{"
        )
        .unwrap();
        writeln!(out, "        f.write_str(match self {{").unwrap();
        for variant in &e.variants {
            let rust_variant = enum_variant_name(variant);
            writeln!(out, "            Self::{rust_variant} => \"{variant}\",").unwrap();
        }
        writeln!(out, "        }})").unwrap();
        writeln!(out, "    }}").unwrap();
        writeln!(out, "}}").unwrap();
        writeln!(out).unwrap();

        // FromStr impl
        writeln!(out, "impl std::str::FromStr for {} {{", e.name).unwrap();
        writeln!(out, "    type Err = ();").unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "    fn from_str(s: &str) -> Result<Self, Self::Err> {{"
        )
        .unwrap();
        writeln!(out, "        match s {{").unwrap();
        for variant in &e.variants {
            let rust_variant = enum_variant_name(variant);
            writeln!(
                out,
                "            \"{variant}\" => Ok(Self::{rust_variant}),"
            )
            .unwrap();
        }
        writeln!(out, "            _ => Err(()),").unwrap();
        writeln!(out, "        }}").unwrap();
        writeln!(out, "    }}").unwrap();
        writeln!(out, "}}").unwrap();
        writeln!(out).unwrap();

        // FromJSVal impl
        write_enum_from_jsval(&mut out, &e.name, &e.variants);

        // ToJSVal impl
        write_enum_to_jsval(&mut out, &e.name, &e.variants);
    }

    out
}

/// Convert a WebIDL enum string value to a Rust variant name.
///
/// WebIDL enum values are arbitrary strings, so the camel-cased form may not be
/// a valid Rust identifier — e.g. `"2d"` stays `2d`, which can't start an
/// identifier, and a punctuation-only value can camel-case to the empty string.
/// Both cases are repaired so the generated enum compiles.
fn enum_variant_name(s: &str) -> String {
    use heck::ToUpperCamelCase;

    if s.is_empty() {
        return "Empty".to_string();
    }

    // Handle kebab-case, space-separated, etc.
    let camel = s.to_upper_camel_case();
    if camel.is_empty() {
        "Empty".to_string()
    } else if camel.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{camel}")
    } else {
        camel
    }
}

/// Write a `FromJSVal` impl that converts a JS string to an enum variant.
///
/// `variants` are the original JS string values (not Rust variant names).
fn write_enum_from_jsval(out: &mut String, enum_name: &str, variants: &[String]) {
    writeln!(out, "impl<'s> FromJSVal<'s> for {enum_name} {{").unwrap();
    writeln!(out, "    type Config = ();").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "    fn from_jsval(scope: &'s Scope<'s>, val: HandleValue<'s>, _: ()) -> Result<Self, ConversionError> {{"
    )
    .unwrap();
    writeln!(out, "        let s = String::from_jsval(scope, val, ())?;").unwrap();
    writeln!(out, "        match s.as_str() {{").unwrap();
    for variant in variants {
        let rust_variant = enum_variant_name(variant);
        writeln!(
            out,
            "            \"{variant}\" => Ok(Self::{rust_variant}),"
        )
        .unwrap();
    }
    writeln!(
        out,
        "            _ => Err(ConversionError::Failure(Cow::Borrowed(c\"invalid value for {enum_name}\"))),",
    )
    .unwrap();
    writeln!(out, "        }}").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
}

/// Write a `ToJSVal` impl that converts an enum variant to a JS string.
///
/// `variants` are the original JS string values (not Rust variant names).
fn write_enum_to_jsval(out: &mut String, enum_name: &str, variants: &[String]) {
    writeln!(out, "impl<'s> ToJSVal<'s> for {enum_name} {{").unwrap();
    writeln!(
        out,
        "    fn to_jsval(&self, scope: &'s Scope<'s>) -> Result<HandleValue<'s>, ConversionError> {{"
    )
    .unwrap();
    writeln!(out, "        match self {{").unwrap();
    for variant in variants {
        let rust_variant = enum_variant_name(variant);
        writeln!(
            out,
            "            Self::{rust_variant} => \"{variant}\".to_jsval(scope),"
        )
        .unwrap();
    }
    writeln!(out, "        }}").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
}

// ---------------------------------------------------------------------------
// Typedef generation
// ---------------------------------------------------------------------------

fn generate_typedefs(typedefs: &[Typedef], spec_url: &str) -> String {
    let mut out = String::new();
    let mut imports = ImportSet::new();

    for td in typedefs {
        collect_type_imports(&td.rust_type, &mut imports);
    }

    writeln!(
        out,
        "// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "//! Type aliases from <{spec_url}>").unwrap();
    writeln!(out).unwrap();
    out.push_str(&imports.render());
    writeln!(out).unwrap();

    for td in typedefs {
        let comment = td
            .rust_type
            .comment
            .as_ref()
            .map(|c| format!(" // {c}"))
            .unwrap_or_default();
        // Type aliases cannot use anonymous lifetimes — replace `'_` with `'a`.
        let type_text = td.rust_type.text.replace("'_", "'a");
        let lifetime = if td.rust_type.text.contains("'_") {
            "<'a>"
        } else {
            ""
        };
        writeln!(
            out,
            "pub type {}{lifetime} = {type_text};{comment}",
            td.name
        )
        .unwrap();
    }

    out
}

// ---------------------------------------------------------------------------
// Callback generation
// ---------------------------------------------------------------------------
// Globals generation (Window / WorkerGlobalScope partial interfaces)
// ---------------------------------------------------------------------------

/// Generate a `globals.rs` file with `#[jsglobals]` for functions and constants
/// from partial interface Window or WindowOrWorkerGlobalScope.
fn generate_globals(ifaces: &[&Interface], spec_url: &str, spec_defs: &SpecDefinitions) -> String {
    let mut out = String::new();
    let mut imports = ImportSet::new();

    imports.add("core_runtime::jsglobals");
    imports.add("js::gc::scope::Scope");
    imports.add("js::error::ExnThrown");

    // Collect all types used in the global functions
    for iface in ifaces {
        for method in iface.methods.iter().chain(iface.static_methods.iter()) {
            collect_type_imports(&method.return_type, &mut imports);
            for p in &method.params {
                collect_type_imports(&p.rust_type, &mut imports);
            }
        }
        for attr in &iface.attributes {
            collect_type_imports(&attr.rust_type, &mut imports);
        }
    }

    writeln!(
        out,
        "// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "//! Global functions and constants from <{spec_url}>").unwrap();
    writeln!(out).unwrap();

    // Only the #[jsglobals] macro import goes at the top level;
    // all other imports go inside `mod globals`.
    writeln!(out, "use core_runtime::jsglobals;").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "#[jsglobals]").unwrap();
    writeln!(out, "pub mod globals {{").unwrap();

    // Render remaining imports inside the module
    for import in &imports.imports {
        if *import != "core_runtime::jsglobals" {
            writeln!(out, "    use {import};").unwrap();
        }
    }

    for iface in ifaces {
        // Constants
        for constant in &iface.constants {
            write_constant(&mut out, constant);
        }
        // Methods (as global functions)
        for method in iface.methods.iter().chain(iface.static_methods.iter()) {
            writeln!(out).unwrap();
            write_method(
                &mut out,
                method,
                true,
                true,
                &iface.name,
                spec_url,
                spec_defs,
            );
        }
    }

    writeln!(out, "}}").unwrap();
    out
}

// ---------------------------------------------------------------------------
// Callback generation
// ---------------------------------------------------------------------------

fn generate_callbacks(callbacks: &[Callback], spec_url: &str) -> String {
    let mut out = String::new();

    writeln!(
        out,
        "// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "//! Callback definitions from <{spec_url}>").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "use js::Function;").unwrap();
    writeln!(out).unwrap();

    for cb in callbacks {
        let params = format_params(&cb.params);
        let return_type = format_return_type(&cb.return_type);
        writeln!(
            out,
            "/// WebIDL callback `{}`: ({params}){return_type}",
            cb.name
        )
        .unwrap();
        writeln!(out, "pub type {}<'s> = Function<'s>;", cb.name).unwrap();
        writeln!(out).unwrap();
    }

    out
}

// ---------------------------------------------------------------------------
// Algorithm generation
// ---------------------------------------------------------------------------

fn generate_algorithms(algorithms: &[Algorithm], spec_url: &str) -> String {
    let mut out = String::new();

    writeln!(
        out,
        "// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "//! Standalone algorithms from <{spec_url}>").unwrap();
    writeln!(out).unwrap();

    // Two distinct spec algorithms can collapse to the same snake_case
    // identifier (e.g. the streams spec defines both `ReadableStreamPipeTo`
    // and the prose concept `ReadableStream pipe to`, which both shape into
    // `readable_stream_pipe_to`). Suffix later collisions with `_2`, `_3`, …
    // so the file compiles; the developer can rename to something meaningful.
    let mut seen: HashMap<String, u32> = HashMap::new();
    for algo in algorithms {
        let base = algo.name.to_snake_case();
        let count = seen.entry(base.clone()).or_insert(0);
        *count += 1;
        let fn_name = if *count == 1 {
            base
        } else {
            format!("{base}_{}", *count)
        };

        if !algo.fragment.is_empty() {
            writeln!(out, "/// <{spec_url}#{}>", algo.fragment).unwrap();
        }
        writeln!(out, "/// {}", algo.heading).unwrap();
        writeln!(out, "pub(crate) fn {fn_name}() {{").unwrap();
        write_step_comments_unindented(&mut out, &algo.steps);
        writeln!(out, "    todo!()").unwrap();
        writeln!(out, "}}").unwrap();
        writeln!(out).unwrap();
    }

    out
}

/// Write algorithm steps as numbered comments with standard (4-space) indentation.
fn write_step_comments_unindented(out: &mut String, steps: &[String]) {
    write_step_comments(out, steps, 4);
}

// ---------------------------------------------------------------------------
// lib.rs generation
// ---------------------------------------------------------------------------

fn generate_lib(mod_names: &[(String, String)], interfaces: &[Interface]) -> String {
    use crate::idl::GLOBAL_INTERFACES;

    let mut out = String::new();

    writeln!(
        out,
        "// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception"
    )
    .unwrap();
    writeln!(out).unwrap();

    // Module declarations
    for (snake, _) in mod_names {
        writeln!(out, "pub mod {snake};").unwrap();
    }

    writeln!(out).unwrap();

    // add_to_global function
    writeln!(out, "use js::gc::scope::Scope;").unwrap();
    writeln!(out, "use js::Object;").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {{"
    )
    .unwrap();

    let has_globals = mod_names.iter().any(|(snake, _)| snake == "globals");

    for iface in interfaces {
        if iface.is_mixin || GLOBAL_INTERFACES.contains(&iface.name.as_str()) {
            continue;
        }
        let snake = iface.name.to_snake_case();
        writeln!(
            out,
            "    {snake}::{name}::add_to_global(scope, global);",
            name = iface.name
        )
        .unwrap();
    }

    if has_globals {
        writeln!(out, "    globals::globals::add_to_global(scope, global);").unwrap();
    }

    writeln!(out, "}}").unwrap();

    out
}

// ---------------------------------------------------------------------------
// Import management
// ---------------------------------------------------------------------------

/// Tracks required `use` imports, deduplicating automatically.
struct ImportSet {
    imports: BTreeSet<String>,
}

impl ImportSet {
    fn new() -> Self {
        Self {
            imports: BTreeSet::new(),
        }
    }

    fn add(&mut self, path: &str) {
        self.imports.insert(path.to_string());
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for import in &self.imports {
            writeln!(out, "use {import};").unwrap();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::idl;

    #[test]
    fn enum_variant_name_handles_invalid_identifiers() {
        // Normal values camel-case as usual.
        assert_eq!(enum_variant_name("no-cors"), "NoCors");
        assert_eq!(enum_variant_name("webgl2"), "Webgl2");
        // Leading-digit values (e.g. the HTML spec's "2d" context id) can't
        // start a Rust identifier and must be prefixed.
        let v = enum_variant_name("2d");
        assert_eq!(v, "_2d");
        assert!(!v.starts_with(|c: char| c.is_ascii_digit()));
        // Empty input and punctuation-only input both fall back to a valid name.
        assert_eq!(enum_variant_name(""), "Empty");
        assert_eq!(enum_variant_name("-"), "Empty");
    }

    #[test]
    fn detect_interface_ref_is_deterministic_and_prefers_earliest() {
        // "null or an element in a different node tree" names both Element and
        // Node; the spec means Element. Earliest-mention selection picks it, and
        // does so deterministically regardless of HashSet iteration order.
        let mut names = HashSet::new();
        names.insert("Node".to_string());
        names.insert("Element".to_string());
        names.insert("DocumentFragment".to_string());
        let desc = "null or an element in a different node tree";
        assert_eq!(
            detect_interface_ref(desc, "DocumentFragment", &names).as_deref(),
            Some("Element")
        );
        // Tie at the same position resolves to the longer, more specific name.
        let mut names = HashSet::new();
        names.insert("Event".to_string());
        names.insert("EventTarget".to_string());
        assert_eq!(
            detect_interface_ref("a potential event target", "Foo", &names).as_deref(),
            Some("EventTarget")
        );
    }

    #[test]
    fn generated_enum_with_leading_digit_value_parses() {
        // Regression: the HTML spec defines `enum OffscreenRenderingContextId
        // { "2d", ... }`. The generated variant must be a valid identifier.
        let idl_text = r#"
enum OffscreenRenderingContextId { "2d", "bitmaprenderer", "webgl", "webgl2" };
        "#;
        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://html.spec.whatwg.org/", &Default::default());
        let enums = files.iter().find(|f| f.filename == "enums.rs").unwrap();
        // The invalid bare `2d` variant must never appear.
        assert!(!enums.content.contains("    2d,"));
        assert!(!enums.content.contains("Self::2d"));
        assert!(enums.content.contains("_2d"));
    }

    #[test]
    fn generate_simple_interface() {
        let idl_text = r#"
[Exposed=Window]
interface URL {
  constructor(USVString url, optional USVString base);
  stringifier attribute USVString href;
  readonly attribute USVString origin;
  USVString toJSON();
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://url.spec.whatwg.org/", &Default::default());

        // Should have url.rs and lib.rs
        assert!(files.iter().any(|f| f.filename == "url.rs"));
        assert!(files.iter().any(|f| f.filename == "lib.rs"));

        let url_file = files.iter().find(|f| f.filename == "url.rs").unwrap();
        assert!(url_file.content.contains("#[webidl_interface]"));
        assert!(url_file.content.contains("#[webidl_methods]"));
        assert!(url_file.content.contains("#[constructor]"));
        assert!(url_file.content.contains("#[getter]"));
        assert!(url_file.content.contains("fn to_json"));
        assert!(url_file.content.contains("todo!()"));
    }

    #[test]
    fn generate_dictionary() {
        let idl_text = r#"
dictionary ResponseInit {
  unsigned short status = 200;
  ByteString statusText = "";
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://fetch.spec.whatwg.org/",
            &Default::default(),
        );

        let dict_file = files
            .iter()
            .find(|f| f.filename == "response_init.rs")
            .unwrap();
        assert!(dict_file.content.contains("#[webidl_dictionary]"));
        assert!(dict_file.content.contains("pub status: u16"));
        assert!(dict_file.content.contains("pub status_text: String"));
    }

    #[test]
    fn generate_enum_file() {
        let idl_text = r#"
enum RequestMode { "navigate", "same-origin", "no-cors", "cors" };
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://fetch.spec.whatwg.org/",
            &Default::default(),
        );

        let enum_file = files.iter().find(|f| f.filename == "enums.rs").unwrap();
        assert!(enum_file.content.contains("pub enum RequestMode"));
        assert!(enum_file.content.contains("Navigate"));
        assert!(enum_file.content.contains("SameOrigin"));
        assert!(enum_file.content.contains("NoCors"));
        assert!(enum_file.content.contains("Cors"));
        assert!(enum_file
            .content
            .contains("impl fmt::Display for RequestMode"));
        assert!(enum_file
            .content
            .contains("impl std::str::FromStr for RequestMode"));
        assert!(enum_file
            .content
            .contains("impl<'s> FromJSVal<'s> for RequestMode"));
        assert!(enum_file
            .content
            .contains("impl<'s> ToJSVal<'s> for RequestMode"));
    }

    #[test]
    fn handle_value_in_any_params() {
        let idl_text = r#"
[Exposed=Window]
interface Foo {
  constructor();
  undefined bar(any value);
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://example.com/", &Default::default());

        let foo_file = files.iter().find(|f| f.filename == "foo.rs").unwrap();
        // Must use HandleValue, never bare Value
        assert!(foo_file.content.contains("HandleValue"));
        assert!(foo_file.content.contains("use js::prelude::HandleValue;"));
    }

    #[test]
    fn interface_with_class_section_doc_comment() {
        let idl_text = r#"
[Exposed=Window]
interface WritableStream {
  constructor();
  readonly attribute boolean locked;
};
        "#;

        let mut spec_defs = SpecDefinitions::default();
        spec_defs
            .class_sections
            .insert("WritableStream".to_string(), "ws-class".to_string());
        spec_defs.member_fragments.insert(
            ("WritableStream".to_string(), "constructor".to_string()),
            "ws-constructor".to_string(),
        );
        spec_defs.member_fragments.insert(
            ("WritableStream".to_string(), "locked".to_string()),
            "ws-locked".to_string(),
        );

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &spec_defs).unwrap();
        let files = generate(&model, "https://streams.spec.whatwg.org/", &spec_defs);

        let ws_file = files
            .iter()
            .find(|f| f.filename == "writable_stream.rs")
            .unwrap();

        // Struct should have class section doc comment
        assert!(ws_file
            .content
            .contains("/// <https://streams.spec.whatwg.org/#ws-class>"));
        // Constructor should have real fragment ID
        assert!(ws_file
            .content
            .contains("/// <https://streams.spec.whatwg.org/#ws-constructor>"));
        // Getter should have real fragment ID
        assert!(ws_file
            .content
            .contains("/// <https://streams.spec.whatwg.org/#ws-locked>"));
    }

    #[test]
    fn interface_with_internal_slots() {
        let idl_text = r#"
[Exposed=Window]
interface WritableStream {
  constructor();
};
        "#;

        let mut spec_defs = SpecDefinitions::default();
        spec_defs.internal_slots.insert(
            "WritableStream".to_string(),
            vec![
                idl::InternalSlot {
                    name: "backpressure".to_string(),
                    description: "A boolean indicating the backpressure signal".to_string(),
                    fragment_id: "writablestream-backpressure".to_string(),
                },
                idl::InternalSlot {
                    name: "storedError".to_string(),
                    description: "A value indicating how the stream failed".to_string(),
                    fragment_id: "writablestream-storederror".to_string(),
                },
                idl::InternalSlot {
                    name: "state".to_string(),
                    description: "A string containing the stream's current state".to_string(),
                    fragment_id: "writablestream-state".to_string(),
                },
                idl::InternalSlot {
                    name: "writeRequests".to_string(),
                    description: "A list of pending write requests".to_string(),
                    fragment_id: "writablestream-writerequests".to_string(),
                },
            ],
        );

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &spec_defs).unwrap();
        let files = generate(&model, "https://streams.spec.whatwg.org/", &spec_defs);

        let ws_file = files
            .iter()
            .find(|f| f.filename == "writable_stream.rs")
            .unwrap();

        // Boolean slot → bool type
        assert!(ws_file.content.contains("backpressure: bool,"));
        // Value slot → Heap<Value>
        assert!(ws_file.content.contains("stored_error: Heap<Value>,"));
        // State slot → u8 with TODO comment
        assert!(ws_file.content.contains("state: u8,"));
        assert!(ws_file
            .content
            .contains("// TODO: Define a state enum for this field"));
        // List slot → Vec<Heap<Value>>
        assert!(ws_file
            .content
            .contains("write_requests: Vec<Heap<Value>>,"));
        // Should NOT have the TODO placeholder
        assert!(!ws_file
            .content
            .contains("// TODO: Add internal state fields"));
        // Should have Heap import
        assert!(ws_file.content.contains("use js::gc::handle::Heap;"));
    }

    #[test]
    fn dictionary_with_spec_link() {
        let idl_text = r#"
dictionary QueuingStrategy {
  unrestricted double highWaterMark;
};
        "#;

        let mut spec_defs = SpecDefinitions::default();
        spec_defs.dictionary_fragments.insert(
            "QueuingStrategy".to_string(),
            "dictdef-queuingstrategy".to_string(),
        );

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &spec_defs).unwrap();
        let files = generate(&model, "https://streams.spec.whatwg.org/", &spec_defs);

        let dict_file = files
            .iter()
            .find(|f| f.filename == "queuing_strategy.rs")
            .unwrap();

        assert!(dict_file
            .content
            .contains("/// <https://streams.spec.whatwg.org/#dictdef-queuingstrategy>"));
        assert!(dict_file.content.contains("#[webidl_dictionary]"));
    }

    #[test]
    fn lookup_fragment_uses_spec_defs() {
        let mut spec_defs = SpecDefinitions::default();
        spec_defs.member_fragments.insert(
            ("WritableStream".to_string(), "constructor".to_string()),
            "ws-constructor".to_string(),
        );

        // Found in spec_defs
        assert_eq!(
            lookup_fragment(&spec_defs, "WritableStream", "constructor"),
            "ws-constructor"
        );

        // Not found → falls back to dom-X-Y pattern
        assert_eq!(
            lookup_fragment(&spec_defs, "WritableStream", "unknown"),
            "dom-writablestream-unknown"
        );
    }

    #[test]
    fn conflicting_method_names_get_js_prefix() {
        let idl_text = r#"
[Exposed=Window]
interface Blob {
  constructor();
  Blob clone();
  undefined from();
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://example.com/", &Default::default());

        let blob_file = files.iter().find(|f| f.filename == "blob.rs").unwrap();
        // "clone" conflicts with Clone::clone, should be renamed to js_clone
        assert!(blob_file.content.contains("fn js_clone"));
        assert!(blob_file.content.contains("name = \"clone\""));
        // "from" conflicts with From::from
        assert!(blob_file.content.contains("fn js_from"));
        assert!(blob_file.content.contains("name = \"from\""));
    }

    #[test]
    fn throwing_constructor_generates_setup_style() {
        let idl_text = r#"
[Exposed=Window]
interface Headers {
  constructor();
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();

        // Manually create a model with throwing constructor steps
        let mut model_with_throw = model;
        model_with_throw.interfaces[0].constructor = Some(Constructor {
            params: vec![],
            algorithm_steps: vec!["If init is not valid, throw a TypeError.".to_string()],
        });

        let files = generate(
            &model_with_throw,
            "https://fetch.spec.whatwg.org/",
            &Default::default(),
        );

        let headers_file = files.iter().find(|f| f.filename == "headers.rs").unwrap();
        // Setup-style: receives &self and scope
        assert!(headers_file.content.contains("&self, scope: &Scope<'_>"));
        assert!(headers_file.content.contains("Result<(), ExnThrown>"));
    }

    #[test]
    fn non_throwing_constructor_generates_old_style() {
        let idl_text = r#"
[Exposed=Window]
interface Simple {
  constructor(DOMString name);
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://example.com/", &Default::default());

        let file = files.iter().find(|f| f.filename == "simple.rs").unwrap();
        assert!(file.content.contains("fn new(name: String) -> Self"));
        assert!(!file.content.contains("&self, scope"));
    }

    #[test]
    fn global_interfaces_go_to_globals_rs() {
        let idl_text = r#"
[Exposed=Window]
partial interface Window {
  undefined alert(DOMString message);
};

[Exposed=Window]
interface URL {
  constructor(USVString url);
  readonly attribute USVString href;
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://html.spec.whatwg.org/", &Default::default());

        // Window should NOT get its own window.rs
        assert!(!files.iter().any(|f| f.filename == "window.rs"));
        // But globals.rs should exist
        let globals_file = files.iter().find(|f| f.filename == "globals.rs").unwrap();
        assert!(globals_file.content.contains("#[jsglobals]"));
        assert!(globals_file.content.contains("fn alert"));

        // URL should still get its own file
        assert!(files.iter().any(|f| f.filename == "url.rs"));

        // lib.rs should call globals::globals::add_to_global, not window::Window
        let lib_file = files.iter().find(|f| f.filename == "lib.rs").unwrap();
        assert!(
            lib_file
                .content
                .contains("globals::globals::add_to_global(scope, global);"),
            "lib.rs should call globals module:\n{}",
            lib_file.content
        );
        assert!(
            !lib_file.content.contains("window::Window::add_to_global"),
            "lib.rs should not reference Window interface directly:\n{}",
            lib_file.content
        );
    }

    #[test]
    fn callbacks_generate_function_type_aliases() {
        let idl_text = r#"
callback UnderlyingSourceStartCallback = any (ReadableStreamController controller);
callback UnderlyingSourcePullCallback = Promise<undefined> (ReadableStreamController controller);
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://streams.spec.whatwg.org/",
            &Default::default(),
        );

        let cb_file = files.iter().find(|f| f.filename == "callbacks.rs").unwrap();
        assert!(cb_file.content.contains("use js::Function;"));
        assert!(cb_file
            .content
            .contains("pub type UnderlyingSourceStartCallback<'s> = Function<'s>;"));
        assert!(cb_file
            .content
            .contains("pub type UnderlyingSourcePullCallback<'s> = Function<'s>;"));
    }

    #[test]
    fn static_method_name_collision_with_instance() {
        let idl_text = r#"
[Exposed=Window]
interface Response {
  constructor();
  Response clone();
  static Response redirect(USVString url);
  static Response json(any data);
  Response json();
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://fetch.spec.whatwg.org/",
            &Default::default(),
        );

        let resp_file = files.iter().find(|f| f.filename == "response.rs").unwrap();
        // "json" exists as both instance and static — static should get static_ prefix
        assert!(resp_file.content.contains("fn static_json"));
        assert!(resp_file
            .content
            .contains("#[static_method(name = \"json\")]"));
        // "redirect" is only static, no collision
        assert!(resp_file.content.contains("fn redirect"));
    }

    #[test]
    fn algorithm_functions_are_pub_crate() {
        use crate::extract::{AlgorithmKind, AlgorithmSteps};

        let idl_text = "";
        let algorithms = vec![AlgorithmSteps {
            heading: "create a readable stream".to_string(),
            kind: AlgorithmKind::Standalone {
                name: "create a readable stream".to_string(),
            },
            steps: vec!["Do something.".to_string()],
            interface: String::new(),
            fragment: String::new(),
        }];

        let model =
            idl::parse_idl(&[idl_text.to_string()], &algorithms, &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://streams.spec.whatwg.org/",
            &Default::default(),
        );

        let algo_file = files
            .iter()
            .find(|f| f.filename == "algorithms.rs")
            .unwrap();
        assert!(algo_file
            .content
            .contains("pub(crate) fn create_a_readable_stream"));
    }

    #[test]
    fn algorithm_doc_comment_includes_spec_link() {
        use crate::extract::{AlgorithmKind, AlgorithmSteps};

        let algorithms = vec![AlgorithmSteps {
            heading: "To slice blob, run these steps:".to_string(),
            kind: AlgorithmKind::Standalone {
                name: "slice blob".to_string(),
            },
            steps: vec!["Let blob be a new Blob.".to_string()],
            interface: String::new(),
            fragment: "slice-blob".to_string(),
        }];

        let model = idl::parse_idl(&[], &algorithms, &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://w3c.github.io/FileAPI/",
            &Default::default(),
        );

        let algo_file = files
            .iter()
            .find(|f| f.filename == "algorithms.rs")
            .unwrap();
        assert!(
            algo_file
                .content
                .contains("/// <https://w3c.github.io/FileAPI/#slice-blob>"),
            "Expected spec link in algorithm doc comment, got:\n{}",
            algo_file.content
        );
    }

    #[test]
    fn algorithm_without_fragment_omits_spec_link() {
        use crate::extract::{AlgorithmKind, AlgorithmSteps};

        let algorithms = vec![AlgorithmSteps {
            heading: "To do something:".to_string(),
            kind: AlgorithmKind::Standalone {
                name: "do something".to_string(),
            },
            steps: vec!["Do it.".to_string()],
            interface: String::new(),
            fragment: String::new(),
        }];

        let model = idl::parse_idl(&[], &algorithms, &Default::default()).unwrap();
        let files = generate(&model, "https://example.com/spec/", &Default::default());

        let algo_file = files
            .iter()
            .find(|f| f.filename == "algorithms.rs")
            .unwrap();
        // Should not have a spec link line when fragment is empty
        assert!(
            !algo_file
                .content
                .contains("/// <https://example.com/spec/#>"),
            "Should not emit empty fragment link, got:\n{}",
            algo_file.content
        );
    }

    #[test]
    fn internal_slot_state_enum_extraction() {
        let idl_text = r#"
[Exposed=Window]
interface ReadableStream {
  constructor();
};
        "#;

        let mut spec_defs = SpecDefinitions::default();
        spec_defs.internal_slots.insert(
            "ReadableStream".to_string(),
            vec![idl::InternalSlot {
                name: "state".to_string(),
                description: "A string indicating the stream's state, one of \"readable\", \"closed\", or \"errored\"".to_string(),
                fragment_id: "readablestream-state".to_string(),
            }],
        );

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &spec_defs).unwrap();
        let files = generate(&model, "https://streams.spec.whatwg.org/", &spec_defs);

        let rs_file = files
            .iter()
            .find(|f| f.filename == "readable_stream.rs")
            .unwrap();
        // Field should use the enum type, not u8
        assert!(rs_file.content.contains("state: ReadableStreamState,"));
        // Enum should be emitted at module level with derive and variants
        assert!(rs_file.content.contains("pub enum ReadableStreamState {"));
        assert!(rs_file.content.contains("#[default]"));
        assert!(rs_file.content.contains("    Readable,"));
        assert!(rs_file.content.contains("    Closed,"));
        assert!(rs_file.content.contains("    Errored,"));
        // Should have Display, FromJSVal, ToJSVal impls
        assert!(rs_file
            .content
            .contains("impl fmt::Display for ReadableStreamState"));
        assert!(rs_file
            .content
            .contains("impl<'s> FromJSVal<'s> for ReadableStreamState"));
        assert!(rs_file
            .content
            .contains("impl<'s> ToJSVal<'s> for ReadableStreamState"));
    }

    #[test]
    fn interface_type_references_get_lifetime() {
        let idl_text = r#"
[Exposed=Window]
interface URL {
  constructor(USVString url);
  static URL? parse(USVString url, optional USVString base);
};

[Exposed=Window]
interface URLSearchParams {
  constructor(optional USVString init = "");
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://url.spec.whatwg.org/", &Default::default());

        let url_file = files.iter().find(|f| f.filename == "url.rs").unwrap();
        // Static method returning Option<URL> should have lifetime
        assert!(url_file.content.contains("Option<URL<'r>>"));
        // Should have 'r lifetime on scope
        assert!(url_file.content.contains("scope: &'r Scope<'_>"));
    }

    #[test]
    fn cross_interface_refs_generate_imports() {
        let idl_text = r#"
[Exposed=Window]
interface URL {
  constructor(USVString url);
  readonly attribute URLSearchParams searchParams;
};

[Exposed=Window]
interface URLSearchParams {
  constructor(optional USVString init = "");
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://url.spec.whatwg.org/", &Default::default());

        let url_file = files.iter().find(|f| f.filename == "url.rs").unwrap();
        // Should import URLSearchParams from sibling module
        assert!(url_file
            .content
            .contains("use super::url_search_params::URLSearchParams;"));
        // The attribute type should have lifetime
        assert!(url_file.content.contains("URLSearchParams<'r>"));

        // URLSearchParams file should NOT import URL (it doesn't reference it)
        let usp_file = files
            .iter()
            .find(|f| f.filename == "url_search_params.rs")
            .unwrap();
        assert!(!usp_file.content.contains("use super::url::URL;"));
    }

    #[test]
    fn internal_slot_typed_interface_ref() {
        let idl_text = r#"
[Exposed=Window]
interface AbortController {
  constructor();
};

[Exposed=Window]
interface AbortSignal : EventTarget {
  constructor();
};
        "#;

        let mut spec_defs = SpecDefinitions::default();
        spec_defs.internal_slots.insert(
            "AbortController".to_string(),
            vec![idl::InternalSlot {
                name: "signal".to_string(),
                description: "(an AbortSignal object).".to_string(),
                fragment_id: "abortcontroller-signal".to_string(),
            }],
        );

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &spec_defs).unwrap();
        let files = generate(&model, "https://dom.spec.whatwg.org/", &spec_defs);

        let ac_file = files
            .iter()
            .find(|f| f.filename == "abort_controller.rs")
            .unwrap();

        // Should use Heap<AbortSignalImpl> instead of Heap<Value>
        assert!(
            ac_file.content.contains("signal: Heap<AbortSignalImpl>,"),
            "Expected Heap<AbortSignalImpl> but got:\n{}",
            ac_file.content
        );
        // Should import Heap
        assert!(ac_file.content.contains("use js::gc::handle::Heap;"));
        // Should NOT import MozHeap (only slot references a known interface)
        assert!(!ac_file.content.contains("MozHeap"));
        // Should import AbortSignalImpl from sibling module (for Heap<AbortSignalImpl>)
        assert!(ac_file
            .content
            .contains("use super::abort_signal::AbortSignalImpl;"));
    }

    #[test]
    fn internal_slot_nullable_interface_ref() {
        let idl_text = r#"
[Exposed=Window]
interface ReadableStream {
  constructor();
};

[Exposed=Window]
interface ReadableStreamDefaultController {
  constructor();
};
        "#;

        let mut spec_defs = SpecDefinitions::default();
        spec_defs.internal_slots.insert(
            "ReadableStream".to_string(),
            vec![idl::InternalSlot {
                name: "controller".to_string(),
                description: "(null or a ReadableStreamDefaultController object).".to_string(),
                fragment_id: "readablestream-controller".to_string(),
            }],
        );

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &spec_defs).unwrap();
        let files = generate(&model, "https://streams.spec.whatwg.org/", &spec_defs);

        let rs_file = files
            .iter()
            .find(|f| f.filename == "readable_stream.rs")
            .unwrap();

        // Nullable interface ref should be Option<Heap<...>>
        assert!(
            rs_file
                .content
                .contains("controller: Option<Heap<ReadableStreamDefaultControllerImpl>>,"),
            "Expected Option<Heap<...>> but got:\n{}",
            rs_file.content
        );
    }

    #[test]
    fn empty_globals_not_emitted() {
        // DOM spec has EventTarget in [Exposed=Window] but it's not a global
        // interface. If there are no methods/constants on Window etc., no
        // globals.rs should be generated.
        let idl_text = r#"
[Exposed=Window]
interface EventTarget {
  constructor();
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://dom.spec.whatwg.org/", &Default::default());

        assert!(
            !files.iter().any(|f| f.filename == "globals.rs"),
            "globals.rs should not be emitted when empty"
        );
    }

    #[test]
    fn extends_syntax_uses_parentheses() {
        let idl_text = r#"
[Exposed=Window]
interface EventTarget {
  constructor();
};

[Exposed=Window]
interface AbortSignal : EventTarget {
  constructor();
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://dom.spec.whatwg.org/", &Default::default());

        let as_file = files
            .iter()
            .find(|f| f.filename == "abort_signal.rs")
            .unwrap();

        // Should use #[webidl_interface(extends = EventTarget)], not
        // #[webidl_interface, extends = EventTarget]
        assert!(
            as_file
                .content
                .contains("#[webidl_interface(extends = EventTarget)]"),
            "Expected parenthesized extends but got:\n{}",
            as_file.content
        );
        assert!(!as_file.content.contains("#[webidl_interface,"));
    }

    #[test]
    fn dictionary_field_uses_interface_type() {
        let idl_text = r#"
[Exposed=Window]
interface AbortSignal : EventTarget {
  constructor();
};

[Exposed=Window]
interface EventTarget {
  constructor();
};

dictionary AddEventListenerOptions {
  AbortSignal signal;
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://dom.spec.whatwg.org/", &Default::default());

        let dict_file = files
            .iter()
            .find(|f| f.filename == "add_event_listener_options.rs")
            .unwrap();

        // AbortSignal field uses the stack newtype (`AbortSignal<'a>`) since
        // `#[webidl_dictionary]` needs `FromJSVal` on the field type, which the
        // stack newtype provides and `Heap<AbortSignalImpl>` does not.
        assert!(
            dict_file
                .content
                .contains("pub signal: Option<AbortSignal<'a>>,"),
            "Expected Option<AbortSignal<'a>> but got:\n{}",
            dict_file.content
        );
        // The struct gains a lifetime parameter for the rooted field.
        assert!(dict_file
            .content
            .contains("pub struct AddEventListenerOptions<'a>"));
        // Import the stack newtype, not the inner Impl.
        assert!(dict_file
            .content
            .contains("use super::abort_signal::AbortSignal;"));
        assert!(!dict_file.content.contains("AbortSignalImpl"));
    }

    #[test]
    fn indirect_interface_ref_in_slot() {
        // "potential event target" indirectly references EventTarget
        let idl_text = r#"
[Exposed=Window]
interface Event {
  constructor(DOMString type);
};

[Exposed=Window]
interface EventTarget {
  constructor();
};
        "#;

        let mut spec_defs = SpecDefinitions::default();
        spec_defs.internal_slots.insert(
            "Event".to_string(),
            vec![idl::InternalSlot {
                name: "target".to_string(),
                description: "(a potential event target). Unless stated otherwise it is null."
                    .to_string(),
                fragment_id: "event-target".to_string(),
            }],
        );

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &spec_defs).unwrap();
        let files = generate(&model, "https://dom.spec.whatwg.org/", &spec_defs);

        let event_file = files.iter().find(|f| f.filename == "event.rs").unwrap();

        // "potential event target" should resolve to EventTarget via indirect match,
        // and "null" in description makes it nullable
        assert!(
            event_file
                .content
                .contains("target: Option<Heap<EventTargetImpl>>,"),
            "Expected Option<Heap<EventTargetImpl>> but got:\n{}",
            event_file.content
        );
    }

    #[test]
    fn pascal_to_words_conversion() {
        assert_eq!(pascal_to_words("EventTarget"), "event target");
        assert_eq!(pascal_to_words("AbortSignal"), "abort signal");
        assert_eq!(
            pascal_to_words("ReadableStreamDefaultController"),
            "readable stream default controller"
        );
        assert_eq!(pascal_to_words("URL"), "u r l");
        assert_eq!(pascal_to_words("Event"), "event");
    }

    #[test]
    fn list_slot_with_interface_ref() {
        // "(a list of AbortSignal objects)" should produce Vec<Heap<AbortSignalImpl>>
        let idl_text = r#"
[Exposed=Window]
interface AbortController {
  constructor();
};

[Exposed=Window]
interface AbortSignal {
  constructor();
};
        "#;

        let mut spec_defs = SpecDefinitions::default();
        spec_defs.internal_slots.insert(
            "AbortController".to_string(),
            vec![idl::InternalSlot {
                name: "dependent signals".to_string(),
                description: "(a list of AbortSignal objects).".to_string(),
                fragment_id: "abortcontroller-dependent-signals".to_string(),
            }],
        );

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &spec_defs).unwrap();
        let files = generate(&model, "https://dom.spec.whatwg.org/", &spec_defs);

        let ac_file = files
            .iter()
            .find(|f| f.filename == "abort_controller.rs")
            .unwrap();

        // Should be a Vec with typed interface ref, not Vec<Heap<Value>>
        assert!(
            ac_file
                .content
                .contains("dependent_signals: Vec<Heap<AbortSignalImpl>>,"),
            "Expected Vec<Heap<AbortSignalImpl>> but got:\n{}",
            ac_file.content
        );
    }

    #[test]
    fn getter_returning_handle_value_gets_scope_param() {
        let idl_text = r#"
[Exposed=Window]
interface AbortSignal : EventTarget {
  constructor();
  readonly attribute any reason;
};

[Exposed=Window]
interface EventTarget {
  constructor();
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://dom.spec.whatwg.org/", &Default::default());

        let file = files
            .iter()
            .find(|f| f.filename == "abort_signal.rs")
            .unwrap();

        // Getter returning HandleValue<'_> should have lifetime tied to scope
        assert!(
            file.content
                .contains("fn reason<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r>"),
            "Expected lifetime on HandleValue getter:\n{}",
            file.content
        );
    }

    #[test]
    fn getter_returning_interface_type_gets_lifetime() {
        let idl_text = r#"
[Exposed=Window]
interface Event {
  constructor(DOMString type);
  readonly attribute EventTarget? target;
};

[Exposed=Window]
interface EventTarget {
  constructor();
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://dom.spec.whatwg.org/", &Default::default());

        let file = files.iter().find(|f| f.filename == "event.rs").unwrap();

        // Getter returning an interface type should have lifetime 'r tied to scope
        assert!(
            file.content
                .contains("fn target<'r>(&self, scope: &'r Scope<'_>) -> Option<EventTarget<'r>>"),
            "Expected lifetime on interface getter:\n{}",
            file.content
        );
    }

    #[test]
    fn extends_generates_parent_field() {
        let idl_text = r#"
[Exposed=Window]
interface Event {
  constructor(DOMString type);
};

[Exposed=Window]
interface CustomEvent : Event {
  constructor(DOMString type);
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://dom.spec.whatwg.org/", &Default::default());

        let file = files
            .iter()
            .find(|f| f.filename == "custom_event.rs")
            .unwrap();

        // Should have parent: Heap<EventImpl> as first field
        assert!(
            file.content.contains("parent: Heap<EventImpl>,"),
            "Expected parent field:\n{}",
            file.content
        );
        // Should import Heap
        assert!(file.content.contains("use js::gc::handle::Heap;"));
        // Should import EventImpl from sibling module
        assert!(file.content.contains("use super::event::EventImpl;"));
    }

    #[test]
    fn dictionary_any_field_uses_handle_value() {
        let idl_text = r#"
[Exposed=Window]
interface Event {
  constructor(DOMString type);
};

dictionary CustomEventInit {
  any detail = null;
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://dom.spec.whatwg.org/", &Default::default());

        let file = files
            .iter()
            .find(|f| f.filename == "custom_event_init.rs")
            .unwrap();

        // `any` becomes HandleValue<'a> — the dictionary gains a lifetime
        // parameter for the rooted handle, and `FromJSVal` on `HandleValue`
        // gives the deserialization the macro needs.
        assert!(
            file.content
                .contains("pub detail: Option<HandleValue<'a>>,"),
            "Expected Option<HandleValue<'a>> for any field with null default:\n{}",
            file.content
        );
        assert!(file.content.contains("pub struct CustomEventInit<'a>"));
        assert!(file.content.contains("use js::prelude::HandleValue;"));
    }

    #[test]
    fn cross_dictionary_imports_in_interface() {
        let idl_text = r#"
[Exposed=Window]
interface CustomEvent : Event {
  constructor(DOMString type, optional CustomEventInit eventInitDict);
};

[Exposed=Window]
interface Event {
  constructor(DOMString type);
};

dictionary CustomEventInit {
  any detail = null;
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://dom.spec.whatwg.org/", &Default::default());

        let file = files
            .iter()
            .find(|f| f.filename == "custom_event.rs")
            .unwrap();

        // Should import the dictionary type from its sibling module
        assert!(
            file.content
                .contains("use super::custom_event_init::CustomEventInit;"),
            "Expected CustomEventInit import:\n{}",
            file.content
        );
    }

    #[test]
    fn method_lifetime_uses_r_not_s() {
        let idl_text = r#"
[Exposed=Window]
interface Event {
  constructor(DOMString type);
  sequence<EventTarget> composedPath();
};

[Exposed=Window]
interface EventTarget {
  constructor();
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://dom.spec.whatwg.org/", &Default::default());

        let file = files.iter().find(|f| f.filename == "event.rs").unwrap();

        // Lifetime should be 'r, not 's (which collides with stack newtypes)
        assert!(
            file.content.contains("<'r>"),
            "Expected lifetime 'r, not 's:\n{}",
            file.content
        );
        assert!(
            !file.content.contains("<'s>"),
            "Should not use 's lifetime:\n{}",
            file.content
        );
        assert!(file.content.contains("scope: &'r Scope<'_>"));
    }

    #[test]
    fn global_method_without_webidl_creates_globals_rs() {
        use crate::extract::{AlgorithmKind, AlgorithmSteps};

        // No WebIDL — only method algorithms on WindowOrWorkerGlobalScope
        let algorithms = vec![AlgorithmSteps {
            heading: "The structuredClone(value, options) method steps are:".to_string(),
            kind: AlgorithmKind::Method {
                name: "structuredClone".to_string(),
                is_static: false,
            },
            steps: vec!["Let serialized be ...".to_string()],
            interface: "WindowOrWorkerGlobalScope".to_string(),
            fragment: String::new(),
        }];

        let model = idl::parse_idl(&[], &algorithms, &Default::default()).unwrap();

        // Model should contain a synthetic WindowOrWorkerGlobalScope interface
        let iface = model
            .interfaces
            .iter()
            .find(|i| i.name == "WindowOrWorkerGlobalScope");
        assert!(iface.is_some(), "Expected synthetic global interface");
        let iface = iface.unwrap();
        assert_eq!(iface.methods.len(), 1);
        assert_eq!(iface.methods[0].name, "structuredClone");

        // structuredClone should NOT be in standalone algorithms
        assert!(
            !model.algorithms.iter().any(|a| a.name == "structuredClone"),
            "Global method should not be a standalone algorithm"
        );

        // Generate files
        let files = generate(&model, "https://html.spec.whatwg.org/", &Default::default());

        // Should produce globals.rs with #[jsglobals]
        let globals_file = files.iter().find(|f| f.filename == "globals.rs");
        assert!(globals_file.is_some(), "Expected globals.rs");
        let globals = &globals_file.unwrap().content;
        assert!(globals.contains("#[jsglobals]"));
        assert!(globals.contains("structured_clone"));

        // lib.rs should reference the globals module
        let lib_file = files.iter().find(|f| f.filename == "lib.rs").unwrap();
        assert!(lib_file.content.contains("pub mod globals;"));
        assert!(lib_file
            .content
            .contains("globals::globals::add_to_global(scope, global);"));
    }

    #[test]
    fn external_parent_extends_emitted() {
        // EventTarget is not defined in this spec, but we still emit
        // `extends` and the parent field so the dependency is visible.
        let idl_text = r#"
[Exposed=Window]
interface FileReader : EventTarget {
  constructor();
  readonly attribute unsigned short readyState;
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://w3c.github.io/FileAPI/",
            &Default::default(),
        );

        let reader = files
            .iter()
            .find(|f| f.filename == "file_reader.rs")
            .unwrap();
        assert!(
            reader.content.contains("extends = EventTarget"),
            "should emit extends even for external parent"
        );
        assert!(
            reader.content.contains("parent: Heap<EventTargetImpl>"),
            "should emit parent field even for external parent"
        );
    }

    #[test]
    fn in_spec_parent_extends_emitted() {
        // When the parent IS defined in this spec, extends should be emitted.
        let idl_text = r#"
[Exposed=Window]
interface Blob {
  constructor();
  readonly attribute unsigned long long size;
};

[Exposed=Window]
interface File : Blob {
  constructor(USVString name);
  readonly attribute DOMString name;
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://w3c.github.io/FileAPI/",
            &Default::default(),
        );

        let file = files.iter().find(|f| f.filename == "file.rs").unwrap();
        assert!(
            file.content.contains("extends = Blob"),
            "should emit extends for in-spec parent"
        );
        assert!(
            file.content.contains("parent: Heap<BlobImpl>"),
            "should emit parent field for in-spec parent"
        );
    }

    #[test]
    fn unknown_types_replaced_with_handle_value() {
        // ReadableStream is not defined in this spec — it should be replaced
        // with HandleValue<'_>.
        let idl_text = r#"
[Exposed=Window]
interface Blob {
  constructor();
  ReadableStream stream();
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://w3c.github.io/FileAPI/",
            &Default::default(),
        );

        let blob = files.iter().find(|f| f.filename == "blob.rs").unwrap();
        assert!(
            blob.content.contains("HandleValue<'r>"),
            "should replace unknown return type with HandleValue"
        );
        assert!(
            blob.content.contains("// returns WebIDL: ReadableStream"),
            "should note the original WebIDL type in a comment"
        );
    }

    #[test]
    fn enum_imported_in_dictionary() {
        let idl_text = r#"
enum EndingType { "transparent", "native" };

dictionary BlobPropertyBag {
  DOMString type = "";
  EndingType endings = "transparent";
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://w3c.github.io/FileAPI/",
            &Default::default(),
        );

        let dict = files
            .iter()
            .find(|f| f.filename == "blob_property_bag.rs")
            .unwrap();
        assert!(
            dict.content.contains("use super::enums::EndingType;"),
            "should import enum type from enums module"
        );
    }

    #[test]
    fn enum_default_in_dictionary() {
        let idl_text = r#"
enum EndingType { "transparent", "native" };

dictionary BlobPropertyBag {
  DOMString type = "";
  EndingType endings = "transparent";
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://w3c.github.io/FileAPI/",
            &Default::default(),
        );

        let dict = files
            .iter()
            .find(|f| f.filename == "blob_property_bag.rs")
            .unwrap();
        assert!(
            dict.content.contains("EndingType::Transparent"),
            "should convert enum string default to variant expression"
        );
        assert!(
            !dict.content.contains("default = \"transparent\""),
            "should not emit raw string default for enum type"
        );
    }

    #[test]
    fn string_default_empty() {
        let idl_text = r#"
dictionary BlobPropertyBag {
  DOMString type = "";
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://w3c.github.io/FileAPI/",
            &Default::default(),
        );

        let dict = files
            .iter()
            .find(|f| f.filename == "blob_property_bag.rs")
            .unwrap();
        assert!(
            dict.content.contains("String::new()"),
            "should convert empty string default to String::new()"
        );
    }

    #[test]
    fn typedef_imported_in_interface() {
        let idl_text = r#"
typedef (BufferSource or Blob or USVString) BlobPart;

[Exposed=Window]
interface Blob {
  constructor(optional sequence<BlobPart> blobParts, optional BlobPropertyBag options = {});
  readonly attribute unsigned long long size;
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(
            &model,
            "https://w3c.github.io/FileAPI/",
            &Default::default(),
        );

        let blob = files.iter().find(|f| f.filename == "blob.rs").unwrap();
        // BlobPart is a union typedef → HandleValue, so sequence<BlobPart>
        // collapses to HandleValue<'_>. The typedef import is not needed.
        // But the file should still compile.
        assert!(
            !blob.content.contains("Vec<BlobPart"),
            "should not have Vec<BlobPart> since union typedef collapses to HandleValue"
        );
    }

    #[test]
    fn vec_of_handle_value_typedef_collapses() {
        // sequence<UnionTypedef> where typedef → HandleValue should collapse
        // to just HandleValue<'_>, not Vec<TypedefName<'_>>.
        let idl_text = r#"
typedef (DOMString or unsigned long) MixedType;

[Exposed=Window]
interface Widget {
  constructor(sequence<MixedType> items);
};
        "#;

        let model = idl::parse_idl(&[idl_text.to_string()], &[], &Default::default()).unwrap();
        let files = generate(&model, "https://example.com/spec/", &Default::default());

        let widget = files.iter().find(|f| f.filename == "widget.rs").unwrap();
        assert!(
            !widget.content.contains("Vec<MixedType"),
            "should not have Vec<MixedType>"
        );
        assert!(
            widget.content.contains("HandleValue"),
            "should collapse sequence<UnionTypedef> to HandleValue"
        );
    }
}
