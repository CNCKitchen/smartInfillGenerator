// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Field-mapped section cap: the stencil cap quad of the capped section view,
// colored by the VOLUMETRIC result field instead of a flat cut color — the
// CAD-style "see the stress inside the part" section. The engine ships the
// recovered nodal field + nodal displacements on the solution grid
// (`sectionVolume`); both live here as 3D textures the cap's fragment shader
// samples at its world position:
//
//   x_rest = p − s·u(x_rest)   (2 fixed-point steps — the exaggerated view
//                               deforms the geometry, the field lives in
//                               rest space)
//   v      = trilinear(field, x_rest)      (or |u| / a u component)
//   color  = LUT(clamp((v − lo)/(hi − lo)))
//
// The LUT is the ColorManager's SHARED jet texture, so banded contours and
// any LUT rewrite apply to the cap automatically. Trilinear filtering is done
// manually with texelFetch (8 taps) — float-texture linear filtering is an
// optional WebGL extension, texelFetch works everywhere.

import * as THREE from "three";
import type { SectionVolume } from "../engine/EngineProtocol";

/** Color-mode uniform: 0 = scalar field texture, 1 = |u|, 2/3/4 = u.x/y/z. */
function modeOf(volume: SectionVolume, dispComp: number): number {
  if (volume.values.length) return 0;
  return dispComp < 0 ? 1 : 2 + dispComp;
}

export class SectionFieldCap {
  /** Shared by every cap quad that shows the field (part + voxel-result). */
  readonly material: THREE.MeshStandardMaterial;

  private uniforms = {
    uField: { value: null as THREE.Data3DTexture | null },
    uDisp: { value: null as THREE.Data3DTexture | null },
    uLut: { value: null as THREE.Texture | null },
    uDims: { value: new THREE.Vector3(2, 2, 2) },
    uOrigin: { value: new THREE.Vector3() },
    uH: { value: 1 },
    uLo: { value: 0 },
    uHi: { value: 1 },
    uFlip: { value: 0 },
    uDefScale: { value: 0 },
    uMode: { value: 0 },
  };

  private volume: SectionVolume | null = null;
  private fieldTex: THREE.Data3DTexture | null = null;
  private dispTex: THREE.Data3DTexture | null = null;

  constructor(lut: THREE.Texture) {
    this.uniforms.uLut.value = lut;
    // Same stencil setup as the plain cap: draw only where the increment/
    // decrement passes marked the open cross-section (stencil != 0).
    this.material = new THREE.MeshStandardMaterial({
      metalness: 0.05,
      roughness: 0.85,
      side: THREE.DoubleSide,
      stencilWrite: true,
      stencilRef: 0,
      stencilFunc: THREE.NotEqualStencilFunc,
      stencilFail: THREE.ReplaceStencilOp,
      stencilZFail: THREE.ReplaceStencilOp,
      stencilZPass: THREE.ReplaceStencilOp,
    });
    this.material.onBeforeCompile = (shader) => {
      Object.assign(shader.uniforms, this.uniforms);
      shader.vertexShader = shader.vertexShader
        .replace("#include <common>", "#include <common>\nvarying vec3 vCapWorld;")
        .replace(
          "#include <worldpos_vertex>",
          "#include <worldpos_vertex>\nvCapWorld = (modelMatrix * vec4(transformed, 1.0)).xyz;"
        );
      shader.fragmentShader = shader.fragmentShader
        .replace(
          "#include <common>",
          /* glsl */ `#include <common>
precision highp sampler3D;
varying vec3 vCapWorld;
uniform sampler3D uField;
uniform sampler3D uDisp;
uniform sampler2D uLut;
uniform vec3 uDims;
uniform vec3 uOrigin;
uniform float uH;
uniform float uLo;
uniform float uHi;
uniform float uFlip;
uniform float uDefScale;
uniform int uMode;

// World position -> continuous node coordinates, clamped into the grid.
vec3 capGridCoord(vec3 p) {
  return clamp((p - uOrigin) / uH, vec3(0.0), uDims - vec3(1.0));
}
// Manual trilinear taps: base node + intra-cell fraction.
void capTaps(vec3 g, out ivec3 i0, out vec3 f) {
  vec3 b = min(floor(g), uDims - vec3(2.0));
  b = max(b, vec3(0.0));
  i0 = ivec3(b);
  f = clamp(g - b, 0.0, 1.0);
}
float capField(vec3 g) {
  ivec3 i; vec3 f; capTaps(g, i, f);
  #define T(dx,dy,dz) texelFetch(uField, i + ivec3(dx,dy,dz), 0).r
  float c00 = mix(T(0,0,0), T(1,0,0), f.x), c10 = mix(T(0,1,0), T(1,1,0), f.x);
  float c01 = mix(T(0,0,1), T(1,0,1), f.x), c11 = mix(T(0,1,1), T(1,1,1), f.x);
  #undef T
  return mix(mix(c00, c10, f.y), mix(c01, c11, f.y), f.z);
}
vec3 capDisp(vec3 g) {
  ivec3 i; vec3 f; capTaps(g, i, f);
  #define T(dx,dy,dz) texelFetch(uDisp, i + ivec3(dx,dy,dz), 0).rgb
  vec3 c00 = mix(T(0,0,0), T(1,0,0), f.x), c10 = mix(T(0,1,0), T(1,1,0), f.x);
  vec3 c01 = mix(T(0,0,1), T(1,0,1), f.x), c11 = mix(T(0,1,1), T(1,1,1), f.x);
  #undef T
  return mix(mix(c00, c10, f.y), mix(c01, c11, f.y), f.z);
}`
        )
        .replace(
          "#include <color_fragment>",
          /* glsl */ `#include <color_fragment>
{
  // Un-deform: the cap cuts the EXAGGERATED geometry, the field lives on the
  // rest shape. Two fixed-point steps of x = p - s*u(x) are ample for the
  // display exaggerations in use.
  vec3 xr = vCapWorld;
  if (uDefScale != 0.0) {
    xr = vCapWorld - uDefScale * capDisp(capGridCoord(xr));
    xr = vCapWorld - uDefScale * capDisp(capGridCoord(xr));
  }
  vec3 g = capGridCoord(xr);
  float v;
  if (uMode == 0) v = capField(g);
  else {
    vec3 u = capDisp(g);
    v = uMode == 1 ? length(u) : (uMode == 2 ? u.x : (uMode == 3 ? u.y : u.z));
  }
  float t = clamp((v - uLo) / max(uHi - uLo, 1e-30), 0.0, 1.0);
  if (uFlip > 0.5) t = 1.0 - t;
  diffuseColor.rgb = texture(uLut, vec2(t, 0.5)).rgb;
}`
        );
    };
  }

  /** The field cap can render: a volume is loaded (textures are built). */
  get active(): boolean {
    return this.volume !== null;
  }

  /** Interior extreme locations (cell centers, rest space) — null for
   *  displacement kinds. */
  get range(): SectionVolume["range"] {
    return this.volume?.range ?? null;
  }

  /** Load (or clear) the volumetric payload; (re)builds the 3D textures. */
  setVolume(volume: SectionVolume | null) {
    if (volume === this.volume) return;
    this.volume = volume;
    this.fieldTex?.dispose();
    this.dispTex?.dispose();
    this.fieldTex = null;
    this.dispTex = null;
    if (!volume) return;
    const [mx, my, mz] = volume.dims;
    const make = (data: Float32Array, format: THREE.AnyPixelFormat) => {
      const tex = new THREE.Data3DTexture(data, mx, my, mz);
      tex.format = format as THREE.PixelFormat;
      tex.type = THREE.FloatType;
      tex.minFilter = THREE.NearestFilter;
      tex.magFilter = THREE.NearestFilter;
      tex.unpackAlignment = 1;
      tex.needsUpdate = true;
      return tex;
    };
    // Scalar field (empty for displacement kinds — a 1-texel dummy keeps the
    // sampler bound; uMode never reads it then).
    this.fieldTex = volume.values.length
      ? make(volume.values, THREE.RedFormat)
      : make(new Float32Array(mx * my * mz), THREE.RedFormat);
    // Displacements arrive packed xyz — expand to RGBA (RGB float upload is
    // not a renderable/supported combination everywhere).
    const n = mx * my * mz;
    const rgba = new Float32Array(4 * n);
    for (let i = 0; i < n; i++) {
      rgba[4 * i] = volume.disp[3 * i];
      rgba[4 * i + 1] = volume.disp[3 * i + 1];
      rgba[4 * i + 2] = volume.disp[3 * i + 2];
    }
    this.dispTex = make(rgba, THREE.RGBAFormat);
    this.uniforms.uField.value = this.fieldTex;
    this.uniforms.uDisp.value = this.dispTex;
    this.uniforms.uDims.value.set(mx, my, mz);
    this.uniforms.uOrigin.value.set(...volume.origin);
    this.uniforms.uH.value = volume.h;
  }

  /** Color-scale + mode sync (call after ColorManager.applyColors so the cap
   *  shares the exact normalization the surface used, legend overrides
   *  included). `dispComp` = -1 |u|, 0/1/2 component — used when the volume
   *  carries no scalar field. */
  setRange(lo: number, hi: number, flip: boolean, dispComp: number) {
    this.uniforms.uLo.value = lo;
    this.uniforms.uHi.value = hi;
    this.uniforms.uFlip.value = flip ? 1 : 0;
    if (this.volume) this.uniforms.uMode.value = modeOf(this.volume, dispComp);
  }

  /** Current displacement exaggeration (autoScale·deformScale·animFactor) —
   *  0 outside the deformed view; drives the shader's un-deform step. */
  setDefScale(s: number) {
    this.uniforms.uDefScale.value = s;
  }

  /** DISPLAYED world position of a rest-space point: rest + s·u(rest), with
   *  the same exaggeration the shader uses (CPU trilinear on the nodal
   *  displacements — places the interior-extreme marker). False = no volume. */
  displacedPoint(rest: [number, number, number], out: THREE.Vector3): boolean {
    const v = this.volume;
    if (!v) return false;
    const [mx, my, mz] = v.dims;
    const g = [0, 0, 0];
    for (let a = 0; a < 3; a++) {
      g[a] = Math.min(Math.max((rest[a] - v.origin[a]) / v.h, 0), v.dims[a] - 1);
    }
    const bx = Math.min(Math.floor(g[0]), mx - 2);
    const by = Math.min(Math.floor(g[1]), my - 2);
    const bz = Math.min(Math.floor(g[2]), mz - 2);
    const f = [g[0] - bx, g[1] - by, g[2] - bz];
    const u = [0, 0, 0];
    for (let oz = 0; oz < 2; oz++) {
      for (let oy = 0; oy < 2; oy++) {
        for (let ox = 0; ox < 2; ox++) {
          const w =
            (ox ? f[0] : 1 - f[0]) * (oy ? f[1] : 1 - f[1]) * (oz ? f[2] : 1 - f[2]);
          const n = 3 * (((bz + oz) * my + (by + oy)) * mx + (bx + ox));
          u[0] += w * v.disp[n];
          u[1] += w * v.disp[n + 1];
          u[2] += w * v.disp[n + 2];
        }
      }
    }
    const s = this.uniforms.uDefScale.value;
    out.set(rest[0] + s * u[0], rest[1] + s * u[1], rest[2] + s * u[2]);
    return true;
  }

  dispose() {
    this.fieldTex?.dispose();
    this.dispTex?.dispose();
    this.material.dispose();
  }
}
