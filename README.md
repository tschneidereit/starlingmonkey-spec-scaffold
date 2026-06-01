# starlingmonkey-spec-scaffold

A command-line tool that generates Rust scaffolding for [StarlingMonkey](https://github.com/bytecodealliance/StarlingMonkey) builtins from WHATWG/W3C web specifications. (For now, the project it's targeting is in a [different repo](https://github.com/tschneidereit/starlingmonkey-ng/), which will eventually replace the official one.)

Given a spec (by URL or shorthand name), it fetches the HTML, extracts the WebIDL definitions and algorithm steps, and emits Rust source files annotated with StarlingMonkey's `#[webidl_interface]`, `#[webidl_methods]`, and related macros.

The generated code is scaffolding, not a working implementation: function bodies are `todo!()`, and the algorithm steps from the spec are inlined as comments for a developer to implement against.

The code *should* compile as-is, but obviously not work at all. Please file bugs if it doesn't compile or misinterprets specifications.

**Note**: This is extremely heuristics-driven and not particularly principled in its approach! That's entirely okay for the use case it was developed for: creating a baseline for an implementation that has all the relevant information included in the form of comments containing all steps of the specification. The idea is to then go and implement the spec, comparing the code against the commens and validating results against WPT tests as you go.

## Building

```sh
cargo build --release
```

The binary is produced at `target/release/starlingmonkey-spec-scaffold`.

## Usage

```sh
starlingmonkey-spec-scaffold <spec> [--output-dir <dir>]
```

- `<spec>` — a shorthand name (see below) or a full `http(s)://` URL.
- `--output-dir <dir>`, `-o <dir>` — directory to write generated files into. If omitted, all files are printed to stdout separated by `// === filename ===` markers.

Progress (spec URL, byte count, extraction and parse statistics) is written to stderr.

### Examples

```sh
# Print the scaffolding for the URL spec to stdout
starlingmonkey-spec-scaffold url

# Write generated files into ./url-builtin/
starlingmonkey-spec-scaffold url --output-dir ./url-builtin

# Use a full URL for a spec without a shorthand
starlingmonkey-spec-scaffold https://notifications.spec.whatwg.org/
```

### Known spec shorthands

| Shorthand  | Spec URL                          |
|------------|-----------------------------------|
| `url`      | https://url.spec.whatwg.org/      |
| `fetch`    | https://fetch.spec.whatwg.org/    |
| `xhr`      | https://xhr.spec.whatwg.org/      |
| `dom`      | https://dom.spec.whatwg.org/      |
| `html`     | https://html.spec.whatwg.org/     |
| `streams`  | https://streams.spec.whatwg.org/  |
| `encoding` | https://encoding.spec.whatwg.org/ |
| `infra`    | https://infra.spec.whatwg.org/    |
| `console`  | https://console.spec.whatwg.org/  |

## What it generates

- **One file per interface** (`<interface>.rs`), containing a `#[webidl_interface]` struct and a `#[webidl_methods]` impl with constructors, methods, getters/setters, and constants. Mixins are folded into the interfaces that include them.
- **Dictionaries, enums, and typedefs**, emitted as `#[webidl_dictionary]` structs, enums, and type aliases.
- **`globals.rs`** for free functions and constants defined on global scopes (`Window`, `WindowOrWorkerGlobalScope`, etc.).
- **`algorithms.rs`** for standalone spec algorithms not attached to an interface.
- **`lib.rs`** tying the modules together with an `add_to_global` entry point.

Each generated item links back to its spec anchor via a doc comment, and each method body lists the spec's numbered algorithm steps as comments above the `todo!()`.

WebIDL types are mapped to Rust signatures using StarlingMonkey's rooted handle types (e.g. `DOMString` → `String`, `Promise<T>` → `Promise<'_>`, `object` → `Object<'_>`, `any` and unions → `HandleValue<'_>`). Lossy mappings carry a comment noting the original WebIDL type.

### Example output

Running `starlingmonkey-spec-scaffold url` produces, among others, a `url.rs` beginning:

```rust
//! <https://url.spec.whatwg.org/>

use core_runtime::webidl_methods;
use core_runtime::webidl_interface;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use super::url_search_params::URLSearchParams;

/// <https://url.spec.whatwg.org/#url-class>
#[webidl_interface]
pub struct URL {
    // TODO: Add internal state fields
}

#[webidl_methods]
impl URL {
    /// <https://url.spec.whatwg.org/#dom-url-url>
    #[constructor]
    fn new(&self, scope: &Scope<'_>, url: String, base: Option<String>) -> Result<(), ExnThrown> {
        // Step 1: Let _parsedURL_ be the result of running the `API URL parser` on _url_ with
        //         _base_, if given.
        // Step 2: If _parsedURL_ is failure, then `throw` a ``TypeError``.
        // Step 3: `Initialize` `this` with _parsedURL_.
        todo!()
    }

    /// <https://url.spec.whatwg.org/#dom-url-href>
    #[getter]
    fn href(&self) -> String {
        // Step 1: Return the serialization of this’s URL.
        todo!()
    }

    // ...
}
```

## Notes and limitations

- The generated code targets StarlingMonkey's `core_runtime` and `js` crates and is meant to be dropped into a StarlingMonkey builtin; it does not compile on its own.
- Output is a starting point. Internal state fields, type details across spec boundaries, and all behavior are left for the developer to fill in.
- Fetching specs requires network access. Specs are read live from their canonical URLs each run.

## License

Apache-2.0 WITH LLVM-exception, see [LICENSE](./LICENSE) for details.
