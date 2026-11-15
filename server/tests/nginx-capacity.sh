#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
test_dir=$(mktemp -d)
cleanup() {
    if [[ -f "$test_dir/nginx.pid" ]]; then
        nginx -p "$test_dir/" -c "$test_dir/nginx.conf" -s stop
    fi
    rm -rf -- "$test_dir"
}
trap cleanup EXIT

hostname=duolink.154.86.119.7.sslip.io
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=$hostname" \
    -addext "subjectAltName=DNS:$hostname" \
    -keyout "$test_dir/key.pem" -out "$test_dir/cert.pem" 2>"$test_dir/openssl.log"

sed -e 's/listen 80;/listen 127.0.0.1:18080;/' \
    -e '/listen \[::\]:80;/d' \
    -e "s@/etc/letsencrypt/live/$hostname/fullchain.pem@$test_dir/cert.pem@" \
    -e "s@/etc/letsencrypt/live/$hostname/privkey.pem@$test_dir/key.pem@" \
    -e "s@/var/log/nginx/duolink-error.log@$test_dir/duolink-error.log@" \
    "$root/server/deploy/nginx.conf" >"$test_dir/duolink.conf"

cat >"$test_dir/nginx.conf" <<EOF
load_module /usr/lib/nginx/modules/ngx_stream_module.so;
pid $test_dir/nginx.pid;
error_log stderr info;
worker_processes 1;
events { worker_connections 2048; }
stream {
    server {
        listen 127.0.0.1:19443;
        proxy_pass 127.0.0.1:9464;
    }
}
http { include $test_dir/duolink.conf; }
EOF

nginx -p "$test_dir/" -c "$test_dir/nginx.conf" -t
nginx -p "$test_dir/" -c "$test_dir/nginx.conf"
DUOLINK_CAPACITY_CA="$test_dir/cert.pem" node "$root/server/tests/capacity.mjs" "$1"
