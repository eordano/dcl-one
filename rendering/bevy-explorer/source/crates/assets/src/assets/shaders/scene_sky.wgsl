#import bevy_pbr::forward_io::VertexOutput

struct SceneSky {
    sun_direction: vec3<f32>,
    has_image: u32,
    moon_direction: vec3<f32>,
    visible: u32,
    sun_color: vec3<f32>,
    night: f32,
}

@group(2) @binding(0) var sky_texture: texture_cube<f32>;
@group(2) @binding(1) var sky_sampler: sampler;
@group(2) @binding(100) var<uniform> scene: SceneSky;
@group(2) @binding(101) var scene_texture: texture_2d<f32>;
@group(2) @binding(102) var scene_sampler: sampler;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let ray = normalize(in.world_normal);
    var color = textureSample(sky_texture, sky_sampler, ray).rgb;
    if scene.has_image != 0u {
        // DCL +Z is Bevy -Z; the centre column faces the scene's forward axis.
        let uv = vec2<f32>(atan2(-ray.x, -ray.z) / 6.28318530718 + 0.5, acos(clamp(ray.y, -1.0, 1.0)) / 3.14159265359);
        color = textureSample(scene_texture, scene_sampler, uv).rgb;
        if scene.visible != 0u {
            let sun = normalize(scene.sun_direction);
            color = max(color, mix(color, scene.sun_color, smoothstep(0.997, 0.999, dot(ray, sun))));
            let moon = normalize(scene.moon_direction);
            let moon_dot = dot(ray, moon);
            let perp = -sun - moon * dot(-sun, moon);
            let away = perp / max(length(perp), 1e-3);
            let bite_dir = normalize(moon + away * 0.012);
            let crescent = smoothstep(0.99914, 0.99971, moon_dot) * (1.0 - smoothstep(0.99905, 0.99968, dot(ray, bite_dir)));
            color += vec3<f32>(1.6) * crescent * scene.night;
            color += vec3<f32>(0.5, 0.5, 0.6) * pow(max(moon_dot, 0.0), 250.0) * scene.night * 0.3;
        }
    }
#ifdef DITHER
    let noise = fract(vec3<f32>(dot(vec2<f32>(171.0, 231.0), in.position.xy)) / vec3<f32>(103.0, 71.0, 97.0));
    color += (noise - 0.5) / 255.0;
#endif
    return vec4<f32>(color, 1.0);
}
