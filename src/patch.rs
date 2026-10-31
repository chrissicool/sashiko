// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::{Result, anyhow};
use mail_parser::{HeaderValue, Message, MessageParser, PartType};
use regex::Regex;
use std::sync::OnceLock;

/// How `parse_email` reads a mail. See designs/DESIGN_RICH_MAIL_INGESTION.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailReading {
    /// As `git format-patch` sends it: the text body, with any diff it
    /// attaches, is the one patch a mail carries, and a reply is discussion.
    FormatPatch,
    /// As a list without a style guide gets it: a diff its sender wrote is a
    /// patch even in a reply, a batch of changesets in one mail is a series,
    /// and every attachment that carries a diff is read on its own.
    Rich,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct PatchsetMetadata {
    pub message_id: String,
    pub subject: String,
    pub author: String,
    pub date: i64,
    pub received_date: Option<i64>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub index: u32,
    pub total: u32,
    pub to: String,
    pub cc: String,
    pub is_patch_or_cover: bool,
    /// A reply carrying a diff its sender wrote, which the rich reading takes
    /// as a patch of its own unless it is the author's next version.
    pub is_reply_patch: bool,
    pub version: Option<u32>,
    pub body: String,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct Patch {
    pub message_id: String,
    pub body: String,
    pub diff: String,
    pub part_index: u32,
    /// The `Subject:` the changeset carried, when it arrived as a
    /// format-patch block inside a larger mail. `None` leaves the patch
    /// named after the mail it came in.
    pub subject: Option<String>,
}

pub fn extract_received_date(raw_email: &[u8]) -> Option<i64> {
    let header_end = raw_email
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(raw_email.len());
    let headers_bytes = &raw_email[..header_end];
    let headers_str = String::from_utf8_lossy(headers_bytes);

    let mut current_header = String::new();
    let mut in_received = false;

    for line in headers_str.lines() {
        if line.starts_with("Received:") {
            if in_received {
                break;
            }
            in_received = true;
            current_header.push_str(line);
        } else if in_received && (line.starts_with(' ') || line.starts_with('\t')) {
            current_header.push_str(line);
        } else if in_received {
            break;
        }
    }

    if current_header.is_empty() {
        return None;
    }

    if let Some(semi_idx) = current_header.rfind(';') {
        let date_str = current_header[semi_idx + 1..].trim();
        if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(date_str) {
            return Some(dt.timestamp());
        }
    }

    None
}

pub fn has_unquoted_diff(text: &str) -> bool {
    let mut has_minus = false;
    let mut has_plus = false;
    let mut has_hunk = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("diff --git ") {
            return true;
        }
        if line.starts_with("--- ") {
            has_minus = true;
        } else if line.starts_with("+++ ") {
            has_plus = true;
        } else if line.starts_with("@@ -") {
            has_hunk = true;
        }
        if has_minus && has_plus && has_hunk {
            return true;
        }
    }
    false
}

/// Returns true if `line` is a `git format-patch` mbox separator line of the
/// form `From <hex-sha> <ctime-timestamp>` (e.g.
/// `From 0123456789abcdef0123456789abcdef01234567 Mon Sep 17 00:00:00 2001`).
fn is_format_patch_mbox_from(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("From ") else {
        return false;
    };
    let Some((sha, tail)) = rest.split_once(' ') else {
        return false;
    };
    if sha.len() < 7 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let b = tail.trim_end().as_bytes();
    let len = b.len();
    len >= 20 && b[len - 8] == b':' && b[len - 11] == b':' && b[len - 5] == b' '
}

/// Strips only a leading `git format-patch` `From <sha> <timestamp>` mbox
/// separator line when the patch appears at the very start of the message body,
/// leaving any subsequent `From:`, `Date:`, and `Subject:` lines at the top of
/// the body for `git mailinfo` (`git am`) to consume as in-body overrides.
fn strip_leading_mbox_from_line(text: &str) -> &str {
    let Some((first, rest)) = text.split_once('\n') else {
        return text;
    };
    if is_format_patch_mbox_from(first.trim_end_matches('\r')) {
        rest
    } else {
        text
    }
}

/// Strips a leading `git format-patch` mbox/RFC header block (`From <sha> ...`,
/// `From:`, `Date:`, `Subject:`, etc.) from an attached patch so `git am` does
/// not split on an embedded `From ` separator line when the attachment is
/// appended to the email body.
fn strip_format_patch_headers(text: &str) -> &str {
    let first_line = text
        .lines()
        .next()
        .unwrap_or_default()
        .trim_end_matches('\r');
    let starts_with_mbox_from = is_format_patch_mbox_from(first_line);
    let starts_with_header = ["from:", "date:", "subject:"]
        .iter()
        .any(|p| first_line.to_ascii_lowercase().starts_with(p));
    if !starts_with_mbox_from && !starts_with_header {
        return text;
    }

    let mut offset = 0;
    let mut line_idx = 0;
    let mut saw_patch_header = false;

    for raw_line in text.split_inclusive('\n') {
        let line = raw_line.trim_end_matches(['\r', '\n']);
        offset += raw_line.len();
        if line.is_empty() {
            if saw_patch_header && line_idx > 0 {
                return &text[offset..];
            }
            return text;
        }
        if line_idx == 0 && starts_with_mbox_from {
            line_idx += 1;
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            line_idx += 1;
            continue;
        }
        if let Some((name, _)) = line.split_once(':')
            && !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_graphic() && !c.is_ascii_whitespace())
        {
            let lower = name.to_ascii_lowercase();
            if matches!(lower.as_str(), "from" | "date" | "subject") {
                saw_patch_header = true;
            }
            line_idx += 1;
            continue;
        }
        return text;
    }

    text
}

fn is_ping_subject(subject: &str) -> bool {
    let lower = subject.trim().to_ascii_lowercase();
    if lower.starts_with("ping:") || lower.starts_with("ping ") || lower.starts_with("ping^") {
        return true;
    }
    subject_prefix_tokens(subject).any(|tok| tok.to_ascii_lowercase().starts_with("ping"))
}

pub fn is_promotable_reply_patch_subject(subject: &str) -> bool {
    let lower = subject.trim().to_lowercase();
    let starts_with_reply = lower.starts_with("re:")
        || lower.starts_with("aw:")
        || lower.starts_with("回复:")
        || lower.starts_with("回复：")
        || lower.starts_with("答复:")
        || lower.starts_with("答复：");
    if !starts_with_reply {
        return false;
    }
    if lower.contains("fwd:")
        || lower.contains("forwarded:")
        || lower.contains("wg:")
        || lower.contains("[reproducer]")
        || lower.contains("(was ")
        || lower.contains("(was:")
    {
        return false;
    }
    has_patch_or_rfc_tag(subject) && !clean_subject(subject).is_empty()
}

/// Whether a bracketed subject tag contains a `PATCH` or `RFC` token.
pub fn has_patch_or_rfc_tag(subject: &str) -> bool {
    subject_prefix_tokens(subject)
        .any(|tok| tok.eq_ignore_ascii_case("patch") || tok.eq_ignore_ascii_case("rfc"))
}

pub fn promote_reply_patch_subject(subject: &str, version: u32) -> String {
    let mut stripped = subject.trim();
    let prefixes = ["re:", "aw:", "回复:", "回复：", "答复:", "答复："];
    loop {
        let lower = stripped.to_lowercase();
        let mut matched = false;
        for prefix in &prefixes {
            if lower.starts_with(prefix)
                && let Some(rest) = stripped.get(prefix.len()..)
            {
                stripped = rest.trim();
                matched = true;
                break;
            }
        }
        if !matched {
            break;
        }
    }

    if version <= 1 || parse_subject_version(stripped) == Some(version) {
        return stripped.to_string();
    }

    static RE_BRACKET_VER: OnceLock<Regex> = OnceLock::new();
    let re_ver =
        RE_BRACKET_VER.get_or_init(|| Regex::new(r"(?i)\[([^\]]*?)\bv\d+\b([^\]]*?)\]").unwrap());
    if let Some(caps) = re_ver.captures(stripped)
        && let Some(m) = caps.get(0)
    {
        let mut out = String::with_capacity(stripped.len() + 4);
        out.push_str(&stripped[..m.start()]);
        out.push_str(&format!("[{}v{}{}]", &caps[1], version, &caps[2]));
        out.push_str(&stripped[m.end()..]);
        return out;
    }

    static RE_BRACKET_TAG: OnceLock<Regex> = OnceLock::new();
    let re_tag = RE_BRACKET_TAG
        .get_or_init(|| Regex::new(r"(?i)\[([^\]]*?\b(?:PATCH|RFC)\b)([^\]]*?)\]").unwrap());
    if let Some(caps) = re_tag.captures(stripped)
        && let Some(m) = caps.get(0)
    {
        let mut out = String::with_capacity(stripped.len() + 8);
        out.push_str(&stripped[..m.start()]);
        out.push_str(&format!("[{} v{}{}]", &caps[1], version, &caps[2]));
        out.push_str(&stripped[m.end()..]);
        return out;
    }

    format!("[PATCH v{version}] {stripped}")
}

pub fn parse_email(
    raw_email: &[u8],
    reading: MailReading,
) -> Result<(PatchsetMetadata, Vec<Patch>)> {
    let received_date = extract_received_date(raw_email);

    let message = MessageParser::default()
        .with_mime_headers()
        .with_address_headers()
        .with_message_ids()
        .header_text(mail_parser::HeaderName::Subject)
        .header_date(mail_parser::HeaderName::Date)
        .header_address("X-Original-From")
        .parse(raw_email)
        .ok_or_else(|| anyhow!("Failed to parse email"))?;

    let message_id = clean_display_name(
        message
            .message_id()
            .ok_or_else(|| anyhow!("No Message-ID header"))?,
    );
    if message_id.is_empty() {
        return Err(anyhow!("Empty Message-ID header"));
    }

    let raw_subject = clean_display_name(message.subject().unwrap_or("(no subject)"));
    let subject = if raw_subject.is_empty() {
        "(no subject)".to_string()
    } else {
        raw_subject
    };

    let mut author = message
        .from()
        .and_then(|addr| addr.first())
        .map(|a| {
            let name = &clean_display_name(a.name().unwrap_or_default());
            let address = a.address().unwrap_or("unknown@localhost").trim();
            let addr = if address.is_empty() || !address.contains('@') {
                "unknown@localhost"
            } else {
                address
            };
            if name.is_empty() || name.to_lowercase() == "unknown" {
                addr.to_string()
            } else {
                format_mailbox(name, addr)
            }
        })
        .unwrap_or_else(|| "unknown@localhost".to_string());

    if let Some(first_addr) = message.header("X-Original-From").and_then(|h| match h {
        mail_parser::HeaderValue::Address(x_orig_addr) => x_orig_addr.first(),
        _ => None,
    }) {
        let name = &clean_display_name(first_addr.name().unwrap_or_default());
        let address = first_addr.address().unwrap_or("").trim();
        let author_email = extract_email(&author);
        if !address.is_empty() && address.contains('@') {
            if is_b4_alias(&author_email, address) {
                author = if name.is_empty() || name.to_lowercase() == "unknown" {
                    address.to_string()
                } else {
                    format_mailbox(name, address)
                };
            } else if author_email.to_lowercase().starts_with("devnull+") {
                tracing::warn!(
                    "Ignoring unverified X-Original-From header due to alias mismatch with From header"
                );
            }
        }
    }

    let date = message.date().map(|d| d.to_timestamp()).unwrap_or(0);

    let to = message
        .to()
        .map(|addr| {
            addr.iter()
                .filter_map(|a| {
                    let address = a.address().unwrap_or("").trim();
                    if address.is_empty() || !address.contains('@') {
                        None
                    } else {
                        let name = &clean_display_name(a.name().unwrap_or_default());
                        if name.is_empty() {
                            Some(address.to_string())
                        } else {
                            Some(format_mailbox(name, address))
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    let cc = message
        .cc()
        .map(|addr| {
            addr.iter()
                .filter_map(|a| {
                    let address = a.address().unwrap_or("").trim();
                    if address.is_empty() || !address.contains('@') {
                        None
                    } else {
                        let name = &clean_display_name(a.name().unwrap_or_default());
                        if name.is_empty() {
                            Some(address.to_string())
                        } else {
                            Some(format_mailbox(name, address))
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    let in_reply_to = match message.in_reply_to() {
        HeaderValue::Text(t) => Some(clean_display_name(t)).filter(|s| !s.is_empty()),
        HeaderValue::TextList(l) => l
            .iter()
            .map(|s| clean_display_name(s))
            .find(|s| !s.is_empty()),
        _ => None,
    };

    let references = match message.references() {
        HeaderValue::Text(t) => {
            let cleaned = clean_display_name(t);
            if cleaned.is_empty() {
                vec![]
            } else {
                vec![cleaned]
            }
        }
        HeaderValue::TextList(l) => l
            .iter()
            .map(|s| clean_display_name(s))
            .filter(|s| !s.is_empty())
            .collect(),
        _ => vec![],
    };

    let (index, mut total) = parse_subject_index(&subject);
    let version = parse_subject_version(&subject);

    let mut body = String::new();
    for i in 0..message.text_body_count() {
        if let Some(text) = message.body_text(i) {
            let normalized = text.replace("\r\n", "\n");
            let content = if has_unquoted_diff(&normalized) {
                if body.trim().is_empty() {
                    body.clear();
                    strip_leading_mbox_from_line(&normalized)
                } else {
                    strip_format_patch_headers(&normalized)
                }
            } else {
                normalized.as_str()
            };
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(content);
        }
    }

    // The rich reading keeps attachments apart from the body, to split each
    // on its own and to tell an attached copy of a diff from the inline one.
    for attachment in message
        .attachments()
        .filter(|_| reading == MailReading::FormatPatch)
    {
        if let Some(att_text) = attachment.text_contents()
            && has_unquoted_diff(att_text)
        {
            let normalized = att_text.replace("\r\n", "\n");
            let stripped = if body.trim().is_empty() {
                body.clear();
                strip_leading_mbox_from_line(&normalized)
            } else {
                strip_format_patch_headers(&normalized)
            };
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(stripped);
        }
    }

    let diff = if has_unquoted_diff(&body) {
        body.clone()
    } else {
        String::new()
    };

    // A body may carry more than one changeset: a list without a style guide
    // takes a batch of patches in a single mail as readily as one per mail.
    // In the rich reading a mail carries a diff only if a changeset is found
    // in it, so a diff that is indented to discuss it does not count.
    // Each attachment is split on its own, so a part boundary is a changeset
    // boundary too. An attached diff is never quoted context, so it is fresh.
    let attached: Vec<Changeset> = match reading {
        MailReading::FormatPatch => Vec::new(),
        MailReading::Rich => diff_attachments(&message)
            .iter()
            .flat_map(|text| split_changesets(text))
            .collect(),
    };
    let has_attached_diff = !attached.is_empty();
    let changesets = match reading {
        MailReading::FormatPatch => Vec::new(),
        MailReading::Rich => merge_changesets(split_changesets(&body), attached, &body),
    };
    let diff = match (reading, changesets.as_slice()) {
        (MailReading::FormatPatch, _) => diff,
        (MailReading::Rich, []) => String::new(),
        // The one changeset has an attached copy, which is the one that
        // applies, so the diff is the attachment's rather than the body.
        (MailReading::Rich, [changeset]) if has_attached_diff => changeset.diff.clone(),
        (MailReading::Rich, _) => body.clone(),
    };
    // A split body is a series that arrived in one message, so it is numbered
    // like one. A single changeset leaves the subject's own numbering alone.
    let split_total = u32::try_from(changesets.len()).unwrap_or(u32::MAX);
    if split_total > 1 {
        total = split_total;
    }

    // Detection logic
    let subject_lower = subject.to_lowercase();
    let subject_clean = subject_lower.trim();
    let has_diff = !diff.is_empty();
    let is_reply = subject_clean.starts_with("re:")
        || subject_clean.starts_with("fwd:")
        || subject_clean.starts_with("forwarded:")
        || subject_clean.starts_with("aw:") // German 'Antwort'
        || subject_clean.starts_with("wg:") // German 'Weitergeleitet'
        || subject_clean.starts_with("回复:") // Chinese 'Re'
        || subject_clean.starts_with("回复：") // Chinese 'Re' with full-width colon
        || subject_clean.starts_with("答复:") // Chinese 'Reply'
        || subject_clean.starts_with("答复：") // Chinese 'Reply'
        || subject_clean.starts_with("[reproducer]") // Reproducers
        || subject_lower.contains("(was ")
        || subject_lower.contains("(was:")
        || (!has_diff && is_ping_subject(&subject));
    let has_patch_tag = subject_clean.contains("patch") || subject_clean.contains("rfc");

    // A message is part of a series if it's a cover letter (index 0) or has multiple parts (total > 1)
    let is_series_metadata = total > 1 || index == 0;

    // It is a patch or cover letter if:
    // 1. It is NOT a reply (Re: ...)
    // 2. AND (It has [PATCH]/[RFC] tag OR it has a diff OR it looks like a series cover letter/part)
    let is_patch_or_cover = !is_reply && (has_patch_tag || has_diff || is_series_metadata);

    // The rich reading also takes a reply that carries a diff: on a list
    // without a style guide, a revised diff is posted in the thread as readily
    // as in a fresh mail. Only lines the sender wrote count, never quoted
    // context. A forward relays someone else's mail, so it is never one.
    let is_forward = subject_clean.starts_with("fwd:")
        || subject_clean.starts_with("forwarded:")
        || subject_clean.starts_with("wg:"); // German 'Weitergeleitet' (forwarded)
    let is_reply_patch =
        reading == MailReading::Rich && !is_patch_or_cover && has_diff && !is_forward;

    let metadata = PatchsetMetadata {
        message_id: message_id.clone(),
        subject,
        author,
        date,
        received_date,
        in_reply_to,
        references,
        index,
        total,
        to,
        cc,
        is_patch_or_cover,
        is_reply_patch,
        version,
        body: body.clone(),
    };

    let patches = if !has_diff || index == 0 {
        Vec::new() // a cover letter carries no patch of its own
    } else if split_total > 1 {
        // Each changeset becomes its own patch, so each is reviewed on its
        // own terms rather than fused into one diff.
        changesets
            .into_iter()
            .enumerate()
            .map(|(n, changeset)| Patch {
                message_id: format!("{}#{}", message_id, n + 1),
                body: changeset.message,
                diff: changeset.diff,
                part_index: u32::try_from(n + 1).unwrap_or(u32::MAX),
                subject: changeset.subject,
            })
            .collect()
    } else {
        vec![Patch {
            message_id,
            body,
            diff,
            part_index: index,
            subject: None,
        }]
    };

    Ok((metadata, patches))
}

/// The decoded text of every attachment that carries a diff, in MIME order.
///
/// Content decides, not type: a diff arrives as `text/x-patch` as readily as
/// `application/octet-stream`. A part that is not text is read as UTF-8, and
/// whatever that makes of an image or a signature carries no hunk.
fn diff_attachments(message: &Message) -> Vec<String> {
    message
        .attachments()
        .filter_map(|part| match &part.body {
            PartType::Text(text) => Some(text.to_string()),
            PartType::Binary(bytes) | PartType::InlineBinary(bytes) => {
                Some(String::from_utf8_lossy(bytes).into_owned())
            }
            PartType::Html(_) | PartType::Message(_) | PartType::Multipart(_) => None,
        })
        .filter(|text| carries_hunk(text))
        .collect()
}

/// Whether `text` holds a hunk header together with a file header.
fn carries_hunk(text: &str) -> bool {
    let mut hunk = false;
    let mut file = false;
    for line in text.lines() {
        hunk |= line.starts_with("@@ -");
        file |= line.starts_with("+++ ") || line.starts_with("diff ");
    }
    hunk && file
}

/// What identifies a changeset across an inline and an attached copy: the
/// path of every `+++` header and the range of every hunk. A mail client
/// wraps lines and turns tabs into spaces, but leaves these alone.
fn fingerprint(diff: &str) -> Vec<&str> {
    diff.lines()
        .filter_map(|line| {
            if let Some(file) = line.strip_prefix("+++ ") {
                file.split_whitespace().next()
            } else if line.starts_with("@@ -") {
                let end = line[2..].find("@@").map_or(line.len(), |at| at + 4);
                Some(&line[..end])
            } else {
                None
            }
        })
        .collect()
}

/// Folds the attached changesets into the inline ones.
///
/// A client damages the inline copy of a diff and leaves the attached one
/// alone, so where both copies arrive the attachment supplies the diff, and
/// the inline copy its place and, where it has them, its subject and message.
/// Two inline or two attached changesets are never merged. An attached
/// changeset with no message of its own is explained by the mail's body.
fn merge_changesets(
    inline: Vec<Changeset>,
    attached: Vec<Changeset>,
    body: &str,
) -> Vec<Changeset> {
    let inline_count = inline.len();
    let mut merged = inline;
    let mut taken = vec![false; inline_count];
    for mut copy in attached {
        let print = fingerprint(&copy.diff);
        let twin = (0..inline_count).find(|&n| !taken[n] && fingerprint(&merged[n].diff) == print);
        if let Some(n) = twin {
            taken[n] = true;
            let inline = &mut merged[n];
            inline.diff = copy.diff;
            if inline.subject.is_none() {
                inline.subject = copy.subject;
            }
            if inline.message.is_empty() {
                inline.message = copy.message;
            }
        } else {
            if copy.message.is_empty() {
                copy.message = body.trim().to_string();
            }
            merged.push(copy);
        }
    }
    merged
}

/// A line that only ever appears inside a diff, so it continues the block the
/// walk is already in rather than starting a new one. Covers git's extended
/// headers and the `Index:`/`===`/`RCS file:` preamble cvs(1) writes per file.
fn is_diff_metadata(line: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "===",
        "RCS file:",
        "retrieving revision",
        "diff -",
        "Index: ",
        "index ",
        "new file mode",
        "deleted file mode",
        "old mode",
        "new mode",
        "similarity index",
        "dissimilarity index",
        "rename from",
        "rename to",
        "copy from",
        "copy to",
        "Binary files",
        "GIT binary patch",
    ];
    PREFIXES.iter().any(|p| line.starts_with(p))
}

/// A line that can begin a diff block: the first file of a changeset, or the
/// next file of the one already being read. Which of the two it is depends on
/// what preceded it, not on the line itself.
fn opens_diff_block(line: &str) -> bool {
    line.starts_with("diff --git ")
        || line.starts_with("Index: ")
        || line.starts_with("--- ")
        || line.starts_with("@@ -")
}

/// A line that carries diff payload: hunk content, a hunk header, or a file
/// header. An empty line counts, because mailers strip the single space from a
/// blank context line.
fn is_diff_payload(line: &str) -> bool {
    line.is_empty() || line.starts_with([' ', '+', '-', '\\', '@']) || is_diff_metadata(line)
}

/// Where each changeset in `body` starts, as an index into its lines.
///
/// tech@ imposes no format, so this keys on structure rather than on any one
/// tool's output. A diff block that follows prose begins a changeset; a diff
/// block that follows hunk content is the next file of the changeset already
/// open. That one rule separates a batch of patches from a single diff touching
/// several files, whether the sender used git format-patch, git diff, cvs diff,
/// or a bare unified diff.
///
/// A diff ends only at prose that opens a paragraph, because mail mangles
/// diffs: quoted-printable left undecoded puts `=20` on a line of its own, and
/// a mailer wrapping a long context line drops the space that marks it. Either
/// looks like prose in the middle of a hunk, but neither follows a blank line,
/// and treating them as a boundary cuts one patch into several.
///
/// Quoted lines are skipped, so a reply discussing someone else's diff
/// contributes no changeset of its own.
fn changeset_starts(body: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut in_diff = false;
    let mut prev_blank = true;
    for (idx, line) in body.lines().enumerate() {
        // Only the quote test may ignore leading space: a context line is
        // identified by exactly that space, so trimming would make every line
        // of hunk body look like prose.
        if line.trim_start().starts_with('>') {
            prev_blank = false;
            continue; // quoted reply context
        }
        if opens_diff_block(line) {
            if !in_diff {
                starts.push(idx);
                in_diff = true;
            }
        } else if in_diff && !is_diff_payload(line) && prev_blank {
            in_diff = false;
        }
        prev_blank = line.trim().is_empty();
    }
    starts
}

/// One changeset of a batched mail: the diff, and the commit it describes.
pub(crate) struct Changeset {
    pub subject: Option<String>,
    pub message: String,
    pub diff: String,
}

/// The `Subject:` of a format-patch block, if its headers are there.
fn preamble_subject(preamble: &str) -> Option<String> {
    preamble
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("Subject: "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The commit message under that `Subject:`, up to the diffstat separator.
fn preamble_message(preamble: &str) -> String {
    let lines: Vec<&str> = preamble.lines().collect();
    let Some(subject_at) = lines.iter().rposition(|l| l.starts_with("Subject: ")) else {
        return String::new();
    };
    let body = lines[subject_at + 1..]
        .iter()
        .take_while(|l| l.trim_end() != "---")
        .copied()
        .collect::<Vec<_>>();
    body.join("\n").trim().to_string()
}

/// Splits `body` into one changeset per commit, dropping any region that turns
/// out to carry no diff after all.
///
/// A region is kept when it holds a hunk under a `+++` file header, or is a
/// git diff, which may have none when its only change is to a mode or a name.
/// A hunk quoted without the file it applies to cannot be applied, so it is
/// discussion, not a changeset. The headers introducing a commit trail the
/// diff before it, so each region hands its tail to the next.
fn split_changesets(body: &str) -> Vec<Changeset> {
    let lines: Vec<&str> = body.lines().collect();
    let starts = changeset_starts(body);
    let mut out = Vec::new();
    // What precedes the first diff introduces it; what trails a diff
    // introduces the next one, so each region hands its tail forward.
    let mut preamble = lines[..starts.first().copied().unwrap_or(0)].join("\n");
    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(lines.len());
        let region = &lines[start..end];
        let cut = region
            .iter()
            .rposition(|l| is_format_patch_mbox_from(l))
            .unwrap_or(region.len());
        let diff = region[..cut].join("\n");
        let carries_diff = (diff.lines().any(|l| l.starts_with("+++ "))
            && diff.lines().any(|l| l.starts_with("@@ -")))
            || diff.lines().any(|l| l.starts_with("diff --git "));
        if carries_diff {
            out.push(Changeset {
                subject: preamble_subject(&preamble),
                message: preamble_message(&preamble),
                diff,
            });
        }
        preamble = region[cut..].join("\n");
    }
    out
}

/// Read the `M/N` part counter out of a patch subject, as `(index, total)`.
/// A subject that carries no counter reads as part 1 of 1.
pub fn parse_subject_index(subject: &str) -> (u32, u32) {
    static RE_BRACKETS: OnceLock<Regex> = OnceLock::new();
    // Match [ ... M/N ... ] but strictly require PATCH, RFC, RESEND, COMMIT or vN before the M/N
    let re_brackets = RE_BRACKETS.get_or_init(|| {
        Regex::new(r"(?i)\[.*?\b(?:PATCH|RFC|RESEND|COMMIT|v\d+)\b.*?(\d+)/(\d+).*?\]").unwrap()
    });

    if let Some(caps) = re_brackets.captures(subject)
        && let (Some(i), Some(t)) = (caps.get(1), caps.get(2))
    {
        let index = i.as_str().parse().unwrap_or(1);
        let total = t.as_str().parse().unwrap_or(1);
        return (index, total);
    }

    static RE_LOOSE: OnceLock<Regex> = OnceLock::new();
    // Match PATCH M/N or RFC M/N (case insensitive)
    let re_loose = RE_LOOSE
        .get_or_init(|| Regex::new(r"(?i)\b(?:PATCH|RFC|RESEND|COMMIT)\s+(\d+)/(\d+)\b").unwrap());

    if let Some(caps) = re_loose.captures(subject)
        && let (Some(i), Some(t)) = (caps.get(1), caps.get(2))
    {
        let index = i.as_str().parse().unwrap_or(1);
        let total = t.as_str().parse().unwrap_or(1);
        return (index, total);
    }

    // Check cleaned subject for "1/2" at start (Handles "[PATCH] 1/2")
    let cleaned = clean_subject(subject);
    static RE_START: OnceLock<Regex> = OnceLock::new();
    let re_start = RE_START.get_or_init(|| Regex::new(r"^\s*(\d+)/(\d+)\b").unwrap());
    if let Some(caps) = re_start.captures(&cleaned)
        && let (Some(i), Some(t)) = (caps.get(1), caps.get(2))
    {
        let index = i.as_str().parse().unwrap_or(1);
        let total = t.as_str().parse().unwrap_or(1);
        return (index, total);
    }

    (1, 1)
}

pub fn parse_subject_version(subject: &str) -> Option<u32> {
    static RE_VER: OnceLock<Regex> = OnceLock::new();
    // Strategy:
    // 1. Inside [...] blocks: find vN preceded by word boundary (e.g. [PATCH v2], [v2])
    // 2. Start of string: ^vN followed by word boundary
    // 3. Following PATCH: PATCH followed by non-word chars and vN (e.g. PATCHv2, [PATCH] v2)
    let re = RE_VER.get_or_init(|| {
        Regex::new(r"(?i)(?:\[[^\]]*?\bv(\d+)\b[^\]]*?\]|^\s*v(\d+)\b|PATCH\W*v(\d+)\b)").unwrap()
    });

    if let Some(caps) = re.captures(subject) {
        if let Some(m) = caps.get(1) {
            return m.as_str().parse().ok();
        }
        if let Some(m) = caps.get(2) {
            return m.as_str().parse().ok();
        }
        if let Some(m) = caps.get(3) {
            return m.as_str().parse().ok();
        }
    }
    None
}

/// Stage of a posting as declared by its subject tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeriesStage {
    /// A bracketed `RFC` token is present (`[RFC PATCH]`, `[PATCH RFC v2]`).
    Rfc,
    /// Any other posting.
    Patch,
}

/// How a newer posting's subject tag relates to an older posting's tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionStep {
    /// The version number grows, or an RFC posting is followed by a
    /// non-RFC one (`v1 -> v2`, `RFC v2 -> v1`, `v1 -> RFC v2`).
    Advances,
    /// The same number without an RFC to non-RFC step (`v2 -> v2`,
    /// `[PATCH] -> [PATCH]`, `v1 -> RFC v1`).
    SameLabel,
    /// The number shrinks without an RFC to non-RFC step (`v3 -> v2`).
    Regresses,
}

/// The revision an author declared in a subject: an ordering hint only.
/// Authors restart numbering after RFC rounds and forget to bump tags, so
/// chronology decides the actual order of postings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubjectRevision {
    pub stage: SeriesStage,
    /// `vN` from the subject, 1 when the subject carries no version tag.
    pub number: u32,
}

impl SubjectRevision {
    pub fn parse(subject: &str) -> Self {
        Self::with_number(subject, parse_subject_version(subject).unwrap_or(1))
    }

    /// The stage parsed from `subject` with an already known version
    /// number (for example the stored `patchsets.version`).
    pub fn with_number(subject: &str, number: u32) -> Self {
        let stage = if subject_prefix_tokens(subject).any(|tok| tok.eq_ignore_ascii_case("rfc")) {
            SeriesStage::Rfc
        } else {
            SeriesStage::Patch
        };
        Self {
            stage,
            number: number.max(1),
        }
    }

    /// Classifies `self` (older posting) followed by `next` (newer posting).
    pub fn step_to(self, next: Self) -> RevisionStep {
        if self.stage == SeriesStage::Rfc && next.stage == SeriesStage::Patch {
            return RevisionStep::Advances;
        }
        match next.number.cmp(&self.number) {
            std::cmp::Ordering::Greater => RevisionStep::Advances,
            std::cmp::Ordering::Equal => RevisionStep::SameLabel,
            std::cmp::Ordering::Less => RevisionStep::Regresses,
        }
    }

    /// The public label: `vN`, or `RFC` / `RFC vN`, matching
    /// `versionLabel()` in the web UI.
    pub fn label(self) -> String {
        match (self.stage, self.number) {
            (SeriesStage::Rfc, 1) => "RFC".to_string(),
            (SeriesStage::Rfc, n) => format!("RFC v{n}"),
            (SeriesStage::Patch, n) => format!("v{n}"),
        }
    }
}

/// Returns bracketed subject tokens in order, including standard patch tags.
pub(crate) fn subject_prefix_tokens(subject: &str) -> impl Iterator<Item = &str> {
    static RE_BRACKETS: OnceLock<Regex> = OnceLock::new();
    let re = RE_BRACKETS.get_or_init(|| Regex::new(r"\[(.*?)\]").unwrap());

    re.captures_iter(subject)
        .filter_map(|cap| cap.get(1))
        .flat_map(|content| {
            content
                .as_str()
                .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '.' && c != '_')
        })
        .filter(|token| !token.is_empty())
}

pub fn get_subject_prefixes(subject: &str) -> Vec<String> {
    let mut prefixes = Vec::new();

    for token in subject_prefix_tokens(subject) {
        let lower = token.to_lowercase();
        // Ignore standard tags
        if lower == "patch" || lower == "rfc" || lower == "resend" {
            continue;
        }
        // Ignore versions (v2, v10)
        if lower.starts_with('v') && lower[1..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        // Ignore pure numbers (often part of 1/2 or just garbage)
        if token.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        prefixes.push(token.to_string());
    }
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

pub fn clean_subject(subject: &str) -> String {
    static RE_BRACKETS: OnceLock<Regex> = OnceLock::new();
    let re = RE_BRACKETS.get_or_init(|| Regex::new(r"\[.*?\]").unwrap());

    // 1. Remove [...] blocks
    let no_brackets = re.replace_all(subject, "");

    // 2. Remove Re:, Fwd: prefixes (case insensitive)
    let mut cleaned = no_brackets.trim().to_string();
    let prefixes = [
        "re:",
        "fwd:",
        "aw:",
        "wg:",
        "forwarded:",
        "回复:",
        "回复：",
        "答复:",
        "答复：",
    ];

    let mut changed = true;
    while changed {
        changed = false;
        let lower = cleaned.to_lowercase();
        for prefix in &prefixes {
            if lower.starts_with(prefix)
                && let Some(rest) = cleaned.get(prefix.len()..)
            {
                cleaned = rest.trim().to_string();
                changed = true;
                break;
            }
        }
    }

    cleaned
}

/// Strips control characters from a display name. An RFC 2047 encoded word
/// can decode to CR/LF, which lettre cannot write into a mailbox header.
pub fn clean_display_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

fn format_mailbox(name: &str, address: &str) -> String {
    let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\" <{}>", escaped, address)
}

/// Splits a comma-separated recipient list into individual entries without
/// splitting on commas inside double-quoted display names.
pub fn split_address_list(list: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;

    for ch in list.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if in_quotes && ch == '\\' {
            current.push(ch);
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quotes = !in_quotes;
            current.push(ch);
            continue;
        }
        if ch == ',' && !in_quotes {
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                result.push(trimmed.to_string());
            }
            current.clear();
        } else {
            current.push(ch);
        }
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() {
        result.push(trimmed.to_string());
    }

    result
}

pub fn extract_email(author: &str) -> String {
    if let Some(start) = author.find('<')
        && let Some(end) = author.find('>')
        && end > start
    {
        return author[start + 1..end].trim().to_string();
    }
    author.trim().to_string()
}

pub fn authors_match(a: &str, b: &str) -> bool {
    let email_a = extract_email(a);
    let email_b = extract_email(b);

    if email_a == email_b {
        return true;
    }

    is_b4_alias(&email_a, &email_b) || is_b4_alias(&email_b, &email_a)
}

fn is_b4_alias(alias: &str, real: &str) -> bool {
    let alias_lower = alias.to_lowercase();
    if !alias_lower.starts_with("devnull+") {
        return false;
    }
    let Some(at_idx) = alias_lower.rfind('@') else {
        return false;
    };
    if at_idx <= 8 {
        return false;
    }

    let encoded_part = &alias_lower[8..at_idx];
    let alias_domain = &alias_lower[at_idx + 1..];

    if alias_domain != "kernel.org" && alias_domain != "linux.dev" {
        return false;
    }

    let real_lower = real.to_lowercase();
    encoded_part == real_lower.replace('@', ".")
}

pub fn extract_changelog_from_body(body: &str) -> Option<String> {
    let lines = body.lines();
    let mut found_separator = false;
    let mut changelog = String::new();

    for line in lines {
        if !found_separator {
            let trimmed = line.trim();
            if trimmed == "---" {
                found_separator = true;
            }
        } else {
            if line.starts_with("diff --git ")
                || line.starts_with("--- a/")
                || line.starts_with("--- /dev/null")
            {
                break;
            }
            changelog.push_str(line);
            changelog.push('\n');
        }
    }

    if found_separator {
        let trimmed_changelog = changelog.trim().to_string();
        if !trimmed_changelog.is_empty() {
            return Some(trimmed_changelog);
        }
    }
    None
}

pub fn inject_changelog_into_git_show(git_show: &str, changelog: &str) -> String {
    if let Some(pos) = git_show.find("diff --git ") {
        let (header, diff) = git_show.split_at(pos);
        let mut result = header.to_string();
        if !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str("---\n");
        result.push_str(changelog);
        result.push('\n');
        if !changelog.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(diff);
        result
    } else {
        format!("{}\n---\n{}\n", git_show, changelog)
    }
}

const MAX_TRAILER_SCAN_LINES: usize = 2000;
const MAX_TRAILER_LINE_BYTES: usize = 2048;
const MAX_EXTRACTED_REF_IDS: usize = 128;

fn is_valid_b4_change_id_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 128
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Metadata extracted from a `b4 prep` empty cover-letter / tracking commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct B4TrackerCommit {
    pub change_id: Option<String>,
    pub edited_subject: Option<String>,
}

/// Detect and parse a `b4 prep` empty tracking commit (`--- b4-submit-tracking ---`).
/// Returns `None` if the commit has a non-empty diff or lacks the tracking marker.
/// Unedited `EDITME` / `*** SUBJECT HERE ***` subject placeholders
/// produce `edited_subject: None`.
pub fn parse_b4_submit_tracking(
    subject: &str,
    message: &str,
    diff: &str,
) -> Option<B4TrackerCommit> {
    if !diff.trim().is_empty() {
        return None;
    }
    if !message
        .lines()
        .take(MAX_TRAILER_SCAN_LINES)
        .any(|l| l.trim() == "--- b4-submit-tracking ---")
    {
        return None;
    }

    let change_id = parse_b4_change_id(message);

    let cleaned_subj = clean_subject(subject);
    let subj_trimmed = cleaned_subj.trim();
    let subj_upper = subj_trimmed.to_ascii_uppercase();
    let edited_subject = if subj_trimmed.is_empty()
        || subj_upper.starts_with("EDITME")
        || subj_trimmed == "*** SUBJECT HERE ***"
    {
        None
    } else {
        Some(subject.trim().to_string())
    };

    Some(B4TrackerCommit {
        change_id,
        edited_subject,
    })
}

/// Extract the `b4` `change-id` from a cover letter, patch body, or
/// `--- b4-submit-tracking ---` JSON block.
/// Stops scanning at the start of a unified diff and ignores quoted lines or
/// `prerequisite-change-id:` trailers.
pub fn parse_b4_change_id(body: &str) -> Option<String> {
    let mut in_b4_tracking_block = false;
    for line in body.lines().take(MAX_TRAILER_SCAN_LINES) {
        if line.starts_with("diff --git ")
            || line.starts_with("--- a/")
            || line.starts_with("--- /dev/null")
        {
            break;
        }
        let trimmed = line.trim();
        if trimmed == "--- b4-submit-tracking ---" {
            in_b4_tracking_block = true;
            continue;
        }
        if trimmed.len() > MAX_TRAILER_LINE_BYTES || trimmed.starts_with('>') {
            continue;
        }
        if in_b4_tracking_block {
            if trimmed.starts_with('#') {
                continue;
            }
            if let Some(pos) = trimmed.find("\"change-id\"") {
                let after_key = &trimmed[pos + "\"change-id\"".len()..];
                if let Some((_, rhs)) = after_key.split_once(':') {
                    let rhs = rhs.trim_start();
                    if let Some(rest) = rhs.strip_prefix('"')
                        && let Some(end_quote) = rest.find('"')
                    {
                        let token = &rest[..end_quote];
                        if is_valid_b4_change_id_token(token) {
                            return Some(token.to_string());
                        }
                    }
                }
            }
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("change-id") {
            continue;
        };
        let token = value.trim();
        if is_valid_b4_change_id_token(token) {
            return Some(token.to_string());
        }
    }
    None
}

fn normalize_candidate_msgid(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim()
        .trim_matches(|c: char| {
            matches!(
                c,
                '<' | '>' | '(' | ')' | '[' | ']' | '"' | '\'' | ',' | '.'
            )
        })
        .trim_end_matches('/');
    if trimmed.is_empty() || trimmed.len() > MAX_TRAILER_LINE_BYTES || !trimmed.is_ascii() {
        return None;
    }
    let decoded = if trimmed.contains("%40") || trimmed.contains("%2B") || trimmed.contains("%2b") {
        trimmed
            .replace("%40", "@")
            .replace("%2B", "+")
            .replace("%2b", "+")
    } else {
        trimmed.to_string()
    };
    if decoded.is_empty()
        || decoded.len() > 998
        || decoded
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        || decoded.contains(['<', '>', ':'])
    {
        return None;
    }
    let (local, domain) = decoded.split_once('@')?;
    if local.is_empty()
        || local.starts_with('/')
        || local.ends_with('/')
        || domain.is_empty()
        || !domain.contains('.')
        || domain.contains('/')
    {
        return None;
    }
    Some(decoded)
}

fn lore_url_regex() -> &'static Regex {
    static RE_LORE_URL: OnceLock<Regex> = OnceLock::new();
    RE_LORE_URL.get_or_init(|| {
        Regex::new(
            r"(?i)https?://(?:lore\.kernel\.org|lkml\.kernel\.org|patchwork\.kernel\.org|patch\.msgid\.link)/(?:project/[a-z0-9._-]+/(?:patch|cover)/|(?:patch|cover)/|r/|all/|lkml/|[a-z0-9._-]+/)?([^\s?#<>()\[\]]+(?:@|%40)[^/\s?#<>()\[\]]+)",
        )
        .expect("static Lore URL regex must compile")
    })
}

/// Matches a line that starts with an explicit previous-version marker
/// immediately followed by a link to that version: "v1: <url>",
/// "[v1] <url>", "v1 link: <url>", "v7 (with test steps): <url>",
/// "v1 (<url>)", b4's "- Link to v1: <url>", "Link[v1]: <url>",
/// "Previous version: <url>", "Version 1: <url>" and
/// "Changes since v1 (<url>):".
///
/// The version token must be followed by whitespace or a separator, so
/// kernel versions such as "v6.8: <url>" never match, and the link must
/// follow the marker directly, so "v2 of the base series: <url>" or a
/// "Link:" trailer below a changelog heading do not count either.
fn version_link_line_regex() -> &'static Regex {
    static RE_VERSION_LINK_LINE: OnceLock<Regex> = OnceLock::new();
    RE_VERSION_LINK_LINE.get_or_init(|| {
        Regex::new(
            r"(?ix)
            ^(?:[-*+]\s*)?
            (?:
                (?:\[\s*v\d+\s*\]|v\d+)(?:\s+links?)?(?:\s*\([^()]*\))?
              | links?\s*(?:to\s+)?(?:\[\s*v\d+\s*\]|v\d+)
              | (?:\[\s*)?rfc(?:[\s-]*v\d+)?(?:\s*\])?(?:\s+links?)?(?:\s*\([^()]*\))?
              | links?\s*(?:to\s+)?(?:the\s+)?rfc(?:[\s-]*v\d+)?
              | (?:previous|prior|earlier|last)\s+(?:version|revision|posting|iteration)
                (?:\s*\([^()]*\))?
              | version\s+\d+
              | changes\s+(?:since|from)\s+(?:\[\s*v\d+\s*\]|v\d+)
              | changes\s+(?:since|from)\s+(?:the\s+)?rfc(?:[\s-]*v\d+)?
            )
            (?:
                (?:\s*[:=]|\s*-+>?|\s+at\b|\s)\s*[(<\[]?\s*
              | \s*[(\[]\s*
            )
            (?P<link>
                https?://(?:lore\.kernel\.org|lkml\.kernel\.org|patchwork\.kernel\.org|patch\.msgid\.link)/\S+
              | <[^<>\s]+@[^<>\s]+>
              | [^\s<>()\[\]]+@[^\s<>()\[\]]+
            )",
        )
        .expect("static version link line regex must compile")
    })
}

fn extract_msgid_or_lore_url(value: &str) -> Option<String> {
    if value.len() > MAX_TRAILER_LINE_BYTES {
        return None;
    }
    if let Some(msgid) = lore_url_regex()
        .captures(value)
        .and_then(|cap| cap.get(1))
        .and_then(|m| normalize_candidate_msgid(m.as_str()))
    {
        return Some(msgid);
    }
    normalize_candidate_msgid(value)
}

/// Extract the Message-IDs of earlier versions of the same patch or series
/// that the author links explicitly in a cover letter or patch body.
///
/// Only lines that start with a previous-version marker and carry the link
/// on the same line count (see `version_link_line_regex`). Threading
/// headers, unmarked Lore links (including any link below a changelog
/// heading that has no marker of its own), kernel-version prose such as
/// "since v6.8", quoted lines, and prerequisite trailers (`Based-on:`,
/// `prerequisite-message-id:`) never name a previous version.
pub fn extract_version_reference_msgids(body: &str) -> Vec<String> {
    let re_line = version_link_line_regex();
    let (prereq_msgids_vec, _) = extract_prerequisite_message_and_patch_ids(body);
    let prereq_msgids: std::collections::HashSet<String> = prereq_msgids_vec.into_iter().collect();

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in body.lines().take(MAX_TRAILER_SCAN_LINES) {
        if out.len() >= MAX_EXTRACTED_REF_IDS {
            break;
        }
        if line.starts_with("diff --git ")
            || line.starts_with("--- a/")
            || line.starts_with("--- /dev/null")
        {
            break;
        }
        let trimmed = line.trim();
        if trimmed.len() > MAX_TRAILER_LINE_BYTES || trimmed.starts_with('>') {
            continue;
        }
        let Some(link) = re_line
            .captures(trimmed)
            .and_then(|cap| cap.name("link"))
            .map(|m| m.as_str())
        else {
            continue;
        };
        if let Some(msgid) = extract_msgid_or_lore_url(link)
            && !prereq_msgids.contains(&msgid)
            && seen.insert(msgid.clone())
        {
            out.push(msgid);
        }
    }

    out
}

/// Extract prerequisite series Message-IDs (`Based-on:`, `prerequisite-message-id:`)
/// and prerequisite patch IDs (`prerequisite-patch-id:`) from a cover letter or
/// patch body so version discovery never mistakes a prerequisite series by the
/// same author for an earlier version of the current series.
pub fn extract_prerequisite_message_and_patch_ids(body: &str) -> (Vec<String>, Vec<String>) {
    let mut msgids = Vec::new();
    let mut seen_msgids = std::collections::HashSet::new();
    let mut patch_ids = Vec::new();
    let mut seen_patch_ids = std::collections::HashSet::new();

    for line in body.lines().take(MAX_TRAILER_SCAN_LINES) {
        if line.starts_with("diff --git ")
            || line.starts_with("--- a/")
            || line.starts_with("--- /dev/null")
        {
            break;
        }
        let trimmed = line.trim();
        if trimmed.len() > MAX_TRAILER_LINE_BYTES || trimmed.starts_with('>') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.eq_ignore_ascii_case("based-on")
            || key.eq_ignore_ascii_case("prerequisite-message-id")
        {
            if msgids.len() < MAX_EXTRACTED_REF_IDS
                && let Some(msgid) = extract_msgid_or_lore_url(value)
                && seen_msgids.insert(msgid.clone())
            {
                msgids.push(msgid);
            }
        } else if key.eq_ignore_ascii_case("prerequisite-patch-id")
            && patch_ids.len() < MAX_EXTRACTED_REF_IDS
            && value.len() == 40
            && value.bytes().all(|b| b.is_ascii_hexdigit())
        {
            let lower = value.to_ascii_lowercase();
            if seen_patch_ids.insert(lower.clone()) {
                patch_ids.push(lower);
            }
        }
    }

    (msgids, patch_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_authors_match() {
        assert!(authors_match(
            "Real Author <author@example.com>",
            "Real Author <author@example.com>"
        ));
        assert!(authors_match(
            "devnull+author.example.com@kernel.org",
            "author@example.com"
        ));
        assert!(authors_match(
            "Real Author <author@example.com>",
            "Real Author via B4 Relay <devnull+author.example.com@kernel.org>"
        ));
        assert!(!authors_match("other@example.com", "author@example.com"));
        assert!(!authors_match(
            "devnull+author.example.com@spam.org",
            "author@example.com"
        ));
    }

    #[test]
    fn test_extract_email() {
        assert_eq!(
            extract_email("Name <email@example.com>"),
            "email@example.com"
        );
        assert_eq!(extract_email("email@example.com"), "email@example.com");
        assert_eq!(extract_email(" <email@example.com> "), "email@example.com");
        assert_eq!(
            extract_email("Name < email@example.com >"),
            "email@example.com"
        );
        assert_eq!(extract_email("Invalid < Format"), "Invalid < Format");
    }

    #[test]
    fn test_extract_received_date() {
        let email = b"Received: from mail.example.com ([192.0.2.1])\r\n \
by mail.example.org with ESMTPS ;\r\n \
Tue, 12 May 2026 00:12:30 +0000\r\n\
\r\n\
Body";
        let expected = chrono::DateTime::parse_from_rfc2822("Tue, 12 May 2026 00:12:30 +0000")
            .unwrap()
            .timestamp();
        assert_eq!(extract_received_date(email), Some(expected));

        let email_no_received = b"Subject: Test\r\n\
            \r\n\
            Body";
        assert_eq!(extract_received_date(email_no_received), None);

        let email_malformed_date = b"Received: from ... ; Invalid Date\r\n\
            \r\n\
            Body";
        assert_eq!(extract_received_date(email_malformed_date), None);
    }

    #[test]
    fn test_clean_subject() {
        assert_eq!(clean_subject("[PATCH] Fix bug"), "Fix bug");
        assert_eq!(clean_subject("[PATCH v2] Fix bug"), "Fix bug");
        assert_eq!(clean_subject("[PATCH 1/2] Fix bug"), "Fix bug");
        assert_eq!(clean_subject("Re: [PATCH] Fix bug"), "Fix bug");
        assert_eq!(clean_subject("[PATCH] Re: Fix bug"), "Fix bug"); // "[PATCH] " removed, then "Re: Fix bug" -> "Fix bug"
        assert_eq!(clean_subject("Subject only"), "Subject only");
        assert_eq!(
            clean_subject("[RFC] [PATCH v3] Complex subject"),
            "Complex subject"
        );
    }

    #[test]
    fn test_clean_subject_chinese() {
        assert_eq!(clean_subject("回复: [PATCH] Fix bug"), "Fix bug");
        assert_eq!(clean_subject("回复：[PATCH] Fix bug"), "Fix bug");
        assert_eq!(clean_subject("回复：回复：[PATCH] Fix bug"), "Fix bug");
    }

    #[test]
    fn test_chinese_reply() {
        let raw =
            b"Message-ID: <reply>\r\nSubject: \xE5\x9B\x9E\xE5\xA4\x8D: [PATCH] fix\r\n\r\nBody";
        // \xE5\x9B\x9E\xE5\xA4\x8D is "回复" in UTF-8.
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(
            !meta.is_patch_or_cover,
            "Chinese reply should not be a patchset"
        );
    }

    #[test]
    fn test_author_parsing() {
        let raw =
            b"Message-ID: <123>\r\nFrom: Test User <test@example.com>\r\nSubject: Test\r\n\r\nBody";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert_eq!(meta.author, "\"Test User\" <test@example.com>");

        let raw_no_name =
            b"Message-ID: <456>\r\nFrom: test2@example.com\r\nSubject: Test\r\n\r\nBody";
        let (meta2, _) = parse_email(raw_no_name, MailReading::FormatPatch).unwrap();
        assert_eq!(meta2.author, "test2@example.com");

        let raw_malformed =
            b"Message-ID: <789>\r\nFrom: \"fqr\" <user.email>\r\nSubject: Test\r\n\r\nBody";
        let (meta3, _) = parse_email(raw_malformed, MailReading::FormatPatch).unwrap();
        assert_eq!(meta3.author, "\"fqr\" <unknown@localhost>");
    }

    #[test]
    fn test_b4_relay_author_parsing() {
        let raw = b"Message-ID: <b4-relay-test>\r\n\
                    From: Real Author via B4 Relay <devnull+author.example.com@kernel.org>\r\n\
                    X-Original-From: Real Author <author@example.com>\r\n\
                    Subject: Test B4 Relay\r\n\r\nBody";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert_eq!(meta.author, "\"Real Author\" <author@example.com>");
    }

    #[test]
    fn test_b4_relay_mismatch_fallback() {
        // Verification: If From is a B4 Relay alias, but X-Original-From is a completely
        // different address, Sashiko ignores the override and falls back to the From alias.
        let raw_mismatch = b"Message-ID: <mismatch-test-2>\r\n\
                             From: Real Author via B4 Relay <devnull+author.example.com@kernel.org>\r\n\
                             X-Original-From: Author Two <author2@example.com>\r\n\
                             Subject: Normal Patch\r\n\r\nBody";
        let (meta_mismatch, _) = parse_email(raw_mismatch, MailReading::FormatPatch).unwrap();
        assert_eq!(
            meta_mismatch.author,
            "\"Real Author via B4 Relay\" <devnull+author.example.com@kernel.org>"
        );
    }

    #[test]
    fn test_recipient_parsing() {
        let raw = b"Message-ID: <1>\r\nFrom: a@b.com\r\nTo: \"Valid User\" <valid@example.com>, invalid_no_at, <another@test.com>\r\nCc: Bad <bad>, \"Good\" <good@example.com>\r\nSubject: Test\r\n\r\nBody";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert_eq!(
            meta.to,
            "\"Valid User\" <valid@example.com>, another@test.com"
        );
        assert_eq!(meta.cc, "\"Good\" <good@example.com>");
    }

    #[test]
    fn test_reply_with_diff_is_not_patchset() {
        // A message that starts with Re: but contains diff --git
        // This simulates a reply quoting a patch or sending an inline fixup
        let raw = b"Message-ID: <123>\r\nSubject: Re: [PATCH] fix bug\r\n\r\n> diff --git a/file b/file\n> index...";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();

        // This fails with current logic because has_diff is true
        assert!(
            !meta.is_patch_or_cover,
            "Reply with diff should NOT be a patchset"
        );
    }

    #[test]
    fn test_diff_without_patch_tag_ignored() {
        let raw = b"Message-ID: <diffnopatch>\r\nSubject: Random fix\r\n\r\ndiff --git a/file b/file\nindex...";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(
            meta.is_patch_or_cover,
            "Diff without [PATCH] tag should still be parsed as patch"
        );
    }

    #[test]
    fn test_normal_patch() {
        let raw = b"Message-ID: <456>\r\nSubject: [PATCH] fix bug\r\n\r\ndiff --git a/file b/file\nindex...";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(meta.is_patch_or_cover);
    }

    #[test]
    fn test_single_patch_no_diff_ignored() {
        let raw =
            b"Message-ID: <nonpatch>\r\nSubject: [PATCH] discussion\r\n\r\nThis is not a patch";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(
            meta.is_patch_or_cover,
            "Single patch without diff should still be parsed due to [PATCH] tag"
        );
    }

    #[test]
    fn test_cover_letter() {
        let raw = b"Message-ID: <789>\r\nSubject: [PATCH 0/5] fix bug\r\n\r\nCover letter body";
        let (meta, patch) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(meta.is_patch_or_cover);
        assert!(patch.is_empty());
    }

    #[test]
    fn test_cover_letter_with_diff() {
        let raw = b"Message-ID: <cover_with_diff>\r\nSubject: [PATCH 0/5] fix bug\r\n\r\nExplanation:\ndiff --git a/file b/file\nindex...";
        let (meta, patch) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(meta.is_patch_or_cover);
        assert!(
            patch.is_empty(),
            "Cover letter (index 0) with diff should NOT be a patch"
        );
    }

    #[test]
    fn test_reply_with_fresh_diff_is_a_reply_patch() {
        // OpenBSD style: the actual diff is posted as a reply in the thread,
        // below some quoted discussion. The unquoted diff makes it a patch.
        let raw = b"Message-ID: <obsd>\r\nSubject: Re: sys/vfs: do not reclaim\r\n\r\n\
> On some date, someone wrote:\n\
> > Thoughts?\n\
> > Index: sys/kern/vfs_subr.c\n\
\n\
Here is the diff:\n\
Index: sys/kern/vfs_subr.c\n\
--- sys/kern/vfs_subr.c\t1\n\
+++ sys/kern/vfs_subr.c\t2\n\
@@ -1021,7 +1021,7 @@\n\
-old\n\
+new\n";
        let (meta, patch) = parse_email(raw, MailReading::Rich).unwrap();
        assert!(
            meta.is_reply_patch,
            "Reply carrying a fresh unquoted diff SHOULD be a patch"
        );
        assert!(
            !meta.is_patch_or_cover,
            "the author's next version decides first"
        );
        assert!(
            !patch.is_empty(),
            "A patch should be extracted from the reply"
        );
    }

    #[test]
    fn test_only_the_rich_reading_takes_a_reply_patch() {
        let raw = b"Message-ID: <fp-reply>\r\nSubject: Re: [PATCH] fix bug\r\n\r\n\
Try this instead:\n\
\n\
--- a/file.c\n\
+++ b/file.c\n\
@@ -1,1 +1,1 @@\n\
-old\n\
+new\n";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(!meta.is_patch_or_cover);
        assert!(!meta.is_reply_patch);

        let (meta, _) = parse_email(raw, MailReading::Rich).unwrap();
        assert!(meta.is_reply_patch);

        // A quoted diff is discussion in either reading.
        let quoted = b"Message-ID: <quoted>\r\nSubject: Re: [PATCH] fix bug\r\n\r\n\
> --- a/file.c\n\
> +++ b/file.c\n\
> @@ -1,1 +1,1 @@\n\
> -old\n\
> +new\n\
\n\
Looks good.\n";
        let (meta, _) = parse_email(quoted, MailReading::Rich).unwrap();
        assert!(!meta.is_reply_patch);

        // A forward relays someone else's mail.
        let forward = b"Message-ID: <fwd>\r\nSubject: Fwd: fix bug\r\n\r\n\
--- a/file.c\n\
+++ b/file.c\n\
@@ -1,1 +1,1 @@\n\
-old\n\
+new\n";
        let (meta, _) = parse_email(forward, MailReading::Rich).unwrap();
        assert!(!meta.is_patch_or_cover);
        assert!(!meta.is_reply_patch);
    }

    #[test]
    fn test_pure_reply() {
        let raw = b"Message-ID: <abc>\r\nSubject: Re: [PATCH] fix bug\r\n\r\nLGTM";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(!meta.is_patch_or_cover);
    }

    #[test]
    fn test_rfc_patch_parsing() {
        let subject = "[RFC PATCH 1/3] My RFC";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 1);
        assert_eq!(total, 3);
    }

    #[test]
    fn test_version_parsing() {
        assert_eq!(parse_subject_version("[PATCH v2] subject"), Some(2));
        assert_eq!(parse_subject_version("[PATCH v3 1/2] subject"), Some(3));
        assert_eq!(parse_subject_version("[PATCH] subject"), None); // v1 implicit
        assert_eq!(parse_subject_version("[RFC v4] subject"), Some(4));
        assert_eq!(parse_subject_version("[PATCH -v2] subject"), Some(2));
        assert_eq!(parse_subject_version("Subject with v2 inside"), None); // v2 ignored
        assert_eq!(parse_subject_version("Subject with devicetree"), None); // 'dev' should not match
        assert_eq!(parse_subject_version("[PATCH 0/10]"), None); // 0/10 is not version
        assert_eq!(parse_subject_version("[PATCH v12]"), Some(12));

        // New cases from analysis
        assert_eq!(parse_subject_version("[PATCH V2 13/13]"), Some(2)); // Uppercase V
        assert_eq!(parse_subject_version("[PATCH bpf-next v5 10/10]"), Some(5)); // Subsystem prefix
        assert_eq!(parse_subject_version("[PATCH RFC v2 8/8]"), Some(2)); // RFC + Version
        assert_eq!(parse_subject_version("[PATCHv5 2/2]"), Some(5)); // Attached version
        assert_eq!(parse_subject_version("[PATCH 00/33 v6]"), Some(6)); // Version at end
        assert_eq!(parse_subject_version("[v3 PATCH 1/1]"), Some(3)); // Version at start

        // Edge case: [PATCH] v3: ...
        assert_eq!(parse_subject_version("[PATCH] v3: subject"), Some(3));
    }

    #[test]
    fn test_subject_revision_parses_stage_number_and_label() {
        let cases = [
            ("[PATCH] mm: fix", SeriesStage::Patch, 1, "v1"),
            ("[PATCH v1] mm: fix", SeriesStage::Patch, 1, "v1"),
            ("[PATCH v3 0/4] mm: fix", SeriesStage::Patch, 3, "v3"),
            ("[RFC PATCH] mm: fix", SeriesStage::Rfc, 1, "RFC"),
            ("[PATCH RFC v2 0/3] mm: fix", SeriesStage::Rfc, 2, "RFC v2"),
            ("[RFC net-next v4] mm: fix", SeriesStage::Rfc, 4, "RFC v4"),
            (
                "[uL RFC PATCH v3 0/8] i2c: omap",
                SeriesStage::Rfc,
                3,
                "RFC v3",
            ),
            // "rfc" outside the brackets is part of the summary.
            (
                "[PATCH] net: document RFC 8200 handling",
                SeriesStage::Patch,
                1,
                "v1",
            ),
        ];
        for (subject, stage, number, label) in cases {
            let rev = SubjectRevision::parse(subject);
            assert_eq!(rev, SubjectRevision { stage, number }, "{subject}");
            assert_eq!(rev.label(), label, "{subject}");
        }
        assert_eq!(SubjectRevision::with_number("[RFC] x", 0).number, 1);
    }

    #[test]
    fn test_subject_revision_steps() {
        let step = |a: &str, b: &str| SubjectRevision::parse(a).step_to(SubjectRevision::parse(b));
        assert_eq!(step("[PATCH] x", "[PATCH v2] x"), RevisionStep::Advances);
        assert_eq!(step("[RFC PATCH] x", "[PATCH] x"), RevisionStep::Advances);
        assert_eq!(
            step("[RFC PATCH v3] x", "[PATCH v1] x"),
            RevisionStep::Advances
        );
        assert_eq!(
            step("[RFC PATCH v2] x", "[PATCH v2] x"),
            RevisionStep::Advances
        );
        assert_eq!(
            step("[PATCH] x", "[RFC PATCH v2] x"),
            RevisionStep::Advances
        );
        assert_eq!(step("[RFC] x", "[RFC v2] x"), RevisionStep::Advances);
        assert_eq!(step("[PATCH] x", "[PATCH] x"), RevisionStep::SameLabel);
        assert_eq!(
            step("[PATCH v2] x", "[PATCH v2 RESEND] x"),
            RevisionStep::SameLabel
        );
        assert_eq!(step("[PATCH] x", "[RFC PATCH] x"), RevisionStep::SameLabel);
        assert_eq!(
            step("[PATCH v3] x", "[PATCH v2] x"),
            RevisionStep::Regresses
        );
        assert_eq!(
            step("[PATCH v2] x", "[RFC PATCH] x"),
            RevisionStep::Regresses
        );
    }

    #[test]
    fn test_has_patch_or_rfc_tag() {
        assert!(has_patch_or_rfc_tag("[PATCH v2 1/3] x"));
        assert!(has_patch_or_rfc_tag("[RFC] x"));
        assert!(has_patch_or_rfc_tag("[net-next,PATCH] x"));
        assert!(!has_patch_or_rfc_tag(
            "linux-next: manual merge of the drm tree"
        ));
        assert!(!has_patch_or_rfc_tag(
            "[Openvpn-devel] [S] Change in openvpn[master]: x"
        ));
    }

    #[test]
    fn test_complex_prefix_parsing() {
        let subject = "[PATCH v2 net-next 02/14] Something";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 2);
        assert_eq!(total, 14);
    }

    #[test]
    fn test_no_patch_prefix_parsing() {
        // Some lists might just use [RFC 1/2]
        let subject = "[RFC 1/2] Just RFC";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 1);
        assert_eq!(total, 2);
    }

    #[test]
    fn test_missed_cover_letter_parsing() {
        let subject = "[PATCH 6.18 000/430] 6.18.3-rc1 review";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 0);
        assert_eq!(total, 430);

        let raw = format!("Message-ID: <123>\r\nSubject: {}\r\n\r\nBody", subject);
        let (meta, _) = parse_email(raw.as_bytes(), MailReading::FormatPatch).unwrap();
        assert!(meta.is_patch_or_cover, "Should be detected as patch/cover");
    }

    #[test]
    fn test_forwarded_reply_is_not_patch() {
        // "Forwarded: Re: ..." should be treated as reply/skip if it has no diff,
        // or if it has diff but looks like a forwarded reply.
        // If it has diff, it might be a forwarded patch.
        // But if it starts with "Re:", it's usually a reply.
        // "Forwarded: Re:" -> effectively a reply.
        let subject = "Forwarded: Re: [syzbot] WARNING in cm109_urb_irq_callback";
        let raw = format!(
            "Message-ID: <456>\r\nSubject: {}\r\n\r\nDiff:\n--- a\n+++ b\n@@ -1 +1 @@",
            subject
        );
        let (meta, _) = parse_email(raw.as_bytes(), MailReading::FormatPatch).unwrap();

        // Current logic might think this is a patch because it has diff and doesn't start with "Re:" (starts with "Forwarded:")
        // We want to ensure it is handled correctly (either as patch if it IS a patch, or ignored if it's just a reply).
        // If it's "Forwarded: Re:", it's likely a discussion.
        // Let's assert what we expect. I expect it NOT to be a patchset root.
        assert!(
            !meta.is_patch_or_cover,
            "Forwarded Re: should not be a patchset"
        );
    }

    #[test]
    fn test_loose_patch_parsing() {
        // Case 1: PATCH prefix
        let subject = "PATCH 1/2: Subject";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 1);
        assert_eq!(total, 2);

        // Case 2: Start of string
        let subject = "1/2: Subject";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 1);
        assert_eq!(total, 2);

        // Case 3: Leading zeros
        let subject = "[PATCH 01/02] Subject";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 1);
        assert_eq!(total, 2);

        // Case 4: Regression test for false positive (508/512)
        let subject = "[PATCH v3] serial: 8250_pci: Fix broken RS485 for F81504/508/512";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 1);
        assert_eq!(total, 1); // Should be 1/1, NOT 508/512
    }

    #[test]
    fn test_subject_prefix_tokens_preserve_order() {
        let subject = "Re: [RESEND PATCH v2][STABLE V6.6-v6.1][net-next,bpf,net-next,1/2] \
                       fix stable USB 3.0";
        assert_eq!(
            subject_prefix_tokens(subject).collect::<Vec<_>>(),
            [
                "RESEND",
                "PATCH",
                "v2",
                "STABLE",
                "V6.6-v6.1",
                "net-next",
                "bpf",
                "net-next",
                "1",
                "2"
            ]
        );
        assert_eq!(
            get_subject_prefixes(subject),
            ["STABLE", "V6.6-v6.1", "bpf", "net-next"]
        );
        assert!(subject_prefix_tokens("STABLE v6.6").next().is_none());
        assert!(subject_prefix_tokens("[STABLE v6.6").next().is_none());
    }

    #[test]
    fn test_get_subject_prefixes() {
        assert_eq!(
            get_subject_prefixes("[PATCH net-next 1/2]"),
            vec!["net-next"]
        );
        assert_eq!(
            get_subject_prefixes("[PATCH v2 bpf-next]"),
            vec!["bpf-next"]
        );
        assert_eq!(get_subject_prefixes("[PATCH RFC]"), Vec::<String>::new());
        assert_eq!(get_subject_prefixes("[PATCH 00/10]"), Vec::<String>::new()); // numbers ignored
        assert_eq!(get_subject_prefixes("[PATCH 6.18]"), vec!["6.18"]);
        assert_eq!(
            get_subject_prefixes("[PATCH net-next v3 0/1]"),
            vec!["net-next"]
        );
        assert_eq!(
            get_subject_prefixes("[PATCH net-next,bpf 1/2]"),
            vec!["bpf", "net-next"]
        ); // sorted
        assert_eq!(get_subject_prefixes("[PATCH]"), Vec::<String>::new());
    }

    #[test]
    fn test_parse_email_author_quoting() {
        let raw = b"From: Thomas Richard (TI) <thomas.richard@bootlin.com>\r\nMessage-ID: <123>\r\nSubject: Test\r\n\r\nDiff:\n--- a\n+++ b\n@@ -1 +1 @@";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert_eq!(
            meta.author,
            "\"Thomas Richard (TI)\" <thomas.richard@bootlin.com>"
        );
    }

    #[test]
    fn test_parse_email_strips_control_characters_from_names() {
        let raw = b"From: =?utf-8?q?Bob=0D=0AX?= <bob@example.com>\r\nTo: =?utf-8?q?Al=0Ace?= <alice@example.com>\r\nCc: =?utf-8?q?=0D=0A?= <carol@example.com>\r\nMessage-ID: <123>\r\nSubject: =?utf-8?q?Sub=0D=0Aject?=\r\n\r\nbody";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert_eq!(meta.author, "\"BobX\" <bob@example.com>");
        assert_eq!(meta.to, "\"Alce\" <alice@example.com>");
        assert_eq!(meta.cc, "carol@example.com");
        assert_eq!(meta.subject, "Subject");
    }

    #[test]
    fn test_split_address_list_preserves_quoted_commas() {
        let raw = b"From: Author <author@example.com>\r\nTo: \"Tabba, Fuad\" <fuad@example.com>, \"Trail\\\\\" <trail@example.com>, list@vger.kernel.org\r\nCc: \"Doe, \\\"Jr,.,\\\" John\" <john@example.com>\r\nMessage-ID: <123>\r\nSubject: Test\r\n\r\nbody";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert_eq!(
            split_address_list(&meta.to),
            vec![
                "\"Tabba, Fuad\" <fuad@example.com>".to_string(),
                "\"Trail\\\\\" <trail@example.com>".to_string(),
                "list@vger.kernel.org".to_string(),
            ]
        );
        assert_eq!(
            split_address_list(&meta.cc),
            vec!["\"Doe, \\\"Jr,.,\\\" John\" <john@example.com>".to_string()]
        );
    }

    #[test]
    fn test_extract_changelog_from_body() {
        let body_with_changelog = "This is the commit message.

Signed-off-by: Me <me@example.com>
---
Changes in v4:
- Rebase on v7.2-rc1, drop dependencies
- (A concurrency fix for mark_new_valid_map was sent independently)
  https://lore.kernel.org/linux-riscv/

diff --git a/file.c b/file.c
index ...";
        assert_eq!(
            extract_changelog_from_body(body_with_changelog),
            Some("Changes in v4:\n- Rebase on v7.2-rc1, drop dependencies\n- (A concurrency fix for mark_new_valid_map was sent independently)\n  https://lore.kernel.org/linux-riscv/".to_string())
        );

        let body_no_changelog = "This is the commit message.

Signed-off-by: Me <me@example.com>
---
diff --git a/file.c b/file.c
index ...";
        assert_eq!(extract_changelog_from_body(body_no_changelog), None);

        let body_no_separator = "This is the commit message.

Signed-off-by: Me <me@example.com>
diff --git a/file.c b/file.c
index ...";
        assert_eq!(extract_changelog_from_body(body_no_separator), None);

        let cover_letter = "This is cover letter.

---
changes:
- test

 fs/file.c | 2 +-";
        assert_eq!(
            extract_changelog_from_body(cover_letter),
            Some("changes:\n- test\n\n fs/file.c | 2 +-".to_string())
        );
    }

    #[test]
    fn test_inject_changelog_into_git_show() {
        let git_show = "commit 1234
Author: Me <me@example.com>
Date:   Mon Jul 6 2026

    This is commit message

diff --git a/file.c b/file.c";
        let changelog = "v2:
- test";
        let expected = "commit 1234
Author: Me <me@example.com>
Date:   Mon Jul 6 2026

    This is commit message

---
v2:
- test

diff --git a/file.c b/file.c";
        assert_eq!(
            inject_changelog_into_git_show(git_show, changelog),
            expected
        );
    }

    #[test]
    fn test_parse_b4_change_id() {
        let body = "Cover letter text\n\n---\nchange-id: 20261012-net-foo-teardown-8a9b1c2d3e4f\nbase-commit: 1234567890abcdef\n";
        assert_eq!(
            parse_b4_change_id(body),
            Some("20261012-net-foo-teardown-8a9b1c2d3e4f".to_string())
        );

        let tracking_json = "Cover prose\n\n--- b4-submit-tracking ---\n# This section is used by b4 prep for tracking purposes.\n{\n  \"series\": {\n    \"revision\": 1,\n    \"change-id\": \"20261012-net-foo-teardown-8a9b1c2d3e4f\",\n    \"prefixes\": []\n  }\n}\n";
        assert_eq!(
            parse_b4_change_id(tracking_json),
            Some("20261012-net-foo-teardown-8a9b1c2d3e4f".to_string())
        );

        let quoted = "> change-id: 20261012-quoted-111111\nprerequisite-change-id: 20261010-prereq-222222\ndiff --git a/foo.c b/foo.c\n+change-id: 20261012-in-diff-333333\n";
        assert_eq!(parse_b4_change_id(quoted), None);
    }

    #[test]
    fn test_parse_b4_submit_tracking() {
        let unedited_msg = "# Lines starting with # will be removed from the cover letter.\n\nSigned-off-by: Dev <dev@example.com>\n\n--- b4-submit-tracking ---\n# This section is used by b4 prep for tracking purposes.\n{\n  \"series\": {\n    \"revision\": 1,\n    \"change-id\": \"20261012-b4-prep-11223344\",\n    \"prefixes\": []\n  }\n}\n";
        let unedited =
            parse_b4_submit_tracking("EDITME: cover title for b4-prep-branch", unedited_msg, "")
                .expect("should detect unedited b4 tracker commit");
        assert_eq!(
            unedited.change_id.as_deref(),
            Some("20261012-b4-prep-11223344")
        );
        assert_eq!(unedited.edited_subject, None);

        let edited_msg = "# Comment line ignored\n># Architecture Overview\n\nExplain why patch 1 prepares the lock before patch 2 drops the old helper.\n\nSigned-off-by: Dev <dev@example.com>\n\n--- b4-submit-tracking ---\n{\"series\": {\"change-id\": \"20261012-b4-prep-11223344\", \"revision\": 2}}\n";
        let edited = parse_b4_submit_tracking("net: foo: refactor lock lifecycle", edited_msg, "")
            .expect("should detect edited b4 tracker commit");
        assert_eq!(
            edited.change_id.as_deref(),
            Some("20261012-b4-prep-11223344")
        );
        assert_eq!(
            edited.edited_subject.as_deref(),
            Some("net: foo: refactor lock lifecycle")
        );

        assert!(
            parse_b4_submit_tracking(
                "net: foo: real commit",
                edited_msg,
                "diff --git a/foo.c b/foo.c\n"
            )
            .is_none()
        );
    }

    #[test]
    fn test_extract_version_reference_msgids() {
        let body = "Fix race in foo_remove when allocating in vmalloc() and setting ptr -> value.\n\
Reported at https://lore.kernel.org/r/20260901-bug-report@kernel.org\n\
See discussion at https://lore.kernel.org/r/20260903-unrelated-thread@kernel.org\n\
Closes: https://lore.kernel.org/r/20260902-closes-bug@kernel.org\n\
Link: https://lore.kernel.org/r/20260904-link-trailer-discussion@kernel.org\n\
\n\
---\n\
Changes in v3:\n\
- Address locking feedback\n\
- Closes: https://lore.kernel.org/r/20260905-bulleted-closes@kernel.org\n\
- Link to v2: https://lore.kernel.org/r/20261010-foo-v2-0-aaaa@kernel.org\n\
- Link to v1: https://lore.kernel.org/netdev/20261001-foo-v1-1-bbbb%40kernel.org/\n\
Link: https://lore.kernel.org/r/20260930-changelog-link@kernel.org\n\
[1] https://lore.kernel.org/all/20260930-footnote@kernel.org/\n\
- Link to v0: <20260929-link-to-v0-msgid@kernel.org>\n\
v1: <20261001-explicit-v1@kernel.org>\n\
* v1: https://lore.kernel.org/bpf/20261001-star-v1@kernel.org/\n\
v2 link: https://lore.kernel.org/all/20261002-v2-link@kernel.org/\n\
v7 (with test steps): https://lore.kernel.org/all/20261003-v7-note@kernel.org/\n\
v1 (https://lore.kernel.org/all/20261004-paren-v1@kernel.org/)\n\
[v3] https://lore.kernel.org/all/20261005-bracket-v3@kernel.org/\n\
- Link[v1]: https://lore.kernel.org/lkml/20261006-link-bracket@kernel.org/\n\
- Link to v4: https://patch.msgid.link/20261007-msgid-link@kernel.org\n\
Previous version: https://lore.kernel.org/r/20261008-previous@kernel.org\n\
Changes since v5 (https://lore.kernel.org/r/20261009-changes-since@kernel.org):\n\
- Patchwork link: https://patchwork.kernel.org/project/netdev/patch/20261001-pw-v1@kernel.org/\n\
v1: https://lore.kernel.org/r/20260925-prereq-url@kernel.org\n\
Based-on: <20260925-prereq-series@kernel.org>\n\
Based-on: https://lore.kernel.org/r/20260925-prereq-url@kernel.org\n\
prerequisite-message-id: <20260920-b4-prereq@kernel.org>\n\
> v1: https://lore.kernel.org/r/20260918-quoted@kernel.org\n\
diff --git a/foo.c b/foo.c\n\
+v1: https://lore.kernel.org/r/should-ignore-diff@kernel.org\n";
        let refs = extract_version_reference_msgids(body);
        assert_eq!(
            refs,
            vec![
                "20261010-foo-v2-0-aaaa@kernel.org",
                "20261001-foo-v1-1-bbbb@kernel.org",
                "20260929-link-to-v0-msgid@kernel.org",
                "20261001-explicit-v1@kernel.org",
                "20261001-star-v1@kernel.org",
                "20261002-v2-link@kernel.org",
                "20261003-v7-note@kernel.org",
                "20261004-paren-v1@kernel.org",
                "20261005-bracket-v3@kernel.org",
                "20261006-link-bracket@kernel.org",
                "20261007-msgid-link@kernel.org",
                "20261008-previous@kernel.org",
                "20261009-changes-since@kernel.org",
            ]
        );
    }

    #[test]
    fn test_extract_version_reference_msgids_ignores_unmarked_links() {
        // A dependency link after a changelog heading names another series,
        // not the previous version of this one.
        let dependency_after_heading = "Changes since v2:\n\
- new patches 3 to 7 fix the range checks\n\
\n\
[1] https://lore.kernel.org/all/20260915070001.113559-3-hao.ge@linux.dev/\n\
Link: https://lore.kernel.org/r/20260915070001.113559-1-hao.ge@linux.dev\n\
This series depends on https://lore.kernel.org/r/20260901-dep@kernel.org\n";
        assert!(extract_version_reference_msgids(dependency_after_heading).is_empty());

        // Kernel-version prose is not a previous-version marker.
        let kernel_versions = "This regressed since v6.8 https://lore.kernel.org/r/20260101-regress@kernel.org\n\
It was fixed in v5.4: https://lore.kernel.org/r/20260102-fixed-in@kernel.org\n\
in v6.12 (https://lore.kernel.org/r/20260103-in-v6-12@kernel.org)\n\
v6.8: https://lore.kernel.org/r/20260104-v6-8-colon@kernel.org\n\
v5.4 (https://lore.kernel.org/r/20260105-v5-4-paren@kernel.org)\n\
Changes since v6.8: https://lore.kernel.org/r/20260106-changes-since-kver@kernel.org\n";
        assert!(extract_version_reference_msgids(kernel_versions).is_empty());

        // A version marker followed by prose names something else.
        let prose = "v2 of the base series: https://lore.kernel.org/r/20260107-base-v2@kernel.org\n\
v1 was discussed at https://lore.kernel.org/r/20260108-discussed@kernel.org\n";
        assert!(extract_version_reference_msgids(prose).is_empty());
    }

    #[test]
    fn test_extract_version_reference_msgids_rfc_markers() {
        // Forms observed in production cover letters of the non-RFC
        // posting that follows an RFC.
        let body = "RFC: https://lore.kernel.org/linux-wireless/20261003-rfc-a@pobox.sk/\n\
- RFC: https://lore.kernel.org/20261028051104.154232-1-sj@kernel.org\n\
RFC v2: https://lore.kernel.org/all/20260914-rfc-v2-0-bf6645bb2f65@oss.qualcomm.com/\n\
RFC-v4: https://lore.kernel.org/all/20250104101224.873926-1-zengheng4@huawei.com/\n\
- Link to RFC: https://lore.kernel.org/bpf/20260902070534.2141812-1-tangyazhou@zju.edu.cn/\n\
- Link to RFC v3: https://lore.kernel.org/all/20261013131309.2790253-1-ridong.chen@linux.dev/\n\
rfc: https://lore.kernel.org/kexec/20260901180713.4185641-1-dmatlack@google.com/\n\
[RFC] https://lore.kernel.org/r/20260801-bracketed-rfc@kernel.org\n\
Changes since RFC: https://lore.kernel.org/r/20260802-changes-since-rfc@kernel.org\n";
        assert_eq!(
            extract_version_reference_msgids(body),
            vec![
                "20261003-rfc-a@pobox.sk",
                "20261028051104.154232-1-sj@kernel.org",
                "20260914-rfc-v2-0-bf6645bb2f65@oss.qualcomm.com",
                "20250104101224.873926-1-zengheng4@huawei.com",
                "20260902070534.2141812-1-tangyazhou@zju.edu.cn",
                "20261013131309.2790253-1-ridong.chen@linux.dev",
                "20260901180713.4185641-1-dmatlack@google.com",
                "20260801-bracketed-rfc@kernel.org",
                "20260802-changes-since-rfc@kernel.org",
            ]
        );

        // IETF RFC references and RFC prose are not previous-version markers.
        let ietf = "RFC 8200: https://lore.kernel.org/r/20260110-ietf@kernel.org\n\
RFC 9110 https://lore.kernel.org/r/20260111-ietf-space@kernel.org\n\
The RFC was discussed at https://lore.kernel.org/r/20260112-discussed@kernel.org\n";
        assert!(extract_version_reference_msgids(ietf).is_empty());
    }

    #[test]
    fn test_extract_prerequisite_message_and_patch_ids() {
        let body = "Cover letter\n---\nBased-on: <20260925-prereq-series@kernel.org>\nBased-on: https://lore.kernel.org/r/20260925-prereq-url%40kernel.org/\nprerequisite-message-id: 20260920-b4-prereq@kernel.org\nprerequisite-patch-id: 0123456789abcdef0123456789abcdef01234567\n";
        let (msgids, patch_ids) = extract_prerequisite_message_and_patch_ids(body);
        assert_eq!(
            msgids,
            vec![
                "20260925-prereq-series@kernel.org",
                "20260925-prereq-url@kernel.org",
                "20260920-b4-prereq@kernel.org"
            ]
        );
        assert_eq!(patch_ids, vec!["0123456789abcdef0123456789abcdef01234567"]);
    }

    #[test]
    fn test_parse_email_decodes_quoted_printable_diff() {
        let raw = b"Message-ID: <qp-patch@example.com>\r\n\
From: Robin Dapp <rdapp.gcc@gmail.com>\r\n\
Subject: [PATCH] lra: Handle subregs in get_dependent_filter.\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=UTF-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Fix subreg handling.\r\n\
\r\n\
diff --git a/gcc/lra-constraints.cc b/gcc/lra-constraints.cc\r\n\
index 7b74ee2bef7..8685c4f97e4 100644\r\n\
--- a/gcc/lra-constraints.cc\r\n\
+++ b/gcc/lra-constraints.cc\r\n\
@@ -1,3 +1,3 @@\r\n\
-  int x =3D 0;\r\n\
+  int x =3D 1;\r\n";

        let (meta, patch) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(meta.is_patch_or_cover);
        let patch = patch.into_iter().next().expect("expected decoded patch");
        assert!(patch.diff.contains("-  int x = 0;\n+  int x = 1;"));
        assert!(!patch.diff.contains("=3D"));
    }

    #[test]
    fn test_parse_email_extracts_attached_format_patch() {
        let raw = b"Message-ID: <attached-patch@example.com>\r\n\
From: Dev <dev@gcc.gnu.org>\r\n\
Subject: [PATCH] tree-vect: fix loop bound\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
\r\n\
--BOUNDARY\r\n\
Content-Type: text/plain; charset=UTF-8\r\n\
\r\n\
Bootstrapped and regtested on x86_64-pc-linux-gnu.\r\n\
--BOUNDARY\r\n\
Content-Type: text/x-patch; name=\"0001-fix.patch\"\r\n\
Content-Disposition: attachment; filename=\"0001-fix.patch\"\r\n\
\r\n\
From 0123456789abcdef0123456789abcdef01234567 Mon Sep 17 00:00:00 2001\r\n\
From: Dev <dev@gcc.gnu.org>\r\n\
Date: Thu, 22 Oct 2026 10:00:00 +0000\r\n\
Subject: [PATCH] tree-vect: fix loop bound\r\n\
\r\n\
gcc/ChangeLog:\r\n\
\t* tree-vect-loop.cc: Fix bound.\r\n\
---\r\n\
diff --git a/gcc/tree-vect-loop.cc b/gcc/tree-vect-loop.cc\r\n\
index 1111111..2222222 100644\r\n\
--- a/gcc/tree-vect-loop.cc\r\n\
+++ b/gcc/tree-vect-loop.cc\r\n\
@@ -10,3 +10,3 @@\r\n\
-  return 0;\r\n\
+  return 1;\r\n\
--BOUNDARY--\r\n";

        let (meta, patch) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(meta.is_patch_or_cover);
        let patch = patch
            .into_iter()
            .next()
            .expect("expected attached patch to be extracted");
        assert!(patch.diff.contains("diff --git a/gcc/tree-vect-loop.cc"));
        assert!(
            !patch.diff.contains("From 0123456789abcdef"),
            "format-patch envelope header should be stripped"
        );
    }

    #[test]
    fn test_quoted_diff_does_not_produce_patch() {
        let raw = b"Message-ID: <quoted-diff@example.com>\r\n\
From: Reviewer <reviewer@gcc.gnu.org>\r\n\
Subject: Re: [PATCH] fix bug\r\n\
\r\n\
On Thu, Oct 22, Author wrote:\r\n\
> diff --git a/file.c b/file.c\r\n\
> --- a/file.c\r\n\
> +++ b/file.c\r\n\
> @@ -1 +1 @@\r\n\
> -old\r\n\
> +new\r\n";

        let (meta, patch) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(!meta.is_patch_or_cover);
        assert!(patch.is_empty());
    }

    #[test]
    fn test_diffless_ping_is_not_patchset() {
        for subject in [
            "Ping: [PATCH] c++: fix template",
            "[PING] [PATCH] c++: fix template",
            "[PING^2][PATCH] c++: fix template",
        ] {
            let raw =
                format!("Message-ID: <ping@example.com>\r\nSubject: {subject}\r\n\r\nPing!\r\n");
            let (meta, patch) = parse_email(raw.as_bytes(), MailReading::FormatPatch).unwrap();
            assert!(
                !meta.is_patch_or_cover,
                "diffless ping '{subject}' should not be treated as a patchset"
            );
            assert!(patch.is_empty());
        }
    }

    #[test]
    fn test_commit_tag_subject_index() {
        assert_eq!(
            parse_subject_index("[gccrs COMMIT 10/12] gccrs: add check"),
            (10, 12)
        );
    }

    #[test]
    fn test_promote_reply_patch_subject() {
        assert!(is_promotable_reply_patch_subject(
            "Re: [PATCH] lra: Handle subregs in get_dependent_filter."
        ));
        assert!(!is_promotable_reply_patch_subject(
            "Forwarded: Re: [PATCH] lra: Handle subregs."
        ));
        assert!(!is_promotable_reply_patch_subject(
            "Re: [syzbot] bug report"
        ));
        assert!(!is_promotable_reply_patch_subject(
            "Re: [syzbot] bug in patch handler"
        ));
        assert_eq!(
            promote_reply_patch_subject(
                "Re: [PATCH] lra: Handle subregs in get_dependent_filter.",
                2
            ),
            "[PATCH v2] lra: Handle subregs in get_dependent_filter."
        );
        assert_eq!(
            promote_reply_patch_subject(
                "Re: [PATCH v2] lra: Handle subregs in get_dependent_filter.",
                3
            ),
            "[PATCH v3] lra: Handle subregs in get_dependent_filter."
        );
    }

    #[test]
    fn test_only_the_rich_reading_splits_a_batch() {
        let raw = b"Message-ID: <batch>\r\nSubject: two fixes\r\n\r\n\
The first one:\n\
\n\
--- a/a.c\n\
+++ b/a.c\n\
@@ -1,1 +1,1 @@\n\
-old\n\
+new\n\
\n\
And the second one:\n\
\n\
--- a/b.c\n\
+++ b/b.c\n\
@@ -2,1 +2,1 @@\n\
-x\n\
+y\n";
        let (meta, patches) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert_eq!(meta.total, 1);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].message_id, "batch");
        assert!(patches[0].diff.contains("b/a.c") && patches[0].diff.contains("b/b.c"));

        let (meta, patches) = parse_email(raw, MailReading::Rich).unwrap();
        assert_eq!(meta.total, 2);
        let ids: Vec<_> = patches.iter().map(|p| p.message_id.as_str()).collect();
        assert_eq!(ids, ["batch#1", "batch#2"]);
        assert!(patches[0].diff.contains("b/a.c") && !patches[0].diff.contains("b/b.c"));
        assert!(patches[1].diff.contains("b/b.c"));
    }

    #[test]
    fn test_an_indented_diff_in_a_report_is_no_patch() {
        // A bug report showing the diff it applied locally, indented as prose.
        let raw = b"Message-ID: <report>\r\nSubject: motd shows the wrong version\r\n\r\n\
After the update motd shows the old version. I changed it like this:\n\
\n\
\x20 --- etc/motd\n\
\x20 +++ etc/motd\n\
\x20 @@ -1,4 +1,4 @@\n\
\x20 -OpenBSD 7.8\n\
\x20 +OpenBSD 7.9\n\
\n\
Is that expected?\n";
        let (meta, patches) = parse_email(raw, MailReading::Rich).unwrap();
        assert!(!meta.is_patch_or_cover);
        assert!(patches.is_empty());
    }

    #[test]
    fn test_a_changeset_keeps_the_headers_that_introduce_it() {
        let body = "\
Here is the first batch.

From 18503e2b4d5c6f7a8b9c0d1e2f3a4b5c6d7e8f90 Mon Sep 17 00:00:00 2001
From: A <a@example.com>
Subject: [PATCH 01/02] qwz: reject state changes while stopped

Backport qwx.c,v 1.75.
---
 sys/dev/ic/qwz.c | 5 +++++

diff --git sys/dev/ic/qwz.c sys/dev/ic/qwz.c
@@ -859,6 +859,11 @@ qwz_newstate(void)
+\tfirst
 
From 40ca1b2c3d4e5f60718293a4b5c6d7e8f9012345 Mon Sep 17 00:00:00 2001
From: A <a@example.com>
Subject: [PATCH 02/02] qwz: retain active key task arguments

Recheck association state after waits.
---
 sys/dev/ic/qwz.c | 4 ++++

diff --git sys/dev/ic/qwz.c sys/dev/ic/qwz.c
@@ -743,6 +743,12 @@ qwz_add_sta_key(void)
+\tsecond
";
        let sets = split_changesets(body);
        assert_eq!(sets.len(), 2, "two changesets");

        assert_eq!(
            sets[0].subject.as_deref(),
            Some("[PATCH 01/02] qwz: reject state changes while stopped")
        );
        assert_eq!(sets[0].message, "Backport qwx.c,v 1.75.");
        assert_eq!(
            sets[1].subject.as_deref(),
            Some("[PATCH 02/02] qwz: retain active key task arguments")
        );
        assert_eq!(sets[1].message, "Recheck association state after waits.");

        // Each diff stops where the next commit is announced, so no part
        // carries the headers of the one after it.
        assert!(sets[0].diff.contains("first"), "{}", sets[0].diff);
        assert!(
            !sets[0].diff.contains("PATCH 02/02"),
            "the next commit's headers leaked into this diff: {}",
            sets[0].diff
        );
        assert!(sets[1].diff.contains("second"), "{}", sets[1].diff);
    }

    #[test]
    fn test_a_multi_file_diff_is_one_changeset() {
        let git = "Here is the fix.\n\n\
            diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n\
            diff --git a/b.h b/b.h\n--- a/b.h\n+++ b/b.h\n@@ -9,1 +9,1 @@\n-x\n+y\n";
        assert_eq!(split_changesets(git).len(), 1);

        let cvs = "It seems we can only assume this.\n\n\
            Index: a.c\n===================================================================\n\
            RCS file: /cvs/src/a.c,v\nretrieving revision 1.2\ndiff -u -p -r1.2 a.c\n\
            --- a.c\n+++ a.c\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n\
            Index: b.c\n===================================================================\n\
            RCS file: /cvs/src/b.c,v\nretrieving revision 1.1\ndiff -u -p -r1.1 b.c\n\
            --- b.c\n+++ b.c\n@@ -3,1 +3,1 @@\n-p\n+q\n";
        assert_eq!(split_changesets(cvs).len(), 1);
    }

    #[test]
    fn test_a_wrapped_context_line_does_not_split_a_patch() {
        // A mailer broke one long context line across three, and only the
        // first keeps the space that marks it as context.
        let body = "Here is a fix:\n\n\
            --- /usr/include/wchar.h.orig\tSat Oct  3 17:45:17 2026\n\
            +++ /usr/include/wchar.h\tSat Oct  3 17:52:29 2026\n\
            @@ -137,6 +137,7 @@\n\
            \n \
            #    if _LIBCPP_HAS_WIDE_CHARACTERS\n\
            #      if defined(__cplusplus) &&\n\
            !defined(_LIBCPP_WCHAR_H_HAS_CONST_OVERLOADS) &&\n\
            defined(_LIBCPP_PREFERRED_OVERLOAD)\n\
            +#if 0\n\
            @@ -191,6 +192,7 @@\n\
            +#endif\n";
        assert_eq!(
            split_changesets(body).len(),
            1,
            "a wrapped context line is not a patch boundary"
        );
    }

    #[test]
    fn test_undecoded_quoted_printable_does_not_split_a_patch() {
        // "=20" is a quoted-printable space left undecoded; it sits against
        // hunk content, not after a blank line.
        let body = "One patch, four files:\n\n\
            diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,1 +1,1 @@\n-old\n+new\n\
            =20\n\
            diff --git a/b.c b/b.c\n--- a/b.c\n+++ b/b.c\n@@ -3,1 +3,1 @@\n-p\n+q\n";
        assert_eq!(
            split_changesets(body).len(),
            1,
            "a quoted-printable artefact is not a patch boundary"
        );
    }

    #[test]
    fn test_prose_between_diffs_splits_them() {
        let body = "First fix:\n\n\
            diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,1 +1,1 @@\n-old\n+new\n\
            \nAnd the second, unrelated one:\n\n\
            Index: b.c\n===================================================================\n\
            --- b.c\n+++ b.c\n@@ -3,1 +3,1 @@\n-p\n+q\n";
        assert_eq!(split_changesets(body).len(), 2);
    }

    #[test]
    fn test_a_quoted_diff_is_not_a_changeset() {
        let body = "I think this is wrong:\n\n\
            > diff --git a/a.c b/a.c\n> --- a/a.c\n> +++ b/a.c\n> @@ -1,1 +1,1 @@\n> -old\n> +new\n\
            \nbecause it breaks the lock.\n";
        assert!(split_changesets(body).is_empty());
    }

    #[test]
    fn test_prose_without_a_hunk_yields_no_changeset() {
        let body = "See below.\n\n--- this is a separator, not a diff ---\n\nthanks\n";
        assert!(split_changesets(body).is_empty());
    }

    #[test]
    fn test_a_hunk_without_a_file_header_is_not_a_changeset() {
        let body = "You can also drop the unused declaration above:\n\n\
            @@ -532,3 +532,2 @@ struct if_sffpage {\n struct socket;\n-struct ifq_ops;\n struct ifnet;\n";
        assert!(split_changesets(body).is_empty());
    }

    /// Base64 as a mailer writes it: 76 columns, CRLF between lines.
    #[cfg(feature = "server")]
    fn base64(text: &str) -> String {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        encoded
            .as_bytes()
            .chunks(76)
            .map(|line| std::str::from_utf8(line).unwrap())
            .collect::<Vec<_>>()
            .join("\r\n")
    }

    #[cfg(feature = "server")]
    const EM_RESET: &str = "From 1111aaaa2222bbbb3333cccc4444dddd5555eeee Mon Sep 17 00:00:00 2001\nFrom: A <a@example.com>\n\
Subject: [PATCH 1/2] em: reset ring\n\nReset the ring on attach.\n---\n\n\
diff --git a/sys/dev/em.c b/sys/dev/em.c\n--- a/sys/dev/em.c\n+++ b/sys/dev/em.c\n\
@@ -1,2 +1,2 @@\n \tctx\n-\told\n+\tnew\n";

    #[cfg(feature = "server")]
    const EM_SCRATCH: &str = "From 2222bbbb3333cccc4444dddd5555eeee6666ffff Mon Sep 17 00:00:00 2001\nFrom: A <a@example.com>\n\
Subject: [PATCH 2/2] em: clear scratch\n\nClear the scratch buffer.\n---\n\n\
diff --git a/sys/dev/em.c b/sys/dev/em.c\n--- a/sys/dev/em.c\n+++ b/sys/dev/em.c\n\
@@ -9,2 +9,2 @@\n \tctx\n-\tx\n+\ty\n";

    #[cfg(feature = "server")]
    const PF_FIX: &str = "--- sys/net/pf.c\t1 Jan 2026\n+++ sys/net/pf.c\t2 Jan 2026\n\
@@ -10,3 +10,3 @@ pf_test(void)\n \tint a;\n-\ta = 0;\n+\ta = 1;\n \treturn a;\n";

    #[cfg(feature = "server")]
    #[test]
    fn test_rich_reading_unwraps_a_signed_multipart() {
        let cover = format!("Hi,\n\ntwo fixes for em(4).\n\n\n{EM_RESET}\n\n{EM_SCRATCH}");
        let raw = format!(
            "Message-ID: <smime>\r\nSubject: em: two fixes\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/signed; protocol=\"application/pkcs7-signature\"; boundary=\"S\"\r\n\r\n\
--S\r\nContent-Type: multipart/mixed; boundary=\"M\"\r\n\r\n\
--M\r\nContent-Type: text/plain; charset=\"utf-8\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
{}\r\n\
--M\r\nContent-Type: text/x-patch; name=\"0001.patch\"\r\n\
Content-Disposition: attachment; filename=\"0001.patch\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
{}\r\n\
--M\r\nContent-Type: text/x-patch; name=\"0002.patch\"\r\n\
Content-Disposition: attachment; filename=\"0002.patch\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
{}\r\n\
--M--\r\n\r\n\
--S\r\nContent-Type: application/pkcs7-signature; name=\"smime.p7s\"\r\n\
Content-Disposition: attachment; filename=\"smime.p7s\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
MIAGCSqGSIb3DQEHAqCAMIACAQExDzANBglghkgBZQMEAgEFADCABgkqhkiG9w0BBwEAAKCC\r\n\
--S--\r\n",
            base64(&cover),
            base64(EM_RESET),
            base64(EM_SCRATCH)
        );

        let (meta, patches) = parse_email(raw.as_bytes(), MailReading::Rich).unwrap();
        assert!(meta.is_patch_or_cover);
        assert!(meta.body.starts_with("Hi,"), "{}", meta.body);
        assert_eq!(meta.total, 2);
        let ids: Vec<_> = patches.iter().map(|p| p.message_id.as_str()).collect();
        assert_eq!(
            ids,
            ["smime#1", "smime#2"],
            "inline and attached copies merge"
        );
        assert_eq!(
            patches[0].subject.as_deref(),
            Some("[PATCH 1/2] em: reset ring")
        );
        assert_eq!(patches[0].body, "Reset the ring on attach.");
        assert!(patches[0].diff.contains("+\tnew"), "{}", patches[0].diff);
        assert!(patches[1].diff.contains("+\ty"), "{}", patches[1].diff);

        // Read as format-patch, the attachments join the body as one patch.
        let (_, patches) = parse_email(raw.as_bytes(), MailReading::FormatPatch).unwrap();
        assert_eq!(patches.len(), 1);
    }

    #[test]
    fn test_rich_reading_decodes_quoted_printable() {
        let raw = b"Message-ID: <qp>\r\nSubject: [PATCH] fix compare\r\nMIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=\"utf-8\"\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n\
--- a/x.c\r\n+++ b/x.c\r\n@@ -1,1 +1,1 @@\r\n\
-\tif (a =3D b && a_long_condition_that_a_mailer_will_wrap_somewhere_along_th=\r\n\
e_way)\r\n\
+\tif (a =3D=3D b && a_long_condition_that_a_mailer_will_wrap_somewhere_along_=\r\n\
the_way)\r\n";
        let (_, patches) = parse_email(raw, MailReading::Rich).unwrap();
        assert_eq!(patches.len(), 1);
        assert!(
            patches[0].diff.contains(
                "+\tif (a == b && a_long_condition_that_a_mailer_will_wrap_somewhere_along_the_way)"
            ),
            "{}",
            patches[0].diff
        );
    }

    #[cfg(feature = "server")]
    #[test]
    fn test_rich_reading_takes_a_diff_that_is_only_attached() {
        let mail = |subject: &str| {
            format!(
                "Message-ID: <att>\r\nSubject: {subject}\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"M\"\r\n\r\n\
--M\r\nContent-Type: text/plain\r\n\r\nThis stops pf_test from returning zero.\r\n\
--M\r\nContent-Type: application/octet-stream; name=\"pf.diff\"\r\n\
Content-Disposition: attachment; filename=\"pf.diff\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
{}\r\n\
--M\r\nContent-Type: text/plain; name=\"dmesg.txt\"\r\n\
Content-Disposition: attachment; filename=\"dmesg.txt\"\r\n\r\n\
OpenBSD 7.8-current (GENERIC.MP)\r\n--- dmesg ends ---\r\n\
--M\r\nContent-Type: image/png; name=\"panic.png\"\r\n\
Content-Disposition: attachment; filename=\"panic.png\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==\r\n\
--M--\r\n",
                base64(PF_FIX)
            )
        };

        for subject in ["pf: return one", "Re: pf: return one"] {
            let raw = mail(subject);
            let (meta, patches) = parse_email(raw.as_bytes(), MailReading::Rich).unwrap();
            if subject.starts_with("Re:") {
                assert!(meta.is_reply_patch, "{subject}");
            } else {
                assert!(meta.is_patch_or_cover, "{subject}");
            }
            assert_eq!(meta.body, "This stops pf_test from returning zero.");
            assert_eq!(
                patches.len(),
                1,
                "{subject}: the dmesg and image are no diffs"
            );
            assert_eq!(patches[0].message_id, "att");
            assert!(patches[0].diff.contains("+\ta = 1;"), "{}", patches[0].diff);
            assert!(!patches[0].diff.contains("dmesg"), "{}", patches[0].diff);
        }
    }

    #[cfg(feature = "server")]
    #[test]
    fn test_the_attached_copy_of_a_damaged_inline_diff_wins() {
        // The mailer turned tabs into spaces in the inline copy, including the
        // one between the path and the date.
        let damaged = PF_FIX.replace('\t', "        ");
        let other = "--- sys/net/if.c\n+++ sys/net/if.c\n@@ -5,1 +5,1 @@\n-\tb = 0;\n+\tb = 2;\n";
        let raw = format!(
            "Message-ID: <twice>\r\nSubject: pf: return one\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"M\"\r\n\r\n\
--M\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: 8bit\r\n\r\n\
Inline for reading, attached for applying.\r\n\r\n{}\r\n\
--M\r\nContent-Type: application/octet-stream; name=\"pf.diff\"\r\n\
Content-Disposition: attachment; filename=\"pf.diff\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
{}\r\n\
--M\r\nContent-Type: text/x-diff; name=\"if.diff\"\r\n\
Content-Disposition: attachment; filename=\"if.diff\"\r\n\r\n\
{}\r\n\
--M--\r\n",
            damaged.replace('\n', "\r\n"),
            base64(PF_FIX),
            other.replace('\n', "\r\n")
        );

        let (meta, patches) = parse_email(raw.as_bytes(), MailReading::Rich).unwrap();
        assert_eq!(meta.total, 2);
        let ids: Vec<_> = patches.iter().map(|p| p.message_id.as_str()).collect();
        assert_eq!(ids, ["twice#1", "twice#2"]);
        assert!(patches[0].diff.contains("+\ta = 1;"), "{}", patches[0].diff);
        assert!(patches[1].diff.contains("+\tb = 2;"), "{}", patches[1].diff);
        // A bare attached diff is explained by the mail's body.
        assert!(
            patches[1].body.starts_with("Inline for reading"),
            "{}",
            patches[1].body
        );
    }
}
