---
name: hird
description: Coordinate coding agents through the hird shared work queue and memory. Use when the user asks to pick up a numbered hird task, work the queue, coordinate parallel agents, or search and record shared project knowledge.
---

# Work through hird

Use the connected hird MCP tools. Treat the queue as shared live state: other
agents and a human may be using it at the same time.

## Get work

- If the user names a task number, call `task_claim` for that `seq`.
- If the user says to work the queue without naming a task, call `task_next`.
  Call it again after finishing each task until it reports that nothing is
  workable.
- Claim before changing files. If another agent holds the task, tell the user
  who holds it instead of doing the same work independently.
- Read the claimed task and its `recalled` facts before starting.

## Work safely

1. Call `task_scope` as soon as you know which files or globs you will change.
   If it reports overlap, coordinate before editing those files.
2. Use `task_update` for concise progress notes and before the lease heartbeat
   expires. Include newly discovered file scope.
3. Treat `footprint`, `changed`, `undeclared`, and `contended` in tool results
   as live working-tree evidence. Declare undeclared files and re-read contended
   files before writing again. `footprint` is hird's own answer to whether the
   task has changed anything — relay it as it stands, and note that its absence
   means hird was not watching rather than that nothing moved.
4. Use `mem_search` before repeating investigation. Use `mem_store` for durable,
   factual findings that will help another session; link them to `task_seq`.

## Finish or hand work back

- Call `task_complete` with a useful result summary after the requested work is
  implemented and verified.
- If the task is a review (it is recused from the work it judges),
  `task_complete` also requires a `verdict`: `"upheld"` if the work stands, or
  `"sent_back"` to reopen the judged work with your findings appended to its
  brief. When sending back, write the result as instructions for whoever
  redoes the work — they will not see your session. Do not fix reviewed work
  inside the review.
- Call `task_fail` only when the task itself failed.
- Call `task_release` when you cannot continue but the task remains valid.
- If one task is really several independently workable jobs, call `task_split`
  with self-contained bodies and accurate file scopes.

Always quote task numbers back to the user. Do not mark work complete merely
because a lease or session is ending.
