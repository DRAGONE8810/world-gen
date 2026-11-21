#version 450

// Packed 8-byte vertex from the mesher (see mesh.rs):
//   a = x (5 bits) | z (5 bits) << 5 | (y + 32768) (16 bits) << 10
//   b = sRGB colour r | g << 8 | b << 16
layout(location = 0) in uint in_a;
layout(location = 1) in uint in_b;

// One entry per drawn mesh, picked by the draw's firstInstance.
// xyz: mesh origin minus camera position, computed on the CPU in 64-bit
// floats, so the GPU only ever sees small numbers and there is no jitter
// even 30,000 km from the world origin. w: cell size (1 for chunks,
// 2^level blocks for LOD tiles).
layout(std430, set = 0, binding = 0) readonly buffer Draws {
    vec4 draws[];
};

layout(push_constant) uniform Push {
    mat4 view_proj;
    vec4 fog; // rgb: linear fog colour, w: fog end distance
} pc;

layout(location = 0) out vec3 v_color;
layout(location = 1) out float v_dist;

void main() {
    vec4 d = draws[gl_InstanceIndex];
    vec3 local = vec3(
        float(in_a & 31u) * d.w,
        float(int((in_a >> 10) & 65535u) - 32768),
        float((in_a >> 5) & 31u) * d.w
    );
    vec3 rel = local + d.xyz;
    gl_Position = pc.view_proj * vec4(rel, 1.0);
    vec3 srgb = vec3(float(in_b & 255u), float((in_b >> 8) & 255u), float((in_b >> 16) & 255u)) / 255.0;
    v_color = pow(srgb, vec3(2.2)); // sRGB -> linear
    v_dist = length(rel.xz);
}
