"""Native event list, draft editing, and bounded input recording."""
from dataclasses import dataclass, field
from pathlib import Path
from tempfile import TemporaryDirectory

from gi.repository import GLib, Gtk

import backend
import streams
import workflows


@dataclass
class Draft:
    """Editable projection of the CLI's native macro format."""
    mode: str = "times"
    repeat: int = 1
    metadata: str = ""
    events: list = field(default_factory=list)

    @classmethod
    def parse(cls, text):
        draft = cls()
        for line in text.splitlines():
            match line.split():
                case ["mode", mode]:
                    draft.mode = mode
                case ["repeat", repeat]:
                    draft.repeat = int(repeat)
                case ["metadata", value]:
                    draft.metadata = value
                case ["extended", delay, operands]:
                    draft.events.append(("extended", operands, int(delay)))
                case [kind, value, delay] if kind in ("up", "down", "tap"):
                    draft.events.append((kind, value, int(delay)))
                case []:
                    pass
                case _:
                    raise ValueError("The macro contains an unrecognized event.")
        return draft

    def encode(self):
        lines = [f"mode {self.mode}", f"repeat {self.repeat}"]
        if self.metadata:
            lines.append(f"metadata {self.metadata}")
        for kind, value, delay in self.events:
            lines.append(f"extended {delay} {value}" if kind == "extended" else f"{kind} {value} {delay}")
        return "\n".join(lines) + "\n"


def event_value(kind, value):
    """Display human-readable key labels and signed movement coordinates."""
    if kind == "extended":
        if value == "unavailable":
            return "Unavailable"
        data = bytes.fromhex(value)
        return ", ".join(str(int.from_bytes(data[i:i + 2], "big", signed=True)) for i in (0, 2))
    return workflows.key_label(value)


def save(name, draft, replace):
    """Persist through the CLI's validated importer or replacement handler."""
    with TemporaryDirectory(prefix=".macro-draft-", dir=backend.ROOT) as folder:
        file = Path(folder) / "macro.txt"
        file.write_text(draft.encode(), encoding="utf-8")
        backend.run(["macro", "update" if replace else "import", name, str(file)])


class Editor(Gtk.Window):
    """Edit and record locally, then explicitly commit a validated draft."""

    MODES = ("times", "press-to-stop", "hold")
    KINDS = ("down", "up", "tap", "extended")
    EVENT_LABELS = ("Press key", "Release key", "Tap key", "Move pointer")

    def __init__(self, parent, name, text, replace):
        super().__init__(transient_for=parent, modal=True, title="Edit Macro" if replace else "New Macro",
                         default_width=610, default_height=560)
        self.parent, self.replace, self.draft = parent, replace, Draft.parse(text)
        self.original = name, self.draft.encode()
        self.discard = False
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12, margin_top=18,
                      margin_bottom=18, margin_start=20, margin_end=20)
        self.set_child(box)
        self.name = Gtk.Entry(placeholder_text="Macro name", text=name, editable=not replace, max_length=64)
        self.name.update_property([Gtk.AccessibleProperty.LABEL], ["Macro name"])
        box.append(self.name)
        options = Gtk.Box(spacing=10)
        self.mode = Gtk.DropDown.new_from_strings(["Repeat a set number of times", "Press again to stop", "While held"])
        self.mode.update_property([Gtk.AccessibleProperty.LABEL], ["When the macro plays"])
        self.mode.set_selected(self.MODES.index(self.draft.mode))
        self.repeat = Gtk.SpinButton.new_with_range(1, 255, 1)
        self.repeat.update_property([Gtk.AccessibleProperty.LABEL], ["Number of repeats"])
        self.repeat.set_value(self.draft.repeat)
        options.append(self.mode)
        repeat_label = Gtk.Label(label="Times")
        options.append(repeat_label)
        options.append(self.repeat)
        def update_repeat(*_):
            show = self.mode.get_selected() == 0
            repeat_label.set_visible(show)
            self.repeat.set_visible(show)
        self.mode.connect("notify::selected", update_repeat)
        update_repeat()
        box.append(options)
        scroll = Gtk.ScrolledWindow(vexpand=True, min_content_height=160)
        self.events = Gtk.ListBox(selection_mode=Gtk.SelectionMode.SINGLE)
        self.events.add_css_class("boxed-list")
        self.events.set_placeholder(Gtk.Label(label="Add an action or record a sequence."))
        self.events.connect("row-selected", self.selected)
        scroll.set_child(self.events)
        box.append(scroll)
        controls = Gtk.Box(spacing=8)
        self.kind = Gtk.DropDown.new_from_strings(list(self.EVENT_LABELS))
        self.value = Gtk.Entry(placeholder_text="Key name, e.g. A", width_chars=13)
        self.kind.update_property([Gtk.AccessibleProperty.LABEL], ["Action"])
        self.value.update_property([Gtk.AccessibleProperty.LABEL], ["Key or distance to move"])
        self.previous_key = None
        self.kind.connect("notify::selected", lambda *_: self.value.set_placeholder_text(
            "Across, down, e.g. 12, −3" if self.kind.get_selected() == 3 else "Key name, e.g. A"))
        self.delay = Gtk.SpinButton.new_with_range(0, 50000, 1)
        self.delay.update_property([Gtk.AccessibleProperty.LABEL], ["Wait in milliseconds"])
        self.delay.set_value(10)
        controls.append(self.kind)
        controls.append(self.value)
        controls.append(Gtk.Label(label="Wait (ms)"))
        controls.append(self.delay)
        box.append(controls)
        buttons = Gtk.Box(spacing=8)
        self.selection_buttons = []
        for name, handler in (("Add", self.add), ("Replace", self.replace_event), ("Remove", self.remove),
                              ("Move up", lambda *_: self.move(-1)), ("Move down", lambda *_: self.move(1))):
            control = Gtk.Button(label=name)
            control.connect("clicked", handler)
            if name != "Add":
                self.selection_buttons.append(control)
            buttons.append(control)
        box.append(buttons)
        self.message = Gtk.Label(xalign=0, wrap=True)
        box.append(self.message)
        footer = Gtk.Box(spacing=8, halign=Gtk.Align.END)
        record = Gtk.Button(label="Record…")
        record.connect("clicked", self.open_recorder)
        footer.append(record)
        commit = Gtk.Button(label="Save Macro")
        commit.add_css_class("suggested-action")
        commit.connect("clicked", self.commit)
        footer.append(commit)
        box.append(footer)
        self.connect("close-request", self.close_editor)
        self.redraw()

    def redraw(self, focus=None):
        while child := self.events.get_first_child():
            self.events.remove(child)
        for kind, value, delay in self.draft.events:
            title = self.EVENT_LABELS[self.KINDS.index(kind)]
            row = Gtk.Label(label=f"{title}   {event_value(kind, value)}   ·   {delay} ms", xalign=0,
                            margin_start=10, margin_top=6, margin_bottom=6)
            self.events.append(row)
        if focus is not None:
            self.events.select_row(self.events.get_row_at_index(focus))
        self.selected(self.events, self.events.get_selected_row())

    def selected(self, _, row):
        for control in self.selection_buttons:
            control.set_sensitive(row is not None)
        self.previous_key = None
        if row:
            kind, value, delay = self.draft.events[row.get_index()]
            self.kind.set_selected(self.KINDS.index(kind))
            if kind == "extended" and value != "unavailable":
                value = event_value(kind, value)
            elif value.startswith("0x"):
                display = event_value(kind, value)
                self.previous_key = (display, value)
                value = display
            self.value.set_text(value)
            self.delay.set_value(delay)

    def event(self):
        value = self.value.get_text().strip()
        kind = self.KINDS[self.kind.get_selected()]
        if self.previous_key and kind != "extended" and value == self.previous_key[0]:
            value = self.previous_key[1]
        if kind == "extended" and value != "unavailable":
            try:
                x, y = value.split(",")
                value = (int(x).to_bytes(2, "big", signed=True) + int(y).to_bytes(2, "big", signed=True)).hex()
            except (ValueError, OverflowError):
                self.message.set_label("Enter how far to move across and down, such as 12, -3.")
                return None
        if not value or any(c.isspace() for c in value):
            self.message.set_label("Enter a key name or a distance to move.")
            return None
        return kind, value, self.delay.get_value_as_int()

    def add(self, *_):
        event = self.event()
        if event:
            row = self.events.get_selected_row()
            index = row.get_index() + 1 if row else len(self.draft.events)
            self.draft.events.insert(index, event)
            self.redraw(index)

    def replace_event(self, *_):
        row, event = self.events.get_selected_row(), self.event()
        if row and event:
            index = row.get_index()
            self.draft.events[index] = event
            self.redraw(index)

    def remove(self, *_):
        if row := self.events.get_selected_row():
            index = row.get_index()
            self.draft.events.pop(index)
            self.redraw(min(index, len(self.draft.events) - 1) if self.draft.events else None)

    def move(self, delta):
        if row := self.events.get_selected_row():
            index = row.get_index()
            destination = index + delta
            if 0 <= destination < len(self.draft.events):
                self.draft.events.insert(destination, self.draft.events.pop(index))
                self.redraw(destination)

    def commit(self, *_):
        name = self.name.get_text().strip()
        if not name:
            self.message.set_label("Give the macro a name.")
            return
        if not workflows.valid_library_name(name):
            self.message.set_label(workflows.NAME_HINT)
            return
        self.draft.mode = self.MODES[self.mode.get_selected()]
        self.draft.repeat = self.repeat.get_value_as_int()
        self.set_sensitive(False)
        future = self.parent.pool.submit(save, name, self.draft, self.replace)

        def done():
            self.set_sensitive(True)
            try:
                future.result()
            except Exception as error:
                self.message.set_label(backend.describe(error))
            else:
                self.parent.message("Macro saved.", transient=True)
                if self.parent.library_reload:
                    self.parent.library_reload()
                self.discard = True
                self.close()
            return False

        future.add_done_callback(lambda _: GLib.idle_add(done))

    def open_recorder(self, *_):
        Recorder(self).present()

    def close_editor(self, *_):
        if not self.get_sensitive():
            return True
        self.draft.mode = self.MODES[self.mode.get_selected()]
        self.draft.repeat = self.repeat.get_value_as_int()
        if self.discard or (self.name.get_text(), self.draft.encode()) == self.original:
            return False
        dialog = Gtk.AlertDialog(message="Discard changes to this macro?", buttons=["Keep Editing", "Discard"],
                                 default_button=0, cancel_button=0)

        def chosen(obj, result):
            try:
                self.discard = obj.choose_finish(result) == 1
            except GLib.Error:
                return
            if self.discard:
                self.close()

        dialog.choose(self, None, chosen)
        return True


class Recorder(Gtk.Window):
    """Choose input sources explicitly and return an unsaved capture."""

    def __init__(self, editor):
        super().__init__(transient_for=editor, modal=True, title="Record a macro",
                         default_width=440, default_height=440)
        self.editor, self.stream = editor, None
        self.lines, self.devices = [], []
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10, margin_top=18,
                      margin_bottom=18, margin_start=20, margin_end=20)
        self.set_child(box)
        box.append(Gtk.Label(label="Choose a mouse or keyboard to record.", xalign=0))
        scroll = Gtk.ScrolledWindow(vexpand=True, min_content_height=120)
        self.choices = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=5)
        scroll.set_child(self.choices)
        box.append(scroll)
        self.status = Gtk.Label(label="Recording stops after one minute.", xalign=0, wrap=True)
        box.append(self.status)
        buttons = Gtk.Box(spacing=8, halign=Gtk.Align.END)
        self.start = Gtk.Button(label="Start Recording", sensitive=False)
        self.start.add_css_class("suggested-action")
        self.stop = Gtk.Button(label="Stop", sensitive=False)
        self.access = Gtk.Button(label="Allow Recording…", visible=False)
        self.access.connect("clicked", lambda *_: self.record(elevated=True))
        self.armed = False
        self.elevated = False
        self.start_when_ready = False
        cancel = Gtk.Button(label="Cancel")
        self.start.connect("clicked", self.record)
        self.stop.connect("clicked", self.stop_recording)
        cancel.connect("clicked", lambda *_: self.close())
        for control in (cancel, self.access, self.stop, self.start):
            buttons.append(control)
        box.append(buttons)
        self.connect("close-request", self.cancel)
        self.load_devices()

    def load_devices(self):
        future = self.editor.parent.pool.submit(backend.run, ["desktop-inputs"])

        def display():
            try:
                version, *devices = future.result().splitlines()
                if version != "r1-inputs\t1":
                    raise ValueError("Unsupported input list")
                for item in devices:
                    path, name = item.split("\t", 1)
                    if not name or any(kind in name.lower() for kind in (
                        "power button", "sleep button", "lid switch", "pc speaker", "hd-audio",
                        "video bus", "system control", "consumer control", "wmi hotkeys",
                    )):
                        continue
                    button = Gtk.CheckButton(label=name)
                    self.choices.append(button)
                    self.devices.append((path, button))
                    button.connect("toggled", self.selection_changed)
                if not self.devices:
                    self.status.set_label("No devices are available to record.")
            except Exception as error:
                self.status.set_label(backend.describe(error))
            return False

        future.add_done_callback(lambda _: GLib.idle_add(display))

    def selection_changed(self, *_):
        selected = sum(button.get_active() for _, button in self.devices)
        self.start.set_sensitive(1 <= selected <= 8)
        if selected > 8:
            self.status.set_label("Choose up to eight devices.")
        elif not self.access.get_visible():
            self.status.set_label("Recording stops after one minute.")

    def record(self, *_, elevated=False):
        if self.stream:
            if self.armed:
                try:
                    self.stream.start_input()
                except GLib.Error:
                    self.status.set_label("Recording could not be started.")
                    self.stream.cancel()
                    return
                self.armed = False
                self.start.set_sensitive(False)
                self.stop.set_sensitive(True)
                self.status.set_label("Recording…")
            return
        nodes = [path for path, button in self.devices if button.get_active()]
        if not 1 <= len(nodes) <= 8:
            self.status.set_label("Choose one to eight input devices.")
            return
        args = ["desktop-record", "500", "60000", "recorded", *nodes]
        self.lines.clear()
        session = backend.session_ready()
        elevated = elevated or session
        self.start_when_ready = session or not elevated
        self.elevated = elevated
        self.access.set_visible(False)
        try:
            self.stream = streams.Stream(backend.command_line(args, elevated=elevated), self.received, self.finished)
        except (GLib.Error, backend.CommandError) as error:
            self.status.set_label(backend.describe(error))
            return
        self.start.set_sensitive(False)
        self.choices.set_sensitive(False)
        self.status.set_label("Getting ready…")

    def received(self, line):
        if line == "r1-record\t1":
            self.armed = True
            if self.start_when_ready:
                self.record()
            else:
                self.start.set_sensitive(True)
                self.status.set_label("Ready. Choose Start Recording to begin.")
        else:
            self.lines.append(line)

    def stop_recording(self, *_):
        if self.stream:
            self.stop.set_sensitive(False)
            try:
                self.stream.stop_input()
            except GLib.Error:
                pass

    def finished(self, success, canceled):
        self.stream = None
        if canceled:
            return
        if success:
            capture = Draft.parse("\n".join(self.lines))
            self.editor.draft.events.extend(capture.events)
            self.editor.redraw()
            self.editor.message.set_label("Recording added. Review it before saving.")
            self.close()
        else:
            self.armed = False
            self.start.set_sensitive(True)
            self.stop.set_sensitive(False)
            self.choices.set_sensitive(True)
            text = "\n".join(self.lines)
            denied = "Permission denied" in text
            self.access.set_visible(denied and not self.elevated)
            self.status.set_label("Allow access to record from this device." if denied else
                                  "Nothing was recorded. Try again.")

    def cancel(self, *_):
        if self.stream:
            self.stream.cancel()
        return False
