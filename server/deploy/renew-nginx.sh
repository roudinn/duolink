#!/bin/sh
set -eu
case " ${RENEWED_DOMAINS:-} " in
    *" link.example.net "*)
        /usr/sbin/nginx -t
        /usr/bin/systemctl reload nginx
        ;;
esac
