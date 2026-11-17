`auth login` keeps one token for a provider in your login file, a file of your own outside every
store. It opens no store: `--repo` and `--as` are read and not used, and the envelope has no
`meta.repo`. It has no MCP tool: a tool call's arguments would record the token.

In this release it keeps the token and does nothing else with it: `clone`, `fetch`, `push` and
`sync` do not read the login file, and take a token from git's credential helpers or from the
variable `--token-env` names, as before.

### Giving the token

`--with-token` reads the token from stdin, so that it is in no argument, where a process list
and a shell's history would show it:

```sh
swarmops auth login --gh --with-token < /home/me/token.txt
```

The white space around the token is dropped. What is left must be 1 to 1024 visible ASCII
characters; anything else is `E_INVALID_ARG` (`details.reason` `token`), and the message never
quotes what stdin held. The token is not checked with the provider: a token that is wrong or
has expired is kept all the same.

### The login file

The file is the one `SWARMOPS_AUTH_FILE` names, by an absolute path. Without the variable it is
`swarmops/auth.toml` in your state directory: `$XDG_STATE_HOME`, else `~/.local/state`; on macOS
`~/Library/Application Support`. On a host that names none of them a login is refused
(`details.reason` `auth_dir`). The result's `file` is the path used.

```toml
# SwarmOps logins. The tokens are in plain text: do not copy, share or commit this file.
version = 1

[[login]]
host = "github.com"
provider = "gh"
source = "with_token"
token = "<the token>"
```

- **Plain text.** The token is not encrypted and no keyring is used: any process that runs as
  you can read it. The file is made with mode 0600 in a directory made with mode 0700.
- **One login per host.** `--gh` is `github.com`. A second login replaces the first, and the
  result says so (`replaced`).
- **What this build does not know is kept.** A rewrite keeps every key, and every login of
  another provider, as it read them. Comments are not kept.
- **No lock.** Of two logins at the same moment, the later one's file stands.

### A file that is refused

`auth login`, [`auth status`](../../commands/auth-status.md) and
[`auth logout`](../../commands/auth-logout.md) refuse a login file they cannot trust or read,
with `E_INVALID_ARG`, the file in `details.auth_file`, and never overwrite it:

| `details.reason` | The file |
| --- | --- |
| `auth_file_kind` | is a symbolic link, or not a regular file |
| `auth_file_mode` | can be read or written by its group or by others; the hint is the `chmod 600` that fixes it |
| `auth_file_version` | names no `version`, or one this build does not read |
| `auth_file` | is larger than 64 KiB, is not TOML, or holds a login that is not one |
| `provider_mismatch` | holds a login for the host with another provider, which this build leaves alone |

A file that cannot be read or written at all is `E_IO`.
