# Known limitations

As of v0.4.2 (reMarkable 2, OS 3.28.0.169, GNOME Wayland host).

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

## Network and security
- **Direct link:** by default input travels over a direct TCP socket with
  `TCP_NODELAY`, unencrypted. SSH still launches and authenticates the session:
  the socket only listens on the address the SSH client connected to, only
  accepts the SSH client's IP, and requires a one-time token sent over SSH.
  This is fine over USB (10.11.99.x). Over shared Wi-Fi, others on the network
  can see the input.
- **`--ssh-only`:** keeps everything inside SSH, but dropbear leaves Nagle on
  for non-PTY sessions, which adds stalls of about 40 ms.

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
