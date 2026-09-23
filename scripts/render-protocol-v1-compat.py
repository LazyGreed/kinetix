#!/usr/bin/env python3
"""Render the v1 field compatibility reference from the executable contract fixture."""

import argparse
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = ROOT / "tests/fixtures/protocol-v1-compatibility.json"


def load_contract():
    data = json.loads(SOURCE.read_text())
    if data.get("schema") != "kinetix.protocol.compatibility" or data.get("schema_version") != 1:
        raise SystemExit("unsupported protocol compatibility fixture schema")
    case_ids = [case["id"] for case in data["cases"]]
    if len(case_ids) != len(set(case_ids)):
        raise SystemExit("duplicate compatibility case id")
    known = set(case_ids)
    for api in data["apis"]:
        for row in api["rows"]:
            if len(row) != 5:
                raise SystemExit(f"invalid compatibility row: {row!r}")
            evidence = row[4] if isinstance(row[4], list) else [row[4]]
            if not evidence:
                raise SystemExit(f"missing evidence for {api['endpoint']} row {row[0]!r}")
            unknown = [case_id for case_id in evidence if case_id not in known]
            if unknown:
                raise SystemExit(
                    f"unknown evidence case(s) {unknown!r} for {api['endpoint']} row {row[0]!r}"
                )
    return data


def cell(value):
    return str(value).replace("|", "\\|").replace("\n", " ")


def render(data):
    q = chr(96)
    lines = [
        "# Protocol v1 field compatibility",
        "",
        "<!-- GENERATED: scripts/render-protocol-v1-compat.py; edit tests/fixtures/protocol-v1-compatibility.json instead. -->",
        "",
        "This document is generated from the same versioned compatibility contract consumed by the",
        "deterministic protocol matrix. It describes Kinetix v1 behavior at the field level; it is",
        "not a claim that every field of every upstream vendor API is implemented.",
        "",
        "## Status semantics",
        "",
        "- **passthrough**: same-format requests keep the original client body; provider-specific fields survive Kinetix.",
        "- **translated**: the field has an explicit canonical representation and is rebuilt for the target adapter.",
        "- **rejected (422)**: the field changes semantics but has no faithful cross-format representation.",
        "- **not guaranteed**: cosmetic/unknown data may be ignored on translation; use same-format passthrough if it matters.",
        "- **exact / estimated**: token-count responses expose the selected mode in " + q + "X-Kinetix-Token-Count" + q + ".",
        "",
    ]

    for api in data["apis"]:
        lines += [
            "## " + api["endpoint"],
            "",
            "| Field / behavior | Native / same-format | Translated / other path | Contract | Evidence |",
            "|---|---|---|---|---|",
        ]
        for field, native, translated, notes, evidence in api["rows"]:
            evidence_ids = evidence if isinstance(evidence, list) else [evidence]
            evidence_cell = "<br>".join(q + cell(case_id) + q for case_id in evidence_ids)
            lines.append(
                "| " + " | ".join(
                    cell(v)
                    for v in (
                        field,
                        native,
                        translated,
                        notes,
                        evidence_cell,
                    )
                ) + " |"
            )
        lines.append("")

    lines += [
        "## Executable evidence catalog",
        "",
        "| Case | Runner | What it proves |",
        "|---|---|---|",
    ]
    for case in data["cases"]:
        lines.append(
            "| "
            + " | ".join(
                (
                    q + cell(case["id"]) + q,
                    cell(case["runner"]),
                    cell(case["summary"]),
                )
            )
            + " |"
        )

    lines += [
        "",
        "The " + q + "http" + q + " cases run through " + q + "scripts/protocol-v1-matrix.py" + q
        + " inside the existing synthetic compatibility harness. The " + q + "cargo" + q
        + " cases run as normal Rust integration tests. Entries marked " + q + "manual" + q
        + " are executable release acceptance and are intentionally excluded from normal PR/local CI. Real Pi, Claude Code,"
        " Responses-client, and external " + q + ".kxp" + q + " sessions remain release-only.",
        "",
    ]
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    data = load_contract()
    output = ROOT / data["generated_doc"]
    rendered = render(data) + "\n"

    if args.check:
        current = output.read_text() if output.exists() else ""
        if current != rendered:
            print(f"{output.relative_to(ROOT)} is stale; run scripts/render-protocol-v1-compat.py", file=sys.stderr)
            return 1
        print(f"{output.relative_to(ROOT)} is up to date")
        return 0

    output.write_text(rendered)
    print(output.relative_to(ROOT))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
