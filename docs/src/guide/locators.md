# Locator API

`Session::locate(xpath)` returns a lazy `Locator` — each action re-snapshots
the AT-SPI tree and re-resolves the selector, so you don't have to worry
about stale element handles. Common methods:

| Method                                     | What it does                                     |
| ------------------------------------------ | ------------------------------------------------ |
| `click()` / `double_click()` / `right_click()` | Invoke the AT-SPI `Action` interface (primary, secondary, tertiary actions) |
| `hover()` / `drag_to(target)` / `drag_to_coords(x, y)` | Pointer-driven hover and drag — lands on real Wayland input events for repaint. `drag_to_coords` releases at raw screen coordinates, so the drop can land off-window (e.g. libadwaita tab drag-out) |
| `focus()` / `scroll_into_view()`           | `Component::grab_focus` and `scroll_to`/`scroll_to_point` |
| `set_text(s)` / `fill(s)`                  | Direct `EditableText` write vs. focus-and-type fallback for widgets without `EditableText` (e.g. `GtkTextView`) |
| `select_option(by)`                        | Pick a child of a Selection-interface container by label or index |
| `text()`                                   | Read via the `Text` interface                    |
| `count()` / `all()` / `inspect_all()`      | Multi-match: count, list of locators, full metadata in one snapshot |
| `name()` / `role()` / `attribute(k)` / `attributes()` / `bounds()` | Accessible name, role, AT-SPI attributes, screen-relative bounds |
| `is_showing()` / `is_enabled()`            | State predicates                                 |
| `wait_for_visible()` / `_hidden()` / `_enabled()` / `_count(n)` / `_text(pred)` | Block until state or predicate holds |
| `wait_for(pred)` / `wait_until(pred)` / `wait_until_async(pred)` | General-purpose predicate auto-waits  |
| `with_timeout(d)`                          | Per-call override of the auto-wait timeout        |
| `nth(i)` / `first()` / `last()` / `parent()` / `locate(sub_xpath)` | Compose sub-locators |

Single-target actions (`click`, `focus`, `set_text`, `text`, ...) error with
`AmbiguousSelector` if the selector matches more than one element. Narrow
with `.nth(i)` or a more specific XPath.
