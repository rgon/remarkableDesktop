# Ghosting prediction

The host keeps an estimate of what the panel really shows and uses it to
decide which tiles get a quality ("settle") refresh. Code: `host/src/ghost.rs`
(constants, the CPU reference, the scheduling helpers), `ghost_drive` and
`ghost_err` in `host/src/edges.wgsl`, `Gpu::ghost_drive` / `ghost_tick` in
`host/src/gpu.rs`, `plan_ghost` in `host/src/stream.rs`. `--no-ghost-model`
turns it off and the old `--settle-ms` timer alone decides.

## What the sources support, and what they do not

Verified by reading the page (all are patents or project pages; I found no
peer-reviewed paper that gives a usable per-pixel model, so none is cited):

* Waveforms are lookup tables indexed by the pixel's previous and next state
  (and temperature, and sometimes time since last drive); a pixel whose
  previous and next state are the same gets a zero (or different) drive.
  <https://patents.google.com/patent/US9159274> (waveform index chosen from
  "temperature of the EPD, optical state of the pixel prior to the current
  optical state and the time since the pixel was last driven"; example
  20 ms subframes, black to white 6 subframes). The same LUT shape
  (previous state x next state frames) is in US8237733, listed in the search
  results, not read in full.
* Ghosting is a lightness error between transitions that end at the same
  nominal gray from different previous states, from the limited pulse-width
  resolution (a frame is the smallest unit, about 50 Hz).
  <https://patents.google.com/patent/US8130192> (it fixes it by halftoning
  with an error table, which is not our situation).
* Remnant voltage: after a drive pulse a pixel keeps a voltage that decays
  roughly exponentially ("a sum of exponentials"), and it changes the next
  drive; DC-imbalanced waveforms make it worse; DC balance means the
  applied-voltage integral is zero when the last state equals the first.
  <https://patents.google.com/patent/US10475396> (example: 15 V for about 1 s
  leaves +-3 V, +-1 V one second later, near zero after ten minutes).
  Another patent family computes a per-frame remnant contribution from the
  drive amplitude and a coefficient (search result for
  <https://patents.google.com/patent/US11935495>, not read in full).
* Partial updates: undriven pixels next to driven ones show "blooming" at the
  boundary, which "can be essentially erased in one or two following
  updates", a third update clears the rest, and the older cure was an
  occasional global (flashy) update; white pixels also drift slowly toward
  light gray over minutes to hours.
  <https://patents.google.com/patent/US11557260>. This is the closest source
  to a thin line's trail, but it describes the neighbours of the driven
  pixels, not the pixels erased.
* Controllers keep per-pixel history: a patent claim seen in a web search
  summary stores at least two prior states of each pixel to choose the
  impulse. I could not tie it to a specific patent number, so it is not
  cited; treat it as unverified.
* The rM2 has no EPDC: the vendor library `libqsgepaper` drives the panel
  from software and takes damaged rectangles plus a mode
  (<https://github.com/pl-semiotics/libqsgepaper-snoop>; also
  <https://github.com/ddvk/remarkable2-framebuffer>). Neither page documents
  waveform tables or what `Pen`, `Ui` and `Content` do, and neither says which
  pixels of a rectangle the engine drives. The tablet code
  (`tablet/src/epd.rs`) only names the modes from the library's metadata.
* Not found / not verified: the epdiy project's waveform handling (the repo
  page and the readthedocs URL I tried did not give it; I do not cite
  details), the i.MX EPDC `mxc_epdc_fb` mode semantics (only the API names
  turned up), the FOSDEM 2026 talk on open waveforms (the abstract says
  waveform LUTs are often an undocumented blob and that careless values cause
  ghosting and retention;
  <https://fosdem.org/2026/schedule/event/epaper-driving-waveforms-explained>).

What the sources do NOT give: any number for how far a DU-like or `Pen`
drive moves a pixel, per transition, on this panel. Every `k` below is a
tunable guess. The model structure is the part the sources support: the
state of a pixel is a continuous optical value; a drive moves it by an
amount that depends on the previous and next state (so paint and erase
differ) and is incomplete in the fast mode; the effect accumulates over
the drive history (so a pixel that was not fully erased starts the next
drive from where it is); and only the pixels the engine drives change.

## Model

Per pixel: `est` in 0..65535 (0 black, 65535 white) and `shown`, the 4-bit
level in the tablet's framebuffer (the vendor engine can only know a pixel
changed by comparing with this). All integer arithmetic, so the shader and
the CPU reference `ghost::Model` agree exactly (tested).

`approach(est, goal, k) = est + (goal - est) * k / 256` (truncated).

Updates that were SENT (the sender reports each one, with the picture it
carried and the tiles its rectangles cover; frames that were rendered but not
sent never touch the model, and the diff the sender uses is against the last
sent frame):

* Fast (`Mode::Pen`, binary): with `to = level >= THRESHOLD` and
  `from = shown >= THRESHOLD`, a pixel with `to != from` is driven:
  `to` white (erase): `est = approach(est, 65535, k_erase)`,
  to black (paint): `est = approach(est, 0, k_paint)`. Others keep `est`.
  `shown = level` for every pixel of the rectangle. The edge view (1-bit
  frames) has its own pair `k_paint_edge`, `k_erase_edge`.
* Settle (`Mode::Ui` / `Content`): every pixel of the rectangle,
  `est = approach(est, level * 4369, k_settle)`.
* Reset (new source or mode, or a new GPU): `est = white`, `shown = 15`.
* Redrive (not sent anywhere yet, see below): pixels that should be white
  and are estimated more than the dead band short of it get an erase drive.

Error: `d = |est - level * 4369|`; if `d > DEAD_BAND`,
`(d - DEAD_BAND) >> 8` is added to the pixel's tile (32x32, the streamer's
grid) and to its 8x8 cell. Units: 256 is one pixel that is wholly wrong.
The tile sums go to the CPU with the frame (one small read, same submit); the
cell sums are in the GPU buffer `GCELL` (binding 18; `Gpu::ghost_cells`
reads it back for tests). The cells with error of at least `CELL_ERR` (64, a
quarter pixel) go through the same GPU box passes as the changed cells of a
frame (`rc_*` in `edges.wgsl`, layer 1; see `tiles.rs`) and come back as a
short list of boxes (`Gpu::ghost_boxes`, same submit as the error sums).
The streamer's settle uses them when `SETTLE_BY_BOXES` (stream.rs) is true:
the tiles still decide which tiles are settled and when (error sums, quiet
time, budget), but the refresh covers only the part of each box that lies in
the chosen tiles (`Grid::rects_within`), so a one-pixel trail is refreshed by
a thin box, not a 32x32 tile; chosen tiles no box reaches keep the tile
rectangle. False gives whole tiles as before. What an update drives in the
model is the 8x8 cells its rectangles cover (`GMASK` is per cell), so a
rectangle smaller than a tile is not taken as having refreshed the whole tile.

GPU structure: `ghost_err` is appended to the existing gray and edge command
buffers (same submit as the frame); `ghost_drive` is its own small command
buffer, submitted when updates were sent (it needs the sent picture, which can
be older than the newest frame); a fourth command buffer measures the error
again after drives when no frame arrived (a still screen). Buffers added: one
word per pixel for the estimate (like the other per-pixel buffers), a
host-written copy of the picture sent (4-bit or 1-bit packed size), the tile
mask, and the tile and cell error sums. The picture is copied in by the CPU
(a memcpy of at most 1.3 MB, skipped when it is the one given last); there is
no per-pixel CPU loop.

### Scheduling (`stream.rs`)

A tile is a candidate once it has been sent in fast mode since its last
settle. With the model on:

* error >= `TILE_ERR` (gray view) or `TILE_ERR_EDGE` (edge view) and the tile
  still for `QUIET` (150 ms): settle it (it would be undone by the next fast
  drive otherwise);
* the `--settle-ms` timer stays the upper bound: when it is due the tile is
  settled if its error is at least `TILE_ERR_WEAK` (about a pixel's worth),
  still or not, and dropped without a refresh if it is clean (that is the
  saving over the pure timer);
* a token bucket (`TILES_PER_S`, `BURST_TILES`) limits the settle area; the
  tiles with the largest error go first; a settle never goes in front of a
  changed frame and waits for the tablet to be idle, as before;
* the sender decides only when the model has taken in every update sent
  (`pushed == applied`); otherwise it asks again after 30 ms.

## Parameters

All in `host/src/ghost.rs`, `Params::default()`.

| name | value | status |
| --- | --- | --- |
| `K_PAINT` (gray view white to black) | 230/256 = 0.90 | guess |
| `K_ERASE` (gray view black to white) | 205/256 = 0.80 | guess; erase weaker than paint |
| `K_PAINT_EDGE` | 230/256 = 0.90 | guess |
| `K_ERASE_EDGE` | 154/256 = 0.60 | guess; the trail of a moved thin line |
| `K_SETTLE` | 248/256 = 0.97 | guess |
| `THRESHOLD` | level 8 of 15 | guess (where the binary mode splits) |
| `DEAD_BAND` | one gray step (4369) | guess |
| `TILE_ERR`, `TILE_ERR_EDGE`, `TILE_ERR_WEAK` | 2048, 768, 256 | guess |
| `QUIET` | 150 ms | guess |
| `TILES_PER_S`, `BURST_TILES` | 500, 300 | guess |

Only the structure (paint and erase differ, drive incomplete, history
accumulates, only driven pixels change, previous-state-dependent) comes from
the sources. Nothing numeric does.

## The edge view

The 1-bit outline frames are sent in fast mode, so the interesting case is a
thin line that moves: the pixels it left (black to white) were erased with
`k_erase_edge < 1` and keep a residue; the new line is painted with
`k_paint_edge`. The model keeps these apart, per direction, and keeps the
parameters of the edge view apart from the gray view. A 1 px trail gives few
error units per tile, hence the lower `TILE_ERR_EDGE`; the 8x8 cell map shows
where the trail is, far tighter than a tile.

### Targeted correction instead of a tile settle: evaluated, not enabled

Idea: redrive only the pixels predicted to hold residue (they went black to
white recently), in the fast mode, as small rectangles (from the cell map),
instead of a quality settle of 32x32 tiles. The model has the pass
(`Drive::Redrive`, tested on the CPU and the GPU: it lowers the error, leaves
black pixels alone). It is not wired to the streamer because:

1. Nothing in the sources says a repeated drive pulse or an overdrive removes
   a residue. What they say is about other mechanisms: US11557260 clears
   boundary blooming with one to three further updates, using paired
   waveforms on the neighbours (not on the erased pixels); US8130192 says
   ghosting is a quantization error of the transition, and does not discuss
   repeated pulses or overdrive; US10475396 treats remnant voltage with
   discharge periods, not extra pulses. So the claim "a second fast pass
   helps" is a hypothesis for this panel, to be measured (below).
2. The vendor engine decides which pixels of a rectangle it drives
   (probably by comparing the framebuffer with its own previous copy). An
   identical resend may do nothing. A redrive would need the tablet to write
   the pixel black and then white, or the tablet to hand the engine a
   forced-redraw flag (`swap` has a flags argument, `tablet/src/epd.rs`,
   always 0 today; what its bits mean is not documented), which is a
   protocol and tablet change.

To try it: measure first (calibration below) whether a second `Pen` update of
the same pixels lowers the residue; if it does, add a rect flag to the
protocol and have the host send `Redrive` rectangles boxed from the cell map.

## Calibrating on the panel (photodiode or camera, Hantek)

The unknowns are the `k` values and the dead band, per transition. With a
photodiode (or a phototransistor behind a lens, a few mm of the panel) on a
Hantek channel, ideally with the panel lit by steady light (a DC lamp, not
mains-flickering) and the photodiode current into a resistor or a
transimpedance stage so the signal is a few hundred mV:

1. Record the reference levels: the sensor voltage with a full white page and
   a full black page, each after a quality refresh and 5 s of rest
   (`V_white`, `V_black`). `est = (V - V_black) / (V_white - V_black)` (a
   reflectance, not linear in the model's optical state; fine as a first
   fit, or use a gamma calibration with a gray ramp).
2. `k_erase` and `k_paint` (per view): set the panel to a known state with a
   settle; send one fast update that flips a patch (white to black), wait
   until the signal is steady (a few hundred ms), read `V`; then flip it
   back, read `V`. Then `k_paint = (1 - est1)`, `k_erase = (est2 - est1) /
   (1 - est1)`. Repeat 20 times in a row and look at the sequence: if `est`
   after the n-th flip pair drifts, the history term is bigger than this
   model's and needs a remnant term.
3. Edge view: put a thin line on the photodiode's spot (a line one pixel
   wide is below the sensor's size, so use a sensor with a small aperture, or
   a macro camera with a fixed exposure, and read the mean over the line's
   old position after moving it). Same procedure, with `k_erase_edge`.
4. `k_settle`: send a gray patch fast, read the error, send a settle, read
   again: `k_settle = (e_before - e_after) / e_before`.
5. Time constants: leave a patch after a fast update and read `V` at 0.1,
   1, 10 s; any drift is remnant voltage or drift (the sources: decay over
   seconds to minutes) and would justify a time term in `est`.
6. Thresholds: with a real session (scrolling, typing, a moving cursor in edge
   view), log tile errors (`--stats` could print them) and the moments the
   panel looks ghosted to the eye; set `TILE_ERR*` just under what you see.
   Check the settle rate against the budget.
7. Redrive: in the edge view, move a line, then send the same pixels again in
   `Pen` (a tablet-side test) and read the residue before and after.

The Hantek MCP tools (`hantek_get_capture`, `hantek_measure_now`) read the
sensor channel; trigger on the tablet's update (a GPIO or the first edge of
the signal) and use a timebase of about 2 s/div for step 2.

## What only the panel can validate

Every number in the table; the paint/erase asymmetry itself; whether `Pen`
drives only pixels whose framebuffer value changed (the model assumes it,
by black/white side); whether `Ui` really settles a rectangle whose pixels
did not change (the model assumes it drives all of them); whether a second
fast pass erases a residue; the effect of temperature (the waveform tables
are per temperature; the model has one set of `k`).
