use std::cell::RefCell;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};

use gbm::AsRaw;
use glow::HasContext;
use khronos_egl as egl;

use crate::{
    DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_BGRA8888, DRM_FORMAT_BGRX8888,
    DRM_FORMAT_RGBA8888, DRM_FORMAT_RGBX8888, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB8888,
};

const EGL_PLATFORM_GBM_KHR: egl::Enum = 0x31D7;
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Attrib = 0x3444;

type EglImageTargetTexture = unsafe extern "C" fn(u32, *const c_void);

const VERTEX_SHADER: &str = r#"#version 100
attribute vec2 a_pos;
varying vec2 v_uv;
void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
    v_uv = a_pos * 0.5 + 0.5;
}"#;

const FRAGMENT_OPAQUE: &str = r#"#version 100
precision mediump float;
varying vec2 v_uv;
uniform sampler2D u_tex;
void main() {
    gl_FragColor = vec4(texture2D(u_tex, v_uv).rgb, 1.0);
}"#;

const FRAGMENT_SAME_ALPHA: &str = r#"#version 100
precision mediump float;
varying vec2 v_uv;
uniform sampler2D u_tex;
void main() {
    gl_FragColor = texture2D(u_tex, v_uv);
}"#;

const FRAGMENT_OTHER_ALPHA: &str = r#"#version 100
precision mediump float;
varying vec2 v_uv;
uniform sampler2D u_tex;
uniform sampler2D u_alpha;
void main() {
    vec4 color = texture2D(u_tex, v_uv);
    color.a = texture2D(u_alpha, v_uv).r;
    gl_FragColor = color;
}"#;

struct ProgramInfo {
    program: glow::Program,
}

struct Programs {
    opaque: ProgramInfo,
    same_alpha: ProgramInfo,
    other_alpha: ProgramInfo,
}

#[derive(Clone, Copy)]
enum AlphaMode {
    /// The source has no usable alpha; write 255.
    Opaque,
    /// The source EGL texture already carries alpha.
    Same,
    /// Alpha lives in the first byte; sample it through a second ABGR image.
    Other,
}

#[derive(Clone, Copy)]
struct SourceSpec {
    fourcc: u32,
    shift: u32,
    alpha: AlphaMode,
}

fn source_spec(format: u32) -> Option<SourceSpec> {
    let spec = match format {
        DRM_FORMAT_ARGB8888 => SourceSpec {
            fourcc: DRM_FORMAT_ARGB8888,
            shift: 0,
            alpha: AlphaMode::Same,
        },
        DRM_FORMAT_ABGR8888 => SourceSpec {
            fourcc: DRM_FORMAT_ABGR8888,
            shift: 0,
            alpha: AlphaMode::Same,
        },
        // Memory B G R X. The exact fourcc is importable and X is ignored.
        DRM_FORMAT_XRGB8888 => SourceSpec {
            fourcc: DRM_FORMAT_XRGB8888,
            shift: 0,
            alpha: AlphaMode::Opaque,
        },
        // Memory R G B X.
        DRM_FORMAT_XBGR8888 => SourceSpec {
            fourcc: DRM_FORMAT_XBGR8888,
            shift: 0,
            alpha: AlphaMode::Opaque,
        },
        // X/A B G R: skip the first byte and read it as ARGB, alpha comes from
        // a second ABGR image for A formats.
        DRM_FORMAT_RGBX8888 => SourceSpec {
            fourcc: DRM_FORMAT_ARGB8888,
            shift: 1,
            alpha: AlphaMode::Opaque,
        },
        DRM_FORMAT_RGBA8888 => SourceSpec {
            fourcc: DRM_FORMAT_ARGB8888,
            shift: 1,
            alpha: AlphaMode::Other,
        },
        // X/A R G B: skip the first byte and read it as ABGR.
        DRM_FORMAT_BGRX8888 => SourceSpec {
            fourcc: DRM_FORMAT_ABGR8888,
            shift: 1,
            alpha: AlphaMode::Opaque,
        },
        DRM_FORMAT_BGRA8888 => SourceSpec {
            fourcc: DRM_FORMAT_ABGR8888,
            shift: 1,
            alpha: AlphaMode::Other,
        },
        _ => return None,
    };
    Some(spec)
}

#[derive(Clone, Copy)]
struct ImageDesc {
    width: u32,
    height: u32,
    fourcc: u32,
    fd: RawFd,
    offset: u32,
    stride: u32,
    modifier: u64,
}

pub struct ConvertedTexture {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: u32,
    pub size: u64,
    pub modifier: u64,
}

struct GpuContext {
    egl: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    context: egl::Context,
    gl: glow::Context,
    gbm: gbm::Device<File>,
    image_target: EglImageTargetTexture,
    programs: Programs,
    vao: glow::VertexArray,
    _vbo: glow::Buffer,
    fbo: glow::Framebuffer,
}

impl GpuContext {
    fn new() -> Result<Self, String> {
        let gbm = open_gbm_device()?;
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }
            .map_err(|error| format!("failed to load EGL: {error:?}"))?;
        let display = unsafe {
            egl.get_platform_display(
                EGL_PLATFORM_GBM_KHR,
                gbm.as_raw() as *mut c_void,
                &[egl::ATTRIB_NONE],
            )
        }
        .map_err(|error| format!("eglGetPlatformDisplay failed: {error:?}"))?;

        egl.initialize(display)
            .map_err(|error| format!("eglInitialize failed: {error:?}"))?;
        egl.bind_api(egl::OPENGL_ES_API)
            .map_err(|error| format!("eglBindAPI failed: {error:?}"))?;

        let config = egl
            .choose_first_config(
                display,
                &[
                    egl::RENDERABLE_TYPE,
                    egl::OPENGL_ES2_BIT,
                    egl::RED_SIZE,
                    8,
                    egl::GREEN_SIZE,
                    8,
                    egl::BLUE_SIZE,
                    8,
                    egl::ALPHA_SIZE,
                    8,
                    egl::NONE,
                ],
            )
            .map_err(|error| format!("eglChooseConfig failed: {error:?}"))?
            .ok_or_else(|| "no EGL config for OpenGL ES2".to_string())?;

        let context = egl
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE],
            )
            .map_err(|error| format!("eglCreateContext failed: {error:?}"))?;

        egl.make_current(display, None, None, Some(context))
            .map_err(|error| format!("eglMakeCurrent failed: {error:?}"))?;

        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name)
                    .map(|function| function as *const c_void)
                    .unwrap_or(std::ptr::null())
            })
        };

        let address = egl
            .get_proc_address("glEGLImageTargetTexture2DOES")
            .ok_or_else(|| "glEGLImageTargetTexture2DOES is unavailable".to_string())?;
        // SAFETY: the EGL implementation just reported this symbol.
        let image_target: EglImageTargetTexture = unsafe { std::mem::transmute(address) };

        let programs = unsafe {
            Programs {
                opaque: ProgramInfo {
                    program: link_program(&gl, FRAGMENT_OPAQUE)?,
                },
                same_alpha: ProgramInfo {
                    program: link_program(&gl, FRAGMENT_SAME_ALPHA)?,
                },
                other_alpha: ProgramInfo {
                    program: link_program(&gl, FRAGMENT_OTHER_ALPHA)?,
                },
            }
        };

        let (vao, vbo) = unsafe {
            let vao = gl
                .create_vertex_array()
                .map_err(|error| format!("create VAO failed: {error:?}"))?;
            let vbo = gl
                .create_buffer()
                .map_err(|error| format!("create VBO failed: {error:?}"))?;
            let vertices: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(
                glow::ARRAY_BUFFER,
                std::slice::from_raw_parts(
                    vertices.as_ptr() as *const u8,
                    std::mem::size_of_val(&vertices),
                ),
                glow::STATIC_DRAW,
            );
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
            (vao, vbo)
        };

        let fbo = unsafe {
            gl.create_framebuffer()
                .map_err(|error| format!("create FBO failed: {error:?}"))?
        };

        eprintln!("myde-drm: GPU path ready ({})", unsafe {
            gl.get_parameter_string(glow::RENDERER)
        });

        Ok(Self {
            egl,
            display,
            context,
            gl,
            gbm,
            image_target,
            programs,
            vao,
            _vbo: vbo,
            fbo,
        })
    }

    fn convert(
        &self,
        width: u32,
        height: u32,
        format: u32,
        fd: RawFd,
        offset: u32,
        stride: u32,
    ) -> Result<ConvertedTexture, String> {
        let spec =
            source_spec(format).ok_or_else(|| format!("unsupported DRM format 0x{format:08x}"))?;
        if stride < width.saturating_mul(4) {
            return Err(format!("stride {stride} is smaller than width * 4"));
        }
        self.egl
            .make_current(self.display, None, None, Some(self.context))
            .map_err(|error| format!("eglMakeCurrent failed: {error:?}"))?;

        let buffer = self
            .gbm
            .create_buffer_object::<()>(
                width,
                height,
                gbm::Format::Argb8888,
                gbm::BufferObjectFlags::LINEAR | gbm::BufferObjectFlags::RENDERING,
            )
            .map_err(|error| format!("gbm_bo_create failed: {error}"))?;
        let dst_stride = buffer.stride();
        let dst_offset = buffer.offset(0);
        let dst_modifier = u64::from(buffer.modifier());
        let dst_fd: OwnedFd = buffer
            .fd()
            .map_err(|error| format!("gbm_bo_get_fd failed: {error}"))?;

        let mut resources = FrameResources::new(self);

        let dst_image = resources.add_image(self.create_image(ImageDesc {
            width,
            height,
            fourcc: DRM_FORMAT_ARGB8888,
            fd: dst_fd.as_raw_fd(),
            offset: dst_offset,
            stride: dst_stride,
            modifier: dst_modifier,
        })?);
        let src_image = resources.add_image(
            self.create_image(ImageDesc {
                width,
                height,
                fourcc: spec.fourcc,
                fd,
                offset: offset
                    .checked_add(spec.shift)
                    .ok_or_else(|| "source offset overflows".to_string())?,
                stride,
                modifier: 0,
            })?,
        );
        let alpha_image = if matches!(spec.alpha, AlphaMode::Other) {
            Some(resources.add_image(self.create_image(ImageDesc {
                width,
                height,
                fourcc: DRM_FORMAT_ABGR8888,
                fd,
                offset,
                stride,
                modifier: 0,
            })?))
        } else {
            None
        };

        unsafe {
            self.gl.bind_vertex_array(Some(self.vao));
            self.gl.viewport(0, 0, width as i32, height as i32);

            let dst_texture = resources.add_texture(self.make_texture(dst_image)?);
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            self.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(dst_texture),
                0,
            );
            let status = self.gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if status != glow::FRAMEBUFFER_COMPLETE {
                return Err(format!("incomplete framebuffer: 0x{status:x}"));
            }

            let src_texture = resources.add_texture(self.make_texture(src_image)?);
            let program = match spec.alpha {
                AlphaMode::Opaque => &self.programs.opaque,
                AlphaMode::Same => &self.programs.same_alpha,
                AlphaMode::Other => &self.programs.other_alpha,
            };

            self.gl.use_program(Some(program.program));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(src_texture));
            self.gl.uniform_1_i32(
                self.gl
                    .get_uniform_location(program.program, "u_tex")
                    .as_ref(),
                0,
            );

            if let Some(alpha_image) = alpha_image {
                self.gl.active_texture(glow::TEXTURE1);
                let alpha_texture = resources.add_texture(self.make_texture(alpha_image)?);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(alpha_texture));
                self.gl.uniform_1_i32(
                    self.gl
                        .get_uniform_location(program.program, "u_alpha")
                        .as_ref(),
                    1,
                );
            }

            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl.finish();

            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.use_program(None);
        }

        drop(resources);
        drop(buffer);
        let raw_fd = dst_fd.into_raw_fd();

        Ok(ConvertedTexture {
            fd: raw_fd,
            offset: dst_offset,
            stride: dst_stride,
            size: dst_stride as u64 * height as u64,
            modifier: dst_modifier,
        })
    }

    fn create_image(&self, desc: ImageDesc) -> Result<egl::Image, String> {
        let mut attributes = vec![
            egl::WIDTH as egl::Attrib,
            desc.width as egl::Attrib,
            egl::HEIGHT as egl::Attrib,
            desc.height as egl::Attrib,
            EGL_LINUX_DRM_FOURCC_EXT,
            desc.fourcc as egl::Attrib,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            desc.fd as egl::Attrib,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            desc.offset as egl::Attrib,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            desc.stride as egl::Attrib,
        ];
        if desc.modifier != 0 {
            attributes.extend_from_slice(&[
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                (desc.modifier & 0xffff_ffff) as egl::Attrib,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (desc.modifier >> 32) as egl::Attrib,
            ]);
        }
        attributes.push(egl::ATTRIB_NONE);

        unsafe {
            self.egl
                .create_image(
                    self.display,
                    egl::Context::from_ptr(egl::NO_CONTEXT),
                    EGL_LINUX_DMA_BUF_EXT,
                    egl::ClientBuffer::from_ptr(std::ptr::null_mut()),
                    &attributes,
                )
                .map_err(|error| format!("eglCreateImage failed: {error:?}"))
        }
    }

    unsafe fn make_texture(&self, image: egl::Image) -> Result<glow::Texture, String> {
        let texture = self
            .gl
            .create_texture()
            .map_err(|error| format!("create texture failed: {error:?}"))?;
        self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        self.gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        self.gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
        );
        self.gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        self.gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        (self.image_target)(glow::TEXTURE_2D, image.as_ptr() as *const c_void);
        Ok(texture)
    }
}

struct FrameResources<'a> {
    context: &'a GpuContext,
    images: Vec<egl::Image>,
    textures: Vec<glow::Texture>,
}

impl<'a> FrameResources<'a> {
    fn new(context: &'a GpuContext) -> Self {
        Self {
            context,
            images: Vec::new(),
            textures: Vec::new(),
        }
    }

    fn add_image(&mut self, image: egl::Image) -> egl::Image {
        self.images.push(image);
        image
    }

    fn add_texture(&mut self, texture: glow::Texture) -> glow::Texture {
        self.textures.push(texture);
        texture
    }
}

impl Drop for FrameResources<'_> {
    fn drop(&mut self) {
        unsafe {
            for texture in self.textures.drain(..) {
                self.context.gl.delete_texture(texture);
            }
        }
        for image in self.images.drain(..) {
            let _ = self.context.egl.destroy_image(self.context.display, image);
        }
    }
}

unsafe fn link_program(gl: &glow::Context, fragment_source: &str) -> Result<glow::Program, String> {
    let program = gl
        .create_program()
        .map_err(|error| format!("create program failed: {error:?}"))?;
    let mut shaders = Vec::new();
    for (kind, source) in [
        (glow::VERTEX_SHADER, VERTEX_SHADER),
        (glow::FRAGMENT_SHADER, fragment_source),
    ] {
        let shader = gl
            .create_shader(kind)
            .map_err(|error| format!("create shader failed: {error:?}"))?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            return Err(format!(
                "shader compile failed: {}",
                gl.get_shader_info_log(shader)
            ));
        }
        gl.attach_shader(program, shader);
        shaders.push(shader);
    }
    gl.bind_attrib_location(program, 0, "a_pos");
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        return Err(format!(
            "program link failed: {}",
            gl.get_program_info_log(program)
        ));
    }
    for shader in shaders {
        gl.detach_shader(program, shader);
        gl.delete_shader(shader);
    }
    Ok(program)
}

fn open_gbm_device() -> Result<gbm::Device<File>, String> {
    let mut candidates = vec!["/dev/dri/renderD128".to_string()];
    if let Ok(entries) = std::fs::read_dir("/dev/dri") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("renderD") {
                let path = format!("/dev/dri/{name}");
                if path != candidates[0] {
                    candidates.push(path);
                }
            }
        }
    }

    let mut last_error = String::from("no DRM render node found");
    for path in candidates {
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => match gbm::Device::new(file) {
                Ok(device) => return Ok(device),
                Err(error) => last_error = format!("{path}: gbm_create_device failed: {error}"),
            },
            Err(error) => last_error = format!("{path}: {error}"),
        }
    }
    Err(last_error)
}

thread_local! {
    static GPU: RefCell<Option<Result<GpuContext, String>>> = const { RefCell::new(None) };
}

/// Convert a LINEAR 32-bit dmabuf into a new ARGB8888 dmabuf on the GPU.
///
/// Returns `Ok(None)` when the GPU path is unavailable; callers should fall
/// back to the CPU converter.
pub fn convert(
    fd: RawFd,
    offset: u64,
    stride: u32,
    width: u32,
    height: u32,
    format: u32,
) -> Result<Option<ConvertedTexture>, String> {
    if source_spec(format).is_none() {
        return Ok(None);
    }
    let offset = match u32::try_from(offset) {
        Ok(offset) => offset,
        Err(_) => return Ok(None),
    };

    GPU.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let context = GpuContext::new();
            if let Err(error) = &context {
                eprintln!("myde-drm: GPU path unavailable, using CPU: {error}");
            }
            *slot = Some(context);
        }
        match slot.as_mut().expect("initialized above") {
            Ok(context) => context
                .convert(width, height, format, fd, offset, stride)
                .map(Some),
            Err(_) => Ok(None),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn converts_rgba_offset_one() {
        let gbm = match open_gbm_device() {
            Ok(gbm) => gbm,
            Err(error) => {
                eprintln!("skip: {error}");
                return;
            }
        };
        let width = 8u32;
        let height = 2u32;
        let mut source = gbm
            .create_buffer_object::<()>(
                width,
                height,
                gbm::Format::Argb8888,
                gbm::BufferObjectFlags::LINEAR | gbm::BufferObjectFlags::RENDERING,
            )
            .unwrap();
        let stride = source.stride();
        source
            .map_mut(0, 0, width, height, |mapped| {
                let bytes = mapped.buffer_mut();
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let offset = y * stride as usize + x * 4;
                        // RGBA8888 memory: A B G R.
                        bytes[offset] = 128;
                        bytes[offset + 1] = (10 + x) as u8;
                        bytes[offset + 2] = (20 + y) as u8;
                        bytes[offset + 3] = (30 + x + y) as u8;
                    }
                }
            })
            .unwrap();
        let source_fd = source.fd().unwrap();

        let converted = match convert(
            source_fd.as_raw_fd(),
            0,
            stride,
            width,
            height,
            crate::DRM_FORMAT_RGBA8888,
        )
        .unwrap()
        {
            Some(converted) => converted,
            None => {
                eprintln!("skip: GPU path unavailable");
                return;
            }
        };

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                converted.size as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                converted.fd,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        let data = unsafe { std::slice::from_raw_parts(ptr as *const u8, converted.size as usize) };
        for y in 0..height as usize {
            for x in 0..width as usize {
                let offset = y * converted.stride as usize + x * 4;
                assert_eq!(
                    &data[offset..offset + 4],
                    &[(10 + x) as u8, (20 + y) as u8, (30 + x + y) as u8, 128],
                    "pixel ({x}, {y})"
                );
            }
        }
        unsafe {
            libc::munmap(ptr, converted.size as usize);
            libc::close(converted.fd);
        }
    }
}
