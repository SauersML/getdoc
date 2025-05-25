// getdoc - main.rs

// --- Standard Library Imports ---
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// --- External Crate Imports ---
use chrono::Local;
use home;
use quote::ToTokens;
use serde::Deserialize;
use syn;

// --- Struct Definitions ---

#[derive(Deserialize, Debug)]
struct RustcDiagnostic {
    #[serde(default)]
    code: Option<RustcErrorCode>,
    level: String,
    spans: Vec<RustcSpan>,
    children: Vec<RustcDiagnostic>,
    rendered: Option<String>, // The human-readable version of this specific diagnostic
}

#[derive(Deserialize, Debug)]
struct RustcErrorCode {
    code: String, // e.g., "E0308"
}

#[derive(Deserialize, Debug)]
struct RustcSpan {
    file_name: String,
}

#[derive(Debug)]
struct ExtractedItem {
    item_kind: String, // e.g., "Function", "Struct", "Enum", "Trait", "Impl", "Mod"
    name: String,
    signature_or_definition: String,
    doc_comments: Vec<String>,
}

// --- Main Function ---

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("[getdoc] Starting analysis...");

    let (compiler_diagnostics_rendered, third_party_files_to_inspect) = run_cargo_and_get_files()?;

    if compiler_diagnostics_rendered.is_empty() && third_party_files_to_inspect.is_empty() {
        println!("[getdoc] No relevant compiler messages found or no third-party files implicated. Exiting.");
        let mut report_writer = BufWriter::new(File::create("report.md")?);
        writeln!(report_writer, "# GetDoc Report - {}", Local::now().to_rfc2822())?;
        writeln!(report_writer, "\n## Compiler Output\n\n```text\nNo errors or warnings reported by the compiler that involved inspectable third-party files.\n```")?;
        println!("[getdoc] Report generated: report.md");
        return Ok(());
    }

    let mut extracted_data: HashMap<PathBuf, Vec<ExtractedItem>> = HashMap::new();
    let mut sorted_file_paths: Vec<PathBuf> = third_party_files_to_inspect.into_iter().collect();
    sorted_file_paths.sort();

    for file_path in &sorted_file_paths {
        println!("[getdoc] Inspecting: {}", file_path.display());
        match extract_items_from_file(file_path) {
            Ok(items) => {
                if !items.is_empty() {
                    extracted_data.insert(file_path.clone(), items);
                } else {
                    println!("[getdoc] No extractable items found in: {}", file_path.display());
                }
            }
            Err(e) => eprintln!("[getdoc] Warning: Could not process file {}: {}", file_path.display(), e),
        }
    }

    generate_markdown_report(&compiler_diagnostics_rendered, &extracted_data, &sorted_file_paths)?;

    println!("[getdoc] Analysis complete. Report generated: report.md");
    Ok(())
}

// --- Helper Functions ---

fn run_cargo_and_get_files() -> Result<(Vec<String>, HashSet<PathBuf>), Box<dyn std::error::Error>> {
    println!("[getdoc] Running `cargo check --message-format=json`...");
    let cargo_output = Command::new("cargo")
        .arg("check")
        .arg("--message-format=json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !cargo_output.status.success() {
        if !cargo_output.stderr.is_empty() {
            let stderr_text = String::from_utf8_lossy(&cargo_output.stderr);
            if !stderr_text.trim().is_empty() {
            }
        }
    }

    let mut rendered_diagnostics: Vec<String> = Vec::new();
    let mut implicated_files: HashSet<PathBuf> = HashSet::new();
    let current_dir = std::env::current_dir()?;
    let cargo_home_dir = home::cargo_home().ok();
    let stdout_str = String::from_utf8_lossy(&cargo_output.stdout);

    for line in stdout_str.lines() {
        if line.trim().is_empty() || !line.starts_with('{') {
            continue;
        }
        match serde_json::from_str::<RustcDiagnostic>(line) {
            Ok(diag) => {
                if diag.level == "error" || diag.level == "warning" {
                    if let Some(rendered) = &diag.rendered {
                        if !rendered.trim().is_empty() {
                            let prefix = diag.code.as_ref().map_or_else(
                                || format!("{}: ", diag.level.to_uppercase()), // ERROR: or WARNING:
                                |ec| format!("{}: {}: ", diag.level.to_uppercase(), ec.code), // ERROR: E0123:
                            );
                            rendered_diagnostics.push(format!("{}{}", prefix, rendered.trim_end()));
                        }
                    }
                }
                // Collect files implicated by errors OR warnings
                if diag.level == "error" || diag.level == "warning" {
                     collect_files_from_diagnostic(&diag, &mut implicated_files, &current_dir, &cargo_home_dir);
                }
            }
            Err(_e) => {
                // eprintln!("[getdoc] Warning: Could not parse JSON line as RustcDiagnostic: {} (Line: '{}')", e, line);
            }
        }
    }
    Ok((rendered_diagnostics, implicated_files))
}

fn collect_files_from_diagnostic(
    diag: &RustcDiagnostic,
    implicated_files: &mut HashSet<PathBuf>,
    current_dir: &Path,
    cargo_home_dir: &Option<PathBuf>,
) {
    for span in &diag.spans {
        let path = PathBuf::from(&span.file_name);
        let absolute_path = if path.is_absolute() {
            path.clone()
        } else {
            current_dir.join(&path)
        };

        if let Ok(canonical_path) = fs::canonicalize(&absolute_path) {
            if !canonical_path.starts_with(current_dir) {
                let is_in_cargo_registry = cargo_home_dir
                    .as_ref()
                    .map_or(false, |ch| canonical_path.starts_with(&ch.join("registry").join("src")));
                let is_in_cargo_git = cargo_home_dir
                    .as_ref()
                    .map_or(false, |ch| canonical_path.starts_with(&ch.join("git").join("checkouts")));

                if (is_in_cargo_registry || is_in_cargo_git) && canonical_path.exists() && canonical_path.is_file() {
                    implicated_files.insert(canonical_path);
                }
            }
        }
    }
    for child_diag in &diag.children {
        collect_files_from_diagnostic(child_diag, implicated_files, current_dir, cargo_home_dir);
    }
}

fn extract_items_from_file(file_path: &PathBuf) -> Result<Vec<ExtractedItem>, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(file_path)?;
    let ast = syn::parse_file(&content)?;
    let mut items = Vec::new();

    for item_syn in ast.items { // Renamed to avoid conflict
        let (item_kind_str, name_str, sig_def_str, doc_comments_vec) = match &item_syn {
            syn::Item::Fn(item_fn) => {
                let docs = extract_doc_comments(&item_fn.attrs);
                let vis_string = item_fn.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let sig = format!("{}{}",
                    vis_prefix,
                    item_fn.sig.to_token_stream().to_string()
                );
                ("Function".to_string(), item_fn.sig.ident.to_string(), sig, docs)
            }
            syn::Item::Struct(item_struct) => {
                let docs = extract_doc_comments(&item_struct.attrs);
                let vis_string = item_struct.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}struct {}{}",
                    vis_prefix,
                    item_struct.ident.to_token_stream().to_string(),
                    item_struct.generics.to_token_stream().to_string()
                );
                ("Struct".to_string(), item_struct.ident.to_string(), def, docs)
            }
            syn::Item::Enum(item_enum) => {
                let docs = extract_doc_comments(&item_enum.attrs);
                let vis_string = item_enum.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}enum {}{}",
                    vis_prefix,
                    item_enum.ident.to_token_stream().to_string(),
                    item_enum.generics.to_token_stream().to_string()
                );
                ("Enum".to_string(), item_enum.ident.to_string(), def, docs)
            }
            syn::Item::Trait(item_trait) => {
                let docs = extract_doc_comments(&item_trait.attrs);
                let vis_string = item_trait.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}trait {}{}",
                    vis_prefix,
                    item_trait.ident.to_token_stream().to_string(),
                    item_trait.generics.to_token_stream().to_string()
                );
                ("Trait".to_string(), item_trait.ident.to_string(), def, docs)
            }
            syn::Item::Mod(item_mod) => {
                let docs = extract_doc_comments(&item_mod.attrs);
                let vis_string = item_mod.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}mod {}",
                    vis_prefix,
                    item_mod.ident.to_token_stream().to_string()
                );
                ("Module".to_string(), item_mod.ident.to_string(), def, docs)
            }
            syn::Item::Impl(item_impl) => {
                let docs = extract_doc_comments(&item_impl.attrs);
                let mut impl_line_tokens = quote::quote! {};
                if let Some(defaultness) = &item_impl.defaultness { defaultness.to_tokens(&mut impl_line_tokens); impl_line_tokens.extend(quote::quote! { }); }
                if let Some(unsafety) = &item_impl.unsafety { unsafety.to_tokens(&mut impl_line_tokens); impl_line_tokens.extend(quote::quote! { }); }
                impl_line_tokens.extend(quote::quote! { impl });
                item_impl.generics.params.to_tokens(&mut impl_line_tokens);
                if !item_impl.generics.params.is_empty() { impl_line_tokens.extend(quote::quote! { }); }


                let mut name_parts: Vec<String> = Vec::new();
                if let Some((opt_bang, trait_path, _for_keyword)) = &item_impl.trait_ {
                    if opt_bang.is_some() { impl_line_tokens.extend(quote::quote! { ! }); }
                    trait_path.to_tokens(&mut impl_line_tokens);
                    name_parts.push(trait_path.to_token_stream().to_string());
                    impl_line_tokens.extend(quote::quote! { for });
                    name_parts.push("for".to_string());
                    impl_line_tokens.extend(quote::quote! { }); // Space after for
                }
                item_impl.self_ty.to_tokens(&mut impl_line_tokens);
                name_parts.push(item_impl.self_ty.to_token_stream().to_string());

                if let Some(where_clause) = &item_impl.generics.where_clause {
                    impl_line_tokens.extend(quote::quote! { }); // Space before where
                    where_clause.to_tokens(&mut impl_line_tokens);
                }

                let name = if name_parts.is_empty() {
                    // This case happens for inherent impls: `impl Type { ... }`
                    // We want the name to be "Type" or similar.
                    item_impl.self_ty.to_token_stream().to_string()
                } else {
                    format!("impl {}", name_parts.join(" "))
                };
                let item_kind = if item_impl.trait_.is_some() { "Trait Impl Block".to_string() } else { "Inherent Impl Block".to_string() };
                (item_kind, name, impl_line_tokens.to_string(), docs)
            }
            syn::Item::Type(item_type) => {
                let docs = extract_doc_comments(&item_type.attrs);
                let vis_string = item_type.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}type {}{} = {};",
                    vis_prefix,
                    item_type.ident.to_token_stream().to_string(),
                    item_type.generics.to_token_stream().to_string(),
                    item_type.ty.to_token_stream().to_string()
                );
                ("Type Alias".to_string(), item_type.ident.to_string(), def, docs)
            }
            syn::Item::Const(item_const) => {
                let docs = extract_doc_comments(&item_const.attrs);
                let vis_string = item_const.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}const {}: {} = ...;",
                    vis_prefix,
                    item_const.ident.to_token_stream().to_string(),
                    item_const.ty.to_token_stream().to_string()
                );
                ("Constant".to_string(), item_const.ident.to_string(), def, docs)
            }
            syn::Item::Static(item_static) => {
                let docs = extract_doc_comments(&item_static.attrs);
                let vis_string = item_static.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}static {}: {} = ...;",
                    vis_prefix,
                    item_static.ident.to_token_stream().to_string(),
                    item_static.ty.to_token_stream().to_string()
                );
                ("Static".to_string(), item_static.ident.to_string(), def, docs)
            }
            _ => continue,
        };
        items.push(ExtractedItem {
            item_kind: item_kind_str,
            name: name_str,
            signature_or_definition: sig_def_str.trim().to_string(),
            doc_comments: doc_comments_vec,
        });
    }
    Ok(items)
}

fn extract_doc_comments(attrs: &[syn::Attribute]) -> Vec<String> {
    attrs.iter()
        .filter_map(|attr| {
            if attr.path().is_ident("doc") {
                match &attr.meta {
                    syn::Meta::NameValue(meta_name_value) => {
                        if let syn::Expr::Lit(expr_lit) = &meta_name_value.value {
                            if let syn::Lit::Str(lit_str) = &expr_lit.lit {
                                return Some(lit_str.value().trim().to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            None
        })
        .collect()
}

fn generate_markdown_report(
    compiler_diagnostics_rendered: &[String],
    extracted_data: &HashMap<PathBuf, Vec<ExtractedItem>>,
    sorted_file_paths: &[PathBuf],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = BufWriter::new(File::create("report.md")?);

    writeln!(writer, "# GetDoc Report - {}", Local::now().to_rfc2822())?;

    writeln!(writer, "\n## Compiler Output (Errors and Warnings)\n")?;
    writeln!(writer, "```text")?;
    if compiler_diagnostics_rendered.is_empty() {
        writeln!(writer, "No errors or warnings reported by the compiler, or none implicated third-party files.")?;
    } else {
        for diag_line in compiler_diagnostics_rendered {
            writeln!(writer, "{}", diag_line)?;
        }
    }
    writeln!(writer, "```\n")?;

    if extracted_data.is_empty() {
        writeln!(writer, "No third-party crate information extracted (or no third-party files were implicated).")?;
    } else {
        for file_path in sorted_file_paths {
            if let Some(items) = extracted_data.get(file_path) {
                writeln!(writer, "---\n## From File: `{}`\n", file_path.display())?;
                if items.is_empty() {
                    writeln!(writer, "_No extractable items (functions, structs, etc.) found or processed in this file._\n")?;
                    continue;
                }
                for item in items {
                    // Use a more specific header if item.name is available and not too generic like "impl"
                    let item_header_name = if item.name == "impl" && item.item_kind.contains("Impl Block") {
                         // For Impl blocks, the constructed signature is more descriptive than "impl"
                         item.signature_or_definition.split('{').next().unwrap_or(&item.name).trim()
                    } else {
                        &item.name
                    };
                    writeln!(writer, "### {} `{}`\n", item.item_kind, item_header_name)?;

                    if !item.doc_comments.is_empty() {
                        for doc_line in &item.doc_comments {
                            if doc_line.is_empty() {
                                writeln!(writer, ">")?;
                            } else {
                                writeln!(writer, "> {}", doc_line)?;
                            }
                        }
                        writeln!(writer)?;
                    }
                    writeln!(writer, "```rust\n{}\n```\n", item.signature_or_definition)?;
                }
            }
        }
    }
    Ok(())
}
