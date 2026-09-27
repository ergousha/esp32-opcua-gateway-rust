//! Check results, console output and the JSON summary.

use serde_json::{json, Value};

/// One assertion.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// The outcome of one phase.
#[derive(Debug, Clone)]
pub struct PhaseResult {
    pub name: String,
    pub checks: Vec<Check>,
    pub skipped: bool,
    pub note: String,
}

impl PhaseResult {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            checks: Vec::new(),
            skipped: false,
            note: String::new(),
        }
    }

    /// Records and prints a check; returns `ok` so callers can branch on it.
    pub fn check(&mut self, name: impl Into<String>, ok: bool, detail: impl Into<String>) -> bool {
        let check = Check {
            name: name.into(),
            ok,
            detail: detail.into(),
        };
        let status = if ok { "PASS" } else { "FAIL" };
        if check.detail.is_empty() {
            println!("    [{status}] {}", check.name);
        } else {
            println!("    [{status}] {} — {}", check.name, check.detail);
        }
        self.checks.push(check);
        ok
    }

    pub fn ok(&self) -> bool {
        self.skipped || self.checks.iter().all(|c| c.ok)
    }

    pub fn passed(&self) -> usize {
        self.checks.iter().filter(|c| c.ok).count()
    }
}

pub fn banner(text: &str) {
    let rule = "=".repeat(78);
    println!("\n{rule}\n{text}\n{rule}");
}

pub fn info(text: impl AsRef<str>) {
    println!("    · {}", text.as_ref());
}

/// Prints the summary and returns `(passed, total)`.
pub fn print_summary(results: &[PhaseResult]) -> (usize, usize) {
    banner("SUMMARY");
    let (mut passed, mut total) = (0, 0);
    for r in results {
        if r.skipped {
            println!("  SKIP  {} — {}", r.name, r.note);
            continue;
        }
        passed += r.passed();
        total += r.checks.len();
        let flag = if r.ok() { "PASS" } else { "FAIL" };
        println!("  {flag}  {}: {}/{}", r.name, r.passed(), r.checks.len());
        for c in r.checks.iter().filter(|c| !c.ok) {
            println!("          ✗ {} — {}", c.name, c.detail);
        }
    }
    println!("\n  {passed}/{total} checks passed");
    (passed, total)
}

/// The machine-readable summary written to the artifacts directory.
pub fn summary_json(thing: &str, endpoint: &str, results: &[PhaseResult]) -> Value {
    let (passed, total) = results
        .iter()
        .filter(|r| !r.skipped)
        .fold((0, 0), |(p, t), r| (p + r.passed(), t + r.checks.len()));
    json!({
        "thing": thing,
        "endpoint": endpoint,
        "phases": results.iter().map(|r| json!({
            "name": r.name,
            "skipped": r.skipped,
            "note": r.note,
            "checks": r.checks.iter().map(|c| json!({
                "name": c.name,
                "ok": c.ok,
                "detail": c.detail,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "passed": passed,
        "total": total,
    })
}
