"""Shark Driver GTK4 desktop entry point."""
import os
import sys

# A GTK application must run in the logged-in user's graphical session. The
# privileged device helper is launched separately through Polkit.
if __name__ == "__main__" and os.geteuid() == 0:
    print("Run Shark Driver as your desktop user, without sudo. Mouse access is authorized separately.",
          file=sys.stderr, flush=True)
    raise SystemExit(1)

import argparse
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from subprocess import TimeoutExpired
from threading import Event

import gi
gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, Gio, GLib, Gtk

import backend
import macro_editor
import streams
import workflows

APP_NAME = "Shark Driver"
APP_ID = "local.sharkdriver.SharkDriver"

CSS = """
.sidebar { padding: 14px 10px; background-color: alpha(currentColor, 0.035);
           border-right: 1px solid alpha(currentColor, 0.10); }
.sidebar row { min-height: 40px; border-radius: 9px; padding: 0 12px; }
.sidebar row label { font-weight: 500; }
.content { padding: 32px 34px; }
.setting { min-height: 49px; padding: 6px 14px; }
.setting + .setting { border-top: 1px solid alpha(currentColor, 0.08); }
.card { border: 1px solid alpha(currentColor, 0.12); border-radius: 12px;
        background-color: alpha(currentColor, 0.022); }
.section { margin-top: 10px; }
.device-card { padding: 20px; }
.device-card image { opacity: 0.82; }
.empty-state { padding: 24px 12px; }
.action-strip { padding: 10px 16px; }
.toast { padding: 8px 14px; border-radius: 12px; background-color: alpha(currentColor, 0.06); }
.app-menu { padding: 6px; }
"""


def label(text="", style=None):
    widget = Gtk.Label(label=text, xalign=0, wrap=True)
    if style:
        widget.add_css_class(style)
    return widget


def vbox(spacing=10):
    return Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=spacing)


def clear(box):
    while child := box.get_first_child():
        box.remove(child)


def button(text, callback, style=None):
    widget = Gtk.Button(label=text)
    if style:
        widget.add_css_class(style)
    widget.connect("clicked", callback)
    return widget


def choice(items, selected=0):
    widget = Gtk.DropDown.new_from_strings(list(items))
    widget.set_selected(selected)
    return widget


def color_control(value):
    control = Gtk.ColorDialogButton(dialog=Gtk.ColorDialog(with_alpha=False))
    color = Gdk.RGBA()
    color.parse("#" + value)
    control.set_rgba(color)
    return control


def color_value(widget):
    color = widget.get_rgba()
    return "".join(f"{round(channel * 255):02x}" for channel in (color.red, color.green, color.blue))


def configure_branding(display):
    if display is None:
        return False
    theme = Gtk.IconTheme.get_for_display(display)
    theme.add_search_path(str(Path(__file__).with_name("icons")))
    Gtk.Window.set_default_icon_name(APP_ID)
    return theme.has_icon(APP_ID)


def apply_portal_color_scheme(settings, value, fallback_dark):
    """Apply the host preference, or keep Adwaita Dark when no theme exists."""
    if value == 0:
        if fallback_dark:
            settings.set_property("gtk-application-prefer-dark-theme", True)
        else:
            settings.reset_property("gtk-application-prefer-dark-theme")
    elif value in (1, 2):
        settings.set_property("gtk-application-prefer-dark-theme", value == 1)


def portal_string_value(value):
    """Unwrap a portal variant and return its string payload, if valid."""
    for _ in range(4):
        if value.get_type_string() != "v":
            break
        value = value.get_variant()
    if value.get_type_string() != "s":
        return None
    return value.get_string()


def portal_color_scheme_value(value):
    """Unwrap the portal's nested variants and return a valid uint32 scheme."""
    for _ in range(4):
        if value.get_type_string() != "v":
            break
        value = value.get_variant()
    if value.get_type_string() != "u":
        return None
    return value.get_uint32()


def gtk4_theme_available(theme_name):
    """Check runtime and read-only host data roots for a GTK4 theme sheet."""
    if not theme_name or theme_name in (".", "..") or Path(theme_name).name != theme_name:
        return False
    data_roots = [Path(path) for path in GLib.get_system_data_dirs()]
    host_user_data = os.environ.get("HOST_XDG_DATA_HOME")
    if host_user_data:
        data_roots.append(Path(host_user_data))
    data_roots.extend((Path("/run/host/usr/local/share"), Path("/run/host/usr/share"),
                       Path("/run/host/user-share")))
    theme_dirs = (root / "themes" / theme_name / "gtk-4.0" for root in data_roots)
    for theme_dir in theme_dirs:
        sheets = [theme_dir / name for name in ("gtk.css", "gtk-dark.css")
                  if (theme_dir / name).is_file()]
        if sheets and all(gtk_theme_css_parses(sheet) for sheet in sheets):
            return True
    return False


def gtk_theme_css_parses(path):
    """Reject a theme stylesheet that the bundled GTK cannot parse."""
    provider = Gtk.CssProvider()
    errors = []
    provider.connect("parsing-error", lambda _provider, _section, error: errors.append(error))
    try:
        provider.load_from_path(str(path))
    except GLib.Error:
        return False
    return not errors


def apply_host_gtk_theme(settings, theme_name):
    """Load the host's GTK4 theme when its stylesheet is visible in the sandbox."""
    if not gtk4_theme_available(theme_name):
        settings.set_property("gtk-theme-name", "Adwaita")
        settings.set_property("gtk-application-prefer-dark-theme", True)
        return False
    settings.set_property("gtk-theme-name", theme_name)
    return True


def portal_color_scheme_changed(_connection, _sender, _path, _interface, _signal, parameters, application):
    """Apply host theme and color changes emitted by the Settings portal."""
    try:
        namespace = parameters.get_child_value(0).get_string()
        key = parameters.get_child_value(1).get_string()
        variant = parameters.get_child_value(2)
        if namespace == "org.freedesktop.appearance" and key == "color-scheme":
            value = portal_color_scheme_value(variant)
            if value in (0, 1, 2):
                application.portal_color_scheme = value
                apply_portal_color_scheme(application.theme_settings, value,
                                          not application.host_theme_available)
        elif namespace == "org.gnome.desktop.interface" and key == "gtk-theme":
            theme_name = portal_string_value(variant)
            if theme_name:
                application.host_theme_available = apply_host_gtk_theme(application.theme_settings, theme_name)
                if application.portal_color_scheme is not None:
                    apply_portal_color_scheme(application.theme_settings, application.portal_color_scheme,
                                              not application.host_theme_available)
    except (GLib.Error, AttributeError, TypeError):
        return


def read_portal_setting(connection, result, request):
    """Read the current host preference, tolerating desktops without this portal."""
    application, setting = request
    try:
        reply = connection.call_finish(result)
        variant = reply.get_child_value(0)
        value = portal_color_scheme_value(variant) if setting == "color-scheme" else portal_string_value(variant)
    except (GLib.Error, AttributeError, TypeError):
        return
    if setting == "color-scheme" and value in (0, 1, 2):
        application.portal_color_scheme = value
        apply_portal_color_scheme(application.theme_settings, value,
                                  not application.host_theme_available)
    elif setting == "gtk-theme" and value:
        application.host_theme_available = apply_host_gtk_theme(application.theme_settings, value)
        if application.portal_color_scheme is not None:
            apply_portal_color_scheme(application.theme_settings, application.portal_color_scheme,
                                      not application.host_theme_available)


def follow_flatpak_appearance(application, display):
    """Subscribe to the host light/dark preference when running in Flatpak."""
    if not backend.FLATPAK:
        return
    try:
        connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    except GLib.Error:
        return
    settings = Gtk.Settings.get_for_display(display)
    application.theme_settings = settings
    application.portal_color_scheme = None
    application.host_theme_available = apply_host_gtk_theme(
        settings, settings.get_property("gtk-theme-name"))
    application.theme_bus = connection
    application.theme_subscription = connection.signal_subscribe(
        "org.freedesktop.portal.Desktop", "org.freedesktop.portal.Settings",
        "SettingChanged", "/org/freedesktop/portal/desktop", None,
        Gio.DBusSignalFlags.NONE, portal_color_scheme_changed, application,
    )
    for namespace, key, setting in (
        ("org.freedesktop.appearance", "color-scheme", "color-scheme"),
        ("org.gnome.desktop.interface", "gtk-theme", "gtk-theme"),
    ):
        connection.call(
            "org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings", "Read",
            GLib.Variant("(ss)", (namespace, key)),
            GLib.VariantType.new("(v)"), Gio.DBusCallFlags.NONE, 1000, None,
            read_portal_setting, (application, setting),
        )


class Window(Gtk.ApplicationWindow):
    """Build the desktop window and coordinate one CLI operation at a time."""

    PAGES = ("Overview", "Pointer", "Buttons", "Lighting", "Power", "Profiles", "Macros")
    PAGE_ICONS = (APP_ID, "go-jump-symbolic", "input-gaming-symbolic",
                  "display-brightness-symbolic", "battery-symbolic", "document-open-recent-symbolic",
                  "media-record-symbolic")

    def __init__(self, app, smoke_test=False):
        super().__init__(application=app, title=APP_NAME, icon_name=APP_ID, default_width=900, default_height=660)
        self.smoke_test = smoke_test
        self.pool = ThreadPoolExecutor(max_workers=1, thread_name_prefix="r1-cli")
        self.closed = Event()
        self.busy = False
        self.dirty = False
        self.controls = []
        self.snapshot = None
        self.snapshot_text = ""
        self.targets = []
        self.manual_target = ()
        self.discovery_pending = False
        self.discovery_timer = None
        self.action_names = ()
        self.active_page = "Overview"
        self.notice_timer = 0
        self.access_setup_notice_shown = False
        self.library_reload = None
        self.elevated_target = ()
        self.connect("close-request", self.close_window)

        self.picker = choice(())
        self.picker.connect("notify::selected", self.select_device)
        self.header = Gtk.HeaderBar()
        heading = Gtk.Box(spacing=7)
        mark = Gtk.Image.new_from_icon_name(APP_ID)
        mark.set_pixel_size(24)
        heading.append(mark)
        title = label(APP_NAME, "heading")
        title.set_wrap(False)
        heading.append(title)
        self.header.set_title_widget(heading)
        self.header.pack_start(self.picker)
        self.picker.set_tooltip_text("Mouse connection")
        refresh = Gtk.Button(icon_name="view-refresh-symbolic", tooltip_text="Refresh connections")
        refresh.connect("clicked", lambda *_: self.discover(force=True))
        self.header.pack_end(self.app_menu())
        self.header.pack_end(refresh)
        self.set_titlebar(self.header)

        layout = Gtk.Box()
        self.set_child(layout)
        self.nav = Gtk.ListBox(selection_mode=Gtk.SelectionMode.SINGLE)
        self.nav.add_css_class("sidebar")
        self.nav.set_size_request(154, -1)
        self.nav.connect("row-selected", self.select_page)
        layout.append(self.nav)
        for index, (page, icon) in enumerate(zip(self.PAGES, self.PAGE_ICONS)):
            row = Gtk.ListBoxRow()
            contents = Gtk.Box(spacing=10)
            contents.append(Gtk.Image.new_from_icon_name(icon))
            contents.append(label(page))
            row.set_child(contents)
            if index == 5:
                row.set_margin_top(12)
            self.nav.append(row)

        main = vbox(0)
        main.set_hexpand(True)
        layout.append(main)
        self.compact_nav = choice(self.PAGES)
        self.compact_nav.set_visible(False)
        self.compact_nav.set_halign(Gtk.Align.START)
        self.compact_nav.add_css_class("action-strip")
        self.compact_nav.connect("notify::selected", self.select_compact_page)
        main.append(self.compact_nav)
        scroll = Gtk.ScrolledWindow(hscrollbar_policy=Gtk.PolicyType.NEVER)
        scroll.set_vexpand(True)
        main.append(scroll)
        self.body = vbox(16)
        self.body.set_halign(Gtk.Align.CENTER)
        self.body.set_size_request(550, -1)
        self.body.add_css_class("content")
        scroll.set_child(self.body)
        self.notice = label()
        self.notice.set_halign(Gtk.Align.CENTER)
        self.notice.set_margin_bottom(10)
        self.notice.add_css_class("toast")
        self.notice.set_visible(False)
        main.append(self.notice)
        self.access_button = button("Allow access…", lambda *_: self.authorize(), "suggested-action")
        self.access_button.set_halign(Gtk.Align.END)
        self.access_button.set_visible(False)
        main.append(self.access_button)
        bar = Gtk.Box(spacing=10, halign=Gtk.Align.END)
        bar.add_css_class("action-strip")
        bar.append(button("Discard", lambda *_: self.render()))
        bar.append(button("Apply", self.apply, "suggested-action"))
        self.revealer = Gtk.Revealer(transition_type=Gtk.RevealerTransitionType.SLIDE_UP, transition_duration=140)
        self.revealer.set_child(bar)
        main.append(self.revealer)
        self.nav.select_row(self.nav.get_row_at_index(0))
        self.connect("realize", self.watch_layout)
        if smoke_test:
            self.submit(lambda: backend.capture(("--simulate", "wired")), self.loaded)
        else:
            if backend.session_supported():
                self.submit(backend.start_session, lambda _: self.discover(force=True))
            else:
                self.discover()
            self.discovery_timer = GLib.timeout_add_seconds(2, self.discover)

    def watch_layout(self, *_):
        surface = self.get_surface()
        surface.connect("layout", self.resize_navigation)

    def resize_navigation(self, _, width, _height):
        compact = width < 760
        if compact != self.compact_nav.get_visible():
            self.nav.set_visible(not compact)
            self.compact_nav.set_visible(compact)
            self.body.set_size_request(360 if compact else 550, -1)

    def select_compact_page(self, *_):
        self.nav.select_row(self.nav.get_row_at_index(self.compact_nav.get_selected()))

    def close_window(self, *_):
        if self.busy:
            self.message("Wait for the current task to finish.")
            return True
        self.closed.set()
        if self.notice_timer:
            GLib.source_remove(self.notice_timer)
            self.notice_timer = 0
        if self.discovery_timer is not None:
            GLib.source_remove(self.discovery_timer)
        self.pool.shutdown(wait=False, cancel_futures=True)
        return False

    def selected_device(self):
        index = self.picker.get_selected()
        return self.targets[index] if index < len(self.targets) else backend.DeviceEntry("", ())

    def target(self):
        return self.selected_device().target

    def message(self, text, transient=False):
        if self.notice_timer:
            GLib.source_remove(self.notice_timer)
            self.notice_timer = 0
        self.notice.set_label(text)
        self.notice.set_visible(bool(text))
        if transient:
            def dismiss():
                self.notice_timer = 0
                self.notice.set_visible(False)
                return False
            self.notice_timer = GLib.timeout_add_seconds(3, dismiss)

    def submit(self, work, complete, failed=None):
        if self.busy:
            return
        self.busy = True
        self.set_sensitive(False)
        self.message("Working…")
        future = self.pool.submit(work)

        def finished():
            if self.closed.is_set():
                return False
            self.busy = False
            self.set_sensitive(True)
            try:
                result = future.result()
            except Exception as error:
                if failed:
                    failed()
                denied = (self.selected_device().configurable and self.target()[:1] == ("--connection",)
                          and backend.device_open_denied(error))
                authorization_available = backend.session_supported() if denied else False
                blocked_without_authorization = denied and not authorization_available
                if blocked_without_authorization:
                    if not self.access_setup_notice_shown:
                        self.access_setup_notice_shown = True
                        self.message("The mouse is detected, but access is not approved. Choose Allow access to authorize this app session.")
                else:
                    self.message(backend.describe(error))
                if denied and authorization_available:
                    self.access_button.set_visible(True)
                if not blocked_without_authorization:
                    print(error, flush=True)
            else:
                self.message("")
                complete(result)
            return False

        future.add_done_callback(lambda _: GLib.idle_add(finished))

    def discover(self, force=False):
        if self.dirty and force:
            self.message("Apply or discard your changes before refreshing.")
        if self.busy or self.dirty or self.discovery_pending:
            return True
        if not force and any(window.get_visible() and window.get_transient_for() == self
                             for window in Gtk.Window.list_toplevels()):
            return True
        self.discovery_pending = True
        cached_actions = self.action_names

        def scan():
            entries = backend.identify_authorized_receivers(
                backend.devices(backend.run(["desktop-list"], timeout=5)))
            names = cached_actions or tuple(backend.run(["button", "actions"]).splitlines())
            return entries, names

        future = self.pool.submit(scan)

        def ready():
            self.discovery_pending = False
            if self.closed.is_set() or self.busy or self.dirty:
                return False
            try:
                entries, self.action_names = future.result()
            except (OSError, ValueError, TimeoutExpired, backend.CommandError) as error:
                if force or not self.targets:
                    self.message(backend.describe(error))
            else:
                self.update_devices(entries, force)
            return False

        future.add_done_callback(lambda _: GLib.idle_add(ready))
        return True

    def update_devices(self, entries, force=False):
        previous = {entry.target: entry for entry in self.targets if entry.model}
        entries = [
            previous.get(entry.target, entry) if not entry.model and entry.target[:2] == ("--connection", "receiver")
            else entry
            for entry in entries
        ] or [backend.DeviceEntry("No mouse connected", ())]
        if entries == self.targets:
            if force:
                self.refresh()
            return
        current = self.selected_device()
        selected = next((index for index, entry in enumerate(entries)
                         if entry.target == self.manual_target), 0)
        if entries[selected].target != self.manual_target:
            self.manual_target = ()
        self.targets = entries
        self.picker.handler_block_by_func(self.select_device)
        self.picker.set_model(Gtk.StringList.new([entry.label for entry in entries]))
        self.picker.set_selected(selected)
        self.picker.handler_unblock_by_func(self.select_device)
        selected_entry = self.selected_device()
        if (selected_entry.target, selected_entry.model, selected_entry.configurable) != (
                current.target, current.model, current.configurable):
            self.activate_device()
        elif force:
            self.refresh()
        elif self.active_page == "Overview":
            self.render()

    def select_device(self, *_):
        self.manual_target = self.target()
        self.activate_device()

    def activate_device(self):
        self.access_button.set_visible(False)
        self.access_setup_notice_shown = False
        target = self.target()
        self.elevated_target = target if target[:1] == ("--connection",) and backend.session_ready() else ()
        self.snapshot = None
        self.snapshot_text = ""
        self.message("")
        self.render()
        self.refresh()

    def refresh(self, *_):
        if self.dirty:
            self.message("Apply or discard your changes before refreshing.")
            return
        target = self.target()
        if target and self.selected_device().configurable:
            elevated = target == self.elevated_target
            self.submit(lambda: backend.capture(target, elevated), self.loaded)
        elif self.targets:
            self.render()

    def loaded(self, snapshot):
        self.snapshot, self.snapshot_text = snapshot
        self.access_button.set_visible(False)
        self.render()
        if self.smoke_test:
            self.nav.select_row(self.nav.get_row_at_index(1))
            self.get_application().smoke_passed = True
            print("Shark Driver smoke test passed with the logo and eight DPI stages.", flush=True)
            GLib.idle_add(self.get_application().quit)

    def select_page(self, _, row):
        if row is None:
            return
        if self.dirty:
            self.nav.handler_block_by_func(self.select_page)
            self.nav.select_row(self.nav.get_row_at_index(self.PAGES.index(self.active_page)))
            self.nav.handler_unblock_by_func(self.select_page)
            self.compact_nav.handler_block_by_func(self.select_compact_page)
            self.compact_nav.set_selected(self.PAGES.index(self.active_page))
            self.compact_nav.handler_unblock_by_func(self.select_compact_page)
            self.message("Apply or discard your changes before leaving this page.")
            return
        self.active_page = self.PAGES[row.get_index()]
        self.compact_nav.handler_block_by_func(self.select_compact_page)
        self.compact_nav.set_selected(row.get_index())
        self.compact_nav.handler_unblock_by_func(self.select_compact_page)
        self.render()

    def render(self):
        clear(self.body)
        self.controls = []
        self.library_reload = None
        self.dirty = False
        self.picker.set_sensitive(True)
        self.revealer.set_reveal_child(False)
        self.body.append(label(self.active_page, "title-1"))
        match self.active_page:
            case "Overview":
                self.overview()
            case "Profiles" | "Macros":
                self.library(self.active_page == "Profiles")
            case "Pointer" if self.snapshot is None and (self.selected_device().polling_read
                                                         or self.selected_device().dpi_read):
                if self.selected_device().polling_read:
                    self.partial_polling(self.group("Pointer Response"))
                if self.selected_device().dpi_read:
                    self.partial_dpi()
            case "Buttons" if self.snapshot is None and self.selected_device().button_read:
                self.partial_buttons()
            case _ if self.snapshot is None:
                device = self.selected_device()
                if device.model and not device.configurable:
                    text = "Shark Driver can identify this mouse. Its settings are unavailable."
                elif self.target()[:1] == ("--bluetooth",):
                    text = "Connect with the cable or receiver to change mouse settings."
                else:
                    text = "Mouse settings will appear when the device is available."
                self.body.append(self.placeholder("preferences-desktop-mouse-symbolic", "Settings unavailable", text))
            case "Pointer":
                self.pointer()
            case "Buttons":
                self.buttons()
            case "Lighting":
                self.lighting()
            case "Power":
                self.power()

    def group(self, title):
        heading = label(title, "heading")
        heading.add_css_class("section")
        self.body.append(heading)
        rows = vbox(0)
        rows.add_css_class("card")
        self.body.append(rows)
        return rows

    def row(self, group, title, *widgets, description=None):
        row = Gtk.Box(spacing=10)
        row.add_css_class("setting")
        text = vbox(3)
        text.set_hexpand(True)
        text.set_valign(Gtk.Align.CENTER)
        text.append(label(title))
        if description:
            detail = label(description, "dim-label")
            detail.set_max_width_chars(39)
            text.append(detail)
        row.append(text)
        for widget in widgets:
            widget.set_valign(Gtk.Align.CENTER)
            if isinstance(widget, Gtk.Label):
                widget.set_wrap(False)
            elif not isinstance(widget, (Gtk.Button, Gtk.MenuButton)):
                role = widget.get_tooltip_text()
                widget.update_property([Gtk.AccessibleProperty.LABEL], [f"{title}: {role}" if role else title])
            row.append(widget)
        group.append(row)
        return row

    def track(self, widget, property_name, value, initial, prefix, instant=False):
        self.controls.append((value, initial, prefix))

        def edited(*_):
            after = value()
            if instant and after and after != initial and not self.dirty and not self.busy:
                self.apply_quick([*prefix, after])
            else:
                self.check_dirty()
        widget.connect(property_name, edited)

    def apply_quick(self, command):
        target, baseline = self.target(), self.snapshot_text
        elevated = target == self.elevated_target

        def done(snapshot):
            self.loaded(snapshot)
            self.message("Saved.", transient=True)

        def failed():
            self.snapshot = None
            self.render()

        self.submit(lambda: backend.quick_setting(target, baseline, command, elevated), done, failed)

    def check_dirty(self):
        self.dirty = any(get() != initial for get, initial, _ in self.controls)
        self.picker.set_sensitive(not self.dirty)
        self.revealer.set_reveal_child(self.dirty)

    def placeholder(self, icon, title, message):
        box = vbox(9)
        box.add_css_class("empty-state")
        image = Gtk.Image.new_from_icon_name(icon)
        image.set_pixel_size(40)
        image.add_css_class("dim-label")
        box.append(image)
        for text, style in ((title, "title-3"), (message, "dim-label")):
            node = label(text, style)
            node.set_xalign(0.5)
            node.set_justify(Gtk.Justification.CENTER)
            node.set_max_width_chars(42)
            box.append(node)
        return box

    def app_menu(self):
        menu = Gtk.MenuButton(icon_name="open-menu-symbolic", tooltip_text="More options")
        popover = Gtk.Popover()
        options = vbox(3)
        options.add_css_class("app-menu")
        for title, action in (("Restore Default Settings…", self.reset_mouse),
                              ("Recover Mouse…", self.find_recovery),
                              ("About", self.about)):
            def activate(_, callback=action):
                popover.popdown()
                callback()
            options.append(button(title, activate, "flat"))
        popover.set_child(options)
        menu.set_popover(popover)
        return menu

    def about(self):
        def show(result):
            dialog = Gtk.AboutDialog(transient_for=self, modal=True, program_name=APP_NAME,
                                     version=result.strip().split()[-1], logo_icon_name=APP_ID,
                                     comments="Mouse settings, profiles, and shortcuts")
            dialog.present()
        self.submit(lambda: backend.run(["--version"]), show)

    def reset_mouse(self):
        if self.snapshot is None:
            self.message("Connect the mouse with USB or the receiver to restore its settings.")
            return
        self.confirm("Restore the mouse’s default settings?", lambda: self.perform(["reset"], "Default settings restored.", device=True, refresh=True),
                     action_label="Restore Defaults")

    def overview(self):
        target = self.target()
        if not target:
            self.body.append(self.placeholder(APP_ID, "Connect your mouse",
                           "Plug in the cable or receiver, or connect the mouse in Bluetooth settings."))
            return
        device = self.selected_device()
        kind = "bluetooth" if target[:1] == ("--bluetooth",) else target[1]
        connection = backend.CONNECTIONS[kind]
        hero = Gtk.Box(spacing=18)
        hero.add_css_class("card")
        hero.add_css_class("device-card")
        icon = Gtk.Image.new_from_icon_name(APP_ID)
        icon.set_pixel_size(42)
        hero.append(icon)
        name = vbox(4)
        model = device.model or ("R1" if self.snapshot else "")
        name.append(label(f"Attack Shark {model}" if model else "Attack Shark mouse", "title-2"))
        name.append(label(connection, "dim-label"))
        hero.append(name)
        self.body.append(hero)
        if model and not device.configurable:
            text = ("Additional settings are still being added for this mouse." if device.polling_read or device.dpi_read else
                    "Mouse detected. Configuration is unavailable for this connection.")
            self.body.append(label(text, "dim-label"))
        elif not model:
            self.body.append(label("Wake the mouse to identify it.", "dim-label"))
        elif kind == "bluetooth":
            self.body.append(label("Connect with the cable or receiver to change mouse settings.", "dim-label"))
        if self.snapshot:
            fields, stages = self.snapshot
            stage = next((dpi for _, dpi, _, _, active in stages if active), None)
            rows = self.group("Pointer")
            self.row(rows, "Sensitivity", label(f"{stage:,} DPI" if stage else "Unavailable"))
            self.row(rows, "Update rate", label(fields.get("Polling", "Unavailable")))
        elif device.polling_read:
            self.partial_polling(self.group("Pointer"))
        if device.status or device.configurable:
            rows = self.group("Mouse Checks")
            self.row(rows, "Battery", button("Check Battery", lambda *_: self.open_stream("status")))
            if kind == "wired" and device.configurable:
                self.row(rows, "Movement and clicks", button("Check Movement", lambda *_: self.open_stream("pointer")))

    def partial_polling(self, rows):
        device = self.selected_device()
        rates = (125, 250, 500, 1000)
        if device.target[:2] == ("--connection", "receiver") and device.model in (
            "X11 Pro", "X3 Pro", "X8 Pro"
        ):
            rates += (2000, 4000, 8000)
        selector = choice([f"{rate} Hz" for rate in rates])
        selector.set_visible(False)
        value = label("")

        def command(args):
            elevated = device.target == self.elevated_target and backend.session_ready()
            return backend.run(["polling", *args], device.target, elevated=elevated)

        def display(reply):
            result = reply.strip()
            if result not in (f"{rate} Hz" for rate in rates):
                self.message("Couldn’t read the mouse’s update rate.")
                return
            if device.polling_write:
                with selector.handler_block(handler):
                    selector.set_selected(rates.index(int(result.split()[0])))
                selector.set_visible(True)
            else:
                value.set_label(result)

        def update(*_):
            rate = rates[selector.get_selected()]
            self.submit(lambda: command(["set", str(rate)]), display,
                        failed=lambda: selector.set_visible(False))

        handler = selector.connect("notify::selected", update)
        refresh = button("Check Update Rate", lambda *_: self.submit(lambda: command(["show"]), display))
        self.row(rows, "Update rate", selector if device.polling_write else value, refresh)

    def partial_dpi(self):
        device = self.selected_device()
        rows = self.group("Sensitivity Levels (DPI)")
        levels = vbox(0)
        self.row(rows, "Sensitivity", button("Read Levels", lambda *_: run(["show"], display)))
        rows.append(levels)
        state = {}

        def run(args, complete):
            def work():
                elevated = device.target == self.elevated_target and backend.session_ready()
                return backend.dpi_stages(backend.run(["dpi", *args], device.target, elevated=elevated))
            self.submit(work, complete, failed=lambda: display({}))

        def display(update):
            if self.active_page != "Pointer" or self.target() != device.target:
                return
            if any(record[3] for record in update.values()):
                state.update({index: (*entry[:3], False) for index, entry in state.items()})
            state.update(update)
            clear(levels)
            for index, (dpi, enabled, color, active) in sorted(state.items()):
                title = f"Level {index}" + (" · Active" if active else "")
                if not device.dpi_write:
                    self.row(levels, title, label(f"{dpi} DPI" if dpi else "Unavailable"))
                    continue
                value = Gtk.Entry(text=dpi, placeholder_text="DPI", width_chars=6, max_length=5,
                                  input_purpose=Gtk.InputPurpose.DIGITS)
                enabled_switch = Gtk.Switch(active=enabled == "on", valign=Gtk.Align.CENTER)
                enabled_switch.set_sensitive(not active and bool(dpi))
                swatch = color_control(color)
                select = button("Use", lambda *_, i=index: run(["select", str(i)], display))
                select.set_sensitive(not active and enabled == "on" and bool(dpi))

                def change(flag, value, index=index):
                    run(["set", str(index), flag, value], display)

                save = button("Set", lambda *_, field=value, update=change: update("--value", field.get_text()))
                value.connect("activate", lambda field, update=change: update("--value", field.get_text()))
                enabled_switch.connect("notify::active", lambda widget, _, update=change:
                                       update("--enabled", "on" if widget.get_active() else "off"))
                swatch.connect("notify::rgba", lambda widget, _, update=change:
                               update("--color", color_value(widget)))
                self.row(levels, title, value, save, swatch, enabled_switch, select)

    def partial_buttons(self):
        device = self.selected_device()
        rows = self.group("Button Assignments")
        controls = vbox(0)
        names = (("left", "Left button"), ("right", "Right button"), ("middle", "Scroll wheel click"),
                 ("forward", "Forward button"), ("back", "Back button"), ("dpi", "Sensitivity button"))
        presets = tuple(action for action in self.action_names if ":" not in action)
        state = {}

        def read():
            def work():
                reply = command(["show"])
                return dict(backend.button_line(line) for line in reply.splitlines())
            self.submit(work, display)

        def command(args):
            elevated = device.target == self.elevated_target and backend.session_ready()
            return backend.run(["button", *args], device.target, elevated=elevated)

        def edit(name, action):
            self.submit(lambda: backend.button_line(command(["set", name, action]).strip()),
                        lambda result: display(dict([result])), failed=lambda: display({}))

        def display(update):
            if self.active_page != "Buttons" or self.target() != device.target:
                return
            state.update(update)
            clear(controls)
            for name, title in names:
                if name not in state:
                    continue
                current = state[name]
                if not device.button_write:
                    self.row(controls, title, label(workflows.action_label(current)))
                    continue
                choices = tuple(dict.fromkeys((current, *presets)))
                picker = choice([workflows.action_label(action) for action in choices] + ["Keyboard shortcut…"])
                picker.set_enable_search(True)
                def selected(widget, _, key=name, actions=choices):
                    index = widget.get_selected()
                    if index == len(actions):
                        widget.set_selected(0)
                        self.capture_shortcut(lambda action: edit(key, action))
                    elif 0 < index < len(actions):
                        edit(key, actions[index])

                picker.connect("notify::selected", selected)
                self.row(controls, title, picker)

        self.row(rows, "Current assignments", button("Read Buttons", lambda *_: read()))
        rows.append(controls)

    def authorize(self):
        target = self.target()
        def load(result):
            self.elevated_target = target
            if not self.dirty:
                self.loaded(result)
                return
            self.access_button.set_visible(False)
            if result[1] == self.snapshot_text:
                self.message("Access is ready. Apply to save your changes.")
            else:
                self.message("The mouse settings changed. Discard and refresh before applying.")
        def work():
            backend.start_session()
            return backend.capture(target, elevated=True)
        self.submit(work, load)

    def pointer(self):
        fields, stages = self.snapshot
        rows = self.group("Pointer Speed")
        active = next((index - 1 for index, _, _, _, current in stages if current), 0)
        selector = choice([f"Level {i}" for i in range(1, len(stages) + 1)], active)
        self.row(rows, "Sensitivity", selector)
        self.track(selector, "notify::selected", lambda: str(selector.get_selected() + 1), str(active + 1),
                   ("dpi", "select"), instant=True)
        rates = ("125", "250", "500", "1000")
        rate = fields["Polling"].split()[0]
        if rate in rates:
            polling = choice([f"{item} Hz" for item in rates], rates.index(rate))
            self.row(rows, "Update rate", polling, description="How often the mouse reports its position.")
            self.track(polling, "notify::selected", lambda: rates[polling.get_selected()], rate, ("polling", "set"),
                       instant=True)
        rows = self.group("Sensitivity Levels (DPI)")
        for index, dpi, enabled, color, is_active in stages:
            value = Gtk.Entry(placeholder_text="Set value", text=str(dpi or ""), width_chars=7,
                              max_width_chars=7, max_length=5, input_purpose=Gtk.InputPurpose.DIGITS)
            enable = Gtk.Switch(active=enabled, valign=Gtk.Align.CENTER)
            swatch = color_control(color)
            value.set_tooltip_text("Sensitivity in DPI")
            swatch.set_tooltip_text("Color for this sensitivity level")
            enable.set_tooltip_text("Use this level when cycling sensitivity")
            enable.set_sensitive(not is_active)
            self.row(rows, f"Level {index}", value, swatch, enable)
            base = ("dpi", "set", str(index))
            self.track(value, "changed", value.get_text, str(dpi or ""), (*base, "--value"))
            self.track(enable, "notify::active", lambda w=enable: "on" if w.get_active() else "off",
                       "on" if enabled else "off", (*base, "--enabled"), instant=bool(dpi) and not is_active)
            self.track(swatch, "notify::rgba", lambda w=swatch: color_value(w), color, (*base, "--color"), instant=True)
        rows = self.group("Pointer Movement")
        for key, title, flag in (("Angle snap", "Straighten pointer movement", "--angle-snap"),
                                 ("Ripple control", "Smooth pointer movement", "--ripple-control")):
            switch = Gtk.Switch(active=fields[key] == "on", valign=Gtk.Align.CENTER)
            self.row(rows, title, switch)
            self.track(switch, "notify::active", lambda w=switch: "on" if w.get_active() else "off",
                       fields[key], ("sensor", "set", flag), instant=True)

    def buttons(self):
        fields, _ = self.snapshot
        rows = self.group("Button assignments")
        presets = tuple(action for action in self.action_names if ":" not in action)
        for name, title in (("left", "Left button"), ("right", "Right button"), ("middle", "Scroll wheel click"),
                            ("forward", "Forward button"), ("back", "Back button"), ("dpi", "Sensitivity button")):
            current = next((v for k, v in fields.items() if k.startswith(name + " (")), "")
            choices = tuple(dict.fromkeys((current, *presets, "Shortcut…")))
            picker = choice([workflows.action_label(action) for action in choices[:-1]] + ["Keyboard shortcut…"])
            picker.set_enable_search(True)
            shortcut = [""]

            def captured(code, state=shortcut, target=name):
                state[0] = code
                if self.dirty:
                    self.check_dirty()
                else:
                    self.apply_quick(["button", "set", target, code])

            custom = button("Press Keys…", lambda _, callback=captured: self.capture_shortcut(callback))
            custom.set_visible(False)
            picker.connect("notify::selected", lambda w, _, change=custom: change.set_visible(
                w.get_selected() == w.get_model().get_n_items() - 1))
            self.row(rows, title, picker, custom)
            def value(w=picker, items=choices, captured_key=shortcut, before=current):
                selected = items[w.get_selected()]
                return (captured_key[0] or before) if selected == "Shortcut…" else selected
            self.track(picker, "notify::selected", value, current, ("button", "set", name), instant=True)

    def capture_shortcut(self, action):
        dialog = Gtk.Window(transient_for=self, modal=True, title="Keyboard Shortcut", default_width=380)
        box = self.placeholder("input-keyboard-symbolic", "Press the keys to use", "Example: Ctrl+C")
        box.append(button("Cancel", lambda *_: dialog.close()))
        dialog.set_child(box)
        controller = Gtk.EventControllerKey()
        controller.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)

        def pressed(_, key, _code, state):
            masks = (
                (Gdk.ModifierType.CONTROL_MASK, "ctrl"), (Gdk.ModifierType.SHIFT_MASK, "shift"),
                (Gdk.ModifierType.ALT_MASK, "alt"), (Gdk.ModifierType.SUPER_MASK, "super"),
            )
            modifiers = [name for bit, name in masks if state & bit]
            key_name = Gdk.keyval_name(Gdk.keyval_to_lower(key)) or ""
            if key_name == "Escape" and not modifiers:
                dialog.close()
                return True
            code = workflows.shortcut_value(key_name, modifiers)
            if code:
                dialog.close()
                action(code)
            return True
        controller.connect("key-pressed", pressed)
        dialog.add_controller(controller)
        dialog.present()

    def lighting(self):
        fields, _ = self.snapshot
        rows = self.group("Appearance")
        mode = fields["Lighting"].split()[0]
        modes = tuple(dict.fromkeys((mode, *backend.MODES)))
        picker = choice([workflows.LIGHT_NAMES.get(item, "Current effect") for item in modes])
        selected = lambda: modes[picker.get_selected()]
        self.row(rows, "Light effect", picker)
        self.track(picker, "notify::selected", selected, mode, ("lighting", "set", "--mode"), instant=True)
        color = fields["Color"].removeprefix("#")
        swatch = color_control(color)
        self.row(rows, "Color", swatch)
        self.track(swatch, "notify::rgba", lambda: color_value(swatch), color, ("lighting", "set", "--color"),
                   instant=True)
        for title, key, flag, low, high, static in (
            ("Brightness", "Brightness", "--brightness", 1, 8, True),
            ("Speed", "Breath speed", "--speed", 4, 8, False),
        ):
            initial = fields[key]
            if not initial.isdigit():
                continue
            slider = Gtk.SpinButton.new_with_range(low, high, 1)
            slider.set_value(int(initial))
            self.row(rows, title, slider)
            def update(*_, w=slider, want_static=static):
                current = selected()
                w.set_sensitive(current != "off" and ((current in ("static", "static-dpi")) == want_static))
            update()
            picker.connect("notify::selected", update)
            self.track(slider, "value-changed", lambda w=slider, old=initial:
                       str(w.get_value_as_int()) if w.get_sensitive() else old,
                       initial, ("lighting", "set", flag))

    def power(self):
        fields, _ = self.snapshot
        rows = self.group("Sleep and Clicks")
        for title, unit, low, high, step, flag in (
            ("Sleep", "seconds", 30, 1800, 30, "--sleep-seconds"),
            ("Deep sleep", "minutes", 1, 60, 1, "--deep-sleep-minutes"),
            ("Debounce", "milliseconds", 4, 50, 2, "--debounce-ms"),
        ):
            value = fields[title].split()[0]
            spin = Gtk.SpinButton.new_with_range(low, high, step)
            spin.set_value(int(value))
            names = {"Sleep": "Sleep after", "Deep sleep": "Deep sleep after", "Debounce": "Click delay"}
            self.row(rows, f"{names[title]} ({unit})", spin,
                     description="Increase to help prevent unwanted double clicks." if title == "Debounce" else None)
            self.track(spin, "value-changed", lambda w=spin: str(w.get_value_as_int()), value,
                       ("timing", "set", flag))

    def apply(self, *_):
        pending = {}
        for value, before, command in self.controls:
            after = value()
            if after == before:
                continue
            if not after:
                self.message("Fill in the edited value before applying.")
                return
            if command[-1].startswith("--"):
                pending.setdefault(command[:-1], []).extend((command[-1], after))
            else:
                pending[command] = [after]
        target = self.target()
        if not target:
            return
        elevated = target == self.elevated_target

        baseline = self.snapshot_text

        def done(snapshot):
            self.loaded(snapshot)
            self.message("Changes saved.", transient=True)

        self.submit(lambda: backend.apply_settings(target, baseline, pending, elevated), done)

    def confirm(self, title, action, action_label="Continue"):
        dialog = Gtk.AlertDialog(message=title, buttons=["Cancel", action_label],
                                 cancel_button=0, default_button=0)
        dialog.choose(self, None, lambda source, result: self.confirmed(source, result, action))

    def confirm_recovery(self):
        """Ask for recovery confirmation after the CLI has synchronized its backup."""
        finished = Event()
        accepted = False

        def ask():
            dialog = Gtk.AlertDialog(
                message="Restart the mouse from recovery?",
                detail="Your backup has been saved. Recovery clears the shared flag and macro storage.",
                buttons=["Cancel", "Recover"], cancel_button=0, default_button=0,
            )
            def chosen(source, result):
                nonlocal accepted
                try:
                    accepted = source.choose_finish(result) == 1
                except GLib.Error:
                    pass
                finally:
                    finished.set()
            dialog.choose(self, None, chosen)
            return False

        GLib.idle_add(ask)
        finished.wait()
        return accepted

    @staticmethod
    def confirmed(dialog, result, action):
        try:
            index = dialog.choose_finish(result)
        except GLib.Error:
            return
        if index == 1:
            action()

    def notice_dialog(self, title, text):
        Gtk.AlertDialog(message=title, detail=text, buttons=["Close"]).show(self)

    def prompt_name(self, title, action, initial="", button_text="Save"):
        dialog = Gtk.Window(transient_for=self, modal=True, title=title, default_width=360)
        box = vbox(12)
        box.set_margin_top(18)
        box.set_margin_bottom(18)
        box.set_margin_start(20)
        box.set_margin_end(20)
        entry = Gtk.Entry(text=initial, placeholder_text="Name", activates_default=True, max_length=64)
        entry.update_property([Gtk.AccessibleProperty.LABEL], ["Name"])
        box.append(entry)
        hint = label(workflows.NAME_HINT, "dim-label")
        box.append(hint)
        controls = Gtk.Box(spacing=8, halign=Gtk.Align.END)
        controls.append(button("Cancel", lambda *_: dialog.close()))

        def accept(*_):
            value = entry.get_text().strip()
            if workflows.valid_library_name(value):
                dialog.close()
                action(value)

        save = button(button_text, accept, "suggested-action")
        def check(*_):
            value = entry.get_text().strip()
            valid = workflows.valid_library_name(value)
            hint.set_visible(bool(value) and not valid)
            save.set_sensitive(valid)
        entry.connect("changed", check)
        check()
        controls.append(save)
        box.append(controls)
        dialog.set_child(box)
        dialog.set_default_widget(save)
        dialog.present()
        entry.grab_focus()

    def choose_file(self, title, action, suggested=None):
        folder = Gio.File.new_for_path(str(backend.ROOT))

        def deliver(file):
            path = file.get_path()
            if not path:
                self.message("Choose a file on this computer.")
                return
            try:
                path = backend.project_file(path)
            except ValueError:
                self.message("Choose a file in the app’s folder.")
                return
            action(path)

        if backend.FLATPAK:
            # All files are inside the sandbox's own data directory or an explicitly shared workspace.
            chooser = Gtk.FileChooserDialog(
                title=title, transient_for=self, modal=True, destroy_with_parent=True,
                action=Gtk.FileChooserAction.SAVE if suggested else Gtk.FileChooserAction.OPEN,
            )
            chooser.add_button("_Cancel", Gtk.ResponseType.CANCEL)
            chooser.add_button("_Save" if suggested else "_Open", Gtk.ResponseType.ACCEPT)
            chooser.set_default_response(Gtk.ResponseType.ACCEPT)
            chooser.set_current_folder(folder)
            if suggested:
                chooser.set_current_name(suggested)

            def respond(source, response):
                file = source.get_file() if response == Gtk.ResponseType.ACCEPT else None
                source.destroy()
                if file is not None:
                    deliver(file)

            chooser.connect("response", respond)
            chooser.present()
            return

        chooser = Gtk.FileDialog(title=title, initial_folder=folder)
        if suggested:
            chooser.set_initial_name(suggested)

        def chosen(source, result):
            try:
                file = source.save_finish(result) if suggested else source.open_finish(result)
            except GLib.Error:
                return
            deliver(file)

        (chooser.save if suggested else chooser.open)(self, None, chosen)

    def perform(self, args, message, device=False, refresh=False, library=False):
        target = self.target() if device else ()
        if device and not self.snapshot:
            self.message("Connect with the cable or receiver to use this action.")
            return

        def done(_):
            self.message(message, transient=True)
            if library and self.library_reload:
                self.library_reload()
            if refresh:
                self.refresh()

        self.submit(lambda: backend.scoped_action(args, target, bool(target and target == self.elevated_target)), done)

    def library(self, profiles):
        group = "profile" if profiles else "macro"
        page = self.active_page
        display_name = "profile" if profiles else "macro"
        toolbar = Gtk.Box(spacing=8)
        self.body.append(toolbar)
        if profiles:
            new = button("Save Current Settings…", lambda *_: self.prompt_name(
                "Save Profile", lambda name: self.perform([group, "save", name], "Profile saved.",
                                                           device=True, library=True)), "suggested-action")
            new.set_sensitive(self.snapshot is not None)
            toolbar.append(new)
            toolbar.append(button("Windows Profiles…", lambda *_: self.choose_file(
                "Open Windows Profiles", self.open_windows_profiles)))
        else:
            toolbar.append(button("New Macro…", lambda *_:
                macro_editor.Editor(self, "", "", False).present(), "suggested-action"))
        toolbar.append(button("Import…", lambda *_: self.choose_file(
            f"Import {display_name.capitalize()}", lambda path: self.import_item(group, path))))
        listing = vbox(0)
        self.body.append(listing)

        def load():
            future = self.pool.submit(backend.run, [group, "list"])

            def ready():
                if self.closed.is_set() or self.active_page != page or self.library_reload is not load:
                    return False
                clear(listing)
                try:
                    names = future.result().splitlines()
                except Exception as error:
                    self.message(backend.describe(error))
                    return False
                if not names:
                    listing.append(self.placeholder(
                        "document-open-recent-symbolic" if profiles else "media-record-symbolic",
                        f"No {page.lower()} yet",
                        "Save your mouse settings to use them again later." if profiles else
                        "Create or record a sequence of keys and mouse actions."))
                    return False
                rows = vbox(0)
                rows.add_css_class("card")
                listing.append(rows)
                for name in names:
                    if profiles:
                        primary = button("Apply", lambda _, item=name: self.use_profile(item))
                        primary.set_sensitive(self.snapshot is not None)
                    else:
                        primary = button("Edit", lambda _, item=name: self.edit_macro(item))
                    self.row(rows, name, primary, self.item_menu(group, name))
                return False
            future.add_done_callback(lambda _: GLib.idle_add(ready))

        self.library_reload = load
        load()

    def choose_windows_model(self, action):
        def choose_model(models):
            if not models:
                self.message("No mouse models are available.")
                return
            current = next((i for i, (_, name) in enumerate(models)
                            if name == self.selected_device().model), 2)
            dialog = Gtk.Window(transient_for=self, modal=True, title="Choose the Mouse Model",
                                default_width=360)
            box = vbox(10)
            for setter in (box.set_margin_top, box.set_margin_bottom, box.set_margin_start, box.set_margin_end):
                setter(18)
            picker = choice([name for _, name in models], current)
            box.append(picker)

            def inspect(*_):
                slot, model = models[picker.get_selected()]
                dialog.close()
                action(slot, model)

            box.append(button("Continue", inspect, "suggested-action"))
            dialog.set_child(box)
            dialog.present()
        self.submit(backend.vendor_models, choose_model)

    def open_windows_profiles(self, path):
        self.choose_windows_model(lambda slot, model:
            self.submit(lambda: backend.run(["profile", "inspect-windows", path, "--model", slot]),
                        lambda result: self.manage_windows_profiles(path, slot, model, result)))

    def manage_windows_profiles(self, path, slot, model, contents):
        active, entries = backend.windows_profile_names(contents, "Profile")
        if not entries:
            self.message("This file has no profiles.")
            return
        dialog = Gtk.Window(transient_for=self, modal=True, title=f"{model} Windows Profiles",
                            default_width=440)
        box = vbox(10)
        for setter in (box.set_margin_top, box.set_margin_bottom, box.set_margin_start, box.set_margin_end):
            setter(18)
        picker = choice([name for _, name in entries], active if active < len(entries) else 0)
        box.append(picker)
        name = Gtk.Entry(placeholder_text="New profile name", max_length=99)
        box.append(name)

        def save(verb, suggested, new_name=None):
            index, _ = entries[picker.get_selected()]
            arguments = ["profile", verb, path, index]
            if new_name is not None:
                arguments.append(new_name)
            dialog.close()
            self.choose_file("Save Windows Profile Copy", lambda destination: self.perform(
                [*arguments, destination, "--model", slot], "Windows profile copy saved."),
                suggested=suggested)

        rename = button("Save Renamed Copy…", lambda *_: save("rename-windows", "pro.data", name.get_text()))
        rename.set_sensitive(False)
        name.connect("changed", lambda *_: rename.set_sensitive(0 < len(name.get_text().encode("utf-16-le")) <= 198))
        box.append(rename)
        box.append(button("Save With This Profile Active…", lambda *_: save("activate-windows", "pro.data")))
        box.append(button("Export This Profile…", lambda *_: save("extract-windows", "profile.pro")))
        dialog.set_child(box)
        dialog.present()

    def edit_macro(self, name):
        self.submit(lambda: backend.run(["macro", "show", name]),
                    lambda text: macro_editor.Editor(self, name, text, True).present())

    def use_profile(self, name):
        self.confirm(f"Use “{name}” on the mouse?", lambda: self.perform(["profile", "apply", name],
                     "Profile applied.", device=True, refresh=True), action_label="Use Profile")

    def import_item(self, group, path):
        path_name = path.rsplit("/", 1)[-1]
        windows = path_name.lower().endswith((".pro", ".data", ".mcr", ".macro"))
        title = f"Import {group.capitalize()}"
        if not windows:
            self.prompt_name(title, lambda name: self.perform([group, "import", name, path],
                             "Imported.", library=True), initial=path_name.rsplit(".", 1)[0], button_text="Import")
            return
        if group == "profile":
            if not self.snapshot:
                self.message("Connect your R1 by USB or receiver to import a Windows profile, or open it in Windows Profiles.")
                return
            self.submit(lambda: backend.run([group, "inspect-windows", path]),
                        lambda contents: self.choose_windows_item(group, path, contents))
        else:
            self.choose_windows_model(lambda slot, _:
                self.submit(lambda: backend.run([group, "inspect-windows", path, "--model", slot]),
                            lambda contents: self.choose_windows_item(group, path, contents, slot)))

    def choose_windows_item(self, group, path, contents, slot=None):
        entries = [line.partition(": ") for line in contents.splitlines() if line[:1].isdigit()]
        kind = group.capitalize()
        labels = [workflows.windows_name(name) or f"{kind} {index}" for index, _, name in entries]
        if not labels:
            self.message(f"This file has no {group}s to import.")
            return
        dialog = Gtk.Window(transient_for=self, modal=True, title=f"Choose a {kind}", default_width=360)
        box = vbox(10)
        box.set_margin_top(16)
        box.set_margin_bottom(16)
        box.set_margin_start(18)
        box.set_margin_end(18)
        picker = choice(labels)
        box.append(picker)

        def choose(*_):
            index = picker.get_selected()
            flags = (["--firmware", self.snapshot[0]["Firmware"]] if group == "profile"
                     else ["--model", slot])
            dialog.close()
            self.prompt_name(f"Import {kind}", lambda name: self.perform(
                [group, "import-windows", name, path, entries[index][0], *flags],
                f"{kind} imported.", library=True), initial=labels[index], button_text="Import")

        box.append(button("Continue…", choose, "suggested-action"))
        dialog.set_child(box)
        dialog.present()

    def item_menu(self, group, name):
        control = Gtk.MenuButton(icon_name="view-more-symbolic", tooltip_text=f"More options for {name}")
        control.update_property([Gtk.AccessibleProperty.LABEL], [f"Options for {name}"])
        popover = Gtk.Popover()
        options = vbox(2)
        options.add_css_class("app-menu")

        def add(title, action, enabled=True):
            def activate(*_):
                popover.popdown()
                action()
            item = button(title, activate, "flat")
            item.set_sensitive(enabled)
            options.append(item)

        if group == "profile":
            add("Update from Mouse…", lambda: self.confirm(f"Replace the settings in “{name}”?",
                lambda: self.perform([group, "update", name], "Profile updated.", device=True, library=True),
                action_label="Update"), self.snapshot is not None)
        else:
            add("Assign to Button…", lambda: self.assign_macro(name), self.snapshot is not None)
        add("Rename…", lambda: self.prompt_name(f"Rename {group.capitalize()}", lambda value:
            self.perform([group, "rename", name, value], "Renamed.", library=True), initial=name))
        add("Export…", lambda: self.choose_file(f"Export {group.capitalize()}", lambda path:
            self.perform([group, "export", name, path], "Exported."),
            suggested=name + (".r1profile" if group == "profile" else ".r1macro")))
        if group == "macro":
            add("Export for Windows…", lambda: self.choose_windows_model(lambda slot, _:
                self.choose_file("Export for Windows", lambda path:
                    self.perform([group, "export-windows", name, path, "--model", slot], "Exported."),
                    suggested=f"{name}.data")))
        else:
            add("Export for Windows…", lambda: self.choose_file("Export for Windows", lambda path:
                self.perform([group, "export-windows", name, path], "Exported."),
                suggested=f"{name}.pro"))
        add("Delete…", lambda: self.confirm(f"Delete “{name}”?", lambda:
            self.perform([group, "delete", name], "Deleted.", library=True), action_label="Delete"))
        popover.set_child(options)
        control.set_popover(popover)
        return control

    def assign_macro(self, name):
        labels = ("Left button", "Right button", "Scroll wheel click", "Forward button",
                  "Back button", "Sensitivity button")
        keys = ("left", "right", "middle", "forward", "back", "dpi")
        dialog = Gtk.Window(transient_for=self, modal=True, title="Choose a Mouse Button", default_width=350)
        box = vbox(10)
        for setter in (box.set_margin_top, box.set_margin_bottom, box.set_margin_start, box.set_margin_end):
            setter(18)
        selector = choice(labels, 3)
        box.append(selector)

        def assign(*_):
            key = keys[selector.get_selected()]
            dialog.close()
            self.perform(["macro", "assign", key, name], "Macro assigned.", device=True, refresh=True)

        box.append(button("Assign Macro", assign, "suggested-action"))
        dialog.set_child(box)
        dialog.present()

    def find_recovery(self):
        def found(result):
            devices = [line.split("\t") for line in result.splitlines()[1:]]
            if len(devices) != 1:
                text = ("Connect just one mouse that needs recovery." if devices else
                        "Connect the mouse with its USB cable. Recovery is available only when the mouse needs it.")
                self.notice_dialog("Recover Mouse", text)
                return
            node, port = devices[0]
            def complete(result):
                if "Canceled. The backup is available." in result:
                    self.notice_dialog("Recovery Canceled", "The backup was saved.")
                else:
                    self.notice_dialog("Recovery Finished", "Reconnect the mouse if it does not appear.")
            self.confirm("Back up and recover the mouse?", lambda: self.choose_file("Save Recovery Backup",
                lambda backup: self.submit(lambda: backend.recover(
                    node, port, backup, self.confirm_recovery, restore=True, elevated=backend.session_ready()),
                    complete),
                suggested="r1-recovery.bin"), action_label="Choose Backup File…")
        self.submit(lambda: backend.run(["desktop-recovery-list"]), found)

    def open_stream(self, kind):
        target = self.target()
        if not target:
            self.message("Connect your mouse first.")
            return
        movement = kind == "pointer"
        passive = not self.selected_device().configurable
        status_command = "desktop-observe-status" if passive else "desktop-status"
        command = ["desktop-pointer-activity"] if movement else [
            status_command, "--count", "10", "--timeout-ms", "1500",
        ]
        dialog = Gtk.Window(transient_for=self, modal=True, title="Mouse Movement" if movement else "Battery",
                            default_width=390)
        box = vbox(12)
        for setter in (box.set_margin_top, box.set_margin_bottom, box.set_margin_start, box.set_margin_end):
            setter(18)
        display = self.placeholder("input-mouse-symbolic" if movement else "battery-symbolic",
                                   "Move or click the mouse" if movement else "Checking battery…", "")
        value = display.get_first_child().get_next_sibling()
        box.append(display)
        box.append(button("Close", lambda *_: dialog.close()))
        dialog.set_child(box)
        seen = False

        def line_received(line):
            nonlocal seen
            reading = ({"pointer-move": "Movement detected", "pointer-button": "Click detected"}.get(line)
                       if movement else workflows.battery_label(line, target[:2] == ("--connection", "wired")))
            if reading:
                seen = True
                value.set_label(reading)

        def done(success, canceled):
            if not canceled and not seen:
                value.set_label(("No movement detected" if movement else "Battery level unavailable") if success else
                                "Couldn’t check the mouse")

        try:
            elevated = not passive and target == self.elevated_target
            stream = streams.Stream(backend.command_line(command, target, elevated), line_received, done)
        except (GLib.Error, backend.CommandError) as error:
            self.message(backend.describe(error))
            return
        dialog.connect("close-request", lambda *_: (stream.cancel(), False)[1])
        dialog.present()


def main():
    parser = argparse.ArgumentParser(description=f"{APP_NAME} desktop")
    parser.add_argument("--smoke-test", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    GLib.set_application_name(APP_NAME)
    GLib.set_prgname(APP_ID)
    app = Gtk.Application(application_id=APP_ID, flags=Gio.ApplicationFlags.NON_UNIQUE)
    app.smoke_passed = False
    app.display_failed = False
    def activate(application):
        display = Gdk.Display.get_default()
        if display is None:
            print("Shark Driver could not connect to your graphical session. Start it from your desktop session.",
                  file=sys.stderr, flush=True)
            application.display_failed = True
            application.quit()
            return
        follow_flatpak_appearance(application, display)
        has_logo = configure_branding(display)
        if args.smoke_test and not has_logo:
            print("Shark Driver logo is missing.", flush=True)
            application.quit()
            return
        provider = Gtk.CssProvider()
        provider.load_from_string(CSS)
        Gtk.StyleContext.add_provider_for_display(display, provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
        Window(application, args.smoke_test).present()
    app.connect("activate", activate)
    def shutdown(application):
        if getattr(application, "theme_subscription", 0):
            application.theme_bus.signal_unsubscribe(application.theme_subscription)
        backend.stop_session()

    app.connect("shutdown", shutdown)
    if args.smoke_test:
        GLib.timeout_add_seconds(15, lambda: (app.quit(), False)[1])
    status = app.run([])
    return 1 if app.display_failed or (args.smoke_test and not app.smoke_passed) else status


if __name__ == "__main__":
    raise SystemExit(main())
