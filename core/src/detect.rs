//! 文档边缘自动检测：缩放 → 灰度 → 模糊 → Canny → 膨胀 → 轮廓 → 四边形拟合

use crate::geometry::{approx_quad, convex_hull, dist, min_area_rect, polygon_area, Pt};
use image::{imageops, GrayImage, RgbImage};
use imageproc::contours::find_contours;
use imageproc::distance_transform::Norm;
use imageproc::edges::canny;
use imageproc::morphology::dilate;
use rayon::prelude::*;

const DETECT_SIZE: u32 = 600;

/// 检测图中最像"一页文档"的四边形，返回原图坐标系下的四角（左上/右上/右下/左下）。
/// 检测不到时返回整幅图像的四角。
pub fn detect_quad(img: &RgbImage) -> [Pt; 4] {
    detect_quad_with_confidence(img).quad
}

#[derive(Debug, Clone, Copy)]
pub struct QuadDetection {
    pub quad: [Pt; 4],
    pub confidence: f32,
}

pub fn detect_quad_with_confidence(img: &RgbImage) -> QuadDetection {
    let (ow, oh) = (img.width(), img.height());
    let scale = (DETECT_SIZE as f32 / ow.max(oh) as f32).min(1.0);
    let (sw, sh) = (
        ((ow as f32 * scale).round() as u32).max(8),
        ((oh as f32 * scale).round() as u32).max(8),
    );
    let small = crate::resize::resize_rgb(img, sw, sh);
    let gray = imageops::grayscale(&small);
    let blurred = imageproc::filter::gaussian_blur_f32(&gray, 1.4);

    // 背景色差图：淡蓝/粉红票据与白背景在灰度上几乎不可见，但颜色差稳定
    let bg = estimate_background(&small);
    let diff = color_diff_map(&small, bg);

    let med = median(&blurred) as f32;
    let lo = (0.55 * med).clamp(8.0, 120.0);
    let hi = (1.35 * med).clamp(30.0, 240.0);

    // 先跑原有两条灰度路径；置信度足够时直接采用，避免普通文档多花时间
    let edges_strict = dilate(&canny(&blurred, lo, hi), Norm::LInf, 1);
    let edges_loose = dilate(&canny(&blurred, lo * 0.5, hi * 0.5), Norm::LInf, 2);
    let mut candidates: Vec<QuadCandidate> = Vec::new();
    if let Some(c) = try_contours(&edges_strict, &blurred, &diff, sw, sh) {
        candidates.push(c);
    }
    if let Some(c) = try_contours(&edges_loose, &blurred, &diff, sw, sh) {
        candidates.push(c);
    }
    let conf_of = |c: &QuadCandidate| confidence_score(&blurred, &diff, c, sw, sh);
    let quick_conf = candidates.iter().map(&conf_of).fold(0.0f32, f32::max);
    // 分位数阈值 + 成片中等色差判据，避免被文字极值拉偏
    let (diff_t, has_color_region) = color_threshold(&diff);
    // 灰度路径失败/置信不足，或确实存在成片淡色纸张时，启用
    // 梯度分位数自适应 / 色差边缘合并 / 色差区域分割 三条补充路径。
    // 后者不能省略：低对比度图里错误框也可能拿到较高置信度，色差分割才是最可靠证据
    if quick_conf < 0.7 || has_color_region {
        let color_signal = has_color_region;

        let (glo, ghi) = gradient_thresholds(&blurred);
        let edges_grad = dilate(&canny(&blurred, glo, ghi), Norm::LInf, 1);
        if let Some(c) = try_contours(&edges_grad, &blurred, &diff, sw, sh) {
            candidates.push(c);
        }
        if color_signal {
            let color_edges = dilate(&canny(&diff, 18.0, 48.0), Norm::LInf, 1);
            let combined_buf: Vec<u8> = edges_loose
                .as_raw()
                .iter()
                .zip(color_edges.as_raw())
                .map(|(&a, &b)| a.max(b))
                .collect();
            let edges_combined = GrayImage::from_raw(sw, sh, combined_buf).unwrap();
            if let Some(c) = try_contours(&edges_combined, &blurred, &diff, sw, sh) {
                candidates.push(c);
            }
            let mask: Vec<u8> = diff
                .as_raw()
                .iter()
                .map(|&v| if v >= diff_t { 255 } else { 0 })
                .collect();
            let mask = GrayImage::from_raw(sw, sh, mask).unwrap();
            if let Some(c) = try_contours(&mask, &blurred, &diff, sw, sh) {
                candidates.push(c);
            }
        }
    }
    // 所有候选按同一置信度择优
    let best = candidates.into_iter().max_by(|a, b| {
        conf_of(a)
            .partial_cmp(&conf_of(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let (quad, confidence) = match best {
        Some(cand) => {
            // 真实置信度：矩形度 + 边缘支持率 + 面积占比（在缩图坐标上评估）
            let conf = confidence_score(&blurred, &diff, &cand, sw, sh);
            let inv = 1.0 / scale;
            let mut out = cand.quad;
            for p in out.iter_mut() {
                p[0] = (p[0] * inv).clamp(0.0, ow as f32 - 1.0);
                p[1] = (p[1] * inv).clamp(0.0, oh as f32 - 1.0);
            }
            // 600px 缩图上的角点回投 4000px 原图误差可达 7~13px，
            // 在原图分辨率上做边缘直线拟合取交点，精化到亚像素级；
            // 低对比度彩色纸张用全分辨率色差图提供梯度
            // Always provide full-resolution color contrast for the final edge refinement.
            // The grayscale gradient remains the primary signal; color difference is additive.
            let full_diff = Some(color_diff_map(img, bg));
            let mut refined = refine_quad_subpixel(img, out, full_diff.as_ref());
            // 直线拟合交点可能落到图外（透视纸张出画时），钳回图像范围内
            for p in refined.iter_mut() {
                p[0] = p[0].clamp(0.0, ow as f32 - 1.0);
                p[1] = p[1].clamp(0.0, oh as f32 - 1.0);
            }
            (refined, conf)
        }
        None => (full_quad(ow, oh), 0.0),
    };
    QuadDetection { quad, confidence }
}

/// 轮廓候选：四边形 + 用于置信度评估的面积信息
struct QuadCandidate {
    quad: [Pt; 4],
    hull_area: f32,
    quad_area: f32,
}

/// 边缘支持率判定的梯度阈值（0~255 Sobel 幅值）
const EDGE_GRAD_MIN: f32 = 22.0;

/// 合成置信度 ∈ [0,1]：
/// - 矩形度（0.45）：quad_area/hull_area，四边形贴合轮廓凸包的程度
/// - 边缘支持率（0.40）：四边采样点处梯度超过阈值的比例，虚检边框此项极低
/// - 面积占比（0.15）：过小不像文档，≈整图说明匹配到图像外框
/// 前端据 confidence < 0.45 提示"请手动调整裁剪框"
fn confidence_score(
    blurred: &GrayImage,
    diff: &GrayImage,
    cand: &QuadCandidate,
    sw: u32,
    sh: u32,
) -> f32 {
    if !quad_is_convex(&cand.quad) {
        return 0.0;
    }
    let rect = (cand.quad_area / cand.hull_area.max(1.0)).clamp(0.0, 1.0);
    let area_ratio = cand.quad_area / ((sw * sh) as f32);
    let area_score = if area_ratio < 0.12 {
        area_ratio / 0.12
    } else if area_ratio <= 0.82 {
        1.0
    } else if area_ratio <= 0.95 {
        (0.95 - area_ratio) / 0.13
    } else {
        ((1.0 - area_ratio) / 0.05).clamp(0.0, 1.0)
    };
    let edge = edge_support_ratio(blurred, diff, &cand.quad);
    let border = border_margin_score(&cand.quad, sw, sh);
    let contrast = edge_side_contrast(blurred, diff, &cand.quad);
    (0.30 * rect + 0.25 * edge + 0.15 * area_score
        + 0.15 * border
        + 0.15 * contrast)
        .clamp(0.0, 1.0)
}

/// 四边各采样 10 点（避开角点），统计灰度/色差图上 Sobel 梯度达标比例。
/// 取两者最大值：低对比度彩色纸张的灰度梯度很弱，但色差边缘很强
fn edge_support_ratio(gray: &GrayImage, diff: &GrayImage, quad: &[Pt; 4]) -> f32 {
    let (w, h) = (gray.width() as i32, gray.height() as i32);
    let buf = gray.as_raw();
    let dbuf = diff.as_raw();
    let mut supported = 0usize;
    let mut total = 0usize;
    for e in 0..4 {
        let (a, b) = (quad[e], quad[(e + 1) % 4]);
        for k in 0..10usize {
            let t = 0.1 + 0.08 * k as f32; // 0.1..0.82
            let x = a[0] + (b[0] - a[0]) * t;
            let y = a[1] + (b[1] - a[1]) * t;
            total += 1;
            let g = sobel_mag(buf, w, h, x, y);
            let d = sobel_mag(dbuf, w, h, x, y);
            if g.max(d) >= EDGE_GRAD_MIN {
                supported += 1;
            }
        }
    }
    if total == 0 { 0.0 } else { supported as f32 / total as f32 }
}

fn quad_is_convex(quad: &[Pt; 4]) -> bool {
    let mut sign = 0i32;
    for i in 0..4 {
        let a = quad[i];
        let b = quad[(i + 1) % 4];
        let c = quad[(i + 2) % 4];
        let cross = (b[0] - a[0]) * (c[1] - b[1])
            - (b[1] - a[1]) * (c[0] - b[0]);
        let s = if cross > 0.0 { 1 } else if cross < 0.0 { -1 } else { 0 };
        if s != 0 {
            if sign != 0 && s != sign {
                return false;
            }
            sign = s;
        }
    }
    sign != 0
}

/// Prefer candidates separated from the image frame. This suppresses a full-photo
/// or desk/table outer contour when a smaller document contour is available.
fn border_margin_score(quad: &[Pt; 4], sw: u32, sh: u32) -> f32 {
    let margin = (sw.min(sh) as f32 * 0.05).max(4.0);
    let mut min_normalized = f32::MAX;
    for p in quad {
        let d = p[0].min(p[1]).min(sw as f32 - 1.0 - p[0])
            .min(sh as f32 - 1.0 - p[1])
            .max(0.0);
        min_normalized = min_normalized.min(d / margin);
    }
    min_normalized.clamp(0.0, 1.0)
}

/// Compare inside/outside intensity and color on both sides of each edge.
/// A real document edge generally has stronger consistent contrast than texture or text.
fn edge_side_contrast(gray: &GrayImage, diff: &GrayImage, quad: &[Pt; 4]) -> f32 {
    let w = gray.width() as i32;
    let h = gray.height() as i32;
    let at = |buf: &[u8], x: i32, y: i32| -> f32 {
        let x = x.clamp(0, w - 1) as usize;
        let y = y.clamp(0, h - 1) as usize;
        buf[y * w as usize + x] as f32
    };
    let mut contrast = 0.0f32;
    let mut count = 0usize;
    for e in 0..4 {
        let a = quad[e];
        let b = quad[(e + 1) % 4];
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let len = dx.hypot(dy);
        if len < 4.0 {
            continue;
        }
        let nx = -dy / len;
        let ny = dx / len;
        for k in 1..=8usize {
            let t = k as f32 / 9.0;
            let x = a[0] + dx * t;
            let y = a[1] + dy * t;
            let offset = (len * 0.012).clamp(2.0, 10.0);
            let ix = (x + nx * offset).round() as i32;
            let iy = (y + ny * offset).round() as i32;
            let ox = (x - nx * offset).round() as i32;
            let oy = (y - ny * offset).round() as i32;
            let gray_d = (at(gray.as_raw(), ix, iy) - at(gray.as_raw(), ox, oy)).abs();
            let diff_d = (at(diff.as_raw(), ix, iy) - at(diff.as_raw(), ox, oy)).abs();
            contrast += (gray_d.max(diff_d) / 64.0).clamp(0.0, 1.0);
            count += 1;
        }
    }
    if count == 0 { 0.0 } else { contrast / count as f32 }
}

/// 采样点处的 Sobel 梯度幅值（越界坐标钳制到图内）
fn sobel_mag(buf: &[u8], w: i32, h: i32, x: f32, y: f32) -> f32 {
    let cx = |v: f32| (v.round() as i32).clamp(0, w - 1) as usize;
    let cy = |v: f32| (v.round() as i32).clamp(0, h - 1) as usize;
    let at = |xx: f32, yy: f32| buf[cy(yy) * w as usize + cx(xx)] as f32;
    let gx = at(x + 1.0, y - 1.0) + 2.0 * at(x + 1.0, y) + at(x + 1.0, y + 1.0)
        - at(x - 1.0, y - 1.0) - 2.0 * at(x - 1.0, y) - at(x - 1.0, y + 1.0);
    let gy = at(x - 1.0, y + 1.0) + 2.0 * at(x, y + 1.0) + at(x + 1.0, y + 1.0)
        - at(x - 1.0, y - 1.0) - 2.0 * at(x, y - 1.0) - at(x + 1.0, y - 1.0);
    (gx * gx + gy * gy).sqrt()
}

type Line = (Pt, Pt); // 直线 = (线上一点, 单位方向)

/// 角点亚像素精化：对每个角点，在其相邻两条边各采样若干点，
/// 沿法线方向做梯度峰值亚像素搜索得到边缘点，各自拟合直线取交点。
/// 任一条边拟合失败或交点偏离过远时，保留原角点。
fn refine_quad_subpixel(
    img: &RgbImage,
    quad: [Pt; 4],
    color_diff: Option<&GrayImage>,
) -> [Pt; 4] {
    // 全分辨率灰度化：单线程 imageops::grayscale 在端机要几十毫秒，改整数权重 + rayon
    let (iw, ih) = (img.width() as usize, img.height() as usize);
    let src = img.as_raw();
    let mut lum = vec![0u8; iw * ih];
    lum.par_chunks_mut(iw).enumerate().for_each(|(y, row)| {
        let base = y * iw * 3;
        for (x, o) in row.iter_mut().enumerate() {
            let s = base + x * 3;
            // ITU-R BT.601 权重（与 image::grayscale 一致）
            *o = ((299 * src[s] as u32 + 587 * src[s + 1] as u32 + 114 * src[s + 2] as u32 + 500)
                / 1000) as u8;
        }
    });
    let (w, h) = (iw as i32, ih as i32);
    let buf = lum;
    let grad = |x: f32, y: f32| -> f32 {
        let g = sobel_mag(&buf, w, h, x, y);
        match color_diff {
            Some(d) if d.width() as i32 == w && d.height() as i32 == h => {
                g.max(sobel_mag(d.as_raw(), w, h, x, y))
            }
            _ => g,
        }
    };
    // 搜索半径：覆盖缩图 1px 回投误差（7~13px）并留余量
    let diag = (img.width() as f32).hypot(img.height() as f32);
    let search = (diag * 0.006).clamp(8.0, 24.0);
    let mut refined = quad;
    for i in 0..4 {
        let c = quad[i];
        let e1 = fit_edge_line(&grad, c, quad[(i + 3) % 4], search);
        let e2 = fit_edge_line(&grad, c, quad[(i + 1) % 4], search);
        if let (Some((p1, d1)), Some((p2, d2))) = (e1, e2) {
            if let Some(p) = line_intersect(p1, d1, p2, d2) {
                if dist(p, c) <= search * 1.5 {
                    refined[i] = p;
                }
            }
        }
    }
    refined
}

/// 在 corner→neighbor 边的近角段采样，逐点沿法线搜索梯度峰值（含亚像素），
/// 收集边缘点后做全最小二乘直线拟合
fn fit_edge_line(
    grad: &dyn Fn(f32, f32) -> f32,
    corner: Pt,
    neighbor: Pt,
    search: f32,
) -> Option<Line> {
    let (ex, ey) = (neighbor[0] - corner[0], neighbor[1] - corner[1]);
    let len = (ex * ex + ey * ey).sqrt();
    if len < 8.0 {
        return None;
    }
    let (ux, uy) = (ex / len, ey / len);
    let (nx, ny) = (-uy, ux);
    // 沿边取 6 个采样点，跳过角点附近（圆角/磨损干扰），覆盖 9%~34% 边长
    let mut pts: Vec<Pt> = Vec::new();
    let s_range = search as i32;
    let proximity = 2.0 * (search * 0.5) * (search * 0.5); // 法线距离高斯权重 σ=半径一半
    for k in 1..=6usize {
        let t = len * (0.05 * k as f32 + 0.04);
        let bx = corner[0] + ux * t;
        let by = corner[1] + uy * t;
        let mut gs: Vec<f32> = Vec::with_capacity((2 * s_range + 1) as usize);
        for s in -s_range..=s_range {
            let sf = s as f32;
            let g = grad(bx + nx * sf, by + ny * sf);
            // 峰值评分：梯度 × 靠近标称边缘的先验，避免被页面内文字抢走
            gs.push(g * (-sf * sf / proximity).exp());
        }
        let bi = (0..gs.len()).reduce(|a, b| if gs[b] > gs[a] { b } else { a })?;
        if gs[bi] <= 0.0 {
            continue;
        }
        // 抛物线插值亚像素偏移
        let delta = if bi > 0 && bi + 1 < gs.len() {
            let (gm, g0, gp) = (gs[bi - 1], gs[bi], gs[bi + 1]);
            let denom = gm - 2.0 * g0 + gp;
            if denom.abs() > 1e-9 {
                ((gm - gp) / (2.0 * denom)).clamp(-1.0, 1.0)
            } else {
                0.0
            }
        } else {
            0.0
        };
        let so = (bi as f32 - s_range as f32) + delta;
        pts.push([bx + nx * so, by + ny * so]);
    }
    if pts.len() < 3 {
        return None;
    }
    // 全最小二乘（PCA 主方向）
    let n = pts.len() as f32;
    let cx = pts.iter().map(|p| p[0]).sum::<f32>() / n;
    let cy = pts.iter().map(|p| p[1]).sum::<f32>() / n;
    let (mut sxx, mut syy, mut sxy) = (0.0f32, 0.0f32, 0.0f32);
    for p in &pts {
        let (dx, dy) = (p[0] - cx, p[1] - cy);
        sxx += dx * dx;
        syy += dy * dy;
        sxy += dx * dy;
    }
    let theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    Some(([cx, cy], [theta.cos(), theta.sin()]))
}

fn line_intersect(p1: Pt, d1: Pt, p2: Pt, d2: Pt) -> Option<Pt> {
    let denom = d1[0] * d2[1] - d1[1] * d2[0];
    if denom.abs() < 1e-6 {
        return None;
    }
    let t = ((p2[0] - p1[0]) * d2[1] - (p2[1] - p1[1]) * d2[0]) / denom;
    Some([p1[0] + t * d1[0], p1[1] + t * d1[1]])
}

pub fn full_quad(w: u32, h: u32) -> [Pt; 4] {
    [
        [0.0, 0.0],
        [w as f32 - 1.0, 0.0],
        [w as f32 - 1.0, h as f32 - 1.0],
        [0.0, h as f32 - 1.0],
    ]
}

fn try_contours(
    edges: &GrayImage,
    gray: &GrayImage,
    diff: &GrayImage,
    sw: u32,
    sh: u32,
) -> Option<QuadCandidate> {
    let img_area = (sw * sh) as f32;
    let contours = find_contours::<i32>(edges);
    let mut best: Option<QuadCandidate> = None;
    for c in &contours {
        if c.points.len() < 8 {
            continue;
        }
        let pts: Vec<Pt> = c.points.iter().map(|p| [p.x as f32, p.y as f32]).collect();
        let hull = convex_hull(&pts);
        if hull.len() < 4 {
            continue;
        }
        let hull_area = polygon_area(&hull);
        // 面积必须占画面 12% 以上，且不能几乎等于整个画面（那是图像边框）
        if hull_area < img_area * 0.12 || hull_area > img_area * 0.985 {
            continue;
        }
        let quad = match approx_quad(&hull) {
            Some(q) => q,
            None => {
                let r = min_area_rect(&hull);
                // 最小外接矩形与凸包面积差太多说明形状不像四边形
                if polygon_area(&r.to_vec()) > hull_area * 1.35 {
                    continue;
                }
                r
            }
        };
        let qa = polygon_area(&quad.to_vec());
        if qa < img_area * 0.10 {
            continue;
        }
        let candidate = QuadCandidate { quad, hull_area, quad_area: qa };
        match &best {
            Some(b)
                if confidence_score(gray, diff, b, sw, sh)
                    >= confidence_score(gray, diff, &candidate, sw, sh) => {}
            _ => best = Some(candidate),
        }
    }
    best
}

fn median(g: &GrayImage) -> u8 {
    let mut hist = [0u32; 256];
    for p in g.pixels() {
        hist[p.0[0] as usize] += 1;
    }
    let half = (g.width() * g.height()) / 2;
    let mut acc = 0u32;
    for (i, &c) in hist.iter().enumerate() {
        acc += c;
        if acc >= half {
            return i as u8;
        }
    }
    128
}

// ---------- 低对比度彩色纸张：背景色差分割 ----------

/// 色差放大倍数：淡蓝/粉红票据与白背景的平均通道差常仅 8~15，灰度上几乎不可见
const COLOR_GAIN: f32 = 6.0;

/// 估计纯色背景：取图像最外 4% 边框带的分通道中位数（比四角更抗局部杂物）
fn estimate_background(img: &RgbImage) -> [u8; 3] {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let band = ((w.min(h) as f32 * 0.04) as usize).max(2);
    let mut hist = [[0u32; 256]; 3];
    let mut total = 0u32;
    for y in 0..h {
        for x in 0..w {
            if x < band || x >= w - band || y < band || y >= h - band {
                let p = img.get_pixel(x as u32, y as u32);
                for c in 0..3 {
                    hist[c][p[c] as usize] += 1;
                }
                total += 1;
            }
        }
    }
    let half = total / 2;
    let mut out = [0u8; 3];
    for c in 0..3 {
        let mut acc = 0u32;
        for (v, &n) in hist[c].iter().enumerate() {
            acc += n;
            if acc >= half {
                out[c] = v as u8;
                break;
            }
        }
    }
    out
}

/// 逐像素到背景色的平均通道绝对差（放大 COLOR_GAIN 倍）。
/// 淡色纸张整体是一块均匀的中等色差区域，文字是更高色差，纯色背景接近 0。
fn color_diff_map(img: &RgbImage, bg: [u8; 3]) -> GrayImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let src = img.as_raw();
    let mut out = vec![0u8; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let base = y * w * 3;
        for (x, o) in row.iter_mut().enumerate() {
            let s = base + x * 3;
            let d = (src[s].abs_diff(bg[0]) as f32
                + src[s + 1].abs_diff(bg[1]) as f32
                + src[s + 2].abs_diff(bg[2]) as f32)
                / 3.0;
            *o = (d * COLOR_GAIN).round().min(255.0) as u8;
        }
    });
    GrayImage::from_raw(w as u32, h as u32, out).unwrap()
}

/// 色差区域分割阈值与信号判据。
///
/// 不能用 Otsu：文字/表格线到背景的色差被放大后顶到 255，Otsu 为最大化类间
/// 方差会把阈值抬到 120+，结果整块淡色纸张（色差 40~90）被判成"背景"，
/// 只剩文字前景，色差分割直接失效。改用：
/// - p90 反映纸张区域的典型色差，阈值取其 40%（钳制 8~60）；
/// - `mid_ratio`：色差落在 12..=150 的像素占比，确保存在"成片的淡色纸张"
///   而不是只有离散的深色文字（黑白文档的文字色差恒为 255）。
fn color_threshold(g: &GrayImage) -> (u8, bool) {
    let mut hist = [0u32; 256];
    for &v in g.as_raw() {
        hist[v as usize] += 1;
    }
    let total = g.as_raw().len() as f32;
    if total == 0.0 {
        return (8, false);
    }
    let mid: u32 = hist[12..=150].iter().sum();
    let mid_ratio = mid as f32 / total;
    let mut acc = 0u32;
    let mut p90 = 0u8;
    for (v, &n) in hist.iter().enumerate() {
        acc += n;
        if acc as f32 >= total * 0.9 {
            p90 = v as u8;
            break;
        }
    }
    let t = ((p90 as f32) * 0.4).clamp(8.0, 60.0) as u8;
    let strong = hist[t as usize..].iter().sum::<u32>() as f32 / total;
    let ok = mid_ratio >= 0.15 && (0.08..=0.92).contains(&strong);
    (t, ok)
}

/// 基于梯度分位数的 Canny 阈值：修复"高亮低对比"图像中
/// `lo = 0.55×中位亮度` 被抬到 120、弱边缘完全丢失的问题
fn gradient_thresholds(g: &GrayImage) -> (f32, f32) {
    let (w, h) = (g.width() as usize, g.height() as usize);
    let buf = g.as_raw();
    let mut hist = [0u32; 128];
    if w < 3 || h < 3 {
        return (8.0, 20.0);
    }
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            // 标准 3×3 Sobel：上/中/下三行分别为 i-w、i、i+w
            let gx = (buf[i - w + 1] as i32 + 2 * buf[i + 1] as i32 + buf[i + w + 1] as i32)
                - (buf[i - w - 1] as i32 + 2 * buf[i - 1] as i32 + buf[i + w - 1] as i32);
            let gy = (buf[i + w - 1] as i32 + 2 * buf[i + w] as i32 + buf[i + w + 1] as i32)
                - (buf[i - w - 1] as i32 + 2 * buf[i - w] as i32 + buf[i - w + 1] as i32);
            let m = ((gx * gx + gy * gy) as f32).sqrt() / 4.0;
            hist[(m as usize).min(127)] += 1;
        }
    }
    let total = hist.iter().sum::<u32>() as f32;
    let percentile = |q: f32| -> f32 {
        let target = total * q;
        let mut acc = 0u32;
        for (v, &n) in hist.iter().enumerate() {
            acc += n;
            if acc as f32 >= target {
                return v as f32;
            }
        }
        127.0
    };
    let lo = percentile(0.80).clamp(3.0, 40.0);
    let hi = percentile(0.95).max(lo * 1.6).clamp(lo * 1.6, 110.0);
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    /// 白背景上的淡蓝票据：灰度差仅 ~8，RGB 色差明显；内部有大量深色文字/表格线，
    /// 其到背景的色差被放大后顶到 255（真实扫描件就是这样，会把 Otsu 阈值拉偏）
    fn low_contrast_paper() -> (RgbImage, [Pt; 4]) {
        let (w, h) = (600u32, 800u32);
        let mut img = RgbImage::new(w, h);
        for p in img.pixels_mut() {
            *p = Rgb([255, 255, 255]);
        }
        let (x0, y0, x1, y1) = (45u32, 70u32, 555u32, 470u32);
        for y in y0..y1 {
            for x in x0..x1 {
                // 灰度 ≈247，与白背景 255 仅差 8
                img.put_pixel(x, y, Rgb([240, 250, 252]));
            }
        }
        for y in (y0 + 30..y1 - 20).step_by(17) {
            for x in (x0 + 20..x1 - 20).step_by(6) {
                for d in 0..3 {
                    img.put_pixel(x + d, y, Rgb([60, 90, 150]));
                }
            }
        }
        let expect: [Pt; 4] = [
            [x0 as f32, y0 as f32],
            [x1 as f32, y0 as f32],
            [x1 as f32, y1 as f32],
            [x0 as f32, y1 as f32],
        ];
        (img, expect)
    }

    fn assert_quad_matches(quad: &[Pt; 4], expect: &[Pt; 4]) {
        for e in expect {
            assert!(
                quad.iter()
                    .any(|q| (q[0] - e[0]).abs() < 16.0 && (q[1] - e[1]).abs() < 16.0),
                "期望角点 {e:?} 未出现在检测结果 {quad:?}"
            );
        }
    }

    #[test]
    fn test_low_contrast_colored_paper_detected() {
        let (img, expect) = low_contrast_paper();
        let det = detect_quad_with_confidence(&img);
        assert!(
            det.confidence > 0.45,
            "低对比度彩色纸张应被检出, conf={}",
            det.confidence
        );
        assert_quad_matches(&det.quad, &expect);
    }

    /// 后端导入用内存原图、编辑页自动检测读 orig.jpg（4:2:2 q92 编码后解码）。
    /// 两条路径必须给出一致的边框，否则会出现"导入时框歪、点自动检测才正常"
    #[test]
    fn test_detection_consistent_after_jpeg_roundtrip() {
        let (img, expect) = low_contrast_paper();
        let a = detect_quad_with_confidence(&img);
        let mut enc = Vec::new();
        {
            let mut e = jpeg_encoder::Encoder::new(&mut enc, 92);
            e.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_1);
            e.encode(
                img.as_raw(),
                img.width() as u16,
                img.height() as u16,
                jpeg_encoder::ColorType::Rgb,
            )
            .unwrap();
        }
        let decoded = image::load_from_memory(&enc).unwrap().to_rgb8();
        let b = detect_quad_with_confidence(&decoded);
        assert!(
            a.confidence > 0.45 && b.confidence > 0.45,
            "两条路径都应检出, conf A={} B={}",
            a.confidence,
            b.confidence
        );
        assert_quad_matches(&a.quad, &expect);
        assert_quad_matches(&b.quad, &expect);
        for q in a.quad {
            assert!(
                b.quad
                    .iter()
                    .any(|p| (p[0] - q[0]).abs() < 12.0 && (p[1] - q[1]).abs() < 12.0),
                "JPEG 往返后角点不一致: {q:?} vs {:?}",
                b.quad
            );
        }
    }
}
