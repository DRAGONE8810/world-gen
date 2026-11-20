#version 450

// Per-vertex data from the mesher: position inside the chunk and colour.
layout(location = 0) in vec3 in_pos;
layout(location = 1) in vec4 in_color; // sRGB bytes, normalised

// Camera-relative rendering: `offset` is (chunk origin - camera position),
// computed on the CPU in 64-bit floats. The GPU only ever sees small numbers,
// so there is no jitter even 30,000 km from the world origin.
layout(push_constant) uniform Push {
    mat4 view_proj;
    vec4 offset_fog;   // xyz: chunk offset, w: fog end distance
    vec4 fog_color;    // linear RGB
} pc;

layout(location = 0) out vec3 v_color;
layout(location = 1) out float v_dist;

void main() {
    vec3 rel = in_pos + pc.offset_fog.xyz;
    gl_Position = pc.view_proj * vec4(rel, 1.0);
    v_color = pow(in_color.rgb, vec3(2.2)); // sRGB -> linear
    v_dist = length(rel.xz);
}
