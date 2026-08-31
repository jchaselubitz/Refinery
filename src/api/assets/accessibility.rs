//! A deterministic accessibility check over the embedded interface.
//!
//! M8's exit criteria ask the interface's forms to pass an accessibility
//! check, and a check that only a person can run is one that stops being run.
//! So the rules that a static document *can* be held to are asserted here, in
//! the test suite, against the exact bytes the daemon serves.
//!
//! What this can prove: every control has a programmatic label, every button
//! and link has an accessible name, every `for`, `aria-labelledby`, and
//! `aria-describedby` resolves, ids are unique, grouped controls sit in a
//! `fieldset` with a `legend`, tables are captioned with scoped headers, and
//! each view has exactly one first-level heading that labels it.
//!
//! What it cannot prove: colour contrast, focus order under real interaction,
//! and whether the wording makes sense to a screen-reader user. Those stay
//! human judgements; this keeps the mechanical half from regressing.
//!
//! Templates are scanned like the rest of the document. Their contents become
//! live nodes the moment a question or a case row is rendered, so a control
//! that is unlabelled inside a template is an unlabelled control on screen.
//! Their text is checked for *association* rather than for wording, because
//! the words arrive at runtime.

use std::collections::{BTreeMap, BTreeSet};

/// One parsed tag.
#[derive(Debug, Clone)]
struct Tag {
    name: String,
    attributes: BTreeMap<String, String>,
    closing: bool,
    void: bool,
    /// Byte offset just past this tag, for reading element text.
    end: usize,
    /// Byte offset of this tag's `<`.
    start: usize,
}

impl Tag {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.get(name).map(String::as_str)
    }
    fn has(&self, name: &str) -> bool {
        self.attributes.contains_key(name)
    }
}

const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

/// Tokenize well-formed HTML into its tags.
///
/// This is not a general parser and does not try to be. The document it reads
/// is in this repository, is hand-written, and is checked by these very tests,
/// so quoted attributes and closed elements are a fair assumption. Anything it
/// cannot parse shows up as a failing assertion rather than a wrong pass.
fn tags(html: &str) -> Vec<Tag> {
    let bytes = html.as_bytes();
    let mut tags = Vec::new();
    let mut index = 0usize;
    while let Some(offset) = html[index..].find('<') {
        let start = index + offset;
        if html[start..].starts_with("<!--") {
            let end = html[start..].find("-->").map(|at| start + at + 3);
            index = end.unwrap_or(html.len());
            continue;
        }
        if html[start..].starts_with("<!") {
            index = html[start..]
                .find('>')
                .map(|at| start + at + 1)
                .unwrap_or(html.len());
            continue;
        }
        let Some(close) = html[start..].find('>').map(|at| start + at) else {
            break;
        };
        let inner = &html[start + 1..close];
        index = close + 1;
        if inner.is_empty() {
            continue;
        }
        let closing = inner.starts_with('/');
        let inner = inner.trim_start_matches('/').trim_end_matches('/');
        let mut parts = inner.splitn(2, |c: char| c.is_whitespace());
        let name = parts.next().unwrap_or_default().to_ascii_lowercase();
        let attributes = parse_attributes(parts.next().unwrap_or_default());
        let void = VOID_ELEMENTS.contains(&name.as_str()) || bytes[close - 1] == b'/';
        tags.push(Tag {
            name,
            attributes,
            closing,
            void,
            start,
            end: close + 1,
        });
    }
    tags
}

fn parse_attributes(source: &str) -> BTreeMap<String, String> {
    let mut attributes = BTreeMap::new();
    let mut rest = source.trim();
    while !rest.is_empty() {
        let name_end = rest
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(rest.len());
        let name = rest[..name_end].trim().to_ascii_lowercase();
        rest = rest[name_end..].trim_start();
        let value = if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            if let Some(quoted) = after.strip_prefix('"') {
                let end = quoted.find('"').unwrap_or(quoted.len());
                rest = &quoted[(end + 1).min(quoted.len())..];
                quoted[..end].to_owned()
            } else {
                let end = after.find(char::is_whitespace).unwrap_or(after.len());
                rest = &after[end..];
                after[..end].to_owned()
            }
        } else {
            String::new()
        };
        if !name.is_empty() {
            attributes.insert(name, value);
        }
        rest = rest.trim_start();
    }
    attributes
}

/// The text an element contains, with markup and whitespace removed.
fn element_text(html: &str, tags: &[Tag], index: usize) -> String {
    let open = &tags[index];
    if open.void {
        return String::new();
    }
    let mut depth = 1usize;
    let mut close_at = html.len();
    for tag in &tags[index + 1..] {
        if tag.name != open.name {
            continue;
        }
        if tag.closing {
            depth -= 1;
            if depth == 0 {
                close_at = tag.start;
                break;
            }
        } else if !tag.void {
            depth += 1;
        }
    }
    let inner = &html[open.end..close_at];
    let mut text = String::new();
    let mut in_tag = false;
    for character in inner.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(character),
            _ => {}
        }
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The stack of open element names at a given tag index.
fn ancestors(tags: &[Tag], index: usize) -> Vec<&str> {
    let mut stack: Vec<&str> = Vec::new();
    for tag in &tags[..index] {
        if tag.void {
            continue;
        }
        if tag.closing {
            if let Some(position) = stack.iter().rposition(|name| *name == tag.name) {
                stack.truncate(position);
            }
        } else {
            stack.push(&tag.name);
        }
    }
    stack
}

const CONTROLS: &[&str] = &["input", "select", "textarea"];
/// Controls that carry their own name or need none.
const SELF_LABELLING_INPUT_TYPES: &[&str] = &["submit", "button", "reset", "hidden", "image"];

fn shell() -> &'static str {
    std::str::from_utf8(super::INDEX.body).expect("the shell is UTF-8")
}

#[test]
fn the_document_declares_a_language_and_a_viewport() {
    let html = shell();
    let tags = tags(html);
    let root = tags
        .iter()
        .find(|tag| tag.name == "html" && !tag.closing)
        .expect("an html element");
    assert_eq!(
        root.attribute("lang"),
        Some("en"),
        "a document with no language leaves a screen reader guessing at pronunciation"
    );
    assert!(
        tags.iter()
            .any(|tag| tag.name == "meta" && tag.attribute("name") == Some("viewport")),
        "without a viewport the page cannot be zoomed on a small screen"
    );
    assert!(
        tags.iter().any(|tag| {
            tag.name == "a"
                && tag
                    .attribute("class")
                    .is_some_and(|c| c.contains("skip-link"))
        }),
        "a keyboard user needs a way past the navigation"
    );
}

#[test]
fn every_identifier_is_unique_and_every_reference_resolves() {
    let html = shell();
    let tags = tags(html);
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for tag in tags.iter().filter(|tag| !tag.closing) {
        if let Some(id) = tag.attribute("id") {
            assert!(ids.insert(id.to_owned()), "duplicate id `{id}`");
        }
    }
    for tag in tags.iter().filter(|tag| !tag.closing) {
        for attribute in [
            "for",
            "aria-labelledby",
            "aria-describedby",
            "aria-controls",
        ] {
            let Some(value) = tag.attribute(attribute) else {
                continue;
            };
            for reference in value.split_whitespace() {
                assert!(
                    ids.contains(reference),
                    "<{}> {attribute}=\"{reference}\" points at no element",
                    tag.name
                );
            }
        }
    }
}

/// The rule the answer form depends on: a control must be reachable by name.
///
/// Association is accepted three ways — a wrapping `<label>`, a `for`/`id`
/// pair, or an explicit `aria-label` — because the page uses the first for
/// cloned templates, where generated ids would have to be kept unique by
/// hand, and the second for the static forms, where an id is natural.
#[test]
fn every_form_control_has_a_programmatic_label() {
    let html = shell();
    let tags = tags(html);
    let labelled_ids: BTreeSet<&str> = tags
        .iter()
        .filter(|tag| tag.name == "label" && !tag.closing)
        .filter_map(|tag| tag.attribute("for"))
        .collect();

    let mut checked = 0usize;
    for (index, tag) in tags.iter().enumerate() {
        if tag.closing || !CONTROLS.contains(&tag.name.as_str()) {
            continue;
        }
        let input_type = tag.attribute("type").unwrap_or("text");
        if tag.name == "input" && SELF_LABELLING_INPUT_TYPES.contains(&input_type) {
            continue;
        }
        checked += 1;
        let wrapped = ancestors(&tags, index).contains(&"label");
        let by_id = tag
            .attribute("id")
            .is_some_and(|id| labelled_ids.contains(id));
        let aria = tag.has("aria-label") || tag.has("aria-labelledby");
        assert!(
            wrapped || by_id || aria,
            "the <{}{}> control at byte {} has no label a screen reader can announce",
            tag.name,
            tag.attribute("id")
                .map(|id| format!(" id={id}"))
                .unwrap_or_default(),
            tag.start
        );
    }
    assert!(checked >= 8, "only {checked} controls were checked");
}

#[test]
fn every_grouped_choice_sits_in_a_fieldset_that_names_the_group() {
    let html = shell();
    let tags = tags(html);
    for (index, tag) in tags.iter().enumerate() {
        if tag.closing || tag.name != "fieldset" {
            continue;
        }
        let legend = tags[index + 1..]
            .iter()
            .take_while(|candidate| !(candidate.name == "fieldset" && candidate.closing))
            .any(|candidate| candidate.name == "legend" && !candidate.closing);
        assert!(legend, "a fieldset at byte {} has no legend", tag.start);
    }
    // The radio and checkbox groups the answer form renders live in the choice
    // template, which is cloned into a fieldset. Assert that template exists so
    // the rule above is not vacuously satisfied by there being no groups.
    assert!(
        tags.iter()
            .any(|tag| tag.attribute("id") == Some("tpl-question-choice")),
        "the choice-question template is missing"
    );
}

#[test]
fn every_button_and_link_has_an_accessible_name() {
    let html = shell();
    let tags = tags(html);
    for (index, tag) in tags.iter().enumerate() {
        if tag.closing || !matches!(tag.name.as_str(), "button" | "a") {
            continue;
        }
        let named = !element_text(html, &tags, index).is_empty()
            || tag.has("aria-label")
            || tag.has("aria-labelledby");
        assert!(
            named,
            "a <{}> at byte {} has no accessible name",
            tag.name, tag.start
        );
    }
}

#[test]
fn every_table_is_captioned_and_its_row_headers_are_scoped() {
    let html = shell();
    let tags = tags(html);
    let mut tables = 0usize;
    for (index, tag) in tags.iter().enumerate() {
        if tag.closing || tag.name != "table" {
            continue;
        }
        tables += 1;
        let body: Vec<&Tag> = tags[index + 1..]
            .iter()
            .take_while(|candidate| !(candidate.name == "table" && candidate.closing))
            .collect();
        assert!(
            body.iter().any(|candidate| candidate.name == "caption"),
            "a table at byte {} has no caption",
            tag.start
        );
        for header in body.iter().filter(|c| c.name == "th" && !c.closing) {
            assert!(
                header.has("scope"),
                "a <th> at byte {} has no scope, so its association is a guess",
                header.start
            );
        }
    }
    assert!(tables >= 2, "expected the case and delivery tables");
}

/// Each view is a landmark section labelled by its own heading, and only one
/// view is ever visible, so exactly one first-level heading is exposed.
#[test]
fn every_view_is_labelled_by_exactly_one_first_level_heading() {
    let html = shell();
    let tags = tags(html);
    let mut views = 0usize;
    for (index, tag) in tags.iter().enumerate() {
        let is_view = tag.name == "section"
            && !tag.closing
            && tag
                .attribute("class")
                .is_some_and(|c| c.split_whitespace().any(|v| v == "view"));
        if !is_view {
            continue;
        }
        views += 1;
        let labelled_by = tag
            .attribute("aria-labelledby")
            .expect("a view section must name its heading");
        let mut depth = 1usize;
        let headings: Vec<&Tag> = tags[index + 1..]
            .iter()
            .take_while(|candidate| {
                if candidate.name == "section" {
                    if candidate.closing {
                        depth -= 1;
                    } else {
                        depth += 1;
                    }
                }
                depth > 0
            })
            .filter(|candidate| candidate.name == "h1" && !candidate.closing)
            .collect();
        assert_eq!(
            headings.len(),
            1,
            "the view labelled by `{labelled_by}` has {} first-level headings",
            headings.len()
        );
        assert_eq!(
            headings[0].attribute("id"),
            Some(labelled_by),
            "a view must be labelled by its own heading"
        );
    }
    assert_eq!(
        views, 6,
        "expected one section per route plus the locked view"
    );
}

#[test]
fn status_and_error_regions_announce_themselves() {
    let html = shell();
    let tags = tags(html);
    assert!(
        tags.iter()
            .any(|tag| tag.attribute("aria-live") == Some("polite")),
        "the interface needs a polite live region for action confirmations"
    );
    let alerts = tags
        .iter()
        .filter(|tag| tag.attribute("role") == Some("alert"))
        .count();
    assert!(
        alerts >= 3,
        "form and page errors must be announced, found {alerts} alert regions"
    );
}

/// The interface renders model-generated text. The one reliable way to keep
/// that text from becoming markup is to have no code path that could turn it
/// into markup, so the script is held to that rule mechanically.
#[test]
fn the_script_never_assigns_markup() {
    let script = std::str::from_utf8(
        super::ASSETS
            .iter()
            .find(|asset| asset.path == "/assets/app.js")
            .expect("the script is embedded")
            .body,
    )
    .expect("the script is UTF-8");
    for forbidden in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "new Function(",
    ] {
        assert!(
            !script.contains(forbidden),
            "`{forbidden}` would let caller- or model-supplied text become markup"
        );
    }
}
