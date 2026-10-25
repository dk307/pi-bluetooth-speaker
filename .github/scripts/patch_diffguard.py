#!/usr/bin/env python3
"""Enables file-based custom_prompt in DiffGuard GitHub Action.

Requires Python >= 3.10.

Bypasses the Linux MAX_ARG_STRLEN (128 KB) environment variable limit by patching
DiffGuard's runner bundle (dist/index.js) to read custom_prompt directly from disk
when custom_prompt points to an existing file path.

Runner Lifecycle Invariant:
The GitHub Actions runner pre-downloads and extracts all actions declared via `uses:`
(including jonit-dev/diffguard pinned in .github/workflows/review.yml) into the runner
workspace (`../_actions/` or `_work/_actions/`) during the job initialization phase
("Set up job"), guaranteeing the action bundle is already present on disk before this
patching step runs.
"""

import glob
import os
import subprocess
import sys

# Pinned commit hash in jonit-dev/diffguard (v1.0.0, matching .github/workflows/review.yml)
# Verified via: git ls-remote https://github.com/jonit-dev/diffguard refs/tags/v1.0.0
# Upstream source line: https://github.com/jonit-dev/diffguard/blob/88f63615c769f8db6031973503c2c40a9a3f4feb/dist/index.js
# Upstream feature request tracker: https://github.com/jonit-dev/diffguard/issues/1
PINNED_SHA = "88f63615c769f8db6031973503c2c40a9a3f4feb"
TARGET_LINE = "const customPrompt = core.getInput('custom_prompt');"
PATCH_MARKER = "_fs.readFileSync(customPrompt, 'utf8')"
PATCH_LINE = (
    TARGET_LINE.replace("const ", "let ", 1)
    + " try { const _fs = require('fs'); if (customPrompt && _fs.existsSync(customPrompt)) "
    "{ customPrompt = _fs.readFileSync(customPrompt, 'utf8'); } } "
    "catch (err) { core.setFailed('Failed to read file-based customPrompt: ' + err.message); throw err; }"
)


def verify_js_syntax(path: str) -> None:
    """Verifies JavaScript syntax via `node --check`, exiting non-zero on failure."""
    try:
        subprocess.run(
            ["node", "--check", path],
            check=True,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except FileNotFoundError:
        print("ERROR: 'node' binary not found in PATH while verifying bundle syntax", file=sys.stderr)
        sys.exit(1)
    except subprocess.TimeoutExpired:
        print(f"ERROR: 'node --check {path}' timed out after 10s", file=sys.stderr)
        sys.exit(1)
    except subprocess.CalledProcessError as exc:
        print(
            f"ERROR: 'node --check {path}' failed (exit {exc.returncode}):\n{exc.stderr}",
            file=sys.stderr,
        )
        sys.exit(1)


def patch_bundle_in_roots(search_roots: list[str]) -> None:
    """Searches `search_roots` for DiffGuard's `dist/index.js` and applies the file-prompt patch."""
    found = False
    for root in search_roots:
        if not os.path.exists(root):
            continue
        for p in glob.glob(
            os.path.join(root, "**", "diffguard", "**", "dist", "index.js"),
            recursive=True,
        ):
            if PINNED_SHA not in p:
                print(
                    f"ERROR: diffguard bundle path '{p}' does not match pinned SHA '{PINNED_SHA}'",
                    file=sys.stderr,
                )
                sys.exit(1)

            with open(p, "r", encoding="utf-8") as f:
                content = f.read()

            if PATCH_MARKER in content and TARGET_LINE in content:
                print(
                    f"ERROR: {p} is in an unknown half-patched state (marker and target both present)",
                    file=sys.stderr,
                )
                sys.exit(1)

            if PATCH_MARKER in content and TARGET_LINE not in content:
                verify_js_syntax(p)
                print(f"Already patched and verified file-based prompt in {p}")
                found = True
                continue

            occurrences = content.count(TARGET_LINE)
            if occurrences != 1:
                print(
                    f"ERROR: expected exactly 1 patch site in {p}, found {occurrences}",
                    file=sys.stderr,
                )
                sys.exit(1)

            with open(p, "w", encoding="utf-8") as f:
                f.write(content.replace(TARGET_LINE, PATCH_LINE, 1))
            with open(p, "r", encoding="utf-8") as f:
                patched = f.read()
            if PATCH_MARKER not in patched or TARGET_LINE in patched:
                print(f"ERROR: patch verification failed in {p}", file=sys.stderr)
                sys.exit(1)
            verify_js_syntax(p)
            print(f"Successfully enabled and verified file-based prompt in {p}")
            found = True

    if not found:
        print(
            f"ERROR: diffguard bundle not found or target string missing in search roots: {search_roots}",
            file=sys.stderr,
        )
        sys.exit(1)


def main() -> None:
    workspace = os.environ.get("GITHUB_WORKSPACE", ".")
    search_roots = [
        "/home/runner/work/_actions",
        os.path.abspath(os.path.join(workspace, "..", "..", "_actions")),
        os.path.abspath(os.path.join(workspace, "..", "_actions")),
    ]
    patch_bundle_in_roots(search_roots)


if __name__ == "__main__":
    main()
