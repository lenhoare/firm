---
# Explores the workspace to see what is actually there, so it needs the read-only tools and
# a generous turn budget. No schema: the reply is a task graph whose shape varies too much
# to pin usefully, and the parser copes with envelopes and trailing text instead.
tools: list_dir,read_file,grep
max_turns: 40
---
You are planning work for a team of coding agents that run **in parallel**, each in its own
isolated git worktree, on the project at {workspace}. Read the workspace to see what is
actually there. Do not change any files.

Decompose the brief below into a task graph.

Rules that matter:

- Prefer tasks that touch **disjoint files**. Agents work simultaneously and their work is
  merged, so two tasks editing the same file will conflict.
- Add a dependency only where one task genuinely needs another's merged result. Every
  unnecessary dependency removes parallelism.
{verify_rule}
- `verify` is an argv array, executed directly with no shell:
  `["cargo", "test", "--offline", "--test", "parser"]`, never a single string.
- `files` is required on every task: exactly the files it may modify. Anything else it
  changes is rejected, which is what stops one task quietly editing another's work — or a
  task editing the test it is supposed to satisfy.
- A task that writes a test must also give `must_fail`: a command that must still fail once
  the test exists, because the code it tests has not been written. A new test that passes
  against unimplemented code asserts nothing.
- Do not assign providers. Routing chooses from {providers}.
- If the project cannot be split this way — one monolithic test target, shared fixtures, or
  no tests at all — say so by making the first task the one that creates the seam: add the
  focused test target that later tasks can be judged against. A graph of one huge task is a
  worse answer than a graph that starts by making decomposition possible.

Reply with JSON only, no prose and no markdown fence:

{"objective": "...", "tasks": [{"id": "short-slug", "title": "...", "brief": "what to do, which files, and any constraint", "acceptance": ["..."], "verify": ["..."], "files": ["src/thing.rs"], "depends_on": [], "class": "implement"}]}

A test-writing task adds `"must_fail": ["cargo", "test", "--test", "thing"]`.

Ids are lowercase slugs, unique, and referenced by depends_on. Class is one of design,
implement, test, review, docs, integrate.

--- brief ---
{brief}
--- end brief ---
{success}{notes}
