#!/bin/sh
set -eu
case " ${RENEWED_DOMAINS:-} " in
    *" duolink.154.86.119.7.sslip.io "*)
        /usr/sbin/nginx -t
        /usr/bin/systemctl reload nginx
        ;;
esac
