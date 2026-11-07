//! Conversion of libghostty-vt grid content into wezterm `Line`s.
use ghostty_vt_sys::*;
use std::collections::HashMap;
use std::sync::Arc;
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::surface::SequenceNo;
use wezterm_term::{
    Blink, Cell, CellAttributes, Hyperlink, Intensity, Line, SemanticType, Underline,
};

/// Caches `Arc<Hyperlink>` instances by URI so that adjacent cells of the
/// same link share an allocation (wezterm compares links by pointer and
/// value when clustering).
#[derive(Default)]
pub struct HyperlinkCache {
    links: HashMap<String, Arc<Hyperlink>>,
}

impl HyperlinkCache {
    fn get(&mut self, uri: &str) -> Arc<Hyperlink> {
        if let Some(link) = self.links.get(uri) {
            return Arc::clone(link);
        }
        if self.links.len() > 4096 {
            self.links.clear();
        }
        let link = Arc::new(Hyperlink::new(uri));
        self.links.insert(uri.to_string(), Arc::clone(&link));
        link
    }
}

fn style_color(color: &GhosttyStyleColor) -> ColorAttribute {
    unsafe {
        match color.tag as u32 {
            GHOSTTY_STYLE_COLOR_PALETTE => ColorAttribute::PaletteIndex(color.value.palette),
            GHOSTTY_STYLE_COLOR_RGB => {
                let rgb = color.value.rgb;
                ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple::from((
                    rgb.r, rgb.g, rgb.b,
                )))
            }
            _ => ColorAttribute::Default,
        }
    }
}

pub fn style_to_attrs(style: &GhosttyStyle, attrs: &mut CellAttributes) {
    let flags = style.flags;
    attrs.set_foreground(style_color(&style.fg_color));
    attrs.set_background(style_color(&style.bg_color));
    if style.underline_color.tag as u32 != GHOSTTY_STYLE_COLOR_NONE {
        attrs.set_underline_color(style_color(&style.underline_color));
    }
    let bold = flags & GHOSTTY_STYLE_FLAG_BOLD != 0;
    let faint = flags & GHOSTTY_STYLE_FLAG_FAINT != 0;
    attrs.set_intensity(match (bold, faint) {
        (true, _) => Intensity::Bold,
        (false, true) => Intensity::Half,
        _ => Intensity::Normal,
    });
    attrs.set_italic(flags & GHOSTTY_STYLE_FLAG_ITALIC != 0);
    attrs.set_blink(if flags & GHOSTTY_STYLE_FLAG_BLINK != 0 {
        Blink::Slow
    } else {
        Blink::None
    });
    attrs.set_reverse(flags & GHOSTTY_STYLE_FLAG_INVERSE != 0);
    attrs.set_invisible(flags & GHOSTTY_STYLE_FLAG_INVISIBLE != 0);
    attrs.set_strikethrough(flags & GHOSTTY_STYLE_FLAG_STRIKETHROUGH != 0);
    attrs.set_overline(flags & GHOSTTY_STYLE_FLAG_OVERLINE != 0);
    attrs.set_underline(match ghostty_style_underline(style) {
        GHOSTTY_SGR_UNDERLINE_SINGLE => Underline::Single,
        GHOSTTY_SGR_UNDERLINE_DOUBLE => Underline::Double,
        GHOSTTY_SGR_UNDERLINE_CURLY => Underline::Curly,
        GHOSTTY_SGR_UNDERLINE_DOTTED => Underline::Dotted,
        GHOSTTY_SGR_UNDERLINE_DASHED => Underline::Dashed,
        _ => Underline::None,
    });
}

#[inline]
fn cell_get<T: Default>(cell: GhosttyCell, what: GhosttyCellData) -> T {
    let mut v = T::default();
    unsafe {
        ghostty_cell_get(cell, what, &mut v as *mut T as *mut _);
    }
    v
}

#[inline]
fn row_get<T: Default>(row: GhosttyRow, what: GhosttyRowData) -> T {
    let mut v = T::default();
    unsafe {
        ghostty_row_get(row, what, &mut v as *mut T as *mut _);
    }
    v
}

/// Read a single physical row starting at `start` (which must refer to
/// column 0 of the row) into a wezterm `Line`.
///
/// # Safety
/// `start` must be a grid ref freshly obtained from the terminal, with no
/// intervening mutation of the terminal.
pub unsafe fn read_row(
    start: GhosttyGridRef,
    cols: u16,
    seqno: SequenceNo,
    links: &mut HyperlinkCache,
) -> Line {
    let mut row: GhosttyRow = 0;
    ghostty_grid_ref_row(&start, &mut row);
    let wrapped: bool = row_get(row, GHOSTTY_ROW_DATA_WRAP);
    let row_styled: bool = row_get(row, GHOSTTY_ROW_DATA_STYLED);
    let row_has_links: bool = row_get(row, GHOSTTY_ROW_DATA_HYPERLINK);
    let row_has_graphemes: bool = row_get(row, GHOSTTY_ROW_DATA_GRAPHEME);

    let mut cells: Vec<Cell> = Vec::with_capacity(cols as usize);
    let mut text = String::new();
    let mut cps = [0u32; 32];
    let mut uri_buf: Vec<u8> = Vec::new();
    let mut last_wide_attrs = CellAttributes::blank();

    for x in 0..cols {
        let mut gref = start;
        gref.x = start.x + x;
        let mut cell: GhosttyCell = 0;
        if ghostty_grid_ref_cell(&gref, &mut cell) != GHOSTTY_SUCCESS {
            break;
        }

        let wide: GhosttyCellWide = cell_get(cell, GHOSTTY_CELL_DATA_WIDE);
        if wide == GHOSTTY_CELL_WIDE_SPACER_TAIL {
            // wezterm stores a blank cell after a double-width cell
            cells.push(Cell::blank_with_attrs(last_wide_attrs.clone()));
            continue;
        }

        let mut attrs = CellAttributes::blank();
        if row_styled && cell_get::<bool>(cell, GHOSTTY_CELL_DATA_HAS_STYLING) {
            let mut style: GhosttyStyle = std::mem::zeroed();
            ghostty_grid_ref_style(&gref, &mut style);
            style_to_attrs(&style, &mut attrs);
        }

        let content: GhosttyCellContentTag = cell_get(cell, GHOSTTY_CELL_DATA_CONTENT_TAG);
        match content {
            GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE => {
                let idx: GhosttyColorPaletteIndex = cell_get(cell, GHOSTTY_CELL_DATA_COLOR_PALETTE);
                attrs.set_background(ColorAttribute::PaletteIndex(idx));
            }
            GHOSTTY_CELL_CONTENT_BG_COLOR_RGB => {
                let rgb: GhosttyColorRgbDefault = cell_get(cell, GHOSTTY_CELL_DATA_COLOR_RGB);
                attrs.set_background(ColorAttribute::TrueColorWithDefaultFallback(
                    SrgbaTuple::from((rgb.0.r, rgb.0.g, rgb.0.b)),
                ));
            }
            _ => {}
        }

        let semantic: GhosttyCellSemanticContent =
            cell_get(cell, GHOSTTY_CELL_DATA_SEMANTIC_CONTENT);
        attrs.set_semantic_type(match semantic {
            GHOSTTY_CELL_SEMANTIC_INPUT => SemanticType::Input,
            GHOSTTY_CELL_SEMANTIC_PROMPT => SemanticType::Prompt,
            _ => SemanticType::Output,
        });

        if row_has_links && cell_get::<bool>(cell, GHOSTTY_CELL_DATA_HAS_HYPERLINK) {
            let mut needed = 0usize;
            ghostty_grid_ref_hyperlink_uri(&gref, std::ptr::null_mut(), 0, &mut needed);
            if needed > 0 {
                uri_buf.resize(needed, 0);
                let mut written = 0usize;
                if ghostty_grid_ref_hyperlink_uri(
                    &gref,
                    uri_buf.as_mut_ptr(),
                    uri_buf.len(),
                    &mut written,
                ) == GHOSTTY_SUCCESS
                {
                    if let Ok(uri) = std::str::from_utf8(&uri_buf[..written]) {
                        attrs.set_hyperlink(Some(links.get(uri)));
                    }
                }
            }
        }

        let width = if wide == GHOSTTY_CELL_WIDE_WIDE { 2 } else { 1 };

        let has_text: bool = cell_get(cell, GHOSTTY_CELL_DATA_HAS_TEXT);
        if !has_text || wide == GHOSTTY_CELL_WIDE_SPACER_HEAD {
            cells.push(Cell::blank_with_attrs(attrs));
            continue;
        }

        text.clear();
        if row_has_graphemes && content == GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME {
            let mut written = 0usize;
            let res = ghostty_grid_ref_graphemes(&gref, cps.as_mut_ptr(), cps.len(), &mut written);
            if res == GHOSTTY_SUCCESS {
                for &cp in &cps[..written] {
                    text.push(char::from_u32(cp).unwrap_or(' '));
                }
            } else {
                let mut big = vec![0u32; written];
                ghostty_grid_ref_graphemes(&gref, big.as_mut_ptr(), big.len(), &mut written);
                for &cp in &big[..written] {
                    text.push(char::from_u32(cp).unwrap_or(' '));
                }
            }
        } else {
            let cp: u32 = cell_get(cell, GHOSTTY_CELL_DATA_CODEPOINT);
            text.push(char::from_u32(cp).unwrap_or(' '));
        }

        if width == 2 {
            last_wide_attrs = attrs.clone();
        }
        cells.push(Cell::new_grapheme_with_width(&text, width, attrs));
    }

    // Make sure the line spans the full width so that wezterm doesn't
    // see a ragged line (which matters for selection and reflow)
    while cells.len() < cols as usize {
        cells.push(Cell::blank());
    }

    let mut line = Line::from_cells(cells, seqno);
    if wrapped {
        line.set_last_cell_was_wrapped(true, seqno);
    }
    line
}

/// GhosttyColorRgb doesn't implement Default; this newtype does so that
/// we can use it with `cell_get`.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct GhosttyColorRgbDefault(pub GhosttyColorRgb);
impl Default for GhosttyColorRgbDefault {
    fn default() -> Self {
        Self(GhosttyColorRgb { r: 0, g: 0, b: 0 })
    }
}
