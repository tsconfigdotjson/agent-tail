# Harness shape drift: claude_code

- Baseline: `2.1.322` (linux)
- Current: `2.1.325` (linux)
- Shape changed: **yes**

The survey compares record types, field paths, content-block types and small
vocabularies. Ids, timestamps and free-text values are deliberately not surveyed, so
value-only changes are not drift.

### Added field paths in `assistant`

- `apiErrorType`
- `madeBy`
