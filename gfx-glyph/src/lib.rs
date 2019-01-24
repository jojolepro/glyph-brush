//! Fast GPU cached text rendering using gfx-rs & rusttype.
//!
//! Makes use of three kinds of caching to optimise frame performance.
//!
//! * Caching of glyph positioning output to avoid repeated cost of identical text
//! rendering on sequential frames.
//! * Caches draw calculations to avoid repeated cost of identical text rendering on
//! sequential frames.
//! * GPU cache logic to dynamically maintain a GPU texture of rendered glyphs.
//!
//! # Example
//!
//! ```no_run
//! use gfx_glyph::{GlyphBrushBuilder, Section};
//! # fn main() -> Result<(), String> {
//! # let events_loop = glutin::EventsLoop::new();
//! # let (_window, _device, mut gfx_factory, gfx_color, gfx_depth) =
//! #     gfx_window_glutin::init::<gfx::format::Srgba8, gfx::format::Depth>(
//! #         glutin::WindowBuilder::new(),
//! #         glutin::ContextBuilder::new(),
//! #         &events_loop).unwrap();
//! # let mut gfx_encoder: gfx::Encoder<_, _> = gfx_factory.create_command_buffer().into();
//!
//! let dejavu: &[u8] = include_bytes!("../../fonts/DejaVuSans.ttf");
//! let mut glyph_brush = GlyphBrushBuilder::using_font_bytes(dejavu).build(gfx_factory.clone());
//!
//! # let some_other_section = Section { text: "another", ..Section::default() };
//! let section = Section {
//!     text: "Hello gfx_glyph",
//!     ..Section::default()
//! };
//!
//! glyph_brush.queue(section);
//! glyph_brush.queue(some_other_section);
//!
//! glyph_brush.draw_queued(&mut gfx_encoder, &gfx_color, &gfx_depth)?;
//! # Ok(())
//! # }
//! ```

#[macro_use]
extern crate thread_profiler;

mod builder;
mod pipe;
#[macro_use]
mod trace;

pub use crate::builder::*;
pub use glyph_brush::{
    rusttype::{self, Font, Point, PositionedGlyph, Rect, Scale, SharedBytes},
    BuiltInLineBreaker, FontId, FontMap, GlyphCruncher, HorizontalAlign, Layout, LineBreak,
    LineBreaker, OwnedSectionText, OwnedVariedSection, PositionedGlyphIter, Section, SectionText,
    VariedSection, VerticalAlign,
};

use crate::pipe::{glyph_pipe, GlyphVertex, RawAndFormat};
use gfx::{
    format,
    handle::{self, RawDepthStencilView, RawRenderTargetView},
    texture,
    traits::FactoryExt,
};
use glyph_brush::{
    rusttype::point, BrushAction, BrushError, DefaultSectionHasher, GlyphPositioner,
};
use log::{log_enabled, warn};
use std::{
    borrow::Cow,
    error::Error,
    fmt,
    hash::{BuildHasher, Hash},
    i32,
};

// Type for the generated glyph cache texture
type TexForm = format::U8Norm;
type TexSurface = <TexForm as format::Formatted>::Surface;
type TexChannel = <TexForm as format::Formatted>::Channel;
type TexFormView = <TexForm as format::Formatted>::View;
type TexSurfaceHandle<R> = handle::Texture<R, TexSurface>;
type TexShaderView<R> = handle::ShaderResourceView<R, TexFormView>;

const IDENTITY_MATRIX4: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// Object allowing glyph drawing, containing cache state. Manages glyph positioning cacheing,
/// glyph draw caching & efficient GPU texture cache updating and re-sizing on demand.
///
/// Build using a [`GlyphBrushBuilder`](struct.GlyphBrushBuilder.html).
///
/// # Example
///
/// ```no_run
/// # use gfx_glyph::{GlyphBrushBuilder};
/// use gfx_glyph::Section;
/// # fn main() -> Result<(), String> {
/// # let events_loop = glutin::EventsLoop::new();
/// # let (_window, _device, mut gfx_factory, gfx_color, gfx_depth) =
/// #     gfx_window_glutin::init::<gfx::format::Srgba8, gfx::format::Depth>(
/// #         glutin::WindowBuilder::new(),
/// #         glutin::ContextBuilder::new(),
/// #         &events_loop).unwrap();
/// # let mut gfx_encoder: gfx::Encoder<_, _> = gfx_factory.create_command_buffer().into();
/// # let dejavu: &[u8] = include_bytes!("../../fonts/DejaVuSans.ttf");
/// # let mut glyph_brush = GlyphBrushBuilder::using_font_bytes(dejavu)
/// #     .build(gfx_factory.clone());
/// # let some_other_section = Section { text: "another", ..Section::default() };
///
/// let section = Section {
///     text: "Hello gfx_glyph",
///     ..Section::default()
/// };
///
/// glyph_brush.queue(section);
/// glyph_brush.queue(some_other_section);
///
/// glyph_brush.draw_queued(&mut gfx_encoder, &gfx_color, &gfx_depth)?;
/// # Ok(())
/// # }
/// ```
///
/// # Caching behaviour
///
/// Calls to [`GlyphBrush::queue`](#method.queue),
/// [`GlyphBrush::pixel_bounds`](#method.pixel_bounds), [`GlyphBrush::glyphs`](#method.glyphs)
/// calculate the positioned glyphs for a section.
/// This is cached so future calls to any of the methods for the same section are much
/// cheaper. In the case of [`GlyphBrush::queue`](#method.queue) the calculations will also be
/// used for actual drawing.
///
/// The cache for a section will be **cleared** after a
/// [`GlyphBrush::draw_queued`](#method.draw_queued) call when that section has not been used since
/// the previous draw call.
pub struct GlyphBrush<'font, R: gfx::Resources, F: gfx::Factory<R>, H = DefaultSectionHasher> {
    font_cache_tex: (
        gfx::handle::Texture<R, TexSurface>,
        gfx_core::handle::ShaderResourceView<R, f32>,
    ),
    texture_filter_method: texture::FilterMethod,
    factory: F,
    program: gfx::handle::Program<R>,
    draw_cache: Option<DrawnGlyphBrush<R>>,
    glyph_brush: glyph_brush::GlyphBrush<'font, H>,

    // config
    depth_test: gfx::state::Depth,
}

impl<R: gfx::Resources, F: gfx::Factory<R>, H> fmt::Debug for GlyphBrush<'_, R, F, H> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GlyphBrush")
    }
}

impl<'font, R: gfx::Resources, F: gfx::Factory<R>, H: BuildHasher> GlyphCruncher<'font>
    for GlyphBrush<'font, R, F, H>
{
    #[inline]
    fn pixel_bounds_custom_layout<'a, S, L>(
        &mut self,
        section: S,
        custom_layout: &L,
    ) -> Option<Rect<i32>>
    where
        L: GlyphPositioner + Hash,
        S: Into<Cow<'a, VariedSection<'a>>>,
    {
        self.glyph_brush
            .pixel_bounds_custom_layout(section, custom_layout)
    }

    #[inline]
    fn glyphs_custom_layout<'a, 'b, S, L>(
        &'b mut self,
        section: S,
        custom_layout: &L,
    ) -> PositionedGlyphIter<'b, 'font>
    where
        L: GlyphPositioner + Hash,
        S: Into<Cow<'a, VariedSection<'a>>>,
    {
        self.glyph_brush
            .glyphs_custom_layout(section, custom_layout)
    }

    #[inline]
    fn fonts(&self) -> &[Font<'font>] {
        self.glyph_brush.fonts()
    }
}

impl<'font, R: gfx::Resources, F: gfx::Factory<R>, H: BuildHasher> GlyphBrush<'font, R, F, H> {
    /// Queues a section/layout to be drawn by the next call of
    /// [`draw_queued`](struct.GlyphBrush.html#method.draw_queued). Can be called multiple times
    /// to queue multiple sections for drawing.
    ///
    /// Used to provide custom `GlyphPositioner` logic, if using built-in
    /// [`Layout`](enum.Layout.html) simply use [`queue`](struct.GlyphBrush.html#method.queue)
    ///
    /// Benefits from caching, see [caching behaviour](#caching-behaviour).
    #[inline]
    pub fn queue_custom_layout<'a, S, G>(&mut self, section: S, custom_layout: &G)
    where
        G: GlyphPositioner,
        S: Into<Cow<'a, VariedSection<'a>>>,
    {
        self.glyph_brush.queue_custom_layout(section, custom_layout)
    }

    /// Queues a section/layout to be drawn by the next call of
    /// [`draw_queued`](struct.GlyphBrush.html#method.draw_queued). Can be called multiple times
    /// to queue multiple sections for drawing.
    ///
    /// Benefits from caching, see [caching behaviour](#caching-behaviour).
    #[inline]
    pub fn queue<'a, S>(&mut self, section: S)
    where
        S: Into<Cow<'a, VariedSection<'a>>>,
    {
        self.glyph_brush.queue(section)
    }

    /// Retains the section in the cache as if it had been used in the last draw-frame.
    ///
    /// Should not be necessary unless using multiple draws per frame with distinct transforms,
    /// see [caching behaviour](#caching-behaviour).
    #[inline]
    pub fn keep_cached_custom_layout<'a, S, G>(&mut self, section: S, custom_layout: &G)
    where
        S: Into<Cow<'a, VariedSection<'a>>>,
        G: GlyphPositioner,
    {
        self.glyph_brush.keep_cached_custom_layout(section, custom_layout)
    }

    /// Retains the section in the cache as if it had been used in the last draw-frame.
    ///
    /// Should not be necessary unless using multiple draws per frame with distinct transforms,
    /// see [caching behaviour](#caching-behaviour).
    #[inline]
    pub fn keep_cached<'a, S>(&mut self, section: S)
    where
        S: Into<Cow<'a, VariedSection<'a>>>,
    {
        self.glyph_brush.keep_cached(section)
    }

    /// Draws all queued sections onto a render target, applying a position transform (e.g.
    /// a projection).
    /// See [`queue`](struct.GlyphBrush.html#method.queue).
    ///
    /// Trims the cache, see [caching behaviour](#caching-behaviour).
    ///
    /// # Raw usage
    /// Can also be used with gfx raw render & depth views if necessary. The `Format` must also
    /// be provided. [See example.](struct.GlyphBrush.html#raw-usage-1)
    #[inline]
    pub fn draw_queued<C, CV, DV>(
        &mut self,
        encoder: &mut gfx::Encoder<R, C>,
        target: &CV,
        depth_target: &DV,
    ) -> Result<(), String>
    where
        C: gfx::CommandBuffer<R>,
        CV: RawAndFormat<Raw = RawRenderTargetView<R>>,
        DV: RawAndFormat<Raw = RawDepthStencilView<R>>,
    {
	    profile_scope!("glyph_brush_draw_queued");
        self.draw_queued_with_transform(IDENTITY_MATRIX4, encoder, target, depth_target)
    }

    /// Draws all queued sections onto a render target, applying a position transform (e.g.
    /// a projection).
    /// See [`queue`](struct.GlyphBrush.html#method.queue).
    ///
    /// Trims the cache, see [caching behaviour](#caching-behaviour).
    ///
    /// # Raw usage
    /// Can also be used with gfx raw render & depth views if necessary. The `Format` must also
    /// be provided.
    ///
    /// ```no_run
    /// # use gfx_glyph::{GlyphBrushBuilder};
    /// # use gfx_glyph::Section;
    /// # use gfx::format;
    /// # use gfx::format::Formatted;
    /// # use gfx::memory::Typed;
    /// # fn main() -> Result<(), String> {
    /// # let events_loop = glutin::EventsLoop::new();
    /// # let (_window, _device, mut gfx_factory, gfx_color, gfx_depth) =
    /// #     gfx_window_glutin::init::<gfx::format::Srgba8, gfx::format::Depth>(
    /// #         glutin::WindowBuilder::new(),
    /// #         glutin::ContextBuilder::new(),
    /// #         &events_loop).unwrap();
    /// # let mut gfx_encoder: gfx::Encoder<_, _> = gfx_factory.create_command_buffer().into();
    /// # let dejavu: &[u8] = include_bytes!("../../fonts/DejaVuSans.ttf");
    /// # let mut glyph_brush = GlyphBrushBuilder::using_font_bytes(dejavu)
    /// #     .build(gfx_factory.clone());
    /// # let raw_render_view = gfx_color.raw();
    /// # let raw_depth_view = gfx_depth.raw();
    /// # let transform = [[0.0; 4]; 4];
    /// glyph_brush.draw_queued_with_transform(
    ///     transform,
    ///     &mut gfx_encoder,
    ///     &(raw_render_view, format::Srgba8::get_format()),
    ///     &(raw_depth_view, format::Depth::get_format()),
    /// )?
    /// # ;
    /// # Ok(())
    /// # }
    /// ```
    pub fn draw_queued_with_transform<C, CV, DV>(
        &mut self,
        transform: [[f32; 4]; 4],
        mut encoder: &mut gfx::Encoder<R, C>,
        target: &CV,
        depth_target: &DV,
    ) -> Result<(), String>
    where
        C: gfx::CommandBuffer<R>,
        CV: RawAndFormat<Raw = RawRenderTargetView<R>>,
        DV: RawAndFormat<Raw = RawDepthStencilView<R>>,
    {
        let (screen_width, screen_height, ..) = target.as_raw().get_dimensions();
        let screen_dims = (u32::from(screen_width), u32::from(screen_height));

        let mut brush_action;

        loop {
            let tex = self.font_cache_tex.0.clone();
			
			{
			profile_scope!("glyph_brush_draw_queued_process_queued");
            brush_action = self.glyph_brush.process_queued(
                screen_dims,
                |rect, tex_data| {
                    let offset = [rect.min.x as u16, rect.min.y as u16];
                    let size = [rect.width() as u16, rect.height() as u16];
                    update_texture(&mut encoder, &tex, offset, size, tex_data);
                },
                to_vertex,
            );
			}

            match brush_action {
                Ok(_) => break,
                Err(BrushError::TextureTooSmall { suggested }) => {
                    let (new_width, new_height) = suggested;

                    if log_enabled!(log::Level::Warn) {
                        warn!(
                            "Increasing glyph texture size {old:?} -> {new:?}. \
                             Consider building with `.initial_cache_size({new:?})` to avoid \
                             resizing. Called from:\n{trace}",
                            old = self.glyph_brush.texture_dimensions(),
                            new = (new_width, new_height),
                            trace = outer_backtrace!()
                        );
                    }

                    match create_texture(&mut self.factory, new_width, new_height) {
                        Ok((new_tex, tex_view)) => {
                            self.glyph_brush.resize_texture(new_width, new_height);

                            if let Some(ref mut cache) = self.draw_cache {
                                cache.pipe_data.font_tex.0 = tex_view.clone();
                            }

                            self.font_cache_tex.1 = tex_view;
                            self.font_cache_tex.0 = new_tex;
                        }
                        Err(_) => {
                            return Err(format!(
                                "Failed to create {}x{} glyph texture",
                                new_width, new_height
                            ));
                        }
                    }
                }
            }
        }

        match brush_action.unwrap() {
            BrushAction::Draw(verts) => {
				profile_scope!("glyph_brush_draw_queued_brush_action");
				
				let vbuf = {
					profile_scope!("glyph_brush_draw_queued_brush_action_vertex_buf");
					println!("verts size: {}", verts.len());
					self.factory.create_vertex_buffer(&verts)
				};

                let draw_cache = if let Some(mut cache) = self.draw_cache.take() {
					profile_scope!("glyph_brush_draw_queued_brush_action_some_cache");
                    cache.pipe_data.vbuf = vbuf;
                    if &cache.pipe_data.out != target.as_raw() {
                        cache.pipe_data.out.clone_from(target.as_raw());
                    }
                    if &cache.pipe_data.out_depth != depth_target.as_raw() {
                        cache.pipe_data.out_depth.clone_from(depth_target.as_raw());
                    }
                    if cache.pso.0 != target.format() {
                        cache.pso = (
                            target.format(),
                            self.pso_using(target.format(), depth_target.format()),
                        );
                    }
                    cache.slice.instances.as_mut().unwrap().0 = verts.len() as _;
                    cache
                } else {
					profile_scope!("glyph_brush_draw_queued_brush_action_none_cache");
                    DrawnGlyphBrush {
                        pipe_data: {
                            let sampler = self.factory.create_sampler(texture::SamplerInfo::new(
                                self.texture_filter_method,
                                texture::WrapMode::Clamp,
                            ));
                            glyph_pipe::Data {
                                vbuf,
                                font_tex: (self.font_cache_tex.1.clone(), sampler),
                                transform,
                                out: target.as_raw().clone(),
                                out_depth: depth_target.as_raw().clone(),
                            }
                        },
                        pso: (
                            target.format(),
                            self.pso_using(target.format(), depth_target.format()),
                        ),
                        slice: gfx::Slice {
                            instances: Some((verts.len() as _, 0)),
                            ..Self::empty_slice()
                        },
                    }
                };

                self.draw_cache = Some(draw_cache);
            }
            BrushAction::ReDraw => {}
        };

        if let Some(&mut DrawnGlyphBrush {
            ref pso,
            ref slice,
            ref mut pipe_data,
            ..
        }) = self.draw_cache.as_mut()
        {
			profile_scope!("glyph_brush_draw_queued_encoder_draw");
            pipe_data.transform = transform;
            encoder.draw(slice, &pso.1, pipe_data);
        }

        Ok(())
    }

    /// Returns the available fonts.
    ///
    /// The `FontId` corresponds to the index of the font data.
    #[inline]
    pub fn fonts(&self) -> &[Font<'_>] {
        self.glyph_brush.fonts()
    }

    fn pso_using(
        &mut self,
        color_format: gfx::format::Format,
        depth_format: gfx::format::Format,
    ) -> gfx::PipelineState<R, glyph_pipe::Meta> {
        self.factory
            .create_pipeline_from_program(
                &self.program,
                gfx::Primitive::TriangleStrip,
                gfx::state::Rasterizer::new_fill(),
                glyph_pipe::Init::new(color_format, depth_format, self.depth_test),
            )
            .unwrap()
    }

    fn empty_slice() -> gfx::Slice<R> {
        gfx::Slice {
            start: 0,
            end: 4,
            buffer: gfx::IndexBuffer::Auto,
            base_vertex: 0,
            instances: None,
        }
    }

    /// Adds an additional font to the one(s) initially added on build.
    ///
    /// Returns a new [`FontId`](struct.FontId.html) to reference this font.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use gfx_glyph::{GlyphBrushBuilder, Section};
    /// # fn main() {
    /// # let events_loop = glutin::EventsLoop::new();
    /// # let (_window, _device, mut gfx_factory, gfx_color, gfx_depth) =
    /// #     gfx_window_glutin::init::<gfx::format::Srgba8, gfx::format::Depth>(
    /// #         glutin::WindowBuilder::new(),
    /// #         glutin::ContextBuilder::new(),
    /// #         &events_loop).unwrap();
    /// # let mut gfx_encoder: gfx::Encoder<_, _> = gfx_factory.create_command_buffer().into();
    ///
    /// // dejavu is built as default `FontId(0)`
    /// let dejavu: &[u8] = include_bytes!("../../fonts/DejaVuSans.ttf");
    /// let mut glyph_brush = GlyphBrushBuilder::using_font_bytes(dejavu).build(gfx_factory.clone());
    ///
    /// // some time later, add another font referenced by a new `FontId`
    /// let open_sans_italic: &[u8] = include_bytes!("../../fonts/OpenSans-Italic.ttf");
    /// let open_sans_italic_id = glyph_brush.add_font_bytes(open_sans_italic);
    /// # glyph_brush.draw_queued(&mut gfx_encoder, &gfx_color, &gfx_depth).unwrap();
    /// # let _ = open_sans_italic_id;
    /// # }
    /// ```
    pub fn add_font_bytes<'a: 'font, B: Into<SharedBytes<'a>>>(&mut self, font_data: B) -> FontId {
        self.glyph_brush.add_font_bytes(font_data)
    }

    /// Adds an additional font to the one(s) initially added on build.
    ///
    /// Returns a new [`FontId`](struct.FontId.html) to reference this font.
    pub fn add_font<'a: 'font>(&mut self, font_data: Font<'a>) -> FontId {
        self.glyph_brush.add_font(font_data)
    }
}

struct DrawnGlyphBrush<R: gfx::Resources> {
    pipe_data: glyph_pipe::Data<R>,
    pso: (gfx::format::Format, gfx::PipelineState<R, glyph_pipe::Meta>),
    slice: gfx::Slice<R>,
}

#[inline]
fn to_vertex(
    glyph_brush::GlyphVertex {
        mut tex_coords,
        pixel_coords,
        bounds,
        screen_dimensions: (screen_w, screen_h),
        color,
        z,
    }: glyph_brush::GlyphVertex,
) -> GlyphVertex {
    let gl_bounds = Rect {
        min: point(
            2.0 * (bounds.min.x / screen_w - 0.5),
            2.0 * (0.5 - bounds.min.y / screen_h),
        ),
        max: point(
            2.0 * (bounds.max.x / screen_w - 0.5),
            2.0 * (0.5 - bounds.max.y / screen_h),
        ),
    };

    let mut gl_rect = Rect {
        min: point(
            2.0 * (pixel_coords.min.x as f32 / screen_w - 0.5),
            2.0 * (0.5 - pixel_coords.min.y as f32 / screen_h),
        ),
        max: point(
            2.0 * (pixel_coords.max.x as f32 / screen_w - 0.5),
            2.0 * (0.5 - pixel_coords.max.y as f32 / screen_h),
        ),
    };

    // handle overlapping bounds, modify uv_rect to preserve texture aspect
    if gl_rect.max.x > gl_bounds.max.x {
        let old_width = gl_rect.width();
        gl_rect.max.x = gl_bounds.max.x;
        tex_coords.max.x = tex_coords.min.x + tex_coords.width() * gl_rect.width() / old_width;
    }
    if gl_rect.min.x < gl_bounds.min.x {
        let old_width = gl_rect.width();
        gl_rect.min.x = gl_bounds.min.x;
        tex_coords.min.x = tex_coords.max.x - tex_coords.width() * gl_rect.width() / old_width;
    }
    // note: y access is flipped gl compared with screen,
    // texture is not flipped (ie is a headache)
    if gl_rect.max.y < gl_bounds.max.y {
        let old_height = gl_rect.height();
        gl_rect.max.y = gl_bounds.max.y;
        tex_coords.max.y = tex_coords.min.y + tex_coords.height() * gl_rect.height() / old_height;
    }
    if gl_rect.min.y > gl_bounds.min.y {
        let old_height = gl_rect.height();
        gl_rect.min.y = gl_bounds.min.y;
        tex_coords.min.y = tex_coords.max.y - tex_coords.height() * gl_rect.height() / old_height;
    }

    GlyphVertex {
        left_top: [gl_rect.min.x, gl_rect.max.y, z],
        right_bottom: [gl_rect.max.x, gl_rect.min.y],
        tex_left_top: [tex_coords.min.x, tex_coords.max.y],
        tex_right_bottom: [tex_coords.max.x, tex_coords.min.y],
        color,
    }
}

// Creates a gfx texture with the given data
fn create_texture<F, R>(
    factory: &mut F,
    width: u32,
    height: u32,
) -> Result<(TexSurfaceHandle<R>, TexShaderView<R>), Box<dyn Error>>
where
    R: gfx::Resources,
    F: gfx::Factory<R>,
{
    let kind = texture::Kind::D2(
        width as texture::Size,
        height as texture::Size,
        texture::AaMode::Single,
    );

    let tex = factory.create_texture(
        kind,
        1 as texture::Level,
        gfx::memory::Bind::SHADER_RESOURCE,
        gfx::memory::Usage::Dynamic,
        Some(<TexChannel as format::ChannelTyped>::get_channel_type()),
    )?;

    let view =
        factory.view_texture_as_shader_resource::<TexForm>(&tex, (0, 0), format::Swizzle::new())?;

    Ok((tex, view))
}

// Updates a texture with the given data (used for updating the GlyphCache texture)
#[inline]
fn update_texture<R, C>(
    encoder: &mut gfx::Encoder<R, C>,
    texture: &handle::Texture<R, TexSurface>,
    offset: [u16; 2],
    size: [u16; 2],
    data: &[u8],
) where
    R: gfx::Resources,
    C: gfx::CommandBuffer<R>,
{
    let info = texture::ImageInfoCommon {
        xoffset: offset[0],
        yoffset: offset[1],
        zoffset: 0,
        width: size[0],
        height: size[1],
        depth: 0,
        format: (),
        mipmap: 0,
    };
    encoder
        .update_texture::<TexSurface, TexForm>(texture, None, info, data)
        .unwrap();
}
