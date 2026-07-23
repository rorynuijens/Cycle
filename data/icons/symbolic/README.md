# Bundled symbolic icons

These icons come from the GNOME icon development kit
(<https://gitlab.gnome.org/Teams/Design/icon-development-kit>), which is
dedicated to the public domain under CC0-1.0.

They are bundled because the GNOME HIG recommends apps ship any icon that is
not part of the guaranteed icon-naming set — relying on the system theme for
names like `heart` or `map` breaks on themes that do not ship them.

Bundled via `data/cycle.gresource.xml` under
`/io/github/rorynuijens/Cycle/icons/scalable/actions/<name>-symbolic.svg`,
which is the layout `GtkIconTheme` expects for resource-based icons.
