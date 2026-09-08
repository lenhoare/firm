# Multi-Agent Planning: Forum + Blackboard + Ensemble DAGs

## Core Architecture

Separate the agents' shared communication into two distinct systems:

### Forum --- shared cognition

The forum is an informal exchange for discoveries, warnings, ideas,
constraints, and coordination.

Examples:

-   `rustc` is unavailable in the environment.
-   An API behaves differently from expected.
-   Another agent is editing a particular file.
-   A simpler implementation approach has been discovered.
-   An assumption made during planning appears to be wrong.

The forum should remain conversational and relatively unconstrained. Its
purpose is **shared situational awareness**, not authoritative task
state.

### Blackboard --- shared execution state

The blackboard contains the authoritative **task DAG** and its current
state. It should be structured and machine-readable rather than
conversational.

A task node might contain:

``` yaml
id: T17
goal: Make manager model selectable
depends_on: [T12]
owner: qwen
status: ready
inputs:
  - config schema
outputs:
  - implementation patch
acceptance:
  - manager model can differ from worker model
  - existing configs remain valid
parallel_safe: true
```

Useful relationships and operations include:

-   `depends_on`
-   `blocked_by`
-   `spawned_from`
-   `supersedes`
-   `add`
-   `split`
-   `merge`
-   `cancel`
-   `reorder`

Agents can propose mutations to the DAG; the manager/scheduler decides
which become authoritative.

The conceptual distinction is:

> **Forum = shared cognition. Blackboard = shared execution state.**

An important bridge between them is **forum insight → proposed DAG
mutation**. For example, discovering on the forum that `rustc` is
unavailable might cause an agent to propose blocking compilation tasks
and adding an environment-diagnosis task.

------------------------------------------------------------------------

## Ensemble Task Decomposition

Rather than asking one manager to create the initial plan, use multiple
agents to independently decompose the user request.

### Stage 1 --- Independent DAGs

Every planning agent receives the original task and independently
constructs a DAG.

Agents should **not see the other DAGs before submitting their own**.
This preserves diversity and avoids anchoring/herding.

The result is a collection of independent hypotheses about how the work
should be decomposed.

### Stage 2 --- Cross-examination

Publish all proposed DAGs to the blackboard.

Each agent then examines the other plans and identifies:

-   missing tasks
-   unnecessary tasks
-   incorrect dependencies
-   opportunities for parallelism
-   tasks that should be split or merged
-   hidden assumptions
-   missing verification steps
-   likely bottlenecks

Agents may either produce a revised/synthesised DAG or, to reduce token
cost, submit structured criticisms and graph mutations.

For example:

``` text
ADD task: backward compatibility check
REMOVE edge: T12 -> T15
MERGE: T21 + T23
SPLIT: T30 -> [schema change, parser change, migration]
```

### Stage 3 --- Candidate Synthesis

One or more agents construct candidate final DAGs using the original
proposals and critiques.

The aim is synthesis rather than voting for one agent's complete plan.

For example, three agents might propose:

``` text
A: investigate config -> modify parser -> tests
B: inspect callers -> modify schema -> migration -> tests
C: modify parser -> compile -> integration test
```

A synthesis could become:

``` text
inspect config + callers
        |
        v
modify schema/parser
        |
        v
migration/backcompat check
        |
        v
compile
        |
        v
integration tests
```

### Stage 4 --- Arbitration

A final arbiter produces the authoritative executable DAG.

The arbiter should work at the **node and edge level**, combining useful
parts of competing plans rather than simply selecting a winning DAG.

Where useful, it can retain provenance:

-   which agents proposed a node
-   which agents proposed a dependency
-   criticisms of a node/edge
-   why an alternative was rejected
-   confidence/agreement

Agreement itself can be useful evidence. A task or dependency
independently proposed by most agents can be marked high-confidence; an
important proposal made by only one agent can be flagged as contentious
rather than silently discarded.

The overall planning pipeline is therefore:

``` text
User Goal
    |
    v
Independent DAGs
    |
    v
Cross-examination
    |
    v
Candidate Syntheses
    |
    v
Arbiter
    |
    v
Authoritative DAG
```

Or conceptually:

> **Independent hypotheses → cross-examination → synthesis → arbitration
> → consensus DAG**

------------------------------------------------------------------------

## Execution and Replanning

The initial DAG should not be treated as immutable.

During execution, agents can discover new information through their work
or through the forum and propose graph mutations:

``` text
PROPOSE_TASK(parent=T17,
             goal="Check backward compatibility",
             reason="Config format changed")

BLOCK(T23,
      reason="Requires result from T19")

MERGE(T31,T34,
      reason="Duplicate investigation")

SPLIT(T41,
      children=[...])
```

The manager/arbiter can accept or reject these proposals.

This creates a cycle:

``` text
PLAN
  |
  v
EXECUTE
  |
  v
OBSERVE
  |
  +---- no significant change ----> continue
  |
  +---- assumption invalidated ----> REPLAN
                                      |
                                      v
                                  updated DAG
```

Replanning should ideally modify only the affected region of the DAG
rather than regenerate the entire plan unnecessarily.

------------------------------------------------------------------------

## Agent Allocation

Once the DAG exists, ready nodes can be scheduled independently.

A useful future extension is a **Contract Net / bidding model**: agents
bid for ready tasks according to capability, confidence, expected cost,
context already held, and availability.

For example:

``` yaml
task: T17
agent: qwen
confidence: 0.87
estimated_tokens: 3500
capability: 0.92
```

This avoids permanently assigning roles such as "Qwen codes" or "Astra
plans". Model capabilities and costs change, so allocation can remain
dynamic.

Large tasks can also be recursively decomposed. A worker receiving a
task that is still too broad can propose child nodes for the global DAG.

------------------------------------------------------------------------

## Using Expensive Models Efficiently

The architecture naturally separates **high-value reasoning** from
routine execution.

A powerful/expensive model can concentrate on:

-   initial decomposition
-   architecture
-   resolving disagreement
-   synthesis
-   arbitration
-   difficult replanning

Cheaper models can handle:

-   bounded implementation tasks
-   searches
-   compilation
-   tests
-   mechanical verification
-   DAG criticism
-   routine execution

This prevents a powerful manager model from spending large amounts of
its budget on tasks such as running existing tests or inspecting routine
screenshots.

------------------------------------------------------------------------

## Design Principle

The interesting property of this architecture is that the multi-agent
system is not merely several models talking to each other.

The **forum lets knowledge propagate**, while the **blackboard converts
that knowledge into coordinated action**.

The DAG becomes the system's explicit, inspectable representation of:

-   what it currently believes needs doing
-   what depends on what
-   what can happen concurrently
-   who is doing it
-   what has been learned
-   where uncertainty remains
-   when the plan needs to change
