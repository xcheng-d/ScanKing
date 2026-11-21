//! 图像质量评估：失焦（模糊）检测
//!
//! 拍完立刻算 Laplacian 方差，低于阈值提示"可能拍糊了，建议重拍"，
//! 从源头避免模糊输入，比任何事后增强都划算。

use image::{imageops, RgbImage};

/// Laplacian 方差低于该值（0~255 灰度域）判定为可能失焦
pub const BLURRY_THRESHOLD: f32 = 80.0;

/// Laplacian 方差清晰度评分：统一在长边 ≤1200px 的缩小图上计算，
/// 与拍摄分辨率解耦。数值越大越清晰；失焦/手抖照片通常远低于 BLURRY_THRESHOLD。
pub fn laplacian_variance(img: &RgbImage) -> f32 {
    let (w, h) = (img.width(), img.height());
    let long = w.max(h);
    let g = if long > 1200 {
        let s = 1200.0 / long as f32;
        imageops::grayscale(&imageops::resize(
            img,
            ((w as f32 * s).round() as u32).max(3),
            ((h as f32 * s).round() as u32).max(3),
            imageops::FilterType::Triangle,
        ))
    } else {
        imageops::grayscale(img)
    };
    let (w, h) = (g.width() as usize, g.height() as usize);
    if w < 3 || h < 3 {
        return f32::MAX;
    }
    let src = g.as_raw();
    let n = ((w - 2) * (h - 2)) as f64;
    let mut sum = 0f64;
    let mut sum_sq = 0f64;
    for y in 1..h - 1 {
        let row = y * w;
        for x in 1..w - 1 {
            let c = src[row + x] as f64;
            let lap = 4.0 * c
                - src[row - w + x] as f64
                - src[row + w + x] as f64
                - src[row + x - 1] as f64
                - src[row + x + 1] as f64;
            sum += lap;
            sum_sq += lap * lap;
        }
    }
    let mean = sum / n;
    ((sum_sq / n - mean * mean) as f32).max(0.0)
}

/// 是否疑似失焦
pub fn is_blurry(img: &RgbImage) -> bool {
    laplacian_variance(img) < BLURRY_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn test_blurry_vs_sharp() {
        // 纯色 + 弱噪声：接近失焦
        let mut blur = RgbImage::new(400, 400);
        for (x, y, p) in blur.enumerate_pixels_mut() {
            let noise = ((x * 7 + y * 13) % 5) as f32;
            *p = Rgb([(200.0 + noise) as u8; 3]);
        }
        // 棋盘格锐利纹理：清晰
        let mut sharp = RgbImage::new(400, 400);
        for (x, y, p) in sharp.enumerate_pixels_mut() {
            let v = if (x / 4 + y / 4) % 2 == 0 { 30 } else { 220 };
            *p = Rgb([v; 3]);
        }
        let (b, s) = (laplacian_variance(&blur), laplacian_variance(&sharp));
        assert!(b < BLURRY_THRESHOLD);
        assert!(s > b * 10.0);
    }
}
