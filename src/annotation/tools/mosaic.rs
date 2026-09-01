//! 马赛克 / 遮挡标注工具（像素化 / 模糊 / 纯色）。
//!
//! - 像素化：分块均值（已验证，匿名性高，极快）
//! - 模糊：`image::imageops::blur` 高斯近似（中等性能，匿名性最好）
//! - 纯色：不透明填充（极快，匿名性最好）

use crate::annotation::Color;
use crate::utils::math::Rect;

/// 对图像指定矩形区域做马赛克（像素化，兼容旧 `block_size` 调用）。
pub fn draw_mosaic(img: &mut image::RgbaImage, rect: Rect, block_size: u32) {
    draw_pixelate(img, rect, block_size)
}

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

/// 高斯模糊（`image::imageops::blur`，sigma≈半径）。
pub fn draw_blur(img: &mut image::RgbaImage, rect: Rect, radius: f32) {
    let x0 = rect.x.clamp(0, img.width() as i32);
    let y0 = rect.y.clamp(0, img.height() as i32);
    let x1 = rect.right().clamp(0, img.width() as i32);
    let y1 = rect.bottom().clamp(0, img.height() as i32);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let w = (x1 - x0) as u32;
    let h = (y1 - y0) as u32;
    let mut patch = image::imageops::crop_imm(img, x0 as u32, y0 as u32, w, h).to_image();
    // image::imageops::blur 内部对 alpha 也做卷积，适合遮挡
    let blurred = image::imageops::blur(&patch, radius.max(1.0));
    for y in 0..h {
        for x in 0..w {
            *img.get_pixel_mut((x0 as u32) + x, (y0 as u32) + y) = *blurred.get_pixel(x, y);
            let _ = &mut patch;
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
        draw_mosaic(&mut img, Rect { x: 0, y: 0, width: 8, height: 8 }, 4);
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
        draw_mosaic(&mut img, Rect { x: -4, y: -4, width: 8, height: 8 }, 4);
        // 不应 panic
        draw_mosaic(&mut img, Rect { x: 100, y: 100, width: 8, height: 8 }, 4);
    }

    #[test]
    fn blur_changes_pixels() {
        let mut img = test_img();
        let before = img.get_pixel(4, 4).0;
        draw_blur(&mut img, Rect { x: 0, y: 0, width: 8, height: 8 }, 5.0);
        assert_ne!(img.get_pixel(4, 4).0, before);
        assert_eq!(img.get_pixel(12, 12).0[0], 192); // 块外不变
    }

    #[test]
    fn solid_fills() {
        let mut img = test_img();
        draw_solid(&mut img, Rect { x: 0, y: 0, width: 4, height: 4 }, Color::BLACK);
        assert_eq!(img.get_pixel(1, 1).0, [0, 0, 0, 255]);
        assert_eq!(img.get_pixel(5, 5).0[0], 80);
    }
}
