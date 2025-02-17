use std::{borrow::Cow, f32::consts, iter, mem};

use glam::{vec3, Mat4, Vec3};
use wgpu::TextureView;

use spark_gap::buffers::{update_mat4_buffer, update_u32_buffer};
use spark_gap::gpu_context::GpuContext;
use spark_gap::texture::DEPTH_FORMAT;

use crate::cube::Vertex;
use crate::debug_shadow::{create_shadow_map_material, shadow_render_debug, ShadowMaterial};
use crate::entities::Entities;
use crate::forward_pass::{create_forward_pass, ForwardPass};
use crate::lights::Lights;
use crate::shadow_pass::{create_shadow_pass, ShadowPass};

pub struct World {
    pub entities: Entities,
    pub lights: Lights,
    pub shadow_material: ShadowMaterial,
    pub shadow_pass: ShadowPass,
    pub forward_pass: ForwardPass,
    pub forward_depth: TextureView,
    pub show_shadows: bool,
    pub layer_number: u32,
    pub camera_position: u32,
}

impl World {
    pub fn new(gpu_context: &mut GpuContext) -> Self {
        let entities = Entities::new(gpu_context);

        let shader = gpu_context.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("shader.wgsl"))),
        });

        let shadow_material = create_shadow_map_material(gpu_context);

        let lights = Lights::new(gpu_context, &shadow_material.texture);

        let forward_depth = create_depth_texture(gpu_context);

        let shadow_pass = create_shadow_pass(gpu_context, &lights, &entities.entity_bind_group_layout, &shader);

        let forward_pass = create_forward_pass(
            gpu_context,
            &entities.entity_bind_group_layout,
            &lights,
            &shader,
            &shadow_material.texture,
        );

        World {
            entities,
            lights,
            shadow_material,
            shadow_pass,
            forward_pass,
            forward_depth,
            show_shadows: false,
            layer_number: 0,
            camera_position: 0,
        }
    }

    pub fn render(&mut self, context: &GpuContext) {
        self.entities.update(context);
        self.lights.update(context);

        let mut encoder = context
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        // shadow pass
        encoder.push_debug_group("shadow pass");

        for (i, light) in self.lights.lights.iter().enumerate() {
            let i = i as u32;

            encoder.push_debug_group(&format!("shadow pass {} (light at position {:?})", i, light.position));

            encoder.insert_debug_marker("render entities");
            {
                let depth_stencil_attachment = wgpu::RenderPassDepthStencilAttachment {
                    view: &light.shadow_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                };

                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[],
                    depth_stencil_attachment: Some(depth_stencil_attachment),
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });

                pass.set_pipeline(&self.shadow_pass.pipeline);
                pass.set_bind_group(0, &self.shadow_pass.bind_group, &[]);

                for entity in &self.entities.entities {
                    pass.set_bind_group(1, &self.entities.entity_bind_group, &[entity.uniform_offset]);

                    pass.set_vertex_buffer(0, entity.vertex_buf.slice(..));
                    pass.set_index_buffer(entity.index_buf.slice(..), entity.index_format);

                    // the instance id is used as an index into the array of lights in the shader to
                    // get the projection view to use for the current light when writing to the light's shadow_view
                    pass.draw_indexed(0..entity.index_count as u32, 0, i..(i + 1));
                }
            }
            encoder.pop_debug_group();
        }
        encoder.pop_debug_group();

        // forward pass
        encoder.push_debug_group("forward rendering pass");

        let frame = context
            .surface
            .get_current_texture()
            .expect("Failed to acquire next swap chain texture");

        let frame_view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        {
            let color_attachment = wgpu::RenderPassColorAttachment {
                view: &frame_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.1,
                        g: 0.2,
                        b: 0.3,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            };

            let depth_stencil_attachment = wgpu::RenderPassDepthStencilAttachment {
                view: &self.forward_depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }; 
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(color_attachment)],
                depth_stencil_attachment: Some(depth_stencil_attachment),
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            let width = context.config.width as f32 / 2.0;
            let height = context.config.height as f32 / 2.0;
            let aspect_ratio = width / height;

            let orthographic_projection = Mat4::orthographic_rh(-width, width, -height, height, 0.1, 1000.0);
            let view = Mat4::look_at_rh(vec3(0.0, 0.0001, 200.0), vec3(0.0, 0.0, 0.0), vec3(0.0, 0.0, 1.0));

            let project_view_matrix = orthographic_projection * view;

            update_mat4_buffer(context, &self.shadow_material.projection_view_buffer, &project_view_matrix);
            update_u32_buffer(context, &self.shadow_material.layer_num_buffer, &self.layer_number);
           
            let pv = match &self.camera_position {
                0 => get_projection_view_matrix(aspect_ratio),
                1 => self.lights.lights[0].projection_view,
                2 => self.lights.lights[1].projection_view,
                _ => Mat4::IDENTITY,
            };

            update_mat4_buffer(context, &self.forward_pass.projection_view_buffer, &pv);

            if self.show_shadows == true {
                // display shadow map
                pass.set_pipeline(&self.shadow_material.shadow_debug_pipeline);
                pass = shadow_render_debug(pass, &self.shadow_material);
            } else {
                // forward pass
                pass.set_pipeline(&self.forward_pass.pipeline);
                pass.set_bind_group(0, &self.forward_pass.bind_group, &[]);

                for entity in &self.entities.entities {
                    pass.set_bind_group(1, &self.entities.entity_bind_group, &[entity.uniform_offset]);

                    pass.set_vertex_buffer(0, entity.vertex_buf.slice(..));
                    pass.set_index_buffer(entity.index_buf.slice(..), entity.index_format);

                    pass.draw_indexed(0..entity.index_count as u32, 0, 0..1);
                }
            }
        }
        encoder.pop_debug_group();

        context.queue.submit(iter::once(encoder.finish()));
        frame.present();
    }

    pub fn resize(&mut self, gpu_context: &GpuContext) {
        let mx_total = get_projection_view_matrix(gpu_context.config.width as f32 / gpu_context.config.height as f32);
        let mx_ref: &[f32; 16] = mx_total.as_ref();

        gpu_context
            .queue
            .write_buffer(&self.forward_pass.projection_view_buffer, 0, bytemuck::cast_slice(mx_ref));

        self.forward_depth = create_depth_texture(gpu_context);
    }
}

pub fn get_vertex_buffer_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: mem::size_of::<Vertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &[
            // vertices
            wgpu::VertexAttribute {
                shader_location: 0,
                format: wgpu::VertexFormat::Sint8x4,
                offset: 0,
            },
            // tex coords
            wgpu::VertexAttribute {
                shader_location: 1,
                format: wgpu::VertexFormat::Sint8x4,
                // Sint8x4 is four signed bytes (i8). vec4<i32> in shaders
                offset: mem::size_of::<[i8; 4]>() as wgpu::BufferAddress,
            },
        ],
    }
}

pub fn get_projection_view_matrix(aspect_ratio: f32) -> Mat4 {
    let projection = Mat4::perspective_rh(consts::FRAC_PI_4, aspect_ratio, 1.0, 200.0);
    let view = Mat4::look_at_rh(Vec3::new(3.0f32, -20.0, 6.0), Vec3::new(0f32, 0.0, 0.0), Vec3::Z);
    projection * view
}

fn create_depth_texture(gpu_context: &GpuContext) -> TextureView {
    let depth_texture = gpu_context.device.create_texture(&wgpu::TextureDescriptor {
        size: wgpu::Extent3d {
            width: gpu_context.config.width,
            height: gpu_context.config.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        label: None,
        view_formats: &[],
    });

    depth_texture.create_view(&wgpu::TextureViewDescriptor::default())
}
