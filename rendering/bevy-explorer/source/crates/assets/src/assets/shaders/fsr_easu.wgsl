// FidelityFX Super Resolution Sample
//
// Copyright (c) 2021 Advanced Micro Devices, Inc. All rights reserved.
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files(the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and / or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions :
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

// The compositor's material (visuals/src/upscale.rs): draws the scene target onto the window.
// Its EASU is a port of AMD FidelityFX FSR 1.0 EASU, the non-packed 32-bit version (FSR_EASU_F:
// FsrEasuCon, FsrEasuTapF, FsrEasuSetF, FsrEasuF) of ffx_fsr1.h. The port keeps the constants as
// floats rather than their bits in uints, and turns `inout` parameters into pointers. EASU wants
// its input in gamma 2.0 and the sRGB scene target reads back linear, so the input callback
// takes the square root and the result is squared. RCAS is not ported: bevy's contrast adaptive
// sharpening runs after this pass. The compositor only runs below scale 1, so there is no copy
// path.

#import bevy_sprite::mesh2d_vertex_output::VertexOutput

@group(2) @binding(0) var source: texture_2d<f32>;
@group(2) @binding(1) var source_sampler: sampler;
// xy: the scene target's size, zw: the window's, in physical pixels
@group(2) @binding(2) var<uniform> params: vec4<f32>;

// ffx_a.h's approximations, which EASU's weights are tuned with
fn APrxLoRcpF1(a: f32) -> f32 {
    return bitcast<f32>(0x7ef07ebbu - bitcast<u32>(a));
}

fn APrxLoRsqF1(a: f32) -> f32 {
    return bitcast<f32>(0x5f347d74u - (bitcast<u32>(a) >> 1u));
}

struct EasuCon {
    con0: vec4<f32>,
    con1: vec4<f32>,
    con2: vec4<f32>,
    con3: vec4<f32>,
}

fn FsrEasuCon(
    // This the rendered image resolution being upscaled
    inputViewportInPixels: vec2<f32>,
    // This is the resolution of the resource containing the input image (useful for dynamic resolution)
    inputSizeInPixels: vec2<f32>,
    // This is the display resolution which the input image gets upscaled to
    outputSizeInPixels: vec2<f32>,
) -> EasuCon {
    var con: EasuCon;
    // Output integer position to a pixel position in viewport.
    con.con0 = vec4(
        inputViewportInPixels * (1.0 / outputSizeInPixels),
        0.5 * inputViewportInPixels * (1.0 / outputSizeInPixels) - 0.5,
    );
    // Viewport pixel position to normalized image space.
    // This is used to get upper-left of 'F' tap.
    let rcp = 1.0 / inputSizeInPixels;
    // Centers of gather4, first offset from upper-left of 'F'.
    //      +---+---+
    //      |   |   |
    //      +--(0)--+
    //      | b | c |
    //  +---F---+---+---+
    //  | e | f | g | h |
    //  +--(1)--+--(2)--+
    //  | i | j | k | l |
    //  +---+---+---+---+
    //      | n | o |
    //      +--(3)--+
    //      |   |   |
    //      +---+---+
    con.con1 = vec4(rcp, 1.0 * rcp.x, -1.0 * rcp.y);
    // These are from (0) instead of 'F'.
    con.con2 = vec4(-1.0 * rcp.x, 2.0 * rcp.y, 1.0 * rcp.x, 2.0 * rcp.y);
    con.con3 = vec4(0.0 * rcp.x, 4.0 * rcp.y, 0.0, 0.0);
    return con;
}

// Input callbacks: gather4 of one channel at 'p'.
fn FsrEasuRF(p: vec2<f32>) -> vec4<f32> {
    return textureGather(0, source, source_sampler, p);
}

fn FsrEasuGF(p: vec2<f32>) -> vec4<f32> {
    return textureGather(1, source, source_sampler, p);
}

fn FsrEasuBF(p: vec2<f32>) -> vec4<f32> {
    return textureGather(2, source, source_sampler, p);
}

// The linear scene target to the gamma 2.0 EASU expects.
fn FsrEasuProcessInput(
    r: ptr<function, vec4<f32>>,
    g: ptr<function, vec4<f32>>,
    b: ptr<function, vec4<f32>>,
) {
    *r = sqrt(*r);
    *g = sqrt(*g);
    *b = sqrt(*b);
}

// Filtering for a given tap for the scalar.
fn FsrEasuTapF(
    aC: ptr<function, vec3<f32>>, // Accumulated color, with negative lobe.
    aW: ptr<function, f32>, // Accumulated weight.
    off: vec2<f32>, // Pixel offset from resolve position to tap.
    dir: vec2<f32>, // Gradient direction.
    len: vec2<f32>, // Length.
    lob: f32, // Negative lobe strength.
    clp: f32, // Clipping point.
    c: vec3<f32>, // Tap color.
) {
    // Rotate offset by direction.
    var v: vec2<f32>;
    v.x = (off.x * (dir.x)) + (off.y * dir.y);
    v.y = (off.x * (-dir.y)) + (off.y * dir.x);
    // Anisotropy.
    v *= len;
    // Compute distance^2.
    var d2 = v.x * v.x + v.y * v.y;
    // Limit to the window as at corner, 2 taps can easily be outside.
    d2 = min(d2, clp);
    // Approximation of lancos2 without sin() or rcp(), or sqrt() to get x.
    //  (25/16 * (2/5 * x^2 - 1)^2 - (25/16 - 1)) * (1/4 * x^2 - 1)^2
    //  |_______________________________________|   |_______________|
    //                   base                             window
    // The general form of the 'base' is,
    //  (a*(b*x^2-1)^2-(a-1))
    // Where 'a=1/(2*b-b^2)' and 'b' moves around the negative lobe.
    var wB = (2.0 / 5.0) * d2 + (-1.0);
    var wA = lob * d2 + (-1.0);
    wB *= wB;
    wA *= wA;
    wB = (25.0 / 16.0) * wB + (-(25.0 / 16.0 - 1.0));
    let w = wB * wA;
    // Do weighted average.
    *aC += c * w;
    *aW += w;
}

// Accumulate direction and length.
fn FsrEasuSetF(
    dir: ptr<function, vec2<f32>>,
    len: ptr<function, f32>,
    pp: vec2<f32>,
    biS: bool,
    biT: bool,
    biU: bool,
    biV: bool,
    lA: f32,
    lB: f32,
    lC: f32,
    lD: f32,
    lE: f32,
) {
    // Compute bilinear weight, branches factor out as predicates are compiler time immediates.
    //  s t
    //  u v
    var w = 0.0;
    if biS { w = (1.0 - pp.x) * (1.0 - pp.y); }
    if biT { w = pp.x * (1.0 - pp.y); }
    if biU { w = (1.0 - pp.x) * pp.y; }
    if biV { w = pp.x * pp.y; }
    // Direction is the '+' diff.
    //    a
    //  b c d
    //    e
    // Then takes magnitude from abs average of both sides of 'c'.
    // Length converts gradient reversal to 0, smoothly to non-reversal at 1, shaped, then adding horz and vert terms.
    let dc = lD - lC;
    let cb = lC - lB;
    var lenX = max(abs(dc), abs(cb));
    lenX = APrxLoRcpF1(lenX);
    let dirX = lD - lB;
    (*dir).x += dirX * w;
    lenX = saturate(abs(dirX) * lenX);
    lenX *= lenX;
    *len += lenX * w;
    // Repeat for the y axis.
    let ec = lE - lC;
    let ca = lC - lA;
    var lenY = max(abs(ec), abs(ca));
    lenY = APrxLoRcpF1(lenY);
    let dirY = lE - lA;
    (*dir).y += dirY * w;
    lenY = saturate(abs(dirY) * lenY);
    lenY *= lenY;
    *len += lenY * w;
}

fn FsrEasuF(
    ip: vec2<u32>, // Integer pixel position in output.
    con: EasuCon, // Constants generated by FsrEasuCon().
) -> vec3<f32> {
    // Get position of 'f'.
    var pp = vec2<f32>(ip) * con.con0.xy + con.con0.zw;
    let fp = floor(pp);
    pp -= fp;
    // 12-tap kernel.
    //    b c
    //  e f g h
    //  i j k l
    //    n o
    // Gather 4 ordering.
    //  a b
    //  r g
    // For packed FP16, need either {rg} or {ab} so using the following setup for gather in all versions,
    //    a b    <- unused (z)
    //    r g
    //  a b a b
    //  r g r g
    //    a b
    //    r g    <- unused (z)
    // Allowing dead-code removal to remove the 'z's.
    let p0 = fp * con.con1.xy + con.con1.zw;
    // These are from p0 to avoid pulling two constants on pre-Navi hardware.
    let p1 = p0 + con.con2.xy;
    let p2 = p0 + con.con2.zw;
    let p3 = p0 + con.con3.xy;
    var bczzR = FsrEasuRF(p0);
    var bczzG = FsrEasuGF(p0);
    var bczzB = FsrEasuBF(p0);
    FsrEasuProcessInput(&bczzR, &bczzG, &bczzB);
    var ijfeR = FsrEasuRF(p1);
    var ijfeG = FsrEasuGF(p1);
    var ijfeB = FsrEasuBF(p1);
    FsrEasuProcessInput(&ijfeR, &ijfeG, &ijfeB);
    var klhgR = FsrEasuRF(p2);
    var klhgG = FsrEasuGF(p2);
    var klhgB = FsrEasuBF(p2);
    FsrEasuProcessInput(&klhgR, &klhgG, &klhgB);
    var zzonR = FsrEasuRF(p3);
    var zzonG = FsrEasuGF(p3);
    var zzonB = FsrEasuBF(p3);
    FsrEasuProcessInput(&zzonR, &zzonG, &zzonB);
    // Simplest multi-channel approximate luma possible (luma times 2, in 2 FMA/MAD).
    let bczzL = bczzB * 0.5 + (bczzR * 0.5 + bczzG);
    let ijfeL = ijfeB * 0.5 + (ijfeR * 0.5 + ijfeG);
    let klhgL = klhgB * 0.5 + (klhgR * 0.5 + klhgG);
    let zzonL = zzonB * 0.5 + (zzonR * 0.5 + zzonG);
    // Rename.
    let bL = bczzL.x;
    let cL = bczzL.y;
    let iL = ijfeL.x;
    let jL = ijfeL.y;
    let fL = ijfeL.z;
    let eL = ijfeL.w;
    let kL = klhgL.x;
    let lL = klhgL.y;
    let hL = klhgL.z;
    let gL = klhgL.w;
    let oL = zzonL.z;
    let nL = zzonL.w;
    // Accumulate for bilinear interpolation.
    var dir = vec2(0.0);
    var len = 0.0;
    FsrEasuSetF(&dir, &len, pp, true, false, false, false, bL, eL, fL, gL, jL);
    FsrEasuSetF(&dir, &len, pp, false, true, false, false, cL, fL, gL, hL, kL);
    FsrEasuSetF(&dir, &len, pp, false, false, true, false, fL, iL, jL, kL, nL);
    FsrEasuSetF(&dir, &len, pp, false, false, false, true, gL, jL, kL, lL, oL);
    // Normalize with approximation, and cleanup close to zero.
    let dir2 = dir * dir;
    var dirR = dir2.x + dir2.y;
    let zro = dirR < (1.0 / 32768.0);
    dirR = APrxLoRsqF1(dirR);
    dirR = select(dirR, 1.0, zro);
    dir.x = select(dir.x, 1.0, zro);
    dir *= vec2(dirR);
    // Transform from {0 to 2} to {0 to 1} range, and shape with square.
    len = len * 0.5;
    len *= len;
    // Stretch kernel {1.0 vert|horz, to sqrt(2.0) on diagonal}.
    let stretch = (dir.x * dir.x + dir.y * dir.y) * APrxLoRcpF1(max(abs(dir.x), abs(dir.y)));
    // Anisotropic length after rotation,
    //  x := 1.0 lerp to 'stretch' on edges
    //  y := 1.0 lerp to 2x on edges
    let len2 = vec2(1.0 + (stretch - 1.0) * len, 1.0 + (-0.5) * len);
    // Based on the amount of 'edge',
    // the window shifts from +/-{sqrt(2.0) to slightly beyond 2.0}.
    let lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    // Set distance^2 clipping point to the end of the adjustable window.
    let clp = APrxLoRcpF1(lob);
    // Accumulation mixed with min/max of 4 nearest.
    //    b c
    //  e f g h
    //  i j k l
    //    n o
    let min4 = min(
        min(vec3(ijfeR.z, ijfeG.z, ijfeB.z), min(vec3(klhgR.w, klhgG.w, klhgB.w), vec3(ijfeR.y, ijfeG.y, ijfeB.y))),
        vec3(klhgR.x, klhgG.x, klhgB.x),
    );
    let max4 = max(
        max(vec3(ijfeR.z, ijfeG.z, ijfeB.z), max(vec3(klhgR.w, klhgG.w, klhgB.w), vec3(ijfeR.y, ijfeG.y, ijfeB.y))),
        vec3(klhgR.x, klhgG.x, klhgB.x),
    );
    // Accumulation.
    var aC = vec3(0.0);
    var aW = 0.0;
    FsrEasuTapF(&aC, &aW, vec2(0.0, -1.0) - pp, dir, len2, lob, clp, vec3(bczzR.x, bczzG.x, bczzB.x)); // b
    FsrEasuTapF(&aC, &aW, vec2(1.0, -1.0) - pp, dir, len2, lob, clp, vec3(bczzR.y, bczzG.y, bczzB.y)); // c
    FsrEasuTapF(&aC, &aW, vec2(-1.0, 1.0) - pp, dir, len2, lob, clp, vec3(ijfeR.x, ijfeG.x, ijfeB.x)); // i
    FsrEasuTapF(&aC, &aW, vec2(0.0, 1.0) - pp, dir, len2, lob, clp, vec3(ijfeR.y, ijfeG.y, ijfeB.y)); // j
    FsrEasuTapF(&aC, &aW, vec2(0.0, 0.0) - pp, dir, len2, lob, clp, vec3(ijfeR.z, ijfeG.z, ijfeB.z)); // f
    FsrEasuTapF(&aC, &aW, vec2(-1.0, 0.0) - pp, dir, len2, lob, clp, vec3(ijfeR.w, ijfeG.w, ijfeB.w)); // e
    FsrEasuTapF(&aC, &aW, vec2(1.0, 1.0) - pp, dir, len2, lob, clp, vec3(klhgR.x, klhgG.x, klhgB.x)); // k
    FsrEasuTapF(&aC, &aW, vec2(2.0, 1.0) - pp, dir, len2, lob, clp, vec3(klhgR.y, klhgG.y, klhgB.y)); // l
    FsrEasuTapF(&aC, &aW, vec2(2.0, 0.0) - pp, dir, len2, lob, clp, vec3(klhgR.z, klhgG.z, klhgB.z)); // h
    FsrEasuTapF(&aC, &aW, vec2(1.0, 0.0) - pp, dir, len2, lob, clp, vec3(klhgR.w, klhgG.w, klhgB.w)); // g
    FsrEasuTapF(&aC, &aW, vec2(1.0, 2.0) - pp, dir, len2, lob, clp, vec3(zzonR.z, zzonG.z, zzonB.z)); // o
    FsrEasuTapF(&aC, &aW, vec2(0.0, 2.0) - pp, dir, len2, lob, clp, vec3(zzonR.w, zzonG.w, zzonB.w)); // n
    // Normalize and dering.
    return min(max4, max(min4, aC * vec3(1.0 / aW)));
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let con = FsrEasuCon(params.xy, params.xy, params.zw);
    let pix = FsrEasuF(vec2<u32>(in.position.xy), con);
    // back from gamma 2.0; the alpha is the nearest texel's
    let alpha = textureLoad(source, vec2<i32>(in.position.xy * params.xy / params.zw), 0).a;
    return vec4(pix * pix, alpha);
}
