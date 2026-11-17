`auth git-credential` is the command git runs, not one a person types: it answers git's
[credential-helper protocol](https://git-scm.com/docs/gitcredentials) from your login file, so
that a plain `git` can use the token [`auth login`](../../commands/auth-login.md) kept. SwarmOps's
own commands do not need it: `clone`, `fetch`, `push` and `sync` send the login's token
themselves ([Where the login is used](../../commands/auth-login.md#where-the-login-is-used)).

It opens no store: `--repo` and `--as` are read and not used. It has no MCP tool: it speaks git's
protocol on stdin and stdout, and prints a token.

### Telling git to use it

For one command:

```sh
git -c credential.helper='!so auth git-credential' push
```

For every repository of yours:

```sh
git config --global credential.helper '!so auth git-credential'
```

git runs the helper when a remote asks for a credential, with the operation as its last word,
and asks its other helpers when this one has no answer. Name the binary by an absolute path where
`so` is not on the `PATH` git runs with.

### What it answers

git writes its request on stdin, `key=value` lines up to a blank line. At most 64 KiB are read.

| Operation | What happens |
| --- | --- |
| `get` | For a request a login is for, two lines on stdout: `username=x-access-token` and `password=` with the login's token. For any other request, nothing. |
| `store` | Nothing: the helper keeps only what `auth login` kept. |
| `erase` | Nothing: a remote that refuses the token does not remove the login. |
| any other word | Nothing. |

A request is one a login is for when both hold:

1. **The protocol** is `https`, or `http` with a host that is a literal loopback address.
2. **The host** is equal to a login's, port included: a login for `github.com` answers a request
   for `github.com` and for `github.com:443`, and none for `gist.github.com` or `github.com:8443`.
   Only a login of a provider this build knows is given.

The request's other keys (`path`, `username`, the ones newer gits add) are passed over. The
answer never says `quit`, so git stays free to ask its next helper. In every case without an
answer the exit code is 0 and both streams are empty.

```text
protocol=https
host=github.com

```

is answered

```text
username=x-access-token
password=<the token>
```

### Streams and failures

stdout belongs to git, so the command prints no envelope there. This is the only command that
prints a token, and it prints it on stdout alone.

A failure leaves stdout empty and puts its envelope on stderr, with the envelope's exit code;
git then goes on as if the helper had no answer, and shows what the helper wrote:

| Failure | Envelope |
| --- | --- |
| A login file that is refused ([`auth login`](../../commands/auth-login.md#a-file-that-is-refused)) | `E_INVALID_ARG` with `details.auth_file` and the file's `details.reason` |
| A request longer than 64 KiB, or one that cannot be read | `E_INVALID_ARG`, `details.reason` `credential_request` |
| No operation, or a flag it does not know | `E_INVALID_ARG`, the usage error |

The login file is opened only for a `get` whose protocol and host could be a login's: `store`,
`erase` and a request for ssh never fail on the file. No envelope and no message holds the token
or what stdin held.
