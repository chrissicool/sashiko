# Design: Rich Mail Ingestion

## Context

A list that takes `git format-patch` mail gets one shape: a plain-text body,
one commit per mail, the diff inline, and discussion in replies. Reading that
shape is simple. The text body is the diff, and a reply is never a patch.

Other lists have no style guide. tech@ and bugs@ at OpenBSD take whatever a
sender writes and a mail client produces:

- **Diffs in replies.** A revised or fixup diff is posted inline as a reply in
  the thread rather than as a fresh mail.
- **Batches.** Several commits arrive in one mail, as format-patch blocks, as
  `git diff`, `cvs diff` or bare unified diffs separated by prose.
- **MIME.** Quoted-printable or base64 text, S/MIME and PGP/MIME signed
  multiparts, HTML alternatives, and diffs attached as files.

A diff in any of these shapes is a submission and has to be reviewed like an
inline one.

A sample of the last 3000 tech@ and 1500 bugs@ articles on gmane shows how
the MIME shapes spread. Of roughly 1100 tech@ mails carrying a diff:

| Shape                                      | tech@ | bugs@ |
|--------------------------------------------|------:|------:|
| Plain text, 7bit or 8bit                   | ~1040 |  ~210 |
| Quoted-printable text                      |    33 |    17 |
| Diff in a text part of a multipart         |    35 |     5 |
| Diff only in an attachment                 |    27 |    10 |
| Same diff inline and attached              |     5 |     2 |

Diff attachments arrive as `text/x-patch`, `text/x-diff`, `text/plain` and
`application/octet-stream`. The same lists attach `.c` files, dmesg output,
images, tarballs and signatures. In six of the seven mails that carry a diff
twice, the inline copy is damaged by the sender's client -- long lines wrapped,
tabs turned into spaces -- and only the attachment applies.

## Design Decisions

- **Opt-in per deployment.** `mailing_lists.rich_mail` selects the rich
  reading for every mail the deployment ingests. Off, mail is read as
  format-patch. A format-patch list gains nothing from the rich reading, and
  anything else arriving there is a mistake by the sender, not a submission to
  recover.
- **The reading is an argument, not a filter.** `parse_email` is told which
  reading to apply. A caller that needs format-patch asks for it, rather than
  trimming the rich result back afterwards.
- **Unwrap, then split, then merge.** MIME is undone before any patch parsing,
  so `split_changesets` only ever sees decoded text and stays the one patch
  parser. Each part is split on its own, so a part boundary is also a
  changeset boundary, and every changeset knows whether it came inline or
  attached.
- **Structure decides, not tooling.** The split keys on the shape of a diff
  in running text, not on any one tool's headers, so a batch splits whether it
  came from `git format-patch`, `git diff`, `cvs diff` or by hand.
- **Content decides, not type.** A part carries a diff if its decoded text
  holds a hunk. Content types and file names on these lists are whatever the
  client guessed, and `application/octet-stream` is as likely a diff as
  `text/x-patch`.
- **The attached copy wins.** When the same changeset arrives inline and
  attached, the attachment supplies the diff. The inline copy exists for
  reading and is the one a client damages.

## Architecture

### Configuration

```toml
[mailing_lists]
track = ["tech@openbsd.org:gmane.os.openbsd.tech"]
rich_mail = true
```

`rich_mail` defaults to false and is set from the environment as
`SASHIKO__MAILING_LISTS__RICH_MAIL`. It applies to every mail the deployment
ingests -- NNTP, mbox submitted through the API or `sashiko-cli` -- and to the
message bodies the web API serves, so a stored mail reads the same way it was
parsed.

Prerequisite series fetched from lore, b4 or Patchew are read as format-patch
whatever the setting: they are format-patch by construction, and in such a
thread a reply carrying a diff is review discussion, not a part of the series.

### The format-patch reading

The raw text body is the mail's body. If it carries diff markers -- a
`diff --git` line, or `--- `, `+++ ` and `@@ -` together -- the whole body is
the diff of a single patch, numbered by the subject's `[PATCH M/N]`. A reply
or forward is never a patch, whatever it carries.

### The rich reading

`parse_email` runs these steps in order:

1. **Unwrap.** Parse the MIME structure, including `Content-Type` and
   `Content-Transfer-Encoding`, and collect the leaf parts in tree order.
   Multipart containers are descended, `multipart/alternative` yields its text
   part, and each leaf is decoded to text: in its charset if it is text, as
   UTF-8 if it is not. The text-body leaves, joined, form the mail's body; the
   rest are attachments.
2. **Classify.** An attachment carries a diff if its decoded text has a hunk
   header (`@@ -`) together with a file header (`+++ ` or `diff `). Attachments
   without one are dropped.
3. **Split.** `split_changesets` runs on the body and on each diff-carrying
   attachment separately, as described under *Splitting*. Every changeset is
   tagged inline or attached.
4. **Merge.** An inline and an attached changeset are the same when their
   fingerprints match: the sequence of file paths from the `+++` headers and
   of `@@` hunk ranges, which a client's whitespace damage leaves intact. The
   merged changeset takes the attachment's diff and the inline copy's
   position. It takes the inline copy's subject and message where that copy
   has them, and the attachment's otherwise. Two inline or two attached
   changesets are never merged.
5. **Attribute.** An attached changeset with no message of its own -- a bare
   diff rather than a format-patch file -- takes the mail's body as its
   message, which is where the sender explained it.
6. **Number.** The merged changesets, in order, are the mail's patches. One
   changeset keeps the subject's numbering. More than one is a series that
   arrived in one message, numbered `1/N` to `N/N` in order, and stored as
   described under *Storage*.

Patch detection runs on the result:

- A mail that is neither a reply nor a forward is a patch or cover letter if
  its subject carries `PATCH` or `RFC`, it carries a series number, or step 6
  produced a changeset.
- A reply is a patch if step 6 produced a changeset. Quoted lines never
  produce one, and an attached diff is never quoted context, so this holds
  exactly when the reply carries a diff its sender wrote.
- A forward is never a patch. It relays someone else's mail.

The stored body is the decoded body from step 1, not the raw MIME source.

### Splitting

`split_changesets` walks a text line by line and finds where each changeset
starts. A line that can open a diff block -- `diff --git `, `Index: `, `--- `
or `@@ -` -- starts a new changeset if it follows prose, and continues the open
one if it follows diff content. That one rule tells a batch of patches apart
from a single diff touching several files.

A diff ends only at prose that opens a paragraph: a line that is neither diff
payload nor diff metadata, following a blank line. Mail damages diffs. A
mailer wrapping a long context line drops the space that marks it, and
undecoded quoted-printable leaves `=20` on a line of its own. Either looks like
prose in the middle of a hunk, but neither follows a blank line, and treating
them as a boundary would cut one patch into several. Lines beginning with `>`
are quoted context and skipped.

The text between two diffs belongs to the second. When it holds format-patch
headers, the changeset takes its subject from the `Subject:` line and its
message from the text between that line and the `---` above the diffstat. The
`From <sha>` separator that starts the next commit cuts the region before it,
so no diff carries the next commit's headers.

A region holding no hunk under a `+++` file header is dropped, so prose that
merely contains a dashed line is not taken for a patch, and neither is a bare
hunk a reviewer quotes without the file it would apply to. A git diff is kept
even without a hunk, since its only change may be a mode or a name.

### Storage

A mail split into several patches cannot store them under its own Message-ID:
`UNIQUE(patchset_id, message_id)` allows one patch per message per patchset.
Each part is stored as `<message-id>#1`, `<message-id>#2` and so on, with a
`messages` row of its own written before the patch that refers to it.

Thread and list views leave out messages whose id contains `#`, so a split
mail shows once in its thread.

## Error Handling

- **Undecodable text.** A part in an unknown charset is decoded lossily. A
  damaged character costs a review at most a misread line, while dropping the
  part could lose the only copy of a diff.
- **Malformed MIME.** A tree the parser cannot fully read yields whatever leaves
  it did read. A mail never fails to ingest on account of its MIME structure.
- **Fingerprint collision.** Two different changesets with the same files and
  hunk ranges would merge, and the attached diff would be the one reviewed.
  Such a collision comes from an inline diff and its attached revision, so the
  attached one is the right one to keep.
- **Signatures and non-diff attachments** never reach `split_changesets`: an
  image, a signature or a dmesg carries no hunk, whatever its decoding made of
  it.
- **`#` in a real Message-ID.** RFC 5322 allows it, and such a message would
  be left out of thread listings too. It is still stored, reviewed and
  reachable by its id; only the listing skips it.

## Out of Scope

- `format=flowed` text. 37 sampled diff mails use it, and how far unflowing
  restores their diffs is not established.
- Compressed attachments (`.diff.gz`, tarballs). The sample has a handful, none
  clearly a patch.
- `message/rfc822` parts. A forwarded mail is not a submission.
- HTML-only mail. The parser's HTML-to-text conversion reaches the pipeline
  as-is; a diff that survives it is reviewed, one that does not is lost.

## Status

Proposed. Not implemented.
