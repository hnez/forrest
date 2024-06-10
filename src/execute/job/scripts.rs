pub const SETUP: &str = r##"#!/bin/bash

set -e -u

SERVICE_FILE="github-action-runner.service"
SERVICE_PATH="/etc/systemd/system/${SERVICE_FILE}"

if ! test -e "${SERVICE_PATH}"
then
    cat > "${SERVICE_PATH}" << 'EOF'
[Unit]
Description=GitHub JIT Runner
After=network.target

[Service]
ExecStartPre=+/usr/bin/mount -o rw,fmask=0022,dmask=0022,uid=runner,gid=runner --mkdir /dev/disk/by-label/JOBDATA /home/runner/config
ExecStart=/home/runner/config/job.sh
ExecStopPost=+/usr/bin/systemctl poweroff
User=runner
WorkingDirectory=/home/runner
KillMode=process
KillSignal=SIGTERM
TimeoutStopSec=5min

[Install]
WantedBy=multi-user.target
EOF

    systemctl daemon-reload
    systemctl enable "${SERVICE_FILE}"
    systemctl start --no-block "${SERVICE_FILE}"
fi

GETTY_SERVICE="serial-getty@ttyS1.service"
GETTY_DIR="/etc/systemd/system/${GETTY_SERVICE}.d/"
GETTY_OVERRIDE="${GETTY_DIR}/override.conf"

if ! test -e "${GETTY_OVERRIDE}"
then
    mkdir -p "${GETTY_DIR}"
    cat > "${GETTY_OVERRIDE}" << 'EOF'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin root --noclear %I $TERM
EOF

    systemctl daemon-reload
    systemctl enable --now --no-block "${GETTY_SERVICE}"
fi
"##;

pub const JOB: &str = r##"#!/bin/bash

set -e -u -o pipefail

VERSION="2.317.0"
HASH="9e883d210df8c6028aff475475a457d380353f9d01877d51cc01a17b2a91161d"

FILE="actions-runner-linux-x64-${VERSION}.tar.gz"
URL="https://github.com/actions/runner/releases/download/v${VERSION}/${FILE}"

if ! test -e "${FILE}"
then
    curl -L -o "${FILE}" "${URL}"
    echo "${HASH} ${FILE}" > "${FILE}.hash"
    sha256sum -c "${FILE}.hash"

    mkdir runner -p
    tar -xf "${FILE}" -C runner
fi

./runner/run.sh --jitconfig JITCONFIG
"##;
