//! 增强滤镜：原图 / 增强 / 黑白 / 灰度 / 去阴影
//! 核心思路：低频背景估计（缩小→高斯模糊→双线性上采样）做光照展平，
//! 避免全分辨率大半径模糊的内存和耗时。

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
        let small = imageops::resize(img, sw, sh, imageops::FilterType::Triangle);
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

/// 增强：基于"亮度"的光照均衡。三个通道乘同一增益，不改变色相与饱和，
/// 白纸会被提亮、阴影被找平，而证件/彩色内容保持原本色泽。
fn enhance(img: &RgbImage) -> RgbImage {
    let bg = LowFreq::build(img, 360, 24.0, 1);
    let (w, h) = (img.width(), img.height());
    let src = img.as_raw();
    let mut out = vec![0u8; src.len()];
    out.par_chunks_mut(w as usize * 3).enumerate().for_each(|(y, row)| {
        for x in 0..w as usize {
            let b = bg.sample(x as u32, y as u32, 0).max(30.0);
            // 增益限幅：暗背景最多提亮 1.8 倍，避免彩色底被"漂白"
            let gain = (243.0 / b).clamp(0.90, 1.80);
            for c in 0..3 {
                let v = src[(y * w as usize + x) * 3 + c] as f32;
                let con = (v * gain - 128.0) * 1.05 + 128.0;
                row[x * 3 + c] = con.clamp(0.0, 255.0) as u8;
            }
        }
    });
    let filtered = RgbImage::from_raw(w, h, out).unwrap();
    usm_sharpen(&filtered)
}

/// 自适应 USM 锐化：out = in + k·w·(in − blur)。
/// 模糊层复用 LowFreq 低频结构（缩小→高斯→上采样），等效高斯 σ≈2px；
/// 权重 w = d²/(d²+T²) 是软门控：平坦纸面 |in−blur| 小 → 不施力，避免放大噪点；
/// 边缘处全力增强笔画对比度，且增益有限、不会产生明显光晕。
/// 明度差加到三个通道（不改色相），对文字观感和 OCR 都更友好。
fn usm_sharpen(img: &RgbImage) -> RgbImage {
    let (w, h) = (img.width(), img.height());
    let long = w.max(h) as f32;
    // 模糊层分辨率取长边一半：等效 σ = sigma_small/scale ≈ 2px，细节层仍来自全分辨率原图
    let small_max = ((long * 0.5) as u32).clamp(64, 1600);
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
