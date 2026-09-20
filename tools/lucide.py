"""Lucide path data, flattened to polylines so PIL can stroke it.

STYLE-GUIDE §11 says a native app vendors the geometry rather than the file:
the `d` attributes copied out of the package verbatim, named by their Lucide
names, flattened at draw time. This is that, in Python, for the one place
Brokey draws a Lucide mark outside the page: its own application icon.

The reference implementation is Muster's `crates/muster-app/src/lucide.rs`.
This is the same shape and deliberately no more: a path parser, arcs through
SVG's own endpoint conversion, and a flatten. It draws nothing itself.

Every `d` below was copied out of `node_modules/lucide-react/dist/esm/icons/`
at lucide-react 1.43.0. Lucide is ISC and is credited in README.md.
"""

from __future__ import annotations

import math
import re

# The viewBox every Lucide icon is drawn in, and the stroke it is drawn with.
VIEWBOX = 24.0
STROKE = 1.5

# One entry per mark, keyed by its Lucide name, holding that icon's `d`
# attributes in the order the package lists them. Names are Lucide's own and
# are not renamed: §11 is explicit that an icon set is a shared enum.
ICONS: dict[str, list[str]] = {
    "package": [
        "M11 21.73a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73z",
        "M12 22V12",
        "m3.3 7 7.703 4.734a2 2 0 0 0 1.994 0L20.7 7",
        "m7.5 4.27 9 5.15",
    ],
    "layers": [
        "M12.83 2.18a2 2 0 0 0-1.66 0L2.6 6.08a1 1 0 0 0 0 1.83l8.58 3.91a2 2 0 0 0 1.66 0l8.58-3.9a1 1 0 0 0 0-1.83z",
        "M2 12a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 12",
        "M2 17a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 17",
    ],
    "boxes": [
        "M2.97 12.92A2 2 0 0 0 2 14.63v3.24a2 2 0 0 0 .97 1.71l3 1.8a2 2 0 0 0 2.06 0L12 19v-5.5l-5-3-4.03 2.42Z",
        "m7 16.5-4.74-2.85",
        "m7 16.5 5-3",
        "M7 16.5v5.17",
        "M12 13.5V19l3.97 2.38a2 2 0 0 0 2.06 0l3-1.8a2 2 0 0 0 .97-1.71v-3.24a2 2 0 0 0-.97-1.71L17 10.5l-5 3Z",
        "m17 16.5-5-3",
        "m17 16.5 4.74-2.85",
        "M17 16.5v5.17",
        "M7.97 4.42A2 2 0 0 0 7 6.13v4.37l5.03 3.01a2 2 0 0 0 2.06 0L17 10.5V6.13a2 2 0 0 0-.97-1.71l-3-1.8a2 2 0 0 0-2.06 0z",
        "M12 8 7.26 5.15",
        "m12 8 4.74-2.85",
        "M12 13.5V8",
    ],
    "blocks": [
        "M10 22V7a1 1 0 0 0-1-1H2a1 1 0 0 0 0 2h5v14a1 1 0 0 0 1 1z",
        "M22 17a1 1 0 0 0-1-1h-5a1 1 0 0 0-1 1v5h6a1 1 0 0 0 1-1z",
        "M9 14h12a1 1 0 0 0 1-1V3a1 1 0 0 0-1-1H10a1 1 0 0 0-1 1z",
    ],
    "store": [
        "M15 21v-5a1 1 0 0 0-1-1h-4a1 1 0 0 0-1 1v5",
        "M17.774 10.31a1.12 1.12 0 0 0-1.549 0 2.5 2.5 0 0 1-3.451 0 1.12 1.12 0 0 0-1.548 0 2.5 2.5 0 0 1-3.452 0 1.12 1.12 0 0 0-1.549 0 2.5 2.5 0 0 1-3.77-3.248l2.889-4.184A2 2 0 0 1 7 2h10a2 2 0 0 1 1.653.873l2.895 4.192a2.5 2.5 0 0 1-3.774 3.244",
        "M4 10.95V19a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-8.05",
    ],
}

_TOKEN = re.compile(r"[MmLlHhVvCcSsQqTtAaZz]|-?\d*\.?\d+(?:[eE][-+]?\d+)?")


def _numbers(tokens: list[str], at: int, count: int) -> tuple[list[float], int]:
    """`count` numbers from `at`, and where reading stopped."""
    values = [float(tokens[at + i]) for i in range(count)]
    return values, at + count


def _cubic(p0, p1, p2, p3, steps: int) -> list[tuple[float, float]]:
    out = []
    for i in range(1, steps + 1):
        t = i / steps
        u = 1 - t
        x = u * u * u * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t * t * t * p3[0]
        y = u * u * u * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t * t * t * p3[1]
        out.append((x, y))
    return out


def _quadratic(p0, p1, p2, steps: int) -> list[tuple[float, float]]:
    # Raised to a cubic rather than flattened separately, so one routine is
    # responsible for how smooth a curve comes out.
    c1 = (p0[0] + 2 / 3 * (p1[0] - p0[0]), p0[1] + 2 / 3 * (p1[1] - p0[1]))
    c2 = (p2[0] + 2 / 3 * (p1[0] - p2[0]), p2[1] + 2 / 3 * (p1[1] - p2[1]))
    return _cubic(p0, c1, c2, p2, steps)


def _arc(p0, rx, ry, rotation, large, sweep, p1, steps: int) -> list[tuple[float, float]]:
    """SVG's endpoint parameterisation, turned into its centre form.

    Straight out of the specification's implementation notes, F.6.5 and
    F.6.6. Lucide leans on arcs for every rounded corner, so this is not an
    edge case: `package` alone has four of them.
    """
    if rx == 0 or ry == 0:
        return [p1]
    rx, ry = abs(rx), abs(ry)
    phi = math.radians(rotation)
    cos_phi, sin_phi = math.cos(phi), math.sin(phi)

    dx2 = (p0[0] - p1[0]) / 2
    dy2 = (p0[1] - p1[1]) / 2
    x1 = cos_phi * dx2 + sin_phi * dy2
    y1 = -sin_phi * dx2 + cos_phi * dy2

    # F.6.6: grow the radii when they are too small to span the endpoints.
    lam = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry)
    if lam > 1:
        scale = math.sqrt(lam)
        rx *= scale
        ry *= scale

    numerator = rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1
    denominator = rx * rx * y1 * y1 + ry * ry * x1 * x1
    factor = math.sqrt(max(0.0, numerator / denominator)) if denominator else 0.0
    if large == sweep:
        factor = -factor
    cx1 = factor * rx * y1 / ry
    cy1 = -factor * ry * x1 / rx

    cx = cos_phi * cx1 - sin_phi * cy1 + (p0[0] + p1[0]) / 2
    cy = sin_phi * cx1 + cos_phi * cy1 + (p0[1] + p1[1]) / 2

    def angle(ux, uy, vx, vy) -> float:
        dot = ux * vx + uy * vy
        length = math.hypot(ux, uy) * math.hypot(vx, vy)
        if length == 0:
            return 0.0
        value = max(-1.0, min(1.0, dot / length))
        sign = -1.0 if ux * vy - uy * vx < 0 else 1.0
        return sign * math.acos(value)

    start = angle(1, 0, (x1 - cx1) / rx, (y1 - cy1) / ry)
    sweep_angle = angle(
        (x1 - cx1) / rx, (y1 - cy1) / ry, (-x1 - cx1) / rx, (-y1 - cy1) / ry
    )
    if not sweep and sweep_angle > 0:
        sweep_angle -= 2 * math.pi
    elif sweep and sweep_angle < 0:
        sweep_angle += 2 * math.pi

    out = []
    for i in range(1, steps + 1):
        theta = start + sweep_angle * i / steps
        x = cos_phi * rx * math.cos(theta) - sin_phi * ry * math.sin(theta) + cx
        y = sin_phi * rx * math.cos(theta) + cos_phi * ry * math.sin(theta) + cy
        out.append((x, y))
    return out


def flatten(d: str, steps: int = 12) -> list[list[tuple[float, float]]]:
    """One `d` attribute as polylines, in the icon's own 24-unit space.

    A list per subpath, because a mark is stroked and not filled: joining two
    subpaths with a line would draw a stroke the icon does not have. A closed
    subpath comes back with its first point repeated at the end, which is what
    makes the join at the start draw like every other join.
    """
    tokens = _TOKEN.findall(d)
    subpaths: list[list[tuple[float, float]]] = []
    current: list[tuple[float, float]] = []
    point = (0.0, 0.0)
    start_point = (0.0, 0.0)
    # The reflected control point that S and T mirror, and which command
    # produced it. Anything else resets the reflection to the point itself.
    last_control: tuple[float, float] | None = None
    last_command = ""

    i = 0
    while i < len(tokens):
        token = tokens[i]
        if token.isalpha():
            command = token
            i += 1
        else:
            # A repeated coordinate pair continues the previous command, and
            # a repeated M continues as an L, which is what the grammar says.
            command = "L" if last_command == "M" else "l" if last_command == "m" else last_command
        relative = command.islower()
        upper = command.upper()

        if upper == "Z":
            if current:
                current.append(start_point)
                subpaths.append(current)
                current = []
            point = start_point
            last_control = None
            last_command = command
            continue

        if upper == "M":
            (x, y), i = _numbers(tokens, i, 2)
            if relative:
                x, y = point[0] + x, point[1] + y
            if current:
                subpaths.append(current)
            point = (x, y)
            start_point = point
            current = [point]
            last_control = None
        elif upper == "L":
            (x, y), i = _numbers(tokens, i, 2)
            if relative:
                x, y = point[0] + x, point[1] + y
            point = (x, y)
            current.append(point)
            last_control = None
        elif upper == "H":
            (x,), i = _numbers(tokens, i, 1)
            if relative:
                x = point[0] + x
            point = (x, point[1])
            current.append(point)
            last_control = None
        elif upper == "V":
            (y,), i = _numbers(tokens, i, 1)
            if relative:
                y = point[1] + y
            point = (point[0], y)
            current.append(point)
            last_control = None
        elif upper in ("C", "S"):
            if upper == "C":
                (x1, y1, x2, y2, x, y), i = _numbers(tokens, i, 6)
                if relative:
                    x1, y1 = point[0] + x1, point[1] + y1
                    x2, y2 = point[0] + x2, point[1] + y2
                    x, y = point[0] + x, point[1] + y
            else:
                (x2, y2, x, y), i = _numbers(tokens, i, 4)
                if relative:
                    x2, y2 = point[0] + x2, point[1] + y2
                    x, y = point[0] + x, point[1] + y
                if last_control and last_command.upper() in ("C", "S"):
                    x1 = 2 * point[0] - last_control[0]
                    y1 = 2 * point[1] - last_control[1]
                else:
                    x1, y1 = point
            current.extend(_cubic(point, (x1, y1), (x2, y2), (x, y), steps))
            last_control = (x2, y2)
            point = (x, y)
        elif upper in ("Q", "T"):
            if upper == "Q":
                (x1, y1, x, y), i = _numbers(tokens, i, 4)
                if relative:
                    x1, y1 = point[0] + x1, point[1] + y1
                    x, y = point[0] + x, point[1] + y
            else:
                (x, y), i = _numbers(tokens, i, 2)
                if relative:
                    x, y = point[0] + x, point[1] + y
                if last_control and last_command.upper() in ("Q", "T"):
                    x1 = 2 * point[0] - last_control[0]
                    y1 = 2 * point[1] - last_control[1]
                else:
                    x1, y1 = point
            current.extend(_quadratic(point, (x1, y1), (x, y), steps))
            last_control = (x1, y1)
            point = (x, y)
        elif upper == "A":
            (rx, ry, rotation, large, sweep, x, y), i = _numbers(tokens, i, 7)
            if relative:
                x, y = point[0] + x, point[1] + y
            current.extend(
                _arc(point, rx, ry, rotation, int(large), int(sweep), (x, y), steps)
            )
            last_control = None
            point = (x, y)
        else:
            raise ValueError(f"the path command {command!r} is not one this reader knows")

        last_command = command

    if current:
        subpaths.append(current)
    return subpaths


def polylines(name: str, steps: int = 12) -> list[list[tuple[float, float]]]:
    """Every subpath of one vendored mark, in its 24-unit space."""
    if name not in ICONS:
        raise KeyError(f"{name} is not vendored here; copy its d attributes out of lucide-react first")
    out: list[list[tuple[float, float]]] = []
    for d in ICONS[name]:
        out.extend(flatten(d, steps))
    return out
