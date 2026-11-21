//! 增强滤镜：原图 / 增强 / 黑白 / 灰度 / 去阴影
//! 核心思路：低频背景估计（缩小→高斯模糊→双线性上采样）做光照展平，
//! 避免全分辨率大半径模糊的内存和耗时。
//! 增强管线 = 三通道白平衡 → 亮度 CLAHE → 导向滤波去噪 → USM 锐化。

use image::{imageops, GrayImage, ImageBuffer, Luma, RgbImage};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filter {
    Original,
    Enhance,
    Bw,
    Gray,
    Shadow,
}

impl Filter {
    pub fn from_name(s: &str) -> Filter {
        match s {
            "enhance" => Filter::Enhance,
            "bw" => Filter::Bw,
            "gray" => Filter::Gray,
            "shadow" => Filter::Shadow,
            _ => Filter::Original,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Filter::Original => "original",
            Filter::Enhance => "enhance",
            Filter::Bw => "bw",
            Filter::Gray => "gray",
            Filter::Shadow => "shadow",
        }
    }
}

pub fn apply_filter(img: &RgbImage, f: Filter) -> RgbImage {
    match f {
        Filter::Original => img.clone(),
        Filter::Enhance => enhance(img),
        Filter::Bw => gray_to_rgb(&binarize(img)),
        Filter::Gray => gray_to_rgb(&shadow_gray(img, false)),
        Filter::Shadow => gray_to_rgb(&shadow_gray(img, true)),
    }
}

/// 低频背景采样器：在缩小图上模糊，按需双线性采样回原坐标
struct LowFreq {
    data: Vec<f32>,
    sw: u32,
    sh: u32,
    fx: f32,
    fy: f32,
    channels: usize,
}

impl LowFreq {
    /// channels=3 时估计 RGB 背景，=1 时估计灰度背景
    fn build(img: &RgbImage, small_max: u32, sigma: f32, channels: usize) -> LowFreq {
        let (ow, oh) = (img.width(), img.height());
        let scale = (small_max as f32 / ow.max(oh) as f32).min(1.0);
        let sw = ((ow as f32 * scale).round() as u32).max(4);
        let sh = ((oh as f32 * scale).round() as u32).max(4);
        let small = crate::resize::resize_rgb(img, sw, sh);
        let mut data = vec![0.0f32; (sw * sh) as usize * channels];
        if channels == 3 {
            let mut buf: ImageBuffer<image::Rgb<f32>, Vec<f32>> = ImageBuffer::new(sw, sh);
            for (x, y, p) in small.enumerate_pixels() {
                buf.put_pixel(x, y, image::Rgb([p[0] as f32, p[1] as f32, p[2] as f32]));
            }
            let blurred = imageproc::filter::gaussian_blur_f32(&buf, sigma);
            for (i, p) in blurred.pixels().enumerate() {
                data[i * 3] = p[0];
                data[i * 3 + 1] = p[1];
                data[i * 3 + 2] = p[2];
            }
        } else {
            let g = imageops::grayscale(&small);
            let mut buf: ImageBuffer<Luma<f32>, Vec<f32>> = ImageBuffer::new(sw, sh);
            for (x, y, p) in g.enumerate_pixels() {
                buf.put_pixel(x, y, Luma([p[0] as f32]));
            }
            let blurred = imageproc::filter::gaussian_blur_f32(&buf, sigma);
            for (i, p) in blurred.pixels().enumerate() {
                data[i] = p[0];
            }
        }
        LowFreq {
            data,
            sw,
            sh,
            fx: (sw as f32 - 1.0) / (ow as f32 - 1.0).max(1.0),
            fy: (sh as f32 - 1.0) / (oh as f32 - 1.0).max(1.0),
            channels,
        }
    }

    #[inline]
    fn sample(&self, x: u32, y: u32, c: usize) -> f32 {
        let sx = x as f32 * self.fx;
        let sy = y as f32 * self.fy;
        let x0 = (sx.floor() as u32).min(self.sw - 1);
        let y0 = (sy.floor() as u32).min(self.sh - 1);
        let x1 = (x0 + 1).min(self.sw - 1);
        let y1 = (y0 + 1).min(self.sh - 1);
        let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
        let idx = |xx: u32, yy: u32| (yy as usize * self.sw as usize + xx as usize) * self.channels + c;
        let p00 = self.data[idx(x0, y0)];
        let p10 = self.data[idx(x1, y0)];
        let p01 = self.data[idx(x0, y1)];
        let p11 = self.data[idx(x1, y1)];
        let top = p00 + (p10 - p00) * fx;
        let bot = p01 + (p11 - p01) * fx;
        top + (bot - top) * fy
    }
}

/// 增强：三通道光照均衡（去色偏）→ 亮度 CLAHE → 导向滤波去噪 → USM 锐化。
/// 用 RGB 三通道低频背景分别估计各通道光照，out = in/bg·243（带限幅）：
/// 发黄老纸、偏色灯光环境自动校正白平衡，字迹/印章颜色因增益限幅得以保留。
fn enhance(img: &RgbImage) -> RgbImage {
    let bg = LowFreq::build(img, 360, 24.0, 3);
    let (w, h) = (img.width(), img.height());
    let src = img.as_raw();
    let mut out = vec![0u8; src.len()];
    out.par_chunks_mut(w as usize * 3).enumerate().for_each(|(y, row)| {
        for x in 0..w as usize {
            for c in 0..3 {
                let b = bg.sample(x as u32, y as u32, c).max(30.0);
                // 增益限幅：最多提亮 1.7 倍，避免彩色底被"漂白"或白平衡过度校正
                let gain = (243.0 / b).clamp(0.90, 1.70);
                let v = src[(y * w as usize + x) * 3 + c] as f32;
                let con = (v * gain - 128.0) * 1.05 + 128.0;
                row[x * 3 + c] = con.clamp(0.0, 255.0) as u8;
            }
        }
    });
    let flattened = RgbImage::from_raw(w, h, out).unwrap();
    let equalized = clahe_luma(&flattened);
    let denoised = guided_denoise(&equalized);
    usm_sharpen(&denoised)
}

/// 自适应 USM 锐化：out = in + k·w·(in − blur)。
/// 模糊层复用 LowFreq 低频结构（缩小→高斯→上采样），等效高斯 σ≈2px；
/// 权重 w = d²/(d²+T²) 是软门控：平坦纸面 |in−blur| 小 → 不施力，避免放大噪点；
/// 边缘处全力增强笔画对比度，且增益有限、不会产生明显光晕。
/// 明度差加到三个通道（不改色相），对文字观感和 OCR 都更友好。
fn usm_sharpen(img: &RgbImage) -> RgbImage {
    let (w, h) = (img.width(), img.height());
    let long = w.max(h) as f32;
    // 模糊层分辨率取长边 1/4：等效 σ≈2px，细节层仍来自全分辨率原图；
    // 更低分辨率只损失极低频精度，USM 观感几乎无差，但 resize+高斯开销降到 1/4
    let small_max = ((long * 0.25) as u32).clamp(64, 800);
    let scale = (small_max as f32 / long).min(1.0);
    let blur = LowFreq::build(img, small_max, (2.0 * scale).max(0.6), 1);
    const K: f32 = 0.7; // 锐化强度
    const T: f32 = 4.0; // 软门控阈值：|d|=T 时权重 0.5，|d|≫T 时趋近 1
    let src = img.as_raw();
    let mut out = vec![0u8; src.len()];
    out.par_chunks_mut(w as usize * 3).enumerate().for_each(|(y, row)| {
        for x in 0..w as usize {
            let gi = (y * w as usize + x) * 3;
            let li = x * 3;
            let (r, g, b) = (src[gi] as f32, src[gi + 1] as f32, src[gi + 2] as f32);
            let lum = 0.299 * r + 0.587 * g + 0.114 * b;
            let d = lum - blur.sample(x as u32, y as u32, 0);
            let a = d * d;
            let add = K * (a / (a + T * T)) * d;
            row[li] = (r + add).clamp(0.0, 255.0) as u8;
            row[li + 1] = (g + add).clamp(0.0, 255.0) as u8;
            row[li + 2] = (b + add).clamp(0.0, 255.0) as u8;
        }
    });
    RgbImage::from_raw(w, h, out).unwrap()
}

#[inline]
fn luma_px(r: f32, g: f32, b: f32) -> f32 {
    0.299 * r + 0.587 * g + 0.114 * b
}

/// 亮度通道 tile-based CLAHE（限制对比度自适应直方图均衡），8×8 网格。
/// 对阴影里文字的局部对比度提升远强于全局分位数拉伸；削顶限制噪声放大。
/// 通过 L'/L 缩放 RGB，色相饱和度不变；相邻 tile 的 LUT 双线性混合避免块状跳变。
fn clahe_luma(img: &RgbImage) -> RgbImage {
    const TILES: usize = 8;
    const CLIP_K: f32 = 2.8; // 直方图削顶限值 = CLIP_K × 均匀分布
    const RATIO_MIN: f32 = 0.6;
    const RATIO_MAX: f32 = 2.5;
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w < TILES || h < TILES {
        return img.clone();
    }
    let src = img.as_raw();
    let lum = |i: usize| -> f32 {
        luma_px(src[i * 3] as f32, src[i * 3 + 1] as f32, src[i * 3 + 2] as f32)
    };
    // tile 边界按 w/TILES 均分，保证每个 tile 非空
    let bound = |t: usize, n: usize| t * n / TILES;
    let mut luts = vec![[0u8; 256]; TILES * TILES];
    for ty in 0..TILES {
        for tx in 0..TILES {
            let (x0, x1) = (bound(tx, w), bound(tx + 1, w));
            let (y0, y1) = (bound(ty, h), bound(ty + 1, h));
            let mut hist = [0u32; 256];
            for y in y0..y1 {
                for x in x0..x1 {
                    let v = lum(y * w + x).round().clamp(0.0, 255.0) as usize;
                    hist[v] += 1;
                }
            }
            // 削顶，超出部分均匀摊回所有灰阶
            let count = ((x1 - x0) * (y1 - y0)) as f32;
            let clip = ((CLIP_K * count / 256.0) as u32).max(1);
            let mut excess = 0u32;
            for c in hist.iter_mut() {
                if *c > clip {
                    excess += *c - clip;
                    *c = clip;
                }
            }
            for c in hist.iter_mut() {
                *c += excess / 256;
            }
            let total: u32 = hist.iter().sum();
            let lut = &mut luts[ty * TILES + tx];
            let mut acc = 0u32;
            for (v, c) in hist.iter().enumerate() {
                acc += c;
                lut[v] = (acc as f32 / total as f32 * 255.0).round() as u8;
            }
        }
    }
    let mut out = vec![0u8; src.len()];
    out.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        let gy = (((y as f32 + 0.5) / h as f32) * TILES as f32 - 0.5)
            .clamp(0.0, (TILES - 1) as f32);
        let ty0 = gy.floor() as usize;
        let ty1 = (ty0 + 1).min(TILES - 1);
        let wy = gy - ty0 as f32;
        for x in 0..w {
            let gx = (((x as f32 + 0.5) / w as f32) * TILES as f32 - 0.5)
                .clamp(0.0, (TILES - 1) as f32);
            let tx0 = gx.floor() as usize;
            let tx1 = (tx0 + 1).min(TILES - 1);
            let wx = gx - tx0 as f32;
            let i = y * w + x;
            let lv = lum(i).round().clamp(0.0, 255.0) as usize;
            let v00 = luts[ty0 * TILES + tx0][lv] as f32;
            let v10 = luts[ty0 * TILES + tx1][lv] as f32;
            let v01 = luts[ty1 * TILES + tx0][lv] as f32;
            let v11 = luts[ty1 * TILES + tx1][lv] as f32;
            let top = v00 + (v10 - v00) * wx;
            let bot = v01 + (v11 - v01) * wx;
            let l2 = top + (bot - top) * wy;
            let ratio = (l2 / lum(i).max(1.0)).clamp(RATIO_MIN, RATIO_MAX);
            for c in 0..3 {
                row[x * 3 + c] = (src[i * 3 + c] as f32 * ratio).clamp(0.0, 255.0) as u8;
            }
        }
    });
    RgbImage::from_raw(w as u32, h as u32, out).unwrap()
}

/// 滑动窗口盒式均值：O(N) 时间、O(w) 额外内存（无积分图大内存开销），
/// 边界按实际窗口大小校正（等效边缘复制填充）
fn box_mean(src: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let mut out = vec![0f32; w * h];
    if w == 0 || h == 0 {
        return out;
    }
    let mut col = vec![0f64; w];
    for y in 0..=r.min(h - 1) {
        for (x, &v) in src[y * w..(y + 1) * w].iter().enumerate() {
            col[x] += v as f64;
        }
    }
    for y in 0..h {
        if y > 0 {
            if y + r <= h - 1 {
                for (x, &v) in src[(y + r) * w..(y + r + 1) * w].iter().enumerate() {
                    col[x] += v as f64;
                }
            }
            if y >= r + 1 {
                for (x, &v) in src[(y - r - 1) * w..(y - r) * w].iter().enumerate() {
                    col[x] -= v as f64;
                }
            }
        }
        let ylo = y.saturating_sub(r);
        let yhi = (y + r).min(h - 1);
        let ycnt = (yhi - ylo + 1) as f64;
        let mut hsum = 0f64;
        for x in 0..=r.min(w - 1) {
            hsum += col[x];
        }
        let out_row = &mut out[y * w..(y + 1) * w];
        for x in 0..w {
            if x > 0 {
                if x + r <= w - 1 {
                    hsum += col[x + r];
                }
                if x >= r + 1 {
                    hsum -= col[x - r - 1];
                }
            }
            let xlo = x.saturating_sub(r);
            let xhi = (x + r).min(w - 1);
            let cnt = ycnt * ((xhi - xlo + 1) as f64);
            out_row[x] = (hsum / cnt) as f32;
        }
    }
    out
}

/// 导向滤波去噪（自导向、边缘保持）：q = mean(a)·I + mean(b)，
/// 其中 a = var/(var+eps)——平坦区 a→0 平滑掉噪点，边缘区 a→1 保持锐利。
/// 以 q/L 缩放 RGB 去掉亮度噪声、保住色彩。低光高 ISO 照片受益最明显。
/// 快速导向滤波：a/b 在 1/2 分辨率上计算后双线性上采样（He et al.），计算量降至 1/4。
fn guided_denoise(img: &RgbImage) -> RgbImage {
    const RADIUS: usize = 4;
    const EPS: f32 = 180.0; // ≈ 平坦区噪声方差上限，越大平滑越强
    const SUB: usize = 2; // 下采样倍数
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 {
        return img.clone();
    }
    let src = img.as_raw();
    let mut lum = vec![0f32; w * h];
    for i in 0..w * h {
        lum[i] = luma_px(src[i * 3] as f32, src[i * 3 + 1] as f32, src[i * 3 + 2] as f32);
    }
    let (sw, sh) = ((w / SUB).max(1), (h / SUB).max(1));
    let small = downsample_gray(&lum, w, h, sw, sh);
    // 以下 4 趟盒式均值都跑在小图上（像素数 1/SUB²）
    let radius_small = (RADIUS / SUB).max(1);
    let mean_i = box_mean(&small, sw, sh, radius_small);
    let sq: Vec<f32> = small.iter().map(|v| v * v).collect();
    let mean_sq = box_mean(&sq, sw, sh, radius_small);
    let mut a = vec![0f32; sw * sh];
    let mut b = vec![0f32; sw * sh];
    for i in 0..sw * sh {
        let m = mean_i[i];
        let var = (mean_sq[i] - m * m).max(0.0);
        a[i] = var / (var + EPS);
        b[i] = (1.0 - a[i]) * m;
    }
    let mean_a = box_mean(&a, sw, sh, radius_small);
    let mean_b = box_mean(&b, sw, sh, radius_small);
    let a_up = upsample_bilinear(&mean_a, sw, sh, w, h);
    let b_up = upsample_bilinear(&mean_b, sw, sh, w, h);
    let mut out = vec![0u8; src.len()];
    out.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let i = y * w + x;
            let q = a_up[i] * lum[i] + b_up[i];
            let ratio = (q / lum[i].max(1.0)).clamp(0.7, 1.5);
            for c in 0..3 {
                row[x * 3 + c] = (src[i * 3 + c] as f32 * ratio).clamp(0.0, 255.0) as u8;
            }
        }
    });
    RgbImage::from_raw(w as u32, h as u32, out).unwrap()
}

/// 面积均值降采样（灰度 f32）：每个目标像素取对应源矩形均值（按行并行）
fn downsample_gray(src: &[f32], w: usize, h: usize, dw: usize, dh: usize) -> Vec<f32> {
    let mut out = vec![0f32; dw * dh];
    out.par_chunks_mut(dw).enumerate().for_each(|(dy, row)| {
        let y0 = dy * h / dh;
        let y1 = ((dy + 1) * h / dh).max(y0 + 1);
        for dx in 0..dw {
            let x0 = dx * w / dw;
            let x1 = ((dx + 1) * w / dw).max(x0 + 1);
            let mut s = 0f32;
            for y in y0..y1 {
                for x in x0..x1 {
                    s += src[y * w + x];
                }
            }
            row[dx] = s / ((y1 - y0) * (x1 - x0)) as f32;
        }
    });
    out
}

/// 双线性上采样（灰度 f32）：坐标按像素中心对齐，边缘钳制
fn upsample_bilinear(src: &[f32], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<f32> {
    let mut out = vec![0f32; dw * dh];
    out.par_chunks_mut(dw)
        .enumerate()
        .for_each(|(y, row)| {
            let gy = (((y as f32 + 0.5) / dh as f32) * sh as f32 - 0.5)
                .clamp(0.0, (sh - 1) as f32);
            let y0 = gy.floor() as usize;
            let y1 = (y0 + 1).min(sh - 1);
            let wy = gy - y0 as f32;
            for x in 0..dw {
                let gx = (((x as f32 + 0.5) / dw as f32) * sw as f32 - 0.5)
                    .clamp(0.0, (sw - 1) as f32);
                let x0 = gx.floor() as usize;
                let x1 = (x0 + 1).min(sw - 1);
                let wx = gx - x0 as f32;
                let v00 = src[y0 * sw + x0];
                let v10 = src[y0 * sw + x1];
                let v01 = src[y1 * sw + x0];
                let v11 = src[y1 * sw + x1];
                let top = v00 + (v10 - v00) * wx;
                let bot = v01 + (v11 - v01) * wx;
                row[x] = top + (bot - top) * wy;
            }
        });
    out
}

/// 黑白文档：Sauvola 局部二值化。
/// T = m·(1 + k·(σ/R − 1))，均值 m 与标准差 σ 用滑动窗口和 O(N) 求出（无积分图大内存开销）。
/// 相比单阈值，对不均匀光照 + 低对比铅笔字更鲁棒；阈值附近 SOFT 灰度过渡形成软边界，消除锯齿。
fn binarize(img: &RgbImage) -> GrayImage {
    let gray = imageops::grayscale(img);
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    let mut out = vec![0u8; w * h];
    if w == 0 || h == 0 {
        return GrayImage::from_raw(w as u32, h as u32, out).unwrap();
    }
    const K: f32 = 0.30; // 敏感度：越大越偏向保留淡墨迹
    const R: f32 = 128.0; // 标准差动态范围（8bit 灰度的一半）
    const SOFT: f32 = 14.0; // 软边界宽度（灰阶）
    const RADIUS: usize = 16; // 窗口半径，33×33 邻域
    let src = gray.as_raw();
    // 每列在垂直窗口 [y−r, y+r] 内的和与平方和，随行滑动增量维护
    let mut col_sum = vec![0f64; w];
    let mut col_sq = vec![0f64; w];
    for y in 0..=RADIUS.min(h - 1) {
        let row = &src[y * w..(y + 1) * w];
        for (x, &v) in row.iter().enumerate() {
            let v = v as f64;
            col_sum[x] += v;
            col_sq[x] += v * v;
        }
    }
    for y in 0..h {
        if y > 0 {
            if y + RADIUS <= h - 1 {
                let row = &src[(y + RADIUS) * w..(y + RADIUS + 1) * w];
                for (x, &v) in row.iter().enumerate() {
                    let v = v as f64;
                    col_sum[x] += v;
                    col_sq[x] += v * v;
                }
            }
            if y >= RADIUS + 1 {
                let row = &src[(y - RADIUS - 1) * w..(y - RADIUS) * w];
                for (x, &v) in row.iter().enumerate() {
                    let v = v as f64;
                    col_sum[x] -= v;
                    col_sq[x] -= v * v;
                }
            }
        }
        let ylo = y.saturating_sub(RADIUS);
        let yhi = (y + RADIUS).min(h - 1);
        let ycnt = (yhi - ylo + 1) as f64;
        // 水平滑动窗口：对列和再求行内窗口和
        let mut hsum = 0f64;
        let mut hsq = 0f64;
        for x in 0..=RADIUS.min(w - 1) {
            hsum += col_sum[x];
            hsq += col_sq[x];
        }
        let out_row = &mut out[y * w..(y + 1) * w];
        for x in 0..w {
            if x > 0 {
                if x + RADIUS <= w - 1 {
                    hsum += col_sum[x + RADIUS];
                    hsq += col_sq[x + RADIUS];
                }
                if x >= RADIUS + 1 {
                    hsum -= col_sum[x - RADIUS - 1];
                    hsq -= col_sq[x - RADIUS - 1];
                }
            }
            let xlo = x.saturating_sub(RADIUS);
            let xhi = (x + RADIUS).min(w - 1);
            let cnt = ycnt * ((xhi - xlo + 1) as f64);
            let m = hsum / cnt;
            let std = (hsq / cnt - m * m).max(0.0).sqrt() as f32;
            let t = (m as f32) * (1.0 + K * (std / R - 1.0));
            // 软边界：v 低于阈值渐变到黑，高于阈值渐变到白
            let alpha = ((src[y * w + x] as f32 - t) / SOFT + 0.5).clamp(0.0, 1.0);
            out_row[x] = (alpha * 255.0) as u8;
        }
    }
    GrayImage::from_raw(w as u32, h as u32, out).unwrap()
}

/// 灰度（strong=false）/ 去阴影灰度增强（strong=true）
fn shadow_gray(img: &RgbImage, strong: bool) -> GrayImage {
    let gray = imageops::grayscale(img);
    if !strong {
        return stretch(&gray, 0.01, 0.99);
    }
    let bg = LowFreq::build(img, 360, 24.0, 1);
    let (w, h) = (gray.width(), gray.height());
    let src = gray.as_raw();
    let mut out = vec![0u8; src.len()];
    out.par_chunks_mut(w as usize).enumerate().for_each(|(y, row)| {
        for x in 0..w as usize {
            let v = src[y * w as usize + x] as f32;
            let b = bg.sample(x as u32, y as u32, 0).max(12.0);
            row[x] = (v * 240.0 / b).clamp(0.0, 255.0) as u8;
        }
    });
    let flat = GrayImage::from_raw(w, h, out).unwrap();
    stretch(&flat, 0.02, 0.995)
}

/// 按分位数做对比度拉伸
fn stretch(g: &GrayImage, lo_q: f32, hi_q: f32) -> GrayImage {
    let mut hist = [0u64; 256];
    for p in g.pixels() {
        hist[p.0[0] as usize] += 1;
    }
    let total: u64 = hist.iter().sum();
    let (mut lo, mut hi) = (0usize, 255usize);
    let mut acc = 0u64;
    for i in 0..256 {
        acc += hist[i];
        if acc as f32 >= total as f32 * lo_q {
            lo = i;
            break;
        }
    }
    acc = 0;
    for i in (0..256).rev() {
        acc += hist[i];
        if acc as f32 >= total as f32 * (1.0 - hi_q) {
            hi = i;
            break;
        }
    }
    if hi <= lo + 5 {
        return g.clone();
    }
    let mut lut = [0u8; 256];
    for (i, l) in lut.iter_mut().enumerate() {
        *l = (((i as f32 - lo as f32) / (hi - lo) as f32) * 255.0).clamp(0.0, 255.0) as u8;
    }
    let mut out = g.clone();
    for p in out.pixels_mut() {
        p.0[0] = lut[p.0[0] as usize];
    }
    out
}

fn gray_to_rgb(g: &GrayImage) -> RgbImage {
    let (w, h) = (g.width(), g.height());
    let mut out = vec![0u8; (w * h * 3) as usize];
    for (i, p) in g.pixels().enumerate() {
        let v = p.0[0];
        out[i * 3] = v;
        out[i * 3 + 1] = v;
        out[i * 3 + 2] = v;
    }
    RgbImage::from_raw(w, h, out).unwrap()
}
