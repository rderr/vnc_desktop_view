//! CPU color conversion for encoders that take frames in system memory.

/// BGRA -> NV12 (Y plane, then interleaved U/V at half resolution), BT.601
/// limited range, on the CPU: for encoders that take NV12 in system memory
/// (Windows without GPU conversion, VA-API). Rows are split across threads;
/// ~2 ms for 1440x900.
pub struct Converter {
    w: usize,
    h: usize,
}

impl Converter {
    pub fn new(width: u32, height: u32) -> Self {
        Self { w: width as usize, h: height as usize }
    }

    /// `bgra` is tightly packed: `width * 4` bytes per row.
    #[allow(dead_code)] // used on Windows
    pub fn nv12(&mut self, bgra: &[u8], out: &mut Vec<u8>) {
        self.nv12_strided(bgra, self.w * 4, out);
    }

    /// `bgra` has `stride` bytes per row (at least `width * 4`).
    pub fn nv12_strided(&mut self, bgra: &[u8], stride: usize, out: &mut Vec<u8>) {
        let (w, h) = (self.w, self.h);
        out.resize(w * h * 3 / 2, 0);
        let (y_plane, uv_plane) = out.split_at_mut(w * h);
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
        // Work in bands of row pairs, so each band owns whole chroma rows.
        let pairs = h / 2;
        let pairs_per_band = pairs.div_ceil(threads).max(1);
        std::thread::scope(|s| {
            let y_bands = y_plane.chunks_mut(pairs_per_band * 2 * w);
            let uv_bands = uv_plane.chunks_mut(pairs_per_band * w);
            for (band, (y_out, uv_out)) in y_bands.zip(uv_bands).enumerate() {
                let first_row = band * pairs_per_band * 2;
                s.spawn(move || convert_band(bgra, stride, w, first_row, y_out, uv_out));
            }
        });
    }
}

fn convert_band(bgra: &[u8], stride: usize, w: usize, first_row: usize, y_out: &mut [u8], uv_out: &mut [u8]) {
    let luma = |b: i32, g: i32, r: i32| (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
    for pair in 0..uv_out.len() / w {
        let row0 = first_row + pair * 2;
        let (src0, src1) = (&bgra[row0 * stride..][..w * 4], &bgra[(row0 + 1) * stride..][..w * 4]);
        let (dst0, dst1) = y_out[pair * 2 * w..(pair * 2 + 2) * w].split_at_mut(w);
        let uv = &mut uv_out[pair * w..(pair + 1) * w];
        for x in (0..w).step_by(2) {
            let px = |row: &[u8], i: usize| (row[i * 4] as i32, row[i * 4 + 1] as i32, row[i * 4 + 2] as i32);
            let p = [px(src0, x), px(src0, x + 1), px(src1, x), px(src1, x + 1)];
            dst0[x] = luma(p[0].0, p[0].1, p[0].2);
            dst0[x + 1] = luma(p[1].0, p[1].1, p[1].2);
            dst1[x] = luma(p[2].0, p[2].1, p[2].2);
            dst1[x + 1] = luma(p[3].0, p[3].1, p[3].2);
            let (b, g, r) = p.iter().fold((0, 0, 0), |a, c| (a.0 + c.0, a.1 + c.1, a.2 + c.2));
            let (b, g, r) = (b / 4, g / 4, r / 4);
            uv[x] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            uv[x + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}
