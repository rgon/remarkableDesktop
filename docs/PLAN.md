# Plan

## Done
- **v1, input and control:** pen and touch forwarded to the host through
  uinput over a direct TCP link (p50 about 1.3 ms), palm rejection, hold
  buttons, monitor targeting.
- **Drawing on the panel:** through the vendor e-ink engine (`libqsgepaper`),
  with xochitl paused and restored afterwards.
- **v2, display:** a virtual or mirrored monitor streamed as 4-bit gray
  updates with acks. Fast refresh, then a settle refresh. The monitor layout
  is kept. Pen and touch are mapped through GNOME's per-device settings.

## Done in v0.6: the tablet as a USB HID device
Implemented as planned below: the FunctionFS gadget, a tablet-owned instance
(`remdesktop start`, `stop`, `session`) with host reconnects, an info box,
greyed-out host-only options, and a SIDE button. Verified on Linux (GNOME):
`hid-multitouch` creates Stylus, Touchscreen and Touchpad inputs. Windows and
macOS are untested.

## Design notes (kept for reference)
**Goal:** input works on any computer with no host software. The tablet itself
is a USB tablet (pen digitizer, touch, mouse), next to the existing USB
network, which stays up. `remdesktop-host` becomes optional, for screen streaming
and GNOME monitor mapping.

### What the tablet has (checked 2026-10-02, OS 3.28, kernel 5.4.70)
- **USB gadget:** a configfs gadget `g_ether` on UDC `ci_hdrc.0`, vendor and
  product 0x04b3:0x4010:
  - config `c.1`: `rndis.usb0`, for Windows;
  - config `c.2`: `ecm.usb1`, for Linux and macOS.
- **No HID gadget function:** `usb_f_hid` isn't built.
- **FunctionFS is built in.** Its filesystem registers once a `functions/ffs.*`
  instance exists, which was verified with a throwaway instance. So our own
  binary can implement HID, with no kernel module.
- **No Wi-Fi SSH:** the tablet is on Wi-Fi, but SSH is only reachable over
  USB. Re-enumerating USB therefore drops our SSH session for a moment.

### Approach
1. **Gadget (tablet, `hid.rs`):**
   1. Create `functions/ffs.remdesktop` and mount it at `/dev/ffs-remdesktop`.
   2. Write the descriptors (one HID interface, interrupt IN at 1 ms) and the
      strings to `ep0`.
   3. Link the function into both `c.1` and `c.2`, then rebind the UDC.
   4. Answer `GET_DESCRIPTOR` (HID report), `SET_IDLE`, `SET_PROTOCOL` and
      `GET_REPORT` from the `ep0` setup events.
   5. Write reports to `ep1`.
   6. Treat `ENABLE`/`DISABLE`/`SUSPEND` as "USB host connected / not".
2. **Report descriptor:** one report ID per device, so pen and touch modes
   switch without re-enumerating:
   - pen digitizer: tip, barrel, eraser, invert, in range, X, Y, pressure,
     X/Y tilt, with physical units;
   - multitouch touchscreen: contact count, plus per contact tip, confidence
     (our palm rejection decides it), contact ID, X and Y;
   - touchpad (precision touchpad layout);
   - absolute mouse, for the pen's mouse mode.
3. **Restore:** the guardian learns the gadget changes. On exit or crash it
   unlinks our function, rebinds the UDC, unmounts and removes the function,
   which brings back exactly what `usb-ether-once` built.
4. **Tablet-owned session:**
   - `remdesktop start` runs the instance detached, as a systemd unit, so the USB
     re-enumeration and SSH sessions coming and going don't kill it.
   - `remdesktop attach` is a small process started over SSH by `remdesktop-host`. It
     hands the host link (direct TCP socket or SSH stdio) to the running
     instance.
   - Input goes over USB HID when USB is enumerated; otherwise over the host
     link with uinput, as now.
5. **On-screen info box:** "USB: connected / not connected" and
   "Host: connected (streaming) / not running". Options that need the host
   (screen views, display streaming) are greyed out and inert without it.
6. **Host:** `remdesktop-host` starts the instance if needed, waits for USB to
   re-enumerate, then attaches. It keeps mapping devices through GNOME
   settings, now by the HID device's vendor:product.

### Steps
1. Prototype: a FunctionFS HID mouse that draws circles for about 15 s, run
   detached, then restore the gadget. Confirms that FunctionFS passes HID
   class requests through, that the composite enumerates with RNDIS/ECM, and
   how long the network blip lasts.
2. The full report descriptor and the HID input path (pen, touch, mouse).
3. Daemon and attach split; the guardian also restores the gadget.
4. Info box and greyed-out options.
5. Host: start, wait, attach, and mapping via HID ids.

### Risks
- **Network blip on (re)enumeration:** SSH over USB drops for about 1–2 s, so
  the instance has to be detached and the host has to reconnect.
- **OS config choice:** Windows picks `c.1` (RNDIS), Linux and macOS `c.2`
  (ECM). The HID function goes in both.
- **Gadget persistence:** reMarkable may rebuild the gadget on replug, reboot
  or update, so it's re-applied each run and never written to
  `usb-ether-once`.
