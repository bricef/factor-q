"""Small dependency-free SVG charts used by the metrics report."""

from __future__ import annotations

from html import escape
from pathlib import Path

WIDTH, HEIGHT = 900, 480
LEFT, TOP, PLOT_W, PLOT_H = 70, 45, 650, 360
COLORS = ("#6baed6", "#4292c6", "#74c476", "#41ab5d", "#238b45",
          "#006d2c", "#fb6a4a", "#cb181d", "#9e9ac8")


def _x(index: int, count: int) -> float:
    return LEFT + (PLOT_W * index / max(count - 1, 1))


def _y(value: float, maximum: float) -> float:
    return TOP + PLOT_H - PLOT_H * value / max(maximum, 1)


def _frame(title: str, maximum: float, dates: list[str]) -> list[str]:
    lines = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">',
        '<rect width="100%" height="100%" fill="white"/>',
        f'<text x="{LEFT}" y="25" font-family="sans-serif" font-size="18">{escape(title)}</text>',
        f'<path d="M {LEFT} {TOP} V {TOP + PLOT_H} H {LEFT + PLOT_W}" fill="none" stroke="#555"/>',
    ]
    for step in range(5):
        value = maximum * step / 4
        y = _y(value, maximum)
        lines.extend((
            f'<path d="M {LEFT} {y:.1f} H {LEFT + PLOT_W}" stroke="#ddd"/>',
            f'<text x="{LEFT-8}" y="{y+4:.1f}" text-anchor="end" font-family="sans-serif" font-size="11">{value:.1f}</text>',
        ))
    if dates:
        for index in sorted({0, len(dates) // 2, len(dates) - 1}):
            lines.append(f'<text x="{_x(index, len(dates)):.1f}" y="{TOP+PLOT_H+20}" text-anchor="middle" font-family="sans-serif" font-size="10">{escape(dates[index][:10])}</text>')
    return lines


def _legend(lines: list[str], names: list[str]) -> None:
    for index, name in enumerate(names):
        y = TOP + index * 24
        color = COLORS[index % len(COLORS)]
        lines.extend((
            f'<rect x="750" y="{y-11}" width="14" height="14" fill="{color}"/>',
            f'<text x="770" y="{y}" font-family="sans-serif" font-size="11">{escape(name)}</text>',
        ))


def stacked_area(path: Path, dates: list[str], series: dict[str, list[int]], title: str) -> None:
    totals = [sum(values[index] for values in series.values()) for index in range(len(dates))]
    maximum = max(totals, default=1)
    lines = _frame(title, maximum, dates)
    lower = [0.0] * len(dates)
    for number, (name, values) in enumerate(series.items()):
        upper = [a + b for a, b in zip(lower, values)]
        points = [( _x(i, len(dates)), _y(value, maximum)) for i, value in enumerate(upper)]
        points += [( _x(i, len(dates)), _y(lower[i], maximum)) for i in reversed(range(len(dates)))]
        if points:
            commands = " ".join(("M" if i == 0 else "L") + f" {x:.1f} {y:.1f}" for i, (x, y) in enumerate(points))
            lines.append(f'<path d="{commands} Z" fill="{COLORS[number % len(COLORS)]}" stroke="white" stroke-width="0.5"/>')
        lower = upper
    _legend(lines, list(series))
    lines.append("</svg>")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def line_chart(path: Path, dates: list[str], series: dict[str, list[float | int | None]], title: str) -> None:
    values = [float(value) for row in series.values() for value in row if value is not None]
    maximum = max(values, default=1)
    lines = _frame(title, maximum, dates)
    for number, (name, row) in enumerate(series.items()):
        segments: list[list[tuple[float, float]]] = []
        segment: list[tuple[float, float]] = []
        for index, value in enumerate(row):
            if value is None:
                if segment:
                    segments.append(segment)
                    segment = []
            else:
                segment.append((_x(index, len(dates)), _y(float(value), maximum)))
        if segment:
            segments.append(segment)
        for points in segments:
            commands = " ".join(("M" if i == 0 else "L") + f" {x:.1f} {y:.1f}" for i, (x, y) in enumerate(points))
            lines.append(f'<path d="{commands}" fill="none" stroke="{COLORS[number % len(COLORS)]}" stroke-width="2"/>')
            for x, y in points:
                lines.append(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="2.5" fill="{COLORS[number % len(COLORS)]}"/>')
    _legend(lines, list(series))
    lines.append("</svg>")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
