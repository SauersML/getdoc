// getdoc - main.rs

// --- Standard Library Imports ---
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// --- External Crate Imports ---
use chrono::Local;
use clap::Parser; // For parsing command-line arguments
use home;
use quote::ToTokens;
use serde::Deserialize;
use syn;
use toml;

// --- CLI Argument Definitions ---

/// A Rust developer tool to provide source code context with compiler errors,
/// especially from third-party crates, across various feature flag combinations.
#[derive(clap::Parser, Debug)] // Use fully qualified path for the derive macro
#[clap(author, version, about, long_about = None)]
struct CliArgs {
    /// Comma-separated list of specific crate features to focus the analysis on.
    /// If provided, `getdoc` runs in "Targeted Mode", checking combinations
    /// relevant to these features within the current environment.
    /// If omitted, `getdoc` runs in "Comprehensive Mode", checking a broader
    /// set of feature combinations (default, no-default, all-features, etc.).
    #[clap(long, value_parser, value_delimiter = ',')]
    features: Option<Vec<String>>,
}

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
    explanation: Option<String>,
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
    originating_diagnostic_span_location: String,
    feature_set_desc: String,
}

#[derive(Debug)]
struct DisplayableDiagnostic {
    level: String,
    code: Option<String>,
    code_explanation: Option<String>,
    rendered: String,
    primary_location_of_diagnostic: String,
    implicated_third_party_files_details: Vec<(PathBuf, String)>, // Contains (CanonicalPath, "filename:line")
}

#[derive(Debug)]
struct ExtractedItem {
    item_kind: String, // e.g., "Function", "Struct", "Impl Method"
    name: String,
    signature_or_definition: String,
    doc_comments: Vec<String>,
    is_sub_item: bool,
}

// --- Structs for Consolidated Diagnostics ---

/// A key to uniquely identify a specific diagnostic instance.
/// Uniqueness is determined by the error level, code, primary location,
/// the full rendered message, and a signature of implicated third-party files.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct DiagnosticInstanceKey {
    level: String,
    code: Option<String>,
    primary_location: String,
    rendered_message: String,
    implicated_files_signature: String, // A sorted, concatenated string of implicated file paths and their detail strings
}

/// Represents a diagnostic instance that has been consolidated.
/// It holds the common information for the diagnostic and a set of all
/// feature sets under which this exact instance occurred.
#[derive(Debug, Clone)]
struct AggregatedDiagnosticInstance {
    level: String,
    code: Option<String>,
    rendered_message: String,
    primary_location: String,
    // Note: The 'code_explanation' field was removed as generic explanations
    // are now handled globally and stored in the 'unique_explanations' map
    // for the report appendix.
    implicated_third_party_files_details: Vec<(PathBuf, String)>,
    feature_set_descriptors: HashSet<String>, // Feature sets that produced this exact diagnostic
}

impl AggregatedDiagnosticInstance {
    /// Creates a new AggregatedDiagnosticInstance from a DisplayableDiagnostic and a feature set.
    fn new(diag_disp: &DisplayableDiagnostic, feature_desc: &str) -> Self {
        Self {
            level: diag_disp.level.clone(),
            code: diag_disp.code.clone(),
            rendered_message: diag_disp.rendered.clone(),
            primary_location: diag_disp.primary_location_of_diagnostic.clone(),
            implicated_third_party_files_details: diag_disp.implicated_third_party_files_details.clone(),
            feature_set_descriptors: {
                let mut set = HashSet::new();
                set.insert(feature_desc.to_string());
                set
            },
        }
    }
}

impl DisplayableDiagnostic {
    /// Creates a stable string signature of implicated third-party files for keying.
    /// The signature is a sorted list of "canonicalized_path_string:detail_location_string" strings, joined by ';'.
    fn get_implicated_files_signature(&self) -> String {
        let mut signature_parts: Vec<String> = self
            .implicated_third_party_files_details
            .iter()
            .map(|(path, detail_loc)| format!("{}:{}", path.to_string_lossy(), detail_loc))
            .collect();
        // Sorting here again for stability even if the source Vec wasn't pre-sorted,
        // though pre-sorting in process_single_diagnostic_data is preferred.
        signature_parts.sort();
        signature_parts.join(";")
    }
}

// --- Main Function ---

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse command-line arguments
    let cli_args = CliArgs::parse();

    // Determine the mode of operation based on CLI arguments
    if cli_args.features.is_some() {
        println!("[getdoc] Starting analysis in Targeted Mode for specified features...");
    } else {
        println!("[getdoc] Starting analysis in Comprehensive Mode for multiple feature sets...");
    }

    let feature_sets_to_check = get_feature_sets_to_check(cli_args.features.as_ref()).unwrap_or_else(|e| {
        eprintln!("[getdoc] Warning: Could not determine feature sets: {}. Proceeding with a minimal check.", e);
        if let Some(target_feats) = cli_args.features.as_ref() {
            if target_feats.is_empty() {
                vec![vec![]] 
            } else {
                vec![vec!["--features".to_string(), target_feats.join(",")]]
            }
        } else {
            vec![vec![]] 
        }
    });

    let mut all_displayable_diagnostics: Vec<(String, Vec<DisplayableDiagnostic>)> = Vec::new();
    let mut all_implicated_files_globally: HashSet<PathBuf> = HashSet::new();
    let mut global_file_referencers: HashMap<PathBuf, HashSet<DiagnosticOriginInfo>> =
        HashMap::new();

    for feature_args in &feature_sets_to_check {
        let feature_desc = if feature_args.is_empty() {
            "default features".to_string()
        } else {
            feature_args.join(" ")
        };
        println!(
            "[getdoc] Running `cargo check --message-format=json {}`...",
            feature_desc
        );

        match run_cargo_check_with_features(feature_args, &feature_desc) {
            Ok((diagnostics_for_run, implicated_files_for_run, referencers_for_run)) => {
                if !diagnostics_for_run.is_empty() {
                    all_displayable_diagnostics.push((feature_desc.clone(), diagnostics_for_run));
                }
                all_implicated_files_globally.extend(implicated_files_for_run);
                for (file, origins) in referencers_for_run {
                    global_file_referencers
                        .entry(file)
                        .or_default()
                        .extend(origins);
                }
            }
            Err(e) => {
                let error_message = format!(
                    "Error running cargo check with configuration '{}': {}",
                    feature_desc, e
                );
                eprintln!("[getdoc] {}", error_message);
                all_displayable_diagnostics.push((
                    feature_desc.clone(),
                    vec![DisplayableDiagnostic {
                        level: "TOOL_ERROR".to_string(),
                        code: None,
                        code_explanation: None,
                        rendered: error_message,
                        primary_location_of_diagnostic: "N/A".to_string(),
                        implicated_third_party_files_details: vec![],
                    }],
                ));
            }
        }
    }

    // Determine mode description once for potential use in minimal report
    let mode_description_for_report = match cli_args.features.as_ref() {
        Some(features_vec) if !features_vec.is_empty() => {
            format!("Targeted Mode for Features: `{}`", features_vec.join(", "))
        }
        Some(_) => "Targeted Mode (Context specified, using crate defaults)".to_string(),
        None => "Comprehensive Mode".to_string(),
    };

    if all_displayable_diagnostics
        .iter()
        .all(|(_, diags)| diags.is_empty())
        && all_implicated_files_globally.is_empty()
    {
        println!(
            "[getdoc] No relevant compiler messages found or no third-party files implicated across all feature checks. Exiting."
        );
        let mut report_writer = BufWriter::new(File::create("report.md")?);
        writeln!(
            report_writer,
            "# GetDoc Report - {} - {}",
            mode_description_for_report, // Use determined mode description
            Local::now().to_rfc2822()
        )?;
        writeln!(
            report_writer,
            "\n## Compiler Output (Errors and Warnings)\n\n```text\nNo errors or warnings reported by the compiler across checked feature configurations, or none implicated third-party files.\n```"
        )?;
        println!("[getdoc] Minimal report generated: report.md");
        return Ok(());
    }

    // --- Consolidate Diagnostics and Collect Explanations ---
    let mut consolidated_diagnostic_instances: HashMap<
        DiagnosticInstanceKey,
        AggregatedDiagnosticInstance,
    > = HashMap::new();
    let mut unique_explanations: HashMap<String, String> = HashMap::new();

    for (feature_desc, diagnostics_for_run) in &all_displayable_diagnostics {
        for diag_disp in diagnostics_for_run {
            if let (Some(code), Some(explanation)) = (&diag_disp.code, &diag_disp.code_explanation)
            {
                if !explanation.trim().is_empty() {
                    unique_explanations
                        .entry(code.clone())
                        .or_insert_with(|| explanation.clone());
                }
            }

            let key = DiagnosticInstanceKey {
                level: diag_disp.level.clone(),
                code: diag_disp.code.clone(),
                primary_location: diag_disp.primary_location_of_diagnostic.clone(),
                rendered_message: diag_disp.rendered.clone(),
                implicated_files_signature: diag_disp.get_implicated_files_signature(),
            };

            let agg_diag_entry = consolidated_diagnostic_instances
                .entry(key)
                .or_insert_with(|| AggregatedDiagnosticInstance::new(diag_disp, feature_desc));

            agg_diag_entry
                .feature_set_descriptors
                .insert(feature_desc.clone());
        }
    }

    let mut sorted_consolidated_diagnostics: Vec<AggregatedDiagnosticInstance> =
        consolidated_diagnostic_instances.into_values().collect();
    sorted_consolidated_diagnostics.sort_by(|a, b| {
        a.primary_location
            .cmp(&b.primary_location)
            .then_with(|| a.code.cmp(&b.code))
            .then_with(|| a.rendered_message.cmp(&b.rendered_message))
    });

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
                    println!(
                        "[getdoc] No extractable items (meeting criteria) found in: {}",
                        file_path.display()
                    );
                }
            }
            Err(e) => eprintln!(
                "[getdoc] Warning: Could not process file {}: {}",
                file_path.display(),
                e
            ),
        }
    }

    generate_markdown_report(
        &sorted_consolidated_diagnostics,
        &unique_explanations,
        &extracted_data,
        &sorted_file_paths,
        &global_file_referencers,
        cli_args.features.as_ref(),
    )?;

    println!("[getdoc] Analysis complete. Report generated: report.md");
    Ok(())
}

// --- Helper Functions ---

/// Determines the sets of feature arguments to pass to `cargo check`.
fn get_feature_sets_to_check(
    context_features: Option<&Vec<String>>,
) -> Result<Vec<Vec<String>>, Box<dyn std::error::Error>> {
    let mut sets: Vec<Vec<String>> = Vec::new();

    if let Some(targets) = context_features {
        println!(
            "[getdoc] Determining feature checks for Targeted Mode (context: {:?})",
            targets
        );
        if targets.is_empty() {
            println!(
                "[getdoc] Targeted features list is empty. Checking with crate default features only."
            );
            sets.push(vec![]);
        } else {
            let features_arg_string = targets.join(",");
            sets.push(vec!["--features".to_string(), features_arg_string.clone()]);
            sets.push(vec![
                "--no-default-features".to_string(),
                "--features".to_string(),
                features_arg_string.clone(),
            ]);
            sets.push(vec![]);
        }
    } else {
        println!("[getdoc] Determining feature checks for Comprehensive Mode.");
        sets.push(vec![]);

        let cargo_toml_path = PathBuf::from("Cargo.toml");
        if cargo_toml_path.exists() {
            match fs::read_to_string(&cargo_toml_path) {
                Ok(cargo_toml_content) => {
                    let parsed_toml: CargoToml =
                        toml::from_str(&cargo_toml_content).unwrap_or_else(|e| {
                            eprintln!("[getdoc] Warning: Failed to parse Cargo.toml: {}. Assuming no custom features.", e);
                            CargoToml::default()
                        });

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
                }
                Err(e) => {
                    eprintln!(
                        "[getdoc] Warning: Could not read Cargo.toml at {:?}: {}. Proceeding with default features check only.",
                        cargo_toml_path, e
                    );
                }
            }
        } else {
            println!(
                "[getdoc] Warning: Cargo.toml not found in current directory. Only checking with default features."
            );
        }
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
) -> Result<
    (
        Vec<DisplayableDiagnostic>,
        HashSet<PathBuf>,
        HashMap<PathBuf, HashSet<DiagnosticOriginInfo>>,
    ),
    Box<dyn std::error::Error>,
> {
    let mut command = Command::new("cargo");
    command.arg("check").arg("--message-format=json");
    command.args(feature_args);

    let cargo_output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !cargo_output.stderr.is_empty() {
        let stderr_text = String::from_utf8_lossy(&cargo_output.stderr);
        if !stderr_text.trim().is_empty() && stderr_text.contains("error:") {
            eprintln!(
                "[getdoc] Cargo command stderr (for features '{}'):\n{}",
                feature_args.join(" "),
                stderr_text
            );
        }
    }

    let mut displayable_diagnostics: Vec<DisplayableDiagnostic> = Vec::new();
    let mut implicated_files_this_run: HashSet<PathBuf> = HashSet::new();
    let mut referencers_this_run: HashMap<PathBuf, HashSet<DiagnosticOriginInfo>> = HashMap::new();

    let current_dir = std::env::current_dir()?;
    let cargo_home_dir = home::cargo_home().ok();
    let stdout_str = String::from_utf8_lossy(&cargo_output.stdout);

    for line in stdout_str.lines() {
        if line.trim().is_empty() || !line.starts_with('{') {
            continue;
        }
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
            Err(_e) => { /* Silently ignore malformed JSON lines */ }
        }
    }
    Ok((
        displayable_diagnostics,
        implicated_files_this_run,
        referencers_this_run,
    ))
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
    let mut current_diag_implicated_tp_files_details: Vec<(PathBuf, String)> = Vec::new();
    let mut primary_location_of_this_diagnostic: Option<String> = None;

    for span in &diag_data.spans {
        if span.is_primary {
            let path_obj = PathBuf::from(&span.file_name);
            let display_path = if path_obj.is_absolute() {
                path_obj
                    .strip_prefix(current_dir)
                    .unwrap_or(&path_obj)
                    .to_path_buf()
            } else {
                path_obj.clone()
            };
            primary_location_of_this_diagnostic =
                Some(format!("{}:{}", display_path.display(), span.line_start));
            break;
        }
    }
    if primary_location_of_this_diagnostic.is_none() && !diag_data.spans.is_empty() {
        let first_span = &diag_data.spans[0];
        let path_obj = PathBuf::from(&first_span.file_name);
        let display_path = if path_obj.is_absolute() {
            path_obj
                .strip_prefix(current_dir)
                .unwrap_or(&path_obj)
                .to_path_buf()
        } else {
            path_obj.clone()
        };
        primary_location_of_this_diagnostic = Some(format!(
            "{}:{} (non-primary)",
            display_path.display(),
            first_span.line_start
        ));
    }
    let final_primary_loc_str = primary_location_of_this_diagnostic
        .clone()
        .unwrap_or_else(|| "Unknown diagnostic location".to_string());

    for span in &diag_data.spans {
        let path_obj = PathBuf::from(&span.file_name);
        let absolute_path = if path_obj.is_absolute() {
            path_obj.clone()
        } else {
            current_dir.join(&path_obj)
        };

        if let Ok(canonical_path) = fs::canonicalize(&absolute_path) {
            if !canonical_path.starts_with(current_dir) {
                let is_in_cargo_registry = cargo_home_dir.as_ref().map_or(false, |ch| {
                    canonical_path.starts_with(&ch.join("registry").join("src"))
                });
                let is_in_cargo_git = cargo_home_dir.as_ref().map_or(false, |ch| {
                    canonical_path.starts_with(&ch.join("git").join("checkouts"))
                });

                if (is_in_cargo_registry || is_in_cargo_git) && canonical_path.is_file() {
                    let tp_file_name = canonical_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    let tp_file_detail = format!("{}:{}", tp_file_name, span.line_start);

                    // Make sure each (canonical_path, detail_string) pair is unique before adding
                    if !current_diag_implicated_tp_files_details
                        .iter()
                        .any(|(p, d)| p == &canonical_path && d == &tp_file_detail)
                    {
                        current_diag_implicated_tp_files_details
                            .push((canonical_path.clone(), tp_file_detail));
                    }
                    implicated_files_overall_run.insert(canonical_path.clone());

                    let origin_info = DiagnosticOriginInfo {
                        level: diag_data.level.clone(),
                        code: diag_data.code.as_ref().map(|c| c.code.clone()),
                        originating_diagnostic_span_location: final_primary_loc_str.clone(),
                        feature_set_desc: feature_desc.to_string(),
                    };
                    referencers_for_run
                        .entry(canonical_path)
                        .or_default()
                        .insert(origin_info);
                }
            }
        }
    }
    // Sort details for consistent signature generation in DisplayableDiagnostic.get_implicated_files_signature
    current_diag_implicated_tp_files_details
        .sort_by(|(p1, d1), (p2, d2)| p1.cmp(p2).then_with(|| d1.cmp(d2)));

    if diag_data.level == "error" || diag_data.level == "warning" {
        if let Some(rendered) = &diag_data.rendered {
            if !rendered.trim().is_empty() {
                let item_code = diag_data.code.as_ref().map(|c| c.code.clone());
                let item_code_explanation =
                    diag_data.code.as_ref().and_then(|c| c.explanation.clone());

                displayable_diagnostics.push(DisplayableDiagnostic {
                    level: diag_data.level.clone(),
                    code: item_code,
                    code_explanation: item_code_explanation,
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

fn extract_items_from_file(
    file_path: &PathBuf,
) -> Result<Vec<ExtractedItem>, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(file_path)?;
    let ast = syn::parse_file(&content)?;
    let mut items = Vec::new();

    for item_syn in ast.items {
        let top_level_docs = match &item_syn {
            syn::Item::Fn(i) => extract_doc_comments(&i.attrs),
            syn::Item::Struct(i) => extract_doc_comments(&i.attrs),
            syn::Item::Enum(i) => extract_doc_comments(&i.attrs),
            syn::Item::Trait(i) => extract_doc_comments(&i.attrs),
            syn::Item::Mod(i) => extract_doc_comments(&i.attrs),
            syn::Item::Impl(i) => extract_doc_comments(&i.attrs),
            syn::Item::Type(i) => extract_doc_comments(&i.attrs),
            syn::Item::Const(i) => extract_doc_comments(&i.attrs),
            syn::Item::Static(i) => extract_doc_comments(&i.attrs),
            syn::Item::Use(i) => extract_doc_comments(&i.attrs),
            syn::Item::ExternCrate(i) => extract_doc_comments(&i.attrs),
            _ => Vec::new(),
        };
        process_item_syn(&item_syn, top_level_docs, &mut items);
    }
    Ok(items)
}

fn process_item_syn(item_syn: &syn::Item, docs: Vec<String>, items: &mut Vec<ExtractedItem>) {
    match item_syn {
        syn::Item::Fn(item_fn) => {
            let vis_string = item_fn.vis.to_token_stream().to_string();
            let vis_prefix = if vis_string.is_empty() {
                "".to_string()
            } else {
                format!("{} ", vis_string.trim_end())
            };
            let sig = format!(
                "{}{}",
                vis_prefix,
                item_fn.sig.to_token_stream().to_string()
            );
            items.push(ExtractedItem {
                item_kind: "Function".to_string(),
                name: item_fn.sig.ident.to_string(),
                signature_or_definition: sig.trim().to_string(),
                doc_comments: docs,
                is_sub_item: false,
            });
        }
        syn::Item::Struct(item_struct) => {
            let vis_string = item_struct.vis.to_token_stream().to_string();
            let vis_prefix = if vis_string.is_empty() {
                "".to_string()
            } else {
                format!("{} ", vis_string.trim_end())
            };
            let def = format!(
                "{}struct {}{}",
                vis_prefix,
                item_struct.ident.to_token_stream().to_string(),
                item_struct.generics.to_token_stream().to_string()
            );
            items.push(ExtractedItem {
                item_kind: "Struct".to_string(),
                name: item_struct.ident.to_string(),
                signature_or_definition: def.trim().to_string(),
                doc_comments: docs,
                is_sub_item: false,
            });
        }
        syn::Item::Enum(item_enum) => {
            let vis_string = item_enum.vis.to_token_stream().to_string();
            let vis_prefix = if vis_string.is_empty() {
                "".to_string()
            } else {
                format!("{} ", vis_string.trim_end())
            };
            let def = format!(
                "{}enum {}{}",
                vis_prefix,
                item_enum.ident.to_token_stream().to_string(),
                item_enum.generics.to_token_stream().to_string()
            );
            items.push(ExtractedItem {
                item_kind: "Enum".to_string(),
                name: item_enum.ident.to_string(),
                signature_or_definition: def.trim().to_string(),
                doc_comments: docs,
                is_sub_item: false,
            });
        }
        syn::Item::Trait(item_trait) => {
            let vis_string = item_trait.vis.to_token_stream().to_string();
            let vis_prefix = if vis_string.is_empty() {
                "".to_string()
            } else {
                format!("{} ", vis_string.trim_end())
            };
            let def = format!(
                "{}trait {}{}{}",
                vis_prefix,
                item_trait.ident.to_token_stream().to_string(),
                item_trait.generics.params.to_token_stream().to_string(),
                item_trait
                    .generics
                    .where_clause
                    .as_ref()
                    .map_or("".to_string(), |wc| format!(
                        " {}",
                        wc.to_token_stream().to_string()
                    ))
            );
            items.push(ExtractedItem {
                item_kind: "Trait".to_string(),
                name: item_trait.ident.to_string(),
                signature_or_definition: def.trim().to_string(),
                doc_comments: docs,
                is_sub_item: false,
            });
        }
        syn::Item::Mod(item_mod) => {
            if item_mod.content.is_none() && docs.is_empty() {
                return;
            }
            let vis_string = item_mod.vis.to_token_stream().to_string();
            let vis_prefix = if vis_string.is_empty() {
                "".to_string()
            } else {
                format!("{} ", vis_string.trim_end())
            };
            let mod_name_str = item_mod.ident.to_token_stream().to_string();
            let def = if item_mod.content.is_some() {
                format!("{}mod {} {{ /* ... */ }}", vis_prefix, mod_name_str)
            } else {
                format!("{}mod {};", vis_prefix, mod_name_str)
            };
            items.push(ExtractedItem {
                item_kind: "Module".to_string(),
                name: mod_name_str,
                signature_or_definition: def.trim().to_string(),
                doc_comments: docs,
                is_sub_item: false,
            });
        }
        syn::Item::Impl(item_impl) => {
            let mut impl_line_tokens = quote::quote! {};
            if let Some(defaultness) = &item_impl.defaultness {
                defaultness.to_tokens(&mut impl_line_tokens);
                impl_line_tokens.extend(quote::quote! {});
            }
            if let Some(unsafety) = &item_impl.unsafety {
                unsafety.to_tokens(&mut impl_line_tokens);
                impl_line_tokens.extend(quote::quote! {});
            }
            impl_line_tokens.extend(quote::quote! { impl });
            item_impl.generics.params.to_tokens(&mut impl_line_tokens);
            if !item_impl.generics.params.is_empty() {
                impl_line_tokens.extend(quote::quote! {});
            }

            let mut name_parts: Vec<String> = Vec::new();
            if let Some((opt_bang, trait_path, _for_keyword)) = &item_impl.trait_ {
                if opt_bang.is_some() {
                    impl_line_tokens.extend(quote::quote! { ! });
                }
                trait_path.to_tokens(&mut impl_line_tokens);
                name_parts.push(trait_path.to_token_stream().to_string().replace(' ', ""));
                impl_line_tokens.extend(quote::quote! { for });
                name_parts.push("for".to_string());
                impl_line_tokens.extend(quote::quote! {});
            }
            item_impl.self_ty.to_tokens(&mut impl_line_tokens);
            name_parts.push(
                item_impl
                    .self_ty
                    .to_token_stream()
                    .to_string()
                    .replace(' ', ""),
            );

            if let Some(where_clause) = &item_impl.generics.where_clause {
                impl_line_tokens.extend(quote::quote! {});
                where_clause.to_tokens(&mut impl_line_tokens);
            }

            let name = if item_impl.trait_.is_none() {
                item_impl
                    .self_ty
                    .to_token_stream()
                    .to_string()
                    .replace(' ', "")
            } else {
                format!("impl {}", name_parts.join(" "))
            };
            let item_kind_str = if item_impl.trait_.is_some() {
                "Trait Impl Block".to_string()
            } else {
                "Inherent Impl Block".to_string()
            };

            items.push(ExtractedItem {
                item_kind: item_kind_str,
                name,
                signature_or_definition: impl_line_tokens.to_string().trim().to_string(),
                doc_comments: docs.clone(),
                is_sub_item: false,
            });

            for impl_item_syn in &item_impl.items {
                let sub_docs = extract_doc_comments(match impl_item_syn {
                    syn::ImplItem::Const(item) => &item.attrs,
                    syn::ImplItem::Fn(item) => &item.attrs,
                    syn::ImplItem::Type(item) => &item.attrs,
                    syn::ImplItem::Macro(item) => &item.attrs,
                    _ => &[],
                });

                match impl_item_syn {
                    syn::ImplItem::Fn(impl_fn) => {
                        let vis_string = impl_fn.vis.to_token_stream().to_string();
                        let vis_prefix = if vis_string.is_empty() {
                            "".to_string()
                        } else {
                            format!("{} ", vis_string.trim_end())
                        };
                        let sig_def_str = format!(
                            "{}{};",
                            vis_prefix,
                            impl_fn.sig.to_token_stream().to_string()
                        );
                        items.push(ExtractedItem {
                            item_kind: "Impl Method".to_string(),
                            name: impl_fn.sig.ident.to_string(),
                            signature_or_definition: sig_def_str.trim().to_string(),
                            doc_comments: sub_docs,
                            is_sub_item: true,
                        });
                    }
                    syn::ImplItem::Const(impl_const) => {
                        let vis_string = impl_const.vis.to_token_stream().to_string();
                        let vis_prefix = if vis_string.is_empty() {
                            "".to_string()
                        } else {
                            format!("{} ", vis_string.trim_end())
                        };
                        let sig_def_str = format!(
                            "{}const {}: {} = ...;",
                            vis_prefix,
                            impl_const.ident.to_token_stream().to_string(),
                            impl_const.ty.to_token_stream().to_string()
                        );
                        items.push(ExtractedItem {
                            item_kind: "Impl Associated Constant".to_string(),
                            name: impl_const.ident.to_string(),
                            signature_or_definition: sig_def_str.trim().to_string(),
                            doc_comments: sub_docs,
                            is_sub_item: true,
                        });
                    }
                    syn::ImplItem::Type(impl_type) => {
                        let vis_string = impl_type.vis.to_token_stream().to_string();
                        let vis_prefix = if vis_string.is_empty() {
                            "".to_string()
                        } else {
                            format!("{} ", vis_string.trim_end())
                        };
                        let sig_def_str = format!(
                            "{}type {}{} = {};",
                            vis_prefix,
                            impl_type.ident.to_token_stream().to_string(),
                            impl_type.generics.to_token_stream().to_string(),
                            impl_type.ty.to_token_stream().to_string()
                        );
                        items.push(ExtractedItem {
                            item_kind: "Impl Associated Type".to_string(),
                            name: impl_type.ident.to_string(),
                            signature_or_definition: sig_def_str.trim().to_string(),
                            doc_comments: sub_docs,
                            is_sub_item: true,
                        });
                    }
                    syn::ImplItem::Macro(impl_macro) => {
                        let sig_def_str = impl_macro.mac.to_token_stream().to_string();
                        let name = impl_macro.mac.path.segments.last().map_or_else(
                            || "unknown_macro".to_string(),
                            |seg| seg.ident.to_string(),
                        );
                        items.push(ExtractedItem {
                            item_kind: "Impl Macro Invocation".to_string(),
                            name,
                            signature_or_definition: sig_def_str.trim().to_string(),
                            doc_comments: sub_docs,
                            is_sub_item: true,
                        });
                    }
                    _ => { /* Verbatim or other unhandled impl items */ }
                }
            }
        }
        syn::Item::Type(item_type) => {
            let vis_string = item_type.vis.to_token_stream().to_string();
            let vis_prefix = if vis_string.is_empty() {
                "".to_string()
            } else {
                format!("{} ", vis_string.trim_end())
            };
            let def = format!(
                "{}type {}{} = {};",
                vis_prefix,
                item_type.ident.to_token_stream().to_string(),
                item_type.generics.to_token_stream().to_string(),
                item_type.ty.to_token_stream().to_string()
            );
            items.push(ExtractedItem {
                item_kind: "Type Alias".to_string(),
                name: item_type.ident.to_string(),
                signature_or_definition: def.trim().to_string(),
                doc_comments: docs,
                is_sub_item: false,
            });
        }
        syn::Item::Const(item_const) => {
            let vis_string = item_const.vis.to_token_stream().to_string();
            let vis_prefix = if vis_string.is_empty() {
                "".to_string()
            } else {
                format!("{} ", vis_string.trim_end())
            };
            let def = format!(
                "{}const {}: {} = ...;",
                vis_prefix,
                item_const.ident.to_token_stream().to_string(),
                item_const.ty.to_token_stream().to_string()
            );
            items.push(ExtractedItem {
                item_kind: "Constant".to_string(),
                name: item_const.ident.to_string(),
                signature_or_definition: def.trim().to_string(),
                doc_comments: docs,
                is_sub_item: false,
            });
        }
        syn::Item::Static(item_static) => {
            let vis_string = item_static.vis.to_token_stream().to_string();
            let vis_prefix = if vis_string.is_empty() {
                "".to_string()
            } else {
                format!("{} ", vis_string.trim_end())
            };
            let def = format!(
                "{}static {}: {} = ...;",
                vis_prefix,
                item_static.ident.to_token_stream().to_string(),
                item_static.ty.to_token_stream().to_string()
            );
            items.push(ExtractedItem {
                item_kind: "Static".to_string(),
                name: item_static.ident.to_string(),
                signature_or_definition: def.trim().to_string(),
                doc_comments: docs,
                is_sub_item: false,
            });
        }
        syn::Item::ExternCrate(item_ec) => {
            let def = item_ec.to_token_stream().to_string();
            let name = if let Some(rename) = &item_ec.rename {
                rename.1.to_string()
            } else {
                item_ec.ident.to_string()
            };
            items.push(ExtractedItem {
                item_kind: "Extern Crate".to_string(),
                name,
                signature_or_definition: def.trim().to_string(),
                doc_comments: docs,
                is_sub_item: false,
            });
        }
        syn::Item::Use(item_use) => {
            let is_public = matches!(item_use.vis, syn::Visibility::Public(_));
            if docs.is_empty() && !is_public {
                return;
            }

            let def = item_use.to_token_stream().to_string();
            let name_str = item_use.tree.to_token_stream().to_string(); // Renamed from 'name' to avoid conflict
            let display_name = if name_str.chars().count() > 70 {
                name_str.chars().take(67).collect::<String>() + "..."
            } else {
                name_str
            };
            items.push(ExtractedItem {
                item_kind: "Use Statement".to_string(),
                name: display_name,
                signature_or_definition: def.trim().to_string(),
                doc_comments: docs,
                is_sub_item: false,
            });
        }
        _ => { /* Other item types are not processed */ }
    }
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
                    _ => { /* Other meta forms for `doc` (like lists or paths) are not standard doc comments */ }
                }
            }
            None
        })
        .collect()
}

fn item_header_name_logic(item: &ExtractedItem) -> String {
    if item.item_kind.contains("Impl Block") && item.name.starts_with("impl ") {
        // For impl blocks, the signature_or_definition usually contains the full impl line,
        // so take up to the first '{' or the whole name if no brace (should not happen for valid impls).
        item.signature_or_definition
            .split('{')
            .next()
            .unwrap_or(&item.name)
            .trim()
            .to_string()
    } else if item.item_kind == "Module" && item.name.is_empty() {
        "Unnamed Module".to_string() // Should be rare with syn parsing actual mods
    } else {
        item.name.clone()
    }
}

/// Generates a Markdown report from the analyzed diagnostics and extracted source code items.
/// Diagnostics are presented in a consolidated format, and error code explanations are globalized.
fn generate_markdown_report(
    // Consolidated and sorted diagnostic instances. Each instance represents a unique error/warning.
    consolidated_diagnostics: &[AggregatedDiagnosticInstance],
    // A collection of unique explanation texts, keyed by error code.
    unique_explanations: &HashMap<String, String>,
    // Data extracted from implicated third-party files.
    extracted_data: &HashMap<PathBuf, Vec<ExtractedItem>>,
    // Sorted list of paths to all implicated third-party files.
    sorted_file_paths: &[PathBuf],
    // Information about which diagnostics referenced which third-party files.
    file_referencers: &HashMap<PathBuf, HashSet<DiagnosticOriginInfo>>,
    // CLI-provided context features, used for the report header.
    context_features: Option<&Vec<String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = BufWriter::new(File::create("report.md")?);

    // --- Report Header ---
    let mode_description = match context_features {
        Some(features_vec) if !features_vec.is_empty() => {
            format!("Targeted Mode for Features: `{}`", features_vec.join(", "))
        }
        Some(_) => "Targeted Mode (Context specified, using crate defaults)".to_string(),
        None => "Comprehensive Mode".to_string(),
    };
    writeln!(
        writer,
        "# GetDoc Report - {} - {}",
        mode_description,
        Local::now().to_rfc2822()
    )?;
    writeln!(
        writer,
        "\nThis report consolidates identical diagnostic messages and centralizes error code explanations in an appendix."
    )?;

    // --- Section B: Consolidated Compiler Diagnostics ---
    writeln!(
        writer,
        "\n## Consolidated Compiler Diagnostics (Errors and Warnings)\n"
    )?;
    if consolidated_diagnostics.is_empty() {
        writeln!(
            writer,
            "```text\nNo relevant errors or warnings reported by the compiler across checked feature configurations, or none implicated third-party files.\n```\n"
        )?;
    } else {
        writeln!(writer, "```text")?;
        for agg_diag in consolidated_diagnostics {
            // Print the core diagnostic message (level, code, rendered text)
            writeln!(
                writer,
                "{}{}",
                agg_diag.code.as_ref().map_or_else(
                    || format!("{}: ", agg_diag.level.to_uppercase()),
                    |c| format!("{}: {}: ", agg_diag.level.to_uppercase(), c)
                ),
                agg_diag.rendered_message
            )?;

            // Print primary location
            writeln!(
                writer,
                "    (Diagnostic primary location: {})",
                agg_diag.primary_location
            )?;

            // Reference to global explanation, if applicable
            if let Some(code) = &agg_diag.code {
                if unique_explanations.contains_key(code) {
                    writeln!(
                        writer,
                        "    (For generic explanation of {}, see Appendix A)",
                        code
                    )?;
                }
            }

            // List feature sets
            let mut sorted_features: Vec<String> =
                agg_diag.feature_set_descriptors.iter().cloned().collect();
            sorted_features.sort(); // For consistent ordering of feature sets
            writeln!(
                writer,
                "    Occurred under feature set(s): {}",
                sorted_features.join(", ")
            )?;

            // List implicated third-party files for this specific instance
            if !agg_diag.implicated_third_party_files_details.is_empty() {
                let file_list = agg_diag
                    .implicated_third_party_files_details
                    .iter()
                    // The detail_loc is "filename:line_start"
                    .map(|(p, detail_loc)| {
                        format!(
                            "`{}` (at `{}`)",
                            p.file_name().unwrap_or_default().to_string_lossy(),
                            detail_loc
                        )
                    })
                    .collect::<Vec<String>>()
                    .join(", ");
                writeln!(
                    writer,
                    "    (Implicates: {} - see details below if extracted)",
                    file_list
                )?;
            }
            writeln!(writer)?; // Add a blank line for readability between diagnostics
        }
        writeln!(writer, "```\n")?;
    }

    // --- Section C: Extracted Third-Party Source Code ---
    if extracted_data.is_empty() && !sorted_file_paths.is_empty() {
        writeln!(writer, "\n## Extracted Third-Party Source Code\n")?;
        writeln!(
            writer,
            "Third-party files were implicated by diagnostics, but no source code items (functions, structs, etc. meeting criteria) were extracted from them, or an error occurred during extraction."
        )?;
    } else if extracted_data.is_empty() {
        // No files implicated or no data extracted
        writeln!(writer, "\n## Extracted Third-Party Source Code\n")?;
        writeln!(
            writer,
            "No third-party crate information extracted (either no third-party files were implicated by diagnostics, or no relevant items were found in them)."
        )?;
    } else {
        // We have extracted data for some files
        writeln!(writer, "\n## Extracted Third-Party Source Code\n")?;
        for file_path in sorted_file_paths {
            // Only create a section for files that were actually implicated and processed.
            // A file might be in sorted_file_paths but not in extracted_data if extraction failed or yielded no items.
            // It should, however, be in file_referencers if it was implicated.
            if extracted_data.contains_key(file_path) || file_referencers.contains_key(file_path) {
                writeln!(writer, "---\n### From File: `{}`\n", file_path.display())?;

                if let Some(origins) = file_referencers.get(file_path) {
                    if !origins.is_empty() {
                        writeln!(writer, "**Referenced by:**")?;
                        let mut sorted_origins: Vec<_> = origins.iter().collect();
                        sorted_origins.sort();
                        for origin in sorted_origins {
                            let level_str = origin.level.to_uppercase();
                            if level_str == "NOTE" || level_str == "HELP" {
                                writeln!(
                                    writer,
                                    "* {} (originating at `{}` from configuration: `{}`)",
                                    level_str,
                                    origin.originating_diagnostic_span_location,
                                    origin.feature_set_desc
                                )?;
                            } else {
                                writeln!(
                                    writer,
                                    "* {} {} (originating at `{}` from configuration: `{}`)",
                                    level_str,
                                    origin.code.as_deref().unwrap_or("N/A"),
                                    origin.originating_diagnostic_span_location,
                                    origin.feature_set_desc
                                )?;
                            }
                        }
                        writeln!(writer)?;
                    }
                }

                if let Some(items) = extracted_data.get(file_path) {
                    if items.is_empty() {
                        // This message is printed if the file was processed but no items met extraction criteria.
                        writeln!(
                            writer,
                            "_No extractable items (functions, structs, etc. meeting criteria) found or processed in this file._\n"
                        )?;
                    } else {
                        let mut in_impl_block_context = false;
                        for item in items {
                            let item_display_name = item_header_name_logic(item);
                            if item.item_kind.contains("Impl Block") && !item.is_sub_item {
                                in_impl_block_context = true;
                                // Using H4 for top-level items within a file section (H3 is "From File: ...")
                                writeln!(
                                    writer,
                                    "#### {} `{}`\n",
                                    item.item_kind, item_display_name
                                )?;
                            } else if item.is_sub_item {
                                // Using H5 for items within an Impl Block
                                let heading = if in_impl_block_context {
                                    "#####"
                                } else {
                                    "#### (Sub-item without Impl context)"
                                };
                                writeln!(
                                    writer,
                                    "{} {} `{}`\n",
                                    heading, item.item_kind, item.name
                                )?;
                            } else {
                                // Top-level item, not an impl block
                                in_impl_block_context = false;
                                writeln!(
                                    writer,
                                    "#### {} `{}`\n",
                                    item.item_kind, item_display_name
                                )?;
                            }

                            if !item.doc_comments.is_empty() {
                                for doc_line in &item.doc_comments {
                                    // So empty doc lines are still quoted to maintain blockquote continuity
                                    writeln!(
                                        writer,
                                        "> {}",
                                        if doc_line.is_empty() { "" } else { doc_line }
                                    )?;
                                }
                                writeln!(writer)?;
                            }
                            writeln!(writer, "```rust\n{}\n```\n", item.signature_or_definition)?;
                        }
                    }
                } else if file_referencers.contains_key(file_path) {
                    // This case covers when a file was implicated by a diagnostic (so it's in file_referencers)
                    // but yielded no extractable items (e.g., due to parsing error of that file by `syn`,
                    // or the file contained no items matching the extraction criteria).
                    writeln!(
                        writer,
                        "_This file was referenced by diagnostics, but no source code items were extracted (possibly due to a parsing issue or no matching items)._\n"
                    )?;
                }
            }
        }
    }

    // --- Section D: Appendix A: Error Code Explanations ---
    if !unique_explanations.is_empty() {
        writeln!(writer, "\n## Appendix A: Error Code Explanations\n")?;
        let mut sorted_explanations: Vec<(&String, &String)> = unique_explanations.iter().collect();
        sorted_explanations.sort_by_key(|(code, _)| *code);

        for (code, explanation_text) in sorted_explanations {
            writeln!(writer, "### Explanation for {}\n", code)?;
            // Properly format multi-line explanations as blockquotes
            explanation_text.trim().lines().for_each(|line| {
                let _ = writeln!(writer, "> {}", line); // The _ = consumes the Result from writeln!
            });
            writeln!(writer)?; // Add a blank line after each explanation block
        }
    }
    Ok(())
}
