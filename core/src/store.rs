//! 文档库存储：每个文档一个目录 + meta.json，页面图片按处理阶段分文件缓存。
//!
//! 目录结构：
//! root/
//!   docs/{doc_id}/meta.json
//!   docs/{doc_id}/pages/{page_id}/orig.jpg      导入原图（已按 EXIF 转正、限制分辨率）
//!   docs/{doc_id}/pages/{page_id}/preview.jpg   编辑器用的缩小原图
//!   docs/{doc_id}/pages/{page_id}/warped.jpg    透视矫正后（未加滤镜）缓存
//!   docs/{doc_id}/pages/{page_id}/work.jpg      最终效果（矫正+旋转+滤镜）
//!   docs/{doc_id}/pages/{page_id}/thumb.jpg     缩略图
//!   docs/{doc_id}/pages/{page_id}/ocr.txt       OCR 结果
//!   exports/*.pdf

use crate::detect;
use crate::filters::{apply_filter, Filter};
use crate::geometry::{warp_quad, Pt};
use crate::idcard;
use crate::pdf::{build_pdf, PageMode, PdfPage};
use crate::quality;
use anyhow::{anyhow, bail, Context, Result};
use image::{imageops, DynamicImage, RgbImage};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_IMPORT_SIDE: u32 = 3200;
const PREVIEW_SIDE: u32 = 1600;
const THUMB_SIDE: u32 = 420;
const JPEG_Q_ORIG: u8 = 92;
const JPEG_Q_WORK: u8 = 88;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageMeta {
    pub id: String,
    pub filter: String,
    /// 原图坐标系四角 [左上,右上,右下,左下]；None 表示整页
    pub quad: Option<[[f32; 2]; 4]>,
    /// 顺时针旋转次数 0-3（矫正后再旋转）
    pub rotation: u32,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub ocr_done: bool,
    #[serde(default)]
    pub processing: bool,
    #[serde(default)]
    pub detection_confidence: Option<f32>,
    /// 清晰度评分（Laplacian 方差），低于 quality::BLURRY_THRESHOLD 提示拍糊
    #[serde(default)]
    pub sharpness: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocMeta {
    pub id: String,
    pub title: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(default)]
    pub tags: Vec<String>,
    pub pages: Vec<PageMeta>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocSummary {
    pub id: String,
    pub title: String,
    pub updated_ms: u64,
    pub page_count: usize,
    pub cover: Option<String>,
    pub tags: Vec<String>,
}

/// 返回给上层（UI）的页面信息，带各阶段文件路径
#[derive(Debug, Clone, Serialize)]
pub struct PageInfo {
    pub meta: PageMeta,
    pub preview: String,
    pub work: String,
    pub thumb: String,
    /// work.jpg 的实际尺寸
    pub work_width: u32,
    pub work_height: u32,
    /// 自动检测得到的四角（原图坐标系）
    pub quad: [[f32; 2]; 4],
    /// preview 相对原图的缩放比
    pub preview_scale: f32,
    /// 自动边框检测状态：1 表示找到候选边框，0 表示回退到整图
    pub detection_confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save_timings_ms: Option<PageSaveTimings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processing_timings_ms: Option<PageProcessingTimings>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageSaveTimings {
    pub raw_write: u64,
    pub decode: u64,
    pub orientation_and_rgb: u64,
    pub resize: u64,
    pub original_encode_and_write: u64,
    pub preview: u64,
    pub work_copy: u64,
    pub thumbnail: u64,
    pub metadata: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageProcessingTimings {
    pub original_decode: u64,
    pub orientation_and_rgb: u64,
    pub resize: u64,
    pub original_encode_and_write: u64,
    pub edge_detection: u64,
    pub quality_check: u64,
    pub preview: u64,
    pub warp_filter_thumbnail: u64,
    pub metadata: u64,
    pub total: u64,
}

pub struct Store {
    root: PathBuf,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

fn save_jpeg(img: &RgbImage, path: &Path, quality: u8) -> Result<()> {
    let out = encode_jpeg(img, quality)?;
    fs::write(path, out).with_context(|| format!("写入失败: {}", path.display()))?;
    Ok(())
}

fn encode_jpeg(img: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    enc.encode_image(img).context("JPEG 编码失败")?;
    Ok(out)
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// 按 EXIF Orientation 转正图像
fn apply_exif_orientation(img: DynamicImage, bytes: &[u8]) -> DynamicImage {
    let orientation = exif_orientation(bytes);
    match orientation {
        2 => DynamicImage::ImageRgba8(imageops::flip_horizontal(&img)),
        3 => DynamicImage::ImageRgba8(imageops::rotate180(&img)),
        4 => DynamicImage::ImageRgba8(imageops::flip_vertical(&img)),
        5 => {
            let r = imageops::rotate90(&img);
            DynamicImage::ImageRgba8(imageops::flip_horizontal(&r))
        }
        6 => DynamicImage::ImageRgba8(imageops::rotate90(&img)),
        7 => {
            let r = imageops::rotate270(&img);
            DynamicImage::ImageRgba8(imageops::flip_horizontal(&r))
        }
        8 => DynamicImage::ImageRgba8(imageops::rotate270(&img)),
        _ => img,
    }
}

fn exif_orientation(bytes: &[u8]) -> u32 {
    exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()
        .and_then(|e| {
            e.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
        })
        .unwrap_or(1)
}

fn source_image_path(page_dir: &Path) -> Option<PathBuf> {
    ["source.jpg", "source.png"]
        .iter()
        .map(|name| page_dir.join(name))
        .find(|path| path.exists())
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Store> {
        let root = root.into();
        fs::create_dir_all(root.join("docs"))?;
        fs::create_dir_all(root.join("exports"))?;
        fs::create_dir_all(root.join("models"))?;
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    fn doc_dir(&self, doc_id: &str) -> PathBuf {
        self.root.join("docs").join(doc_id)
    }

    fn page_dir(&self, doc_id: &str, page_id: &str) -> PathBuf {
        self.doc_dir(doc_id).join("pages").join(page_id)
    }

    fn load_doc(&self, doc_id: &str) -> Result<DocMeta> {
        let p = self.doc_dir(doc_id).join("meta.json");
        let data = fs::read(&p).with_context(|| format!("文档不存在: {}", doc_id))?;
        Ok(serde_json::from_slice(&data)?)
    }

    fn save_doc(&self, doc: &mut DocMeta) -> Result<()> {
        doc.updated_ms = now_ms();
        atomic_write_json(&self.doc_dir(&doc.id).join("meta.json"), doc)
    }

    // ---------- 文档管理 ----------

    pub fn create_doc(&self, title: &str) -> Result<DocMeta> {
        let doc = DocMeta {
            id: new_id(),
            title: if title.trim().is_empty() {
                format!("扫描 {}", chrono_like(now_ms()))
            } else {
                title.trim().to_string()
            },
            created_ms: now_ms(),
            updated_ms: now_ms(),
            tags: Vec::new(),
            pages: Vec::new(),
        };
        fs::create_dir_all(self.doc_dir(&doc.id).join("pages"))?;
        atomic_write_json(&self.doc_dir(&doc.id).join("meta.json"), &doc)?;
        Ok(doc)
    }

    pub fn list_docs(&self) -> Result<Vec<DocSummary>> {
        let mut out = Vec::new();
        let docs_dir = self.root.join("docs");
        for entry in fs::read_dir(&docs_dir)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            let meta_path = entry.path().join("meta.json");
            let Ok(data) = fs::read(&meta_path) else { continue };
            let Ok(doc) = serde_json::from_slice::<DocMeta>(&data) else { continue };
            let cover = doc.pages.first().map(|p| {
                let page_dir = self.page_dir(&doc.id, &p.id);
                let thumb_path = page_dir.join("thumb.jpg");
                if thumb_path.exists() {
                    thumb_path
                } else {
                    let original_path = page_dir.join("orig.jpg");
                    if original_path.exists() {
                        original_path
                    } else {
                        source_image_path(&page_dir).unwrap_or(original_path)
                    }
                }
                .to_string_lossy()
                .to_string()
            });
            out.push(DocSummary {
                id: doc.id,
                title: doc.title,
                updated_ms: doc.updated_ms,
                page_count: doc.pages.len(),
                cover,
                tags: doc.tags,
            });
        }
        out.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        Ok(out)
    }

    pub fn search_docs(&self, query: &str) -> Result<Vec<DocSummary>> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return self.list_docs();
        }
        let all = self.list_docs()?;
        let mut out = Vec::new();
        for s in all {
            let mut hit = s.title.to_lowercase().contains(&q)
                || s.tags.iter().any(|t| t.to_lowercase().contains(&q));
            if !hit {
                // 搜 OCR 文本
                if let Ok(doc) = self.load_doc(&s.id) {
                    for p in &doc.pages {
                        let f = self.page_dir(&doc.id, &p.id).join("ocr.txt");
                        if let Ok(txt) = fs::read_to_string(&f) {
                            if txt.to_lowercase().contains(&q) {
                                hit = true;
                                break;
                            }
                        }
                    }
                }
            }
            if hit {
                out.push(s);
            }
        }
        Ok(out)
    }

    pub fn get_doc(&self, doc_id: &str) -> Result<DocMeta> {
        self.load_doc(doc_id)
    }

    pub fn rename_doc(&self, doc_id: &str, title: &str) -> Result<()> {
        let mut doc = self.load_doc(doc_id)?;
        doc.title = title.trim().to_string();
        self.save_doc(&mut doc)
    }

    pub fn set_tags(&self, doc_id: &str, tags: Vec<String>) -> Result<()> {
        let mut doc = self.load_doc(doc_id)?;
        doc.tags = tags;
        self.save_doc(&mut doc)
    }

    pub fn delete_doc(&self, doc_id: &str) -> Result<()> {
        let dir = self.doc_dir(doc_id);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    // ---------- 页面处理 ----------

    /// 同步导入一页，供非交互调用和测试使用。
    pub fn add_page(&self, doc_id: &str, bytes: &[u8]) -> Result<PageInfo> {
        let pending = self.add_page_pending(doc_id, bytes)?;
        self.process_pending_page(doc_id, &pending.meta.id)
    }

    /// 快速保存原图，耗时检测与派生图生成留给后台任务。
    pub fn add_page_pending(&self, doc_id: &str, bytes: &[u8]) -> Result<PageInfo> {
        let total_start = Instant::now();
        let format = image::guess_format(bytes).context("无法识别图片格式")?;
        let extension = match format {
            image::ImageFormat::Jpeg => "jpg",
            image::ImageFormat::Png => "png",
            _ => bail!("不支持的图片格式"),
        };
        let (raw_width, raw_height) = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .context("无法读取图片尺寸")?
            .into_dimensions()
            .context("无法读取图片尺寸")?;
        let orientation = exif_orientation(bytes);
        let (width, height) = if (5..=8).contains(&orientation) {
            (raw_height, raw_width)
        } else {
            (raw_width, raw_height)
        };

        let mut doc = self.load_doc(doc_id)?;
        let page_id = new_id();
        let pdir = self.page_dir(doc_id, &page_id);
        fs::create_dir_all(&pdir)?;

        let stage_start = Instant::now();
        let source_path = pdir.join(format!("source.{extension}"));
        fs::write(&source_path, bytes)
            .with_context(|| format!("写入失败: {}", source_path.display()))?;
        let raw_write_ms = stage_start.elapsed().as_millis();

        let stage_start = Instant::now();
        let meta = PageMeta {
            id: page_id.clone(),
            filter: "original".into(),
            quad: None,
            rotation: 0,
            width,
            height,
            ocr_done: false,
            processing: true,
            detection_confidence: None,
            sharpness: None,
        };
        doc.pages.push(meta.clone());
        self.save_doc(&mut doc)?;
        let metadata_ms = stage_start.elapsed().as_millis();

        let mut info = self.page_info(doc_id, &page_id)?;
        info.save_timings_ms = Some(PageSaveTimings {
            raw_write: raw_write_ms as u64,
            decode: 0,
            orientation_and_rgb: 0,
            resize: 0,
            original_encode_and_write: 0,
            preview: 0,
            work_copy: 0,
            thumbnail: 0,
            metadata: metadata_ms as u64,
            total: total_start.elapsed().as_millis() as u64,
        });
        eprintln!(
            "[ScanKing timing] save doc={} page={} bytes={} image={}x{} raw_write={}ms metadata={}ms total={}ms",
            doc_id,
            page_id,
            bytes.len(),
            width,
            height,
            raw_write_ms,
            metadata_ms,
            total_start.elapsed().as_millis()
        );
        Ok(info)
    }

    pub fn process_pending_page(&self, doc_id: &str, page_id: &str) -> Result<PageInfo> {
        let total_start = Instant::now();
        let page_dir = self.page_dir(doc_id, page_id);
        let orig_path = page_dir.join("orig.jpg");
        let (original, decode_ms, orientation_rgb_ms, resize_ms, original_save_ms) =
            if let Some(source_path) = source_image_path(&page_dir) {
                let stage_start = Instant::now();
                let bytes = fs::read(&source_path)
                    .with_context(|| format!("原图丢失: {}", source_path.display()))?;
                let decoded = image::load_from_memory(&bytes).context("无法解码图片")?;
                let decode_ms = stage_start.elapsed().as_millis();

                let stage_start = Instant::now();
                let mut original = apply_exif_orientation(decoded, &bytes).to_rgb8();
                let orientation_rgb_ms = stage_start.elapsed().as_millis();

                let stage_start = Instant::now();
                if original.width().max(original.height()) > MAX_IMPORT_SIDE {
                    let scale = MAX_IMPORT_SIDE as f32
                        / original.width().max(original.height()) as f32;
                    original = imageops::resize(
                        &original,
                        (original.width() as f32 * scale) as u32,
                        (original.height() as f32 * scale) as u32,
                        imageops::FilterType::Triangle,
                    );
                }
                let resize_ms = stage_start.elapsed().as_millis();

                let stage_start = Instant::now();
                let encoded = encode_jpeg(&original, JPEG_Q_ORIG)?;
                fs::write(&orig_path, encoded)
                    .with_context(|| format!("写入失败: {}", orig_path.display()))?;
                let original_save_ms = stage_start.elapsed().as_millis();
                fs::remove_file(&source_path)
                    .with_context(|| format!("删除临时原图失败: {}", source_path.display()))?;
                (original, decode_ms, orientation_rgb_ms, resize_ms, original_save_ms)
            } else {
                let stage_start = Instant::now();
                let original = image::open(&orig_path)
                    .with_context(|| format!("原图丢失: {}", orig_path.display()))?
                    .to_rgb8();
                (original, stage_start.elapsed().as_millis(), 0, 0, 0)
            };
        let stage_start = Instant::now();
        let detection = detect::detect_quad_with_confidence(&original);
        let detection_ms = stage_start.elapsed().as_millis();

        // 失焦门控：拍完立刻评估清晰度，模糊输入提前暴露
        let stage_start = Instant::now();
        let sharpness = quality::laplacian_variance(&original);
        let quality_ms = stage_start.elapsed().as_millis();

        let stage_start = Instant::now();
        let mut doc = self.load_doc(doc_id)?;
        let meta = doc
            .pages
            .iter_mut()
            .find(|page| page.id == page_id)
            .ok_or_else(|| anyhow!("待处理页面已不存在"))?;
        if !meta.processing {
            return self.page_info(doc_id, page_id);
        }
        meta.width = original.width();
        meta.height = original.height();
        meta.quad = Some(detection.quad);
        meta.detection_confidence = Some(detection.confidence);
        meta.sharpness = Some(sharpness);
        self.save_doc(&mut doc)?;
        let metadata_ms = stage_start.elapsed().as_millis();

        let stage_start = Instant::now();
        let preview = shrink(&original, PREVIEW_SIDE);
        save_jpeg(&preview, &self.page_dir(doc_id, page_id).join("preview.jpg"), 82)?;
        let preview_ms = stage_start.elapsed().as_millis();

        let stage_start = Instant::now();
        self.reprocess_with_original(doc_id, page_id, true, Some(original))?;
        let warp_filter_ms = stage_start.elapsed().as_millis();

        let mut doc = self.load_doc(doc_id)?;
        let meta = doc
            .pages
            .iter_mut()
            .find(|page| page.id == page_id)
            .ok_or_else(|| anyhow!("待处理页面已不存在"))?;
        meta.processing = false;
        self.save_doc(&mut doc)?;
        let mut info = self.page_info(doc_id, page_id)?;
        info.processing_timings_ms = Some(PageProcessingTimings {
            original_decode: decode_ms as u64,
            orientation_and_rgb: orientation_rgb_ms as u64,
            resize: resize_ms as u64,
            original_encode_and_write: original_save_ms as u64,
            edge_detection: detection_ms as u64,
            quality_check: quality_ms as u64,
            preview: preview_ms as u64,
            warp_filter_thumbnail: warp_filter_ms as u64,
            metadata: metadata_ms as u64,
            total: total_start.elapsed().as_millis() as u64,
        });
        eprintln!(
            "[ScanKing timing] process doc={} page={} decode={}ms exif_rgb={}ms resize={}ms original_encode_write={}ms detect={}ms metadata={}ms preview={}ms warp_filter_thumb={}ms total={}ms",
            doc_id,
            page_id,
            decode_ms,
            orientation_rgb_ms,
            resize_ms,
            original_save_ms,
            detection_ms,
            metadata_ms,
            preview_ms,
            warp_filter_ms,
            total_start.elapsed().as_millis()
        );
        Ok(info)
    }

    pub fn mark_page_processing_failed(&self, doc_id: &str, page_id: &str) -> Result<()> {
        let mut doc = self.load_doc(doc_id)?;
        if let Some(meta) = doc.pages.iter_mut().find(|page| page.id == page_id) {
            meta.processing = false;
            meta.detection_confidence = Some(0.0);
            self.save_doc(&mut doc)?;
        }
        Ok(())
    }

    /// 重新执行 矫正(可选) → 旋转 → 滤镜 管线
    fn reprocess(&self, doc_id: &str, page_id: &str, rewarp: bool) -> Result<()> {
        self.reprocess_with_original(doc_id, page_id, rewarp, None)
    }

    fn reprocess_with_original(
        &self,
        doc_id: &str,
        page_id: &str,
        rewarp: bool,
        original: Option<RgbImage>,
    ) -> Result<()> {
        let doc = self.load_doc(doc_id)?;
        let meta = doc
            .pages
            .iter()
            .find(|p| p.id == page_id)
            .ok_or_else(|| anyhow!("页面不存在"))?
            .clone();
        let pdir = self.page_dir(doc_id, page_id);

        let warped_path = pdir.join("warped.jpg");
        let warped: RgbImage = if rewarp || !warped_path.exists() {
            let orig = match original {
                Some(image) => image,
                None => image::open(pdir.join("orig.jpg"))
                    .context("原图丢失")?
                    .to_rgb8(),
            };
            let w = match meta.quad {
                Some(q) => {
                    let quad: [Pt; 4] = [q[0], q[1], q[2], q[3]];
                    warp_quad(&orig, quad)
                }
                None => orig,
            };
            save_jpeg(&w, &warped_path, JPEG_Q_ORIG)?;
            w
        } else {
            image::open(&warped_path)?.to_rgb8()
        };

        let rotated = match meta.rotation % 4 {
            1 => imageops::rotate90(&warped),
            2 => imageops::rotate180(&warped),
            3 => imageops::rotate270(&warped),
            _ => warped,
        };
        let filtered = apply_filter(&rotated, Filter::from_name(&meta.filter));
        save_jpeg(&filtered, &pdir.join("work.jpg"), JPEG_Q_WORK)?;
        let thumb = shrink(&filtered, THUMB_SIDE);
        save_jpeg(&thumb, &pdir.join("thumb.jpg"), 78)?;
        Ok(())
    }

    pub fn page_info(&self, doc_id: &str, page_id: &str) -> Result<PageInfo> {
        let doc = self.load_doc(doc_id)?;
        let meta = doc
            .pages
            .iter()
            .find(|p| p.id == page_id)
            .ok_or_else(|| anyhow!("页面不存在"))?
            .clone();
        let pdir = self.page_dir(doc_id, page_id);
        let original_path = pdir.join("orig.jpg");
        let displayed_original = if original_path.exists() {
            original_path.clone()
        } else {
            source_image_path(&pdir).unwrap_or(original_path)
        };
        let preview_path = pdir.join("preview.jpg");
        let work_path = pdir.join("work.jpg");
        let thumb_path = pdir.join("thumb.jpg");
        let displayed_preview = if preview_path.exists() { preview_path } else { displayed_original.clone() };
        let displayed_work = if work_path.exists() { work_path } else { displayed_original.clone() };
        let displayed_thumb = if thumb_path.exists() { thumb_path } else { displayed_original.clone() };
        let (ww, wh) = image::image_dimensions(&displayed_work).unwrap_or((0, 0));
        let quad = meta.quad.unwrap_or_else(|| detect::full_quad(meta.width, meta.height));
        let preview_scale = {
            let (pw, _) = image::image_dimensions(&displayed_preview).unwrap_or((meta.width, meta.height));
            pw as f32 / meta.width as f32
        };
        Ok(PageInfo {
            preview: displayed_preview.to_string_lossy().to_string(),
            work: displayed_work.to_string_lossy().to_string(),
            thumb: displayed_thumb.to_string_lossy().to_string(),
            work_width: ww,
            work_height: wh,
            quad,
            preview_scale,
            detection_confidence: meta.detection_confidence,
            save_timings_ms: None,
            processing_timings_ms: None,
            meta,
        })
    }

    /// 重新自动检测四角（不落盘，仅返回结果）
    pub fn detect_page(&self, doc_id: &str, page_id: &str) -> Result<[[f32; 2]; 4]> {
        let pdir = self.page_dir(doc_id, page_id);
        let orig = image::open(pdir.join("orig.jpg"))?.to_rgb8();
        Ok(detect::detect_quad(&orig))
    }

    /// 设置四角与旋转并重新处理
    pub fn set_geometry(
        &self,
        doc_id: &str,
        page_id: &str,
        quad: Option<[[f32; 2]; 4]>,
        rotation: u32,
    ) -> Result<PageInfo> {
        let mut doc = self.load_doc(doc_id)?;
        {
            let meta = doc
                .pages
                .iter_mut()
                .find(|p| p.id == page_id)
                .ok_or_else(|| anyhow!("页面不存在"))?;
            meta.quad = quad;
            meta.rotation = rotation % 4;
            meta.ocr_done = false;
        }
        self.save_doc(&mut doc)?;
        self.reprocess(doc_id, page_id, true)?;
        let _ = fs::remove_file(self.page_dir(doc_id, page_id).join("ocr.txt"));
        self.page_info(doc_id, page_id)
    }

    /// 只换滤镜（用 warped.jpg 缓存，速度快）
    pub fn set_filter(&self, doc_id: &str, page_id: &str, filter: &str) -> Result<PageInfo> {
        let mut doc = self.load_doc(doc_id)?;
        {
            let meta = doc
                .pages
                .iter_mut()
                .find(|p| p.id == page_id)
                .ok_or_else(|| anyhow!("页面不存在"))?;
            meta.filter = filter.to_string();
        }
        self.save_doc(&mut doc)?;
        self.reprocess(doc_id, page_id, false)?;
        self.page_info(doc_id, page_id)
    }

    pub fn delete_page(&self, doc_id: &str, page_id: &str) -> Result<()> {
        let mut doc = self.load_doc(doc_id)?;
        doc.pages.retain(|p| p.id != page_id);
        self.save_doc(&mut doc)?;
        let dir = self.page_dir(doc_id, page_id);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    pub fn reorder_pages(&self, doc_id: &str, page_ids: &[String]) -> Result<()> {
        let mut doc = self.load_doc(doc_id)?;
        let mut new_pages = Vec::with_capacity(doc.pages.len());
        for id in page_ids {
            if let Some(p) = doc.pages.iter().find(|p| &p.id == id) {
                new_pages.push(p.clone());
            }
        }
        for p in &doc.pages {
            if !page_ids.contains(&p.id) {
                new_pages.push(p.clone());
            }
        }
        if new_pages.len() != doc.pages.len() {
            bail!("页面列表不一致");
        }
        doc.pages = new_pages;
        self.save_doc(&mut doc)
    }

    // ---------- OCR 文本 ----------

    pub fn save_ocr_text(&self, doc_id: &str, page_id: &str, text: &str) -> Result<()> {
        let mut doc = self.load_doc(doc_id)?;
        {
            let meta = doc
                .pages
                .iter_mut()
                .find(|p| p.id == page_id)
                .ok_or_else(|| anyhow!("页面不存在"))?;
            meta.ocr_done = true;
        }
        self.save_doc(&mut doc)?;
        fs::write(self.page_dir(doc_id, page_id).join("ocr.txt"), text)?;
        Ok(())
    }

    pub fn get_ocr_text(&self, doc_id: &str, page_id: &str) -> Option<String> {
        fs::read_to_string(self.page_dir(doc_id, page_id).join("ocr.txt")).ok()
    }

    pub fn doc_ocr_text(&self, doc_id: &str) -> Result<String> {
        let doc = self.load_doc(doc_id)?;
        let mut out = String::new();
        for (i, p) in doc.pages.iter().enumerate() {
            if let Some(t) = self.get_ocr_text(doc_id, &p.id) {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&format!("—— 第 {} 页 ——\n{}", i + 1, t));
            }
        }
        Ok(out)
    }

    pub fn work_image_path(&self, doc_id: &str, page_id: &str) -> PathBuf {
        self.page_dir(doc_id, page_id).join("work.jpg")
    }

    pub fn load_work_image(&self, doc_id: &str, page_id: &str) -> Result<RgbImage> {
        Ok(image::open(self.work_image_path(doc_id, page_id))?.to_rgb8())
    }

    // ---------- 导出 ----------

    pub fn export_pdf(&self, doc_id: &str, mode: &str, quality: u8) -> Result<PathBuf> {
        let doc = self.load_doc(doc_id)?;
        if doc.pages.is_empty() {
            bail!("文档没有页面");
        }
        let mut pages = Vec::new();
        for p in &doc.pages {
            let path = self.page_dir(doc_id, &p.id).join("work.jpg");
            let quality = quality.clamp(30, 95);
            // 质量与已存 work.jpg 不同时重新编码
            let (jpeg, w, h) = if quality >= JPEG_Q_WORK {
                let bytes = fs::read(&path)?;
                let (w, h) = image::image_dimensions(&path)?;
                (bytes, w, h)
            } else {
                let img = image::open(&path)?.to_rgb8();
                let mut out = Vec::new();
                let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
                enc.encode_image(&img)?;
                let (w, h) = (img.width(), img.height());
                (out, w, h)
            };
            pages.push(PdfPage { jpeg, width: w, height: h });
        }
        let pdf = build_pdf(&pages, PageMode::from_name(mode))?;
        let safe_title: String = doc
            .title
            .chars()
            .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
            .collect();
        let path = self.root.join("exports").join(format!("{}_{}.pdf", safe_title, &doc.id[..6]));
        fs::write(&path, pdf)?;
        Ok(path)
    }

    /// 证件拼页：把两页合成到一张 A4 上，作为新页面追加
    pub fn idcard_compose(&self, doc_id: &str, page_a: &str, page_b: &str) -> Result<PageInfo> {
        let a = image::open(self.work_image_path(doc_id, page_a))?.to_rgb8();
        let b = image::open(self.work_image_path(doc_id, page_b))?.to_rgb8();
        let composed = idcard::compose(&a, &b);

        let mut doc = self.load_doc(doc_id)?;
        let page_id = new_id();
        let pdir = self.page_dir(doc_id, &page_id);
        fs::create_dir_all(&pdir)?;
        save_jpeg(&composed, &pdir.join("orig.jpg"), JPEG_Q_ORIG)?;
        let preview = shrink(&composed, PREVIEW_SIDE);
        save_jpeg(&preview, &pdir.join("preview.jpg"), 82)?;
        let meta = PageMeta {
            id: page_id.clone(),
            filter: "original".into(),
            quad: None,
            rotation: 0,
            width: composed.width(),
            height: composed.height(),
            ocr_done: false,
            processing: false,
            detection_confidence: None,
            sharpness: None,
        };
        doc.pages.push(meta);
        self.save_doc(&mut doc)?;
        self.reprocess(doc_id, &page_id, true)?;
        self.page_info(doc_id, &page_id)
    }
}

fn shrink(img: &RgbImage, max_side: u32) -> RgbImage {
    let (w, h) = (img.width(), img.height());
    if w.max(h) <= max_side {
        return img.clone();
    }
    let s = max_side as f32 / w.max(h) as f32;
    imageops::resize(img, (w as f32 * s) as u32, (h as f32 * s) as u32, imageops::FilterType::Triangle)
}

/// 简易时间格式（避免引入 chrono）：UTC+8 的 "MMDD-HHMM"
fn chrono_like(ms: u64) -> String {
    let secs = ms / 1000 + 8 * 3600;
    let days = secs / 86400;
    let (h, m) = ((secs % 86400) / 3600, (secs % 3600) / 60);
    // 从 1970-01-01 起算的简化公历
    let mut year = 1970u64;
    let mut d = days;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let yd = if leap { 366 } else { 365 };
        if d < yd {
            break;
        }
        d -= yd;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let months = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 1;
    for len in months {
        if d < len {
            break;
        }
        d -= len;
        month += 1;
    }
    format!("{:02}{:02}-{:02}{:02}", month, d + 1, h, m)
}
