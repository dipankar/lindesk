#!/usr/bin/env python3
"""{{name}}: a lindesk {{kind}} plugin.

Unit tests:   python3 test_{{module}}.py
Conformance:  lindeskctl plugin test .
"""
import os
import sys

# Use the SDK in LINDESK_SDK_PATH if set, else the vendored copy next to this file.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
if os.environ.get("LINDESK_SDK_PATH"):
    sys.path.insert(0, os.environ["LINDESK_SDK_PATH"])

import lindesk_plugin as lp  # noqa: E402

{{body}}

if __name__ == "__main__":
    lp.run(name="{{name}}", version="0.1.0")
