#!/usr/bin/env python3
"""Generates the full custom review prompt for DiffGuard AI PR Review.

Reads repository guidelines directly from:
- AGENTS.md (Root architecture, target hardware, safety rules, coverage)
- services/core/AGENTS.md (Hardware abstraction, inherent mocking, domain types)
- services/.guidelines/microsoft-rust-guidelines.md (Microsoft Pragmatic Rust rules)

Formats all rules into a comprehensive review prompt and writes it to GITHUB_OUTPUT
as a multiline step output, staying safely below the Linux MAX_ARG_STRLEN (128 KB)
process environment variable limit.
"""

import os
import re
from pathlib import Path

# Linux kernel MAX_ARG_STRLEN limit is 128 KB (131,072 bytes).
# We enforce a safe upper threshold of 120 KB (122,880 bytes).
MAX_PROMPT_BYTES = 122880
FENCE_RE = re.compile(r"^\s*(```|~~~)")


def load_file(path: Path) -> str:
    """Reads file text from repository path."""
    if not path.is_file():
        raise FileNotFoundError(f"Guidelines file not found at: {path}")
    return path.read_text(encoding="utf-8").strip()


def split_top_level_sections(md: str) -> list[str]:
    """Splits markdown into top-level sections (# Title), ignoring headers inside code fences."""
    lines = md.splitlines(keepends=True)
    sections = []
    current_section = []
    current_fence = None

    for line in lines:
        m = FENCE_RE.match(line)
        if m:
            fence_chars = m.group(1)
            if current_fence is None:
                current_fence = fence_chars
            elif current_fence == fence_chars:
                current_fence = None
            current_section.append(line)
            continue

        if current_fence is None and re.match(r"^# [A-Z]", line) and current_section:
            sections.append("".join(current_section))
            current_section = [line]
        else:
            current_section.append(line)

    if current_fence is not None:
        raise ValueError(
            f"unbalanced code fences detected in markdown file: fence {current_fence} was not closed"
        )

    if current_section:
        sections.append("".join(current_section))
    return sections


def build_review_prompt(repo_root: Path) -> str:
    """Assembles the complete PR review prompt containing the full guidelines."""
    agents_path = repo_root / "AGENTS.md"
    core_agents_path = repo_root / "services" / "core" / "AGENTS.md"
    guidelines_path = repo_root / "services" / ".guidelines" / "microsoft-rust-guidelines.md"

    agents_content = load_file(agents_path)
    core_agents_content = load_file(core_agents_path)
    raw_guidelines = load_file(guidelines_path)

    # Process guidelines:
    # 1. Filter sections that are irrelevant to native Rust code reviews (e.g. C-FFI Windows DLLs,
    #    proc-macro crate development, and meta AI-assistant prompt instructions).
    sections = split_top_level_sections(raw_guidelines)
    kept_sections = []
    excluded_keywords = [
        "AI Guidelines",
        "FFI Guidelines",
        "Macros Guidelines",
    ]

    for section in sections:
        header = section.splitlines()[0] if section else ""
        if any(keyword in header for keyword in excluded_keywords):
            continue
        kept_sections.append(section)

    guidelines_body = "\n".join(kept_sections)

    # 2. Strip markdown link anchors ({ #M-... }), URLs in links ([text](url) -> text),
    #    link-reference definitions ([name]: url), HTML comments, and consecutive blank lines.
    guidelines_body = re.sub(r"<!--.*?-->", "", guidelines_body, flags=re.DOTALL)
    guidelines_body = re.sub(r"^\s*\[[^\]]+\]:\s+\S+.*$", "", guidelines_body, flags=re.MULTILINE)
    guidelines_body = re.sub(r"\s*\{\s*#[A-Za-z0-9\-_]+\s*\}", "", guidelines_body)
    guidelines_body = re.sub(r"\[([^\]]+)\]\([^\)]+\)", r"\1", guidelines_body)
    guidelines_body = re.sub(r"\n{3,}", "\n\n", guidelines_body).strip()

    prompt = f"""You are reviewing pull requests for the Raspberry Pi Bluetooth Speaker & DSP Appliance project.
Perform a rigorous, constructive, and actionable engineering review adhering strictly to the guidelines and standards below.

# SECTION 1: REPOSITORY ROOT RULES (AGENTS.md)
{agents_content}

# SECTION 2: CORE SERVICE & HARDWARE RULES (services/core/AGENTS.md)
{core_agents_content}

# SECTION 3: MICROSOFT PRAGMATIC RUST GUIDELINES (FULL RULES & BEST PRACTICES)
{guidelines_body}

# REVIEW INSTRUCTIONS & REQUIRED OUTPUT FORMAT:
- Analyze all diffs thoroughly against the above standards.
- Point out concrete issues with exact file paths and line numbers.
- Avoid generic advice; focus on actionable defects, invariant violations, unsafe patterns, and compliance gaps.
- Structure your review using the following sections:
  1. Brief Summary of changes
  2. Strengths / Good patterns observed
  3. Critical Issues (bugs, invariant violations, unhandled errors, panics, boundary issues)
  4. Compliance Findings (Pragmatic Rust Guidelines, Target Architecture Cortex-A53 / NEON, Code Coverage >85-90%+)
  5. Actionable Recommendations with code snippets where helpful
"""
    byte_len = len(prompt.encode("utf-8"))
    print(
        f"Generated review prompt size: {byte_len} bytes ({byte_len / 1024:.2f} KB) across {len(kept_sections)} guideline sections"
    )

    if byte_len > MAX_PROMPT_BYTES:
        raise ValueError(
            f"Review prompt size ({byte_len} bytes) exceeds maximum allowed limit "
            f"({MAX_PROMPT_BYTES} bytes) to satisfy Linux MAX_ARG_STRLEN."
        )

    return prompt


def main() -> None:
    # Resolve repository root
    script_dir = Path(__file__).resolve().parent
    repo_root = script_dir.parent.parent

    prompt = build_review_prompt(repo_root)

    github_output = os.environ.get("GITHUB_OUTPUT")
    if github_output:
        delimiter = f"PR_REVIEW_PROMPT_EOF_{os.urandom(8).hex()}"
        with open(github_output, "a", encoding="utf-8") as out:
            out.write(f"prompt<<{delimiter}\n{prompt}\n{delimiter}\n")
        print("Successfully exported 'prompt' to GITHUB_OUTPUT.")
    else:
        print("GITHUB_OUTPUT environment variable not set (running locally).")


if __name__ == "__main__":
    main()
