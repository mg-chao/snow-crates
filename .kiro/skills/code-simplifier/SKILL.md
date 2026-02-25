---
name: code-simplifier
description: Simplify and refine recently modified code for clarity, consistency, and maintainability while preserving exact functionality. Use after implementing features, fixing bugs, or refactoring when code should be polished before review.
license: Apache-2.0
metadata:
  source: https://github.com/anthropics/claude-plugins-official/tree/main/plugins/code-simplifier
  source_author: Anthropic
  source_version: 1.0.0
  adapted_for: Kiro
---

# Code Simplifier

## Goal

Refine code so it is easier to read, easier to maintain, and more consistent with project conventions, without changing behavior.

## Scope

Focus on code changed in the current task or recent edits unless the user requests a wider review.

## Simplification Workflow

1. Identify recently modified files and touched code blocks.
2. Confirm required behavior from surrounding code, tests, and task context.
3. Apply project-specific standards from repository guidance (for example `AGENTS.md`, `CLAUDE.md`, lint rules, formatter rules, and existing local patterns).
4. Simplify structure while preserving semantics.
5. Validate that behavior is unchanged.
6. Summarize only meaningful simplifications.

## Simplification Rules

- Preserve all features, outputs, side effects, and external interfaces.
- Prefer explicit control flow and names over dense one-liners.
- Avoid nested ternary operators; use `if/else` chains or `switch` for multi-branch logic.
- Reduce unnecessary nesting and remove redundant abstractions.
- Keep helpful abstractions that improve separation of concerns.
- Remove comments that only restate obvious code, but keep comments that capture intent, constraints, or non-obvious decisions.
- Keep error handling consistent with project patterns.

## Quality Checks

When available, run relevant tests, type checks, linters, or build steps for touched files. If checks cannot run, state that clearly.
