# Security policy

`agent-tail` reads local coding-agent session data. It is designed to be read-only, offline,
and bounded in memory, but security reports are welcome.

## Reporting a vulnerability

Please do not open a public issue for a security problem. Use GitHub's private vulnerability
reporting for this repository:

<https://github.com/tsconfigdotjson/agent-tail/security/advisories/new>

Include the affected version or commit, a description of the issue, reproduction steps, and
any suggested fix. We will acknowledge the report and coordinate disclosure.

## Scope

Useful reports include:

- a path traversal or symlink issue in transcript discovery;
- reading or writing outside the documented harness roots;
- opening a live database instead of a private snapshot;
- leaking a secret or real home path through a fixture, report, log, or error message;
- a denial-of-service issue caused by malformed input, unbounded memory, or excessive parsing;
- an unexpected network call or subprocess.

The fixture scanner is a safety net, not a guarantee. If you find a secret or personal path in
the committed corpus, report it privately so it can be removed and the capture process fixed.
