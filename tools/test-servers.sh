#!/usr/bin/env bash
# Local FTP/FTPS and S3 servers for trying FileDermaga.
#
#   tools/test-servers.sh start   # start servers (first run installs packages, ~30s)
#   tools/test-servers.sh stop    # stop and remove them
#
# FTPS (TLS required):  ftpes://test@127.0.0.1:2121/   password: test123   (use -k)
# FTP  (plaintext):     ftp://test@127.0.0.1:2122/     password: test123
# S3:                   http://127.0.0.1:9100           any access/secret key
set -euo pipefail

DIR="${XDG_CACHE_HOME:-$HOME/.cache}/file-dermaga-test-servers"

start() {
    mkdir -p "$DIR/data"
    chmod a+rwx "$DIR/data"
    if [[ ! -f "$DIR/cert.pem" ]]; then
        openssl req -x509 -newkey rsa:2048 -nodes -keyout "$DIR/key.pem" -out "$DIR/cert.pem" \
            -days 365 -subj "/CN=localhost" 2>/dev/null
        chmod a+r "$DIR/key.pem"
    fi
    cat >| "$DIR/server.py" <<'EOF'
import sys
from pyftpdlib.authorizers import DummyAuthorizer
from pyftpdlib.handlers import TLS_FTPHandler, FTPHandler
from pyftpdlib.servers import FTPServer

mode, port = sys.argv[1], int(sys.argv[2])
auth = DummyAuthorizer()
auth.add_user("test", "test123", "/data", perm="elradfmwMT")
if mode == "tls":
    h = TLS_FTPHandler
    h.certfile, h.keyfile = "/srv/cert.pem", "/srv/key.pem"
    h.tls_control_required = h.tls_data_required = True
else:
    h = FTPHandler
h.authorizer = auth
h.passive_ports = range(port + 100, port + 120)
FTPServer(("127.0.0.1", port), h).serve_forever()
EOF
    docker run -d --rm --name file-dermaga-test-ftp --network host -v "$DIR:/srv" -v "$DIR/data:/data" \
        python:3.13-slim sh -c "pip install -q pyftpdlib pyopenssl; \
            python /srv/server.py tls 2121 & python /srv/server.py plain 2122" >/dev/null
    docker run -d --rm --name file-dermaga-test-s3 --network host python:3.13-slim \
        sh -c "pip install -q 'moto[server]'; moto_server -H 127.0.0.1 -p 9100" >/dev/null

    echo -n "waiting for servers"
    for _ in $(seq 1 90); do
        if (exec 3<>/dev/tcp/127.0.0.1/2121) 2>/dev/null &&
           (exec 3<>/dev/tcp/127.0.0.1/2122) 2>/dev/null &&
           (exec 3<>/dev/tcp/127.0.0.1/9100) 2>/dev/null; then
            echo " ready"
            echo "FTP files are stored in $DIR/data"
            return
        fi
        echo -n "."
        sleep 2
    done
    echo " timed out; see: docker logs file-dermaga-test-ftp / file-dermaga-test-s3" >&2
    exit 1
}

stop() {
    # also stops containers started before the rename (gojjira-test-*)
    docker stop file-dermaga-test-ftp file-dermaga-test-s3 gojjira-test-ftp gojjira-test-s3 2>/dev/null || true
}

case "${1:-}" in
    start) start ;;
    stop) stop ;;
    *) echo "usage: $0 start|stop" >&2; exit 2 ;;
esac
