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

#[cfg(feature = "server")]
use anyhow::{Result, anyhow};
#[cfg(feature = "server")]
use mail_parser::{HeaderValue, Message, MessageParser, PartType};
use regex::Regex;
use std::sync::OnceLock;

/// How `parse_email` reads a mail. See designs/DESIGN_RICH_MAIL_INGESTION.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailReading {
    /// As `git format-patch` sends it: the whole text body is the one patch a
    /// mail carries, and a reply carries none.
    FormatPatch,
    /// As a list without a style guide gets it: a fresh diff in a reply is a
    /// patch, a batch of changesets in one mail is a series, and MIME is
    /// unwrapped down to the text of every part, attachments included.
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

#[cfg(feature = "server")]
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

/// Whether a subject marks a reply or forward rather than a submission.
#[cfg(feature = "server")]
fn is_reply_subject(subject: &str) -> bool {
    let subject_lower = subject.to_lowercase();
    let subject_clean = subject_lower.trim();
    subject_clean.starts_with("re:")
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
}

#[cfg(feature = "server")]
pub fn parse_email(
    raw_email: &[u8],
    reading: MailReading,
) -> Result<(PatchsetMetadata, Vec<Patch>)> {
    let received_date = extract_received_date(raw_email);

    let parser = MessageParser::default()
        .with_address_headers()
        .with_message_ids()
        .header_text(mail_parser::HeaderName::Subject)
        .header_date(mail_parser::HeaderName::Date)
        .header_address("X-Original-From");
    // Naming any header to parse leaves every other one raw, the MIME headers
    // included, and a message whose Content-Type is raw is one undecoded text
    // part. Only the rich reading looks past that.
    let parser = match reading {
        MailReading::FormatPatch => parser,
        MailReading::Rich => parser.with_mime_headers(),
    };
    let message = parser
        .parse(raw_email)
        .ok_or_else(|| anyhow!("Failed to parse email"))?;

    let message_id = message
        .message_id()
        .ok_or_else(|| anyhow!("No Message-ID header"))?
        .to_string();

    let subject = message.subject().unwrap_or("(no subject)").to_string();

    let mut author = message
        .from()
        .and_then(|addr| addr.first())
        .map(|a| {
            let name = a.name().unwrap_or_default().trim();
            let address = a.address().unwrap_or("unknown@localhost").trim();
            let addr = if address.is_empty() || !address.contains('@') {
                "unknown@localhost"
            } else {
                address
            };
            if name.is_empty() || name.to_lowercase() == "unknown" {
                addr.to_string()
            } else {
                format!("\"{}\" <{}>", name.replace('"', "\\\""), addr)
            }
        })
        .unwrap_or_else(|| "unknown@localhost".to_string());

    if let Some(first_addr) = message.header("X-Original-From").and_then(|h| match h {
        mail_parser::HeaderValue::Address(x_orig_addr) => x_orig_addr.first(),
        _ => None,
    }) {
        let name = first_addr.name().unwrap_or_default().trim();
        let address = first_addr.address().unwrap_or("").trim();
        let author_email = extract_email(&author);
        if !address.is_empty() && address.contains('@') {
            if is_b4_alias(&author_email, address) {
                author = if name.is_empty() || name.to_lowercase() == "unknown" {
                    address.to_string()
                } else {
                    format!("\"{}\" <{}>", name.replace('"', "\\\""), address)
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
                        let name = a.name().unwrap_or_default().trim();
                        if name.is_empty() {
                            Some(address.to_string())
                        } else {
                            Some(format!("\"{}\" <{}>", name.replace("\"", "\\\""), address))
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
                        let name = a.name().unwrap_or_default().trim();
                        if name.is_empty() {
                            Some(address.to_string())
                        } else {
                            Some(format!("\"{}\" <{}>", name.replace("\"", "\\\""), address))
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    let in_reply_to = match message.in_reply_to() {
        HeaderValue::Text(t) => Some(t.to_string()),
        HeaderValue::TextList(l) => l.first().map(|s| s.to_string()),
        _ => None,
    };

    let references = match message.references() {
        HeaderValue::Text(t) => vec![t.to_string()],
        HeaderValue::TextList(l) => l.iter().map(|s| s.to_string()).collect(),
        _ => vec![],
    };

    let (index, total) = parse_subject_index(&subject);
    let version = parse_subject_version(&subject);

    let mut body = String::new();
    for i in 0..message.text_body_count() {
        if let Some(text) = message.body_text(i) {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&text);
        }
    }

    // Detection logic
    let subject_lower = subject.to_lowercase();
    let subject_clean = subject_lower.trim();
    let is_reply = is_reply_subject(&subject);
    let has_patch_tag = subject_clean.contains("patch") || subject_clean.contains("rfc");

    // A message is part of a series if it's a cover letter (index 0) or has multiple parts (total > 1)
    let is_series_metadata = total > 1 || index == 0;

    let (is_patch_or_cover, total, patches) = match reading {
        MailReading::FormatPatch => {
            let has_diff = body.contains("diff --git")
                || (body.contains("--- ") && body.contains("+++ ") && body.contains("@@ -"));
            let is_submission = !is_reply && (has_patch_tag || has_diff || is_series_metadata);
            // The whole body is the one patch the mail carries. A reply
            // carries none: whatever diff it holds is review discussion.
            let patches = if has_diff && index != 0 && !is_reply {
                vec![Patch {
                    message_id: message_id.clone(),
                    body: body.clone(),
                    diff: body.clone(),
                    part_index: index,
                    subject: None,
                }]
            } else {
                Vec::new()
            };
            (is_submission, total, patches)
        }
        MailReading::Rich => {
            // A forward relays someone else's content; the forwarder isn't submitting a
            // patch, so a fresh diff inside a forward should not promote it to a patchset.
            let is_forward = subject_clean.starts_with("fwd:")
                || subject_clean.starts_with("forwarded:")
                || subject_clean.starts_with("wg:"); // German 'Weitergeleitet' (forwarded)
            let attachments = diff_attachments(&message);
            let (has_diff, total, patches) =
                read_rich(&message_id, &body, &attachments, index, total);
            // A mail carries a diff only if a changeset was found in it, so a
            // diff that is quoted or indented to discuss it does not count. A
            // reply (but not a forward) carrying one is a patch, e.g. an
            // OpenBSD diff posted as a thread reply.
            let is_submission = !is_reply && (has_patch_tag || has_diff || is_series_metadata);
            (is_submission || (has_diff && !is_forward), total, patches)
        }
    };

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
        version,
        body: body.clone(),
    };

    Ok((metadata, patches))
}

/// The rich reading of a mail's body and its diff-carrying attachments: a batch
/// of changesets in one mail is a series. Returns whether the mail carries a
/// fresh diff, its part count, and its patches.
#[cfg(feature = "server")]
fn read_rich(
    message_id: &str,
    body: &str,
    attachments: &[String],
    index: u32,
    total: u32,
) -> (bool, u32, Vec<Patch>) {
    // A body may carry more than one changeset: tech@ takes a batch of patches
    // in a single mail as readily as one per mail. Each becomes its own patch,
    // so each is reviewed on its own terms rather than fused into one diff.
    // Only a diff the sender wrote is a changeset, not ">"-quoted reply context.
    let inline = split_changesets(body);
    // Each attachment is split on its own, so a part boundary is a changeset
    // boundary too. An attached diff is never quoted context, so it is fresh.
    let attached: Vec<Changeset> = attachments
        .iter()
        .flat_map(|text| split_changesets(text))
        .collect();
    let has_attached_diff = !attached.is_empty();
    let changesets = merge_changesets(inline, attached, body);
    let has_diff = !changesets.is_empty();
    let split_total = u32::try_from(changesets.len()).unwrap_or(u32::MAX);
    // A split body is a series that arrived in one message, so it is numbered
    // like one. A single changeset leaves the subject's own numbering alone.
    let total = if split_total > 1 { split_total } else { total };

    let patches = if index == 0 {
        Vec::new() // a cover letter carries no patch of its own
    } else if split_total > 1 {
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
    } else if has_attached_diff {
        // The one changeset has an attached copy, which is the one that
        // applies, so the diff is the attachment's rather than the body.
        changesets
            .into_iter()
            .map(|changeset| Patch {
                message_id: message_id.to_string(),
                body: body.to_string(),
                diff: changeset.diff,
                part_index: index,
                subject: None,
            })
            .collect()
    } else if has_diff {
        vec![Patch {
            message_id: message_id.to_string(),
            body: body.to_string(),
            diff: body.to_string(),
            part_index: index,
            subject: None,
        }]
    } else {
        Vec::new()
    };

    (has_diff, total, patches)
}

/// The decoded text of every attachment that carries a diff, in MIME order.
///
/// Content decides, not type: a diff arrives as `text/x-patch` as readily as
/// `application/octet-stream`. A part that is not text is read as UTF-8, and
/// whatever that makes of an image or a signature carries no hunk.
#[cfg(feature = "server")]
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
#[cfg(feature = "server")]
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
#[cfg(feature = "server")]
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
#[cfg(feature = "server")]
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
#[cfg(feature = "server")]
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
#[cfg(feature = "server")]
fn opens_diff_block(line: &str) -> bool {
    line.starts_with("diff --git ")
        || line.starts_with("Index: ")
        || line.starts_with("--- ")
        || line.starts_with("@@ -")
}

/// A line that carries diff payload: hunk content, a hunk header, or a file
/// header. An empty line counts, because mailers strip the single space from a
/// blank context line.
#[cfg(feature = "server")]
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
#[cfg(feature = "server")]
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
#[cfg(feature = "server")]
pub(crate) struct Changeset {
    pub subject: Option<String>,
    pub message: String,
    pub diff: String,
}

/// The `Subject:` of a format-patch block, if its headers are there.
#[cfg(feature = "server")]
fn preamble_subject(preamble: &str) -> Option<String> {
    preamble
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("Subject: "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The commit message under that `Subject:`, up to the diffstat separator.
#[cfg(feature = "server")]
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
#[cfg(feature = "server")]
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
            .rposition(|l| crate::mbox::is_mbox_separator(l.as_bytes()))
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
    // Match [ ... M/N ... ] but strictly require PATCH, RFC, RESEND or vN before the M/N
    let re_brackets = RE_BRACKETS.get_or_init(|| {
        Regex::new(r"(?i)\[.*?\b(?:PATCH|RFC|RESEND|v\d+)\b.*?(\d+)/(\d+).*?\]").unwrap()
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
    let re_loose =
        RE_LOOSE.get_or_init(|| Regex::new(r"(?i)\b(?:PATCH|RFC|RESEND)\s+(\d+)/(\d+)\b").unwrap());

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

pub fn get_subject_prefixes(subject: &str) -> Vec<String> {
    static RE_BRACKETS: OnceLock<Regex> = OnceLock::new();
    let re = RE_BRACKETS.get_or_init(|| Regex::new(r"\[(.*?)\]").unwrap());

    let mut prefixes = Vec::new();

    for cap in re.captures_iter(subject) {
        if let Some(content) = cap.get(1) {
            // Split by whitespace or non-word characters.
            // "PATCH net-next 1/2" -> "PATCH", "net-next", "1/2"
            // "PATCH,net-next,1/2" -> "PATCH", "net-next", "1/2"
            let tokens: Vec<&str> = content
                .as_str()
                .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '.' && c != '_')
                .filter(|s| !s.is_empty())
                .collect();

            for token in tokens {
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
        }
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
    let prefixes = ["re:", "fwd:", "aw:", "forwarded:", "回复:", "回复："];

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

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
    #[test]
    fn test_b4_relay_author_parsing() {
        let raw = b"Message-ID: <b4-relay-test>\r\n\
                    From: Real Author via B4 Relay <devnull+author.example.com@kernel.org>\r\n\
                    X-Original-From: Real Author <author@example.com>\r\n\
                    Subject: Test B4 Relay\r\n\r\nBody";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert_eq!(meta.author, "\"Real Author\" <author@example.com>");
    }

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
    #[test]
    fn test_reply_with_fresh_diff_is_patchset() {
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
            meta.is_patch_or_cover,
            "Reply carrying a fresh unquoted diff SHOULD be a patchset"
        );
        assert!(
            !patch.is_empty(),
            "A patch should be extracted from the reply"
        );
    }

    #[cfg(feature = "server")]
    #[test]
    fn test_format_patch_reading_takes_no_patch_from_a_reply() {
        let raw = b"Message-ID: <fp-reply>\r\nSubject: Re: [PATCH] fix bug\r\n\r\n\
Try this instead:\n\
\n\
--- a/file.c\n\
+++ b/file.c\n\
@@ -1,1 +1,1 @@\n\
-old\n\
+new\n";
        let (meta, patches) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(!meta.is_patch_or_cover);
        assert!(patches.is_empty());

        let (meta, patches) = parse_email(raw, MailReading::Rich).unwrap();
        assert!(meta.is_patch_or_cover);
        assert_eq!(patches.len(), 1);
    }

    #[cfg(feature = "server")]
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
    const EM_RESET: &str = "From 1111 Mon Sep 17 00:00:00 2001\nFrom: A <a@example.com>\n\
Subject: [PATCH 1/2] em: reset ring\n\nReset the ring on attach.\n---\n\n\
diff --git a/sys/dev/em.c b/sys/dev/em.c\n--- a/sys/dev/em.c\n+++ b/sys/dev/em.c\n\
@@ -1,2 +1,2 @@\n \tctx\n-\told\n+\tnew\n";

    #[cfg(feature = "server")]
    const EM_SCRATCH: &str = "From 2222 Mon Sep 17 00:00:00 2001\nFrom: A <a@example.com>\n\
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

        // Read as format-patch, the body is MIME source, which holds no diff.
        let (_, patches) = parse_email(raw.as_bytes(), MailReading::FormatPatch).unwrap();
        assert!(patches.is_empty());
    }

    #[cfg(feature = "server")]
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
            assert!(meta.is_patch_or_cover, "{subject}");
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

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
    #[test]
    fn test_diff_without_patch_tag_ignored() {
        let raw = b"Message-ID: <diffnopatch>\r\nSubject: Random fix\r\n\r\ndiff --git a/file b/file\nindex...";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(
            meta.is_patch_or_cover,
            "Diff without [PATCH] tag should still be parsed as patch"
        );
    }

    #[cfg(feature = "server")]
    #[test]
    fn test_normal_patch() {
        let raw = b"Message-ID: <456>\r\nSubject: [PATCH] fix bug\r\n\r\ndiff --git a/file b/file\nindex...";
        let (meta, _) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(meta.is_patch_or_cover);
    }

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
    #[test]
    fn test_cover_letter() {
        let raw = b"Message-ID: <789>\r\nSubject: [PATCH 0/5] fix bug\r\n\r\nCover letter body";
        let (meta, patch) = parse_email(raw, MailReading::FormatPatch).unwrap();
        assert!(meta.is_patch_or_cover);
        assert!(patch.is_empty());
    }

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
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
    /// A diff touching several files is one changeset, not one per file.
    ///
    /// The next file's header follows hunk content directly, with no prose
    /// between, which is what separates it from a new patch.
    #[cfg(feature = "server")]
    #[test]
    fn test_a_changeset_keeps_the_headers_that_introduce_it() {
        let body = "\
Here is the first batch.

From 1850 Mon Sep 17 00:00:00 2001
From: A <a@example.com>
Subject: [PATCH 01/02] qwz: reject state changes while stopped

Backport qwx.c,v 1.75.
---
 sys/dev/ic/qwz.c | 5 +++++

diff --git sys/dev/ic/qwz.c sys/dev/ic/qwz.c
@@ -859,6 +859,11 @@ qwz_newstate(void)
+\tfirst
 
From 40ca Mon Sep 17 00:00:00 2001
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

    #[cfg(feature = "server")]
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

    /// Prose between two diffs makes them two changesets, whatever the format.
    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
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

    #[cfg(feature = "server")]
    #[test]
    fn test_prose_between_diffs_splits_them() {
        let body = "First fix:\n\n\
            diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,1 +1,1 @@\n-old\n+new\n\
            \nAnd the second, unrelated one:\n\n\
            Index: b.c\n===================================================================\n\
            --- b.c\n+++ b.c\n@@ -3,1 +3,1 @@\n-p\n+q\n";
        assert_eq!(split_changesets(body).len(), 2);
    }

    /// A reply quoting someone else's diff contributes no changeset.
    #[cfg(feature = "server")]
    #[test]
    fn test_a_quoted_diff_is_not_a_changeset() {
        let body = "I think this is wrong:\n\n\
            > diff --git a/a.c b/a.c\n> --- a/a.c\n> +++ b/a.c\n> @@ -1,1 +1,1 @@\n> -old\n> +new\n\
            \nbecause it breaks the lock.\n";
        assert!(split_changesets(body).is_empty());
    }

    /// Prose that merely mentions a dashed separator is not a diff.
    #[cfg(feature = "server")]
    #[test]
    fn test_prose_without_a_hunk_yields_no_changeset() {
        let body = "See below.\n\n--- this is a separator, not a diff ---\n\nthanks\n";
        assert!(split_changesets(body).is_empty());
    }

    /// A reviewer suggesting a change as a bare hunk is discussing it: without
    /// the file it applies to, it cannot be applied.
    #[cfg(feature = "server")]
    #[test]
    fn test_a_hunk_without_a_file_header_is_not_a_changeset() {
        let body = "You can also drop the unused declaration above:\n\n\
            @@ -532,3 +532,2 @@ struct if_sffpage {\n struct socket;\n-struct ifq_ops;\n struct ifnet;\n";
        assert!(split_changesets(body).is_empty());
    }
}
