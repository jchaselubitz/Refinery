//! Deterministic scoring for the versioned refinement-quality corpus.
//!
//! The evaluator is intentionally not another model. Each case declares the
//! source facts and observable qualities its candidate prompt must carry, and
//! this module measures those facts reproducibly. It is a regression baseline:
//! live model runs can replace a case's candidate output, then the same rubric
//! says exactly which quality moved.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{
    PromptReference, PromptValidationContext, RefinedPrompt, Transcript, ValidationReport,
};

/// A versioned evaluation-set manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationManifest {
    /// Manifest schema, independent of the wire-contract version.
    pub schema_version: u32,
    /// Human-readable immutable set version, for example `v1`.
    pub set_version: String,
    /// Lowest permitted mean across all case scores.
    pub minimum_average: f64,
    /// Lowest permitted mean for any individual case.
    pub minimum_case_score: f64,
    /// Cases in the set.
    pub cases: Vec<EvaluationCase>,
}

/// Paths and rubric for one evaluation case.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationCase {
    /// Stable case name.
    pub id: String,
    /// Transcript and questions, relative to the manifest.
    pub input: PathBuf,
    /// Repository fixture root, relative to the manifest.
    pub repository: PathBuf,
    /// Candidate refined prompt, relative to the manifest.
    pub output: PathBuf,
    /// Observable quality expectations.
    pub rubric: EvaluationRubric,
}

/// One case's transcript and interaction history.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationInput {
    /// The original conversation.
    pub transcript: Transcript,
    /// Questions the backend asked before producing the candidate.
    #[serde(default)]
    pub questions_asked: Vec<String>,
}

/// Terms and references behind the five scored dimensions.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationRubric {
    /// Source facts the output needs in order to stand alone.
    pub self_containment_terms: Vec<String>,
    /// Original-request facts the output must preserve.
    pub faithfulness_terms: Vec<String>,
    /// Claims that would contradict or invent source material.
    #[serde(default)]
    pub forbidden_claims: Vec<String>,
    /// Repository paths the output must cite and the fixture must contain.
    #[serde(default)]
    pub repository_paths: Vec<PathBuf>,
    /// Requirements or constraints that must be made explicit.
    pub explicitness_terms: Vec<String>,
    /// The useful number of questions for this case.
    pub expected_question_count: usize,
    /// Topics every useful question should cover.
    #[serde(default)]
    pub question_topics: Vec<String>,
}

/// Scores for one evaluation case, each bounded to `[0, 1]`.
#[derive(Debug, Clone, Serialize)]
pub struct CaseScores {
    /// Case identifier.
    pub id: String,
    /// Whether the prompt carries enough context to execute alone.
    pub self_containment: f64,
    /// Whether it preserves the source request without invented claims.
    pub faithfulness: f64,
    /// Whether repository claims point to supplied fixture material.
    pub grounding: f64,
    /// Whether requirements and constraints are stated directly.
    pub explicitness: f64,
    /// Whether clarification is useful and no broader than necessary.
    pub question_economy: f64,
    /// Arithmetic mean of the five dimensions.
    pub overall: f64,
}

/// The complete report a scripted run prints or serializes.
#[derive(Debug, Serialize)]
pub struct EvaluationReport {
    /// Evaluation-set version.
    pub set_version: String,
    /// Per-case scores.
    pub cases: Vec<CaseScores>,
    /// Mean of every case's overall score.
    pub average: f64,
    /// Threshold configured by the set.
    pub minimum_average: f64,
    /// Per-case threshold configured by the set.
    pub minimum_case_score: f64,
    /// Whether both thresholds were met.
    pub passed: bool,
}

/// Load and score an evaluation set rooted at a manifest directory.
pub fn run(root: &Path) -> Result<EvaluationReport, String> {
    let manifest_path = root.join("manifest.json");
    let manifest: EvaluationManifest = read_json(&manifest_path)?;
    if manifest.schema_version != 1 {
        return Err(format!(
            "{} uses unsupported evaluation schema {}",
            manifest_path.display(),
            manifest.schema_version
        ));
    }
    if manifest.cases.is_empty() {
        return Err("the evaluation set has no cases".to_owned());
    }
    if !(0.0..=1.0).contains(&manifest.minimum_average)
        || !(0.0..=1.0).contains(&manifest.minimum_case_score)
    {
        return Err("evaluation thresholds must be between 0 and 1".to_owned());
    }

    let canonical_root = std::fs::canonicalize(root)
        .map_err(|error| format!("opening evaluation root {}: {error}", root.display()))?;
    let mut scores = Vec::with_capacity(manifest.cases.len());
    for case in &manifest.cases {
        scores.push(score_case(&canonical_root, case)?);
    }
    let average = scores.iter().map(|score| score.overall).sum::<f64>() / scores.len() as f64;
    let passed = average >= manifest.minimum_average
        && scores
            .iter()
            .all(|score| score.overall >= manifest.minimum_case_score);
    Ok(EvaluationReport {
        set_version: manifest.set_version,
        cases: scores,
        average,
        minimum_average: manifest.minimum_average,
        minimum_case_score: manifest.minimum_case_score,
        passed,
    })
}

fn score_case(root: &Path, case: &EvaluationCase) -> Result<CaseScores, String> {
    if case.id.trim().is_empty() {
        return Err("an evaluation case has a blank id".to_owned());
    }
    let input_path = resolve(root, &case.input)?;
    let output_path = resolve(root, &case.output)?;
    let repository = resolve(root, &case.repository)?;
    if !repository.is_dir() {
        return Err(format!(
            "case {} repository fixture {} is not a directory",
            case.id,
            repository.display()
        ));
    }
    let input: EvaluationInput = read_json(&input_path)?;
    let mut transcript_report = ValidationReport::new();
    input
        .transcript
        .validate_into(&mut transcript_report, "transcript");
    transcript_report
        .into_result()
        .map_err(|report| format!("case {} transcript: {report}", case.id))?;
    let output: RefinedPrompt = read_json(&output_path)?;
    output
        .validate(&PromptValidationContext::default())
        .map_err(|report| format!("case {} output: {report}", case.id))?;

    let source_text = input
        .transcript
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    let output_text = prompt_text(&output).to_lowercase();

    for term in &case.rubric.faithfulness_terms {
        if !source_text.contains(&term.to_lowercase()) {
            return Err(format!(
                "case {} faithfulness term {term:?} is not present in its transcript",
                case.id
            ));
        }
    }

    let self_containment = coverage(&output_text, &case.rubric.self_containment_terms);
    let faithful_facts = coverage(&output_text, &case.rubric.faithfulness_terms);
    let contradictions = case
        .rubric
        .forbidden_claims
        .iter()
        .filter(|claim| output_text.contains(&claim.to_lowercase()))
        .count();
    let faithfulness = if contradictions == 0 {
        faithful_facts
    } else {
        0.0
    };
    let grounding = grounding_score(root, &repository, &output, case)?;
    let explicitness = coverage(&output_text, &case.rubric.explicitness_terms);
    let question_economy = question_score(&input, &case.rubric);
    let overall =
        (self_containment + faithfulness + grounding + explicitness + question_economy) / 5.0;

    Ok(CaseScores {
        id: case.id.clone(),
        self_containment,
        faithfulness,
        grounding,
        explicitness,
        question_economy,
        overall,
    })
}

fn grounding_score(
    root: &Path,
    repository: &Path,
    output: &RefinedPrompt,
    case: &EvaluationCase,
) -> Result<f64, String> {
    if case.rubric.repository_paths.is_empty() {
        return Ok(1.0);
    }
    let cited: Vec<&str> = output
        .references
        .iter()
        .filter_map(|reference| match reference {
            PromptReference::RepositoryPath { path, .. } => Some(path.as_str()),
            PromptReference::Attachment { .. } | PromptReference::External { .. } => None,
        })
        .collect();
    let mut matched = 0;
    for expected in &case.rubric.repository_paths {
        let fixture = resolve(repository, expected)?;
        if !fixture.is_file() || !fixture.starts_with(root) {
            return Err(format!(
                "case {} grounding path {} is missing from its repository fixture",
                case.id,
                expected.display()
            ));
        }
        if cited.iter().any(|path| Path::new(path) == expected) {
            matched += 1;
        }
    }
    Ok(matched as f64 / case.rubric.repository_paths.len() as f64)
}

fn question_score(input: &EvaluationInput, rubric: &EvaluationRubric) -> f64 {
    let count_score =
        usize::from(input.questions_asked.len() == rubric.expected_question_count) as f64;
    let questions = input.questions_asked.join("\n").to_lowercase();
    let topic_score = coverage(&questions, &rubric.question_topics);
    (count_score + topic_score) / 2.0
}

fn coverage(haystack: &str, terms: &[String]) -> f64 {
    if terms.is_empty() {
        return 1.0;
    }
    let haystack = haystack.to_lowercase();
    terms
        .iter()
        .filter(|term| haystack.contains(&term.to_lowercase()))
        .count() as f64
        / terms.len() as f64
}

fn prompt_text(prompt: &RefinedPrompt) -> String {
    let mut parts = vec![
        prompt.title.as_str(),
        prompt.prompt.as_str(),
        prompt.objective.as_str(),
    ];
    for values in [
        &prompt.context,
        &prompt.requirements,
        &prompt.constraints,
        &prompt.acceptance_criteria,
        &prompt.assumptions,
        &prompt.unresolved_questions,
    ] {
        parts.extend(values.iter().map(String::as_str));
    }
    for reference in &prompt.references {
        match reference {
            PromptReference::RepositoryPath { path, note } => {
                parts.push(path);
                parts.extend(note.as_deref());
            }
            PromptReference::Attachment { note, .. } => parts.extend(note.as_deref()),
            PromptReference::External { value, note } => {
                parts.push(value);
                parts.extend(note.as_deref());
            }
        }
    }
    parts.join("\n")
}

fn resolve(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!(
            "evaluation path {} must stay relative to its root",
            relative.display()
        ));
    }
    let joined = root.join(relative);
    let canonical = std::fs::canonicalize(&joined)
        .map_err(|error| format!("opening {}: {error}", joined.display()))?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "evaluation path {} escapes its root",
            relative.display()
        ));
    }
    Ok(canonical)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let input = std::fs::read_to_string(path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    serde_json::from_str(&input).map_err(|error| format!("parsing {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_v1_set_meets_its_baseline() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("evaluations/v1");
        let report = run(&root).expect("the evaluation set is valid");
        assert!(report.passed, "{report:#?}");
        assert_eq!(report.cases.len(), 3);
    }

    #[test]
    fn coverage_is_case_insensitive_and_empty_expectations_are_full_credit() {
        assert_eq!(
            coverage("Retry only GET", &["retry".into(), "get".into()]),
            1.0
        );
        assert_eq!(coverage("anything", &[]), 1.0);
    }

    #[test]
    fn traversal_is_refused_before_a_fixture_is_opened() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("evaluations/v1");
        let canonical = std::fs::canonicalize(root).unwrap();
        assert!(resolve(&canonical, Path::new("../secret")).is_err());
    }
}
