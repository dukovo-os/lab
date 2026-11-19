#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Installs the programs the lab PC runs for the tests' other side (an NFS
# server and client, a KDC, a mail catcher, WireGuard and so on) on a
# Debian, Ubuntu or Mint lab PC. Run it once as root:
#
#   sudo sh scripts/lab-pc-tools.sh
#
# The tests start each server themselves, on ports of their own, and stop
# it at the end, so the system services these packages bring are turned
# off here: nothing new listens on the home network. chrony stays on: it
# takes over from systemd-timesyncd keeping this PC's clock.
set -eu

if [ "$(id -u)" -ne 0 ]; then
	echo "run it as root: sudo sh $0" >&2
	exit 1
fi

export DEBIAN_FRONTEND=noninteractive
# A broken third-party list or mirror must not stop the install: what
# these packages need comes from the distribution's own lists.
apt-get update || echo "apt-get update had errors; installing from the lists it has" >&2
apt-get install -y --no-install-recommends \
	nfs-common nfs-ganesha nfs-ganesha-vfs \
	restic prometheus chrony \
	krb5-kdc krb5-admin-server \
	wireguard-tools \
	libavif16 fonts-ipafont-gothic xfonts-cyrillic

for unit in nfs-ganesha prometheus krb5-kdc krb5-admin-server \
	rpcbind.socket rpcbind; do
	systemctl disable --now "$unit" 2>/dev/null || true
done

# Mailpit is not packaged: its release, checked against the digest
# GitHub records for it.
version=v1.33.0
digest=d0ec50080cbf3e7c1fa2cda2ee68125d7894175811084b1f8f47c6cfc6f487c8
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -fsSL -o "$work/mailpit.tar.gz" \
	"https://github.com/axllent/mailpit/releases/download/$version/mailpit-linux-amd64.tar.gz"
echo "$digest  $work/mailpit.tar.gz" | sha256sum -c -
tar -xzf "$work/mailpit.tar.gz" -C "$work" mailpit
install -m 755 "$work/mailpit" /usr/local/bin/mailpit

echo "lab PC tools installed"
