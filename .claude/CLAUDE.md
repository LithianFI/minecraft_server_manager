# Global Guidelines

These apply to every project. Project-level CLAUDE.md files add to or override them.

## Precedence
- Existing codebase conventions take precedence over this file. When they conflict, follow the codebase and mention the discrepancy.
- Follow the language's idiomatic naming and style (e.g. PascalCase methods in C#, snake_case in Python).

## Process & Communication
- Ask when ambiguity would change the design or behavior. For minor choices, pick the simplest option and state the assumption.
- If there is a better way to achieve what I want, suggest it before implementing.
- For larger changes or additions (new features, changes spanning several files, design decisions, or changes to existing behavior), present a plan stating what will be changed and why, and wait for confirmation.
- For small changes or precise instructions, skip the plan and make the change directly.
- Keep changes targeted; don't modify anything that isn't required for the task.
- If you notice something that could be improved outside the task's scope, mention it rather than changing it.
- Prefer simple solutions; avoid unnecessary complexity and speculative abstraction.

## Before Making Changes
- Read what the change requires; for larger changes, also check callers and existing patterns for similar problems.
- Understand the cause before fixing a bug; don't patch symptoms. If the cause is unclear, investigate or ask rather than guessing.
- For larger changes, consider at least one alternative and note in a sentence why you chose your approach.
- Identify what else the change affects (callers, tests, configuration, docs) before editing.
- If the task turns out larger or different than expected, stop and report before continuing.

## Ask Before
- Adding, removing, or upgrading dependencies
- Changing public APIs or contracts
- Deleting files
- Committing, pushing, or rewriting git history
- Modifying configuration, infrastructure, or deployment files

## Efficiency
- Search (grep/glob) to locate code before opening files; read only the relevant sections of large files.
- Don't re-read files you've already read unless they've changed.
- Make targeted edits; don't rewrite whole files for small changes.
- Run the narrowest relevant tests first, with quiet or filtered output; run the full suite before reporting done.
- Keep responses concise. Don't narrate minor steps; report results, decisions, and anything that needs my attention.
- Summarize what changed and why; don't echo back code you just wrote or restate my request.

## Verification
- Before reporting a task complete, build and run the relevant tests.
- A successful build only proves it compiles. For logic changes (not layout or captions), also run the code, scaling the depth to the change's size and risk:
  - Small, low-risk changes: existing tests covering the path, or a quick run of the affected code, are enough.
  - Larger changes, new logic, or untested assumptions about library behavior: if existing tests don't exercise the changed path, write a small throwaway program in a scratch directory outside the repo (never in the repo) that exercises it, run it, and report what it showed.
- Say which level of verification you chose and why.
- State explicitly what was verified and what wasn't (e.g. "builds and unit tests pass; not run against a live server").

## Code Organization
- Aim for functions under ~30 lines; extract helpers when nesting exceeds two levels.
- One function, one responsibility.
- Group related constants at module level; no magic numbers inline.
- Use intention-revealing names; avoid abbreviations except domain-standard ones.
- Booleans should read as questions: `isValid` / `is_valid`, `hasPermission`, `shouldRetry`.

## Validation & Errors
- Validate inputs and preconditions at function entry; use early returns to keep nesting shallow.
- Use the project's established error-handling pattern. If none exists: exceptions for unexpected failures, Try-pattern or Result types for expected ones. Don't mix approaches within a module.
- Never swallow exceptions or lose stack traces; add context when rethrowing.
- Avoid returning null where an empty collection or Try-pattern works; enable nullable annotations where the language supports them.

## Architecture
- Prefer immutable data and pure transformations; minimize mutable state.
- Keep side effects (I/O, network, database) at the edges.
- Inject abstractions at seams: I/O, external services, and anything that needs swapping in tests. Don't create interfaces with a single implementation by default.
- Keep modules loosely coupled through well-defined contracts.

## Security
- Treat all external input as untrusted; validate it before use.
- Use parameterized queries and safe APIs; never build SQL, shell commands, or markup by string concatenation.
- Never trust client-side data for authorization; verify server-side.
- Never log secrets, credentials, or personal data at any log level.

## Performance
- Batch operations against slow resources (database, network) instead of looping one call at a time.
- Don't cache speculatively; cache when repeated use is evident or measured, and define invalidation at the same time.

## Configuration
- Files on disk are the source of truth for configuration and infrastructure (e.g. Compose files over UI-driven setup).
- No hard-coded environment-specific values; externalize them.
- Validate configuration at startup and fail fast on missing required settings.

## Comments
- Comments explain why, not what. Keep them short.
- Longer explanations are fine for non-obvious domain, protocol, or vendor behavior.

## Project Memory
- If the project has a MEMORY.md, keep it current: when a convention, architectural decision, or notable edge case is agreed, update it immediately rather than waiting for the end of the session.
- When I say "wrap up", summarize the session's decisions, new conventions, and known technical debt into MEMORY.md.
- Keep MEMORY.md under ~100 lines; remove outdated entries and condense rather than appending indefinitely.

## This Project
@MEMORY.md
