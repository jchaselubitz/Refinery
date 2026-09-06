//! Rendering a [`RefinedPrompt`] as one block of text a person can copy.
//!
//! The wire form is structured so destinations can pick the parts they need.
//! A person pasting into a chat or a ticket wants prose, so this lays the
//! parts out in Markdown in the order they read best and skips empty ones.

use crate::domain::{PromptReference, RefinedPrompt};

/// Render the whole prompt as Markdown.
pub fn markdown(prompt: &RefinedPrompt) -> String {
    let mut out = String::new();
    out.push_str("# ");
    out.push_str(prompt.title.trim());
    out.push_str("\n\n");

    section(&mut out, "Objective", prompt.objective.trim());
    section(&mut out, "Prompt", prompt.prompt.trim());
    list(&mut out, "Context", &prompt.context);
    list(&mut out, "Requirements", &prompt.requirements);
    list(&mut out, "Constraints", &prompt.constraints);
    list(&mut out, "Acceptance criteria", &prompt.acceptance_criteria);
    if !prompt.references.is_empty() {
        let items: Vec<String> = prompt.references.iter().map(reference).collect();
        list(&mut out, "References", &items);
    }
    list(&mut out, "Assumptions", &prompt.assumptions);
    list(
        &mut out,
        "Unresolved questions",
        &prompt.unresolved_questions,
    );

    out.trim_end().to_owned() + "\n"
}

fn section(out: &mut String, heading: &str, body: &str) {
    if body.is_empty() {
        return;
    }
    out.push_str("## ");
    out.push_str(heading);
    out.push_str("\n\n");
    out.push_str(body);
    out.push_str("\n\n");
}

fn list(out: &mut String, heading: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str("## ");
    out.push_str(heading);
    out.push_str("\n\n");
    for item in items {
        out.push_str("- ");
        out.push_str(item.trim());
        out.push('\n');
    }
    out.push('\n');
}

fn reference(reference: &PromptReference) -> String {
    let (text, note) = match reference {
        PromptReference::RepositoryPath { path, note } => (format!("`{path}`"), note),
        PromptReference::Attachment {
            attachment_id,
            note,
        } => (format!("attachment {attachment_id}"), note),
        PromptReference::External { value, note } => (value.clone(), note),
    };
    match note {
        Some(note) if !note.trim().is_empty() => format!("{text} — {}", note.trim()),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SchemaVersion;

    fn prompt() -> RefinedPrompt {
        RefinedPrompt {
            schema_version: SchemaVersion::CURRENT,
            title: "Resumable importer".into(),
            prompt: "Make the importer resume.".into(),
            objective: "Never restart from zero.".into(),
            context: vec![],
            requirements: vec!["Persist progress per batch".into()],
            constraints: vec![],
            acceptance_criteria: vec!["Interrupting and restarting continues".into()],
            references: vec![PromptReference::RepositoryPath {
                path: "src/import.rs".into(),
                note: Some("the loop".into()),
            }],
            assumptions: vec![],
            unresolved_questions: vec![],
        }
    }

    #[test]
    fn empty_sections_are_omitted_and_order_is_stable() {
        let text = markdown(&prompt());
        assert_eq!(
            text,
            "# Resumable importer\n\n\
             ## Objective\n\nNever restart from zero.\n\n\
             ## Prompt\n\nMake the importer resume.\n\n\
             ## Requirements\n\n- Persist progress per batch\n\n\
             ## Acceptance criteria\n\n- Interrupting and restarting continues\n\n\
             ## References\n\n- `src/import.rs` — the loop\n"
        );
        assert!(!text.contains("Context"));
        assert!(!text.contains("Assumptions"));
    }
}
