#!/usr/bin/env bash
# nextest's setup script: every test process gets a scratch GRUNTAS_HOME, so
# nothing a test runs — krowk's own git, which makes the home — creates the
# developer's real ~/.gruntas or copies krowk's ~/.krowk into it.
set -euo pipefail
home="$PWD/target/krowk-test-home"
mkdir -p "$home"
chmod 700 "$home"
echo "GRUNTAS_HOME=$home" >> "$NEXTEST_ENV"
