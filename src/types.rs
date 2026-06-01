// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Maps WebIDL types to Rust type strings for code generation.
//!
//! All generated function signatures use rooted/handle types — never bare
//! `Value` or raw pointers.

use weedle::types::{
    ConstType, FloatingPointType, IntegerType, NonAnyType, ReturnType, SingleType, Type,
    UnionMemberType,
};

/// A rendered Rust type string ready for code generation.
#[derive(Debug, Clone)]
pub struct RustType {
    /// The Rust type text (e.g. `String`, `HandleValue<'_>`).
    pub text: String,
    /// An optional comment noting the original WebIDL type when the mapping
    /// is lossy (e.g. union types mapped to `HandleValue<'_>`).
    pub comment: Option<String>,
    /// Whether this type needs `use js::native::HandleValue;` in the imports.
    pub needs_handle_value: bool,
    /// Whether this type needs `use js::Object;` in the imports.
    pub needs_object: bool,
    /// Whether this type needs `use js::promise::Promise;` in the imports.
    pub needs_promise: bool,
    /// Whether this type needs `use js::Function;` in the imports.
    pub needs_function: bool,
}

impl RustType {
    fn simple(text: &str) -> Self {
        Self {
            text: text.to_string(),
            comment: None,
            needs_handle_value: false,
            needs_object: false,
            needs_promise: false,
            needs_function: false,
        }
    }

    fn handle_value(comment: Option<String>) -> Self {
        Self {
            text: "HandleValue<'_>".to_string(),
            comment,
            needs_handle_value: true,
            needs_object: false,
            needs_promise: false,
            needs_function: false,
        }
    }

    fn promise() -> Self {
        Self {
            text: "Promise<'_>".to_string(),
            comment: None,
            needs_handle_value: false,
            needs_object: false,
            needs_promise: true,
            needs_function: false,
        }
    }

    fn object() -> Self {
        Self {
            text: "Object<'_>".to_string(),
            comment: None,
            needs_handle_value: false,
            needs_object: true,
            needs_promise: false,
            needs_function: false,
        }
    }

    fn function() -> Self {
        Self {
            text: "Function<'_>".to_string(),
            comment: None,
            needs_handle_value: false,
            needs_object: false,
            needs_promise: false,
            needs_function: true,
        }
    }

    fn optional(inner: Self) -> Self {
        Self {
            text: format!("Option<{}>", inner.text),
            comment: inner.comment,
            needs_handle_value: inner.needs_handle_value,
            needs_object: inner.needs_object,
            needs_promise: inner.needs_promise,
            needs_function: inner.needs_function,
        }
    }

    fn vec(inner: Self) -> Self {
        Self {
            text: format!("Vec<{}>", inner.text),
            comment: inner.comment,
            needs_handle_value: inner.needs_handle_value,
            needs_object: inner.needs_object,
            needs_promise: inner.needs_promise,
            needs_function: inner.needs_function,
        }
    }
}

/// Map a WebIDL return type to a Rust type string.
pub fn map_return_type(ty: &ReturnType<'_>) -> RustType {
    match ty {
        ReturnType::Undefined(_) => RustType::simple("()"),
        ReturnType::Type(t) => map_type(t),
    }
}

/// Map a WebIDL type to a Rust type string, using rooted handle types.
pub fn map_type(ty: &Type<'_>) -> RustType {
    match ty {
        Type::Single(s) => map_single_type(s),
        Type::Union(may_be_null) => {
            let members: Vec<String> = may_be_null
                .type_
                .body
                .list
                .iter()
                .map(format_union_member)
                .collect();
            let base = RustType::handle_value(Some(format!("WebIDL: ({})", members.join(" or "))));
            if may_be_null.q_mark.is_some() {
                RustType::optional(base)
            } else {
                base
            }
        }
    }
}

fn map_single_type(ty: &SingleType<'_>) -> RustType {
    match ty {
        SingleType::Any(_) => RustType::handle_value(None),
        SingleType::NonAny(na) => map_non_any_type(na),
    }
}

fn map_non_any_type(ty: &NonAnyType<'_>) -> RustType {
    match ty {
        // Primitive types (each wrapped in MayBeNull)
        NonAnyType::Boolean(mbn) => maybe_null(RustType::simple("bool"), mbn.q_mark.is_some()),
        NonAnyType::Byte(mbn) => maybe_null(RustType::simple("i8"), mbn.q_mark.is_some()),
        NonAnyType::Octet(mbn) => maybe_null(RustType::simple("u8"), mbn.q_mark.is_some()),

        // Integer types
        NonAnyType::Integer(mbn) => {
            let base = map_integer_type(&mbn.type_);
            maybe_null(base, mbn.q_mark.is_some())
        }

        // Floating point types
        NonAnyType::FloatingPoint(mbn) => {
            let base = map_float_type(&mbn.type_);
            maybe_null(base, mbn.q_mark.is_some())
        }

        // String types
        NonAnyType::ByteString(mbn) => maybe_null(RustType::simple("String"), mbn.q_mark.is_some()),
        NonAnyType::DOMString(mbn) => maybe_null(RustType::simple("String"), mbn.q_mark.is_some()),
        NonAnyType::USVString(mbn) => maybe_null(RustType::simple("String"), mbn.q_mark.is_some()),

        // Object type
        NonAnyType::Object(mbn) => maybe_null(RustType::object(), mbn.q_mark.is_some()),

        // Sequence<T>
        NonAnyType::Sequence(mbn) => {
            let inner = map_type(&mbn.type_.generics.body);
            let base = RustType::vec(inner);
            maybe_null(base, mbn.q_mark.is_some())
        }

        // Promise<T>
        NonAnyType::Promise(_) => RustType::promise(),

        // FrozenArray<T> — treat like sequence
        NonAnyType::FrozenArrayType(mbn) => {
            let inner = map_type(&mbn.type_.generics.body);
            let base = RustType::vec(inner);
            maybe_null(base, mbn.q_mark.is_some())
        }

        // Record<K, V>
        NonAnyType::RecordType(mbn) => {
            let key = map_record_key_type(&mbn.type_.generics.body.0);
            let val = map_type(&mbn.type_.generics.body.2);
            let base =
                RustType::handle_value(Some(format!("WebIDL: record<{}, {}>", key, val.text)));
            maybe_null(base, mbn.q_mark.is_some())
        }

        // Named type references (interfaces, enums, typedefs)
        NonAnyType::Identifier(mbn) => {
            let name = mbn.type_.0;
            let base = map_named_type(name);
            maybe_null(base, mbn.q_mark.is_some())
        }

        // Symbol
        NonAnyType::Symbol(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: symbol".to_string())),
            mbn.q_mark.is_some(),
        ),

        // Error
        NonAnyType::Error(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: Error".to_string())),
            mbn.q_mark.is_some(),
        ),

        // Typed arrays and buffer types
        NonAnyType::ArrayBuffer(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: ArrayBuffer".to_string())),
            mbn.q_mark.is_some(),
        ),
        NonAnyType::DataView(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: DataView".to_string())),
            mbn.q_mark.is_some(),
        ),
        NonAnyType::Int8Array(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: Int8Array".to_string())),
            mbn.q_mark.is_some(),
        ),
        NonAnyType::Int16Array(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: Int16Array".to_string())),
            mbn.q_mark.is_some(),
        ),
        NonAnyType::Int32Array(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: Int32Array".to_string())),
            mbn.q_mark.is_some(),
        ),
        NonAnyType::Uint8Array(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: Uint8Array".to_string())),
            mbn.q_mark.is_some(),
        ),
        NonAnyType::Uint16Array(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: Uint16Array".to_string())),
            mbn.q_mark.is_some(),
        ),
        NonAnyType::Uint32Array(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: Uint32Array".to_string())),
            mbn.q_mark.is_some(),
        ),
        NonAnyType::Uint8ClampedArray(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: Uint8ClampedArray".to_string())),
            mbn.q_mark.is_some(),
        ),
        NonAnyType::Float32Array(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: Float32Array".to_string())),
            mbn.q_mark.is_some(),
        ),
        NonAnyType::Float64Array(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: Float64Array".to_string())),
            mbn.q_mark.is_some(),
        ),
        NonAnyType::ArrayBufferView(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: ArrayBufferView".to_string())),
            mbn.q_mark.is_some(),
        ),
        NonAnyType::BufferSource(mbn) => maybe_null(
            RustType::handle_value(Some("WebIDL: BufferSource".to_string())),
            mbn.q_mark.is_some(),
        ),
    }
}

fn maybe_null(base: RustType, nullable: bool) -> RustType {
    if nullable {
        RustType::optional(base)
    } else {
        base
    }
}

fn map_integer_type(ty: &IntegerType) -> RustType {
    match ty {
        IntegerType::Short(s) if s.unsigned.is_some() => RustType::simple("u16"),
        IntegerType::Short(_) => RustType::simple("i16"),
        IntegerType::Long(l) if l.unsigned.is_some() => RustType::simple("u32"),
        IntegerType::Long(_) => RustType::simple("i32"),
        IntegerType::LongLong(ll) if ll.unsigned.is_some() => RustType::simple("u64"),
        IntegerType::LongLong(_) => RustType::simple("i64"),
    }
}

fn map_float_type(ty: &FloatingPointType) -> RustType {
    match ty {
        FloatingPointType::Float(_) => RustType::simple("f32"),
        FloatingPointType::Double(_) => RustType::simple("f64"),
    }
}

fn map_record_key_type(ty: &weedle::types::RecordKeyType<'_>) -> &'static str {
    match ty {
        weedle::types::RecordKeyType::Byte(_) => "ByteString",
        weedle::types::RecordKeyType::DOM(_) => "DOMString",
        weedle::types::RecordKeyType::USV(_) => "USVString",
        weedle::types::RecordKeyType::NonAny(_) => "NonAny",
    }
}

/// Map a well-known interface or type name to its Rust representation.
fn map_named_type(name: &str) -> RustType {
    match name {
        // Types that have native Rust mappings (Web platform buffer types)
        "ArrayBuffer" | "SharedArrayBuffer" | "ArrayBufferView" | "BufferSource" | "DataView" => {
            RustType::handle_value(Some(format!("WebIDL: {name}")))
        }

        // Typed array types
        "Uint8Array" | "Uint16Array" | "Uint32Array" | "Uint8ClampedArray" | "Int8Array"
        | "Int16Array" | "Int32Array" | "Float32Array" | "Float64Array" | "BigInt64Array"
        | "BigUint64Array" => RustType::handle_value(Some(format!("WebIDL: {name}"))),

        // The WebIDL `Function` callback alias — `js::Function<'s>` is a Stack-rooted handle.
        "Function" => RustType::function(),

        // Everything else: assume it's an interface, dictionary, or enum.
        // If the type is from this spec, `add_interface_lifetimes` adds the
        // GC lifetime; if from another spec, the user fills in the import.
        _ => RustType::simple(name),
    }
}

fn format_union_member(member: &UnionMemberType<'_>) -> String {
    match member {
        UnionMemberType::Single(s) => {
            let t = map_non_any_type(&s.type_);
            t.text
        }
        UnionMemberType::Union(u) => {
            let members: Vec<String> = u.type_.body.list.iter().map(format_union_member).collect();
            format!("({})", members.join(" or "))
        }
    }
}

/// Map a WebIDL constant type to a Rust type.
pub fn map_const_type(ct: &ConstType<'_>) -> RustType {
    match ct {
        ConstType::Boolean(_) => RustType::simple("bool"),
        ConstType::Byte(_) => RustType::simple("i8"),
        ConstType::Octet(_) => RustType::simple("u8"),
        ConstType::Integer(mbn) => map_integer_type(&mbn.type_),
        ConstType::FloatingPoint(mbn) => map_float_type(&mbn.type_),
        ConstType::Identifier(mbn) => map_named_type(mbn.type_.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_and_map(idl_type: &str) -> RustType {
        let idl = format!("typedef {idl_type} TestType;");
        let parsed = weedle::parse(&idl).expect("failed to parse IDL");
        let def = &parsed[0];
        if let weedle::Definition::Typedef(td) = def {
            map_type(&td.type_.type_)
        } else {
            panic!("expected typedef");
        }
    }

    #[test]
    fn primitives() {
        assert_eq!(parse_and_map("boolean").text, "bool");
        assert_eq!(parse_and_map("byte").text, "i8");
        assert_eq!(parse_and_map("octet").text, "u8");
        assert_eq!(parse_and_map("unsigned short").text, "u16");
        assert_eq!(parse_and_map("short").text, "i16");
        assert_eq!(parse_and_map("unsigned long").text, "u32");
        assert_eq!(parse_and_map("long").text, "i32");
        assert_eq!(parse_and_map("unsigned long long").text, "u64");
        assert_eq!(parse_and_map("long long").text, "i64");
        assert_eq!(parse_and_map("float").text, "f32");
        assert_eq!(parse_and_map("double").text, "f64");
    }

    #[test]
    fn strings() {
        assert_eq!(parse_and_map("DOMString").text, "String");
        assert_eq!(parse_and_map("USVString").text, "String");
        assert_eq!(parse_and_map("ByteString").text, "String");
    }

    #[test]
    fn any_maps_to_handle_value() {
        let t = parse_and_map("any");
        assert_eq!(t.text, "HandleValue<'_>");
        assert!(t.needs_handle_value);
    }

    #[test]
    fn nullable() {
        let t = parse_and_map("DOMString?");
        assert_eq!(t.text, "Option<String>");
    }

    #[test]
    fn sequence() {
        let t = parse_and_map("sequence<unsigned long>");
        assert_eq!(t.text, "Vec<u32>");
    }

    #[test]
    fn promise() {
        let t = parse_and_map("Promise<undefined>");
        assert_eq!(t.text, "Promise<'_>");
        assert!(t.needs_promise);
    }

    #[test]
    fn named_type_interface() {
        // Interface names not in the buffer/typed-array lists pass through
        // as simple types — `add_interface_lifetimes` adds GC lifetimes later
        let t = parse_and_map("ReadableStream");
        assert_eq!(t.text, "ReadableStream");
        assert!(!t.needs_handle_value);
    }

    #[test]
    fn named_type_unknown_interface() {
        let t = parse_and_map("Headers");
        assert_eq!(t.text, "Headers");
        assert!(!t.needs_handle_value);
    }

    #[test]
    fn object_type() {
        let t = parse_and_map("object");
        assert_eq!(t.text, "Object<'_>");
        assert!(t.needs_object);
    }
}
