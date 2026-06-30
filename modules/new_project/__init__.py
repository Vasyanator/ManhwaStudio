"""new_project package.

Historically this package exported the legacy PyQt6 "New project" window
(`show_new_project_full`). That UI was rewritten in Rust under
`src/launcher/new_project/` and moved to `old_or_test/2.X/`, so it is no longer
imported here. Keeping `__init__` import-light is required: the AI backend's
browser service imports `modules.new_project.adv_fetch_cli` /
`adv_fetch_cloak_cli`, which initializes this package first. Importing the old
PyQt6 window here would force PyQt6 into the headless backend process.

Only the headless Selenium/CloakBrowser fetch CLIs and their shared `common`
helpers remain in this package.
"""

__all__: list[str] = []
