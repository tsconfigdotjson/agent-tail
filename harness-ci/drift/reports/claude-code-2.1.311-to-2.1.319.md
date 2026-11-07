# Harness shape drift: claude_code

- Baseline: `2.1.311` (linux)
- Current: `2.1.319` (linux)
- Shape changed: **yes**

The survey compares record types, field paths, content-block types and small
vocabularies. Ids, timestamps and free-text values are deliberately not surveyed, so
value-only changes are not drift.

### Removed record types

- `ai-title`

### Added field paths in `assistant`

- `failedResponseHadNoToolCall`
- `promptCacheRequested`
- `toolInputStreamMs`

### Removed field paths in `ai-title`

- `aiTitle`
- `sessionId`
- `type`
