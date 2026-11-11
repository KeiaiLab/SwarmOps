#!/bin/sh
# The container's first process: makes the run's keys, certificate and logins, starts sshd,
# fcgiwrap and nginx, then says ready by /run/entrance-ready. Everything is made here, offline,
# so the image holds no key. ENTRANCE_PLANT=auth_off plants the self-check's misconfiguration:
# nginx without a login, sshd accepting any key.
set -eu

conf=/etc/swarmops-entrance
dev=/home/dev
mkdir -p "$conf" /run/sshd

# 1. The host's ssh keys and a TLS certificate for so-host, its own authority.
ssh-keygen -A >/dev/null
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 2 \
    -subj /CN=so-host -addext subjectAltName=DNS:so-host \
    -keyout "$conf/tls.key" -out "$conf/tls.pem" 2>/dev/null
chmod 0640 "$conf/tls.key"
chgrp www-data "$conf/tls.key"
echo '127.0.0.1 so-host' >>/etc/hosts

# 2. Web logins: ada writes, bob reads (the access file says so); eve has none.
: >"$conf/htpasswd"
for login in ada bob; do
    printf '%s:%s\n' "$login" "$(openssl passwd -apr1 "pw-$login")" >>"$conf/htpasswd"
done
chgrp www-data "$conf/htpasswd"
chmod 0640 "$conf/htpasswd"

# 3. The client's keys: ada's and bob's lines name their identities; eve's key is on no line.
install -d -o dev -g dev -m 0700 "$dev/.ssh"
install -d -o so -g so -m 0700 /home/so/.ssh
: >/home/so/.ssh/authorized_keys
for who in ada bob eve; do
    runuser -u dev -- ssh-keygen -q -t ed25519 -N '' -C "$who" -f "$dev/.ssh/$who"
    printf 'Host so-%s\n  HostName 127.0.0.1\n  HostKeyAlias so-host\n  User so\n  IdentityFile %s\n  IdentitiesOnly yes\n  BatchMode yes\n  StrictHostKeyChecking yes\n\n' \
        "$who" "$dev/.ssh/$who" >>"$dev/.ssh/config"
done
for who in ada bob; do
    printf 'command="swarmops serve --root /srv/so --as person/%s",restrict %s\n' \
        "$who" "$(cat "$dev/.ssh/$who.pub")" >>/home/so/.ssh/authorized_keys
done
chown so:so /home/so/.ssh/authorized_keys
chmod 0600 /home/so/.ssh/authorized_keys
for key in /etc/ssh/ssh_host_*_key.pub; do
    printf 'so-host %s\n' "$(cut -d' ' -f1,2 "$key")" >>"$dev/.ssh/known_hosts"
done
chown dev:dev "$dev/.ssh/config" "$dev/.ssh/known_hosts"

# 4. The client's git: the certificate trusted, ada's and bob's web logins stored, no prompt.
printf 'https://ada:pw-ada@so-host\nhttps://bob:pw-bob@so-host\n' >"$dev/.git-credentials"
cat >"$dev/.gitconfig" <<EOF
[user]
	name = Dev
	email = dev@so-host.invalid
[http]
	sslCAInfo = $conf/tls.pem
[credential]
	helper = store
[init]
	defaultBranch = main
EOF
chown dev:dev "$dev/.git-credentials" "$dev/.gitconfig"
chmod 0600 "$dev/.git-credentials"

# 5. The self-check's plant: authentication off at both entrances.
if [ "${ENTRANCE_PLANT:-}" = auth_off ]; then
    sed -i 's/auth_basic "SwarmOps";/auth_basic off;/' /etc/nginx/sites-enabled/swarmops.conf
    printf 'AuthorizedKeysCommand /usr/local/sbin/plant-any-key %%t %%k\nAuthorizedKeysCommandUser nobody\n' \
        >>/etc/ssh/sshd_config.d/swarmops.conf
fi

# 6. The entrances: sshd, fcgiwrap as the serving account (its socket for so's group, which
#    nginx's workers are in), nginx.
/usr/sbin/sshd
runuser -u so -- sh -c 'umask 007; exec fcgiwrap -s unix:/run/swarmops/fcgiwrap.socket' &
while [ ! -S /run/swarmops/fcgiwrap.socket ]; do sleep 0.1; done
nginx

touch /run/entrance-ready
exec sleep infinity
