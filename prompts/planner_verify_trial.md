- Each task needs a `verify` command: the exact argv the controller will run to decide
  whether that task's work is acceptable. It must be scoped to that task, because while
  other tasks are unfinished a whole-project check necessarily fails. It must fail now and
  pass once the task is done — a check that already passes proves nothing.
