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
        Regex::new(r"(\d+)\s+pass").unwrap();
    static ref BUN_TEST_FAIL_COUNT_RE: Regex =
        Regex::new(r"(\d+)\s+fail").unwrap();
    static ref BUN_TEST_SKIP_RE: Regex =
        Regex::new(r"(\d+)\s+skip").unwrap();
    static ref BUN_TEST_EXPECT_RE: Regex =
        Regex::new(r"(\d+)\s+expect").unwrap();

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

#[derive(Debug, Clone)]
pub enum BunCommand {
    Test,
    Install,
    Build,
    Run { script: String },
}

pub fn run(cmd: BunCommand, args: &[String], verbose: u8, skip_env: bool) -> Result<i32> {
    match cmd {
        BunCommand::Test => run_test(args, verbose),
        BunCommand::Install => run_install(args, verbose, skip_env),
        BunCommand::Build => run_build(args, verbose),
        BunCommand::Run { script } => run_script(&script, args, verbose, skip_env),
    }
}

fn run_test(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    cmd.arg("test");
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
        |raw| filter_bun_test(raw),
        runner::RunOptions::default(),
    )
}

fn run_install(args: &[String], verbose: u8, skip_env: bool) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    cmd.arg("install");
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
        |raw| filter_bun_install(raw),
        runner::RunOptions::default(),
    )
}

fn run_build(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    cmd.arg("build");
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
        |raw| filter_bun_build(raw),
        runner::RunOptions::default(),
    )
}

fn run_script(script: &str, args: &[String], verbose: u8, skip_env: bool) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    cmd.arg("run").arg(script);
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
        |raw| filter_bun_run(raw),
        runner::RunOptions::default(),
    )
}

pub fn run_passthrough(args: &[OsString], verbose: u8) -> Result<i32> {
    runner::run_passthrough("bun", args, verbose)
}

/// Filter bun test output: strip passing tests, keep failures and summary.
///
/// Bun test output format:
/// ```text
/// bun test v1.1.0 (abcdef0)
///
/// path/to/test.ts:
/// ✓ test name [0.50ms]
/// ✗ failing test
///   ... error details ...
///
///  10 pass
///  1 fail
///  2 skip
///  50 expect() calls
/// Ran 11 tests across 3 files. [150.00ms]
/// ```
fn filter_bun_test(output: &str) -> String {
    let clean = strip_ansi(output);
    let lines: Vec<&str> = clean.lines().collect();

    if lines.is_empty() {
        return String::new();
    }

    let mut failures: Vec<&str> = Vec::new();
    let mut summary_lines: Vec<&str> = Vec::new();
    let mut in_failure = false;
    let mut has_failures = false;

    for line in &lines {
        let trimmed = line.trim();

        // Detect failure marker
        if trimmed.starts_with("✗") || trimmed.starts_with("✘") || trimmed.starts_with("FAIL") {
            in_failure = true;
            has_failures = true;
            failures.push(line);
            continue;
        }

        // Collect failure details (indented lines after a failure)
        if in_failure {
            if trimmed.is_empty()
                || trimmed.starts_with("✓")
                || trimmed.starts_with("✔")
                || trimmed.starts_with("bun test")
            {
                in_failure = false;
            } else {
                failures.push(line);
                continue;
            }
        }

        // Collect summary lines (pass/fail/skip counts and "Ran X tests" line)
        if BUN_TEST_SUMMARY_RE.is_match(trimmed)
            || BUN_TEST_FAIL_COUNT_RE.is_match(trimmed)
            || BUN_TEST_SKIP_RE.is_match(trimmed)
            || BUN_TEST_EXPECT_RE.is_match(trimmed)
            || trimmed.starts_with("Ran ")
        {
            summary_lines.push(trimmed);
        }
    }

    let mut result = Vec::new();

    if has_failures {
        // Show failures
        result.push("FAILURES:".to_string());
        for line in &failures {
            result.push(line.to_string());
        }
        result.push(String::new());
    }

    // Always show summary
    if !summary_lines.is_empty() {
        for line in &summary_lines {
            result.push(line.to_string());
        }
    } else {
        // Fallback: return compact summary if we couldn't parse
        result.push("bun test: completed".to_string());
    }

    let output = result.join("\n");
    if output.trim().is_empty() {
        "bun test: completed".to_string()
    } else {
        output
    }
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

    if result.is_empty() {
        "bun install: ok".to_string()
    } else {
        result.join("\n")
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

    if result.is_empty() {
        "bun build: ok".to_string()
    } else {
        result.join("\n")
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

    if result.is_empty() {
        "ok".to_string()
    } else {
        result.join("\n")
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
        assert!(!output.contains("✓"));
        // Should contain summary
        assert!(output.contains("pass"));

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

        // Should contain failure info
        assert!(output.contains("FAILURES:"));
        assert!(output.contains("✗") || output.contains("fail"));
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
}
