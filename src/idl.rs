// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Parses WebIDL text into a structured intermediate representation.
//!
//! Uses `weedle2` for parsing and builds a `SpecModel` that captures
//! interfaces, dictionaries, enums, mixins, and their members.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use weedle::interface::InterfaceMember;
use weedle::mixin::MixinMember;

use crate::extract::{AlgorithmKind, AlgorithmSteps, SpecDefinitions, Step};
use crate::types::{map_return_type, map_type, RustType};

/// Interface names that represent the global scope, not real interfaces.
/// Methods on these interfaces are installed as global functions via `#[jsglobals]`.
pub const GLOBAL_INTERFACES: &[&str] =
    &["Window", "WindowOrWorkerGlobalScope", "WorkerGlobalScope"];

/// An internal slot extracted from a spec's internal slots table.
#[derive(Debug, Clone)]
pub struct InternalSlot {
    /// The slot name without brackets (e.g., "storedError" from "[[storedError]]").
    pub name: String,
    /// The non-normative description from the spec table.
    pub description: String,
    /// The `<dfn>` fragment ID for this slot (e.g., "writablestream-storederror").
    pub fragment_id: String,
}

// ---------------------------------------------------------------------------
// Intermediate representation
// ---------------------------------------------------------------------------

/// The full model of all definitions extracted from a spec.
#[derive(Debug, Default)]
pub struct SpecModel {
    pub interfaces: Vec<Interface>,
    pub dictionaries: Vec<Dictionary>,
    pub enums: Vec<Enum>,
    pub typedefs: Vec<Typedef>,
    pub callbacks: Vec<Callback>,
    pub includes: Vec<IncludesStatement>,
    /// Standalone algorithms not tied to a specific interface member.
    pub algorithms: Vec<Algorithm>,
}

#[derive(Debug, Default)]
pub struct Interface {
    pub name: String,
    pub extends: Option<String>,
    pub constructor: Option<Constructor>,
    pub attributes: Vec<Attribute>,
    pub methods: Vec<Method>,
    pub static_methods: Vec<Method>,
    pub constants: Vec<Constant>,
    pub iterable: Option<Iterable>,
    pub is_mixin: bool,
    pub internal_slots: Vec<InternalSlot>,
}

#[derive(Debug, Clone)]
pub struct Constructor {
    pub params: Vec<Param>,
    pub algorithm_steps: Vec<Step>,
}

#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    pub rust_type: RustType,
    pub readonly: bool,
    /// Getter algorithm steps, if any.
    pub getter_steps: Vec<Step>,
    /// Setter algorithm steps, if any.
    pub setter_steps: Vec<Step>,
}

#[derive(Debug, Clone)]
pub struct Method {
    pub name: String,
    pub params: Vec<Param>,
    pub return_type: RustType,
    pub algorithm_steps: Vec<Step>,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub rust_type: RustType,
    pub optional: bool,
    pub variadic: bool,
}

#[derive(Debug, Clone)]
pub struct Constant {
    pub name: String,
    pub rust_type: RustType,
    pub value: String,
}

/// An `iterable<...>` declaration. Parsed and stored, but codegen does not yet
/// emit iterator methods for it.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Iterable {
    pub key_type: Option<RustType>,
    pub value_type: RustType,
}

#[derive(Debug)]
pub struct Dictionary {
    pub name: String,
    /// Parent dictionary name from `dictionary X : Y`. Used by
    /// [`flatten_dictionary_inheritance`] to copy inherited members in.
    pub extends: Option<String>,
    pub members: Vec<DictMember>,
}

#[derive(Debug, Clone)]
pub struct DictMember {
    pub name: String,
    pub rust_type: RustType,
    pub required: bool,
    pub default_value: Option<String>,
    /// When this member was copied in from a parent dictionary (WebIDL
    /// dictionary inheritance), the parent's name; `None` for own members.
    pub inherited_from: Option<String>,
}

#[derive(Debug)]
pub struct Enum {
    pub name: String,
    pub variants: Vec<String>,
}

#[derive(Debug)]
pub struct Typedef {
    pub name: String,
    pub rust_type: RustType,
}

#[derive(Debug)]
pub struct Callback {
    pub name: String,
    pub return_type: RustType,
    pub params: Vec<Param>,
}

/// A standalone algorithm from the spec, not tied to a specific interface member.
///
/// Examples: "basic URL parser", "API URL parser", "URL serializer".
#[derive(Debug)]
pub struct Algorithm {
    pub name: String,
    pub heading: String,
    pub steps: Vec<Step>,
    /// The URL fragment identifier for linking to this algorithm in the spec.
    pub fragment: String,
}

#[derive(Debug)]
pub struct IncludesStatement {
    pub target: String,
    pub mixin: String,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse WebIDL text blocks into a `SpecModel`.
///
/// `idl_blocks` are the raw WebIDL text strings extracted from `<pre class="idl">`
/// elements. `algorithms` are the algorithm steps extracted from spec prose.
pub fn parse_idl(
    idl_blocks: &[String],
    algorithms: &[AlgorithmSteps],
    spec_defs: &SpecDefinitions,
) -> Result<SpecModel> {
    let mut model = SpecModel::default();

    for (i, block) in idl_blocks.iter().enumerate() {
        let block = preprocess_idl(block);
        let definitions = match std::panic::catch_unwind(|| weedle::parse(&block)) {
            Ok(Ok(defs)) => defs,
            Ok(Err(e)) => {
                eprintln!("Warning: skipping unparseable WebIDL block {i}: {e}");
                continue;
            }
            Err(_) => {
                eprintln!("Warning: skipping WebIDL block {i} (parser panic)");
                continue;
            }
        };

        process_definitions(&definitions, &mut model, algorithms);
    }

    apply_includes(&mut model);

    // Copy inherited members into child dictionaries (`dictionary B : A`).
    flatten_dictionary_inheritance(&mut model);

    // Merge internal slots from spec definitions into interfaces
    for iface in &mut model.interfaces {
        if let Some(slots) = spec_defs.internal_slots.get(&iface.name) {
            iface.internal_slots = slots.clone();
        }
    }

    // Collect standalone algorithms, deduplicating by name.
    // For methods on known global interfaces (Window, WindowOrWorkerGlobalScope,
    // WorkerGlobalScope) that don't exist in the model, create synthetic global
    // interfaces so codegen generates #[jsglobals] instead of standalone functions.
    // For other unmatched methods/constructors, promote to standalone algorithms.
    let interface_names: HashSet<String> =
        model.interfaces.iter().map(|i| i.name.clone()).collect();
    for algo in algorithms {
        let name = match &algo.kind {
            AlgorithmKind::Standalone { name } => name.clone(),
            AlgorithmKind::Method { name, is_static: _ } => {
                if !algo.interface.is_empty() && interface_names.contains(algo.interface.as_str()) {
                    // Interface exists in the model — handled during WebIDL parsing
                    continue;
                }
                // Check if this is a global scope method
                if GLOBAL_INTERFACES.contains(&algo.interface.as_str()) {
                    add_method_to_global_interface(
                        &mut model,
                        &algo.interface,
                        name,
                        &algo.heading,
                        &algo.steps,
                    );
                    continue;
                }
                name.clone()
            }
            AlgorithmKind::Constructor { class } => {
                if !interface_names.contains(class.as_str()) {
                    class.clone()
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        // Dedup repeated extractions of the same algorithm (same name AND same
        // spec anchor). Distinct algorithms can share a name — e.g. the fetch
        // spec defines "append" for both header lists and Headers objects —
        // and must all be kept; codegen disambiguates colliding function
        // names with numeric suffixes.
        if !model
            .algorithms
            .iter()
            .any(|a| a.name == name && a.fragment == algo.fragment)
        {
            model.algorithms.push(Algorithm {
                name,
                heading: algo.heading.clone(),
                steps: algo.steps.clone(),
                fragment: algo.fragment.clone(),
            });
        }
    }

    // Add GC lifetime parameters to types that reference interfaces from this spec.
    // Interface types are stack newtypes (e.g. `URL<'s>`) and need a lifetime when
    // used as parameter or return types.
    add_interface_lifetimes(&mut model);

    // Replace any remaining unknown type names (from other specs) with
    // HandleValue<'_> so the generated code compiles without importing
    // types that aren't available.
    replace_unknown_types(&mut model);

    Ok(model)
}

/// Add a method to a synthetic global interface in the model.
///
/// If the interface doesn't exist yet, creates it. This handles specs that
/// define methods on `WindowOrWorkerGlobalScope` or similar global scope
/// interfaces without any WebIDL blocks.
fn add_method_to_global_interface(
    model: &mut SpecModel,
    interface_name: &str,
    method_name: &str,
    heading: &str,
    steps: &[Step],
) {
    let method = Method {
        name: method_name.to_string(),
        params: Vec::new(),
        return_type: RustType {
            text: "HandleValue<'_>".to_string(),
            comment: None,
            needs_handle_value: true,
            needs_object: false,
            needs_promise: false,
            needs_function: false,
        },
        algorithm_steps: steps.to_vec(),
    };
    if let Some(iface) = model
        .interfaces
        .iter_mut()
        .find(|i| i.name == interface_name)
    {
        // Avoid duplicates
        if !iface.methods.iter().any(|m| m.name == method_name) {
            iface.methods.push(method);
        }
    } else {
        model.interfaces.push(Interface {
            name: interface_name.to_string(),
            methods: vec![method],
            ..Default::default()
        });
    }
    // Also keep as a standalone algorithm so the algorithm steps are recorded
    // in algorithms.rs with the full heading context.
    let _ = heading;
}

/// Preprocess a WebIDL block to strip constructs that weedle2 cannot parse.
///
/// Handles:
/// - `Exposed=*` → `Exposed=Window` (weedle2 doesn't support wildcard)
/// - `Transferable` extended attribute (not part of core WebIDL grammar)
/// - `async_iterable<T>(...)` interface members (weedle2 panics on these)
/// - HTML artifacts leftover from `<a>` elements in spec `<pre>` blocks
fn preprocess_idl(block: &str) -> String {
    let mut result = String::with_capacity(block.len());

    for line in block.lines() {
        let trimmed = line.trim();

        // Strip async_iterable lines — these cause weedle2 panics and aren't
        // standard WebIDL that we need for code generation
        if trimmed.starts_with("async_iterable") || trimmed.starts_with("async iterable") {
            continue;
        }

        let mut line = line.to_string();

        // Replace Exposed=* with Exposed=Window
        line = line.replace("Exposed=*", "Exposed=Window");

        // Strip Transferable from extended attribute lists:
        // [Exposed=Window, Transferable] → [Exposed=Window]
        // [Transferable, Exposed=Window] → [Exposed=Window]
        // [Transferable] → [] (unlikely in practice)
        line = line.replace(", Transferable]", "]");
        line = line.replace("[Transferable, ", "[");
        line = line.replace("[Transferable]", "");

        result.push_str(&line);
        result.push('\n');
    }

    result
}

fn process_definitions(
    definitions: &[weedle::Definition<'_>],
    model: &mut SpecModel,
    algorithms: &[AlgorithmSteps],
) {
    for def in definitions {
        match def {
            weedle::Definition::Interface(iface) => {
                if let Ok(interface) = parse_interface(iface, algorithms)
                    && !model.interfaces.iter().any(|i| i.name == interface.name)
                {
                    model.interfaces.push(interface);
                }
            }
            weedle::Definition::PartialInterface(partial) => {
                let name = partial.identifier.0.to_string();
                let iface = find_or_create_interface(&mut model.interfaces, &name);
                parse_interface_members(&partial.members.body, iface, algorithms);
            }
            weedle::Definition::InterfaceMixin(mixin) => {
                let mut iface = Interface {
                    name: mixin.identifier.0.to_string(),
                    is_mixin: true,
                    ..Default::default()
                };
                parse_mixin_members(&mixin.members.body, &mut iface, algorithms);
                model.interfaces.push(iface);
            }
            weedle::Definition::PartialInterfaceMixin(partial) => {
                let name = partial.identifier.0.to_string();
                let iface = find_or_create_interface(&mut model.interfaces, &name);
                iface.is_mixin = true;
                parse_mixin_members(&partial.members.body, iface, algorithms);
            }
            weedle::Definition::Dictionary(dict) => {
                let parsed = parse_dictionary(dict);
                if !model.dictionaries.iter().any(|d| d.name == parsed.name) {
                    model.dictionaries.push(parsed);
                }
            }
            weedle::Definition::PartialDictionary(partial) => {
                let name = partial.identifier.0.to_string();
                let existing = model.dictionaries.iter_mut().find(|d| d.name == name);
                if let Some(existing) = existing {
                    for member in &partial.members.body {
                        existing.members.push(parse_dict_member(member));
                    }
                } else {
                    let mut dict = Dictionary {
                        name,
                        extends: None,
                        members: Vec::new(),
                    };
                    for member in &partial.members.body {
                        dict.members.push(parse_dict_member(member));
                    }
                    model.dictionaries.push(dict);
                }
            }
            weedle::Definition::Enum(e) => {
                let name = e.identifier.0.to_string();
                if !model.enums.iter().any(|existing| existing.name == name) {
                    model.enums.push(Enum {
                        name,
                        variants: e
                            .values
                            .body
                            .list
                            .iter()
                            .map(|v| v.value.0.to_string())
                            .collect(),
                    });
                }
            }
            weedle::Definition::Typedef(td) => {
                let name = td.identifier.0.to_string();
                if !model.typedefs.iter().any(|t| t.name == name) {
                    model.typedefs.push(Typedef {
                        name,
                        rust_type: map_type(&td.type_.type_),
                    });
                }
            }
            weedle::Definition::CallbackInterface(_) => {
                // Skip callback interfaces
            }
            weedle::Definition::Callback(cb) => {
                let name = cb.identifier.0.to_string();
                if !model.callbacks.iter().any(|c| c.name == name) {
                    model.callbacks.push(Callback {
                        name,
                        return_type: map_return_type(&cb.return_type),
                        params: parse_arguments(&cb.arguments.body.list),
                    });
                }
            }
            weedle::Definition::IncludesStatement(inc) => {
                model.includes.push(IncludesStatement {
                    target: inc.lhs_identifier.0.to_string(),
                    mixin: inc.rhs_identifier.0.to_string(),
                });
            }
            weedle::Definition::Namespace(_) | weedle::Definition::PartialNamespace(_) => {}
            _ => {}
        }
    }
}

/// Merge mixin members into target interfaces based on `includes` statements.
fn apply_includes(model: &mut SpecModel) {
    let includes = std::mem::take(&mut model.includes);

    for inc in &includes {
        let mixin_idx = model
            .interfaces
            .iter()
            .position(|i| i.name == inc.mixin && i.is_mixin);
        if let Some(mixin_idx) = mixin_idx {
            let mixin_attrs = model.interfaces[mixin_idx].attributes.clone();
            let mixin_methods = model.interfaces[mixin_idx].methods.clone();
            let mixin_constants = model.interfaces[mixin_idx].constants.clone();

            let target = find_or_create_interface(&mut model.interfaces, &inc.target);
            for attr in mixin_attrs {
                if !target.attributes.iter().any(|a| a.name == attr.name) {
                    target.attributes.push(attr);
                }
            }
            for method in mixin_methods {
                if !target.methods.iter().any(|m| m.name == method.name) {
                    target.methods.push(method);
                }
            }
            for constant in mixin_constants {
                if !target.constants.iter().any(|c| c.name == constant.name) {
                    target.constants.push(constant);
                }
            }
        }
    }

    model.includes = includes;
}

/// Flatten WebIDL dictionary inheritance by copying each parent dictionary's
/// members into its descendants.
///
/// Dictionary inheritance is structural, not prototype-based: a value for
/// `dictionary B : A` is converted by reading A's members and B's members off
/// the same object. The `#[webidl_dictionary]` macro builds its `FromJSVal`
/// solely from the struct's own fields, so inherited members must be
/// materialized here. A child's own member shadows an inherited one of the same
/// name. Parents from other specs (not present in this model) are skipped.
fn flatten_dictionary_inheritance(model: &mut SpecModel) {
    let by_name: HashMap<String, usize> = model
        .dictionaries
        .iter()
        .enumerate()
        .map(|(i, d)| (d.name.clone(), i))
        .collect();

    // Resolve each dictionary's inherited members up front, reading the original
    // (pre-flatten) member lists, so the result is independent of order and of
    // the mutation below.
    let inherited_per_dict: Vec<Vec<DictMember>> = model
        .dictionaries
        .iter()
        .map(|dict| {
            let mut seen: HashSet<String> = dict.members.iter().map(|m| m.name.clone()).collect();
            let mut visited: HashSet<String> = HashSet::from([dict.name.clone()]);
            let mut inherited: Vec<DictMember> = Vec::new();
            let mut parent = dict.extends.clone();
            while let Some(parent_name) = parent {
                if !visited.insert(parent_name.clone()) {
                    break; // cycle guard
                }
                let Some(&pi) = by_name.get(&parent_name) else {
                    break; // parent defined in another spec — can't flatten
                };
                let parent_dict = &model.dictionaries[pi];
                for m in &parent_dict.members {
                    if seen.insert(m.name.clone()) {
                        inherited.push(DictMember {
                            inherited_from: Some(parent_name.clone()),
                            ..m.clone()
                        });
                    }
                }
                parent = parent_dict.extends.clone();
            }
            inherited
        })
        .collect();

    // Prepend inherited members (ancestors before the dictionary's own members).
    for (dict, mut inherited) in model.dictionaries.iter_mut().zip(inherited_per_dict) {
        if inherited.is_empty() {
            continue;
        }
        inherited.append(&mut dict.members);
        dict.members = inherited;
    }
}

/// Add `<'_>` lifetime parameters to all `RustType` references that name an
/// interface or lifetime-bearing typedef from this spec. Interface types
/// become stack newtypes (e.g. `URL<'s>`) and require a lifetime when used as
/// parameter or return types. Typedefs whose underlying Rust type contains a
/// lifetime (e.g. `type BlobPart<'a> = HandleValue<'a>`) similarly require a
/// lifetime argument at usage sites.
fn add_interface_lifetimes(model: &mut SpecModel) {
    let iface_names: HashSet<String> = model
        .interfaces
        .iter()
        .filter(|i| !i.is_mixin)
        .map(|i| i.name.clone())
        .collect();

    // Typedefs whose Rust type contains a lifetime parameter also need <'_>
    // at usage sites. Detect by checking for `<'` in the mapped type text.
    let lifetime_typedef_names: HashSet<String> = model
        .typedefs
        .iter()
        .filter(|td| td.rust_type.text.contains("<'"))
        .map(|td| td.name.clone())
        .collect();

    // Callback type aliases all carry a `<'s>` lifetime (they alias to
    // `js::Function<'s>`), so references in dictionaries/methods need `<'_>`.
    let callback_names: HashSet<String> =
        model.callbacks.iter().map(|cb| cb.name.clone()).collect();

    let mut names_needing_lifetime = iface_names;
    names_needing_lifetime.extend(lifetime_typedef_names);
    names_needing_lifetime.extend(callback_names);

    if names_needing_lifetime.is_empty() {
        return;
    }

    for iface in &mut model.interfaces {
        if iface.is_mixin {
            continue;
        }

        // Constructor params
        if let Some(ctor) = &mut iface.constructor {
            for p in &mut ctor.params {
                add_lifetime_to_type(&mut p.rust_type, &names_needing_lifetime);
            }
        }

        // Attributes
        for attr in &mut iface.attributes {
            add_lifetime_to_type(&mut attr.rust_type, &names_needing_lifetime);
        }

        // Instance methods
        for method in &mut iface.methods {
            add_lifetime_to_type(&mut method.return_type, &names_needing_lifetime);
            for p in &mut method.params {
                add_lifetime_to_type(&mut p.rust_type, &names_needing_lifetime);
            }
        }

        // Static methods
        for method in &mut iface.static_methods {
            add_lifetime_to_type(&mut method.return_type, &names_needing_lifetime);
            for p in &mut method.params {
                add_lifetime_to_type(&mut p.rust_type, &names_needing_lifetime);
            }
        }
    }

    // Dictionary member types
    for dict in &mut model.dictionaries {
        for member in &mut dict.members {
            add_lifetime_to_type(&mut member.rust_type, &names_needing_lifetime);
        }
    }

    // Callback return types and params
    for cb in &mut model.callbacks {
        add_lifetime_to_type(&mut cb.return_type, &names_needing_lifetime);
        for p in &mut cb.params {
            add_lifetime_to_type(&mut p.rust_type, &names_needing_lifetime);
        }
    }
}

/// Rust primitive and standard library types that should not be replaced
/// with `HandleValue<'_>`. These are types that `map_named_type` and other
/// type mappers already handle correctly.
const KNOWN_RUST_TYPES: &[&str] = &[
    "bool",
    "u8",
    "i8",
    "u16",
    "i16",
    "u32",
    "i32",
    "u64",
    "i64",
    "f32",
    "f64",
    "String",
    "HandleValue",
    "Object",
    "Promise",
    "Value",
    "Function",
    "()",
];

/// Replace unknown type references (types from other specs) with
/// `HandleValue<'_>` so generated code compiles without external dependencies.
///
/// Collects all type names defined in this spec (interfaces, enums, typedefs,
/// dictionaries) and replaces any remaining bare identifiers with
/// `HandleValue<'_>` plus a comment noting the original WebIDL type.
fn replace_unknown_types(model: &mut SpecModel) {
    // Build the set of all type names known from this spec
    let mut known_names: HashSet<String> = HashSet::new();
    for iface in &model.interfaces {
        known_names.insert(iface.name.clone());
    }
    for e in &model.enums {
        known_names.insert(e.name.clone());
    }
    for td in &model.typedefs {
        known_names.insert(td.name.clone());
    }
    for dict in &model.dictionaries {
        known_names.insert(dict.name.clone());
    }
    for cb in &model.callbacks {
        known_names.insert(cb.name.clone());
    }

    // Typedefs whose underlying Rust type is HandleValue (union types).
    // Vec<Typedef<'_>> where typedef → HandleValue cannot be FromJSVal-deserialized
    // element-by-element, so `Vec<Typedef>` must collapse to `HandleValue<'_>`.
    let handle_value_typedefs: HashSet<String> = model
        .typedefs
        .iter()
        .filter(|td| td.rust_type.text.starts_with("HandleValue"))
        .map(|td| td.name.clone())
        .collect();

    for iface in &mut model.interfaces {
        if let Some(ctor) = &mut iface.constructor {
            for p in &mut ctor.params {
                replace_unknown_in_type(&mut p.rust_type, &known_names, &handle_value_typedefs);
            }
        }
        for attr in &mut iface.attributes {
            replace_unknown_in_type(&mut attr.rust_type, &known_names, &handle_value_typedefs);
        }
        for method in iface
            .methods
            .iter_mut()
            .chain(iface.static_methods.iter_mut())
        {
            replace_unknown_in_type(
                &mut method.return_type,
                &known_names,
                &handle_value_typedefs,
            );
            for p in &mut method.params {
                replace_unknown_in_type(&mut p.rust_type, &known_names, &handle_value_typedefs);
            }
        }
    }

    for dict in &mut model.dictionaries {
        for member in &mut dict.members {
            replace_unknown_in_type(&mut member.rust_type, &known_names, &handle_value_typedefs);
        }
    }

    for cb in &mut model.callbacks {
        replace_unknown_in_type(&mut cb.return_type, &known_names, &handle_value_typedefs);
        for p in &mut cb.params {
            replace_unknown_in_type(&mut p.rust_type, &known_names, &handle_value_typedefs);
        }
    }
}

/// If `ty.text` is or contains an unknown type name (a bare PascalCase
/// identifier not in `known_names` or `KNOWN_RUST_TYPES`), replace it
/// with `HandleValue<'_>`. Also collapses `Vec<Typedef<'_>>` where the
/// typedef resolves to `HandleValue` — such sequences can't be
/// deserialized element-by-element.
fn replace_unknown_in_type(
    ty: &mut RustType,
    known_names: &HashSet<String>,
    hv_typedefs: &HashSet<String>,
) {
    // Extract the "core" type name — strip Option<>, Vec<>, and lifetimes
    let text = ty.text.trim();

    // Handle wrapper types: Option<T>, Vec<T>
    if let Some(inner) = strip_wrapper(text, "Option") {
        let mut inner_ty = RustType {
            text: inner.to_string(),
            comment: ty.comment.clone(),
            needs_handle_value: ty.needs_handle_value,
            needs_object: ty.needs_object,
            needs_promise: ty.needs_promise,
            needs_function: ty.needs_function,
        };
        replace_unknown_in_type(&mut inner_ty, known_names, hv_typedefs);
        if inner_ty.text != inner {
            ty.text = format!("Option<{}>", inner_ty.text);
            ty.comment = inner_ty.comment;
            ty.needs_handle_value = inner_ty.needs_handle_value;
            ty.needs_object = inner_ty.needs_object;
            ty.needs_promise = inner_ty.needs_promise;
            ty.needs_function = inner_ty.needs_function;
        }
        return;
    }
    if let Some(inner) = strip_wrapper(text, "Vec") {
        // Vec<TypedefName<'_>> where typedef → HandleValue can't be
        // deserialized as a Rust vec; collapse to HandleValue<'_>.
        let inner_bare = inner.strip_suffix("<'_>").unwrap_or(inner);
        if hv_typedefs.contains(inner_bare) {
            let original = inner_bare.to_string();
            ty.text = "HandleValue<'_>".to_string();
            ty.needs_handle_value = true;
            ty.comment = Some(
                ty.comment
                    .take()
                    .unwrap_or_else(|| format!("WebIDL: sequence<{original}>")),
            );
            return;
        }

        let mut inner_ty = RustType {
            text: inner.to_string(),
            comment: ty.comment.clone(),
            needs_handle_value: ty.needs_handle_value,
            needs_object: ty.needs_object,
            needs_promise: ty.needs_promise,
            needs_function: ty.needs_function,
        };
        replace_unknown_in_type(&mut inner_ty, known_names, hv_typedefs);
        if inner_ty.text != inner {
            ty.text = format!("Vec<{}>", inner_ty.text);
            ty.comment = inner_ty.comment;
            ty.needs_handle_value = inner_ty.needs_handle_value;
            ty.needs_object = inner_ty.needs_object;
            ty.needs_promise = inner_ty.needs_promise;
            ty.needs_function = inner_ty.needs_function;
        }
        return;
    }

    // Strip lifetime suffix for checking (e.g. "Blob<'_>" → "Blob")
    let bare = text.strip_suffix("<'_>").unwrap_or(text);

    // Skip known Rust primitives, known spec types, and already-resolved types
    if KNOWN_RUST_TYPES.contains(&bare)
        || known_names.contains(bare)
        || bare.contains('<')
        || bare.contains('(')
        || bare.contains(' ')
        || bare.is_empty()
    {
        return;
    }

    // This looks like an unknown external type — replace with HandleValue<'_>
    let original_name = bare.to_string();
    let existing_comment = ty.comment.take();
    ty.text = "HandleValue<'_>".to_string();
    ty.needs_handle_value = true;
    ty.comment = Some(existing_comment.unwrap_or_else(|| format!("WebIDL: {original_name}")));
}

/// Strip a generic wrapper type, returning the inner type text.
/// e.g., `strip_wrapper("Option<Foo>", "Option")` → `Some("Foo")`
fn strip_wrapper<'a>(text: &'a str, wrapper: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(wrapper)?;
    let rest = rest.strip_prefix('<')?;
    let rest = rest.strip_suffix('>')?;
    Some(rest)
}

/// If `ty.text` contains a bare interface name (not already followed by `<`),
/// replace it with `Name<'_>`.
fn add_lifetime_to_type(ty: &mut RustType, iface_names: &HashSet<String>) {
    for name in iface_names {
        // Match the interface name as a whole word not already followed by '<'.
        // We check character boundaries to avoid replacing substrings
        // (e.g. "URLSearchParams" should not match against "URL" if
        // "URLSearchParams" is also an interface — but since we process all
        // names, both will be handled correctly as long as we process longer
        // names first... Actually, we need to be careful here.)
        let mut result = String::with_capacity(ty.text.len() + 4);
        let mut remaining = ty.text.as_str();
        while let Some(pos) = remaining.find(name.as_str()) {
            let after = pos + name.len();
            // Check that the match is at a word boundary
            let before_ok = pos == 0 || !remaining.as_bytes()[pos - 1].is_ascii_alphanumeric();
            let after_ok =
                after >= remaining.len() || !remaining.as_bytes()[after].is_ascii_alphanumeric();
            let not_already_lifetime =
                after >= remaining.len() || remaining.as_bytes()[after] != b'<';

            if before_ok && after_ok && not_already_lifetime {
                result.push_str(&remaining[..pos]);
                result.push_str(name);
                result.push_str("<'_>");
                remaining = &remaining[after..];
            } else {
                result.push_str(&remaining[..after]);
                remaining = &remaining[after..];
            }
        }
        result.push_str(remaining);
        ty.text = result;
    }
}

fn find_or_create_interface<'a>(
    interfaces: &'a mut Vec<Interface>,
    name: &str,
) -> &'a mut Interface {
    let idx = interfaces.iter().position(|i| i.name == name);
    match idx {
        Some(i) => &mut interfaces[i],
        None => {
            interfaces.push(Interface {
                name: name.to_string(),
                ..Default::default()
            });
            interfaces.last_mut().unwrap()
        }
    }
}

fn parse_interface(
    iface: &weedle::InterfaceDefinition<'_>,
    algorithms: &[AlgorithmSteps],
) -> Result<Interface> {
    let extends = iface
        .inheritance
        .as_ref()
        .map(|i| i.identifier.0.to_string());

    let mut interface = Interface {
        name: iface.identifier.0.to_string(),
        extends,
        ..Default::default()
    };

    // Note: the legacy `[Constructor(...)]` extended-attribute syntax is not
    // handled. Modern WHATWG/W3C specs declare constructors with a `constructor`
    // operation, which `parse_interface_members` picks up below.
    parse_interface_members(&iface.members.body, &mut interface, algorithms);

    Ok(interface)
}

fn parse_interface_members(
    members: &[InterfaceMember<'_>],
    interface: &mut Interface,
    algorithms: &[AlgorithmSteps],
) {
    for member in members {
        match member {
            InterfaceMember::Attribute(attr) => {
                add_attribute(
                    interface,
                    attr.identifier.0,
                    &attr.type_.type_,
                    attr.readonly.is_some(),
                    algorithms,
                );
            }
            InterfaceMember::Operation(op) => {
                if let Some(ident) = &op.identifier {
                    let is_static = op.modifier.is_some();
                    add_operation(
                        interface,
                        ident.0,
                        &op.args.body.list,
                        &op.return_type,
                        is_static,
                        algorithms,
                    );
                }
            }
            InterfaceMember::Const(c) => {
                add_constant(interface, c.identifier.0, &c.const_type, &c.const_value);
            }
            InterfaceMember::Constructor(ctor) => {
                let params = parse_arguments(&ctor.args.body.list);
                let algo_steps = lookup_constructor_steps(algorithms, &interface.name);
                interface.constructor = Some(Constructor {
                    params,
                    algorithm_steps: algo_steps,
                });
            }
            InterfaceMember::Iterable(iter) => match iter {
                weedle::interface::IterableInterfaceMember::Single(s) => {
                    interface.iterable = Some(Iterable {
                        key_type: None,
                        value_type: map_type(&s.generics.body.type_),
                    });
                }
                weedle::interface::IterableInterfaceMember::Double(d) => {
                    interface.iterable = Some(Iterable {
                        key_type: Some(map_type(&d.generics.body.0.type_)),
                        value_type: map_type(&d.generics.body.2.type_),
                    });
                }
            },
            InterfaceMember::Stringifier(_)
            | InterfaceMember::Maplike(_)
            | InterfaceMember::Setlike(_)
            | InterfaceMember::AsyncIterable(_) => {}
        }
    }
}

fn parse_mixin_members(
    members: &[MixinMember<'_>],
    interface: &mut Interface,
    algorithms: &[AlgorithmSteps],
) {
    for member in members {
        match member {
            MixinMember::Attribute(attr) => {
                add_attribute(
                    interface,
                    attr.identifier.0,
                    &attr.type_.type_,
                    attr.readonly.is_some(),
                    algorithms,
                );
            }
            MixinMember::Operation(op) => {
                if let Some(ident) = &op.identifier {
                    add_operation(
                        interface,
                        ident.0,
                        &op.args.body.list,
                        &op.return_type,
                        false,
                        algorithms,
                    );
                }
            }
            MixinMember::Const(c) => {
                add_constant(interface, c.identifier.0, &c.const_type, &c.const_value);
            }
            MixinMember::Stringifier(_) => {}
        }
    }
}

/// Add an attribute to an interface, deduplicating by name.
fn add_attribute(
    interface: &mut Interface,
    name: &str,
    ty: &weedle::types::Type<'_>,
    readonly: bool,
    algorithms: &[AlgorithmSteps],
) {
    if interface.attributes.iter().any(|a| a.name == name) {
        return;
    }
    let getter_steps = lookup_getter_steps(algorithms, name, &interface.name);
    let setter_steps = lookup_setter_steps(algorithms, name, &interface.name);
    interface.attributes.push(Attribute {
        name: name.to_string(),
        rust_type: map_type(ty),
        readonly,
        getter_steps,
        setter_steps,
    });
}

/// Add a method or static method to an interface, deduplicating by name.
fn add_operation(
    interface: &mut Interface,
    name: &str,
    args: &[weedle::argument::Argument<'_>],
    return_type: &weedle::types::ReturnType<'_>,
    is_static: bool,
    algorithms: &[AlgorithmSteps],
) {
    let target = if is_static {
        &interface.static_methods
    } else {
        &interface.methods
    };
    if target.iter().any(|m| m.name == name) {
        return;
    }
    let params = parse_arguments(args);
    let rt = map_return_type(return_type);
    let algo_steps = lookup_method_steps(algorithms, name, is_static, &interface.name);
    let method = Method {
        name: name.to_string(),
        params,
        return_type: rt,
        algorithm_steps: algo_steps,
    };
    if is_static {
        interface.static_methods.push(method);
    } else {
        interface.methods.push(method);
    }
}

/// Add a constant to an interface.
fn add_constant(
    interface: &mut Interface,
    name: &str,
    const_type: &weedle::types::ConstType<'_>,
    const_value: &weedle::literal::ConstValue<'_>,
) {
    interface.constants.push(Constant {
        name: name.to_string(),
        rust_type: crate::types::map_const_type(const_type),
        value: format_const_value(const_value),
    });
}

fn parse_dictionary(dict: &weedle::DictionaryDefinition<'_>) -> Dictionary {
    let extends = dict
        .inheritance
        .as_ref()
        .map(|i| i.identifier.0.to_string());
    let members = dict.members.body.iter().map(parse_dict_member).collect();
    Dictionary {
        name: dict.identifier.0.to_string(),
        extends,
        members,
    }
}

fn parse_dict_member(member: &weedle::dictionary::DictionaryMember<'_>) -> DictMember {
    let default_value = member
        .default
        .as_ref()
        .map(|d| format_default_value(&d.value));
    DictMember {
        name: member.identifier.0.to_string(),
        rust_type: map_type(&member.type_),
        required: member.required.is_some(),
        default_value,
        inherited_from: None,
    }
}

fn parse_arguments(args: &[weedle::argument::Argument<'_>]) -> Vec<Param> {
    args.iter()
        .map(|arg| match arg {
            weedle::argument::Argument::Single(s) => Param {
                name: s.identifier.0.to_string(),
                rust_type: map_type(&s.type_.type_),
                optional: s.optional.is_some(),
                variadic: false,
            },
            weedle::argument::Argument::Variadic(v) => Param {
                name: v.identifier.0.to_string(),
                rust_type: map_type(&v.type_),
                optional: false,
                variadic: true,
            },
        })
        .collect()
}

fn format_const_value(val: &weedle::literal::ConstValue<'_>) -> String {
    match val {
        weedle::literal::ConstValue::Boolean(b) => if b.0 { "true" } else { "false" }.to_string(),
        weedle::literal::ConstValue::Float(f) => format_float_lit(f),
        weedle::literal::ConstValue::Integer(i) => format_integer_lit(i),
        weedle::literal::ConstValue::Null(_) => "()".to_string(),
    }
}

fn format_float_lit(f: &weedle::literal::FloatLit<'_>) -> String {
    match f {
        weedle::literal::FloatLit::Value(v) => v.0.to_string(),
        weedle::literal::FloatLit::NegInfinity(_) => "f64::NEG_INFINITY".to_string(),
        weedle::literal::FloatLit::Infinity(_) => "f64::INFINITY".to_string(),
        weedle::literal::FloatLit::NaN(_) => "f64::NAN".to_string(),
    }
}

fn format_integer_lit(i: &weedle::literal::IntegerLit<'_>) -> String {
    match i {
        weedle::literal::IntegerLit::Dec(d) => d.0.to_string(),
        weedle::literal::IntegerLit::Hex(h) => h.0.to_string(),
        weedle::literal::IntegerLit::Oct(o) => o.0.to_string(),
    }
}

fn format_default_value(val: &weedle::literal::DefaultValue<'_>) -> String {
    match val {
        weedle::literal::DefaultValue::Boolean(b) => if b.0 { "true" } else { "false" }.to_string(),
        weedle::literal::DefaultValue::Float(f) => format_float_lit(f),
        weedle::literal::DefaultValue::Integer(i) => format_integer_lit(i),
        weedle::literal::DefaultValue::String(s) => format!("\"{}\"", s.0),
        weedle::literal::DefaultValue::EmptyArray(_) => "vec![]".to_string(),
        weedle::literal::DefaultValue::EmptyDictionary(_) => "Default::default()".to_string(),
        weedle::literal::DefaultValue::Null(_) => "None".to_string(),
    }
}

/// Look up algorithm steps for a method, preferring interface-scoped matches.
fn lookup_method_steps(
    algorithms: &[AlgorithmSteps],
    method_name: &str,
    is_static: bool,
    interface_name: &str,
) -> Vec<Step> {
    lookup_steps(algorithms, interface_name, |kind| {
        matches!(
            kind,
            AlgorithmKind::Method { name, is_static: s }
            if name.eq_ignore_ascii_case(method_name) && *s == is_static
        )
    })
}

/// Look up getter algorithm steps, preferring interface-scoped matches.
fn lookup_getter_steps(
    algorithms: &[AlgorithmSteps],
    attr_name: &str,
    interface_name: &str,
) -> Vec<Step> {
    lookup_steps(algorithms, interface_name, |kind| {
        matches!(
            kind,
            AlgorithmKind::Getter { name } if name.eq_ignore_ascii_case(attr_name)
        )
    })
}

/// Look up setter algorithm steps, preferring interface-scoped matches.
fn lookup_setter_steps(
    algorithms: &[AlgorithmSteps],
    attr_name: &str,
    interface_name: &str,
) -> Vec<Step> {
    lookup_steps(algorithms, interface_name, |kind| {
        matches!(
            kind,
            AlgorithmKind::Setter { name } if name.eq_ignore_ascii_case(attr_name)
        )
    })
}

/// Look up constructor algorithm steps for a class by name.
fn lookup_constructor_steps(algorithms: &[AlgorithmSteps], class_name: &str) -> Vec<Step> {
    lookup_steps(
        algorithms,
        class_name,
        |kind| matches!(kind, AlgorithmKind::Constructor { class } if class.eq_ignore_ascii_case(class_name)),
    )
}

/// Generic two-pass algorithm step lookup.
///
/// First tries to find an algorithm scoped to `interface_name`, then falls back
/// to an unscoped match (empty interface field).
fn lookup_steps(
    algorithms: &[AlgorithmSteps],
    interface_name: &str,
    matches_kind: impl Fn(&AlgorithmKind) -> bool,
) -> Vec<Step> {
    // Pass 1: interface-scoped match.
    if let Some(algo) = algorithms
        .iter()
        .find(|a| matches_kind(&a.kind) && a.interface.eq_ignore_ascii_case(interface_name))
    {
        return algo.steps.clone();
    }
    // Pass 2: unscoped fallback.
    if let Some(algo) = algorithms
        .iter()
        .find(|a| matches_kind(&a.kind) && a.interface.is_empty())
    {
        return algo.steps.clone();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_interface() {
        let idl = r#"
[Exposed=Window]
interface URL {
  constructor(USVString url, optional USVString base);
  stringifier attribute USVString href;
  readonly attribute USVString origin;
  attribute USVString protocol;
  USVString toJSON();
  static boolean canParse(USVString url, optional USVString base);
};
        "#;

        let model = parse_idl(&[idl.to_string()], &[], &Default::default()).unwrap();
        assert_eq!(model.interfaces.len(), 1);

        let url = &model.interfaces[0];
        assert_eq!(url.name, "URL");
        assert!(url.constructor.is_some());

        let ctor = url.constructor.as_ref().unwrap();
        assert_eq!(ctor.params.len(), 2);
        assert_eq!(ctor.params[0].name, "url");
        assert!(ctor.params[1].optional);

        // href + origin + protocol = 3 attributes
        assert_eq!(url.attributes.len(), 3);
        assert!(url
            .attributes
            .iter()
            .any(|a| a.name == "href" && !a.readonly));
        assert!(url
            .attributes
            .iter()
            .any(|a| a.name == "origin" && a.readonly));

        // toJSON is an instance method
        assert_eq!(url.methods.len(), 1);
        assert_eq!(url.methods[0].name, "toJSON");

        // canParse is a static method
        assert_eq!(url.static_methods.len(), 1);
        assert_eq!(url.static_methods[0].name, "canParse");
    }

    #[test]
    fn parse_dictionary() {
        let idl = r#"
dictionary RequestInit {
  ByteString method;
  required USVString url;
  boolean keepalive = false;
};
        "#;

        let model = parse_idl(&[idl.to_string()], &[], &Default::default()).unwrap();
        assert_eq!(model.dictionaries.len(), 1);

        let dict = &model.dictionaries[0];
        assert_eq!(dict.name, "RequestInit");
        assert_eq!(dict.members.len(), 3);
        assert!(dict.members.iter().any(|m| m.name == "url" && m.required));
        assert!(dict
            .members
            .iter()
            .any(|m| m.name == "keepalive" && m.default_value.is_some()));
    }

    #[test]
    fn dictionary_inheritance_flattens_with_marker() {
        let idl = r#"
dictionary EventInit {
  boolean bubbles = false;
  boolean cancelable = false;
};

dictionary CustomEventInit : EventInit {
  any detail = null;
};
        "#;

        let model = parse_idl(&[idl.to_string()], &[], &Default::default()).unwrap();

        let cei = model
            .dictionaries
            .iter()
            .find(|d| d.name == "CustomEventInit")
            .unwrap();
        // Inherited members are materialized alongside the own member.
        assert!(cei.members.iter().any(|m| m.name == "bubbles"));
        assert!(cei.members.iter().any(|m| m.name == "cancelable"));
        assert!(cei.members.iter().any(|m| m.name == "detail"));
        // Inherited members are marked; the own member is not.
        let bubbles = cei.members.iter().find(|m| m.name == "bubbles").unwrap();
        assert_eq!(bubbles.inherited_from.as_deref(), Some("EventInit"));
        let detail = cei.members.iter().find(|m| m.name == "detail").unwrap();
        assert_eq!(detail.inherited_from, None);

        // The parent dictionary is left intact.
        let ei = model
            .dictionaries
            .iter()
            .find(|d| d.name == "EventInit")
            .unwrap();
        assert_eq!(ei.members.len(), 2);
    }

    #[test]
    fn dictionary_inheritance_is_transitive_and_child_shadows() {
        let idl = r#"
dictionary A { DOMString shared; boolean a = false; };
dictionary B : A { boolean b = false; };
dictionary C : B { long shared; };
        "#;

        let model = parse_idl(&[idl.to_string()], &[], &Default::default()).unwrap();
        let c = model.dictionaries.iter().find(|d| d.name == "C").unwrap();

        // Transitive: `a` comes from grandparent A, `b` from parent B.
        assert_eq!(
            c.members.iter().find(|m| m.name == "a").unwrap().inherited_from.as_deref(),
            Some("A")
        );
        assert!(c.members.iter().any(|m| m.name == "b"));
        // `shared` is declared in both A and C; the child's own member wins and
        // appears exactly once, unmarked.
        assert_eq!(c.members.iter().filter(|m| m.name == "shared").count(), 1);
        let shared = c.members.iter().find(|m| m.name == "shared").unwrap();
        assert_eq!(shared.inherited_from, None);
    }

    #[test]
    fn parse_enum() {
        let idl = r#"
enum RequestMode { "navigate", "same-origin", "no-cors", "cors" };
        "#;

        let model = parse_idl(&[idl.to_string()], &[], &Default::default()).unwrap();
        assert_eq!(model.enums.len(), 1);
        assert_eq!(model.enums[0].name, "RequestMode");
        assert_eq!(model.enums[0].variants.len(), 4);
    }

    #[test]
    fn parse_includes_merges_mixin() {
        let idl = r#"
interface mixin Body {
  readonly attribute ReadableStream? body;
  readonly attribute boolean bodyUsed;
  Promise<ArrayBuffer> arrayBuffer();
};

interface Request {
  constructor(USVString url);
  readonly attribute USVString method;
};

Request includes Body;
        "#;

        let model = parse_idl(&[idl.to_string()], &[], &Default::default()).unwrap();

        let request = model
            .interfaces
            .iter()
            .find(|i| i.name == "Request")
            .expect("Request interface should exist");

        // Request should have its own attribute (method) + Body's attributes (body, bodyUsed)
        assert_eq!(request.attributes.len(), 3);
        assert!(request.attributes.iter().any(|a| a.name == "method"));
        assert!(request.attributes.iter().any(|a| a.name == "body"));
        assert!(request.attributes.iter().any(|a| a.name == "bodyUsed"));

        // Request should have Body's method (arrayBuffer)
        assert!(request.methods.iter().any(|m| m.name == "arrayBuffer"));
    }

    #[test]
    fn preprocess_strips_async_iterable() {
        let idl = r#"
[Exposed=Window]
interface ReadableStream {
  constructor(optional object underlyingSource, optional QueuingStrategy strategy = {});
  readonly attribute boolean locked;
  async_iterable<any>(optional ReadableStreamIteratorOptions options = {});
  Promise<undefined> cancel(optional any reason);
};
        "#;

        let result = preprocess_idl(idl);
        assert!(!result.contains("async_iterable"));
        assert!(result.contains("constructor"));
        assert!(result.contains("cancel"));
    }

    #[test]
    fn preprocess_strips_transferable() {
        let idl = "[Exposed=*, Transferable]\ninterface ReadableStream {};";
        let result = preprocess_idl(idl);
        assert!(!result.contains("Transferable"));
        assert!(result.contains("[Exposed=Window]"));
    }

    #[test]
    fn preprocess_replaces_exposed_star() {
        let idl = "[Exposed=*]\ninterface Foo {};";
        let result = preprocess_idl(idl);
        assert!(result.contains("[Exposed=Window]"));
        assert!(!result.contains("Exposed=*"));
    }

    #[test]
    fn internal_slots_merge_into_interface() {
        let idl = r#"
[Exposed=Window]
interface WritableStream {
  constructor();
  readonly attribute boolean locked;
};
        "#;

        let mut spec_defs = crate::extract::SpecDefinitions::default();
        spec_defs.internal_slots.insert(
            "WritableStream".to_string(),
            vec![
                InternalSlot {
                    name: "backpressure".to_string(),
                    description: "A boolean flag".to_string(),
                    fragment_id: "ws-backpressure".to_string(),
                },
                InternalSlot {
                    name: "storedError".to_string(),
                    description: "A stored error value".to_string(),
                    fragment_id: "ws-storederror".to_string(),
                },
            ],
        );

        let model = parse_idl(&[idl.to_string()], &[], &spec_defs).unwrap();
        let ws = &model.interfaces[0];
        assert_eq!(ws.internal_slots.len(), 2);
        assert_eq!(ws.internal_slots[0].name, "backpressure");
        assert_eq!(ws.internal_slots[1].name, "storedError");
    }

    #[test]
    fn replace_unknown_types_in_params() {
        let idl = r#"
[Exposed=Window]
interface FileReader : EventTarget {
  constructor();
  attribute EventHandler onloadstart;
};
        "#;

        let model = parse_idl(&[idl.to_string()], &[], &Default::default()).unwrap();
        let reader = &model.interfaces[0];

        // EventHandler is from another spec — it should be replaced with HandleValue
        let handler_attr = reader
            .attributes
            .iter()
            .find(|a| a.name == "onloadstart")
            .unwrap();
        assert_eq!(
            handler_attr.rust_type.text, "HandleValue<'_>",
            "EventHandler should be replaced with HandleValue<'_>"
        );
        assert!(
            handler_attr
                .rust_type
                .comment
                .as_ref()
                .unwrap()
                .contains("EventHandler"),
            "should note original type name"
        );
    }

    #[test]
    fn replace_unknown_types_preserves_known() {
        let idl = r#"
[Exposed=Window]
interface Blob {
  constructor();
  readonly attribute unsigned long long size;
};

[Exposed=Window]
interface File : Blob {
  constructor();
  readonly attribute DOMString name;
};
        "#;

        let model = parse_idl(&[idl.to_string()], &[], &Default::default()).unwrap();
        let file = model.interfaces.iter().find(|i| i.name == "File").unwrap();

        // Blob is from THIS spec — its type references should NOT be replaced
        let name_attr = file.attributes.iter().find(|a| a.name == "name").unwrap();
        assert_eq!(
            name_attr.rust_type.text, "String",
            "DOMString should map to String, not be replaced"
        );
    }

    #[test]
    fn standalone_algorithms_with_same_name_but_different_fragments_are_kept() {
        // The fetch spec defines "append" both for header lists
        // (#concept-header-list-append) and for Headers objects
        // (#concept-headers-append). Both may classify to the same standalone
        // name; distinct spec anchors must still yield distinct algorithms.
        let make = |fragment: &str, heading: &str| AlgorithmSteps {
            heading: heading.to_string(),
            kind: AlgorithmKind::Standalone {
                name: "append a header".to_string(),
            },
            steps: vec![Step::numbered(1, "Do something.")],
            interface: String::new(),
            fragment: fragment.to_string(),
        };
        let algos = vec![
            make(
                "concept-header-list-append",
                "To append a header (name, value) to a header list list:",
            ),
            make(
                "concept-headers-append",
                "To append a header (name, value) to a Headers object headers, run these steps:",
            ),
            // Exact duplicate of the first (same name AND fragment): still deduped.
            make(
                "concept-header-list-append",
                "To append a header (name, value) to a header list list:",
            ),
        ];

        let model = parse_idl(&[], &algos, &Default::default()).unwrap();
        assert_eq!(
            model.algorithms.len(),
            2,
            "distinct fragments must both be kept, identical ones deduped: {:?}",
            model.algorithms.iter().map(|a| &a.fragment).collect::<Vec<_>>()
        );
        assert!(model
            .algorithms
            .iter()
            .any(|a| a.fragment == "concept-headers-append"));
    }

    #[test]
    fn typedef_lifetime_added() {
        let idl = r#"
typedef (BufferSource or Blob or USVString) BlobPart;

[Exposed=Window]
interface Blob {
  constructor(optional sequence<BlobPart> blobParts);
};
        "#;

        let model = parse_idl(&[idl.to_string()], &[], &Default::default()).unwrap();

        // BlobPart typedef maps to HandleValue (union type).
        // sequence<BlobPart> should collapse to HandleValue<'_> since
        // Vec<HandleValue<'_>> is not valid for FromJSVal.
        let blob = model.interfaces.iter().find(|i| i.name == "Blob").unwrap();
        let ctor = blob.constructor.as_ref().unwrap();
        let blob_parts = &ctor.params[0];
        assert!(
            blob_parts.rust_type.text.contains("HandleValue<'_>"),
            "sequence<BlobPart> should collapse to HandleValue<'_>, got: {}",
            blob_parts.rust_type.text
        );
    }
}
