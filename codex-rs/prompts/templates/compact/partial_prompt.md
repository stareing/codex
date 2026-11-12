You are writing a CONTEXT CHECKPOINT for the RECENT part of the conversation. The earlier part — everything before the user message where the recent part starts — is kept exactly as it is and does not need to be summarized. The recent part (that user message and everything after it) is about to be replaced by your summary: another model will continue the work from the kept earlier part followed by your summary. Nothing from the recent part survives except what you write here — not the user's messages, your replies, tool calls and their output, or files you read.

Answer with text only; do not call tools. A tool call fails this task.

First think in an <analysis> block: walk through the recent part in order and note, for each part, what the user asked for, what you did, the decisions and why, exact details (paths, names, commands, values, short code that matters), errors and how they were resolved, and anything the user corrected or asked to be done differently. Use the earlier part only to understand the recent one; do not repeat it. Then check that the summary below leaves out nothing from the recent part the next model would need. The analysis is discarded; only the summary is kept.

Then write the summary in a <summary> block with these sections (leave a section out only when it has nothing):

1. Goal: what the user asked for in the recent part, and what they asked for most recently.
2. User messages: every message the user wrote in the recent part (not tool output), in order, quoted or closely paraphrased — they are not kept anywhere else.
3. User preferences and corrections: constraints, rules and feedback from the recent part to keep following. Quote short phrases exactly.
4. Key facts: technical concepts, decisions taken and their reasons, versions, configuration, things checked and found true or false in the recent part.
5. Files and code: each file read, changed or created in the recent part — why it matters, what changed, and any short snippet the next model would otherwise have to re-read to continue.
6. Errors and fixes: what failed, the cause, the fix, and anything still unresolved.
7. Done so far: work in the recent part that is finished and verified (say how it was verified).
8. Still to do: tasks from the recent part that are not finished, in order.
9. Where it stopped: precisely what was being worked on right before this checkpoint — the file, the command, the state it was left in.
10. Next step: the single next action, and only if it follows from the user's latest request; quote the user's words that ask for it. If the last task was finished, say so instead of inventing new work.

Be precise and complete rather than short: concrete names and values beat general descriptions. Write in the language the user writes in. Summarize the RECENT part only.
