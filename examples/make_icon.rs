//! Regenerates assets/icon.png (1024×1024): `cargo run --example make_icon`.
//! Coral rounded square, white play triangle — drawn with 4×4 supersampling.

fn main() {
    const N: u32 = 1024;
    const SS: u32 = 4;
    let body = 824.0_f32; // macOS icon grid: 824px body on a 1024 canvas
    let off = (N as f32 - body) / 2.0;
    let radius = 185.0_f32;
    let top = [0xFF, 0x7A, 0x5C];
    let bottom = [0xE8, 0x3A, 0x4E];

    // play triangle (slightly right of centre so it looks optically centred)
    let (cx, cy, s) = (N as f32 / 2.0 + 28.0, N as f32 / 2.0, 330.0_f32);
    let tri = [(cx - s * 0.5, cy - s * 0.58), (cx - s * 0.5, cy + s * 0.58), (cx + s * 0.5, cy)];

    let in_body = |x: f32, y: f32| {
        let (lx, ly) = (x - off, y - off);
        if lx < 0.0 || ly < 0.0 || lx > body || ly > body {
            return false;
        }
        let qx = (lx - lx.clamp(radius, body - radius)).abs();
        let qy = (ly - ly.clamp(radius, body - radius)).abs();
        qx * qx + qy * qy <= radius * radius
    };
    let in_tri = |x: f32, y: f32| {
        let e = |a: (f32, f32), b: (f32, f32)| (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0);
        let (d1, d2, d3) = (e(tri[0], tri[1]), e(tri[1], tri[2]), e(tri[2], tri[0]));
        (d1 >= 0.0 && d2 >= 0.0 && d3 >= 0.0) || (d1 <= 0.0 && d2 <= 0.0 && d3 <= 0.0)
    };

    let img = image::RgbaImage::from_fn(N, N, |px, py| {
        let (mut cov_body, mut cov_tri) = (0.0, 0.0);
        for sy in 0..SS {
            for sx in 0..SS {
                let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                if in_body(x, y) {
                    cov_body += 1.0;
                    if in_tri(x, y) {
                        cov_tri += 1.0;
                    }
                }
            }
        }
        let n = (SS * SS) as f32;
        let (a, w) = (cov_body / n, if cov_body > 0.0 { cov_tri / cov_body } else { 0.0 });
        let t = ((py as f32 - off) / body).clamp(0.0, 1.0);
        let c = |i: usize| {
            let g = top[i] as f32 + (bottom[i] as f32 - top[i] as f32) * t;
            (g + (255.0 - g) * w).round() as u8
        };
        image::Rgba([c(0), c(1), c(2), (a * 255.0).round() as u8])
    });
    img.save("assets/icon.png").expect("writing assets/icon.png");
    println!("wrote assets/icon.png");
}
