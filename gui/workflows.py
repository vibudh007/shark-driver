"""Presentation labels for device actions and activity."""

import re

BUTTON_NAMES = {
    "disabled": "Do nothing",
    "left-click": "Primary click",
    "right-click": "Secondary click",
    "middle-click": "Middle click",
    "dpi-cycle": "Cycle sensitivity",
    "dpi-up": "Increase sensitivity",
    "dpi-down": "Decrease sensitivity",
    "easy-aim": "Slow the pointer",
    "fire": "Rapid click",
    "files": "Open files",
    "led-loop": "Cycle lighting effects",
    "media-stop": "Stop playback",
    "media-player": "Open music player",
    "play-pause": "Play or pause",
    "ctrl-equals": "Ctrl+=",
    "ctrl-minus": "Ctrl+−",
}

LIGHT_NAMES = {
    "off": "Off", "static": "Steady color", "breathing": "Fade in and out",
    "neon": "Cycle colors", "color-breathing": "Fade between colors",
    "static-dpi": "Sensitivity color", "breathing-dpi": "Pulsing sensitivity color",
}


def action_label(action):
    """Describe an existing mapping without exposing device codes."""
    if action in BUTTON_NAMES:
        return BUTTON_NAMES[action]
    if action.startswith("raw:"):
        return "Existing assignment"
    if action.startswith("macro:"):
        return "Play a macro"
    if action.startswith("key:"):
        return "+".join(key_label(key) for key in action[4:].split("+"))
    return action.replace("-", " ").capitalize()


def key_label(value):
    """Name common keys and mouse buttons without showing their device code."""
    if not value.startswith("0x"):
        return "Super" if value == "super" else value.upper() if len(value) == 1 else value.capitalize()
    key = int(value, 16)
    if 4 <= key <= 29:
        return chr(ord("A") + key - 4)
    if 30 <= key <= 39:
        return "1234567890"[key - 30]
    if 58 <= key <= 69:
        return f"F{key - 57}"
    return {40: "Enter", 41: "Escape", 42: "Backspace", 43: "Tab", 44: "Space", 45: "−", 46: "=",
            73: "Insert", 74: "Home", 75: "Page Up", 76: "Delete", 77: "End", 78: "Page Down",
            79: "Right Arrow", 80: "Left Arrow", 81: "Down Arrow", 82: "Up Arrow",
            0xE0: "Left Ctrl", 0xE1: "Left Shift", 0xE2: "Left Alt", 0xE3: "Left Super",
            0xE4: "Right Ctrl", 0xE5: "Right Shift", 0xE6: "Right Alt", 0xE7: "Right Super",
            0xF1: "Left mouse button", 0xF2: "Right mouse button", 0xF3: "Middle mouse button",
            0xF4: "Forward button", 0xF5: "Back button"}.get(key, "Other key")


def battery_label(record, wired=False):
    """Present only recognized battery values and charging information."""
    match record.split("\t"):
        case ["battery-percent", value] if value.isdigit() and int(value) <= 100:
            return f"{value}%"
        case ["battery", _, "2"] if wired:
            return "100% · Fully charged"
        case ["battery", value, state] if value.isdigit() and int(value) <= 10:
            return f"{int(value) * 10}%" + (" · Charging" if wired or state == "3" else "")
        case _:
            return None


def shortcut_value(key_name, modifiers):
    """Translate a captured GTK key to the CLI's accepted vocabulary."""
    name = key_name.lower().replace("_", "-")
    names = {"return": "enter", "backspace": "backspace", "escape": "escape", "iso-left-tab": "tab"}
    name = names.get(name, name)
    simple = len(name) == 1 and name.isascii() and name.isalnum()
    function = name.startswith("f") and name[1:].isdigit() and 1 <= int(name[1:]) <= 24
    if simple or function or name in {
        "enter", "escape", "backspace", "space", "tab", "delete", "insert", "home", "end",
        "left", "right", "up", "down", "page-up", "page-down", "minus", "equal",
        "comma", "period", "slash", "semicolon", "apostrophe", "backslash",
    }:
        return "key:" + "+".join([*modifiers, name])
    return None


def valid_library_name(name):
    """Validate names against the CLI library's portable filename rules."""
    return re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}", name) is not None


NAME_HINT = "Start with a letter or number. Use letters, numbers, dashes, underscores, or dots."


def windows_name(text):
    """Read the CLI's escaped Windows display name without restoring control characters."""
    def decoded(match):
        if match[1]:
            char = chr(int(match[1], 16))
            return char if char.isprintable() else " "
        return match[2] if match[2] in ("\\", "'", '"') else " "

    return re.sub(r"\\(?:u\{([a-f0-9]{1,6})\}|(.))", decoded, text).strip()
