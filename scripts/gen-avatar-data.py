#!/usr/bin/env python3
r"""Generate crates/ui/src/agent_avatar_data.rs from the Grok-bot avatar assets.

Source: D:\AI-OS\references\grok-bot-avatars (avatars.json + custom-shapes.json).
The `globe` shape is skipped on purpose: its meridians need a separate
rotation animation the Rust renderer does not implement.

Run:  python scripts/gen-avatar-data.py [<assets dir>]
"""
import json
import pathlib
import sys

SRC = pathlib.Path(
    sys.argv[1] if len(sys.argv) > 1 else r"D:\AI-OS\references\grok-bot-avatars"
)
OUT = pathlib.Path(__file__).resolve().parent.parent / "crates" / "ui" / "src" / "agent_avatar_data.rs"
SKIP = {"globe"}
# The palette labels are German in the source; the UI is English.
LABELS = {
    "white": "White", "brown": "Brown", "red": "Red", "orange": "Orange",
    "yellow": "Yellow", "green": "Green", "teal": "Teal", "blue": "Blue",
    "violet": "Violet", "pink": "Pink", "gray": "Gray",
}


def q(text):
    """A Rust string literal (the data is plain ASCII)."""
    return json.dumps(text)


def f(value):
    text = repr(float(value))
    return text if ("." in text or "e" in text) else text + ".0"


def eye(e):
    return (
        "EyeRect { x: {}, y: {}, w: {}, h: {}, r: {}, rot: {}, cx: {}, cy: {} }".replace("{}", "%s")
        % tuple(f(e[k]) for k in ("x", "y", "w", "h", "r", "rot", "cx", "cy"))
    )


def main():
    base = json.loads((SRC / "avatars.json").read_text(encoding="utf-8"))
    extra = json.loads((SRC / "custom-shapes.json").read_text(encoding="utf-8"))
    order = [k for k in base["shapeOrder"] + extra["shapeOrder"] if k not in SKIP]
    shapes = dict(base["shapes"])
    shapes.update(extra["shapes"])

    lines = [
        "//! Generated avatar geometry - do not edit by hand.",
        "//!",
        "//! Source: `D:\\AI-OS\\references\\grok-bot-avatars` (`avatars.json` and",
        "//! `custom-shapes.json`, the procedurally extracted Grok-bot mascots).",
        "//! Generator: `scripts/gen-avatar-data.py`. The `globe` shape is left out:",
        "//! its rotating meridians are not implemented in the Rust renderer.",
        "",
        "#[allow(unused_imports)]",
        "use super::agent_avatar::{AvatarColor, AvatarPart, AvatarShape, EyeRect};",
        "",
        "/// The source viewBox: `-15 -15 259 259`.",
        "pub const VIEW_MIN: f32 = %s;" % f(base["viewBox"].split()[0]),
        "pub const VIEW_SIZE: f32 = %s;" % f(base["viewBox"].split()[2]),
        "/// Centre of the figure in viewBox units; the `scale` pivots here.",
        "pub const CENTER: f32 = %s;" % f(base["center"]),
        "",
        "pub const SHAPES: &[AvatarShape] = &[",
    ]
    for key in order:
        s = shapes[key]
        parts = s.get("parts") or []
        parts_src = "&[]"
        if parts:
            items = ", ".join(
                "AvatarPart { d: %s, opacity: %s, even_odd: %s }"
                % (q(p["d"]), f(p.get("opacity", 1.0)), "true" if p.get("fillRule") == "evenodd" else "false")
                for p in parts
            )
            parts_src = "&[%s]" % items
        lines.append("    AvatarShape {")
        lines.append("        key: %s," % q(key))
        lines.append("        label: %s," % q(s.get("label", key.capitalize())))
        lines.append("        scale: %s," % f(s["scale"]))
        lines.append("        even_odd: %s," % ("true" if s.get("fillRule") == "evenodd" else "false"))
        lines.append("        d: %s," % q(s["d"]))
        lines.append("        eyes: [%s, %s]," % (eye(s["eyes"][0]), eye(s["eyes"][1])))
        lines.append("        parts: %s," % parts_src)
        lines.append("    },")
    lines += ["];", "", "pub const PALETTE: &[AvatarColor] = &["]
    for c in base["palette"]:
        rgb = int(c["value"].lstrip("#"), 16)
        lines.append(
            "    AvatarColor { id: %s, label: %s, rgb: 0x%06X },"
            % (q(c["id"]), q(LABELS.get(c["id"], c["label"])), rgb)
        )
    lines += ["];", ""]
    OUT.write_text("\n".join(lines), encoding="utf-8")
    print("wrote %s (%d shapes, %d colours)" % (OUT, len(order), len(base["palette"])))


if __name__ == "__main__":
    main()
