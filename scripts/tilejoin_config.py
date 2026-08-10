#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Detect niri outputs and generate a validated display-group block."""

from __future__ import annotations

import argparse
import itertools
import json
import subprocess
from collections import Counter
from dataclasses import dataclass, replace
from typing import Any, cast

CONFIG_SCHEMA_VERSION = 1


@dataclass(frozen=True)
class Output:
    connector: str
    make: str
    model: str
    serial: str | None
    mode: dict[str, Any]
    modes: tuple[dict[str, Any], ...]
    transform: str
    scale: float
    logical: dict[str, Any]
    physical_size: tuple[int, int] | None

    @property
    def stable_selector(self) -> str:
        if self.make != "Unknown" and self.model != "Unknown" and self.serial:
            return f"{self.make} {self.model} {self.serial}"
        return self.connector

    @property
    def transformed_size(self) -> tuple[int, int]:
        size = (int(self.mode["width"]), int(self.mode["height"]))
        if self.transform in {"90", "270", "Flipped90", "Flipped270"}:
            return size[1], size[0]
        return size


@dataclass(frozen=True)
class ConfigOptions:
    name: str = "display-wall"
    scale: float | None = None
    relaxed_refresh: bool = False
    composited: bool = False


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="detect compatible outputs and print a niri display-group configuration"
    )
    parser.add_argument("connectors", nargs="*", help="two to four connector names")
    parser.add_argument("--name", default="display-wall", help="logical output name")
    parser.add_argument("--scale", type=float, help="logical output scale")
    parser.add_argument(
        "--relaxed-refresh",
        action="store_true",
        help="permit up to 100 mHz of member refresh mismatch",
    )
    parser.add_argument(
        "--composited",
        action="store_true",
        help="disable per-member direct scanout",
    )
    parser.add_argument(
        "--json-file",
        help="read niri output JSON from a file instead of the running compositor",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="list detected outputs without generating configuration",
    )
    parser.add_argument(
        "--diagnose",
        action="store_true",
        help="explain why every two-to-four-output candidate is accepted or rejected",
    )
    return parser.parse_args()


def parse_outputs(data: dict[str, Any]) -> list[Output]:
    outputs = []
    for connector, value in data.items():
        current = value.get("current_mode")
        logical = value.get("logical")
        if current is None or logical is None or "+" in connector:
            continue
        modes = value.get("modes", [])
        if not 0 <= current < len(modes):
            continue
        physical = value.get("physical_size")
        outputs.append(
            Output(
                connector=connector,
                make=value.get("make") or "Unknown",
                model=value.get("model") or "Unknown",
                serial=value.get("serial"),
                mode=modes[current],
                modes=tuple(modes),
                transform=str(logical.get("transform", "Normal")),
                scale=float(logical.get("scale", 1.0)),
                logical=logical,
                physical_size=tuple(physical) if physical else None,
            )
        )
    return sorted(outputs, key=lambda output: output.connector)


def query_output_data() -> dict[str, Any]:
    try:
        result = subprocess.run(
            ["niri", "msg", "-j", "outputs"],
            check=True,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        raise SystemExit("error: niri is not installed or not on PATH") from None
    except subprocess.CalledProcessError as error:
        message = error.stderr.strip() or "niri IPC request failed"
        raise SystemExit(f"error: {message}") from None
    return cast(dict[str, Any], json.loads(result.stdout))


def load_outputs(path: str | None) -> list[Output]:
    if path:
        with open(path, encoding="utf-8") as file:
            data = json.load(file)
    else:
        data = query_output_data()
    return parse_outputs(data)


def choose_common_modes(outputs: list[Output], relaxed: bool) -> list[Output] | None:
    """Mirror runtime strict/relaxed refresh selection at each current resolution."""
    tolerance = 100 if relaxed else 5
    candidate_sets = []
    for output in outputs:
        width = int(output.mode["width"])
        height = int(output.mode["height"])
        candidates = [
            mode
            for mode in output.modes
            if int(mode["width"]) == width and int(mode["height"]) == height
        ]
        if not candidates:
            return None
        candidate_sets.append(candidates)

    targets = sorted({int(mode["refresh_rate"]) for modes in candidate_sets for mode in modes})
    choices = []
    for target in targets:
        selected = []
        for modes in candidate_sets:
            compatible = [
                mode for mode in modes if abs(int(mode["refresh_rate"]) - target) <= tolerance
            ]
            if not compatible:
                break
            selected.append(
                min(
                    compatible,
                    key=lambda mode: (
                        abs(int(mode["refresh_rate"]) - target),
                        not bool(mode.get("is_preferred")),
                        -int(mode["refresh_rate"]),
                    ),
                )
            )
        if len(selected) != len(outputs):
            continue
        refreshes = [int(mode["refresh_rate"]) for mode in selected]
        spread = max(refreshes) - min(refreshes)
        if spread > tolerance:
            continue
        preferred = sum(bool(mode.get("is_preferred")) for mode in selected)
        choices.append(((-spread, preferred, min(refreshes)), selected))
    if not choices:
        return None
    selected = max(choices, key=lambda choice: choice[0])[1]
    return [replace(output, mode=mode) for output, mode in zip(outputs, selected, strict=True)]


def touching(left: Output, right: Output) -> bool:
    a = left.logical
    b = right.logical
    horizontal = (
        a["y"] == b["y"]
        and a["height"] == b["height"]
        and (a["x"] + a["width"] == b["x"] or b["x"] + b["width"] == a["x"])
    )
    vertical = (
        a["x"] == b["x"]
        and a["width"] == b["width"]
        and (a["y"] + a["height"] == b["y"] or b["y"] + b["height"] == a["y"])
    )
    return bool(horizontal or vertical)


def auto_pair(outputs: list[Output], relaxed: bool = False) -> list[Output]:
    pairs = [
        selected
        for index, left in enumerate(outputs)
        for right in outputs[index + 1 :]
        if (selected := choose_common_modes([left, right], relaxed)) is not None
        and touching(*selected)
        and layout_is_valid([left, right])
    ]
    if len(pairs) == 1:
        return pairs[0]
    if not pairs:
        raise SystemExit(
            "error: no unique touching compatible pair detected; pass connector names explicitly"
        )
    names = ", ".join(f"{pair[0].connector}+{pair[1].connector}" for pair in pairs)
    raise SystemExit(f"error: multiple compatible pairs detected ({names}); choose connectors")


def kdl_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def format_mode(mode: dict[str, Any]) -> str:
    refresh = int(mode["refresh_rate"])
    return f"{mode['width']}x{mode['height']}@{refresh // 1000}.{refresh % 1000:03d}"


def config_transform(transform: str) -> str:
    return {
        "Normal": "normal",
        "Flipped": "flipped",
        "Flipped90": "flipped-90",
        "Flipped180": "flipped-180",
        "Flipped270": "flipped-270",
    }.get(transform, transform)


def _rounded_pixel(value: float, label: str) -> int:
    rounded = round(value)
    if abs(value - rounded) > 0.05:
        raise ValueError(f"{label} does not land on a physical pixel ({value:g})")
    return int(rounded)


def member_positions(outputs: list[Output]) -> list[tuple[int, int]]:
    """Translate the current logical arrangement into normalized physical-pixel positions."""
    if not outputs:
        return []
    scale = outputs[0].scale
    if any(abs(output.scale - scale) > 1e-6 for output in outputs[1:]):
        raise ValueError(
            "selected outputs use different scales; align their scales first or write member "
            "positions manually"
        )

    origin_x = min(int(output.logical["x"]) for output in outputs)
    origin_y = min(int(output.logical["y"]) for output in outputs)
    positions = [
        (
            _rounded_pixel((int(output.logical["x"]) - origin_x) * scale, "member x"),
            _rounded_pixel((int(output.logical["y"]) - origin_y) * scale, "member y"),
        )
        for output in outputs
    ]

    rects = [
        (*position, *output.transformed_size)
        for output, position in zip(outputs, positions, strict=True)
    ]
    for index, (x, y, width, height) in enumerate(rects):
        right = x + width
        bottom = y + height
        for other_x, other_y, other_width, other_height in rects[index + 1 :]:
            if (
                x < other_x + other_width
                and other_x < right
                and y < other_y + other_height
                and other_y < bottom
            ):
                raise ValueError("selected output rectangles overlap in physical space")

    bounding_width = max(x + width for x, _, width, _ in rects)
    bounding_height = max(y + height for _, y, _, height in rects)
    covered = sum(width * height for _, _, width, height in rects)
    if covered != bounding_width * bounding_height:
        raise ValueError(
            "selected outputs do not cover one rectangular physical area; arrange them as a "
            "flush row/grid or write positions manually"
        )
    return positions


def layout_is_valid(outputs: list[Output]) -> bool:
    try:
        member_positions(outputs)
    except ValueError:
        return False
    return True


def candidate_diagnostics(outputs: list[Output], relaxed: bool = False) -> list[str]:
    """Explain group acceptance using the same refresh and geometry policy as generation."""
    diagnostics = []
    maximum = min(4, len(outputs))
    for count in range(2, maximum + 1):
        for candidate in itertools.combinations(outputs, count):
            names = "+".join(output.connector for output in candidate)
            selected = choose_common_modes(list(candidate), relaxed)
            if selected is None:
                policy = "relaxed (100 mHz)" if relaxed else "strict (5 mHz)"
                diagnostics.append(f"reject {names}: no common {policy} refresh")
                continue
            try:
                member_positions(selected)
            except ValueError as error:
                diagnostics.append(f"reject {names}: {error}")
                continue
            selectors = selectors_for(selected, outputs)
            connector_fallbacks = [
                output.connector
                for output, selector in zip(selected, selectors, strict=True)
                if selector == output.connector and output.stable_selector != output.connector
            ]
            detail = ""
            if connector_fallbacks:
                detail = "; duplicate EDID identity, using connector selector for " + ", ".join(
                    connector_fallbacks
                )
            diagnostics.append(f"accept {names}{detail}")
    if not diagnostics:
        diagnostics.append("reject: fewer than two active ordinary outputs were discovered")
    return diagnostics


def selectors_for(outputs: list[Output], discovered: list[Output] | None = None) -> list[str]:
    discovered = outputs if discovered is None else discovered
    counts = Counter(output.stable_selector.casefold() for output in discovered)
    return [
        output.connector
        if counts[output.stable_selector.casefold()] > 1
        else output.stable_selector
        for output in outputs
    ]


def select_outputs(
    outputs: list[Output], connectors: list[str], relaxed: bool = False
) -> list[Output]:
    if not connectors:
        return auto_pair(outputs, relaxed)
    if not 2 <= len(connectors) <= 4:
        raise SystemExit("error: choose between two and four connectors")
    if len(set(connectors)) != len(connectors):
        raise SystemExit("error: connector names must be unique")

    by_connector = {output.connector: output for output in outputs}
    missing = [name for name in connectors if name not in by_connector]
    if missing:
        raise SystemExit(f"error: connector not found: {', '.join(missing)}")
    selected = [by_connector[name] for name in connectors]
    compatible = choose_common_modes(selected, relaxed)
    if compatible is None:
        policy = "relaxed" if relaxed else "strict"
        raise SystemExit(f"error: selected outputs have no {policy} common refresh")
    return compatible


def config_for(
    outputs: list[Output],
    options: ConfigOptions,
    discovered: list[Output] | None = None,
) -> str:
    scale = options.scale if options.scale is not None else outputs[0].scale
    try:
        positions = member_positions(outputs)
    except ValueError as error:
        raise SystemExit(f"error: cannot infer display-group geometry: {error}") from None
    selectors = selectors_for(outputs, discovered)
    lines = [
        f"// Generated by niri-tilejoin; config-schema={CONFIG_SCHEMA_VERSION}",
        f"output {kdl_string(options.name)} {{",
        "    display-group {",
    ]
    for output, selector, (x, y) in zip(outputs, selectors, positions, strict=True):
        lines.extend(
            [
                f"        member {kdl_string(selector)} {{",
                f"            mode {kdl_string(format_mode(output.mode))}",
                f"            transform {kdl_string(config_transform(output.transform))}",
                f"            position x={x} y={y}",
                "        }",
            ]
        )
    lines.extend(
        [
            f"        primary {kdl_string(selectors[0])}",
            "        refresh-sync "
            + kdl_string("relaxed" if options.relaxed_refresh else "strict"),
            f"        render-policy {kdl_string('composited' if options.composited else 'auto')}",
            "    }",
            f"    scale {scale:g}",
            "}",
        ]
    )
    return "\n".join(lines)


def main() -> None:
    args = parse_args()
    outputs = load_outputs(args.json_file)
    if args.list:
        for output in outputs:
            width, height = output.transformed_size
            print(
                f"{output.connector}: {format_mode(output.mode)}, transform {output.transform}, "
                f"post-transform {width}x{height}, selector {output.stable_selector!r}"
            )
        return

    if args.diagnose:
        print("\n".join(candidate_diagnostics(outputs, args.relaxed_refresh)))
        return

    selected = select_outputs(outputs, args.connectors, args.relaxed_refresh)
    options = ConfigOptions(
        name=args.name,
        scale=args.scale,
        relaxed_refresh=args.relaxed_refresh,
        composited=args.composited,
    )
    print(config_for(selected, options, outputs))


if __name__ == "__main__":
    main()
