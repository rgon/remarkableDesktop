# Known limitations

As of v0.6.1 (reMarkable 2, OS 3.28.0.169, GNOME Wayland host).

## Firmware coupling
remvnc drives the panel through internal, unexported parts of reMarkable's
e-ink engine (`/usr/lib/plugins/scenegraph/libqsgepaper.so`):

- `EPFramebuffer::instance()`
- `EPFramebuffer::swapBuffers(QRect, EPScreenMode, QFlags<UpdateFlag>)`
- the screen buffer, found by intercepting `QImage`'s external-buffer constructor

A firmware update can rename or change any of these; 3.28 already changed
`swapBuffers`. If a symbol is missing, `remvnc` stops at startup with an error
and leaves the tablet untouched. The `EPScreenMode` values are inferred from
the library's metadata (Pen, Mono, Animation, UI, Content, Sleep) rather than
documented.

## Handing the screen back to xochitl
xochitl is paused (`SIGSTOP`) while remvnc runs. On exit remvnc copies
xochitl's last frame back to the panel and resumes it. That frame is found by
scanning xochitl's memory for a 1404×1872 RGB32 `QImage`.

- **Exactly one match, or several identical ones:** the frame is restored and
  xochitl is resumed, with no restart.
- **Several different matches** (seen once in four runs): xochitl is resumed and
  then restarted so it repaints, which takes about 10 s. When this happens the
  run logs the image headers, to help find a way to pick the right one.
- While xochitl is paused, its clock, sync and sleep timer are frozen too.

### Recovery
The guardian runs as a transient systemd unit (`remvnc-guardian-<pid>`) and
logs to the journal.

- If remvnc crashes or is killed, the guardian resumes and restarts xochitl.
- If the guardian dies too, xochitl's 60 s systemd watchdog restarts it.
- `remvnc restore` does the same by hand.

## USB HID mode
- **Re-enumeration:** adding or removing our HID function makes USB
  re-enumerate. The USB network drops for about 1–2 s, and any SSH session
  over USB with it. `remvnc start` therefore runs the instance detached, and
  `remvnc-host` reconnects by itself.
- **RNDIS instead of ECM:** with our HID function present, Linux picks the
  gadget's first configuration (RNDIS) instead of ECM. The network still
  works, but under a different interface name.
- **Untested on Windows and macOS.** macOS has no RNDIS driver and may not pick
  the ECM configuration.
- **Mapping without a host:** with no `remvnc-host`, the OS maps the HID
  stylus and touchscreen by itself: GNOME picks a monitor, and the aspect
  ratio may be stretched.
- **Engine signals:** the e-ink engine installs its own SIGTERM/SIGINT
  handlers; we take them back after it loads, so stopping goes through our
  clean exit.
- **No core dumps:** they are disabled for remvnc, because on this tablet they
  go to reMarkable's crash reporter (memfault).
- **HID latency probe:** `--stats` pairs tablet markers with host events in
  order, and resyncs whenever a report goes missing, so it samples only part
  of the reports.

## Network and security
- **Host link:** the instance listens on a TCP port with `TCP_NODELAY`, on
  all interfaces, unencrypted. A host must present a one-time token, which it
  reads from `/run/remvnc/session` (root-only) over SSH, so SSH is what
  authenticates. This is fine over USB (10.11.99.x). Over shared Wi-Fi,
  others on the network can see the streamed screen, and input when not on
  USB HID.

## Input
- **Monitor targeting:** relies on the compositor mapping an unassigned
  absolute device across the bounding box of all monitors (Mutter does). The
  host then squeezes coordinates into the chosen monitor's share. A compositor
  that maps devices differently would put input in the wrong place.
- **Aspect ratio:** targeting a monitor stretches the tablet to fill it.
- **Touchpad mode:** relative, so monitor targeting doesn't apply to it.
- **Pen cursor in some apps:** in PEN mode the app must draw the pen cursor
  (Wayland tablet protocol). Chromium and Electron don't, so no cursor shows.
  Use MOUSE mode, which has no pressure or tilt, or run Chrome with
  `--ozone-platform=x11`.
- **libinput smoothing:** libinput averages the last 4 pen samples unless
  `host/remvnc.quirks` is installed in `/etc/libinput/local-overrides.quirks`
  (then log out and back in). Light strokes register only above 5% pressure.
- **Settings reset each run:** touch mode, pen mode and target monitor are not
  remembered.
- **Palm thresholds:** tuned on one hand (fingertip contact size 8–26, palm
  44–96, threshold 40). Override with `--palm-major` and `--pen-grace-ms`.
- **Latency figures:** `--stats` measures tablet kernel event → host uinput
  write. Compositor, rendering and scanout time come on top.

## Host requirements
- Write access to `/dev/uinput`, via `host/99-remvnc-uinput.rules` and
  membership of the `input` group.
- A Wayland compositor with `xdg-output`, for the monitor list.
- Linux x86_64 or another 64-bit host. The uinput event layout assumes a 64-bit
  `struct timeval`.
