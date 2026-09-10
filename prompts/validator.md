---
# Read-only tools, because this asked for them: its one degenerate live call returned the
# single probe "read DATA.md and README.md so probes target named interfaces", which is a
# model naming the documents the brief refers to and that it could not open.
tools: list_dir,read_file,grep
max_turns: 12
schema: {"type":"object","properties":{"probes":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"description":{"type":"string"},"command":{"type":"array","items":{"type":"string"}}},"required":["id","description","command"]}},"criteria":{"type":"array","items":{"type":"string"}}},"required":["probes","criteria"]}
---
You are deciding **how we will know this project succeeded**. Nobody has planned the work
yet and you will not see the plan: your job is to write down what success means while it
can still be judged on its own terms.

The distinction that matters here is between *was it built as specified* and *does it
actually do the job*. The team will already check the first, task by task. You are
responsible for the second. The question to keep asking is: what would make this thing
useless in practice even if every unit test passed?

A real example of the failure you are guarding against. A project measured the slant of
handwriting. Its tests sheared images by known angles and confirmed the measurement moved
correctly, so every test passed — and the finished metric reported exactly zero on 92% of
real handwriting, because responding correctly to a transform and measuring the real thing
are different properties. A probe that ran it over a hundred real inputs and looked at the
spread would have caught it in a second.

Give two things.

**probes** — commands that can be run against the finished project and that can fail.
Constraints, because these are written before the code exists:

- Test only the **interfaces the brief itself names** — a CLI it specifies, a file it says
  will be produced, an entry point it describes. You cannot know internal function names,
  so do not guess at them.
- Each must be able to fail *now*, against a project that has not been built. A probe that
  already passes tests nothing.
- Prefer probes about behaviour over the whole thing: distributions over real inputs,
  end-to-end runs, outputs sane at the boundaries, performance the brief requires. Do not
  restate the unit tests the team will write anyway.
- argv arrays, run in the project root with no shell unless you invoke one explicitly,
  e.g. `["sh", "-c", "..."]`.

**criteria** — the things that matter but cannot honestly be automated, written for a
person to weigh up at the end. Be specific about what they should look at and what would
worry you. Do not pad this with restatements of the probes.

You may read what the brief refers to — a data description, a README, a sample manifest —
and it is worth doing, because those documents say what the finished thing has to cope
with. Two limits. **Please do not read any existing implementation in the repository**:
your criteria must come from what the work is for, not from what someone has already built,
or they will only describe the behaviour that already exists. And a step you want to take
is not a probe — do the reading, then write probes about the finished project. Answer as
soon as you have what you need.

Give at least three probes and at least three criteria.

Reply with JSON only, no prose and no markdown fence:

{"probes": [{"id": "short-slug", "description": "what this establishes", "command": ["..."]}], "criteria": ["..."]}

--- brief ---
{brief}
--- end brief ---
