#!/bin/sh
# The CGI entry: a web login names person/<login>. fcgiwrap passes neither PATH nor HOME; serve
# keeps its memo keys under the serving account's home.
PATH=/usr/local/bin:/usr/bin:/bin
HOME=/home/so
export PATH HOME
exec swarmops serve --cgi --root /srv/so --user-family person
