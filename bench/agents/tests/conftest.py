"""Pytest bootstrap for bench/agents/tests/.

bench/agents/*.py are plain scripts (not an installed package), so make
them importable by name (harness_common, harness_opencode, ...) by putting
bench/agents/ on sys.path. See tests/README-style header comment in
test_harness_common.py for the full run command.
"""

import sys
from pathlib import Path

AGENTS_DIR = Path(__file__).resolve().parents[1]
if str(AGENTS_DIR) not in sys.path:
    sys.path.insert(0, str(AGENTS_DIR))
