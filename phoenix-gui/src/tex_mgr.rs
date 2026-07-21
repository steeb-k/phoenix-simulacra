//! CPU rasterizer texture manager.
//!
//! Holds the egui font atlas and any user/image textures as plain RGBA byte
//! buffers so `Renderer::paint` (see `main.rs`) can sample them per-pixel on the
//! CPU. Fed from egui's `TexturesDelta` every frame — no GPU involved. Ported
//! from the Diskoria app, which pioneered this GPU-free path for WinPE.

pub(crate) struct TexEntry {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl TexEntry {
    pub fn sample(&self, uv_x: f32, uv_y: f32) -> [f32; 4] {
        let px = (uv_x * self.width as f32)
            .floor()
            .clamp(0.0, self.width as f32 - 1.0) as usize;
        let py = (uv_y * self.height as f32)
            .floor()
            .clamp(0.0, self.height as f32 - 1.0) as usize;
        let idx = (py * self.width + px) * 4;
        if idx + 3 >= self.rgba.len() {
            return [1.0, 0.0, 1.0, 1.0];
        }
        [
            self.rgba[idx] as f32 / 255.0,
            self.rgba[idx + 1] as f32 / 255.0,
            self.rgba[idx + 2] as f32 / 255.0,
            self.rgba[idx + 3] as f32 / 255.0,
        ]
    }
}

pub(crate) struct TextureManager {
    pub font_atlas: Option<TexEntry>,
    pub textures: std::collections::HashMap<egui::TextureId, TexEntry>,
}

impl TextureManager {
    pub fn new() -> Self {
        Self {
            font_atlas: None,
            textures: std::collections::HashMap::new(),
        }
    }

    pub fn update(&mut self, delta: &egui::TexturesDelta) {
        for (id, image_delta) in &delta.set {
            let (w, h, rgba) = match &image_delta.image {
                egui::ImageData::Font(font_img) => {
                    let w = font_img.width();
                    let h = font_img.height();
                    let rgba: Vec<u8> = font_img
                        .pixels
                        .iter()
                        .flat_map(|&cov| {
                            let v = (cov * 255.0 + 0.5) as u8;
                            [v, v, v, v]
                        })
                        .collect();
                    (w, h, rgba)
                }
                egui::ImageData::Color(color_img) => {
                    let w = color_img.width();
                    let h = color_img.height();
                    let rgba: Vec<u8> = color_img
                        .pixels
                        .iter()
                        .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
                        .collect();
                    (w, h, rgba)
                }
            };

            let entry = if let Some([ox, oy]) = image_delta.pos {
                if *id == egui::TextureId::default() {
                    if let Some(atlas) = &mut self.font_atlas {
                        for row in 0..h {
                            for col in 0..w {
                                let src = (row * w + col) * 4;
                                let dst = ((oy + row) * atlas.width + (ox + col)) * 4;
                                if dst + 3 < atlas.rgba.len() && src + 3 < rgba.len() {
                                    atlas.rgba[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
                                }
                            }
                        }
                    }
                    continue;
                } else if let Some(entry) = self.textures.get_mut(id) {
                    for row in 0..h {
                        for col in 0..w {
                            let src = (row * w + col) * 4;
                            let dst = ((oy + row) * entry.width + (ox + col)) * 4;
                            if dst + 3 < entry.rgba.len() && src + 3 < rgba.len() {
                                entry.rgba[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
                            }
                        }
                    }
                    continue;
                } else {
                    TexEntry {
                        width: w,
                        height: h,
                        rgba,
                    }
                }
            } else {
                TexEntry {
                    width: w,
                    height: h,
                    rgba,
                }
            };

            if *id == egui::TextureId::default() {
                self.font_atlas = Some(entry);
            } else {
                self.textures.insert(*id, entry);
            }
        }

        for id in &delta.free {
            self.textures.remove(id);
        }
    }

    /// Alpha coverage for the font atlas — returns [0, 1].
    pub fn sample_alpha_f(&self, uv_x: f32, uv_y: f32) -> f32 {
        let Some(atlas) = &self.font_atlas else {
            return 1.0;
        };
        let px = (uv_x * atlas.width as f32)
            .floor()
            .clamp(0.0, atlas.width as f32 - 1.0) as usize;
        let py = (uv_y * atlas.height as f32)
            .floor()
            .clamp(0.0, atlas.height as f32 - 1.0) as usize;
        let idx = (py * atlas.width + px) * 4 + 3;
        atlas.rgba.get(idx).copied().unwrap_or(0) as f32 / 255.0
    }

    /// RGBA sample for image textures — returns [r, g, b, a] in [0, 1].
    pub fn sample_rgba(&self, id: egui::TextureId, uv_x: f32, uv_y: f32) -> [f32; 4] {
        self.textures
            .get(&id)
            .map(|e| e.sample(uv_x, uv_y))
            .unwrap_or([1.0, 0.0, 1.0, 1.0])
    }
}
