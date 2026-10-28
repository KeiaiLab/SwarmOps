# Serve over HTTP

Let people and CI reach a store with stock git over HTTPS, through `swarmops serve --cgi` behind a
web server. Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages). The two web
server configurations below are examples to adapt, not copied from a run.

## Before you start

- Everything [Serve over ssh](serve-over-ssh.md#before-you-start) needs on the server, and its
  [serving account](serve-over-ssh.md#the-serving-account) with its state directory: here the web
  server runs `serve` as that account.
- The store's [`access.toml`](serve-over-ssh.md#write-accesstoml): the same file decides both
  entries.
- A web server that terminates TLS, authenticates the caller and runs CGI programs: nginx with
  fcgiwrap, or Apache.

## How it works

```text
git (client) ──HTTPS──▶ web server: TLS, authentication (REMOTE_USER)
                          │ CGI, PATH_INFO = /acme.git/info/refs
                          ▼
                        swarmops serve --cgi --root /srv/so --user-family person
                          │ 1 the caller: person/$REMOTE_USER, or --as
                          │ 2 the store: PATH_INFO's first segment
                          │ 3 access.toml: may the caller read?
                          │ 4 git/serve.git refreshed from the store
                          ▼
                        git http-backend on serve.git, then the same hook as over ssh
```

The web server runs `serve` once per request. `serve` answers git's smart HTTP alone:
`<name>.git/info/refs`, `<name>.git/git-upload-pack` and `<name>.git/git-receive-pack`. A push is
decided ref by ref by the same hook, rules and refusals as
[over ssh](serve-over-ssh.md#refusals-a-client-sees).

## The CGI entry

`serve` takes its options on its command line, and a CGI program runs without arguments, so the
web server runs a two-line script, here `/usr/local/libexec/swarmops-serve.cgi`, made executable:

```sh
#!/bin/sh
exec swarmops serve --cgi --root /srv/so --user-family person
```

- `--user-family person` names the caller `person/$REMOTE_USER`, the user the web server
  authenticated: give each person a web login named as their identity's instance (`ada` for
  `person/ada`). A request without `REMOTE_USER` is refused with `E_AUTH`.
- `--as <family>/<instance>` instead names one caller for every request of the entry, whatever the
  web server authenticated: a read-only entry with `--as web/anonymous` and a grant of `read` alone,
  say.
- `serve` sets `http.receivepack=true` for git, so pushes reach the hook even without
  `REMOTE_USER`: the access file alone decides them.
- `serve` reads the memo key's directory from `SWARMOPS_MEMO_KEY_DIR`, `XDG_STATE_HOME` or `HOME`.
  A web server may run CGI programs with none of them: set `HOME` or `SWARMOPS_MEMO_KEY_DIR` in the
  script then.

## nginx and fcgiwrap

Run an fcgiwrap instance as the serving account, listening on its own socket, and pass git's three
smart HTTP paths under `/so/` to it:

```nginx
location ~ ^/so(/[^/]+/(?:info/refs|git-upload-pack|git-receive-pack))$ {
    auth_basic "SwarmOps";
    auth_basic_user_file /etc/nginx/swarmops.htpasswd;
    client_max_body_size 0;                 # serve bounds a push at 2 GiB
    include fastcgi_params;
    fastcgi_param SCRIPT_FILENAME /usr/local/libexec/swarmops-serve.cgi;
    fastcgi_param PATH_INFO $1;
    fastcgi_param REMOTE_USER $remote_user;
    fastcgi_pass unix:/run/fcgiwrap-swarmops.socket;
}
```

nginx passes the request's headers on, `Git-Protocol` among them, so protocol v2 needs nothing
more.

## Apache

`ScriptAlias` maps `/so/` to the script, and the rest of the path becomes `PATH_INFO`. Apache runs
CGI programs as its own user: run this one as the serving account through suEXEC
(`SuexecUserGroup`, the script where suEXEC accepts it, `suexec -V` names the directory), or give
the stores to Apache's user.

```apache
ScriptAlias /so/ /usr/local/libexec/swarmops-serve.cgi/
<Location /so/>
    AuthType Basic
    AuthName "SwarmOps"
    AuthUserFile /etc/apache2/swarmops.htpasswd
    Require valid-user
</Location>
```

Apache passes the request's headers on as well, `Git-Protocol` among them.

## What a client types

The URL is the web server's prefix and the store's name:

```sh
git clone https://so-host/so/acme.git
git pull --rebase
git push origin HEAD:refs/land/main           # land: squashed onto main
git push origin feature/x                     # a branch of its own (FORMAT 2)
```

git asks for the web login when the web server answers 401, or takes it from a credential helper.
Landing, conflicts and the rest read as [over ssh](serve-over-ssh.md#what-a-client-types).

## Refusals a client sees

A refusal before git starts is an HTTP status with the text `<CODE>: <message>`, which git shows
after the URL; the envelope goes to the web server's error log. A push refused for one ref reads as
over ssh.

```text
Status: 403
Content-Type: text/plain

E_AUTH: serve --user-family names no caller: the web server sets no REMOTE_USER
```

| Status | Codes | Typical cause |
| --- | --- | --- |
| 403 | [`E_AUTH`](../reference/errors.md#e_auth), [`E_POLICY_DENIED`](../reference/errors.md#e_policy_denied) | No `REMOTE_USER`, or one that is no instance name; no grant names the caller; no or an invalid `access.toml`; no read granted |
| 404 | [`E_NOT_A_REPO`](../reference/errors.md#e_not_a_repo) | A path that is none of git's smart HTTP requests, or no such store |
| 400 | [`E_INVALID_ARG`](../reference/errors.md#e_invalid_arg) | The first segment names no store (`bad_path`) |
| 503 | A retryable code, such as [`E_BUSY`](../reference/errors.md#e_busy) | Another request's refresh held `git/serve.lock` for 10 s: retry |
| 500 | Any other | Read the envelope in the web server's error log |

`serve` answers 403, never 401: the web server asks for credentials, `serve` decides what the
authenticated caller may do. The table of [refusals over ssh](serve-over-ssh.md#refusals-a-client-sees)
says what to do about each.

## Limits and what is not served

As [over ssh](serve-over-ssh.md#limits): a push of at most 2 GiB and 1,000 refs. The web server's
own body limit must not be smaller (`client_max_body_size 0` above).
[Not served](serve-over-ssh.md#not-served) applies here too, and so does git's dumb HTTP protocol:
`serve` answers the smart one alone.
