"""Short-lived, asynchronously drained CLI streams for GTK."""
from gi.repository import Gio, GLib

import backend


class Stream:
    """Read and reap a single CLI child on the GLib main context."""

    def __init__(self, argv, line, done):
        flags = Gio.SubprocessFlags
        self.process = Gio.Subprocess.new(argv, flags.STDIN_PIPE | flags.STDOUT_PIPE | flags.STDERR_MERGE)
        self.output = Gio.DataInputStream.new(self.process.get_stdout_pipe())
        self.on_line, self.on_done = line, done
        self.canceled = False
        self.finished = False
        self.output.read_line_async(GLib.PRIORITY_DEFAULT, None, self._read)

    def _read(self, stream, result):
        try:
            line, _ = stream.read_line_finish_utf8(result)
        except GLib.Error:
            self.cancel()
            line = None
        if line is None:
            self.process.wait_async(None, self._finished)
        else:
            if not self.canceled:
                self.on_line(line)
            stream.read_line_async(GLib.PRIORITY_DEFAULT, None, self._read)

    def _finished(self, process, result):
        process.wait_finish(result)
        self.finished = True
        success = process.get_successful()
        self.on_done(success, self.canceled)

    def stop_input(self):
        """Close the capture stop pipe so Rust returns its balanced draft."""
        self.process.get_stdin_pipe().close(None)

    def start_input(self):
        """Arm capture only after the operator presses Start."""
        self.process.get_stdin_pipe().write_all(b"START\n", None)

    def cancel(self):
        """Terminate a read-only stream and discard its pending result."""
        if not self.canceled and not self.finished:
            self.canceled = True
            try:
                self.stop_input()
            except GLib.Error:
                pass
            self.process.force_exit()


def start(arguments, target, line, done):
    """Stream from the same project CLI used for ordinary actions."""
    return Stream(backend.command_line(arguments, target), line, done)
