// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Hover-card copy for the step panels (see HelpTip / InfoTip).
//
// House rule for the panels: the panel shows the control, its live value, and
// at most ONE short line that changes with the state. Everything that explains,
// qualifies or warns in the abstract lives here and appears on the ⓘ. Panels
// that carry a paragraph per control read as documentation, not as an
// instrument — station 5 alone had a dozen of them.

import type { HelpContent } from "./HelpTip";

// ---------------- 1 · Model ----------------

export const MODEL_HELP: Record<string, HelpContent> = {
  orientation: {
    title: "Print orientation",
    text: [
      "Z is the build direction: the part is analyzed exactly as it would sit on the plate, and layer-adhesion safety treats Z as the weak direction.",
      "“Place on face” turns the face you click to the build plate; the ⟳ buttons nudge 5° about a world axis.",
      "Loads keep their world directions, so reorienting the part changes what the layers have to carry — results reset and need a re-solve.",
    ],
  },
  surface: {
    title: "Surface detection",
    text: [
      "Splits the skin into the pickable surfaces you assign loads and supports to.",
      "Crease angle groups triangles that meet at less than the angle — lower it if separate faces merge, raise it if one face shatters into patches.",
      "A STEP import can use its exact BREP faces instead: one pickable surface per CAD face, with no angle to tune.",
    ],
  },
  rescale: {
    title: "Rescale the model",
    text: [
      "An STL carries no units. Imported with the wrong one, the part comes in 25.4× too large or too small — and every stress with it.",
      "Rescale here instead of re-importing; the bounding box in the status bar confirms the size.",
    ],
  },
  components: {
    title: "Components",
    text: [
      "Multi-body files (STEP assemblies, multi-shell STL/3MF) list every body here — STEP assemblies as their CAD hierarchy, with subassembly checkboxes toggling the whole group. Only checked bodies are analyzed — typically the printed part, with screws, bearings and other hardware suppressed (Suppress all, then re-enable the parts to analyze).",
      "Suppressed bodies are hidden and contribute nothing: no stiffness, no mass, no load path. Active bodies merge into ONE printed part — they fuse where they touch, and a warning appears if they don't.",
      "A ×N badge marks a part placed N times in the assembly — all copies toggle and highlight together.",
      "Toggling is instant; the expensive rebuild (re-seat on the plate, results reset) runs ONCE when you leave this step. Loads and supports on a suppressed body pause and come back with it.",
    ],
  },
};

// ---------------- 3 · Properties ----------------

export const PROP_HELP: Record<string, HelpContent> = {
  material: {
    title: "Material",
    text: [
      "Sets the stiffness, density and strength every result is computed from — including the layer-adhesion strengths used for the safety factor.",
      "The presets are typical values for the filament class. Measured your own? Open the material manager (the \"edit\" link below, or ⚙ Settings) — every value, the FDM/isotropic switch and the stress–strain charts live there; the whole analysis follows.",
      "Isotropic materials (machined metal, cast parts, resin prints) have no build direction: the part is analyzed fully dense, the print settings below disappear, and the safety factor runs against yield. Optimization becomes classic part topology — material removal, exported as an STL.",
    ],
  },
  walls: {
    title: "Perimeters & line width",
    text: [
      "Perimeters × line width is the solid wall the analysis assumes and the wall_loops the exported 3MF prints.",
      "Match the line width to the profile you actually print with — a wall analyzed thicker than it prints reports a part stiffer than it is.",
    ],
  },
  shells: {
    title: "Top & bottom layers",
    text: [
      "Layers × layer height is the solid shell on up- and down-facing surfaces, exported as the top/bottom shell count.",
      "0 layers means no shells at all: the infill shows through the surface (showpieces), and the part loses the plate stiffness those skins give it.",
    ],
  },
  infill: {
    title: "Infill pattern & density",
    text: [
      "The uniform print that “Solve as printed” analyzes, and the starting point the optimizer's budget follows.",
      "Density enters the analysis through the pattern's E(ρ) curve — the calibrated stiffness-vs-density law of the active infill property set (⚙ Settings → Infill properties); its accuracy is the accuracy of every as-printed number.",
    ],
  },
  pattern: {
    title: "Why cubic only",
    text: [
      "Sparse infill is analyzed as transversely isotropic: stiff in the layer plane, softer along the build axis, measured from the real sliced toolpath rather than assumed.",
      "Cubic is the pattern that model fits. Its anisotropy holds to within measurement noise across 20–70 % density, and two independent consistency checks on the measured tensor agree to 0.06 %.",
      "Grid and rectilinear are not merely uncalibrated — they are tetragonal, a different material class, and this model mispredicts their in-plane shear by 28× to 86×. Gyroid's ratios swing ±32 % across the density band. Offering them would mean reporting numbers we know are wrong.",
      "Solid regions still print rectilinear or concentric (set below) — there the material is dense and none of this applies.",
    ],
  },
  anisotropic: {
    title: "Anisotropic infill",
    text: [
      "Sparse infill is not equally stiff in every direction: it is stiffer within a layer than across the layers, because the bond between layers is weaker than the bead itself.",
      "With this on, the analysis uses the measured cubic tensor — about 0.80× the in-plane stiffness along the build axis, and shear moduli that are not tied to E and ν. The numbers come from homogenizing the real sliced toolpath, not from an assumed formula.",
      "Off gives the older isotropic model, which treats infill as equally stiff in all directions. Use it to compare with earlier results — it reads the part stiffer than it is along Z.",
      "Walls and solid shells are unaffected either way: dense material really is isotropic.",
    ],
  },
  resolution: {
    title: "Analysis resolution",
    text: [
      "The part is voxelized into a hex grid; this sets the cell size. Finer resolves thin features and stress peaks better and costs time and memory (the cap is 4M cells).",
      "Snap to the wall picks a cell size that divides the wall thickness (h = wall/k) so the skin lands on whole cell layers.",
      "Composite skin blends wall and infill stiffness inside partially covered cells — it keeps a coarse grid honest, though the geometry stays blocky.",
    ],
  },
};

// ---------------- 4 · Analyze ----------------

export const ANALYZE_HELP: Record<string, HelpContent> = {
  analysis: {
    title: "Analysis type",
    text: [
      "Static: one linear solve under the current loads and supports — deflection, stress and safety factor.",
      "Modal: the lowest natural frequencies and mode shapes of the part as supported by the first load case. Constrained, undamped and force-free — the stiffness choice below sets both stiffness and mass.",
    ],
  },
  stiffness: {
    title: "Stiffness model",
    text: [
      "As printed: solid skin, interior at the uniform infill from Properties through the calibrated E(ρ) curve. This is the part you would actually hold.",
      "Solid material: fully dense E₀ everywhere — the CAD-ideal reference. Run both to answer “how much stiffness does printing cost me?”.",
    ],
  },
  run: {
    title: "Check & solve",
    text: [
      "Check looks for rigid-body freedom: anything the part can still do without deforming is animated in the viewport, so under-constrained setups show themselves before a solve.",
      "Solve lands in the Results view — field picker under the view tabs, playback at the bottom, min/max markers and click-to-edit scale in the legend. As-printed runs also fill the dock on the right (mass, deflection, minimum safety factor).",
    ],
  },
};

// ---------------- 5 · Optimization ----------------

export const OPT_HELP: Record<string, HelpContent> = {
  section: {
    title: "Optimize infill",
    text: [
      "Puts material where the loads need it: the optimizer solves the part, sees which material carries load, and redistributes density until the design stops improving.",
      "The result is a set of infill regions (or a new shape) you export to the slicer — same part, same walls, less mass for the same stiffness.",
    ],
  },
  goal: {
    title: "Optimization goal",
    text: [
      "Stiffest: spend a fixed material budget as well as possible — the least deflection you can get for that mass.",
      "Match stiffness: the other way round. Name a uniform infill % you are happy with and get the LIGHTEST graded design that is just as stiff.",
      "Safety factor: the lightest design whose safety factor stays at or above a target under every included load step.",
      "Frequency: spend a fixed material budget to push the part's lowest natural frequency as HIGH as possible — the goal for anything driven by vibration, where you want the first resonance to sit above the excitation.",
      "Frequency works differently from the other three. Free vibration has no forces in it, so your loads are ignored and only the supports (of the first load case) matter. It also fights itself: material adds stiffness, which raises the frequency, but it also adds mass, which lowers it. The optimizer places density only where the stiffness wins.",
    ],
  },
  goalFrequency: {
    title: "Maximize first natural frequency",
    text: [
      "Keeps your infill budget and rearranges it so the part's fundamental frequency f₁ is as high as it can be at that weight.",
      "Read the result against the equal-weight uniform print quoted in the log — that comparison, not the raw Hz, is what the optimization bought you.",
      "This is an undamped free-vibration estimate of the analyzed geometry. Treat it as a design aid and confirm anything load-bearing against a measurement or FEA.",
    ],
  },
  budget: {
    title: "Infill budget",
    text: [
      "The mean infill of the interior — the same scale as your slicer's uniform infill %. Walls and shells come on top of it.",
      "The optimizer keeps this average and only decides WHERE the density goes: dense where the load path runs, near-empty where nothing is carried.",
    ],
  },
  budgetMatch: {
    title: "As stiff as uniform",
    text: [
      "Names the reference print: the optimizer finds the lightest graded design with the same stiffness as a uniform print at this infill %.",
      "It searches for the budget that hits that stiffness over a few warm-started passes, so this run takes longer than a fixed-budget one.",
    ],
  },
  budgetBinary: {
    title: "Infill budget (binary)",
    text: [
      "Mean interior density, but every cell ends up either at the printability floor or fully solid — nothing in between.",
      "The optimizer runs SIMP-penalized so the design drives itself black-and-white instead of settling on grey mid-densities.",
    ],
  },
  budgetSolid: {
    title: "Retained volume",
    text: [
      "Keeps this share of the design volume as solid material and removes the rest — the stiffest shape at that mass.",
      "This is topology optimization, not infill: the output is a new part outline. Material under loads and supports is kept regardless.",
    ],
  },
  sfTarget: {
    title: "Target safety factor",
    text: [
      "Uses as little material as possible while the safety factor stays at or above the target under every included load step.",
      "A pre-flight check reports honestly when even 100 % fill cannot reach it, instead of quietly returning the best it managed.",
      "Strengths come from the material presets (or your own measurements) with Gibson–Ashby scaling for the infill — a design aid, not a certified safety factor.",
    ],
  },
  sfMeasure: {
    title: "Measured against",
    text: [
      "Material: von Mises stress against the in-plane tensile strength — the filament's own limit.",
      "Layers: tension across the layers plus interlayer shear. This is how FDM parts usually fail, and it depends on the print orientation.",
      "Material + layers takes the worse of the two per cell — the same number the SF plot shows.",
    ],
  },
  mode: {
    title: "Optimization mode",
    text: [
      "Graded: several discrete infill densities placed from the optimized field. The part keeps its shape and its walls; only the infill varies. This is what a slicer can print.",
      "Binary: hollow or solid — the interior is either the printability floor or 100 % dense.",
      "Part Topo: topology optimization. Material is REMOVED to make a new lightweight shape — no infill, no walls; what is kept prints solid.",
    ],
  },
  solidFill: {
    title: "Solid fill pattern",
    text: [
      "The infill pattern written for the fully dense regions. Rectilinear is the usual choice; concentric follows the region outline and can look better on show parts.",
    ],
  },
  selfSupport: {
    title: "Self-supporting (overhang constraint)",
    text: [
      "Constrains the design so it prints without support material. Build direction is Z — the print orientation from step 1.",
      "The angle is measured from horizontal: 0° allows flat overhangs (no constraint), 90° allows only vertical walls. In Part Topo it shapes the outer surface; for infill it forces unsupported dense material down to the floor density.",
      "Advisory: the voxel staircase can still nick the angle locally.",
    ],
  },
  levels: {
    title: "Density levels",
    text: [
      "How many discrete infill densities the graded result is quantized to — one modifier region per level in the export.",
      "Auto places them from the optimized density field with the bottom level pinned at the printability floor.",
      "Type a comma-separated list (e.g. 10, 40, 70) to pin exactly those densities — match them to values you have calibration data for. A level at 100 % exports as solid rectilinear fill. Clear the box, or press auto, to go back.",
    ],
  },
  retainBc: {
    title: "Keep load & support regions solid",
    text: [
      "Forces the material under every load and support to stay in the design — recommended, and how a real mounting face survives the optimization.",
      "Switched off, this is pure topology optimization: the result may carve away material right under a load, which is mathematically optimal and physically useless.",
    ],
  },
  symmetry: {
    title: "Planar symmetry",
    text: [
      "Mirror-paired cells share one density, so the result comes out symmetric about the plane even when the load case is not.",
      "Drag the orange plane's arrow to move it and the rings to tilt it (shown while this step is open). Cells whose mirror lands outside the part stay free.",
    ],
  },
  minMember: {
    title: "Minimum member size",
    text: [
      "Roughly the diameter of the thinnest structure the optimizer may create — thicker members print more reliably, thinner ones blur away during optimization.",
      "Defaults to 4× your line width. At 0 only the numerical anti-checkerboard floor applies.",
      "It is enforced by a filter over the grid, so a coarse mesh caps how large a member size can actually be held.",
    ],
  },
  settings: {
    title: "Optimize print settings",
    text: [
      "Answers the question you would otherwise ask your slicer: how many walls and how much infill does this part need to hold?",
      "It searches perimeters (2–8) × infill (10–70 % in 5 % steps) on the current analysis mesh for the LIGHTEST uniform print that still reaches your safety-factor target. Top and bottom layers follow the walls (⌈perimeters × line width ÷ layer height⌉).",
      "Line width, layer height and pattern stay as you set them — they are printer choices, not strength knobs.",
    ],
  },
  settingsSf: {
    title: "Safety-factor target",
    text: [
      "The lowest scored cell of the criterion field has to reach this number. Pick the criterion safety-factor plot to see it — the minimum marker sits on exactly the cell the number came from.",
      "Cells inside a rigid support's own singularity radius are left out: their stress is a modelling artifact that never converges. They are greyed in that plot; the plain safety-factor plot still shows them.",
      "Advisory, not a certified safety factor: strengths come from the material table, and graded infill is scaled by the same E(ρ) law as its stiffness.",
    ],
  },
  settingsApply: {
    title: "Apply settings & verify",
    text: [
      "Writes the winning perimeters, top/bottom layers and infill into Properties, then runs the normal Solve once · As printed.",
      "The number you keep therefore comes from the standard verification on the real (snapped) mesh, not from inside the search.",
    ],
  },
  orientation: {
    title: "Optimize orientation",
    text: [
      "Scores every build direction (rotation X/Y, ±90°) by the minimum layer-adhesion safety factor of the current result — all load steps, worst case.",
      "One solve is enough: the stress field does not care how the part is oriented, only the layer criterion does. Peaks within ~3 cells of rigid constraints are excluded from the score but reported alongside.",
      "Click or drag on the map to preview an orientation — the rotation and coloring are display-only until you reorient the part in step 1.",
    ],
  },
};

// ---------------- Build sim ----------------

export const BUILD_HELP: Record<string, HelpContent> = {
  shrinkPhys: {
    title: "Warp from material shrink",
    text: [
      "Inherent-strain warp via sequential layer activation: layers are added one at a time, each shrinking as it cools, and the part pulls itself out of shape.",
      "The shrink is derived from physics — the material locks at its Tg/Tc and contracts by its CTE down to room temperature. Edit Tg and CTE in ⚙ Settings.",
      "Uncalibrated: the warp SHAPE is meaningful, the absolute magnitude is not.",
    ],
  },
  shrink: {
    title: "Warp from material shrink",
    text: [
      "Inherent-strain warp via sequential layer activation: layers are added one at a time, each shrinking as it cools, and the part pulls itself out of shape.",
      "This material has no thermal data, so the raw shrink percentages apply directly. Edit them in ⚙ Settings, or add Tg/CTE to derive them from physics.",
      "Uncalibrated: the warp SHAPE is meaningful, the absolute magnitude is not.",
    ],
  },
  temps: {
    title: "Bed & chamber temperature",
    text: [
      "They set the temperature ladder — which layers are still warm, and therefore still free to move, while the part builds.",
      "The total shrink from lock temperature to room temperature is unchanged; what changes is how much of it is locked into the part as warp.",
    ],
  },
  run: {
    title: "Run build simulation",
    text: [
      "Ignores supports and loads: the inputs are the part, the material shrink, the as-printed infill density (once optimized) and the build plate.",
      "It runs on a coarser grid than the analysis for speed. The result lands in the deformed Results view, where On bed / Released switch with no re-solve.",
    ],
  },
};

// ---------------- 6 · View & export ----------------

export const EXPORT_HELP: Record<string, HelpContent> = {
  tune: {
    title: "Fine-tune surface",
    text: [
      "Re-cuts the exported geometry from the optimized density field at a different level — it moves the surface in or out.",
      "It does NOT re-optimize and does not respect the budget: retain more and the part gets heavier than the run that produced it.",
    ],
  },
  cutaway: {
    title: "Density cutaway",
    text: [
      "Display only: hides everything below the threshold so you can look at the load path inside the part instead of at its painted surface.",
    ],
  },
  smoothing: {
    title: "Region smoothing",
    text: [
      "Melts the voxel staircase off the exported region surfaces. Updates live — the export uses exactly what you see.",
      "Heavy smoothing rounds off small features; the analysis was run on the unsmoothed grid either way.",
    ],
  },
  threemf: {
    title: "Slicer project (.3mf)",
    text: [
      "Opens in the slicer with the part, the modifier volumes and their infill densities already set.",
      "ONLY densities are overridden — walls, shells, temperatures and everything else come from your own profile. A level pinned at 100 % also gets rectilinear infill on its region so it slices truly solid.",
    ],
  },
  shape: {
    title: "Optimized shape (.stl)",
    text: [
      "A single watertight body of the kept material — re-slice it (solid / 100 % infill) or take it back into CAD.",
      "Material under loads and supports was kept automatically; floating islands were dropped.",
    ],
  },
  positioned: {
    title: "Slicer project (.3mf)",
    text: [
      "This run has no modifier volumes to ship, so the project carries the model itself — on the plate, in the orientation that was analyzed.",
      "After an as-printed analysis it also carries the walls, shells and infill % the solve assumed, so what you print is what was checked. After a Part Topo run it carries the optimized body at 100 % infill. Everything else comes from your own profile.",
    ],
  },
  color3mf: {
    title: "Color 3MF",
    text: [
      "Paints the active result field into discrete filament bands across the current contour min/max and exports it as a multi-color 3MF.",
      "Triangles are cut along the band iso-lines, so the transitions are sharp and the mesh stays watertight. Opens painted in Bambu Studio / OrcaSlicer.",
    ],
  },
};
