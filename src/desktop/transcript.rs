//! Turning pasted text into a [`Transcript`].
//!
//! A person pastes whatever they have: a chat export with `User:` and
//! `Assistant:` prefixes, a Slack thread, or a single paragraph of feedback.
//! The parser recognises role prefixes at the start of a line and otherwise
//! treats the whole paste as one user message, so nothing a person pastes is
//! ever refused for its shape. Bounds are left to the request contract, which
//! reports every violation at once.

use crate::domain::{MessageRole, Transcript, TranscriptMessage};

/// Parse pasted text into an ordered transcript.
///
/// Returns `None` when the text holds nothing but whitespace.
pub fn parse(text: &str) -> Option<Transcript> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    let mut messages: Vec<(MessageRole, String)> = Vec::new();
    let mut leading = String::new();

    for line in text.lines() {
        match role_prefix(line) {
            Some((role, rest)) => messages.push((role, rest.to_owned())),
            None => match messages.last_mut() {
                Some((_, content)) => {
                    content.push('\n');
                    content.push_str(line);
                }
                None => {
                    leading.push_str(line);
                    leading.push('\n');
                }
            },
        }
    }

    // Text before the first role line belongs to the person who pasted it.
    if !leading.trim().is_empty() {
        messages.insert(0, (MessageRole::User, leading));
    }

    let messages = messages
        .into_iter()
        .filter_map(|(role, content)| {
            let content = content.trim().to_owned();
            (!content.is_empty()).then_some(TranscriptMessage {
                id: None,
                role,
                content,
                author: None,
                created_at: None,
            })
        })
        .collect::<Vec<_>>();

    if messages.is_empty() {
        None
    } else {
        Some(Transcript::new(messages))
    }
}

/// Recognise `Role: text` at the start of a line.
///
/// Markdown decoration around the label (`**User:**`, `> User:`, `- User:`)
/// is tolerated because chat exports rarely agree on one style.
fn role_prefix(line: &str) -> Option<(MessageRole, &str)> {
    let trimmed = line.trim_start_matches([' ', '\t', '>', '-', '*', '#', '_']);
    let (label, rest) = trimmed.split_once(':')?;
    let label = label
        .trim()
        .trim_matches(['*', '_', '[', ']', '(', ')'])
        .trim();
    if label.is_empty() || label.len() > 12 || label.contains(' ') {
        return None;
    }
    let role = match label.to_ascii_lowercase().as_str() {
        "user" | "human" | "me" | "person" | "customer" => MessageRole::User,
        "assistant" | "ai" | "model" | "agent" | "bot" | "claude" | "gemini" | "gpt" => {
            MessageRole::Assistant
        }
        "system" => MessageRole::System,
        "tool" => MessageRole::Tool,
        _ => return None,
    };
    Some((role, rest.trim_start_matches(['*', '_']).trim_start()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_one_user_message() {
        let transcript = parse("  The importer restarts from zero.\n\nAlways.  ").unwrap();
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].role, MessageRole::User);
        assert_eq!(
            transcript.messages[0].content,
            "The importer restarts from zero.\n\nAlways."
        );
    }

    #[test]
    fn role_prefixes_split_messages_and_keep_continuation_lines() {
        let text = "User: it breaks\nstill broken\n**Assistant:** no checkpoint\nSystem: be brief\nTool: ran";
        let transcript = parse(text).unwrap();
        let roles: Vec<_> = transcript.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::System,
                MessageRole::Tool
            ]
        );
        assert_eq!(transcript.messages[0].content, "it breaks\nstill broken");
        assert_eq!(transcript.messages[1].content, "no checkpoint");
    }

    #[test]
    fn text_before_the_first_role_line_is_from_the_user() {
        let transcript = parse("Context first.\nAI: reply").unwrap();
        assert_eq!(transcript.messages.len(), 2);
        assert_eq!(transcript.messages[0].role, MessageRole::User);
        assert_eq!(transcript.messages[0].content, "Context first.");
        assert_eq!(transcript.messages[1].role, MessageRole::Assistant);
    }

    #[test]
    fn ordinary_colons_do_not_start_messages() {
        let transcript = parse("Note: the build is red\nError: E0308 mismatched types").unwrap();
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].role, MessageRole::User);
    }

    #[test]
    fn whitespace_and_empty_role_lines_are_nothing() {
        assert!(parse("   \n\t").is_none());
        assert!(parse("User:\nAssistant:   ").is_none());
    }
}
