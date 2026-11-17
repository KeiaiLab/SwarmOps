`auth login` keeps one token for a provider in your login file, a file of your own outside every
store. The token is one GitHub grants when you approve a code in a browser
([Asking GitHub for the token](#asking-github-for-the-token)), or one you hand over on stdin
([Giving the token](#giving-the-token)). It opens no store: `--repo` and `--as` are read and not
used, and the envelope has no `meta.repo`. It has no MCP tool: approving a code is a person's
act, and a tool call's arguments would record a token handed over.

The token is then what `clone`, `fetch`, `push`, `sync` and `remote add` send to an https remote
at the login's host that names no `--token-env`
([Where the login is used](#where-the-login-is-used)).

### Asking GitHub for the token

Without `--with-token`, `auth login --gh` runs GitHub's device flow:

```sh
SWARMOPS_GH_CLIENT_ID=your-client-id swarmops auth login --gh
```

```text
Open https://github.com/login/device and enter the code ABCD-1234
Waiting for approval (the code expires in 15 minutes).
```

The two lines go to stderr; stdout carries the envelope alone, once the login is kept. Open the
page on any device, enter the code and approve it. The command asks GitHub again every few
seconds, at the pace GitHub sets, until the code is approved, refused or expired. Stopping it
(Ctrl-C) keeps nothing.

- **The client ID.** GitHub grants the token in the name of an OAuth app, which
  `SWARMOPS_GH_CLIENT_ID` names by its client ID. SwarmOps has registered no app of its own yet,
  so this build has no client ID built in, and without the variable the login is `E_INVALID_ARG`
  (`details.reason` `client_id_unset`). Use the client ID of an OAuth app of yours with the
  device flow turned on in its settings. A client ID is public, and the flow uses no client
  secret.
- **The scopes.** `--scope` names a scope to ask for and repeats; none asks for `repo`, which
  reads and writes repositories. A push that adds or changes a file under `.github/workflows/`
  also needs `workflow`: `--scope repo --scope workflow`. The login records the scopes GitHub
  says the token has (`scopes`).
- **curl.** SwarmOps has no network code of its own: each request is one run of your `curl`,
  the one `SWARMOPS_CURL` names by an absolute path, else the first on `PATH`. curl gets the
  request on its stdin, so that the device code is in no argument, and a cleared environment but
  for `PATH`, the proxy variables (`HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` and their lower-case
  forms) and the certificate variables (`SSL_CERT_FILE`, `SSL_CERT_DIR`, `CURL_CA_BUNDLE`); your
  `.curlrc` is not read. A host without curl is `E_INVALID_ARG` (`details.reason`
  `curl_missing`); `--with-token` needs none.
- **Where the requests go.** To `https://github.com/login/device/code` and
  `https://github.com/login/oauth/access_token`, and nowhere else: no flag, variable or answer
  moves them, and no redirect is followed. The page shown is checked to be at `github.com`.

What is checked before GitHub is asked for anything: the scopes, the client ID, that the login
file can keep the login ([A file that is refused](#a-file-that-is-refused)), and curl. Then:

| The flow ends | Error | `details` |
| --- | --- | --- |
| The code was refused on the page | `E_AUTH` | `reason` `access_denied` |
| The code expired before it was approved | `E_AUTH` | `reason` `expired_token` |
| The app has the device flow turned off | `E_AUTH` | `reason` `device_flow_disabled` |
| GitHub knows no app by the client ID | `E_AUTH` | `reason` `bad_client_id` |
| The token came with a refresh token | `E_AUTH` | `reason` `expiring_token` |
| The answer is none the device flow has | `E_AUTH` | `reason` `unexpected_answer` |
| GitHub did not answer: the first request, or three polls in a row | `E_REMOTE_UNREACHABLE` | `phase` `login`, `host`, `cause`, `curl_exit` |

Each `E_AUTH` (exit 5) has `details.provider` and `details.host`, GitHub's own word for the
refusal when it gave one (`details.provider_error`) and the HTTP status (`details.http_status`).
No error holds GitHub's answer, the device code or a token, and no login is kept.
`E_REMOTE_UNREACHABLE` (exit 4) can be run again; it has no `details.remote`, a login being of
no store's remote.

A token that expires is refused because this build could not renew it: a GitHub App with
expiring user tokens answers so. An OAuth app's tokens do not expire.

### Giving the token

`--with-token` reads the token from stdin, so that it is in no argument, where a process list
and a shell's history would show it. It reaches no network and needs no curl and no client ID,
which suits a host where nobody is there to approve a code:

```sh
swarmops auth login --gh --with-token < /home/me/token.txt
```

The white space around the token is dropped. What is left must be 1 to 1024 visible ASCII
characters; anything else is `E_INVALID_ARG` (`details.reason` `token`), and the message never
quotes what stdin held. The token is not checked with the provider: a token that is wrong or
has expired is kept all the same.

`--with-token` takes no value. A token typed after it, or anywhere else on the line, is a usage
error (`E_INVALID_ARG`, exit 2) that keeps nothing, and its envelope quotes no word of the line:
the message is a fixed one and the hint is the command's usage. A line that lacks `--gh`, or
that has both `--with-token` and `--scope`, is answered in clap's words, which name flags alone.
A token typed as the value of `--scope` is refused as a scope (`details.reason` `scope`), by a
message that states what a scope is and does not quote the value.
The token was still in an argument, where a process list and a shell's history show it: revoke
such a token.

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

`source` is `with_token` or `device`; a login by the device flow also has `scopes`.

- **Plain text.** The token is not encrypted and no keyring is used: any process that runs as
  you can read it. The file is made with mode 0600 in a directory made with mode 0700.
- **One login per host.** `--gh` is `github.com`. A second login replaces the first, and the
  result says so (`replaced`).
- **What this build does not know is kept.** A rewrite keeps every top-level key and table,
  and every login it does not replace, another provider's among them, as it read them. The
  login it replaces is written anew, with this build's keys alone: a key of the old entry that
  this build does not know went with the old token. Comments are not kept.
- **No lock.** Of two logins at the same moment, the later one's file stands.

### Where the login is used

Each command that reaches a git remote ([`clone`](../../commands/clone.md),
[`fetch`](../../commands/fetch.md), [`push`](../../commands/push.md),
[`sync`](../../commands/sync.md), and [`remote add`](../../commands/remote-add.md) when it asks
for the default branch) picks one credential, in this order:

1. **The remote names a variable** (`--token-env`): the variable's token, and nothing else. With
   the variable unset no token is sent, the login is not used in its place, and a refusal says so
   (`details.token_env_unset`).
2. **An https remote whose host is the login's**: the login's token. git gets it as it gets a
   variable's token, as an HTTP header for that remote's URL alone (basic authentication, user
   name `x-access-token`), through its environment and never as an argument.
3. **Anything else**: no token from SwarmOps. git's own credentials apply: a credential helper,
   an ssh key.

A git you run yourself can use the login too, with
[`auth git-credential`](../../commands/auth-git-credential.md) as its credential helper.

The host matches when it is equal, port included: a login for `github.com` goes to
`https://github.com/…` and to `https://github.com:443/…`, and not to `gist.github.com`, to
`github.com:8443` or to a name that ends in `github.com`. An ssh remote never reads the login
file. Only a login of a provider this build knows is sent.

A remote that refuses the login's token ends `E_REMOTE_REJECTED` (`details.reason` `auth`, or
`not_found` for a repository the token cannot see) with `details.login_used` true. The login's
token goes with the first request, before git asks any credential helper, so a token that has
expired can fail a remote that a helper would open: log in again, or log out
([`auth logout`](../../commands/auth-logout.md)) to leave the remote to git's credentials.

A command that sends a token, the login's or a variable's, follows no HTTP redirect: git would
add the token itself to what it sends the address a redirect names. A remote that answers its
first request with a redirect (a repository that was renamed or moved) therefore ends
`E_REMOTE_REJECTED` (`details.reason` `unclassified`, the status in `details.stderr_tail`);
record the new URL with [`remote add`](../../commands/remote-add.md). A command that sends no
token follows a redirect as git does.

A login file that is refused (below) fails these commands before git runs. The MCP server's
`fetch`, `push` and `sync` tools use the login file of the user the server runs as.

### A file that is refused

`auth login`, [`auth status`](../../commands/auth-status.md),
[`auth logout`](../../commands/auth-logout.md),
[`auth git-credential`](../../commands/auth-git-credential.md) and the commands that would send a
login's token refuse a login file they cannot trust or read, with `E_INVALID_ARG`, the file in
`details.auth_file`, and never overwrite it:

| `details.reason` | The file |
| --- | --- |
| `auth_file_kind` | is a symbolic link, or not a regular file, or was replaced by another file each time it was opened |
| `auth_file_mode` | can be read or written by its group or by others; the hint is the `chmod 600` that fixes it |
| `auth_file_version` | names no `version`, or one this build does not read |
| `auth_file` | is larger than 64 KiB, is not TOML, or holds a login that is not one |
| `provider_mismatch` | holds a login for the host with another provider, which this build leaves alone |

A file that cannot be read or written at all is `E_IO`.
