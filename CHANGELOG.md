# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-11-13

### Added

- **`desktop_record` and `desktop_flow`: record a human demonstration of a desktop task and replay it through the
  gate.** The desktop twin of `browser_record` / `browser_flow`. `desktop_record` (`start`, `stop` with a `name`
  to save, `status`) listens to a person's real keyboard and mouse and keeps *what was touched*, not where:
  for a click, the role, title and parents (up to the window) of the element under the pointer, the point only
  as a fallback hint, and the menu path for a menu item; a run of typing becomes one `type` step on the field
  that had the focus (Backspace edits the run); chords and non-printing keys become `shortcut` steps. Text typed
  into a secure field is cleared as it is read and the step holds a `secret_ref`, supplied as `secrets` at replay,
  as the browser recorder does. Recording is a dangerous-tier tool (the one tool that reads a person's input, so
  it must be named in `policy.enable`), an explicit bounded session (at most 600 s, 4000 events, 200 steps), keeps
  events only for `allowed_apps` and counts the others (`dropped_other_apps`) without storing them, and writes a
  marker file that makes `agentctl doctor` print `RECORDING ACTIVE` while it runs. On macOS it is a listen-only
  `CGEventTap` (Input Monitoring plus Accessibility); without them the call is `PERM_DENIED` naming which, and
  nothing is recorded. On Linux it is `UNSUPPORTED_OS`: the remote-desktop portal can inject input but cannot
  listen to it. `desktop_flow` (`list`, `get`, `save`, `delete`, `run`) keeps flows in `desktop_flows.json` beside
  the browser's. `run` takes a snapshot of each step's app without pinning it, finds the element again by role,
  title and ancestry (exact; fuzzy when the title only contains the recorded one or the surroundings changed;
  the recorded point only when nothing matches; two equally good candidates are an error, never a guess), and
  performs the step as the ordinary call it stands for (`ui_action`, `keyboard_type`, `set_value`,
  `keyboard_shortcut`, `mouse_action`) through the same relay `input_steps` uses, so tier, role, consent,
  foreground mode and audit see every step as its own call. Each step reports its `route` (`ax`, `hid`,
  `pid_event`) and `matched`; the first failure stops the run with `failed_at`. A click on a text field replays as
  `focus` (a text area offers no press), so it can run in the background; under `foreground = "never"` a flow
  whose steps would use the keyboard is refused before the first runs, and the coordinate fallback is refused
  like any pointer action. The human-override watcher is untouched: it samples the pointer only while the server is
  driving, and `desktop_record` does not mark the server as driving. Measured on macOS: a click in a TextEdit
  scratch document and a typed word recorded as a `textarea` click (exact point kept as a hint) plus a `type` step
  on the `textarea`, and replayed into a fresh document (matched, route reported, text arrived). `value set` steps
  are written by hand with `save` (op `set`): the recorder sees keys, not values.
- **`observe`: a window's tree and, where the tree is thin, its text, in one call.** An agent facing an app it
  cannot read used to chain `get_ui_tree`, `capture_window` and `ocr_region` and then work out which pixel to
  click. `observe` returns the accessibility snapshot with `get_ui_tree`'s own refs (they resolve in
  `ui_action`) and, when the tree is *sparse* for the chosen window, also the window's text read off its pixels
  and joined to the tree: text that sits on an element is attached to its ref, text that sits on nothing
  becomes a numbered mark (`m1`, `m2`..., in reading order) with the screen point and box `mouse_action` takes.
  Sparse is a pure, tested rule stated in the tool description and echoed in every result: fewer than 5
  actionable elements in the window, or fewer than one per 120000 square points of its area, or a childless
  canvas, web view, group, scroll area or image covering a quarter of it. `ocr` (`auto`, `always`, `never`)
  overrides it, `max_elements` caps the entries (the rest are counted), and `marks_image: true` returns the
  window's screenshot with the numbers drawn on it, sized from the same `[vision]` detail tiers as
  `capture_window`. It is passive: the app is read without pinning it (a new `without_pin` snapshot mode
  installs the refs but leaves the session's pinned app alone), nothing is raised, focused or clicked, and it
  works on a window behind others. The capture's `via` and any covered-window `warning` / `covered_by` are
  carried through; on macOS the covering windows' rectangles are now kept with the cover report, and text
  inside them is left out, where a platform only knows that something covers the window every mark is flagged
  `unreliable`. Text on a secure field is withheld and counted, never returned. A named `app` is held to
  `allowed_apps` by the name given and by the app it resolves to, and the output is marked untrusted. The
  join is a source list (`ocr` today), the place a local grounding model would add boxes. Text is read in
  the recogniser's fast mode unless `ocr_level: "accurate"` is asked for: on the development Mac the accurate
  mode measured 25 to 52 s wall time per window, mostly waiting on the system, against 0.1 s for fast. Lives
  in `mcp-window` (the crate that already composes the tree with other reads), the merge and the numbered
  drawing in `mcp-vision`. Tested live against Finder (nothing pinned, raised or moved), a TextEdit scratch
  document (the typed word attached to its text area, auto mode reading text exactly when the tree is sparse)
  and a TextEdit window behind Finder under `foreground = "never"` (reported covered by Finder, 86%).
- **`wait_for` is woken by the app's own change notifications on macOS.** It registers an `AXObserver` on the
  target app (value, focus, window, title, selection, layout and menu notifications) on a thread of its own and
  re-checks the condition when one arrives, instead of every 150 ms; a 500 ms safety re-check stays because
  many apps post some changes unreliably or not at all. The result says what noticed: `woke_by` is `immediate`
  (true at the first look), `event`, or `poll` (found by the timer, which is also what an app that refuses
  observation, or any platform without events such as Linux, gets, at the old 150 ms). `waited_ms` is
  unchanged and a timeout has the same shape as before. The observer, its run-loop source, its callback
  context and its thread are torn down when the wait ends by success, timeout, cancellation or the call being
  dropped, and waiting still does not pin or raise the app. Measured against TextEdit (text written through
  accessibility 300 ms into a wait, median of 7): the wait returned 187 ms after the 300 ms mark by event,
  against 382 ms with a 150 ms timer re-reading the tree; the figure includes the time `set_value` itself
  takes to land the write, which is the same in both. `expect` clauses on input tools use the same wait.
- **`scroll` with `dx` / `dy`: trackpad-style smooth scrolling with gesture phases and momentum.** Pixel deltas
  (positive is right and down) sent as a stream of events over `duration_ms` (default 250), began, changed,
  ended, as a trackpad produces them, which apps that smooth or hand off a scroll key off; the deltas sum
  exactly to the request. `momentum: true` adds the coast of a flick after release (macOS, using the public
  `kCGScrollWheelEventScrollPhase` and `kCGScrollWheelEventMomentumPhase` fields; Linux sends the portal's
  smooth axis with its `finish` flag and lets the compositor coast, and the result says momentum was not
  sent). Capped at 10000 px per axis, 3000 ms, and momentum at 3000 px and 60 events; a cancelled or
  interrupted gesture still posts its closing phase. It uses the real pointer like any `scroll`, so
  `never` refuses it and `polite` borrows it. Measured in TextEdit: a 600 px drag moved the document 17.4%,
  the same drag with momentum 20.0%. Pinch, rotate and swipe are not attempted (`scroll` `gesture`
  returns `UNSUPPORTED_OS`): macOS has no public API to synthesise them, and the error names the working
  route (cmd+plus / cmd+minus or ctrl-scroll, the app's rotate command, back / forward shortcuts, ctrl+arrow
  between spaces).
- **`foreground = "polite"`: borrow the pointer and keyboard when you are idle, and give them back.** A third
  setting between `allow` and `never`, for actions that genuinely need the real pointer, keyboard or a raised
  app (`mouse_action`, `scroll`, `hover`, `drag_drop`, `keyboard_type` and `keyboard_shortcut` when no pinned
  background app takes them by process, `ui_action show_menu`, `menu_open`, `menu_invoke`, `control_window
  restore`, `focus_app` and `control_window focus`). Such a call first waits until you have been idle for
  `input.polite_idle_ms` (default 1500; the OS idle clock, with agentctl's own input not counted as you:
  a synthetic pointer move was measured to reset it exactly as a real one does) for at most
  `input.polite_max_wait_ms` (default 15000), then fails with a retryable `TIMEOUT` ("the user is active")
  having done nothing. Otherwise it records the frontmost app and the pointer, acts, and puts both back, on
  error paths and a dropped call too; the result's `polite` says what was returned (`returned.app`,
  `returned.pointer`) and `waited_ms`. An `input_steps` batch borrows once and restores once. If you move
  the pointer mid-action, the action is abandoned with buttons and keys released and fails with the same
  retryable error; the app is put back but the pointer is left in your hand. The human-override watcher trips
  the kill switch in this mode exactly as under `allow` (taking the pointer is a stop request in every
  mode), and the STOP file stops everything in every mode. `AGENTCTL_FOREGROUND` can only tighten (`never` over
  `polite` over `allow`): `polite` from the environment applies over a file's `allow`, and neither `allow`
  nor `polite` loosens a stricter file. Not supported, and refused with the reason: `keyboard_shortcut`
  `action: "down"` (a held key outlives the call, so it cannot be given back), and any call that needs the
  desktop on a platform with no idle clock (Linux). A polite call costs about half a second more, spent
  watching for an app that is slow to come forward. `focus_app` and `control_window focus` raise the app only
  for the length of the call (`focus_app` still pins it), and `launch` starts apps without activating them.
  `agentctl doctor` reports the mode and its timings. Courtesy, not containment: see `docs/threat-model.md`.
  Measured on macOS with Finder in front, the pointer parked at a known spot and a TextEdit scratch document:
  a click-and-type batch landed its text and afterwards Finder was frontmost and the pointer 0.0 points from
  where it was; `focus_app` raised TextEdit and handed the front back.

- **Keys reach a pinned background app by process, without raising it (`input: "pid_event"`).** When the app
  pinned with `focus_app` is not frontmost, `keyboard_type` and `keyboard_shortcut` (press) post their key
  events to that process (`CGEventPostToPid`) instead of bringing it forward and typing into the foreground;
  with the pinned app in front, or nothing pinned, they are `hid` as before. Per-process delivery is
  unreliable in some apps, so it is never taken on trust: typed text is read back from the app's focused
  element (or the element a `ref` named) and the call is an error when it did not change, or cannot be read
  (a password field, an app with no readable text), naming `set_value` and `focus_app activate: true` as the
  ways forward; it does not fall back to `hid`, which would take the foreground. A chord has no read-back,
  so its result says `verified: true` only when the focused element visibly changed and `verified: false`
  with a note otherwise. Measured on TextEdit behind Finder: typed text and text-system chords
  (`shift+left`) arrived and the topmost window, the active app, Finder's focused element and the pointer
  were unchanged; the menu key equivalent `cmd+a` was not delivered and the result said so. The
  destructive-command screen applies to the pinned target, as it does to the frontmost app on `hid`. Held
  keys (`action: "down"`) stay on the frontmost app, and a pid press is refused, saying why, while keys are
  held. `foreground = "never"` allows the route, since the measurement showed it neither moves the pointer
  nor changes the frontmost app, and still refuses keys that would go to the frontmost app. Not available
  on Linux: the RemoteDesktop portal has no per-process delivery, so keys there go to the focused window as
  before.

- **`input_steps` runs up to 20 desktop input actions in one call.** An agent filling a dialog spent a model
  turn on every click and keystroke; `browser_act` `steps` removed that for the browser and the desktop had
  nothing like it. A step is `{tool, args}`, the tool being `ui_action`, `set_value`, `select_text`,
  `ui_fill_form`, `keyboard_type`, `keyboard_shortcut`, `mouse_action`, `scroll`, `hover`, `drag_drop` or
  `clipboard_write`; anything else (`exec`, a browser tool, `input_steps` itself, the two read tools) is an
  `INVALID_ARGS` error naming the step, and so is a malformed step, before any step runs. Steps run in order
  and the first failure ends the batch with `failed_at`; the result is `{ran, total, failed_at, elapsed_ms,
  steps: [{i, tool, ok, ...that step's result, error}]}`. It is not a way around a check: each step is handed
  back to the server's own `dispatch_call` (through a second `Server` over the same input engine, policy and
  audit log), so the policy tier, the role's allowed and denied tools, consent, the hard invariants, the
  kill switch apply to every step as they do to a call made alone, and every step has
  its own audit entry (a `secret: true` step is redacted there). Under `foreground = "never"` the whole batch
  is checked first, so one refused step means no step ran. The kill switch, a human taking the pointer and a
  client cancel are checked between steps; the batch has 30 seconds, and a step still running at the end is
  cancelled. A synthetic personal-data token cannot be passed in a batch (the batch is not a token sink); type
  it with its own call. Held keys follow the existing rule per step. Limits: a step that names an element by
  `name` resolves it against the snapshot as it is at that moment, so after a step that changes the screen
  use a `ref` or end the batch there. A batch on a server nobody wired (`bind_input_steps`) is refused, not
  run unchecked. Unverified: the live TextEdit test (`textedit_input_steps_edit_a_scratch_document_in_one_call`)
  is written but has not been run, and nothing here has run on Linux.

- **Desktop actions report `effects`, what they changed, in the same call.** `ui_action`, `set_value`,
  `keyboard_type`, `keyboard_shortcut` (press), `mouse_action` clicks, `select_text`, `ui_fill_form` and each
  step of `input_steps` add `effects` to their result when something changed, with only the fields that did:
  `app` (the frontmost application), `focus` (`{role, name}`), `appeared` and `disappeared` (titles of the
  target application's windows, at most 5 each), `retitled` (`{from, to}`), `overlay` (`{kind, title, text,
  buttons}` of a sheet, dialog, alert, popover or menu that opened; the desktop twin of the browser's
  `effects.overlay`) and `overlay_closed`, and `value` or `checked` of the element a `ui_action` pressed by
  `ref`. Before, the agent called `get_ui_tree` after each action to find out. It is not a snapshot diff: a
  fixed set of attribute reads before and after (about 1 to 4 ms each once an app has been read, 15 to 150 ms the
  first time; measured on macOS with the accessibility API only), a 120 ms settle between them, and a pure
  comparison. Reading raises no window and moves no focus, so it works under `foreground = "never"`. A fact
  that could not be read is left out, never guessed, and a fact missing from either reading is not compared.
  Nothing is emitted when nothing changed, so a missing `effects` means nothing was seen to change among
  those facts, which is not proof that nothing did: an app may react after the wait, and a change inside a
  window's contents is not among them. Text is clipped (names 60 characters, a value 80). `effects: false`
  skips the reads and the wait, and a call with `expect` keeps its own delta. Limits: a sheet is found only as
  a direct child of a window (where a TextEdit save panel sits), an app that exposes no accessibility windows
  (Chrome until its accessibility is on) reports none, and a `ui_action` named by `name` has no `value` or
  `checked`. On Linux the window list, dialogs and frontmost application are read from AT-SPI and `focus` is
  not; that code type-checks but has not been run. The live test
  (`effects_report_the_save_sheet_a_shortcut_opens_and_closes`) is written and has not been run.

- **`foreground = "never"` (`[input]` table, or `AGENTCTL_FOREGROUND=never`) refuses every tool that would
  take your pointer, keyboard or foreground.** The default, `"allow"`, changes nothing. With `never`,
  `mouse_action`, `scroll`, `hover`, `drag_drop`, `keyboard_type`, `keyboard_shortcut` (press or hold) and
  `input_showcase` are refused with `POLICY_DENIED` before anything runs, as are `ui_action` `focus`,
  `show_menu` and `select` with an `option`, a `ui_fill_form` containing any of those, `menu_open`,
  `menu_invoke`, `focus_app` with an app, and `control_window` `focus` and `restore`. Each refusal says what
  was refused, that `foreground = "never"` is why, and the tool that does the job in the background where one
  exists (`ui_action click` for a click on a ref, `set_value` for typing into a field, `scroll_into_view` or
  `increment` for scrolling; `drag_drop` and `hover` have none). `set_value`, `select_text`, `get_selection`,
  the other `ui_action` actions, the clipboard and the window and menu reads keep working and never call the
  step that raises an app. `launch` runs `open -g` on macOS and is refused on Linux, which has no way to
  start an app without activating it. The check is made twice: in each tool, first, so a refused call has not
  taken a snapshot or raised a consent dialog; and in a wrapper around the platform backend, so a tool added
  later still posts nothing. Only the config file or the environment sets it; the environment can add `never`
  but not remove it, an unknown value is a startup error, and no role, access profile or tool argument
  changes it. `agentctl doctor` reports the mode. `ui_action` `show_menu` and `select` with an option, `menu_open`, `menu_invoke`
  and `control_window restore` are refused; each was measured against TextEdit with Finder in front and
  brought the app forward, left a menu on screen, or put the restored window above Finder's.
  `ui_action focus` is allowed on macOS, where three runs left the frontmost app alone (it stays refused on
  Linux, which was not measured). What `never` does not cover: an app raising itself in answer to an
  accessibility action, a launched app taking focus anyway, a headed browser driven by `browser_*` tools
  (the engine brings tabs forward), and anything run through `exec` or `pty_*`. See `docs/threat-model.md`.
- **`capture_window` on macOS reads the window itself through ScreenCaptureKit, so a window with other windows
  on top of it comes back as itself.** It used to run `screencapture -R` on the window's rectangle, which
  photographs whatever is on screen there, and spawned a process and wrote a temp PNG each time. It now asks
  ScreenCaptureKit (`SCContentFilter(desktopIndependentWindow:)` + `SCScreenshotManager.captureImage`) for
  that one window, found by the window server's id (`CGWindowID`) of the window the `app` and `window_id`
  resolved to (and checked against the owning process), never by frame or by the frontmost app, and encodes
  the PNG in memory. The result has the same shape and coordinate meaning as before (`width`/`height` are
  pixels after the `max_edge` downscale, `downscaled_from` is the native pixel size, `coordinate_mapping` is
  the window's rectangle in points), so `grid`, `ocr_region` coordinates and click mapping are unchanged.
  Every capture result now carries `via`: `screencapturekit`, `screencapture` or (Linux) `xdg-desktop-portal`,
  set from the path that ran. The covered-window `warning` and `covered_by` are given only when the image
  came from the screen (`via: "screencapture"`); a `screencapturekit` image is the window's own pixels and
  carries none. `ocr_region` always reads the screen, so it always carries them. Limits: the ScreenCaptureKit
  path needs macOS 14 (`SCScreenshotManager`) and Screen Recording; without the permission it returns
  `PERM_DENIED` (no fallback, the old path fails the same way). Where ScreenCaptureKit is missing (macOS 12.3
  to 13), times out (10 s), cannot find the window or cannot start a stream for it (error -3811, seen on
  some windows that are on screen), it falls back to `screencapture` and says so in `via`. A window that
  renders no pixels is an error, never a blank image. `capture_screen` and `ocr_region` still use
  `screencapture`. The binary now links ScreenCaptureKit, so it needs macOS 12.3 or later to start. Verified
  on the development Mac (macOS 27, display awake): ScreenCaptureKit returned real images at the window's
  size times the display scale for Finder, Notes, System Settings and Activity Monitor windows that were
  covered 99 to 100% by other windows, and for some Terminal windows; a Finder capture showed Finder's
  content with none of what covered it. It could not start a stream (-3811) for other Terminal windows and a
  Chrome window, which fell back to `screencapture`; why those windows differ is not known. Not verified:
  ScreenCaptureKit on macOS 14 or earlier, and Screen Recording being refused. Adds `objc2-screen-capture-kit`,
  `objc2-core-graphics`, `objc2-image-io` and `objc2-io-surface` (each `Zlib OR Apache-2.0 OR MIT`) on macOS
  only.

- **`browser_act` and `browser_fill_form` say when an action left a dialog or layer in the way.** In a recorded
  MiniWoB++ `login-user-popup` run, focusing the username raised a popup (OK / Cancel) over the form and
  disabled it; `browser_fill_form` still returned `submitted: true`, and the model answered DONE on a form
  that was never submitted. `effects.overlay` (`{text, ref, covers}`) now appears on an act, on each of its
  `steps` and on a fill when a dialog or positioned layer is on top of controls that were clickable before the
  action and were not under it (`covers` counts up to 40 of them, left out when 0), when a modal dialog opened
  (`<dialog>` shown with `showModal()`, or `aria-modal`), or when a new layer fills a quarter of the viewport.
  A toast or menu beside the controls is not reported, and nothing is added when there is nothing to say. On
  `browser_fill_form` it is the only effect reported.

- **`ui_action` gains `show_menu`, `increment`, `decrement` and `pick`; `scroll_to_visible` is accepted for
  `scroll_into_view`.** They act through the accessibility API on the element itself (AXShowMenu, AXIncrement,
  AXDecrement, AXPick), so they need neither the pointer nor the app in front. On macOS the element's own
  action list is read first, and one that lacks the action is an `UNSUPPORTED_OS` error naming what it does
  offer (`'Volume' does not offer AXPick; it offers AXPress (click), AXIncrement (increment)`), never an `ok`.
  `increment` and `decrement` read the value before and after and report `value_after` and `changed`
  (`changed: false` is a control at its limit or one that ignored the step; `verified: false` is a control
  whose value cannot be read). `show_menu` and `pick` have no read-back. On Linux `increment`/`decrement` move
  an AT-SPI Value by its minimum increment, clamped to its range; `show_menu` and `pick` look for an action of
  that name on the element (`menu`, `popup`, `pick`...) and otherwise name the actions it has. Before, a
  context menu needed a right click with the real pointer, and a slider needed a drag. `double_click` and
  `right_click` remain an accessibility press on macOS, which has no distinct press for either.
- **Every input result says which route acted: `input` is `ax` or `hid`.** `ax` is the accessibility API, with
  no pointer movement and the app free to stay in the background; `hid` is synthetic system input that moves
  the real cursor or types into the frontmost app. It is reported by `ui_action`, `set_value`,
  `ui_fill_form` (per field), `mouse_action`, `scroll`, `hover`, `drag_drop`, `keyboard_type` and
  `keyboard_shortcut` (a press), taken from the code path that ran rather than from the request: the backend
  returns the route of `ui_action`. `keyboard_type` on a ref focuses it through the accessibility API but
  types with system key events, so it is `hid`. On Linux `right_click` clicks at the element's centre through
  the portal and is `hid`. The field is absent when nothing needed doing (checking a checked box), and on
  `keyboard_shortcut` `down` and `up`. Before, a result could not tell an agent whether the user's cursor had
  just been taken.
- **`select_text` and `get_selection`.** `select_text` selects text in a text field or text area by `ref` or
  `name`: `all`, `start` and `end` (character offsets, `end` exclusive; equal places the caret), `text` (the
  `occurrence`-th match, from 1), or `from_text` and `to_text` (through the end of the first `to_text` after
  the first `from_text`). The element's text is read first and the request resolved against it. Matching is
  exact, case and accents included, and non-overlapping. On macOS it sets `AXSelectedTextRange`, converting
  characters to UTF-16 units (a character outside the Basic Multilingual Plane is two, a combining accent is
  one of its own), then reads back the range and `AXSelectedText`; an element that reports a different range
  than was asked for fails the call and says what it reports. An element whose selection is not settable is an
  `UNSUPPORTED_OS` error, and there is no pointer fallback: it never drags to select. On Linux it uses the AT-SPI
  Text interface (`set_selection`, `add_selection`; a caret through `set_caret_offset`). `get_selection` reads
  the selection of a ref or name, or of the focused element of the driven app when none is given (macOS only;
  on Linux give a ref), without changing it. Both report `start`, `end`, `length`, `text` (cut at 2000
  characters, with `truncated`), `role` and `input: ax`, refuse password fields, and are standard tier like
  `set_value`. Before, selecting a span meant a pointer drag or shift-arrow chords with no way to confirm
  the result. A live test against TextEdit exists (gated behind `AGENTCTL_LIVE_GUI=1`); which other
  apps honour a set selection is not verified. The Linux code type-checks but has not been run.
- **`drag_drop` takes `path`.** Up to 200 waypoints `{x, y}` (the argument and limit of `browser_act drag`) that
  the drag passes through in order, releasing at the last, with the moves (`steps`, 2 to 100, default 20) spread
  over the segments by length and at least one landing on every waypoint. Give no `to` with a path. Waypoints
  are in the frame of `from`: offsets from the element's top-left when it is a ref or name, else absolute screen
  points. Every waypoint is checked against the allowed window bounds as the endpoints are, and one outside
  denies the whole drag before the button goes down. Before, a desktop drag was a straight line, so a brush
  stroke, a lasso or a curved slider path could not be drawn. The result reports `waypoints`. The path maths
  now lives in `mcp-types` and is shared with the browser engine.
- **`keyboard_shortcut` takes `action`: `press` (default), `down` or `up`.** `down` holds a lone modifier
  (`shift`, `cmd+shift`) or one key, in the same key names `press` takes, and keeps it down across later calls:
  every click, drag, scroll and chord until `up` carries the held modifiers, whatever `modifiers` the call
  names. `up` releases the named keys, or everything held when no `combo` is given. Before, Shift could only
  be held inside a single `modifiers` argument, so a multi-step selection or a press-and-hold was impossible.
  At most 6 keys are held at once; holding one already held renews its time limit. An unknown key is
  `INVALID_ARGS` before anything is pressed. A key is released by `up`; automatically after 30 seconds unless
  renewed (the result's `auto_release_ms`); when any other input call fails (the error says which keys were
  released); when the STOP file appears or a human takes the mouse, although every later call is refused by
  then; and when the server stops. On macOS a held modifier is stamped on the flags of the events agentctl
  posts, not pressed as a key, so it cannot stay latched in the system if the process dies; software that
  reads the keyboard state rather than event flags does not see it, and `keyboard_type` ignores it. A held
  ordinary key is a real key-down. On Linux every held key is a real press through the portal. A crash or
  `SIGKILL` cannot run the release, which on Linux can leave a held key down.

- **`focus_app` with `activate: false` pins an app without raising it, and every window id says which app it
  belongs to.** Driving an app that stays in the background had one door: `get_ui_tree` with `app` pinned it
  as a side effect, and `focus_app`, the tool named for pinning, always raised it, so under
  `foreground = "never"` there was no refused-or-working way to say "this is the app". `focus_app {app,
  activate: false}` sets the same pin and writes nothing to the app (no `AXFrontmost`, no window touched); its
  result is `{target, pid, activated: false}`. Under `never` that form is allowed and the raising form
  (`activate` absent, `true`, or anything that is not the boolean `false`) is refused with a suggestion
  naming it. `list_windows` with `app` lists a named app whether or not it is frontmost, and each window now
  carries `pid` and a `state`: `onscreen`, `minimized`, or `offscreen` (another Space, or the app is hidden;
  the window server lists these, the accessibility tree of the app does not). `capture_window` and
  `ocr_region` take `app` with `window_id` (the app whose list the id is from; default the pinned app, else
  the frontmost), and their results carry `window: {app, pid, title, from}`. `list_windows`, `get_ui_tree`,
  `find_elements`, `ui_extract`, `control_window`, `menu_open`, `menu_invoke`, `menu_list` and
  `handle_dialogs` results name the application they used and why (`target`/`from` is `named`, `pinned` or
  `frontmost`, so a call that named no app shows whether it got the pin or whatever was in front);
  `ui_action`, `set_value`, `select_text` and `get_selection` by ref say the `app` the ref belongs to. A
  `list_windows` with no app and nothing pinned says `scope: "all_apps"`. An app name that matches no running
  application, and a `window_id` the app does not have, are errors that name the app (and, for the id, how
  many windows it has); none falls back to another app. `capture_window` and `ocr_region` on macOS read
  the screen area, so a window that is minimised or off screen is an error (nothing of it is drawn), and one
  that other applications' windows cover by 5% or more of its area (read from the window server) is still
  captured, with a top-level `warning` and `covered_by: [{app, fraction}]` (largest first, fractions of the
  window's area, capped at 1) saying whose pixels are in the image in place of the window's own. An agent
  that sees `warning` should not read those parts as the window's content; on Linux a covered window
  cannot be told and shows what covers it.
  Unverified: the live test (`a_background_app_is_pinned_listed_and_read_by_window_id_without_taking_the_foreground`)
  is written and has not been run; nothing here has run on Linux, which type-checks only; no capture was
  taken, so the `screencapture` half of a window read, and a window on another Space (none was open when
  this was written), are untested.

- **A locked screen is reported, not acted on.** While the lock screen is up (or another user's session has the
  console after fast user switching) macOS discards the key and pointer events a program posts, and the post
  itself still succeeds, so the `hid` route reported an action as done that never arrived. Two live tests showed
  it: typed text never appeared and the HID idle clock did not move. Every call that would post key or pointer
  events now checks the session first (`CGSessionCopyCurrentDictionary`: `CGSSessionScreenIsLocked`,
  `kCGSSessionOnConsoleKey`, a local lookup) and fails before anything runs with `ACTION_FAILED` ("the screen is
  locked ... nothing was done", suggestion beginning `retryable:`). That covers `mouse_action`, `hover`, `scroll`,
  `drag_drop`, `keyboard_type`, `keyboard_shortcut` (a key release is let through), an `input_steps` batch
  containing any of them (refused whole, so no earlier step half-runs), the per-process `pid_event` route, and
  the window calls that raise an app (`focus_app`, `menu_open`, ...). Under `foreground = "polite"` it refuses at
  once instead of waiting for idle and then posting into the void. The same check sits at the input backend
  boundary, so a path the tool layer did not foresee fails too. Measured with the screen locked: `list_windows`
  and `menu_list` still answer, `get_ui_tree` and `observe` return `ok` with only the application root
  (flagged `partial`, with a hint that blames slowness), `capture_screen` fails ("no displays"), and
  `capture_window` and `ocr_region` fail (ScreenCaptureKit -3811, then `screencapture` "could not create image
  from rect"); the accessibility actions `ui_action` / `set_value` are not refused but were not exercised while
  locked. A display that is only asleep with the screen unlocked is not treated as blocking: whether a posted
  event wakes it is not documented and could not be measured. `agentctl doctor` prints a `screen:` line
  (ok, LOCKED, OFF CONSOLE or unknown), `idle_status` gains `screen_locked` and `input_state`, and the GUI
  live suites now skip, saying why, when the screen is locked. On Linux the input engine has no cheap reader
  (logind is on the system bus the input backend does not hold), so it reports unknown and does not block;
  `idle_status` and `doctor` read logind's `LockedHint` alongside GNOME's screen saver.

- **`foreground = "never"` and `"polite"` now cover the browser engine.** Before, a visible (headed) Chrome was
  outside the setting: the engine sent `Page.bringToFront` on every connect, `browser_tabs open` went through
  `/json/new` (which activates the tab it makes), and a visible launch started a window. Inventory of what can raise
  a window or take focus: `Page.bringToFront` on connect and when a screencast starts; `/json/new` and
  `/json/activate` in `browser_tabs`; `Target.createTarget` for a branch's context; starting Chromium, and
  Safari through safaridriver. Under `never` (and under `polite`, which applies the same rule because the browser
  engine cannot borrow and return the desktop) the page is not brought forward (`foregrounded` is false), tabs
  and branch tabs are created with `background: true`, `browser_tabs activate` is refused for a visible browser,
  and a visible Chromium or Safari launch is refused, all `POLICY_DENIED` naming `launch.headless: true` and
  attaching to a running Chrome; headless is unaffected. Focus-dependent pages keep working because one idle
  DevTools session per tab holds `Emulation.setFocusEmulationEnabled` while the browser is connected (a session
  per call is not enough: Chrome ends the emulation when any session that set it closes, so the page saw a blur
  after every call). `allow` is unchanged. Verified against a real headless Chrome by reading the commands off the
  wire and with a focus fixture (`docs/fixtures/focus_probe.html`). **Measured:** a visible Chrome (throwaway
  profile, attach mode) driven under `never` on macOS left Finder frontmost throughout
  (`live_browser_headed_never.rs`, 3 of 3 runs on an unlocked screen). **Not measured:** Safari, Linux, and
  attaching to the Chrome window a person uses every day (its window order). A background tab in a visible browser may be hidden to the page and throttled.

- **`--access <level>` and `AGENTCTL_ACCESS`: choose the permission level when starting the server.** `ask`,
  `auto` or `bypass`, as `policy.access` in the config file. The strictness order is `ask` < `auto` < `bypass`
  (`ask` is the most restrictive). When the config file sets `access`, the flag or variable may only move it
  toward `ask`: asking for a looser level is a startup error that names both values, not a silent ignore.
  When the file sets nothing, the flag sets the level. The variable does not: every level turns on more than
  the default, and an inherited variable is too easy to leave set by accident, so `AGENTCTL_ACCESS` on its own
  is a startup error naming the file and the flag. An unknown value is a startup error. When both the flag and
  the variable are given, the flag stands.
- **`policy.dry_run = true` and `--dry-run`: rehearse at any level.** Reads run, and anything that would change
  something reports what it would have done and does not do it, under `ask`, `auto` and `bypass` alike. Dry run
  can only make a session do less. `mode = "dry_run"` keeps working as the older spelling, and is no longer
  lost when `access` is set (it used to be overwritten).
- **`agentctl doctor` prints the policy in three lines**, `access:` (the level, what it means and whether it came
  from the config file, the `--access` flag or `AGENTCTL_ACCESS`), `scope:` (categories, apps, filesystem
  roots, commands, hosts, browser origins) and `stops:` (kill switch path, human override, `foreground`), plus
  `dry run: on` when it is. It also lists, under `ignored keys:`, every config key nothing reads with the
  nearest key it knows (`policy.catgories (did you mean policy.categories?)`), and under `config warnings:`
  every older spelling and no-op setting found at load. The same warnings are logged at startup.
- **`config.example.toml` is reorganised around three ideas**: `access` (the one permission choice), scope (what
  it may touch) and safety stops, with a "Start here" block of a few lines and everything else under Scope,
  Safety stops and Advanced. Every setting the loader reads is documented in it, and a test now keeps it so.

### Changed

- **The pure-Rust OCR engine is `ocrs` 0.14 on `rten` 0.28** (was 0.13 on 0.26). The two ONNX model files are
  the same ones as before and pinned by the same SHA-256, so cached models stay valid and nothing is
  downloaded again; `AGENTCTL_OCR_MODELS` and `AGENTCTL_OCR_MODELS_UNVERIFIED` behave as before. Recognised text
  was identical on six rendered test pages and recognition time within 3 %. The update also drops three
  dependencies (`flatbuffers`, `anyhow`, `bitflags` 1.x).
- **Window, menu, dialog and capture calls that name an `app` act on it without pinning it.** `list_windows`,
  `menu_list`, `handle_dialogs` and the rest resolved `app` through the call that pins, so reading one app's
  menu silently changed which app a later call with no `app` acted on. Only `focus_app` and the
  `get_ui_tree` family (`get_ui_tree`, `find_elements`, `ui_extract`) pin now. `wait_for` with `app` watches
  it without pinning it (see Fixed). A flow that named an app in `list_windows` or `menu_list` and then
  called `get_ui_tree` with no `app` expecting it now reads the pinned or frontmost app, and the result's
  `target` says so.
- **A `window_id` is the window's place in its own application's list, the same in `list_windows` with or
  without `app`.** `list_windows { app }` numbered windows by the app's accessibility order and `list_windows`
  with no app by the window server's, so one window had two ids, and `capture_window` read either against the
  frontmost app's accessibility windows. Both lists now come from the window server (the same windows and
  numbering whether or not Accessibility is granted; titles it redacts are filled from the app when
  Accessibility is granted). For an app whose windows the two orders agree on, ids are unchanged; where they
  differed, `list_windows { app }` ids now match the no-app list. Windows under 120x80 points and untitled
  surfaces (when titles are readable) are not listed, as they were not in the no-app list. `minimized` is
  true only for a window the app reports minimised.
- **`allowed_apps` applies to every window tool that names an app.** It gated `launch` and `focus_app`. It now
  also gates `list_windows`, `control_window`, `menu_open`, `menu_invoke`, `menu_list`, `handle_dialogs`,
  `wait_for`, `capture_window` and `ocr_region` when they name an `app`, by the name given and by the
  application it resolves to. It still does not gate `get_ui_tree`, `find_elements` or `ui_extract` with
  `app`, which read an app the list does not cover; that is unchanged and recorded in
  `docs/threat-model.md`.

- **With `access` set, `categories` now narrows it instead of being discarded.** Before, writing
  `categories = ["vision"]` beside `access = "auto"` was silently ignored and every category was on. Now only
  the categories you list are on (an empty list turns all of them off); leave `categories` out and the level
  turns every category on, as before. This can only make a session do less, but if you have a stale
  `categories` line beside `access`, that line now takes effect: remove it, or list what you want. A tool in a
  category that is not listed is neither advertised in `tools/list` nor callable. Configs without `access` are
  unchanged byte for byte.
- **Under `access`, a config that also carries `mode` or `enable` now says so.** Both were always ignored once
  `access` was set, with no word. Each is now named in a startup warning (`mode = "dry_run"` is not a conflict;
  it is the older spelling of `dry_run`). Without `access` they work exactly as before and warn about nothing.
- **When two spellings of one setting disagree, startup stops.** `policy.demo` and `input.demo`,
  `policy.role` and `policy.default_role`, `[invariants]` and `[policy.invariants]` and the other aliases
  listed below were read into a hash map, so which one won was arbitrary and could change between runs. Two
  spellings with the same value are fine (and warn); two with different values are now a startup error naming
  both.
- **`agentctl --role serve`, and any flag that takes a value, no longer swallows a command name.** `--role`,
  `--demo-speed` and `--access` take the next argument or `--flag=value`; a missing value, or a command name
  where the value should be, is a usage error (`--role=serve` if a role really is called that).

### Deprecated

Each of these still works exactly as before, loads with a startup warning that names what to use, and appears
under `config warnings:` in `agentctl doctor`. Nothing here is more permissive than it was.

- **Older spellings of one setting** (use the first): `policy.demo` (was `input.demo`, `demo.enabled`),
  `policy.demo_speed` (was `input.demo_speed`, `demo.speed`), `policy.role` (was `policy.default_role`),
  `[invariants]` (was `[policy.invariants]`), and in a custom role `require_consent_for` (was
  `require_consent_for_tiers`, `require_consent_tiers`, `require_consent_for_tools`, `require_consent_tools`).
- **The roles `operator`, `admin`, `auditor`, `tester` and `superadmin`** are no longer documented and still
  resolve to the same profiles. `operator` is `access = "ask"`; `admin` and `superadmin` grant nothing, so drop
  the role; `auditor` is `readonly`; `tester` is `qa`. The documented presets are `readonly`, `qa`, `browser`
  and `browser-core`. Custom `[roles.<name>]` tables are unchanged and now live in the advanced part of the docs.
- **`policy.mode` and `policy.enable`** are the older way to say what `access` says. They keep working when
  `access` is not set; prefer `access`.

### Removed

- **`max_denials` (`policy.max_denials` and `roles.<name>.max_denials`) never did anything, and the denial
  budget behind it is gone.** The documentation said a session would be aborted after that many denied calls.
  Nothing ever acted on the count: the call that recorded each denial ignored the answer, so no session was
  ever aborted. The counter, its plumbing and the `Policy::record_denial` and `denial_count` methods are
  removed. A config file that still sets the key loads, with a warning that it has no effect and never had;
  remove the line. Denials are still audited (`decision: "deny"`), and the audit log's schema is unchanged.

### Fixed

- **`idle_status` never reported a locked screen on macOS.** It looked for `CGSSessionScreenIsLocked` with
  `ioreg -k`, which matches nothing (the key lives inside `IOConsoleUsers`), so `locked` was always `false`,
  including while the screen was locked. It now reads the session dictionary.

- **macOS: `launch` right after `close_app` failed with "could not launch".** The closed app was still known to
  LaunchServices for a moment and `open -a` answered error -600, so an agent that closed TextEdit and opened it
  again was told it could not be started. `launch` (and the background launch used under
  `foreground = "never"`) now retries that one error for up to 8 seconds, and any other failure carries
  `open`'s own message instead of a bare "could not launch".
- **macOS: a minimised window was listed as `onscreen`.** The window server leaves out the on-screen key for a
  window that is not drawn, and its absence was read as "on screen", so `minimized` was always false.
  `control_window restore` with no `title` now un-minimises a minimised window; it used to act on the main
  window, which is the one in view, and did nothing.
- **macOS: `ui_action` on an element that lacks the action said only "unsupported on this element".** Some
  apps answer a missing action with the attribute-unsupported code instead of the action one, and that path
  skipped the message that names what the element does offer. Both now say, for example, that TextEdit's text
  area does not offer AXScrollToVisible and offers only AXShowMenu. A disabled popup says what to do next.

- **`control_window`, `menu_open` and `menu_invoke` acted on the frontmost app whatever `app` said.** The
  argument was accepted and discarded (`_app`), so `control_window { app: "TextEdit", action: "close" }` closed
  a window of the app in front, and `menu_invoke { app, path }` pressed a menu item in it. They now act on the
  named app, or the pinned one, or the frontmost, and the result's `target` says which. A title that matches
  none of the app's windows is an error naming the app and how many windows it has.
- **`capture_window` and `ocr_region` with a `window_id` read the frontmost app's accessibility windows, never
  the app the id came from.** With TextEdit pinned behind Terminal, `window_id: 0` was Terminal's first
  window, and the capture was of Terminal. They also photographed whatever was on screen at the window's
  coordinates, so a window covered by another app's came back as that app's pixels. See Added for the id and
  the covered-window warning. Seen on the development Mac: Finder's only window listed as `onscreen` and covered 100% by
  Terminal, which a capture would have returned as Finder's.
- **`wait_for` with `app` pinned that app and replaced the elements `@eN` refs pointed at.** Its polls took
  ordinary snapshots, so `wait_for { app: "Finder", ... }` between `get_ui_tree` of TextEdit and `ui_action
  @e5` left `@e5` resolving in Finder's tree, where an element with the same role and name would be acted on.
  The polls are now passive reads: nothing is pinned and the last walk's refs are kept. An `expect` clause is
  not passive (it installs what it last saw), so it is unchanged.
- **An app whose windows were all minimised or on another Space could not be named unless its process name
  matched exactly.** Name resolution looked only at on-screen window owners, then `pgrep -x`. It now also
  looks at off-screen owners, exact before substring.
- **Linux: a `window_id` from one app's list could resolve to a window of another app.** Ids indexed the last
  `list_windows` result, and an id past the end re-listed every app and returned that list's entry. Ids are
  now each window's place in its own app's list and are read against the app named, else the pinned one, else
  the active one, with an error naming the app and its window count past the end. Naming an app in a window
  or menu call also pinned it there, as on macOS, and no longer does. Type-checks only; not run.
- **`browser_fill_form` no longer reports `submitted: true` for a submit that did not happen.** It clicked the
  submit control with a script `click()`, which a disabled control ignores and a person could not make on a
  control under a dialog. It now looks first: a disabled control, or one with a dialog or positioned layer
  over its centre, is not clicked, and the result is `submitted: false` with `submit_blocked` (`disabled`, or
  `covered by "<text of the layer>"`). The fields are still filled and the call still succeeds.
- **A plain-word query clicked the instruction sentence instead of the inbox row.** On MiniWoB++ `email-inbox`,
  `browser_act` `click query:"Gilli"` (no `by`) matched the sender's name twice, in the task's instruction and
  in the row, and clicked the instruction. A text match counted as clickable only when it was, or sat inside,
  a link, button, input, role or `onclick`; the row is a `div` with a pointer cursor and a script handler, so
  both matches tied and document order picked the sentence. Matches now rank control, then text a pointer
  cursor or a `tabindex` marks as clickable (the cursor is inherited, so the words inside a row show it), then
  prose. Exact before partial and visible before hidden still come first, `index` walks the same order, and
  `matches` is the same count.
- **`within` searched only the first element its selector matched.** `within:".email-thread"` (one match per
  row) answered NOT_FOUND for text that was in the second row. `within`, as a CSS selector or an XPath, now
  scopes the search to every element it matches, in document order, for `css`, `xpath`, `text`, `role` and the
  no-`by` fallback; `index` and `matches` count across all of them. A `within` that matches nothing is still
  an error and never widens the search.

### Security

- **tokio 1.54.1**, for RUSTSEC-2026-0386 (`AsyncFd::new` and related constructors violate I/O safety), and
  async-executor 1.14.1 in place of the yanked 1.14.0. The "unmaintained" notices for `zvariant`,
  `zvariant_derive` and `zbus_names` (folded into the zbus repository, not vulnerable, Linux only) are ignored
  by id in `deny.toml` until zbus, ashpd and atspi release against the merged crate.
- **sha2 0.11** for the audit log's hash chain and the OCR model pins (digests are unchanged), with libc,
  serde_json and wl-clipboard-rs patch updates.

## [0.3.0] - 2026-10-10

### Added

- **`scroll` takes `modifiers`.** `["cmd"]`, `["ctrl"]`, `["shift"]` or `["opt"]`, the names `mouse_action` and
  `drag_drop` already accept, are held for the whole wheel gesture, so Cmd+wheel (macOS) or Ctrl+wheel zooms a
  document, map or canvas and Shift+wheel scrolls sideways where an app binds it. Before, the wheel always
  arrived bare and an agent had no way to zoom with the pointer. On macOS the modifiers ride on the wheel
  event's flags, as they do on a modified click; on Linux the keys are pressed through the portal before the
  wheel steps and released after them, even when a step fails. An unknown name is `INVALID_ARGS` before
  anything moves, and the result reports `modifiers`.
- **Modifier keys on `browser_act` pointer actions.** `modifiers: ["shift" | "ctrl" | "alt" | "meta"]` holds
  those keys through a click, double or right click, hover, mouse_down/move/up, drag or scroll: every mouse event
  carries the CDP modifier bits (`shiftKey`, `ctrlKey`... in the page) and the keys themselves go down before the
  pointer sequence and up after it, so a tool that listens for the Shift key sees it held. Before this an agent
  could not shift-click to extend a selection, shift-drag to constrain a shape or ctrl+wheel to zoom a canvas;
  the result reports `modifiers`. Each entry of a `steps` batch may carry its own. Chrome only.
- **Drag control for brush strokes and freehand shapes.** `drag` takes `moves` (2 to 200, default 12),
  `duration_ms` (how long they take in all, at most 10000; 15 ms a step by default), `button` (left, middle or
  right) and `path`, up to 200 waypoints `{x, y}` that the drag passes through in order before releasing at
  the last, with the moves spread over the segments by length and at least one landing on every waypoint. The
  waypoints are in the frame of the `to_ref`/`to_query` element when one is given, else of the source element
  (a canvas's bitmap pixels, so a stroke on a canvas ref is in its pixels end to end), else viewport px. `dx`/`dy`, `to_x`/`to_y` and `to_ref`/`to_query` work as before. The result says `moves`,
  `duration_ms` and, with a path, `waypoints`. A CVAT-style lasso or brush stroke was a straight line before.
- **`docs/fixtures/whiteboard.html`:** a dependency-free drawing page (a 2x CSS-scaled bordered canvas that
  records pointer events, strokes and polygons, an SVG rect with a draggable vertex, wheel and modifier-key
  recording) with live tests in `crates/mcp-browser/tests/whiteboard_live.rs`.
- **`browser_screenshot` reads text: `ocr` and `find`.** A canvas app, an image-based UI or an annotation
  tool shows its text as pixels: `browser_snapshot` sees one element and `browser_query` finds nothing, so
  the only way to click "Save" was a screenshot the model read with its own eyes, every turn, guessing a
  coordinate off the picture. `ocr=true` returns every visible line with `bounds` and `center` in viewport
  CSS px, and `find="Save"` returns only the lines containing it, best first (whole line, then whole word,
  then a fragment), each with the `x`,`y` to pass to `browser_act`; no match is `ok` with `count 0` and a
  hint naming the nearest lines read. Boxes are mapped from image pixels through the capture's scale, so
  they are right on a 2x display and for a `ref` capture, and the image stays out of the result unless
  `image=true`. The browser-side twin of the desktop `ocr_region`: same argument names, same result shape.
  The recogniser is the pure-Rust `ocrs` engine, now in `mcp-vision` behind its `ocr` feature and shared
  with the Linux desktop backend, so one model download (about 20 MB, on first use into the state
  directory or `$AGENTCTL_OCR_MODELS`) serves both. Labels side by side on one row, which the recogniser
  reads as one line, are split where the words are a row height or more apart, so each gets its own
  centre; this applies to `ocr_region` on Linux too. Fixture: `docs/fixtures/ocr_canvas.html`.

### Changed

- **Audit key generation uses `rand_core::OsRng` directly.** The audit log signing key was the only thing in
  the workspace that used `rand`, and only to reach the operating system's randomness. `rand_core` 0.6 is what
  `ed25519-dalek` is built on and already in the tree, so `rand`, `rand_chacha` and `ppv-lite86` leave it:
  fewer crates, and one version of `rand_core` whatever `rand` does next. Keys come from the same source as
  before.

### Fixed

- **macOS: `mouse_action move` with a button held posted a plain `MouseMoved`.** `down`, `move`, `up` is how
  an agent composes a drag `drag_drop` cannot express (a pause mid-path, a hover over the target before the
  drop, a second button), but the moves went out as `MouseMoved`, so anything that tracks `LeftMouseDragged`,
  `RightMouseDragged` or `OtherMouseDragged` (Finder, sliders, canvases, text selection) saw a click and an
  idle pointer; only `drag_drop` sent real drag events. The backend now remembers which buttons it holds and,
  while one is down, posts the matching `*MouseDragged` event with the button number and pressure set, for
  `move`, `hover` and the positioning move before `scroll`. The record is released on `up`, when `drag_drop`
  ends, and when the human-override brake fires, so a takeover never leaves a later move claiming a drag.
  Linux already carried the held button through the portal and is unchanged.
- **Canvas coordinates on `browser_act` are bitmap pixels.** The docs said a canvas target's `x` and `y` were
  its pixel coordinates, but they were added to the bounding box as CSS px, so on a canvas that is CSS-scaled
  or has a border the click landed on the wrong pixel (a 400-wide canvas styled 800px wide was clicked at
  bitmap (50, 25) for x 100, y 50). They are now mapped through the content box by `clientWidth / width`,
  as published canvas regions already were, and a region's offsets count from its own corner in bitmap pixels.
  `click_at`, `at`, `from`, `to` and `scroll_at` add `pixel` (the bitmap pixel) on a canvas, and `hit` names
  the pixel under any point that lands on one.
### Security

- **macOS release binaries are signed and notarized.** Each macOS archive's `agentctl` is signed with a
  Developer ID certificate (Techbeaver IT Systems LTD), hardened runtime and timestamp, and submitted to
  Apple's notary service by the release workflow; Gatekeeper runs a browser download without the
  `xattr -d com.apple.quarantine` step that 0.2.1 needed. The Sigstore build attestation remains the proof of
  which workflow and tag produced an archive.
- **OCR model files are pinned by SHA-256.** The two `ocrs` models are downloaded once and parsed
  in-process; before, a modified or substituted file, on disk or from the download, would have been
  loaded. Every file is now verified against a pinned digest before anything parses it, whether it was
  just fetched or was already there: a mismatch is refused and named, a bad download or a bad file in the
  state directory is deleted so the next call fetches afresh, and a file under `AGENTCTL_OCR_MODELS` is
  left for the operator to replace. `AGENTCTL_OCR_MODELS_UNVERIFIED=1` loads operator-supplied models
  without the check, for models of their own; it does nothing for the default directory.

## [0.2.1] - 2026-10-10

Most of this release comes from two field sessions: a QA run and a demo recording against a server-rendered
HTMX app, where several browser tools answered before the page had done what the agent asked, and a job
application on a React site, where custom dropdowns, controlled text fields and the CV upload could not be
operated.

### Added

- **A Claude Code plugin and a usage skill.** `/plugin install agentctl --marketplace paooxide/ui-draw-mcp`
  registers the server and adds `skills/agentctl/SKILL.md`: observe cheaply, act on refs, verify with
  `expect`, and stop at a policy refusal, consent denial or kill switch instead of routing around it. The
  skill never tells an agent to change agentctl's own policy.
- **`browser_act` pointer actions:** `double_click`, `triple_click`, `right_click`, `scroll` and `drag`, all
  real CDP input. `x` and `y` place a click or hover at a point (offsets inside the target, so a canvas is
  clicked at its own pixel coordinates; viewport px without one), and the result's `click_at` says where it
  landed. `drag` goes from the target (or a point) to `to_ref`/`to_query`, `to_x`/`to_y` or `dx`/`dy` in held
  steps, which covers sliders, sortable lists and selecting text, and plays native `draggable` elements back
  through Chrome's drag interception. In a MiniWoB++ run canvas, drag and slider tasks failed for want of
  these. Chrome only.
- **`browser_act` effects:** every result now carries `effects`, holding only what the action changed, so the
  model does not spend a turn on a snapshot to find out. `url` and `title` when they changed, `new_tab`
  (`target_id`, `url`) for a tab or popup the action opened, `dialog` (`type`, `message`, `answered`) for an
  alert, confirm or prompt, `appeared` and `disappeared` (at most 5 each, a short role, name and ref per
  element) for dialogs, menus, listboxes, toasts, high z-index overlays and interactive elements that became
  visible or went away, and `focus` for the newly focused element. It is one bounded page script before the
  action and one after, read about 120 ms later (not at all extra when `wait_after` is `settle`); a navigation
  reports `url` and `title` only, and an action that changed nothing has no `effects`. In a MiniWoB++ run
  login-user-popup (a popup appears mid-task and must be dismissed) and email-inbox (clicking reply reveals a
  form) cost an extra snapshot per step for want of it. Chrome only.
- **`browser_act` `mouse_move`, `mouse_down`, `mouse_up` and `hold_ms`.** `mouse_move` (and `hover` with `x`
  and `y`) moves the real pointer without clicking, so mouseover, mousemove and `:hover` fire. `mouse_down`
  and `mouse_up` take an element or a point and a `button`; moves in between carry the held button, so a drag
  or hold the page implements itself can be composed, and a bare `mouse_up` releases where the pointer is.
  `hold_ms` (at most 10000, an error above) holds a click between press and release, or a key between down and
  up. Spellings `move`, `mousemove`, `mousedown`, `mouseup`, `press_and_hold` and `long_press` (a click held
  800 ms) are accepted. Chrome only; Safari says so.
- **`browser_act` `hit`.** A pointer action given `x` and `y` answers with the element under the point
  (`tag`, `id`, short `text`, `canvas`), so a click that landed on the wrong thing, or on a canvas, shows it.
- **A labelled coordinate grid on screenshots.** `browser_screenshot`, `capture_screen` and `capture_window`
  take `grid` (and `grid_step`, default 100, minimum 25) and draw lines on the returned image labelled with
  the numbers the click tools take: CSS px for `browser_act` x/y, screen points for `mouse_action` (the
  window's own position included for `capture_window`). Models place clicks badly from a bare image,
  worse when it is 2x or downscaled; the grid is in click space whatever the scale, and the result gives
  `scale` (image px per unit), `grid_step` and `origin`. An element screenshot is labelled with the
  element's viewport position, not from 0. Drawn on the decoded PNG, so the page is untouched. The drawer
  is `mcp_vision::grid`, using the `png` and `base64` crates already in the tree through `mcp-linux`.
- **`ocr_region` `find`.** `find: "Save"` (case-insensitive, whitespace-collapsed; `exact` for a line that is
  exactly the text) returns only the matching lines, best first (a label before a sentence that mentions it),
  each with `x`,`y` to pass to `mouse_action` and its box, instead of the whole page of text: "click the Save
  label in a custom-drawn app" is two small calls. The recogniser reports lines, so a match inside a line
  has an estimated box. `window_id` reads one window; boxes are screen points either way.
- **Desktop name misses point at OCR.** When `ui_action` (and the other by-name tools) or `find_elements`
  find no element and the app's tree has fewer than 12 elements, the hint says the app may draw its own UI
  and to call `ocr_region` with `find` and a `window_id`, then `mouse_action`. Wording only: nothing is
  captured or read on the model's behalf, so the vision tools' own policy still applies.
- **Desktop: choose from a popup button or combo box by its text.** `ui_action select` and `ui_fill_form`
  take `option`; on macOS the popup is opened and the matching menu item pressed, on Linux the AT-SPI combo
  box's item is invoked. Before, both backends dropped `option`, so a popup button only opened and the call
  said ok. The result reports `selected` and `changed`; no match lists the options, and a disabled item or a
  control that shows something else afterwards is an error.
- **Desktop: target by name.** `ui_action`, `set_value`, `keyboard_type`, `scroll`, `hover`, `drag_drop` and
  `mouse_action` take `name` (with an optional `role`) besides `ref`, ranked exact, prefix, then substring,
  controls first, in tree order; a miss lists the closest candidates. `find_elements` gained role synonyms
  (popupbutton, combobox, textfield, and so on), matches values, and hints at close names when nothing
  matches. `mouse_action` and `drag_drop` take a point relative to an element, and numeric arguments may be
  given as strings.

- **More keys for `browser_act press`:** ArrowDown, ArrowUp, ArrowLeft, ArrowRight, Home, End, PageUp,
  PageDown, Backspace, Delete and Space, sent as real key events, so a listbox or a react-select menu can be
  driven from the keyboard. Before, only Enter, Escape and Tab worked.
- **`browser_screencast`**, video of a tab: `start` polls `Page.captureScreenshot` (JPEG, 1 to 30 fps,
  default 15) on a session of its own that keeps focus emulation on, `stop` writes an ffconcat with the real
  frame timestamps and encodes an H.264 mp4 with ffmpeg, `status` lists what is running. Recordings survive
  a navigation, end at `max_seconds` (default 300, at most 1800) or when the tab closes, and stop with
  `browser_disconnect`. The showcase cursor is in the page, so it is in the video. Without ffmpeg (looked
  for on `PATH`, then in Homebrew and `/usr` locations) the frames are kept with the command to encode them.
  `browser_record` remains flow recording; it never made video, which its name suggested.
- **`browser_screenshot` `save: true`** writes the PNG and returns `{path, width, height, bytes}` instead of
  the image. Files go only under agentctl's state directory (`media/screenshots`, `media/screencasts`),
  with names agentctl picks; the newest 200 screenshots are kept.
- **`browser_showcase` `cursor_size`** (16 to 96 px, default 32), and the result now says whether the
  overlay is really on the page: `rendered`, plus a `warning` when it is not (no tab, no `<body>` yet, the
  script threw). `browser_act` adds `showcase_rendered`.
- **`browser_act` `wait_after: "settle"`** waits for what the action started: a navigation, then
  `htmx_settled` when the page has htmx, then the network going quiet, all bounded by `timeout_ms`. The
  result adds `navigated`, `requests_started` and `settled` (plus `settle_error` when the wait ran out; the
  action itself still counts as done). A click that starts nothing costs about 2 s here. Chrome only.
- **`browser_upload`**, files into a page's `<input type=file>`. A real click opens the OS file chooser,
  which agentctl cannot drive, so a CV or any other attachment could not be sent; `browser_fill_form` treated
  the input as text and profile restore skipped it. It takes the same locator as `browser_act` plus `paths`
  (1 to 10 files, 50 MiB each), follows a `<label>` to its input (or takes the one file input inside an
  element), refuses several files for an input without `multiple`, and sets the files with
  `DOM.setFileInputFiles`, so Chrome fires trusted `input` and `change` events. It returns names and sizes,
  never contents. Dangerous tier, because a page can read whatever is attached: every path goes through the
  fs engine's jail (`fs.roots`, with credential stores refused) and only the resolved path is used, so with
  no `fs.roots` it refuses. Chrome only; the Safari engine returns Unsupported.
- **`browser_act` `scroll: "none" | "nearest" | "center"`**, default `nearest`.
- **`browser_eval` `timeout_ms`** (default 10000, 100 to 60000) and **`detached`**. On a timeout Chrome is
  told to terminate the script, and the error says that timers and promises it already scheduled may still
  run. A script that navigates the page now returns `{navigated: true, value: null}` instead of failing with
  "Inspected target navigated or closed". `detached` returns `{started: true}` without awaiting a returned
  promise.
- **`browser_connect` `launch.args`** (an allowlist of display, language and pacing flags; anything else is
  refused with the list) and **`launch.background_throttling`**.
- **Built-in `browser` role** (`--role browser`, `AGENTCTL_ROLE=browser`, `policy.role`): only the browser
  tools are advertised, 27 instead of 124. `access` turns every category on, so before this the only way to
  get a browser-only list with `access` set was a custom role.
- **Built-in `browser-core` role** (`--role browser-core`, `AGENTCTL_ROLE=browser-core`): the 12 browser
  tools a page task needs (`browser_connect`, `browser_tabs`, `browser_navigate`, `browser_snapshot`,
  `browser_query`, `browser_act`, `browser_fill_form`, `browser_wait`, `browser_screenshot`,
  `browser_extract`, `browser_dialog`, `browser_upload`). The tool list is re-sent every turn, so a smaller
  one is cheaper on every call: about 3.0k tokens against 6.5k for `browser` (4.5k before the descriptions
  below were shortened). `browser_upload` stays dangerous-tier; listing it only lets an operator enable it.
- **`ToolDescriptor::details`**, docs-only text that `tools/list` does not send and
  `agentctl tools --markdown` prints after the description.
- **`browser_snapshot` `diff: true`** (or `since: "last"`) returns what changed since the last snapshot the
  server returned for the tab: `added` nodes, `removed` refs and `changed` nodes with `{field: [was, now]}`,
  `unchanged: N`, and `url`/`title` when they moved. Nodes are matched by ref, then by tag, role and name when
  refs shifted (the shift is reported as a change of `ref`). With no earlier snapshot, another document, another
  mode or `root_selector`, or a diff no smaller than the page, the reply is the full snapshot marked
  `diff: "full"` with a `reason`. Memory is the last snapshot of at most 16 tabs, dropped on tab close and
  disconnect. The default stays the full snapshot. In MiniWoB++ Haiku runs the full page was re-sent after every
  action; on a 21-node form a one-field diff is 132 bytes against 2120.
- **`browser_act` `steps`**: up to 20 actions in one call, run in order through the same path as a single act,
  stopping at the first failure. The result is `{ran, total, failed_at, steps: [...]}` and the call is ok only
  if every step was; a failed batch is an error that still carries the per-step results. `snapshot: "diff"`
  (or `"full"`) adds the page after the last step. `target_id` comes from the call; `wait_after` and
  `timeout_ms` are inherited by steps that set none; `secret` is not, so each step that types one flags it
  (the audit log redacts per step). Filling a form took one call per field.
- **Role locators:** `by: "role"` takes the ARIA role as `query` and an optional accessible `name` (a
  case-insensitive substring, exact matches first). Roles are explicit or implied (button, link with href,
  textbox, checkbox, radio, combobox/listbox, heading with `[level=n]`, img, list, listitem, tab, menuitem,
  option, dialog and the landmarks); the name comes from aria-labelledby, aria-label, an associated label,
  alt, title, placeholder or the text. The spellings models copy from Playwright and Testing Library are read
  as role queries without a `by`: `role=button[name="Submit"]` (name may be `/sub/i`),
  `getByRole('button', { name: 'Submit' })` and the snapshot's `button "Submit"`. A miss lists the elements
  that do have the role, nearest name first. Benchmarks written for Playwright send these spellings.
- **Shadow DOM and same-origin iframes are searched** by CSS, text and role alike. Refs to elements in a
  frame are `<frame ref>::frame/<path>` (chained for nested frames) and resolve again on later calls; refs
  outside frames are unchanged. `browser_snapshot` lists interactive elements in open shadow roots and
  same-origin frames, with boxes in page coordinates (the frame's offset, border and padding added), and
  names the frames it could not read as `frames_skipped` (`ref` and `src`). A locator miss says when a
  cross-origin frame was not searched. Before, text matching stopped at a shadow root and any frame was
  invisible to a selector.

### Changed

- **Snapshot nodes carry control state.** A text field's `value` (never a password's), a checkbox or radio's
  `checked`, `expanded` (`aria-expanded`, or a `<details>` summary) and `selected` for `aria-selected` are
  listed when present; before, what a field held was visible only as its name when it had no placeholder, and a
  ticked box not at all. A field is now named by its `aria-label`, placeholder, `<label>` or title rather than
  by its content, so typing does not rename it (a diff and a name-based locator stay stable); a checkbox is
  named by its label instead of `on`. A button made of an `<input>` is still named by its value.
- **A click on an element in a same-origin frame is real input** at the element's page position, as the
  top page's is, instead of a synthetic `click()`; `type` into a frame field is a real insertion too. A frame
  covered by another element still falls back to the synthetic click and says why. Shadow-root CSS matches no
  longer wait for the light DOM to have none: both are returned, light DOM first.

- **Shorter browser tool list.** The MiniWoB++ dry run (`eval/README.md`) found Claude Haiku 4.5 using about
  twice the input tokens with agentctl's browser tools than with Playwright MCP, and the tool list is part of
  every turn. The wire descriptions and schema property descriptions of the browser tools now say what a model
  needs to call them (what the tool does, non-obvious arguments, result fields to read); engine caveats,
  rationale and long enumerations moved to `details` and are in `docs/tools.md`, nothing was dropped. The
  `browser` role's list went from about 9.7k to 6.5k tokens (-33%), measured as the JSON of `tools/list`
  at 4 characters a token.
- **`browser_snapshot` nodes are compact.** Null and empty fields are left out (a missing `role`, `name`,
  `semantic_intent` or `bound_state` means none), `is_enabled: true` is gone and a disabled node says
  `disabled: true`, and an XPath ref drops `[1]` on a tag with no same-tag sibling (`/html/body/form/input`,
  not `/html/body[1]/form[1]/input[1]`). Refs saved in flows and checkpoints still resolve. A 68-node form
  page went from about 13.5 KB to 8 KB, which matters at 400 nodes a snapshot and with models that copy refs
  back by hand.
- **`browser_act click` and `type` are real input on Chrome.** A click was `el.click()`: no `pointerdown`,
  `mousedown`, `pointerup` or `mouseup`, so react-select (which opens its menu on `mousedown`) did nothing.
  It now scrolls the element into view, hit-tests its centre and, when the element is what sits there, sends
  a real mouse click at that point; the result says `input: "cdp"`. A click stays `el.click()`, with
  `input: "synthetic"` and an `input_reason`, for the Safari engine, `<select>` and `<option>` (a real click
  opens a native popup), file inputs (it would open the OS file chooser), elements in a child frame, ones
  with no size or off screen, and ones something else covers. `type` focuses the field, selects what is in it
  and sends `Input.insertText`, so `beforeinput` and `input` fire as for a person typing and React, Lexical
  or ProseMirror state follows; empty text clears the field. The result carries `value_after` (only
  `value_length` for a password, one-time-code or card field or a `secret` call). `type` into a field
  something disables or marks read-only, and into other element kinds, still sets the value. A recording
  now sees the browser's own `change` when focus leaves a field that was typed into, as with a person.
- **The showcase pointer is real and visible.** With showcase on, Chrome gets eased `mouseMoved` events
  along the glide, so the page sees the pointer arrive (`mousemove`, `:hover`, tooltips), and the drawn
  cursor follows the same curve, starting where it last was even after a navigation. `hover` always moves
  the real pointer, showcase or not, so CSS `:hover` applies; the synthetic `mouseover` remains for an
  element something else covers. The cursor is 32 px by default with a
  coloured glow, the click ripple lasts 900 / 700 / 300 ms (cinematic / demo / snappy) and the click waits a
  beat after it starts, so it is on screen when the click lands. In the field the cursor was a 15 px dark
  arrow and a 400 ms ripple that a recording almost never caught.
- **A headed `launch` no longer throttles behind other windows.** It passes
  `--disable-backgrounding-occluded-windows --disable-renderer-backgrounding
  --disable-background-timer-throttling --disable-features=CalculateNativeWinOcclusion`, and connecting
  brings the active tab to the front. In the field a covered Chrome reported `visibilityState: "hidden"`,
  ran `setTimeout(100)` in about 900 ms, and froze a screen recording. `background_throttling: true` opts
  out. `browser_eval` also enables focus emulation on its session, since that does not outlive a connection.
- **`browser_act` no longer scrolls a wide page sideways.** It scrolled every element to the centre on both
  axes; in the field that left the page at `scrollX` 497 with a white strip after opening a drawer. It now
  scrolls only as far as needed, instantly, and focuses with `preventScroll`. `browser_fill_form` focuses
  with `preventScroll` too.
- **`browser_wait` refuses ambiguous arguments.** Two different conditions (say `condition` plus
  `network_idle: true`) are an error naming both, where the first match used to win silently;
  `navigation: false` no longer selects a navigation wait; `condition: "selector"` without `selector` says
  so. `condition` is the documented form and the booleans are aliases.
- **The default `browser_eval` time limit is 10 s**, down from the 20 s transport timeout.
- **`browser_connect` returns the tabs.** `tabs` (`target_id`, `title`, `url`; page targets, most recently
  active first) and `target_id` (the active one), for an attach and a launch. In a MiniWoB++ dry run every
  run spent a call on `browser_tabs` right after connecting, because the result had a `browser_id` and no
  tab id. Titles and URLs come from the page, so the result is marked untrusted like `browser_tabs`.
- **`target_id` is optional on the tab-scoped browser tools.** Left out, it is the active tab of the only
  connected browser; with none or several connected the error says to connect or which browser ids to pass.
  A `browser_id` (`"1"` or `1`) means that browser's active tab: the dry run's model passed it for
  `target_id` five times and got `NOT_FOUND`. A default is reported as `target_id` in the result.
  `browser_tabs`, `browser_branch create`, the list actions and `browser_screencast stop` still name their
  tab.
- **`browser_act` `type` and `press` with no `ref` or `query` act on the focused element.** The dry run's
  model wanted to type into the field it had just clicked and got `INVALID_ARGS`. With nothing focused
  (`<body>`) it says so.
- **`browser_fill_form` fields take `by`** (`css`, `xpath`, `text`), found as `browser_act` finds them, and
  a selector starting with `/` or `(` is XPath without it. Every selector used to be CSS, so an XPath one
  was a `SyntaxError`. A label found by text fills the control it labels.

### Fixed

- **A `browser_act` hover could report ok when the pointer never moved.** The real move was treated as
  decoration and a failed dispatch was dropped. A move that is not delivered is now an error, like every other
  pointer event.
- **`browser_screenshot` said `width: 0, height: 0` next to a whole-page image.** Only element captures
  measured themselves, and a weak model in a benchmark run could read the zeros as a blank page. The inline
  result now takes the size from the PNG header, as `save: true` already did.
- **React-controlled fields ignored `browser_act type`, `select` and `browser_fill_form`.** React tracks
  each controlled input's value through an accessor it installs on the element and drops an `input` event
  when the tracked value already equals the new one; `el.value = x` goes through that accessor, so the page's state
  never updated. Values (and `checked`) are now assigned through the native prototype setter, in `type`
  (Safari, and wherever a real insertion is not possible), `select`, `browser_fill_form` and the
  `browser_checkpoint` restore.
- **`by: "text"` clicked the wrong element and reported ok.** On a MiniWoB++ click-button page the
  instruction `Click on the "next" button.` comes before `<button>next</button>`, and the locator took the
  first element whose text held the query, so `next` clicked the instruction; `ok` hit an `okay` button, and
  `Next` did not match `next`. Matching is now case-insensitive over whitespace-collapsed text (also a
  button input's value and `aria-label`), takes the innermost element holding the query and lifts it to the
  control around it, and ranks exact before substring, visible before hidden, clickable before not, then document order. When any
  match is exact the substring ones are dropped. `browser_act`, `browser_query` and `browser_upload` share it.
- **A plain word as a `browser_act` query found nothing.** `by` defaulted to css, so `"query": "Gilli"` was
  the tag `<gilli>`, and `"Section #1"` did not parse: in a 130-task MiniWoB++ run these were the most
  common tool error (174 "element not found"). With no `by` a query is now CSS when it parses and matches,
  else visible text, and the result says `matched_by`. Spellings from other tools are read as what they mean
  (`text=…`, `css=…`, `xpath=…`, a trailing `:has-text("…")`, an XPath or a cut-down snapshot ref passed as a
  query), in `browser_query`, `browser_upload`, `browser_fill_form`, `within` and the snapshot's
  `root_selector` too. A miss says what was tried and names up to three elements with a word of the query.
- **`browser_snapshot` left out what a person clicks when the page gives it no role.** Rows and icon buttons
  wired with jQuery or `addEventListener` (the email client's send, reply and trash icons) were missing, so
  the model could not find them. An element with a pointer cursor is now listed (the outermost one), named
  by its label, title, alt, icon file name or id, and that name works as a text query.
- **`browser_act press` refused combos and errored with nothing focused.** `ctrl+a`, `Control+A`,
  `cmd+shift+z`, `Shift+Tab`, single characters, F1-F12 and spellings like Esc or Return now work; ctrl or
  cmd with a, c, x, v, z or y edits as a person's shortcut does, also on macOS where headless Chrome ignores
  the key event's own editing. With no target and nothing focused the key goes to the page. Key events no
  longer carry `nativeVirtualKeyCode`, which macOS Chrome read as a Mac key code (F5 arrived as PageUp).
- **`browser_act` threw `el.click is not a function` on SVG shapes.** They are clicked with real input on a
  point of the shape, or a dispatched pointer and mouse event sequence.
- **`browser_act` action spellings** such as `key`, `dblclick`, `triple-click` and `drag_and_drop` are
  accepted, and an unknown action lists the valid ones. `browser_wait` with only a duration sleeps (at most
  30 s) instead of failing.
- **Desktop tools said ok without checking.** `set_value` and `keyboard_type` read the field back and report
  `value_after` and `changed`; a write the application ignored is an error. `check` and `uncheck` read the
  state first and after on macOS too, so they are idempotent and a toggle that did not take is reported.
- **`keyboard_shortcut` rejected `Control+A`, `Cmd+Shift+Z` and `Return`, and pressed only `b` for `a+b`.**
  Combos are case-insensitive, accept `-` as a separator, named and literal punctuation, a `+` key,
  `Insert`, forward delete and F-keys, and `mod` (Cmd on macOS, Ctrl on Linux); two non-modifier keys are an
  error, and an unknown key's error lists the accepted names. `ui_fill_form` no longer picks among
  same-named fields in hash order.
- **Choosing from a native `<select>` reported ok and chose nothing.** In the MiniWoB++ choose-list task
  the model clicked an `<option>` ref, or called `select` on one; `click()` on an option selects nothing and
  setting `value` on one rewrites its value attribute, so 3 of the 4 agentctl failures in the second dry run
  were a list still showing its first name. An option now stands for its select: a click on it, or `select`
  on it, chooses it there and fires `input` and `change`. `select` on the list matches the option's value,
  then its text, then either ignoring case, and `browser_fill_form` does the same. The result says
  `selected` and `changed`; no match, a disabled option, or a page that puts the list back is an error, the
  first listing the options. A click on the `<select>` itself chooses nothing and its `input_reason` says to
  use `select`. `browser_snapshot` names a select by its label (it showed the current value) and adds
  `options` (the first 25) and `selected`.
- **`browser_act` now says what it hit:** the result has `target {tag, text}` (a field is named by its
  label, `aria-label`, placeholder or name, never its value) and, for a `query` locator, `matches`, the size of
  the ranked list, so a wrong target shows instead of a silent ok.
- **`cursor_style` did nothing**: every style drew the same arrow. Each now has its own look.
- **`browser_showcase` reported `ok` for an overlay that was not drawn.** It was built with `innerHTML`,
  which throws on a page that requires Trusted Types, and the throw was swallowed. It is now built with DOM
  calls, and failure is reported (above).
- **`demo = true` never turned on the browser overlay**, although 0.2.0 said it did; it only set the
  glide speed.
- **`glide_ms` was unbounded**; it is capped at 3000 ms.
- **`network_idle` watched nothing.** It checked `document.readyState === 'complete'` and slept 400 ms, so
  straight after a click it reported the old, still loaded page as settled before the click's request had
  begun, and the next read saw the page as it was. Twice in the field this was taken for an app bug. It
  now counts fetch and XHR (a hook that `browser_act` installs before acting, which wraps `window.fetch`
  and `XMLHttpRequest.prototype.send`), checks the Performance API's resource entries, needs 500 ms of
  quiet, and first waits for a navigation the last click, submit or key press may have started.
  `browser_assert wait_network_idle` uses the same wait. A page that always has a request open (long
  polling) now times out where it used to settle.
- **`htmx_settled` settled before a delayed request began.** Its listeners went in on the first probe, so
  a debounced or delayed `hx-trigger` looked settled. The act now installs them first, and right after an
  act the wait gives a request up to 1.5 s to start.
- **`dom_settled`'s description claimed animation frames were tracked.** They are not; the description
  says so now.
- **A backslash left before a quote in an XPath broke the lookup.** `//*[@id=\"tt\"]` as a `ref`, a
  `by: "xpath"` query, an XPath `within` or an XPath fill_form selector failed with "not a valid XPath
  expression" (seen in the MiniWoB++ dry run, where the model copied refs with the escapes). `\"` and `\'`
  are never valid XPath, so they are now dropped; CSS selectors, where they are valid, are untouched.
- **A jQuery or Playwright pseudo-class in a CSS selector gave a raw `DOMException`.** `:contains(`,
  `:has-text(`, `:text(`, `:visible`, `:eq(`, `:first` and `:last` are not CSS. `browser_act`,
  `browser_query` and `browser_fill_form` now answer `INVALID_ARGS` with the browser's message and a
  suggestion to use `by: "text"` (or act's `text` filter).

### Security

- **`fs_archive` extract could write credential stores and the server's own config.** Extraction went
  straight into the destination and was checked only for escapes from the roots, afterwards; an archive
  carrying `.ssh/authorized_keys` or `.agentctl/config.toml` landed inside a root of `~` with nothing
  refused. Archives now extract into a staging directory, every entry is checked against the jail (deny
  list included) before anything moves into place, and an archive holding a symlink or special file is
  refused. A refused archive leaves the destination unchanged.
- **`fs_archive` compress could bundle a denied path into a readable archive.** Compressing a directory
  that held `.ssh/` (or any other denied path) is now refused, and `zip` stores symlinks as links instead
  of following them.
- **`fs_search` read files the jail denies.** The walk started inside the jail but never re-checked what
  it found, so a search under `~` returned matching lines from `~/.ssh/` and `~/.agentctl/`. Denied files
  are now skipped.
- **The server's own files were only protected at their default location.** `/.agentctl/` is on the deny
  list, but `$AGENTCTL_CONFIG`, `policy.kill_switch_file`, `policy.audit_dir` and
  `policy.audit_signing_key` can each point elsewhere, and inside a root an agent could then rewrite its
  own policy, pre-empt the STOP file or trash the audit log. Those paths are now refused wherever they
  live, and `fs_delete` and `fs_move` refuse a directory that holds one.
- **Release archives carry signed build provenance.** Each archive is attested through Sigstore with the
  release workflow's GitHub identity; `gh attestation verify <archive> -R paooxide/ui-draw-mcp` proves it
  was built by this repository's workflow from the tag. macOS Developer ID signing and notarization are
  wired into the workflow and run once the signing secrets are configured.

## [0.2.0] - 2026-10-04

### Added

- **`browser_connect` `launch.port = 0`** (also the default when `port` is omitted) lets Chrome choose its own
  remote-debugging port: the launch passes `--remote-debugging-port=0` and reads the bound port from the
  profile's `DevToolsActivePort`, which the connect result reports as `port` (plus `owned_user_data_dir` for a
  profile the launch created). The old pick-a-free-port,
  release, then hope approach left a window for another process to take the port first. An explicit non-zero
  port is still honoured, and the live test suites now all launch on port 0 so they can run in parallel.
- **Reproducible latency bench** (`cargo run --release --example bench -- --n 30`, see `docs/bench/`).
  It starts `agentctl serve` over stdio under a temporary config and times `ping`, `list_windows`,
  `capture_screen`, `ocr_region`, and `browser_snapshot` and `browser_act` against a local fixture page, after
  5 discarded warm-up calls, reporting min, median, p95, max and standard deviation. Each call is timed twice:
  the client round trip, and the server's own `latency_ms` read back from the audit log. Raw samples, the
  commit, OS, CPU, display and Chrome version go to `docs/bench/results/`. An operation that fails (a missing
  Screen Recording grant, say) is recorded as skipped with the server's error code, never filled in. Pointer
  movement only runs under `AGENTCTL_LIVE_GUI=1`. There is no cloud comparison: that needs the same task run
  end to end against a hosted model.
- **PII tokenizer, opt-in** (`policy.anonymize`, `AGENTCTL_ANONYMIZE`). Tool results reach the model with
  personal data replaced by stable tokens (`<SSN_1>`, `<EMAIL_2>`): registered names, SSA-valid SSNs,
  Luhn-checked card numbers, phone numbers, emails, MRNs, IPv4 addresses and common API key formats. The
  audit log keeps the tokens. Plaintext is restored only when a token is typed into a local field
  (`keyboard_type`, `set_value`, `ui_fill_form`, `browser_fill_form`, `browser_act`); a token in any other
  tool's arguments is refused before the gate, so it cannot be sent off the machine in a URL, command or
  file. Typing a token into a field on a page the model chose still delivers it; that gap is pinned in
  `documented_known_bypasses`. Off by default, because it rewrites every result.
- **Signed audit logs and compliance export.** Each audit record carries a sequence number, the previous
  record's hash and an Ed25519 signature, so deletion, reordering or editing breaks the chain.
  `agentctl audit keygen` creates a signing key for `policy.audit_signing_key`; `agentctl audit verify
  --pubkey` fails unless the log was signed by that key, and without one reports the log as
  self-consistent only. `agentctl audit export` writes a SOC2-style report or a HIPAA access-event CSV/JSON.
- **Role profiles and argument invariants.** `policy.role` (built in: `readonly`, `qa`, `operator`,
  `admin`, or one defined under `[roles.<name>]`) narrows the visible tools by category, tier or name,
  can require consent, and can lower the denial budget; a role never widens anything. `[invariants]`
  refuses protected paths and denied domains anywhere in a call's arguments before the gate, including
  inside command lines, in `file:` URLs and through symlinks. It is a check on argument text, not
  containment; relative paths, shell expansion, encoding and DNS are documented as out of reach.
- **Compound native forms and extraction.** `ui_fill_form` sets text fields, checkboxes, switches and
  pop-ups in one call and can submit and check the result; `ui_extract` reads a native table, form or list
  into JSON.
- **Demo mode** (`policy.demo`, `--demo`, `--demo-speed cinematic|demo|snappy|instant`), for screencasts:
  the pointer glides along an eased curve, and browser pages show a pointer, click ripples and a typing
  label that masks secret fields. Off by default. A glide stops at the next waypoint when a human takes the
  mouse or the call is cancelled.
- **Browser forms, extraction and profiles.** `browser_fill_form` fills many fields and optionally submits
  in one call; `browser_extract` reads schema-shaped records from a page; `browser_profile` saves and
  restores cookies and storage, and `browser_connect` can start from a profile. `browser_wait` gains
  `dom_settled`, and `browser_assert` can wait for it first.
- **Branches and checkpoints.** `browser_branch` tries a path in a separate browser context seeded with the
  page's cookies and storage, then commits it to the visible tab or discards it (at most 8 at once,
  `AGENTCTL_MAX_BRANCHES`; their tabs are closed on discard and at shutdown). `browser_checkpoint` saves
  and rolls back URL, form fields, storage and cookies, and names any field it could not restore.
- **Scoped acting and htmx.** `browser_act` can resolve its target inside a CSS or XPath container, filter
  by text and pick by index; a container that matches nothing is an error. `browser_act` also gains
  `press` (Enter, Escape, Tab as real key events) and `secret`. `browser_wait htmx_settled` follows
  htmx's request and settle events.
- **Shadow DOM and canvas regions.** Snapshots descend into open shadow roots. A canvas that publishes its
  interactive regions (`__agentctl_regions` or `data-canvas-regions`) gets child nodes that are clicked
  with real mouse input; other canvases stay opaque.
- **Challenge handshake.** `browser_challenge` detects a CAPTCHA or one-time-code prompt, shows an overlay
  and waits for a person to clear it. It never tries to solve one.
- **Recording.** `browser_record` (and `agentctl record`, which launches its own browser unless given
  `--attach <port>`) captures clicks, typing, key presses and navigations into a `browser_flow` that
  replays. Password, PIN, one-time-code, card, token and API-key fields are recorded as a named
  `secret_ref`; their values are supplied at replay time (`browser_flow run` `secrets`, or
  `AGENTCTL_SECRET_<REF>` for `agentctl test`) and never stored or logged. While a recording runs, the
  tab's JavaScript dialogs are answered as `dialogs` says. A person recording by hand answers their own
  `confirm()`: in a visible browser the default is `human`, where the recorder only listens (Chrome shows
  the dialog natively and also announces it to the Page-domain client), and how the person answered
  becomes a `dialog` step. `accept` and `dismiss` have the recorder answer; a headless browser has nobody
  to answer, so `dismiss` (or the tab's `browser_dialog` policy) is its default and `human` is refused
  rather than left to hang the tab. A `dialog` step sets the tab's standing policy, so it is placed
  before the click, typing or key press that raised the dialog; alerts are not recorded (they have one
  way out) and a prompt's typed text is not kept, since it may be a secret, so replay accepts it empty.
  Recording with `dialogs: "accept"`, and running a flow with an accepting dialog step, ask for the same
  consent as `browser_dialog policy: "accept"`.
- **Safari, experimental.** `browser_connect` takes `launch.browser = "safari"` and drives Safari through
  `safaridriver`'s W3C WebDriver. Operations WebDriver cannot do (key presses, device emulation, branching,
  checkpoints, recording, network capture) return `UNSUPPORTED`. Needs a one-time `safaridriver --enable`;
  its live suite runs with `AGENTCTL_LIVE_SAFARI=1`.
- Release archives are also built for Linux on ARM64.

- **UX testing: accessibility, design-token style, component and responsive checks.** `browser_assert`
  gained UX clauses that ride the same `{passed, checks}` flow and `agentctl test` report as the functional
  ones: `a11y` runs a built-in WCAG audit (alt text, form labels, control names, colour contrast, target
  size, positive tabindex, duplicate ids, page lang); `style` checks computed colours/fonts/font-sizes/
  spacing against allow-lists and flags off-token values; `component` asserts one element's role, visibility
  and states (disabled/expanded/checked/...); `within` scopes any of these to a component subtree. New
  `browser_viewport` tool (and `viewport` flow step) emulates device metrics for responsive testing.
- **UX testing: visual regression and a judge-scored UX review.** `browser_assert` gained `visual`
  (screenshot vs a named baseline: the first run saves it, later runs diff the pixels in-page and fail past
  a tolerance or on a dimension change) and `ux` (the judge scores clarity/hierarchy/affordance/consistency
  over the page's facts; advisory by default and skipped when the judge is off, `gate:true` makes a low
  dimension fail). Baselines are a file-backed store next to the flow store. `agentctl test` now surfaces
  every UX check (a11y violations, off-token values, visual diff, UX scores) per flow in its report, so a
  run shows them even when it passed overall.

- **Browser engine built for native regression/UI testing.** `browser_act` can locate by `by`+`query`
  selector in one call (no separate `browser_query`). `browser_capture` installs a persistent page hook
  recording fetch/XHR with request/response bodies plus console errors and uncaught exceptions
  (Dangerous-tier; off unless enabled). `browser_assert` settles then checks text/url/selector and, with
  capture on, no console errors and no failed requests, returning `{passed, checks}`. `browser_flow`
  saves and replays a named sequence of steps deterministically, stopping at the first failing step so a
  green run never needs a model.
- **`agentctl test`** replays saved flows and reports each flow's result, elapsed time and the issues it
  hit (failing step, console errors, failed requests), writing a `--json` report (per-flow and total ms)
  and exiting non-zero on failure (or on any issue with `--strict`) for CI. With no `--attach` it launches
  its own throwaway browser and stops it when done; it shows a window when a display is present (so a local
  run can be watched) and stays headless in CI, with `--headed`/`--headless` to force either way.
- **The browser engine can launch any installed Chromium-family browser and always stops the tree it
  started.** Launch discovery now covers Chrome, Chromium, Edge and Brave across native, snap and flatpak
  locations (the engine speaks CDP, so Firefox remains unsupported; Safari is experimental, below). Each launch takes its own
  free port rather
  than a fixed one, so back-to-back launches never collide. A launched browser is stopped with a CDP
  `Browser.close`, the only thing that reaps a sandboxed (flatpak/snap) browser's whole process tree,
  which a signal to the launcher cannot reach.
- **More places the judge helps, and per-use thresholds.** `handle_dialogs` takes `intent` and returns a
  `suggestion` naming the button that serves it (advice only; it presses nothing). `memory_find` takes
  `rerank` to reorder recalled recipes by semantic fit to the goal, falling back to success-count order
  when the judge is unavailable. `agentctl bridge --prune` lets the reference client ask the judge which
  tools a task plausibly needs and declares only those to the model, always keeping a core observe/wait
  set and degrading to the full list. `[judge]` now takes optional `destructive_threshold`,
  `injection_threshold` and `match_threshold`, each falling back to `threshold`, so the destructive second
  opinion, the injection flag and semantic ranking can be tuned separately.
- **Clipboard image and file-list formats (Linux).** `clipboard_read`/`clipboard_write` handle
  `format: "image"` (a base64 PNG) and `format: "files"` (a newline-separated `text/uri-list`) in addition
  to text and HTML, over the Wayland `wlr-data-control` path or the XWayland `xclip` fallback (`xsel`
  carries text only, and says so).
- **One-word permission profiles.** `policy.access = "ask" | "auto" | "bypass"` replaces enabling each
  category and naming each dangerous tool. All three turn on every capability; `ask` confirms a dangerous
  tool or destructive action through the dialog, `auto` runs unattended but refuses a clearly destructive
  action, and `bypass` turns off consent and the destructive gate (kill switch and human-override remain).
  The granular `categories`/`enable` config still works when `access` is unset.
- **Secret-safe input.** `keyboard_type`, `set_value`, `pty_write` and `clipboard_write` take
  `secret: true` for entering a password the owner provides. The payload is redacted from the append-only
  audit log (length marker only) and is never sent to the judge, while the real value still reaches the OS.
  The destructive-pattern check still runs offline. Without the flag, typed text is logged verbatim as
  before.
- **The judge** (`mcp-judge`, `[judge]` in the config, off by default): typed judgments from a System One
  model (TypeSafe's `jev`), consulted only where a judgment can tighten a decision or rank candidates.
  `find_elements` takes `describe` and returns candidates ranked with probabilities; `wait_for` and
  `expect` take `judge`, a claim about the UI, and report its probability; results marked untrusted get a
  second opinion on whether their text is addressed to a model (the flag can be added, never removed);
  text headed for a terminal or a PTY gets a second opinion after the destructive patterns (a yes
  escalates, a no changes nothing). Unreachable, keyless or disabled, it is skipped and counted. The key
  is read from `TYPESAFE_API_KEY`, `.env` or `~/.agentctl/typesafe.key`, never from config or argv. A
  red-team suite pins that no scripted answer can loosen anything.

- **Linux desktop backend** (`mcp-linux`). Perception over AT-SPI2: `get_ui_tree`, `find_elements`,
  `get_element` and delta snapshots read the same tree a screen reader does, with refs that keep the
  object reference and fall back to a path replay and an identity search when a widget is replaced.
  Semantic input (`ui_action`, `set_value`) through the widgets' own AT-SPI actions and text interfaces,
  so no pointer is involved. Keyboard and pointer input through the `RemoteDesktop` portal, which asks the
  person once and is remembered; `cmd` in a combo is translated to Control. Capture through the
  `Screenshot` portal, display geometry from Mutter, text recognition with the `ocrs` engine (models
  fetched on first use, or placed by hand). Windows, applications, menus and dialogs from AT-SPI;
  `launch` and `focus_app` through desktop entries and `org.freedesktop.Application`; `control_window`
  through GNOME's shortcuts. Session control over D-Bus: lock, idle, notifications, volume, colour scheme,
  brightness, do-not-disturb, MPRIS media, logind power, `espeak-ng` speech. Validated on GNOME 50 on
  Wayland.
- A consent dialog on Linux: `zenity --question` with Deny as the default, a critical notification with
  Allow and Deny buttons when zenity is absent, and a timeout that denies either way.
- `agentctl doctor` on Linux reports the accessibility bus, the session accessibility flag, the portal
  versions, whether the input grant has been given, the consent channel, the OCR models and the helper
  binaries, each with what to do when it is missing.
- The commodity engines now have real Linux paths where they shelled out to macOS tools: trash via `gio`
  (with an XDG fallback), storage and mounts via `lsblk`, `findmnt` and `udisksctl`, Wi-Fi and VPN via
  `nmcli`, services via `systemctl`, the keyring via the Secret Service (existence checks never see the
  value), bus devices, sysctl, process maps and telemetry from `/sys` and `/proc`.
- A Linux live suite (`agentctl/tests/live_linux.rs`): the read-only half runs on any graphical session,
  the acting half behind `AGENTCTL_LIVE_GUI=1`.
- The Linux clipboard falls back to `xclip` or `xsel` over XWayland where the compositor withholds the
  `wlr-data-control` protocol (GNOME/Mutter), and reports the limitation clearly where no bridge exists.

### Fixed

- **Showcase overlay errors replaced the real error of `browser_act` and `browser_fill_form`.** The overlay script
  was spliced bare into the action's async function, so when a page made it throw (Trusted Types forbid its
  `innerHTML`; a page can also lack a `head`) the whole function rejected: a missing element, a bad selector or a
  stale ref came back as the overlay's `TypeError`, and an action that would have succeeded was reported as
  failed. This showed on Safari, where it was seen; the same script runs on Chrome. The overlay is now contained
  in its own `try`, so it can only cost the animation, and a failed overlay install from `browser_showcase` is
  logged instead of dropped.
- **Safari: `browser_eval` fails on a page whose CSP has no `unsafe-eval`.** `browser_eval` evaluated the text with
  an indirect `eval` inside the page, which `Content-Security-Policy: script-src 'self'` refuses (`EvalError`), so
  every eval on such a page was an error. The script now probes `eval` with a constant before running the code;
  when the probe is refused (CSP, Trusted Types, or a page that broke `eval`), none of the code has run, and it
  sends the code as the WebDriver script body itself, which the driver injects and
  the page CSP does not govern: first as an expression, then, if it does not parse as one, as a function body. A
  returned promise is awaited either way. An error from the code itself never triggers the second route, however
  it is worded, so page script cannot make the agent's code run twice. On that route statements have no completion value, so they need an
  explicit `return`. Chrome is unaffected: `Runtime.evaluate` bypasses the page CSP.
- **Safari: `htmx_settled` is now tested against the real htmx.** The earlier live test used a stand-in that faked
  htmx's events. A test now serves the vendored htmx 2.0.4 (`crates/mcp-browser/tests/fixtures/`), clicks an
  `hx-get` button whose response the server delays, and checks that `htmx_settled` holds until the swap is in the
  page. It passed on WebKit without a code change.
- **Safari: `browser_capture start` never armed the page, `browser_eval` ran only expressions, and `branch`/`checkpoint`
  said the tab was not found.** The live suite on a real Safari showed three faults. The capture hook starts with a
  line break, so `return <hook>` returned before running it (automatic semicolon insertion) and `start` reported
  success with nothing recording. `browser_eval` wrapped the text in `return (...)`, so `a(); b` was a syntax error
  and a returned promise came back as `{}`; it now evaluates like the Chrome path (last statement's value, promises
  awaited, a throw is an error). `branch_create`, `checkpoint_save` and `checkpoint_rollback` on a Safari tab
  failed with "target not found in any connected browser" and now return `UNSUPPORTED`.
  `browser_wait htmx_settled` on a page without htmx is `NOT_FOUND` as on Chrome, and `challenge_cleared` works on
  Safari instead of being an unknown condition.
- **Safari live tests** now cover snapshot, click and type by ref, `fill_form`, `within`, `htmx_settled` against a
  local fixture (no CDN), hostile semantic fields, and the explicit `UNSUPPORTED` results (recorder, network,
  branches, checkpoints, emulation). The fixture servers handle each connection on its own thread: Safari opens idle
  speculative sockets, and a one-at-a-time server stalled the page load behind one.

- **Safari session creation no longer hides "automation not enabled" behind a timeout.** safaridriver waits ~30 s
  for Safari before answering "session not created ... timed out while connecting to a Safari instance", but the
  client gave up after 15 s and reported a bare `Timeout`. Session creation now waits 45 s, and that answer (like
  the explicit "Allow remote automation" one) is a `PermissionDenied` that says to enable Develop > Allow Remote
  Automation, run `safaridriver --enable`, restart Safari and accept any prompt.

- A running call could not be stopped: the per-call cancel token was never tripped, so the kill switch
  and a human taking the mouse only blocked the next call. A running call now checks the kill switch every
  50 ms and cancels its token, and `notifications/cancelled` reaches the call it names; the stdio loop
  keeps reading during a call so the cancel can arrive.
- `browser_wait navigation` returned on the page being left when a click started its navigation a moment
  later (a timer, a debounce), because that page still reports `complete`. It now waits for a loaded
  document that is not the one the action left, and a click that navigates nowhere within 2 s settles
  with `navigated: false`.
- `browser_wait navigation` gave up on a click whose handler navigated later than the fixed 2 s window
  (a slow analytics call before `location` changes), settling on the old page with `navigated: false`
  while the new one was still coming. `navigation_timeout_ms` (0 to 30000, default 2000) sets the window
  per call, and a `wait` flow step takes it too. `timeout_ms` still bounds the whole wait.
- `browser_navigate back` and `forward` did not mark the document they left the way `goto` and `reload`
  do, so a `wait navigation` after them had only the old page's `readyState` to go on. They now plant the
  marker, so the wait is for the history entry's page. An entry made by `history.pushState` or a fragment
  change keeps the document, and Chrome says so, so the marker is dropped and the wait settles at once
  instead of running out its timeout.
- `browser_checkpoint rollback` could succeed from Chrome's HTTP cache. It navigates to the saved URL, and
  for a page served with `max-age` Chrome answered from disk: with the server down the rollback reported
  `rolled_back: true` for a page nobody had fetched, and with the server up it could restore form state
  into an out-of-date copy. The load now bypasses the HTTP cache (for that load only), so an unreachable
  server fails the rollback with the navigation error and a changed page is the one you see. The result
  carries `cache_bypassed`, true when the rollback navigated. A rollback that finds the tab already on the
  checkpoint's URL loads nothing, as before.
- The CDP client could lose half a WebSocket frame when a read was cancelled by a timeout, corrupting the
  rest of the stream. Reads are now buffered and cancel-safe.
- `pty_spawn` with no `shell` picked the first allowed shell whether or not it existed, so on a machine
  without `/bin/zsh` every default spawn failed. It now prefers `$SHELL` when allowed and present, then
  the first allowed shell that exists, and names every candidate when none does.
- The `docs/tools.md` check now runs on both CI legs, since both build every engine.
- The HTTP transport could not be relied on to deliver `notifications/cancelled`, and it dropped a call
  that outlived the read timeout. Connections are now served concurrently, up to `max_connections` (64;
  the next one gets a 503 rather than queueing), each with the full origin, token and size checks, so a
  cancel POST reaches a `tools/call` POST that is still running. The 30 s read timeout now bounds only
  receiving the request, not the tool call.
- A request the client cancelled with `notifications/cancelled` was still answered. The MCP spec says
  the receiver should not respond, so neither transport now writes a reply for it (HTTP returns an empty
  `202`); the audit post-record is still written, including for a call cancelled before it started. A call
  stopped by the kill switch is still answered, since the client is still waiting.
- Human takeover detection was silently off on every Linux session. `pointer_position` always answered
  `None`, and the watcher said so only at info level. On X11 it now reads the pointer through `xdotool`
  and the watcher runs. Where that is impossible (Wayland, including XWayland, which only sees the
  pointer over X11 windows; no `DISPLAY`; no `xdotool`) it logs at warn level at startup, and `agentctl
  doctor` shows `human takeover` with the reason. Not tested on a live X11 session.
- On macOS, `keyboard_type` posted a whole string on one key event, and Apple documents that only the first
  20 UTF-16 code units of a string set on one event are used, so long text could be cut while the call
  reported the full count. Text now goes out in events of at most 20 units, never splitting a surrogate
  pair and, as far as a block-based table of combining marks, joiners, skin tones and flags allows, never
  a grapheme cluster. A takeover between pieces stops the typing and says how much was typed. The 200
  character live test is written but has not been run.
- `--demo-speed` accepted any string and quietly ran at the default speed, while the config file and the
  environment refused an unknown one. An unknown or missing value is now a usage error (exit 2) naming
  `cinematic, demo, snappy, instant, off`, and the browser showcase speed comes from the same preset as
  the pointer glide instead of a second copy of the mapping.

### Security

- **The browser recorder ignores synthetic DOM events.** Page script could `el.click()`, dispatch `input` or
  `change` after setting a value, or fire a synthetic Enter, and the recorder saved each as something the
  person did. It now records an event only when it is trusted (`isTrusted`) or when agentctl armed it: while a
  tab is recorded, `browser_act` (click, type, select) and `browser_fill_form` run in the recorder's isolated
  world and queue one expectation per synthetic event they dispatch, which the first matching event uses up,
  so a page handler that re-dispatches the same event is dropped too. `docs/threat-model.md` lists what a page
  can still influence.
- **Browser snapshot `semantic_intent` and `bound_state` are sanitized and bounded.** Both come from the page
  (`data-intent`, `data-state`, React props, canvas region fields), so a hostile page could put quotes,
  newlines or a forged `@e9` line in an intent, or hand over a cyclic or 100 KB state. An intent is now
  reduced to `[A-Za-z0-9_.-]`, 48 characters (the rule the OS snapshot already used). `bound_state` goes
  through an injected serializer (depth 4, 20 keys or items, 200-character strings, cycles, functions and DOM
  nodes dropped) and a 2 KiB cap in Rust that replaces a larger value with `{"truncated":true,"bytes":N}`.
  Chrome, Safari and canvas regions share the Rust step. macOS and Linux nodes still carry neither field.
- **A page cannot forge recorded steps.** The recorder and its `__agentctl_rec` binding lived in the page's
  main world, so page script could call the binding and add clicks or typing that nobody made to a
  recording, or replace the function to read what was typed. They now live in an isolated world: the DOM
  is shared, so the recorder still sees real clicks and typing, but the binding exists only there and the
  page cannot reach it or the recorder's state. Page script can still dispatch synthetic DOM events, which
  the recorder sees like the agent's own `browser_act` clicks.
- **Config values are checked, not guessed.** A boolean was read as `value == "true"`, so
  `human_override = "yes"` or `"True"` silently turned the human-takeover stop off, and a known setting
  with the wrong type (`max_denials = "5"`) was dropped as an unknown key. Booleans must be `true` or
  `false`, and a known key of the wrong type is an error; only keys the loader does not know are
  tolerated.
- **Accessibility snapshots escape page text.** Element names and values were written into the
  snapshot's quoted fields verbatim, so a control named `Cancel"`, a newline and `@e9 button "Approve`
  showed the model a line for an element that does not exist. Quotes, backslashes and control characters
  are now escaped.
- **`browser_tabs open` runs the navigation policy.** Chrome loads the URL as it creates the tab, so
  opening a tab at a URL bypassed `browser.allowed_origins` and the private-address check that
  `browser_navigate` applies.
- `browser_navigate` now runs the same resolved-address guard as `http_request`. With
  `browser.allowed_origins` empty the agent could point the browser at cloud metadata or a loopback
  service; every address the target resolves to must now be public unless the new
  `browser.allow_private` is on. Only `http`, `https` and `about:blank` are accepted, since `file:` walks
  past the filesystem jail and `javascript:` past the `browser_eval` opt-in.
- `browser.allowed_origins` entries are matched on the parsed origin, not as a string prefix:
  `https://ok.example` no longer admits `https://ok.example.evil`.

### Changed

- Release archives are built for macOS (Apple silicon and Intel) and Linux (x86_64 and ARM64) only. There is
  no Windows archive: the Windows desktop backend does not exist, and nothing else has been tested on Windows.
  The Linux archives link against glibc 2.28, so they run on RHEL 8, Debian 10, Ubuntu 20.04 and anything
  newer; the release workflow fails a Linux build that needs more, or that does not start on AlmaLinux 8.
- The SSRF guard moved from `mcp-net` into its own dependency-free crate, `mcp-ssrf`, so the browser
  engine can share it without one engine depending on another. `mcp-net` re-exports it unchanged.
- Driving a local development server through the browser now needs `browser.allow_private = true`.
- Tool descriptions say what a model needs to plan with: `browser_connect` lists what returns `UNSUPPORTED`
  on Safari (and that a Safari open before automation was enabled must be quit), `browser_act` lists `press`,
  `browser_eval` states its result semantics and the Safari `return` rule under a CSP, `browser_record` says the
  agent's own actions are recorded and page-faked events are not, `browser_branch`/`browser_checkpoint` say
  Chrome only, `browser_showcase` says it is decoration that never changes a result, and `input_showcase` no
  longer promises desktop overlays it does not draw.

## [0.1.0] - 2026-09-03

First tagged release. An MCP server that gives an AI agent grounded control of a real computer: 107 tools
across 12 capability categories, gated by a policy layer that treats the driving agent as untrusted.

### Added

- **Protocol.** MCP over stdio (JSON-RPC 2.0, protocol revision `2025-11-25`): `initialize`, `tools/list`
  filtered to enabled categories, and `tools/call`. An optional loopback HTTP transport with mandatory
  bearer authentication.
- **Policy kernel.** Category and tier gates, a per-tool opt-in for dangerous tools, an out-of-band native
  consent dialog with a prompt budget, a denial budget, a kill switch, secret redaction, and an
  append-only JSONL audit log written before and after every call.
- **Perception.** `get_ui_tree` and `get_element` over the macOS accessibility tree, with element refs that
  are stable paths carrying role and name identity rather than native handles, so they survive UI mutation.
  `list_displays`, `capture_screen` and `capture_window` return native MCP image blocks, deduplicated
  against the previous frame and right-sized for vision-token cost.
- **Input.** Semantic actions (`ui_action`, `set_value`), keyboard (`keyboard_type`, `keyboard_shortcut`),
  coordinate input (`mouse_action`, `scroll`, `hover`, `drag_drop`) and the clipboard. Keystrokes destined
  for a shell are screened for destructive commands, with editors screened alongside terminals because
  their integrated terminals run the same shell.
- **Windows and applications.** `list_windows`, `list_apps`, `launch`, `close_app`, `control_window`,
  `focus_app`, `menu_open`/`menu_invoke`/`menu_list`, `handle_dialogs`, and the `wait_for` settle
  primitive.
- **Browser.** Thirteen tools over the Chrome DevTools Protocol, with a hand-rolled CDP client rather than
  a heavyweight automation dependency. Page-side XPath refs are re-resolved on act, so they survive
  navigation.
- **Commodity engines.** Filesystem behind a resolve-then-check path jail; `exec` on argv with no shell by
  default; HTTP with SSRF containment; read-only system telemetry; credentials with no plaintext read; real
  PTY sessions; package lifecycle with a protected set; optional recall of task recipes.
- **Session and desktop.** `notify_user`, the agent's channel to a human, plus idle status, screen lock,
  media and power control, speech and audio playback.
- **CLI.** `serve`, `doctor`, `config print`, and `tools` for a generated tool reference.
- **Test support.** An in-process MCP client so tests drive the real protocol, and live task suites that
  exercise a real browser and a real desktop end to end.
- `expect` postconditions on `ui_action`, `set_value`, `keyboard_type`,
  `keyboard_shortcut` and `mouse_action`. The action runs, the condition is
  waited for, and the result carries the delta against the snapshot taken
  before it, so checking whether an action worked is one call rather than
  four. A failed expectation still returns the delta, because the action
  happened and what it did is what the agent needs to see.
- `since` on `get_ui_tree`: return what changed rather than the whole tree.
- `gone` and `focused` conditions on `wait_for`, which now shares its evaluator
  with `expect`.
- `ocr_region`: read text off the screen through Apple's Vision framework,
  returning a box per line in screen coordinates. The fallback for surfaces the
  accessibility tree does not describe, and unlike a screenshot it hands back
  coordinates that can be clicked. No longer deferred: a small Swift helper is
  compiled on first use rather than linking a framework or requiring the Xcode
  toolchain at build time.
- `find_elements`: query the accessibility tree by role, name substring or
  proximity to a screen point instead of reading all of it. On a busy
  application a targeted query is an order of magnitude smaller than the full
  tree, and the refs it returns are usable by `ui_action` because it takes and
  installs a fresh snapshot rather than querying a retained one.
- MCP tool annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`,
  `openWorldHint`) and display titles in `tools/list`, derived from the tier the
  policy gate already enforces.
- `policy.mode = "dry_run"`: a rehearsal in which read-tier tools run normally
  and anything that would change something reports what it would have done,
  including whether a human would have been asked.
- `agentctl bridge`: a reference MCP client driven by Gemini, which spawns
  `agentctl serve` as a child and runs the whole call/response loop over the
  real transport. It doubles as a schema conformance test: every descriptor is
  asserted to be a fixed point of the declaration sanitizer, so a tool that
  grows a keyword the API rejects fails in CI rather than taking every other
  declaration down with it. The API key is read from the environment, `.env` or
  `~/.agentctl/gemini.key`, never from an argument.
- `agentctl transcript --from-audit`: rebuild the same session record for a
  client that does not cooperate with us, from the audit log. Recordings in
  `docs/fixtures/` are validated against the live tool list in CI.
- `browser_disconnect`, and a shutdown hook so browsers this session launched
  are stopped on exit rather than leaked.
- `notifications/progress` on stdio, driven by `_meta.progressToken`. A wait is
  the one place this server is deliberately slow, so it is the one place
  silence is ambiguous between working and hung.
- MCP resources (`resources/list`, `resources/read`): the latest screenshot,
  the tail of the audit log, and the effective configuration with secrets
  redacted, so a person operating the client can see what the agent is working
  from without spending a turn to ask.
- MCP prompts (`prompts/list`, `prompts/get`): a cookbook for driving a GUI
  app, filling a web form, and acting-and-verifying in one step.
- `agentctl tools` and a generated tool reference at `docs/tools.md`, checked by
  CI so it cannot drift from the descriptors.

### Security

Six defects found by the hardening phase, each now pinned by a test:

- `rm  -rf /` with two spaces walked through the destructive-command check. Substring matching against a
  raw command string is defeated by the space bar; matching now runs on a whitespace-collapsed, lowercased
  form, with privilege words checked as tokens.
- `~/.SSH/id_rsa` bypassed the credential deny-list on macOS's case-insensitive filesystem, opening the
  very same file as `~/.ssh/id_rsa`. Comparison is now case-insensitive.
- IPv6 transition addresses smuggled IPv4 past the SSRF guard: `64:ff9b::a9fe:a9fe` (NAT64) and
  `2002:a9fe:a9fe::` (6to4) both reach cloud metadata. The guard now judges the embedded v4 address.
- An unterminated JSON-RPC frame exhausted memory before any policy ran. Frames are capped and the stream
  resynchronises at the next newline.
- A large floating-point request id came back as a different number, so a client could never match the
  reply to its request. Ids are restricted to strings and integers.
- `notifications/initialized` sent with an id got no reply. Notification-ness is decided by the absence of
  an id, not by the method name.

Also in this release:

- The server's own state directory is denied inside any filesystem root. With `roots = ["~"]` the agent
  could otherwise rewrite `config.toml` to widen its policy, truncate the audit log, or delete the STOP
  file.
- Fuzzing of the JSON-RPC parser with no fuzzing dependency, and red-team suites for the destructive gate,
  the filesystem jail and the SSRF guard. Each suite ends with a test asserting the known bypasses still
  exist, so the limits of a heuristic stay visible.
- **Human override.** Reaching for the mouse now stops the agent. While
  agentctl is driving, a sustained divergence between where the pointer is and
  where the server put it trips the kill switch, cancels any drag in progress,
  and posts a notification explaining how to resume. Configured under `[input]`;
  keyboard has no equivalent signal and is deliberately not covered. The kill
  switch now records *why* it engaged, and persists the reason so a restart
  does not silently resume the agent.

- Results from the tools that return third-party content now carry
  `provenance: "untrusted"`, with an advisory `suspicious_instructions` flag for
  text that reads as an instruction aimed at a model. Applied centrally so it
  cannot be forgotten or spoofed. `redteam_injection.rs` documents what the
  heuristic misses.
- The server's own state directory is denied inside any filesystem root.

### Fixed

Field bugs that only a run on real hardware could surface:

- `AXFocusedApplication` returns nothing on modern macOS even with an app plainly frontmost, which made
  every accessibility and window tool fail. The frontmost process is now resolved from the CoreGraphics
  window list.
- Synthetic mouse events left `kCGMouseEventClickState` unset, so double- and triple-clicks arrived as
  ordinary single clicks, and a plain click behaved as a shift-click by inheriting latched modifiers.
- `keyboard_type` inherited whatever modifiers the system believed were held. With Command latched, every
  character became a menu shortcut: nothing was typed, and the call still reported the full character
  count.
- `list_windows` discarded its `app` argument and answered about whatever was frontmost, so asking about
  one application returned another's windows.
- Drag was a teleport with no intermediate motion, which targets that track movement ignore.
- Sheets, popovers and cross-process authentication panels were invisible to `handle_dialogs`.
- A JavaScript dialog wedged a tab once the CDP `Page` domain was enabled.
- Launched browsers were never stopped: the `Child` handle was dropped, leaking a process and a temporary
  profile per launch.
- Only the filesystem jail canonicalised its roots, so a root configured as `/tmp/work` never matched a
  resolved path on macOS, where `/tmp` is a symlink.
- `expect` was declared beside the arguments rather than among them on all five
  action tools. Valid JSON, and invisible to any client that reads the schema
  to learn what a tool takes, so the act-and-confirm round trip could only be
  used by someone who had read the source.
- `list_apps` returned every process on the machine, including daemons and
  shells that `launch` and `focus_app` would never accept. It now names the
  applications that own windows.
- `list_windows` with no `app` resolved to the frontmost application, so the
  obvious opening question (what is open?) returned one app's windows, or
  none, with nothing to say it had been narrowed. It now covers the machine
  unless an app is named or `focus_app` has pinned one, and reads the
  CoreGraphics window list rather than walking accessibility trees, because
  Chromium exposes no tree until something asks it to.
- `keyboard_type` inherited latched modifier flags, so with Command held every
  character became a menu shortcut: nothing was typed and the call reported
  success.
- `list_windows` ignored its `app` argument and answered about whatever was
  frontmost.
- An accessibility traversal had no time bound. Every attribute read is an IPC
  round trip to the target application, so a busy or large app made each one
  slow: measured at 12 seconds for 58 elements from Finder. Because the
  traversal is synchronous FFI it never yields, so a `wait_for` with a one
  second deadline overran it twelvefold and nothing could interrupt it. The
  walk now has its own budget, and a tree cut short reports `partial: true`
  with advice on narrowing the scope. A partial tree and a genuinely small one
  are otherwise indistinguishable, and an agent that cannot tell concludes the
  control it needs does not exist.

### Known limitations

- Desktop control is macOS-only. The browser and commodity engines are platform-independent.
- Release binaries are unsigned and un-notarized.
- `ocr_region`, `capture_audio` and `virtual_desktop` are deferred; `privilege_run` and process-memory
  writes are deliberate omissions.
- The destructive-command gate cannot see through shell expansion, and hard links are invisible to path
  resolution. Both are documented in [`SECURITY.md`](SECURITY.md) and asserted by tests.

[Unreleased]: https://github.com/paooxide/ui-draw-mcp/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/paooxide/ui-draw-mcp/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/paooxide/ui-draw-mcp/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/paooxide/ui-draw-mcp/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/paooxide/ui-draw-mcp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/paooxide/ui-draw-mcp/tree/v0.1.0
