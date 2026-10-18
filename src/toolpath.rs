use crate::gcode;
use egui::{Pos2, Rect, Vec2};
use egui_bench::viewer3d::{
    Camera, Lines, Look, Material, Projector, Scene, Shading, Style, Surface, V3, Viewer,
};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

const MAX_SEGMENTS: usize = 140_000;
const FLAT: f32 = 0.0025;
const MAX_TOLERANCE: f32 = 0.2;
const SNAPSHOT_SEGMENTS: usize = 40_000;
const SNAPSHOT_TOLERANCE: f32 = 1.0;
const SNAPSHOT_FILL: f32 = 0.92;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seg {
    pub a: [f32; 2],
    pub b: [f32; 2],
    pub z: f32,
    pub raw: u32,
    pub stripped: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Toolpath {
    pub segs: Vec<Seg>,
    pub layer_start: Vec<usize>,
    pub layer_z: Vec<f32>,
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub parsed: usize,
}

impl Toolpath {
    pub fn layers(&self) -> usize {
        self.layer_start.len()
    }

    pub fn layer_of(&self, seg: usize) -> usize {
        self.layer_start.partition_point(|&s| s <= seg).saturating_sub(1)
    }

    pub fn printed(&self, mark: Mark) -> usize {
        self.segs.partition_point(|s| mark.offset(s) <= mark.at)
    }

    pub fn size(&self) -> [f32; 3] {
        [self.max[0] - self.min[0], self.max[1] - self.min[1], self.max[2] - self.min[2]]
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mark {
    pub raw: bool,
    pub at: u32,
}

impl Mark {
    fn offset(self, s: &Seg) -> u32 {
        if self.raw { s.raw } else { s.stripped }
    }
}

struct Machine {
    pos: [f32; 3],
    e: f32,
    relative: bool,
    relative_e: bool,
    raw: u64,
    stripped: u64,
    segs: Vec<Seg>,
    parsed: usize,
    floor: usize,
}

fn straight(last: &Seg, to: [f32; 2], flat: f32) -> bool {
    let d1 = [last.b[0] - last.a[0], last.b[1] - last.a[1]];
    let d2 = [to[0] - last.b[0], to[1] - last.b[1]];
    let cross = d1[0] * d2[1] - d1[1] * d2[0];
    let dot = d1[0] * d2[0] + d1[1] * d2[1];
    let l1 = (d1[0] * d1[0] + d1[1] * d1[1]).sqrt();
    let l2 = (d2[0] * d2[0] + d2[1] * d2[1]).sqrt();
    l1 > 0.0 && l2 > 0.0 && dot > 0.0 && cross.abs() <= flat * l1 * l2
}

fn distance_to(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let d = [b[0] - a[0], b[1] - a[1]];
    let len2 = d[0] * d[0] + d[1] * d[1];
    if len2 < 1e-12 {
        return ((p[0] - a[0]).powi(2) + (p[1] - a[1]).powi(2)).sqrt();
    }
    ((p[0] - a[0]) * d[1] - (p[1] - a[1]) * d[0]).abs() / len2.sqrt()
}

fn simplify_chain(chain: &[Seg], tolerance: f32, out: &mut Vec<Seg>) {
    let n = chain.len();
    let mut keep = vec![false; n + 1];
    keep[0] = true;
    keep[n] = true;
    let point = |i: usize| if i == n { chain[n - 1].b } else { chain[i].a };
    let mut stack = vec![(0usize, n)];
    while let Some((from, to)) = stack.pop() {
        if to - from < 2 {
            continue;
        }
        let (a, b) = (point(from), point(to));
        let (mut far, mut worst) = (from, 0.0f32);
        for i in from + 1..to {
            let d = distance_to(point(i), a, b);
            if d > worst {
                worst = d;
                far = i;
            }
        }
        if worst > tolerance {
            keep[far] = true;
            stack.push((from, far));
            stack.push((far, to));
        }
    }
    let mut start = 0;
    for i in 1..=n {
        if keep[i] {
            let last = &chain[i - 1];
            out.push(Seg {
                a: point(start),
                b: point(i),
                z: last.z,
                raw: last.raw,
                stripped: last.stripped,
            });
            start = i;
        }
    }
}

fn continues(start: &Seg, prev: &Seg, next: &Seg) -> bool {
    let (d0, d) = (direction(start), direction(next));
    prev.z == next.z && prev.b == next.a && d0[0] * d[0] + d0[1] * d[1] > 0.0
}

fn simplify(segs: Vec<Seg>, tolerance: f32) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::with_capacity(segs.len() / 2);
    let mut from = 0;
    while from < segs.len() {
        let mut to = from + 1;
        while to < segs.len() && continues(&segs[from], &segs[to - 1], &segs[to]) {
            to += 1;
        }
        simplify_chain(&segs[from..to], tolerance, &mut out);
        from = to;
    }
    out
}

fn argument(word: &str) -> Option<(char, f32)> {
    let mut chars = word.chars();
    let letter = chars.next()?.to_ascii_uppercase();
    letter.is_ascii_alphabetic().then(|| chars.as_str().parse().ok().map(|v| (letter, v)))?
}

impl Machine {
    fn push(&mut self, a: [f32; 2], b: [f32; 2], z: f32) {
        self.parsed += 1;
        let raw = self.raw.min(u32::MAX as u64) as u32;
        let stripped = self.stripped.min(u32::MAX as u64) as u32;
        let open = self.segs.len() > self.floor;
        if open
            && let Some(last) = self.segs.last_mut()
            && last.z == z
            && last.b == a
            && straight(last, b, FLAT)
        {
            last.b = b;
            last.raw = raw;
            last.stripped = stripped;
            return;
        }
        self.segs.push(Seg { a, b, z, raw, stripped });
    }

    fn command(&mut self, cmd: &str) {
        let mut words = cmd.split_whitespace();
        let Some(code) = words.next() else { return };
        match code.to_ascii_uppercase().as_str() {
            "G90" => self.relative = false,
            "G91" => self.relative = true,
            "M82" => self.relative_e = false,
            "M83" => self.relative_e = true,
            "G92" => {
                for (letter, v) in words.filter_map(argument) {
                    match letter {
                        'X' => self.pos[0] = v,
                        'Y' => self.pos[1] = v,
                        'Z' => self.pos[2] = v,
                        'E' => self.e = v,
                        _ => {}
                    }
                }
            }
            "G0" | "G1" => self.travel(words, None),
            "G2" => self.travel(words, Some(true)),
            "G3" => self.travel(words, Some(false)),
            _ => {}
        }
    }

    fn travel<'a>(&mut self, words: impl Iterator<Item = &'a str>, arc: Option<bool>) {
        let mut to = self.pos;
        let (mut e, mut i, mut j) = (None, 0.0, 0.0);
        for (letter, v) in words.filter_map(argument) {
            match letter {
                'X' => to[0] = if self.relative { self.pos[0] + v } else { v },
                'Y' => to[1] = if self.relative { self.pos[1] + v } else { v },
                'Z' => to[2] = if self.relative { self.pos[2] + v } else { v },
                'E' => e = Some(v),
                'I' => i = v,
                'J' => j = v,
                _ => {}
            }
        }
        let extruded = match e {
            Some(v) if self.relative_e => v,
            Some(v) => {
                let d = v - self.e;
                self.e = v;
                d
            }
            None => 0.0,
        };
        let moved = to[0] != self.pos[0] || to[1] != self.pos[1];
        if extruded > 1e-6 && moved {
            let from = [self.pos[0], self.pos[1]];
            let end = [to[0], to[1]];
            match arc {
                Some(clockwise) if i != 0.0 || j != 0.0 => {
                    for (a, b) in arc_points(from, end, [i, j], clockwise) {
                        self.push(a, b, to[2]);
                    }
                }
                _ => self.push(from, end, to[2]),
            }
        }
        self.pos = to;
    }
}

fn arc_points(
    from: [f32; 2],
    to: [f32; 2],
    offset: [f32; 2],
    clockwise: bool,
) -> Vec<([f32; 2], [f32; 2])> {
    let centre = [from[0] + offset[0], from[1] + offset[1]];
    let radius = (offset[0] * offset[0] + offset[1] * offset[1]).sqrt();
    let start = (from[1] - centre[1]).atan2(from[0] - centre[0]);
    let mut end = (to[1] - centre[1]).atan2(to[0] - centre[0]);
    let tau = std::f32::consts::TAU;
    if clockwise {
        while end >= start {
            end -= tau;
        }
    } else {
        while end <= start {
            end += tau;
        }
    }
    let sweep = end - start;
    let steps = ((sweep.abs() * radius / 0.6).ceil() as usize).clamp(2, 96);
    let point = |n: usize| {
        if n == steps {
            to
        } else {
            let a = start + sweep * n as f32 / steps as f32;
            [centre[0] + radius * a.cos(), centre[1] + radius * a.sin()]
        }
    };
    (0..steps).map(|n| (if n == 0 { from } else { point(n) }, point(n + 1))).collect()
}

pub fn parse(path: &Path) -> Result<Toolpath, String> {
    parse_within(path, MAX_SEGMENTS, MAX_TOLERANCE)
}

pub fn parse_within(path: &Path, most: usize, coarsest: f32) -> Result<Toolpath, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut m = Machine {
        pos: [0.0; 3],
        e: 0.0,
        relative: false,
        relative_e: false,
        raw: 0,
        stripped: 0,
        segs: Vec::new(),
        parsed: 0,
        floor: 0,
    };
    let mut first_layer = None;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n =
            reader.read_until(b'\n', &mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        m.raw += n as u64;
        let line = String::from_utf8_lossy(&buf);
        if first_layer.is_none() && gcode::is_layer_change(&line) {
            m.floor = m.segs.len();
            first_layer = Some(m.floor);
        }
        if let Some(cmd) = gcode::command(&line) {
            m.stripped += cmd.len() as u64 + 1;
            m.command(cmd);
        }
    }
    let parsed = m.parsed;
    let mut segs = m.segs;
    segs.drain(..first_layer.unwrap_or(0));
    if segs.is_empty() {
        return Err(format!("{} has no printing moves to draw", path.display()));
    }
    let mut tolerance = 0.0f32;
    while segs.len() > most && tolerance < coarsest {
        tolerance = if tolerance == 0.0 { 0.02 } else { (tolerance * 1.5).min(coarsest) };
        segs = simplify(segs, tolerance);
    }
    let mut layer_start = Vec::new();
    let mut layer_z: Vec<f32> = Vec::new();
    let mut min = [f32::MAX; 3];
    let mut max = [f32::MIN; 3];
    for (k, s) in segs.iter().enumerate() {
        if layer_z.last().is_none_or(|z| (z - s.z).abs() > 0.0005) {
            layer_start.push(k);
            layer_z.push(s.z);
        }
        for p in [s.a, s.b] {
            for axis in 0..2 {
                min[axis] = min[axis].min(p[axis]);
                max[axis] = max[axis].max(p[axis]);
            }
        }
        min[2] = min[2].min(s.z);
        max[2] = max[2].max(s.z);
    }
    Ok(Toolpath { segs, layer_start, layer_z, min, max, parsed })
}

const LAYER: f32 = 0.2;
const BUCKETS: usize = 1600;
const MAX_PER_LAYER: usize = 24;
const SHARP: f32 = -0.2;
const LOW: [f32; 3] = [0.30, 0.55, 0.70];
const HIGH: [f32; 3] = [0.74, 0.92, 0.98];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Plastic;

impl Material for Plastic {
    fn shading(&self) -> Shading {
        Shading { gloss: 0.4, sharpness: 26.0, metal: 0.0, capped: false, tinted: true }
    }
}

pub struct Built {
    pub scene: std::sync::Arc<Scene<Plastic>>,
    pub ranges: Vec<(usize, usize)>,
    pub layer_of: Vec<usize>,
}

fn height_colour(t: f32) -> V3 {
    let t = t.clamp(0.0, 1.0);
    [0, 1, 2].map(|k| LOW[k] + (HIGH[k] - LOW[k]) * t)
}

struct Profile {
    u: Vec<f32>,
    v: Vec<f32>,
    nu: Vec<f32>,
    nv: Vec<f32>,
}

fn profile(points: usize, width: f32, height: f32) -> Profile {
    let mut p = Profile { u: Vec::new(), v: Vec::new(), nu: Vec::new(), nv: Vec::new() };
    for j in 0..points {
        let t = std::f32::consts::PI * (1.0 - j as f32 / (points - 1) as f32);
        let (sin, cos) = t.sin_cos();
        p.u.push(cos * width / 2.0);
        p.v.push(sin * height - height);
        let (nx, nz) = (cos * 2.0 / width, sin / height);
        let len = (nx * nx + nz * nz).sqrt();
        p.nu.push(nx / len);
        p.nv.push(nz / len);
    }
    p
}

fn direction(s: &Seg) -> [f32; 2] {
    let d = [s.b[0] - s.a[0], s.b[1] - s.a[1]];
    let len = (d[0] * d[0] + d[1] * d[1]).sqrt().max(1e-9);
    [d[0] / len, d[1] / len]
}

fn joins(a: &Seg, b: &Seg) -> bool {
    let (da, db) = (direction(a), direction(b));
    a.z == b.z && a.b == b.a && da[0] * db[0] + da[1] * db[1] >= SHARP
}

fn left(d: [f32; 2]) -> [f32; 2] {
    [-d[1], d[0]]
}

fn corner(before: Option<[f32; 2]>, after: Option<[f32; 2]>) -> ([f32; 2], f32) {
    match (before, after) {
        (Some(a), Some(b)) => {
            let (na, nb) = (left(a), left(b));
            let sum = [na[0] + nb[0], na[1] + nb[1]];
            let len = (sum[0] * sum[0] + sum[1] * sum[1]).sqrt().max(1e-6);
            let unit = [sum[0] / len, sum[1] / len];
            let cos = unit[0] * na[0] + unit[1] * na[1];
            (unit, 1.0 / cos.max(0.5))
        }
        (Some(d), None) | (None, Some(d)) => (left(d), 1.0),
        (None, None) => ([0.0, 1.0], 1.0),
    }
}

fn triangle(s: &mut Surface<Plastic>, c: [(V3, V3); 3]) {
    for (p, n) in c {
        s.positions.push(p);
        s.normals.push(n);
    }
}

fn chain(
    s: &mut Surface<Plastic>,
    segs: &[Seg],
    before: Option<[f32; 2]>,
    after: Option<[f32; 2]>,
    shape: &Profile,
    thick: f32,
) {
    let k = shape.u.len();
    let first = &segs[0];
    let mut rings: Vec<Vec<(V3, V3)>> = Vec::with_capacity(segs.len() + 1);
    for i in 0..=segs.len() {
        let at = if i == 0 { first.a } else { segs[i - 1].b };
        let behind = if i == 0 { before } else { Some(direction(&segs[i - 1])) };
        let ahead = if i == segs.len() { after } else { Some(direction(&segs[i])) };
        let (unit, stretch) = corner(behind, ahead);
        let z = first.z;
        rings.push(
            (0..k)
                .map(|j| {
                    let across = shape.u[j] * stretch;
                    let p = [at[0] + unit[0] * across, at[1] + unit[1] * across, z + shape.v[j]];
                    let n = [unit[0] * shape.nu[j], unit[1] * shape.nu[j], shape.nv[j]];
                    (p, n)
                })
                .collect(),
        );
    }
    let _ = thick;
    for w in rings.windows(2) {
        for j in 0..k - 1 {
            let (a, b, c, d) = (w[0][j], w[1][j], w[1][j + 1], w[0][j + 1]);
            triangle(s, [a, b, c]);
            triangle(s, [a, c, d]);
        }
    }
    let cap = |s: &mut Surface<Plastic>, ring: &[(V3, V3)], n: V3| {
        for j in 1..k - 1 {
            triangle(s, [(ring[0].0, n), (ring[j].0, n), (ring[j + 1].0, n)]);
        }
    };
    if before.is_none() {
        let d = direction(first);
        cap(s, &rings[0], [-d[0], -d[1], 0.0]);
    }
    if after.is_none() {
        let d = direction(&segs[segs.len() - 1]);
        cap(s, &rings[segs.len()], [d[0], d[1], 0.0]);
    }
}

fn bucket(
    s: &mut Surface<Plastic>,
    layer: &[Seg],
    from: usize,
    to: usize,
    shape: &Profile,
    thick: f32,
) {
    let mut i = from;
    while i < to {
        let mut j = i + 1;
        while j < to && joins(&layer[j - 1], &layer[j]) {
            j += 1;
        }
        let before = (i > 0 && joins(&layer[i - 1], &layer[i])).then(|| direction(&layer[i - 1]));
        let after =
            (j < layer.len() && joins(&layer[j - 1], &layer[j])).then(|| direction(&layer[j]));
        chain(s, &layer[i..j], before, after, shape, thick);
        i = j;
    }
}

pub fn build(tp: &Toolpath) -> Built {
    let layers = tp.layers();
    let rise = (tp.max[2] - tp.min[2]).max(0.001);
    let points = match tp.segs.len() {
        0..=40_000 => 7,
        40_001..=120_000 => 5,
        _ => 3,
    };
    let per_layer = (BUCKETS / layers.max(1)).clamp(1, MAX_PER_LAYER);
    let mut surfaces: Vec<Surface<Plastic>> = Vec::new();
    let mut ranges = Vec::new();
    let mut layer_of = Vec::new();
    for layer in 0..layers {
        let from = tp.layer_start[layer];
        let to = tp.layer_start.get(layer + 1).copied().unwrap_or(tp.segs.len());
        let z = tp.layer_z[layer];
        let below = if layer == 0 { 0.0 } else { tp.layer_z[layer - 1] };
        let thick = if layer == 0 { z.min(LAYER) } else { (z - below).clamp(0.04, 0.8) };
        let width = (thick * 2.25).clamp(0.4, 0.8);
        let shape = profile(points, width, thick);
        let colour = height_colour((z - tp.min[2]) / rise);
        let count = to - from;
        let buckets = per_layer.min(count).max(1);
        for b in 0..buckets {
            let (lo, hi) = (from + count * b / buckets, from + count * (b + 1) / buckets);
            if lo == hi {
                continue;
            }
            let mut surface =
                Surface { colour, group: surfaces.len(), material: Plastic, ..Default::default() };
            bucket(&mut surface, &tp.segs[from..to], lo - from, hi - from, &shape, thick);
            surfaces.push(surface);
            ranges.push((lo, hi));
            layer_of.push(layer);
        }
    }
    let centre = [
        (tp.min[0] + tp.max[0]) / 2.0,
        (tp.min[1] + tp.max[1]) / 2.0,
        (tp.min[2] + tp.max[2]) / 2.0,
    ];
    let size = tp.size();
    let radius =
        (0.5 * (size[0] * size[0] + size[1] * size[1] + size[2] * size[2]).sqrt()).max(1.0);
    let (x0, y0, x1, y1, floor) = (tp.min[0], tp.min[1], tp.max[0], tp.max[1], tp.min[2] - LAYER);
    let corners = [[x0, y0, floor], [x1, y0, floor], [x1, y1, floor], [x0, y1, floor]];
    let outline = Lines {
        segments: (0..4).map(|k| [corners[k], corners[(k + 1) % 4]]).collect(),
        colour: [0.35, 0.38, 0.45],
        group: surfaces.len(),
        width: 1.0,
        material: Plastic,
    };
    let scene = Scene { surfaces, lines: vec![outline], centre, radius, ..Default::default() };
    Built { scene: std::sync::Arc::new(scene), ranges, layer_of }
}

pub fn looks(built: &Built, upto: usize, printed: Option<usize>, rest: bool) -> Vec<Look> {
    let now = printed.map(|n| {
        let at = n.saturating_sub(1);
        built
            .ranges
            .partition_point(|&(_, end)| end <= at)
            .min(built.ranges.len().saturating_sub(1))
    });
    let mut out: Vec<Look> = built
        .layer_of
        .iter()
        .enumerate()
        .map(|(group, &layer)| {
            if layer > upto {
                return Look::HIDDEN;
            }
            match now {
                None => Look::default(),
                Some(n) if group < n => Look::default(),
                Some(n) if group == n => Look { selected: true, ..Look::default() },
                Some(_) if rest => Look { opacity: 0.12, ..Look::default() },
                Some(_) => Look::HIDDEN,
            }
        })
        .collect();
    out.push(Look::default());
    out
}

pub fn snapshot(path: &Path, width: usize, height: usize) -> Result<gcode::Thumbnail, String> {
    let tp = parse_within(path, SNAPSHOT_SEGMENTS, SNAPSHOT_TOLERANCE)?;
    let built = build(&tp);
    let scene = built.scene.clone();
    let size = Vec2::new(width as f32, height as f32);
    let rect = Rect::from_min_size(Pos2::ZERO, size);
    let corners: Vec<V3> = (0..8)
        .map(|k| {
            let pick = |axis: usize, bit: usize| {
                if k >> bit & 1 == 0 { tp.min[axis] } else { tp.max[axis] }
            };
            [pick(0, 0), pick(1, 1), pick(2, 2)]
        })
        .collect();
    let bounds = |camera: &Camera| {
        let projector = Projector::new(&scene, camera, rect);
        corners
            .iter()
            .filter_map(|p| projector.project(*p))
            .fold(Rect::NOTHING, |r, p| r.union(Rect::from_min_max(p, p)))
    };
    let start = Camera::default();
    let seen = bounds(&start);
    let camera = start.panned(&scene, size, rect.center() - seen.center());
    let fit = (size.x * SNAPSHOT_FILL / seen.width().max(1.0))
        .min(size.y * SNAPSHOT_FILL / seen.height().max(1.0));
    let camera = Camera { zoom: camera.zoom * fit, ..camera };
    let viewer =
        Viewer::new(scene, camera).style(style()).looks(looks(&built, usize::MAX, None, false));
    let image = viewer.render_soft(width, height);
    let rgba = image.pixels.iter().flat_map(|c| c.to_array()).collect();
    Ok(gcode::Thumbnail { width, height, rgba })
}

pub fn style() -> Style {
    Style { selected_tint: 1.0, ..Style::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str, body: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("printfarm-path-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("part.gcode");
        std::fs::write(&path, body).unwrap();
        path
    }

    const SQUARE: &str = "G28\nM82\nG90\nG1 X0 Y0 Z0.2\nG92 E0\n;LAYER_CHANGE\n\
        G1 X10 Y0 E1\nG1 X10 Y10 E2\nG1 X0 Y10 E3\nG1 X0 Y0 E4\n\
        ;LAYER_CHANGE\nG1 Z0.4\nG1 X10 Y0 E5\nG1 X10 Y10 E6\n";

    #[test]
    fn extrusion_moves_become_segments_by_layer() {
        let tp = parse(&scratch("square", SQUARE)).unwrap();
        assert_eq!(tp.segs.len(), 6);
        assert_eq!(tp.layers(), 2);
        assert_eq!(tp.layer_z, [0.2, 0.4]);
        assert_eq!(tp.layer_start, [0, 4]);
        assert_eq!(tp.min, [0.0, 0.0, 0.2]);
        assert_eq!(tp.max, [10.0, 10.0, 0.4]);
        assert_eq!(tp.layer_of(3), 0);
        assert_eq!(tp.layer_of(4), 1);
    }

    #[test]
    fn relative_extrusion_and_retracts_are_understood() {
        let body = "M83\nG90\nG1 X0 Y0 Z0.2\nG1 E-1\nG1 X5 Y0 E0.4\nG1 X5 Y5 E-1\nG1 X9 Y5 E0.4\n";
        let tp = parse(&scratch("relative", body)).unwrap();
        assert_eq!(tp.segs.len(), 2);
    }

    #[test]
    fn straight_runs_are_merged_and_curves_are_not() {
        let line = "G1 X0 Y0 Z0.2\nG92 E0\nG1 X1 Y0 E1\nG1 X2 Y0 E2\nG1 X3 Y0 E3\nG1 X3 Y1 E4\n";
        let tp = parse(&scratch("merge", line)).unwrap();
        assert_eq!(tp.segs.len(), 2);
        assert_eq!(tp.segs[0].b, [3.0, 0.0]);
        assert_eq!(tp.parsed, 4);
    }

    #[test]
    fn arcs_are_drawn_as_a_curve_from_start_to_end() {
        let body = "G90\nM82\nG92 E0\nG1 X10 Y0 Z0.2\nG92 E0\nG3 X0 Y0 I-5 J0 E5\n";
        let tp = parse(&scratch("arc", body)).unwrap();
        assert!(tp.segs.len() > 8, "{}", tp.segs.len());
        let last = tp.segs.last().unwrap();
        assert!((last.b[0]).abs() < 1e-3 && (last.b[1]).abs() < 1e-3);
        assert!(tp.max[1] > 4.9 && tp.min[1] > -0.01, "{:?} {:?}", tp.min, tp.max);
    }

    #[test]
    fn offsets_track_both_file_and_stripped_bytes() {
        let body = "; header comment\nG1 X0 Y0 Z0.2\nG92 E0\nG1 X5 Y0 E1 ; a move\n";
        let tp = parse(&scratch("offsets", body)).unwrap();
        let s = tp.segs[0];
        assert_eq!(s.raw as usize, body.len());
        assert_eq!(
            s.stripped as usize,
            "G1 X0 Y0 Z0.2\n".len() + "G92 E0\n".len() + "G1 X5 Y0 E1\n".len()
        );
        assert_eq!(tp.printed(Mark { raw: true, at: 10 }), 0);
        assert_eq!(tp.printed(Mark { raw: true, at: body.len() as u32 }), 1);
    }

    #[test]
    fn the_purge_line_before_the_first_layer_is_not_part_of_the_print() {
        let body = "M83\nG1 X2 Y10 Z0.28\nG1 X2 Y140 E10\nG1 X2.3 Y140\nG1 X2.3 Y10 E10\n\
            ;LAYER_CHANGE\nG1 Z0.2\nG1 X60 Y60\nG1 X70 Y60 E1\nG1 X70 Y70 E1\n";
        let tp = parse(&scratch("purge", body)).unwrap();
        assert_eq!(tp.segs.len(), 2);
        assert_eq!(tp.min, [60.0, 60.0, 0.2]);
        assert_eq!(tp.max, [70.0, 70.0, 0.2]);
    }

    #[test]
    fn a_file_with_no_printing_has_no_preview() {
        assert!(parse(&scratch("empty", "G28\nG1 X5 Y5\n")).is_err());
    }

    #[test]
    fn a_chain_is_one_swept_bead_with_shared_corners_and_end_caps() {
        let tp = parse(&scratch("scene", SQUARE)).unwrap();
        let built = build(&tp);
        let verts: usize = built.scene.surfaces.iter().map(|s| s.positions.len()).sum();
        let normals: usize = built.scene.surfaces.iter().map(|s| s.normals.len()).sum();
        assert_eq!(verts, normals);
        assert_eq!(built.ranges.len(), built.scene.surfaces.len());
        assert_eq!(built.ranges.first(), Some(&(0, 1)));
        assert_eq!(built.ranges.last(), Some(&(5, 6)));
        assert_eq!(built.layer_of, [0, 0, 0, 0, 1, 1]);
        let one = &built.scene.surfaces[0];
        assert_eq!(one.positions.len(), 36 + 5 * 3);
        let lo = tp.min[2] - 0.2 - 1e-3;
        for s in &built.scene.surfaces {
            for p in &s.positions {
                assert!(p[2] >= lo && p[2] <= tp.max[2] + 1e-3, "{p:?}");
                assert!(p[0] > -1.0 && p[0] < 11.0 && p[1] > -1.0 && p[1] < 11.0, "{p:?}");
            }
            for n in &s.normals {
                let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                assert!((len - 1.0).abs() < 1e-3, "{n:?}");
            }
        }
        assert_eq!(built.scene.lines[0].group, built.scene.surfaces.len());
    }

    #[test]
    fn a_bucket_joining_the_previous_one_makes_no_end_cap_there() {
        let tp = parse(&scratch("seam", SQUARE)).unwrap();
        let built = build(&tp);
        let first = &built.scene.surfaces[0];
        let second = &built.scene.surfaces[1];
        assert_eq!(first.positions.len(), 36 + 5 * 3);
        assert_eq!(second.positions.len(), 36 + 0);
        let last = &built.scene.surfaces[3];
        assert_eq!(last.positions.len(), 36 + 5 * 3);
    }

    #[test]
    fn looks_follow_the_print_and_hide_what_is_not_printed_yet() {
        let tp = parse(&scratch("looks", SQUARE)).unwrap();
        let built = build(&tp);
        let all = looks(&built, 1, None, false);
        assert_eq!(all.len(), built.ranges.len() + 1);
        assert!(all.iter().all(|l| l.visible() && !l.selected));
        let selected = |l: &[Look]| l.iter().position(|x| x.selected);
        for (printed, group) in [(0, 0), (1, 0), (2, 1), (3, 2), (6, 5), (99, 5)] {
            let live = looks(&built, 1, Some(printed), false);
            assert_eq!(selected(&live), Some(group), "printed {printed}");
            assert!(live[..group].iter().all(|l| l.visible() && !l.selected));
            assert!(live[group + 1..built.ranges.len()].iter().all(|l| !l.visible()));
            assert!(live[built.ranges.len()].visible());
        }
        let rest = looks(&built, 1, Some(2), true);
        assert!(rest[3].visible() && rest[3].opacity < 0.2);
        let cut = looks(&built, 0, Some(6), false);
        assert!(cut[0].visible() && !cut[4].visible() && !cut[5].visible());
    }

    #[test]
    fn simplifying_a_raster_keeps_every_line() {
        let mut segs = Vec::new();
        let mut at = [0.0f32, 0.0];
        for row in 0..40 {
            let y = row as f32 * 0.45;
            let x = if row % 2 == 0 { 20.0 } else { 0.0 };
            for to in [[at[0], y], [x, y]] {
                if to != at {
                    segs.push(Seg { a: at, b: to, z: 0.2, raw: 0, stripped: 0 });
                    at = to;
                }
            }
        }
        let fewer = simplify(segs.clone(), 1.0);
        assert_eq!(fewer.len(), segs.len());
    }

    #[test]
    fn simplifying_keeps_every_layer_and_the_ends_of_every_chain() {
        let mut body = String::from("G90\nM82\nG92 E0\nG1 X0 Y0 Z0.2\n");
        let mut e = 0.0;
        for layer in 0..6 {
            body.push_str(&format!(
                ";LAYER_CHANGE\nG1 Z{:.1}\nG1 X0 Y0\n",
                0.2 + layer as f32 * 0.2
            ));
            for i in 0..400 {
                let a = i as f32 * 0.015;
                e += 0.05;
                body.push_str(&format!(
                    "G1 X{:.4} Y{:.4} E{e:.3}\n",
                    20.0 * a.cos(),
                    20.0 * a.sin()
                ));
            }
        }
        let tp = parse(&scratch("coarse", &body)).unwrap();
        assert_eq!(tp.layers(), 6);
        let fewer = simplify(tp.segs.clone(), 0.1);
        assert!(fewer.len() < tp.segs.len() / 3, "{} of {}", fewer.len(), tp.segs.len());
        let layers = |v: &[Seg]| {
            let mut z: Vec<f32> = v.iter().map(|s| s.z).collect();
            z.dedup();
            z
        };
        assert_eq!(layers(&fewer), layers(&tp.segs));
        for layer in 0..6 {
            let z = 0.2 + layer as f32 * 0.2;
            let of = |v: &[Seg]| {
                v.iter().filter(|s| (s.z - z).abs() < 1e-4).copied().collect::<Vec<_>>()
            };
            let (a, b) = (of(&tp.segs), of(&fewer));
            assert_eq!(a.first().unwrap().a, b.first().unwrap().a);
            assert_eq!(a.last().unwrap().b, b.last().unwrap().b);
            let near = |p: [f32; 2], chain: &[Seg]| {
                chain.iter().map(|s| distance_to(p, s.a, s.b)).fold(f32::MAX, f32::min)
            };
            for s in &a {
                assert!(near(s.a, &b) < 0.15, "z {z}");
            }
        }
    }
}
