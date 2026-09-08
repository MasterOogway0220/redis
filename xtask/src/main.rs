//! Repository checks that fail CI.
//!
//! Run with `cargo xtask <check>`. See `.cargo/config.toml` for the alias.
//!
//! These enforce sections 4 and 9 of `kvlite-packaging.md`. Rules stated in a
//! document decay; rules that fail the build do not.

use std::collections::BTreeSet;
use std::process::{Command, ExitCode};

/// The complete, intended dependency graph, transitively.
///
/// Anything a crate reaches that is not listed here is a violation. Adding a line
/// is a deliberate architectural act and shows up in review as one.
///
/// Rule 1 lives in the `kvlite-core` row: it may reach `kvlite-api` and nothing else.
/// Rule 2 lives in the shape of the whole table: no row names a crate above it.
const ALLOWED: &[(&str, &[&str])] = &[
    ("kvlite-api", &[]),
    ("kvlite-resp", &[]),
    ("kvlite-core", &["kvlite-api"]),
    ("kvlite-server", &["kvlite-api", "kvlite-core", "kvlite-resp", "tokio"]),
    ("kvlite-testing", &["kvlite-api", "kvlite-core", "kvlite-resp", "kvlite-server", "tokio"]),
    ("kvlite", &["kvlite-api", "kvlite-core", "kvlite-resp", "kvlite-server", "tokio"]),
];

/// Third-party crates that may appear anywhere `tokio` is allowed.
///
/// These are tokio's own transitive dependencies. They are listed rather than
/// waved through so that a *new* one appearing shows up as a build failure and gets
/// looked at, which is the entire point of the exercise.
const TOKIO_TREE: &[&str] = &[
    "bytes",
    "errno",
    "libc",
    "mio",
    "pin-project-lite",
    "proc-macro2",
    "quote",
    "signal-hook-registry",
    "socket2",
    "syn",
    "tokio-macros",
    "unicode-ident",
    "wasi",
    "windows-link",
    "windows-sys",
    "windows-targets",
    "windows_aarch64_gnullvm",
    "windows_aarch64_msvc",
    "windows_i686_gnu",
    "windows_i686_gnullvm",
    "windows_i686_msvc",
    "windows_x86_64_gnu",
    "windows_x86_64_gnullvm",
    "windows_x86_64_msvc",
];

/// Crates that must build without `std`, because other people build against them.
const NO_STD: &[&str] = &["kvlite-api", "kvlite-resp"];

/// Crates whose public surface is tracked in `public-api.txt` (packaging spec §5).
const TRACKED_API: &[&str] =
    &["kvlite-api", "kvlite-resp", "kvlite-core", "kvlite-server", "kvlite-testing"];

/// What `cargo public-api` is told to leave out.
///
/// Auto-trait, blanket and derived impls are produced by the compiler, so they move
/// when the toolchain moves rather than when our API does. Tracking them would turn
/// a rustc release into a red build with no cause in this repository — and this runs
/// on nightly, which moves daily.
const API_OMIT: &str = "blanket-impls,auto-trait-impls,auto-derived-impls";

fn main() -> ExitCode {
    let checks: Vec<String> = std::env::args().skip(1).collect();
    let requested: Vec<&str> = checks.iter().map(String::as_str).collect();

    let failures = match requested.as_slice() {
        [] | ["all"] => check_layering() + check_no_std() + check_isolated(),
        ["check-layering"] => check_layering(),
        ["check-no-std"] => check_no_std(),
        ["check-isolated"] => check_isolated(),
        ["public-api"] => regenerate_public_api(),
        [other, ..] => {
            eprintln!("unknown check: {other}");
            eprintln!(
                "usage: cargo xtask [all|check-layering|check-no-std|check-isolated|public-api]"
            );
            return ExitCode::FAILURE;
        }
    };

    if failures == 0 {
        println!("\nall checks passed");
        ExitCode::SUCCESS
    } else {
        eprintln!("\n{failures} check(s) failed");
        ExitCode::FAILURE
    }
}

// ---- Rules 1 and 2 --------------------------------------------------------

fn check_layering() -> usize {
    println!("== dependency rules (packaging spec section 4) ==");
    let mut failures = 0;

    for (krate, allowed) in ALLOWED {
        let Some(actual) = transitive_dependencies(krate) else {
            eprintln!("  FAIL {krate}: could not read its dependency tree");
            failures += 1;
            continue;
        };

        let permitted: BTreeSet<&str> = allowed
            .iter()
            .copied()
            .chain(allowed.contains(&"tokio").then_some(TOKIO_TREE).into_iter().flatten().copied())
            .collect();

        let violations: Vec<&String> =
            actual.iter().filter(|dep| !permitted.contains(dep.as_str())).collect();

        if violations.is_empty() {
            let third_party = actual.iter().filter(|dep| !dep.starts_with("kvlite")).count();
            println!("  ok   {krate} ({third_party} third-party, transitively)");
        } else {
            for dep in violations {
                eprintln!("  FAIL {krate} must not depend on {dep}");
            }
            failures += 1;
        }
    }
    failures
}

/// Every crate reachable from `krate` through normal dependencies — not dev, not
/// build. Transitive, because a transitive dependency is still a dependency, and
/// still a reason somebody cannot adopt the crate.
fn transitive_dependencies(krate: &str) -> Option<BTreeSet<String>> {
    let output = Command::new(cargo())
        .args(["tree", "--package", krate, "--edges", "normal", "--prefix", "none", "--no-dedupe"])
        .output()
        .ok()?;

    if !output.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        return None;
    }

    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|name| *name != krate)
            .map(str::to_string)
            .collect(),
    )
}

// ---- the no_std floor (packaging spec section 9) --------------------------

fn check_no_std() -> usize {
    println!("\n== no_std support (packaging spec section 9) ==");

    // Building for a target with no std at all is the only honest test: a crate can
    // say `#![no_std]` and still pull in a dependency that needs it.
    const TARGET: &str = "thumbv7em-none-eabihf";

    if !target_installed(TARGET) {
        println!("  skip  {TARGET} is not installed (rustup target add {TARGET})");
        return 0;
    }

    let mut failures = 0;
    for krate in NO_STD {
        let status =
            Command::new(cargo()).args(["build", "--package", krate, "--target", TARGET]).status();

        match status {
            Ok(status) if status.success() => println!("  ok   {krate} builds for {TARGET}"),
            _ => {
                eprintln!("  FAIL {krate} does not build for {TARGET}");
                failures += 1;
            }
        }
    }
    failures
}

fn target_installed(target: &str) -> bool {
    Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).lines().any(|line| line.trim() == target))
        .unwrap_or(false)
}

// ---- Rule 3 ---------------------------------------------------------------

fn check_isolated() -> usize {
    println!("\n== publishable in isolation (packaging spec section 4, rule 3) ==");

    // `cargo publish --dry-run` packages the crate and builds it from the packaged
    // form. That is what catches a path dependency with no version, and a file that
    // is in the working tree but not in the package.
    //
    // It does NOT catch feature unification, which needs `cargo hack
    // --feature-powerset` from outside the workspace. That runs in CI, where the
    // tool is available; see .github/workflows/ci.yml.
    let mut failures = 0;

    for (krate, _) in ALLOWED {
        let output = Command::new(cargo())
            .args(["publish", "--dry-run", "--allow-dirty", "--package", krate])
            .output();

        let Ok(output) = output else {
            eprintln!("  FAIL {krate}: could not run cargo publish");
            failures += 1;
            continue;
        };

        if output.status.success() {
            println!("  ok   {krate} packages cleanly");
            continue;
        }

        // Until the first release, a crate with internal dependencies cannot be
        // dry-run published, because those dependencies are not on the registry yet.
        // That is the expected state, not a packaging bug — and the moment the first
        // release goes out in dependency order, these turn green on their own.
        // Anything else is a real failure and must stay one.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no matching package named `kvlite") {
            println!("  pend {krate} awaits its kvlite dependencies being published");
        } else {
            eprintln!("  FAIL {krate} is not publishable on its own");
            eprintln!("{}", indent(&stderr));
            failures += 1;
        }
    }
    failures
}

fn indent(text: &str) -> String {
    text.lines().map(|line| format!("       {line}\n")).collect()
}

// ---- the public API snapshots (packaging spec section 5) ------------------

/// Rewrites every `public-api.txt` with exactly the flags CI compares against.
///
/// CI diffs its own regeneration against these files, so the flags have to match.
/// Keeping them here rather than in the workflow means there is one place to change
/// them, and no way for the two to drift apart.
fn regenerate_public_api() -> usize {
    println!("== regenerating public API snapshots (packaging spec section 5) ==");
    let mut failures = 0;

    for krate in TRACKED_API {
        let path = format!("crates/{krate}/public-api.txt");

        let output = Command::new(cargo())
            .args(["public-api", "--package", krate, "--omit", API_OMIT])
            .output();

        match output {
            Ok(output) if output.status.success() => {
                if let Err(err) = std::fs::write(&path, &output.stdout) {
                    eprintln!("  FAIL {krate}: could not write {path}: {err}");
                    failures += 1;
                    continue;
                }
                let lines = output.stdout.iter().filter(|byte| **byte == b'\n').count();
                println!("  ok   {path} ({lines} items)");
            }
            Ok(output) => {
                eprintln!("  FAIL {krate}: cargo public-api failed");
                eprintln!("{}", indent(&String::from_utf8_lossy(&output.stderr)));
                failures += 1;
            }
            Err(_) => {
                eprintln!("  FAIL {krate}: cargo-public-api is not installed");
                eprintln!("       cargo install cargo-public-api --locked");
                failures += 1;
            }
        }
    }
    failures
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}
