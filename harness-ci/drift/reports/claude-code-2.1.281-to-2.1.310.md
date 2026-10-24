# Harness shape drift: claude_code

- Baseline: `2.1.281` (linux)
- Current: `2.1.310` (linux)
- Shape changed: **yes**

The survey compares record types, field paths, content-block types and small
vocabularies. Ids, timestamps and free-text values are deliberately not surveyed, so
value-only changes are not drift.

### Added record types

- `api-request`
- `api-request-blob`
- `api-request-shape`

### Added field paths in `api-request`

- `base`
- `cacheControl`
- `cacheControl[]`
- `cacheControl[].path`
- `cacheControl[].path[]`
- `cacheControl[].value`
- `cacheControl[].value.type`
- `id`
- `keep`
- `params`
- `params.betas`
- `params.betas[]`
- `params.context_management`
- `params.context_management.edits`
- `params.context_management.edits[]`
- `params.context_management.edits[].keep`
- `params.context_management.edits[].type`
- `params.max_tokens`
- `params.model`
- `params.thinking`
- `querySource`
- `requestDigest`
- `sessionId`
- `shapeHash`
- `tail`
- `tail[]`
- `timestamp`
- `type`
- `version`

### Added field paths in `api-request-blob`

- `hash`
- `message`
- `message.content`
- `message.role`
- `sessionId`
- `type`

### Added field paths in `api-request-shape`

- `sessionId`
- `shape`
- `shapeHash`
- `timestamp`
- `type`
- `version`

### Added field paths in `assistant`

- `message.usage.fallback_credit`
- `requestRef`
- `requestedModel`

### Added field paths in `attachment`

- `renderedBesideToolResult`
- `renderedRole`

### Added field paths in `user`

- `permissionDecision`
- `permissionDecision.decision`
- `permissionDecision.grantScope`
- `permissionDecision.reasonCode`
- `permissionDecision.reasonType`
- `permissionDecision.source`
- `sandboxed`
- `turnPosition`
- `turnPosition.promptIndex`
- `turnPosition.turnIndex`

### Added enum values in `permissionDecision.decision`

- `accept`

### Added enum values in `permissionDecision.reasonType`

- `mode`

### Added enum values in `permissionDecision.source`

- `config`
- `user_temporary`
