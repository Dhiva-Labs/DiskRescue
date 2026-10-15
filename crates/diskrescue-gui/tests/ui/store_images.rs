//! Made-up photographs for the Store screenshots of the results page: soft
//! landscapes, a night city and flowers, drawn from a few noise functions and
//! saved as real baseline JPEG files (a small encoder, so that the tests need
//! no image library). Nothing here is random: the same scene and seed always
//! give the same picture.

type Rgb = [f32; 3];

/// A kind of picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    Sunset,
    Dawn,
    Alpine,
    Meadow,
    Dunes,
    Night,
    Lake,
    Sea,
    City,
    Flower,
}

impl Scene {
    pub const ALL: [Scene; 10] = [
        Scene::Sunset,
        Scene::Meadow,
        Scene::Lake,
        Scene::Flower,
        Scene::Alpine,
        Scene::Sea,
        Scene::City,
        Scene::Dunes,
        Scene::Dawn,
        Scene::Night,
    ];
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    [
        lerp(a[0], b[0], t),
        lerp(a[1], b[1], t),
        lerp(a[2], b[2], t),
    ]
}

fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn hex(c: u32) -> Rgb {
    [
        ((c >> 16) & 255) as f32 / 255.0,
        ((c >> 8) & 255) as f32 / 255.0,
        (c & 255) as f32 / 255.0,
    ]
}

fn hash(x: i32, y: i32, seed: u32) -> f32 {
    let mut n = (x as u32).wrapping_mul(0x9E37_79B1)
        ^ (y as u32).wrapping_mul(0x85EB_CA6B)
        ^ seed.wrapping_mul(0xC2B2_AE35);
    n ^= n >> 15;
    n = n.wrapping_mul(0x2C1B_3C6D);
    n ^= n >> 12;
    n = n.wrapping_mul(0x297A_2D39);
    n ^= n >> 15;
    (n & 0xFF_FFFF) as f32 / 16_777_216.0
}

fn vnoise(x: f32, y: f32, seed: u32) -> f32 {
    let (xf, yf) = (x.floor(), y.floor());
    let (fx, fy) = (x - xf, y - yf);
    let (u, v) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let (xi, yi) = (xf as i32, yf as i32);
    let a = hash(xi, yi, seed);
    let b = hash(xi + 1, yi, seed);
    let c = hash(xi, yi + 1, seed);
    let d = hash(xi + 1, yi + 1, seed);
    lerp(lerp(a, b, u), lerp(c, d, u), v)
}

/// Fractal noise in 0..1.
fn fbm(x: f32, y: f32, seed: u32, octaves: u32) -> f32 {
    let (mut sum, mut amp, mut freq, mut norm) = (0.0, 0.5, 1.0, 0.0);
    for i in 0..octaves {
        sum += amp * vnoise(x * freq, y * freq, seed.wrapping_add(i * 7919));
        norm += amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum / norm
}

struct Layer {
    color: Rgb,
    /// Height of the skyline (0 top .. 1 bottom) where the noise is 0.
    base: f32,
    amp: f32,
    freq: f32,
    octaves: u32,
    /// How much of the haze colour the layer takes (0 near .. 1 far).
    haze: f32,
    /// Sharp peaks (mountains) instead of rolling hills.
    ridge: bool,
}

struct Land {
    top: Rgb,
    mid: Rgb,
    low: Rgb,
    /// x, y, radius, colour.
    sun: Option<(f32, f32, f32, Rgb)>,
    clouds: f32,
    cloud_tint: Rgb,
    stars: bool,
    horizon: f32,
    layers: Vec<Layer>,
    /// Water below the horizon, mirroring what is above it.
    water: Option<Rgb>,
    /// Sand along the bottom edge.
    shore: Option<Rgb>,
}

fn layer(
    color: u32,
    base: f32,
    amp: f32,
    freq: f32,
    octaves: u32,
    haze: f32,
    ridge: bool,
) -> Layer {
    Layer {
        color: hex(color),
        base,
        amp,
        freq,
        octaves,
        haze,
        ridge,
    }
}

fn land(scene: Scene, seed: u32) -> Land {
    let pick = |n: u32| (seed % n) as usize;
    match scene {
        Scene::Sunset => {
            let p = [
                (0x1b1f4a, 0x7a3b7b, 0xffa05a, 0xffd27a),
                (0x23336b, 0xc4547a, 0xffc27a, 0xffe6a8),
                (0x0f2a52, 0x4d6fa8, 0xf6b98a, 0xfff0c0),
                (0x2b1b4f, 0xb5486a, 0xff8a4c, 0xffc980),
            ][pick(4)];
            let h = 0.62;
            Land {
                top: hex(p.0),
                mid: hex(p.1),
                low: hex(p.2),
                sun: Some((
                    0.3 + 0.4 * hash(seed as i32, 3, 1),
                    h - 0.07,
                    0.05,
                    hex(p.3),
                )),
                clouds: 0.35,
                cloud_tint: mix(hex(p.2), hex(p.1), 0.35),
                stars: false,
                horizon: h,
                layers: vec![
                    layer(0x6b4a86, h - 0.03, 0.17, 2.4, 4, 0.55, true),
                    layer(0x4a3470, h + 0.05, 0.15, 3.1, 4, 0.3, true),
                    layer(0x2a2050, h + 0.15, 0.13, 3.6, 3, 0.1, false),
                    layer(0x120f26, 0.92, 0.07, 6.0, 3, 0.0, false),
                ],
                water: None,
                shore: None,
            }
        }
        Scene::Dawn => {
            let p = [
                (0x5c7fb8, 0xf0a7b8, 0xffe2c0),
                (0x7a8fc9, 0xf6b9a4, 0xfff0d0),
            ][pick(2)];
            let h = 0.6;
            Land {
                top: hex(p.0),
                mid: hex(p.1),
                low: hex(p.2),
                sun: Some((
                    0.7 - 0.3 * hash(seed as i32, 5, 2),
                    h - 0.04,
                    0.04,
                    hex(0xfff6dc),
                )),
                clouds: 0.5,
                cloud_tint: hex(0xffd9d0),
                stars: false,
                horizon: h,
                layers: vec![
                    layer(0xa9b4d8, h - 0.02, 0.12, 2.0, 4, 0.7, true),
                    layer(0x7f8dbb, h + 0.05, 0.12, 2.6, 4, 0.45, false),
                    layer(0x55658f, h + 0.13, 0.12, 3.2, 3, 0.2, false),
                    layer(0x2f3a5c, 0.9, 0.08, 5.0, 3, 0.0, false),
                ],
                water: None,
                shore: None,
            }
        }
        Scene::Alpine => {
            let h = 0.66;
            Land {
                top: hex(0x2f6fc0),
                mid: hex(0x7fb2e6),
                low: hex(0xdcecfa),
                sun: None,
                clouds: 0.55,
                cloud_tint: hex(0xffffff),
                stars: false,
                horizon: h,
                layers: vec![
                    layer(
                        0xe3ecf7,
                        0.62,
                        0.40,
                        2.2 + (seed % 3) as f32 * 0.3,
                        5,
                        0.35,
                        true,
                    ),
                    layer(0x9fb4cc, 0.72, 0.30, 3.0, 5, 0.2, true),
                    layer(0x1f4a36, 0.8, 0.12, 5.0, 4, 0.05, false),
                    layer(0x14301f, 0.94, 0.07, 7.0, 3, 0.0, false),
                ],
                water: None,
                shore: None,
            }
        }
        Scene::Meadow => {
            let h = 0.6;
            Land {
                top: hex(0x3d86d6),
                mid: hex(0x8ec5f0),
                low: hex(0xe6f3fb),
                sun: None,
                clouds: 0.62,
                cloud_tint: hex(0xffffff),
                stars: false,
                horizon: h,
                layers: vec![
                    layer(0x86b6a0, h - 0.02, 0.1, 2.0, 4, 0.6, false),
                    layer(0x6aa84f, h + 0.06, 0.13, 2.6, 4, 0.25, false),
                    layer(0x4f9142, h + 0.15, 0.13, 3.2, 3, 0.1, false),
                    layer(0x3b7a35, 0.9, 0.1, 4.0, 3, 0.0, false),
                ],
                water: None,
                shore: None,
            }
        }
        Scene::Dunes => {
            let h = 0.55;
            Land {
                top: hex(0x4f8fcf),
                mid: hex(0xf0c89a),
                low: hex(0xffe1b8),
                sun: None,
                clouds: 0.15,
                cloud_tint: hex(0xfff4e6),
                stars: false,
                horizon: h,
                layers: vec![
                    layer(0xe0b07a, h + 0.02, 0.16, 1.4, 2, 0.35, false),
                    layer(0xcf9456, h + 0.13, 0.2, 1.7, 2, 0.15, false),
                    layer(0xb87a40, h + 0.27, 0.22, 1.5, 2, 0.05, false),
                    layer(0x8e5a30, 0.97, 0.12, 2.0, 2, 0.0, false),
                ],
                water: None,
                shore: None,
            }
        }
        Scene::Night => {
            let h = 0.7;
            Land {
                top: hex(0x050a1e),
                mid: hex(0x0f1f4a),
                low: hex(0x3a4a7a),
                sun: Some((
                    0.25 + 0.5 * hash(seed as i32, 9, 3),
                    0.22,
                    0.035,
                    hex(0xf4f4e8),
                )),
                clouds: 0.1,
                cloud_tint: hex(0x39466b),
                stars: true,
                horizon: h,
                layers: vec![
                    layer(0x1d2a52, h - 0.04, 0.2, 2.0, 4, 0.4, true),
                    layer(0x131c3a, h + 0.05, 0.16, 3.0, 4, 0.15, true),
                    layer(0x070a16, 0.93, 0.08, 6.0, 3, 0.0, false),
                ],
                water: None,
                shore: None,
            }
        }
        Scene::Lake => {
            let h = 0.56;
            let p = [
                (0x1c2b5e, 0xc65a7a, 0xffb878, 0xffe0a0),
                (0x34588f, 0xe89a8a, 0xffe0b0, 0xfff3d0),
            ][pick(2)];
            Land {
                top: hex(p.0),
                mid: hex(p.1),
                low: hex(p.2),
                sun: Some((0.55, h - 0.08, 0.045, hex(p.3))),
                clouds: 0.4,
                cloud_tint: mix(hex(p.2), hex(p.1), 0.4),
                stars: false,
                horizon: h,
                layers: vec![
                    layer(0x7a6a96, h, 0.15, 2.2, 4, 0.55, true),
                    layer(0x4b3f6e, h, 0.09, 3.0, 4, 0.3, true),
                    layer(0x241d3f, h, 0.05, 4.0, 3, 0.1, false),
                ],
                water: Some(hex(0x23305a)),
                shore: None,
            }
        }
        Scene::Sea => {
            let h = 0.5;
            Land {
                top: hex(0x2c7fd0),
                mid: hex(0x7dc0ee),
                low: hex(0xe9f6fb),
                sun: Some((0.7, 0.3, 0.05, hex(0xffffff))),
                clouds: 0.45,
                cloud_tint: hex(0xffffff),
                stars: false,
                horizon: h,
                layers: vec![],
                water: Some(hex(0x0b6f9e)),
                shore: Some(hex(0xe9d6a8)),
            }
        }
        Scene::City | Scene::Flower => unreachable!("drawn by their own functions"),
    }
}

impl Land {
    fn sky(&self, u: f32, v: f32, aspect: f32, seed: u32) -> Rgb {
        let t = (v / self.horizon).clamp(0.0, 1.0);
        let mut c = if t < 0.55 {
            mix(self.top, self.mid, smooth(0.0, 0.55, t))
        } else {
            mix(self.mid, self.low, smooth(0.55, 1.0, t))
        };
        if self.clouds > 0.0 {
            let n = fbm(u * 3.2 * aspect + seed as f32, v * 7.0, seed ^ 0x5bd1, 5);
            let k = smooth(1.0 - self.clouds, 1.0 - self.clouds + 0.28, n)
                * (0.35 + 0.65 * (1.0 - t * 0.6));
            c = mix(c, self.cloud_tint, k * 0.8);
        }
        if self.stars && t < 0.85 {
            let (sx, sy) = ((u * 900.0) as i32, (v * 900.0) as i32);
            let s = hash(sx, sy, seed ^ 0x77);
            if s > 0.9975 {
                let b = (s - 0.9975) / 0.0025;
                c = mix(c, [1.0, 1.0, 0.95], 0.4 + 0.6 * b);
            }
        }
        if let Some((sx, sy, r, col)) = self.sun {
            let d = (((u - sx) * aspect).powi(2) + (v - sy).powi(2)).sqrt();
            let glow = (-d * 5.5).exp() * 0.55;
            let disc = 1.0 - smooth(r * 0.85, r, d);
            for i in 0..3 {
                c[i] = (c[i] + col[i] * glow).min(1.0);
            }
            c = mix(c, [1.0, 0.98, 0.92], disc);
        }
        c
    }
}

fn draw_land(w: usize, h: usize, seed: u32, l: &Land) -> Vec<u8> {
    let aspect = w as f32 / h as f32;
    // Skylines, once per column.
    let skylines: Vec<Vec<f32>> = l
        .layers
        .iter()
        .enumerate()
        .map(|(i, ly)| {
            (0..w)
                .map(|x| {
                    let u = x as f32 / w as f32;
                    let n = fbm(
                        u * ly.freq * aspect + i as f32 * 17.3 + seed as f32 * 0.37,
                        i as f32 * 3.1,
                        seed.wrapping_add(i as u32 * 101),
                        ly.octaves,
                    );
                    let n = if ly.ridge {
                        1.0 - (2.0 * n - 1.0).abs()
                    } else {
                        n
                    };
                    ly.base - ly.amp * n
                })
                .collect()
        })
        .collect();
    let haze = l.low;
    // The colour of the scene at (x, v), above the water line.
    let above = |x: usize, u: f32, v: f32| -> Rgb {
        let mut c = l.sky(u, v, aspect, seed);
        for (i, ly) in l.layers.iter().enumerate() {
            let y = skylines[i][x];
            if v > y && (l.water.is_none() || v <= l.horizon) {
                let depth = ((v - y) / 0.25).clamp(0.0, 1.0);
                let mut col = mix(ly.color, haze, ly.haze);
                // Lit from the horizon: edges glow, bodies darken.
                col = mix(
                    col,
                    [col[0] * 0.78, col[1] * 0.78, col[2] * 0.82],
                    smooth(0.0, 1.0, depth),
                );
                // Rock and foliage: a soft mottling, finer on the near layers.
                let tex = fbm(
                    u * (14.0 + 10.0 * i as f32) * aspect,
                    v * 22.0,
                    seed ^ (i as u32 * 31),
                    3,
                );
                let k = if ly.ridge { 0.34 } else { 0.18 };
                let f = 1.0 - k * 0.5 + k * tex;
                col = [
                    (col[0] * f).min(1.0),
                    (col[1] * f).min(1.0),
                    (col[2] * f).min(1.0),
                ];
                c = col;
            }
        }
        c
    };
    let mut out = vec![0u8; w * h * 3];
    for py in 0..h {
        let v = py as f32 / h as f32;
        for px in 0..w {
            let u = px as f32 / w as f32;
            let mut c;
            if let Some(wc) = l.water.filter(|_| v > l.horizon) {
                let depth = v - l.horizon;
                let rv = l.horizon - depth;
                let ripple =
                    (vnoise(u * 14.0, v * 120.0, seed ^ 0xabc) - 0.5) * 0.018 * (0.3 + depth * 4.0);
                let ru = (u + ripple).clamp(0.0, 0.9999);
                let rx = ((ru * w as f32) as usize).min(w - 1);
                c = above(rx, ru, rv.max(0.0));
                c = mix(c, wc, (0.3 + depth * 1.1).min(0.8));
                let streak = vnoise(u * 5.0, v * 220.0, seed ^ 0x51) - 0.5;
                for ch in &mut c {
                    *ch = (*ch * (1.0 + streak * 0.22)).clamp(0.0, 1.0);
                }
            } else {
                c = above(px, u, v);
            }
            if let Some(sand) = l.shore {
                let edge = 0.9 + 0.03 * fbm(u * 6.0, 1.0, seed ^ 0x99, 3);
                if v > edge {
                    let foam = smooth(edge, edge + 0.012, v)
                        * (1.0 - smooth(edge + 0.012, edge + 0.03, v));
                    c = mix(
                        sand,
                        [sand[0] * 0.85, sand[1] * 0.82, sand[2] * 0.75],
                        (v - edge) * 6.0,
                    );
                    c = mix(c, [1.0, 1.0, 1.0], foam * 0.7);
                }
            }
            put(&mut out, w, px, py, c, u, v, seed);
        }
    }
    out
}

fn city(w: usize, h: usize, seed: u32) -> Vec<u8> {
    let aspect = w as f32 / h as f32;
    let sky = Land {
        top: hex(0x070b25),
        mid: hex(0x2b2a6b),
        low: hex(0xd7608a),
        sun: Some((0.72, 0.28, 0.04, hex(0xfff2d8))),
        clouds: 0.3,
        cloud_tint: hex(0x7a4b8f),
        stars: true,
        horizon: 0.78,
        layers: vec![],
        water: None,
        shore: None,
    };
    // Buildings from the back to the front: x0, x1, top, window colour.
    let mut towers: Vec<(f32, f32, f32, f32)> = Vec::new();
    let mut x = -0.02;
    let mut i = 0;
    while x < 1.0 {
        let bw = 0.03 + 0.06 * hash(i, 1, seed);
        let top = 0.34 + 0.4 * hash(i, 2, seed).powf(1.4);
        let shade = 0.5 + 0.5 * hash(i, 3, seed);
        towers.push((x, x + bw, top, shade));
        x += bw * (0.55 + 0.4 * hash(i, 4, seed));
        i += 1;
    }
    let mut out = vec![0u8; w * h * 3];
    for py in 0..h {
        let v = py as f32 / h as f32;
        for px in 0..w {
            let u = px as f32 / w as f32;
            let mut c = sky.sky(u, v.min(0.78), aspect, seed);
            if v > 0.78 {
                c = hex(0x1a1030);
            }
            for (n, (x0, x1, top, shade)) in towers.iter().enumerate() {
                if u >= *x0 && u < *x1 && v >= *top {
                    let base = mix(hex(0x0e0f24), hex(0x2a2347), *shade * 0.6);
                    c = base;
                    // Lit windows on a grid.
                    let (cu, cv) = ((u - x0) * 260.0, (v - top) * 170.0);
                    let (gx, gy) = (cu.floor() as i32, cv.floor() as i32);
                    let (fx, fy) = (cu - cu.floor(), cv - cv.floor());
                    if fx > 0.25 && fx < 0.8 && fy > 0.3 && fy < 0.8 {
                        let lit = hash(gx + n as i32 * 31, gy, seed ^ 0x13);
                        if lit > 0.55 {
                            let warm = hash(gx, gy + n as i32 * 7, seed);
                            c = mix(hex(0xffd27a), hex(0xfff2c8), warm);
                            c = mix(c, base, 0.1);
                        }
                    }
                }
            }
            put(&mut out, w, px, py, c, u, v, seed);
        }
    }
    out
}

fn flower(w: usize, h: usize, seed: u32) -> Vec<u8> {
    let aspect = w as f32 / h as f32;
    let (bg1, bg2, petal, core) = [
        (0x16331f, 0x3f7a3a, 0xf4a8c8, 0xffd23a),
        (0x2a1830, 0x7a3a6a, 0xfff2f6, 0xf2a020),
        (0x10281e, 0x2f6f4f, 0xffb347, 0x6a3a10),
        (0x1b2540, 0x4a6aa8, 0xfdfdfd, 0xffd23a),
    ][(seed % 4) as usize];
    let (bg1, bg2, petal, core) = (hex(bg1), hex(bg2), hex(petal), hex(core));
    // Soft out-of-focus discs.
    let discs: Vec<(f32, f32, f32, Rgb, f32)> = (0..46)
        .map(|i| {
            let r = 0.02 + 0.08 * hash(i, 1, seed).powi(2);
            let c = mix(
                mix(bg2, petal, hash(i, 5, seed) * 0.6),
                hex(0xfff4c0),
                hash(i, 6, seed) * 0.35,
            );
            (
                hash(i, 2, seed) * aspect,
                hash(i, 3, seed),
                r,
                c,
                0.12 + 0.3 * hash(i, 4, seed),
            )
        })
        .collect();
    let (fx, fy) = (
        aspect * (0.4 + 0.2 * hash(1, 1, seed)),
        0.5 + 0.08 * hash(2, 2, seed),
    );
    let petals = 7 + (seed % 5) as i32;
    let mut out = vec![0u8; w * h * 3];
    for py in 0..h {
        let v = py as f32 / h as f32;
        for px in 0..w {
            let u = px as f32 / w as f32;
            let mut c = mix(
                bg1,
                bg2,
                smooth(0.0, 1.0, v * 0.8 + 0.2 * fbm(u * 4.0, v * 4.0, seed, 3)),
            );
            for (dx, dy, r, col, a) in &discs {
                let d = (((u * aspect - dx).powi(2) + (v - dy).powi(2)).sqrt()) / r;
                let edge = 1.0 - smooth(0.82, 1.0, d);
                let rim = smooth(0.6, 0.95, d) * edge;
                c = mix(c, *col, (edge * 0.55 + rim * 0.35) * a);
            }
            let (x, y) = (u * aspect - fx, v - fy);
            let r = (x * x + y * y).sqrt();
            let a = y.atan2(x);
            let shape = 0.2 * (0.62 + 0.38 * (a * petals as f32 / 2.0 + 0.4).cos().abs());
            if r < shape {
                let t = r / shape;
                let mut pc = mix(core, petal, smooth(0.12, 0.45, t));
                pc = mix(pc, [pc[0] * 0.8, pc[1] * 0.7, pc[2] * 0.8], t * t * 0.7);
                let vein =
                    ((a * petals as f32 * 2.0).sin() * 0.5 + 0.5) * smooth(0.2, 0.9, t) * 0.12;
                for ch in &mut pc {
                    *ch = (*ch - vein).clamp(0.0, 1.0);
                }
                c = mix(c, pc, 1.0 - smooth(0.96, 1.0, t));
            }
            put(&mut out, w, px, py, c, u, v, seed);
        }
    }
    out
}

/// Vignette, a little grain, write the pixel.
#[allow(clippy::too_many_arguments)]
fn put(out: &mut [u8], w: usize, px: usize, py: usize, c: Rgb, u: f32, v: f32, seed: u32) {
    let d = ((u - 0.5).powi(2) + (v - 0.5).powi(2)).sqrt();
    let vig = 1.0 - 0.38 * smooth(0.25, 0.8, d);
    let grain = (hash(px as i32, py as i32, seed ^ 0xfeed) - 0.5) * 0.035;
    for (i, ch) in c.iter().enumerate() {
        out[(py * w + px) * 3 + i] = ((ch * vig + grain).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    }
}

/// RGB pixels (row by row) of the picture `scene` / `seed`. `w` and `h` must be
/// multiples of 8.
pub fn photo(scene: Scene, seed: u32, w: usize, h: usize) -> Vec<u8> {
    match scene {
        Scene::City => city(w, h, seed),
        Scene::Flower => flower(w, h, seed),
        s => draw_land(w, h, seed, &land(s, seed)),
    }
}

// ------------------------------------------------------------- JPEG encoder

#[rustfmt::skip]
const ZIGZAG: [usize; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

#[rustfmt::skip]
const LUMA_Q: [u8; 64] = [
    16, 11, 10, 16,  24,  40,  51,  61,
    12, 12, 14, 19,  26,  58,  60,  55,
    14, 13, 16, 24,  40,  57,  69,  56,
    14, 17, 22, 29,  51,  87,  80,  62,
    18, 22, 37, 56,  68, 109, 103,  77,
    24, 35, 55, 64,  81, 104, 113,  92,
    49, 64, 78, 87, 103, 121, 120, 101,
    72, 92, 95, 98, 112, 100, 103,  99,
];

#[rustfmt::skip]
const CHROMA_Q: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99,
    18, 21, 26, 66, 99, 99, 99, 99,
    24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99,
];

const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
#[rustfmt::skip]
const AC_LUMA_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
    0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5,
    0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2,
    0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
#[rustfmt::skip]
const AC_CHROMA_VALUES: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xA1, 0xB1, 0xC1, 0x09, 0x23, 0x33, 0x52, 0xF0,
    0x15, 0x62, 0x72, 0xD1, 0x0A, 0x16, 0x24, 0x34, 0xE1, 0x25, 0xF1, 0x17, 0x18, 0x19, 0x1A, 0x26,
    0x27, 0x28, 0x29, 0x2A, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5,
    0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3,
    0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA,
    0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];

/// A Huffman table: code and length of each symbol.
struct Huff {
    code: [u16; 256],
    len: [u8; 256],
}

fn huff(bits: &[u8; 16], values: &[u8]) -> Huff {
    let mut h = Huff {
        code: [0; 256],
        len: [0; 256],
    };
    let (mut code, mut k) = (0u16, 0usize);
    for l in 1..=16 {
        for _ in 0..bits[l - 1] {
            h.code[values[k] as usize] = code;
            h.len[values[k] as usize] = l as u8;
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    h
}

#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    fn put(&mut self, code: u32, len: u32) {
        if len == 0 {
            return;
        }
        self.acc = (self.acc << len) | (code & ((1 << len) - 1));
        self.n += len;
        while self.n >= 8 {
            let b = ((self.acc >> (self.n - 8)) & 0xFF) as u8;
            self.out.push(b);
            if b == 0xFF {
                self.out.push(0);
            }
            self.n -= 8;
        }
        self.acc &= (1 << self.n) - 1;
    }

    fn finish(&mut self) {
        if self.n > 0 {
            let pad = 8 - self.n;
            self.put((1 << pad) - 1, pad);
        }
    }
}

/// Size category and extra bits of a coefficient.
fn category(v: i32) -> (u32, u32) {
    let size = 32 - v.unsigned_abs().leading_zeros();
    let bits = if v >= 0 { v } else { v + (1 << size) - 1 };
    (size, bits as u32)
}

fn scaled_table(base: &[u8; 64], quality: u32) -> [u8; 64] {
    let q = quality.clamp(1, 100);
    let s = if q < 50 { 5000 / q } else { 200 - 2 * q };
    let mut t = [0u8; 64];
    for i in 0..64 {
        t[i] = ((base[i] as u32 * s + 50) / 100).clamp(1, 255) as u8;
    }
    t
}

/// A baseline JPEG (4:4:4) of RGB pixels; `w` and `h` multiples of 8.
pub fn jpeg(rgb: &[u8], w: usize, h: usize, quality: u32) -> Vec<u8> {
    assert!(w % 8 == 0 && h % 8 == 0 && rgb.len() == w * h * 3);
    let qt = [
        scaled_table(&LUMA_Q, quality),
        scaled_table(&CHROMA_Q, quality),
    ];
    let dc = [
        huff(&DC_LUMA_BITS, &DC_VALUES),
        huff(&DC_CHROMA_BITS, &DC_VALUES),
    ];
    let ac = [
        huff(&AC_LUMA_BITS, &AC_LUMA_VALUES),
        huff(&AC_CHROMA_BITS, &AC_CHROMA_VALUES),
    ];

    let mut out: Vec<u8> = vec![0xFF, 0xD8];
    // JFIF header.
    out.extend([
        0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0,
    ]);
    for (id, t) in qt.iter().enumerate() {
        out.extend([0xFF, 0xDB, 0, 67, id as u8]);
        out.extend(ZIGZAG.iter().map(|&z| t[z]));
    }
    out.extend([0xFF, 0xC0, 0, 17, 8]);
    out.extend((h as u16).to_be_bytes());
    out.extend((w as u16).to_be_bytes());
    out.extend([3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1]);
    for (class, id, bits, vals) in [
        (0u8, 0u8, &DC_LUMA_BITS, &DC_VALUES[..]),
        (1, 0, &AC_LUMA_BITS, &AC_LUMA_VALUES[..]),
        (0, 1, &DC_CHROMA_BITS, &DC_VALUES[..]),
        (1, 1, &AC_CHROMA_BITS, &AC_CHROMA_VALUES[..]),
    ] {
        let len = 2 + 1 + 16 + vals.len();
        out.extend([0xFF, 0xC4]);
        out.extend((len as u16).to_be_bytes());
        out.push(class << 4 | id);
        out.extend(bits);
        out.extend(vals);
    }
    out.extend([0xFF, 0xDA, 0, 12, 3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);

    // 8-point DCT basis, scaled so that two passes give the 2-D transform.
    let mut basis = [[0f32; 8]; 8];
    for (u, row) in basis.iter_mut().enumerate() {
        let alpha = if u == 0 { (0.5f32).sqrt() } else { 1.0 } / 2.0;
        for (x, b) in row.iter_mut().enumerate() {
            *b = alpha * (((2 * x + 1) as f32) * u as f32 * std::f32::consts::PI / 16.0).cos();
        }
    }
    let mut bits = Bits::default();
    let mut prev = [0i32; 3];
    for by in 0..h / 8 {
        for bx in 0..w / 8 {
            for (comp, last_dc) in prev.iter_mut().enumerate() {
                // Level-shifted samples of this block.
                let mut f = [[0f32; 8]; 8];
                for (y, row) in f.iter_mut().enumerate() {
                    for (x, s) in row.iter_mut().enumerate() {
                        let i = ((by * 8 + y) * w + bx * 8 + x) * 3;
                        let (r, g, b) = (rgb[i] as f32, rgb[i + 1] as f32, rgb[i + 2] as f32);
                        *s = match comp {
                            0 => 0.299 * r + 0.587 * g + 0.114 * b,
                            1 => -0.168_736 * r - 0.331_264 * g + 0.5 * b + 128.0,
                            _ => 0.5 * r - 0.418_688 * g - 0.081_312 * b + 128.0,
                        } - 128.0;
                    }
                }
                let mut tmp = [[0f32; 8]; 8];
                for y in 0..8 {
                    for u in 0..8 {
                        tmp[y][u] = (0..8).map(|x| f[y][x] * basis[u][x]).sum();
                    }
                }
                let t = if comp == 0 { 0 } else { 1 };
                let mut q = [0i32; 64];
                for v in 0..8 {
                    for u in 0..8 {
                        let c: f32 = (0..8).map(|y| tmp[y][u] * basis[v][y]).sum();
                        q[v * 8 + u] = (c / qt[t][v * 8 + u] as f32).round() as i32;
                    }
                }
                // DC difference, then the run-length coded AC coefficients.
                let diff = q[0] - *last_dc;
                *last_dc = q[0];
                let (size, extra) = category(diff);
                bits.put(
                    dc[t].code[size as usize] as u32,
                    dc[t].len[size as usize] as u32,
                );
                bits.put(extra, size);
                let mut run = 0;
                for &z in &ZIGZAG[1..] {
                    let c = q[z];
                    if c == 0 {
                        run += 1;
                        continue;
                    }
                    while run > 15 {
                        bits.put(ac[t].code[0xF0] as u32, ac[t].len[0xF0] as u32);
                        run -= 16;
                    }
                    let (size, extra) = category(c);
                    let sym = (run << 4 | size) as usize;
                    bits.put(ac[t].code[sym] as u32, ac[t].len[sym] as u32);
                    bits.put(extra, size);
                    run = 0;
                }
                if run > 0 {
                    bits.put(ac[t].code[0] as u32, ac[t].len[0] as u32);
                }
            }
        }
    }
    bits.finish();
    out.extend(bits.out);
    out.extend([0xFF, 0xD9]);
    out
}
