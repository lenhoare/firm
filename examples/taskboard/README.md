# Taskboard trial

A small dependency-free Rust library for turning a line-oriented task board into a deterministic status report. It is deliberately unfinished for Firm's second live multi-provider trial.

Input contains one task per line:

```text
# comments and blank lines are ignored
todo | alice | 30 | Write parser
doing | bob | 20 | Add report
done | alice | 10 | Define types
```

Fields are `status | owner | minutes | title`. Surrounding whitespace is ignored. Status is exactly `todo`, `doing`, or `done`; owner and title must be non-empty; minutes must be a positive `u32`. Parsing collects all invalid lines as 1-indexed issues instead of stopping at the first one.

The public API is defined in `src/lib.rs`. The report format and selection rules are pinned by tests. Keep the crate dependency-free and deterministic.
