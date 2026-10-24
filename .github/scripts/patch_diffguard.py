#!/usr/bin/env python3
"""Enables file-based custom_prompt in DiffGuard GitHub Action.

Bypasses the Linux MAX_ARG_STRLEN (128 KB) environment variable limit by patching
DiffGuard's runner bundle (dist/index.js) to read custom_prompt directly from disk
when custom_prompt points to an existing file path.
"""

import glob
import os
import subprocess
import sys


def main() -> None:
    workspace = os.environ.get("GITHUB_WORKSPACE", ".")
    search_roots = [
        "/home/runner/work/_actions",
        os.path.abspath(os.path.join(workspace, "..", "..", "_actions")),
        os.path.abspath(os.path.join(workspace, "..", "_actions")),
    ]
    found = False
    for root in search_roots:
        if not os.path.exists(root):
            continue
        for p in glob.glob(
            os.path.join(root, "**", "diffguard", "**", "dist", "index.js"),
            recursive=True,
        ):
            with open(p, "r", encoding="utf-8") as f:
                content = f.read()
            # Pinned target in jonit-dev/diffguard@88f63615c769f8db6031973503c2c40a9a3f4feb (dist/index.js)
            # Upstream tracking for native file-based custom_prompt: https://github.com/jonit-dev/diffguard/issues
            target = "const customPrompt = core.getInput('custom_prompt');"
            patch = (
                "let customPrompt = core.getInput('custom_prompt'); "
                "try { const _fs = require('fs'); if (customPrompt && _fs.existsSync(customPrompt)) "
                "{ customPrompt = _fs.readFileSync(customPrompt, 'utf8'); } } "
                "catch (err) { core.setFailed('Failed to read file-based customPrompt: ' + err.message); throw err; }"
            )
            if target in content:
                with open(p, "w", encoding="utf-8") as f:
                    f.write(content.replace(target, patch))
                with open(p, "r", encoding="utf-8") as f:
                    patched = f.read()
                if "fs.readFileSync" not in patched:
                    print(f"ERROR: patch verification failed in {p}", file=sys.stderr)
                    sys.exit(1)
                subprocess.run(["node", "--check", p], check=True)
                print(f"Successfully enabled and verified file-based prompt in {p}")
                found = True

    if not found:
        print(
            f"ERROR: diffguard bundle not found or target string missing in search roots: {search_roots}",
            file=sys.stderr,
        )
        sys.exit(1)


if __name__ == "__main__":
    main()
