#!/bin/bash

set -e -u -o pipefail

VERSION="2.318.0"
HASH="28ed88e4cedf0fc93201a901e392a70463dbd0213f2ce9d57a4ab495027f3e2f"

FILE="actions-runner-linux-x64-${VERSION}.tar.gz"
URL="https://github.com/actions/runner/releases/download/v${VERSION}/${FILE}"

if ! test -e "${FILE}"
then
    curl --location --output "${FILE}" "${URL}"
    echo "${HASH} ${FILE}" > "${FILE}.hash"
    sha256sum --check "${FILE}.hash"

    mkdir --parents runner
    tar --extract --file "${FILE}" --directory runner
fi

./runner/run.sh --jitconfig <JITCONFIG>
