#!/usr/bin/env python3
"""Tiny GTK app used by the Xpra PoC to create a real transient modal dialog.

Shows a main window titled RAST-POC-MAIN, then opens a modal Gtk.Dialog whose
WM_TRANSIENT_FOR points at the main window. The shim must report the dialog's
metadata (`transient-for`, `modal`) so the RAST desktop can attach it to its
parent window and keep it out of the taskbar.
"""
import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk  # noqa: E402


class PoCApp(Gtk.Window):
    def __init__(self):
        super().__init__(title="RAST-POC-MAIN")
        self.set_default_size(420, 260)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        label = Gtk.Label(label="RAST transient dialog PoC\n(main window)")
        box.pack_start(label, True, True, 12)
        button = Gtk.Button(label="Open dialog")
        button.connect("clicked", lambda *_: self.open_dialog())
        box.pack_start(button, False, False, 12)
        self.add(box)
        self.connect("destroy", Gtk.main_quit)
        GLib.timeout_add(2500, self.open_dialog)
        self.show_all()

    def open_dialog(self):
        if getattr(self, "_dialog", None) is not None:
            return False
        dialog = Gtk.Dialog(title="RAST-POC-DIALOG", transient_for=self, modal=True)
        dialog.set_default_size(360, 180)
        dialog.add_button("Cancel", Gtk.ResponseType.CANCEL)
        dialog.add_button("OK", Gtk.ResponseType.OK)
        content = dialog.get_content_area()
        content.add(Gtk.Label(label="This modal dialog belongs to the main window."))
        dialog.connect("response", lambda dlg, _resp: (dlg.destroy(), setattr(self, "_dialog", None)))
        dialog.connect("destroy", lambda *_: setattr(self, "_dialog", None))
        self._dialog = dialog
        dialog.show_all()
        return False


if __name__ == "__main__":
    app = PoCApp()
    Gtk.main()
