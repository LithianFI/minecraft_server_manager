---
name: handoff
description: Write a handover of the current task to .claude/handoff.md so a fresh chat can resume it. Use when the user says "handoff", "handover", "write a handover", or that the context is getting full.
---

# Handoff

Writes `.claude/handoff.md` (overwriting the previous one) so a new chat can pick up the current task by reading that
file. A new chat already loads CLAUDE.md, MEMORY.md and the subsystem notes, so the handover carries only the
current task's state, not anything those files already record.

## Before writing

1. Anything agreed this session that belongs in MEMORY.md, a subsystem `CLAUDE.md` or a `docs/*-reference.md` goes
   there first (global guidelines, Project Memory). The handover then points to it.
2. Run `git status --short` and `git log --oneline -5` so the handover can name uncommitted work and the last commit.

## Rules

- Point to files, not their contents: `docs/character-reference.md` "stage 8", not a restatement of stage 8.
- Record what isn't written down anywhere: work in progress, half-made edits, failed attempts and why they failed,
  hypotheses being tested, what was verified and what wasn't, and questions waiting on the user.
- Be specific: file paths with line numbers, exact commands, exact error text.
- Keep it short, under ~60 lines. Leave out empty sections.
- The file is tracked in git; commit it only when the user asks.

## Template

Write the file in exactly this shape:

```markdown
# Handover — <task name> (<YYYY-MM-DD>)

We're continuing an ongoing task. Use this handover as the current source of truth for the task's state unless I
correct it; CLAUDE.md and MEMORY.md conventions still apply. Point out any conflict with them; don't guess.

## Goal
<what we're trying to achieve, one or two lines>

## Status
- Done: <with commit hashes where committed>
- In progress: <exactly where it stopped; uncommitted files>
- Still to do: <remaining steps, in order>

## Key context
- <files, docs sections, artifact links, tools/commands that matter for this task>
- <success criteria; what the user wants verified and how>

## Decisions made
- <decision> — <short reason>

## Avoid
- <failed approaches and why; wrong directions; out-of-scope items the user turned down>

## Open questions / blockers
- <unresolved, and who has to answer>

## Next step
- <the first concrete action>

## How to respond
- Start with a short summary of your understanding (a few lines).
- Read the files listed above only as far as the next step needs.
- Continue from "Next step"; don't restart or redo finished work.
- Ask only the questions that block the work.
```

## After writing

Tell the user the file is written, and that a new chat should start with: "Read `.claude/handoff.md` and resume."
