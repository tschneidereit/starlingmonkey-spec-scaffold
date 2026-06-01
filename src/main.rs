// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Generates StarlingMonkey builtin scaffolding from WHATWG/W3C specs.
//!
//! Reads a web spec (by URL or shorthand name), extracts WebIDL blocks
//! and algorithm steps, and produces Rust source files with
//! `#[webidl_interface]`, `#[webidl_methods]`, and related annotations.
//!
//! The generated code contains `todo!()` bodies — it's scaffolding for
//! a developer to fill in, not a working implementation.

mod codegen;
mod extract;
mod fetch;
mod idl;
mod types;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

/// Generate StarlingMonkey builtin scaffolding from a web spec.
#[derive(Parser)]
#[command(name = "spec-scaffold", version, about)]
struct Cli {
    /// Spec URL or shorthand name (e.g., "url", "fetch", "xhr").
    spec: String,

    /// Output directory for generated files. If not given, prints to stdout.
    #[arg(short, long)]
    output_dir: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve spec URL
    let url = fetch::resolve_spec_url(&cli.spec)?;
    eprintln!("Fetching {url}...");

    // Fetch the spec HTML
    let html = fetch::fetch_spec(&url)?;
    eprintln!("Fetched {} bytes", html.len());

    // Extract WebIDL blocks
    let idl_blocks = extract::extract_idl_blocks(&html);
    eprintln!("Found {} WebIDL blocks", idl_blocks.len());

    // Extract algorithm steps
    let algorithms = extract::extract_algorithms(&html);
    eprintln!("Found {} algorithm sections", algorithms.len());

    // Extract spec definition anchors and internal slots
    let spec_defs = extract::extract_spec_definitions(&html);
    eprintln!(
        "Found {} class sections, {} member fragments, {} internal slot tables",
        spec_defs.class_sections.len(),
        spec_defs.member_fragments.len(),
        spec_defs.internal_slots.len(),
    );

    // Parse WebIDL into our model
    let mut idl_texts: Vec<String> = idl_blocks.iter().map(|b| b.text.clone()).collect();
    idl_texts.dedup();
    let model = idl::parse_idl(&idl_texts, &algorithms, &spec_defs)
        .context("failed to parse WebIDL from spec")?;

    eprintln!(
        "Parsed: {} interfaces, {} dictionaries, {} enums, {} typedefs",
        model.interfaces.iter().filter(|i| !i.is_mixin).count(),
        model.dictionaries.len(),
        model.enums.len(),
        model.typedefs.len(),
    );

    // Generate Rust source files
    let files = codegen::generate(&model, &url, &spec_defs);

    // Output
    if let Some(output_dir) = &cli.output_dir {
        codegen::write_files(&files, output_dir)?;
        eprintln!("Wrote {} files to {}", files.len(), output_dir.display());
        for file in &files {
            eprintln!("  {}", file.filename);
        }
    } else {
        // Print to stdout with separators
        for file in &files {
            println!("// === {} ===", file.filename);
            println!();
            print!("{}", file.content);
            println!();
        }
    }

    Ok(())
}
