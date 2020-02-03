//! Exporting of layouts into _PDF_ documents.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use tide::{PdfWriter, Rect, Ref, Trailer, Version};
use tide::content::Content;
use tide::doc::{Catalog, Page, PageTree, Resource, Text};
use tide::font::{
    CIDFont, CIDFontType, CIDSystemInfo, FontDescriptor, FontFlags, Type0Font,
    CMap, CMapEncoding, FontStream, GlyphUnit, WidthRecord,
};

use toddle::{Font, OwnedFont, LoadError};
use toddle::types::Tag;
use toddle::query::FontIndex;
use toddle::tables::{
    CharMap, Header, HorizontalMetrics, MacStyleFlags,
    Name, NameEntry, Post, OS2,
};

use crate::GlobalFontLoader;
use crate::layout::{MultiLayout, Layout, LayoutAction};
use crate::size::Size;


/// Export a layouted list of boxes. The same font loader as used for
/// layouting needs to be passed in here since the layout only contains
/// indices referencing the loaded fonts. The raw PDF ist written into the
/// target writable, returning the number of bytes written.
pub fn export<W: Write>(
    layout: &MultiLayout,
    loader: &GlobalFontLoader,
    target: W,
) -> PdfResult<usize> {
    PdfExporter::new(layout, loader, target)?.write()
}

/// The data relevant to the export of one document.
struct PdfExporter<'a, W: Write> {
    writer: PdfWriter<W>,
    layouts: &'a MultiLayout,

    /// Since we cross-reference pages and fonts with their IDs already in the document
    /// catalog, we need to know exactly which ID is used for what from the beginning.
    /// Thus, we compute a range for each category of object and stored these here.
    offsets: Offsets,

    /// Each font has got an index from the font loader. However, these may not be
    /// ascending from zero. Since we want to use the indices 0 .. num_fonts we
    /// go through all font usages and assign a new index for each used font.
    /// This remapping is stored here because we need it when converting the
    /// layout actions in `ExportProcess::write_page`.
    font_remap: HashMap<FontIndex, usize>,

    /// These are the fonts sorted by their *new* ids, that is, the values of `font_remap`.
    fonts: Vec<OwnedFont>,
}

/// Indicates which range of PDF IDs will be used for which contents.
struct Offsets {
    catalog: Ref,
    page_tree: Ref,
    pages: (Ref, Ref),
    contents: (Ref, Ref),
    fonts: (Ref, Ref),
}

impl<'a, W: Write> PdfExporter<'a, W> {
    /// Prepare the export. Only once [`ExportProcess::write`] is called the
    /// writing really happens.
    fn new(
        layouts: &'a MultiLayout,
        font_loader: &GlobalFontLoader,
        target: W,
    ) -> PdfResult<PdfExporter<'a, W>> {
        let (fonts, font_remap) = Self::subset_fonts(layouts, font_loader)?;
        let offsets = Self::calculate_offsets(layouts.len(), fonts.len());

        Ok(PdfExporter {
            writer: PdfWriter::new(target),
            layouts,
            offsets,
            font_remap,
            fonts,
        })
    }

    /// Subsets all fonts and assign a new PDF-internal index to each one. The
    /// returned hash map maps the old indices (used by the layouts) to the new
    /// one used in the PDF. The new ones index into the returned vector of
    /// owned fonts.
    fn subset_fonts(
        layouts: &'a MultiLayout,
        font_loader: &GlobalFontLoader,
    ) -> PdfResult<(Vec<OwnedFont>, HashMap<FontIndex, usize>)> {
        let mut fonts = Vec::new();
        let mut font_chars: HashMap<FontIndex, HashSet<char>> = HashMap::new();
        let mut old_to_new: HashMap<FontIndex, usize> = HashMap::new();
        let mut new_to_old: HashMap<usize, FontIndex> = HashMap::new();
        let mut active_font = FontIndex::MAX;

        // We want to find out which fonts are used at all and which chars are
        // used for those. We use this information to create subsetted fonts.
        for layout in layouts {
            for action in &layout.actions {
                match action {
                    LayoutAction::WriteText(text) => {
                        font_chars
                            .entry(active_font)
                            .or_insert_with(HashSet::new)
                            .extend(text.chars());
                    },

                    LayoutAction::SetFont(index, _) => {
                        active_font = *index;

                        let next_id = old_to_new.len();
                        let new_id = *old_to_new
                            .entry(active_font)
                            .or_insert(next_id);

                        new_to_old
                            .entry(new_id)
                            .or_insert(active_font);
                    },

                    _ => {}
                }
            }
        }

        let num_fonts = old_to_new.len();
        let mut font_loader = font_loader.borrow_mut();

        // All tables not listed here are dropped.
        let tables: Vec<_> = [
            b"name", b"OS/2", b"post", b"head", b"hhea", b"hmtx", b"maxp",
            b"cmap", b"cvt ", b"fpgm", b"prep", b"loca", b"glyf",
        ].iter().map(|&s| Tag(*s)).collect();

        // Do the subsetting.
        for index in 0 .. num_fonts {
            let old_index = new_to_old[&index];
            let font = font_loader.get_with_index(old_index);

            let chars = font_chars[&old_index].iter().cloned();
            let subsetted = match font.subsetted(chars, tables.iter().copied()) {
                Ok(data) => Font::from_bytes(data)?,
                Err(_) => font.clone(),
            };

            fonts.push(subsetted);
        }

        Ok((fonts, old_to_new))
    }

    /// We need to know in advance which IDs to use for which objects to
    /// cross-reference them. Therefore, we calculate the indices in the
    /// beginning.
    fn calculate_offsets(layout_count: usize, font_count: usize) -> Offsets {
        let catalog = 1;
        let page_tree = catalog + 1;
        let pages = (page_tree + 1, page_tree + layout_count as Ref);
        let contents = (pages.1 + 1, pages.1 + layout_count as Ref);
        let font_offsets = (contents.1 + 1, contents.1 + 5 * font_count as Ref);

        Offsets {
            catalog,
            page_tree,
            pages,
            contents,
            fonts: font_offsets,
        }
    }

    /// Write everything (writing entry point).
    fn write(&mut self) -> PdfResult<usize> {
        self.writer.write_header(Version::new(1, 7))?;
        self.write_preface()?;
        self.write_pages()?;
        self.write_fonts()?;
        self.writer.write_xref_table()?;
        self.writer.write_trailer(Trailer::new(self.offsets.catalog))?;
        Ok(self.writer.written())
    }

    /// Write the document catalog and page tree.
    fn write_preface(&mut self) -> PdfResult<()> {
        // The document catalog.
        self.writer.write_obj(self.offsets.catalog, &Catalog::new(self.offsets.page_tree))?;

        // The font resources.
        let start = self.offsets.fonts.0;
        const NUM_OBJECTS_PER_FONT: usize = 5;
        let fonts = (0 .. self.fonts.len()).map(|i| {
            Resource::Font((i + 1) as u32, start + (NUM_OBJECTS_PER_FONT * i) as u32)
        });

        // The root page tree.
        self.writer.write_obj(
            self.offsets.page_tree,
            PageTree::new()
                .kids(ids(self.offsets.pages))
                .resources(fonts),
        )?;

        // The page objects (non-root nodes in the page tree).
        let iter = ids(self.offsets.pages)
            .zip(ids(self.offsets.contents))
            .zip(self.layouts);

        for ((page_id, content_id), page) in iter {
            let rect = Rect::new(
                0.0,
                0.0,
                page.dimensions.x.to_pt(),
                page.dimensions.y.to_pt(),
            );

            self.writer.write_obj(
                page_id,
                Page::new(self.offsets.page_tree)
                    .media_box(rect)
                    .content(content_id),
            )?;
        }

        Ok(())
    }

    /// Write the contents of all pages.
    fn write_pages(&mut self) -> PdfResult<()> {
        for (id, page) in ids(self.offsets.contents).zip(self.layouts) {
            self.write_page(id, &page)?;
        }
        Ok(())
    }

    /// Write the content of a page.
    fn write_page(&mut self, id: u32, page: &Layout) -> PdfResult<()> {
        // Moves and font switches are always cached and only flushed once
        // needed.
        let mut text = Text::new();
        let mut active_font = (std::usize::MAX, 0.0);
        let mut next_pos = None;

        for action in &page.actions {
            match action {
                LayoutAction::MoveAbsolute(pos) => {
                    next_pos = Some(*pos);
                },

                LayoutAction::SetFont(id, size) => {
                    active_font = (self.font_remap[id], size.to_pt());
                    text.tf(active_font.0 as u32 + 1, size.to_pt());
                }

                LayoutAction::WriteText(string) => {
                    if let Some(pos) = next_pos.take() {
                        let x = pos.x.to_pt();
                        let y = (page.dimensions.y - pos.y - Size::pt(active_font.1)).to_pt();
                        text.tm(1.0, 0.0, 0.0, 1.0, x, y);
                    }

                    text.tj(self.fonts[active_font.0].encode_text(&string)?);
                },

                LayoutAction::DebugBox(_) => {}
            }
        }

        self.writer.write_obj(id, &text.to_stream())?;

        Ok(())
    }

    /// Write all the fonts.
    fn write_fonts(&mut self) -> PdfResult<()> {
        let mut id = self.offsets.fonts.0;

        for font in &mut self.fonts {
            // ---------------------------------------------
            // Extract information from the name table.
            let name = font
                .read_table::<Name>()?
                .get_decoded(NameEntry::PostScriptName)
                .unwrap_or_else(|| "unknown".to_string());

            let base_font = format!("ABCDEF+{}", name);
            let system_info = CIDSystemInfo::new("Adobe", "Identity", 0);

            // Write the base font object referencing the CID font.
            self.writer.write_obj(
                id,
                Type0Font::new(
                    base_font.clone(),
                    CMapEncoding::Predefined("Identity-H".to_string()),
                    id + 1,
                )
                .to_unicode(id + 3),
            )?;

            // ---------------------------------------------
            // Extract information from the head and hmtx tables.
            let head = font.read_table::<Header>()?;

            let font_unit_ratio = 1.0 / (head.units_per_em as f32);
            let font_unit_to_size = |x| Size::pt(font_unit_ratio * x);
            let font_unit_to_glyph_unit = |fu| {
                let size = font_unit_to_size(fu);
                (1000.0 * size.to_pt()).round() as GlyphUnit
            };

            let italic = head.mac_style.contains(MacStyleFlags::ITALIC);
            let bounding_box = Rect::new(
                font_unit_to_glyph_unit(head.x_min as f32),
                font_unit_to_glyph_unit(head.y_min as f32),
                font_unit_to_glyph_unit(head.x_max as f32),
                font_unit_to_glyph_unit(head.y_max as f32),
            );

            // Transform the width into PDF units.
            let widths: Vec<_> = font
                .read_table::<HorizontalMetrics>()?
                .metrics
                .iter()
                .map(|m| font_unit_to_glyph_unit(m.advance_width as f32))
                .collect();

            // Write the CID font referencing the font descriptor.
            self.writer.write_obj(
                id + 1,
                CIDFont::new(
                    CIDFontType::Type2,
                    base_font.clone(),
                    system_info.clone(),
                    id + 2,
                )
                .widths(vec![WidthRecord::start(0, widths)]),
            )?;

            // ---------------------------------------------
            // Extract information from the post table.
            let post = font.read_table::<Post>()?;
            let fixed_pitch = post.is_fixed_pitch;
            let italic_angle = post.italic_angle.to_f32();

            let mut flags = FontFlags::empty();
            flags.set(FontFlags::SERIF, name.contains("Serif"));
            flags.set(FontFlags::FIXED_PITCH, fixed_pitch);
            flags.set(FontFlags::ITALIC, italic);
            flags.insert(FontFlags::SYMBOLIC);
            flags.insert(FontFlags::SMALL_CAP);

            // ---------------------------------------------
            // Extract information from the OS/2 table.
            let os2 = font.read_table::<OS2>()?;

            // Write the font descriptor (contains the global information about the font).
            self.writer.write_obj(id + 2, FontDescriptor::new(base_font, flags, italic_angle)
                .font_bbox(bounding_box)
                .ascent(font_unit_to_glyph_unit(os2.s_typo_ascender as f32))
                .descent(font_unit_to_glyph_unit(os2.s_typo_descender as f32))
                .cap_height(font_unit_to_glyph_unit(
                    os2.s_cap_height.unwrap_or(os2.s_typo_ascender) as f32,
                ))
                .stem_v((10.0 + 0.244 * (os2.us_weight_class as f32 - 50.0)) as GlyphUnit)
                .font_file_2(id + 4)
            )?;

            // ---------------------------------------------
            // Extract information from the cmap table.

            let cmap = CMap::new("Custom", system_info, font
                .read_table::<CharMap>()?
                .mapping
                .iter()
                .map(|(&c, &cid)| (cid, c))
            );

            // Write the CMap, which maps glyphs to unicode codepoints.
            self.writer.write_obj(id + 3, &cmap)?;

            // ---------------------------------------------
            // Finally write the subsetted font program.

            self.writer.write_obj(id + 4, &FontStream::new(font.data().get_ref()))?;

            id += 5;
        }

        Ok(())
    }
}

/// Create an iterator from a reference pair.
fn ids((start, end): (Ref, Ref)) -> impl Iterator<Item = Ref> {
    start ..= end
}

/// The error type for _PDF_ exporting.
pub enum PdfExportError {
    /// An error occured while subsetting the font for the _PDF_.
    Font(LoadError),
    /// An I/O Error on the underlying writable.
    Io(io::Error),
}

error_type! {
    self: PdfExportError,
    res: PdfResult,
    show: f => match self {
        PdfExportError::Font(err) => err.fmt(f),
        PdfExportError::Io(err) => err.fmt(f),
    },
    source: match self {
        PdfExportError::Font(err) => Some(err),
        PdfExportError::Io(err) => Some(err),
    },
    from: (err: io::Error, PdfExportError::Io(err)),
    from: (err: LoadError, PdfExportError::Font(err)),
}
