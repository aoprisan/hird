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
- Read the claimed task, its `recalled` facts, and its `built_on` entries
  before starting. `built_on` is what the tasks this one depends on said when
  they finished — the context the dependency order exists to hand you. An
  entry marked `provisional` is done but still under review; its work could be
  sent back and reopened while you build on it.
- If the claim carries `previously`, the task has been held before, and
  whatever that holder left uncommitted is in your working tree looking like
  code that was always there. Re-read the files it names before building on
  or over them; `hird diff <seq> --tenure <n>` (named in the sentence) shows
  exactly what that round changed.
- If the claim carries `questions`, read every answer before continuing. An
  earlier holder stopped specifically because guessing would have been wrong;
  the recorded answer is part of the task brief.

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
4. If a `task_update` reply carries `ground_shifted`, a task this one builds on
   has stopped being done — usually sent back by a review. Stop, re-read that
   task (its brief now carries the findings), and adjust before building
   further on its work. Tell the user.
5. Use `mem_search` before repeating investigation. Use `mem_store` for durable,
   factual findings that will help another session; link them to `task_seq`.

## Summoned by the queue

- If a prompt tells you a hird task is ready or claimable (it may name
  `HIRD_EVENT` values like `unblocked`, `review_filed` or `lease_expired`, and
  usually comes from a configured `dispatch_hook`), treat it as "pick up that
  task": call `task_claim` for the number it names, and fall back to
  `task_next` if someone else got there first.
- A claim refused for recusal ("a different harness") is not an error to
  retry: the task is a review of your own harness's work. Call `task_next`
  for something else, and mention the review needs other hands if no other
  agent is around to take it.
- A claim refused for missing capabilities is also not a race to retry. The
  task needs a harness configured with the labels the refusal names. Call
  `task_next` for compatible work and tell the human what equipment is missing
  if the queue has nothing else.

## More hands, inside herdr

Only when the user asks for parallel work on the queue — never on your own
initiative — and `test "${HERDR_ENV:-}" = 1` passes, you are inside
[herdr](https://herdr.dev) and can put more agents on the board:

1. Check the queue first: `task_list`. Spawn nothing when nothing is workable.
2. Follow the herdr skill (or `herdr --help`) to split a pane and start an
   agent in it, preferring a harness different from your own — reviews are
   refused to the harness that did the work, so a mixed pair keeps the review
   loop moving.
3. Prompt it with exactly what you were told, e.g.
   `herdr agent prompt worker1 "work the hird queue"`.
4. Do not wait on it. It claims its own tasks; the queue keeps you out of each
   other's files.

Without herdr (or without permission), say what is workable and let the user
dispatch instead.

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
- Call `task_release` when you cannot continue but the task remains valid. If
  no agent can continue until the human decides or supplies something, include
  one precise `question`; hird will keep the task out of dispatch until the
  human answers it. Do not use a question for work another agent could do —
  release normally or split that work instead.
- If one task is really several independently workable jobs, call `task_split`
  with self-contained bodies, accurate file scopes, and `requires` labels on
  pieces that need capabilities not every harness has.

Always quote task numbers back to the user. Do not mark work complete merely
because a lease or session is ending.
