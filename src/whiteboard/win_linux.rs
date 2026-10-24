use hbb_common::{bail, ResultType};
use tiny_skia::{FillRule, Paint, PathBuilder, PixmapMut, Point, Rect, Transform};
use ttf_parser::Face;
// 用于桥接 `ttf-parser` 与 `tiny-skia` 的辅助结构体。
struct PathBuilderWrapper<'a> {
    path_builder: &'a mut PathBuilder,
    transform: Transform,
}

impl ttf_parser::OutlineBuilder for PathBuilderWrapper<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let mut pt = Point::from_xy(x, y);
        self.transform.map_point(&mut pt);
        self.path_builder.move_to(pt.x, pt.y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let mut pt = Point::from_xy(x, y);
        self.transform.map_point(&mut pt);
        self.path_builder.line_to(pt.x, pt.y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let mut pt1 = Point::from_xy(x1, y1);
        self.transform.map_point(&mut pt1);
        let mut pt = Point::from_xy(x, y);
        self.transform.map_point(&mut pt);
        self.path_builder.quad_to(pt1.x, pt1.y, pt.x, pt.y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let mut pt1 = Point::from_xy(x1, y1);
        self.transform.map_point(&mut pt1);
        let mut pt2 = Point::from_xy(x2, y2);
        self.transform.map_point(&mut pt2);
        let mut pt = Point::from_xy(x, y);
        self.transform.map_point(&mut pt);
        self.path_builder
            .cubic_to(pt1.x, pt1.y, pt2.x, pt2.y, pt.x, pt.y);
    }

    fn close(&mut self) {
        self.path_builder.close();
    }
}

// 把一段文本连同白色背景矩形绘制到 pixmap 上。
pub(super) fn draw_text(
    pixmap: &mut PixmapMut,
    face: &Face,
    text: &str,
    x: f32,
    y: f32,
    paint: &Paint,
    font_size: f32,
) {
    let units_per_em = face.units_per_em() as f32;
    let scale = font_size / units_per_em;

    // --- 1. 计算背景所需的文本尺寸 ---
    let mut total_width = 0.0;
    for ch in text.chars() {
        let glyph_id = face.glyph_index(ch).unwrap_or_default();
        if let Some(h_advance) = face.glyph_hor_advance(glyph_id) {
            total_width += h_advance as f32 * scale;
        }
    }

    // 使用字体度量以获得一致的背景高度。
    let font_height = (face.ascender() - face.descender()) as f32 * scale;
    let ascent = face.ascender() as f32 * scale;
    // 在文本周围加一些内边距
    let padding = 3.0;

    let mut bg_filled = false;
    // --- 2. 绘制白色背景矩形 ---
    if let Some(bg_rect) = Rect::from_xywh(
        x - padding,
        y - ascent - padding,
        total_width + 2.0 * padding,
        font_height + 2.0 * padding,
    ) {
        // 圆角半径
        let radius = 5.0;
        let path = {
            let mut pb = PathBuilder::new();
            let r_x = bg_rect.x();
            let r_y = bg_rect.y();
            let r_w = bg_rect.width();
            let r_h = bg_rect.height();
            pb.move_to(r_x + radius, r_y);
            pb.line_to(r_x + r_w - radius, r_y);
            pb.quad_to(r_x + r_w, r_y, r_x + r_w, r_y + radius);
            pb.line_to(r_x + r_w, r_y + r_h - radius);
            pb.quad_to(r_x + r_w, r_y + r_h, r_x + r_w - radius, r_y + r_h);
            pb.line_to(r_x + radius, r_y + r_h);
            pb.quad_to(r_x, r_y + r_h, r_x, r_y + r_h - radius);
            pb.line_to(r_x, r_y + radius);
            pb.quad_to(r_x, r_y, r_x + radius, r_y);
            pb.close();
            pb.finish()
        };

        if let Some(path) = path {
            let mut bg_paint = Paint::default();
            bg_paint.set_color_rgba8(255, 255, 255, 255);
            bg_paint.anti_alias = true;
            pixmap.fill_path(
                &path,
                &bg_paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
            bg_filled = true;
        }
    }

    // --- 3. 绘制文本 ---
    let transform = Transform::from_translate(x, y).pre_scale(scale, -scale);
    let mut path_builder = PathBuilder::new();
    let mut current_x = 0.0;

    for ch in text.chars() {
        let glyph_id = face.glyph_index(ch).unwrap_or_default();

        let mut builder = PathBuilderWrapper {
            path_builder: &mut path_builder,
            transform: transform.post_translate(current_x, 0.0),
        };

        face.outline_glyph(glyph_id, &mut builder);

        if let Some(h_advance) = face.glyph_hor_advance(glyph_id) {
            current_x += h_advance as f32 * scale;
        }
    }

    if let Some(path) = path_builder.finish() {
        if bg_filled {
            let mut text_paint = Paint::default();
            text_paint.set_color_rgba8(0, 0, 0, 255);
            text_paint.anti_alias = true;
            pixmap.fill_path(
                &path,
                &text_paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        } else {
            pixmap.fill_path(&path, paint, FillRule::Winding, Transform::identity(), None);
        }
    }
}

pub(super) fn create_font_face() -> ResultType<Face<'static>> {
    let mut font_db = fontdb::Database::new();
    font_db.load_system_fonts();
    let query = fontdb::Query {
        families: &[fontdb::Family::Monospace, fontdb::Family::SansSerif],
        ..fontdb::Query::default()
    };
    let Some(font_id) = font_db.query(&query) else {
        bail!("No monospace or sans-serif font found!");
    };
    let Some((font_source, face_index)) = font_db.face_source(font_id) else {
        bail!("No face found for font!");
    };
    // 把字体数据载入一个静态切片，以满足 `ttf-parser` 的生命周期要求。
    // 我们用 `Box::leak` 故意泄漏这块内存，这里这样做是可以接受的，因为字体数据
    // 在应用的整个生命周期内都需要它。
    let font_data: &'static [u8] = Box::leak(match font_source {
        fontdb::Source::File(path) => std::fs::read(path)?.into_boxed_slice(),
        fontdb::Source::Binary(data) => data.as_ref().as_ref().to_vec().into_boxed_slice(),
        fontdb::Source::SharedFile(path, _) => std::fs::read(path)?.into_boxed_slice(),
    });
    let face = Face::parse(font_data, face_index)?;
    Ok(face)
}
