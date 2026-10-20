// The per-pixel stages of edges.rs on the GPU: blur, Sobel, non-maximum
// suppression, hysteresis and the placement of the one-pixel line on the
// darker side. Each stage mirrors its CPU twin there, so both give the same
// outline. build.rs turns this into SPIR-V (naga); gpu.rs runs it.
//
// Every buffer holds one u32 per pixel, except the packed input and result
// (four pixels per word), so no stage needs 8-bit storage access.

struct Params {
    w: u32,
    h: u32,
}

@group(0) @binding(0) var<storage, read> src_px: array<u32>;
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
@group(0) @binding(7) var<storage, read_write> buf_result: array<u32>;
@group(0) @binding(8) var<storage, read> prm: Params;

const LOW: i32 = 96;
const HIGH: i32 = 240;

fn gray(i: u32) -> u32 {
    return (src_px[i >> 2u] >> ((i & 3u) * 8u)) & 255u;
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

// Four outline pixels to one word, for the read-back.
@compute @workgroup_size(16, 16)
fn pack_out(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = prm.w;
    if (g.x >= w || g.y >= prm.h) {
        return;
    }
    let i = g.y * w + g.x;
    if ((i & 3u) != 0u) {
        return;
    }
    let n = w * prm.h;
    var word = 0u;
    for (var j = 0u; j < 4u; j++) {
        if (i + j < n) {
            word |= (buf_out[i + j] & 255u) << (8u * j);
        }
    }
    buf_result[i >> 2u] = word;
}
