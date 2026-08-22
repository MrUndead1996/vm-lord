//! Deterministic desktop workloads, for the tests and the benchmark.
//!
//! An ordinary module rather than a feature: the round-trip property, the
//! golden vectors and `cargo display-bench` all need the same frames, and a
//! guest binary that calls none of them drops the module at link time.
//!
//! The scenes are synthetic on purpose. They exist to exercise the encoder's
//! decision -- flat regions, a small moving block, a whole frame replaced --
//! not to look like a desktop.

use crate::geometry::{Geometry, Rect};

/// A workload, named by what it does to a desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scene {
    /// Nothing moves after the first frame.
    StaticDesktop,
    /// One small block appears per frame, as a character would.
    Typing,
    /// The whole frame shifts up, with new content entering at the bottom.
    Scrolling,
    /// A filled rectangle travels diagonally over a fixed background.
    MovingWindow,
    /// Every pixel is redrawn every frame.
    FullscreenVideo,
}

impl Scene {
    /// Every scene, in the order the benchmark reports them.
    pub const ALL: [Self; 5] = [
        Self::StaticDesktop,
        Self::Typing,
        Self::Scrolling,
        Self::MovingWindow,
        Self::FullscreenVideo,
    ];

    /// This scene's name, for a table or a failure message.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::StaticDesktop => "static desktop",
            Self::Typing => "typing",
            Self::Scrolling => "scrolling",
            Self::MovingWindow => "moving window",
            Self::FullscreenVideo => "fullscreen video",
        }
    }
}

/// xorshift64*, so a scene is the same on every machine and every run.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Zero is the one state xorshift cannot leave.
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next(&mut self) -> u32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }
}

/// Successive frames of one scene.
pub struct Generator {
    scene: Scene,
    geometry: Geometry,
    rng: Rng,
    background: Vec<u8>,
    frame: Vec<u8>,
    damage: Vec<Rect>,
    step: u32,
    caret: (u32, u32),
    window: (u32, u32),
}

impl Generator {
    /// A generator for one scene, reproducible from `seed`.
    #[must_use]
    pub fn new(scene: Scene, geometry: Geometry, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let background = paint_background(geometry, &mut rng);

        Self {
            scene,
            geometry,
            rng,
            frame: background.clone(),
            background,
            damage: Vec::new(),
            step: 0,
            caret: (0, 0),
            window: (0, 0),
        }
    }

    /// The rectangles the last frame actually wrote.
    ///
    /// A capture backend's damage is a hint about the same thing, which is why
    /// the tests feed these to the encoder as one.
    #[must_use]
    pub fn damage(&self) -> &[Rect] {
        &self.damage
    }

    /// The next frame, four bytes per pixel and `width * 4` per row.
    pub fn next_frame(&mut self) -> &[u8] {
        let width = self.geometry.width();
        let height = self.geometry.height();
        let whole = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };

        self.damage.clear();

        if self.step == 0 {
            self.step += 1;
            self.damage.push(whole);
            return &self.frame;
        }

        match self.scene {
            Scene::StaticDesktop => {}
            Scene::Typing => {
                let block = clipped(self.geometry, self.caret, (8, 16));
                let ink = 0x00E0_E0E0 ^ self.rng.next();
                fill(&mut self.frame, self.geometry, block, ink);
                self.damage.push(block);

                self.caret.0 += 8;
                if self.caret.0 + 8 > width {
                    self.caret.0 = 0;
                    self.caret.1 = (self.caret.1 + 16) % height.max(1);
                }
            }
            Scene::Scrolling => {
                let stride = width as usize * 4;
                let shift = 40.min(height) as usize;
                self.frame.copy_within(shift * stride.., 0);

                let start = (height as usize - shift) * stride;
                for chunk in self.frame[start..].chunks_exact_mut(4) {
                    chunk.copy_from_slice(&self.rng.next().to_le_bytes());
                }
                self.damage.push(whole);
            }
            Scene::MovingWindow => {
                let size = (400.min(width), 300.min(height));
                let old = clipped(self.geometry, self.window, size);
                restore(&mut self.frame, &self.background, self.geometry, old);
                self.damage.push(old);

                self.window.0 = (self.window.0 + 7) % width;
                self.window.1 = (self.window.1 + 7) % height;
                let new = clipped(self.geometry, self.window, size);
                fill(&mut self.frame, self.geometry, new, 0x0033_6699);
                self.damage.push(new);
            }
            Scene::FullscreenVideo => {
                for chunk in self.frame.chunks_exact_mut(4) {
                    chunk.copy_from_slice(&self.rng.next().to_le_bytes());
                }
                self.damage.push(whole);
            }
        }

        self.step += 1;
        &self.frame
    }
}

/// A desktop's worth of flat panels, which is what makes a keyframe
/// compressible at all.
fn paint_background(geometry: Geometry, rng: &mut Rng) -> Vec<u8> {
    let width = geometry.width();
    let height = geometry.height();
    let mut frame = vec![0u8; geometry.frame_bytes()];

    fill(
        &mut frame,
        geometry,
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
        0x0020_2428,
    );

    let panel = Rect {
        x: 0,
        y: 0,
        width,
        height: 28.min(height),
    };
    fill(&mut frame, geometry, panel, 0x0011_1417);

    let mut y = 40;
    while y + 60 < height {
        let mut x = 20;
        while x + 120 < width {
            fill(
                &mut frame,
                geometry,
                Rect {
                    x,
                    y,
                    width: 120,
                    height: 60,
                },
                0x0040_4040 ^ (rng.next() & 0x000F_0F0F),
            );
            x += 160;
        }
        y += 100;
    }

    frame
}

/// A window's rectangle, clipped to the frame.
fn clipped(geometry: Geometry, at: (u32, u32), size: (u32, u32)) -> Rect {
    Rect {
        x: at.0,
        y: at.1,
        width: size.0.min(geometry.width() - at.0),
        height: size.1.min(geometry.height() - at.1),
    }
}

/// Paints one colour over a rectangle.
fn fill(frame: &mut [u8], geometry: Geometry, rect: Rect, colour: u32) {
    let stride = geometry.width() as usize * 4;
    for y in rect.y..rect.y + rect.height {
        let start = y as usize * stride + rect.x as usize * 4;
        for chunk in frame[start..start + rect.width as usize * 4].chunks_exact_mut(4) {
            chunk.copy_from_slice(&colour.to_le_bytes());
        }
    }
}

/// Puts the background back where a window was.
fn restore(frame: &mut [u8], background: &[u8], geometry: Geometry, rect: Rect) {
    let stride = geometry.width() as usize * 4;
    for y in rect.y..rect.y + rect.height {
        let start = y as usize * stride + rect.x as usize * 4;
        let end = start + rect.width as usize * 4;
        frame[start..end].copy_from_slice(&background[start..end]);
    }
}
