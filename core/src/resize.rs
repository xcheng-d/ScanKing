//! 快速缩放：分离式三角形核 + rayon 并行，端侧热路径的统一缩放入口。
//! 质量与 imageops::resize(Triangle) 一致，避免其单线程逐像素 f32 采样的开销。

use image::{GrayImage, RgbImage};
use rayon::prelude::*;

struct ResampleTaps {
    /// 采样下标（构建时已钳制到图内，热循环里无需再判界）
    idxs: Vec<usize>,
    weights: Vec<f32>,
}

/// 为一维方向生成三角形核重采样权重表（降采样时支持域按 1/scale 放大）
fn build_taps(src_n: usize, dst_n: usize) -> Vec<ResampleTaps> {
    let scale = dst_n as f32 / src_n as f32;
    let support = if scale < 1.0 { 1.0 / scale } else { 1.0 };
    let last = src_n as i64 - 1;
    (0..dst_n)
        .map(|i| {
            let center = (i as f32 + 0.5) / scale - 0.5;
            let lo = (center - support).ceil() as i64;
            let hi = (center + support).floor() as i64;
            let n = (hi - lo + 1).max(1) as usize;
            let mut idxs = Vec::with_capacity(n);
            let mut weights = Vec::with_capacity(n);
            let mut sum = 0.0f32;
            for s in lo..=hi {
                let w = (1.0 - (s as f32 - center).abs() / support).max(0.0);
                idxs.push(s.clamp(0, last) as usize);
                weights.push(w);
                sum += w;
            }
            for w in weights.iter_mut() {
                *w /= sum;
            }
            ResampleTaps { idxs, weights }
        })
        .collect()
}

/// 两趟分离式重采样，ch 为通道数（1 或 3）
fn resample(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, ch: usize) -> Vec<u8> {
    let taps_x = build_taps(sw, dw);
    let taps_y = build_taps(sh, dh);
    // 趟一：水平 sw×sh → dw×sh（行内累加，抽头下标预计算）
    let mut mid = vec![0u8; dw * sh * ch];
    mid.par_chunks_mut(dw * ch).enumerate().for_each(|(y, row)| {
        let row_base = y * sw * ch;
        for (i, t) in taps_x.iter().enumerate() {
            let mut acc = [0f32; 4];
            for (&xx, &w) in t.idxs.iter().zip(t.weights.iter()) {
                let s = row_base + xx * ch;
                for c in 0..ch {
                    acc[c] += w * src[s + c] as f32;
                }
            }
            for c in 0..ch {
                row[i * ch + c] = acc[c].round().clamp(0.0, 255.0) as u8;
            }
        }
    });
    // 趟二：垂直 dw×sh → dw×dh。整行累加（源行连续、可向量化），
    // 复用每线程的 f32 行缓冲，避免逐像素重复判界
    let mut out = vec![0u8; dw * dh * ch];
    let row_len = dw * ch;
    out.par_chunks_mut(row_len)
        .enumerate()
        .for_each_init(
            || vec![0f32; row_len],
            |acc: &mut Vec<f32>, (y, row)| {
                acc.fill(0.0);
                let t = &taps_y[y];
                for (&yy, &w) in t.idxs.iter().zip(t.weights.iter()) {
                    let base = yy * row_len;
                    let srow = &mid[base..base + row_len];
                    for (a, &v) in acc.iter_mut().zip(srow.iter()) {
                        *a += w * v as f32;
                    }
                }
                for (o, &a) in row.iter_mut().zip(acc.iter()) {
                    *o = a.round().clamp(0.0, 255.0) as u8;
                }
            },
        );
    out
}

/// 大幅降采样（比例 ≥3）时直接面积均值：每个源像素只读一次，
/// 比带核重采样（抽头数随比例增长）快数倍，质量上是三角形核的良好近似
fn box_resample(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, ch: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dh * ch];
    out.par_chunks_mut(dw * ch).enumerate().for_each(|(dy, row)| {
        let y0 = dy * sh / dh;
        let y1 = ((dy + 1) * sh / dh).max(y0 + 1);
        for dx in 0..dw {
            let x0 = dx * sw / dw;
            let x1 = ((dx + 1) * sw / dw).max(x0 + 1);
            let mut acc = [0u32; 4];
            for y in y0..y1 {
                let base = y * sw;
                for x in x0..x1 {
                    let s = (base + x) * ch;
                    for (c, a) in acc.iter_mut().enumerate().take(ch) {
                        *a += src[s + c] as u32;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            for (c, a) in acc.iter().enumerate().take(ch) {
                row[dx * ch + c] = ((*a + n / 2) / n) as u8;
            }
        }
    });
    out
}

#[inline]
fn use_box(sw: usize, sh: usize, dw: usize, dh: usize) -> bool {
    sw >= dw * 3 && sh >= dh * 3
}

/// RGB 快速缩放（仅降采样更高效；放大也正确但此处不用于放大）
pub(crate) fn resize_rgb(src: &RgbImage, dw: u32, dh: u32) -> RgbImage {
    let (sw, sh) = (src.width() as usize, src.height() as usize);
    let (dw, dh) = (dw.max(1) as usize, dh.max(1) as usize);
    if sw == 0 || sh == 0 || (sw <= dw && sh <= dh) {
        return src.clone();
    }
    let out = if use_box(sw, sh, dw, dh) {
        box_resample(src.as_raw(), sw, sh, dw, dh, 3)
    } else {
        resample(src.as_raw(), sw, sh, dw, dh, 3)
    };
    RgbImage::from_raw(dw as u32, dh as u32, out).unwrap()
}

/// 灰度快速缩放
pub(crate) fn resize_gray(src: &GrayImage, dw: u32, dh: u32) -> GrayImage {
    let (sw, sh) = (src.width() as usize, src.height() as usize);
    let (dw, dh) = (dw.max(1) as usize, dh.max(1) as usize);
    if sw == 0 || sh == 0 || (sw <= dw && sh <= dh) {
        return src.clone();
    }
    let out = if use_box(sw, sh, dw, dh) {
        box_resample(src.as_raw(), sw, sh, dw, dh, 1)
    } else {
        resample(src.as_raw(), sw, sh, dw, dh, 1)
    };
    GrayImage::from_raw(dw as u32, dh as u32, out).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern_img(w: u32, h: u32) -> RgbImage {
        let mut img = RgbImage::new(w, h);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let smooth = ((x * 255 / w.max(1)) + (y * 255 / h.max(1))) / 2;
            let fine = if (x / 3 + y / 3) % 2 == 0 { 40 } else { 0 };
            *p = image::Rgb([
                smooth.clamp(0, 215) as u8 + fine,
                (255 - smooth).clamp(0, 215) as u8 + fine,
                ((smooth * 7) % 216) as u8 + fine,
            ]);
        }
        img
    }

    #[test]
    fn test_resize_rgb_matches_reference_and_faster() {
        let src = pattern_img(2400, 1800);
        let a = resize_rgb(&src, 1200, 900);
        let b = image::imageops::resize(&src, 1200, 900, image::imageops::FilterType::Triangle);
        assert_eq!((a.width(), a.height()), (1200, 900));
        let (mut diff, mut max_diff) = (0u64, 0u32);
        for (p, q) in a.pixels().zip(b.pixels()) {
            for c in 0..3 {
                let d = (p[c] as i32 - q[c] as i32).unsigned_abs();
                diff += d as u64;
                max_diff = max_diff.max(d);
            }
        }
        let avg = diff as f64 / (1200 * 900 * 3) as f64;
        assert!(avg < 2.0, "与参考实现平均偏差过大: {avg:.3}");
        assert!(max_diff <= 24, "单像素最大偏差过大: {max_diff}");
        // 计时对比（信息性，不作硬断言）
        let t0 = std::time::Instant::now();
        let _ = resize_rgb(&src, 1800, 1350);
        let fast = t0.elapsed();
        let t1 = std::time::Instant::now();
        let _ = image::imageops::resize(
            &src,
            1800,
            1350,
            image::imageops::FilterType::Triangle,
        );
        println!("resize_rgb={fast:?} imageops::resize={:?}", t1.elapsed());
    }

    #[test]
    fn test_box_path_keeps_mean() {
        // 7.6 倍降采样走面积均值路径：亮度均值应基本保持
        let src = pattern_img(3200, 2400);
        let out = resize_rgb(&src, 420, 315);
        assert_eq!((out.width(), out.height()), (420, 315));
        let mean = |img: &RgbImage| {
            img.as_raw().iter().map(|&v| v as f64).sum::<f64>() / img.as_raw().len() as f64
        };
        assert!((mean(&out) - mean(&src)).abs() < 2.0, "均值偏差过大");
        let t = std::time::Instant::now();
        let _ = resize_rgb(&src, 420, 315);
        println!("thumb 3200->420: {:?}", t.elapsed());
    }

    #[test]
    fn test_resize_gray_dims() {
        let g = GrayImage::from_fn(1000, 800, |x, y| image::Luma([((x + y) % 251) as u8]));
        let r = resize_gray(&g, 300, 240);
        assert_eq!((r.width(), r.height()), (300, 240));
        // 均值应基本保持
        let mean = |img: &GrayImage| {
            img.as_raw().iter().map(|&v| v as f64).sum::<f64>() / img.as_raw().len() as f64
        };
        assert!((mean(&r) - mean(&g)).abs() < 12.0);
    }
}
