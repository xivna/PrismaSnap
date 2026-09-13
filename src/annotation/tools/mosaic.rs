//! 马赛克 / 遮挡标注工具（像素化 / 模糊 / 纯色）。
//!
//! - 像素化：分块均值（已验证，匿名性高，极快）
//! - 模糊：`libblur::stack_blur` O(1) 近似高斯（与预览一致，匿名性最好，`Channels4`）
//! - 纯色：不透明填充（极快，匿名性最好）

use libblur::{stack_blur, FastBlurChannels, ThreadingPolicy};

use crate::annotation::Color;
use crate::utils::math::Rect;

/// 像素化（块均值）。
pub fn draw_pixelate(img: &mut image::RgbaImage, rect: Rect, block_size: u32) {
    let bs = block_size.max(2) as i32;
    let x0 = rect.x.clamp(0, img.width() as i32);
    let y0 = rect.y.clamp(0, img.height() as i32);
    let x1 = rect.right().clamp(0, img.width() as i32);
    let y1 = rect.bottom().clamp(0, img.height() as i32);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    // 先拷出区域原始像素（避免块间污染）
    let w = (x1 - x0) as u32;
    let h = (y1 - y0) as u32;
    let mut buf = image::RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            *buf.get_pixel_mut(x, y) = *img.get_pixel(x0 as u32 + x, y0 as u32 + y);
        }
    }
    for by in (y0..y1).step_by(bs as usize) {
        for bx in (x0..x1).step_by(bs as usize) {
            let bx1 = (bx + bs).min(x1);
            let by1 = (by + bs).min(y1);
            // 块均值（避免左上角取色导致的条纹感）
            let mut r_sum: u32 = 0;
            let mut g_sum: u32 = 0;
            let mut b_sum: u32 = 0;
            let mut a_sum: u32 = 0;
            let mut cnt: u32 = 0;
            for py in by..by1 {
                for px in bx..bx1 {
                    let p = buf.get_pixel((px - x0) as u32, (py - y0) as u32).0;
                    r_sum += p[0] as u32;
                    g_sum += p[1] as u32;
                    b_sum += p[2] as u32;
                    a_sum += p[3] as u32;
                    cnt += 1;
                }
            }
            let col = image::Rgba([
                (r_sum / cnt) as u8,
                (g_sum / cnt) as u8,
                (b_sum / cnt) as u8,
                (a_sum / cnt) as u8,
            ]);
            for py in by..by1 {
                for px in bx..bx1 {
                    *img.get_pixel_mut(px as u32, py as u32) = col;
                }
            }
        }
    }
}

/// 模糊采样外扩区域（预览与导出共用）。
///
/// 模糊需要矩形外的真实像素参与边缘计算：先外扩 30%（至少 24px）再模糊、
/// 最后只写回原矩形。预览与导出都走这里，保证模糊边缘观感一致
/// （2026-09-12：导出此前直接对矩形裁剪模糊，边缘 clamp 复制与预览的
/// padded 结果不同，用户反馈"预览与保存不一致"）。
pub fn padded_rect_for_blur(rect: Rect, image: &image::RgbaImage) -> Rect {
    let pad_w = (rect.width as f32 * 0.3).max(24.0) as i32;
    let pad_h = (rect.height as f32 * 0.3).max(24.0) as i32;
    let x0 = (rect.x - pad_w).max(0);
    let y0 = (rect.y - pad_h).max(0);
    let x1 = (rect.right() + pad_w).min(image.width() as i32);
    let y1 = (rect.bottom() + pad_h).min(image.height() as i32);
    Rect::from_points(x0, y0, x1, y1)
}

/// 高斯模糊（`libblur::stack_blur` O(1) 近似，与预览一致）。
///
/// 外扩后模糊再裁回（见 [`padded_rect_for_blur`]），边缘与周围画面自然衔接。
pub fn draw_blur(img: &mut image::RgbaImage, rect: Rect, radius: f32) {
    let x0 = rect.x.clamp(0, img.width() as i32);
    let y0 = rect.y.clamp(0, img.height() as i32);
    let x1 = rect.right().clamp(0, img.width() as i32);
    let y1 = rect.bottom().clamp(0, img.height() as i32);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let clipped = Rect::from_points(x0, y0, x1, y1);
    let padded = padded_rect_for_blur(clipped, img);
    if padded.width < 2 || padded.height < 2 {
        return;
    }
    let mut patch = image::imageops::crop_imm(
        img,
        padded.x as u32,
        padded.y as u32,
        padded.width,
        padded.height,
    )
    .to_image();
    // StackBlur 近似高斯，O(1)/px，视觉与高斯几乎一致，Channels4 含 alpha
    let r = radius.max(1.0) as u32;
    stack_blur(
        patch.as_mut(),
        padded.width * 4,
        padded.width,
        padded.height,
        r.clamp(2, 254),
        FastBlurChannels::Channels4,
        ThreadingPolicy::Single,
    );
    let dx = (x0 - padded.x) as u32;
    let dy = (y0 - padded.y) as u32;
    for y in 0..(y1 - y0) as u32 {
        for x in 0..(x1 - x0) as u32 {
            *img.get_pixel_mut(x0 as u32 + x, y0 as u32 + y) = *patch.get_pixel(dx + x, dy + y);
        }
    }
}

/// 纯色遮挡（不透明填充）。
pub fn draw_solid(img: &mut image::RgbaImage, rect: Rect, color: Color) {
    let x0 = rect.x.clamp(0, img.width() as i32);
    let y0 = rect.y.clamp(0, img.height() as i32);
    let x1 = rect.right().clamp(0, img.width() as i32);
    let y1 = rect.bottom().clamp(0, img.height() as i32);
    for y in y0..y1 {
        for x in x0..x1 {
            *img.get_pixel_mut(x as u32, y as u32) = image::Rgba([color.r, color.g, color.b, 255]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::math::Rect;

    const W: u32 = 16;
    const H: u32 = 16;
    fn test_img() -> image::RgbaImage {
        let mut img = image::RgbaImage::new(W, H);
        for y in 0..H {
            for x in 0..W {
                let v = (x * 16) as u8;
                *img.get_pixel_mut(x, y) = image::Rgba([v, v, v, 255]);
            }
        }
        img
    }

    #[test]
    fn mosaic_blocks_fill() {
        let mut img = test_img();
        draw_pixelate(&mut img, Rect { x: 0, y: 0, width: 8, height: 8 }, 4);
        // 块均值：(0..4) 列灰度 0,16,32,48 均值 24
        assert_eq!(img.get_pixel(2, 2).0[0], 24);
        // 下一块 (4..8) 64,80,96,112 均值 88
        assert_eq!(img.get_pixel(6, 2).0[0], 88);
        // 块外不变
        assert_eq!(img.get_pixel(10, 10).0[0], 160);
    }

    #[test]
    fn out_of_bounds_safe() {
        let mut img = test_img();
        draw_pixelate(&mut img, Rect { x: -4, y: -4, width: 8, height: 8 }, 4);
        // 不应 panic
        draw_pixelate(&mut img, Rect { x: 100, y: 100, width: 8, height: 8 }, 4);
    }

    #[test]
    fn blur_changes_pixels() {
        // rect 内部实心 200、外部 100：外扩模糊后内部靠近边缘处应被外侧像素拉低
        let mut img = image::RgbaImage::from_pixel(16, 16, image::Rgba([100, 100, 100, 255]));
        for y in 0..8 {
            for x in 0..8 {
                *img.get_pixel_mut(x, y) = image::Rgba([200, 200, 200, 255]);
            }
        }
        draw_blur(&mut img, Rect { x: 0, y: 0, width: 8, height: 8 }, 5.0);
        let inside = img.get_pixel(5, 5).0[0];
        assert!(inside < 200 && inside > 100, "内部应被外扩像素拉低，实际 {inside}");
        assert_eq!(img.get_pixel(12, 12).0[0], 100); // 块外不变
    }

    #[test]
    fn padded_rect_expands_with_min_24_and_clamps() {
        let img = image::RgbaImage::new(64, 64);
        // 小矩形 16..32：最小外扩 24px → -8..56，被图边界钳制为 0..56
        let p = padded_rect_for_blur(Rect { x: 16, y: 16, width: 16, height: 16 }, &img);
        assert_eq!((p.x, p.y, p.width, p.height), (0, 0, 56, 56));
        // 贴边矩形：外扩被图边界钳制
        let p2 = padded_rect_for_blur(Rect { x: 0, y: 0, width: 10, height: 10 }, &img);
        assert_eq!((p2.x, p2.y, p2.right(), p2.bottom()), (0, 0, 34, 34));
    }

    #[test]
    fn blur_edge_blends_outside_pixels() {
        // 左侧外部为黑、矩形内部为白：外扩模糊后靠近左边缘的像素
        // 应受外部黑色影响而变暗（旧"只裁矩形"实现边缘 clamp 复制白色，不会变暗）
        let mut img = image::RgbaImage::new(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                let v = if (24..40).contains(&x) { 255 } else { 0 };
                *img.get_pixel_mut(x, y) = image::Rgba([v, v, v, 255]);
            }
        }
        draw_blur(&mut img, Rect { x: 24, y: 24, width: 16, height: 16 }, 12.0);
        let left_edge = img.get_pixel(26, 32).0[0];
        let center = img.get_pixel(32, 32).0[0];
        assert!(left_edge < center, "左边缘 {left_edge} 应受外部黑色影响 < 中心 {center}");
    }

    #[test]
    fn solid_fills() {
        let mut img = test_img();
        draw_solid(&mut img, Rect { x: 0, y: 0, width: 4, height: 4 }, Color::BLACK);
        assert_eq!(img.get_pixel(1, 1).0, [0, 0, 0, 255]);
        assert_eq!(img.get_pixel(5, 5).0[0], 80);
    }
}
