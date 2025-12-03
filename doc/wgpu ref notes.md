WGPU reference notes

NavCore WGPU Integration Manual

This document provides a comprehensive reference for using WGPU (the Rust implementation of WebGPU) in the NavCore project. It covers the full graphics pipeline and best practices for offline vector chart rendering, dynamic overlays (e.g. Signal K data), and cross-platform performance on Raspberry Pi 4/5 and Apple Silicon Mac. The guide is structured into sections that mirror NavCore’s needs, including WGPU architecture, buffer/texture handling, bind groups, WGSL shader usage, pipeline configuration, instancing techniques, platform-specific optimizations, porting notes from QuteNav, ecosystem tools, and debugging/profiling strategies. Code snippets and design tips are included to illustrate key concepts, and references to official documentation are provided for further details.

1. WGPU Architecture (Device, Queue, Surface, Pipelines)

WGPU is a cross-platform GPU API in Rust that abstracts modern graphics backends (Vulkan, Metal, Direct3D12, OpenGL, etc.) . It is built on the WebGPU standard, offering low-level control and safety similar to Vulkan/Metal, but with a simpler, Web-friendly design  . Understanding WGPU’s core objects is the first step:
	•	Instance & Adapter: The starting point is an Instance, which represents WGPU as a whole on the system. From it, you request an Adapter – essentially a handle to a physical GPU (or software renderer) that meets your criteria (e.g. high-performance vs. low-power)  . Typically, you use Instance::request_adapter (optionally specifying a backend or power preference) and await the result. The adapter provides info about the GPU and supported features (like limits, optional features, etc.).
	•	Device & Queue: From the adapter, you then request a Device – a logical GPU device that you will use to create resources and execute work. This is an asynchronous operation in WGPU; NavCore can use the pollster crate’s block_on to simplify this in a synchronous context. The device creation returns both a Device and a Queue . The Device is your interface to allocate GPU resources (buffers, textures, pipelines, etc.) and is akin to an OpenGL context or Vulkan device. The Queue is an object for submitting command buffers and performing certain data uploads. WGPU queues are analogous to a graphics queue in Vulkan and are used to execute commands on the device asynchronously. All rendering and compute commands must ultimately be sent through a queue for the GPU to execute them.
	•	Surface & Swapchain: If you are rendering to a window (for live display), you create a Surface tied to a window (with Winit, you can use instance.create_surface(&window) – note this is unsafe due to raw window handle usage). The surface represents a platform-specific swapchain or drawable. You then configure the surface with a Surface Configuration: color format (e.g. Bgra8Unorm on most platforms), pixel size equal to the window size, vsync/present mode (e.g. Fifo for vsync). Example:

let surface_format = surface.get_supported_formats(&adapter)[0];
surface.configure(&device, &wgpu::SurfaceConfiguration {
    format: surface_format,
    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
    width: window_width,
    height: window_height,
    present_mode: wgpu::PresentMode::Fifo, // vsync enabled
});

A configured surface will yield a new frame texture each frame via surface.get_current_texture(), which you render into and then present (by calling frame.present() or dropping it at end of frame).

	•	Command Encoders & Render Passes: WGPU uses command buffers to record work for the GPU. You create a CommandEncoder from the device for each frame (or batch of work), then begin render passes or compute passes on it. A Render Pass is started with encoder.begin_render_pass(render_pass_descriptor), where you specify color and depth attachments (with load/store ops). Inside a render pass you set pipeline state, bind groups, and issue draw calls. When done, you call render_pass.end() (or drop it). After encoding all passes, you finish the encoder to produce a CommandBuffer, and then submit that to the queue. This explicit command recording might be new compared to immediate-mode OpenGL, but it allows WGPU to batch work efficiently and align with modern API design .
	•	Render Pipeline: A core concept in WGPU is the RenderPipeline, which encapsulates the entire state for drawing (except the dynamic viewport/blend constants). A pipeline in WGPU includes references to your shader modules (compiled GPU programs), the fixed-function state (primitive topology, culling mode, depth/stencil state, color blend state, etc.), and the bind group layouts (defining what resource slots are available to the shaders)  . You can think of it as a pre-baked GPU state object similar to a “pipeline state object” in Vulkan/Metal or a linked shader program in OpenGL, but also including rasterization state . For each category of rendering (e.g. drawing chart polygons, drawing lines, drawing icon sprites), NavCore will create a pipeline. Switching pipelines at draw time is allowed, but you should minimize pipeline switches for performance. It’s common to use one pipeline per shader or per major render pass.
	•	Shader Modules: Shaders are provided as WGSL source (or SPIR-V, GLSL if using appropriate features) and compiled into a ShaderModule via device.create_shader_module. The pipeline descriptor references the module and an entry-point function name for each stage (vertex/fragment). WGPU relies on the naga library internally to translate WGSL into the correct native shader format (SPIR-V for Vulkan, MSL for Metal, etc.) , so you don’t have to deal with those conversions.

Importantly, WGPU is thread-safe and uses reference counting for its objects. You can clone handles like Buffers or Textures cheaply and share them (they internally reference the same GPU object) . When you drop an object, GPU memory is freed only after pending GPU work is done using it. Also note that WGPU performs API validation in debug mode – it will check that you follow the spec (e.g. not using resources after they are dropped or before they are ready). If something is wrong, it will log errors/warnings rather than just failing silently (more on debugging later).

Summary: Initialize WGPU by choosing an adapter (the Pi or Mac GPU) and obtaining a device+queue  . Create a surface for on-screen rendering (if needed) and configure it. Organize your drawing via command encoders and render passes. Pre-create render pipelines encapsulating shaders and state. This architecture sets the stage for efficient rendering across platforms.

2. Buffer and Texture Usage (Staging, Tiling, Updates)

Buffers and textures are the fundamental GPU resource types for storing data. In WGPU, you must be explicit about their usage and carefully manage updates, especially on memory-constrained platforms. This section covers how to create and use buffers/textures for NavCore’s tile-based chart data and dynamic content, including staging uploads and tile management strategies.

Buffers in WGPU: A Buffer is a contiguous block of GPU memory. Buffers store vertex arrays, index arrays, uniform data, storage data, etc. WGPU requires specifying how you will use a buffer at creation time via usage flags (e.g. VERTEX, INDEX, UNIFORM, COPY_DST for CPU-to-GPU uploads, etc.). This allows backends to choose optimal memory types. A buffer is created with a size (bytes) and usage flags; it cannot be resized later, so you either allocate big enough or create new ones as needed. As a definition: “A buffer is a blob of data on the GPU… contiguous and generally used to store arrays or structures.” .

For example, to create a vertex buffer from Rust data, you can use the convenient util::DeviceExt::create_buffer_init which internally maps the buffer and copies the data on creation  :

use wgpu::util::DeviceExt;
let vertex_data: &[u8] = bytemuck::cast_slice(&vertices);
let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
    label: Some("Tile Vertex Buffer"),
    contents: vertex_data,
    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
});

Here we marked it as COPY_DST as well so we can update it after creation. If the data is static, you could omit COPY_DST. (We use the bytemuck crate to cast a slice of Vertex structs to bytes for GPU upload, as shown above .)

Staging Buffers and Map vs. Write: On platforms like Web or without special features, WGPU does not allow direct CPU mapping of arbitrary GPU buffers by default (to ensure portability)  . Instead, the typical pattern for updating buffers is to use the Queue::write_buffer method. write_buffer lets you copy data from the CPU to a slice of a buffer. Under the hood, WGPU will allocate a temporary staging buffer if needed and schedule a copy on the GPU for the next submission  . This is simple to use:

queue.write_buffer(&vertex_buffer, 0, new_data_bytes);

However, note that write_buffer is deferred; it doesn’t immediately block the CPU. It enqueues the data to be copied when you next submit a command buffer on that queue . This means you can call it multiple times per frame to update different buffers, and all will be executed together.

For frequent, dynamic updates (like moving overlay symbols every frame), there are a few strategies:
	•	Mapped at Creation: If you create a buffer with mapped_at_creation=true, you can get a CPU-accessible slice right away to write initial data . After writing, you must call buffer.unmap(). This is great for one-time uploads (initialization) since it avoids extra copies. It’s used internally by create_buffer_init.
	•	Staging Belt: WGPU provides a utility called StagingBelt to efficiently handle many small buffer updates each frame. It manages a ring-buffer of staging memory to avoid allocating a new temporary for every update . If NavCore needs to update lots of small uniforms or dynamic vertex chunks per frame (e.g. dozens of Signal K data points), using a StagingBelt can reduce overhead. The StagingBelt essentially lets you write into a mapped memory once, then it issues one big copy for all updates. Steps:
	1.	Call staging_belt.write_buffer(&mut encoder, &buffer, offset, size, &device) to obtain a BufferViewMut to CPU-write .
	2.	Write your data into that view.
	3.	After encoding commands, call staging_belt.finish() to finalize copies , then submit the encoder.
	4.	After submission, call staging_belt.recall() to recycle the memory .
This avoids an extra copy compared to queue.write_buffer by recycling internal buffers . It’s ideal for many small updates (like per-object uniforms) where queue.write_buffer overhead accumulates.
	•	Direct Mapping with Feature: On native platforms with unified memory (like RPi and Apple M1), WGPU offers an optional feature Features::MAPPABLE_PRIMARY_BUFFERS to allow mapping GPU-local buffers directly . If enabled when requesting the device (check that the adapter supports it), you could map a buffer with the MAP_WRITE usage and write to it directly each frame, skipping staging. Use with caution: this is only beneficial on integrated memory systems , and on discrete GPUs it would hurt performance (so WGPU might not enable it on those). On Raspberry Pi (which shares RAM) and Mac (unified memory), this can simplify things – you could keep a buffer per tile mapped persistently for updates. But the safer approach is to use write_buffer or StagingBelt, which work everywhere and let WGPU handle the details.

Texture usage and updates: NavCore will use textures for things like chart symbol images (icons), possibly pattern fills, and maybe offscreen tile caching. Creating a texture in WGPU also requires specifying usage flags (TEXTURE_BINDING for sampling, RENDER_ATTACHMENT for render targets, COPY_DST for uploads, etc.) and the format/size. For example, a symbol atlas texture might be created as:

let texture = device.create_texture(&wgpu::TextureDescriptor {
    label: Some("Symbol Atlas"),
    size: wgpu::Extent3d { width: atlas_width, height: atlas_height, depth_or_array_layers: 1 },
    mip_level_count: 1,
    sample_count: 1,
    dimension: wgpu::TextureDimension::D2,
    format: wgpu::TextureFormat::Rgba8Unorm,
    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
});

To upload pixel data into a texture, you have two main paths:
	•	Use queue.write_texture for a simple CPU-to-GPU upload. This is great for small textures or one-time loads. You provide a TextureCopyView (destination texture + mip/region) and the data (as bytes plus layout info like bytes_per_row). Like write_buffer, this is done on the CPU side and scheduled on next submit. It’s synchronous from the CPU perspective (the function copies your data into a staging memory immediately). Be mindful of the limit: extremely large writes via this method might internally chunk the data.
	•	For larger or repeated updates (like a video or frequently changing overlay image), consider using a buffer as intermediate. You can put pixel data into a mapped buffer and then use encoder.copy_buffer_to_texture to transfer it. This can be more efficient if you want to update only a portion of a texture or do batched uploads. The row padding must be observed: WGPU expects data rows aligned to COPY_BYTES_PER_ROW_ALIGNMENT (usually 256 bytes) .

Tile-based rendering resources: In NavCore’s chart tiling system, each map tile likely has its own geometry. You might maintain one vertex buffer (and index buffer) per tile, or use larger buffers that store multiple tiles’ geometry. There are trade-offs:
	•	Per-tile buffers: This is straightforward – when a tile is loaded, create a buffer for its vertices/indices. This localizes memory and makes it easy to free when the tile is evicted (just drop the buffer). The overhead is having many buffer objects; WGPU can handle hundreds or thousands of buffers, but each one is a GPU allocation, so extremely many small buffers might cause fragmentation or slight CPU overhead. Still, for a moderate number of visible tiles (say the view might show tens of tiles), this is fine.
	•	Chunked large buffers: Alternatively, you can allocate a big buffer and sub-allocate regions for tiles (e.g. a chunk allocator as mentioned in the porting plan). This can reduce the number of distinct GPU allocations and might improve memory locality. On Raspberry Pi, using contiguous memory for large allocations could have benefits (the Pi’s OS can use CMA - Contiguous Memory Allocator - for big chunks). Implementing this means you manage offsets for each tile’s data within a giant buffer. If a tile is unloaded, you could mark its chunk free for reuse. This is complex but can be worthwhile if fragmentation of many small buffers becomes an issue. In practice, start simple (per tile) and optimize if needed.

Updating tile content: Chart data is mostly static once loaded, but dynamic overlays or style changes can require buffer updates. If you need to update vertex data (say, if we implement dynamic generalization or user edits), you can map or write_buffer as described. If a tile is completely redrawn (e.g. user zooms and we decide to regenerate geometry at a different level of detail), you might just create a new buffer and drop the old – simpler than in-place updates, given WGPU’s creation is fairly cheap. Dropping old buffers will free memory after the GPU finishes using them (WGPU ensures no use-after-free).

For texture tiling (if NavCore ever uses a mosaic of textures, e.g. for raster data or caching vector renders into tiles), you’d manage a collection of texture objects. WGPU texture arrays (2D array textures) can sometimes simplify this by packing multiple layers in one texture, but only if they share resolution and format. Otherwise, multiple texture objects is fine. Use sampler descriptors appropriately (on Pi, prefer linear filtering for smooth scaling, but note that Pi’s GPU might struggle with very large textures – consider smaller atlas sizes or mipmaps for scaling down charts if needed).

In summary for buffers/textures: Always specify correct usage flags (e.g. include COPY_DST if you plan to update, RENDER_ATTACHMENT if you will render to it, etc.). Use queue.write_buffer/texture for convenience, but consider StagingBelt or direct mapping for frequent updates to minimize copies . Organize chart geometry per tile for clarity, and free GPU resources for tiles that go off-screen to stay within memory limits. For symbol and pattern textures, create atlases or arrays and update them with the above methods. This explicit data management might be more involved than OpenGL’s implicit behavior, but it gives you control to optimize for embedded devices.

3. Bind Groups and Uniform Management

Bind Groups in WGPU are how you bind resources (buffers, textures, samplers) to shaders. They are analogous to “descriptor sets” in Vulkan or to binding multiple uniforms/textures at once in OpenGL. A Bind Group Layout defines the types of resources (and their shader visibility), and a Bind Group is an instance of that layout bound to actual GPU resources. NavCore’s renderer will use bind groups to manage uniform data like transformation matrices and styling parameters, as well as bind the chart symbol atlas texture and samplers.

Key points for bind groups and uniforms:
	•	Uniform Buffers: A common way to pass dynamic values each frame or draw is with a uniform buffer (a buffer with BufferUsages::UNIFORM). In WGSL, you’ll define a uniform struct and it gets bound to the shader. For example, you might have:

struct ViewUniform { 
    view_proj: mat4x4<f32>; 
    zoom_level: f32; 
    // ... (pad to 16-byte alignment as needed)
};
@group(0) @binding(0)
var<uniform> view: ViewUniform;

In Rust, you create a buffer of size sizeof(ViewUniform) with usage UNIFORM | COPY_DST, and update it each frame with the current matrix, etc. Then you create a bind group layout for group(0) binding(0) as a uniform buffer binding. Finally, create a bind group that holds the buffer. Once created, you can set this bind group on the render pipeline and all shaders with that layout can access the uniform.
WGPU requires alignment for dynamic uniform buffers: if you store an array of uniform structs in one buffer (to use dynamic indexing or to offset for multiple objects), each element must be aligned to 256 bytes by default (this is the min_uniform_buffer_offset_alignment, often 256) . Keep this in mind if you plan to use one big buffer for many small uniforms.

	•	Dynamic Bindings vs. Separate Bindings: In WebGPU’s model, you cannot bind a different buffer for each draw without creating a new bind group or using dynamic offsets. Creating bind groups frequently (per object) can be costly; instead, design the bind group layouts to allow dynamic uniform offsets when possible. For example, you could allocate a large uniform buffer containing an array of per-tile or per-layer uniforms, and then use one bind group (with one binding) for all of them, changing the offset on each draw. To do this, mark the binding as dynamic in the layout (in Rust: BufferBindingType::Uniform { has_dynamic_offset: true }). Then when issuing a draw, call render_pass.set_bind_group(0, &bind_group, &[offset]) to point to the correct chunk. This way you don’t need a distinct bind group per tile if all tiles share the same layout structure in the buffer. Dynamic offsets are efficient, but remember the offset must be a multiple of 256 bytes .
	•	Multiple Bind Groups: Shaders can use multiple bind groups (group0, group1, etc.), each with different frequency of update. A common convention:
	•	Group 0: Frame uniforms (camera matrices, global settings – updated once per frame).
	•	Group 1: Per material or per tile (e.g. lighting or style parameters, or tile-specific data – updated when you switch tile or layer).
	•	Group 2: Textures/Samplers (atlas textures or pattern textures that remain constant across many draws).
Using separate groups for these allows, for example, binding the texture atlas once and not rebinding it for every object, just keep it in a group that stays set . Meanwhile, group 0 could be re-bound each frame to a new uniform buffer with the camera matrix. This grouping minimizes bind changes. You design the PipelineLayout of each pipeline to include these group layouts in a specific order (group indices in WGSL must match the pipeline layout order).
	•	Uniform vs Storage: WebGPU also supports storage buffers (var<storage> in WGSL). For large data arrays (like a large table of points or attributes that shaders might need), a storage buffer can be used. Uniform buffers are typically limited in size (maybe 64KB at minimum), whereas storage buffers can be larger (multiple MBs). However, storage buffer accesses are slower and more flexible (read-write). For NavCore, uniforms suffice for things like transformation matrices, and vertex buffers handle per-vertex data. Storage buffers might be considered if you want to pass a lot of custom data (for instance, an array of lighthouse flash period values for each light – but even that likely fits in a uniform). Uniforms are constant during a draw call and cannot be written by shaders in WebGPU.
	•	Samplers and Textures in Bind Groups: To use a texture in a shader (e.g. the chart symbol atlas), you need two things bound: the texture itself and a sampler. The sampler is a separate binding that defines filtering and wrapping behavior. In WGSL:

@group(2) @binding(0) var atlas_texture: texture_2d<f32>;
@group(2) @binding(1) var atlas_sampler: sampler;

You would create a bind group layout for group 2 with a TextureBinding (with sample type matching the texture format) and a SamplerBinding. The bind group for the atlas would then be created once (since the atlas is static or rarely changes), and set at the beginning of a frame. All draws that need the atlas can reuse that binding. This is more efficient than binding each icon texture separately for each draw as done in older OpenGL; we batch all symbols into one atlas and bind once.

	•	Push Constants (optional): WebGPU supports push constants as a native-only feature (small bits of data updated directly in command buffer) , but WGSL as of writing doesn’t have them in the core spec. If NavCore needs to send a few bytes very frequently (like a single value that changes per draw and you want to avoid buffer updates), push constants could be enabled via Features::PUSH_CONSTANTS. However, given the limitations (only 32 bytes on Metal, for instance) and that it’s not Web-compatible, you might opt to stick with uniform buffers. For cross-platform simplicity, this manual will assume uniforms for most cases. (Push constants would primarily help if you had many tiny pieces of data to update per draw – e.g. a single float – and you wanted to avoid a 256-byte uniform overhead. It’s a micro-optimization that can be added if profiling shows a bottleneck.)
	•	Binding Resource Lifetime: Once you create a bind group referencing a buffer or texture, you must not drop the underlying resource while the bind group is in use. If you plan to delete a tile’s buffer, ensure it’s not still bound for a draw yet to be submitted. In practice, if you drop a buffer that’s still queued in a command buffer, WGPU will keep it alive until the GPU is done (thanks to internal refcounting). But it’s good practice to update/destroy bind groups when their resources change. For example, if you rotate out a new uniform buffer each frame for the camera (to avoid writing to the same one being read by GPU), you might have two bind groups and ping-pong between them.

Bind Group creation example: Suppose we have:
	•	Group 0: one uniform buffer (the view/projection matrix).
	•	Group 1: one uniform buffer (tile-specific data, e.g. tile model matrix or styling constants).
	•	Group 2: one texture + one sampler (symbol atlas).

We would create three BindGroupLayout objects (for group0,1,2). Then create a PipelineLayout combining those layouts in that order. In code:

let group0_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
    entries: &[
       wgpu::BindGroupLayoutEntry { // binding 0: uniform buffer
           binding: 0,
           visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
           ty: wgpu::BindingType::Buffer {
               ty: wgpu::BufferBindingType::Uniform,
               has_dynamic_offset: false,
               min_binding_size: None
           },
           count: None,
       }
    ],
    label: Some("Group0 Layout"),
});

(similar for group1 and group2 with their resources). Then:

let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
    label: Some("Main Pipeline Layout"),
    bind_group_layouts: &[&group0_layout, &group1_layout, &group2_layout],
    push_constant_ranges: &[],
});

When binding, if we didn’t use dynamic offsets, we can create one bind group per buffer/texture and reuse it. For example:

let group2 = device.create_bind_group(&BindGroupDescriptor {
    layout: &group2_layout,
    entries: &[
      { binding: 0, resource: wgpu::BindingResource::TextureView(&atlas_texture_view) },
      { binding: 1, resource: wgpu::BindingResource::Sampler(&atlas_sampler) },
    ],
    label: Some("Atlas bind group"),
});

We do this once, store it. For group0 (view uniform), if it updates every frame, you have two options: recreate the bind group each frame after writing the buffer, or (more efficient) create it once and just update the buffer contents. In WGPU, you can update buffer content without re-making the bind group as long as the buffer is the same. So typically, create bind groups at initialization and reuse them each frame; just update the underlying uniform buffers via queue.write_buffer. Recreating bind groups is not extremely heavy, but it is best avoided per-frame on performance-sensitive devices.

Uniform management patterns: To avoid stalling the GPU when updating uniform buffers, a common pattern is double-buffering the uniform data. For example, have two uniform buffers for the view matrix and alternate each frame, so the GPU reads one while the CPU writes the other. With WGPU’s model, this is often not needed if you always write via queue.write_buffer, because it schedules the write and doesn’t block. But if you were mapping a buffer and writing in place, double buffering would ensure you’re not writing to the one the GPU is currently reading (which would otherwise require a sync). Given WGPU’s internal synchronization, you might only consider double-buffering if you measure stutters.

Another tip is grouping multiple related uniforms into one struct to reduce bind group count. For instance, rather than separate bindings for projection matrix, and other global settings, pack them into one GlobalUniforms struct and use one binding. Fewer bindings can be marginally faster to set. The trade-off is less flexibility if parts update at different rates (but since setting one uniform or the whole struct is the same cost, it’s fine).

Summary: Bind groups define what data your shaders see. Design your WGSL to expect certain group/binding indices for camera matrices, style info, and textures. Create corresponding bind groups in Rust, and update the underlying buffers each frame or as needed. Use dynamic offsets if you want to use one bind group for many objects by indexing into a big uniform buffer . This will reduce the churn of creating bind groups or binding many different ones. By managing uniforms and textures through bind groups, you get explicit control and can ensure maximum reuse (e.g. bind the atlas once, rather than switching texture every draw as in older APIs).

4. WGSL Shader Architecture and Conventions

WebGPU Shader Language (WGSL) is the shading language for WGPU. It is a modern, strongly-typed language that will feel familiar if you know GLSL or HLSL, but it has its own conventions. WGSL is designed to easily translate to all backend shader formats , and it is the preferred way to write shaders in WGPU (support for SPIR-V or GLSL input is optional and likely to be phased out) . Here we outline how NavCore’s shaders should be structured in WGSL and important conventions to follow.

Shader stages and entry points: You will typically write at least a vertex shader and a fragment shader (and possibly compute shaders for other tasks). In WGSL, each shader entry point is a function annotated with @vertex, @fragment, or @compute. For example:

@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) tile_coord: vec2<f32>) -> VertexOutput {
    var out: VertexOutput;
    // ... transform position ...
    out.clip_position = view_proj * vec4<f32>(position, 0.0, 1.0);
    out.world_coord = tile_coord;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // e.g., sample a pattern or atlas using in.world_coord
    let color = textureSample(atlas_texture, atlas_sampler, in.tex_coords);
    return color;
}

Each entry point can take inputs:
	•	Vertex shader inputs typically come from vertex buffers. They are labeled with @location(N) where N matches the vertex attribute location in the pipeline. They can also take built-in inputs like @builtin(vertex_index) or @builtin(instance_index) if needed (similar to GLSL’s gl_VertexID, etc.).
	•	Fragment shader inputs are the outputs of the vertex shader that are interpolated. In the example, VertexOutput is a struct we defined to carry data from VS to FS. You mark the position output with @builtin(position) to signify the clip-space position (equivalent to writing to gl_Position in GLSL)  . Other fields in the output struct are interpolated by default (perspective-correct interpolation for float vectors). You can control interpolation (flat vs smooth) via annotations if needed.

The fragment shader must output the final color (and optionally depth, or multiple render targets if using). We annotate the return with @location(0) to denote the first color attachment . If we had multiple render targets, we’d return a tuple or struct with multiple @location outputs.

Binding resources in WGSL: As discussed in Section 3, resources like uniforms and textures appear in WGSL as var<uniform> or var<storage> or plain texture_2d, etc., with @group and @binding attributes to match your bind group layout. For example:

@group(0) @binding(0) var<uniform> view: ViewUniform;
@group(2) @binding(0) var atlas_texture: texture_2d<f32>;
@group(2) @binding(1) var atlas_sampler: sampler;

This must mirror exactly what you set up on the Rust side (same group and binding numbers, same resource types). If they mismatch, WGPU will throw a binding type error when you create the pipeline. Note that WGSL currently doesn’t support #include or macros, so keeping these definitions synchronized is a manual (or build-script) task. You might choose to generate your WGSL from a common source of truth or do compile-time checks by reflecting on the SPIR-V, but often simplest is to carefully maintain these.

WGSL language tips:
	•	Types must match exactly; WGSL does not do implicit numeric conversions. For example, if you have an u32 and you need an f32, you must cast explicitly (f32(my_uint)). The tutorial shows this in practice  .
	•	WGSL uses a C-like syntax. Semicolons are required. It supports let (immutable) and var (mutable) for local variables . It has control structures (if, for, switch) similar to GLSL, but no while (you can use for, with continuing).
	•	No default precision qualifiers like GLSL’s highp/mediump – all types are full precision by default (which is usually fine on modern GPUs).
	•	Matrices in WGSL are in column-major order by default (matching GLSL and most GPU math libraries). If you use Rust libraries like cgmath or glam to build matrices, they also use column-major, so you can typically pass them directly. Just be careful if manually transposing somewhere – you usually do not need to transpose as you did in some OpenGL cases, because OpenGL’s GLM often gave row-major memory unless specified. In short, ensure the memory layout of your uniform matrices matches WGSL’s expectation (column-major). By default, a mat4x4<f32> in WGSL expects the same memory layout as 16 floats in column-major (i.e., positions of basis vectors in contiguous memory).
	•	Coordinate systems: WebGPU’s normalized device coordinate (NDC) system is similar to OpenGL’s: X: -1..1 left-right, Y: -1..1 top-bottom, Z: 0..1 (WebGPU uses 0 to 1 for depth by default, whereas OpenGL used -1..1). The difference is the framebuffer coordinate system: in OpenGL, the origin is bottom-left, in DirectX/Metal, it’s top-left. WebGPU standardizes this by flipping the Y in the projection when using certain backends. In practice, WGPU on Vulkan/Metal has a transform for the surface that might auto-flip. If your rendered image appears upside-down on some platform, you may need to flip the Y in your projection matrix. One way is to scale the Y by -1 in the projection for those backends. WGPU’s SurfaceConfiguration has an composite_alpha and surface orientation setting on some platforms, but currently it’s safest to handle it manually if needed. Since NavCore likely uses an orthographic Mercator projection, just ensure that what you consider “up” (north on the map) appears correctly oriented on all platforms. Testing on Mac (Metal) vs. other devices will confirm if a flip is needed. If using winit, the coordinate system for window pixels is typically top-left origin, so matching that in NDC makes sense (i.e., likely you will invert Y in the projection compared to OpenGL’s projection).
	•	No built-in gl_FragCoord: If you need the fragment’s pixel position (for say, screen-space effects or checking even/odd for stippling), WGSL provides @builtin(position) in fragment shaders too, but it gives the position in pixels as a 4-float vector where .xy are the pixel coordinates and .zw the 1/width and 1/height IIRC. Actually, in WGSL, @builtin(position) in fragment stage yields a vec4<f32> whose x,y are the pixel center coordinates (with (0,0) at top-left of the target if using default). If you needed it, you could interpolate it from the vertex shader by just passing the position, or enable position in fragment directly by declaring fn fs_main(@builtin(position) coord: vec4<f32>). Use this carefully as it might vary by backend coordinate space.
	•	Discarding fragments: In WGSL, you can discard a fragment by calling discard; inside the fragment shader. However, avoid using discard unless necessary (e.g. for masking out certain pixels) because it disables early depth optimization . For example, if implementing S-52 area pattern masks, try to use alpha blending with an alpha mask texture instead of discard, so that early-Z can still function. If you must use discard (say for a complex stipple pattern that can’t be done otherwise), be aware on tile-based GPUs (RPi’s Broadcom, Mali, Apple) it can hurt performance by forcing per-sample processing  . An alternative is to pre-compute geometry for holes or transparency rather than discarding in shader.
	•	WGSL and QuteNav’s GLSL: If porting existing GLSL code from QuteNav, note differences:
	•	No implicit uniform declarations; everything must be in var<uniform> with explicit binding.
	•	texture() function calls in GLSL become textureSample(texture, sampler, coord) in WGSL.
	•	No default precision, no layout(location=X) in shader (instead use the attribute syntax shown).
	•	No in/out keywords for stage I/O; use function return types and parameters.
	•	No vec4 color = texture(u_sampler, uv); fragColor = color; style – instead return the value.
	•	If QuteNav used GLSL extensions or older features (e.g. gl_PointCoord, or built-in variables like gl_FragColor), you’ll need to adapt. For point sprites (gl_PointCoord equivalent), WebGPU doesn’t have fixed-function point sprites; you’d render textured quads for symbols instead (which likely QuteNav already did).

Coordinate transformations and Mercator in shaders: Likely, most coordinate transforms (lat/long to Mercator XY) will be done on CPU and passed as vertex coordinates or via uniforms (NavCore already computes Mercator projection matrix on CPU ). The vertex shader will then just apply a matrix to go to clip space. If needed, you could also do Mercator formula in shader (the math involves transcendental functions for lat->Y which WGSL can do, e.g. tan and log for Mercator Y = R * log(tan(pi/4 + lat/2))). But doing it on CPU is fine and possibly more precise for double precision needs. Use the GPU for what it’s best at: lots of linear algebra on many vertices.

WGSL tooling: Remember that WGPU will produce validation errors if your WGSL is not valid or uses features not supported on the target. Use the references like WebGPU spec or tutorials for WGSL syntax. If you run into compile errors, the error messages come from naga and can be cryptic (they might include a SPIR-V translation hint). A strategy is to compile your WGSL with naga offline (there are CLI tools, or use Chrome/Firefox WebGPU debugger) to pinpoint issues. Another tool: shaderc can compile GLSL to SPIR-V which WGPU can ingest if you enable the glsl feature, but note that doing so bypasses WGSL’s stricter checks and might allow things that then fail at runtime. As of 2025, it’s recommended to stick to WGSL for forward-compatibility .

In summary, WGSL is straightforward once you get used to it. Keep your shaders organized (perhaps one WGSL file per pipeline, or even share a common library of code via your own text include mechanism). Use struct definitions to communicate between stages. Exploit WGSL’s features like arrays, functions, let bindings for clarity – it’s quite expressive. And always consider performance on the target GPUs: e.g., minimize branching and divergence in shaders (especially on Pi’s simpler GPU), prefer vec4 operations over scalar when possible (GPUs like 4-wide operations), and avoid heavy loops in the fragment shader (precompute on CPU if you can, or use lookup textures).

5. Render Passes and Pipeline State (Depth, Culling, Blending)

Configuring the pipeline and render passes correctly is critical for achieving the desired visual output and performance. This section discusses how to set up depth testing (for z-ordering and early depth culling), face culling (to skip drawing backfaces of polygons), blending (for transparency and overlay effects), and how to structure render passes in NavCore.

Render Pass basics: A render pass in WGPU is begun with a descriptor that specifies attachments:
	•	One or more color targets (the surface frame or offscreen texture) with a load operation (clear or load existing content) and a store operation (store or discard the result).
	•	An optional depth/stencil target with its own load/store ops.

In code:

let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor { label: Some("Main Encoder") });
let render_pass_desc = RenderPassDescriptor {
    color_attachments: &[
        Some(RenderPassColorAttachment {
            view: &frame.view,
            resolve_target: None,
            ops: Operations { 
                load: Operations::LoadOp::Clear(Color::BLACK), // clear to black (e.g. ocean background)
                store: true,
            },
        })
    ],
    depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
        view: &depth_texture_view,
        depth_ops: Some(Operations {
            load: LoadOp::Clear(1.0), // 1.0 = far, clearing depth each frame
            store: true,
        }),
        stencil_ops: None,
    }),
    // ... omit stencil if not using
};
let mut rpass = encoder.begin_render_pass(&render_pass_desc);
// then inside rpass: set pipeline, set bind groups, draw...

By clearing depth to 1.0 (the far plane) and clearing color at the start, we ensure no remnants of previous frames. This is usually what you want unless you plan to overlay on an existing image. (On tiled GPUs, it’s actually beneficial to clear rather than load old data, to avoid reading from memory  .)

Depth Testing and Early-Z: Enabling depth testing allows the GPU to skip drawing fragments that are behind something already drawn. For 2D chart rendering, we can leverage depth in a creative way to manage layer order and improve performance:
	•	You’ll create a depth texture (for example, 24-bit or 32-bit) and include it in the render pass. In the pipeline state, set depth_stencil to something like:

depth_stencil: Some(DepthStencilState {
    format: TextureFormat::Depth24Plus, // a typical format
    depth_write_enabled: true,
    depth_compare: CompareFunction::Less, // pass if fragment is closer (less depth) 
    stencil: Default::default(), // not using stencil here
    bias: Default::default(),
})

Then in the vertex shader, you must output a Position.z that corresponds to the layer depth. For example, you could encode different chart layers or feature types at different Z values. Perhaps all base polygons (land, water) get z=0.5, and symbol markers get z=0.0 (closer to camera), etc. By doing so, when the pipeline writes depth, the GPU will know a symbol at 0.0 is in front of a polygon at 0.5 and will skip fragments of the polygon underneath where the symbol overlaps (if drawn afterward, though ideally you draw opaque first then transparent). However, sorting by depth in 2D can be tricky if you don’t actually have a concept of elevation. Instead, consider using depth as follows:
NavCore plan suggests “Enable depth test + early-Z for area layers behind symbols” . One approach:
	•	Draw all area fills (polygons) first, with depth test enabled and depth write enabled. Assign them all the same depth value (e.g. 0.9 for “far”). They will essentially just fill the depth buffer where drawn.
	•	Then draw symbols and lines with depth test enabled but depth write disabled, and use a nearer depth value (e.g. 0.1 for “near”). Because symbols are given a smaller depth (closer), they will appear on top of areas regardless of draw order (even if a symbol’s draw call came before a polygon’s, the depth test ensures polygon doesn’t override it if polygon’s depth is further). Actually, to make that work, polygons must write depth = 0.9, symbols test against depth but since symbols are at 0.1 < 0.9, a “Less” depth compare means they will pass even if polygon pixel was drawn (since 0.1 < 0.9). If a symbol is drawn before a polygon, the polygon would still draw because 0.9 is not less than the symbol’s 0.1 (polygon is further, so it would fail the depth test because depth test is “Less” meaning fragment passes if new depth < existing). Actually, if symbol drawn first wrote 0.1 at that pixel, then later polygon at 0.9 comes, the test says 0.9 < 0.1? No, that fails, so polygon pixel behind symbol would be skipped – which is exactly what we want: prevent polygon from overdrawing symbol!** This way the order of draw calls doesn’t matter for those – depth logic enforces the layering. However, usually you’d anyway draw polygons first, symbols last, for logical layering and to avoid needing that trick. The main performance benefit of depth is if you draw many overlapping polygons, the GPU can avoid shading pixels of ones behind others.
Also, by drawing opaque objects front-to-back (closest first), you maximize early-Z efficiency . But usually in a 2D map, you don’t have truly overlapping geometry in depth, it’s more layering. Nonetheless, imagine two opaque polygons covering the whole screen – if you draw the nearer one first and write depth, the second (behind) will fail depth test everywhere and effectively not shade anything (the GPU can early-reject those fragments). This saves a lot of fragment shader work. So for charts, if certain layers occlude others completely (e.g. land might occlude background water), using depth and drawing land first could let water’s fragments be skipped. However, careful: if the polygons are coplanar (same depth) and overlapping, depth test can cause flickering (“z-fighting”). So assign clear depth separation values per layer or disable depth write when not needed.
On Raspberry Pi’s tile-based renderer, early-Z operates a bit differently (tile GPUs often do hidden surface removal in a tile, even without a depth buffer). Still, using a depth buffer and testing is beneficial and recommended, as it aligns with the hardware’s expectations and enables standard GPU hidden-surface optimizations . The Pi’s GPU (VideoCore) and Apple’s GPU (tile-deferred) will both handle depth well as long as you avoid discard and keep depth comparisons simple.

	•	Depth format: Depth24Plus is a safe default. It picks a 24-bit or 32-bit depth format available on the platform (with or without stencil). We don’t really need stencil for charts, unless implementing complex masking; if needed, you could use Depth24PlusStencil8. Depth buffer size is typically the same as the surface size. Ensure you recreate the depth texture when the window resizes.
	•	Face Culling: By default, WGPU does not cull any triangle faces (all triangles are drawn). You can enable back-face culling in the pipeline’s primitive state:

primitive: PrimitiveState {
    front_face: FrontFace::Ccw, // define vertex winding order that counts as front
    cull_mode: Some(Face::Back), // cull back faces
    // other settings like topology, strip_index_format if needed
    ..Default::default()
},

If your polygon triangulation uses consistent winding (counter-clockwise vertices for front faces, for example) , enabling backface culling will avoid drawing the “inside” faces. In 2D maps, all triangles of a fill might face the camera anyway (no actual 3D rotation), but if some polygons have holes or the triangulator outputs both orientations, you need to be careful. It might be fine to enable culling to skip drawing redundant back sides of extruded shapes (not common in flat maps) or if you inadvertently have overlapping geometry. Likely, enabling culling is safe and can slightly reduce fragment workload (roughly halving triangle drawing cost if every triangle’s backface is not drawn). However, if you see missing geometry, it could be that the winding is inconsistent and culling culled something it shouldn’t. In that case, either fix the winding in the generator or turn off culling.
According to the plan, NavCore already sets front_face = CCW  and presumably will use CCW for all visible geometry orientation. So we can enable backface cull.

	•	Blending: For any transparent or translucent rendering (like overlaying a semi-transparent radar image, or drawing lights that glow), blending must be configured. WGPU blending is set per color attachment in the pipeline’s color_targets. For example:

color_targets: &[
    Some(ColorTargetState {
        format: surface_format,
        blend: Some(BlendState::ALPHA_BLENDING),
        write_mask: ColorWrites::ALL,
    })
],

BlendState::ALPHA_BLENDING is a preset for common premultiplied-alpha blending (source alpha, one minus source alpha) which suits standard transparency. If using straight alpha (not premultiplied), you might use src_alpha/one_minus_src_alpha for color and one/one_minus_src_alpha for alpha. In WGSL, output colors should be premultiplied (or ensure blending factors align with your choice). For simple cases, use the preset and output colors with premultiplied alpha (e.g. if drawing a texture with alpha, sample it, it likely is premultiplied if you prepared it so, or multiply color by alpha in shader before output).
NavCore’s likely use of blending:
	•	Radar overlay: This could be a semi-transparent overlay (maybe as an image or colored polygon). You’d enable blending so underlying chart shows through. Possibly use additive blending (BlendFactor::One, BlendFactor::One) for radar if you want to “light up” the base map rather than cover it. The plan notes “Blend modes for radar overlay & translucency”  – meaning we might experiment with different blending (standard alpha vs additive).
	•	Translucent areas: If any S-52 style uses translucency (like a shallow water overlay that’s partially transparent blue), you’ll need blending for those draws.
	•	When drawing with blending, draw order matters: blended primitives should typically be drawn after opaque ones, and sorted from farthest to nearest to avoid artifacts (standard painter’s algorithm for transparency, since WebGPU doesn’t do order-independent transparency automatically). For 2D maps, you can often get away with drawing translucent overlays last, as a single layer, so sorting isn’t a big issue (all base map is opaque, then one translucent overlay on top).
Also note: do not write to depth when drawing transparent objects (otherwise they can occlude other transparent objects incorrectly). So for radar overlay or any translucent layer, set depth_write_enabled = false but still do depth test if you want it to respect the depth of terrain below (e.g. if an overlay should only appear on top of land if land is closer? In 2D that situation doesn’t apply – overlays likely cover everything regardless of depth). Probably simplest: for translucent overlays, depth test off or set compare always pass, so they just overlay. Or draw them in a separate pass with depth disabled.

	•	Multiple render passes: In a simple scenario, you can do all drawing in a single render pass to the surface (target and depth). Sometimes multiple passes are useful:
	•	If you want to render to an offscreen texture first (e.g. render the chart to a low-res texture for some effect, or do a multi-step effect like first render radar to texture, then blend to screen with a specific filter).
	•	If using different MSAA settings or needing to change the set of attachments (though WebGPU doesn’t support mid-pass attachment changes, you’d end the pass and start another).
By default, try to do everything in one pass for efficiency (tile-based GPUs prefer one pass because ending a pass may cause a tile buffer flush to memory)  . The plan’s performance notes indicate aiming to avoid unnecessary state changes and passes . For NavCore, a single render pass drawing all layers in correct order is feasible.

If at some point you have UI overlays drawn with a different pipeline (say a UI library or custom drawing), you could either integrate it into the same pass (if it shares the same swapchain target) or do a separate pass. For instance, some projects do the 3D/2D scene in one pass, then do an immediate GUI in another with blending on and depth off. That’s fine, just remember each extra pass might incur cost on mobile GPUs.
	•	MSAA (Multi-Sample Anti-Aliasing): WGPU supports MSAA by specifying pipeline.multisample.count > 1 and creating your render target with the same sample count (except surfaces, which can’t be MSAA directly – you render to an MSAA texture then resolve). On Raspberry Pi 4/5, MSAA 4x is supported (OpenGL ES 3.2 level, and Vulkan). However, MSAA increases memory and bandwidth use. The porting plan shows multisample.count: 4 in an example , indicating they plan to use 4x MSAA for quality. This will smooth jagged lines (important for diagonal lines on charts). It’s usually worth it if performance allows. On Pi, 4x MSAA is a trade-off: it will do 4 fragment shader executions per pixel on polygons unless covered by early-Z. Because charts are not super high resolution and mostly flat colored, Pi might handle it, but watch for fillrate issues when many tiles cover the screen. If needed, dropping to 2x or 0x on Pi specifically is an option via conditional configuration.

Pipeline Creation: It’s worth noting that creating a pipeline is somewhat expensive (hundreds of microseconds maybe). You should create all necessary pipelines at startup or level load, not mid-frame. NavCore will likely have a few pipelines: e.g. pipeline_lines, pipeline_polygons, pipeline_symbols. Possibly also variations for day/night mode if shader differ, or with/without some features. But avoid having an explosion of pipelines.

State changes minimization: When drawing, try to batch by pipeline and bind group state:
	•	Draw all objects that use pipeline A consecutively, so you set the pipeline once then issue multiple draw calls.
	•	Within that, try to minimize bind group switches. For example, if many symbols share the same atlas (group2 stays same) and only group1 (tile-specific) changes, the cost is low. But if you had to swap atlas textures (group2) often, maybe combine them if possible.
	•	This is why instancing (next section) is valuable: it lets you draw many instances with one bind operation.

Finally, always test different platform outputs. Use known reference images to verify that enabling depth or blending doesn’t change the visual result except as intended (depth shouldn’t show artifacts like unexpected hiding of symbols – if it does, likely a depth ordering issue to fix by adjusting when you enable depth test or the values used).

In summary, configure depth testing to help GPU skip drawing covered pixels (improving performance) and to enforce correct layering (ensuring symbols on top) . Use face culling if your geometry winding is consistent to skip invisible sides. Set up blending for any transparent overlays or effects (and handle their ordering). Usually, draw order will be: opaque background first (with depth writes), then possibly translucent stuff (with blending and no depth writes) last. These pipeline state settings, once tuned, will give correct visual layering akin to QuteNav’s layering system but now accelerated via GPU depth and blending logic.

6. Instancing and Batching (for Symbols and Overlays)

To maximize rendering performance on systems like RPi and to reduce CPU overhead, it’s crucial to batch draw calls and use instancing for repeating graphics. Instancing allows drawing many copies of a geometry (for example, a buoy symbol or a ship icon) in a single GPU draw call, with different transformations or colors for each copy. Batching refers to drawing multiple primitives together under one pipeline and bind state, rather than issuing separate draw calls for each.

Instanced drawing in WGPU: WGPU’s RenderPass::draw() and draw_indexed() functions have parameters for instance_count and first_instance. By default, if you call draw(num_vertices, 1, 0, 0), it draws one instance. If you set instance_count > 1, the vertex shader will run multiple times for each vertex – once per instance, with @builtin(instance_index) incrementing each time . We can supply per-instance data via:
	•	A second vertex buffer set with step_mode: Instance in the pipeline’s vertex buffer layout .
	•	Or via a storage buffer or uniform array indexed by instance_index, but the vertex buffer method is more straightforward and efficient for large numbers of instances.

For example, suppose we want to draw 100 buoy symbols, each represented by a small triangle mesh (or a quad composed of two triangles). We can do:
	•	Create a vertex buffer for the shape of one buoy (in model space, e.g. a unit-size triangle or quad).
	•	Create an instance buffer containing 100 entries of a struct like:

struct BuoyInstance {
    pos: [f32; 2],
    rotation: f32,
    symbol_index: u32,
};

This could store where to place the buoy, maybe its orientation (if needed), and which symbol image index to use (for example, if using a texture atlas of multiple buoy icons).

	•	In the WGSL vertex shader, have inputs:

@location(0) position: vec2<f32>,   // from vertex buffer (buoy shape vertices)
@location(1) instance_pos: vec2<f32>, // from instance buffer
@location(2) instance_rot: f32,      // etc.
@location(3) instance_symbol: u32,

We mark in Rust pipeline that buffer 0 has attributes at locations 0 (and maybe 1 if shape has color or UV per vertex), with step_mode=Vertex, and buffer 1 has attributes at locations 1,2,3 with step_mode=Instance. The pipeline description might look like:

let vertex_buffers = &[
    // buffer 0: shape vertices
    VertexBufferLayout {
        array_stride: size_of::<Vertex>(), // e.g. 2D pos plus maybe UV
        step_mode: VertexStepMode::Vertex,
        attributes: &[
           VertexAttribute { offset: 0, format: VertexFormat::Float32x2, shader_location: 0 },
           // if shape vertices also have UVs: {offset: 8, format: Float32x2, shader_location: X} 
        ],
    },
    // buffer 1: instance data
    VertexBufferLayout {
        array_stride: size_of::<BuoyInstance>(),
        step_mode: VertexStepMode::Instance,
        attributes: &[
           VertexAttribute { offset: 0, format: VertexFormat::Float32x2, shader_location: 1 },
           VertexAttribute { offset: 8, format: VertexFormat::Float32,   shader_location: 2 },
           VertexAttribute { offset: 12, format: VertexFormat::Uint32,   shader_location: 3 },
        ],
    },
];
// include this in RenderPipelineDescriptor.vertex.buffer_layouts


	•	When drawing:

rpass.set_vertex_buffer(0, shape_vertex_buffer.slice(..));
rpass.set_vertex_buffer(1, instance_buffer.slice(..));
rpass.drawIndexed(shape_index_count, instance_count, 0, 0, 0);

This will draw instance_count instances with one call.

In the vertex shader, you use each instance’s data to position the shape:

// Pseudocode in WGSL:
@vertex
fn vs_main(@location(0) pos: vec2<f32>,
           @location(1) instance_pos: vec2<f32>,
           @location(2) instance_rot: f32,
           @location(3) sym_id: u32) -> VertexOutput {
    var out: VertexOutput;
    // rotate pos by instance_rot (if needed) and translate by instance_pos:
    let cos_r = cos(instance_rot);
    let sin_r = sin(instance_rot);
    let rotated = vec2<f32>(
        cos_r * pos.x - sin_r * pos.y,
        sin_r * pos.x + cos_r * pos.y
    );
    let world = rotated + instance_pos;
    out.clip_position = view_proj * vec4<f32>(world, 0.0, 1.0);
    out.tex_coords = getSymbolUV(sym_id, ...); // maybe compute or fetch UV for this symbol
    return out;
}

This way, all buoys are drawn with one pipeline binding, one bind group setting (assuming they use same atlas etc.), and one draw call. The GPU handles iterating over instances efficiently.

Why instancing matters: Without instancing, you might loop in Rust and call draw() 100 times for 100 buoys, which incurs CPU overhead for each and increases driver work. Instancing collapses that to 1 call, significantly reducing CPU cost and allowing the GPU to treat all 100 instances together (which can enable further optimizations). On Raspberry Pi’s limited CPU, this is important. Also, it reduces the amount of WebGPU API validation overhead (which can be non-trivial per call).

Batching draw calls: Even beyond instancing, you should batch draws by pipeline and bind group. Group objects that use the same shader and resources. For example:
	•	Draw all chart polygons (land, depth areas, etc.) that use the same pipeline in one sequence. You might still have to change a uniform (e.g. a color or texture) between different polygon types – if so, consider if you can batch those too by using a push constant or instance attribute to select a color. Alternatively, sort by color and draw each group separately. The goal is to minimize pipeline swaps and bind changes.
	•	Draw all lines with one pipeline. Perhaps use an instance buffer to store per-line segment data if many share the same style. If lines have different widths or patterns, you might encode those in the instance data and have the shader adjust (like an attribute for line width). Or use a small uniform array of styles indexed by instance_id to avoid separate pipelines for each line style.
	•	Draw all symbols with one pipeline (as in the buoy example). If some symbols use different icon images, put them in the atlas and simply vary the UV or atlas layer per instance, rather than using separate pipelines or separate draw calls per icon.

The porting plan explicitly mentions “Batch tile draws to minimize pipeline state switches” . This suggests instead of drawing tile-by-tile (where you’d switch pipeline multiple times for each tile: draw tile polygons, then tile lines, then tile symbols, then next tile, etc.), you should draw by layer across tiles: e.g. set pipeline for polygons, draw all tiles’ polygons; switch to lines pipeline, draw all lines in all tiles; etc. This significantly reduces how often you re-bind pipelines and potentially how often you have to bind new uniforms (since e.g. the camera uniform stays the same, only maybe the tile-specific transform changes per tile which you could supply via instance data or a dynamic uniform). This reordering is possible because depth buffering and careful layering ensure the final image is the same regardless of per-tile grouping. The only consideration is if you rely on draw order for blending or overlap. But generally, all land polygons can be drawn together (they don’t overlap tiles anyway except at edges seamlessly), so that’s fine. Same for lines and points.

Implementing this in practice: Instead of having the main loop be “for each tile: set tile transform, draw polys, draw lines, draw symbols”, do:
	•	For polygons: aggregate all polygon vertices from all loaded tiles into one big buffer (or multiple buffers but you can draw them sequentially with one pipeline). Or use one buffer per tile but still you can do: for each tile’s buffer: set buffer, draw (but pipeline remains set throughout).
	•	You might use an index buffer and a single large vertex buffer that concatenates multiple tiles’ geometry, then call draw_indexed with ranges corresponding to each tile. This could even be one draw call if you also use an instance attribute to shift coordinates per tile – but merging many tile’s geometry into one draw might be complex, and if geometry count is huge, you may hit 16-bit index limits or need 32-bit indices (which is fine, WGPU supports 32-bit indices).
	•	A simpler compromise: sort draw calls by pipeline, but still do one call per tile per layer. That’s already better than switching pipelines per tile. If there are N tiles and 3 pipelines, originally naive approach: 3 * N state changes. Batching by pipeline: only 3 state changes total (one for each pipeline), and N draws for N tiles under each, which is an improvement. If you can also combine those N draws into fewer (via instancing or merging geometry), even better.

Instancing for tile geometry? If each tile’s polygons share identical structure (unlikely, each tile has unique shapes), instancing doesn’t apply directly. Instancing is great for repeated objects like symbols or patterned markers. It’s not as useful for arbitrary polygon shapes unless those shapes repeat (which in charts they typically don’t – each coastline is unique). So, focus instancing on repeated glyphs or markers:
	•	Ship/AIS targets, navigation aids: definitely instance those.
	•	Text labels: if eventually rendering text, a text rendering approach could instance each character quad or use a glyph atlas.
	•	Patterns: If a fill pattern is implemented by stamping a small sprite repeatedly, you could instance that sprite draw across the area. But usually, it’s easier to just do pattern in the fragment shader. However, for something like buoy “sweep” sectors (colored arc sections repeating), maybe not a common case.

Draw Indexed vs Draw: If you have indexed geometry, use draw_indexed to avoid duplicating vertices. The instance count parameter is available on both. Keep in mind WebGPU (and thus WGPU) at one point didn’t allow base_vertex or base_instance in draw calls due to compatibility constraints, but by now draw_indexed(base_vertex, first_instance) is allowed in wgpu’s API (the parameters exist). That means you can use a single index buffer for multiple tile meshes by offsetting the vertex base. Each tile’s indices would be relative to its vertex offset, and you call draw_indexed(index_count, 1, first_index, vertex_offset, 0). If base_vertex is not supported on all backends, an alternative is to duplicate vertex data or use separate draw calls.

Given performance sensitivity, it’s worth noting that on Raspberry Pi’s GPU, issuing <1000 draw calls per frame might already saturate CPU. Batching could reduce that to a few dozen or less. Instancing moves even more load to GPU (which is good since GPU can handle lots of parallel instances).

One more advanced batching idea: Render Bundles. WGPU has a concept of RenderBundle which is a pre-recorded sequence of draw calls that can be replayed quickly each frame (it’s like secondary command buffers in Vulkan). For example, if the set of draws is static (like the background chart doesn’t change), you could record them into a bundle once and just execute the bundle each frame. This saves CPU work. However, since charts can pan/zoom, the vertex data might change or at least the transformation uniform changes, which might limit bundle use (bundles can inherit bind groups and pipeline state from the parent pass IIRC, or you can set them inside, but changes to uniforms each frame means re-recording anyway). Bundles are more useful if you have a static scene that’s drawn repeatedly. NavCore’s dynamic nature (panning, new tiles loading) means we likely won’t use them heavily. So focus on instancing and sorting as above.

To summarize instancing/batching: Use instancing for all repeated visual elements, especially symbols and markers, to draw many in one call . Sort and batch draw calls by pipeline and resource state to avoid redundant state changes. The result is a dramatically lower CPU overhead: the GPU might be drawing thousands of objects but the CPU only issued a handful of commands. This is vital for embedded targets where single-core CPU speed is limited. With proper instancing, even a Raspberry Pi can handle large numbers of overlay points or repeated patterns smoothly.

7. Platform-Specific Optimizations (RPi4/5 and Mac ARM)

Different platforms have different GPU architectures and constraints. NavCore targets the Raspberry Pi 4/5 (Broadcom VideoCore GPUs) and Mac ARM (Apple’s M1/M2 GPUs), which are tile-based deferred rendering (TBDR) GPUs – they operate by dividing the framebuffer into small tiles and processing each tile in on-chip memory for efficiency  . This yields great performance per watt but requires mindful coding to avoid breaking the tiling optimizations. Here we cover optimizations and considerations specific to these platforms, including memory limits and thermal constraints.

Early-Z and minimal overdraw: We’ve touched on this, but to reiterate: enabling depth testing and drawing front-to-back can significantly reduce fragment shading work on any GPU, but especially on mobile/TBDR GPUs like VideoCore and Apple’s. The Pi’s GPU can do an Early-Z pass within each tile to reject covered fragments before running the fragment shader . However, certain shader features like discard or alpha-to-coverage will disable early depth test , meaning every fragment runs the shader even if it’s behind something. So:
	•	Avoid discard in fragment shaders (and avoid alpha test-like tricks). Instead use blending or precomputed transparency masks if possible. If you must mask (e.g. pattern stencils), consider using an 8-bit alpha texture and blend, rather than throwing away fragments.
	•	Draw opaque things first, with depth write on. Then draw transparent things. Even though transparent ones can’t early-Z cull (they have to blend regardless), at least your opaque ones already wrote depth to cut off other opaque behind them.
	•	Limit overdraw: don’t draw multiple layers of full-screen semi-transparent polygons if not needed. E.g., instead of drawing a full-screen rectangle with 50% opacity for night mode and then drawing everything under it (causing every pixel to do two blends), consider adjusting the color palette of the map itself for night mode to avoid an extra blend pass. Overdraw kills performance on mobile GPUs because it multiplies fragment work.

Memory and resource limits on Raspberry Pi: Raspberry Pi shares its main memory with the GPU. Pi 4/5 have up to 8GB RAM, but not all of that is available for GPU – the OS dynamically allocates (on Pi4/5 with newer drivers, there isn’t a fixed split, it uses CMA). Still, assume the GPU memory is precious. A few guidelines:
	•	Use 16-bit or 8-bit formats where possible for textures. For instance, if a texture is just a mask or grayscale, use R8Unorm instead of RGBA8. If you don’t need high color precision, maybe use Bgra8Unorm (8 bits per channel) or even a 16-bit format for something (like normals can often use RGBA4 or so, but probably not relevant for charts).
	•	Avoid extremely large textures. If you have an atlas, consider limiting its size (e.g. 2048x2048) and using multiple atlas textures if needed rather than one huge 8192x8192, which might not fit in Pi GPU’s tile memory comfortably or might cause allocation issues. The Pi4’s VideoCore VI supports up to 4096 or 8192 textures in GL, but performance might degrade at extremes.
	•	Pi GPU likely has a tile cache of limited size (e.g. 16x16 tile at 4x MSAA and 4 bytes per pixel per attachment – you can estimate). Don’t use too many render targets at once – e.g. limit to color + depth. Using multiple color attachments (MRTs) means the tile memory usage multiplies, which could force the GPU to do more passes per tile (losing performance).
	•	Pi’s Vulkan driver is relatively new. If WGPU’s Vulkan backend shows issues on Pi, consider falling back to the GLES (OpenGL ES) backend for Pi (enabled via WGPU_BACKEND=gl). The GL path might be more mature on that platform, albeit potentially slightly less efficient. Monitor GPU driver errors; if Vulkan is stable (which by 2025 it is mostly, as Pi4 got Vulkan 1.2 support ), use it for better debug tooling like RenderDoc.

Thermal and clock throttling: The Pi (especially Pi4) can run hot when the GPU and CPU are heavily used simultaneously (it has no active cooling unless you add it). Running the GPU at 100% for extended periods (like animating the map or heavy overlays) will cause the Pi to throttle (slow down clock speeds) to stay within thermal limits. This can cause sudden performance drops. Strategies to mitigate:
	•	Frame rate limiting: If you don’t need ultra-high FPS (for a map, 30 or 60 FPS is fine), consider enabling vsync (PresentMode::Fifo will lock to monitor refresh, usually 60Hz). This prevents the app from running unconstrained (which could be ~100+ FPS on simple scenes, wasting power).
	•	If vsync is 60 and the Pi still overheats, you might intentionally cap at 30 FPS by skipping every other frame’s draw (just not issuing draw if not needed). Perhaps provide a setting for “power saving mode” to reduce refresh rate when not actively panning.
	•	Monitor temperature if possible (as noted, logging /sys/class/thermal/thermal_zone0/temp was in plan ). If it exceeds a threshold, you could reduce workload (lower resolution or detail).
	•	Simplify shaders for Pi: As noted in plan, avoid complex fragment shaders on Mali/VideoCore (no discard, and try to avoid heavy math). The Pi’s GPU is moderately powerful but nowhere near a desktop GPU. For example, if implementing lighting or procedural patterns, consider precomputing as much as possible. Use lookup textures for any complicated function (e.g. if you needed a expensive math function, better to use a small LUT texture and sample it).
	•	Parallelize CPU work: The Pi’s CPU is quad-core (Pi4) or more (Pi5 might be quad or six). The render thread (WGPU) might run on one core; if you have other cores idle, offload non-GPU work there (parsing data, AI computations, etc.). Keep the core handling WGPU from doing heavy extra work that could slow feed to GPU.

Apple Silicon (Mac ARM) specifics: Apple’s GPUs (in M1/M2) are also tile-based deferred renderers with unified memory. They generally have much more performance headroom than Pi’s, but some similar rules apply:
	•	Avoid unnecessary memory moves: Unified memory means the CPU and GPU share the same memory, so mapping a buffer and writing is actually quite efficient on Mac (no separate VRAM copy). This is why MAPPABLE_PRIMARY_BUFFERS is supported on Metal. We definitely want to use that feature on Mac if allowed , because it means we can update buffers by mapping them (which ends up just writing to shared memory) instead of doing a copy through a staging buffer.
	•	Apple’s driver and Metal are very optimized; still, discard or heavy blending can cause tile memory overflow. If you draw too many layers with blending or do something like big draw calls with lots of small triangles that overdraw, the tile memory might spill (the GPU then has to write intermediate results to RAM and read back – a performance hit). Early-Z helps avoid that by not shading those covered fragments at all.
	•	Apple GPUs do not support some things: e.g., they don’t support 32-bit atomics in shaders or geometry/tessellation shaders (WebGPU doesn’t have those anyway). They also have a limit of 4 push constants (32 bytes) if enabled. Keep within typical limits (which WGPU’s defaults ensure).
	•	A MacBook has better cooling than Pi but still can throttle under sustained GPU load (especially the fanless MacBook Air). So similar advice: don’t run unnecessarily high FPS or complexity when not needed, to avoid battery drain and heat.
	•	Check Metal GPU family features: Apple GPU have tiers of feature sets. For instance, MSAA is supported and fast (the tiler can do MSAA with little overhead, then resolve per tile). So using 4x MSAA on Mac is fine.

Driver quirks:
	•	On Raspberry Pi, the Vulkan driver might have bugs. If you find a rendering artifact or crash specific to Pi (and not on Mac/PC), it could be a driver issue. Sometimes changing usage flags or simplifying a shader works around it. For example, certain combinations of depth format or image layout might not work well; WGPU tries to cover these, but being on the newest stuff means encountering edge cases. Keep WGPU up-to-date as Pi’s drivers improve.
	•	On Mac, one quirk historically: if you create very large buffers (hundreds of MB), you might hit OS memory limits or performance issues. Try to keep individual buffers reasonably sized (like < a few dozen MB). For charts this is easy, geometry is not that heavy (tens of thousands of vertices perhaps).
	•	Another Mac quirk: the default Metal viewport coordinates differ (Metal uses a coordinate system with origin top-left for pixel coordinates). WGPU handles this by internally flipping the Y or adjusting the transform in surface config. Just be careful if you do anything involving frag_coord as mentioned.

CMA on Pi (Contiguous Memory Allocator): The plan mentioned “CMA hinting” . There’s no direct WGPU API for CMA, but the idea is to allocate large buffers so that the Linux kernel gives a contiguous physical memory block (beneficial for DMA to GPU). The Pi’s GPU might prefer contiguous physical memory for optimal access. If you allocate many small buffers scattered in memory, it could be less efficient. The advice to use a chunk allocator for vertex/index buffers likely is to allocate one big chunk (say 16MB) and suballocate, increasing the chance that chunk is one contiguous piece. This dovetails with our earlier discussion on chunking tile data.

Precision and Quality settings:
	•	On Pi, you might lower some quality settings if needed: e.g., if MSAA 4x is too slow, drop to 2x or none on Pi while keeping 4x on Mac.
	•	Another might be anisotropic filtering for textures (if you used any for oblique angles - not typical in top-down maps, but maybe for 3D view mode if any). Anisotropic filtering is an optional feature in WGPU; it’s not needed for a straight top-down view.
	•	If enabling optional features (like texture_compression, etc.), ensure Pi supports them. Probably not needed in NavCore’s use case.

Multithreading WGPU: WGPU’s Device and Queue are thread-safe (Send/Sync). You could record commands on multiple threads by creating multiple encoders and then joining command buffers, or use multiple queues if you had (WebGPU only has one queue currently). On Pi’s 4 cores, one approach is to move some heavy CPU work off the main thread. But be cautious with threading WGPU calls themselves – for simplicity, you might keep all WGPU calls on one thread (the main render thread) and just prepare data on others.

Testing on hardware: Each platform might reveal different bottlenecks:
	•	On Pi, often pixel shading and memory bandwidth are the bottleneck (the GPU has decent geometry throughput but limited fillrate). So watch out for full-screen effects or drawing too many pixels multiple times.
	•	On Mac, you have more fillrate, but could be bound by something else like vertex processing if you push extremely high vertex counts (less likely here), or by your CPU preparation code if not optimized.

Software fallback: In case WGPU couldn’t get a native adapter (like no Vulkan/Metal), it might fall back to a software rasterizer (like on a headless server, WGPU might use a “gl” backend that could be LLVMpipe). Obviously, that would be slow; you generally want a real GPU. Pi and Mac both have GPUs, so that’s fine. But if one day NavCore runs on other SBCs or without GPU, you might detect adapter type (WGPU gives AdapterInfo.device_type, e.g., CPU vs Integrated vs Discrete )  and warn or reduce features accordingly.

In summary, for Raspberry Pi: lean on GPU-friendly techniques (depth test, instancing) to maximize throughput, minimize overdraw and avoid heavy fragment operations to keep it cool and smooth  . Manage memory by using moderate resource sizes and freeing unused tiles. For Mac ARM: take advantage of unified memory (fast updates) and greater power, but still follow good practices (avoid needless passes, reduce blending layers) to keep performance high and battery usage reasonable. In both cases, testing and profiling on the actual devices (see next section on profiling) will guide you to specific tweaks (e.g., “radar overlay at 50% alpha caused Pi to drop frames, so we adjusted to lower resolution or less frequent update”).

8. Porting Notes from QuteNav (S-52 Styling, Mercator Transforms, GPU Resource Management)

NavCore is a reimplementation of QuteNav’s functionality with Rust and WGPU. Here we highlight how certain aspects of QuteNav (which used Qt/OpenGL) map to WGPU, focusing on chart styling (S-52 standard), coordinate transforms, and GPU resource management differences.

S-52 styling system: QuteNav implemented IHO S-52, which defines how electronic chart features are portrayed (colors, symbols, line styles, area patterns, priority of layers, etc.). In QuteNav (OpenGL/Qt), much of the styling logic was likely on the CPU: it determined for each feature which symbol to use, what color, whether to draw it or omit based on conditions (CS rules). That logic will remain on CPU in NavCore as well. The difference is in how the style is applied when rendering:
	•	Colors: QuteNav might have loaded colors from a config (like “LAND areas = RGB value”). In OpenGL, they may have simply set a glColor or used a uniform per draw call. In WGPU, you’ll likely use a uniform or push constant for color if it changes frequently, or bake it into vertex data if each vertex has a fixed color. For large area polygons, it’s probably easier to use a uniform per draw (or per instance if you batch polygons of different colors).
	•	For example, you could have each polygon feature carry a “feature_code” and then in the shader have a lookup table of colors (small array of vec3). But WebGPU doesn’t allow big arrays of uniforms without either giant buffers or storage. Instead, simpler: assign the actual color to each vertex (redundant but straightforward) or group by color and use uniform. Because charts have relatively limited palette (S-52 has a known set of colors), a nice trick: you could use a small palette texture (1D texture of all S-52 colors). Then each vertex only needs an index (like an 8-bit index into that palette). The fragment shader samples the palette texture with that index to get the actual RGB. This is akin to old indexed color, but it saves passing full 24-bit color per vertex. However, unless memory is extremely tight, passing color per vertex is fine too. It’s a design choice. If you do go palette route, ensure filtering is nearest and do index+0.5 offset properly.
	•	Line styles: S-52 uses various line dash patterns (e.g., boundary lines might be dashed). In QuteNav, they may have implemented this either by generating the dash pattern into the geometry (tessellating the line into segments and gaps) or by using a texture with the pattern and texturing along the line. With WGPU, both approaches are possible:
	•	Geometry method: The CPU determines dash segments and creates vertices only for the visible segments. Simpler shader (just a solid line shader), but more CPU work. This is okay if CPU can handle it.
	•	Shader method: Use a 1D texture or a procedural calculation in the fragment shader to apply a stipple. For instance, pass a texture that is 8x1 pixels representing the dash pattern (e.g., [11110000] bits for dashed), and have the fragment shader sample it based on the line distance (which requires knowing how far along the line the fragment is – which you can get by a varying if you pass, say, a param that increments along the line length). QuteNav being OpenGL ES might have used glLineStipple if it was available (but I think ES doesn’t have glLineStipple, they might have implemented manually). In any case, either reimplement similarly or consider the load on Pi’s GPU if fragment does a mod or texture for every pixel of line. It might be fine if resolution is moderate.
	•	Possibly, S-52 line patterns are few and short, so a small texture lookup is fine.
	•	Area patterns: Many S-52 areas have fill patterns (like marsh areas have a pattern of diagonal lines, etc.). QuteNav likely used OpenGL textures for these fills (since they mentioned requiring inkscape to generate some SVG to PNG). The typical approach: each area feature has an associated texture (like a small tileable image) and they texture-map the polygon with it. In OpenGL they might have enabled repeating and set texture coords scaled to world coords. In WGPU, you can do exactly that: have a set of pattern textures in an atlas or array, and for a given polygon, supply the pattern index and maybe a transformation for texture coordinates. The fragment shader can sample the correct pattern. Because WebGPU doesn’t allow truly “bindless” textures without extensions, you might use a texture array for patterns: combine all pattern images into a single 2D array texture (all same size, or pad them to same size). Then pass an instance_pattern_index to the shader (or uniform) to pick layer in the array. This way you bind just one texture array for all patterns . Alternatively, store patterns in one big atlas sheet and pass UV scaling for each polygon to pick its region – but then wrapping (repeat) becomes tricky if they’re not power-of-two aligned. Texture array is cleaner for distinct small patterns.
	•	Another approach: If patterns are simple (like stripes), you could procedurally generate them in shader (just use a math function). But S-52 includes some complex symbols in patterns, so better to use actual bitmaps.
	•	Symbols (points): QuteNav loaded an XML of symbols and presumably had a texture atlas of PNGs for them. In NavCore, we do similarly (in fact the plan notes a symbol atlas already done in runtime optimizations ). The major difference: QuteNav possibly used Qt’s QPainter or QML for some symbol drawing, or OpenGL with glDrawPixels or texture quads. We will use textured quads in WGPU. This we have covered: use instancing for symbols, all pulling from one atlas texture (likely we’ll use one array texture or one big atlas).
	•	Pay attention to symbol rotation: some symbols need to be rotated according to ship heading or feature orientation (e.g., buoy topmarks might rotate with buoy orientation). QuteNav had “Symbol rotation logic based on orientation” as a TODO . With instancing, we can include rotation per instance and apply in shader as shown.
	•	If any symbols need scale or flashing (like lighthouses have a phase but that’s dynamic overlay possibly via blinking – might implement by toggling their visibility via CPU).
	•	Priority and draw order: S-52 defines display priorities (e.g., important features drawn on top of less important). QuteNav likely sorted drawing by those priorities. In NavCore with depth, we can encode priority in depth values if we want, or simply still sort by priority and draw in that order. Depth could automate it, but careful: if we give higher priority items a nearer depth, we can let depth test handle it. However, if items at same depth overlap, might still need correct order for blending. It may be simpler to just order draws by priority on CPU, using separate pipelines or just separate draw call sequences. Since priorities are coarse (like categories), you can even assign a depth per category to enforce an overall order, then still sort within those categories if needed for proper transparency.
	•	E.g., Land areas priority 1 (depth 0.9), depth areas priority 2 (0.85), lines priority 3 (0.8), symbols priority 4 (0.1). Then you mostly ensure symbols appear above all else via depth. That covers a lot, but some lower priority symbol vs higher priority symbol ordering – might also consider that but usually all symbols are same-ish layer except maybe some have “under radar” vs “above radar” categories.
	•	The plan suggests early-Z mostly for areas behind symbols, so likely they still handle order mostly on CPU and just use depth to avoid overdraw.

Mercator projection and coordinate handling: QuteNav presumably used glm to calculate projection matrices , and had helper functions to convert lat/lon to screen pixels and vice versa (toChartPix, fromChartPix) . In NavCore, we similarly:
	•	Use something like cgmath or glam to build an orthographic projection that transforms world Mercator coordinates to normalized device coords (-1..1). The plan indicates Mercator projection matrix done . One must ensure this matrix exactly matches QuteNav’s behavior to avoid any offset or scale differences. This is critical for things like picking (the user clicks somewhere, and the lat/lon under the cursor should match between old and new impl).
	•	Mercator specifics: likely using spherical Mercator (EPSG:3857) where x = lon * k, y = ln(tan(pi/4+lat/2))*k. QuteNav reading of S57 likely provides coordinates in degrees or already projected? Actually S57 is in lat/lon, QuteNav probably projected them on the fly or during SENC creation. NavCore likely does similarly – parse lat/lon, then transform to Mercator XY (in meters or normalized).
	•	QuteNav might have used double precision on CPU for projection for accuracy. Our GPU will use 32-bit float which over large ranges (global) might introduce tiny errors (Mercator values for lat 85 are huge). Typically, charts are in a bounded region, so it’s fine.
	•	Floating origin: If coordinates are very large (e.g., Mercator X at 180° is ~20 million meters), the single-precision float could lose precision if used directly in the shader. If the chart covers a small region, one can subtract an origin (e.g., center of the chart) to bring coordinates near zero before multiplying by the matrix. That could be done on CPU to all vertices. QuteNav likely did something similar or maybe used double on CPU and cast to float for GPU relative to view. The plan’s mention of matching toChartPix suggests they ensure pixel-perfect alignment with QuteNav, which implies careful handling of floating point math.
	•	We should confirm that WGPU’s coordinate outputs align: likely if using the same mercator math and same screen resolution logic, it will align.

GPU resource management: Differences between QuteNav (OpenGL/Qt) and NavCore (WGPU):
	•	Context and Lifecycle: In Qt OpenGL, you had to worry about context current, maybe context loss on device switch, etc. WGPU simplifies this; context loss can happen but it’s rarer (mostly if GPU resets or something). If device is lost, WGPU surfaces it as a DeviceLost error, and you’d need to recreate the device and resources. Good news: WGPU can reconnect to a new adapter if needed, but implementing robust device lost handling is complex and often not needed for desktop/mobile (it’s more relevant for web where context can be lost).
	•	Shader compilation: QuteNav used GLSL shaders (likely compiled at runtime by GL driver). We use WGSL with WGPU’s offline or runtime compilation via naga. The pipeline creation might take a bit of time (like GLSL did). One could do that during load screens.
	•	State changes: OpenGL allowed changing state (enabling blending, setting uniforms) between draws freely. WGPU demands pipelines encapsulate most state and bind groups encapsulate resource state. This might require reorganizing QuteNav’s rendering loop logic. For example, QuteNav might have done: enable blend, draw lights, disable blend, etc. In WGPU, that’s two pipelines (one with blending, one without). Or QuteNav might have toggled a polygon offset or line smoothing state; in WGPU, that would be pipeline config (depth bias in DepthStencilState for polygon offset, for example).
	•	Resource cleanup: In OpenGL, forgetting to delete VBOs or textures could leak GPU memory. In WGPU/Rust, when the Buffer or Texture goes out of scope and no commands refer to it, it will be freed (internally reference counted) . However, WGPU doesn’t guarantee immediate deallocation on drop; it schedules it after all GPU work using it is done. The porting plan mentions implementing tile eviction (LRU)  – in WGPU you can drop the buffers of off-screen tiles to free memory. There’s no explicit “delete” call needed, just ensure no lingering strong references. If memory is still not reclaimed fast enough (since GPU might not have executed deletion commands yet), you can force a cleanup by device.poll(Maintain::Wait) which blocks until all queued GPU work finishes and then freed resources can actually be returned. Use that sparingly (maybe when memory is low and you absolutely need to reclaim).
	•	Bindless vs bindful: QuteNav using OpenGL 4.5 could have possibly used bindless textures (some GL extensions allowed it) to avoid binding textures for each draw. WebGPU doesn’t fully allow that (except via arrays or storage buffers with indices), so we adopt the array-of-textures or atlas strategies described. It’s a bit more constrained but also more predictable.
	•	CPU-GPU synchronization: In OpenGL, if you weren’t careful calling glMapBuffer at the wrong time, you’d stall the GPU. WGPU tries to prevent you from doing things that stall without you knowing – e.g., queue.write_buffer defers until safe. So the code might actually simplify regarding fences and sync that QuteNav had to worry about. But you should still avoid scenarios like writing to the same buffer that’s still in use by GPU (which WGPU enforces by double buffering or by waiting internally). The plan to defer uploads until render pass init  is naturally achieved by WGPU’s approach: you prepare all data, then encode pass – no immediate mode updates.

Porting summary: Most of the higher-level logic from QuteNav can remain (feature processing, styling decisions), but the implementation shifts:
	•	Shaders are now WGSL and likely split by primitive type (point/line/area), whereas QuteNav might have had a single multi-purpose shader with branching (or multiple, depending). NavCore currently uses separate pipelines for point/line/triangle , which is wise.
	•	Many small GL draw calls become fewer batched WGPU draw calls (this is a change in mindset but straightforward given instancing).
	•	The S-52 rules still run on CPU (C++ vs Rust now). The output of those rules (which symbol, color, etc.) feed into GPU via buffers/uniforms.
	•	WGPU will give you validation warnings if something’s off (like using a buffer with wrong usage or not updating a bind group). QuteNav’s GL might have silently done something or glGetError had to be checked manually. In NavCore, leverage those warnings (enable WGPU_LOG=warn or similar) to catch mistakes early .

A specific note: The plan item “Simplify fragment shaders for Mali GPUs (no discard)”  – we explained that in section 7. QuteNav’s shaders might have liberally used discard (for pattern masks or for drawing only certain fragments of symbols). In the WGPU version, we try to eliminate that: e.g., for patterned area fill, instead of discarding pixels outside pattern, maybe draw the whole quad and use an alpha mask so that unseen parts are just transparent (blending turned off for opaque shapes, but you can still have an alpha channel just not doing blending if opaque? Actually, if you just want holes, can output alpha=0 and use blending, but then you need blending on which can kill early-Z anyway and cost fillrate. Hmm). Alternatively, break geometry for patterns (generate actual triangles for visible parts). That is more preprocessing but might be better for tile GPUs. This is an ongoing design decision – might need profiling.

Finally, when porting, test incrementally: Start by drawing basic stuff (maybe ignore patterns, use flat colors) to ensure Mercator transform and layering works. Then add complexity (textures for symbols, patterns, etc.) verifying each step matches QuteNav’s output (the plan mentions screenshot diffs and golden images ). Having reference images from QuteNav is extremely useful to validate the new pipeline. WGPU should allow achieving pixel-perfect results with the right care taken in math and blending.

9. Supporting Ecosystem Tools (naga, shaderc, winit, pollster, etc.)

Developing with WGPU involves a few auxiliary libraries and tools that NavCore will utilize to ease development:
	•	Winit (Windowing and Events): NavCore uses winit for creating windows and handling input in a cross-platform way. Winit provides the window handle required to create a WGPU surface  and an event loop to drive rendering. This is already integrated (the plan mentions DPI scaling with winit for Mercator matrix ). With winit, ensure you handle resize events (reconfigure the surface on resize) and possibly scale factor changes (for HiDPI, adjusting your projection or canvas size). Winit also abstracts X11/Wayland on Pi’s Linux and Cocoa on Mac, so you don’t have to worry about those details.
	•	Pollster (block_on for async): WGPU’s device/adapter requests are async (futures). pollster is a tiny crate that provides block_on() to execute an async function to completion easily in a synchronous context . NavCore likely uses it at startup to get the adapter/device:

let (device, queue) = pollster::block_on(adapter.request_device(&device_desc, None)).unwrap();

Pollster is basically just a quick executor for one future. It’s fine for one-time setup or occasional tasks. If you had a more complex async workflow (like streaming in data, asynchronous shader compilation), you might integrate a full async runtime (tokio, etc.), but for our rendering loop, which is largely synchronous each frame, pollster for init is enough.

	•	Naga (shader translation): Naga is the library that WGPU uses internally to parse WGSL and generate SPIR-V/MetalSL/etc. You typically don’t interact with naga directly in an application, but it’s good to be aware of it:
	•	If a shader fails to compile, the errors might reference “naga” or use naga’s phrasing. They might point to a line and say things like “Unknown attribute” or “Type mismatch” etc. The error messages are improving, but can sometimes be opaque.
	•	There is a wgsl-analyzer for VSCode that uses naga to give you live feedback on WGSL.
	•	If absolutely needed, you can use naga as a library to pre-validate or convert shaders offline, but in most cases just let WGPU handle it at runtime.
	•	Naga also means WGPU can accept SPIR-V or GLSL if you enable features. WGPU has features: Features::SPIRV_SHADER_PASSTHROUGH or similar, and the Rust crate feature glsl that includes shaderc support. If you wanted to reuse QuteNav’s GLSL directly, you could enable WGPU’s “glsl” feature, include shaderc, and compile GLSL to SPIR-V at runtime, then create ShaderModule from that SPIR-V bytes. However, long-term it’s better to port to WGSL because WGSL is the canonical WebGPU language . But shaderc could be a bridging tool if needed to get something running quick. (Also note: using SPIR-V might disable some validation or compatibility – e.g., on the web, SPIR-V input is not allowed. On Pi/Mac native it’s fine.)
	•	Shaderc (GLSL compiler): If needed, add shaderc crate. It binds to Google’s shaderc library which compiles GLSL/HLSL to SPIR-V. Example usage:

let mut compiler = shaderc::Compiler::new().unwrap();
let spirv_binary = compiler.compile_into_spirv(glsl_source, shaderc::ShaderKind::Fragment, "frag.glsl", "main", None).unwrap();
let module = device.create_shader_module(&ShaderModuleDescriptor {
    label: Some("GLSL Frag Shader"),
    source: ShaderSource::SpirV(Cow::Borrowed(spirv_binary.as_binary())),
});

This requires enabling WGPU’s spirv feature (and maybe glsl). It’s useful if you have a lot of existing GLSL. But consider that certain GLSL features might not map cleanly to WebGPU (like push constants or separate image/sampler in GLSL must be tweaked to WebGPU’s combined model). The better route as followed is rewriting to WGSL to suit NavCore’s new architecture, which is already in progress per pipeline separation.

	•	wgpu-profiler (GPU profiling crate): This is a community crate for profiling GPU time of code sections. It wraps WGPU’s query set usage. If you enable it, you can mark scopes in code and later get timing results for them (and even feed to profiling UI or Tracy). The plan mentions using it or similar for performance analysis . To use:
	•	Include wgpu-profiler crate.
	•	Create a GpuProfiler with GpuProfiler::new(&device, settings). Settings define how many timer queries etc.
	•	Before drawing, each frame, you usually do profiler.begin_frame().
	•	Within a render pass or encoder, you wrap code in profiler.scope("name", &mut encoder) which returns a wrapper that when dropped will insert the GPU timestamp queries .
	•	After submission, call profiler.end_frame() and then later collect results with profiler.process_finished_frame(...) which gives timings.
	•	This data can be output to Chrome trace format  or to a live viewer like Tracy or puffin if enabled .
	•	It’s extremely useful to see where GPU time is going (e.g., “fragment pass took 4ms on Pi but 1ms on Mac” etc).
	•	Note that using queries might require enabling the optional Features::TIMESTAMP_QUERY on device request. Most GPUs support it (Metal does, Vulkan does).
	•	The crate handles the necessary alignment and ensuring not to exceed query limits.
	•	RenderDoc (frame capture): RenderDoc is an external tool, not a crate, but it’s worth mention because it’s one of the best ways to debug graphics. On Windows or Linux, you can launch NavCore via RenderDoc and capture a frame, inspecting all the draw calls, bound resources, shader inputs, etc. RenderDoc has preliminary support for Vulkan on Raspberry Pi (assuming Pi’s Vulkan driver is standard). On Mac, RenderDoc doesn’t support Metal directly, but if you run WGPU with Vulkan via MoltenVK (enabling vulkan-portability feature), you might capture. Alternatively, Xcode’s GPU Frame Debugger is the Apple way (but then you need to integrate with Xcode).
	•	In any case, being able to pause and inspect a drawn frame is immensely helpful for verifying that layers draw in correct order, that uniforms have expected values, etc. With WGPU, you can also enable a capture by setting environment variable WGPU_TRACE_PATH to some folder – then WGPU will log all API calls to a trace file . That can be replayed with a tool (the wgpu-tools might have a replay utility).
	•	However, a trace is more for reproducibility/bug-reporting, whereas RenderDoc is interactive.
	•	Logging (env_logger / tracing): As seen in the beginner tutorial, they initialize env_logger so that WGPU internal logs will be printed . WGPU uses the log crate internally. So if you set RUST_LOG=warn,wgpu_core=info,wgpu_hal=info for example, you’ll get detailed logs from the core (like when creating devices or if a validation error occurs) . For development, set RUST_LOG=wgpu_core=warn to catch warnings (like “command buffer memory leak” or “unused bind group” etc.). In release, you might turn that off for performance. NavCore could also integrate tracing for its own logs, but not required.
	•	Continuous integration and testing tools: The plan mentions golden image tests and CI on RPi via GitHub actions . While not directly WGPU-specific, some tools might help:
	•	Headless WGPU: WGPU can be run without a window by using a Headless Surface or just creating a device with no surface and doing offscreen render to a texture. This can be used in CI (though requires the CI runner to have Vulkan/Metal capabilities – for RPi, one could use llvmpipe for a software rasterizer in CI, or use a dummy Vulkan on a runner with GPU like in a lab).
	•	There’s a project wgpu-test that uses the serializable trace approach to run automated tests on WGPU. But that might be internal to WGPU’s own CI.
	•	WGPU versions and docs: WGPU is moving fast (currently up to 0.27 as per docs we cited). Keep an eye on changelogs for improvements or breaking changes. The official docs.rs page and examples are valuable . Also, the WebGPU spec on W3C and the MDN docs provide general guidance (we used some MDN content earlier about architecture)  .

In summary, the Rust/WGPU ecosystem comes with many helpers:
	•	Use winit for cross-platform windowing and events.
	•	Use pollster (or an async runtime) to deal with WGPU’s async where needed.
	•	Let naga and WGPU handle shader translation, but know it’s there when debugging shaders.
	•	Optionally use shaderc if you have legacy shaders to bring over.
	•	Leverage wgpu-profiler to get insight into GPU performance in development, and tools like RenderDoc for graphical debugging.
	•	Use logging to catch issues (e.g., enable WGPU validation messages).
All these will aid in making NavCore’s development smoother and ensure parity with QuteNav’s output and performance.

10. Debugging and Profiling (Logging, RenderDoc, wgpu-profiler)

Building a graphics-intensive application like NavCore means you’ll need robust debugging and profiling to ensure correctness and optimal performance, especially on target devices. This section outlines strategies and tools for debugging rendering issues and measuring performance.

Logging and WGPU Validation: WGPU provides internal validation and error messages. It’s important to enable and monitor these:
	•	Initialize logging at start (e.g. via env_logger::init() or using the tracing crate with a LogSubscriber). Set the environment variable RUST_LOG to show WGPU logs . For development, RUST_LOG=wgpu_core=warn,wgpu_hal=warn is a good level – it will notify of any API misuse or performance caveats (like using a texture in an unsupported way, or if the GPU timings are slow in debug layers).
	•	WGPU by default enables validation in debug builds. You can explicitly set InstanceDescriptor.flags = InstanceFlags::VALIDATION when creating the WGPU instance to ensure validation layers are on. Validation will catch things like binding mismatches or using resources without proper usage flags, and produce warnings or errors in the log.
	•	Use Device::on_uncaptured_error to catch errors that aren’t returned (like an error during command submission). For example:

device.on_uncaptured_error(|ty, msg| {
    log::error!("WGPU error: {:?}: {}", ty, msg);
});

This ensures you see if anything goes wrong at runtime that WGPU didn’t panic on (like a recoverable error).

	•	If you encounter a device lost error (say, out-of-memory or TDR on Windows), WGPU will call the uncaptured error callback. In that case, you may need to recreate the device. But this should be rare if at all on Pi/Mac.

GPU Debugging with RenderDoc / PIX / Xcode:
	•	RenderDoc: A powerful frame capture tool (Windows/Linux). You can use it on Raspberry Pi (if using Vulkan backend) – Pi’s Vulkan driver is compatible with RenderDoc. On Mac, RenderDoc doesn’t support Metal directly, but as mentioned, one workaround is running WGPU in Vulkan mode via MoltenVK. Alternatively, use a Windows or Linux build on hardware that supports it for debugging, since the rendering code is cross-platform. RenderDoc allows you to pause on a frame and inspect every draw call, the pipeline state, bound textures, uniforms, and even see the mesh and texture content. This is extremely useful to verify that, for example, “the land polygon draw call used the expected vertex count and uniform values” or “the symbol atlas texture is correctly uploaded”. It can also do an overlay of depth values, etc., helping to debug depth testing issues.
	•	RenderDoc can also show the timing of each draw call (on supported GPUs) which helps find slow steps.
	•	To use RenderDoc, you typically just run your program through it (it intercepts Vulkan/GL calls automatically). There’s no code change needed, except you might want to name your objects in WGPU for easier identification (using label fields on pipeline, buffer, texture descriptors). WGPU passes these labels to the backend, and RenderDoc will show them. For example, set label: Some("Coastline Vertex Buffer") when creating – in RenderDoc you’ll see that name.
	•	PIX (DirectX) / NSight: Not directly applicable since we’re not on DirectX or using NVIDIA on Pi/Mac.
	•	Xcode GPU Frame Debugger: On macOS, if you run your app via Xcode, you can capture frames and inspect Metal draw calls. Since WGPU uses Metal under the hood on Mac, Xcode should show you the Metal commands. You’ll see your WGSL shaders translated to MSL, etc. The integration is a bit involved (you need an Xcode project or attach to process).
	•	GPU Capture via WGPU Trace: WGPU has an internal capture feature. If you set the env var WGPU_TRACE to a directory, WGPU will log all API calls and resource creations to files in that directory . You can later use wgpu-tools to replay that trace on another machine. This is mainly for reporting bugs to WGPU developers or doing offline analysis. It’s not as visual as RenderDoc but is a deterministic replay of the frame. For NavCore, this might be used to capture a frame on Pi and replay on a desktop to investigate an issue (because the trace is at the WGPU API level, which is portable). The plan hints at capturing a frame via wgpu trace for verification .

Performance profiling:
	•	wgpu-profiler crate: As discussed, integrate wgpu-profiler to measure GPU times for sections of your rendering. For example, you can wrap the calls that draw background polygons, the calls that draw lines, and the calls that draw symbols in separate profiling scopes. When running on RPi or Mac, you can get timing data for each scope:

{
    let _prof = profiler.begin_scope("Draw Polygons", &mut encoder);
    // set pipeline and draw all polygons
}
{
    let _prof = profiler.begin_scope("Draw Lines", &mut encoder);
    // draw lines
}
// etc...

The profiler will insert GPU timestamp queries around those sections . After GPU completes the frame, you can retrieve times. This way, you might find, for instance, that drawing lines is taking much longer than expected on Pi (maybe due to heavy overdraw), so you know to optimize that (maybe by simplifying line shader or using simpler joins). The profiler data can be written to a Chrome trace JSON and loaded in Chrome’s chrome://tracing tool for a timeline view .
	•	Don’t forget to request Features::TIMESTAMP_QUERY when creating the device if using this (and ensure the adapter supports it; Vulkan/Metal do).
	•	This adds some overhead (each query is a minor cost), but it’s negligible compared to frame time if you don’t use too many. On Pi, the number of query slots is limited (maybe 64 or so), but wgpu-profiler manages that with a ring buffer.

	•	Frame rate and CPU profiling: In addition to GPU, measure the CPU time per frame to ensure the CPU-side (e.g., style rules, buffer uploads) isn’t the bottleneck. You can use standard Rust profiling (instruments on Mac, or just log timestamps). The plan’s suggestion to log thermal and frame times in CI is smart .
	•	A simple way: use instant::Instant::now() at frame start and end to compute frame time.
	•	Or integrate with tracing crate and emit spans for different stages (input handling, update, render) which can be viewed in a trace.
	•	Since NavCore is likely GPU-bound when drawing lots of features, focus was on GPU, but if you see low GPU usage and still low FPS, maybe CPU is the limiter.
	•	Automated Testing of output: They plan a “golden image” diff. You could actually use WGPU in headless mode to render an image and compare pixel values to a reference. WGPU can operate without a window by using SurfaceTexture::surface (offscreen) or using a normal texture and saving it. For example:

let output_texture = device.create_texture(..render_target_desc..);
// Render pass to output_texture instead of the swapchain.
// After rendering, read back:
let buffer = device.create_buffer(...COPY_DST|MAP_READ..., size = pixels_size);
encoder.copy_texture_to_buffer(output_texture.as_image_copy(), buffer.as_buffer_copy(), bytes_per_row etc.);
// submit and then map buffer to get bytes.

Then compare bytes with a known PNG. This is heavy but as a CI/verification step, it’s doable. Ensure to account for nondeterminism (if any) and differences in floating point between GPU vendors (shouldn’t be an issue for basic rendering; but some differences can occur if using non-integer arithmetic extensively).
	•	Another trick: run both QuteNav and NavCore on the same input and compare their outputs. If needed, we can even overlay them with a difference shader to visually spot any mismatches.

Common pitfalls to debug:
	•	Nothing renders: Check that you set the pipeline and bind groups correctly each draw. WGPU requires you to set a bind group for each index that the shader uses. If you forget or off-by-one, WGPU will log a binding error and draw nothing. Also ensure the vertex buffer is set for each slot used. The logs will usually tell if a draw was skipped due to missing resources.
	•	Validation error: The log might say e.g., “vertex attribute out of bounds” – meaning your vertex buffer is smaller than the pipeline expects for the number of vertices you drew. This helps catch issues early (like wrong vertex count).
	•	Performance suddenly drops: Possibly hitting some fallback (like too many dynamic uniform offsets can cause some drivers to struggle, or maybe a certain pattern triggered GPU thrashing). Use the profiler and logs (some backends log when they have to do something inefficient, e.g. mapping memory might warn if it had to block).
	•	Memory leaks: If you notice VRAM usage growing, ensure you drop buffers/textures for evicted tiles. WGPU’s util::StagingBelt might accumulate if you don’t call recall(). Use device.generate_report() (if available, it was experimental) to see resource counts, or simply log when creating/dropping major resources.

In conclusion, leverage the available tools as part of your development cycle:
	•	Use logs and validation to catch mistakes early (WGPU is very helpful here compared to raw GL).
	•	Use RenderDoc/Xcode to deep-dive into a frame’s details when visuals are incorrect or to verify state.
	•	Use wgpu-profiler and similar instrumentation to measure performance on target hardware, guiding optimizations (like removing that expensive discard).
	•	Automate regression testing with frame captures and comparisons, so that as you tweak for performance, you don’t inadvertently break the rendering output fidelity.

By systematically using these debugging and profiling approaches, you can ensure NavCore’s WGPU renderer matches QuteNav’s output pixel-perfectly and runs efficiently on all target platforms.

⸻

Sources:
	•	WebGPU/WGPU official documentation and tutorials for API usage   .
	•	WGPU Porting Plan (NavCore internal) for context on required features  .
	•	Arm Mali GPU Best Practices for early depth testing and fragment discards  .
	•	“Learn WGPU” tutorial series for examples of pipeline, buffers, and WGSL usage  .
	•	WGPU documentation on features like MAPPABLE_PRIMARY_BUFFERS and texture arrays  .
	•	WGPU GitHub discussions and Stack Overflow for performance tips (buffer update strategies, etc.) .
	•	MDN WebGPU guide for general architecture overview  .