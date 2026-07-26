# Write the release notes for 0.2

An example of a task body worth writing. Pass it in with either

    hird add "Write the release notes" --body-file examples/task-body.md
    git log --oneline v0.1..HEAD | hird add "Write the release notes" --body-file -

The body is markdown, is handed to the agent verbatim by `task_get`, and is the
only context the agent has that you did not say out loud. Write it for someone
who was not in the room: a different harness, a different model, next week.

## What to do

Add a `## 0.2` section to `CHANGELOG.md`, above `## 0.1`, covering everything
merged since the `v0.1` tag.

## How to decide what goes in

- One line per user-visible change, in the imperative: "Refuse overlapping
  claims when `path_conflicts = "refuse"`".
- Group under **Added**, **Changed** and **Fixed**. Drop a heading rather than
  writing "none".
- Refactors, test-only changes and dependency bumps do not belong here.

## Done means

- `CHANGELOG.md` has the new section and nothing else changed.
- Every entry names something a user of `hird` can observe from the outside.
- `just check` passes.

## Notes

The `v0.1` tag is the boundary, not the merge date — some commits landed on
branches cut before it.
