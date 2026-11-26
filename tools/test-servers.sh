#!/usr/bin/env bash
# Local test servers for trying FileDermaga (all in Docker, bound to 127.0.0.1).
#
#   tools/test-servers.sh start   # start servers (first run installs packages, ~1 min)
#   tools/test-servers.sh stop    # stop and remove them
#
# FTPS (TLS required):  ftpes://test@127.0.0.1:2121/            password: test123 (use -k)
# FTP  (plaintext):     ftp://test@127.0.0.1:2122/              password: test123
# SFTP:                 sftp://test@127.0.0.1:2222/             password: test123
#                       or key: -i ~/.cache/file-dermaga-test-servers/id_ed25519
# WebDAV (HTTP):        dav://test@127.0.0.1:8088/dav/          password: test123
# WebDAV (HTTPS):       davs://test@127.0.0.1:8443/dav/         password: test123 (use -k)
# WebDAV (Digest auth): dav://test@127.0.0.1:8089/dav/          password: test123
# S3:                   http://127.0.0.1:9100                   any access/secret key
#
# Services whose port is already in use are skipped.
set -euo pipefail

DIR="${XDG_CACHE_HOME:-$HOME/.cache}/file-dermaga-test-servers"

port_open() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

run_unless_open() { # port name docker-args...
    local port=$1 name=$2
    shift 2
    if port_open "$port"; then
        echo "port $port already in use, not starting $name"
    else
        docker run -d --rm --name "$name" --network host "$@" >/dev/null
    fi
}

start() {
    mkdir -p "$DIR/data" "$DIR/dav"
    chmod a+rwx "$DIR/data" "$DIR/dav"
    if [[ ! -f "$DIR/cert.pem" ]]; then
        openssl req -x509 -newkey rsa:2048 -nodes -keyout "$DIR/key.pem" -out "$DIR/cert.pem" \
            -days 365 -subj "/CN=localhost" 2>/dev/null
        chmod a+r "$DIR/key.pem"
    fi
    if [[ ! -f "$DIR/id_ed25519" ]]; then
        ssh-keygen -q -t ed25519 -N "" -C "file-dermaga-test" -f "$DIR/id_ed25519"
    fi

    cat >| "$DIR/ftp_server.py" <<'EOF'
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

    # WebDAV is served below /dav (like Nextcloud's /remote.php/dav/...).
    for p in 8088 8443; do
        cat >| "$DIR/wsgidav-$p.yaml" <<EOF
host: 127.0.0.1
port: $p
provider_mapping:
  "/dav": "/davroot"
http_authenticator:
  accept_basic: true
  accept_digest: false
  default_to_digest: false
simple_dc:
  user_mapping:
    "*":
      "test":
        password: "test123"
verbose: 1
EOF
    done
    printf 'ssl_certificate: /srv/cert.pem\nssl_private_key: /srv/key.pem\n' >> "$DIR/wsgidav-8443.yaml"
    # Digest-only variant (no Basic) on 8089.
    sed -e 's/^port: 8088/port: 8089/' -e 's/accept_basic: true/accept_basic: false/' \
        -e 's/accept_digest: false/accept_digest: true/' -e 's/default_to_digest: false/default_to_digest: true/' \
        "$DIR/wsgidav-8088.yaml" >| "$DIR/wsgidav-8089.yaml"

    run_unless_open 2121 file-dermaga-test-ftp -v "$DIR:/srv" -v "$DIR/data:/data" python:3.13-slim \
        sh -c "pip install -q pyftpdlib pyopenssl; python /srv/ftp_server.py tls 2121 & python /srv/ftp_server.py plain 2122"
    run_unless_open 9100 file-dermaga-test-s3 python:3.13-slim \
        sh -c "pip install -q 'moto[server]'; moto_server -H 127.0.0.1 -p 9100"
    run_unless_open 2222 file-dermaga-test-sftp -v "$DIR:/srv:ro" debian:bookworm-slim sh -c '
        apt-get update -qq && apt-get install -y -qq openssh-server >/dev/null &&
        useradd -m -s /bin/bash test && echo test:test123 | chpasswd &&
        mkdir -p /run/sshd /home/test/.ssh && cp /srv/id_ed25519.pub /home/test/.ssh/authorized_keys &&
        chown -R test:test /home/test/.ssh && chmod 700 /home/test/.ssh && chmod 600 /home/test/.ssh/authorized_keys &&
        exec /usr/sbin/sshd -D -e -p 2222 -o ListenAddress=127.0.0.1 -o PasswordAuthentication=yes'
    run_unless_open 8088 file-dermaga-test-dav -v "$DIR:/srv:ro" -v "$DIR/dav:/davroot" python:3.13-slim \
        sh -c "pip install -q wsgidav cheroot; wsgidav -c /srv/wsgidav-8088.yaml & exec wsgidav -c /srv/wsgidav-8443.yaml"
    run_unless_open 8089 file-dermaga-test-davdigest -v "$DIR:/srv:ro" -v "$DIR/dav:/davroot" python:3.13-slim \
        sh -c "pip install -q wsgidav cheroot; exec wsgidav -c /srv/wsgidav-8089.yaml"

    echo -n "waiting for servers"
    for _ in $(seq 1 120); do
        if port_open 2121 && port_open 2122 && port_open 9100 && port_open 2222 && port_open 8088 && port_open 8443 && port_open 8089; then
            echo " ready"
            echo "FTP files: $DIR/data   WebDAV files: $DIR/dav   SSH key: $DIR/id_ed25519"
            return
        fi
        echo -n "."
        sleep 2
    done
    echo " timed out; see: docker logs file-dermaga-test-{ftp,s3,sftp,dav}" >&2
    exit 1
}

stop() {
    # also stops containers started before the rename (gojjira-test-*)
    docker stop file-dermaga-test-ftp file-dermaga-test-s3 file-dermaga-test-sftp file-dermaga-test-dav file-dermaga-test-davdigest \
        gojjira-test-ftp gojjira-test-s3 2>/dev/null || true
}

case "${1:-}" in
    start) start ;;
    stop) stop ;;
    *) echo "usage: $0 start|stop" >&2; exit 2 ;;
esac
