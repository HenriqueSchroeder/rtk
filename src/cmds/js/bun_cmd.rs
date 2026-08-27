//! Filters Bun output — test results, install logs, build summaries, script output.

use crate::core::runner;
use crate::core::tee::force_tee_hint;
use crate::core::truncate::{CAP_ERRORS, CAP_LIST, CAP_WARNINGS};
use crate::core::utils::{resolved_command, strip_ansi};
use anyhow::Result;
use regex::Regex;
use std::ffi::OsString;
use std::sync::LazyLock;

/// Failure blocks kept before the rest is deferred to the tee file.
const MAX_BUN_FAILURES: usize = CAP_WARNINGS;

/// Bun's per-count summary lines: " N pass" / " N fail" / " N skip" / " N todo".
/// Anchored to end-of-line so ordinary console output like "5 passengers
/// boarded" is never mistaken for the "N pass" summary.
static BUN_TEST_COUNT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\d+\s+(pass|fail|skip|todo)\s*$").unwrap());
/// " N expect() calls", plus the snapshot variant " N snapshots, N expect() calls".
static BUN_TEST_EXPECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(\d+\s+snapshots?,\s*)?\d+\s+expect\(\)").unwrap());
/// "snapshots: +1 added" / "snapshots: 1 obsolete".
static BUN_SNAPSHOT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^snapshots:\s").unwrap());
/// Matches "X tests failed:" trailing summary section
static BUN_TESTS_FAILED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+\s+tests?\s+failed:").unwrap());

/// Real bun test result markers ALWAYS end with a timing suffix like `[0.08ms]`.
/// Requiring it prevents app console output (e.g. `✓ Cache refreshed`,
/// `FAILED to connect`) from being misread as pass/fail markers.
static BUN_FAIL_MARKER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(✗|✘|\(fail\)).*\[\d+(\.\d+)?(ms|s)\]\s*$").unwrap());
static BUN_PASS_MARKER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(✓|✔|\(pass\)).*\[\d+(\.\d+)?(ms|s)\]\s*$").unwrap());

/// Version banners: "bun test v1.3.14 (0d9b296a)", "bun install v…", "bun add v…".
static BUN_BANNER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^bun (test|install|add|remove|build|run) v\d").unwrap());

/// A bundled artifact line: "index.js  72 bytes  (entry point)", "app.js  45.2KB".
static BUN_BUILD_ENTRY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\S+\s+[\d.]+\s*(bytes|KB|MB|GB|B)\b").unwrap());

/// An installed/removed package line: "+ zod@4.4.3", "installed chalk@6.0.0".
static BUN_PACKAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([+-]\s+\S+@|installed\s+\S+@)").unwrap());

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

    runner::run_filtered_with_exit(
        cmd,
        "bun",
        &format!("test {}", args.join(" ")),
        filter_bun_test,
        runner::RunOptions::with_tee("bun-test"),
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

    runner::run_filtered_with_exit(
        cmd,
        "bun",
        &format!("install {}", args.join(" ")),
        filter_bun_install,
        runner::RunOptions::with_tee("bun-install"),
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

    runner::run_filtered_with_exit(
        cmd,
        "bun",
        &format!("build {}", args.join(" ")),
        filter_bun_build,
        runner::RunOptions::with_tee("bun-build"),
    )
}

pub fn run(script: &str, args: &[String], verbose: u8, skip_env: bool) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    // NOTE: unlike test/install/build we do NOT force LC_ALL=C here — `bun run`
    // executes an arbitrary user script, and overriding the locale for the whole
    // child process tree changes its behaviour (e.g. a Python build step would
    // fall back to ASCII stdio). filter_bun_run does not parse structured output,
    // so consistent locale isn't needed.
    cmd.args(run_args(script, args));
    if skip_env {
        cmd.env("SKIP_ENV_VALIDATION", "1");
    }

    if verbose > 0 {
        eprintln!("Running: bun run {} {}", script, args.join(" "));
    }

    runner::run_filtered_with_exit(
        cmd,
        "bun",
        &format!("run {} {}", script, args.join(" ")),
        filter_bun_run,
        runner::RunOptions::with_tee("bun-run"),
    )
}

/// argv for `bun run`.
///
/// `--silent` suppresses the `$ <cmd>` echo at the source, and it has to come
/// before the script name or bun forwards it to the script. Stripping the echo
/// from the output instead does not work: bun writes it to stderr while the
/// script writes to stdout, and stdout lands first in the capture, so "drop the
/// first $-prefixed line" eats a line of the script's own output.
fn run_args(script: &str, args: &[String]) -> Vec<String> {
    let mut argv = Vec::with_capacity(args.len() + 3);
    argv.push("run".to_string());
    argv.push("--silent".to_string());
    argv.push(script.to_string());
    argv.extend_from_slice(args);
    argv
}

pub fn run_passthrough(args: &[OsString], verbose: u8) -> Result<i32> {
    runner::run_passthrough("bun", args, verbose)
}

pub fn run_tool(args: &[String], verbose: u8) -> Result<i32> {
    let os_args: Vec<OsString> = args.iter().map(Into::into).collect();
    runner::run_passthrough("bunx", &os_args, verbose)
}

/// Join filtered lines, or report why nothing was kept.
///
/// An empty filter result with a non-zero exit means the command died before
/// printing anything parseable (a SIGKILLed test run prints the banner and
/// nothing else). Reporting "ok" there would hide a crash behind a success
/// message, so the exit code is surfaced instead.
fn finish(lines: Vec<String>, exit_code: i32, label: &str) -> String {
    let joined = lines.join("\n");
    let output = joined.trim();
    if !output.is_empty() {
        return output.to_string();
    }
    if exit_code != 0 {
        return format!("{} exited with code {}", label, exit_code);
    }
    "ok".to_string()
}

/// Append `… +N more <label>` plus a tee hint pointing at the full content.
fn push_overflow(result: &mut Vec<String>, hidden: usize, label: &str, full: &str, slug: &str) {
    result.push(format!("… +{} more {}", hidden, label));
    if let Some(hint) = force_tee_hint(full, slug) {
        result.push(hint);
    }
}

/// Filter bun test output: strip passing tests, keep failures and summary.
///
/// Handles both TTY format (✓/✗) and piped format ((pass)/(fail)). Bun prints
/// the error context BEFORE the failure marker, so lines are buffered and
/// flushed as a block when a (fail)/✗ marker appears. Lines are kept verbatim:
/// the caret in `expect(...)` context points at a column, and a `toEqual` diff
/// carries its own indentation, so trimming would corrupt both.
fn filter_bun_test(output: &str, exit_code: i32) -> String {
    let clean = strip_ansi(output);

    let mut failures: Vec<Vec<String>> = Vec::new();
    let mut summary: Vec<String> = Vec::new();
    let mut coverage: Vec<String> = Vec::new();
    let mut buffer: Vec<String> = Vec::new();
    let mut in_failed_summary = false;

    for line in clean.lines() {
        let trimmed = line.trim();

        // Skip empty lines upfront — never buffer or match against them
        if trimmed.is_empty() {
            continue;
        }

        // "X tests failed:" trailing section — skip (duplicates already captured)
        if BUN_TESTS_FAILED_RE.is_match(trimmed) {
            in_failed_summary = true;
            buffer.clear();
            continue;
        }
        if in_failed_summary {
            if is_fail_marker(trimmed) {
                continue;
            }
            in_failed_summary = false;
            // Fall through to the regular checks
        }

        // `--coverage` table: explicitly requested, so never dropped
        if is_coverage_row(trimmed) {
            buffer.clear();
            coverage.push(line.to_string());
            continue;
        }

        // Failure marker — the buffered context belongs to this failure
        if is_fail_marker(trimmed) {
            let mut block = std::mem::take(&mut buffer);
            block.push(line.to_string());
            failures.push(block);
            continue;
        }

        // Pass marker, file header or banner — discard buffered lines
        if is_pass_marker(trimmed) || is_test_file_header(trimmed) || BUN_BANNER_RE.is_match(trimmed)
        {
            buffer.clear();
            continue;
        }

        if is_summary_line(trimmed) {
            buffer.clear();
            summary.push(trimmed.to_string());
            continue;
        }

        // Potential error context, flushed if a fail marker follows
        buffer.push(line.to_string());
    }

    let mut result = Vec::new();

    if !failures.is_empty() {
        result.push("FAILURES:".to_string());
        for block in failures.iter().take(MAX_BUN_FAILURES) {
            result.extend(block.iter().cloned());
        }
        if failures.len() > MAX_BUN_FAILURES {
            let all: Vec<String> = failures.iter().map(|b| b.join("\n")).collect();
            push_overflow(
                &mut result,
                failures.len() - MAX_BUN_FAILURES,
                "failures",
                &all.join("\n\n"),
                "bun-test-failures",
            );
        }
        result.push(String::new());
    }

    result.extend(coverage);

    if !summary.is_empty() {
        result.extend(summary);
    } else if failures.is_empty() && !buffer.is_empty() {
        // No markers and no summary: bun aborted before running any test (a
        // module-resolution or syntax error). Surface what it printed rather
        // than collapsing to a misleading "ok".
        result.extend(buffer.iter().take(CAP_ERRORS).cloned());
        if buffer.len() > CAP_ERRORS {
            push_overflow(
                &mut result,
                buffer.len() - CAP_ERRORS,
                "lines",
                &buffer.join("\n"),
                "bun-test",
            );
        }
    }

    finish(result, exit_code, "bun test")
}

fn is_fail_marker(line: &str) -> bool {
    BUN_FAIL_MARKER_RE.is_match(line)
}

fn is_pass_marker(line: &str) -> bool {
    BUN_PASS_MARKER_RE.is_match(line)
}

fn is_test_file_header(line: &str) -> bool {
    line.ends_with(':') && (line.contains(".test.") || line.contains(".spec."))
}

fn is_summary_line(line: &str) -> bool {
    BUN_TEST_COUNT_RE.is_match(line)
        || BUN_TEST_EXPECT_RE.is_match(line)
        || BUN_SNAPSHOT_RE.is_match(line)
        || line.starts_with("Ran ")
        || line.starts_with("Bailed out after ")
}

/// A row of the `--coverage` table. Every row (header, separator and per-file)
/// carries the three column separators, which ordinary test output does not.
fn is_coverage_row(line: &str) -> bool {
    line.matches('|').count() >= 3
}

/// Filter bun install output: strip progress bars, keep summary.
fn filter_bun_install(output: &str, exit_code: i32) -> String {
    let clean = strip_ansi(output);
    let mut packages: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }
        // Skip progress indicators
        if trimmed.contains('⸩') || trimmed.contains('⸨') {
            continue;
        }
        // Skip resolution/download progress
        if trimmed.starts_with("Resolving")
            || trimmed.starts_with("Downloading")
            || trimmed.starts_with("Extracting")
        {
            continue;
        }
        if BUN_BANNER_RE.is_match(trimmed) {
            continue;
        }

        // Keep: package list, counts, warnings, errors, lockfile info
        if BUN_PACKAGE_RE.is_match(trimmed) {
            packages.push(trimmed.to_string());
        } else {
            rest.push(trimmed.to_string());
        }
    }

    let mut result: Vec<String> = packages.iter().take(CAP_LIST).cloned().collect();
    if packages.len() > CAP_LIST {
        push_overflow(
            &mut result,
            packages.len() - CAP_LIST,
            "packages",
            &packages.join("\n"),
            "bun-install",
        );
    }
    result.extend(rest);

    finish(result, exit_code, "bun install")
}

/// Filter bun build output: keep errors, warnings, artifacts and the summary.
fn filter_bun_build(output: &str, exit_code: i32) -> String {
    let clean = strip_ansi(output);
    let mut entries: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }
        if BUN_BANNER_RE.is_match(trimmed) {
            continue;
        }
        // Keep errors and warnings
        if trimmed.contains("error")
            || trimmed.contains("warn")
            || trimmed.contains("Error")
            || trimmed.contains("Warn")
        {
            rest.push(trimmed.to_string());
            continue;
        }
        // Keep bundled artifacts ("index.js  72 bytes  (entry point)")
        if BUN_BUILD_ENTRY_RE.is_match(trimmed) {
            entries.push(trimmed.to_string());
            continue;
        }
        // Keep summary lines
        if trimmed.starts_with("Bundled")
            || trimmed.contains("built")
            || trimmed.contains("Bundle")
            || trimmed.starts_with("Done")
        {
            rest.push(trimmed.to_string());
        }
    }

    let mut result: Vec<String> = entries.iter().take(CAP_LIST).cloned().collect();
    if entries.len() > CAP_LIST {
        push_overflow(
            &mut result,
            entries.len() - CAP_LIST,
            "artifacts",
            &entries.join("\n"),
            "bun-build",
        );
    }
    result.extend(rest);

    finish(result, exit_code, "bun build")
}

/// Filter bun run output: keep the script's own output.
///
/// The `$ <cmd>` echo is suppressed by `--silent` in `run()`, so nothing here
/// inspects `$`-prefixed lines: they all belong to the script. Lines are never
/// trimmed either, this is passthrough of arbitrary output where indentation
/// is content.
fn filter_bun_run(output: &str, exit_code: i32) -> String {
    let clean = strip_ansi(output);
    let mut result = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }
        if BUN_BANNER_RE.is_match(trimmed) {
            continue;
        }

        result.push(line.to_string());
    }

    finish(result, exit_code, "bun run")
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
        let output = filter_bun_test(input, 0);

        // Should NOT contain individual test lines
        assert!(
            !output.contains("(pass)"),
            "Filtered output still contains passing tests"
        );
        // Should contain summary
        assert!(output.contains("pass"));
        // Should NOT contain failure header
        assert!(!output.contains("FAILURES:"));

        // Bun 1.3.14 already prints just the summary when nothing fails, so
        // there is nothing to compress here; savings are asserted on the
        // failure-heavy fixtures, where the volume actually is.
        assert!(
            output.contains("5 pass") && output.contains("Ran 5 tests"),
            "summary lost:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_bun_test_with_failures() {
        let input = include_str!("../../../tests/fixtures/bun_test_fail.txt");
        let output = filter_bun_test(input, 1);

        // Should contain failure header and error details
        assert!(
            output.contains("FAILURES:"),
            "Missing FAILURES header:\n{}",
            output
        );
        assert!(
            output.contains("(fail)") || output.contains("fail"),
            "Missing failure markers:\n{}",
            output
        );
        // Should contain error context (captured from lines before the marker)
        assert!(
            output.contains("Expected:") && output.contains("Received:"),
            "Missing error details:\n{}",
            output
        );
        // Should NOT contain passing tests
        assert!(
            !output.contains("(pass)"),
            "Contains passing tests:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_bun_test_keeps_caret_column() {
        // The caret under an `expect(...)` call points at a column. Trimming the
        // context lines would move it, so the alignment must survive verbatim.
        let input = include_str!("../../../tests/fixtures/bun_test_fail.txt");
        let output = filter_bun_test(input, 1);

        let caret = output
            .lines()
            .find(|l| l.trim() == "^")
            .expect("caret line must be kept");
        let indent = caret.len() - caret.trim_start().len();
        assert!(
            indent > 10,
            "caret lost its column (indent {}):\n{}",
            indent,
            output
        );
    }

    #[test]
    fn test_filter_bun_test_keeps_diff_indentation() {
        // A `toEqual` diff is indentation-sensitive: the -/+ lines and their
        // nesting are what make the diff readable.
        let input = include_str!("../../../tests/fixtures/bun_test_mixed.txt");
        let output = filter_bun_test(input, 1);

        assert!(
            output.contains("-   \"name\": \"gadget\",")
                && output.contains("+   \"name\": \"widget\","),
            "toEqual diff lost its indentation:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_bun_test_keeps_todo_and_snapshot_counts() {
        let input = include_str!("../../../tests/fixtures/bun_test_mixed.txt");
        let output = filter_bun_test(input, 1);

        for expected in ["2 pass", "1 skip", "1 todo", "2 fail", "snapshots: +1 added"] {
            assert!(
                output.contains(expected),
                "summary line {:?} was dropped:\n{}",
                expected,
                output
            );
        }
    }

    #[test]
    fn test_filter_bun_test_keeps_bailed_out() {
        let input = include_str!("../../../tests/fixtures/bun_test_bail.txt");
        let output = filter_bun_test(input, 1);

        assert!(
            output.contains("Bailed out after 1 failure"),
            "--bail notice was dropped:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_bun_test_keeps_coverage_table() {
        let input = include_str!("../../../tests/fixtures/bun_test_coverage.txt");
        let output = filter_bun_test(input, 1);

        assert!(
            output.contains("% Funcs") && output.contains("All files"),
            "--coverage table was dropped:\n{}",
            output
        );
        // The snapshot variant of the expect() line must survive too
        assert!(
            output.contains("1 snapshots, 4 expect() calls"),
            "snapshot expect() summary was dropped:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_bun_test_caps_failures_with_hint() {
        let input = include_str!("../../../tests/fixtures/bun_test_many.txt");
        let output = filter_bun_test(input, 1);

        let kept = output.lines().filter(|l| l.contains("(fail)")).count();
        assert_eq!(
            kept, MAX_BUN_FAILURES,
            "failure blocks were not capped:\n{}",
            output
        );
        assert!(
            output.contains("more failures"),
            "capped output must say what was hidden:\n{}",
            output
        );
        // Summary still survives the cap
        assert!(
            output.contains("40 fail"),
            "summary lost after capping:\n{}",
            output
        );

        let savings = 100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "bun test (40 failures): expected >=60% savings, got {:.1}%",
            savings
        );
    }

    #[test]
    fn test_filter_bun_test_crash_reports_exit_code() {
        // A SIGKILLed run prints the banner and nothing else. Exit 137 must not
        // be reported as "ok".
        let input = include_str!("../../../tests/fixtures/bun_test_crash.txt");
        let output = filter_bun_test(input, 137);

        assert_ne!(output, "ok", "crash reported as success");
        assert!(
            output.contains("137"),
            "exit code must be surfaced:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_bun_test_empty() {
        let output = filter_bun_test("", 0);
        assert!(output.is_empty() || output == "ok");
    }

    #[test]
    fn test_filter_bun_install() {
        let input = include_str!("../../../tests/fixtures/bun_install.txt");
        let output = filter_bun_install(input, 0);

        // Should not contain progress noise
        assert!(!output.contains("Resolving"));
        assert!(!output.contains("Downloading"));
        // Should keep the installed packages and the count
        assert!(
            output.contains("+ zod@4.4.3") && output.contains("2 packages installed"),
            "install summary lost:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_bun_build() {
        let input = include_str!("../../../tests/fixtures/bun_build.txt");
        let output = filter_bun_build(input, 0);

        // The artifact line is the point of `bun build`; it must survive
        assert!(
            output.contains("index.js") && output.contains("72 bytes"),
            "build artifact was dropped:\n{}",
            output
        );
        assert!(
            output.contains("Bundled 1 module"),
            "build summary was dropped:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_bun_run_keeps_script_output() {
        let input = include_str!("../../../tests/fixtures/bun_run_script.txt");
        let output = filter_bun_run(input, 0);

        assert_eq!(output, "hello from script");
    }

    #[test]
    fn test_filter_bun_run_keeps_every_dollar_line() {
        // With `--silent` there is no echo to strip, so every `$` line is the
        // script's. Dropping one used to eat real output: bun writes the echo
        // to stderr and the script to stdout, and stdout lands first.
        let input = "$HOME is not set\n$ docker build .\ndone\n";
        let output = filter_bun_run(input, 0);

        assert_eq!(output, "$HOME is not set\n$ docker build .\ndone");
    }

    #[test]
    fn test_bun_run_passes_silent_before_script() {
        // `--silent` is a `bun run` flag: after the script name it would be
        // forwarded to the script instead.
        let argv = run_args("dev", &["--watch".to_string()]);
        assert_eq!(argv, vec!["run", "--silent", "dev", "--watch"]);
    }

    #[test]
    fn test_filter_bun_run_empty() {
        let output = filter_bun_run("\n\n\n", 0);
        assert_eq!(output, "ok");
    }

    #[test]
    fn test_filter_bun_run_empty_after_crash() {
        let output = filter_bun_run("\n\n\n", 1);
        assert_eq!(output, "bun run exited with code 1");
    }

    #[test]
    fn test_filter_bun_install_empty() {
        let output = filter_bun_install("\n\n", 0);
        assert_eq!(output, "ok");
    }

    #[test]
    fn test_filter_bun_build_empty() {
        let output = filter_bun_build("", 0);
        assert_eq!(output, "ok");
    }

    #[test]
    fn test_filter_bun_test_real_output() {
        let input = include_str!("../../../tests/fixtures/bun_test_real.txt");
        let output = filter_bun_test(input, 1);

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
        let output = filter_bun_test(input, 1);

        assert!(
            output.contains("FAILURES:"),
            "Missing FAILURES for TTY format:\n{}",
            output
        );
        assert!(
            output.contains("Expected:"),
            "Missing error context for TTY format:\n{}",
            output
        );
        assert!(
            output.contains("1 pass"),
            "Missing summary for TTY format:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_bun_test_load_failure_not_ok() {
        // A module/syntax error aborts before any test runs — no markers, no
        // summary. Must NOT collapse to a misleading "ok"; the error must survive.
        let input = "bun test v1.1.38 (abc)\n\nerror: Cannot find module 'foo' from 'test.ts'\n      at resolve (bun:internal)\n";
        let output = filter_bun_test(input, 1);
        assert_ne!(
            output, "ok",
            "load failure must not be reported as ok:\n{}",
            output
        );
        assert!(
            output.contains("Cannot find module"),
            "error text must survive:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_bun_test_app_log_fail_prefix_no_false_failure() {
        // An app logging a line starting with "FAIL..." during a passing test
        // must not fabricate a failure — "FAIL" is not a bun marker.
        let input = "bun test v1.1.38 (abc)\n\napp.test.ts:\nFAILED to connect to db, retrying...\n(pass) connects after retry [1.00ms]\n\n 1 pass\n 0 fail\n 1 expect() calls\nRan 1 tests across 1 files. [5.00ms]\n";
        let output = filter_bun_test(input, 0);
        assert!(
            !output.contains("FAILURES:"),
            "must not fabricate failures from app logs:\n{}",
            output
        );
        assert!(output.contains("1 pass"), "summary must survive:\n{}", output);
    }

    #[test]
    fn test_filter_bun_test_app_log_checkmark_preserves_context() {
        // An app log starting with ✓ (no timing suffix) must not be mistaken for
        // a pass marker and clear the buffered error context of a real failure.
        let input = "bun test v1.1.38 (abc)\n\napp.test.ts:\n  Expected: 1\n  Received: 2\n✓ cache refreshed after retry\n(fail) computes value [1.00ms]\n\n 0 pass\n 1 fail\n 1 expect() calls\nRan 1 tests across 1 files. [3.00ms]\n";
        let output = filter_bun_test(input, 1);
        assert!(
            output.contains("FAILURES:"),
            "real failure must be reported:\n{}",
            output
        );
        assert!(
            output.contains("Expected:") && output.contains("Received:"),
            "error context must be preserved:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_bun_test_app_log_pass_word_not_summary() {
        // A console line like "5 passengers ..." must not be misread as the
        // "N pass" summary line (word-boundary bug).
        let input = "bun test v1.1.38 (abc)\n\napp.test.ts:\n  Expected: 5\n  Received: 3\n5 passengers boarded before the fail\n(fail) boarding count [1.00ms]\n\n 0 pass\n 1 fail\n 1 expect() calls\nRan 1 tests across 1 files. [2.00ms]\n";
        let output = filter_bun_test(input, 1);
        assert!(
            output.contains("Expected:") && output.contains("Received:"),
            "context must be preserved, not misfiled as summary:\n{}",
            output
        );
        assert!(
            output.contains("0 pass") && output.contains("1 fail"),
            "real summary must survive:\n{}",
            output
        );
    }
}
