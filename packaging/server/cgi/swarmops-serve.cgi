#!/bin/sh
# The CGI entry the nginx site runs through fcgiwrap as the serving account: the caller is
# person/<web login>. HOME keeps the account's memo keys.
HOME=/var/lib/swarmops-server
export HOME
exec /usr/bin/swarmops serve --cgi --root /srv/so --user-family person
