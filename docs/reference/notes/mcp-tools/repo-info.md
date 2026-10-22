It takes no arguments: pass `{}`.
Call it first to check that the server's store is there: a store that is not there fails every
call with `E_NOT_A_REPO`, and the server serves on.
The store is opened anew for each call, so the counts include what other processes committed.
