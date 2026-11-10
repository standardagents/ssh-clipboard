#!/usr/bin/env bash
set -euo pipefail

# All build dependencies come from Ubuntu. Unrelated browser/vendor repositories
# preinstalled on hosted runners must not block builds with stale index hashes.
sources=/etc/apt/sources.list.d/ubuntu.sources
test -f "$sources"
apt-get -o "Dir::Etc::sourcelist=$sources" -o Dir::Etc::sourceparts=- \
  -o Acquire::Retries=3 update
apt-get -o "Dir::Etc::sourcelist=$sources" -o Dir::Etc::sourceparts=- \
  -o Acquire::Retries=3 install -y "$@"
