You are writing a CONTEXT CHECKPOINT. The conversation so far is about to be replaced by your summary: another model will continue the work from it, together with the user's own messages (those are kept word for word, so do not copy them out). Everything else — your replies, tool calls and their output, files you read — will be gone unless you write it down here.

Answer with text only; do not call tools.

First think in an <analysis> block: walk through the conversation in order and note, for each part, what the user asked for, what you did, the decisions and why, exact details (paths, names, commands, values, short code that matters), errors and how they were resolved, and anything the user corrected or asked to be done differently. Then check that the summary below leaves out nothing the next model would need. The analysis is discarded; only the summary is kept.

Then write the summary in a <summary> block with these sections (leave a section out only when it has nothing):

1. Goal: what the user wants overall, and what they asked for most recently.
2. User preferences and corrections: constraints, rules and feedback to keep following (e.g. "run only the related tests", "never commit X"). Quote short phrases exactly.
3. Key facts: technical concepts, decisions taken and their reasons, versions, configuration, things checked and found true or false.
4. Files and code: each file read, changed or created — why it matters, what changed, and any short snippet the next model would otherwise have to re-read to continue.
5. Errors and fixes: what failed, the cause, the fix, and anything still unresolved.
6. Done so far: work that is finished and verified (say how it was verified).
7. Still to do: tasks the user asked for that are not finished, in order.
8. Where it stopped: precisely what was being worked on right before this checkpoint — the file, the command, the state it was left in.
9. Next step: the single next action, and only if it follows from the user's latest request; quote the user's words that ask for it. If the last task was finished, say so instead of inventing new work.

Be precise and complete rather than short: concrete names and values beat general descriptions. Write in the language the user writes in.
