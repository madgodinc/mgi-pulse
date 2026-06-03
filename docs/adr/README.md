# Architecture decision records

Notes-to-future-selves. Each one explains why a call was made and what
would have to be true to revisit it. None of them are policy — if you
have a reason to change a decision, add an ADR that supersedes the old
one and link both ways.

## Records

- [0001 — No async runtime](0001-no-async-runtime.md)
- [0002 — mmap for files, owned bytes for streams](0002-mmap-vs-stream.md)
- [0003 — `tail -F | pulse -` and `--follow` are both supported](0003-follow-two-paths.md)
- [0004 — Per-source format dispatch and auto-detect](0004-format-detection.md)

## Format

One Markdown file per decision. Sections: Status, Date, Decision,
Context, Reasons (or "Why"), Revisit when, Anti-decisions (when there
are specific things to *not* do). Keep them short — a long ADR usually
means the decision should have been split.
