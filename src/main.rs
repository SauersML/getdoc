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
use toml;

// --- Struct Definitions ---

#[derive(Deserialize, Debug, Default)]
struct CargoToml {
    #[serde(default)]
    features: HashMap<String, Vec<String>>,
}

#[derive(Deserialize, Debug)]
struct TopLevelCargoMessage {
    reason: String,
    #[serde(default)]
    message: Option<RustcDiagnosticData>,
}

#[derive(Deserialize, Debug, Clone)]
struct RustcDiagnosticData {
    // `message: String` (raw message) removed; `rendered` is used.
    #[serde(default)]
    code: Option<RustcErrorCode>,
    level: String,
    spans: Vec<RustcSpan>,
    children: Vec<RustcDiagnosticData>,
    rendered: Option<String>,
}

#[derive(Deserialize, Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd)]
struct RustcErrorCode {
    code: String,
}

#[derive(Deserialize, Debug, Clone)]
struct RustcSpan {
    file_name: String,
    is_primary: bool,
    line_start: usize,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd)]
struct DiagnosticOriginInfo {
    level: String,
    code: Option<String>,
    originating_diagnostic_span_location: String, // file_name:line_start of the primary span of the diagnostic
    feature_set_desc: String,
}

#[derive(Debug)]
struct DisplayableDiagnostic {
    level: String,
    code: Option<String>,
    rendered: String,
    primary_location_of_diagnostic: String, // file_name:line_start of its own primary span
    implicated_third_party_files_details: Vec<(PathBuf, String)>, // (file_path, "file_name:line" of primary span within it)
}

#[derive(Debug)]
struct ExtractedItem {
    item_kind: String,
    name: String,
    signature_or_definition: String,
    doc_comments: Vec<String>,
}

// --- Main Function ---

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("[getdoc] Starting analysis for multiple feature sets...");

    let feature_sets_to_check = get_feature_sets_to_check().unwrap_or_else(|e| {
        eprintln!("[getdoc] Warning: Could not determine feature sets from Cargo.toml: {}. Proceeding with default check only.", e);
        vec![vec![]]
    });

    let mut all_displayable_diagnostics: Vec<(String, Vec<DisplayableDiagnostic>)> = Vec::new();
    let mut all_implicated_files_globally: HashSet<PathBuf> = HashSet::new();
    let mut global_file_referencers: HashMap<PathBuf, HashSet<DiagnosticOriginInfo>> = HashMap::new();

    for feature_args in &feature_sets_to_check {
        let feature_desc = if feature_args.is_empty() {
            "default features".to_string()
        } else {
            feature_args.join(" ")
        };
        println!("[getdoc] Running `cargo check --message-format=json {}`...", feature_desc);

        match run_cargo_check_with_features(feature_args, &feature_desc) {
            Ok((diagnostics_for_run, implicated_files_for_run, referencers_for_run)) => {
                if !diagnostics_for_run.is_empty() {
                    all_displayable_diagnostics.push((feature_desc.clone(), diagnostics_for_run));
                }
                all_implicated_files_globally.extend(implicated_files_for_run);
                for (file, origins) in referencers_for_run {
                    global_file_referencers.entry(file).or_default().extend(origins);
                }
            }
            Err(e) => {
                let error_message = format!("Error running cargo check with configuration '{}': {}", feature_desc, e);
                eprintln!("[getdoc] {}", error_message);
                all_displayable_diagnostics.push((feature_desc.clone(), vec![DisplayableDiagnostic {
                    level: "TOOL_ERROR".to_string(),
                    code: None,
                    rendered: error_message,
                    primary_location_of_diagnostic: "N/A".to_string(),
                    implicated_third_party_files_details: vec![],
                }]));
            }
        }
    }

    if all_displayable_diagnostics.iter().all(|(_, diags)| diags.is_empty()) && all_implicated_files_globally.is_empty() {
        println!("[getdoc] No relevant compiler messages found or no third-party files implicated across all feature checks. Exiting.");
        let mut report_writer = BufWriter::new(File::create("report.md")?);
        writeln!(report_writer, "# GetDoc Report - {}", Local::now().to_rfc2822())?;
        writeln!(report_writer, "\n## Compiler Output (Errors and Warnings)\n\n```text\nNo errors or warnings reported by the compiler across checked feature configurations, or none implicated third-party files.\n```")?;
        println!("[getdoc] Minimal report generated: report.md");
        return Ok(());
    }

    let mut extracted_data: HashMap<PathBuf, Vec<ExtractedItem>> = HashMap::new();
    let mut sorted_file_paths: Vec<PathBuf> = all_implicated_files_globally.into_iter().collect();
    sorted_file_paths.sort();

    for file_path in &sorted_file_paths {
        println!("[getdoc] Inspecting: {}", file_path.display());
        match extract_items_from_file(file_path) {
            Ok(items) => {
                if !items.is_empty() {
                    extracted_data.insert(file_path.clone(), items);
                } else {
                    println!("[getdoc] No extractable items (meeting criteria) found in: {}", file_path.display());
                }
            }
            Err(e) => eprintln!("[getdoc] Warning: Could not process file {}: {}", file_path.display(), e),
        }
    }

    generate_markdown_report(&all_displayable_diagnostics, &extracted_data, &sorted_file_paths, &global_file_referencers)?;

    println!("[getdoc] Analysis complete. Report generated: report.md");
    Ok(())
}

// --- Helper Functions ---

fn get_feature_sets_to_check() -> Result<Vec<Vec<String>>, Box<dyn std::error::Error>> {
    let mut sets = Vec::new();
    sets.push(vec![]); // Default features

    let cargo_toml_path = PathBuf::from("Cargo.toml");
    if !cargo_toml_path.exists() {
        println!("[getdoc] Warning: Cargo.toml not found in current directory. Only checking with default features.");
        return Ok(sets);
    }

    let cargo_toml_content = fs::read_to_string(cargo_toml_path)?;
    let parsed_toml: CargoToml = toml::from_str(&cargo_toml_content).unwrap_or_default();

    if !parsed_toml.features.is_empty() {
        sets.push(vec!["--no-default-features".to_string()]);
        for feature_name in parsed_toml.features.keys() {
            if feature_name != "default" {
                sets.push(vec![
                    "--no-default-features".to_string(),
                    "--features".to_string(),
                    feature_name.clone(),
                ]);
            }
        }
        sets.push(vec!["--all-features".to_string()]);
    }

    let mut unique_sets_str: HashSet<String> = HashSet::new();
    let mut unique_sets_vec: Vec<Vec<String>> = Vec::new();
    for set in sets {
        let mut sorted_set_for_key = set.clone();
        sorted_set_for_key.sort();
        let set_key = sorted_set_for_key.join(" ");
        if unique_sets_str.insert(set_key) {
            unique_sets_vec.push(set);
        }
    }
    Ok(unique_sets_vec)
}

fn run_cargo_check_with_features(
    feature_args: &[String],
    feature_desc: &str,
) -> Result<(Vec<DisplayableDiagnostic>, HashSet<PathBuf>, HashMap<PathBuf, HashSet<DiagnosticOriginInfo>>), Box<dyn std::error::Error>> {
    let mut command = Command::new("cargo");
    command.arg("check").arg("--message-format=json");
    command.args(feature_args);

    let cargo_output = command.stdout(Stdio::piped()).stderr(Stdio::piped()).output()?;

    if !cargo_output.stderr.is_empty() {
        let stderr_text = String::from_utf8_lossy(&cargo_output.stderr);
        if !stderr_text.trim().is_empty() && stderr_text.contains("error:") {
            eprintln!("[getdoc] Cargo command stderr (for features '{}'):\n{}", feature_args.join(" "), stderr_text);
        }
    }

    let mut displayable_diagnostics: Vec<DisplayableDiagnostic> = Vec::new();
    let mut implicated_files_this_run: HashSet<PathBuf> = HashSet::new();
    let mut referencers_this_run: HashMap<PathBuf, HashSet<DiagnosticOriginInfo>> = HashMap::new();

    let current_dir = std::env::current_dir()?;
    let cargo_home_dir = home::cargo_home().ok();
    let stdout_str = String::from_utf8_lossy(&cargo_output.stdout);

    for line in stdout_str.lines() {
        if line.trim().is_empty() || !line.starts_with('{') { continue; }
        match serde_json::from_str::<TopLevelCargoMessage>(line) {
            Ok(top_level_msg) => {
                if top_level_msg.reason == "compiler-message" {
                    if let Some(diag_data) = top_level_msg.message {
                        process_single_diagnostic_data(
                            &diag_data,
                            &mut displayable_diagnostics,
                            &mut implicated_files_this_run,
                            &mut referencers_this_run,
                            &current_dir,
                            &cargo_home_dir,
                            feature_desc,
                        );
                    }
                }
            }
            Err(_e) => { /* Optional: eprintln! for debug */ }
        }
    }
    Ok((displayable_diagnostics, implicated_files_this_run, referencers_this_run))
}

fn process_single_diagnostic_data(
    diag_data: &RustcDiagnosticData,
    displayable_diagnostics: &mut Vec<DisplayableDiagnostic>,
    implicated_files_overall_run: &mut HashSet<PathBuf>,
    referencers_for_run: &mut HashMap<PathBuf, HashSet<DiagnosticOriginInfo>>,
    current_dir: &Path,
    cargo_home_dir: &Option<PathBuf>,
    feature_desc: &str,
) {
    let mut current_diag_implicated_tp_files_details = Vec::new();
    let mut primary_location_of_this_diagnostic: Option<String> = None;

    // First, find the primary location of this diagnostic itself
    for span in &diag_data.spans {
        if span.is_primary {
            let path_obj = PathBuf::from(&span.file_name);
            let display_path = if path_obj.is_absolute() {
                path_obj.strip_prefix(current_dir).unwrap_or(&path_obj).to_path_buf()
            } else {
                path_obj.clone() // Relative paths in diagnostics are usually relative to project root
            };
            primary_location_of_this_diagnostic = Some(format!("{}:{}", display_path.display(), span.line_start));
            break; // Take the first primary span
        }
    }
    // Fallback if no primary span
    if primary_location_of_this_diagnostic.is_none() && !diag_data.spans.is_empty() {
        let first_span = &diag_data.spans[0];
        let path_obj = PathBuf::from(&first_span.file_name);
        let display_path = if path_obj.is_absolute() {
                path_obj.strip_prefix(current_dir).unwrap_or(&path_obj).to_path_buf()
            } else {
                path_obj.clone()
            };
        primary_location_of_this_diagnostic = Some(format!("{}:{} (non-primary)", display_path.display(), first_span.line_start));
    }
    let final_primary_loc_str = primary_location_of_this_diagnostic.clone().unwrap_or_else(|| "Unknown diagnostic location".to_string());


    // Now, identify implicated third-party files and create origin info
    for span in &diag_data.spans {
        let path_obj = PathBuf::from(&span.file_name);
        let absolute_path = if path_obj.is_absolute() { path_obj.clone() } else { current_dir.join(&path_obj) };

        if let Ok(canonical_path) = fs::canonicalize(&absolute_path) {
            if !canonical_path.starts_with(current_dir) { // A third-party file
                let is_in_cargo_registry = cargo_home_dir.as_ref().map_or(false, |ch| canonical_path.starts_with(&ch.join("registry").join("src")));
                let is_in_cargo_git = cargo_home_dir.as_ref().map_or(false, |ch| canonical_path.starts_with(&ch.join("git").join("checkouts")));

                if (is_in_cargo_registry || is_in_cargo_git) && canonical_path.is_file() {
                    let tp_file_detail = format!("{}:{}", canonical_path.file_name().unwrap_or_default().to_string_lossy(), span.line_start);
                    if !current_diag_implicated_tp_files_details.iter().any(|(p, _)| p == &canonical_path) { // Avoid duplicate Paths, details might differ
                        current_diag_implicated_tp_files_details.push((canonical_path.clone(), tp_file_detail));
                    }
                    implicated_files_overall_run.insert(canonical_path.clone());

                    let origin_info = DiagnosticOriginInfo {
                        level: diag_data.level.clone(),
                        code: diag_data.code.as_ref().map(|c| c.code.clone()),
                        originating_diagnostic_span_location: final_primary_loc_str.clone(),
                        feature_set_desc: feature_desc.to_string(),
                    };
                    referencers_for_run.entry(canonical_path).or_default().insert(origin_info);
                }
            }
        }
    }

    if diag_data.level == "error" || diag_data.level == "warning" {
        if let Some(rendered) = &diag_data.rendered {
            if !rendered.trim().is_empty() {
                displayable_diagnostics.push(DisplayableDiagnostic {
                    level: diag_data.level.clone(),
                    code: diag_data.code.as_ref().map(|c| c.code.clone()),
                    rendered: rendered.trim_end().to_string(),
                    implicated_third_party_files_details: current_diag_implicated_tp_files_details,
                    primary_location_of_diagnostic: final_primary_loc_str.clone(),
                });
            }
        }
    }

    for child in &diag_data.children {
        process_single_diagnostic_data(
            child,
            displayable_diagnostics,
            implicated_files_overall_run,
            referencers_for_run,
            current_dir,
            cargo_home_dir,
            feature_desc,
        );
    }
}

fn extract_items_from_file(file_path: &PathBuf) -> Result<Vec<ExtractedItem>, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(file_path)?;
    let ast = syn::parse_file(&content)?;
    let mut items = Vec::new();

    for item_syn in ast.items {
        let docs = match &item_syn {
            syn::Item::Fn(i) => extract_doc_comments(&i.attrs),
            syn::Item::Struct(i) => extract_doc_comments(&i.attrs),
            syn::Item::Enum(i) => extract_doc_comments(&i.attrs),
            syn::Item::Trait(i) => extract_doc_comments(&i.attrs),
            syn::Item::Mod(i) => extract_doc_comments(&i.attrs),
            syn::Item::Impl(i) => extract_doc_comments(&i.attrs),
            syn::Item::Type(i) => extract_doc_comments(&i.attrs),
            syn::Item::Const(i) => extract_doc_comments(&i.attrs),
            syn::Item::Static(i) => extract_doc_comments(&i.attrs),
            _ => Vec::new(),
        };

        let (item_kind_str, name_str, sig_def_str) = match &item_syn {
            syn::Item::Fn(item_fn) => {
                let vis_string = item_fn.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let sig = format!("{}{}", vis_prefix, item_fn.sig.to_token_stream().to_string());
                ("Function".to_string(), item_fn.sig.ident.to_string(), sig)
            }
            syn::Item::Struct(item_struct) => {
                let vis_string = item_struct.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}struct {}{}", vis_prefix, item_struct.ident.to_token_stream().to_string(), item_struct.generics.to_token_stream().to_string());
                ("Struct".to_string(), item_struct.ident.to_string(), def)
            }
            syn::Item::Enum(item_enum) => {
                let vis_string = item_enum.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}enum {}{}", vis_prefix, item_enum.ident.to_token_stream().to_string(), item_enum.generics.to_token_stream().to_string());
                ("Enum".to_string(), item_enum.ident.to_string(), def)
            }
            syn::Item::Trait(item_trait) => {
                let vis_string = item_trait.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}trait {}{}{}",
                    vis_prefix,
                    item_trait.ident.to_token_stream().to_string(),
                    item_trait.generics.params.to_token_stream().to_string(),
                    item_trait.generics.where_clause.as_ref().map_or("".to_string(), |wc| format!(" {}", wc.to_token_stream().to_string()))
                );
                ("Trait".to_string(), item_trait.ident.to_string(), def)
            }
            syn::Item::Mod(item_mod) => {
                if item_mod.content.is_none() && docs.is_empty() { continue; } 
                let vis_string = item_mod.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}mod {}", vis_prefix, item_mod.ident.to_token_stream().to_string());
                ("Module".to_string(), item_mod.ident.to_string(), def)
            }
            syn::Item::Impl(item_impl) => {
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
                    impl_line_tokens.extend(quote::quote! { });
                }
                item_impl.self_ty.to_tokens(&mut impl_line_tokens);
                name_parts.push(item_impl.self_ty.to_token_stream().to_string());
                
                if let Some(where_clause) = &item_impl.generics.where_clause {
                    impl_line_tokens.extend(quote::quote! { });
                    where_clause.to_tokens(&mut impl_line_tokens);
                }
                
                let name = if item_impl.trait_.is_none() {
                    item_impl.self_ty.to_token_stream().to_string()
                } else {
                    format!("impl {}", name_parts.join(" "))
                };
                let item_kind = if item_impl.trait_.is_some() { "Trait Impl Block".to_string() } else { "Inherent Impl Block".to_string() };
                (item_kind, name, impl_line_tokens.to_string())
            }
            syn::Item::Type(item_type) => {
                if docs.is_empty() { continue; }
                let vis_string = item_type.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}type {}{} = {};",
                    vis_prefix,
                    item_type.ident.to_token_stream().to_string(),
                    item_type.generics.to_token_stream().to_string(),
                    item_type.ty.to_token_stream().to_string()
                );
                ("Type Alias".to_string(), item_type.ident.to_string(), def)
            }
            syn::Item::Const(item_const) => {
                if docs.is_empty() { continue; }
                let vis_string = item_const.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}const {}: {} = ...;",
                    vis_prefix,
                    item_const.ident.to_token_stream().to_string(),
                    item_const.ty.to_token_stream().to_string()
                );
                ("Constant".to_string(), item_const.ident.to_string(), def)
            }
            syn::Item::Static(item_static) => {
                if docs.is_empty() { continue; }
                let vis_string = item_static.vis.to_token_stream().to_string();
                let vis_prefix = if vis_string.is_empty() { "".to_string() } else { format!("{} ", vis_string.trim_end()) };
                let def = format!("{}static {}: {} = ...;",
                    vis_prefix,
                    item_static.ident.to_token_stream().to_string(),
                    item_static.ty.to_token_stream().to_string()
                );
                ("Static".to_string(), item_static.ident.to_string(), def)
            }
            _ => continue,
        };
        items.push(ExtractedItem {
            item_kind: item_kind_str,
            name: name_str,
            signature_or_definition: sig_def_str.trim().to_string(),
            doc_comments: docs,
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
    all_compiler_diagnostics: &[(String, Vec<DisplayableDiagnostic>)],
    extracted_data: &HashMap<PathBuf, Vec<ExtractedItem>>,
    sorted_file_paths: &[PathBuf],
    file_referencers: &HashMap<PathBuf, HashSet<DiagnosticOriginInfo>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = BufWriter::new(File::create("report.md")?);

    writeln!(writer, "# GetDoc Report - {}", Local::now().to_rfc2822())?;

    writeln!(writer, "\n## Compiler Output (Errors and Warnings)\n")?;
    if all_compiler_diagnostics.is_empty() || all_compiler_diagnostics.iter().all(|(_, diags)| diags.is_empty()) {
        writeln!(writer, "```text\nNo errors or warnings reported by the compiler across checked feature configurations, or none implicated third-party files.\n```\n")?;
    } else {
        for (feature_desc, diagnostics) in all_compiler_diagnostics {
            if !diagnostics.is_empty() {
                writeln!(writer, "### Diagnostics for: {}\n", feature_desc)?;
                writeln!(writer, "```text")?;
                for diag_disp in diagnostics {
                    writeln!(writer, "{}{}",
                        diag_disp.code.as_ref().map_or_else(
                            || format!("{}: ", diag_disp.level.to_uppercase()),
                            |c| format!("{}: {}: ", diag_disp.level.to_uppercase(), c)
                        ),
                        diag_disp.rendered
                    )?;
                    writeln!(writer, "    (Diagnostic primary location: {})", diag_disp.primary_location_of_diagnostic)?;
                    if !diag_disp.implicated_third_party_files_details.is_empty() {
                        let file_list = diag_disp.implicated_third_party_files_details.iter()
                            .map(|(p, detail_loc)| format!("`{}` (at `{}`)", p.file_name().unwrap_or_default().to_string_lossy(), detail_loc))
                            .collect::<Vec<String>>().join(", ");
                        writeln!(writer, "    (Implicates: {} - see details below if extracted)", file_list)?;
                    }
                }
                writeln!(writer, "```\n")?;
            }
        }
    }

    if extracted_data.is_empty() {
        writeln!(writer, "No third-party crate information extracted (or no third-party files were implicated).")?;
    } else {
        for file_path in sorted_file_paths {
            if let Some(items) = extracted_data.get(file_path) {
                writeln!(writer, "---\n## From File: `{}`\n", file_path.display())?;

                if let Some(origins) = file_referencers.get(file_path) {
                    if !origins.is_empty() {
                        writeln!(writer, "**Referenced by:**")?;
                        let mut sorted_origins: Vec<_> = origins.iter().collect();
                        sorted_origins.sort();
                        for origin in sorted_origins {
                            writeln!(writer, "* {} {} (originating at `{}` from configuration: `{}`)",
                                origin.level.to_uppercase(),
                                origin.code.as_deref().unwrap_or("N/A"),
                                origin.originating_diagnostic_span_location, // Changed field name
                                origin.feature_set_desc
                            )?;
                        }
                        writeln!(writer)?;
                    }
                }

                if items.is_empty() {
                    writeln!(writer, "_No extractable items (functions, structs, etc. meeting criteria) found or processed in this file._\n")?;
                    continue;
                }
                for item in items {
                    let item_header_name = if item.item_kind.contains("Impl Block") && item.name.starts_with("impl ") {
                         item.signature_or_definition.split('{').next().unwrap_or(&item.name).trim()
                    } else if item.item_kind == "Module" && item.name.is_empty() {
                        "Unnamed Module"
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
