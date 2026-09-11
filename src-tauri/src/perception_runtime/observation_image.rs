//! Bounded, in-memory temporal contact sheets. No screenshots are written to disk.
use std::{io::Cursor, time::Instant};

use base64::{engine::general_purpose::STANDARD, Engine};

const CANVAS_WIDTH: usize = 2304;
const LATEST_WIDTH: usize = 2304;
const LATEST_HEIGHT: usize = 1296;
const OLDEST_WIDTH: usize = 512;
const HISTORY_WIDTH_STEP: usize = 96;
const LABEL_HEIGHT: usize = 48;
const GAP: usize = 16;
const MAX_FRAMES: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Panel {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

struct DecodedImage {
    pixels: Vec<u8>,
    width: usize,
    height: usize,
    channels: usize,
}

pub fn compose(urls: &[String], captured: &[Instant], now: Instant) -> Result<Vec<String>, ()> {
    if urls.is_empty() {
        return Ok(Vec::new());
    }
    if urls.len() > MAX_FRAMES || captured.len() != urls.len() {
        return Err(());
    }

    let (panels, canvas_height) = layout(urls.len());
    let mut canvas = vec![0_u8; CANVAS_WIDTH * canvas_height * 3];
    let mut ages = Vec::with_capacity(urls.len());
    for (index, ((url, at), panel)) in urls.iter().zip(captured).zip(&panels).enumerate() {
        let age = now.saturating_duration_since(*at).as_secs();
        ages.push(age);
        let label = if age == 0 {
            "NOW".into()
        } else {
            format!("{age}s")
        };
        draw_label(
            &mut canvas,
            canvas_height,
            panel.x + 12,
            panel.y + 8,
            &label,
        );
        draw_image(&mut canvas, canvas_height, *panel, &decode_png(url)?)?;
        debug_assert_eq!(index == urls.len() - 1, panel.width == LATEST_WIDTH);
    }
    crate::dev_event!("observation.contact_sheet", "frames" => urls.len(), "ages_seconds" => ages,
        "width" => CANVAS_WIDTH, "height" => canvas_height);

    let mut encoded = Vec::new();
    {
        let mut encoder =
            png::Encoder::new(&mut encoded, CANVAS_WIDTH as u32, canvas_height as u32);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|_| ())?;
        writer.write_image_data(&canvas).map_err(|_| ())?;
    }
    Ok(vec![format!(
        "data:image/png;base64,{}",
        STANDARD.encode(encoded)
    )])
}

fn layout(count: usize) -> (Vec<Panel>, usize) {
    let history_count = count.saturating_sub(1);
    let mut panels = Vec::with_capacity(count);
    let mut x = 0;
    let mut y = 0;
    let mut row_height = 0;
    for index in 0..history_count {
        let width = OLDEST_WIDTH + index * HISTORY_WIDTH_STEP;
        let height = width * 9 / 16;
        if x > 0 && x + width > CANVAS_WIDTH {
            y += row_height + GAP;
            x = 0;
            row_height = 0;
        }
        panels.push(Panel {
            x,
            y,
            width,
            height,
        });
        x += width + GAP;
        row_height = row_height.max(LABEL_HEIGHT + height);
    }
    if history_count > 0 {
        y += row_height + GAP;
    }
    panels.push(Panel {
        x: 0,
        y,
        width: LATEST_WIDTH,
        height: LATEST_HEIGHT,
    });
    (panels, y + LABEL_HEIGHT + LATEST_HEIGHT)
}

fn decode_png(url: &str) -> Result<DecodedImage, ()> {
    let data = url.strip_prefix("data:image/png;base64,").ok_or(())?;
    if data.len() > 24 * 1024 * 1024 {
        return Err(());
    }
    let bytes = STANDARD.decode(data).map_err(|_| ())?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(());
    }
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|_| ())?;
    let info = reader.info();
    if info.width == 0 || info.height == 0 || info.width > 4096 || info.height > 4096 {
        return Err(());
    }
    let mut pixels = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut pixels).map_err(|_| ())?;
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => return Err(()),
    };
    Ok(DecodedImage {
        pixels,
        width: info.width as usize,
        height: info.height as usize,
        channels,
    })
}

fn draw_image(
    canvas: &mut [u8],
    canvas_height: usize,
    panel: Panel,
    image: &DecodedImage,
) -> Result<(), ()> {
    let scale =
        (panel.width as f64 / image.width as f64).min(panel.height as f64 / image.height as f64);
    let width = (image.width as f64 * scale).round() as usize;
    let height = (image.height as f64 * scale).round() as usize;
    let origin_x = panel.x + (panel.width - width) / 2;
    let origin_y = panel.y + LABEL_HEIGHT + (panel.height - height) / 2;
    if origin_x + width > CANVAS_WIDTH || origin_y + height > canvas_height {
        return Err(());
    }
    for row in 0..height {
        for col in 0..width {
            let source = ((row * image.height / height) * image.width + col * image.width / width)
                * image.channels;
            let target = ((origin_y + row) * CANVAS_WIDTH + origin_x + col) * 3;
            canvas[target..target + 3].copy_from_slice(&image.pixels[source..source + 3]);
        }
    }
    Ok(())
}

fn draw_label(canvas: &mut [u8], canvas_height: usize, x: usize, y: usize, label: &str) {
    for (index, ch) in label.chars().enumerate() {
        let glyph: [u8; 7] = match ch {
            '0' => [14, 17, 19, 21, 25, 17, 14],
            '1' => [4, 12, 4, 4, 4, 4, 14],
            '2' => [14, 17, 1, 2, 4, 8, 31],
            '3' => [30, 1, 1, 14, 1, 1, 30],
            '4' => [2, 6, 10, 18, 31, 2, 2],
            '5' => [31, 16, 16, 30, 1, 1, 30],
            '6' => [14, 16, 16, 30, 17, 17, 14],
            '7' => [31, 1, 2, 4, 8, 8, 8],
            '8' => [14, 17, 17, 14, 17, 17, 14],
            '9' => [14, 17, 17, 15, 1, 1, 14],
            's' => [0, 0, 15, 16, 14, 1, 30],
            'N' => [17, 25, 25, 21, 19, 19, 17],
            'O' => [14, 17, 17, 17, 17, 17, 14],
            'W' => [17, 17, 17, 21, 21, 21, 10],
            _ => [0; 7],
        };
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5 {
                if bits & (1 << (4 - col)) == 0 {
                    continue;
                }
                for dy in 0..4 {
                    for dx in 0..4 {
                        let px = x + index * 24 + col * 4 + dx;
                        let py = y + row * 4 + dy;
                        if px < CANVAS_WIDTH && py < canvas_height {
                            let offset = (py * CANVAS_WIDTH + px) * 3;
                            canvas[offset..offset + 3].fill(255);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn solid(color: [u8; 3]) -> String {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 16, 9);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&color.repeat(16 * 9))
                .unwrap();
        }
        format!("data:image/png;base64,{}", STANDARD.encode(bytes))
    }

    #[test]
    fn layouts_grow_from_oldest_to_latest_for_one_through_three_frames() {
        for count in 1..=3 {
            let (panels, height) = layout(count);
            assert_eq!(panels.len(), count);
            assert!(height >= LABEL_HEIGHT + LATEST_HEIGHT);
            assert_eq!(panels.last().unwrap().width, LATEST_WIDTH);
            assert!(panels.windows(2).all(|pair| pair[0].width < pair[1].width));
        }
    }

    #[test]
    fn three_dynamic_panels_render_in_order_with_actual_relative_labels() {
        let now = Instant::now();
        let colors = [[255, 0, 0], [0, 255, 0], [0, 0, 255]];
        let urls = colors.map(solid);
        let captured = [8, 2, 0].map(|age| now - Duration::from_secs(age));
        let result = compose(&urls, &captured, now).unwrap();
        let bytes = STANDARD
            .decode(result[0].split_once(',').unwrap().1)
            .unwrap();
        if let Ok(path) = std::env::var("AI_JARVIS_TEST_CONTACT_SHEET") {
            std::fs::write(path, &bytes).unwrap();
        }
        let mut reader = png::Decoder::new(Cursor::new(bytes)).read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut pixels).unwrap();
        let (panels, expected_height) = layout(3);
        assert_eq!(
            (info.width as usize, info.height as usize),
            (CANVAS_WIDTH, expected_height)
        );
        for (panel, color) in panels.iter().zip(colors) {
            let x = panel.x + panel.width / 2;
            let y = panel.y + LABEL_HEIGHT + panel.height / 2;
            let offset = (y * CANVAS_WIDTH + x) * 3;
            assert_eq!(&pixels[offset..offset + 3], &color);
        }
        let four_urls = [
            solid([1, 1, 1]),
            solid([2, 2, 2]),
            solid([3, 3, 3]),
            solid([4, 4, 4]),
        ];
        let four_captured = [3, 2, 1, 0].map(|age| now - Duration::from_secs(age));
        assert!(compose(&four_urls, &four_captured, now).is_err());
        assert!(compose(&urls, &captured[..2], now).is_err());
        assert!(compose(&[], &[], now).unwrap().is_empty());
    }
}
