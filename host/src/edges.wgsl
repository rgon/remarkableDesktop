// The whole picture path on the GPU: the raw capture to the panel's gray
// frame (scale, letterbox, rotate), then for the edge view blur, Sobel,
// non-maximum suppression, hysteresis, the placement of the one-pixel line on
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
// Set by a hysteresis pass that promoted a pixel.
@group(0) @binding(6) var<storage, read_write> buf_changed: atomic<u32>;
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
    return 0.299 * f32((p >> 16u) & 255u) + 0.587 * f32((p >> 8u) & 255u) + 0.114 * f32(p & 255u);
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

// One round of hysteresis: a weak pixel next to an edge becomes one. Run
// until a round changes nothing.
@compute @workgroup_size(16, 16)
fn hyst(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let i = g.y * w + g.x;
    if (buf_state[i] != 1u) {
        return;
    }
    let wi = i32(w);
    let hi = i32(prm.h);
    let x = i32(g.x);
    let y = i32(g.y);
    for (var dy = -1; dy <= 1; dy++) {
        for (var dx = -1; dx <= 1; dx++) {
            let nx = x + dx;
            let ny = y + dy;
            if ((dx != 0 || dy != 0) && nx >= 0 && ny >= 0 && nx < wi && ny < hi && buf_state[u32(ny) * w + u32(nx)] == 2u) {
                buf_state[i] = 2u;
                atomicStore(&buf_changed, 1u);
                return;
            }
        }
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
    }
}
