# Architecture Decision Records

These are notes-to-future-selves. They explain **why** a design call
was made and what would have to be true for it to be revisited.
None of them are policy; if you have a good reason to change one,
add an ADR that supersedes the old one and link both ways.

## Records

- [0001 — No async runtime](0001-no-async-runtime.md)
- [0002 — mmap for files, owned bytes for streams](0002-mmap-vs-stream.md)
- [0003 — `tail -F | pulse -` AND `--follow` both supported](0003-follow-two-paths.md)
- [0004 — Per-source format dispatch + auto-detect](0004-format-detection.md)

## Format

Each ADR is a single Markdown file with sections: Status, Date,
Decision, Context, Why, Revisit if, Anti-decisions (when there are
specific things to *not* do). Keep them short — a long ADR is a
sign the decision should have been split.
