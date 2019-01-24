//! Example of glyph_brush usage with raw OpenGL.
//!
//! Uses instanced rendering with 1 vertex per glyph referencing a 1 byte per pixel texture.
//!
//! Controls
//! * Scroll to size text.
//! * Type to modify text.
//! * Resize window.

use gl::types::*;
use glutin::{Api, GlContext, GlProfile, GlRequest};
use glyph_brush::{rusttype::*, *};
use std::{
    env,
    ffi::CString,
    io::{self, Write},
    mem, ptr, str,
};

type Res<T> = Result<T, Box<std::error::Error>>;
/// `[left_top * 3, right_bottom * 2, tex_left_top * 2, tex_right_bottom * 2, color * 4]`
type Vertex = [GLfloat; 13];

macro_rules! gl_assert_ok {
    () => {{
        let err = gl::GetError();
        assert_eq!(err, gl::NO_ERROR, "{}", gl_err_to_str(err));
    }};
}

fn main() -> Res<()> {
    env_logger::init();

    if cfg!(target_os = "linux") {
        // winit wayland is currently still wip
        if env::var("WINIT_UNIX_BACKEND").is_err() {
            env::set_var("WINIT_UNIX_BACKEND", "x11");
        }
        // disables vsync sometimes on x11
        if env::var("vblank_mode").is_err() {
            env::set_var("vblank_mode", "0");
        }
    }

    let mut events = glutin::EventsLoop::new();
    let title = "glyph_brush opengl example - scroll to size, type to modify";

    let window = glutin::GlWindow::new(
        glutin::WindowBuilder::new()
            .with_dimensions((1024, 576).into())
            .with_title(title),
        glutin::ContextBuilder::new()
            .with_gl_profile(GlProfile::Core)
            .with_gl(GlRequest::Specific(Api::OpenGl, (3, 2)))
            .with_srgb(true),
        &events,
    )?;
    unsafe { window.make_current()? };

    let dejavu: &[u8] = include_bytes!("../../fonts/OpenSans-Light.ttf");
    let mut glyph_brush = GlyphBrushBuilder::using_font_bytes(dejavu).build();

    // Load the OpenGL function pointers
    gl::load_with(|symbol| window.get_proc_address(symbol) as _);

    // Create GLSL shaders
    let vs = compile_shader(include_str!("shader/vert.glsl"), gl::VERTEX_SHADER)?;
    let fs = compile_shader(include_str!("shader/frag.glsl"), gl::FRAGMENT_SHADER)?;
    let program = link_program(vs, fs)?;

    let mut vao = 0;
    let mut vbo = 0;
    let mut glyph_texture = 0;

    unsafe {
        // Create Vertex Array Object
        gl::GenVertexArrays(1, &mut vao);
        gl::BindVertexArray(vao);

        // Create a Vertex Buffer Object
        gl::GenBuffers(1, &mut vbo);
        gl::BindBuffer(gl::ARRAY_BUFFER, vbo);

        {
            // Create a texture for the glyphs
            // The texture holds 1 byte per pixel as alpha data
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
            gl::GenTextures(1, &mut glyph_texture);
            gl::BindTexture(gl::TEXTURE_2D, glyph_texture);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as _);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as _);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as _);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as _);
            let (width, height) = glyph_brush.texture_dimensions();
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::RED as _,
                width as _,
                height as _,
                0,
                gl::RED,
                gl::UNSIGNED_BYTE,
                ptr::null(),
            );
            gl_assert_ok!();
        }

        // Use shader program
        gl::UseProgram(program);
        gl::BindFragDataLocation(program, 0, CString::new("out_color")?.as_ptr());

        // Specify the layout of the vertex data
        let mut offset = 0;
        for (v_field, float_count) in &[
            ("left_top", 3),
            ("right_bottom", 2),
            ("tex_left_top", 2),
            ("tex_right_bottom", 2),
            ("color", 4),
        ] {
            let attr = gl::GetAttribLocation(program, CString::new(*v_field)?.as_ptr());
            if attr < 0 {
                return Err(format!("{} GetAttribLocation -> {}", v_field, attr).into());
            }
            gl::VertexAttribPointer(
                attr as _,
                *float_count,
                gl::FLOAT,
                gl::FALSE as _,
                mem::size_of::<Vertex>() as _,
                offset as _,
            );
            gl::EnableVertexAttribArray(attr as _);
            gl::VertexAttribDivisor(attr as _, 1);

            offset += float_count * 4;
        }

        // Enabled alpha blending
        gl::Enable(gl::BLEND);
        gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
        // Use srgb for consistency with other examples
        gl::Enable(gl::FRAMEBUFFER_SRGB);
        gl::ClearColor(0.02, 0.02, 0.02, 1.0);
    }

    let mut text: String = include_str!("text/lipsum.txt").into();
    let mut font_size: f32 = 18.0;

    let mut loop_helper = spin_sleep::LoopHelper::builder().build_with_target_rate(250.0);
    let mut running = true;
    let mut vertex_count = 0;
    let mut dimensions = window
        .get_inner_size()
        .ok_or("get_inner_size = None")?
        .to_physical(window.get_hidpi_factor());

    while running {
        loop_helper.loop_start();

        events.poll_events(|event| {
            use glutin::*;
            if let Event::WindowEvent { event, .. } = event {
                match event {
                    WindowEvent::CloseRequested => running = false,
                    WindowEvent::Resized(size) => {
                        let dpi = window.get_hidpi_factor();
                        window.resize(size.to_physical(dpi));
                        if let Some(ls) = window.get_inner_size() {
                            dimensions = ls.to_physical(dpi);
                            unsafe {
                                gl::Viewport(0, 0, dimensions.width as _, dimensions.height as _);
                            }
                        }
                    }
                    WindowEvent::KeyboardInput {
                        input:
                            KeyboardInput {
                                state: ElementState::Pressed,
                                virtual_keycode: Some(keypress),
                                ..
                            },
                        ..
                    } => match keypress {
                        VirtualKeyCode::Escape => running = false,
                        VirtualKeyCode::Back => {
                            text.pop();
                        }
                        _ => (),
                    },
                    WindowEvent::ReceivedCharacter(c) => {
                        if c != '\u{7f}' && c != '\u{8}' {
                            text.push(c);
                        }
                    }
                    WindowEvent::MouseWheel {
                        delta: MouseScrollDelta::LineDelta(_, y),
                        ..
                    } => {
                        // increase/decrease font size
                        let old_size = font_size;
                        let mut size = font_size;
                        if y > 0.0 {
                            size += (size / 4.0).max(2.0)
                        } else {
                            size *= 4.0 / 5.0
                        };
                        font_size = size.max(1.0).min(2000.0);
                        if (font_size - old_size).abs() > 1e-2 {
                            eprint!("\r                            \r");
                            eprint!("font-size -> {:.1}", font_size);
                            let _ = io::stderr().flush();
                        }
                    }
                    _ => {}
                }
            }
        });

        let width = dimensions.width as f32;
        let height = dimensions.height as _;
        let scale = Scale::uniform((font_size * window.get_hidpi_factor() as f32).round());

        glyph_brush.queue(Section {
            text: &text,
            scale,
            screen_position: (0.0, 0.0),
            bounds: (width / 3.15, height),
            color: [0.9, 0.3, 0.3, 1.0],
            ..Section::default()
        });

        glyph_brush.queue(Section {
            text: &text,
            scale,
            screen_position: (width / 2.0, height / 2.0),
            bounds: (width / 3.15, height),
            color: [0.3, 0.9, 0.3, 1.0],
            layout: Layout::default()
                .h_align(HorizontalAlign::Center)
                .v_align(VerticalAlign::Center),
            ..Section::default()
        });

        glyph_brush.queue(Section {
            text: &text,
            scale,
            screen_position: (width, height),
            bounds: (width / 3.15, height),
            color: [0.3, 0.3, 0.9, 1.0],
            layout: Layout::default()
                .h_align(HorizontalAlign::Right)
                .v_align(VerticalAlign::Bottom),
            ..Section::default()
        });

        let mut brush_action;
        loop {
            brush_action = glyph_brush.process_queued(
                (width as _, height as _),
                |rect, tex_data| unsafe {
                    // Update part of gpu texture with new glyph alpha values
                    gl::TexSubImage2D(
                        gl::TEXTURE_2D,
                        0,
                        rect.min.x as _,
                        rect.min.y as _,
                        rect.width() as _,
                        rect.height() as _,
                        gl::RED,
                        gl::UNSIGNED_BYTE,
                        tex_data.as_ptr() as _,
                    );
                    gl_assert_ok!();
                },
                to_vertex,
            );

            match brush_action {
                Ok(_) => break,
                Err(BrushError::TextureTooSmall { suggested, .. }) => unsafe {
                    let (new_width, new_height) = suggested;
                    eprint!("\r                            \r");
                    eprintln!("Resizing glyph texture -> {}x{}", new_width, new_height);
                    // Recreate texture as a larger size to fit more
                    gl::TexImage2D(
                        gl::TEXTURE_2D,
                        0,
                        gl::RED as _,
                        new_width as _,
                        new_height as _,
                        0,
                        gl::RED,
                        gl::UNSIGNED_BYTE,
                        ptr::null(),
                    );
                    gl_assert_ok!();
                    glyph_brush.resize_texture(new_width, new_height);
                },
            }
        }
        match brush_action? {
            BrushAction::Draw(vertices) => {
                // Draw new vertices
                vertex_count = vertices.len();
                unsafe {
                    gl::BufferData(
                        gl::ARRAY_BUFFER,
                        (vertex_count * mem::size_of::<Vertex>()) as GLsizeiptr,
                        vertices.as_ptr() as _,
                        gl::STATIC_DRAW,
                    );
                }
            }
            BrushAction::ReDraw => {}
        }

        unsafe {
            gl::Clear(gl::COLOR_BUFFER_BIT);
            gl::DrawArraysInstanced(gl::TRIANGLE_STRIP, 0, 4, vertex_count as _);
        }

        window.swap_buffers()?;

        if let Some(rate) = loop_helper.report_rate() {
            window.set_title(&format!("{} {:.0} FPS", title, rate));
        }
        loop_helper.loop_sleep();
    }

    unsafe {
        gl::DeleteProgram(program);
        gl::DeleteShader(fs);
        gl::DeleteShader(vs);
        gl::DeleteBuffers(1, &vbo);
        gl::DeleteVertexArrays(1, &vao);
        gl::DeleteTextures(1, &glyph_texture);
    }
    Ok(())
}

fn gl_err_to_str(err: u32) -> &'static str {
    match err {
        gl::INVALID_ENUM => "INVALID_ENUM",
        gl::INVALID_VALUE => "INVALID_VALUE",
        gl::INVALID_OPERATION => "INVALID_OPERATION",
        gl::INVALID_FRAMEBUFFER_OPERATION => "INVALID_FRAMEBUFFER_OPERATION",
        gl::OUT_OF_MEMORY => "OUT_OF_MEMORY",
        gl::STACK_UNDERFLOW => "STACK_UNDERFLOW",
        gl::STACK_OVERFLOW => "STACK_OVERFLOW",
        _ => "Unknown error",
    }
}

fn compile_shader(src: &str, ty: GLenum) -> Res<GLuint> {
    let shader;
    unsafe {
        shader = gl::CreateShader(ty);
        // Attempt to compile the shader
        let c_str = CString::new(src.as_bytes())?;
        gl::ShaderSource(shader, 1, &c_str.as_ptr(), ptr::null());
        gl::CompileShader(shader);

        // Get the compile status
        let mut status = GLint::from(gl::FALSE);
        gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut status);

        // Fail on error
        if status != GLint::from(gl::TRUE) {
            let mut len = 0;
            gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut len);
            let mut buf = Vec::with_capacity(len as usize);
            buf.set_len((len as usize) - 1); // subtract 1 to skip the trailing null character
            gl::GetShaderInfoLog(
                shader,
                len,
                ptr::null_mut(),
                buf.as_mut_ptr() as *mut GLchar,
            );
            return Err(str::from_utf8(&buf)?.into());
        }
    }
    Ok(shader)
}

fn link_program(vs: GLuint, fs: GLuint) -> Res<GLuint> {
    unsafe {
        let program = gl::CreateProgram();
        gl::AttachShader(program, vs);
        gl::AttachShader(program, fs);
        gl::LinkProgram(program);
        // Get the link status
        let mut status = GLint::from(gl::FALSE);
        gl::GetProgramiv(program, gl::LINK_STATUS, &mut status);

        // Fail on error
        if status != GLint::from(gl::TRUE) {
            let mut len: GLint = 0;
            gl::GetProgramiv(program, gl::INFO_LOG_LENGTH, &mut len);
            let mut buf = Vec::with_capacity(len as usize);
            buf.set_len((len as usize) - 1); // subtract 1 to skip the trailing null character
            gl::GetProgramInfoLog(
                program,
                len,
                ptr::null_mut(),
                buf.as_mut_ptr() as *mut GLchar,
            );
            return Err(str::from_utf8(&buf)?.into());
        }
        Ok(program)
    }
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
) -> Vertex {
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

    [
        gl_rect.min.x,
        gl_rect.max.y,
        z,
        gl_rect.max.x,
        gl_rect.min.y,
        tex_coords.min.x,
        tex_coords.max.y,
        tex_coords.max.x,
        tex_coords.min.y,
        color[0],
        color[1],
        color[2],
        color[3],
    ]
}
