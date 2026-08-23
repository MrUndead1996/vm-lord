//! The guest's desktop on the screen, and the words over it when there is none.
//!
//! D3D11 draws the picture and Direct2D draws the overlay. What arrives from
//! the guest is damage rather than frames, and that is what is uploaded: a
//! desktop somebody is typing on moves kilobytes a second where a pipeline that
//! pushed whole frames would move megabytes.
//!
//! The device is opened on hardware where there is any and on WARP where there
//! is not, so a machine without a GPU still shows a desktop -- and can still run
//! these tests.
//!
//! Nothing here logs a pixel. Sizes, rectangle counts and error codes only.

use vmlord_display_codec::{Geometry, OwnedCursorImage, Rect};
use windows::{
    Win32::{
        Foundation::{HMODULE, HWND, RECT},
        Graphics::{
            Direct2D::{
                Common::{D2D_RECT_F, D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT},
                D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
                D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
                D2D1_RENDER_TARGET_USAGE_NONE, D2D1CreateFactory, ID2D1Factory, ID2D1RenderTarget,
                ID2D1SolidColorBrush,
            },
            Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP},
            Direct3D11::{
                D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice,
                ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
            },
            DirectWrite::{
                DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_WEIGHT_NORMAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
                DWRITE_TEXT_ALIGNMENT_CENTER, DWriteCreateFactory, IDWriteFactory,
                IDWriteTextFormat,
            },
            Dxgi::{
                Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                CreateDXGIFactory2, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
                DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD,
                DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIFactory2, IDXGISurface, IDXGISwapChain1,
            },
            Gdi::{CreateBitmap, DeleteObject, HBITMAP},
        },
        UI::WindowsAndMessaging::{
            CreateIconIndirect, DestroyIcon, GCLP_HCURSOR, HCURSOR, ICONINFO, SetClassLongPtrW,
        },
    },
    core::HSTRING,
};

use crate::{
    status::{Progress, Status, buttons},
    video,
};

/// How many device losses one session recovers from.
///
/// A fourth loss in one session is a driver that is not going to settle, and
/// patience is not what it needs: the viewer says so and offers Retry.
pub const MAX_DEVICE_LOSSES: u32 = 3;

/// The ground behind everything, and behind the overlay.
const BACKGROUND: D2D1_COLOR_F = D2D1_COLOR_F {
    r: 0.07,
    g: 0.07,
    b: 0.08,
    a: 1.0,
};

/// The type size of the state's own word.
const LABEL_SIZE: f32 = 28.0;

/// The type size of everything under it.
const DETAIL_SIZE: f32 = 14.0;

/// One window's device, swapchain and texture.
pub struct Renderer {
    hwnd: HWND,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// An `Option` so that a rebuild can release the old one first: DXGI
    /// refuses a second swapchain for a window that still has one.
    swapchain: Option<IDXGISwapChain1>,
    target: Option<ID3D11RenderTargetView>,
    /// The stream's own texture, which the back buffer is drawn from.
    texture: Option<ID3D11Texture2D>,
    stream: Option<Geometry>,
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    cursor: Option<HCURSOR>,
    losses: u32,
    /// How many rectangles the last upload issued, for the tests.
    uploaded: usize,
}

impl Renderer {
    /// Opens a device and a swapchain for `hwnd`.
    ///
    /// # Errors
    ///
    /// A message naming what refused: no device at all, or no swapchain on it.
    pub fn open(hwnd: HWND) -> Result<Self, String> {
        let (device, context) = create_device()?;
        let swapchain = create_swapchain(&device, hwnd)?;

        // SAFETY: a factory creation with a well-known interface id.
        let d2d: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }
                .map_err(|error| format!("Direct2D could not be started: {error}"))?;
        // SAFETY: as above, for DirectWrite.
        let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }
            .map_err(|error| format!("DirectWrite could not be started: {error}"))?;

        let mut renderer = Self {
            hwnd,
            device,
            context,
            swapchain: Some(swapchain),
            target: None,
            texture: None,
            stream: None,
            d2d,
            dwrite,
            cursor: None,
            losses: 0,
            uploaded: 0,
        };
        renderer.rebuild_target()?;

        Ok(renderer)
    }

    /// The swapchain, where there is one.
    fn swapchain(&self) -> Result<&IDXGISwapChain1, String> {
        self.swapchain
            .as_ref()
            .ok_or_else(|| "the window has no swapchain".to_owned())
    }

    /// Sizes the stream's texture to `geometry`.
    ///
    /// A second config replaces the texture rather than resizing it: geometry
    /// never changes inside an encoder, so a new geometry is a new stream.
    ///
    /// # Errors
    ///
    /// A message naming what the device refused.
    pub fn configure(&mut self, geometry: Geometry) -> Result<(), String> {
        let descriptor = D3D11_TEXTURE2D_DESC {
            Width: geometry.width(),
            Height: geometry.height(),
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };

        let mut texture = None;
        // SAFETY: `descriptor` lives across the call and `texture` receives the
        // one reference this renderer then owns.
        unsafe {
            self.device
                .CreateTexture2D(&raw const descriptor, None, Some(&mut texture))
        }
        .map_err(|error| format!("the stream's texture could not be created: {error}"))?;

        log::info!(
            "the viewer's texture is {}x{}",
            geometry.width(),
            geometry.height()
        );
        self.texture = texture;
        self.stream = Some(geometry);

        Ok(())
    }

    /// The stream's size, once one has been configured.
    #[must_use]
    pub fn stream_size(&self) -> Option<(u32, u32)> {
        self.stream
            .map(|geometry| (geometry.width(), geometry.height()))
    }

    /// How many rectangles the last upload issued.
    #[must_use]
    pub fn uploaded_rectangles(&self) -> usize {
        self.uploaded
    }

    /// Uploads the rectangles of `frame` that changed.
    ///
    /// # Errors
    ///
    /// A message when no stream has been configured: a frame with no geometry
    /// is one this build will not guess the shape of.
    pub fn upload(&mut self, frame: &[u8], damage: &[Rect]) -> Result<(), String> {
        let geometry = self
            .stream
            .ok_or_else(|| "a frame arrived before the stream was configured".to_owned())?;
        let texture = self
            .texture
            .as_ref()
            .ok_or_else(|| "a frame arrived before the stream had a texture".to_owned())?;

        let stride = geometry.width() as usize * 4;
        if frame.len() < stride * geometry.height() as usize {
            return Err(format!(
                "a frame of {} bytes is short for a {}x{} stream",
                frame.len(),
                geometry.width(),
                geometry.height()
            ));
        }

        let mut issued = 0;
        let mut bytes = 0usize;
        for rect in damage {
            let Some(clipped) = clip(*rect, geometry.width(), geometry.height()) else {
                continue;
            };

            let box_ = D3D11_BOX {
                left: clipped.x,
                top: clipped.y,
                front: 0,
                right: clipped.x + clipped.width,
                bottom: clipped.y + clipped.height,
                back: 1,
            };
            let offset = clipped.y as usize * stride + clipped.x as usize * 4;

            // SAFETY: `frame` is valid for reads through the last row of the
            // clipped rectangle, which the length check above and `clip`
            // together guarantee, and the box is inside the texture.
            unsafe {
                self.context.UpdateSubresource(
                    texture,
                    0,
                    Some(&raw const box_),
                    frame[offset..].as_ptr().cast(),
                    u32::try_from(stride).expect("a stride under four gigabytes"),
                    0,
                );
            }

            issued += 1;
            bytes += clipped.width as usize * clipped.height as usize * 4;
        }

        log::trace!("{issued} rectangles uploaded, {bytes} bytes");
        self.uploaded = issued;

        Ok(())
    }

    /// Makes `image` the window's cursor.
    ///
    /// # Errors
    ///
    /// A message naming the GDI call that refused.
    pub fn set_cursor(&mut self, image: &OwnedCursorImage) -> Result<(), String> {
        if image.width == 0 || image.height == 0 {
            return Ok(());
        }

        let pixels = video::premultiplied(image);
        let width = i32::try_from(image.width).map_err(|_| "a cursor wider than an i32")?;
        let height = i32::try_from(image.height).map_err(|_| "a cursor taller than an i32")?;

        // SAFETY: `pixels` is `width * height * 4` bytes, which is what a
        // 32-bit bitmap of this size reads.
        let colour = unsafe { CreateBitmap(width, height, 1, 32, Some(pixels.as_ptr().cast())) };
        // The alpha channel does the masking, so the mask is a formality --
        // but `CreateIconIndirect` insists on one.
        // SAFETY: `None` asks for an uninitialised monochrome bitmap.
        let mask = unsafe { CreateBitmap(width, height, 1, 1, None) };
        if colour.is_invalid() || mask.is_invalid() {
            delete_bitmap(colour);
            delete_bitmap(mask);
            return Err("the cursor's bitmaps could not be created".to_owned());
        }

        let info = ICONINFO {
            fIcon: false.into(),
            xHotspot: image.hotspot_x,
            yHotspot: image.hotspot_y,
            hbmMask: mask,
            hbmColor: colour,
        };
        // SAFETY: `info` names two bitmaps this function owns and lives across
        // the call; the icon it returns owns copies of them.
        let icon = unsafe { CreateIconIndirect(&raw const info) };
        delete_bitmap(colour);
        delete_bitmap(mask);
        let icon = icon.map_err(|error| format!("the cursor could not be built: {error}"))?;

        let previous = self.cursor.replace(HCURSOR(icon.0));
        self.apply_cursor(Some(HCURSOR(icon.0)));
        destroy_cursor(previous);

        Ok(())
    }

    /// Shows or hides the guest's cursor over the window.
    pub fn show_cursor(&mut self, visible: bool) {
        let cursor = if visible { self.cursor } else { None };
        self.apply_cursor(cursor);
    }

    /// Puts a cursor on the window's class, or clears it.
    fn apply_cursor(&self, cursor: Option<HCURSOR>) {
        let value = cursor.map_or(0, |handle| handle.0 as isize);
        // SAFETY: `self.hwnd` names a window of this process, and the value is
        // either zero or an icon this renderer owns for as long as it is set.
        unsafe { SetClassLongPtrW(self.hwnd, GCLP_HCURSOR, value) };
    }

    /// Draws one frame: the desktop where there is one, the overlay where not.
    ///
    /// # Errors
    ///
    /// A message naming device loss, which is what [`Renderer::recover`]
    /// answers, or whatever else the swapchain refused.
    pub fn present(&mut self, progress: &Progress, vm_name: &str) -> Result<(), String> {
        let target = self
            .target
            .clone()
            .ok_or_else(|| "the swapchain has no render target".to_owned())?;

        // SAFETY: `target` is this renderer's own view of the current back
        // buffer, and the colour lives across the call.
        unsafe {
            self.context
                .ClearRenderTargetView(&target, &[BACKGROUND.r, BACKGROUND.g, BACKGROUND.b, 1.0]);
        }

        if progress.is_running() {
            self.blit()?;
        } else {
            self.overlay(progress, vm_name)?;
        }

        // SAFETY: a present on this renderer's own swapchain.
        let presented = unsafe { self.swapchain()?.Present(1, Default::default()) };
        if presented == DXGI_ERROR_DEVICE_REMOVED || presented == DXGI_ERROR_DEVICE_RESET {
            return Err(format!("the graphics device was lost: {presented:?}"));
        }
        presented
            .ok()
            .map_err(|error| format!("the frame could not be presented: {error}"))
    }

    /// Copies the stream's texture into the back buffer.
    ///
    /// The overlapping region rather than a stretch: scaling, letterboxing and
    /// what to do with a window that is not the guest's size are #120's, and
    /// guessing at them here would only be something to undo.
    fn blit(&self) -> Result<(), String> {
        let (Some(texture), Some(geometry)) = (self.texture.as_ref(), self.stream) else {
            return Ok(());
        };

        // SAFETY: buffer zero of this renderer's own swapchain.
        let back: ID3D11Texture2D = unsafe { self.swapchain()?.GetBuffer(0) }
            .map_err(|error| format!("the back buffer could not be taken: {error}"))?;
        let mut descriptor = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `descriptor` lives across the call.
        unsafe { back.GetDesc(&raw mut descriptor) };

        let width = geometry.width().min(descriptor.Width);
        let height = geometry.height().min(descriptor.Height);
        if width == 0 || height == 0 {
            return Ok(());
        }

        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        // SAFETY: both textures are `B8G8R8A8_UNORM` and the region is inside
        // each of them.
        unsafe {
            self.context.CopySubresourceRegion(
                &back,
                0,
                0,
                0,
                0,
                texture,
                0,
                Some(&raw const region),
            );
        }

        Ok(())
    }

    /// Draws the status overlay over the back buffer.
    fn overlay(&self, progress: &Progress, vm_name: &str) -> Result<(), String> {
        // SAFETY: buffer zero of this renderer's own swapchain.
        let surface: IDXGISurface = unsafe { self.swapchain()?.GetBuffer(0) }
            .map_err(|error| format!("the back buffer could not be taken: {error}"))?;

        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: Default::default(),
        };
        // SAFETY: `surface` and `properties` live across the call.
        let target: ID2D1RenderTarget = unsafe {
            self.d2d
                .CreateDxgiSurfaceRenderTarget(&surface, &raw const properties)
        }
        .map_err(|error| format!("the overlay's surface could not be built: {error}"))?;

        let size = self.client_size();
        let label = self.text_format(LABEL_SIZE)?;
        let detail = self.text_format(DETAIL_SIZE)?;

        // SAFETY: the render target is this function's own, and every reference
        // below outlives the drawing it is used in.
        unsafe {
            target.BeginDraw();
            let ground = BACKGROUND;
            target.Clear(Some(&raw const ground));

            let ink = self.brush(&target, 0.90, 0.90, 0.92)?;
            draw_text(
                &target,
                &label,
                progress.label(),
                rectangle(0.0, size.1 / 2.0 - 60.0, size.0, 40.0),
                &ink,
            );
            draw_text(
                &target,
                &detail,
                vm_name,
                rectangle(0.0, size.1 / 2.0 - 18.0, size.0, 20.0),
                &ink,
            );

            if let Status::Failed(reason) = progress.status() {
                let dim = self.brush(&target, 0.70, 0.70, 0.74)?;
                draw_text(
                    &target,
                    &detail,
                    reason,
                    rectangle(0.0, size.1 / 2.0 + 6.0, size.0, 36.0),
                    &dim,
                );

                let (width, height) = self.client_pixels();
                let face = self.brush(&target, 0.20, 0.20, 0.24)?;
                for (button, (x, y, w, h)) in buttons(width, height) {
                    let area = rectangle(x as f32, y as f32, w as f32, h as f32);
                    target.FillRectangle(&raw const area, &face);
                    draw_text(&target, &detail, &format!("{button:?}"), area, &ink);
                }
            }

            target
                .EndDraw(None, None)
                .map_err(|error| format!("the overlay could not be drawn: {error}"))?;
        }

        Ok(())
    }

    /// One solid brush on a render target.
    fn brush(
        &self,
        target: &ID2D1RenderTarget,
        r: f32,
        g: f32,
        b: f32,
    ) -> Result<ID2D1SolidColorBrush, String> {
        let colour = D2D1_COLOR_F { r, g, b, a: 1.0 };
        // SAFETY: `colour` lives across the call.
        unsafe { target.CreateSolidColorBrush(&raw const colour, None) }
            .map_err(|error| format!("a brush could not be made: {error}"))
    }

    /// One centred text format at `size`.
    fn text_format(&self, size: f32) -> Result<IDWriteTextFormat, String> {
        let family = HSTRING::from("Segoe UI");
        let locale = HSTRING::from("en-us");
        // SAFETY: both strings are NUL-terminated and live across the call.
        let format = unsafe {
            self.dwrite.CreateTextFormat(
                &family,
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                &locale,
            )
        }
        .map_err(|error| format!("a text format could not be made: {error}"))?;

        // SAFETY: the format is this function's own.
        unsafe {
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
            let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
        }

        Ok(format)
    }

    /// The client area, in the pixels the overlay measures in.
    fn client_pixels(&self) -> (i32, i32) {
        let mut rectangle = RECT::default();
        // SAFETY: `self.hwnd` names a window of this process.
        if unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetClientRect(self.hwnd, &raw mut rectangle)
        }
        .is_err()
        {
            return (0, 0);
        }

        (
            rectangle.right - rectangle.left,
            rectangle.bottom - rectangle.top,
        )
    }

    /// The client area as the overlay's floats.
    fn client_size(&self) -> (f32, f32) {
        let (width, height) = self.client_pixels();

        (width as f32, height as f32)
    }

    /// Resizes the swapchain to a new client area.
    ///
    /// # Errors
    ///
    /// A message naming what the swapchain refused.
    pub fn resize_swapchain(&mut self, width: u32, height: u32) -> Result<(), String> {
        if width == 0 || height == 0 {
            return Ok(());
        }

        // The view holds the old buffers; nothing may until they are gone.
        self.target = None;
        // SAFETY: this renderer owns the swapchain and has released its view.
        unsafe {
            self.swapchain()?.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                Default::default(),
            )
        }
        .map_err(|error| format!("the swapchain could not be resized: {error}"))?;

        self.rebuild_target()
    }

    /// Rebuilds everything after a device loss, up to [`MAX_DEVICE_LOSSES`].
    ///
    /// Answers `false` once the budget is spent. The caller asks the guest for
    /// a keyframe afterwards: the device that was lost held the only copy of
    /// what was on screen.
    ///
    /// # Errors
    ///
    /// A message when a device could not be opened at all.
    pub fn recover(&mut self) -> Result<bool, String> {
        if self.losses >= MAX_DEVICE_LOSSES {
            log::warn!(
                "the graphics device was lost {} times; not recovering again",
                self.losses
            );
            return Ok(false);
        }
        self.losses += 1;
        log::warn!("rebuilding the graphics device, loss {}", self.losses);

        // Everything of the lost device goes first: DXGI refuses a second
        // swapchain for a window that still has one, and the texture and the
        // view belong to a device that is no longer there.
        self.target = None;
        self.texture = None;
        self.swapchain = None;

        let (device, context) = create_device()?;
        self.swapchain = Some(create_swapchain(&device, self.hwnd)?);
        self.device = device;
        self.context = context;
        self.rebuild_target()?;

        if let Some(geometry) = self.stream {
            self.configure(geometry)?;
        }

        Ok(true)
    }

    /// Builds the render target view over the current back buffer.
    fn rebuild_target(&mut self) -> Result<(), String> {
        // SAFETY: buffer zero of this renderer's own swapchain.
        let back: ID3D11Texture2D = unsafe { self.swapchain()?.GetBuffer(0) }
            .map_err(|error| format!("the back buffer could not be taken: {error}"))?;

        let mut view = None;
        // SAFETY: `back` is a texture of this device and `view` receives the
        // one reference this renderer owns.
        unsafe {
            self.device
                .CreateRenderTargetView(&back, None, Some(&mut view))
        }
        .map_err(|error| format!("the render target could not be built: {error}"))?;

        self.target = view;

        Ok(())
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.apply_cursor(None);
        destroy_cursor(self.cursor.take());
    }
}

/// Opens a device: hardware where there is any, WARP where there is not.
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let mut last = String::new();

    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        match create_device_of(driver) {
            Ok(pair) => return Ok(pair),
            Err(error) => {
                log::debug!("no {driver:?} device: {error}");
                last = error;
            }
        }
    }

    Err(format!("no graphics device could be opened: {last}"))
}

/// Opens a device of one driver type.
fn create_device_of(
    driver: D3D_DRIVER_TYPE,
) -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let mut device = None;
    let mut context = None;

    // SAFETY: every out parameter lives across the call and receives the one
    // reference the caller then owns. `BGRA_SUPPORT` is what Direct2D needs of
    // the device it draws through.
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|error| error.to_string())?;

    match (device, context) {
        (Some(device), Some(context)) => Ok((device, context)),
        _ => Err("the device was opened without a context".to_owned()),
    }
}

/// Builds a flip-discard swapchain over one window.
fn create_swapchain(device: &ID3D11Device, hwnd: HWND) -> Result<IDXGISwapChain1, String> {
    // SAFETY: a factory creation with no flags.
    let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory2(Default::default()) }
        .map_err(|error| format!("DXGI could not be started: {error}"))?;

    let descriptor = DXGI_SWAP_CHAIN_DESC1 {
        Width: 0,
        Height: 0,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        // Two, which is what flip-discard needs and no more: a viewer that is
        // one frame behind the guest is a viewer nobody notices.
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        ..Default::default()
    };

    // SAFETY: `device`, `hwnd` and `descriptor` all live across the call.
    unsafe { factory.CreateSwapChainForHwnd(device, hwnd, &raw const descriptor, None, None) }
        .map_err(|error| format!("the swapchain could not be created: {error}"))
}

/// Clips a rectangle to the stream, or drops it if nothing is left.
fn clip(rect: Rect, width: u32, height: u32) -> Option<Rect> {
    if rect.x >= width || rect.y >= height {
        return None;
    }

    let clipped = Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width.min(width - rect.x),
        height: rect.height.min(height - rect.y),
    };
    if clipped.width == 0 || clipped.height == 0 {
        return None;
    }

    Some(clipped)
}

/// One string in one rectangle.
///
/// # Safety
///
/// `target` must be between `BeginDraw` and `EndDraw`.
unsafe fn draw_text(
    target: &ID2D1RenderTarget,
    format: &IDWriteTextFormat,
    text: &str,
    area: D2D_RECT_F,
    brush: &ID2D1SolidColorBrush,
) {
    let wide: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: `wide` and `area` live across the call, and the caller has begun
    // drawing on `target`.
    unsafe {
        target.DrawText(
            &wide,
            format,
            &raw const area,
            brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            Default::default(),
        );
    }
}

/// One rectangle, in the overlay's coordinates.
fn rectangle(left: f32, top: f32, width: f32, height: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// Releases a GDI bitmap, if there is one.
fn delete_bitmap(bitmap: HBITMAP) {
    if bitmap.is_invalid() {
        return;
    }

    // SAFETY: the caller owns the bitmap and deletes it once.
    unsafe {
        let _ = DeleteObject(bitmap.into());
    }
}

/// Releases a cursor icon, if there is one.
fn destroy_cursor(cursor: Option<HCURSOR>) {
    let Some(cursor) = cursor else {
        return;
    };

    // SAFETY: the renderer owns the icon and destroys it once.
    unsafe {
        let _ = DestroyIcon(windows::Win32::UI::WindowsAndMessaging::HICON(cursor.0));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, mpsc},
        time::Instant,
    };

    use vmlord_display_codec::{Geometry, PixelFormat, Rect, TileSize};

    use super::Renderer;
    use crate::{
        status::Progress,
        windows::window::{Shared, Window},
    };

    fn window() -> Window {
        let (events, received) = mpsc::channel();
        // The receiver is leaked deliberately: the window outlives it in the
        // binary, and a dropped receiver would only make `report` log.
        std::mem::forget(received);

        Window::open(
            "renderer test - VMLord Display",
            640,
            480,
            Arc::new(Shared::new(events)),
        )
        .expect("a window")
    }

    fn geometry(width: u32, height: u32) -> Geometry {
        Geometry::new(width, height, TileSize::ThirtyTwo, PixelFormat::Bgra8888)
            .expect("a geometry the codec allows")
    }

    #[test]
    fn a_renderer_opens_on_this_machine() {
        let window = window();

        // Hardware where there is any, WARP where there is not: a test host and
        // a headless build agent both have to be able to run this.
        Renderer::open(window.handle()).expect("a device, hardware or WARP");
    }

    #[test]
    fn a_stream_config_sizes_the_texture_and_a_second_one_replaces_it() {
        let window = window();
        let mut renderer = Renderer::open(window.handle()).expect("a device");

        renderer.configure(geometry(320, 200)).expect("a texture");
        assert_eq!(renderer.stream_size(), Some((320, 200)));

        renderer.configure(geometry(640, 480)).expect("a texture");
        assert_eq!(renderer.stream_size(), Some((640, 480)));
    }

    #[test]
    fn only_the_rectangles_that_changed_are_uploaded_and_they_are_clipped() {
        let window = window();
        let mut renderer = Renderer::open(window.handle()).expect("a device");
        let geometry = geometry(100, 60);
        renderer.configure(geometry).expect("a texture");

        let frame = vec![0x7f; geometry.frame_bytes()];
        // The last column and row are narrower than a tile, and a rectangle
        // that runs past the edge is clipped rather than refused.
        let damage = [
            Rect {
                x: 0,
                y: 0,
                width: 32,
                height: 32,
            },
            Rect {
                x: 96,
                y: 32,
                width: 32,
                height: 32,
            },
        ];

        renderer.upload(&frame, &damage).expect("an upload");
        assert_eq!(renderer.uploaded_rectangles(), 2);
    }

    #[test]
    fn an_upload_before_any_stream_config_is_refused_rather_than_guessed_at() {
        let window = window();
        let mut renderer = Renderer::open(window.handle()).expect("a device");

        assert!(
            renderer
                .upload(
                    &[0; 16],
                    &[Rect {
                        x: 0,
                        y: 0,
                        width: 2,
                        height: 2
                    }]
                )
                .is_err()
        );
    }

    #[test]
    fn the_overlay_presents_in_every_state_that_shows_one() {
        let window = window();
        let mut renderer = Renderer::open(window.handle()).expect("a device");
        let now = Instant::now();
        let mut progress = Progress::new(now);

        renderer
            .present(&progress, "ubuntu-24.04")
            .expect("a present");
        progress.tick(now + crate::status::RETRY_BUDGET);
        renderer
            .present(&progress, "ubuntu-24.04")
            .expect("a present");
    }

    #[test]
    fn a_device_is_recovered_a_bounded_number_of_times() {
        let window = window();
        let mut renderer = Renderer::open(window.handle()).expect("a device");

        for _ in 0..super::MAX_DEVICE_LOSSES {
            assert!(renderer.recover().expect("a rebuilt device"));
        }
        assert!(
            !renderer.recover().expect("the count is not an error"),
            "a fourth loss in one session is not recovered from"
        );
    }
}
