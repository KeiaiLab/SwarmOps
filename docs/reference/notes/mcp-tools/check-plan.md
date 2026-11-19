It plans the changeset `rev` against `target` (default `main`) on the store the server was
started on, and with `landed` the change as `land_check` would land it. There is no directory
form here, which `swarmops check plan --dir` serves on the command line, and no workspace form:
for a workspace's edits, call `ws_snapshot` first and plan the changeset it returns.
A plan informs: `check_run` runs every check of its stage, whatever a plan says. Take a verdict
you do not know as `run`.
