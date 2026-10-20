// The whole picture path on the GPU: the raw capture to the panel's gray
// frame (scale, letterbox, rotate), then for the edge view blur, Sobel,
// non-maximum suppression, hysteresis (as connected components, in a fixed
// number of passes), the placement of the one-pixel line on
// the darker side, dark pixels, the diff against the previous frame and the
// packing to bits. build.rs turns this into SPIR-V (naga); gpu.rs runs it.
//
// Every buffer holds one u32 per pixel, except the packed input and result
// (four pixels per word), so no stage needs 8-bit storage access.

struct Params {
    // The frame the stages work on: the panel's portrait size.
    w: u32,
    h: u32,
    // Pixels of the original darker than this turn black, whatever the
    // outlines say (0: off).
    dark: u32,
    // The raw capture's size, the tablet's logical (unrotated) size, the
    // rotation to the panel (0 none, 1 counterclockwise, 2 half turn, 3
    // clockwise) and where the scaled capture sits in the logical frame.
    src_w: u32,
    src_h: u32,
    log_w: u32,
    log_h: u32,
    rot: u32,
    sc_x: u32,
    sc_y: u32,
    sc_w: u32,
    sc_h: u32,
    // The ghosting model: what the pass does (1 fast, 2 settle, 3 reset, 4
    // redrive), the picture's kind (0 gray4, 1 bits), the shares of a drive
    // in 1/256 (paint = white to black, erase = black to white; gray view,
    // then edge view; settle), the level that splits black from white and
    // the dead band.
    g_mode: u32,
    g_kind: u32,
    g_k_paint: u32,
    g_k_erase: u32,
    g_e_paint: u32,
    g_e_erase: u32,
    g_k_settle: u32,
    g_thr: u32,
    g_deadband: u32,
    // 1 when the capture's bytes are R, G, B, x instead of B, G, R, x.
    swap: u32,
    // The dirty rectangles: the frame's slot (0..31) in the history word of
    // each cell, the slots of the frames since the one the sender has (a bit
    // per slot), and 1 when the frame has nothing to be compared with (every
    // cell counts as changed).
    r_slot: u32,
    r_mask: u32,
    r_full: u32,
}

// The gray frame (made by `convert` from the raw capture), four pixels to a word.
@group(0) @binding(0) var<storage, read_write> src_px: array<u32>;
@group(0) @binding(1) var<storage, read_write> buf_tmp: array<u32>;
@group(0) @binding(2) var<storage, read_write> buf_blur: array<u32>;
// Gradient magnitude in the low 16 bits, direction (0 x, 1 y, 2 diagonal,
// 3 anti-diagonal) above.
@group(0) @binding(3) var<storage, read_write> buf_grad: array<u32>;
// 0 nothing, 1 weak edge candidate, 2 edge.
@group(0) @binding(4) var<storage, read_write> buf_state: array<u32>;
// 0 on an outline pixel, 255 elsewhere (filled with 255 before the stages run).
@group(0) @binding(5) var<storage, read_write> buf_out: array<u32>;
// Union-find forest over the pixels for hysteresis: each pixel points to a
// pixel of its component with a smaller index, roots point to themselves.
@group(0) @binding(6) var<storage, read_write> buf_label: array<atomic<u32>>;
// The finished frame, one bit per pixel (set = white), MSB first, every row
// padded to whole bytes: the wire format of `bits1` in the proto crate.
@group(0) @binding(7) var<storage, read_write> buf_bits: array<u32>;
@group(0) @binding(8) var<storage, read> prm: Params;
// The raw capture: one BGRx pixel per word.
@group(0) @binding(11) var<storage, read> buf_src: array<u32>;
// The gray frame as `gray4` packs it (see pack_gray4), and the previous one.
@group(0) @binding(12) var<storage, read_write> buf_gray4: array<u32>;
@group(0) @binding(13) var<storage, read_write> buf_prev4: array<u32>;
// One flag per tile of the grid in tiles.rs: set when the outline differs
// from the previous frame's anywhere in it.
@group(0) @binding(9) var<storage, read_write> buf_tiles: array<atomic<u32>>;
// The previous frame's outline (copied from buf_out when a frame is done).
@group(0) @binding(10) var<storage, read> buf_prev: array<u32>;
// The ghosting model's state per pixel, the picture last sent (written by
// the host, packed as g_kind says), the 8x8 cells that update covered, and
// the error sums per tile and per 8x8 cell.
@group(0) @binding(14) var<storage, read_write> buf_est: array<u32>;
@group(0) @binding(15) var<storage, read> buf_sent: array<u32>;
@group(0) @binding(16) var<storage, read> buf_gmask: array<u32>;
@group(0) @binding(17) var<storage, read_write> buf_gerr: array<atomic<u32>>;
@group(0) @binding(18) var<storage, read_write> buf_gcell: array<atomic<u32>>;

// The history of changed cells: one word per 8x8 cell, bit k set when the
// cell changed in the frame that used slot k (frame number mod 32).
@group(0) @binding(19) var<storage, read_write> buf_cword: array<atomic<u32>>;
// Working storage of the rectangle passes, a section per layer (0 the
// changed cells, 1 the cells with ghosting error): see `map_at`.
@group(0) @binding(20) var<storage, read_write> buf_rc: array<atomic<u32>>;
// The result the CPU reads, a section per layer: a count, a counter for the
// sub-boxes, then the boxes.
@group(0) @binding(21) var<storage, read_write> buf_rlist: array<atomic<u32>>;

// Must equal tiles::TILE.
const TILE: u32 = 32u;

// Gradient magnitudes (|gx| + |gy| of a Sobel, four times the brightness step):
// an edge is certain from HIGH, and may fade to LOW while joined to one.
const LOW: i32 = 112;
const HIGH: i32 = 280;

fn gray(i: u32) -> u32 {
    return (src_px[i >> 2u] >> ((i & 3u) * 8u)) & 255u;
}

fn src_luma(x: i32, y: i32) -> f32 {
    let p = buf_src[u32(y) * prm.src_w + u32(x)];
    var r = (p >> 16u) & 255u;
    var b = p & 255u;
    if (prm.swap != 0u) {
        r = p & 255u;
        b = (p >> 16u) & 255u;
    }
    return 0.299 * f32(r) + 0.587 * f32((p >> 8u) & 255u) + 0.114 * f32(b);
}

// The gray of one panel pixel from the raw capture: undo the rotation to
// find the logical pixel, leave the bars white, else scale bilinearly.
fn converted(ox: u32, oy: u32) -> u32 {
    let lw = i32(prm.log_w);
    let lh = i32(prm.log_h);
    var lx = i32(ox);
    var ly = i32(oy);
    if (prm.rot == 1u) {
        lx = lw - 1 - i32(oy);
        ly = i32(ox);
    } else if (prm.rot == 2u) {
        lx = lw - 1 - i32(ox);
        ly = lh - 1 - i32(oy);
    } else if (prm.rot == 3u) {
        lx = i32(oy);
        ly = lh - 1 - i32(ox);
    }
    let rx = lx - i32(prm.sc_x);
    let ry = ly - i32(prm.sc_y);
    if (rx < 0 || ry < 0 || rx >= i32(prm.sc_w) || ry >= i32(prm.sc_h)) {
        return 255u;
    }
    let fx = (f32(rx) + 0.5) * f32(prm.src_w) / f32(prm.sc_w) - 0.5;
    let fy = (f32(ry) + 0.5) * f32(prm.src_h) / f32(prm.sc_h) - 0.5;
    let x0 = i32(floor(fx));
    let y0 = i32(floor(fy));
    let tx = fx - f32(x0);
    let ty = fy - f32(y0);
    let maxx = i32(prm.src_w) - 1;
    let maxy = i32(prm.src_h) - 1;
    let xa = clamp(x0, 0, maxx);
    let xb = clamp(x0 + 1, 0, maxx);
    let ya = clamp(y0, 0, maxy);
    let yb = clamp(y0 + 1, 0, maxy);
    let top = mix(src_luma(xa, ya), src_luma(xb, ya), tx);
    let bottom = mix(src_luma(xa, yb), src_luma(xb, yb), tx);
    return u32(clamp(mix(top, bottom, ty) + 0.5, 0.0, 255.0));
}

// Raw capture to the gray frame, four panel pixels per thread.
@compute @workgroup_size(16, 16)
fn convert(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let n = w * prm.h;
    let k = g.y * w + g.x;
    if (k * 4u >= n) {
        return;
    }
    var word = 0u;
    for (var j = 0u; j < 4u; j++) {
        let i = k * 4u + j;
        if (i < n) {
            word |= converted(i % w, i / w) << (8u * j);
        }
    }
    src_px[k] = word;
}

fn gray_x(row: u32, x: i32, wi: i32) -> u32 {
    return gray(row + u32(clamp(x, 0, wi - 1)));
}

// 1 4 6 4 1 / 16, borders clamped.
@compute @workgroup_size(16, 16)
fn blur_h(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let wi = i32(w);
    let x = i32(g.x);
    let row = g.y * w;
    let acc = gray_x(row, x - 2, wi) + 4u * gray_x(row, x - 1, wi) + 6u * gray_x(row, x, wi) + 4u * gray_x(row, x + 1, wi) + gray_x(row, x + 2, wi);
    buf_tmp[row + g.x] = acc / 16u;
}

fn tmp_y(y: i32, x: u32, w: u32, hi: i32) -> u32 {
    return buf_tmp[u32(clamp(y, 0, hi - 1)) * w + x];
}

@compute @workgroup_size(16, 16)
fn blur_v(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let hi = i32(prm.h);
    let y = i32(g.y);
    let acc = tmp_y(y - 2, g.x, w, hi) + 4u * tmp_y(y - 1, g.x, w, hi) + 6u * tmp_y(y, g.x, w, hi) + 4u * tmp_y(y + 1, g.x, w, hi) + tmp_y(y + 2, g.x, w, hi);
    buf_blur[g.y * w + g.x] = acc / 16u;
}

fn bl(x: i32, y: i32) -> i32 {
    return i32(buf_blur[u32(y) * prm.w + u32(x)]);
}

fn interior(x: i32, y: i32) -> bool {
    return x >= 1 && y >= 1 && x < i32(prm.w) - 1 && y < i32(prm.h) - 1;
}

@compute @workgroup_size(16, 16)
fn sobel(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let x = i32(g.x);
    let y = i32(g.y);
    var v = 0u;
    if (interior(x, y)) {
        let gx = (bl(x + 1, y - 1) + 2 * bl(x + 1, y) + bl(x + 1, y + 1)) - (bl(x - 1, y - 1) + 2 * bl(x - 1, y) + bl(x - 1, y + 1));
        let gy = (bl(x - 1, y + 1) + 2 * bl(x, y + 1) + bl(x + 1, y + 1)) - (bl(x - 1, y - 1) + 2 * bl(x, y - 1) + bl(x + 1, y - 1));
        let ax = abs(gx);
        let ay = abs(gy);
        // tan(22.5 degrees) ~ 0.414 separates the axes from the diagonals.
        var d = 3u;
        if (ay * 1000 < ax * 414) {
            d = 0u;
        } else if (ax * 1000 < ay * 414) {
            d = 1u;
        } else if ((gx > 0) == (gy > 0)) {
            d = 2u;
        }
        v = u32(ax + ay) | (d << 16u);
    }
    buf_grad[g.y * w + g.x] = v;
}

fn mag(i: u32) -> i32 {
    return i32(buf_grad[i] & 0xffffu);
}

// Local maxima across the edge, then the two thresholds.
@compute @workgroup_size(16, 16)
fn nms(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let x = i32(g.x);
    let y = i32(g.y);
    let i = g.y * w + g.x;
    var s = 0u;
    if (interior(x, y)) {
        let m = mag(i);
        if (m >= LOW) {
            let d = buf_grad[i] >> 16u;
            var a = i - 1u;
            var b = i + 1u;
            if (d == 1u) {
                a = i - w;
                b = i + w;
            } else if (d == 2u) {
                a = i - w - 1u;
                b = i + w + 1u;
            } else if (d == 3u) {
                a = i - w + 1u;
                b = i + w - 1u;
            }
            // `>` on one side and `>=` on the other: a plateau keeps one pixel.
            if (m > mag(a) && m >= mag(b)) {
                if (m >= HIGH) {
                    s = 2u;
                } else {
                    s = 1u;
                }
            }
        }
    }
    buf_state[i] = s;
}

// Hysteresis as connected components of the candidates (weak or edge pixels,
// 8-connected): a weak pixel becomes an edge when its component holds one,
// which is what promoting weak pixels next to edges until nothing changes
// gives. buf_tmp (free after the blur) flags the components with an edge.
@compute @workgroup_size(16, 16)
fn cc_init(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= prm.w || g.y >= prm.h) {
        return;
    }
    let i = g.y * prm.w + g.x;
    atomicStore(&buf_label[i], i);
    buf_tmp[i] = 0u;
}

// The root of a pixel's component, shortening the path on the way.
fn find(start: u32) -> u32 {
    var x = start;
    loop {
        let p = atomicLoad(&buf_label[x]);
        if (p == x) {
            return x;
        }
        let gp = atomicLoad(&buf_label[p]);
        atomicStore(&buf_label[x], gp);
        x = gp;
    }
    return x;
}

// Joins two components, the larger root under the smaller.
fn join(a0: u32, b0: u32) {
    var a = find(a0);
    var b = find(b0);
    loop {
        if (a == b) {
            return;
        }
        let hi = max(a, b);
        let lo = min(a, b);
        if (atomicCompareExchangeWeak(&buf_label[hi], hi, lo).exchanged) {
            return;
        }
        a = find(hi);
        b = find(lo);
    }
}

// Each candidate joins the candidates before it among its neighbors.
@compute @workgroup_size(16, 16)
fn cc_link(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let i = g.y * w + g.x;
    if (buf_state[i] == 0u) {
        return;
    }
    if (g.x > 0u && buf_state[i - 1u] != 0u) {
        join(i, i - 1u);
    }
    if (g.y > 0u) {
        if (buf_state[i - w] != 0u) {
            join(i, i - w);
        }
        if (g.x > 0u && buf_state[i - w - 1u] != 0u) {
            join(i, i - w - 1u);
        }
        if (g.x + 1u < w && buf_state[i - w + 1u] != 0u) {
            join(i, i - w + 1u);
        }
    }
}

// Flags the components that hold an edge.
@compute @workgroup_size(16, 16)
fn cc_flag(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= prm.w || g.y >= prm.h) {
        return;
    }
    let i = g.y * prm.w + g.x;
    if (buf_state[i] == 2u) {
        buf_tmp[find(i)] = 1u;
    }
}

// Weak pixels of a flagged component become edges.
@compute @workgroup_size(16, 16)
fn hyst(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= prm.w || g.y >= prm.h) {
        return;
    }
    let i = g.y * prm.w + g.x;
    if (buf_state[i] == 1u && buf_tmp[find(i)] == 1u) {
        buf_state[i] = 2u;
    }
}

fn step_of(d: u32) -> vec2<i32> {
    if (d == 0u) {
        return vec2<i32>(1, 0);
    }
    if (d == 1u) {
        return vec2<i32>(0, 1);
    }
    if (d == 2u) {
        return vec2<i32>(1, 1);
    }
    return vec2<i32>(1, -1);
}

// One black pixel per edge pixel: the edge pixel itself, or the one behind
// it if that is darker in the original, so outlines sit inside shapes.
@compute @workgroup_size(16, 16)
fn place_line(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let x = i32(g.x);
    let y = i32(g.y);
    let i = g.y * w + g.x;
    if (!interior(x, y) || buf_state[i] != 2u) {
        return;
    }
    let st = step_of(buf_grad[i] >> 16u);
    // Toward the brighter side.
    var s = st;
    if (buf_blur[u32(y + st.y) * w + u32(x + st.x)] < buf_blur[u32(y - st.y) * w + u32(x - st.x)]) {
        s = -st;
    }
    var k = 0;
    if (gray(u32(y - s.y) * w + u32(x - s.x)) < gray(i)) {
        k = -1;
    }
    buf_out[u32(y + k * s.y) * w + u32(x + k * s.x)] = 0u;
}

// The naive fill: dark pixels are black, with no regard for outlines.
@compute @workgroup_size(16, 16)
fn dark_px(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h || prm.dark == 0u) {
        return;
    }
    let i = g.y * w + g.x;
    if (gray(i) < prm.dark) {
        buf_out[i] = 0u;
    }
}

// Which tiles changed since the previous frame.
@compute @workgroup_size(16, 16)
fn diff_tiles(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let i = g.y * w + g.x;
    if ((buf_out[i] & 255u) != (buf_prev[i] & 255u)) {
        let cols = (w + TILE - 1u) / TILE;
        atomicStore(&buf_tiles[(g.y / TILE) * cols + g.x / TILE], 1u);
        atomicOr(&buf_cword[(g.y / CELL) * cw() + g.x / CELL], 1u << prm.r_slot);
    }
}

// The frame as bits. Each thread makes one output word (four bytes of the
// packed stream), which may span rows, so rows are found from the byte index.
@compute @workgroup_size(16, 16)
fn pack_bits(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let row_bytes = (w + 7u) / 8u;
    let total = row_bytes * prm.h;
    let k = g.y * w + g.x;
    if (k * 4u >= total) {
        return;
    }
    var word = 0u;
    for (var j = 0u; j < 4u; j++) {
        let b = k * 4u + j;
        if (b < total) {
            let row = b / row_bytes;
            let col = b % row_bytes;
            var byte = 0u;
            for (var i = 0u; i < 8u; i++) {
                let x = col * 8u + i;
                if (x < w && (buf_out[row * w + x] & 255u) > 127u) {
                    byte |= 0x80u >> i;
                }
            }
            word |= byte << (8u * j);
        }
    }
    buf_bits[k] = word;
}

fn level4(v: u32) -> u32 {
    return (v * 15u + 127u) / 255u;
}

// The gray frame as 4-bit levels, two pixels a byte (the first in the high
// nibble), rows padded to whole bytes: the wire format of `gray4` in the
// proto crate. Each thread makes one output word, found from its byte index.
@compute @workgroup_size(16, 16)
fn pack_gray4(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let row_bytes = (w + 1u) / 2u;
    let total = row_bytes * prm.h;
    let k = g.y * w + g.x;
    if (k * 4u >= total) {
        return;
    }
    var word = 0u;
    for (var j = 0u; j < 4u; j++) {
        let b = k * 4u + j;
        if (b < total) {
            let row = b / row_bytes;
            let x = (b % row_bytes) * 2u;
            let hi = level4(gray(row * w + x));
            var lo = 0u;
            if (x + 1u < w) {
                lo = level4(gray(row * w + x + 1u));
            }
            word |= ((hi << 4u) | lo) << (8u * j);
        }
    }
    buf_gray4[k] = word;
}

// Which tiles changed since the previous gray frame.
@compute @workgroup_size(16, 16)
fn diff_gray4(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let b = g.y * ((w + 1u) / 2u) + g.x / 2u;
    let shift = (b & 3u) * 8u;
    let a = (buf_gray4[b >> 2u] >> shift) & 255u;
    let p = (buf_prev4[b >> 2u] >> shift) & 255u;
    var now = a & 15u;
    var before = p & 15u;
    if ((g.x & 1u) == 0u) {
        now = a >> 4u;
        before = p >> 4u;
    }
    if (now != before) {
        let cols = (w + TILE - 1u) / TILE;
        atomicStore(&buf_tiles[(g.y / TILE) * cols + g.x / TILE], 1u);
        atomicOr(&buf_cword[(g.y / CELL) * cw() + g.x / CELL], 1u << prm.r_slot);
    }
}

// ---- The ghosting model (see ghost.rs, which has the same arithmetic on
// the CPU, and docs/GHOSTING.md). One word per pixel in buf_est: the
// estimated optical state in the low 16 bits (0 black .. 65535 white) and
// the 4-bit level the tablet's framebuffer holds there above.

const LV: u32 = 4369u;

fn approach(est: u32, goal: u32, k: u32) -> u32 {
    return u32(i32(est) + (i32(goal) - i32(est)) * i32(k) / 256);
}

// A 0..15 level out of a packed picture: gray4 nibbles, or bits (set =
// white, so 15), as g_kind says.
fn packed_level(is_sent: bool, x: u32, y: u32) -> u32 {
    let w = prm.w;
    if (prm.g_kind == 0u) {
        let b = y * ((w + 1u) / 2u) + x / 2u;
        var a = buf_sent[b >> 2u];
        if (!is_sent) {
            a = buf_gray4[b >> 2u];
        }
        let v = (a >> ((b & 3u) * 8u)) & 255u;
        if ((x & 1u) == 0u) {
            return v >> 4u;
        }
        return v & 15u;
    }
    let b = y * ((w + 7u) / 8u) + x / 8u;
    var a = buf_sent[b >> 2u];
    if (!is_sent) {
        a = buf_bits[b >> 2u];
    }
    let v = (a >> ((b & 3u) * 8u)) & 255u;
    return select(0u, 15u, ((v >> (7u - (x & 7u))) & 1u) != 0u);
}

// What an update that went to the tablet did to the pixels of the marked
// cells (buf_gmask): g_mode 1 fast, 2 settle, 3 reset, 4 redrive. The
// picture sent is in buf_sent.
@compute @workgroup_size(16, 16)
fn ghost_drive(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    if (buf_gmask[(g.y / CELL) * cw() + g.x / CELL] == 0u) {
        return;
    }
    let i = g.y * w + g.x;
    if (prm.g_mode == 3u) {
        buf_est[i] = 0xffffu | (15u << 16u);
        return;
    }
    var est = buf_est[i] & 0xffffu;
    let shown = buf_est[i] >> 16u;
    let tl = packed_level(true, g.x, g.y);
    let to_white = tl >= prm.g_thr;
    var paint = prm.g_k_paint;
    var erase = prm.g_k_erase;
    if (prm.g_kind == 1u) {
        paint = prm.g_e_paint;
        erase = prm.g_e_erase;
    }
    if (prm.g_mode == 2u || prm.g_mode == 5u) {
        // 5 is a clear: a settle that lands exactly.
        est = approach(est, tl * LV, select(prm.g_k_settle, 256u, prm.g_mode == 5u));
    } else if (prm.g_mode == 4u) {
        if (to_white && est + prm.g_deadband < 65535u) {
            buf_est[i] = approach(est, 65535u, erase) | (shown << 16u);
        }
        return;
    } else if (to_white != (shown >= prm.g_thr)) {
        if (to_white) {
            est = approach(est, 65535u, erase);
        } else {
            est = approach(est, 0u, paint);
        }
    }
    buf_est[i] = est | (tl << 16u);
}

// The error of the estimate against the newest frame, summed per tile and
// per 8x8 cell (units of 1/256 of a wholly wrong pixel, dead band off).
@compute @workgroup_size(16, 16)
fn ghost_err(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let est = i32(buf_est[g.y * w + g.x] & 0xffffu);
    let d = u32(abs(est - i32(packed_level(false, g.x, g.y) * LV)));
    if (d > prm.g_deadband) {
        let e = (d - prm.g_deadband) >> 8u;
        atomicAdd(&buf_gerr[(g.y / TILE) * ((w + TILE - 1u) / TILE) + g.x / TILE], e);
        atomicAdd(&buf_gcell[(g.y / CELL) * cw() + g.x / CELL], e);
    }
}

// ---- Dirty rectangles, found on the GPU. The map of changed 8x8 cells (and,
// as a second layer, of the cells whose ghosting error is at least CELL_ERR)
// is cut into connected components, each with its bounding box; a component
// whose box is mostly empty is cut into bands along its longer side, whose
// boxes are joined again while they stay well filled. The boxes go to a list
// the CPU reads with the frame. Set cells at most GAP empty cells apart are
// one component; the boxes hold only cells that are set. tiles.rs has the
// same algorithm on the CPU (`cell_boxes`), which the tests compare with.

// Must equal tiles::CELL, GAP, BOXES, SPLIT_AREA, MAX_BANDS, MERGE_FILL and
// ghost::CELL_ERR.
const CELL: u32 = 8u;
const GAP: i32 = 1;
const RECTS: u32 = 64u;
const SPLIT_AREA: u32 = 16u;
const MAX_BANDS: u32 = 16u;
const MERGE_FILL: u32 = 60u;
const CELL_ERR: u32 = 64u;
// Words of a layer's section in buf_rlist: a count, a sub-box counter, two
// spare, then RECTS boxes of four.
const RL: u32 = 260u;

fn cw() -> u32 {
    return (prm.w + 7u) / 8u;
}

fn ch() -> u32 {
    return (prm.h + 7u) / 8u;
}

fn ncells() -> u32 {
    return cw() * ch();
}

// A layer's section of buf_rc: the map (1 per set cell), the union-find
// forest, a record per cell (the component's box if it is a root) and the
// sub-boxes (as many as cells). A record: min x, min y, max x, max y, count,
// first sub-box, number of sub-boxes, band height (+ 65536 for bands of columns).
fn map_at(l: u32, c: u32) -> u32 {
    return l * ncells() * 18u + c;
}

fn lab_at(l: u32, c: u32) -> u32 {
    return l * ncells() * 18u + ncells() + c;
}

fn rec_at(l: u32, c: u32) -> u32 {
    return l * ncells() * 18u + ncells() * 2u + c * 8u;
}

fn sub_at(l: u32, s: u32) -> u32 {
    return l * ncells() * 18u + ncells() * 10u + s * 8u;
}

fn rec_clear(at: u32) {
    atomicStore(&buf_rc[at], 0xffffu);
    atomicStore(&buf_rc[at + 1u], 0xffffu);
    for (var k = 2u; k < 8u; k++) {
        atomicStore(&buf_rc[at + k], 0u);
    }
}

fn rec_add(at: u32, x: u32, y: u32) {
    atomicMin(&buf_rc[at], x);
    atomicMin(&buf_rc[at + 1u], y);
    atomicMax(&buf_rc[at + 2u], x);
    atomicMax(&buf_rc[at + 3u], y);
    atomicAdd(&buf_rc[at + 4u], 1u);
}

// A cell's history bit for this frame's slot, cleared (or, for a frame with
// nothing to compare with, set), before the diff stages write to it.
@compute @workgroup_size(16, 16)
fn rc_slot(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= cw() || g.y >= ch()) {
        return;
    }
    let c = g.y * cw() + g.x;
    let bit = 1u << prm.r_slot;
    if (prm.r_full != 0u) {
        atomicOr(&buf_cword[c], bit);
    } else {
        atomicAnd(&buf_cword[c], ~bit);
    }
}

// The map of a layer, and a fresh forest and fresh records (dispatched with
// one z per layer).
@compute @workgroup_size(16, 16)
fn rc_init(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= cw() || g.y >= ch()) {
        return;
    }
    let l = g.z;
    let c = g.y * cw() + g.x;
    var on = 0u;
    if (l == 0u) {
        if ((atomicLoad(&buf_cword[c]) & prm.r_mask) != 0u) {
            on = 1u;
        }
    } else if (atomicLoad(&buf_gcell[c]) >= CELL_ERR) {
        on = 1u;
    }
    atomicStore(&buf_rc[map_at(l, c)], on);
    atomicStore(&buf_rc[lab_at(l, c)], c);
    // Only the set cells can be roots, and sub-boxes are cleared when they are given out.
    if (on != 0u) {
        rec_clear(rec_at(l, c));
    }
    if (c == 0u) {
        atomicStore(&buf_rlist[l * RL], 0u);
        atomicStore(&buf_rlist[l * RL + 1u], 0u);
        // The last box takes whatever does not fit: the union of the rest.
        let last = l * RL + 4u + (RECTS - 1u) * 4u;
        atomicStore(&buf_rlist[last], 0xffffu);
        atomicStore(&buf_rlist[last + 1u], 0xffffu);
        atomicStore(&buf_rlist[last + 2u], 0u);
        atomicStore(&buf_rlist[last + 3u], 0u);
    }
}

fn find_c(l: u32, start: u32) -> u32 {
    var x = start;
    loop {
        let p = atomicLoad(&buf_rc[lab_at(l, x)]);
        if (p == x) {
            return x;
        }
        let gp = atomicLoad(&buf_rc[lab_at(l, p)]);
        atomicStore(&buf_rc[lab_at(l, x)], gp);
        x = gp;
    }
    return x;
}

fn join_c(l: u32, a0: u32, b0: u32) {
    var a = find_c(l, a0);
    var b = find_c(l, b0);
    loop {
        if (a == b) {
            return;
        }
        let hi = max(a, b);
        let lo = min(a, b);
        if (atomicCompareExchangeWeak(&buf_rc[lab_at(l, hi)], hi, lo).exchanged) {
            return;
        }
        a = find_c(l, hi);
        b = find_c(l, lo);
    }
}

// Each set cell joins the set cells before it (in rows) within GAP + 1 cells.
@compute @workgroup_size(16, 16)
fn rc_link(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= cw() || g.y >= ch()) {
        return;
    }
    let l = g.z;
    let cols = i32(cw());
    let x = i32(g.x);
    let y = i32(g.y);
    let c = g.y * cw() + g.x;
    if (atomicLoad(&buf_rc[map_at(l, c)]) == 0u) {
        return;
    }
    for (var dy = 0; dy <= GAP + 1; dy++) {
        for (var dx = -(GAP + 1); dx <= GAP + 1; dx++) {
            if (dy == 0 && dx >= 0) {
                continue;
            }
            let nx = x + dx;
            let ny = y - dy;
            if (nx < 0 || ny < 0 || nx >= cols) {
                continue;
            }
            let n = u32(ny * cols + nx);
            if (atomicLoad(&buf_rc[map_at(l, n)]) != 0u) {
                join_c(l, c, n);
            }
        }
    }
}

// Adds a run of set cells of one row (x, y, n cells) to the box at `at`.
fn rec_add_run(at: u32, x: u32, y: u32, n: u32) {
    atomicMin(&buf_rc[at], x);
    atomicMin(&buf_rc[at + 1u], y);
    atomicMax(&buf_rc[at + 2u], x + n - 1u);
    atomicMax(&buf_rc[at + 3u], y);
    atomicAdd(&buf_rc[at + 4u], n);
}

// Each run of set cells of a row (cells of one component, and with `band`
// of one band of the component's sub-boxes) is added to its box by one
// thread, so a big component does not make every cell wait for the same
// few words. The band is 0 for the box of the component itself.
fn run_end(l: u32, root: u32, x: u32, y: u32, bands: bool) -> u32 {
    var e = x;
    let first = band_of(l, root, x, y, bands);
    while (e + 1u < cw()) {
        let c = y * cw() + e + 1u;
        if (atomicLoad(&buf_rc[map_at(l, c)]) == 0u || find_c(l, c) != root || band_of(l, root, e + 1u, y, bands) != first) {
            break;
        }
        e++;
    }
    return e;
}

// The band a cell belongs to in the sub-boxes of its component's record (0 without sub-boxes).
fn band_of(l: u32, root: u32, x: u32, y: u32, bands: bool) -> u32 {
    if (!bands) {
        return 0u;
    }
    let r = rec_at(l, root);
    let info = atomicLoad(&buf_rc[r + 7u]);
    let band = info & 65535u;
    if (info >= 65536u) {
        return (x - atomicLoad(&buf_rc[r])) / band;
    }
    return (y - atomicLoad(&buf_rc[r + 1u])) / band;
}

// Whether the cell before this one in the row is set, of the same
// component and band: then another thread has the run it is in.
fn run_start(l: u32, root: u32, x: u32, y: u32, bands: bool) -> bool {
    if (x == 0u) {
        return true;
    }
    let p = y * cw() + x - 1u;
    return atomicLoad(&buf_rc[map_at(l, p)]) == 0u || find_c(l, p) != root || band_of(l, root, x - 1u, y, bands) != band_of(l, root, x, y, bands);
}

// Each run of set cells is added to the box of its component.
@compute @workgroup_size(16, 16)
fn rc_box(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= cw() || g.y >= ch()) {
        return;
    }
    let l = g.z;
    let c = g.y * cw() + g.x;
    if (atomicLoad(&buf_rc[map_at(l, c)]) == 0u) {
        return;
    }
    let root = find_c(l, c);
    if (run_start(l, root, g.x, g.y, false)) {
        rec_add_run(rec_at(l, root), g.x, g.y, run_end(l, root, g.x, g.y, false) - g.x + 1u);
    }
}

// A component that fills less than half of its box, and is not small, gets
// sub-boxes: bands (at most MAX_BANDS) along the longer side of the box.
@compute @workgroup_size(16, 16)
fn rc_split(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= cw() || g.y >= ch()) {
        return;
    }
    let l = g.z;
    let c = g.y * cw() + g.x;
    if (atomicLoad(&buf_rc[map_at(l, c)]) == 0u || atomicLoad(&buf_rc[lab_at(l, c)]) != c) {
        return;
    }
    let r = rec_at(l, c);
    let bw = atomicLoad(&buf_rc[r + 2u]) - atomicLoad(&buf_rc[r]) + 1u;
    let bh = atomicLoad(&buf_rc[r + 3u]) - atomicLoad(&buf_rc[r + 1u]) + 1u;
    if (atomicLoad(&buf_rc[r + 4u]) * 2u >= bw * bh || bw * bh < SPLIT_AREA) {
        return;
    }
    let by_col = bw > bh;
    let len = select(bh, bw, by_col);
    let band = (len + MAX_BANDS - 1u) / MAX_BANDS;
    let n = (len + band - 1u) / band;
    if (n < 2u) {
        return;
    }
    let first = atomicAdd(&buf_rlist[l * RL + 1u], n);
    if (first + n <= ncells()) {
        for (var k = 0u; k < n; k++) {
            rec_clear(sub_at(l, first + k));
        }
        atomicStore(&buf_rc[r + 5u], first);
        atomicStore(&buf_rc[r + 6u], n);
        atomicStore(&buf_rc[r + 7u], band + select(0u, 65536u, by_col));
    }
}

// Each run of set cells of a split component, within one band, is added to
// the sub-box of the band.
@compute @workgroup_size(16, 16)
fn rc_sub(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= cw() || g.y >= ch()) {
        return;
    }
    let l = g.z;
    let c = g.y * cw() + g.x;
    if (atomicLoad(&buf_rc[map_at(l, c)]) == 0u) {
        return;
    }
    let root = find_c(l, c);
    let r = rec_at(l, root);
    if (atomicLoad(&buf_rc[r + 6u]) == 0u) {
        return;
    }
    if (run_start(l, root, g.x, g.y, true)) {
        let k = band_of(l, root, g.x, g.y, true);
        rec_add_run(sub_at(l, atomicLoad(&buf_rc[r + 5u]) + k), g.x, g.y, run_end(l, root, g.x, g.y, true) - g.x + 1u);
    }
}

// Appends a box (cell corners, the far one exclusive) to the layer's list;
// the last place of the list is the union of the boxes that do not fit.
fn emit_box(l: u32, x0: u32, y0: u32, x1: u32, y1: u32) {
    let idx = atomicAdd(&buf_rlist[l * RL], 1u);
    let at = l * RL + 4u + min(idx, RECTS - 1u) * 4u;
    if (idx < RECTS - 1u) {
        atomicStore(&buf_rlist[at], x0);
        atomicStore(&buf_rlist[at + 1u], y0);
        atomicStore(&buf_rlist[at + 2u], x1);
        atomicStore(&buf_rlist[at + 3u], y1);
    } else {
        atomicMin(&buf_rlist[at], x0);
        atomicMin(&buf_rlist[at + 1u], y0);
        atomicMax(&buf_rlist[at + 2u], x1);
        atomicMax(&buf_rlist[at + 3u], y1);
    }
}

// One box for a component, or for a split one its bands in order, joined
// while the cells set fill at least MERGE_FILL percent of the joined box.
@compute @workgroup_size(16, 16)
fn rc_emit(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= cw() || g.y >= ch()) {
        return;
    }
    let l = g.z;
    let c = g.y * cw() + g.x;
    if (atomicLoad(&buf_rc[map_at(l, c)]) == 0u || atomicLoad(&buf_rc[lab_at(l, c)]) != c) {
        return;
    }
    let r = rec_at(l, c);
    let n = atomicLoad(&buf_rc[r + 6u]);
    if (n == 0u) {
        emit_box(l, atomicLoad(&buf_rc[r]), atomicLoad(&buf_rc[r + 1u]), atomicLoad(&buf_rc[r + 2u]) + 1u, atomicLoad(&buf_rc[r + 3u]) + 1u);
        return;
    }
    let first = atomicLoad(&buf_rc[r + 5u]);
    var have = false;
    var gx0 = 0u;
    var gy0 = 0u;
    var gx1 = 0u;
    var gy1 = 0u;
    var gn = 0u;
    for (var k = 0u; k < n; k++) {
        let s = sub_at(l, first + k);
        let cnt = atomicLoad(&buf_rc[s + 4u]);
        if (cnt == 0u) {
            continue;
        }
        let x0 = atomicLoad(&buf_rc[s]);
        let y0 = atomicLoad(&buf_rc[s + 1u]);
        let x1 = atomicLoad(&buf_rc[s + 2u]) + 1u;
        let y1 = atomicLoad(&buf_rc[s + 3u]) + 1u;
        if (have) {
            let ux0 = min(gx0, x0);
            let uy0 = min(gy0, y0);
            let ux1 = max(gx1, x1);
            let uy1 = max(gy1, y1);
            if ((gn + cnt) * 100u >= MERGE_FILL * (ux1 - ux0) * (uy1 - uy0)) {
                gx0 = ux0;
                gy0 = uy0;
                gx1 = ux1;
                gy1 = uy1;
                gn += cnt;
                continue;
            }
            emit_box(l, gx0, gy0, gx1, gy1);
        }
        gx0 = x0;
        gy0 = y0;
        gx1 = x1;
        gy1 = y1;
        gn = cnt;
        have = true;
    }
    if (have) {
        emit_box(l, gx0, gy0, gx1, gy1);
    }
}
