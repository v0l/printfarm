use std::collections::BTreeMap;
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq)]
pub struct Mesh {
    pub rows: Vec<Vec<Option<f32>>>,
    pub read: SystemTime,
    pub probed: bool,
}

impl Mesh {
    pub fn columns(&self) -> usize {
        self.rows.iter().map(Vec::len).max().unwrap_or(0)
    }

    pub fn values(&self) -> impl Iterator<Item = f32> + '_ {
        self.rows.iter().flatten().flatten().copied()
    }

    pub fn span(&self) -> Option<(f32, f32)> {
        let mut it = self.values();
        let first = it.next()?;
        Some(it.fold((first, first), |(lo, hi), v| (lo.min(v), hi.max(v))))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Leveling {
    pub enabled: Option<bool>,
    pub fade: Option<f32>,
    pub mesh: Option<Mesh>,
}

impl Leveling {
    pub fn read_state(&mut self, line: &str) -> bool {
        let text = line.trim_start_matches("echo:").trim();
        if let Some(rest) = text.strip_prefix("Bed Leveling") {
            match rest.trim().to_ascii_uppercase().as_str() {
                "ON" => self.enabled = Some(true),
                "OFF" => self.enabled = Some(false),
                _ => return false,
            }
            return true;
        }
        if let Some(rest) = text.strip_prefix("Fade Height") {
            self.fade = rest.trim_start_matches(':').trim().parse().ok();
            return true;
        }
        if let Some(rest) = text.strip_prefix("M420 ") {
            for word in rest.split_whitespace() {
                if let Some(s) = word.strip_prefix('S') {
                    self.enabled = Some(s == "1");
                } else if let Some(z) = word.strip_prefix('Z') {
                    self.fade = z.parse().ok();
                }
            }
            return true;
        }
        false
    }
}

pub fn grid_title(line: &str) -> Option<bool> {
    let t = line.trim_start_matches("echo:").trim();
    if t.starts_with("Subdivided") {
        Some(false)
    } else if t.starts_with("Bilinear Leveling Grid") || t.starts_with("Measured points") {
        Some(true)
    } else {
        None
    }
}

pub struct GridReader {
    keep: bool,
    header: bool,
    rows: BTreeMap<usize, Vec<Option<f32>>>,
}

fn point(word: &str) -> Option<Option<f32>> {
    if word.chars().all(|c| c == '=' || c == '.' || c == '-') || word.eq_ignore_ascii_case("nan") {
        return Some(None);
    }
    if !word.contains('.') {
        return None;
    }
    word.parse().ok().map(Some)
}

impl GridReader {
    pub fn new(keep: bool) -> GridReader {
        GridReader { keep, header: false, rows: BTreeMap::new() }
    }

    pub fn feed(&mut self, line: &str) -> bool {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some((first, rest)) = words.split_first() else { return false };
        let integers = words.iter().all(|w| w.parse::<usize>().is_ok());
        if integers && !self.header && self.rows.is_empty() {
            self.header = true;
            return true;
        }
        let Ok(index) = first.parse::<usize>() else { return false };
        let values: Option<Vec<Option<f32>>> = rest.iter().map(|w| point(w)).collect();
        match values {
            Some(v) if !v.is_empty() => {
                self.rows.insert(index, v);
                true
            }
            _ => false,
        }
    }

    pub fn finish(self) -> Option<Vec<Vec<Option<f32>>>> {
        if !self.keep || self.rows.is_empty() {
            return None;
        }
        let n = self.rows.keys().max().copied()? + 1;
        Some((0..n).map(|i| self.rows.get(&i).cloned().unwrap_or_default()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(lines: &[&str]) -> Option<Vec<Vec<Option<f32>>>> {
        let mut it = lines.iter();
        let keep = grid_title(it.next().unwrap()).unwrap();
        let mut g = GridReader::new(keep);
        for l in it {
            if !g.feed(l) {
                break;
            }
        }
        g.finish()
    }

    #[test]
    fn reads_a_bilinear_grid() {
        let rows = read(&[
            "Bilinear Leveling Grid:",
            "      0      1      2      3      4",
            " 0 +0.485 +0.295 +0.182 +0.095 +0.110",
            " 1 +0.388 +0.230 +0.108 -0.020 -0.068",
            " 2 +0.375 +0.210 +0.040 -0.102 -0.152",
            " 3 +0.380 +0.180 +0.010 -0.148 -0.237",
            " 4 +0.418 +0.158 -0.023 -0.183 =====",
            "ok",
        ])
        .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0][0], Some(0.485));
        assert_eq!(rows[1][3], Some(-0.020));
        assert_eq!(rows[4][4], None);
        let m = Mesh { rows, read: SystemTime::now(), probed: false };
        assert_eq!(m.columns(), 5);
        assert_eq!(m.span(), Some((-0.237, 0.485)));
    }

    #[test]
    fn reads_a_manual_mesh() {
        let rows = read(&[
            "echo:Measured points:",
            "        0        1        2",
            " 0 +0.12345 -0.01000 +0.00000",
            " 1 +0.10000 +0.05000 -0.03000",
            "X:0.00 Y:0.00 Z:10.00 E:0.00 Count X:0 Y:0 Z:4000",
        ])
        .unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Some(0.12345), Some(-0.01), Some(0.0)],
                vec![Some(0.1), Some(0.05), Some(-0.03)]
            ]
        );
    }

    #[test]
    fn skips_the_subdivided_grid() {
        assert!(
            read(&["Subdivided with CATMULL ROM Leveling Grid:", "   0   1", " 0 +0.1 +0.2"])
                .is_none()
        );
    }

    #[test]
    fn reads_leveling_state() {
        let mut l = Leveling::default();
        assert!(l.read_state("echo:Bed Leveling ON"));
        assert!(l.read_state("echo:Fade Height 10.00"));
        assert_eq!((l.enabled, l.fade), (Some(true), Some(10.0)));
        assert!(l.read_state("echo:  M420 S0 Z5.00"));
        assert_eq!((l.enabled, l.fade), (Some(false), Some(5.0)));
        assert!(!l.read_state("echo:Unknown command"));
    }
}
