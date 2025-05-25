# `getdoc`: Rust Error Contextualizer

`getdoc` is a developer tool for Rust projects designed to improve the debugging process by providing relevant source code context directly alongside compiler errors and warnings, especially those originating from or implicating third-party crates.

## Problem Solved

When a Rust project encounters compiler errors (e.g., trait bounds not satisfied, unresolved imports, type mismatches) that involve code from external dependencies, developers often need to:
1.  Identify the implicated third-party crate and specific code.
2.  Navigate to the crate's documentation (docs.rs) or its source code.
3.  Understand the relevant definitions, traits, or function signatures.

This process involves context switching and web searches, slowing down development. `getdoc` aims to minimize this by bringing the necessary information directly into a consolidated report.

## Features

* **Multi-Feature Analysis**: Runs `cargo check` across various feature flag combinations (default, no-default, all-features, individual features) to uncover a wider range of potential issues.
* **Compiler Output Aggregation**: Captures errors and warnings from `cargo check --message-format=json`.
* **Third-Party Code Focus**: Identifies diagnostics that involve code from dependencies (typically located in `~/.cargo/registry` or `~/.cargo/git`).
* **Source Code Extraction**: For each implicated third-party source file:
    * Parses the Rust code using `syn`.
    * Extracts relevant item definitions (functions, structs, enums, traits, impl blocks, associated items, type aliases, constants, extern crates, use statements).
    * Includes documentation comments (`///`, `//!`) associated with these items.
    * Displays error code explanations directly in the report.
* **Markdown Reporting**: Generates a single `report.md` file containing:
    * A list of compiler diagnostics, grouped by the feature set under which they occurred.
    * For each implicated third-party source file:
        * A list of the project's diagnostics that referenced this file.
        * Extracted documentation and definitions from that file, with a hierarchical display for items within `impl` blocks.

## How It Works

1.  **Determine Feature Sets**: Reads `Cargo.toml` to find available features and constructs a list of feature combinations to check.
2.  **Run Cargo Check**: For each feature set, executes `cargo check --message-format=json`.
3.  **Process Diagnostics**:
    * Parses the JSON output from `cargo check`.
    * Identifies errors and warnings.
    * Determines if any spans within a diagnostic point to third-party source files.
    * Collects details about these "implicated files" and the diagnostics that reference them.
4.  **Extract from Implicated Files**:
    * For each unique third-party source file identified, `getdoc` reads and parses its content.
    * It extracts definitions and doc comments for various Rust items (structs, functions, impl blocks and their contents, etc.).
5.  **Generate Report**: Compiles all collected diagnostics and extracted source code information into `report.md`.

## Usage

1.  Make sure you have Rust and Cargo installed.
2.  Run `cargo install getdoc`.
3.  Navigate to your Rust project's root directory (the one containing `Cargo.toml`).
4.  Run `getdoc` from that directory.
5.  After execution, a `report.md` file will be generated in your project's root directory.

The tool prints progress to the console (e.g., `[getdoc] Starting analysis...`, `[getdoc] Running cargo check ...`).

## Output

The `report.md` file will contain:
* A header with the report generation timestamp.
* A section for "Compiler Output (Errors and Warnings)", detailing issues per feature combination, including error code explanations where available.
* Sections for each implicated third-party file ("From File: ..."), showing:
    * Which local diagnostics referenced this file.
    * Extracted items (structs, functions, traits, impls, etc.) from that file, including their signatures and doc comments.

This allows you to see the error, the compiler's explanation, and the relevant parts of the third-party code all in one place.
