#![allow(unused_variables, dead_code)]

use webrender::api::{LayoutPixel, GlyphInstance};
use euclid::{Length, TypedRect, TypedSize2D, TypedPoint2D};
use rusttype::{Font, Scale, GlyphId};
use {
    resources::AppResources,
    display_list::TextInfo,
    css_parser::{
        TextAlignmentHorz, FontSize, BackgroundColor,
        FontId, TextAlignmentVert, LineHeight, LayoutOverflow
    },
    text_cache::{TextId, TextCache},
};

/// Rusttype has a certain sizing hack, I have no idea where this number comes from
/// Without this adjustment, we won't have the correct horizontal spacing
pub(crate) const RUSTTYPE_SIZE_HACK: f32 = 72.0 / 41.0;

pub(crate) const PX_TO_PT: f32 = 72.0 / 96.0;

/// Words are a collection of glyph information, i.e. how much 
/// horizontal space each of the words in a text block and how much 
/// space each individual glyph take up.
///
/// This is important for calculating metrics such as the minimal 
/// bounding box of a block of text, for example - without actually
/// acessing the font at all.
///
/// Be careful when caching this - the `Words` are independent of the
/// original font, so be sure to note the font ID if you cache this struct.
#[derive(Debug, Clone)]
pub struct Words(Vec<SemanticWordItem>);

/// A `Word` contains information about the layout of a single word
#[derive(Debug, Clone)]
pub struct Word {
    /// Glyphs, positions are relative to the first character of the word
    pub glyphs: Vec<GlyphInstance>,
    /// The sum of the width of all the characters
    pub total_width: f32,
}

/// Either a white-space delimited word, tab or return character
#[derive(Debug, Clone)]
pub enum SemanticWordItem {
    /// Encountered a word (delimited by spaces)
    Word(Word),
    // `\t` or `x09`
    Tab,
    /// `\r`, `\n` or `\r\n`, escaped: `\x0D`, `\x0A` or `\x0D\x0A`
    Return,
}

impl SemanticWordItem {
    pub fn is_return(&self) -> bool {
        use self::SemanticWordItem::*;
        match self {
            Return => true,
            _ => false,
        }
    }
}

/// Returned struct for the pass-1 text run test.
///
/// Once the text is parsed and split into words + normalized, we can calculate
/// (without looking at the text itself), if the text overflows the parent rectangle,
/// in order to determine if we need to show a scroll bar.
#[derive(Debug, Clone)]
pub(crate) struct TextOverflowPass1 {
    /// Is the text overflowing in the horizontal direction?
    pub(crate) horizontal: TextOverflow,
    /// Is the text overflowing in the vertical direction?
    pub(crate) vertical: TextOverflow,
}

/// In the case that we do overflow the rectangle (in any direction),
/// we need to now re-calculate the positions for the words (because of the reduced available
/// space that is now taken up by the scrollbars).
#[derive(Debug, Copy, Clone)]
pub(crate) struct TextOverflowPass2 {
    /// Is the text overflowing in the horizontal direction?
    pub(crate) horizontal: TextOverflow,
    /// Is the text overflowing in the vertical direction?
    pub(crate) vertical: TextOverflow,
}

/// These metrics are important for showing the scrollbars
#[derive(Debug, Copy, Clone)]
pub(crate) enum TextOverflow {
    /// Text is overflowing, by how much (in pixels)?
    /// Necessary for determining the size of the scrollbar
    IsOverflowing(f32),
    /// Text is in bounds, how much space (in pixels) is available until
    /// the edge of the rectangle? Necessary for centering / aligning text vertically.
    InBounds(f32),
}

impl TextOverflow {
    pub fn is_overflowing(&self) -> bool {
        use self::TextOverflow::*;
        match self {
            IsOverflowing(_) => true,
            InBounds(_) => false,
        }
    }
}

#[derive(Debug, Copy, Clone)]
struct HarfbuzzAdjustment(pub f32);

#[derive(Debug, Copy, Clone)]
struct KnuthPlassAdjustment(pub f32);

/// Holds info necessary for layouting / styling scrollbars
#[derive(Debug, Clone)]
pub(crate) struct ScrollbarInfo {
    /// Total width (for vertical scrollbars) or height (for horizontal scrollbars)
    /// of the scrollbar in pixels
    pub(crate) width: usize,
    /// Padding of the scrollbar, in pixels. The inner bar is `width - padding` pixels wide.
    pub(crate) padding: usize,
    /// Style of the scrollbar (how to draw it)
    pub(crate) bar_color: BackgroundColor,
    /// How to draw the "up / down" arrows
    pub(crate) triangle_color: BackgroundColor,
    /// Style of the scrollbar background
    pub(crate) background_color: BackgroundColor,
}

/// Temporary struct that contains various metrics related to a font - 
/// useful so we don't have to access the font to look up certain widths
#[derive(Debug, Copy, Clone)]
pub struct FontMetrics {
    /// Width of the space character
    space_width: f32,
    /// Usually 4 * space_width
    tab_width: f32,
    /// font_size * line_height
    vertical_advance: f32,
    /// Offset of the font from the top of the bounding rectangle
    offset_top: f32,
    /// Font size (for rusttype, includes the `RUSTTYPE_SIZE_HACK`) in **pt** (not px)
    /// Used for vertical layouting (since it includes the line height)
    font_size_with_line_height: Scale,
    /// Same as `font_size_with_line_height` but without the line height incorporated.
    /// Used for horizontal layouting
    font_size_no_line_height: Scale,
}

/// ## Inputs
///
/// - `app_resources`: This is only used for caching - if you already have a `LargeString`, which
///    stores the word boundaries for the given font, we don't have to re-calculate the font metrics again.
/// - `bounds`: The bounds of the rectangle containing the text
/// - `horiz_alignment`: Usually parsed from the `text-align` attribute: horizontal alignment of the text
/// - `vert_alignment`: Usually parsed from the `align-items` attribute on the parent node
///    or the `align-self` on the child node: horizontal alignment of the text
/// - `font`: The font to use for layouting (only the ID)
/// - `font_size`: The font size (without line height)
/// - `line_height`: The line height (100% = 1.0). I.e. `line-height = 1.2;` scales the text vertically by 1.2x
/// - `text`: The actual text to layout. Will be unicode-normalized after the Unicode Normalization Form C
///   (canonical decomposition followed by canonical composition).
/// - `overflow`: If the scrollbars should be show, parsed from the `overflow-{x / y}` fields
/// - `scrollbar_info`: Mostly used to reserve space for the scrollbar, if necessary.
///
/// ## Returns
///
/// - `Vec<GlyphInstance>`: The layouted glyphs. If a scrollbar is necessary, they will be layouted so that
///   the scrollbar has space to the left or bottom (so it doesn't overlay the text)
/// - `TextOverflowPass2`: This is internally used for aligning text (horizontally / vertically), but
///   it is necessary for drawing the scrollbars later on, to determine the height of the bar. Contains
///   info about if the text has overflown the rectangle, and if yes, by how many pixels
pub(crate) fn get_glyphs(
    app_resources: &mut AppResources,
    bounds: &TypedRect<f32, LayoutPixel>,
    horiz_alignment: TextAlignmentHorz,
    vert_alignment: TextAlignmentVert,
    target_font_id: &FontId,
    target_font_size: &FontSize,
    line_height: Option<LineHeight>,
    text: &TextInfo,
    overflow: &LayoutOverflow,
    scrollbar_info: &ScrollbarInfo)
-> (Vec<GlyphInstance>, TextOverflowPass2)
{
    use css_parser::{TextOverflowBehaviour, TextOverflowBehaviourInner};

    let target_font = app_resources.font_data.get(target_font_id).expect("Drawing with invalid font!");

    let font_metrics = calculate_font_metrics(&target_font.0, target_font_size, line_height);

    // (1) Split the text into semantic items (word, tab or newline) OR get the cached
    // text and scale it accordingly.
    //
    // This function also normalizes the unicode characters and calculates kerning.
    //
    // NOTE: This should be revisited, the caching does unnecessary cloning.
    let words_owned;
    let words = match text {
        TextInfo::Cached(text_id) => {
            get_words_cached(text_id, &target_font.0, target_font_id, target_font_size, font_metrics.font_size_no_line_height, &mut app_resources.text_cache)
        },
        TextInfo::Uncached(s) => {
            words_owned = split_text_into_words(s, &target_font.0, font_metrics.font_size_no_line_height);
            &words_owned
        },
    };

    // (2) Calculate the additions / subtractions that have to be take into account
    // let harfbuzz_adjustments = calculate_harfbuzz_adjustments(&text, &target_font.0);

    // (3) Determine if the words will overflow the bounding rectangle
    let overflow_pass_1 = estimate_overflow_pass_1(&words, &bounds.size, &font_metrics, &overflow);

    // (4) If the lines overflow, subtract the space needed for the scrollbars and calculate the length
    // again (TODO: already layout characters here?)
    let (new_size, overflow_pass_2) =
        estimate_overflow_pass_2(&words, &bounds.size, &font_metrics, &overflow, scrollbar_info, overflow_pass_1);

    let max_horizontal_text_width = if overflow.allows_horizontal_overflow() { None } else { Some(new_size.width) };

    // (5) Align text to the left, initial layout of glyphs
    let (mut positioned_glyphs, line_break_offsets, _, _) =
        words_to_left_aligned_glyphs(words, &target_font.0, max_horizontal_text_width, &font_metrics);

    // (6) Add the harfbuzz adjustments to the positioned glyphs
    // apply_harfbuzz_adjustments(&mut positioned_glyphs, harfbuzz_adjustments);

    // (7) Calculate the Knuth-Plass adjustments for the (now layouted) glyphs
    let knuth_plass_adjustments = calculate_knuth_plass_adjustments(&positioned_glyphs, &line_break_offsets);

    // (8) Add the Knuth-Plass adjustments to the positioned glyphs
    apply_knuth_plass_adjustments(&mut positioned_glyphs, knuth_plass_adjustments);

    // (9) Align text horizontally (early return if left-aligned)
    align_text_horz(horiz_alignment, &mut positioned_glyphs, &line_break_offsets, &overflow_pass_2);

    // (10) Align text vertically (early return if text overflows)
    align_text_vert(vert_alignment, &mut positioned_glyphs, &line_break_offsets, &overflow_pass_2);

    // (11) Add the self.origin to all the glyphs to bring them from glyph space into world space
    add_origin(&mut positioned_glyphs, bounds.origin.x, bounds.origin.y);

    (positioned_glyphs, overflow_pass_2)
}

impl FontMetrics {
    /// Given a font, font size and line height, calculates the `FontMetrics` necessary
    /// which are later used to layout a block of text
    pub fn new<'a>(font: &Font<'a>, font_size: &FontSize, line_height: Option<LineHeight>) -> Self {
        calculate_font_metrics(font, font_size, line_height)
    }
}

fn calculate_font_metrics<'a>(font: &Font<'a>, font_size: &FontSize, line_height: Option<LineHeight>) -> FontMetrics {

    let font_size_f32 = font_size.0.to_pixels() * RUSTTYPE_SIZE_HACK * PX_TO_PT;
    let line_height = match line_height { Some(lh) => (lh.0).number, None => 1.0 };
    let font_size_with_line_height = Scale::uniform(font_size_f32 * line_height);
    let font_size_no_line_height = Scale::uniform(font_size_f32);

    let space_width = font.glyph(' ').scaled(font_size_no_line_height).h_metrics().advance_width;
    let tab_width = 4.0 * space_width; // TODO: make this configurable

    let v_metrics_scaled = font.v_metrics(font_size_with_line_height);
    let v_advance_scaled = v_metrics_scaled.ascent - v_metrics_scaled.descent + v_metrics_scaled.line_gap;
    let offset_top = v_metrics_scaled.ascent / 2.0;

    FontMetrics {
        vertical_advance: v_advance_scaled,
        space_width,
        tab_width,
        offset_top,
        font_size_with_line_height,
        font_size_no_line_height,
    }
}

fn get_words_cached<'a>(
    text_id: &TextId,
    font: &Font<'a>,
    font_id: &FontId,
    font_size: &FontSize,
    font_size_no_line_height: Scale,
    text_cache: &'a mut TextCache)
-> &'a Words
{
    use std::collections::hash_map::Entry::*;
    use FastHashMap;

    let mut should_words_be_scaled = false;

    match text_cache.cached_strings.entry(*text_id) {
        Occupied(mut font_hash_map) => {

            let font_size_map = font_hash_map.get_mut().entry(font_id.clone()).or_insert_with(|| FastHashMap::default());
            let is_new_font = font_size_map.is_empty();

            match font_size_map.entry(*font_size) {
                Occupied(existing_font_size_words) => { }
                Vacant(v) => {
                    if is_new_font {
                        v.insert(split_text_into_words(&text_cache.string_cache[text_id], font, font_size_no_line_height));
                    } else {
                        // If we can get the words from any other size, we can just scale them here
                        // ex. if an existing font size gets scaled.
                       should_words_be_scaled = true;
                    }
                }
            }
        },
        Vacant(_) => { },
    }

    // We have an entry in the font size -> words cache already, but it's not the right font size
    // instead of recalculating the words, we simply scale them up.
    if should_words_be_scaled {
        let words_cloned = {
            let font_size_map = &text_cache.cached_strings[&text_id][&font_id];
            let (old_font_size, next_words_for_font) = font_size_map.iter().next().unwrap();
            let mut words_cloned: Words = next_words_for_font.clone();
            let scale_factor = font_size.0.to_pixels() / old_font_size.0.to_pixels();

            scale_words(&mut words_cloned, scale_factor);
            words_cloned
        };

        text_cache.cached_strings.get_mut(&text_id).unwrap().get_mut(&font_id).unwrap().insert(*font_size, words_cloned);
    }

    text_cache.cached_strings.get(&text_id).unwrap().get(&font_id).unwrap().get(&font_size).unwrap()
}

fn scale_words(words: &mut Words, scale_factor: f32) {
    // Scale the horizontal width of the words to match the new font size
    // Since each word has a local origin (i.e. the first character of each word
    // is at (0, 0)), we can simply scale the X position of each glyph by a
    // certain factor.
    //
    // So if we previously had a 12pt font and now a 13pt font,
    // we simply scale each glyph position by 13 / 12. This is faster than
    // re-calculating the font metrics (from Rusttype) each time we scale a
    // large amount of text.
    for word in words.0.iter_mut() {
        if let SemanticWordItem::Word(ref mut w) = word {
            w.glyphs.iter_mut().for_each(|g| g.point.x *= scale_factor);
            w.total_width *= scale_factor;
        }
    }
}

/// This function is also used in the `text_cache` module for caching large strings.
///
/// It is one of the most expensive functions, use with care.
pub(crate) fn split_text_into_words<'a>(text: &str, font: &Font<'a>, font_size: Scale)
-> Words
{
    use unicode_normalization::UnicodeNormalization;

    let mut words = Vec::new();

    let mut word_caret = 0.0;
    let mut cur_word_length = 0.0;
    let mut chars_in_this_word = Vec::new();
    let mut glyphs_in_this_word = Vec::new();
    let mut last_glyph = None;

    fn end_word(words: &mut Vec<SemanticWordItem>,
                glyphs_in_this_word: &mut Vec<GlyphInstance>,
                cur_word_length: &mut f32,
                word_caret: &mut f32,
                last_glyph: &mut Option<GlyphId>)
    {
        // End of word
        words.push(SemanticWordItem::Word(Word {
            glyphs: glyphs_in_this_word.drain(..).collect(),
            total_width: *cur_word_length,
        }));

        // Reset everything
        *last_glyph = None;
        *word_caret = 0.0;
        *cur_word_length = 0.0;
    }

    for cur_char in text.nfc() {
        match cur_char {
            '\t' => {
                // End of word + tab
                if !chars_in_this_word.is_empty() {
                    end_word(
                        &mut words,
                        &mut glyphs_in_this_word,
                        &mut cur_word_length,
                        &mut word_caret,
                        &mut last_glyph);
                }
                words.push(SemanticWordItem::Tab);
            },
            '\n' => {
                // End of word + newline
                if !chars_in_this_word.is_empty() {
                    end_word(
                        &mut words,
                        &mut glyphs_in_this_word,
                        &mut cur_word_length,
                        &mut word_caret,
                        &mut last_glyph);
                }
                words.push(SemanticWordItem::Return);
            },
            ' ' => {
                if !chars_in_this_word.is_empty() {
                    end_word(
                        &mut words,
                        &mut glyphs_in_this_word,
                        &mut cur_word_length,
                        &mut word_caret,
                        &mut last_glyph);
                }
            },
            cur_char =>  {
                // Regular character
                use rusttype::Point;

                let g = font.glyph(cur_char);

                // calculate the real width
                let glyph_metrics = g.standalone().get_data().unwrap();
                let extents = glyph_metrics.extents.unwrap();
                let horz_rect_width = extents.max.x - extents.min.x;
                let horiz_advance = horz_rect_width as f32 * glyph_metrics.scale_for_1_pixel * font_size.x;

                let id = g.id();

                if let Some(last) = last_glyph {
                    word_caret += font.pair_kerning(font_size, last, id);
                }

                let word_caret_saved = word_caret;

                last_glyph = Some(id);
                word_caret += horiz_advance;
                cur_word_length += horiz_advance;

                glyphs_in_this_word.push(GlyphInstance {
                    index: id.0,
                    point: TypedPoint2D::new(word_caret_saved, 0.0),
                });

                chars_in_this_word.push(cur_char);
            }
        }
    }

    // Push last word
    if !chars_in_this_word.is_empty() {
        end_word(
            &mut words,
            &mut glyphs_in_this_word,
            &mut cur_word_length,
            &mut word_caret,
            &mut last_glyph);
    }

    Words(words)
}

// First pass: calculate if the words will overflow (using the tabs)
#[inline(always)]
fn estimate_overflow_pass_1(
    words: &Words,
    rect_dimensions: &TypedSize2D<f32, LayoutPixel>,
    font_metrics: &FontMetrics,
    overflow: &LayoutOverflow)
-> TextOverflowPass1
{
    use self::SemanticWordItem::*;
    
    let words = &words.0;

    let FontMetrics { space_width, tab_width, vertical_advance, offset_top, .. } = *font_metrics;

    let max_text_line_len_horizontal = 0.0;

    // Determine the maximum width and height that the text needs for layout

    // This is actually tricky. Horizontal scrollbars and vertical scrollbars
    // behave differently.
    //
    // Vertical scrollbars always show the length - when the
    // vertical length = the height of the rectangle, the scrollbar (the actual bar)
    // is 0.5x the height of the rectangle, aligned at the top.
    //
    // Horizontal scrollbars, on the other hand, are 1.0x the width of the rectangle,
    // when the width is filled.

    // TODO: this is duplicated code

    let mut max_hor_len = None;

    let vertical_length = {
        if overflow.allows_horizontal_overflow() {
            // If we can overflow horizontally, we only need to sum up the `Return`
            // characters, since the actual length of the line doesn't matter
            words.iter().filter(|w| w.is_return()).count() as f32 * vertical_advance
        } else {
            // TODO: should this be cached? The calculation is probably quick, but this
            // is essentially the same thing as we do in the actual text layout stage
            let mut max_line_cursor: f32 = 0.0;
            let mut cur_line_cursor = 0.0;
            let mut cur_line = 0;

            for w in words {
                match w {
                    Word(w) => {
                        if cur_line_cursor + w.total_width > rect_dimensions.width {
                            max_line_cursor = max_line_cursor.max(cur_line_cursor);
                            cur_line_cursor = 0.0;
                            cur_line += 1;
                        }
                        cur_line_cursor += w.total_width + space_width;
                    },
                    // TODO: also check for rect break after tabs? Kinda pointless, isn't it?
                    Tab => cur_line_cursor += tab_width,
                    Return => {
                        max_line_cursor = max_line_cursor.max(cur_line_cursor);
                        cur_line_cursor = 0.0;
                        cur_line += 1;
                    }
                }
            }

            max_hor_len = Some(cur_line_cursor);

            let cur_vertical = (cur_line as f32 * vertical_advance) + offset_top;

            cur_vertical
        }
    };

    let vertical_length = if vertical_length > rect_dimensions.height {
        TextOverflow::IsOverflowing(vertical_length - rect_dimensions.height)
    } else {
        TextOverflow::InBounds(rect_dimensions.height - vertical_length)
    };

    let horizontal_length = {

        let horz_max = if overflow.allows_horizontal_overflow() {

            let mut cur_line_cursor = 0.0;
            let mut max_line_cursor: f32 = 0.0;

            for w in words {
                match w {
                    Word(w) => cur_line_cursor += w.total_width,
                    Tab => cur_line_cursor += tab_width,
                    Return => {
                        max_line_cursor = max_line_cursor.max(cur_line_cursor);
                        cur_line_cursor = 0.0;
                    }
                }
            }

            max_line_cursor
        } else {
           max_hor_len.unwrap()
        };

        if horz_max > rect_dimensions.width {
            TextOverflow::IsOverflowing(horz_max - rect_dimensions.width)
        } else {
            TextOverflow::InBounds(rect_dimensions.width - horz_max)
        }
    };

    TextOverflowPass1 {
        horizontal: horizontal_length,
        vertical: vertical_length,
    }
}

#[inline(always)]
fn estimate_overflow_pass_2(
    words: &Words,
    rect_dimensions: &TypedSize2D<f32, LayoutPixel>,
    font_metrics: &FontMetrics,
    overflow: &LayoutOverflow,
    scrollbar_info: &ScrollbarInfo,
    pass1: TextOverflowPass1)
-> (TypedSize2D<f32, LayoutPixel>, TextOverflowPass2)
{    
    let FontMetrics { space_width, tab_width, vertical_advance, offset_top, .. } = *font_metrics;

    let mut new_size = *rect_dimensions;

    // TODO: make this 10px stylable

    // Subtract the space necessary for the scrollbars from the rectangle
    //
    // NOTE: this is switched around - if the text overflows vertically, the
    // scrollbar gets shown on the right edge, so we need to subtract from the
    // **width** of the rectangle.

    if pass1.horizontal.is_overflowing() {
        new_size.height -= scrollbar_info.width as f32;
    }

    if pass1.vertical.is_overflowing() {
        new_size.width -= scrollbar_info.width as f32;
    }

    // If the words are not overflowing, just take the result from the first pass
    let recalc_scrollbar_info = if pass1.horizontal.is_overflowing() || pass1.vertical.is_overflowing() {
        estimate_overflow_pass_1(words, &new_size, font_metrics, overflow)
    } else {
        pass1
    };

    (new_size, TextOverflowPass2 {
        horizontal: recalc_scrollbar_info.horizontal,
        vertical: recalc_scrollbar_info.vertical,
    })
}

#[inline(always)]
fn calculate_harfbuzz_adjustments<'a>(text: &str, font: &Font<'a>)
-> Vec<HarfbuzzAdjustment>
{
    use harfbuzz_rs::*;
    use harfbuzz_rs::rusttype::SetRustTypeFuncs;
    /*
    let path = "path/to/some/font_file.otf";
    let index = 0; //< face index in the font file
    let face = Face::from_file(path, index).unwrap();
    let mut font = Font::new(face);

    font.set_rusttype_funcs();

    let output = UnicodeBuffer::new().add_str(text).shape(&font, &[]);
    let positions = output.get_glyph_positions();
    let infos = output.get_glyph_infos();

    for (position, info) in positions.iter().zip(infos) {
        println!("gid: {:?}, cluster: {:?}, x_advance: {:?}, x_offset: {:?}, y_offset: {:?}",
            info.codepoint, info.cluster, position.x_advance, position.x_offset, position.y_offset);
    }
    */
    Vec::new() // TODO
}

/// If `max_horizontal_width` is `None`, it means that the text is allowed to overflow
/// the rectangle horizontally
#[inline(always)]
fn words_to_left_aligned_glyphs<'a>(
    words: &Words,
    font: &Font<'a>,
    max_horizontal_width: Option<f32>,
    font_metrics: &FontMetrics)
-> (Vec<GlyphInstance>, Vec<(usize, f32)>, f32, f32)
{
    let words = &words.0;

    let FontMetrics { space_width, tab_width, vertical_advance, offset_top, .. } = *font_metrics;

    // left_aligned_glyphs stores the X and Y coordinates of the positioned glyphs,
    // left-aligned
    let mut left_aligned_glyphs = Vec::<GlyphInstance>::new();

    enum WordCaretMax {
        SomeMaxWidth(f32),
        NoMaxWidth(f32),
    }

    // The line break offsets (neded for center- / right-aligned text contains:
    //
    // - The index of the glyph at which the line breaks
    // - How much space each line has (to the right edge of the containing rectangle)
    let mut line_break_offsets = Vec::<(usize, WordCaretMax)>::new();

    // word_caret is the current X position of the "pen" we are writing with
    let mut word_caret = 0.0;
    let mut current_line_num = 0;
    let mut max_word_caret = 0.0;

    for word in words {
        use self::SemanticWordItem::*;
        match word {
            Word(word) => {
                let text_overflows_rect = match max_horizontal_width {
                    Some(max) => word_caret + word.total_width > max,
                    // If we don't have a maximum horizontal width, the text can overflow the
                    // bounding rectangle in the horizontal direction
                    None => false,
                };

                if text_overflows_rect {
                    let space_until_horz_return = match max_horizontal_width {
                        Some(s) => WordCaretMax::SomeMaxWidth(s - word_caret),
                        None => WordCaretMax::NoMaxWidth(word_caret),
                    };
                    line_break_offsets.push((left_aligned_glyphs.len() - 1, space_until_horz_return));
                    if word_caret > max_word_caret {
                        max_word_caret = word_caret;
                    }
                    word_caret = 0.0;
                    current_line_num += 1;
                }

                for glyph in &word.glyphs {
                    let mut new_glyph = *glyph;
                    let push_x = word_caret;
                    let push_y = (current_line_num as f32 * vertical_advance) + offset_top;
                    new_glyph.point.x += push_x;
                    new_glyph.point.y += push_y;
                    left_aligned_glyphs.push(new_glyph);
                }

                // Add the word width to the current word_caret
                word_caret += word.total_width + space_width;
            },
            Tab => {
                word_caret += tab_width;
            },
            Return => {
                // TODO: dupliated code
                let space_until_horz_return = match max_horizontal_width {
                    Some(s) => WordCaretMax::SomeMaxWidth(s - word_caret),
                    None => WordCaretMax::NoMaxWidth(word_caret),
                };
                line_break_offsets.push((left_aligned_glyphs.len() - 1, space_until_horz_return));
                if word_caret > max_word_caret {
                    max_word_caret = word_caret;
                }
                word_caret = 0.0;
                current_line_num += 1;
            },
        }
    }

    // push the infos about the last line
    if !left_aligned_glyphs.is_empty() {
        let space_until_horz_return = match max_horizontal_width {
            Some(s) => WordCaretMax::SomeMaxWidth(s - word_caret),
            None => WordCaretMax::NoMaxWidth(word_caret),
        };
        line_break_offsets.push((left_aligned_glyphs.len() - 1, space_until_horz_return));
        if word_caret > max_word_caret {
            max_word_caret = word_caret;
        }
    }

    let min_enclosing_width = max_word_caret;
    let min_enclosing_height = (current_line_num as f32 * vertical_advance) + offset_top;

    let line_break_offsets = line_break_offsets.into_iter().map(|(line, space_r)| {
        let space_r = match space_r {
            WordCaretMax::SomeMaxWidth(s) => s,
            WordCaretMax::NoMaxWidth(word_caret) => max_word_caret - word_caret,
        };
        (line, space_r)
    }).collect();

    (left_aligned_glyphs, line_break_offsets, min_enclosing_width, min_enclosing_height)
}

#[inline(always)]
fn apply_harfbuzz_adjustments(positioned_glyphs: &mut [GlyphInstance], harfbuzz_adjustments: Vec<HarfbuzzAdjustment>)
{
    // TODO
}

#[inline(always)]
fn calculate_knuth_plass_adjustments(positioned_glyphs: &[GlyphInstance], line_break_offsets: &[(usize, f32)])
-> Vec<KnuthPlassAdjustment>
{
    // TODO
    Vec::new()
}

#[inline(always)]
fn apply_knuth_plass_adjustments(positioned_glyphs: &mut [GlyphInstance], knuth_plass_adjustments: Vec<KnuthPlassAdjustment>)
{
    // TODO
}

#[inline(always)]
fn align_text_horz(alignment: TextAlignmentHorz, glyphs: &mut [GlyphInstance], line_breaks: &[(usize, f32)], overflow: &TextOverflowPass2)
{
    use css_parser::TextAlignmentHorz::*;

    // Text alignment is theoretically very simple:
    //
    // If we have a bunch of text, such as this (the `glyphs`):

    // ^^^^^^^^^^^^
    // ^^^^^^^^
    // ^^^^^^^^^^^^^^^^
    // ^^^^^^^^^^

    // and we have information about how much space each line has to the right:
    // (the "---" is the space)

    // ^^^^^^^^^^^^----
    // ^^^^^^^^--------
    // ^^^^^^^^^^^^^^^^
    // ^^^^^^^^^^------

    // Then we can center-align the text, by just taking the "-----", dividing
    // it by 2 and moving all characters to the right:

    // --^^^^^^^^^^^^--
    // ----^^^^^^^^----
    // ^^^^^^^^^^^^^^^^
    // ---^^^^^^^^^^---

    // Same for right-aligned text, but without the "divide by 2 step"

    if line_breaks.is_empty() {
        return; // ??? maybe a 0-height rectangle?
    }

    // assert that the last info in the line_breaks vec has the same glyph index
    // i.e. the last line has to end with the last glyph
    assert!(glyphs.len() - 1 == line_breaks[line_breaks.len() - 1].0);

    let multiply_factor = match alignment {
        Left => { return; },
        Center => 0.5, // move the line by the half width
        Right => 1.0, // move the line by the full width
    };

    let mut current_line_num = 0;
    for (glyph_idx, glyph) in glyphs.iter_mut().enumerate() {
        if glyph_idx > line_breaks[current_line_num].0 {
            current_line_num += 1;
        }
        let space_added_full = line_breaks[current_line_num].1;
        glyph.point.x += space_added_full * multiply_factor;
    }
}

#[inline(always)]
fn align_text_vert(alignment: TextAlignmentVert, glyphs: &mut [GlyphInstance], line_breaks: &[(usize, f32)], overflow: &TextOverflowPass2) {

    use self::TextOverflow::*;
    use self::TextAlignmentVert::*;

    assert!(glyphs.len() - 1 == line_breaks[line_breaks.len() - 1].0);

    let multiply_factor = match alignment {
        Top => return,
        Center => 0.5,
        Bottom => 1.0,
    };

    let space_to_add = match overflow.vertical {
        IsOverflowing(_) => return,
        InBounds(s) => s * multiply_factor,
    };

    glyphs.iter_mut().for_each(|g| g.point.y += space_to_add);
}

/// Adds the X and Y offset to each glyph in the positioned glyph
#[inline(always)]
fn add_origin(positioned_glyphs: &mut [GlyphInstance], x: f32, y: f32)
{
    for c in positioned_glyphs {
        c.point.x += x;
        c.point.y += y;
    }
}

// -------------------------- PUBLIC API -------------------------- //

pub type IndexOfLineBreak = usize;
pub type RemainingSpaceToRight = f32;

/// Returned result from the `layout_text` function
#[derive(Debug, Clone)]
pub struct LayoutTextResult {
    /// The words, broken into 
    pub words: Words,
    /// Left-aligned glyphs
    pub layouted_glyphs: Vec<GlyphInstance>,
    /// The line_breaks contain:
    ///
    /// - The index of the glyph at which the line breaks (index into the `self.layouted_glyphs`)
    /// - How much space each line has (to the right edge of the containing rectangle)
    pub line_breaks: Vec<(IndexOfLineBreak, RemainingSpaceToRight)>,
    /// Minimal width of the layouted text
    pub min_width: f32,
    /// Minimal height of the layouted text
    pub min_height: f32,
}

/// Layout a string of text horizontally, given a font with its metrics.
pub fn layout_text<'a>(
    text: &str, 
    font: &Font<'a>, 
    font_metrics: &FontMetrics) 
-> LayoutTextResult
{
    // NOTE: This function is different from the get_glyphs function that is
    // used internally to azul.
    //
    // This function simply lays out a text, without trying to fit it into a rectangle.
    // This function does not calculate any overflow.
    let words = split_text_into_words(text, font, font_metrics.font_size_no_line_height);
    let (layouted_glyphs, line_breaks, min_width, min_height) = 
        words_to_left_aligned_glyphs(&words, font, None, font_metrics);
    
    LayoutTextResult {
        words, layouted_glyphs, line_breaks, min_width, min_height
    }
}

#[test]
fn test_it_should_add_origin() {
    let mut instances = vec![
        GlyphInstance {
            index: 20,
            point: TypedPoint2D::new(0.0, 0.0),
        },
        GlyphInstance {
            index: 40,
            point: TypedPoint2D::new(20.0, 10.0),
        },
    ];

    add_origin(&mut instances, 13.0, 0.0);

    assert_eq!(instances[0].point.x as usize, 13);
    assert_eq!(instances[0].point.y as usize, 0);
    assert_eq!(instances[1].point.x as usize, 33);
    assert_eq!(instances[1].point.y as usize, 10);
}