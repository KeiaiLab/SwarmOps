It reads the tree of `rev` (default `main`) from the store the server was started on: there is no
directory form here, which `swarmops check list --dir` serves on the command line.
An answer from the store says `vouched: false` and names the tree it read; with `stage`, it lists
only the checks of that stage and echoes it. Each check's declared JUnit and SARIF reports come in
its `reports`.
Nothing runs the checks it lists.
