#!/usr/bin/env bash
# nextest's setup script: every test process gets a scratch KROWK_HOME, so
# nothing a test runs — krowk's own git, which makes the home — creates the
# developer's real ~/.krowk or moves an older krowk's files into it.
set -euo pipefail
home="$PWD/target/krowk-test-home"
mkdir -p "$home"
chmod 700 "$home"
echo "KROWK_HOME=$home" >> "$NEXTEST_ENV"
