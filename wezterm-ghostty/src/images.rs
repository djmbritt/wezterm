//! Kitty graphics support.
//!
//! libghostty-vt implements the kitty graphics protocol: it parses the
//! APC sequences, decodes and stores the images and tracks placements,
//! anchored to grid rows so that they scroll with the text.  wezterm's
//! renderer draws images as per-cell slices (`ImageCell`s attached to cell
//! attributes), so this module resolves every placement to the stable rows
//! and columns it covers and computes the texture slice for each cell.
//!
//! Placements created with `a=p`/`a=T` are read from libghostty's image
//! storage, which covers the scrollback as well as the screen.  Virtual
//! placements (Unicode placeholder cells, U+10EEEE) are only resolved by
//! libghostty's render state, which covers the active area.
use ghostty_vt_sys::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Once};
use wezterm_term::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
use wezterm_term::{Cell, StableRowIndex};

/// Codepoint that kitty uses for Unicode placeholder cells
pub const PLACEHOLDER: u32 = 0x10EEEE;

/// Storage limit in bytes when kitty graphics are enabled.  This matches
/// kitty's own default quota.
pub(crate) const STORAGE_LIMIT: u64 = 320 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub image_id: u32,
    pub placement_id: u32,
    pub z: i32,
    /// Stable row and column of the top-left cell
    pub row: StableRowIndex,
    pub col: i64,
    pub grid_cols: u32,
    pub grid_rows: u32,
    /// Offset of the image within the top-left cell, in pixels
    pub offset_x: u32,
    pub offset_y: u32,
    /// Size to draw the image at, in pixels
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// The part of the image to draw, in image pixels
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
}

impl Placement {
    fn rows(&self) -> std::ops::Range<StableRowIndex> {
        self.row..self.row + self.grid_rows as StableRowIndex
    }
}

struct CachedImage {
    generation: u64,
    width: u32,
    height: u32,
    data: Arc<ImageData>,
}

pub struct Images {
    storage_generation: Option<u64>,
    render_generation: Option<u64>,
    placements: Vec<Placement>,
    cache: HashMap<u32, CachedImage>,
    placement_iter: GhosttyKittyGraphicsPlacementIterator,
    render_iter: GhosttyRenderStateImageIterator,
}

impl Images {
    pub fn new() -> Self {
        install_png_decoder();
        let mut placement_iter = std::ptr::null_mut();
        let mut render_iter = std::ptr::null_mut();
        unsafe {
            ghostty_kitty_graphics_placement_iterator_new(std::ptr::null(), &mut placement_iter);
            ghostty_render_state_image_iterator_new(std::ptr::null(), &mut render_iter);
        }
        Self {
            storage_generation: None,
            render_generation: None,
            placements: vec![],
            cache: HashMap::new(),
            placement_iter,
            render_iter,
        }
    }

    /// Enable or disable the protocol on a terminal
    /// `local_media` allows the file, temporary file and shared memory
    /// transmission media. Leave it off for output that comes from another
    /// machine (a Rex server): those media read files on *this* machine.
    pub fn configure(term: GhosttyTerminal, enabled: bool, local_media: bool) {
        let limit: u64 = if enabled { STORAGE_LIMIT } else { 0 };
        let on = enabled && local_media;
        let tmp = std::env::temp_dir();
        let tmp = tmp.to_string_lossy();
        let tmp = GhosttyString {
            ptr: tmp.as_ptr(),
            len: tmp.len(),
        };
        unsafe {
            ghostty_terminal_set(
                term,
                GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT,
                &limit as *const u64 as *const _,
            );
            // wezterm has always accepted the file, temporary file and
            // shared memory transmission media
            ghostty_terminal_set(
                term,
                GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_FILE,
                &on as *const bool as *const _,
            );
            ghostty_terminal_set(
                term,
                GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_SHARED_MEM,
                &on as *const bool as *const _,
            );
            ghostty_terminal_set(
                term,
                GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_TEMP_FILE,
                if on {
                    &tmp as *const GhosttyString as *const _
                } else {
                    std::ptr::null()
                },
            );
        }
    }

    /// Forget everything; used when switching screens or resizing
    pub fn reset(&mut self) {
        self.storage_generation = None;
        self.render_generation = None;
    }

    /// Re-read the placements if libghostty reports that they changed.
    /// Returns the stable rows whose image content may have changed.
    ///
    /// `stable_offset` is the stable row of screen row 0, and `active_top`
    /// the stable row of the top of the active area.
    pub fn update(
        &mut self,
        term: GhosttyTerminal,
        render: GhosttyRenderState,
        stable_offset: StableRowIndex,
        active_top: StableRowIndex,
    ) -> Vec<StableRowIndex> {
        let graphics = storage(term);
        let mut storage_generation = 0u64;
        let mut render_generation = 0u64;
        unsafe {
            if !graphics.is_null() {
                ghostty_kitty_graphics_get(
                    graphics,
                    GHOSTTY_KITTY_GRAPHICS_DATA_GENERATION,
                    &mut storage_generation as *mut u64 as *mut _,
                );
            }
            ghostty_render_state_get(
                render,
                GHOSTTY_RENDER_STATE_DATA_IMAGE_PLACEMENTS_GENERATION,
                &mut render_generation as *mut u64 as *mut _,
            );
        }
        if self.storage_generation == Some(storage_generation)
            && self.render_generation == Some(render_generation)
        {
            return vec![];
        }
        self.storage_generation = Some(storage_generation);
        self.render_generation = Some(render_generation);

        let mut placements = vec![];
        if !graphics.is_null() {
            unsafe {
                self.read_storage_placements(term, graphics, stable_offset, &mut placements);
            }
        }
        unsafe {
            self.read_virtual_placements(render, active_top, &mut placements);
        }

        // Drop decoded images that are no longer referenced
        let live: HashSet<u32> = placements.iter().map(|p| p.image_id).collect();
        self.cache.retain(|id, _| live.contains(id));

        let mut rows = HashSet::new();
        for p in self.placements.iter().chain(placements.iter()) {
            rows.extend(p.rows());
        }
        self.placements = placements;
        rows.into_iter().collect()
    }

    unsafe fn read_storage_placements(
        &mut self,
        term: GhosttyTerminal,
        graphics: GhosttyKittyGraphics,
        stable_offset: StableRowIndex,
        out: &mut Vec<Placement>,
    ) {
        if ghostty_kitty_graphics_get(
            graphics,
            GHOSTTY_KITTY_GRAPHICS_DATA_PLACEMENT_ITERATOR,
            &mut self.placement_iter as *mut _ as *mut _,
        ) != GHOSTTY_SUCCESS
        {
            return;
        }
        let iter = self.placement_iter;
        let pget = |what: GhosttyKittyGraphicsPlacementData| -> u32 {
            let mut v = 0u32;
            ghostty_kitty_graphics_placement_get(iter, what, &mut v as *mut u32 as *mut _);
            v
        };
        while ghostty_kitty_graphics_placement_next(iter) {
            let mut is_virtual = false;
            ghostty_kitty_graphics_placement_get(
                iter,
                GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IS_VIRTUAL,
                &mut is_virtual as *mut bool as *mut _,
            );
            if is_virtual {
                continue;
            }
            let image_id = pget(GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IMAGE_ID);
            let image = ghostty_kitty_graphics_image(graphics, image_id);
            if image.is_null() {
                continue;
            }

            let mut sel: GhosttySelection = std::mem::zeroed();
            if ghostty_kitty_graphics_placement_rect(iter, image, term, &mut sel) != GHOSTTY_SUCCESS
            {
                continue;
            }
            let mut start = GhosttyPointCoordinate { x: 0, y: 0 };
            if ghostty_terminal_point_from_grid_ref(
                term,
                &sel.start,
                GHOSTTY_POINT_TAG_SCREEN,
                &mut start,
            ) != GHOSTTY_SUCCESS
            {
                continue;
            }

            let mut info: GhosttyKittyGraphicsPlacementRenderInfo = std::mem::zeroed();
            info.size = std::mem::size_of::<GhosttyKittyGraphicsPlacementRenderInfo>();
            if ghostty_kitty_graphics_placement_render_info(iter, image, term, &mut info)
                != GHOSTTY_SUCCESS
            {
                continue;
            }

            let mut z = 0i32;
            ghostty_kitty_graphics_placement_get(
                iter,
                GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_Z,
                &mut z as *mut i32 as *mut _,
            );

            out.push(Placement {
                image_id,
                placement_id: pget(GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_PLACEMENT_ID),
                z,
                row: stable_offset + start.y as StableRowIndex,
                col: start.x as i64,
                grid_cols: info.grid_cols,
                grid_rows: info.grid_rows,
                offset_x: pget(GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_X_OFFSET),
                offset_y: pget(GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_Y_OFFSET),
                pixel_width: info.pixel_width,
                pixel_height: info.pixel_height,
                source_x: info.source_x,
                source_y: info.source_y,
                source_width: info.source_width,
                source_height: info.source_height,
            });
        }
    }

    unsafe fn read_virtual_placements(
        &mut self,
        render: GhosttyRenderState,
        active_top: StableRowIndex,
        out: &mut Vec<Placement>,
    ) {
        if ghostty_render_state_get(
            render,
            GHOSTTY_RENDER_STATE_DATA_IMAGE_ITERATOR,
            &mut self.render_iter as *mut _ as *mut _,
        ) != GHOSTTY_SUCCESS
        {
            return;
        }
        let iter = self.render_iter;
        fn get<T: Default>(iter: GhosttyRenderStateImageIterator, what: u32) -> T {
            let mut v = T::default();
            unsafe {
                ghostty_render_state_image_get(iter, what, &mut v as *mut T as *mut _);
            }
            v
        }
        while ghostty_render_state_image_iterator_next(iter) {
            let protocol: GhosttyImageProtocol =
                get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_PROTOCOL);
            let kind: GhosttyRenderStateKittyPlacementKind =
                get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_KITTY_KIND);
            if protocol != GHOSTTY_IMAGE_PROTOCOL_KITTY
                || kind != GHOSTTY_RENDER_STATE_KITTY_PLACEMENT_KIND_VIRTUAL
            {
                continue;
            }
            let row: i32 = get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_VIEWPORT_ROW);
            let col: i32 = get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_VIEWPORT_COL);
            out.push(Placement {
                image_id: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_IMAGE_ID),
                placement_id: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_KITTY_PLACEMENT_ID),
                z: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_Z),
                row: active_top + row as StableRowIndex,
                col: col as i64,
                grid_cols: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_GRID_COLS),
                grid_rows: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_GRID_ROWS),
                offset_x: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_CELL_OFFSET_X),
                offset_y: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_CELL_OFFSET_Y),
                pixel_width: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_PIXEL_WIDTH),
                pixel_height: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_PIXEL_HEIGHT),
                source_x: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_X),
                source_y: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_Y),
                source_width: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_WIDTH),
                source_height: get(iter, GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_HEIGHT),
            });
        }
    }

    /// Returns the decoded pixels of an image, converting them to RGBA
    /// and caching them until the image's generation changes.
    fn image(
        &mut self,
        term: GhosttyTerminal,
        image_id: u32,
    ) -> Option<(u32, u32, Arc<ImageData>)> {
        let graphics = storage(term);
        if graphics.is_null() {
            return None;
        }
        unsafe {
            let image = ghostty_kitty_graphics_image(graphics, image_id);
            if image.is_null() {
                return None;
            }
            let iget = |what: GhosttyKittyGraphicsImageData| -> u64 {
                let mut v = 0u64;
                ghostty_kitty_graphics_image_get(image, what, &mut v as *mut u64 as *mut _);
                v
            };
            let generation = iget(GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_GENERATION);
            if let Some(cached) = self.cache.get(&image_id) {
                if cached.generation == generation {
                    return Some((cached.width, cached.height, Arc::clone(&cached.data)));
                }
            }
            let mut width = 0u32;
            let mut height = 0u32;
            let mut format: GhosttyKittyImageFormat = GHOSTTY_KITTY_IMAGE_FORMAT_RGBA;
            ghostty_kitty_graphics_image_get(
                image,
                GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_WIDTH,
                &mut width as *mut u32 as *mut _,
            );
            ghostty_kitty_graphics_image_get(
                image,
                GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_HEIGHT,
                &mut height as *mut u32 as *mut _,
            );
            ghostty_kitty_graphics_image_get(
                image,
                GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_FORMAT,
                &mut format as *mut _ as *mut _,
            );
            let mut pixels = GhosttyString {
                ptr: std::ptr::null(),
                len: 0,
            };
            if ghostty_kitty_graphics_image_get(
                image,
                GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_PIXELS,
                &mut pixels as *mut GhosttyString as *mut _,
            ) != GHOSTTY_SUCCESS
                || pixels.ptr.is_null()
                || width == 0
                || height == 0
            {
                return None;
            }
            let src = std::slice::from_raw_parts(pixels.ptr, pixels.len);
            let rgba = to_rgba(src, format, width, height)?;
            let data = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
                width, height, rgba,
            )));
            self.cache.insert(
                image_id,
                CachedImage {
                    generation,
                    width,
                    height,
                    data: Arc::clone(&data),
                },
            );
            Some((width, height, data))
        }
    }

    /// Attach image slices to the cells of the line at stable row `row`
    pub fn apply_to_row(
        &mut self,
        term: GhosttyTerminal,
        row: StableRowIndex,
        cells: &mut [Cell],
        cell_width: u32,
        cell_height: u32,
    ) {
        if self.placements.is_empty() || cell_width == 0 || cell_height == 0 {
            return;
        }
        let mut placements: Vec<Placement> = self
            .placements
            .iter()
            .filter(|p| p.rows().contains(&row))
            .cloned()
            .collect();
        // Attach in z order; wezterm sorts by z when drawing, but a stable
        // order keeps the line's shape hash deterministic
        placements.sort_by_key(|p| (p.z, p.image_id, p.placement_id));

        for p in placements {
            let Some((img_w, img_h, data)) = self.image(term, p.image_id) else {
                continue;
            };
            if p.pixel_width == 0
                || p.pixel_height == 0
                || p.source_width == 0
                || p.source_height == 0
            {
                continue;
            }
            let cy = (row - p.row) as u32;
            // Vertical extent of this cell, relative to the placement origin
            let (y0, y1) = (cy * cell_height, (cy + 1) * cell_height);
            let (iy0, iy1) = (p.offset_y, p.offset_y + p.pixel_height);
            let (a_y, b_y) = (y0.max(iy0), y1.min(iy1));
            if a_y >= b_y {
                continue;
            }
            let v = |py: u32| {
                (p.source_y as f32
                    + (py - iy0) as f32 * p.source_height as f32 / p.pixel_height as f32)
                    / img_h as f32
            };

            for cx in 0..p.grid_cols {
                let col = p.col + cx as i64;
                if col < 0 || col as usize >= cells.len() {
                    continue;
                }
                let (x0, x1) = (cx * cell_width, (cx + 1) * cell_width);
                let (ix0, ix1) = (p.offset_x, p.offset_x + p.pixel_width);
                let (a_x, b_x) = (x0.max(ix0), x1.min(ix1));
                if a_x >= b_x {
                    continue;
                }
                let u = |px: u32| {
                    (p.source_x as f32
                        + (px - ix0) as f32 * p.source_width as f32 / p.pixel_width as f32)
                        / img_w as f32
                };
                let image = ImageCell::with_z_index(
                    TextureCoordinate::new_f32(u(a_x), v(a_y)),
                    TextureCoordinate::new_f32(u(b_x), v(b_y)),
                    Arc::clone(&data),
                    p.z,
                    (a_x - x0) as u16,
                    (a_y - y0) as u16,
                    (x1 - b_x) as u16,
                    (y1 - b_y) as u16,
                    Some(p.image_id),
                    Some(p.placement_id),
                );
                cells[col as usize]
                    .attrs_mut()
                    .attach_image(Box::new(image));
            }
        }
    }
}

impl Drop for Images {
    fn drop(&mut self) {
        unsafe {
            ghostty_kitty_graphics_placement_iterator_free(self.placement_iter);
            ghostty_render_state_image_iterator_free(self.render_iter);
        }
    }
}

fn storage(term: GhosttyTerminal) -> GhosttyKittyGraphics {
    let mut graphics: GhosttyKittyGraphics = std::ptr::null_mut();
    unsafe {
        if ghostty_terminal_get(
            term,
            GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS,
            &mut graphics as *mut _ as *mut _,
        ) != GHOSTTY_SUCCESS
        {
            return std::ptr::null_mut();
        }
    }
    graphics
}

fn to_rgba(src: &[u8], format: GhosttyKittyImageFormat, w: u32, h: u32) -> Option<Vec<u8>> {
    let n = (w as usize).checked_mul(h as usize)?;
    let (bpp, f): (usize, fn(&[u8]) -> [u8; 4]) = match format {
        GHOSTTY_KITTY_IMAGE_FORMAT_RGBA => (4, |p| [p[0], p[1], p[2], p[3]]),
        GHOSTTY_KITTY_IMAGE_FORMAT_RGB => (3, |p| [p[0], p[1], p[2], 255]),
        GHOSTTY_KITTY_IMAGE_FORMAT_GRAY_ALPHA => (2, |p| [p[0], p[0], p[0], p[1]]),
        GHOSTTY_KITTY_IMAGE_FORMAT_GRAY => (1, |p| [p[0], p[0], p[0], 255]),
        _ => return None,
    };
    if src.len() < n * bpp {
        return None;
    }
    if bpp == 4 {
        return Some(src[..n * 4].to_vec());
    }
    let mut out = Vec::with_capacity(n * 4);
    for px in src[..n * bpp].chunks_exact(bpp) {
        out.extend_from_slice(&f(px));
    }
    Some(out)
}

/// libghostty-vt has no built-in PNG decoder; it asks the embedder to
/// provide one.  The decoder is process-global.
fn install_png_decoder() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        ghostty_sys_set(
            GHOSTTY_SYS_OPT_DECODE_PNG,
            decode_png as *const std::ffi::c_void,
        );
    });
}

unsafe extern "C" fn decode_png(
    _userdata: *mut std::ffi::c_void,
    allocator: *const GhosttyAllocator,
    data: *const u8,
    data_len: usize,
    out: *mut GhosttySysImage,
) -> bool {
    let bytes = std::slice::from_raw_parts(data, data_len);
    let img = match image::load_from_memory_with_format(bytes, image::ImageFormat::Png) {
        Ok(img) => img.to_rgba8(),
        Err(err) => {
            log::warn!("kitty graphics: failed to decode PNG: {err:#}");
            return false;
        }
    };
    let (width, height) = img.dimensions();
    let raw = img.into_raw();
    let buf = ghostty_alloc(allocator, raw.len());
    if buf.is_null() {
        return false;
    }
    std::ptr::copy_nonoverlapping(raw.as_ptr(), buf, raw.len());
    (*out).width = width;
    (*out).height = height;
    (*out).data = buf;
    (*out).data_len = raw.len();
    true
}
