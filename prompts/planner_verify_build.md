- A `verify` command is optional here, and worth giving only where a real one exists: a
  check that genuinely fails now and passes once the task is done. Do not invent one to
  fill the field. Where there is none, the project's own tests act as a guard against
  breakage and a reviewer reads the diff, so what matters is that `acceptance` states
  plainly what done looks like — specific enough that someone reading the diff could tell
  whether it was achieved.
