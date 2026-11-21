//! 文档边缘自动检测：缩放 → 灰度 → 模糊 → Canny → 膨胀 → 轮廓 → 四边形拟合

use crate::geometry::{approx_quad, convex_hull, dist, min_area_rect, polygon_area, Pt};
use image::{imageops, GrayImage, RgbImage};
use imageproc::contours::find_contours;
use imageproc::distance_transform::Norm;
use imageproc::edges::canny;
use imageproc::morphology::dilate;

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
    let small = imageops::resize(img, sw, sh, imageops::FilterType::Triangle);
    let gray = imageops::grayscale(&small);
    let blurred = imageproc::filter::gaussian_blur_f32(&gray, 1.4);

    let med = median(&blurred) as f32;
    let lo = (0.55 * med).clamp(8.0, 120.0);
    let hi = (1.35 * med).clamp(30.0, 240.0);
    let mut best = try_contours(&dilate(&canny(&blurred, lo, hi), Norm::LInf, 1), sw, sh);

    // 第二遍：更宽松的阈值，处理弱对比场景
    if best.is_none() {
        let edges = dilate(&canny(&blurred, lo * 0.5, hi * 0.5), Norm::LInf, 2);
        best = try_contours(&edges, sw, sh);
    }

    let (quad, confidence) = match best {
        Some(q) => {
            let inv = 1.0 / scale;
            let mut out = q;
            for p in out.iter_mut() {
                p[0] = (p[0] * inv).clamp(0.0, ow as f32 - 1.0);
                p[1] = (p[1] * inv).clamp(0.0, oh as f32 - 1.0);
            }
            // 600px 缩图上的角点回投 4000px 原图误差可达 7~13px，
            // 在原图分辨率上做边缘直线拟合取交点，精化到亚像素级
            let refined = refine_quad_subpixel(img, out);
            (refined, 1.0)
        }
        None => (full_quad(ow, oh), 0.0),
    };
    QuadDetection { quad, confidence }
}

type Line = (Pt, Pt); // 直线 = (线上一点, 单位方向)

/// 角点亚像素精化：对每个角点，在其相邻两条边各采样若干点，
/// 沿法线方向做梯度峰值亚像素搜索得到边缘点，各自拟合直线取交点。
/// 任一条边拟合失败或交点偏离过远时，保留原角点。
fn refine_quad_subpixel(img: &RgbImage, quad: [Pt; 4]) -> [Pt; 4] {
    let gray = imageops::grayscale(img);
    let (w, h) = (gray.width() as i32, gray.height() as i32);
    let buf = gray.into_raw();
    let grad = move |x: f32, y: f32| -> f32 {
        let cx = |v: f32| (v.round() as i32).clamp(0, w - 1) as usize;
        let cy = |v: f32| (v.round() as i32).clamp(0, h - 1) as usize;
        let at = |xx: f32, yy: f32| buf[cy(yy) * w as usize + cx(xx)] as f32;
        let gx = at(x + 1.0, y - 1.0) + 2.0 * at(x + 1.0, y) + at(x + 1.0, y + 1.0)
            - at(x - 1.0, y - 1.0) - 2.0 * at(x - 1.0, y) - at(x - 1.0, y + 1.0);
        let gy = at(x - 1.0, y + 1.0) + 2.0 * at(x, y + 1.0) + at(x + 1.0, y + 1.0)
            - at(x - 1.0, y - 1.0) - 2.0 * at(x, y - 1.0) - at(x + 1.0, y - 1.0);
        (gx * gx + gy * gy).sqrt()
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

fn try_contours(edges: &GrayImage, sw: u32, sh: u32) -> Option<[Pt; 4]> {
    let img_area = (sw * sh) as f32;
    let contours = find_contours::<i32>(edges);
    let mut best: Option<([Pt; 4], f32)> = None;
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
        match best {
            Some((_, a)) if a >= qa => {}
            _ => best = Some((quad, qa)),
        }
    }
    best.map(|(quad, _)| quad)
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
