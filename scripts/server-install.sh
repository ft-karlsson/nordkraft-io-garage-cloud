#!/bin/bash
# Kept for the old install URL — the real installer is install-node.sh.
exec bash "$(dirname "$0")/install-node.sh" --role server "$@"
