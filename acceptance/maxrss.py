#!/usr/bin/env python3
"""usage: maxrss.py OUT_FILE CMD [ARGS...] -- run CMD (stdio inherited), write its peak RSS (MB) to OUT_FILE, exit with its code."""
import resource
import subprocess
import sys

rc = subprocess.call(sys.argv[2:])
open(sys.argv[1], "w").write(str(resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss // 1024))
sys.exit(rc)
