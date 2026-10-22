//! `finance bench`: how well do the rules and the AI categorize a labeled set of
//! made-up descriptions (bench/descriptions.toml)?
//!
//! The AI is asked about every case, even ones a rule already matches, so its score
//! says how good the model is on its own. Rule coverage is reported separately.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::ai::Ollama;
use crate::money::Money;
use crate::rules::Rules;

#[derive(Deserialize)]
struct BenchFile {
    #[serde(rename = "case")]
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    description: String,
    amount: String,
    category: String,
    split: String,
}

/// Runs the cases in `split` ("dev", "test", or every case when `None`).
pub fn run(rules: &Rules, ai: Option<&Ollama>, file: &Path, split: Option<&str>) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let bench: BenchFile = toml::from_str(&text).with_context(|| format!("in {}", file.display()))?;
    let mut cases = Vec::new();
    for case in &bench.cases {
        if !rules.categories.contains(&case.category) {
            bail!(
                "case {:?} uses category {:?}, which is not in rules.toml",
                case.description,
                case.category
            );
        }
        let amount = Money::parse(&case.amount)
            .with_context(|| format!("case {:?} has a bad amount {:?}", case.description, case.amount))?;
        if split.is_none_or(|s| s == case.split) {
            cases.push((case, amount));
        }
    }
    println!("{} cases{}", cases.len(), split.map(|s| format!(" ({s})")).unwrap_or_default());
    report_rules(rules, &cases);
    if let Some(ai) = ai {
        report_ai(rules, ai, &cases)?;
    }
    Ok(())
}

fn report_rules(rules: &Rules, cases: &[(&Case, Money)]) {
    let matched: Vec<_> =
        cases.iter().filter_map(|(c, _)| rules.category_for(&c.description).map(|got| (c, got))).collect();
    let correct = matched.iter().filter(|(c, got)| *got == c.category).count();
    println!("rules: match {}/{} cases, {} of those right", matched.len(), cases.len(), correct);
    for (c, got) in matched.iter().filter(|(c, got)| *got != c.category) {
        println!("   rule x {} -> {got} (want {})", c.description, c.category);
    }
}

fn report_ai(rules: &Rules, ai: &Ollama, cases: &[(&Case, Money)]) -> Result<()> {
    ai.categorize("PADARIA", Money(-100), rules)?; // load the model before timing
    let start = Instant::now();
    let mut correct = 0;
    // What the app would do: the rule's answer when one matches, otherwise the AI's.
    let mut pipeline_correct = 0;
    let mut confusions: BTreeMap<(String, String), Vec<&str>> = BTreeMap::new();
    for (case, amount) in cases {
        let got = ai.categorize(&case.description, *amount, rules)?.unwrap_or_else(|| "(no answer)".into());
        let final_answer = rules.category_for(&case.description).unwrap_or(&got);
        if final_answer == case.category {
            pipeline_correct += 1;
        }
        if got == case.category {
            correct += 1;
        } else {
            confusions.entry((case.category.clone(), got)).or_default().push(&case.description);
        }
    }
    let n = cases.len().max(1);
    let per_call = start.elapsed().as_secs_f64() / n as f64;
    let percent = |k: usize| k as f64 / n as f64 * 100.0;
    println!(
        "AI ({}): {correct}/{} right ({:.0}%), {per_call:.1} s per call",
        ai.model,
        cases.len(),
        percent(correct)
    );
    println!("rules then AI: {pipeline_correct}/{} right ({:.0}%)", cases.len(), percent(pipeline_correct));
    for ((want, got), descriptions) in &confusions {
        println!("   {want} -> {got}: {}", descriptions.join(", "));
    }
    Ok(())
}
