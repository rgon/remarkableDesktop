# Anti-ghost: clearing stuck pixels

Design note, not implemented. No code was changed. Status marks:
**[V]** read on the page named, **[S]** search-result summary only, not read
in full, **[U]** could not be verified, **[I]** inference by the author.
(No configured skill covers e-ink research; this was done with web search and
the repo.)

## Problem

Thin 1-px lines drawn in `Mode::Pen` (edge view) and scrolled content leave
ghosts and pixels that stay gray. The Ui settle (non-flashing) does not clear
them. Hypothesis from the user: flash the zones around the stuck area.

## What the sources say

1. A full-quality update that flashes black and white clears ghosting.
   * libremarkable's constant comments (rM1 mxcfb, not the rM2 engine):
     `WAVEFORM_MODE_INIT` "flashes black/white once to clear ghosting when
     used with UPDATE_MODE_FULL"; `GC16` "High fidelity (flashes black/white
     when used with UPDATE_MODE_FULL)"; `GC16_FAST` "remarkable uses this for
     UI"; `REAGL` "ghost compensation waveform". **[V]**
     <https://raw.githubusercontent.com/canselcik/libremarkable/master/src/framebuffer/common.rs>
   * Glossary: ghosting is "a faint trace of the previous image that fast,
     low-tone-count modes cannot fully clear"; a full refresh, typically
     GC16-based, clears it "at the cost of a visible flash"; DU/A2 are the
     fast modes. Secondary source. **[V]**
     <https://inkframe.net/en/guides/e-paper-waveform-modes-glossary>
   * epdiy (open driver): `epd_clear_area_cycles(area, cycles, cycle_time)`
     "clears an area by flashing it", `cycles` = "number of black-to-white
     clear cycles"; `epd_fullclear()` "Bring the display to a fully white state
     and get rid of any remaining artifacts". The page does not use the word
     ghosting and I could not read the implementation (the raw-source URLs I
     tried were 404 or lacked it), so the pulse sequence is **[U]**.
     <https://epdiy.readthedocs.io/en/latest/api.html> **[V]** for the
     quotes. A project using epdiy says its full refresh runs `epd_clear()`
     first "so it really clears ghosting from unchanged pixels" **[S]**.
   * Kobo/Kindle/KOReader: firmware flashes every N page turns to prevent
     ghosting; Kobo lets the user set 1 to 6 pages. Forum/blog level. **[S]**
   * mxcfb header: `UPDATE_MODE_PARTIAL=0`, `UPDATE_MODE_FULL=1`,
     `EPDC_FLAG_FORCE_MONOCHROME`. KOReader notes on PocketBook: the vendor
     uses GC16 always, faster modes "mild artifacts at best". **[S]**
     <https://coral.googlesource.com/linux-imx/+/refs/tags/9-3/include/uapi/linux/mxcfb.h>
   * FBInk: `--flash` "ask the eInk driver to do a black flash when
     refreshing the area"; REAGL "expect to be flashing in order to behave
     properly" on some devices; no ghosting text. **[V]**
     <https://github.com/NiLuJe/FBInk/blob/master/CLI.md>

2. Why a charge sticks. Remnant voltage is mainly ionic polarisation in the
   display materials; ghosting is its most visible effect; DC imbalance also
   shortens life; the fixes in the patents are discharge periods or backlight,
   i.e. driver design, not user tricks. **[S]** (search-result text of
   US9881564, US8558783, US12339559; US10475396 was read earlier, see
   `docs/GHOSTING.md`.) Consequence **[I]**: pixels driven mostly in one
   direction by an unbalanced fast waveform (Pen, 1-bit line painted and erased
   with incomplete pulses) accumulate a net charge; a stronger, balanced
   black-and-white sequence in a full waveform is the thing that resets them.
   Waiting also helps (decay over seconds to minutes) but is slow.

3. The surround. US11557260: in a partial update the controller also drives
   the neighbours of the changing ("primary") pixels, the "secondary" pixels,
   with a waveform that leaves them in the same optical state, because
   undriven neighbours bloom; the bloom "can be essentially erased in one or
   two following updates", an optional third clears the rest; claim 9 has the
   primary pixels defining an edge of the image. It does not say a global
   update is needed. **[V]**
   <https://patents.google.com/patent/US11557260>
   Consequence **[I]**: a thin line is almost all edge, so its halo is exactly
   this bloom, and Pen drives only the changed pixels, not their neighbours.
   So flashing the surround is supported for the halo; for the trail where
   the line was (pixels driven black to white with an incomplete pulse) it is
   the pixels themselves that need the drive.

4. Freeink-sdk PR 101 (an open e-paper driver): the driver skips a pixel
   already requested white under the same waveform bank when requested white
   again, so a "clean refresh" (black, white, then target) applied twice does
   not clear: only the union of old and new ink is driven and the white
   background never is; the fix is to force the driver. **[V]**
   <https://github.com/Free-Ink/freeink-sdk/pull/101>
   This is the closest evidence that engines skip unchanged pixels and that a
   "flash white over white" is a no-op. It is a different driver; nothing
   proves the reMarkable engine does it. **[U]** for rM2.

5. reMarkable engine internals. Not documented anywhere I could reach.
   `libqsgepaper-snoop`, `remarkable2-framebuffer`, libremarkable's symbol
   list and quill PR 5 give the signature
   `swapBuffers(QRect, EPScreenMode, QFlags<UpdateFlag>)` (3.28) and, for
   older code, `sendUpdate(QRect, WaveformMode, UpdateMode, bool)`; none give
   the enum values, what `UpdateFlag` bits do, or whether unchanged pixels are
   driven. quill reports only that on the Paper Pro modes 0/1 are fast
   mono/gray. **[V]** (absence). So what `Pen`, `Ui`, `Content` do is **[U]**;
   the repo infers the names from metadata (`docs/KNOWN_LIMITATIONS.md`).
   User-level advice for the rM2 (blogs): the tablet refreshes the screen
   fully on page changes and a quick power-button double press or restart
   clears ghosts; weak sources. **[S]**
   <https://github.com/ddvk/remarkable2-framebuffer>,
   <https://github.com/pl-semiotics/libqsgepaper-snoop>,
   <https://github.com/MaximeRivest/quill/pull/5>

Not found: any manufacturer document (E Ink) saying how many cycles clear a
stuck pixel, or a ring-only clearing recipe. Nothing says repeated flashing
beyond one or two cycles helps; epdiy exposes the cycle count, which suggests
the practice, not the proof.

## Answers

**(a) Does a black (then white) flash unstick pixels, region or ring?**
Yes, as far as the sources go, if the pulses are real drives in a full
waveform: the INIT/GC16 flash-black-white is the documented clearing
mechanism (item 1) and epdiy's clear is that sequence. The effect is on the
pixels driven. So:

* Trail and stuck pixels (they hold charge): flash the pixels themselves,
  region plus a margin. A ring only would leave the stuck interior untouched
  and draw a visible frame; it fixes only the halo.
* Halo and bloom at the boundary of a Pen update: the surround matters
  (US11557260), and a margin of at least one pixel beyond the stuck region
  covers it. One rectangle (box plus margin) does both and has no inner edge
  to show, so: **flash the box plus margin, not the ring.** The user's idea
  is right about the surround, wrong about leaving the middle out.
* The margin must stay inside the redraw, because a flash only on the margin
  would show a black/white border (the flash waveform ends in the new frame,
  so a margin that is redrawn together with the interior is fine).

**(b) Which mode, and does fill plus swap of an already-white region drive?**
Unknown for the rM2 engine **[U]**. `Mode::Content` is the best guess for
the flashing GC16-like update (the repo already calls it "flashing
full-quality"; `Ui` is probably GC16_FAST-like, "remarkable uses this for
UI" in the rM1 header **[V]** for rM1 only). `Mode::Mono`, `Animation`
untested. `swap()` passes `flags = 0` always; the `UpdateFlag` bits are
unknown, and a "full update" bit is the thing to find (see experiment).
Assume the engine drives only pixels whose framebuffer value changed (item 4
shows other drivers do; the ghost model also assumes it). Then:

* `clear_screen` in `tablet/src/daemon.rs` fills white, swaps, fills black,
  swaps, redraws, swaps (all `Content`). Under that assumption every pixel
  gets at least one real change: pixels white in the frame are driven only in
  the black step (and again in the redraw), pixels black in the frame in the
  white step and the redraw. It works, but a stuck-gray pixel that is white
  in the frame sees a single black pulse and a single white one, no
  white-then-black-then-white. If the engine instead compares with a
  stuck-state estimate or drives all pixels, it is fine too.
* The "white" step over a white region is a no-op in the skip model, so a
  region clear must start with **black**, then white, then redraw. B, W, redraw
  gives every pixel two real drives under the skip model.
* A way to avoid depending on skipping: make each step a real change for
  every pixel. A black fill and a white fill are, except for pixels already
  that color in the previous step, which is the case above; no further
  forcing is needed unless the experiment shows the engine ignores the
  change test the other way (drives nothing for same-valued pixels even
  after a mode change).

**(c) Minimal policy.**

1. The ghost model cannot see stuck pixels: its `est` is pulled to the
   target by every settle (`K_SETTLE` 0.97), so after one settle it reports
   clean, which is exactly the user's symptom. Add a model-free *charge
   debt* per 32x32 tile (the streamer's grid), in `host/src/stream.rs`:
   debt += number of pixels driven in fast mode in that tile (use the
   `dirty` mask, weighted x2 for the edge view, which paints and erases 1-bit
   lines with incomplete pulses; the same count the model already folds),
   debt -= a large step (set to 0) when the tile gets a Clear. A settle in
   Ui reduces it only a little (say halve; **[U]** needs measuring).
2. Trigger a Clear for tiles whose debt is above a threshold, and that
   have been still for a long time (`CLEAR_QUIET` about 2 s; longer than
   `QUIET` 150 ms, so scrolling and typing are never flashed), after their
   normal settle has been sent. Scrolling resets the quiet timer, so a
   clear only follows the scroll, never interrupts it.
3. Region: the tiles chosen, dilated by one tile (32 px, covers the halo),
   merged into rectangles by the existing `grid.rects`. If the merged area
   exceeds about a third of the panel, send one full-panel clear instead
   (one flash is less annoying than many).
4. Mode: tablet-side sequence in `Content` (flashing): black fill, swap;
   white fill, swap; restore the frame's pixels, swap (still `Content`, so
   the last step lands the real grays cleanly). One cycle by default,
   `--clear-cycles` for more.
5. Rate: at most one clear per 15 s and at most ~10 % of the panel per
   clear except the full-panel case; a clear never goes in front of a
   changed frame (same rule as settles) and is dropped if the tile changes
   before it is sent. Needs the budget in `Budget` as a second bucket.
6. Model: after a clear, `Drive::Clear` sets `est` to the target and
   `shown = level` for the covered tiles (like a settle with `k = 256`), and
   `Unsettled` is cleared for them. In the edge view the redraw is the 1-bit
   frame converted to its gray levels, so the settle in `Content` also
   restores gray; the edge view being 1-bit does not change the policy.
7. CLR button: black fill, `Content` swap; white fill, `Content` swap;
   restore frame, `Content` swap. Do it for the whole panel (as now, but
   black first), twice if the experiment says so, then tell the host so it
   resets the model and the `Unsettled` map (today the host never learns
   of the clear; the model's `est` keeps its old, now wrong, state, and a
   later settle may be sent or skipped wrongly).

**(d) Protocol and tablet changes (exact places).**

* `proto/src/lib.rs`: add `RECT_CLEAR: u8 = 8` next to `RECT_SETTLE`,
  `RECT_BITS1`, `RECT_LAST`: "flash this rect black and white, then redraw it
  in the settle mode". Round-trip test beside the `RECT_SETTLE | RECT_LAST`
  one. A tablet-to-host message for the CLR button: either a new
  `Msg::Cleared` (new tag, optional so an old peer ignores it) or re-send
  `Msg::Display { mode }`, which probably triggers `fresh_start` in the
  sender (not checked); add it only after reading how `fresh_start` is set.
* `tablet/src/display.rs`: `Display::show`: if `flags & RECT_CLEAR != 0`,
  run the clear sequence on `r` (save the rect's pixels from `epd.fb`, fill
  black, swap `Mode::Content`, fill white, swap, put the pixels back, swap
  in the settle mode, `Content`); count it in `Timing` separately since it
  blocks for the length of the flashes. A helper `fill_gray` already exists
  in `tablet/src/epd.rs` (`ImageInfo::fill_gray`). `tablet/src/daemon.rs`
  at the `Incoming::Decoded` branch needs no change besides skipping
  `ink.overwritten` ordering (the ink preview must be restored first,
  `ink.restore`).
* `tablet/src/daemon.rs::clear_screen`: order black, white; restore;
  send the new message to the host. `clear_ink` stays Ui.
* `host/src/stream.rs`: `Unsettled` gets a `debt: Vec<u32>` (or a new
  struct beside it); `plan_ghost` returns a `Plan::Clear` or a separate
  `plan_clear` called when no settle is due; `send_rects(&rects, frame,
  RECT_SETTLE | RECT_CLEAR, None)` in a new `Work::Clear` arm next to
  `Work::Settle`, followed by `push(Drive::Clear, ...)`.
* `host/src/ghost.rs` and `host/src/gpu.rs` / `host/src/edges.wgsl`: add
  `Drive::Clear` (CPU `Model` and the shader's `ghost_drive`), constants
  `CLEAR_DEBT`, `CLEAR_QUIET`, `CLEAR_MIN_INTERVAL`, `CLEAR_MARGIN_TILES`.
* `tablet/src/epd.rs`: if the experiment finds a full-update flag, pass it
  in `swap(..., flags)` for the clear steps (`flags` is 0 today).

**(e) What only the panel can tell, and the experiment.**
Unknown until measured: what `Pen`/`Ui`/`Content` do; whether `Content` over a
rect flashes and how long (ms) per step; whether a swap of unchanged pixels
drives them; what the `UpdateFlag` bits do; how much of a stuck charge one
B-W cycle clears; whether the margin matters; the debt constants.

Setup: photodiode behind a small aperture (or a phone camera on a tripod,
fixed exposure and white balance, slow-motion if possible) on a Hantek
channel as in `docs/GHOSTING.md` (steady DC lamp, TIA so the signal is a few
hundred mV). Reference: all-white and all-black levels after a CLR, 5 s rest.

1. Make a stuck area: in the edge view draw a 1-px line, move it 50 times
   (or scroll text for 30 s), then stop. Record the photodiode on the old
   line position (trail) and 2 px beside the line (halo) after 1 s and 10 s.
2. Test each treatment on a fresh stuck area, measure the residue after 5 s:
   A Ui settle (today); B `Content` settle of the same pixels with the
   framebuffer unchanged (checks whether unchanged pixels are driven: watch the
   scope for a flash and the camera for flashing); C black fill, white
   fill, redraw in `Content` on the box; D C on box plus 32 px margin; E C on
   the ring only (box left alone); F C twice; G the whole-panel CLR;
   H wait 5 min without doing anything.
3. Expected from the sources: C/D clear the trail, D clears the halo too, E
   leaves the trail, B does nothing if the engine skips unchanged pixels, F
   clears slightly more than C. A result that contradicts any of these changes
   the policy; a flash that leaves the surround darker or lighter than the
   rest of the panel (visible edge) means the margin should be larger or the
   whole-panel path used more.
4. Log the duration of each `epd.swap` call (the `Timing` in `Display`
   already does) to size the rate limit; the scope's trigger on the first
   edge of the flash gives the actual optical time.
5. Try `flags` values 1, 2, 4 in `swap` on a test build (the sources do not
   say what they do) and look for the one that makes a flash on an unchanged
   rect; use it only if it does and is stable across firmware.
6. Repeat at two temperatures if possible (waveforms are per temperature).

## Recommendation (first)

Stop relying on the model to find stuck pixels: it cannot, because a settle
resets its estimate. Add the tablet-side clear (black, white, redraw, all
`Content`) as a new rect flag `RECT_CLEAR`, send it for the boxes (plus a one
tile margin) of tiles with high accumulated fast-drive debt after a long quiet,
rate limited, and fix the CLR button to go black first, then white, redraw,
and tell the host so it resets its model. Validate the engine behaviour
(experiment 2B and 5) before tuning anything.
