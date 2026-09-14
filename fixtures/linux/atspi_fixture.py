#!/usr/bin/env python3
import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gtk


window = Gtk.Window(title="Comptrol AT SPI Fixture")
button = Gtk.Button(label="Submit")
button.set_name("comptrol-submit")


def submit(_button):
    button.set_label("Submitted")


button.connect("clicked", submit)
window.add(button)
window.connect("destroy", Gtk.main_quit)
window.show_all()
Gtk.main()
