//! Filters Bun output — test results, install logs, build summaries, script output.

use crate::core::runner;
use crate::core::utils::{resolved_command, strip_ansi};
use anyhow::Result;
use lazy_static::lazy_static;
use regex::Regex;
use std::ffi::OsString;

lazy_static! {
    /// Matches bun test summary line: "X pass, Y fail, Z skip" (with optional ANSI)
    static ref BUN_TEST_SUMMARY_RE: Regex =
        Regex::new(r"^\s*\d+\s+pass").unwrap();
    static ref BUN_TEST_FAIL_COUNT_RE: Regex =
        Regex::new(r"^\s*\d+\s+fail$").unwrap();
    static ref BUN_TEST_SKIP_RE: Regex =
        Regex::new(r"^\s*\d+\s+skip").unwrap();
    static ref BUN_TEST_EXPECT_RE: Regex =
        Regex::new(r"^\s*\d+\s+expect").unwrap();
    /// Matches "X tests failed:" trailing summary section
    static ref BUN_TESTS_FAILED_RE: Regex =
        Regex::new(r"^\d+\s+tests?\s+failed:").unwrap();

    /// Matches bun install summary: "bun install v1.x (Xms)" or "X packages installed"
    static ref BUN_INSTALL_DONE_RE: Regex =
        Regex::new(r"(?i)(\d+)\s+packages?\s+installed").unwrap();
    static ref BUN_INSTALL_SPEED_RE: Regex =
        Regex::new(r"\[\d+(\.\d+)?(ms|s)\]").unwrap();

    /// Matches bun build output size lines
    static ref BUN_BUILD_ENTRY_RE: Regex =
        Regex::new(r"^\s*\S+\s+[\d.]+(KB|MB|B|GB)\s*$").unwrap();

    /// Matches lines that are just whitespace or ANSI reset codes
    static ref NOISE_LINE_RE: Regex =
        Regex::new(r"^\s*(\x1b\[[0-9;]*m)*\s*$").unwrap();
}

pub fn test(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    cmd.arg("test");
    cmd.env("LC_ALL", "C");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: bun test {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "bun",
        &format!("test {}", args.join(" ")),
        filter_bun_test,
        runner::RunOptions::default(),
    )
}

pub fn install(args: &[String], verbose: u8, skip_env: bool) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    cmd.arg("install");
    cmd.env("LC_ALL", "C");
    for arg in args {
        cmd.arg(arg);
    }
    if skip_env {
        cmd.env("SKIP_ENV_VALIDATION", "1");
    }

    if verbose > 0 {
        eprintln!("Running: bun install {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "bun",
        &format!("install {}", args.join(" ")),
        filter_bun_install,
        runner::RunOptions::default(),
    )
}

pub fn build(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    cmd.arg("build");
    cmd.env("LC_ALL", "C");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: bun build {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "bun",
        &format!("build {}", args.join(" ")),
        filter_bun_build,
        runner::RunOptions::default(),
    )
}

pub fn run(script: &str, args: &[String], verbose: u8, skip_env: bool) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    cmd.arg("run").arg(script);
    cmd.env("LC_ALL", "C");
    for arg in args {
        cmd.arg(arg);
    }
    if skip_env {
        cmd.env("SKIP_ENV_VALIDATION", "1");
    }

    if verbose > 0 {
        eprintln!("Running: bun run {} {}", script, args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "bun",
        &format!("run {} {}", script, args.join(" ")),
        filter_bun_run,
        runner::RunOptions::default(),
    )
}

pub fn run_passthrough(args: &[OsString], verbose: u8) -> Result<i32> {
    runner::run_passthrough("bun", args, verbose)
}

/// Filter bun test output: strip passing tests, keep failures and summary.
///
/// Handles both TTY format (✓/✗) and piped format ((pass)/(fail)).
/// Bun outputs error context BEFORE the failure marker, so we buffer
/// potential error lines and flush them when a (fail)/✗ marker appears.
fn filter_bun_test(output: &str) -> String {
    let clean = strip_ansi(output);
    let lines: Vec<&str> = clean.lines().collect();

    if lines.is_empty() {
        return String::new();
    }

    let mut failures: Vec<String> = Vec::new();
    let mut summary_lines: Vec<&str> = Vec::new();
    let mut error_buffer: Vec<&str> = Vec::new();
    let mut has_failures = false;
    let mut in_failed_summary = false;

    for line in &lines {
        let trimmed = line.trim();

        // Skip empty lines upfront — never buffer or match against them
        if trimmed.is_empty() {
            continue;
        }

        // "X tests failed:" trailing section — skip (duplicates already captured)
        if BUN_TESTS_FAILED_RE.is_match(trimmed) {
            in_failed_summary = true;
            error_buffer.clear();
            continue;
        }
        if in_failed_summary {
            if is_fail_marker(trimmed) {
                continue;
            }
            in_failed_summary = false;
            // Fall through to summary detection
        }

        // Failure marker — flush error buffer as context
        if is_fail_marker(trimmed) {
            has_failures = true;
            for buf_line in &error_buffer {
                failures.push(buf_line.to_string());
            }
            error_buffer.clear();
            failures.push(trimmed.to_string());
            continue;
        }

        // Pass marker or file header — discard buffered lines (not error context)
        if is_pass_marker(trimmed) || is_test_file_header(trimmed) || trimmed.starts_with("bun test") {
            error_buffer.clear();
            continue;
        }

        // Summary lines
        if BUN_TEST_SUMMARY_RE.is_match(trimmed)
            || BUN_TEST_FAIL_COUNT_RE.is_match(trimmed)
            || BUN_TEST_SKIP_RE.is_match(trimmed)
            || BUN_TEST_EXPECT_RE.is_match(trimmed)
            || trimmed.starts_with("Ran ")
        {
            error_buffer.clear();
            summary_lines.push(trimmed);
            continue;
        }

        // Buffer as potential error context (will be flushed if followed by a fail marker)
        error_buffer.push(trimmed);
    }

    let mut result = Vec::new();

    if has_failures {
        result.push("FAILURES:".to_string());
        for line in &failures {
            result.push(line.to_string());
        }
        result.push(String::new());
    }

    if !summary_lines.is_empty() {
        for line in &summary_lines {
            result.push(line.to_string());
        }
    } else {
        result.push("bun test: completed".to_string());
    }

    let joined = result.join("\n");
    let output = joined.trim();
    if output.is_empty() {
        "bun test: completed".to_string()
    } else {
        output.to_string()
    }
}

fn is_fail_marker(line: &str) -> bool {
    line.starts_with('✗') || line.starts_with('✘') || line.starts_with("(fail)") || line.starts_with("FAIL")
}

fn is_pass_marker(line: &str) -> bool {
    line.starts_with('✓') || line.starts_with('✔') || line.starts_with("(pass)")
}

fn is_test_file_header(line: &str) -> bool {
    line.ends_with(':') && (line.contains(".test.") || line.contains(".spec."))
}

/// Filter bun install output: strip progress bars, keep summary.
fn filter_bun_install(output: &str) -> String {
    let clean = strip_ansi(output);
    let mut result = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();

        // Skip empty lines
        if trimmed.is_empty() {
            continue;
        }
        // Skip progress indicators
        if trimmed.contains("⸩") || trimmed.contains("⸨") {
            continue;
        }
        // Skip resolution/download progress
        if trimmed.starts_with("Resolving")
            || trimmed.starts_with("Downloading")
            || trimmed.starts_with("Extracting")
        {
            continue;
        }
        // Skip bun install header noise
        if trimmed.starts_with("bun install v") {
            continue;
        }

        // Keep: package count, warnings, errors, lockfile info
        result.push(trimmed.to_string());
    }

    let joined = result.join("\n");
    let output = joined.trim();
    if output.is_empty() {
        "bun install: ok".to_string()
    } else {
        output.to_string()
    }
}

/// Filter bun build output: keep errors, warnings, and final summary.
fn filter_bun_build(output: &str) -> String {
    let clean = strip_ansi(output);
    let mut result = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }
        // Skip bun build header
        if trimmed.starts_with("bun build v") {
            continue;
        }
        // Keep errors and warnings
        if trimmed.contains("error") || trimmed.contains("warn") || trimmed.contains("Error") {
            result.push(trimmed.to_string());
            continue;
        }
        // Keep build output entries (file sizes)
        if BUN_BUILD_ENTRY_RE.is_match(trimmed) {
            result.push(trimmed.to_string());
            continue;
        }
        // Keep summary lines
        if trimmed.contains("built") || trimmed.contains("Bundle") || trimmed.starts_with("Done") {
            result.push(trimmed.to_string());
        }
    }

    let joined = result.join("\n");
    let output = joined.trim();
    if output.is_empty() {
        "bun build: ok".to_string()
    } else {
        output.to_string()
    }
}

/// Filter bun run output: strip boilerplate, keep script output.
fn filter_bun_run(output: &str) -> String {
    let clean = strip_ansi(output);
    let mut result = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();

        // Skip empty lines
        if trimmed.is_empty() {
            continue;
        }
        // Skip bun run boilerplate ($ command echo)
        if trimmed.starts_with("$") && trimmed.len() < 200 {
            continue;
        }
        // Skip bun version header
        if trimmed.starts_with("bun run v") {
            continue;
        }

        result.push(line.to_string());
    }

    let joined = result.join("\n");
    let output = joined.trim();
    if output.is_empty() {
        "ok".to_string()
    } else {
        output.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    #[test]
    fn test_filter_bun_test_all_pass() {
        let input = include_str!("../../../tests/fixtures/bun_test_pass.txt");
        let output = filter_bun_test(input);

        // Should NOT contain individual test lines
        assert!(!output.contains("(pass)"), "Filtered output still contains passing tests");
        // Should contain summary
        assert!(output.contains("pass"));
        // Should NOT contain failure header
        assert!(!output.contains("FAILURES:"));

        // Token savings
        let savings =
            100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "bun test (pass): expected >=60% savings, got {:.1}%",
            savings
        );
    }

    #[test]
    fn test_filter_bun_test_with_failures() {
        let input = include_str!("../../../tests/fixtures/bun_test_fail.txt");
        let output = filter_bun_test(input);

        // Should contain failure header and error details
        assert!(output.contains("FAILURES:"), "Missing FAILURES header:\n{}", output);
        assert!(
            output.contains("(fail)") || output.contains("fail"),
            "Missing failure markers:\n{}", output
        );
        // Should contain error context (captured from lines before the marker)
        assert!(
            output.contains("Expected:") && output.contains("Received:"),
            "Missing error details:\n{}", output
        );
        // Should NOT contain passing tests
        assert!(!output.contains("(pass)"), "Contains passing tests:\n{}", output);
    }

    #[test]
    fn test_filter_bun_test_empty() {
        let output = filter_bun_test("");
        assert!(output.is_empty() || output == "bun test: completed");
    }

    #[test]
    fn test_filter_bun_install() {
        let input = include_str!("../../../tests/fixtures/bun_install.txt");
        let output = filter_bun_install(input);

        // Should not contain progress noise
        assert!(!output.contains("Resolving"));
        assert!(!output.contains("Downloading"));

        // Token savings
        let savings =
            100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "bun install: expected >=60% savings, got {:.1}%",
            savings
        );
    }

    #[test]
    fn test_filter_bun_build() {
        let input = include_str!("../../../tests/fixtures/bun_build.txt");
        let output = filter_bun_build(input);

        // Should keep error/summary info
        assert!(
            output.contains("built") || output.contains("ok") || output.contains("Done"),
            "Expected build summary in output: {}",
            output
        );
    }

    #[test]
    fn test_filter_bun_run_strips_boilerplate() {
        let input = "$ next build\n\nCreating optimized build...\n✓ Build completed in 4.2s\n";
        let output = filter_bun_run(input);

        assert!(!output.contains("$ next"));
        assert!(output.contains("Build completed"));
    }

    #[test]
    fn test_filter_bun_run_empty() {
        let output = filter_bun_run("\n\n\n");
        assert_eq!(output, "ok");
    }

    #[test]
    fn test_filter_bun_install_empty() {
        let output = filter_bun_install("\n\n");
        assert_eq!(output, "bun install: ok");
    }

    #[test]
    fn test_filter_bun_build_empty() {
        let output = filter_bun_build("");
        assert_eq!(output, "bun build: ok");
    }

    #[test]
    fn test_filter_bun_test_real_output() {
        let input = include_str!("../../../tests/fixtures/bun_test_real.txt");
        let output = filter_bun_test(input);

        // Must NOT contain passing test lines
        assert!(
            !output.contains("(pass)"),
            "Filtered output still contains passing tests:\n{}",
            output
        );
        // Must contain failure details with error context
        assert!(
            output.contains("FAILURES:"),
            "Filtered output missing FAILURES header:\n{}",
            output
        );
        assert!(
            output.contains("Expected:") && output.contains("Received:"),
            "Filtered output missing error details:\n{}",
            output
        );
        // Must contain summary
        assert!(
            output.contains("79 pass"),
            "Filtered output missing summary:\n{}",
            output
        );
        assert!(
            output.contains("Ran 81 tests"),
            "Filtered output missing run count:\n{}",
            output
        );

        // Token savings must be >= 60%
        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "bun test (real): expected >=60% savings, got {:.1}% ({} -> {} tokens)\nOutput:\n{}",
            savings,
            input_tokens,
            output_tokens,
            output
        );
    }

    #[test]
    fn test_filter_bun_test_tty_format() {
        // TTY format uses ✓/✗ instead of (pass)/(fail)
        let input = "bun test v1.1.0 (abc)\n\ntest.ts:\n✓ passes [1ms]\n  Expected: 1\n  Received: 2\n✗ fails [1ms]\n\n 1 pass\n 1 fail\n 2 expect() calls\nRan 2 tests across 1 files. [10.00ms]\n";
        let output = filter_bun_test(input);

        assert!(output.contains("FAILURES:"), "Missing FAILURES for TTY format:\n{}", output);
        assert!(output.contains("Expected:"), "Missing error context for TTY format:\n{}", output);
        assert!(output.contains("1 pass"), "Missing summary for TTY format:\n{}", output);
    }
}
