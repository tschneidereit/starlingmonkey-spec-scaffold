// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Extracts WebIDL blocks and algorithm steps from spec HTML.

use std::collections::HashMap;

use scraper::{ElementRef, Html, Node, Selector};

use crate::idl::InternalSlot;

/// A WebIDL block extracted from a `<pre class="idl">` element.
#[derive(Debug, Clone)]
pub struct IdlBlock {
    pub text: String,
}

/// The kind of construct an algorithm is associated with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlgorithmKind {
    /// Constructor: "The new ClassName(args) constructor steps are:"
    Constructor { class: String },
    /// Instance or static method: "The {static} methodName(args) method steps are:"
    Method { name: String, is_static: bool },
    /// Getter: "The attrName getter steps are {to ...}:"
    Getter { name: String },
    /// Setter: "The attrName setter steps are:"
    Setter { name: String },
    /// Standalone algorithm: "The API URL parser takes..." / "To initialize..."
    Standalone { name: String },
}

/// A single algorithm step with its hierarchical label.
///
/// Labels mirror the spec's step structure:
/// - top-level steps: "1", "2", ...
/// - nested substeps (e.g. inside a loop): "5.1", "5.5.2", ...
/// - switch branches: "10 `Blob`" (the branch condition after the step number)
/// - substeps of a switch branch: "10 `Blob`.1"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The hierarchical step label, e.g. "5.1" or "10 `Blob`".
    pub label: String,
    /// The step text, with inline markup annotations. Unordered sub-lists
    /// are rendered as "- "-prefixed lines separated by newlines.
    pub text: String,
}

impl Step {
    /// Create a step with a plain top-level number label.
    pub fn numbered(n: usize, text: impl Into<String>) -> Self {
        Step {
            label: n.to_string(),
            text: text.into(),
        }
    }
}

/// An algorithm extracted from spec prose, associated with a method or concept.
#[derive(Debug, Clone)]
pub struct AlgorithmSteps {
    /// The heading text that introduces the algorithm, with inline markup kept
    /// as annotations (`` `link` ``, `_var_`, …) the same way step text is.
    pub heading: String,
    /// What kind of construct this algorithm is for.
    pub kind: AlgorithmKind,
    /// The numbered steps in the algorithm. For one-liner descriptions
    /// (e.g., "The origin getter steps are to return..."), this contains
    /// a single entry with the description text.
    pub steps: Vec<Step>,
    /// The interface this algorithm belongs to, from `data-algorithm-for`
    /// or section context. Empty when not available.
    pub interface: String,
    /// The `id` attribute of the `<dfn>` element that defines this algorithm,
    /// used as the URL fragment for linking to the spec. Empty when not available.
    pub fragment: String,
}

/// Extract all WebIDL blocks from the spec HTML.
///
/// Specs use two markup conventions:
/// - `<pre class="idl">` — the bikeshed default used by URL, Fetch, Streams, etc.
///   (often combined with other classes, e.g. `class="def highlight idl"`).
/// - `<pre><code class='idl'>` — the HTML spec wraps its IDL in a `<code>` child.
pub fn extract_idl_blocks(html: &str) -> Vec<IdlBlock> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("pre.idl, pre > code.idl").expect("valid CSS selector");

    document
        .select(&selector)
        .map(|el| {
            let text = el.text().collect::<String>();
            IdlBlock { text }
        })
        .collect()
}

/// Extract algorithm steps from spec prose.
///
/// WHATWG specs conventionally structure algorithms in two ways:
///
/// 1. **Inline headings** (`<p>`, `<dt>`, `<dd>`): text like "The foo() method
///    steps are:" followed by a sibling `<ol>` with numbered steps.
///
/// 2. **Algorithm divs** (`<div class="algorithm">`): the heading text is a
///    direct child of the `<div>`, and the `<ol>` is also a child (not a
///    sibling). The `data-algorithm-for` attribute names the interface.
///
/// One-liner algorithms (e.g., "The origin getter steps are to return...")
/// are also captured, with the description as a single step.
pub fn extract_algorithms(html: &str) -> Vec<AlgorithmSteps> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    // Pass 1: inline headings in <p>, <dt>, <dd> with sibling <ol>
    let p_selector = Selector::parse("p, dt, dd").expect("valid CSS selector");
    let dfn_for_sel = Selector::parse("dfn[data-dfn-for]").expect("valid CSS selector");
    let dfn_id_sel = Selector::parse("dfn[id]").expect("valid CSS selector");

    for element in document.select(&p_selector) {
        // Skip <p> elements inside algorithm divs — Pass 2 handles those
        // with proper interface resolution from data-algorithm-for / data-dfn-for.
        if element.ancestors().filter_map(ElementRef::wrap).any(|a| {
            a.value().name() == "div"
                && (a.value().attr("data-algorithm").is_some()
                    || a.value().classes().any(|c| c == "algorithm"))
        }) {
            continue;
        }

        let raw_text = element.text().collect::<String>();
        let text = normalize_whitespace(&raw_text);

        if !looks_like_algorithm_heading(&text) {
            continue;
        }

        let kinds = classify_heading(&text);
        if kinds.is_empty() {
            continue;
        }

        // The heading is reported with its inline markup preserved; only
        // classification above runs on the plain text.
        let formatted = normalize_whitespace(&extract_formatted_text(&element));

        // Extract the interface name from a nested <dfn data-dfn-for> if present.
        // FileAPI-style specs define methods inside <p> elements that contain
        // <dfn data-dfn-for="Blob" data-dfn-type="method"> tags.
        let interface = element
            .select(&dfn_for_sel)
            .find_map(|dfn| dfn.value().attr("data-dfn-for").filter(|s| !s.is_empty()))
            .unwrap_or_default()
            .to_string();

        // Extract the fragment ID from the first <dfn id="..."> in the heading.
        let fragment = element
            .select(&dfn_id_sel)
            .find_map(|dfn| dfn.value().attr("id").filter(|s| !s.is_empty()))
            .unwrap_or_default()
            .to_string();

        // Check for one-liner: heading contains "steps are to" with no following <ol>
        let is_one_liner = is_one_liner_algorithm(&text);

        if is_one_liner {
            // Extract the description from the heading itself
            let description = extract_one_liner_description(&formatted);
            for kind in kinds {
                results.push(AlgorithmSteps {
                    heading: formatted.clone(),
                    kind,
                    steps: vec![Step::numbered(1, description.clone())],
                    interface: interface.clone(),
                    fragment: fragment.clone(),
                });
            }
            continue;
        }

        // Walk next siblings to find the first <ol>
        let mut found_ol = false;
        for sibling in element.next_siblings() {
            let Some(sib) = ElementRef::wrap(sibling) else {
                continue; // text node
            };
            let name = sib.value().name();

            if name == "ol" {
                let steps = extract_ol_steps(&sib);
                if !steps.is_empty() {
                    for kind in &kinds {
                        results.push(AlgorithmSteps {
                            heading: formatted.clone(),
                            kind: kind.clone(),
                            steps: steps.clone(),
                            interface: interface.clone(),
                            fragment: fragment.clone(),
                        });
                    }
                }
                found_ol = true;
                break;
            }

            // Stop at section headings or other algorithm headings
            if matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
                break;
            }

            // Stop at <p> that is itself an algorithm heading (adjacent algorithms)
            if name == "p" {
                let sib_text = normalize_whitespace(&sib.text().collect::<String>());
                if looks_like_algorithm_heading(&sib_text) {
                    break;
                }
            }
        }

        // If no <ol> was found and it wasn't a one-liner, the heading might
        // describe steps inline. Capture it as a standalone description.
        if !found_ol && !is_one_liner {
            // Check if this is a "runs these steps:" heading where the <ol> wasn't
            // immediately adjacent — skip these since we can't reliably extract steps.
        }
    }

    // Pass 2: <div class="algorithm"> or <div data-algorithm> elements where
    // the heading text and <ol> are both children of the div. The Streams spec
    // uses class="algorithm"; the HTML spec uses the data-algorithm attribute.
    let div_selector =
        Selector::parse("div.algorithm, div[data-algorithm]").expect("valid CSS selector");
    let dfn_type_sel = Selector::parse("dfn[data-dfn-type]").expect("valid CSS selector");

    for element in document.select(&div_selector) {
        // Extract the interface name from data-algorithm-for, if present.
        // Fall back to data-dfn-for on a nested <dfn> element (used by the
        // HTML spec for methods like structuredClone on WindowOrWorkerGlobalScope).
        let interface = element
            .value()
            .attr("data-algorithm-for")
            .filter(|s| !s.is_empty())
            .or_else(|| {
                element
                    .select(&dfn_for_sel)
                    .next()?
                    .value()
                    .attr("data-dfn-for")
            })
            .unwrap_or_default()
            .to_string();

        // Extract the fragment ID from the first <dfn id="..."> in the div.
        let fragment = element
            .select(&dfn_id_sel)
            .find_map(|dfn| dfn.value().attr("id").filter(|s| !s.is_empty()))
            .unwrap_or_default()
            .to_string();

        // Collect heading text: all content before the first <ol> child, once
        // as plain text (what classification matches against) and once with
        // inline markup preserved (what gets reported).
        let text = normalize_whitespace(&extract_div_algorithm_heading(&element, false));
        let formatted = normalize_whitespace(&extract_div_algorithm_heading(&element, true));

        // Classification priority for algorithm divs:
        // 1. Textual pattern matching on heading prose (most specific — captures
        //    details like is_static that structured attributes omit)
        // 2. data-algorithm attribute — reliable, spec-author-curated name/kind
        // 3. <dfn data-dfn-type> metadata inside the div
        let data_algo = element.value().attr("data-algorithm").unwrap_or_default();
        let mut kinds = classify_heading(&text);

        if kinds.is_empty() {
            kinds = classify_from_data_attribute(data_algo, &interface);
        }

        if kinds.is_empty() {
            // Fall back to <dfn data-dfn-type> inside the div (e.g., FileAPI
            // uses <dfn data-dfn-type="method"> without "method steps" phrasing).
            if let Some(kind) = classify_from_dfn(&element, &dfn_type_sel) {
                kinds.push(kind);
            }
        }

        if kinds.is_empty() {
            continue;
        }

        // Find the first <ol> that is a direct child of the div. Descendant
        // <ol>s must not count: they may belong to a switch arm (URL's
        // "origin") or to a nested algorithm div (Streams' "read all bytes"),
        // and treating them as this algorithm's steps would steal them.
        let has_ol = direct_child_elements(&element, "ol").next();

        if let Some(ol) = has_ol {
            let steps = extract_ol_steps(&ol);
            if !steps.is_empty() {
                for kind in &kinds {
                    results.push(AlgorithmSteps {
                        heading: formatted.clone(),
                        kind: kind.clone(),
                        steps: steps.clone(),
                        interface: interface.clone(),
                        fragment: fragment.clone(),
                    });
                }
            }
        } else if element
            .children()
            .filter_map(ElementRef::wrap)
            .any(|c| is_step_block(&c))
        {
            // No <ol> child, but the algorithm body is a top-level <ul>
            // condition list or <dl class="switch"> — extract it as a single
            // step (with bullet lines / branch substeps) rather than
            // flattening everything into the heading.
            let mut steps = Vec::new();
            collect_step_body(&element, "1", &mut steps);
            if !steps.is_empty() {
                for kind in kinds {
                    results.push(AlgorithmSteps {
                        heading: formatted.clone(),
                        kind,
                        steps: steps.clone(),
                        interface: interface.clone(),
                        fragment: fragment.clone(),
                    });
                }
            }
        } else {
            // No step-bearing block at all — the heading is the algorithm's
            // entire prose, so it also becomes the single step comment.
            let is_one_liner = is_one_liner_algorithm(&text);
            let description = if is_one_liner {
                extract_one_liner_description(&formatted)
            } else {
                // Extract a useful description from the heading text by stripping
                // the introductory phrase.
                extract_div_oneliner_description(&formatted)
            };
            if !description.is_empty() {
                for kind in kinds {
                    results.push(AlgorithmSteps {
                        heading: formatted.clone(),
                        kind,
                        steps: vec![Step::numbered(1, description.clone())],
                        interface: interface.clone(),
                        fragment: fragment.clone(),
                    });
                }
            }
        }
    }

    results
}

/// Extract the heading text from a `<div class="algorithm">` element.
///
/// Collects all text content from nodes before the first step-bearing block
/// child (`<ol>`, `<ul>`, or `<dl class="switch">`) or nested algorithm div,
/// which is the algorithm heading (e.g., "The cancel(reason) method steps are:").
///
/// With `formatted`, inline markup is preserved as annotations (`` `link` ``,
/// `_var_`, …), the same way step text is; this is used when the heading itself
/// becomes the algorithm's description, so prose-only algorithms read like
/// regular steps. Heading classification uses the plain variant.
fn extract_div_algorithm_heading(div: &ElementRef, formatted: bool) -> String {
    let mut heading = String::new();
    for child in div.children() {
        if let Some(child_el) = ElementRef::wrap(child) {
            if is_step_block(&child_el) || is_algorithm_div(&child_el) {
                break;
            }
            // Recurse into inline elements (dfn, code, etc.) to get their text
            if formatted {
                push_merging_backticks(&mut heading, &format_element(&child_el));
            } else {
                heading.push_str(&child_el.text().collect::<String>());
            }
        } else if let Node::Text(t) = child.value() {
            push_merging_backticks(&mut heading, t);
        }
    }
    heading
}

/// Check whether an element is itself an algorithm div (a nested algorithm
/// definition, processed separately).
fn is_algorithm_div(el: &ElementRef) -> bool {
    el.value().name() == "div"
        && (el.value().attr("data-algorithm").is_some()
            || el.value().classes().any(|c| c == "algorithm"))
}

/// Check whether an element is a block that holds algorithm steps: a numbered
/// list, a bulleted condition list, or a switch.
fn is_step_block(el: &ElementRef) -> bool {
    match el.value().name() {
        "ol" | "ul" => true,
        "dl" => el.value().classes().any(|c| c == "switch"),
        _ => false,
    }
}

/// Extract steps from a top-level `<ol>` element, recursing into nested
/// structures so each spec step becomes its own labeled entry:
///
/// - nested `<ol>` substeps get dotted labels ("5.1", "5.5.2", ...)
/// - `<dl class="switch">` branches get the branch condition appended to the
///   parent step's label ("10 `Blob`"), with substeps as "10 `Blob`.1"
/// - `<ul>` sub-lists stay part of their step's text as "- "-prefixed lines
fn extract_ol_steps(ol: &ElementRef) -> Vec<Step> {
    let mut steps = Vec::new();
    collect_ol_steps(ol, "", &mut steps);
    steps
}

/// Collect steps from an `<ol>`'s direct `<li>` children, labeling each with
/// `prefix.N` (or just `N` at the top level).
fn collect_ol_steps(ol: &ElementRef, prefix: &str, out: &mut Vec<Step>) {
    for (i, li) in direct_child_elements(ol, "li").enumerate() {
        let label = if prefix.is_empty() {
            (i + 1).to_string()
        } else {
            format!("{prefix}.{}", i + 1)
        };
        collect_step_body(&li, &label, out);
    }
}

/// Collect the step for one `<li>` (or switch-branch `<dd>`) body, then recurse
/// into any nested step structures it contains.
///
/// Inline content (paragraphs, text, `<ul>` sub-lists) becomes this step's
/// text; nested `<ol>` lists and `<dl class="switch">` branches become
/// separate steps with derived labels. If the body has no inline text of its
/// own (e.g. a switch branch that consists solely of substeps), no step is
/// emitted for it — the nested steps' labels carry the context.
fn collect_step_body(body: &ElementRef, label: &str, out: &mut Vec<Step>) {
    enum NestedBlock<'a> {
        Substeps(ElementRef<'a>),
        Switch(ElementRef<'a>),
    }

    let mut lines: Vec<String> = Vec::new();
    let mut inline = String::new();
    let mut nested = Vec::new();

    let flush_inline = |inline: &mut String, lines: &mut Vec<String>| {
        let text = normalize_step_text(inline);
        if !text.is_empty() {
            lines.push(text);
        }
        inline.clear();
    };

    for child in body.children() {
        if let Some(el) = ElementRef::wrap(child) {
            match el.value().name() {
                "ol" => nested.push(NestedBlock::Substeps(el)),
                "dl" if el.value().classes().any(|c| c == "switch") => {
                    nested.push(NestedBlock::Switch(el));
                }
                "dl" => {
                    flush_inline(&mut inline, &mut lines);
                    collect_dl_lines(&el, &mut lines);
                }
                "ul" => {
                    flush_inline(&mut inline, &mut lines);
                    for item in direct_child_elements(&el, "li") {
                        let text = normalize_step_text(&extract_formatted_text(&item));
                        if !text.is_empty() {
                            lines.push(format!("- {text}"));
                        }
                    }
                }
                _ => push_merging_backticks(&mut inline, &format_element(&el)),
            }
        } else if let Node::Text(t) = child.value() {
            push_merging_backticks(&mut inline, t);
        }
    }
    flush_inline(&mut inline, &mut lines);

    if !lines.is_empty() {
        out.push(Step {
            label: label.to_string(),
            text: lines.join("\n"),
        });
    }

    for block in nested {
        match block {
            NestedBlock::Substeps(ol) => collect_ol_steps(&ol, label, out),
            NestedBlock::Switch(dl) => collect_switch_steps(&dl, label, out),
        }
    }
}

/// Collect `- name: value` bullet lines from a plain (non-switch) `<dl>`'s
/// definition pairs, e.g. the request-property list in step 12 of the Request
/// constructor. Consecutive `<dt>`s share the following `<dd>`'s value.
fn collect_dl_lines(dl: &ElementRef, out: &mut Vec<String>) {
    let mut names: Vec<String> = Vec::new();
    for child in dl.children().filter_map(ElementRef::wrap) {
        match child.value().name() {
            "dt" => {
                let text = normalize_whitespace(&extract_formatted_text(&child));
                if !text.is_empty() {
                    names.push(text);
                }
            }
            "dd" => {
                let value = normalize_step_text(&extract_formatted_text(&child));
                if !names.is_empty() || !value.is_empty() {
                    out.push(format!("- {}: {value}", names.join(", ")));
                }
                names.clear();
            }
            _ => {}
        }
    }
}

/// Collect steps from a `<dl class="switch">`'s branches.
///
/// Each `<dt>` holds a branch condition; consecutive `<dt>`s share the
/// following `<dd>`. The branch label is the parent step's label followed by
/// the condition(s), e.g. "10 `Blob`" or "4 `day`, `week`".
fn collect_switch_steps(dl: &ElementRef, parent_label: &str, out: &mut Vec<Step>) {
    let mut conditions: Vec<String> = Vec::new();
    for child in dl.children().filter_map(ElementRef::wrap) {
        match child.value().name() {
            "dt" => {
                let text = normalize_whitespace(&extract_formatted_text(&child));
                if !text.is_empty() {
                    conditions.push(text);
                }
            }
            "dd" => {
                let label = format!("{parent_label} {}", conditions.join(", "));
                collect_step_body(&child, &label, out);
                conditions.clear();
            }
            _ => {}
        }
    }
}

/// Classify an algorithm from the `data-algorithm` attribute on a `<div>`.
///
/// Spec authors encode the algorithm kind in the attribute value:
/// - `"hash setter"` / `"href getter"` → Setter / Getter
/// - `"URL(url, base)"` with `data-algorithm-for="URL"` → Constructor
/// - `"append(name, value)"` with `data-algorithm-for="URLSearchParams"` → Method
/// - `"blob-constructor"` / `"file-constructor"` → Constructor
/// - `"slice blob"`, `"API URL parser"` → Standalone
///
/// This is more reliable than textual pattern matching on the heading prose.
fn classify_from_data_attribute(data_algorithm: &str, interface: &str) -> Vec<AlgorithmKind> {
    let trimmed = data_algorithm.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    // "hash setter", "protocol setter" → Setter
    if let Some(name) = trimmed.strip_suffix(" setter") {
        return vec![AlgorithmKind::Setter {
            name: name.to_string(),
        }];
    }

    // "href getter" → Getter
    if let Some(name) = trimmed.strip_suffix(" getter") {
        return vec![AlgorithmKind::Getter {
            name: name.to_string(),
        }];
    }

    // "blob-constructor", "file-constructor" → Constructor
    if trimmed.ends_with("-constructor") || trimmed == "constructor" {
        // Extract class name: "blob-constructor" → "Blob" (capitalize first letter)
        if let Some(prefix) = trimmed.strip_suffix("-constructor") {
            let mut class = prefix.to_string();
            if let Some(first) = class.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            return vec![AlgorithmKind::Constructor { class }];
        }
        // Bare "constructor" — use the interface name
        if !interface.is_empty() {
            return vec![AlgorithmKind::Constructor {
                class: interface.to_string(),
            }];
        }
    }

    // Comma-separated multiple members:
    // "slice(start, end, contentType), slice(start, end), slice(start), slice()"
    // Split on "), " to handle each overload, but they map to the same method.
    if trimmed.contains("), ") {
        // Extract the method name from the first item
        if let Some(paren) = trimmed.find('(') {
            let name = trimmed[..paren].trim();
            if !name.is_empty() {
                let is_static = false; // can't tell from attr alone
                return vec![AlgorithmKind::Method {
                    name: name.to_string(),
                    is_static,
                }];
            }
        }
    }

    // Has parentheses: "URL(url, base)", "append(name, value)", "sort()"
    if let Some(paren) = trimmed.find('(') {
        let name = trimmed[..paren].trim();
        if !name.is_empty() {
            // Constructor if the name matches the interface
            if !interface.is_empty() && name == interface {
                return vec![AlgorithmKind::Constructor {
                    class: interface.to_string(),
                }];
            }
            // "ClassName/method" notation: "URL/extract an origin" handled below,
            // but "URLSearchParams(init)" is a constructor
            return vec![AlgorithmKind::Method {
                name: name.to_string(),
                is_static: false,
            }];
        }
    }

    // "URL/extract an origin" → method on URL
    if let Some(slash) = trimmed.find('/') {
        let class_part = &trimmed[..slash];
        let name_part = &trimmed[slash + 1..];
        if !class_part.is_empty() && !name_part.is_empty() {
            // Treat as a standalone algorithm associated with the class
            return vec![AlgorithmKind::Standalone {
                name: name_part.to_string(),
            }];
        }
    }

    // Bare name with an interface → could be a getter (like "origin" for "URL")
    // or an associated algorithm (like "initialize" for "URL").
    // The heading text determines the exact kind, so fall through to let textual
    // classification refine it. But provide a standalone fallback.
    vec![AlgorithmKind::Standalone {
        name: trimmed.to_string(),
    }]
}

/// Classify an algorithm from `<dfn data-dfn-type>` metadata inside a div.
///
/// Used as a fallback when text-based `classify_heading` fails, e.g. for
/// FileAPI-style headings like "The slice() method returns a new Blob..."
/// where no "method steps" phrasing appears.
fn classify_from_dfn(element: &ElementRef, dfn_type_sel: &Selector) -> Option<AlgorithmKind> {
    let dfn = element.select(dfn_type_sel).next()?;
    let dfn_type = dfn.value().attr("data-dfn-type")?;
    let dfn_text = normalize_whitespace(&dfn.text().collect::<String>());

    match dfn_type {
        "method" => {
            // Extract method name from dfn text like "slice()" or "readAsText(blob, encoding)"
            let name = dfn_text
                .find('(')
                .map(|p| &dfn_text[..p])
                .unwrap_or(&dfn_text)
                .trim();
            if name.is_empty() {
                return None;
            }
            let is_static = element
                .text()
                .collect::<String>()
                .to_lowercase()
                .contains("static method");
            Some(AlgorithmKind::Method {
                name: name.to_string(),
                is_static,
            })
        }
        "constructor" => {
            let class = dfn
                .value()
                .attr("data-dfn-for")
                .unwrap_or_default()
                .to_string();
            if class.is_empty() {
                // Try extracting from dfn text like "Blob()"
                let name = dfn_text
                    .find('(')
                    .map(|p| &dfn_text[..p])
                    .unwrap_or(&dfn_text)
                    .trim()
                    .to_string();
                if name.is_empty() {
                    return None;
                }
                Some(AlgorithmKind::Constructor { class: name })
            } else {
                Some(AlgorithmKind::Constructor { class })
            }
        }
        _ => None,
    }
}

/// Extract a one-liner description from a div algorithm heading.
///
/// For headings like "The createObjectURL(obj) static method must return the
/// result of adding an entry to the blob URL store for obj.", strips the
/// introductory "The X() method must " prefix and returns the action part.
fn extract_div_oneliner_description(text: &str) -> String {
    let lower = text.to_lowercase();
    // Look for "must " after the member name and extract the rest
    if let Some(must_pos) = lower.find("must ") {
        let rest = text[must_pos + 5..].trim();
        // Capitalize first letter and ensure trailing period
        if let Some(first) = rest.chars().next() {
            let mut desc = first.to_uppercase().to_string();
            desc.push_str(&rest[first.len_utf8()..]);
            if !desc.ends_with('.') {
                desc.push('.');
            }
            return desc;
        }
    }
    text.to_string()
}

// ---------------------------------------------------------------------------
// Spec definition extraction (fragment IDs + internal slots)
// ---------------------------------------------------------------------------

/// Extracted definition anchors and internal slots from the spec HTML.
#[derive(Debug, Default)]
pub struct SpecDefinitions {
    /// Maps interface name → heading fragment for the class section.
    /// E.g., "WritableStream" → "ws-class".
    pub class_sections: HashMap<String, String>,

    /// Maps (class_name, member_key) → dfn fragment ID.
    /// member_key is: "constructor", or the method/attribute name.
    /// E.g., ("WritableStream", "constructor") → "ws-constructor".
    pub member_fragments: HashMap<(String, String), String>,

    /// Maps interface name → list of internal slots.
    pub internal_slots: HashMap<String, Vec<InternalSlot>>,

    /// Maps dictionary name → dfn fragment ID.
    /// E.g., "ReadableWritablePair" → "dictdef-readablewritablepair".
    pub dictionary_fragments: HashMap<String, String>,
}

/// Extract spec definition anchors and internal slots from the HTML.
///
/// Parses `<dfn>` elements for member-level fragment IDs and `<h*>` headings
/// for class-section and internal-slots-section IDs. Parses internal slots
/// tables to extract slot names and descriptions, and prose "associated"
/// fields from paragraph text.
pub fn extract_spec_definitions(html: &str) -> SpecDefinitions {
    let document = Html::parse_document(html);
    let mut defs = SpecDefinitions::default();

    extract_dfn_fragments(&document, &mut defs);
    extract_class_headings(&document, &mut defs);
    extract_internal_slots_tables(&document, &mut defs);
    extract_associated_fields(&document, &mut defs);

    defs
}

/// Extract fragment IDs from `<dfn>` elements with `data-dfn-for` attributes.
fn extract_dfn_fragments(document: &Html, defs: &mut SpecDefinitions) {
    let selector = Selector::parse("dfn[id]").expect("valid CSS selector");

    for el in document.select(&selector) {
        let attrs = el.value();
        let Some(id) = attrs.attr("id") else {
            continue;
        };

        // Dictionary definitions: <dfn id="dictdef-xxx">
        if id.starts_with("dictdef-") {
            let text: String = el.text().collect();
            let name = text.trim().to_string();
            if !name.is_empty() {
                defs.dictionary_fragments.insert(name, id.to_string());
            }
            continue;
        }

        let Some(dfn_for) = attrs.attr("data-dfn-for") else {
            continue;
        };
        if dfn_for.is_empty() {
            continue;
        }

        let text: String = el.text().collect();
        let text = text.trim();
        if text.is_empty() {
            continue;
        }

        // Skip internal slot dfns (handled separately via tables)
        if text.starts_with("[[") {
            continue;
        }

        // Determine member key from the dfn text
        let member_key = if text.starts_with("new ") {
            // Constructor: "new WritableStream(underlyingSink, strategy)"
            "constructor".to_string()
        } else if let Some(paren_pos) = text.find('(') {
            // Method: "abort(reason)" → "abort"
            text[..paren_pos].trim().to_string()
        } else {
            // Attribute/getter: "locked"
            text.to_string()
        };

        if !member_key.is_empty() {
            defs.member_fragments
                .insert((dfn_for.to_string(), member_key), id.to_string());
        }
    }
}

/// Extract class section headings (e.g., `<h3 id="ws-class">The WritableStream class</h3>`).
fn extract_class_headings(document: &Html, defs: &mut SpecDefinitions) {
    let selector = Selector::parse("h2[id], h3[id], h4[id], h5[id]").expect("valid CSS selector");

    for el in document.select(&selector) {
        let Some(id) = el.value().attr("id") else {
            continue;
        };

        let text: String = el.text().collect();
        let text = text.trim();

        // Match "N.N. The ClassName class" or "N.N. ClassName class" patterns
        // Strip the section number prefix (e.g., "4.2. ")
        let stripped = strip_section_number(text);

        let class_name = stripped
            .strip_prefix("The ")
            .and_then(|rest| rest.strip_suffix(" class"))
            .or_else(|| stripped.strip_suffix(" class"));

        if let Some(class_name) = class_name {
            let class_name = class_name.trim().to_string();
            if !class_name.is_empty() {
                defs.class_sections.insert(class_name, id.to_string());
            }
        }
    }
}

/// Strip a leading section number like "4.2. " or "5.2.1. " from heading text.
fn strip_section_number(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut i = 0;
    // Skip digits and dots
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    // Skip trailing whitespace after number
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    // Only strip if we actually found a section number pattern
    if i > 0 && i < text.len() {
        &text[i..]
    } else {
        text
    }
}

/// Extract internal slots tables from sections with "Internal slots" headings.
fn extract_internal_slots_tables(document: &Html, defs: &mut SpecDefinitions) {
    let selector = Selector::parse("h2[id], h3[id], h4[id], h5[id]").expect("valid CSS selector");

    for heading in document.select(&selector) {
        let text: String = heading.text().collect();
        let stripped = strip_section_number(text.trim());
        if stripped != "Internal slots" {
            continue;
        }

        // Determine which class this internal slots section belongs to by
        // looking at the heading's ID suffix or the preceding class heading.
        let heading_id = heading.value().attr("id").unwrap_or_default().to_string();

        let class_name = find_class_for_internal_slots(&heading_id, defs);
        if class_name.is_empty() {
            continue;
        }

        // Find the next <table> after this heading
        let mut node = heading.next_sibling();
        while let Some(n) = node {
            if let Some(el) = ElementRef::wrap(n) {
                if el.value().name() == "table" {
                    let slots = parse_internal_slots_table(&el);
                    if !slots.is_empty() {
                        defs.internal_slots.insert(class_name.clone(), slots);
                    }
                    break;
                }
                // Stop if we hit another heading
                if matches!(el.value().name(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
                    break;
                }
            }
            node = n.next_sibling();
        }
    }
}

/// Determine the class name for an internal slots section from heading ID.
///
/// Internal-slots heading IDs typically follow a pattern like:
/// - "ws-internal-slots" → look up which class has "ws-class"
/// - "rs-internal-slots" → look up which class has "rs-class"
/// - "default-reader-internal-slots" → look up "default-reader-class"
fn find_class_for_internal_slots(heading_id: &str, defs: &SpecDefinitions) -> String {
    let prefix = heading_id.strip_suffix("-internal-slots").unwrap_or("");
    if prefix.is_empty() {
        return String::new();
    }

    // Look for a class section with the matching prefix
    let class_heading_id = format!("{prefix}-class");
    for (class_name, section_id) in &defs.class_sections {
        if *section_id == class_heading_id {
            return class_name.clone();
        }
    }

    String::new()
}

/// Parse an internal slots `<table>` element into a list of `InternalSlot`s.
fn parse_internal_slots_table(table: &ElementRef<'_>) -> Vec<InternalSlot> {
    let mut slots = Vec::new();
    let tr_selector = Selector::parse("tbody tr, tr").expect("valid CSS selector");
    let td_selector = Selector::parse("td").expect("valid CSS selector");
    let dfn_selector = Selector::parse("dfn[id]").expect("valid CSS selector");

    for row in table.select(&tr_selector) {
        let cells: Vec<ElementRef<'_>> = row.select(&td_selector).collect();
        if cells.len() < 2 {
            continue;
        }

        // First cell has the slot name in a <dfn> element
        let slot_cell = &cells[0];
        let desc_cell = &cells[1];

        // Get the dfn element for the fragment ID
        let fragment_id = slot_cell
            .select(&dfn_selector)
            .next()
            .and_then(|d| d.value().attr("id"))
            .unwrap_or_default()
            .to_string();

        // Get the slot name text (e.g., "[[storedError]]")
        let raw_name: String = slot_cell.text().collect();
        let raw_name = raw_name.trim();

        // Strip [[ and ]] brackets
        let name = raw_name
            .strip_prefix("[[")
            .and_then(|s| s.strip_suffix("]]"))
            .unwrap_or(raw_name)
            .to_string();

        if name.is_empty() {
            continue;
        }

        // Get the description text (strip HTML tags)
        let description = normalize_whitespace(&desc_cell.text().collect::<String>());

        slots.push(InternalSlot {
            name,
            description,
            fragment_id,
        });
    }

    slots
}

/// Extract internal state fields described in prose using the "associated" pattern.
///
/// Many specs describe internal state fields with prose like:
/// - "An `AbortController` object has an associated signal (an `AbortSignal` object)."
/// - "An `AbortSignal` object has a dependent (a boolean), which is initially false."
/// - "Shadow roots have an associated mode ("open" or "closed")."
///
/// These are identified by `<dfn>` elements with a `data-dfn-for` attribute inside
/// `<p>` elements whose text contains "has an associated", "has a", "have an associated",
/// or "have a" immediately before the `<dfn>`.
fn extract_associated_fields(document: &Html, defs: &mut SpecDefinitions) {
    let p_selector = Selector::parse("p").expect("valid CSS selector");
    let dfn_selector = Selector::parse("dfn[data-dfn-for][id]").expect("valid CSS selector");

    for p_el in document.select(&p_selector) {
        // Skip paragraphs inside tables — those are handled by extract_internal_slots_tables
        if p_el
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(|a| a.value().name() == "table")
        {
            continue;
        }

        let p_text = normalize_whitespace(&p_el.text().collect::<String>());

        // Check if the paragraph describes an "associated" field
        if !is_associated_prose(&p_text) {
            continue;
        }

        // Find all <dfn data-dfn-for="ClassName" id="..."> elements in this paragraph
        for dfn_el in p_el.select(&dfn_selector) {
            let attrs = dfn_el.value();
            let Some(dfn_for) = attrs.attr("data-dfn-for") else {
                continue;
            };
            let Some(id) = attrs.attr("id") else {
                continue;
            };
            if dfn_for.is_empty() {
                continue;
            }

            let field_name: String = dfn_el.text().collect();
            let field_name = field_name.trim().to_string();
            if field_name.is_empty() || field_name.starts_with("[[") {
                continue;
            }

            // Extract the description: everything after the <dfn> in the paragraph text.
            // Typically in parentheses, e.g., "(an AbortSignal object)".
            let description = extract_description_after_dfn(&p_text, &field_name);

            defs.internal_slots
                .entry(dfn_for.to_string())
                .or_default()
                .push(InternalSlot {
                    name: field_name,
                    description,
                    fragment_id: id.to_string(),
                });
        }
    }
}

/// Check whether paragraph text describes an "associated" internal field.
fn is_associated_prose(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("has an associated")
        || lower.contains("have an associated")
        || lower.contains("have associated")
        || contains_has_a_pattern(&lower)
}

/// Check for "has a FIELD_NAME (" pattern — distinguished from unrelated "has a"
/// usage by requiring a parenthesized type description to follow.
fn contains_has_a_pattern(lower_text: &str) -> bool {
    // Match "has a " or "have a " followed eventually by "(" for a type annotation
    for pattern in &["has a ", "have a "] {
        if let Some(pos) = lower_text.find(pattern) {
            let after = &lower_text[pos + pattern.len()..];
            // Must contain a parenthesized type description reasonably close
            if after.contains('(') {
                return true;
            }
        }
    }
    false
}

/// Extract the type description that follows a field name in prose text.
///
/// Given paragraph text like "An AbortController object has an associated signal
/// (an AbortSignal object). Unless stated otherwise it is null.", extracts
/// "(an AbortSignal object). Unless stated otherwise it is null." as the description.
fn extract_description_after_dfn(p_text: &str, field_name: &str) -> String {
    // Find the field name in the paragraph text
    let Some(pos) = p_text.find(field_name) else {
        return String::new();
    };

    let after = p_text[pos + field_name.len()..].trim();

    // Strip leading punctuation/whitespace (comma, space)
    let after = after.trim_start_matches([' ', ','].as_slice()).trim();

    normalize_whitespace(after)
}

/// Iterate direct child elements of a given tag name.
fn direct_child_elements<'a>(
    parent: &'a ElementRef<'a>,
    tag: &'a str,
) -> impl Iterator<Item = ElementRef<'a>> {
    parent
        .children()
        .filter_map(ElementRef::wrap)
        .filter(move |el| el.value().name() == tag)
}

/// Extract text from an element, preserving semantic HTML markup as inline annotations.
///
/// - `<a>` and `<code>` → backtick-quoted: `` `text` ``
/// - `<em>`, `<i>`, `<var>` → underscore-wrapped: `_text_`
/// - `<strong>`, `<b>` → asterisk-wrapped: `*text*`
/// - Other elements → recurse into children
fn extract_formatted_text(element: &ElementRef) -> String {
    let mut result = String::new();
    for child in element.children() {
        if let Some(child_el) = ElementRef::wrap(child) {
            push_merging_backticks(&mut result, &format_element(&child_el));
        } else if let Node::Text(t) = child.value() {
            push_merging_backticks(&mut result, t);
        }
    }
    result
}

/// Append `piece` to `result`, merging a backtick boundary between them.
///
/// Specs write byte sequences with literal backticks around a `<code>`
/// element (`` `<code>GET</code>` ``); together with the backticks
/// [`format_element`] adds for the `<code>` itself that would double up, so
/// adjacent backticks across a concatenation boundary collapse into one.
fn push_merging_backticks(result: &mut String, piece: &str) {
    let piece = match piece.strip_prefix('`') {
        Some(rest) if result.ends_with('`') => rest,
        _ => piece,
    };
    result.push_str(piece);
}

/// Format a single element, applying the annotation for its own tag around its
/// formatted content.
///
/// Elements with `class="note"` (spec-editorial asides, whatever their tag)
/// become their own "Spec note: "-prefixed line; the newlines survive
/// step-text normalization (see [`normalize_step_text`]).
fn format_element(element: &ElementRef) -> String {
    let inner = extract_formatted_text(element);
    if element.value().classes().any(|c| c == "note") {
        return format!("{NOTE_BREAK}Spec note: {inner}{NOTE_BREAK}");
    }
    match element.value().name() {
        // Spec markup nests <code><a>…</a></code> (and the reverse); wrapping
        // both layers would double the backticks, so content that is already
        // exactly one backtick-quoted span passes through unchanged. Empty
        // elements (e.g. self-link anchors) get no backticks at all, and
        // interior backticks (dfn names embedding a code span) are dropped
        // rather than colliding with the wrapping pair.
        "a" | "code" if inner.trim().is_empty() || is_single_backtick_span(&inner) => inner,
        "a" | "code" => format!("`{}`", inner.replace('`', "")),
        "em" | "i" | "var" => format!("_{inner}_"),
        "strong" | "b" => format!("*{inner}*"),
        _ => inner,
    }
}

/// Whether `text` (modulo surrounding whitespace) is a single backtick-quoted
/// span, e.g. `` `TypeError` `` — i.e. already fully quoted, with no interior
/// backticks.
fn is_single_backtick_span(text: &str) -> bool {
    let t = text.trim();
    t.len() >= 2 && t.starts_with('`') && t.ends_with('`') && !t[1..t.len() - 1].contains('`')
}

/// Sentinel emitted by [`format_element`] around note content to mark line
/// breaks. Source newlines can't serve as the marker — HTML indentation puts
/// them everywhere — so a control character no spec text contains is used and
/// resolved (or collapsed, in single-line contexts) during normalization.
const NOTE_BREAK: char = '\u{1}';

/// Normalize step text while keeping the line structure produced by
/// [`format_element`] for notes: each note-delimited segment is
/// whitespace-collapsed individually, and empty segments are dropped.
fn normalize_step_text(text: &str) -> String {
    text.split(NOTE_BREAK)
        .map(normalize_whitespace)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapse runs of whitespace (including newlines) into single spaces and trim.
///
/// [`NOTE_BREAK`] markers count as whitespace, so single-line contexts
/// (headings, switch conditions) inline note content instead of leaking the
/// sentinel.
pub fn normalize_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut prev_was_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() || ch == NOTE_BREAK {
            if !prev_was_space && !result.is_empty() {
                result.push(' ');
            }
            prev_was_space = true;
        } else {
            prev_was_space = false;
            result.push(ch);
        }
    }
    // Trim trailing space
    if result.ends_with(' ') {
        result.pop();
    }
    result
}

/// Heuristic: does this text look like an algorithm heading?
fn looks_like_algorithm_heading(text: &str) -> bool {
    let lower = text.to_lowercase();
    // "The foo() method steps are:"
    lower.ends_with("steps are:")
        // "... runs these steps:" / "... and then runs these steps:"
        || lower.ends_with("these steps:")
        // "... must act as follows:" (FileAPI: slice method)
        || lower.ends_with("as follows:")
        // "... must run the following steps:" (FileAPI: Blob constructor)
        || lower.ends_with("following steps:")
        // "... must run the steps below:" (FileAPI: abort method)
        || lower.ends_with("steps below:")
        // "... runs the associated steps:" (FileAPI: package data)
        || lower.ends_with("associated steps:")
        // "The foo() method steps are to return..."
        || lower.contains("method steps are")
        // "The foo getter steps are to return..."
        || lower.contains("getter steps are")
        // "The foo setter steps are"
        || lower.contains("setter steps are")
        // "The new Foo() constructor steps are:"
        || lower.contains("constructor steps are")
        // "To serialize a URL:" / "To initialize a URLSearchParams object:"
        || (lower.starts_with("to ") && lower.ends_with(':'))
        // "2.7.3 StructuredSerializeInternal ( value, forStorage [ , memory ] )"
        // Section-numbered abstract operation signatures used by the HTML spec.
        || is_abstract_operation_signature(text)
}

/// Classify a heading into one or more `AlgorithmKind` values.
///
/// Some headings define multiple algorithms, e.g.:
/// "The href getter steps and the toJSON() method steps are to return..."
pub fn classify_heading(text: &str) -> Vec<AlgorithmKind> {
    let lower = text.to_lowercase();
    let mut kinds = Vec::new();

    // Check for combined headings: "The X getter steps and the Y() method steps"
    // Split on " and the " to handle combined definitions
    let parts: Vec<&str> = if lower.contains(" and the ") {
        text.split(" and the ")
            .enumerate()
            .map(|(i, part)| {
                if i == 0 {
                    part
                } else {
                    // Reconstruct "The " prefix for subsequent parts
                    part
                }
            })
            .collect()
    } else {
        vec![text]
    };

    for (i, part) in parts.iter().enumerate() {
        let part_lower = part.to_lowercase();
        // Prefix "the " for parts after "and the" splitting
        let (effective, effective_original) = if i > 0 {
            (format!("the {part_lower}"), format!("the {part}"))
        } else {
            (part_lower.clone(), part.to_string())
        };

        if let Some(kind) = classify_single_heading(&effective, &effective_original) {
            kinds.push(kind);
        }
    }

    // If no specific pattern matched, try standalone
    if kinds.is_empty()
        && let Some(kind) = classify_as_standalone(text)
    {
        kinds.push(kind);
    }

    kinds
}

/// Classify a single heading fragment into an `AlgorithmKind`.
fn classify_single_heading(lower: &str, original: &str) -> Option<AlgorithmKind> {
    // Constructor: "the new ClassName(args) constructor steps are"
    if lower.contains("constructor steps") {
        // Extract class name: "the new URL(url, base) constructor steps are:"
        let after_new = lower.find("new ").map(|p| &original[p + 4..]);
        if let Some(rest) = after_new {
            let class_end = rest.find('(').unwrap_or(rest.len());
            let class = rest[..class_end].trim().to_string();
            if !class.is_empty() {
                return Some(AlgorithmKind::Constructor { class });
            }
        }
    }

    // Getter: "the attrName getter steps"
    if lower.contains("getter steps") {
        // Extract attribute name
        let before_getter = lower.find("getter steps")?;
        let prefix = original[..before_getter].trim();
        let name = prefix
            .strip_prefix("the ")
            .or_else(|| prefix.strip_prefix("The "))
            .unwrap_or(prefix)
            .trim();
        if !name.is_empty() {
            return Some(AlgorithmKind::Getter {
                name: name.to_string(),
            });
        }
    }

    // Setter: "the attrName setter steps"
    if lower.contains("setter steps") {
        let before_setter = lower.find("setter steps")?;
        let prefix = original[..before_setter].trim();
        let name = prefix
            .strip_prefix("the ")
            .or_else(|| prefix.strip_prefix("The "))
            .unwrap_or(prefix)
            .trim();
        if !name.is_empty() {
            return Some(AlgorithmKind::Setter {
                name: name.to_string(),
            });
        }
    }

    // Static method: "the static methodName(args) method steps are"
    if lower.contains("method steps") {
        let is_static = lower.contains("static ");
        // Extract method name: find text before "(", after "static " or "the "
        let before_method = lower.find("method steps")?;
        let prefix = original[..before_method].trim();

        // Remove "the " / "The " prefix
        let name_part = prefix
            .strip_prefix("the ")
            .or_else(|| prefix.strip_prefix("The "))
            .unwrap_or(prefix)
            .trim();
        // Remove "static " prefix
        let name_part = name_part
            .strip_prefix("static ")
            .unwrap_or(name_part)
            .trim();
        // Remove "(args)" suffix
        let name = name_part
            .find('(')
            .map(|p| &name_part[..p])
            .unwrap_or(name_part)
            .trim();

        if !name.is_empty() {
            return Some(AlgorithmKind::Method {
                name: name.to_string(),
                is_static,
            });
        }
    }

    // FileAPI-style method: "The text() method, when invoked, must run..." or
    // "The revokeObjectURL(url) static method must run..." or
    // "When the abort() method is called..."
    if lower.contains(" method") {
        let is_static = lower.contains("static method");
        let kw = if is_static {
            " static method"
        } else {
            " method"
        };
        // Try each occurrence of the keyword — the first may lack parens if
        // the heading mentions the member name in prose before the formal
        // "(args) method" phrasing.
        let mut offset = 0;
        while let Some(rel_pos) = lower[offset..].find(kw) {
            let kw_pos = offset + rel_pos;
            if let Some(name) = extract_name_before_parens(&original[..kw_pos]) {
                return Some(AlgorithmKind::Method { name, is_static });
            }
            offset = kw_pos + kw.len();
        }
    }

    // FileAPI-style constructor: "The Blob() constructor can be invoked..." or
    // "The File() constructor is invoked..."
    // Headings may mention "constructor" multiple times (once in prose, once
    // in the formal "Name() constructor" phrasing), so try each occurrence.
    if lower.contains(" constructor") {
        let mut offset = 0;
        while let Some(rel_pos) = lower[offset..].find(" constructor") {
            let kw_pos = offset + rel_pos;
            if let Some(class) = extract_name_before_parens(&original[..kw_pos]) {
                return Some(AlgorithmKind::Constructor { class });
            }
            offset = kw_pos + " constructor".len();
        }
    }

    None
}

/// Extract a member name from text ending with a parenthesized argument list.
///
/// Given text like "The revokeObjectURL(url)" or "When the abort()",
/// returns "revokeObjectURL" or "abort" respectively.
fn extract_name_before_parens(text: &str) -> Option<String> {
    let paren_close = text.rfind(')')?;
    let paren_open = text[..paren_close].rfind('(')?;
    let name_part = text[..paren_open].trim();
    // Get the last word (the member name)
    let name = name_part
        .rsplit_once(|c: char| c.is_whitespace())
        .map(|(_, n)| n)
        .unwrap_or(name_part);
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// Classify a heading as a standalone algorithm.
fn classify_as_standalone(text: &str) -> Option<AlgorithmKind> {
    let lower = text.to_lowercase();

    // Abstract operation signatures: "2.7.3 StructuredSerializeInternal ( value, ... )"
    if let Some(name) = extract_abstract_operation_name(text) {
        return Some(AlgorithmKind::Standalone { name });
    }

    // "To initialize a URL object..." / "To update a URLSearchParams object..."
    if lower.starts_with("to ") {
        let rest = &text[3..];
        // Take the verb phrase up to the first comma, period, or colon —
        // but not one inside a parenthesized parameter list like "(name, value)",
        // which would truncate the name mid-list and collide algorithms that
        // share a verb (e.g. the fetch spec's two "append" algorithms).
        let end = find_outside_parens(rest, &[',', ':', '.']).unwrap_or(rest.len());
        let mut name = strip_paren_groups(&rest[..end]);
        strip_param_description(&mut name);
        if !name.is_empty() {
            return Some(AlgorithmKind::Standalone { name });
        }
    }

    // "The API URL parser takes..." / "The basic URL parser takes..."
    // "The URL parser takes..." / "The host parser takes..."
    // "The domain to ASCII algorithm, given ..., runs these steps:"
    // "The slice blob algorithm given a Blob ..., must act as follows:"
    // "A FileReader fr has an associated read operation algorithm, which ... runs the following steps:"
    if (lower.contains("takes ")
        || lower.contains("these steps")
        || lower.contains("following steps")
        || lower.contains("associated steps")
        || lower.contains(" algorithm"))
        && !lower.contains("method steps")
        && !lower.contains("getter steps")
        && !lower.contains("setter steps")
        && !lower.contains("constructor steps")
    {
        // Extract algorithm name from various phrasings
        let prefix = if let Some(pos) = lower.find(" takes ") {
            &text[..pos]
        } else if let Some(pos) = lower.find(", and then runs these steps") {
            &text[..pos]
        } else if let Some(pos) = lower.find(" runs these steps") {
            &text[..pos]
        } else if let Some(pos) = lower.find(" runs the following steps") {
            &text[..pos]
        } else if let Some(pos) = lower.find(" runs the associated steps") {
            &text[..pos]
        } else if let Some(pos) = lower.find(" must act as follows") {
            &text[..pos]
        } else if let Some(pos) = lower.find(" is used to refer to") {
            &text[..pos]
        } else {
            return None;
        };
        let mut name = prefix
            .strip_prefix("The ")
            .or_else(|| prefix.strip_prefix("the "))
            .unwrap_or(prefix)
            .trim()
            .to_string();
        // Strip "has an associated" prefix for "A Blob has an associated X algorithm" pattern
        if let Some(pos) = name.to_lowercase().find("has an associated ") {
            name = name[pos + "has an associated ".len()..].to_string();
        }
        strip_param_description(&mut name);
        // Strip trailing ", which" left when the clause was split at the verb phrase
        if let Some(stripped) = name.strip_suffix(", which") {
            name = stripped.to_string();
        }
        // Strip trailing " algorithm" suffix (e.g. "slice blob algorithm" → "slice blob")
        if let Some(stripped) = name.strip_suffix(" algorithm") {
            name = stripped.to_string();
        }
        if !name.is_empty() {
            return Some(AlgorithmKind::Standalone {
                name: name.to_string(),
            });
        }
    }

    None
}

/// Find the first occurrence of any of `needles` in `text` that is not
/// inside parentheses.
fn find_outside_parens(text: &str, needles: &[char]) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 && needles.contains(&c) => return Some(i),
            _ => {}
        }
    }
    None
}

/// Remove parenthesized groups (parameter lists like "(name, value)") from an
/// algorithm name and collapse the whitespace they leave behind.
fn strip_paren_groups(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    normalize_whitespace(&out)
}

/// Strip trailing parameter descriptions from an algorithm name.
///
/// Removes suffixes like ", given a string domain and a boolean beStrict",
/// " given an IPv6 address address", or ", which given blob, a type" from
/// algorithm names. When multiple patterns match, truncates at the earliest
/// position to avoid retaining stray parameter text.
fn strip_param_description(name: &mut String) {
    let lower = name.to_lowercase();
    let patterns = [", which given ", ", which ", ", given ", " given "];
    let earliest = patterns.iter().filter_map(|p| lower.find(p)).min();
    if let Some(pos) = earliest {
        name.truncate(pos);
    }
}

/// Check if a heading looks like an abstract operation signature.
///
/// Matches patterns like:
/// - `"2.7.3 StructuredSerializeInternal ( value, forStorage [ , memory ] )"`
/// - `"StructuredClone ( value, options )"`
///
/// These are section headings with an UpperCamelCase identifier followed by
/// a parenthesized parameter list.
fn is_abstract_operation_signature(text: &str) -> bool {
    extract_abstract_operation_name(text).is_some()
}

/// Extract the algorithm name from an abstract operation signature heading.
///
/// Returns the CamelCase operation name if the heading matches, e.g.
/// `"2.7.3 StructuredSerializeInternal ( value, ... )"` → `"StructuredSerializeInternal"`.
fn extract_abstract_operation_name(text: &str) -> Option<String> {
    // Strip leading section numbers like "2.7.3 "
    let stripped = text
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ' ')
        .trim();

    // Must have parenthesized params
    let paren_pos = stripped.find('(')?;
    let name = stripped[..paren_pos].trim();

    // Must be a single CamelCase identifier (starts uppercase, contains lowercase)
    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    // Reject names with spaces (to avoid matching prose headings)
    if name.contains(' ') {
        return None;
    }
    // Must contain at least one lowercase letter (not all-caps like "URL")
    if !name.chars().any(|c| c.is_ascii_lowercase()) {
        return None;
    }

    Some(name.to_string())
}

/// Check if a heading is a one-liner algorithm description (no following <ol>).
///
/// One-liners use "steps are to" instead of "steps are:" —
/// e.g., "The origin getter steps are to return the serialization of this's URL's origin."
fn is_one_liner_algorithm(text: &str) -> bool {
    let lower = text.to_lowercase();
    // "steps are to " indicates an inline description, not a numbered list
    lower.contains("steps are to ")
}

/// Extract the description text from a one-liner algorithm heading.
///
/// E.g., "The href getter steps are to return the serialization of this's URL."
/// → "Return the serialization of this's URL."
fn extract_one_liner_description(text: &str) -> String {
    let lower = text.to_lowercase();
    if let Some(pos) = lower.find("steps are to ") {
        let desc_start = pos + "steps are to ".len();
        let description = text[desc_start..].trim();
        // Capitalize the first letter
        let mut chars = description.chars();
        match chars.next() {
            Some(c) => {
                let mut result = c.to_uppercase().to_string();
                result.push_str(chars.as_str());
                // Remove trailing period if present (we add our own formatting)
                result
            }
            None => String::new(),
        }
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_idl_from_html() {
        let html = r#"
            <html><body>
            <pre class="idl">
interface URL {
  constructor(USVString url, optional USVString base);
  stringifier attribute USVString href;
};
            </pre>
            <p>Some other text</p>
            <pre class="example">not idl</pre>
            <pre class="idl">
dictionary URLSearchParamsInit {
  USVString query;
};
            </pre>
            </body></html>
        "#;

        let blocks = extract_idl_blocks(html);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].text.contains("interface URL"));
        assert!(blocks[1].text.contains("dictionary URLSearchParamsInit"));
    }

    #[test]
    fn extract_idl_from_html_spec_style() {
        // The HTML spec wraps IDL in <pre><code class='idl'> rather than
        // <pre class="idl"> — verify both forms are picked up.
        let html = r#"
            <html><body>
            <pre><code class='idl'>
[Exposed=Worker]
interface WorkerLocation {
  readonly attribute USVString href;
};
            </code></pre>
            </body></html>
        "#;

        let blocks = extract_idl_blocks(html);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].text.contains("interface WorkerLocation"));
        assert!(blocks[0].text.contains("Exposed=Worker"));
    }

    #[test]
    fn extract_algorithm_steps_from_html() {
        let html = r#"
            <html><body>
            <p>The <code>append(<var>name</var>, <var>value</var>)</code> method steps are:</p>
            <ol>
                <li>Validate name and value.</li>
                <li>Append (name, value) to this's entry list.</li>
                <li>Update this's URL.</li>
            </ol>
            <p>Some unrelated text</p>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert!(algos[0].heading.contains("append"));
        assert_eq!(algos[0].steps.len(), 3);
        assert!(algos[0].steps[0].text.contains("Validate"));
        assert!(
            matches!(&algos[0].kind, AlgorithmKind::Method { name, is_static: false } if name == "append")
        );
    }

    #[test]
    fn algorithm_heading_matching() {
        assert!(looks_like_algorithm_heading(
            "The append(name, value) method steps are:"
        ));
        assert!(looks_like_algorithm_heading("To serialize a URL:"));
        assert!(looks_like_algorithm_heading(
            "The href getter steps are to return the serialization of this's URL."
        ));
        assert!(looks_like_algorithm_heading("The href setter steps are:"));
        assert!(looks_like_algorithm_heading(
            "The new URL(url, base) constructor steps are:"
        ));
        assert!(!looks_like_algorithm_heading("Some random text"));
    }

    #[test]
    fn classify_method_heading() {
        let kinds = classify_heading("The append(name, value) method steps are:");
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Method { name, is_static: false } if name == "append")
        );
    }

    #[test]
    fn classify_static_method_heading() {
        let kinds = classify_heading("The static parse(url, base) method steps are:");
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Method { name, is_static: true } if name == "parse")
        );
    }

    #[test]
    fn classify_constructor_heading() {
        let kinds = classify_heading("The new URL(url, base) constructor steps are:");
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Constructor { class } if class == "URL"));
    }

    #[test]
    fn classify_getter_heading() {
        let kinds = classify_heading(
            "The origin getter steps are to return the serialization of this's URL's origin.",
        );
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Getter { name } if name == "origin"));
    }

    #[test]
    fn classify_setter_heading() {
        let kinds = classify_heading("The href setter steps are:");
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Setter { name } if name == "href"));
    }

    #[test]
    fn classify_combined_heading() {
        let kinds = classify_heading(
            "The href getter steps and the toJSON() method steps are to return the serialization of this's URL.",
        );
        assert_eq!(kinds.len(), 2);
        assert!(matches!(&kinds[0], AlgorithmKind::Getter { name } if name == "href"));
        assert!(
            matches!(&kinds[1], AlgorithmKind::Method { name, is_static: false } if name == "toJSON")
        );
    }

    #[test]
    fn classify_standalone_to() {
        let kinds = classify_heading("To initialize a URL object url with a URL urlRecord:");
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Standalone { name } if name == "initialize a URL object url with a URL urlRecord")
        );
    }

    #[test]
    fn classify_standalone_ignores_commas_inside_parens() {
        // The comma inside "(name, value)" must not truncate the name, and the
        // parenthesized parameter list is dropped. Otherwise the header-list
        // and Headers-object "append" algorithms of the fetch spec collapse to
        // the identical name "append a header (name".
        let kinds = classify_heading(
            "To append a header (name, value) to a Headers object headers, run these steps:",
        );
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Standalone { name } if name == "append a header to a Headers object headers"),
            "got: {:?}",
            kinds[0]
        );

        let kinds = classify_heading("To append a header (name, value) to a header list list:");
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Standalone { name } if name == "append a header to a header list list"),
            "got: {:?}",
            kinds[0]
        );
    }

    #[test]
    fn classify_standalone_takes() {
        let kinds = classify_heading(
            "The API URL parser takes a scalar value string url and an optional null-or-scalar value string base (default null), and then runs these steps:",
        );
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Standalone { name } if name == "API URL parser")
        );
    }

    #[test]
    fn normalize_whitespace_collapses() {
        assert_eq!(normalize_whitespace("hello\n  world\n"), "hello world");
        assert_eq!(normalize_whitespace("  multi   spaces  "), "multi spaces");
        assert_eq!(normalize_whitespace("clean text"), "clean text");
    }

    #[test]
    fn one_liner_algorithm_extraction() {
        let html = r#"
            <html><body>
            <p>The origin getter steps are to return the serialization of this's URL's origin.</p>
            <p>Some unrelated text</p>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert!(matches!(&algos[0].kind, AlgorithmKind::Getter { name } if name == "origin"));
        assert_eq!(algos[0].steps.len(), 1);
        assert!(algos[0].steps[0].text.starts_with("Return"));
    }

    #[test]
    fn one_liner_algorithm_keeps_inline_markup() {
        let html = r##"
            <html><body>
            <p>The <dfn>method</dfn> getter steps are to return <a href="#this">this</a>’s
            <a href="#request">request</a>’s <var>method</var>.</p>
            </body></html>
        "##;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert_eq!(
            algos[0].steps[0].text,
            "Return `this`’s `request`’s _method_."
        );
    }

    #[test]
    fn prose_only_algorithm_div_keeps_inline_markup() {
        let html = r##"
            <html><body>
            <div class="algorithm" data-algorithm="contains" data-algorithm-for="header list">
                <p>A <a href="#header-list">header list</a> <var>list</var>
                <dfn id="header-list-contains">contains</dfn> a
                <a href="#header-name">header name</a> <var>name</var> if <var>list</var>
                <a href="#list-contain">contains</a> a matching
                <a href="#header">header</a>.</p>
            </div>
            </body></html>
        "##;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        let expected = "A `header list` _list_ contains a `header name` _name_ \
                        if _list_ `contains` a matching `header`.";
        assert_eq!(algos[0].heading, expected);
        assert_eq!(algos[0].steps.len(), 1);
        assert_eq!(algos[0].steps[0].text, expected);
    }

    #[test]
    fn extract_getter_setter_algorithms() {
        let html = r#"
            <html><body>
            <p>The host getter steps are:</p>
            <ol>
                <li>Let url be this's URL.</li>
                <li>If url's host is null, then return the empty string.</li>
            </ol>
            <p>The host setter steps are:</p>
            <ol>
                <li>If this's URL's cannot-be-a-base-URL is true, then return.</li>
                <li>Basic URL parse the given value.</li>
            </ol>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 2);
        assert!(matches!(&algos[0].kind, AlgorithmKind::Getter { name } if name == "host"));
        assert_eq!(algos[0].steps.len(), 2);
        assert!(matches!(&algos[1].kind, AlgorithmKind::Setter { name } if name == "host"));
        assert_eq!(algos[1].steps.len(), 2);
    }

    #[test]
    fn extract_class_sections_from_headings() {
        let html = r#"
            <html><body>
            <h3 id="ws-class">4.2. The WritableStream class</h3>
            <h3 id="rs-class">4.1. The ReadableStream class</h3>
            <h3 id="response-class">5.5. Response class</h3>
            </body></html>
        "#;

        let defs = extract_spec_definitions(html);
        assert_eq!(
            defs.class_sections.get("WritableStream"),
            Some(&"ws-class".to_string())
        );
        assert_eq!(
            defs.class_sections.get("ReadableStream"),
            Some(&"rs-class".to_string())
        );
        assert_eq!(
            defs.class_sections.get("Response"),
            Some(&"response-class".to_string())
        );
    }

    #[test]
    fn extract_member_fragments_from_dfns() {
        let html = r#"
            <html><body>
            <dfn data-dfn-for="WritableStream" id="ws-constructor">new WritableStream()</dfn>
            <dfn data-dfn-for="WritableStream" id="ws-locked">locked</dfn>
            <dfn data-dfn-for="WritableStream" id="ws-abort">abort(reason)</dfn>
            <dfn data-dfn-for="Response" id="dom-response">Response(body, init)</dfn>
            </body></html>
        "#;

        let defs = extract_spec_definitions(html);
        assert_eq!(
            defs.member_fragments
                .get(&("WritableStream".to_string(), "constructor".to_string())),
            Some(&"ws-constructor".to_string())
        );
        assert_eq!(
            defs.member_fragments
                .get(&("WritableStream".to_string(), "locked".to_string())),
            Some(&"ws-locked".to_string())
        );
        assert_eq!(
            defs.member_fragments
                .get(&("WritableStream".to_string(), "abort".to_string())),
            Some(&"ws-abort".to_string())
        );
    }

    #[test]
    fn extract_dictionary_fragments() {
        let html = r#"
            <html><body>
            <dfn id="dictdef-queuingstrategy">QueuingStrategy</dfn>
            <dfn id="dictdef-responseinit">ResponseInit</dfn>
            </body></html>
        "#;

        let defs = extract_spec_definitions(html);
        assert_eq!(
            defs.dictionary_fragments.get("QueuingStrategy"),
            Some(&"dictdef-queuingstrategy".to_string())
        );
        assert_eq!(
            defs.dictionary_fragments.get("ResponseInit"),
            Some(&"dictdef-responseinit".to_string())
        );
    }

    #[test]
    fn extract_internal_slots_table() {
        let html = r#"
            <html><body>
            <h3 id="ws-class">The WritableStream class</h3>
            <h4 id="ws-internal-slots">Internal slots</h4>
            <table>
              <thead><tr><th>Internal slot</th><th>Description (non-normative)</th></tr></thead>
              <tbody>
                <tr><td><dfn id="writablestream-backpressure">[[backpressure]]</dfn></td><td>A boolean indicating the backpressure signal</td></tr>
                <tr><td><dfn id="writablestream-storedError">[[storedError]]</dfn></td><td>A value indicating how the stream failed</td></tr>
              </tbody>
            </table>
            </body></html>
        "#;

        let defs = extract_spec_definitions(html);
        let slots = defs.internal_slots.get("WritableStream").unwrap();
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].name, "backpressure");
        assert!(slots[0].description.contains("boolean"));
        assert_eq!(slots[0].fragment_id, "writablestream-backpressure");
        assert_eq!(slots[1].name, "storedError");
        assert_eq!(slots[1].fragment_id, "writablestream-storedError");
    }

    #[test]
    fn extract_algorithms_from_div_algorithm() {
        // Streams-style <div class="algorithm"> with heading text and <ol> as children.
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="cancel(reason)" data-algorithm-for="ReadableStream">
                The <dfn data-dfn-for="ReadableStream"><code>cancel(<var>reason</var>)</code></dfn> method steps are:
                <ol>
                    <li>If ! IsReadableStreamLocked(this) is true, return a promise rejected with a TypeError.</li>
                    <li>Return ! ReadableStreamCancel(this, reason).</li>
                </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert!(matches!(
            &algos[0].kind,
            AlgorithmKind::Method { name, is_static: false } if name == "cancel"
        ));
        assert_eq!(algos[0].steps.len(), 2);
        assert!(algos[0].steps[0].text.contains("IsReadableStreamLocked"));
    }

    #[test]
    fn extract_constructor_from_div_algorithm() {
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="ReadableStream(underlyingSource, strategy)" data-algorithm-for="ReadableStream">
                The <dfn><code>new ReadableStream(underlyingSource, strategy)</code></dfn> constructor steps are:
                <ol>
                    <li>Perform ! InitializeReadableStream(this).</li>
                    <li>Set up the controller.</li>
                </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert!(matches!(
            &algos[0].kind,
            AlgorithmKind::Constructor { class } if class == "ReadableStream"
        ));
        assert_eq!(algos[0].steps.len(), 2);
    }

    #[test]
    fn extract_getter_from_div_algorithm() {
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="locked" data-algorithm-for="ReadableStream">
                The <dfn><code>locked</code></dfn> getter steps are:
                <ol>
                    <li>Return ! IsReadableStreamLocked(this).</li>
                </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert!(matches!(
            &algos[0].kind,
            AlgorithmKind::Getter { name } if name == "locked"
        ));
        assert_eq!(algos[0].steps.len(), 1);
    }

    #[test]
    fn extract_static_method_from_div_algorithm() {
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="from(asyncIterable)" data-algorithm-for="ReadableStream">
                The static <dfn><code>from(asyncIterable)</code></dfn> method steps are:
                <ol>
                    <li>Return ? ReadableStreamFromIterable(asyncIterable).</li>
                </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert!(matches!(
            &algos[0].kind,
            AlgorithmKind::Method { name, is_static: true } if name == "from"
        ));
        assert_eq!(algos[0].steps.len(), 1);
    }

    #[test]
    fn extract_one_liner_from_div_algorithm() {
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="readable" data-algorithm-for="GenericTransformStream">
                The <dfn><code>readable</code></dfn> getter steps are to return this's transform.[[readable]].
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert!(matches!(&algos[0].kind, AlgorithmKind::Getter { name } if name == "readable"));
        assert_eq!(algos[0].steps.len(), 1);
        assert!(algos[0].steps[0].text.contains("Return"));
    }

    #[test]
    fn extract_multiple_div_algorithms() {
        // Multiple algorithm divs for different interfaces.
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="locked" data-algorithm-for="ReadableStream">
                The <dfn><code>locked</code></dfn> getter steps are:
                <ol>
                    <li>Return ! IsReadableStreamLocked(this).</li>
                </ol>
            </div>
            <div class="algorithm" data-algorithm="locked" data-algorithm-for="WritableStream">
                The <dfn><code>locked</code></dfn> getter steps are:
                <ol>
                    <li>Return ! IsWritableStreamLocked(this).</li>
                </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 2);
        assert!(matches!(&algos[0].kind, AlgorithmKind::Getter { name } if name == "locked"));
        assert!(matches!(&algos[1].kind, AlgorithmKind::Getter { name } if name == "locked"));
        assert!(algos[0].steps[0].text.contains("ReadableStream"));
        assert!(algos[1].steps[0].text.contains("WritableStream"));
    }

    #[test]
    fn div_algorithm_and_p_algorithm_both_extracted() {
        // Both <p>-based and <div>-based algorithms should be extracted.
        let html = r#"
            <html><body>
            <p>The append(<var>name</var>, <var>value</var>) method steps are:</p>
            <ol>
                <li>Validate name.</li>
                <li>Append (name, value).</li>
            </ol>
            <div class="algorithm" data-algorithm="cancel(reason)">
                The cancel(reason) method steps are:
                <ol>
                    <li>Cancel the stream.</li>
                </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 2);
        assert!(matches!(
            &algos[0].kind,
            AlgorithmKind::Method { name, .. } if name == "append"
        ));
        assert!(matches!(
            &algos[1].kind,
            AlgorithmKind::Method { name, .. } if name == "cancel"
        ));
    }

    #[test]
    fn extract_associated_field_has_an_associated() {
        let html = r#"
            <html><body>
            <p>An <code class="idl"><a>AbortController</a></code> object has an associated
            <dfn data-dfn-for="AbortController" id="abortcontroller-signal">signal</dfn>
            (an <code>AbortSignal</code> object), which is initially a new <code>AbortSignal</code>
            object.</p>
            </body></html>
        "#;

        let defs = extract_spec_definitions(html);
        let slots = defs.internal_slots.get("AbortController").unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].name, "signal");
        assert_eq!(slots[0].fragment_id, "abortcontroller-signal");
        assert!(slots[0].description.contains("AbortSignal"));
    }

    #[test]
    fn extract_associated_field_has_a_boolean() {
        let html = r#"
            <html><body>
            <p>An <code class="idl"><a>AbortSignal</a></code> object has a
            <dfn data-dfn-for="AbortSignal" id="abortsignal-dependent">dependent</dfn>
            (a boolean), which is initially false.</p>
            </body></html>
        "#;

        let defs = extract_spec_definitions(html);
        let slots = defs.internal_slots.get("AbortSignal").unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].name, "dependent");
        assert_eq!(slots[0].fragment_id, "abortsignal-dependent");
        assert!(slots[0].description.contains("boolean"));
    }

    #[test]
    fn extract_associated_field_have_an_associated() {
        // Plural form: "Shadow roots have an associated mode"
        let html = r#"
            <html><body>
            <p><a>Shadow roots</a> have an associated
            <dfn data-dfn-for="ShadowRoot" id="shadowroot-mode">mode</dfn>
            ("<code>open</code>" or "<code>closed</code>").</p>
            </body></html>
        "#;

        let defs = extract_spec_definitions(html);
        let slots = defs.internal_slots.get("ShadowRoot").unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].name, "mode");
        assert_eq!(slots[0].fragment_id, "shadowroot-mode");
    }

    #[test]
    fn extract_multiple_associated_fields_same_interface() {
        let html = r#"
            <html><body>
            <p>An <code class="idl"><a>AbortSignal</a></code> object has an associated
            <dfn data-dfn-for="AbortSignal" id="abortsignal-abort-reason">abort reason</dfn>,
            which is a JavaScript value. It is undefined unless specified otherwise.</p>
            <p>An <code class="idl"><a>AbortSignal</a></code> object has a
            <dfn data-dfn-for="AbortSignal" id="abortsignal-dependent">dependent</dfn>
            (a boolean), which is initially false.</p>
            </body></html>
        "#;

        let defs = extract_spec_definitions(html);
        let slots = defs.internal_slots.get("AbortSignal").unwrap();
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].name, "abort reason");
        assert_eq!(slots[0].fragment_id, "abortsignal-abort-reason");
        assert_eq!(slots[1].name, "dependent");
        assert_eq!(slots[1].fragment_id, "abortsignal-dependent");
    }

    #[test]
    fn associated_fields_not_extracted_from_tables() {
        // Fields inside tables should not be duplicated by the prose extraction
        let html = r#"
            <html><body>
            <h3 id="ws-class">The WritableStream class</h3>
            <h4 id="ws-internal-slots">Internal slots</h4>
            <table>
              <tbody>
                <tr><td><dfn id="ws-backpressure">[[backpressure]]</dfn></td><td>A boolean</td></tr>
              </tbody>
            </table>
            <p>A WritableStream has an associated
            <dfn data-dfn-for="WritableStream" id="ws-extra">extra field</dfn>
            (a string).</p>
            </body></html>
        "#;

        let defs = extract_spec_definitions(html);
        let slots = defs.internal_slots.get("WritableStream").unwrap();
        // Should have the table slot plus the prose slot
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].name, "backpressure");
        assert_eq!(slots[1].name, "extra field");
    }

    #[test]
    fn associated_field_skips_internal_slot_brackets() {
        // [[slotName]] style dfns in prose should be skipped (handled by table extraction)
        let html = r#"
            <html><body>
            <p>Each stream has an associated
            <dfn data-dfn-for="Stream" id="stream-slot">[[state]]</dfn>
            (a string).</p>
            </body></html>
        "#;

        let defs = extract_spec_definitions(html);
        // Should not extract [[state]] — those have their own table-based extraction
        assert!(!defs.internal_slots.contains_key("Stream"));
    }

    #[test]
    fn no_associated_fields_from_unrelated_prose() {
        // Paragraphs without "has an associated" or similar patterns should not produce fields
        let html = r#"
            <html><body>
            <p>The <code>AbortController</code> interface defines a
            <dfn data-dfn-for="AbortController" id="dom-abortcontroller-signal">signal</dfn>
            attribute.</p>
            </body></html>
        "#;

        let defs = extract_spec_definitions(html);
        assert!(!defs.internal_slots.contains_key("AbortController"));
    }

    #[test]
    fn abstract_operation_signature_detected() {
        assert!(is_abstract_operation_signature(
            "2.7.3 StructuredSerializeInternal ( value, forStorage [ , memory ] )"
        ));
        assert!(is_abstract_operation_signature(
            "StructuredSerialize ( value )"
        ));
        // Must be CamelCase, not all-caps
        assert!(!is_abstract_operation_signature("URL ( input )"));
        // Must not have spaces in the name
        assert!(!is_abstract_operation_signature(
            "The basic URL parser ( input )"
        ));
        // No parenthesized params
        assert!(!is_abstract_operation_signature("Serializable objects"));
    }

    #[test]
    fn extract_abstract_operation_name_from_heading() {
        assert_eq!(
            extract_abstract_operation_name(
                "2.7.3 StructuredSerializeInternal ( value, forStorage [ , memory ] )"
            ),
            Some("StructuredSerializeInternal".to_string())
        );
        assert_eq!(
            extract_abstract_operation_name("StructuredSerialize ( value )"),
            Some("StructuredSerialize".to_string())
        );
        assert_eq!(
            extract_abstract_operation_name(
                "2.7.6 StructuredDeserialize ( serialized, targetRealm [ , memory ] )"
            ),
            Some("StructuredDeserialize".to_string())
        );
    }

    #[test]
    fn classify_abstract_operation_as_standalone() {
        let kinds = classify_heading(
            "2.7.3 StructuredSerializeInternal ( value, forStorage [ , memory ] )",
        );
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Standalone { name } if name == "StructuredSerializeInternal")
        );
    }

    #[test]
    fn abstract_operation_heading_recognized() {
        assert!(looks_like_algorithm_heading(
            "2.7.3 StructuredSerializeInternal ( value, forStorage [ , memory ] )"
        ));
        assert!(looks_like_algorithm_heading(
            "StructuredSerialize ( value )"
        ));
    }

    #[test]
    fn data_algorithm_div_extracted() {
        let html = r#"
            <html><body>
            <div data-algorithm="">
            <p>The <code>structuredClone(<var>value</var>,
            <var>options</var>)</code> method steps are:</p>
            <ol>
                <li>Let serialized be the result of calling StructuredSerialize.</li>
                <li>Return the result of calling StructuredDeserialize.</li>
            </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert!(
            !algos.is_empty(),
            "should find algorithm in data-algorithm div"
        );
        let algo = algos.iter().find(
            |a| matches!(&a.kind, AlgorithmKind::Method { name, .. } if name == "structuredClone"),
        );
        assert!(
            algo.is_some(),
            "should classify structuredClone as a method"
        );
        assert_eq!(algo.unwrap().steps.len(), 2);
    }

    #[test]
    fn data_algorithm_div_with_abstract_operation() {
        let html = r#"
            <html><body>
            <div data-algorithm="">
            <h4>2.7.4 StructuredSerialize ( value )</h4>
            <ol>
                <li>Return ? StructuredSerializeInternal(value, false).</li>
            </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert!(
            !algos.is_empty(),
            "should find algorithm in data-algorithm div"
        );
        let algo = algos.iter().find(|a| {
            matches!(&a.kind, AlgorithmKind::Standalone { name } if name == "StructuredSerialize")
        });
        assert!(
            algo.is_some(),
            "should classify StructuredSerialize as standalone"
        );
        assert_eq!(algo.unwrap().steps.len(), 1);
    }

    #[test]
    fn dfn_data_dfn_for_provides_interface() {
        let html = r#"
            <html><body>
            <div data-algorithm="">
            <p>The <dfn data-dfn-for="WindowOrWorkerGlobalScope">
            <code>structuredClone(<var>value</var>)</code></dfn>
            method steps are:</p>
            <ol>
                <li>Let serialized be the result.</li>
            </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        let algo = algos.iter().find(
            |a| matches!(&a.kind, AlgorithmKind::Method { name, .. } if name == "structuredClone"),
        );
        assert!(algo.is_some(), "should find structuredClone method");
        assert_eq!(
            algo.unwrap().interface,
            "WindowOrWorkerGlobalScope",
            "interface should come from dfn data-dfn-for"
        );
    }

    // FileAPI-style heading pattern tests

    #[test]
    fn heading_as_follows_recognized() {
        assert!(looks_like_algorithm_heading(
            "The slice() method returns a new Blob object. It must act as follows:"
        ));
    }

    #[test]
    fn heading_following_steps_recognized() {
        assert!(looks_like_algorithm_heading(
            "When the Blob() constructor is invoked, user agents must run the following steps:"
        ));
    }

    #[test]
    fn heading_steps_below_recognized() {
        assert!(looks_like_algorithm_heading(
            "When the abort() method is called, the user agent must run the steps below:"
        ));
    }

    #[test]
    fn classify_fileapi_method_phrase() {
        let kinds = classify_heading("The text() method, when invoked, must run these steps:");
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Method { name, is_static: false } if name == "text")
        );
    }

    #[test]
    fn classify_fileapi_method_with_args() {
        let kinds = classify_heading(
            "The readAsText(blob, encoding) method, when invoked, must run these steps:",
        );
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Method { name, is_static: false } if name == "readAsText")
        );
    }

    #[test]
    fn classify_fileapi_static_method() {
        let kinds =
            classify_heading("The revokeObjectURL(url) static method must run these steps:");
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Method { name, is_static: true } if name == "revokeObjectURL")
        );
    }

    #[test]
    fn classify_fileapi_method_as_follows() {
        let kinds = classify_heading(
            "The slice() method returns a new Blob object with bytes ranging from start to end. It must act as follows:",
        );
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Method { name, is_static: false } if name == "slice")
        );
    }

    #[test]
    fn classify_fileapi_method_when_called() {
        let kinds = classify_heading(
            "When the abort() method is called, the user agent must run the steps below:",
        );
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Method { name, is_static: false } if name == "abort")
        );
    }

    #[test]
    fn classify_fileapi_constructor_phrase() {
        let kinds = classify_heading(
            "The Blob() constructor can be invoked with zero or more parameters. When the Blob() constructor is invoked, user agents must run the following steps:",
        );
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Constructor { class } if class == "Blob"));
    }

    #[test]
    fn classify_fileapi_constructor_file() {
        let kinds = classify_heading(
            "The File() constructor is invoked with two or three parameters. When the File() constructor is invoked, user agents must run the following steps:",
        );
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Constructor { class } if class == "File"));
    }

    #[test]
    fn fileapi_paragraph_method_with_dfn_interface() {
        let html = r#"
            <html><body>
            <p>The <dfn data-dfn-for="Blob" data-dfn-type="method">text()</dfn>
            method, when invoked, must run these steps:</p>
            <ol>
                <li>Let stream be the result of calling get stream on this.</li>
                <li>Let reader be the result of getting a reader from stream.</li>
            </ol>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        let algo = algos
            .iter()
            .find(|a| matches!(&a.kind, AlgorithmKind::Method { name, .. } if name == "text"));
        assert!(algo.is_some(), "should find text method");
        let algo = algo.unwrap();
        assert_eq!(algo.interface, "Blob", "interface should be Blob from dfn");
        assert_eq!(algo.steps.len(), 2, "should have 2 steps");
    }

    #[test]
    fn fileapi_div_constructor_with_following_steps() {
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="blob-constructor">
            The Blob() constructor can be invoked with zero or more parameters.
            When the Blob() constructor is invoked,
            user agents must run the following steps:
            <ol>
                <li>If invoked with zero parameters, return a new Blob object.</li>
                <li>Let bytes be the result of processing blob parts.</li>
                <li>Return a new Blob object with bytes and type.</li>
            </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        let algo = algos
            .iter()
            .find(|a| matches!(&a.kind, AlgorithmKind::Constructor { class } if class == "Blob"));
        assert!(algo.is_some(), "should find Blob constructor");
        assert_eq!(algo.unwrap().steps.len(), 3, "should have 3 steps");
    }

    #[test]
    fn fileapi_div_method_as_follows() {
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="slice()" data-algorithm-for="Blob">
            The <dfn data-dfn-for="Blob" data-dfn-type="method">slice()</dfn> method
            returns a new Blob object. It must act as follows:
            <ol>
                <li>Let sliceStart be null.</li>
                <li>Let sliceEnd be null.</li>
                <li>Return a new Blob.</li>
            </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        let algo = algos
            .iter()
            .find(|a| matches!(&a.kind, AlgorithmKind::Method { name, .. } if name == "slice"));
        assert!(algo.is_some(), "should find slice method");
        let algo = algo.unwrap();
        assert_eq!(algo.interface, "Blob", "interface should be Blob");
        assert_eq!(algo.steps.len(), 3, "should have 3 steps");
    }

    #[test]
    fn fileapi_div_method_dfn_fallback() {
        // When heading text is completely unrecognizable (e.g. novel phrasing),
        // classification falls back to <dfn data-dfn-type="method">.
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="createObjectURL">
            The <dfn data-dfn-for="URL" data-dfn-type="method">createObjectURL(obj)</dfn>
            static method must return the result of adding an entry.
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        let algo = algos.iter().find(
            |a| matches!(&a.kind, AlgorithmKind::Method { name, .. } if name == "createObjectURL"),
        );
        assert!(
            algo.is_some(),
            "should find createObjectURL via dfn fallback"
        );
        let algo = algo.unwrap();
        assert!(
            matches!(
                &algo.kind,
                AlgorithmKind::Method {
                    is_static: true,
                    ..
                }
            ),
            "should detect static from heading text"
        );
        assert_eq!(algo.steps.len(), 1, "should have one-liner description");
    }

    #[test]
    fn fileapi_div_oneliner_no_ol() {
        // A div algorithm with no <ol> should produce a one-liner description step.
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="foo">
            The foo() method, when invoked, must return the result of bar.
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        let algo = algos
            .iter()
            .find(|a| matches!(&a.kind, AlgorithmKind::Method { name, .. } if name == "foo"));
        assert!(algo.is_some(), "should find foo method");
        assert_eq!(algo.unwrap().steps.len(), 1, "should have one-liner step");
    }

    #[test]
    fn extract_name_before_parens_basic() {
        assert_eq!(
            extract_name_before_parens("The text()"),
            Some("text".to_string())
        );
        assert_eq!(
            extract_name_before_parens("The readAsText(blob, encoding)"),
            Some("readAsText".to_string())
        );
        assert_eq!(
            extract_name_before_parens("When the abort()"),
            Some("abort".to_string())
        );
        assert_eq!(extract_name_before_parens("no parens"), None);
    }

    #[test]
    fn classify_constructor_multi_occurrence() {
        // FileAPI pattern: "constructor" appears twice — once in prose without
        // parens, once as the formal "Name() constructor" phrasing.
        let text = "The File constructor is invoked with two or three parameters. When the File() constructor is invoked, user agents must run the following steps:";
        let kinds = classify_heading(text);
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Constructor { class } if class == "File"));
    }

    // --- classify_from_data_attribute tests ---

    #[test]
    fn data_attr_setter() {
        let kinds = classify_from_data_attribute("hash setter", "URL");
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Setter { name } if name == "hash"));
    }

    #[test]
    fn data_attr_getter() {
        let kinds = classify_from_data_attribute("href getter", "URL");
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Getter { name } if name == "href"));
    }

    #[test]
    fn data_attr_dash_constructor() {
        let kinds = classify_from_data_attribute("blob-constructor", "Blob");
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Constructor { class } if class == "Blob"));
    }

    #[test]
    fn data_attr_bare_constructor() {
        let kinds = classify_from_data_attribute("constructor", "Headers");
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Constructor { class } if class == "Headers"));
    }

    #[test]
    fn data_attr_constructor_by_interface_match() {
        // Name matches interface → constructor
        let kinds = classify_from_data_attribute("URL(url, base)", "URL");
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Constructor { class } if class == "URL"));
    }

    #[test]
    fn data_attr_method_with_parens() {
        let kinds = classify_from_data_attribute("append(name, value)", "URLSearchParams");
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Method { name, .. } if name == "append"));
    }

    #[test]
    fn data_attr_comma_separated_overloads() {
        let kinds = classify_from_data_attribute(
            "slice(start, end, contentType), slice(start, end), slice(start), slice()",
            "Blob",
        );
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Method { name, .. } if name == "slice"));
    }

    #[test]
    fn data_attr_slash_notation() {
        let kinds = classify_from_data_attribute("URL/extract an origin", "");
        assert_eq!(kinds.len(), 1);
        assert!(
            matches!(&kinds[0], AlgorithmKind::Standalone { name } if name == "extract an origin")
        );
    }

    #[test]
    fn data_attr_bare_standalone() {
        let kinds = classify_from_data_attribute("slice blob", "Blob");
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], AlgorithmKind::Standalone { name } if name == "slice blob"));
    }

    #[test]
    fn data_attr_empty() {
        let kinds = classify_from_data_attribute("", "Blob");
        assert!(kinds.is_empty());
    }

    #[test]
    fn data_attr_fallback_in_pass2() {
        // When textual classification fails, data-algorithm provides the fallback.
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="slice blob" data-algorithm-for="Blob">
                <p>The <code>slice blob</code> steps are:</p>
                <ol>
                    <li>Slice the blob.</li>
                </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert!(
            matches!(&algos[0].kind, AlgorithmKind::Standalone { name } if name == "slice blob")
        );
    }

    #[test]
    fn data_attr_text_overrides_when_more_specific() {
        // Textual "static method steps" should win over data-algorithm's plain method.
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="from(asyncIterable)" data-algorithm-for="ReadableStream">
                The static <dfn><code>from(asyncIterable)</code></dfn> method steps are:
                <ol>
                    <li>Return ? ReadableStreamFromIterable(asyncIterable).</li>
                </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert!(matches!(
            &algos[0].kind,
            AlgorithmKind::Method { name, is_static: true } if name == "from"
        ));
    }

    #[test]
    fn div_algorithm_extracts_dfn_fragment() {
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="slice blob">
                To <dfn id="slice-blob">slice blob</dfn>, run these steps:
                <ol>
                    <li>Let blob be a new Blob.</li>
                </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert_eq!(algos[0].fragment, "slice-blob");
    }

    #[test]
    fn p_algorithm_extracts_dfn_fragment() {
        let html = r#"
            <html><body>
            <p>To <dfn id="concept-basic-url-parser">basic URL parser</dfn>, run these steps:</p>
            <ol>
                <li>Parse the input.</li>
            </ol>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert_eq!(algos[0].fragment, "concept-basic-url-parser");
    }

    #[test]
    fn algorithm_without_dfn_id_has_empty_fragment() {
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="do something">
                To do something, run these steps:
                <ol>
                    <li>Do it.</li>
                </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert_eq!(algos[0].fragment, "");
    }

    #[test]
    fn interface_member_extracts_dfn_fragment() {
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="locked" data-algorithm-for="ReadableStream">
                The <dfn id="rs-locked" data-dfn-for="ReadableStream" data-dfn-type="attribute">locked</dfn> getter steps are:
                <ol>
                    <li>Return ! IsReadableStreamLocked(this).</li>
                </ol>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert_eq!(algos[0].fragment, "rs-locked");
    }

    #[test]
    fn nested_ol_substeps_get_dotted_labels() {
        let html = r#"
            <html><body>
            <p>To frob a widget:</p>
            <ol>
                <li><p>Let <var>x</var> be 0.</p></li>
                <li><p>While true:</p>
                    <ol>
                        <li><p>Increment <var>x</var>.</p></li>
                        <li><p>If <var>x</var> is 3, then:</p>
                            <ol><li><p>Break.</p></li></ol>
                        </li>
                    </ol>
                </li>
                <li><p>Return <var>x</var>.</p></li>
            </ol>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        let steps = &algos[0].steps;
        let labels: Vec<&str> = steps.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["1", "2", "2.1", "2.2", "2.2.1", "3"]);
        assert_eq!(steps[1].text, "While true:");
        assert_eq!(steps[3].text, "If _x_ is 3, then:");
        assert_eq!(steps[4].text, "Break.");
    }

    #[test]
    fn switch_branches_get_labeled_steps() {
        let html = r#"
            <html><body>
            <p>To process a widget:</p>
            <ol>
                <li><p>Switch on <var>object</var>:</p>
                    <dl class="switch">
                        <dt><code>Blob</code></dt>
                        <dd><p>Set <var>source</var> to <var>object</var>.</p>
                            <p>Set <var>length</var> to <var>object</var>’s size.</p></dd>
                        <dt><a href="x">byte sequence</a></dt>
                        <dd><p>Set <var>source</var> to <var>object</var>.</p></dd>
                    </dl>
                </li>
                <li><p>Return <var>source</var>.</p></li>
            </ol>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        let steps = &algos[0].steps;
        let labels: Vec<&str> = steps.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["1", "1 `Blob`", "1 `byte sequence`", "2"]);
        assert_eq!(steps[0].text, "Switch on _object_:");
        assert_eq!(
            steps[1].text,
            "Set _source_ to _object_. Set _length_ to _object_’s size."
        );
        assert_eq!(steps[2].text, "Set _source_ to _object_.");
    }

    #[test]
    fn switch_branch_with_substeps_gets_dotted_branch_labels() {
        let html = r#"
            <html><body>
            <p>To process a widget:</p>
            <ol>
                <li><p>Switch on <var>state</var>:</p>
                    <dl class="switch">
                        <dt><code>ready</code></dt>
                        <dd>
                            <ol>
                                <li><p>Let <var>a</var> be 1.</p></li>
                                <li><p>Return <var>a</var>.</p></li>
                            </ol>
                        </dd>
                    </dl>
                </li>
            </ol>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        let steps = &algos[0].steps;
        let labels: Vec<&str> = steps.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["1", "1 `ready`.1", "1 `ready`.2"]);
        assert_eq!(steps[1].text, "Let _a_ be 1.");
    }

    #[test]
    fn switch_branch_labels_collapse_nested_code_link_backticks() {
        let html = r#"
            <html><body>
            <p>To process a widget:</p>
            <ol>
                <li><p>Switch on <var>object</var>:</p>
                    <dl class="switch">
                        <dt><code class="idl"><a href="x">Blob</a></code></dt>
                        <dd><p>Return true.</p></dd>
                    </dl>
                </li>
            </ol>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        let labels: Vec<&str> = algos[0].steps.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["1", "1 `Blob`"]);
    }

    #[test]
    fn nested_code_and_link_yield_single_backticks() {
        let html = r##"
            <html><body>
            <p>The clone() method steps are:</p>
            <ol>
                <li>If this is unusable, then throw a <code><a href="#te">TypeError</a></code>.</li>
                <li>Return a <a href="#resp"><code>Response</code></a> object.</li>
            </ol>
            </body></html>
        "##;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert_eq!(
            algos[0].steps[0].text,
            "If this is unusable, then throw a `TypeError`."
        );
        assert_eq!(algos[0].steps[1].text, "Return a `Response` object.");
    }

    #[test]
    fn empty_self_link_anchor_yields_no_backticks() {
        let html = r##"
            <html><body>
            <p>The clone() method steps are:</p>
            <ol>
                <li><a class="self-link" href="#step"></a>Set this’s signal.</li>
            </ol>
            </body></html>
        "##;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert_eq!(algos[0].steps[0].text, "Set this’s signal.");
    }

    #[test]
    fn literal_backticks_around_code_merge_into_one_pair() {
        // Specs write byte sequences as `<code>GET</code>` — with literal
        // backtick characters in the surrounding text.
        let html = r##"
            <html><body>
            <p>The clone() method steps are:</p>
            <ol>
                <li>If method is neither `<code>GET</code>` nor `<code>HEAD</code>`, then throw.</li>
            </ol>
            </body></html>
        "##;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert_eq!(
            algos[0].steps[0].text,
            "If method is neither `GET` nor `HEAD`, then throw."
        );
    }

    #[test]
    fn link_text_containing_code_span_keeps_one_backtick_pair() {
        // Some dfn names embed a code span, e.g. the "`multipart/form-data`
        // encoding algorithm"; the inner backticks would collide with the
        // wrapping pair and are dropped.
        let html = r##"
            <html><body>
            <p>The clone() method steps are:</p>
            <ol>
                <li>Run the <a href="#mfd">`<code>multipart/form-data</code>` encoding algorithm</a>.</li>
            </ol>
            </body></html>
        "##;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert_eq!(
            algos[0].steps[0].text,
            "Run the `multipart/form-data encoding algorithm`."
        );
    }

    #[test]
    fn multiple_dts_share_one_branch_label() {
        let html = r#"
            <html><body>
            <p>To process a widget:</p>
            <ol>
                <li><p>Switch on <var>unit</var>:</p>
                    <dl class="switch">
                        <dt><code>day</code></dt>
                        <dt><code>week</code></dt>
                        <dd><p>Return true.</p></dd>
                    </dl>
                </li>
            </ol>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        let steps = &algos[0].steps;
        let labels: Vec<&str> = steps.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["1", "1 `day`, `week`"]);
        assert_eq!(steps[1].text, "Return true.");
    }

    #[test]
    fn div_algorithm_with_top_level_ul_gets_bullet_step() {
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="CORS-unsafe request-header byte">
                <p>A <dfn id="cors-unsafe-request-header-byte">CORS-unsafe request-header byte</dfn> is a byte <var>byte</var> for which one of the following is true:</p>
                <ul class="brief">
                    <li><p><var>byte</var> is less than 0x20 and is not 0x09 HT</p></li>
                    <li><p><var>byte</var> is 0x22 (")</p></li>
                </ul>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        // The heading must stop before the list, not swallow it.
        assert_eq!(
            algos[0].heading,
            "A CORS-unsafe request-header byte is a byte _byte_ for which one of the following is true:"
        );
        let steps = &algos[0].steps;
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].label, "1");
        assert_eq!(
            steps[0].text,
            "A CORS-unsafe request-header byte is a byte _byte_ for which one of the following is true:\n- _byte_ is less than 0x20 and is not 0x09 HT\n- _byte_ is 0x22 (\")"
        );
    }

    #[test]
    fn div_algorithm_with_top_level_switch_gets_branch_steps() {
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="frob a widget">
                <p>To <dfn id="frob">frob a widget</dfn>, switch on <var>kind</var>:</p>
                <dl class="switch">
                    <dt><code>gadget</code></dt>
                    <dd><p>Return true.</p></dd>
                </dl>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        assert_eq!(algos[0].heading, "To frob a widget, switch on _kind_:");
        let labels: Vec<&str> = algos[0].steps.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["1", "1 `gadget`"]);
        assert_eq!(algos[0].steps[0].text, "To frob a widget, switch on _kind_:");
        assert_eq!(algos[0].steps[1].text, "Return true.");
    }

    #[test]
    fn div_algorithm_with_switch_holding_ols_keeps_all_branches() {
        // URL spec "origin" shape: the algorithm body is a top-level
        // <dl class="switch"> whose arms contain the <ol>s. The first arm's
        // list must not be mistaken for the whole algorithm's steps.
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="origin" data-algorithm-for="url">
                <p>The <dfn id="concept-url-origin">origin</dfn> of a URL <var>url</var> is computed by switching on <var>url</var>’s scheme:</p>
                <dl class="switch">
                    <dt>"<code>blob</code>"</dt>
                    <dd>
                        <ol>
                            <li><p>Let <var>pathURL</var> be the path.</p></li>
                            <li><p>Return <var>pathURL</var>’s origin.</p></li>
                        </ol>
                    </dd>
                    <dt>"<code>file</code>"</dt>
                    <dd><p>Return an opaque origin.</p></dd>
                </dl>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        let labels: Vec<&str> = algos[0].steps.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "1",
                "1 \"`blob`\".1",
                "1 \"`blob`\".2",
                "1 \"`file`\""
            ]
        );
        assert_eq!(algos[0].steps[3].text, "Return an opaque origin.");
    }

    #[test]
    fn outer_algorithm_does_not_steal_nested_algorithm_steps() {
        // Streams spec "read all bytes" shape: a one-liner algorithm div
        // containing a nested algorithm div that has the only <ol>.
        let html = r#"
            <html><body>
            <div class="algorithm" data-algorithm="read all bytes">
                <p>To <dfn id="read-all-bytes">read all bytes</dfn> from a reader <var>reader</var>: <a href="x">read-loop</a> given <var>reader</var> and a new byte sequence.</p>
                <div class="algorithm" data-algorithm="read-loop">
                    To <dfn id="read-loop">read-loop</dfn> given <var>reader</var> and <var>bytes</var>:
                    <ol>
                        <li><p>Let <var>readRequest</var> be a new read request.</p></li>
                        <li><p>Perform <var>readRequest</var>.</p></li>
                    </ol>
                </div>
            </div>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 2);
        let outer = algos
            .iter()
            .find(|a| matches!(&a.kind, AlgorithmKind::Standalone { name } if name.contains("read all bytes")))
            .unwrap();
        let inner = algos
            .iter()
            .find(|a| matches!(&a.kind, AlgorithmKind::Standalone { name } if name.contains("read-loop")))
            .unwrap();

        // The outer algorithm's heading and steps must not include the nested
        // algorithm's content.
        assert!(!outer.heading.contains("readRequest"));
        assert_eq!(outer.steps.len(), 1);
        assert!(!outer.steps[0].text.contains("readRequest"));
        // The nested algorithm still gets its own numbered steps.
        assert_eq!(inner.steps.len(), 2);
        assert_eq!(inner.steps[0].label, "1");
    }

    #[test]
    fn ul_items_become_bullet_lines() {
        let html = r#"
            <html><body>
            <p>To check a request:</p>
            <ol>
                <li><p>If all of the following conditions are true:</p>
                    <ul>
                        <li><p><var>request</var>’s mode is "<code>cors</code>"</p></li>
                        <li><p><var>request</var>’s client is not null</p></li>
                    </ul>
                    <p>then return true.</p>
                </li>
            </ol>
            </body></html>
        "#;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        let steps = &algos[0].steps;
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].label, "1");
        assert_eq!(
            steps[0].text,
            "If all of the following conditions are true:\n- _request_’s mode is \"`cors`\"\n- _request_’s client is not null\nthen return true."
        );
    }

    #[test]
    fn dl_definitions_become_bullet_lines() {
        // A plain (non-switch) <dl> inside a step holds name/value property
        // pairs, e.g. step 12 of the Request constructor. Each dt/dd pair
        // becomes a "- name: value" bullet line.
        let html = r##"
            <html><body>
            <p>To make a request:</p>
            <ol>
                <li><p>Set <var>request</var> to a new <a href="#request">request</a> with the following properties:</p>
                    <dl>
                        <dt><a href="#url">URL</a>
                        <dd><var>request</var>’s <a href="#url">URL</a>.
                        <dt><a href="#unsafe">unsafe-request flag</a>
                        <dd>Set.
                    </dl>
                </li>
            </ol>
            </body></html>
        "##;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        let steps = &algos[0].steps;
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].label, "1");
        assert_eq!(
            steps[0].text,
            "Set _request_ to a new `request` with the following properties:\n- `URL`: _request_’s `URL`.\n- `unsafe-request flag`: Set."
        );
    }

    #[test]
    fn multiple_dts_share_one_definition_line() {
        let html = r##"
            <html><body>
            <p>To make a request:</p>
            <ol>
                <li><p>Set <var>request</var> to a new request with the following properties:</p>
                    <dl>
                        <dt><a href="#mode">mode</a>
                        <dt><a href="#cache">cache mode</a>
                        <dd><var>request</var>’s <a href="#mode">mode</a>.
                    </dl>
                </li>
            </ol>
            </body></html>
        "##;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        let steps = &algos[0].steps;
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].text,
            "Set _request_ to a new request with the following properties:\n- `mode`, `cache mode`: _request_’s `mode`."
        );
    }

    #[test]
    fn note_in_dl_value_gets_own_spec_note_line() {
        // Notes embedded in a definition value (e.g. the origin property in
        // step 12 of the Request constructor) move to their own line with a
        // "Spec note: " prefix instead of running into the value text.
        let html = r##"
            <html><body>
            <p>To make a request:</p>
            <ol>
                <li><p>Set <var>request</var> to a new request with the following properties:</p>
                    <dl>
                        <dt><a href="#origin">origin</a>
                        <dd><var>request</var>’s <a href="#origin">origin</a>.
                            <span class="note">The propagation of the origin is only significant
                            for navigation requests.</span>
                    </dl>
                </li>
            </ol>
            </body></html>
        "##;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        let steps = &algos[0].steps;
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].text,
            "Set _request_ to a new request with the following properties:\n- `origin`: _request_’s `origin`.\nSpec note: The propagation of the origin is only significant for navigation requests."
        );
    }

    #[test]
    fn note_paragraph_in_step_gets_own_spec_note_line() {
        let html = r##"
            <html><body>
            <p>To make a request:</p>
            <ol>
                <li><p>If <var>init</var> is not empty, then:</p>
                    <p class="note">This is done to ensure that redirected requests no longer
                    appear to come from the original source.</p>
                </li>
            </ol>
            </body></html>
        "##;

        let algos = extract_algorithms(html);
        assert_eq!(algos.len(), 1);
        let steps = &algos[0].steps;
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].text,
            "If _init_ is not empty, then:\nSpec note: This is done to ensure that redirected requests no longer appear to come from the original source."
        );
    }
}
